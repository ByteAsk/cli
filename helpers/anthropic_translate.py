#!/usr/bin/env python3
"""Responses API <-> Anthropic Messages API translation (stdlib only).

The ByteAsk engine speaks ONLY the OpenAI Responses API. This module lets the
gateway route ``claude-*`` models to Anthropic by translating each request
(Responses -> Messages) and transducing the streaming response (Anthropic SSE ->
Responses SSE). The Codex client is stateless (it resends the full history every
turn, ``store=false``, no ``previous_response_id``), so translation is per-request
with NO server-side conversation state.

Byte-accuracy notes on the verified wire contracts that shaped this module:
  * A Responses request ``input[]`` is a list of ``ResponseItem`` dicts, tagged by
    ``type`` (snake_case). ``function_call_output.output`` is a bare string OR a
    bare array of content items (never an object).
  * The engine's SSE parser needs only the JSON ``type``; it ignores
    ``sequence_number``/``output_index``. ``response.created`` needs just
    ``{"response":{}}``; ``response.completed`` needs ``response.id`` and, IF a
    ``usage`` object is present, all of ``input_tokens``/``output_tokens``/
    ``total_tokens`` (else the stream hard-errors). A text delta requires a
    preceding ``response.output_item.added`` (else the engine panics
    "OutputTextDelta without active item").

Public API:
  is_anthropic_model(model) -> bool
  anthropic_model_id(model) -> str
  translate_request(responses_body: dict) -> dict          # Anthropic Messages body
  route_headers(api_key) -> dict                           # Anthropic auth/version headers
  Transducer(model)  .feed(anthropic_event)->[str]  .close()->[str]
  iter_sse(byte_iter) -> yields parsed SSE event dicts
  UnsupportedItemError                                       # raised on an un-translatable item
"""
import copy
import hashlib
import json
import re
import sys

import effort as _effort


def _warn(msg):
    """One-line stderr note. Runs under the gateway (journalctl) AND under the
    user's byok sidecar, so keep it short and never raise."""
    try:
        sys.stderr.write(f"[anthropic] {msg}\n")
    except Exception:
        pass

# --- config --------------------------------------------------------------- #
# Client slug -> real Anthropic model id. Slugs are chosen to equal the real ids
# so this is identity today; the map is the seam for friendly aliases later.
MODEL_MAP = {
    "claude-opus-4-8": "claude-opus-4-8",
    "claude-opus-5": "claude-opus-5",
    "claude-opus-4-7": "claude-opus-4-7",
    "claude-sonnet-5": "claude-sonnet-5",
    "claude-sonnet-4-6": "claude-sonnet-4-6",
    # Identity entries, same as every entry above. MODEL_MAP.get(model, model)
    # passes these through unchanged even with no entry; they are listed for
    # discoverability, not because it changes behavior.
    # ⚠️ Anthropic's id is DASHED. The dotted slug is ours (catalog/pricing); sent as-is the
    # API answers 404 "Did you mean claude-fable-5-1?". Probed live 2026-10-09.
    "claude-fable-5.1": "claude-fable-5-1",
    "claude-opus-5-5": "claude-opus-5-5",
    "claude-sonnet-5-5": "claude-sonnet-5-5",
    "claude-haiku-4-5": "claude-haiku-4-5",
    # 2026-10-09: accepts output_config.effort low..max AND thinking:{adaptive}
    # (probed live), so it is NOT in NO_EFFORT_MODELS, unlike 4.5.
    "claude-haiku-5-5": "claude-haiku-5-5",
}
# Anthropic REQUIRES max_tokens; the Responses request never sends one. Per-model
# cap (conservative; confirm against the model's true max output at build).
# claude-fable-5.1 and claude-haiku-4-5 are deliberately NOT
# listed here — their real max-output limits weren't in the pricing data this was
# built from, so (same as claude-fable-5 before them) they fall back to
# MAX_OUTPUT_DEFAULT below rather than guess a number.
# Every model listed takes up to 128K output tokens (Haiku 4.5: 64K) per Anthropic's model table (2026-11-11): 64K
# leaves room for thinking and a long reply inside the 1M-token context, and a reply that still reaches it continues
# (the output-limit block below). Haiku 4.5 (200K context) gets 32K, so a long prompt plus the cap fits its window.
MAX_OUTPUT_TOKENS = {"claude-opus-4-8": 64000, "claude-opus-5": 64000, "claude-opus-4-7": 64000,
                     "claude-sonnet-5": 64000, "claude-sonnet-4-6": 64000,
                     "claude-opus-5-5": 64000, "claude-sonnet-5-5": 64000, "claude-haiku-5-5": 64000,
                     "claude-fable-5": 64000, "claude-fable-5-1": 64000, "claude-haiku-4-5": 32000}
