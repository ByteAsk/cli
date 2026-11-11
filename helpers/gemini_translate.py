#!/usr/bin/env python3
"""Responses API <-> Google Gemini generateContent API translation (stdlib only).

The ByteAsk engine speaks ONLY the OpenAI Responses API. This module lets the
gateway route ``gemini-*`` models to Google by translating each request
(Responses -> generateContent) and transducing the streamed response (Gemini SSE
-> Responses SSE). Stateless per-request (the Codex client resends full history).

Mirrors ``anthropic_translate.py``; the Gemini-specific differences that shaped it:
  * Roles are ``user`` / ``model`` (not assistant); content is a ``parts`` array.
  * Function calls carry an ``id`` (for parallel matching) BUT a ``functionResponse``
    also REQUIRES the function ``name`` — Responses' ``function_call_output`` only has
    the ``call_id``, so translate_request builds a per-request ``call_id -> name`` map
    (from the FunctionCall items earlier in the SAME resent history) to recover it.
  * ``functionCall.args`` / ``functionResponse.response`` are OBJECTS (not strings).
  * Caching is IMPLICIT (automatic) — no cache_control; no per-token cache-write, only
    a read discount. So Gemini bills on the existing {in, cached, out} model with
    cache_write=0. Usage: input=promptTokenCount (incl cached), cached=
    cachedContentTokenCount, output=candidatesTokenCount+thoughtsTokenCount.
  * Thinking is left at default (thinkingConfig omitted), BUT thinking models (esp.
    Gemini 3.x) 400 a follow-up whose functionCall omits the emitted ``thoughtSignature``
    — so the gateway remembers ``call_id -> signature`` and re-attaches it (see below).

Public API:
  is_gemini_model(model) -> bool ; gemini_model_id(model) -> str
  translate_request(responses_body: dict) -> dict
  route(model, api_key) -> (path, headers)
  Transducer(model) .feed(gemini_chunk)->[str] .close()->[str]
  iter_sse(byte_iter) -> yields parsed SSE data dicts
  UnsupportedItemError
"""
import base64
import copy
import hashlib
import json
import os
import re
import sqlite3
import sys
import threading
import time
import uuid

import effort as _effort


def _warn(msg):
    """One-line stderr note. Runs under the gateway (journalctl) AND under the
    user's byok sidecar, so keep it short and never raise."""
    try:
        sys.stderr.write(f"[gemini] {msg}\n")
    except Exception:
        pass

# --- config --------------------------------------------------------------- #
MODEL_MAP = {  # client slug -> real Gemini id (identity today; seam for aliases)
    "gemini-2.5-pro": "gemini-2.5-pro",
    "gemini-2.5-flash": "gemini-2.5-flash",
    "gemini-2.5-flash-lite": "gemini-2.5-flash-lite",
    "gemini-3.5-flash": "gemini-3.5-flash",
    "gemini-3.5-flash-lite": "gemini-3.5-flash-lite",
    "gemini-3.6-flash": "gemini-3.6-flash",
    # Both real Gemini ids (verified against ai.google.dev).
    "gemini-3.7-flash": "gemini-3.7-flash",
    "gemini-3.8-flash": "gemini-3.8-flash",
    "gemini-3.1-pro-preview": "gemini-3.1-pro-preview",
}
MAX_OUTPUT_TOKENS = {"gemini-2.5-pro": 65536, "gemini-3.1-pro-preview": 65536,
                     # Gemini thinking tokens come OUT of the output budget, so a
                     # 16K default can return finishReason: MAX_TOKENS with empty
                     # text once thinkingLevel is high. Give every flash model that
                     # serves effort levels (3.6/3.7/3.8 in the served
                     # model catalog) the same real headroom.
                     "gemini-3.6-flash": 32768, "gemini-3.5-flash-lite": 32768,
                     "gemini-3.7-flash": 32768, "gemini-3.8-flash": 32768}
# Engine reasoning effort -> Gemini `thinkingConfig.thinkingLevel`.
# Valid values PROBED LIVE 2026-07-25: minimal / low / medium / high (200);
# none / off / max / dynamic (400). No xhigh or max rung exists.
EFFORT_MAP = {
    "none": "minimal", "minimal": "minimal", "low": "low", "medium": "medium",
    "high": "high", "xhigh": "high", "max": "high", "ultra": "high",
}
MAX_OUTPUT_DEFAULT = 16384
API_VERSION = "v1beta"

# Opt-in reasoning capture: when on, ask Gemini to return its thought summary
# (buffered into Transducer.output_items, never streamed to the engine). Default off.
_REASONING_CAPTURE = (os.environ.get("BYTEASK_REASONING_CAPTURE", "")
                      .strip().lower() in ("1", "true", "yes", "on"))

# --- thought-signature round-trip ----------------------------------------- #
# Gemini THINKING models (esp. the 3.x series) HARD-REJECT (400 "Function call is
# missing a thought signature") a follow-up turn whose model `functionCall` part does
# not carry back the `thoughtSignature` the model emitted. The Responses `function_call`
# item has no field to carry it, so we remember `call_id -> thoughtSignature` from the
# response and re-attach it when the client resends that functionCall in a later turn.
#
# Signatures are up to ~3 KB, so we can NOT stuff them into the call_id (the engine echoes
# call_ids verbatim and a huge one is risky). Instead: an in-memory L1 (fast, bounded FIFO)
# BACKED BY an optional sqlite L2 (init_signature_store) so signatures SURVIVE A GATEWAY
# RESTART — the failure the in-memory-only cache had: a deploy in the middle of a Gemini
# tool conversation wiped every signature, so the next turn 400'd. Synthesized call_ids are
# uuid-based (globally unique — a persisted signature can never be handed to the wrong later
# call, even across a restart where a counter would reset and collide). Fail-safe: any sqlite
# error disables L2 and falls back to L1 (== the old behavior); it never breaks a request.
_SIG_LOCK = threading.Lock()
_SIG_CACHE = {}            # call_id -> thoughtSignature   (L1, in-process)
_SIG_ORDER = []            # insertion order, for FIFO eviction
_SIG_CAP = 8192
_SIG_DB = None             # sqlite3.Connection (L2) once init_signature_store() succeeds
_SIG_DB_TTL = 172800       # forget persisted signatures older than 48h (dead conversations)
_SIG_PRUNE_EVERY = 200     # amortize the age-based prune
_SIG_WRITES = [0]

