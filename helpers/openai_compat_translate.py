#!/usr/bin/env python3
"""Responses API <-> OpenAI Chat Completions translation, for self-hosted /
custom OpenAI-compatible model servers (vLLM / TGI / SGLang / Ollama / llama.cpp
/ LiteLLM). Stdlib only. Mirrors anthropic_translate / gemini_translate.

The ByteAsk engine speaks ONLY the Responses API; a chat-completions-only server
speaks `/v1/chat/completions`. This module bridges the two:

  translate_request(body, model_id) -> (chat_body, name_map)
      Responses request dict -> Chat Completions request dict, plus the reverse
      tool-name map (sanitized -> original) the Transducer needs to un-mangle
      streamed tool calls.
  Transducer(model, name_map) : feed(chunk) / close() -> Responses SSE frames
      Chat `chat.completion.chunk` (or a full `chat.completion`) -> Responses SSE.
  iter_sse(byte_iter) : yields parsed chat chunks (skips the `[DONE]` sentinel).

Design notes:
  * Chat Completions is the CLOSEST ancestor of Responses, but this is agent-
    protocol EMULATION, not a trivial wire map: streamed tool_call deltas, usage
    synth, dangling-tool repair, and per-server quirks all live here.
  * DEGRADE, don't 400, on unknown history items. When a user switches
    gpt/claude/gemini -> a self/* model mid-session, the resent Responses history
    carries items an open model never produced (tool_search_call, web_search_call,
    additional_tools, compaction, reasoning...). The other translators hard-fail on
    unknown items; here that would kill the session, so we turn them into benign
    history text instead. Only genuinely malformed input raises.
  * Tool names are sanitized to the Chat function-name charset and REVERSED on the
    way back (an MCP/custom name like `mcp__srv__do.thing` must round-trip).

Public API:
  translate_request, Transducer, iter_sse, sanitize_tool_name, UnsupportedItemError
"""
from __future__ import annotations

import copy
import hashlib
import json
import re
import sys


def _warn(msg):
    """One-line stderr note. Runs under the gateway (journalctl, Fireworks path)
    AND under the user's byok sidecar (every `self/*` endpoint), so keep it short
    and never raise."""
    try:
        sys.stderr.write(f"[compat] {msg}\n")
    except Exception:
        pass

# Chat Completions function names must match ^[a-zA-Z0-9_-]{1,64}$ on most servers.
_NAME_OK = re.compile(r"^[a-zA-Z0-9_-]{1,64}$")

# JSON-Schema-only keywords that OpenAI-compatible servers (esp. strict vLLM tool
# parsers) may reject in a function `parameters` schema. Dropped recursively.
_SCHEMA_DROP_KEYS = frozenset({
    "additionalProperties", "unevaluatedProperties", "patternProperties",
    "additionalItems", "$schema", "$id", "$ref", "$defs", "$comment",
    "definitions", "strict",
})

# --------------------------------------------------------------------------- #
# text-leaked tool calls (models that never use the structured channel)        #
# --------------------------------------------------------------------------- #
# Observed in the wild (qwen2.5-coder:3b via Ollama, Qwen2.5-Coder-7B via vLLM):
# instead of a structured `tool_calls` delta, the model emits its call as plain
# CONTENT — a fenced ```json block, a `<tool_call>`/`<tool>`/`<xml>`/`<response>`
# tag wrapper, Mistral's `[TOOL_CALLS] [...]`, or a bare JSON object. The
# Transducer holds such blocks out of the text stream and recovers them into
# real calls, gated on the DECLARED tool names — so a hallucinated tool stays
# visible text, never a bogus call. `<think>...</think>` reasoning blocks
# (DeepSeek-R1 style) are stripped from the stream entirely.
_MARKER_RE = re.compile(
    r"</?\s*(think|tool_call|toolcall|tool|function_call|functioncall|function"
    r"|xml|response|invoke)\b|\[TOOL_CALLS\]",
    re.I)
_THINK_CLOSE_RE = re.compile(r"</\s*think\s*>", re.I)
_LEAK_MARKERS = ("<think>", "<tool_call>", "<toolcall>", "<tool>",
                 "<function_call>", "<functioncall>", "<function>", "<xml>",
                 "<response>", "<invoke>", "[TOOL_CALLS]", "```")
_HOLD_MAX = 262144        # a held block larger than this flushes back to text


def _partial_marker_len(s):
    """Longest suffix of ``s`` that could still grow into a leak marker (so a
    `<tool_call>` tag split across stream chunks is never half-emitted)."""
    tail = s[-14:].lower()
    for i in range(len(tail)):
        suf = tail[i:]
        for mk in _LEAK_MARKERS:
            if len(suf) < len(mk) and mk.startswith(suf):
                return len(tail) - i
    return 0


def _extract_json_values(s):
    """All top-level JSON values embedded in ``s`` (tolerates surrounding prose)."""
    vals, dec, i, n = [], json.JSONDecoder(), 0, len(s)
    while i < n:
        if s[i] in "{[":
            try:
                v, end = dec.raw_decode(s, i)
                vals.append(v)
                i = end
                continue
            except Exception:
                pass
        i += 1
    return vals


def _strip_fence(s):
    """Drop a ```lang ... ``` wrapper around ``s``, if present."""
    s = (s or "").strip()
    if s.startswith("```"):
        nl = s.find("\n")
        if nl != -1:
            s = s[nl + 1:]
        if s.rstrip().endswith("```"):
            s = s.rstrip()[:-3]
    return s.strip()


def _gate_name(name, gate):
    """Canonical declared name for ``name`` (case-insensitive), or None if the
    name isn't declared (gate=None accepts anything)."""
    if not isinstance(name, str) or not name:
        return None
    if gate is None or name in gate:
        return name
    return {g.lower(): g for g in gate}.get(name.lower())