# Models that reject BOTH `output_config.effort` ("does not support the effort
# parameter") and `thinking:{adaptive}` ("adaptive thinking is not supported").
# PROBED LIVE 2026-09-30 on claude-haiku-4-5; every other served Claude accepts both.
NO_EFFORT_MODELS = frozenset({"claude-haiku-4-5"})
# Engine reasoning effort -> Anthropic `output_config.effort`.
# PROBED LIVE 2026-07-25 against api.anthropic.com under the pinned
# anthropic-version below, with tools + cache_control + stream all present:
# every served Claude model accepts low/medium/high/max; `claude-sonnet-4-6`
# rejects `xhigh` ("Supported levels: high, low, max, medium"). The catalog
# advertises exactly [low, medium, high, max], so the interesting rows here are
# the ones that CANNOT be picked from the menu -- they arrive when a user carries
# a gpt effort over via the persisted global `model_reasoning_effort`.
#   none/minimal -> low   : Anthropic has no such rung; `low` is the floor.
#   xhigh        -> high  : NOT `max` -- a silent upgrade to the most expensive
#                           rung is the wrong way to be wrong about cost.
#   ultra        -> max   : engine-only level above `max`; `max` is the ceiling.
EFFORT_MAP = {
    "none": "low", "minimal": "low", "low": "low", "medium": "medium",
    "high": "high", "xhigh": "high", "max": "max", "ultra": "max",
}
MAX_OUTPUT_DEFAULT = 16384
ANTHROPIC_VERSION = "2023-06-01"
# 5-minute ephemeral cache (write 1.25x, read 0.1x). Switch to {"type":"ephemeral",
# "ttl":"1h"} (write 2.0x, refreshed free) if sessions show long idle gaps.
CACHE_CONTROL = {"type": "ephemeral"}


class UnsupportedItemError(Exception):
    """A Responses ``input`` item type we refuse to silently drop. The gateway
    turns this into a clear client error instead of corrupting the next prompt."""


def is_anthropic_model(model):
    return isinstance(model, str) and model.startswith("claude")


def anthropic_model_id(model):
    return MODEL_MAP.get(model, model)


def route_headers(api_key):
    """Anthropic upstream auth/version headers (NOT OpenAI's Bearer scheme)."""
    return {"x-api-key": api_key or "", "anthropic-version": ANTHROPIC_VERSION,
            "content-type": "application/json"}


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
_WIRE_NAME_OK = re.compile(r"^[A-Za-z0-9_-]{1,64}$")     # Anthropic's tool-name rule
_WIRE_NAME_BAD = re.compile(r"[^A-Za-z0-9_-]")
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
    """The Anthropic request body, carrying the NameMap the Transducer needs. A plain dict
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


# ==== request: Responses -> Anthropic Messages ============================= #
def _text_of(content_items, kinds):
    """Concatenate the ``text`` of content items whose type is in ``kinds``."""
    out = []
    for c in content_items or []:
        if isinstance(c, dict) and c.get("type") in kinds and isinstance(c.get("text"), str):
            out.append(c["text"])
    return "".join(out)


def _image_block(image_url):
    """Anthropic image block from a Responses ``input_image.image_url`` (a data-URL
    ``data:<mime>;base64,<payload>`` or an ``http(s)://`` url)."""
    if isinstance(image_url, str) and image_url.startswith("data:") and ";base64," in image_url:
        header, b64 = image_url.split(";base64,", 1)
        media_type = header[len("data:"):] or "image/png"
        return {"type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": b64}}
    return {"type": "image", "source": {"type": "url", "url": image_url}}