# When we have NO real signature for a functionCall — it came from a DIFFERENT
# provider (a user switching gpt-* -> gemini mid-conversation), or the sig was
# evicted/lost — Gemini 3.x hard-400s ("Function call is missing a
# thought_signature"). Omitting is tolerated in some shapes but not others.
# Google recognizes ONE placeholder sentinel:
# base64("context_engineering_is_the_way_to_go"). Verified 2026-07-15 against
# the live API — accepted (200) on gemini-2.5-pro / 3.1-pro / 3.5-flash, while a
# random base64 string 400s "Invalid thought signature". Strictly safer than
# omitting: accepted everywhere omitting is, and fixes the cases where it 400s.
_PLACEHOLDER_SIG = base64.b64encode(b"context_engineering_is_the_way_to_go").decode()


def _new_call_id(name):
    """A globally-unique synthesized call_id. uuid (not a resettable counter) so a
    signature persisted before a restart is never echoed to the wrong later call."""
    return "%s_%s" % (name or "fn", uuid.uuid4().hex)


def init_signature_store(path):
    """Enable durable (restart-surviving) signature storage at ``path`` (a sqlite file).
    The gateway calls this at startup; the standalone BYOK sidecar leaves it unset and
    uses L1 only. Fail-safe: on any error L2 stays disabled (pure in-memory behavior)."""
    global _SIG_DB
    try:
        d = os.path.dirname(path)
        if d:
            os.makedirs(d, exist_ok=True)
        conn = sqlite3.connect(path, check_same_thread=False)
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("CREATE TABLE IF NOT EXISTS gemini_sigs "
                     "(call_id TEXT PRIMARY KEY, sig TEXT NOT NULL, ts INTEGER NOT NULL)")
        conn.commit()
        with _SIG_LOCK:
            _SIG_DB = conn
    except Exception:
        _SIG_DB = None    # stay in-memory; never fatal


def _l1_put(call_id, sig):   # caller holds _SIG_LOCK
    if call_id not in _SIG_CACHE:
        _SIG_ORDER.append(call_id)
        while len(_SIG_ORDER) > _SIG_CAP:
            _SIG_CACHE.pop(_SIG_ORDER.pop(0), None)
    _SIG_CACHE[call_id] = sig


def _remember_signature(call_id, sig):
    if not call_id or not sig:
        return
    with _SIG_LOCK:
        _l1_put(call_id, sig)
        if _SIG_DB is not None:
            try:
                now = int(time.time())
                _SIG_DB.execute("INSERT OR REPLACE INTO gemini_sigs VALUES (?,?,?)",
                                (call_id, sig, now))
                _SIG_WRITES[0] += 1
                if _SIG_WRITES[0] % _SIG_PRUNE_EVERY == 0:
                    _SIG_DB.execute("DELETE FROM gemini_sigs WHERE ts < ?",
                                    (now - _SIG_DB_TTL,))
                _SIG_DB.commit()
            except Exception:
                pass    # L1 still holds it for this process; never break the request


def _recall_signature(call_id):
    if not call_id:
        return None
    with _SIG_LOCK:
        sig = _SIG_CACHE.get(call_id)
        if sig is not None:
            return sig
        if _SIG_DB is not None:
            try:
                row = _SIG_DB.execute(
                    "SELECT sig FROM gemini_sigs WHERE call_id=?", (call_id,)).fetchone()
                if row:
                    _l1_put(call_id, row[0])   # warm L1 so later turns skip the DB
                    return row[0]
            except Exception:
                pass
    return None


class UnsupportedItemError(Exception):
    """A Responses ``input`` item type we refuse to silently drop."""


def is_gemini_model(model):
    return isinstance(model, str) and model.startswith("gemini")


def gemini_model_id(model):
    return MODEL_MAP.get(model, model)


def route(model, api_key):
    """(upstream path, headers) for a streaming Gemini call."""
    path = f"/{API_VERSION}/models/{gemini_model_id(model)}:streamGenerateContent?alt=sse"
    return path, {"x-goog-api-key": api_key or "", "content-type": "application/json"}


# ==== tool names: namespaces, tool_search, deferred ("loaded") tools ======= #
# The engine defers every MCP tool and every sub-agent tool: they are NOT in tools[].
# The model finds them with the client-side `tool_search` tool and the engine answers with a
# `tool_search_output` INPUT item whose `tools` are `namespace` objects. OpenAI loads those
# server-side; here we declare them ourselves, as ordinary tools with provider-legal names,
# and map every call back to the engine's (namespace, name) PAIR -- the only thing the
# engine resolves a call by. (Captured from the real engine: tests/fixtures/.)
#
# ⚠️ This block is deliberately duplicated in gemini_translate.py and openai_compat_translate.py.
# They are served to users as three separate files; a shared helper module would be a fourth
# file that every installer fetch list and nginx block must know about.
_DEFAULT_NAMESPACE = "functions"
_WIRE_NAME_OK = re.compile(r"^[A-Za-z_][A-Za-z0-9_.:-]{0,63}$")   # Gemini: letter/_ first, then a-z 0-9 _ . : -
_WIRE_NAME_BAD = re.compile(r"[^A-Za-z0-9_.:-]")
_WIRE_NAME_MAX = 64
TOOL_SEARCH_NAME = "tool_search"
_CUSTOM_SCHEMA = {"type": "object", "properties": {"input": {"type": "string"}},
                  "required": ["input"]}