# Structural XML tags that are wrappers, never argument names.
_XML_STRUCTURAL = frozenset({
    "name", "functioncall", "function_call", "function", "tool", "tool_call",
    "toolcall", "xml", "response", "invoke", "arguments", "parameters", "args",
    "parameter", "input",
})


def _parse_xmlish_calls(text, gate=None):
    """Second-chance parser: some models leak PURE-XML calls with no JSON at all
    (observed live: ```xml <FunctionCall><name>get_time</name>...). Handles
    <name>X</name>, <invoke name="X">, per-arg <parameter name="k">v</parameter>
    and simple <key>value</key> argument elements."""
    out = []
    # every named call site: <name>X</name> or a tool-ish element with name="X"
    sites = [(m.start(), m.group(1)) for m in
             re.finditer(r"<name>\s*([^<\s]+?)\s*</name>", text, re.I)]
    sites += [(m.start(), m.group(1)) for m in
              re.finditer(r"<(?:invoke|function(?:_?call)?|tool(?:_?call)?)\b[^>]*?"
                          r"\bname=\"([^\"]+)\"", text, re.I)]
    sites.sort()
    if not sites and gate:
        # the element TAG is the tool name itself: <get_time/> ...
        for g in sorted(gate):
            m = re.search(r"<%s\b" % re.escape(g), text)
            if m:
                sites.append((m.start(), g))
    for i, (pos, raw_name) in enumerate(sites):
        name = _gate_name(raw_name, gate)
        if not name:
            continue
        scope = text[pos:sites[i + 1][0]] if i + 1 < len(sites) else text[pos:]
        args = {}
        am = re.search(r"<(arguments|parameters|args|input)>([\s\S]*?)</\1>", scope, re.I)
        if am:
            inner = am.group(2).strip()
            if inner:
                try:
                    v = json.loads(inner)
                    args = v if isinstance(v, dict) else {"input": inner}
                except Exception:
                    kv = re.findall(r"<(\w+)>([\s\S]*?)</\1>", inner)
                    args = dict(kv) if kv else {"input": inner}
        else:
            kv = re.findall(r"<parameter\s+name=\"(\w+)\">([\s\S]*?)</parameter>",
                            scope, re.I)
            if kv:
                args = dict(kv)
            else:
                kv = re.findall(r"<(\w+)>([^<]*)</\1>", scope)
                args = {k: v for k, v in kv if k.lower() not in _XML_STRUCTURAL}
        out.append((name, json.dumps(args)))
    return out


def _parse_leaked_calls(text, gate=None):
    """Parse tool calls a model leaked as plain text -> [(name, args_json_str)].
    ``gate`` is the declared tool-name set (None accepts any name); a name not in
    the gate — even case-insensitively — is rejected, so hallucinated tools stay
    text. Accepts {"name", "arguments"|"parameters"|"args"|"input"} shapes plus
    the nested OpenAI {"function": {...}} form, single objects or arrays; falls
    back to a pure-XML parser for models that leak XML-structured calls."""
    text = _strip_fence(text)                # tolerate a markdown fence wrapper
    out = []
    for v in _extract_json_values(text):
        for d in (v if isinstance(v, list) else [v]):
            if not isinstance(d, dict):
                continue
            fn = d.get("function") if isinstance(d.get("function"), dict) else {}
            raw_name = d.get("name") or d.get("tool") or d.get("tool_name") or fn.get("name")
            name = _gate_name(raw_name, gate)
            if not name:
                continue
            raw = None
            for k in ("arguments", "parameters", "args", "input"):
                if k in d:
                    raw = d[k]
                    break
                if k in fn:
                    raw = fn[k]
                    break
            out.append((name, _tool_args(raw)))
    if not out:
        out = _parse_xmlish_calls(text, gate)
    return out


# Keys a leaked call envelope may carry beside the name. A block holding ONLY
# these, with no usable name, is a degenerate call rather than prose.
_ENVELOPE_KEYS = frozenset({"arguments", "parameters", "args", "input",
                            "function", "type", "id", "index"})


def _is_degenerate_call(text):
    """True when ``text`` is EXACTLY a tool-call envelope whose name is missing or
    empty — e.g. ``{"name": "", "arguments": {}}``. Small models that cannot follow
    their own tool-call template emit this shape for ordinary prompts (observed
    live: qwen2.5-coder:3b answering a plain "hi" with tools attached). It is not
    dispatchable (no name to call) and not prose, so the caller drops it instead
    of printing raw JSON at the user.

    Deliberately narrow, so a legitimate JSON answer is never swallowed: a real
    but undeclared name (a hallucinated tool) is NOT degenerate and stays visible
    text by design, and anything that is not strict JSON, or that carries a key
    outside the envelope set, is left alone."""
    text = _strip_fence(text)
    if not text or text[0] not in "{[":
        return False
    try:
        v = json.loads(text)
    except Exception:
        return False
    items = v if isinstance(v, list) else [v]
    if not items:
        return False
    for d in items:
        if not isinstance(d, dict) or "name" not in d:
            return False
        nm = d.get("name")
        if isinstance(nm, str) and nm.strip():
            return False                     # a usable name: not degenerate
        if set(d) - {"name"} - _ENVELOPE_KEYS:
            return False                     # carries real content: it's prose
    return True


class UnsupportedItemError(Exception):
    """Raised only for genuinely malformed input — NOT for unknown history item
    types (those are degraded to text so a mid-session model switch never 400s)."""