def _message_content_blocks(content_items):
    """Responses Message.content[] -> Anthropic content blocks (text + image).
    Encrypted-content items are supplementary opaque copies -> skipped."""
    blocks = []
    for c in content_items or []:
        if not isinstance(c, dict):
            continue
        t = c.get("type")
        if t in ("input_text", "output_text") and isinstance(c.get("text"), str):
            if c["text"]:
                blocks.append({"type": "text", "text": c["text"]})
        elif t == "input_image" and c.get("image_url"):
            blocks.append(_image_block(c["image_url"]))
        # input_image with no url, or encrypted_content items: skip (no analog).
    return blocks


def _tool_result_content(output):
    """Responses function/custom tool output -> Anthropic tool_result.content.
    ``output`` is a bare string OR a bare array of content items (never an object)."""
    if isinstance(output, str):
        return output
    if isinstance(output, list):
        blocks = []
        for c in output:
            if not isinstance(c, dict):
                continue
            t = c.get("type")
            if t == "input_text" and isinstance(c.get("text"), str):
                blocks.append({"type": "text", "text": c["text"]})
            elif t == "input_image" and c.get("image_url"):
                blocks.append(_image_block(c["image_url"]))
            # encrypted_content items skipped
        return blocks or ""
    return ""


def _tool_use_input(raw):
    """Coerce a tool-call argument payload to a JSON OBJECT (Anthropic tool_use.input
    must be an object). FunctionCall.arguments is a JSON string; CustomToolCall.input
    is freeform text. Non-object JSON / freeform is wrapped under ``input``."""
    if isinstance(raw, dict):
        return raw
    if isinstance(raw, str):
        try:
            v = json.loads(raw)
            return v if isinstance(v, dict) else {"input": raw}
        except Exception:
            return {"input": raw}
    return {}


def _append(messages, role, block):
    """Append a content block, MERGING into the previous message when it has the
    same role (Anthropic requires strict user/assistant alternation; this collapses
    parallel tool calls / parallel tool outputs into one turn)."""
    if messages and messages[-1]["role"] == role:
        messages[-1]["content"].append(block)
    else:
        messages.append({"role": role, "content": [block]})


def _translate_item(item, messages, wire_for=None):
    """Translate one Responses ``input`` item into Anthropic message block(s),
    appending to ``messages``. Raises UnsupportedItemError on a type we won't drop.
    ``wire_for(namespace, name)`` gives the declared wire name of a namespaced tool."""
    wire_for = wire_for or (lambda namespace, name: _wire_name(namespace, name))
    t = item.get("type")
    if t == "message":
        role = item.get("role")
        blocks = _message_content_blocks(item.get("content"))
        if role == "assistant":
            for b in blocks:
                _append(messages, "assistant", b)
        else:  # user / developer / system-as-user
            for b in blocks:
                _append(messages, "user", b)
    elif t in ("function_call", "custom_tool_call", "local_shell_call"):
        call_id = item.get("call_id") or item.get("id") or ""
        name = item.get("name") or ("local_shell" if t == "local_shell_call" else "tool")
        if t != "local_shell_call":
            name = wire_for(item.get("namespace"), name)
        raw = item.get("input") if t == "custom_tool_call" else (
            item.get("action") if t == "local_shell_call" else item.get("arguments"))
        _append(messages, "assistant",
                {"type": "tool_use", "id": call_id, "name": name,
                 "input": _tool_use_input(raw)})
    elif t in ("function_call_output", "custom_tool_call_output"):
        _append(messages, "user",
                {"type": "tool_result", "tool_use_id": item.get("call_id") or "",
                 "content": _tool_result_content(item.get("output"))})
    elif t == "tool_search_call":
        _append(messages, "assistant",
                {"type": "tool_use", "id": item.get("call_id") or item.get("id") or "",
                 "name": "tool_search", "input": _tool_use_input(item.get("arguments"))})
    elif t == "tool_search_output":
        _append(messages, "user",
                {"type": "tool_result", "tool_use_id": item.get("call_id") or "",
                 "content": _render_search_output(item, wire_for)})
    elif t in ("web_search_call", "image_generation_call"):
        # OpenAI server-side tools with no Anthropic analog: preserve as faithful
        # TEXT so history isn't lost (these only appear on a mid-session GPT->Claude
        # switch; Claude never emits them).
        _append(messages, "assistant",
                {"type": "text", "text": _server_tool_text(t, item)})
    elif t == "agent_message":
        text = _text_of(item.get("content"), ("input_text",))
        if text:
            _append(messages, "assistant", {"type": "text", "text": text})
    elif t in ("compaction", "context_compaction"):
        # Local auto-compaction summary. encrypted_content is an OPAQUE server blob
        # we cannot decrypt, so we cannot reproduce the summary text; emit a
        # non-corrupting marker (minor context loss only on very long sessions).
        _append(messages, "user",
                {"type": "text",
                 "text": "[Earlier conversation was summarized/compacted to save context.]"})
    elif t in ("reasoning", "compaction_trigger", "additional_tools"):
        return  # deliberate drops: reasoning off; markers / responses-lite only
    else:
        raise UnsupportedItemError(
            f"Responses item type {t!r} is not yet supported on Claude")