class NameMap(dict):
    """Reverse map for ONE request: wire (provider-facing) name -> engine tool name.

    ``.namespaces`` {wire: namespace} for namespaced tools, ``.custom`` the wires of freeform
    (custom) tools, ``.search`` the wire of the tool_search emulation (or None). A plain tool
    whose wire name equals its engine name has no entry, so an all-plain request is empty."""

    def __init__(self):
        super().__init__()
        self.namespaces = {}
        self.custom = set()
        self.search = None


class _Request(dict):
    """The Gemini request body, carrying the NameMap the Transducer needs. A plain dict
    for every other purpose (json.dumps, indexing)."""
    name_map = None


def _wire_name(namespace, name, force_hash=False):
    """Provider-legal name for the engine tool (namespace, name). Deterministic. A plain tool
    with a legal name is returned unchanged; a namespaced one is joined the way the engine
    joins MCP names (namespace without trailing '_' + '__' + name); an illegal or over-long
    result is sanitized and gets a short hash of the ORIGINAL pair so two distinct tools can
    never share a name."""
    name = name or "tool"
    ns = "" if (not namespace or namespace == _DEFAULT_NAMESPACE) else namespace
    raw = name if not ns else ns.rstrip("_") + "__" + name
    if not force_hash and _WIRE_NAME_OK.match(raw):
        return raw
    digest = hashlib.sha1(("%s\0%s" % (ns, name)).encode("utf-8")).hexdigest()[:8]
    base = _WIRE_NAME_BAD.sub("_", raw)
    if not re.match(r"[A-Za-z_]", base):
        base = "_" + base                      # Gemini names must start with a letter or '_'
    base = base[:_WIRE_NAME_MAX - 9].rstrip("_") or "tool"
    return base + "_" + digest


def _loadable_entries(tools):
    """(namespace, tool_dict) for every callable tool in a `tools` list that may hold plain
    function/custom tools and `namespace` objects (a tool_search_output's `tools`)."""
    for entry in tools or []:
        if not isinstance(entry, dict):
            continue
        if entry.get("type") == "namespace":
            ns = entry.get("name")
            for sub in entry.get("tools") or []:
                if isinstance(sub, dict):
                    yield ns, sub
        else:
            yield None, entry


def _collect_tools(body):
    """Every tool the model may call in this request -> (specs, NameMap, wire_for).

    Order is stable so the provider's prompt prefix stays cacheable: the engine's own tools[]
    in the order sent, then the tools loaded by `tool_search_output` items in the order they
    appear in the (fully resent) history. ``wire_for(namespace, name)`` maps an engine
    (namespace, name) to the wire name declared here, or the pure encoding if it was never
    declared (a call in history whose search result was compacted away)."""
    nm = NameMap()
    specs, by_key, taken = [], {}, {}

    def add(namespace, name, description, parameters, custom=False, search=False, fmt=None):
        if not isinstance(name, str) or not name:
            return
        ns = "" if (not namespace or namespace == _DEFAULT_NAMESPACE) else namespace
        key = (ns, name)
        if key in by_key:
            return
        wire = _wire_name(ns, name)
        if taken.get(wire, key) != key:
            wire = _wire_name(ns, name, force_hash=True)
        taken[wire] = key
        by_key[key] = wire
        if wire != name or ns:
            nm[wire] = name
        if ns:
            nm.namespaces[wire] = ns
        if custom:
            nm.custom.add(wire)
        if search:
            nm.search = wire
        specs.append({"wire": wire, "description": description or "", "parameters": parameters,
                      "custom": custom, "search": search, "format": fmt})

    def from_tool(namespace, tool):
        tt = tool.get("type")
        if tt == "function":
            add(namespace, tool.get("name"), tool.get("description"), tool.get("parameters"))
        elif tt in ("custom", "freeform"):
            add(namespace, tool.get("name"), tool.get("description"), None, custom=True,
                fmt=tool.get("format"))

    for tool in body.get("tools") or []:
        if not isinstance(tool, dict):
            continue
        if tool.get("type") == "tool_search":
            add(None, TOOL_SEARCH_NAME, tool.get("description"), tool.get("parameters"), search=True)
        else:
            for ns, sub in _loadable_entries([tool]):
                from_tool(ns, sub)
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("type") == "tool_search_output":
            for ns, sub in _loadable_entries(item.get("tools")):
                from_tool(ns, sub)

    def wire_for(namespace, name):
        ns = "" if (not namespace or namespace == _DEFAULT_NAMESPACE) else namespace
        return by_key.get((ns, name)) or _wire_name(ns, name)

    return specs, nm, wire_for


def _render_search_output(item, wire_for):
    """The tool_result the model reads after tool_search: WHICH names it can now call."""
    lines = []
    for ns, tool in _loadable_entries(item.get("tools")):
        name = tool.get("name")
        if isinstance(name, str) and name:
            desc = " ".join((tool.get("description") or "").split())
            lines.append("- %s: %s" % (wire_for(ns, name), desc[:160]))
    if not lines:
        return "No matching tools were found."
    return ("Loaded %d tool(s). They are now available: call them directly by these exact names.\n%s"
            % (len(lines), "\n".join(lines)))


def _search_args(raw):
    """tool_search arguments from the model: an object with `query` and a numeric `limit`.
    Models often stringify numbers and the engine rejects "8" for a number field."""
    try:
        v = json.loads(raw) if isinstance(raw, str) else raw
    except Exception:
        v = None
    if not isinstance(v, dict):
        return {}
    out = {}
    if isinstance(v.get("query"), str):
        out["query"] = v["query"]
    lim = v.get("limit")
    if isinstance(lim, str) and lim.strip().isdigit():
        lim = int(lim.strip())
    if isinstance(lim, (int, float)) and not isinstance(lim, bool):
        out["limit"] = int(lim)
    return out