# --------------------------------------------------------------------------- #
# tool-name mapping (reversible)                                              #
# --------------------------------------------------------------------------- #
def sanitize_tool_name(name):
    """A Chat-safe function name. Deterministic AND collision-resistant: a valid
    name is returned unchanged; an invalid one is sanitized and suffixed with a
    short hash of the ORIGINAL, so two distinct names that would otherwise
    sanitize to the same string (`a.b` and `a/b` -> `a_b`) — or truncate to it at
    64 chars — still get distinct, reversible safe names. Order-independent, so a
    tool declared in `tools[]` and its call in resent history map identically."""
    name = name or "tool"
    if _NAME_OK.match(name):
        return name
    base = (re.sub(r"[^a-zA-Z0-9_-]", "_", name)[:57].strip("_")) or "tool"
    h = hashlib.sha1(name.encode("utf-8")).hexdigest()[:6]
    return f"{base}_{h}"


# ==== tool names: namespaces, tool_search, deferred ("loaded") tools ======= #
# The engine defers every MCP tool and every sub-agent tool: they are NOT in tools[].
# The model finds them with the client-side `tool_search` tool and the engine answers with a
# `tool_search_output` INPUT item whose `tools` are `namespace` objects. OpenAI loads those
# server-side; here we declare them ourselves, as ordinary tools with provider-legal names,
# and map every call back to the engine's (namespace, name) PAIR -- the only thing the
# engine resolves a call by. (Captured from the real engine: tests/fixtures/.)
#
# ⚠️ This block is deliberately duplicated in anthropic_translate.py and gemini_translate.py.
# They are served to users as three separate files; a shared helper module would be a fourth
# file that every installer fetch list and nginx block must know about.
_DEFAULT_NAMESPACE = "functions"
_WIRE_NAME_BAD = re.compile(r"[^A-Za-z0-9_-]")
_WIRE_NAME_MAX = 64
TOOL_SEARCH_NAME = "tool_search"


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


def _wire_name(namespace, name, force_hash=False):
    """Chat-safe name for the engine tool (namespace, name). Deterministic. A plain tool keeps
    ``sanitize_tool_name``'s long-standing result; a namespaced one is joined the way the
    engine joins MCP names (namespace without trailing '_' + '__' + name) and run through the
    same sanitizer; ``force_hash`` is the collision escape hatch (hash of the ORIGINAL pair)."""
    name = name or "tool"
    ns = "" if (not namespace or namespace == _DEFAULT_NAMESPACE) else namespace
    raw = name if not ns else ns.rstrip("_") + "__" + name
    if not force_hash:
        return sanitize_tool_name(raw)
    digest = hashlib.sha1(("%s\0%s" % (ns, name)).encode("utf-8")).hexdigest()[:8]
    base = _WIRE_NAME_BAD.sub("_", raw)[:_WIRE_NAME_MAX - 9].rstrip("_") or "tool"
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


# --------------------------------------------------------------------------- #
# request: Responses -> Chat Completions                                      #
# --------------------------------------------------------------------------- #
_SCHEMA_COMBINATORS = ("anyOf", "oneOf", "allOf")


def _sanitize_schema(node):
    """Drop the keywords OpenAI-compatible tool parsers reject, and fix the shapes a real MCP
    server can publish that they 400 on. PROBED LIVE 2026-10-19 on Fireworks kimi-k3: `items` as
    a list, and `required` naming a property that is not defined (also nested). Walks only the
    places a sub-schema can sit (properties / items / anyOf-oneOf-allOf); every other value is
    copied as is. Returns a new tree."""
    if node is True or node is False:
        return {}                                    # a boolean schema: "anything"
    if not isinstance(node, dict):
        return node
    out = {}
    for k, v in node.items():
        if k in _SCHEMA_DROP_KEYS:
            continue
        if k == "properties" and isinstance(v, dict):
            out[k] = {pk: _sanitize_schema(pv) for pk, pv in v.items()}
        elif k in _SCHEMA_COMBINATORS and isinstance(v, list):
            out[k] = [_sanitize_schema(x) for x in v]
        elif k == "items":
            out[k] = _sanitize_schema(v[0] if v else {}) if isinstance(v, list) else _sanitize_schema(v)
        else:
            out[k] = copy.deepcopy(v)
    props = out.get("properties")
    if isinstance(out.get("required"), list) and isinstance(props, dict):
        out["required"] = [r for r in out["required"] if r in props]
    return out