def _server_tool_text(t, item):
    if t == "web_search_call":
        act = item.get("action") or {}
        q = act.get("query") or " ".join(act.get("queries") or [])
        return f"[web search performed: {q}]"
    return "[image generation performed]"


_SCHEMA_COMBINATORS = ("anyOf", "oneOf", "allOf")


def _tidy_schema(node, root=False):
    """Fix the JSON-Schema shapes a real MCP server can publish that Anthropic 400s on (one bad
    tool schema fails the WHOLE request, so a single odd MCP tool would end every Claude turn).
    PROBED LIVE 2026-10-19 on claude-haiku-5-5: `items` as a list, and an input_schema with no
    `type` ("Field required"). `required` naming an undefined property is tolerated here but is
    filtered too, so the same schema is valid on every provider. Walks only the places a
    sub-schema can sit (properties / items / anyOf-oneOf-allOf); every other value, booleans
    such as `additionalProperties: false` or `uniqueItems: true` included, is copied as is.
    Returns a new tree."""
    if node is True or node is False:
        return {}                                    # a boolean schema: "anything"
    if not isinstance(node, dict):
        return node
    out = {}
    for k, v in node.items():
        if k == "properties" and isinstance(v, dict):
            out[k] = {pk: _tidy_schema(pv) for pk, pv in v.items()}
        elif k in _SCHEMA_COMBINATORS and isinstance(v, list):
            out[k] = [_tidy_schema(x) for x in v]
        elif k == "items":
            out[k] = _tidy_schema(v[0] if v else {}) if isinstance(v, list) else _tidy_schema(v)
        else:
            out[k] = copy.deepcopy(v)
    props = out.get("properties")
    if isinstance(out.get("required"), list) and isinstance(props, dict):
        out["required"] = [r for r in out["required"] if r in props]
    if root and "type" not in out:
        out["type"] = "object"
    return out


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
    """Declared tool specs (see ``_collect_tools``) -> Anthropic tools[]. Function tools map
    cleanly; freeform ('custom') tools get a single-string input_schema (the model's call is
    returned as a custom_tool_call, see ``_call_item``). Tools OpenAI hosts itself
    (web_search, image_generation) have no Anthropic client analog -> not advertised."""
    out = []
    for sp in specs:
        if sp["custom"]:
            schema = dict(_CUSTOM_SCHEMA, properties=dict(_CUSTOM_SCHEMA["properties"]))
        else:
            schema = _tidy_schema(sp["parameters"] or {"type": "object", "properties": {}}, root=True)
        out.append({"name": sp["wire"],
                    "description": _freeform_description(sp) if sp["custom"] else sp["description"],
                    "input_schema": schema})
    return out


def _place_cache_control(system_blocks, tools, messages):
    """Add cache_control breakpoints on the stable prefix: last tool, last
    system block, and the last block of the last message (rolling). tools -> system
    -> messages order; <=3 of the 4 allowed breakpoints."""
    if tools:
        tools[-1] = dict(tools[-1], cache_control=CACHE_CONTROL)
    if system_blocks:
        system_blocks[-1] = dict(system_blocks[-1], cache_control=CACHE_CONTROL)
    if messages and messages[-1]["content"]:
        last = messages[-1]["content"]
        last[-1] = dict(last[-1], cache_control=CACHE_CONTROL)