def _call_item(nm, wire, args, call_id, custom_names=()):
    """The Responses item for one model tool call: a tool_search_call, a custom_tool_call (the
    engine refuses a freeform tool such as apply_patch called as a plain function) or a
    function_call carrying the engine's namespace. An unknown wire name degrades to a plain
    function_call (the engine then answers "unsupported call", which the model can read).
    ``nm`` is a NameMap (or None / a plain {wire: name} dict, which decodes names only)."""
    nm = nm if nm is not None else {}
    if getattr(nm, "search", None) is not None and wire == nm.search:
        return {"type": "tool_search_call", "call_id": call_id, "execution": "client",
                "arguments": _search_args(args)}
    name = nm.get(wire, wire)
    ns = getattr(nm, "namespaces", {}).get(wire)
    if wire in getattr(nm, "custom", ()) or (not ns and name in custom_names):
        inp = args
        try:
            d = json.loads(args)
            if isinstance(d, dict) and isinstance(d.get("input"), str):
                inp = d["input"]
        except Exception:
            pass
        item = {"type": "custom_tool_call", "name": name, "input": inp, "call_id": call_id}
    else:
        item = {"type": "function_call", "name": name, "arguments": args, "call_id": call_id}
    if ns:
        item["namespace"] = ns
    return item


# ==== request: Responses -> Gemini generateContent ========================= #
def _image_part(image_url):
    """Responses input_image.image_url -> Gemini part. data-URL -> inlineData;
    http(s) url -> fileData (Google must be able to reach it — best effort)."""
    if isinstance(image_url, str) and image_url.startswith("data:") and ";base64," in image_url:
        header, b64 = image_url.split(";base64,", 1)
        mime = header[len("data:"):] or "image/png"
        return {"inlineData": {"mimeType": mime, "data": b64}}
    return {"fileData": {"fileUri": image_url, "mimeType": "image/*"}}


def _parts_from_message(content_items):
    parts = []
    for c in content_items or []:
        if not isinstance(c, dict):
            continue
        t = c.get("type")
        if t in ("input_text", "output_text") and isinstance(c.get("text"), str):
            if c["text"]:
                parts.append({"text": c["text"]})
        elif t == "input_image" and c.get("image_url"):
            parts.append(_image_part(c["image_url"]))
    return parts


def _args_object(raw):
    """functionCall.args must be an OBJECT. FunctionCall.arguments is a JSON string;
    CustomToolCall.input is freeform text."""
    if isinstance(raw, dict):
        return raw
    if isinstance(raw, str):
        try:
            v = json.loads(raw)
            return v if isinstance(v, dict) else {"input": raw}
        except Exception:
            return {"input": raw}
    return {}


def _response_object(output):
    """functionResponse.response must be an OBJECT. Wrap Responses' string/array."""
    if isinstance(output, str):
        return {"output": output}
    if isinstance(output, list):
        text = "".join(c.get("text", "") for c in output
                       if isinstance(c, dict) and c.get("type") == "input_text")
        return {"output": text}
    return {"output": ""}


def _append(contents, role, part):
    """Append a part, merging into the previous content when same-role."""
    if contents and contents[-1]["role"] == role:
        contents[-1]["parts"].append(part)
    else:
        contents.append({"role": role, "parts": [part]})


def _translate_item(item, contents, call_names, wire_for=None):
    """One Responses ``input`` item -> Gemini content part(s). ``call_names`` is the
    per-request call_id->name map used to recover functionResponse names;
    ``wire_for(namespace, name)`` gives the declared wire name of a namespaced tool."""
    wire_for = wire_for or (lambda namespace, name: _wire_name(namespace, name))
    t = item.get("type")
    if t == "message":
        role = "model" if item.get("role") == "assistant" else "user"
        for p in _parts_from_message(item.get("content")):
            _append(contents, role, p)
    elif t in ("function_call", "custom_tool_call", "local_shell_call"):
        call_id = item.get("call_id") or item.get("id") or ""
        name = item.get("name") or ("local_shell" if t == "local_shell_call" else "tool")
        if t != "local_shell_call":
            name = wire_for(item.get("namespace"), name)
        raw = (item.get("input") if t == "custom_tool_call"
               else item.get("action") if t == "local_shell_call"
               else item.get("arguments"))
        call_names[call_id] = name
        fc = {"name": name, "args": _args_object(raw)}
        if call_id:
            fc["id"] = call_id
        part = {"functionCall": fc}
        # Thinking models 400 without a signature. Use the real one if we captured
        # it from a prior Gemini turn; otherwise the accepted placeholder sentinel
        # (cross-provider switch / evicted sig) so Gemini 3.x doesn't reject it.
        part["thoughtSignature"] = _recall_signature(call_id) or _PLACEHOLDER_SIG
        _append(contents, "model", part)
    elif t in ("function_call_output", "custom_tool_call_output"):
        call_id = item.get("call_id") or ""
        name = call_names.get(call_id, "tool")   # Gemini functionResponse REQUIRES a name
        fr = {"name": name, "response": _response_object(item.get("output"))}
        if call_id:
            fr["id"] = call_id
        _append(contents, "user", {"functionResponse": fr})
    elif t in ("tool_search_call",):
        call_id = item.get("call_id") or item.get("id") or ""
        call_names[call_id] = "tool_search"
        _append(contents, "model", {"functionCall": {
            "name": "tool_search", "args": _args_object(item.get("arguments")),
            **({"id": call_id} if call_id else {})},
            "thoughtSignature": _recall_signature(call_id) or _PLACEHOLDER_SIG})
    elif t in ("tool_search_output", "mcp_tool_call_output"):
        call_id = item.get("call_id") or ""
        _append(contents, "user", {"functionResponse": {
            "name": call_names.get(call_id, "tool"),
            "response": {"output": (_render_search_output(item, wire_for)
                                    if t == "tool_search_output"
                                    else json.dumps(item.get("output", "")))},
            **({"id": call_id} if call_id else {})}})
    elif t in ("web_search_call", "image_generation_call"):
        _append(contents, "model", {"text": _server_tool_text(t, item)})
    elif t == "agent_message":
        text = "".join(c.get("text", "") for c in (item.get("content") or [])
                       if isinstance(c, dict) and c.get("type") == "input_text")
        if text:
            _append(contents, "model", {"text": text})
    elif t in ("compaction", "context_compaction"):
        _append(contents, "user",
                {"text": "[Earlier conversation was summarized/compacted to save context.]"})
    elif t in ("reasoning", "compaction_trigger", "additional_tools"):
        return  # deliberate drops (reasoning off; markers)
    else:
        raise UnsupportedItemError(f"Responses item type {t!r} is not yet supported on Gemini")