# The engine always names the format this; keep it so a server that echoes the
# name back in an error message says something the user can search for.
OUTPUT_SCHEMA_NAME = "codex_output_schema"


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
    is reported -- never silently swallowed."""
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


def _content_parts(content):
    """Responses message content (list of typed parts) -> Chat content.

    Returns a plain string when it's all text (the common case, best server
    compatibility), else a list of {type:text|image_url} parts."""
    if isinstance(content, str):
        return content
    texts, parts, has_image = [], [], False
    for c in content or []:
        if not isinstance(c, dict):
            continue
        t = c.get("type")
        if t in ("input_text", "output_text", "text") and isinstance(c.get("text"), str):
            texts.append(c["text"])
            parts.append({"type": "text", "text": c["text"]})
        elif t in ("input_image", "image_url") and (c.get("image_url") or c.get("url")):
            has_image = True
            url = c.get("image_url") or c.get("url")
            if isinstance(url, dict):
                url = url.get("url")
            parts.append({"type": "image_url", "image_url": {"url": url}})
    if has_image:
        return parts
    return "".join(texts)


def _tool_args(raw):
    """Chat tool_calls carry `arguments` as a JSON STRING. Normalize whatever the
    Responses item gave us (string, dict, or freeform) into that."""
    if isinstance(raw, str):
        raw = raw.strip()
        if raw.startswith("{") or raw.startswith("["):
            return raw               # already JSON text
        return json.dumps({"input": raw}) if raw else "{}"
    if isinstance(raw, dict):
        return json.dumps(raw)
    return "{}"


def _degrade(item, messages):
    """Turn an unknown / server-tool / history-only Responses item into benign
    chat history text instead of raising (mid-session model switch survival)."""
    t = item.get("type")
    if t == "reasoning":
        return                                    # internal; drop silently
    if t in ("web_search_call", "image_generation_call"):
        act = item.get("action") or {}
        q = act.get("query") or " ".join(act.get("queries") or [])
        note = f"[{t.replace('_call','').replace('_',' ')} performed{': ' + q if q else ''}]"
        messages.append({"role": "assistant", "content": note})
        return
    if t in ("tool_search_call", "mcp_tool_call"):
        messages.append({"role": "assistant", "content": "[tool search performed]"})
        return
    if t in ("tool_search_output", "mcp_tool_call_output"):
        out = item.get("tools", item.get("output", ""))
        messages.append({"role": "user", "content": f"[tool search result: {json.dumps(out)[:400]}]"})
        return
    if t in ("compaction", "context_compaction"):
        messages.append({"role": "user",
                         "content": "[Earlier conversation was summarized to save context.]"})
        return
    if t in ("compaction_trigger", "additional_tools"):
        return                                    # markers; drop
    # Truly unknown: keep the session alive with a visible, harmless breadcrumb.
    messages.append({"role": "assistant", "content": f"[unsupported item {t!r} omitted]"})


def _translate_item(item, messages, name_map, wire_for=None):
    wire_for = wire_for or (lambda namespace, name: _wire_name(namespace, name))
    t = item.get("type")
    if t == "message":
        role = "assistant" if item.get("role") == "assistant" else "user"
        messages.append({"role": role, "content": _content_parts(item.get("content"))})
    elif t == "tool_search_call" and name_map.search is not None:
        # The request declares tool_search, so replay the call as a real one (the model
        # made it, or a model that could has been switched to this one mid-session).
        messages.append({
            "role": "assistant", "content": None,
            "tool_calls": [{"id": item.get("call_id") or item.get("id") or "", "type": "function",
                            "function": {"name": name_map.search,
                                         "arguments": _tool_args(item.get("arguments"))}}]})
    elif t == "tool_search_output" and name_map.search is not None:
        messages.append({"role": "tool", "tool_call_id": item.get("call_id") or "",
                         "content": _render_search_output(item, wire_for)})
    elif t in ("function_call", "custom_tool_call", "local_shell_call"):
        call_id = item.get("call_id") or item.get("id") or ""
        orig = item.get("name") or ("local_shell" if t == "local_shell_call" else "tool")
        ns = item.get("namespace") if t != "local_shell_call" else None
        safe = wire_for(ns, orig) if t != "local_shell_call" else sanitize_tool_name(orig)
        if safe != orig and safe not in name_map:
            name_map[safe] = orig
            if ns and ns != _DEFAULT_NAMESPACE:
                name_map.namespaces[safe] = ns
        raw = (item.get("input") if t == "custom_tool_call"
               else item.get("action") if t == "local_shell_call"
               else item.get("arguments"))
        messages.append({
            "role": "assistant", "content": None,
            "tool_calls": [{"id": call_id, "type": "function",
                            "function": {"name": safe, "arguments": _tool_args(raw)}}]})
    elif t in ("function_call_output", "custom_tool_call_output"):
        out = item.get("output")
        if not isinstance(out, str):
            out = json.dumps(out)
        messages.append({"role": "tool", "tool_call_id": item.get("call_id") or "",
                         "content": out})
    else:
        _degrade(item, messages)                  # NEVER raise on unknown history


def _is_custom_tool(tool):
    """Engine 'freeform'/custom (grammar) tools — e.g. apply_patch — carry no JSON
    parameters; they take one raw text input."""
    return (tool.get("type") in ("custom", "freeform")
            or ("format" in tool and "parameters" not in tool))


def custom_tool_names(tools):
    """Names of custom/freeform tools in a RESPONSES tools list. Pass to the
    Transducer so their calls come back as `custom_tool_call` items (the shape
    the engine expects for a tool it declared custom)."""
    return {t.get("name") for t in tools or []
            if isinstance(t, dict) and t.get("name") and _is_custom_tool(t)}


def _translate_tools(specs):
    out = []
    for sp in specs:
        desc = sp["description"]
        if sp["custom"]:
            # A freeform/grammar tool (apply_patch). Chat Completions has no
            # grammar concept, so expose it as a function taking ONE string
            # argument `input` and fold the grammar definition into the
            # description — this is what makes apply_patch callable at all on
            # chat-only servers.
            fmt = sp["format"] if isinstance(sp["format"], dict) else {}
            definition = fmt.get("definition") or fmt.get("syntax") or ""
            desc = (desc + " Pass the ENTIRE raw tool input as the single string "
                    "argument `input` — no other keys.")
            if isinstance(definition, str) and definition.strip():
                desc += "\nInput syntax:\n" + definition[:2000]
            params = {"type": "object",
                      "properties": {"input": {
                          "type": "string",
                          "description": "The complete raw text input for this tool."}},
                      "required": ["input"]}
        else:
            params = sp["parameters"] or {"type": "object", "properties": {}}
        out.append({"type": "function", "function": {
            "name": sp["wire"], "description": desc,
            "parameters": _sanitize_schema(params)}})
    return out


def _is_tool_call_only(m):
    """An assistant message that carries ONLY tool calls — the exact shape
    `_translate_item` emits for a `function_call` item (content is None)."""
    return (m.get("role") == "assistant" and m.get("content") is None
            and isinstance(m.get("tool_calls"), list) and m["tool_calls"])


def _merge_tool_call_runs(messages):
    """Collapse a run of consecutive tool-call-only assistant messages into ONE
    assistant message carrying every call.

    The Responses API models parallel tool calls as N separate `function_call`
    items, so `_translate_item` emits N separate assistant messages and the
    history reads:

        assistant(tool_calls=[A]) assistant(tool_calls=[B]) tool(A) tool(B)

    Chat Completions models the same turn as ONE assistant message whose
    `tool_calls` array holds both — which is what OpenAI itself emits, and
    therefore what strict servers are built to parse. The split shape leaves the
    two tool messages trailing an assistant block that declares only one call.
    Servers that resolve a tool message's name positionally against the
    immediately preceding block then cannot resolve the second one:
    Fireworks' Kimi K3 rejects it outright with *"tool messages need a
    resolvable tool name: carry `tool`/`name`, or match a preceding assistant
    tool_call by order."* (probed live 2026-08-05 — the split shape is the ONLY
    one of six shapes it refuses, and it is the only one of six Fireworks models
    that refuses it).

    Merging is enough on its own: once the calls share one block, resolution is
    by `tool_call_id` and the order of the tool messages no longer matters
    (verified — a reversed pair is accepted). So this does NOT also add a `name`
    field to tool messages, which would put a new key on every request to every
    user-owned `self/*` endpoint we cannot test.

    Only content-less tool-call messages are merged, so an assistant message
    with real text (including the `_degrade` breadcrumb) always breaks the run
    and is never folded into a tool-call block."""
    out = []
    for m in messages:
        if out and _is_tool_call_only(m) and _is_tool_call_only(out[-1]):
            merged = dict(out[-1])
            merged["tool_calls"] = list(merged["tool_calls"]) + list(m["tool_calls"])
            out[-1] = merged
        else:
            out.append(m)
    return out


def _repair_tool_pairs(messages):
    """Every assistant tool_call must be answered by a tool message before the next
    assistant/user turn, or chat servers 400. Synthesize a placeholder for any
    unanswered call (an interrupted/cancelled turn in the resent history)."""
    answered = {m.get("tool_call_id") for m in messages if m.get("role") == "tool"}
    out = []
    for i, m in enumerate(messages):
        out.append(m)
        for tc in (m.get("tool_calls") or []):
            cid = tc.get("id")
            nxt = messages[i + 1] if i + 1 < len(messages) else None
            if cid and cid not in answered and not (
                    nxt and nxt.get("role") == "tool" and nxt.get("tool_call_id") == cid):
                out.append({"role": "tool", "tool_call_id": cid,
                            "content": "[no tool result: the call did not complete]"})
                answered.add(cid)
    return out


def translate_request(body, model_id):
    """Responses request dict -> (Chat Completions request dict, name_map).

    ``model_id`` is the id the SERVER expects (decoupled from the ByteAsk picker
    alias). ``name_map`` is {sanitized_tool_name: original} for the Transducer."""
    if not isinstance(body, dict):
        raise UnsupportedItemError("request body must be a JSON object")
    specs, name_map, wire_for = _collect_tools(body)
    messages = []
    instr = body.get("instructions")
    if isinstance(instr, str) and instr.strip():
        messages.append({"role": "system", "content": instr})
    for item in body.get("input") or []:
        if isinstance(item, dict):
            _translate_item(item, messages, name_map, wire_for)
    messages = _repair_tool_pairs(_merge_tool_call_runs(messages))

    chat = {"model": model_id, "messages": messages, "stream": True,
            "stream_options": {"include_usage": True}}
    tools = _translate_tools(specs)
    if tools:
        chat["tools"] = tools
    if isinstance(body.get("temperature"), (int, float)):
        chat["temperature"] = body["temperature"]
    if isinstance(body.get("max_output_tokens"), int):
        chat["max_tokens"] = body["max_output_tokens"]

    # `--output-schema` -> the Chat Completions structured-output field. Support
    # is uneven across the servers this module drives (vLLM guided decoding,
    # Fireworks, TGI, Ollama, LM Studio, llama.cpp), and there is no capability
    # handshake to probe, so we send the standard spelling and let the server
    # answer. Two deliberate calls:
    #   * The schema is NOT run through `_sanitize_schema`. That helper strips
    #     `additionalProperties` because picky vLLM *tool* parsers reject it --
    #     but OpenAI-compatible strict mode REQUIRES `additionalProperties: false`,
    #     so sanitizing here would break the very thing being requested.
    #   * `strict` is forwarded exactly as the engine sent it rather than forced.
    # This block is inert unless the user passed `--output-schema`, so it cannot
    # regress any existing session.
    schema, strict = output_schema_of(body)
    if schema is not None:
        chat["response_format"] = {
            "type": "json_schema",
            "json_schema": {"name": OUTPUT_SCHEMA_NAME,
                            "schema": schema,
                            "strict": strict},
        }
    return chat, name_map


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


def rewrite_incomplete_line(line, prior):
    """For one SSE line of a native Responses stream: the bytes to send instead when it is a response.incomplete for
    the output limit, else None. That event becomes the note's events and a response.completed (status completed,
    end_turn false unless the cut-offs in a row reached CUTOFF_MAX_IN_A_ROW); an output item the limit cut off is
    left out. The engine reads each event's JSON type, so the event line already sent for the old event does not
    matter."""
    if b"response.incomplete" not in line or b"max_output_tokens" not in line:
        return None
    s = line.strip()
    if not s.startswith(b"data:"):
        return None
    try:
        obj = json.loads(s[5:].strip())
    except ValueError:
        return None
    resp = obj.get("response") if isinstance(obj, dict) else None
    if obj.get("type") != "response.incomplete" or not isinstance(resp, dict) \
            or (resp.get("incomplete_details") or {}).get("reason") != "max_output_tokens":
        return None
    text, keep_going = cutoff_note(prior)
    done = dict(resp, status="completed", incomplete_details=None, end_turn=not keep_going)
    if isinstance(done.get("output"), list):
        done["output"] = [i for i in done["output"] if not (isinstance(i, dict) and i.get("status") == "incomplete")]
        done["output"].append(cutoff_item(text))
    added = {"type": "response.output_item.added", "item": {"type": "message", "role": "assistant", "content": []}}
    return ("data: " + json.dumps(added) + "\n\n" + "".join(_cutoff_frames(text)[1:])
            + _frame("response.completed", {"response": done})).encode()


class IncompleteRewriter:
    """A native Responses byte stream, line-aligned, with its output-limit response.incomplete rewritten
    (rewrite_incomplete_line). feed() returns what to send now; flush() the rest at the end."""

    def __init__(self, prior):
        self.prior, self.buf = prior, b""

    def feed(self, chunk):
        self.buf += chunk
        nl = self.buf.rfind(b"\n")
        if nl < 0:
            # hold a partial line, but never more than 16 MB (the terminal line holds the whole response)
            if len(self.buf) > 16 << 20:
                out, self.buf = self.buf, b""
                return out
            return b""
        head, self.buf = self.buf[:nl + 1], self.buf[nl + 1:]
        return self._rewrite(head)

    def flush(self):
        out, self.buf = self._rewrite(self.buf), b""
        return out

    def _rewrite(self, data):
        if b"response.incomplete" not in data:
            return data
        try:
            return b"".join(rewrite_incomplete_line(ln, self.prior) or ln for ln in data.splitlines(keepends=True))
        except Exception:  # the stream as it came, never a broken one
            return data


# --------------------------------------------------------------------------- #
# response: Chat Completions SSE -> Responses SSE                             #
# --------------------------------------------------------------------------- #
def _frame(event_type, payload):
    payload = dict(payload, type=event_type)
    return f"event: {event_type}\ndata: {json.dumps(payload)}\n\n"


class Transducer:
    """Stateful Chat-SSE -> Responses-SSE transducer. Feed parsed chat chunks
    (chat.completion.chunk dicts, or a full chat.completion). Always emits a
    terminal (completed | failed) so the engine never hangs.

    Works-with-any-model hardening:
      * TEXT-LEAKED tool calls (fenced ```json, <tool_call>/<tool>/<xml>/
        <response> tags, [TOOL_CALLS], bare JSON) are held out of the text
        stream and recovered into real calls — gated on the declared tool names
        (``known_tools``), so a hallucinated tool stays visible text;
      * <think>...</think> reasoning blocks are stripped from the stream;
      * streamed tool_calls tolerate a missing ``index``; legacy
        ``function_call`` deltas and finish_reason "function_call" are honored;
      * tools the engine declared custom/freeform (``custom_tools``, e.g.
        apply_patch) come back as `custom_tool_call` items carrying the raw
        ``input`` string — the shape the engine expects for them."""

    def __init__(self, model, name_map=None, known_tools=None, custom_tools=None, prior_cutoffs=0):
        self.model = model
        self.prior_cutoffs = prior_cutoffs  # output-limit cut-offs in a row before this request (prior_cutoffs())
        self.cut_call = False               # a tool call the output limit cut off (dropped)
        # `is not None`, not `or`: an all-plain NameMap is empty (falsy) but still carries .search
        self.name_map = name_map if name_map is not None else {}
        self.custom_tools = set(custom_tools or ())
        # Names a leaked call may use: the chat-visible (sanitized) names plus
        # the originals. None = no gating (accept any well-formed name).
        self._gate = None
        if known_tools is not None:
            self._gate = set(known_tools) | set(self.name_map.values())
        self.response_id = "resp_selfhosted"
        self.output_items = []        # completed output items, in emit order
        self.reasoning_buf = ""       # reasoning_content buffered for output_items, never streamed
        self.created = False
        self.text_open = False
        self.text_buf = ""
        self.tool_calls = {}          # key -> {id, name, args}
        self.tool_order = []
        self.usage = {"input": 0, "cached": 0, "output": 0}
        self.finish_reason = None
        self.produced = False
        self.done = False
        self.dropped_degenerate = 0   # nameless call envelopes swallowed (bad tool model)
        # text pipeline (leak/think handling)
        self._mode = "normal"         # normal | think | hold
        self._carry = ""              # withheld tail (possible split marker)
        self._hold = ""               # candidate leaked-call block being held
        self._hold_kind = ""          # tag | mistral | start
        self._hold_tag = ""
        self._started_visible = False
        self.recovered = []           # [(name, args_json)] leaked calls recovered
        self._recovered_any = False

    def _created(self):
        self.created = True
        return _frame("response.created", {"response": {"id": self.response_id}})

    def _flush_reasoning(self):
        """Append accumulated reasoning_content to output_items as a reasoning item
        (NOT streamed to the engine, so no user-facing change). Idempotent; safe on
        empty."""
        if self.reasoning_buf:
            self.output_items.append({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": self.reasoning_buf}]})
            self.reasoning_buf = ""

    def _close_text(self):
        if not self.text_open:
            return []
        self.text_open = False
        item = {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": self.text_buf}]}
        self.text_buf = ""
        self.output_items.append(item)
        return [_frame("response.output_item.done", {"item": item})]

    # ---- text pipeline: stream visible text, hold leak/think blocks -------- #
    def _emit_visible(self, s):
        if not s:
            return []
        frames = []
        self.produced = True
        self._started_visible = True
        if not self.text_open:
            self.text_open = True
            frames.append(_frame("response.output_item.added",
                                 {"item": {"type": "message", "role": "assistant",
                                           "content": []}}))
        self.text_buf += s
        frames.append(_frame("response.output_text.delta", {"delta": s}))
        return frames

    def _feed_text(self, s):
        frames = []
        buf = self._carry + s
        self._carry = ""
        while True:
            if self._mode == "think":
                m = _THINK_CLOSE_RE.search(buf)
                if not m:
                    self._carry = buf[-10:]       # tail may hold a split </think>
                    return frames
                buf = buf[m.end():]
                self._mode = "normal"
                continue
            if self._mode == "hold":
                self._hold += buf
                res, leftover = self._resolve_hold(final=False)
                frames += res
                if leftover is None:
                    return frames                  # still holding
                buf = leftover
                continue
            # ---- normal ----
            if not buf:
                return frames
            if not self._started_visible and not self.text_buf:
                st = buf.lstrip()
                if not st or st in ("`", "``"):
                    self._carry = buf              # can't classify yet
                    return frames
                if st[0] in "{[" or st.startswith("```"):
                    # whole-message JSON / fenced block: a classic tool leak
                    self._mode, self._hold_kind, self._hold = "hold", "start", buf
                    buf = ""
                    continue
            m = _MARKER_RE.search(buf)
            while m and m.group(0).startswith("</"):
                m = _MARKER_RE.search(buf, m.end())   # a closing tag is plain text
            if m:
                frames += self._emit_visible(buf[:m.start()])
                tag = (m.group(1) or "").lower()
                rest = buf[m.start():]
                if tag == "think":
                    self._mode = "think"
                    buf = rest[m.end() - m.start():]
                    continue
                self._mode = "hold"
                self._hold_kind = "tag" if tag else "mistral"
                self._hold_tag = tag
                self._hold = rest
                buf = ""
                continue
            k = _partial_marker_len(buf)
            if k:
                frames += self._emit_visible(buf[:-k])
                self._carry = buf[-k:]
            else:
                frames += self._emit_visible(buf)
            return frames

    def _resolve_hold(self, final):
        """Try to turn the held block into recovered tool calls. Returns
        (frames, leftover): leftover None = keep holding; otherwise the block
        was consumed (recovered) or flushed back as text, and ``leftover`` is
        the text after it (continue in normal mode)."""
        h = self._hold
        if len(h) > _HOLD_MAX:                     # safety valve: it's just text
            self._mode, self._hold = "normal", ""
            return self._emit_visible(h), ""
        inner = leftover = None
        if self._hold_kind == "tag":
            cm = re.search(r"</\s*%s\s*>" % re.escape(self._hold_tag), h, re.I) \
                if self._hold_tag else None
            if cm:
                # parse the WHOLE block incl. the opening tag — a name attribute
                # (<invoke name="x">) lives there, not in the inner content
                inner, leftover = h[:cm.end()], h[cm.end():]
            elif final:
                inner = h
            else:
                return [], None
        elif self._hold_kind == "mistral":
            if not final:
                return [], None
            inner = h.split("]", 1)[1] if "]" in h else h
        else:                                      # "start": bare JSON or fence
            st = h.lstrip()
            off = len(h) - len(st)
            if st.startswith("```"):
                nl = st.find("\n")
                cm = st.find("```", nl + 1) if nl != -1 else -1
                if cm != -1:
                    inner, leftover = st[:cm + 3], h[off + cm + 3:]
                elif final:
                    inner = st
                else:
                    return [], None
            elif final:
                inner = st
            else:
                return [], None
        self._mode, self._hold = "normal", ""
        cands = _parse_leaked_calls(inner, self._gate)
        if cands:
            self.recovered += cands
            return [], leftover or ""
        text = h if leftover is None else h[:len(h) - len(leftover)]
        if _is_degenerate_call(inner):
            # A call envelope with no usable name: neither dispatchable nor prose.
            # Showing it puts raw JSON on the user's screen, so drop it instead.
            self.dropped_degenerate += 1
            return [], leftover or ""
        # Not a tool call after all — it was real text; put it back on the wire.
        return self._emit_visible(text), leftover or ""

    def _finalize_text_pipeline(self):
        frames = []
        if self._mode == "hold":
            res, leftover = self._resolve_hold(final=True)
            frames += res
            if leftover:
                frames += self._emit_visible(leftover)
        elif self._mode == "normal" and self._carry:
            frames += self._emit_visible(self._carry)
        # an unclosed <think> block is dropped by design
        self._mode, self._carry, self._hold = "normal", "", ""
        return frames

    # ---- call emission ----------------------------------------------------- #
    def _emit_call_item(self, wire, args, call_id):
        # A tool_search_call, a custom_tool_call (the engine declared this tool freeform: it
        # expects the raw text as `input`) or a function_call carrying its namespace.
        item = _call_item(self.name_map, wire, args, call_id, self.custom_tools)
        self.output_items.append(item)
        return _frame("response.output_item.done", {"item": item})

    def _flush_calls(self):
        frames = []
        had_structured = bool(self.tool_order)
        for idx in self.tool_order:
            tc = self.tool_calls.get(idx) or {}
            wire = tc.get("name") or "tool"
            args = tc.get("args") or ""
            if self.finish_reason == "length" and not _args_complete(args):
                self.cut_call = True  # arguments cut off by the output limit: never run a half-written call
                continue
            if not (args.strip().startswith("{") or args.strip().startswith("[")):
                args = args or "{}"
            frames.append(self._emit_call_item(wire, args, tc.get("id") or f"call_{idx}"))
        self.tool_calls, self.tool_order = {}, []
        if had_structured:
            self.recovered = []        # the structured channel wins; drop text dupes
        for i, (name, args) in enumerate(self.recovered):
            frames.append(self._emit_call_item(name, args, f"call_leak_{i}"))
            self._recovered_any = True
            self.produced = True
        self.recovered = []
        return frames

    def _completed(self):
        self._flush_reasoning()   # reasoning item into output_items (on every path, not streamed)
        self.done = True
        had_tool = (self.finish_reason in ("tool_calls", "function_call")
                    or self._recovered_any)
        end_turn = not had_tool
        if self.finish_reason == "length":  # cut off by the output limit: its note went out first (_limit_note)
            end_turn = not cutoff_note(self.prior_cutoffs)[1]
        usage = {
            "input_tokens": self.usage["input"],
            "input_tokens_details": {"cached_tokens": self.usage["cached"]},
            "output_tokens": self.usage["output"],
            "total_tokens": self.usage["input"] + self.usage["output"],
            "cache_write_tokens": 0,
        }
        return _frame("response.completed", {"response": {
            "id": self.response_id, "usage": usage, "end_turn": end_turn}})

    def _limit_note(self):
        """The output-limit note's events, before the terminal event of a reply the limit cut off."""
        if self.finish_reason != "length":
            return []
        text, _ = cutoff_note(self.prior_cutoffs)
        self.output_items.append(cutoff_item(text))
        return _cutoff_frames(text)

    def _failed(self, code, message):
        self._flush_reasoning()   # capture partial CoT even on a failed stream
        self.done = True
        return _frame("response.failed", {"response": {
            "id": self.response_id, "status": "failed",
            "error": {"code": code, "message": message}}})

    def feed(self, chunk):
        if not isinstance(chunk, dict):
            return []
        frames = []
        if not self.created:
            frames.append(self._created())
        if isinstance(chunk.get("error"), dict):
            err = chunk["error"]
            frames += self._finalize_text_pipeline()
            frames += self._close_text()
            frames.append(self._failed(str(err.get("code") or err.get("type") or "error"),
                                       err.get("message") or "upstream error"))
            return frames
        # A full (non-streamed) chat.completion, if the server ignored stream:true.
        obj = chunk.get("object")
        choices = chunk.get("choices") or []
        for ch in choices:
            delta = ch.get("delta") if obj != "chat.completion" else ch.get("message")
            delta = delta or {}
            content = delta.get("content")
            if isinstance(content, str) and content:
                frames += self._feed_text(content)
            # Reasoning models (gpt-oss/deepseek/glm/... on Fireworks, DeepSeek-R1 on
            # self/*) stream chain-of-thought in reasoning_content (some in reasoning).
            # Buffer it for output_items; NEVER stream it (no user-facing change).
            rc = delta.get("reasoning_content") or delta.get("reasoning")
            if isinstance(rc, str) and rc:
                self.reasoning_buf += rc
            fc = delta.get("function_call")          # legacy single-call format
            if isinstance(fc, dict):
                self.produced = True
                slot = self.tool_calls.setdefault("legacy", {"id": "", "name": "", "args": ""})
                if "legacy" not in self.tool_order:
                    self.tool_order.append("legacy")
                if fc.get("name"):
                    slot["name"] = fc["name"]
                if isinstance(fc.get("arguments"), str):
                    slot["args"] += fc["arguments"]
            for tc in (delta.get("tool_calls") or []):
                self.produced = True
                fn = tc.get("function") or {}
                idx = tc.get("index")
                if idx is None:
                    # No index (some servers): a delta carrying an id or a name
                    # starts a NEW call; a bare-arguments delta continues the last.
                    if tc.get("id") or fn.get("name"):
                        idx = len(self.tool_order)
                    else:
                        idx = self.tool_order[-1] if self.tool_order else 0
                slot = self.tool_calls.setdefault(idx, {"id": "", "name": "", "args": ""})
                if idx not in self.tool_order:
                    self.tool_order.append(idx)
                if tc.get("id"):
                    slot["id"] = tc["id"]
                if fn.get("name"):
                    slot["name"] = fn["name"]
                if isinstance(fn.get("arguments"), str):
                    slot["args"] += fn["arguments"]
            if ch.get("finish_reason"):
                self.finish_reason = ch["finish_reason"]
        self._capture_usage(chunk.get("usage"))
        # A full response arrives complete: close it out now.
        if obj == "chat.completion":
            frames += self._finalize_text_pipeline()
            frames += self._close_text()
            frames += self._flush_calls()
            frames += self._limit_note()
            frames.append(self._completed())
        return frames

    def _capture_usage(self, u):
        if not isinstance(u, dict):
            return
        if isinstance(u.get("prompt_tokens"), (int, float)):
            self.usage["input"] = int(u["prompt_tokens"])
        det = u.get("prompt_tokens_details") or {}
        if isinstance(det.get("cached_tokens"), (int, float)):
            self.usage["cached"] = int(det["cached_tokens"])
        if isinstance(u.get("completion_tokens"), (int, float)):
            self.usage["output"] = int(u["completion_tokens"])

    def close(self):
        if self.done:
            return []
        frames = []
        if not self.created:
            frames.append(self._created())
        frames += self._finalize_text_pipeline()
        frames += self._close_text()
        frames += self._flush_calls()
        if self.finish_reason or self.produced:
            frames += self._limit_note()
            frames.append(self._completed())
        else:
            frames.append(self._failed("stream_interrupted",
                                        "self-hosted model stream ended before any content"))
        return frames


# --------------------------------------------------------------------------- #
# SSE parsing: chat uses `data: {json}\n\n` frames + a `data: [DONE]` sentinel #
# --------------------------------------------------------------------------- #
def iter_sse(byte_iter):
    buf = b""
    for chunk in byte_iter:
        if not chunk:
            continue
        buf = (buf + chunk).replace(b"\r\n", b"\n")   # tolerate CRLF from proxies
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
    payload = b" ".join(parts)
    if payload.strip() == b"[DONE]":                  # stream terminator, not JSON
        return None
    try:
        return json.loads(payload)
    except Exception:
        return None