def _repair_tool_pairs(messages):
    """Anthropic requires every ``tool_use`` to be answered by a ``tool_result`` in the
    IMMEDIATELY following user message (else HTTP 400 "tool_use ids ... without tool_result
    blocks immediately after"). A dangling tool_use is common in real sessions: the model
    makes a tool call, the user interrupts/cancels and types something new, so the Responses
    history carries the function_call with NO function_call_output. For each assistant turn
    with tool_use blocks, make the next message a user turn that LEADS with a tool_result for
    every one of those ids (synthesizing a placeholder for any missing) — keeping the request
    valid without dropping history. Also hoists tool_results to the front (Anthropic wants
    them first in the user turn)."""
    i = 0
    while i < len(messages):
        msg = messages[i]
        if msg.get("role") == "assistant":
            use_ids = [b.get("id") for b in msg.get("content", [])
                       if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("id")]
            if use_ids:
                nxt = messages[i + 1] if i + 1 < len(messages) else None
                if not (nxt and nxt.get("role") == "user"):
                    nxt = {"role": "user", "content": []}
                    messages.insert(i + 1, nxt)
                have = {b.get("tool_use_id") for b in nxt["content"]
                        if isinstance(b, dict) and b.get("type") == "tool_result"}
                synth = [{"type": "tool_result", "tool_use_id": tid,
                          "content": "[no tool result: the call did not complete]"}
                         for tid in use_ids if tid not in have]
                results = [b for b in nxt["content"]
                           if isinstance(b, dict) and b.get("type") == "tool_result"]
                others = [b for b in nxt["content"]
                          if not (isinstance(b, dict) and b.get("type") == "tool_result")]
                nxt["content"] = synth + results + others   # tool_results lead the user turn
        i += 1
    return messages


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


def _close_objects(node, changed=None):
    """Force ``additionalProperties: false`` on every object node.

    PROBED LIVE 2026-07-27: Anthropic REQUIRES it on EVERY object in the tree,
    nested ones included --
        "output_config.format.schema: For 'object' type, 'additionalProperties'
         must be explicitly set to false"
    A user-authored schema routinely omits it (or sets it true), which would hard-400
    the turn, so inject rather than let a valid JSON Schema be rejected. This is the
    same closed-world rule OpenAI enforces for strict mode, and the engine defaults
    `output_schema_strict` to true, so it matches the requested semantics.

    Returns ``(schema, changed)`` where ``changed`` lists the paths we rewrote."""
    changed = [] if changed is None else changed

    def walk(n, path):
        if isinstance(n, dict):
            out = {}
            for k, v in n.items():
                out[k] = walk(v, f"{path}.{k}" if path else k)
            if out.get("type") == "object" and out.get("additionalProperties") is not False:
                if "additionalProperties" in out:
                    changed.append(f"{path or '<root>'} (was {out['additionalProperties']!r})")
                else:
                    changed.append(path or "<root>")
                out["additionalProperties"] = False
            return out
        if isinstance(n, list):
            return [walk(v, f"{path}[{i}]") for i, v in enumerate(n)]
        return n

    return walk(node, ""), changed