def _server_tool_text(t, item):
    if t == "web_search_call":
        act = item.get("action") or {}
        return f"[web search performed: {act.get('query') or ' '.join(act.get('queries') or [])}]"
    return "[image generation performed]"


# Gemini's function-declaration `parameters` follows a SUBSET of OpenAPI 3.0 Schema,
# not full JSON Schema. It hard-rejects (400) JSON-Schema-only keywords that OpenAI /
# Anthropic accept — most notably `additionalProperties` (Codex sets it to `false` for
# strict mode). Strip those keywords recursively, at every nesting level.
_SCHEMA_DROP_KEYS = frozenset({
    "additionalProperties", "unevaluatedProperties", "patternProperties",
    "additionalItems", "$schema", "$id", "$ref", "$defs", "$comment",
    "definitions", "strict",
})


def _sanitize_schema(node):
    """Recursively make a JSON Schema acceptable to Gemini: drop the keywords it rejects (above)
    AND rewrite the constructs it 400s on. One bad tool schema fails the WHOLE request, so a
    single odd MCP tool would otherwise take down every Gemini turn of the session.

    PROBED LIVE 2026-10-19 on gemini-3.5-flash-lite. Rejected: `type` as a list, `const`,
    `enum` with non-string values, `items` as a list, a boolean schema, `required` naming a
    property that is not defined (also nested). Accepted as is: anyOf / oneOf / allOf (with a
    `{"type":"null"}` branch, which is how Pydantic spells Optional), default, title, format,
    min/max/pattern, an object with no properties, `$ref` (dropped here)."""
    if node is True or node is False:
        return {}                                   # boolean schema: "anything" (a schema position only)
    if not isinstance(node, dict):
        return node
    out = {}
    for k, v in node.items():
        if k in _SCHEMA_DROP_KEYS:
            continue
        if k == "properties" and isinstance(v, dict):
            out[k] = {pk: _sanitize_schema(pv) for pk, pv in v.items()}
        elif k in ("anyOf", "oneOf", "allOf") and isinstance(v, list):
            out[k] = [_sanitize_schema(x) for x in v]
        elif k == "items":
            out[k] = _sanitize_schema(v[0] if v else {}) if isinstance(v, list) else _sanitize_schema(v)
        else:
            out[k] = copy.deepcopy(v)
    note = []
    t = out.get("type")
    if isinstance(t, list):
        types = [x for x in t if x != "null"]
        if len(types) == 1:
            out["type"] = types[0]
            if "null" in t:
                out["nullable"] = True
        elif types:
            del out["type"]
            out["anyOf"] = [{"type": x} for x in types] + ([{"type": "null"}] if "null" in t else [])
        else:
            out["type"] = "null"
    if "const" in out:
        c = out.pop("const")
        if isinstance(c, str):
            out["enum"] = [c]
        else:
            note.append("Must be exactly %s." % json.dumps(c))
    en = out.get("enum")
    if isinstance(en, list) and any(not isinstance(x, str) for x in en):
        del out["enum"]
        note.append("Allowed values: %s." % ", ".join(json.dumps(x) for x in en))
    props = out.get("properties")
    if isinstance(props, dict):
        out.setdefault("type", "object")             # `properties` is only legal on an object
    req = out.get("required")
    if isinstance(req, list):
        keep = [r for r in req if isinstance(props, dict) and r in props]
        if keep:
            out["required"] = keep
        else:
            del out["required"]
    if note:
        out["description"] = " ".join(filter(None, [out.get("description"), " ".join(note)]))
    return out


def _dropped_schema_keys(node, found=None):
    """Which `_SCHEMA_DROP_KEYS` `_sanitize_schema` would remove from this tree.

    Only used to WARN. Dropping `$ref`/`$defs` from a user-authored output schema
    silently flattens a reference into `{}` (an "anything goes" node), so a schema
    that leans on them is honoured only in part -- say so rather than pretend."""
    found = set() if found is None else found
    if isinstance(node, dict):
        for k, v in node.items():
            if k in _SCHEMA_DROP_KEYS:
                found.add(k)
            else:
                _dropped_schema_keys(v, found)
    elif isinstance(node, list):
        for v in node:
            _dropped_schema_keys(v, found)
    return found


def output_schema_of(body):
    """The engine's ``--output-schema``, or None.

    Wire shape, from `codex-api::create_text_param_for_request`
    (`core/src/client.rs` -> the Responses `text` param):

        "text": {"format": {"type": "json_schema",
                            "strict": <bool>,          # defaults true engine-side
                            "schema": {...},           # the user's JSON Schema
                            "name": "codex_output_schema"}}

    Returns ``(schema, strict)``, or ``(None, False)`` when the caller asked for
    no structured output. A malformed `text.format` returns ``(None, False)`` and
    is reported by the caller -- never silently swallowed."""
    text = body.get("text")
    if not isinstance(text, dict):
        return None, False
    fmt = text.get("format")
    if not isinstance(fmt, dict):
        return None, False
    if fmt.get("type") != "json_schema" or not isinstance(fmt.get("schema"), dict):
        _warn("ignored an unusable text.format (type=%r, schema=%s) -- "
              "structured output was NOT applied"
              % (fmt.get("type"), type(fmt.get("schema")).__name__))
        return None, False
    return fmt["schema"], bool(fmt.get("strict", True))


