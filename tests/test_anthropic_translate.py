#!/usr/bin/env python3
"""Exhaustive tests for the Responses <-> Anthropic Messages translator.

Covers: request translation (full item coverage, merge, images,
cache_control, hard-fail) and the SSE transducer (added-before-delta, buffered
tool_use, complete usage, error/drop termination). Stdlib unittest only.
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import anthropic_translate as A

EPH = {"type": "ephemeral"}


def frame_json(frame):
    """Pull the JSON dict out of one 'event: ..\ndata: {..}\n\n' frame."""
    return json.loads(frame.split("data: ", 1)[1])


def run(events, model="claude-sonnet-5"):
    """Feed events through a Transducer + close(); return the emitted event dicts."""
    t = A.Transducer(model)
    frames = []
    for e in events:
        frames += t.feed(e)
    frames += t.close()
    return t, [frame_json(f) for f in frames]


# ============================ request translation ========================== #
class TestRoute(unittest.TestCase):
    def test_is_anthropic_model(self):
        self.assertTrue(A.is_anthropic_model("claude-opus-4-8"))
        self.assertTrue(A.is_anthropic_model("claude-sonnet-5"))
        self.assertFalse(A.is_anthropic_model("gpt-5.4"))
        self.assertFalse(A.is_anthropic_model(None))

    def test_route_headers(self):
        h = A.route_headers("sk-ant-xyz")
        self.assertEqual(h["x-api-key"], "sk-ant-xyz")
        self.assertEqual(h["anthropic-version"], A.ANTHROPIC_VERSION)
        self.assertNotIn("Authorization", h)


class TestTranslateRequest(unittest.TestCase):
    def base(self, **kw):
        body = {"model": "claude-sonnet-5", "input": []}
        body.update(kw)
        return A.translate_request(body)

    def test_model_maxtokens_stream(self):
        r = self.base(model="claude-opus-4-8",
                      input=[{"type": "message", "role": "user",
                              "content": [{"type": "input_text", "text": "hi"}]}])
        self.assertEqual(r["model"], "claude-opus-4-8")
        self.assertEqual(r["max_tokens"], A.MAX_OUTPUT_TOKENS["claude-opus-4-8"])
        self.assertTrue(r["stream"])

    def test_unknown_model_default_maxtokens(self):
        r = A.translate_request({"model": "claude-future-9", "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]}]})
        self.assertEqual(r["max_tokens"], A.MAX_OUTPUT_DEFAULT)

    def test_instructions_to_system_with_cache(self):
        r = self.base(instructions="be terse",
                      input=[{"type": "message", "role": "user",
                              "content": [{"type": "input_text", "text": "hi"}]}])
        self.assertEqual(r["system"][0]["text"], "be terse")
        self.assertEqual(r["system"][-1]["cache_control"], EPH)

    def test_empty_instructions_no_system(self):
        r = self.base(instructions="   ", input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        self.assertNotIn("system", r)

    def test_user_and_assistant_text(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]}])
        self.assertEqual([m["role"] for m in r["messages"]], ["user", "assistant"])
        self.assertEqual(r["messages"][0]["content"][0], {"type": "text", "text": "q",
                                                          **({"cache_control": EPH} if False else {})})
        self.assertEqual(r["messages"][1]["content"][0]["text"], "a")

    def test_function_call_to_tool_use(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "shell", "arguments": '{"command":["ls"]}',
             "call_id": "toolu_1"}])
        tu = r["messages"][1]["content"][0]
        self.assertEqual(tu["type"], "tool_use")
        self.assertEqual(tu["id"], "toolu_1")
        self.assertEqual(tu["name"], "shell")
        self.assertEqual(tu["input"], {"command": ["ls"]})

    def test_function_call_output_string(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "s", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": "done"}])
        tr = r["messages"][-1]["content"][0]
        self.assertEqual(tr, {"type": "tool_result", "tool_use_id": "c1", "content": "done",
                              "cache_control": EPH})

    def test_function_call_output_content_items(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "s", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_text", "text": "line1"},
                {"type": "input_image", "image_url": "https://x/y.png"},
                {"type": "encrypted_content", "encrypted_content": "opaque"}]}])
        content = r["messages"][-1]["content"][0]["content"]
        # text + image kept; encrypted_content dropped
        self.assertEqual(content[0], {"type": "text", "text": "line1"})
        self.assertEqual(content[1]["type"], "image")
        self.assertEqual(len(content), 2)

    def test_parallel_tool_calls_merge_one_assistant_turn(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "a", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call", "name": "b", "arguments": "{}", "call_id": "c2"}])
        assistant = r["messages"][1]
        self.assertEqual(assistant["role"], "assistant")
        self.assertEqual([b["type"] for b in assistant["content"]], ["tool_use", "tool_use"])
        self.assertEqual([b["id"] for b in assistant["content"]], ["c1", "c2"])

    def test_parallel_tool_outputs_merge_one_user_turn(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "a", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call", "name": "b", "arguments": "{}", "call_id": "c2"},
            {"type": "function_call_output", "call_id": "c1", "output": "r1"},
            {"type": "function_call_output", "call_id": "c2", "output": "r2"}])
        user_turn = r["messages"][2]
        self.assertEqual(user_turn["role"], "user")
        self.assertEqual([b["tool_use_id"] for b in user_turn["content"]], ["c1", "c2"])

    def test_image_data_url(self):
        r = self.base(input=[{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/jpeg;base64,QUJD"}]}])
        img = r["messages"][0]["content"][0]
        self.assertEqual(img["type"], "image")
        self.assertEqual(img["source"], {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"})

    def test_image_http_url(self):
        r = self.base(input=[{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "https://x/y.png"}]}])
        img = r["messages"][0]["content"][0]
        self.assertEqual(img["source"], {"type": "url", "url": "https://x/y.png"})

    def test_tools_to_input_schema_with_cache(self):
        r = self.base(
            input=[{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]}],
            tools=[{"type": "function", "name": "shell", "description": "run", "strict": False,
                    "parameters": {"type": "object", "properties": {"c": {"type": "string"}}}}])
        t = r["tools"][0]
        self.assertEqual(t["name"], "shell")
        self.assertEqual(t["input_schema"], {"type": "object", "properties": {"c": {"type": "string"}}})
        self.assertNotIn("strict", t)
        self.assertEqual(t["cache_control"], EPH)          # last tool cached
        self.assertEqual(r["tool_choice"], {"type": "auto"})

    def test_custom_freeform_tool_string_schema(self):
        r = self.base(
            input=[{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]}],
            tools=[{"type": "custom", "name": "apply_patch", "description": "patch"}])
        self.assertEqual(r["tools"][0]["input_schema"]["properties"], {"input": {"type": "string"}})

    def test_cache_control_order_and_rolling(self):
        # BPs land on last tool, last system block, and last message's last block.
        r = self.base(
            instructions="sys",
            input=[{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            tools=[{"type": "function", "name": "a", "description": "", "parameters": {"type": "object"}},
                   {"type": "function", "name": "b", "description": "", "parameters": {"type": "object"}}])
        self.assertNotIn("cache_control", r["tools"][0])      # only LAST tool
        self.assertEqual(r["tools"][1]["cache_control"], EPH)
        self.assertEqual(r["system"][-1]["cache_control"], EPH)
        self.assertEqual(r["messages"][-1]["content"][-1]["cache_control"], EPH)

    def test_reasoning_dropped(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [], "encrypted_content": "blob"}])
        # only the user message survives
        self.assertEqual(len(r["messages"]), 1)
        self.assertEqual(r["messages"][0]["role"], "user")

    def test_compaction_becomes_text_not_dropped(self):
        r = self.base(input=[
            {"type": "compaction", "encrypted_content": "blob"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "now"}]}])
        # compaction preserved as a (user) text marker, not silently dropped
        joined = json.dumps(r["messages"])
        self.assertIn("compacted", joined)

    def test_local_shell_and_custom_and_toolsearch_covered(self):
        # None of these should raise; all become tool_use/tool_result blocks.
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]},
            {"type": "local_shell_call", "call_id": "c1", "status": "completed",
             "action": {"type": "exec", "command": ["ls"]}},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
            {"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** patch"},
            {"type": "custom_tool_call_output", "call_id": "c2", "output": "applied"}])
        blob = json.dumps(r["messages"])
        self.assertIn("tool_use", blob)
        self.assertIn("apply_patch", blob)

    def test_web_search_call_becomes_text(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]},
            {"type": "web_search_call", "status": "completed",
             "action": {"type": "search", "query": "weather"}}])
        self.assertIn("web search performed: weather", json.dumps(r["messages"]))

    def test_unknown_item_hard_fails(self):
        with self.assertRaises(A.UnsupportedItemError):
            self.base(input=[
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]},
                {"type": "some_future_item", "data": 1}])

    def test_first_message_forced_user(self):
        # history starting with an assistant item gets a synthetic leading user turn
        r = self.base(input=[
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]}])
        self.assertEqual(r["messages"][0]["role"], "user")


# ============================ SSE transduction ============================= #
class TestTransducer(unittest.TestCase):
    def _msg_start(self, **usage):
        u = {"input_tokens": 0, "cache_read_input_tokens": 0,
             "cache_creation_input_tokens": 0, "output_tokens": 1}
        u.update(usage)
        return {"type": "message_start", "message": {"id": "msg_1", "usage": u}}

    def test_thinking_buffered_not_streamed(self):
        # Extended thinking, when present, is captured into output_items as a
        # reasoning item but NEVER streamed to the
        # engine; the real answer still streams normally.
        t, evs = run([
            self._msg_start(input_tokens=10),
            {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "thinking_delta", "thinking": "let me "}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "thinking_delta", "thinking": "reason"}},
            {"type": "content_block_stop", "index": 0},
            {"type": "content_block_start", "index": 1, "content_block": {"type": "text"}},
            {"type": "content_block_delta", "index": 1,
             "delta": {"type": "text_delta", "text": "answer"}},
            {"type": "content_block_stop", "index": 1},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}},
            {"type": "message_stop"}])
        reasoning = [i for i in t.output_items if i.get("type") == "reasoning"]
        self.assertEqual(len(reasoning), 1)
        self.assertEqual(reasoning[0]["summary"][0]["text"], "let me reason")
        streamed = [e.get("item", {}).get("type") for e in evs if "item" in e]
        self.assertNotIn("reasoning", streamed)   # not leaked to the engine
        self.assertIn("message", streamed)         # answer still streams

    def test_text_turn_added_before_delta(self):
        _t, evs = run([
            self._msg_start(input_tokens=10),
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": "hello"}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}},
            {"type": "message_stop"}])
        types = [e["type"] for e in evs]
        self.assertEqual(types[0], "response.created")
        i_add = types.index("response.output_item.added")
        i_delta = types.index("response.output_text.delta")
        i_done = types.index("response.output_item.done")
        self.assertLess(i_add, i_delta)                 # added BEFORE delta (else engine panics)
        self.assertLess(i_delta, i_done)
        self.assertEqual(types[-1], "response.completed")
        done = evs[i_done]["item"]
        self.assertEqual(done, {"type": "message", "role": "assistant",
                                "content": [{"type": "output_text", "text": "hello"}]})

    def test_tool_use_turn_buffered(self):
        _t, evs = run([
            self._msg_start(input_tokens=10),
            {"type": "content_block_start", "index": 0,
             "content_block": {"type": "tool_use", "id": "toolu_9", "name": "shell"}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "input_json_delta", "partial_json": '{"command":'}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "input_json_delta", "partial_json": '["ls"]}'}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}},
            {"type": "message_stop"}])
        # no output_text.delta for a tool call; function_call delivered via item.done
        self.assertNotIn("response.output_text.delta", [e["type"] for e in evs])
        fc = [e["item"] for e in evs if e["type"] == "response.output_item.done"][0]
        self.assertEqual(fc, {"type": "function_call", "name": "shell",
                              "arguments": '{"command":["ls"]}', "call_id": "toolu_9"})

    def test_multiple_tool_use(self):
        _t, evs = run([
            self._msg_start(),
            {"type": "content_block_start", "index": 0,
             "content_block": {"type": "tool_use", "id": "t1", "name": "a"}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "input_json_delta", "partial_json": "{}"}},
            {"type": "content_block_stop", "index": 0},
            {"type": "content_block_start", "index": 1,
             "content_block": {"type": "tool_use", "id": "t2", "name": "b"}},
            {"type": "content_block_delta", "index": 1,
             "delta": {"type": "input_json_delta", "partial_json": "{}"}},
            {"type": "content_block_stop", "index": 1},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}},
            {"type": "message_stop"}])
        calls = [e["item"]["call_id"] for e in evs if e["type"] == "response.output_item.done"]
        self.assertEqual(calls, ["t1", "t2"])

    def test_usage_synth_complete_and_openai_shaped(self):
        t, evs = run([
            self._msg_start(input_tokens=10, cache_read_input_tokens=100,
                            cache_creation_input_tokens=20),
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 42}},
            {"type": "message_stop"}])
        u = evs[-1]["response"]["usage"]
        # OpenAI convention: input_tokens includes cache read+write
        self.assertEqual(u["input_tokens"], 130)
        self.assertEqual(u["input_tokens_details"]["cached_tokens"], 100)
        self.assertEqual(u["cache_write_tokens"], 20)
        self.assertEqual(u["output_tokens"], 42)
        self.assertEqual(u["total_tokens"], 172)   # all 3 present -> engine won't error
        for k in ("input_tokens", "output_tokens", "total_tokens"):
            self.assertIn(k, u)

    def test_end_turn_true_when_no_tool(self):
        _t, evs = run([self._msg_start(),
                       {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                       {"type": "content_block_stop", "index": 0},
                       {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                        "usage": {"output_tokens": 3}},
                       {"type": "message_stop"}])
        self.assertTrue(evs[-1]["response"]["end_turn"])

    def test_end_turn_false_when_tool_use(self):
        _t, evs = run([self._msg_start(),
                       {"type": "content_block_start", "index": 0,
                        "content_block": {"type": "tool_use", "id": "t", "name": "a"}},
                       {"type": "content_block_stop", "index": 0},
                       {"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                        "usage": {"output_tokens": 3}},
                       {"type": "message_stop"}])
        self.assertFalse(evs[-1]["response"]["end_turn"])

    def test_error_midstream_emits_failed_not_hang(self):
        _t, evs = run([self._msg_start(),
                       {"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}])
        types = [e["type"] for e in evs]
        self.assertIn("response.failed", types)
        self.assertEqual(evs[-1]["response"]["error"]["code"], "server_is_overloaded")

    def test_socket_drop_before_message_stop_synthesizes_terminal(self):
        # stream ends after some content, no message_stop -> close() must terminate
        _t, evs = run([self._msg_start(),
                       {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                       {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": "partial"}}])
        # last event is a terminal (failed), never left hanging
        self.assertEqual(evs[-1]["type"], "response.failed")
        self.assertEqual(evs[-1]["response"]["error"]["code"], "stream_interrupted")

    def test_created_always_precedes_terminal(self):
        # even a bare error before any message_start still yields created+failed
        _t, evs = run([{"type": "error", "error": {"type": "api_error", "message": "x"}}])
        self.assertEqual([e["type"] for e in evs], ["response.created", "response.failed"])

    def test_ping_ignored(self):
        _t, evs = run([self._msg_start(),
                       {"type": "ping"},
                       {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                        "usage": {"output_tokens": 1}},
                       {"type": "message_stop"}])
        self.assertNotIn("ping", [e["type"] for e in evs])
        self.assertEqual(evs[-1]["type"], "response.completed")


class TestIterSSE(unittest.TestCase):
    def test_parses_event_data_frames_split_across_chunks(self):
        raw = (b'event: message_start\ndata: {"type":"message_start",'
               b'"message":{"id":"m"}}\n\n'
               b'event: ping\ndata: {"type": "ping"}\n\n')
        # feed in awkward 7-byte chunks to exercise buffering
        chunks = [raw[i:i + 7] for i in range(0, len(raw), 7)]
        evs = list(A.iter_sse(iter(chunks)))
        self.assertEqual(evs[0]["type"], "message_start")
        self.assertEqual(evs[0]["message"]["id"], "m")
        self.assertEqual(evs[1]["type"], "ping")

    def test_trailing_event_without_blank_line(self):
        evs = list(A.iter_sse(iter([b'data: {"type":"message_stop"}'])))
        self.assertEqual(evs[-1]["type"], "message_stop")

    def test_malformed_data_skipped(self):
        evs = list(A.iter_sse(iter([b'data: not-json\n\ndata: {"type":"ping"}\n\n'])))
        self.assertEqual(evs, [{"type": "ping"}])


# ============================ --output-schema ============================== #
# The engine serializes `--output-schema` into the Responses `text` param as
#   {"format": {"type":"json_schema","strict":bool,"schema":{...},
#               "name":"codex_output_schema"}}
# (codex-api::create_text_param_for_request). This translator used to ignore it,
# so the flag was accepted and discarded with no error.
#
# Mapped to Anthropic's NATIVE structured outputs (`output_config.format`).
# PROBED LIVE 2026-07-27 on claude-sonnet-4-6 under the pinned anthropic-version:
#   * works with NO anthropic-beta header;
#   * the deprecated top-level `output_format` 400s and points here;
#   * the format object accepts ONLY {type, schema} -- `strict`/`name` are rejected;
#   * every object node needs `additionalProperties: false`, nested included;
#   * it coexists with tools, and a tool-forcing prompt still returns
#     stop_reason=tool_use, so the agent loop is NOT hijacked. That is why the
#     forced-single-tool workaround was rejected.
def text_param(schema, strict=True):
    return {"format": {"type": "json_schema", "strict": strict,
                       "schema": schema, "name": "codex_output_schema"}}


SCHEMA = {"type": "object", "additionalProperties": False,
          "properties": {"answer": {"type": "string"}},
          "required": ["answer"]}


class TestOutputSchema(unittest.TestCase):
    def base(self, **kw):
        body = {"model": "claude-sonnet-5", "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]}]}
        body.update(kw)
        return A.translate_request(body)

    def test_absent_when_not_requested(self):
        """The whole feature must be inert unless --output-schema was passed."""
        r = self.base()
        self.assertNotIn("format", r.get("output_config") or {})

    def test_schema_reaches_output_config(self):
        r = self.base(text=text_param(SCHEMA))
        self.assertEqual(r["output_config"]["format"],
                         {"type": "json_schema", "schema": SCHEMA})

    def test_effort_and_format_coexist(self):
        """REGRESSION: effort and format share `output_config`. Assigning a fresh
        dict for either one silently deletes the other."""
        r = self.base(text=text_param(SCHEMA), reasoning={"effort": "high"})
        self.assertEqual(r["output_config"]["effort"], "high")
        self.assertIn("format", r["output_config"])
        self.assertEqual(r["thinking"], {"type": "adaptive"})

    def test_format_without_effort_still_emitted(self):
        r = A.translate_request({"model": "claude-sonnet-5", "text": text_param(SCHEMA),
                                 "input": [{"type": "message", "role": "user",
                                            "content": [{"type": "input_text", "text": "hi"}]}]})
        self.assertIn("format", r["output_config"])

    def test_strict_and_name_are_never_sent(self):
        """Anthropic rejects them: 'output_config.format.strict: Extra inputs are
        not permitted' (probed live)."""
        r = self.base(text=text_param(SCHEMA))
        self.assertEqual(set(r["output_config"]["format"]), {"type", "schema"})

    def test_additional_properties_injected_at_root(self):
        open_schema = {"type": "object", "properties": {"a": {"type": "string"}}}
        r = self.base(text=text_param(open_schema))
        self.assertIs(r["output_config"]["format"]["schema"]["additionalProperties"], False)

    def test_additional_properties_injected_when_nested(self):
        nested = {"type": "object", "properties": {
            "rows": {"type": "array", "items": {
                "type": "object", "properties": {"f": {"type": "string"}}}}}}
        r = self.base(text=text_param(nested))
        s = r["output_config"]["format"]["schema"]
        self.assertIs(s["additionalProperties"], False)
        self.assertIs(s["properties"]["rows"]["items"]["additionalProperties"], False)

    def test_additional_properties_true_is_overridden(self):
        r = self.base(text=text_param({"type": "object", "additionalProperties": True,
                                       "properties": {"a": {"type": "string"}}}))
        self.assertIs(r["output_config"]["format"]["schema"]["additionalProperties"], False)

    def test_non_object_nodes_untouched(self):
        s = {"type": "object", "properties": {"n": {"type": "integer", "minimum": 0}}}
        r = self.base(text=text_param(s))
        self.assertEqual(r["output_config"]["format"]["schema"]["properties"]["n"],
                         {"type": "integer", "minimum": 0})

    def test_input_schema_not_mutated(self):
        """We must not rewrite the caller's dict in place -- the gateway logs the
        same body it forwards."""
        original = {"type": "object", "properties": {"a": {"type": "string"}}}
        snapshot = json.loads(json.dumps(original))
        self.base(text=text_param(original))
        self.assertEqual(original, snapshot)

    def test_coexists_with_tools(self):
        r = self.base(text=text_param(SCHEMA), tools=[
            {"type": "function", "name": "t", "description": "d",
             "parameters": {"type": "object", "properties": {}}}])
        self.assertIn("format", r["output_config"])
        self.assertEqual(r["tool_choice"], {"type": "auto"})
        self.assertEqual(r["tools"][0]["name"], "t")

    def test_malformed_format_is_ignored_not_raised(self):
        for bad in ({"format": {"type": "text"}},
                    {"format": {"type": "json_schema", "schema": "not-a-dict"}},
                    {"format": "nope"},
                    "nope"):
            r = self.base(text=bad)
            self.assertNotIn("format", r.get("output_config") or {})

    def test_close_objects_reports_what_it_changed(self):
        _, changed = A._close_objects({"type": "object", "properties": {
            "b": {"type": "object", "properties": {}}}})
        self.assertIn("<root>", changed)
        self.assertIn("properties.b", changed)
        self.assertEqual(A._close_objects(SCHEMA)[1], [])

    def test_output_schema_of_reads_strict(self):
        self.assertEqual(A.output_schema_of({"text": text_param(SCHEMA, strict=False)}),
                         (SCHEMA, False))
        self.assertEqual(A.output_schema_of({"text": text_param(SCHEMA)}), (SCHEMA, True))
        self.assertEqual(A.output_schema_of({}), (None, False))


if __name__ == "__main__":
    unittest.main()