def translate_request(body):
    """Responses request dict -> Anthropic Messages request dict (streaming)."""
    model = body.get("model")
    instructions = body.get("instructions")
    system_blocks = []
    if isinstance(instructions, str) and instructions.strip():
        system_blocks = [{"type": "text", "text": instructions}]

    specs, name_map, wire_for = _collect_tools(body)
    messages = []
    for item in body.get("input") or []:
        if isinstance(item, dict):
            _translate_item(item, messages, wire_for)
    _repair_tool_pairs(messages)   # pair every tool_use with a tool_result (dangling -> synth)
    # Anthropic requires the first message to be a user turn.
    if not messages or messages[0]["role"] != "user":
        messages.insert(0, {"role": "user", "content": [{"type": "text", "text": "(continue)"}]})

    tools = _translate_tools(specs)
    _place_cache_control(system_blocks, tools, messages)

    out = _Request(
        model=anthropic_model_id(model),
        max_tokens=MAX_OUTPUT_TOKENS.get(model, MAX_OUTPUT_DEFAULT),
        messages=messages,
        stream=True,
    )
    out.name_map = name_map
    if system_blocks:
        out["system"] = system_blocks
    if tools:
        out["tools"] = tools
        out["tool_choice"] = {"type": "auto"}

    # Reasoning effort + the thinking axis. Effort alone is close to inert on
    # Anthropic: `claude-opus-4-8` and `claude-sonnet-4-6` run WITHOUT thinking
    # when the field is omitted (which is what we did before this), so the knob
    # would move almost nothing on them. We therefore send `adaptive` alongside
    # -- the newer models already think by default, so this only levels up those
    # two. NEVER send {"type": "disabled"}: `claude-fable-5` 400s on it outright
    # and `claude-opus-5` 400s on it above effort `high`.
    #
    # ⚠️ This raises per-request cost on opus-4-8 / sonnet-4-6 (thinking tokens
    # are billed): extended thinking is enabled on them whenever an effort is
    # sent. Deliberate: without it the effort knob does almost nothing there.
    #
    # Multi-turn safety PROVEN LIVE 2026-07-25: replaying an assistant turn
    # WITHOUT its thinking blocks (which is exactly what this translator does --
    # `_translate_item` drops incoming reasoning items and the Transducer never
    # streams thinking back) returns 200 on a tool-use follow-up. Anthropic does
    # not require thinking echoed back, so no signature store is needed here.
    #
    # ⚠️ `effort` and `--output-schema` BOTH live under `output_config`. Assigning
    # a fresh dict for either one silently deletes the other -- the exact failure
    # mode that also applies to Gemini's `thinkingConfig`. Build the dict once,
    # then merge, and never re-assign `out["output_config"]` below this point.
    output_config = {}

    effort = None if model in NO_EFFORT_MODELS else _effort.clamp_effort(body, EFFORT_MAP)
    if effort:
        output_config["effort"] = effort
        out["thinking"] = {"type": "adaptive"}

    # `--output-schema` -> Anthropic's NATIVE structured outputs. PROBED LIVE
    # 2026-07-27 on claude-sonnet-4-6:
    #   * `output_config.format` works under the pinned anthropic-version above
    #     with NO `anthropic-beta` header (the older top-level `output_format`
    #     spelling is deprecated and 400s, pointing here).
    #   * It coexists with tools + effort + thinking:adaptive + streaming (200),
    #     and -- the load-bearing check -- with a tool-forcing prompt the model
    #     STILL returns `stop_reason: tool_use`. So the agent loop is intact.
    #     This is why we do NOT use the classic forced-single-tool workaround:
    #     that would hijack `tool_choice` and break every agentic `exec` run,
    #     which always carries tools.
    #   * The format object accepts ONLY {type, schema}; `strict` and `name`
    #     are rejected ("Extra inputs are not permitted"). Anthropic structured
    #     output is always strict, so a request asking for strict=false gets a
    #     stricter result than it asked for -- say so rather than pretend.
    schema, strict = output_schema_of(body)
    if schema is not None:
        closed, injected = _close_objects(schema)
        if injected:
            _warn("output_schema: forced additionalProperties=false on %s -- "
                  "Anthropic rejects open objects" % ", ".join(injected))
        if not strict:
            _warn("output_schema: strict=false was requested, but Anthropic "
                  "structured output is always strict; applying it strictly")
        output_config["format"] = {"type": "json_schema", "schema": closed}

    if output_config:
        out["output_config"] = output_config
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


# ==== response: Anthropic SSE -> Responses SSE ============================= #
def _frame(event_type, payload):
    """One Responses SSE frame. The engine dispatches on the JSON ``type`` (the
    ``event:`` line is redundant but standard)."""
    payload = dict(payload, type=event_type)
    return f"event: {event_type}\ndata: {json.dumps(payload)}\n\n"


_ERROR_CODE = {
    "overloaded_error": "server_is_overloaded",
    "rate_limit_error": "rate_limit_exceeded",
    "invalid_request_error": "invalid_prompt",
    "authentication_error": "invalid_request",
    "permission_error": "invalid_request",
    "not_found_error": "invalid_request",
    "api_error": "server_error",
}