def _freeform_description(sp):
    """A freeform tool (apply_patch) reaches the model as a function taking ONE string `input`.
    Its own description says "FREEFORM ... do not wrap the patch in JSON", which contradicts
    that, and the grammar lives in `format.definition`, which function calling cannot carry.
    So say how to call it and fold the grammar in (the same text openai_compat has always
    added). Without it Gemini flash-lite needed four tries to produce a valid patch."""
    fmt = sp["format"] if isinstance(sp["format"], dict) else {}
    definition = fmt.get("definition") or fmt.get("syntax") or ""
    desc = (sp["description"] + " Pass the ENTIRE raw tool input as the single string "
            "argument `input` \u2014 no other keys.")
    if isinstance(definition, str) and definition.strip():
        desc += "\nInput syntax:\n" + definition[:2000]
    return desc


def _translate_tools(specs):
    """Declared tool specs (see ``_collect_tools``) -> a single Gemini functionDeclarations
    list. Freeform ('custom') tools take one string `input` (the model's call is returned as a
    custom_tool_call, see ``_call_item``). OpenAI-hosted tools have no analog -> not advertised."""
    decls = []
    for sp in specs:
        if sp["custom"]:
            params = {"type": "object", "properties": {"input": {"type": "string"}},
                      "required": ["input"]}
        else:
            params = _sanitize_schema(sp["parameters"] or {"type": "object", "properties": {}})
        decls.append({"name": sp["wire"],
                      "description": _freeform_description(sp) if sp["custom"] else sp["description"],
                      "parameters": params})
    return decls


def _repair_tool_pairs(contents):
    """Gemini (esp. 3.x thinking models) rejects a model ``functionCall`` that isn't
    answered by a ``functionResponse`` in the next user turn. A dangling functionCall
    happens in real sessions when a tool call is interrupted/cancelled before its output,
    so the Responses history carries the function_call with no function_call_output.
    Synthesize a placeholder functionResponse (matched by id, else name) for any unanswered
    functionCall so the request stays valid, and hoist functionResponses to the front of the
    user turn."""
    i = 0
    while i < len(contents):
        c = contents[i]
        if c.get("role") == "model":
            calls = [p["functionCall"] for p in c.get("parts", [])
                     if isinstance(p, dict) and isinstance(p.get("functionCall"), dict)]
            if calls:
                nxt = contents[i + 1] if i + 1 < len(contents) else None
                if not (nxt and nxt.get("role") == "user"):
                    nxt = {"role": "user", "parts": []}
                    contents.insert(i + 1, nxt)
                frs = [p["functionResponse"] for p in nxt["parts"]
                       if isinstance(p, dict) and isinstance(p.get("functionResponse"), dict)]
                have_ids = {fr.get("id") for fr in frs if fr.get("id")}
                have_names = {fr.get("name") for fr in frs}
                synth = []
                for fc in calls:
                    cid = fc.get("id"); nm = fc.get("name") or "tool"
                    answered = (cid in have_ids) if cid else (nm in have_names)
                    if not answered:
                        fr = {"name": nm, "response": {"output": "[no tool result: the call did not complete]"}}
                        if cid:
                            fr["id"] = cid
                        synth.append({"functionResponse": fr})
                responses = [p for p in nxt["parts"] if isinstance(p, dict) and "functionResponse" in p]
                others = [p for p in nxt["parts"] if not (isinstance(p, dict) and "functionResponse" in p)]
                nxt["parts"] = synth + responses + others
        i += 1
    return contents


def translate_request(body):
    """Responses request dict -> Gemini generateContent request dict."""
    model = body.get("model")
    out = _Request()

    instructions = body.get("instructions")
    if isinstance(instructions, str) and instructions.strip():
        out["systemInstruction"] = {"parts": [{"text": instructions}]}

    specs, name_map, wire_for = _collect_tools(body)
    contents = []
    call_names = {}
    for item in body.get("input") or []:
        if isinstance(item, dict):
            _translate_item(item, contents, call_names, wire_for)
    _repair_tool_pairs(contents)   # pair every functionCall with a functionResponse (dangling -> synth)
    if not contents or contents[0]["role"] != "user":
        contents.insert(0, {"role": "user", "parts": [{"text": "(continue)"}]})
    out["contents"] = contents
    out.name_map = name_map

    decls = _translate_tools(specs)
    if decls:
        out["tools"] = [{"functionDeclarations": decls}]
        out["toolConfig"] = {"functionCallingConfig": {"mode": "AUTO"}}

    out["generationConfig"] = {"maxOutputTokens": MAX_OUTPUT_TOKENS.get(model, MAX_OUTPUT_DEFAULT)}

    # `--output-schema` -> constrained decoding. PROBED LIVE 2026-07-27 on
    # gemini-3.6-flash: `responseSchema` coexists with `tools` (200), and with a
    # tool-forcing prompt the model STILL returns a `functionCall` rather than
    # schema-shaped JSON -- so this does not break the agentic tool loop; the
    # schema binds the final text turn. `responseMimeType` is not strictly
    # required (a bare responseSchema also returns JSON) but we send it to make
    # the intent explicit. Gemini's schema dialect is an OpenAPI 3.0 subset, so
    # it 400s on `additionalProperties` -- reuse the tool-schema sanitizer.
    schema, _strict = output_schema_of(body)
    if schema is not None:
        dropped = _dropped_schema_keys(schema)
        if dropped:
            _warn("output_schema: dropped Gemini-unsupported keyword(s) %s; "
                  "the schema is applied WITHOUT them"
                  % ", ".join(sorted(dropped)))
        out["generationConfig"]["responseMimeType"] = "application/json"
        out["generationConfig"]["responseSchema"] = _sanitize_schema(schema)
    # Reasoning capture (opt-in, BYTEASK_REASONING_CAPTURE=1): ask Gemini to
    # RETURN its thought summary (the model already thinks -- 2.5+ bill thinking
    # tokens regardless -- this just surfaces a summary). The Transducer buffers
    # those thought parts into output_items and does NOT stream them to the
    # engine, so there is no user-facing behavior change (only a marginal summary-
    # token cost). Default off -> thinkingConfig omitted, exactly as before.
    #
    # ⚠️ BOTH of these write into the SAME `thinkingConfig` dict. Assigning a
    # fresh dict for either one silently deletes the other -- and deleting
    # `includeThoughts` would stop the thought summary from being returned,
    # with no error and no failing test. Build the dict once, then merge.
    thinking_config = {}
    if _REASONING_CAPTURE:
        thinking_config["includeThoughts"] = True

    # Reasoning effort -> thinkingLevel. PROBED LIVE 2026-07-25: minimal/low/
    # medium/high are accepted on gemini-3.6-flash, 3.5-flash-lite and
    # 3.1-pro-preview; none/off/max/dynamic are a 400. There is no xhigh/max --
    # Gemini tops out at `high`, so the top engine rungs collapse onto it.
    # NB `thinkingBudget: 0` is ALSO a 400 on the 3.x flash models, so thinking
    # cannot be switched off here at all; `minimal` is the floor.
    level = _effort.clamp_effort(body, EFFORT_MAP)
    if level:
        thinking_config["thinkingLevel"] = level

    if thinking_config:
        out["generationConfig"]["thinkingConfig"] = thinking_config
    return out