class Transducer:
    """Stateful Anthropic-SSE -> Responses-SSE transducer. Feed parsed Anthropic
    event dicts; get back Responses SSE frame strings. Guarantees a terminal
    (response.completed or response.failed) so the engine never hangs."""

    def __init__(self, model, name_map=None, prior_cutoffs=0):
        self.model = model
        self.name_map = name_map  # NameMap from translate_request: wire name -> engine (namespace, name)
        self.prior_cutoffs = prior_cutoffs  # output-limit cut-offs in a row before this request (prior_cutoffs())
        self.cut_call = False     # a tool call whose arguments the output limit cut off (dropped)
        self.response_id = "resp_pending"
        self.blocks = {}          # index -> {"kind","text"|"id"/"name"/"json"}
        self.usage = {"input": 0, "cache_read": 0, "cache_write": 0, "output": 0}
        self.output_items = []    # every completed output_item.done payload, in order
        self.stop_reason = None
        self.had_tool_use = False
        self.done = False         # emitted a terminal event?
        self.created = False

    # -- helpers ---------------------------------------------------------- #
    def _created(self):
        self.created = True
        return _frame("response.created", {"response": {"id": self.response_id}})

    def _completed(self, end_turn=None):
        self.done = True
        total_input = (self.usage["input"] + self.usage["cache_read"]
                       + self.usage["cache_write"])
        usage = {
            "input_tokens": total_input,
            # cache writes also inside the details, where OpenAI-style clients (the engine's usage
            # reader) look for them; the top-level copy below stays for the gateway's billing
            "input_tokens_details": {"cached_tokens": self.usage["cache_read"],
                                     "cache_write_tokens": self.usage["cache_write"]},
            "output_tokens": self.usage["output"],
            "total_tokens": total_input + self.usage["output"],
            # consumed by the gateway's _scan_usage_line for 4-field billing:
            "cache_write_tokens": self.usage["cache_write"],
        }
        return _frame("response.completed", {"response": {
            "id": self.response_id, "usage": usage,
            "end_turn": (not self.had_tool_use) if end_turn is None else end_turn}})

    def _failed(self, code, message):
        self.done = True
        return _frame("response.failed", {"response": {
            "id": self.response_id, "status": "failed",
            "error": {"code": code, "message": message}}})

    # -- main ------------------------------------------------------------- #
    def feed(self, ev):
        t = ev.get("type")
        if t == "message_start":
            msg = ev.get("message") or {}
            self.response_id = msg.get("id") or self.response_id
            self._capture_usage(msg.get("usage"))
            return [self._created()]
        if t == "content_block_start":
            return self._block_start(ev)
        if t == "content_block_delta":
            return self._block_delta(ev)
        if t == "content_block_stop":
            return self._block_stop(ev)
        if t == "message_delta":
            self._capture_usage((ev.get("usage")))
            self.stop_reason = (ev.get("delta") or {}).get("stop_reason") or self.stop_reason
            return []
        if t == "message_stop":
            frames = []
            if not self.created:
                frames.append(self._created())
            if self.stop_reason == "max_tokens" or self.cut_call:
                text, keep_going = cutoff_note(self.prior_cutoffs)  # cut off by the output limit: say so, go on
                frames += _cutoff_frames(text)
                self.output_items.append(cutoff_item(text))
                frames.append(self._completed(end_turn=not keep_going))
                return frames
            frames.append(self._completed())
            return frames
        if t == "error":
            err = ev.get("error") or {}
            code = _ERROR_CODE.get(err.get("type"), "server_error")
            frames = []
            if not self.created:
                frames.append(self._created())
            frames.append(self._failed(code, err.get("message") or "anthropic error"))
            return frames
        return []  # ping, unknown -> ignore

    def _capture_usage(self, u):
        """Merge whatever usage fields are present. message_start carries
        input/cache_read/cache_write/output(=initial); message_delta carries the
        final output_tokens (and only fields it includes overwrite)."""
        if not isinstance(u, dict):
            return
        for src, dst in (("input_tokens", "input"),
                         ("cache_read_input_tokens", "cache_read"),
                         ("cache_creation_input_tokens", "cache_write"),
                         ("output_tokens", "output")):
            v = u.get(src)
            if isinstance(v, (int, float)):
                self.usage[dst] = int(v)

    def _block_start(self, ev):
        idx = ev.get("index")
        cb = ev.get("content_block") or {}
        kind = cb.get("type")
        if kind == "text":
            self.blocks[idx] = {"kind": "text", "text": ""}
            # REQUIRED before any output_text.delta (else engine panics).
            item = {"type": "message", "role": "assistant", "content": []}
            return [_frame("response.output_item.added", {"item": item})]
        if kind == "tool_use":
            self.had_tool_use = True
            self.blocks[idx] = {"kind": "tool_use", "id": cb.get("id") or "",
                                "name": cb.get("name") or "tool", "json": ""}
            return []
        if kind in ("thinking", "redacted_thinking"):
            # Extended thinking is off on the wire, but if it IS present its text
            # is buffered here, appended to self.output_items as a reasoning item
            # at block_stop, and NEVER streamed to the engine (returns no frames).
            self.blocks[idx] = {"kind": "thinking", "text": ""}
            return []
        # any other block type -> ignore
        self.blocks[idx] = {"kind": "ignore"}
        return []

    def _block_delta(self, ev):
        idx = ev.get("index")
        blk = self.blocks.get(idx)
        delta = ev.get("delta") or {}
        if not blk:
            return []
        if blk["kind"] == "text" and delta.get("type") == "text_delta":
            blk["text"] += delta.get("text") or ""
            return [_frame("response.output_text.delta", {"delta": delta.get("text") or ""})]
        if blk["kind"] == "tool_use" and delta.get("type") == "input_json_delta":
            blk["json"] += delta.get("partial_json") or ""   # buffered; engine ignores deltas
        if blk["kind"] == "thinking" and delta.get("type") == "thinking_delta":
            blk["text"] += delta.get("thinking") or ""       # buffered, not streamed
        return []

    def _block_stop(self, ev):
        blk = self.blocks.get(ev.get("index"))
        if not blk:
            return []
        if blk["kind"] == "text":
            item = {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": blk["text"]}]}
            self.output_items.append(item)
            return [_frame("response.output_item.done", {"item": item})]
        if blk["kind"] == "tool_use":
            if not _args_complete(blk["json"]):
                self.cut_call = True  # arguments cut off by the output limit: never run a half-written call
                return []
            args = blk["json"] if blk["json"].strip() else "{}"
            item = _call_item(self.name_map, blk["name"], args, blk["id"])
            self.output_items.append(item)
            return [_frame("response.output_item.done", {"item": item})]
        if blk["kind"] == "thinking":
            # reasoning item goes to output_items only; NOT emitted to the engine stream
            if blk["text"]:
                self.output_items.append({
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": blk["text"]}]})
            return []
        return []

    def close(self):
        """Stream ended. If no terminal was emitted (socket drop before
        message_stop), synthesize one so the engine doesn't hang."""
        if self.done:
            return []
        frames = []
        if not self.created:
            frames.append(self._created())
        frames.append(self._failed("stream_interrupted",
                                    "anthropic stream ended before message_stop"))
        return frames


# ==== SSE line parsing (Anthropic upstream bytes -> event dicts) =========== #
def iter_sse(byte_iter):
    """Parse an Anthropic SSE byte stream into event dicts (yields each event's
    ``data:`` JSON, which itself carries the ``type``)."""
    buf = b""
    for chunk in byte_iter:
        if not chunk:
            continue
        buf += chunk
        while b"\n\n" in buf:
            raw, buf = buf.split(b"\n\n", 1)
            data = _sse_data(raw)
            if data is not None:
                yield data
    # trailing event without a final blank line
    data = _sse_data(buf)
    if data is not None:
        yield data


def _sse_data(raw):
    data_parts = []
    for line in raw.split(b"\n"):
        line = line.strip()
        if line.startswith(b"data:"):
            data_parts.append(line[5:].strip())
    if not data_parts:
        return None
    try:
        return json.loads(b" ".join(data_parts))
    except Exception:
        return None