# ==== a reply the model's output limit cut off ============================= #
# ⚠️ This block is deliberately duplicated in anthropic_translate.py, gemini_translate.py and
# openai_compat_translate.py (each helper file is installed on its own; there is no shared module).
# A model that stops at its output limit (Anthropic stop_reason max_tokens, Gemini finishReason
# MAX_TOKENS, Chat Completions finish_reason length, Responses response.incomplete max_output_tokens)
# has NOT finished the agent's turn. Reporting it as finished stopped the agent mid-task (users typed
# "continue" over and over), and a tool call cut off mid-arguments went out with no arguments at all.
# Now a cut-off tool call is dropped (it would only fail and be repeated), one assistant note says the
# reply was cut off (the user sees it; the model reads it and splits its work), and the response
# completes with end_turn false, so the engine sends the next request at once. After
# CUTOFF_MAX_IN_A_ROW cut-offs in a row (counted from these notes in the request's input since the
# user's last message) the note says so and the turn ends: a model that keeps hitting the limit
# cannot loop on the user's money.
CUTOFF_MARK = "[Output limit reached"
CUTOFF_MAX_IN_A_ROW = 3


def _item_text(item):
    content = item.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text") or "" for p in content if isinstance(p, dict))
    return ""


def prior_cutoffs(body):
    """How many output-limit notes the request's input holds since the user's last message."""
    items = body.get("input") if isinstance(body, dict) else None
    if not isinstance(items, list):
        return 0
    n = 0
    for item in reversed(items):
        if not isinstance(item, dict) or item.get("type", "message") != "message":
            continue
        if item.get("role") == "user":
            break
        if item.get("role") == "assistant" and _item_text(item).lstrip().startswith(CUTOFF_MARK):
            n += 1
    return n


def cutoff_note(prior):
    """(the note's text, whether the turn goes on) for a reply cut off after `prior` cut-offs in a row."""
    if prior + 1 >= CUTOFF_MAX_IN_A_ROW:
        return (CUTOFF_MARK + " %d times in a row, so I stopped here. Say \"continue\" to go on; large edits "
                "and long outputs go in smaller steps.]" % (prior + 1)), False
    return (CUTOFF_MARK + ": my reply was cut off, so I am continuing. I will split large edits and long "
            "outputs into smaller steps.]"), True


def cutoff_item(text):
    return {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}


def _cutoff_frames(text):
    """The note as Responses stream events: added, its text, done."""
    return [_frame("response.output_item.added",
                   {"item": {"type": "message", "role": "assistant", "content": []}}),
            _frame("response.output_text.delta", {"delta": text}),
            _frame("response.output_item.done", {"item": cutoff_item(text)})]


def _args_complete(args):
    """Whether a tool call's arguments are whole JSON (a call the output limit cut off has partial JSON)."""
    if not isinstance(args, str) or not args.strip():
        return True
    try:
        json.loads(args)
        return True
    except ValueError:
        return False


# ==== response: Gemini SSE -> Responses SSE ================================ #
def _frame(event_type, payload):
    payload = dict(payload, type=event_type)
    return f"event: {event_type}\ndata: {json.dumps(payload)}\n\n"


class Transducer:
    """Stateful Gemini-SSE -> Responses-SSE transducer. Feed parsed Gemini chunks
    (GenerateContentResponse dicts); guarantees a terminal so the engine never hangs."""

    def __init__(self, model, name_map=None, prior_cutoffs=0):
        self.prior_cutoffs = prior_cutoffs  # output-limit cut-offs in a row before this request (prior_cutoffs())
        self.model = model
        self.name_map = name_map  # NameMap from translate_request: wire name -> engine (namespace, name)
        self.response_id = "resp_gemini"
        self.created = False
        self.text_open = False
        self.text_buf = ""
        self.usage = {"input": 0, "cached": 0, "output": 0}
        self.output_items = []    # every completed output_item.done payload, in order
        self.finish_reason = None
        self.had_tool_use = False
        self.produced = False
        self.done = False
        self._fn = 0
        self.thought_buf = ""     # Gemini "thought" summary text, buffered for
        #                           output_items (reasoning), never streamed.

    def _created(self):
        self.created = True
        return _frame("response.created", {"response": {"id": self.response_id}})

    def _flush_thought(self):
        """Append accumulated thought text to output_items as a reasoning item (not
        streamed to the engine). Call before finishing the response."""
        if self.thought_buf:
            self.output_items.append({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": self.thought_buf}]})
            self.thought_buf = ""

    def _close_text(self):
        if not self.text_open:
            return []
        self.text_open = False
        item = {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": self.text_buf}]}
        self.text_buf = ""
        self.output_items.append(item)
        return [_frame("response.output_item.done", {"item": item})]

    def _completed(self, end_turn=None):
        self.done = True
        total_in = self.usage["input"]           # promptTokenCount already includes cached
        usage = {
            "input_tokens": total_in,
            "input_tokens_details": {"cached_tokens": self.usage["cached"]},
            "output_tokens": self.usage["output"],
            "total_tokens": total_in + self.usage["output"],
            "cache_write_tokens": 0,             # Gemini implicit caching has no per-token write
        }
        return _frame("response.completed", {"response": {
            "id": self.response_id, "usage": usage,
            "end_turn": (not self.had_tool_use) if end_turn is None else end_turn}})

    def _failed(self, code, message):
        self.done = True
        return _frame("response.failed", {"response": {
            "id": self.response_id, "status": "failed",
            "error": {"code": code, "message": message}}})

    def feed(self, chunk):
        frames = []
        if not self.created:
            frames.append(self._created())
        # A pre-stream / mid-stream error object.
        if isinstance(chunk.get("error"), dict):
            err = chunk["error"]
            frames += self._close_text()
            frames.append(self._failed(str(err.get("status") or err.get("code") or "error"),
                                       err.get("message") or "gemini error"))
            return frames
        for cand in chunk.get("candidates") or []:
            for part in (cand.get("content") or {}).get("parts") or []:
                if not isinstance(part, dict):
                    continue
                if part.get("thought"):
                    if isinstance(part.get("text"), str):
                        self.thought_buf += part["text"]  # buffer for output_items, don't stream
                    continue
                if isinstance(part.get("text"), str):
                    self.produced = True
                    if not self.text_open:
                        self.text_open = True
                        frames.append(_frame("response.output_item.added",
                                             {"item": {"type": "message", "role": "assistant",
                                                       "content": []}}))
                    self.text_buf += part["text"]
                    frames.append(_frame("response.output_text.delta", {"delta": part["text"]}))
                elif isinstance(part.get("functionCall"), dict):
                    self.produced = True
                    self.had_tool_use = True
                    frames += self._close_text()   # close any open text item first
                    fc = part["functionCall"]
                    self._fn += 1
                    # Always a fresh uuid call_id (never Gemini's fc["id"], which isn't unique
                    # across responses) so the persisted signature is keyed to exactly this call
                    # and can't collide with another conversation's — even across a restart.
                    call_id = _new_call_id(fc.get("name"))
                    if part.get("thoughtSignature"):
                        _remember_signature(call_id, part["thoughtSignature"])
                    fc_item = _call_item(self.name_map, fc.get("name", ""),
                                         json.dumps(fc.get("args") or {}), call_id)
                    self.output_items.append(fc_item)
                    frames.append(_frame("response.output_item.done", {"item": fc_item}))
            if cand.get("finishReason"):
                self.finish_reason = cand["finishReason"]
                self._flush_thought()
        self._capture_usage(chunk.get("usageMetadata"))
        return frames

    def _capture_usage(self, u):
        """Gemini usageMetadata is CUMULATIVE — the last chunk's value wins."""
        if not isinstance(u, dict):
            return
        if isinstance(u.get("promptTokenCount"), (int, float)):
            self.usage["input"] = int(u["promptTokenCount"])
        if isinstance(u.get("cachedContentTokenCount"), (int, float)):
            self.usage["cached"] = int(u["cachedContentTokenCount"])
        # thinking tokens are billed as output
        out = 0
        for k in ("candidatesTokenCount", "thoughtsTokenCount"):
            if isinstance(u.get(k), (int, float)):
                out += int(u[k])
        if out:
            self.usage["output"] = out

    def close(self):
        if self.done:
            return []
        frames = []
        if not self.created:
            frames.append(self._created())
        self._flush_thought()
        frames += self._close_text()
        if self.finish_reason == "MAX_TOKENS":
            text, keep_going = cutoff_note(self.prior_cutoffs)  # cut off by the output limit: say so, go on
            frames += _cutoff_frames(text)
            self.output_items.append(cutoff_item(text))
            frames.append(self._completed(end_turn=not keep_going))
        elif self.finish_reason or self.produced:
            frames.append(self._completed())
        else:
            frames.append(self._failed("stream_interrupted",
                                        "gemini stream ended before any content"))
        return frames


# ==== SSE parsing (Gemini uses `data: {json}` frames, like Anthropic) ======= #
def iter_sse(byte_iter):
    buf = b""
    for chunk in byte_iter:
        if not chunk:
            continue
        # Gemini terminates SSE frames with CRLF (`\r\n\r\n`), not LF (`\n\n`). Normalize
        # so the blank-line frame split fires per-frame. WITHOUT this, multi-frame
        # responses never split, pile into the trailing flush, get concatenated into one
        # bad `json.loads`, and yield ZERO events -> "gemini stream ended before any
        # content". (Re-normalizing the whole buffer also heals a `\r\n` straddling a
        # read boundary; the buffer is small since consumed frames are removed.)
        buf = (buf + chunk).replace(b"\r\n", b"\n")
        while b"\n\n" in buf:
            raw, buf = buf.split(b"\n\n", 1)
            data = _sse_data(raw)
            if data is not None:
                yield data
    data = _sse_data(buf)
    if data is not None:
        yield data


def _sse_data(raw):
    parts = [ln[5:].strip() for ln in raw.split(b"\n") if ln.strip().startswith(b"data:")]
    if not parts:
        return None
    try:
        return json.loads(b" ".join(parts))
    except Exception:
        return None
