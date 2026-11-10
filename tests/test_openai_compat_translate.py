#!/usr/bin/env python3
"""Tests for openai_compat_translate — Responses <-> Chat Completions for
self-hosted / custom endpoints. Mirrors the anthropic/gemini translator rigor:
request translation, streamed transducer, degrade-unknown-history (the Codex
regression), reversible tool-name map, dangling-tool repair, and an e2e against
a mock chat-completions SSE server."""
import io
import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import openai_compat_translate as T


def frames_to_events(frames):
    """Parse the transducer's SSE frames into (event_type, data) pairs."""
    out = []
    for f in frames:
        etype = None
        data = None
        for ln in f.split("\n"):
            if ln.startswith("event: "):
                etype = ln[7:]
            elif ln.startswith("data: "):
                data = json.loads(ln[6:])
        if etype:
            out.append((etype, data))
    return out


class TestTranslateRequest(unittest.TestCase):
    def test_instructions_become_system(self):
        chat, _ = T.translate_request({"instructions": "be terse", "input": []}, "m")
        self.assertEqual(chat["messages"][0], {"role": "system", "content": "be terse"})

    def test_user_text_message(self):
        chat, _ = T.translate_request({"input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]}]}, "m")
        self.assertEqual(chat["messages"][0], {"role": "user", "content": "hi"})

    def test_image_content_becomes_parts(self):
        chat, _ = T.translate_request({"input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAA"}]}]}, "m")
        parts = chat["messages"][0]["content"]
        self.assertEqual(parts[0]["type"], "text")
        self.assertEqual(parts[1]["type"], "image_url")

    def test_function_call_roundtrip(self):
        chat, _ = T.translate_request({"input": [
            {"type": "function_call", "call_id": "c1", "name": "exec",
             "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "c1", "output": "a.txt"}]}, "m")
        asst = chat["messages"][0]
        self.assertEqual(asst["role"], "assistant")
        self.assertEqual(asst["tool_calls"][0]["id"], "c1")
        self.assertEqual(asst["tool_calls"][0]["function"]["name"], "exec")
        self.assertEqual(chat["messages"][1], {"role": "tool", "tool_call_id": "c1", "content": "a.txt"})

    def test_tools_schema_sanitized(self):
        chat, _ = T.translate_request({"input": [], "tools": [
            {"type": "function", "name": "t", "description": "d",
             "parameters": {"type": "object", "additionalProperties": False,
                            "properties": {"x": {"type": "string"}}}}]}, "m")
        self.assertNotIn("additionalProperties", json.dumps(chat["tools"]))
        self.assertEqual(chat["tools"][0]["function"]["name"], "t")

    def test_stream_options_present(self):
        chat, _ = T.translate_request({"input": []}, "m")
        self.assertTrue(chat["stream"])
        self.assertEqual(chat["stream_options"], {"include_usage": True})


class TestDegradeUnknownHistory(unittest.TestCase):
    """The Codex regression: a gpt/claude/gemini -> self/* switch resends history
    with items an open model never produced. These MUST degrade to text, not 400."""

    def _roles_ok(self, item):
        chat, _ = T.translate_request({"input": [item]}, "m")   # must NOT raise
        return chat["messages"]

    def test_tool_search_call_degrades(self):
        msgs = self._roles_ok({"type": "tool_search_call", "call_id": "x", "arguments": "{}"})
        self.assertTrue(any("tool search" in str(m.get("content", "")).lower() for m in msgs))

    def test_web_search_call_degrades(self):
        msgs = self._roles_ok({"type": "web_search_call", "action": {"query": "rust lifetimes"}})
        self.assertTrue(any("rust lifetimes" in str(m.get("content", "")) for m in msgs))

    def test_compaction_degrades(self):
        msgs = self._roles_ok({"type": "compaction"})
        self.assertTrue(any("summarized" in str(m.get("content", "")).lower() for m in msgs))

    def test_reasoning_dropped_silently(self):
        chat, _ = T.translate_request({"input": [{"type": "reasoning", "content": []}]}, "m")
        self.assertEqual(chat["messages"], [])

    def test_truly_unknown_type_degrades_not_raises(self):
        msgs = self._roles_ok({"type": "some_future_item_v9"})
        self.assertTrue(any("unsupported item" in str(m.get("content", "")).lower() for m in msgs))

    def test_malformed_body_raises(self):
        with self.assertRaises(T.UnsupportedItemError):
            T.translate_request("not a dict", "m")


class TestToolNameMap(unittest.TestCase):
    def test_invalid_name_sanitized_and_mapped(self):
        chat, nm = T.translate_request({"input": [], "tools": [
            {"type": "function", "name": "mcp__srv__do.thing", "parameters": {}}]}, "m")
        safe = chat["tools"][0]["function"]["name"]
        self.assertRegex(safe, r"^[a-zA-Z0-9_-]{1,64}$")
        self.assertEqual(nm[safe], "mcp__srv__do.thing")          # reverse map kept

    def test_valid_name_unchanged_no_map_entry(self):
        chat, nm = T.translate_request({"input": [], "tools": [
            {"type": "function", "name": "apply_patch", "parameters": {}}]}, "m")
        self.assertEqual(chat["tools"][0]["function"]["name"], "apply_patch")
        self.assertEqual(nm, {})

    def test_colliding_names_stay_distinct_and_reversible(self):
        # `a.b` and `a/b` both naively sanitize to `a_b`; the reverse map would
        # lie. They must get distinct safe names, both mapped.
        chat, nm = T.translate_request({"input": [], "tools": [
            {"type": "function", "name": "a.b", "parameters": {}},
            {"type": "function", "name": "a/b", "parameters": {}}]}, "m")
        names = [t["function"]["name"] for t in chat["tools"]]
        self.assertEqual(len(set(names)), 2)                    # distinct
        self.assertEqual({nm[n] for n in names}, {"a.b", "a/b"})  # reverse correctly

    def test_sanitize_is_deterministic(self):
        self.assertEqual(T.sanitize_tool_name("mcp__s__do.thing"),
                         T.sanitize_tool_name("mcp__s__do.thing"))

    def test_transducer_reverses_the_name(self):
        # model streams the SANITIZED name; the Responses function_call must carry the original.
        td = T.Transducer("m", name_map={"mcp_srv_do_thing": "mcp__srv__do.thing"})
        frames = td.feed({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {
            "tool_calls": [{"index": 0, "id": "c9", "type": "function",
                            "function": {"name": "mcp_srv_do_thing", "arguments": "{}"}}]},
            "finish_reason": "tool_calls"}]})
        frames += td.close()
        evs = frames_to_events(frames)
        fc = next(d["item"] for e, d in evs if e == "response.output_item.done"
                  and d["item"]["type"] == "function_call")
        self.assertEqual(fc["name"], "mcp__srv__do.thing")


class TestRepairDanglingTool(unittest.TestCase):
    def test_dangling_call_gets_synth_result(self):
        chat, _ = T.translate_request({"input": [
            {"type": "function_call", "call_id": "d1", "name": "exec", "arguments": "{}"},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "never mind"}]}]}, "m")
        # a tool message answering d1 must be synthesized before the user turn
        tool_msgs = [m for m in chat["messages"] if m.get("role") == "tool"]
        self.assertEqual(tool_msgs[0]["tool_call_id"], "d1")

    def test_answered_call_not_double_repaired(self):
        chat, _ = T.translate_request({"input": [
            {"type": "function_call", "call_id": "d2", "name": "exec", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "d2", "output": "done"}]}, "m")
        self.assertEqual(len([m for m in chat["messages"] if m.get("role") == "tool"]), 1)


class TestMergeParallelToolCalls(unittest.TestCase):
    """Parallel tool calls must land in ONE assistant message, the shape OpenAI
    itself emits. The split shape (one assistant message per call) is refused by
    Fireworks' Kimi K3: "tool messages need a resolvable tool name: carry
    `tool`/`name`, or match a preceding assistant tool_call by order."
    Probed live 2026-08-05 against all six Fireworks models."""

    PARALLEL = [
        {"type": "function_call", "call_id": "p1", "name": "get_weather",
         "arguments": '{"city":"Paris"}'},
        {"type": "function_call", "call_id": "p2", "name": "get_time",
         "arguments": '{"city":"Paris"}'},
        {"type": "function_call_output", "call_id": "p1", "output": "18C"},
        {"type": "function_call_output", "call_id": "p2", "output": "14:00"},
    ]

    def test_parallel_calls_share_one_assistant_message(self):
        chat, _ = T.translate_request({"input": self.PARALLEL}, "m")
        asst = [m for m in chat["messages"] if m.get("tool_calls")]
        self.assertEqual(len(asst), 1, "parallel calls must not split across messages")
        self.assertEqual([tc["id"] for tc in asst[0]["tool_calls"]], ["p1", "p2"])
        self.assertEqual([tc["function"]["name"] for tc in asst[0]["tool_calls"]],
                         ["get_weather", "get_time"])

    def test_every_call_still_answered_exactly_once(self):
        chat, _ = T.translate_request({"input": self.PARALLEL}, "m")
        tools = [m for m in chat["messages"] if m.get("role") == "tool"]
        self.assertEqual([m["tool_call_id"] for m in tools], ["p1", "p2"])

    def test_partially_answered_parallel_turn_is_repaired(self):
        """An aborted parallel turn: p2 has no output. The placeholder lands after
        the merged block, i.e. out of call order — accepted, because once merged
        Kimi K3 resolves by tool_call_id (a reversed pair was verified 200)."""
        chat, _ = T.translate_request({"input": self.PARALLEL[:3]}, "m")
        asst = [m for m in chat["messages"] if m.get("tool_calls")]
        self.assertEqual(len(asst), 1)
        answered = {m["tool_call_id"] for m in chat["messages"] if m.get("role") == "tool"}
        self.assertEqual(answered, {"p1", "p2"})

    def test_assistant_text_between_calls_breaks_the_run(self):
        """Only content-less tool-call messages merge. Real assistant text must
        never be folded into a tool-call block or the turn order is rewritten."""
        chat, _ = T.translate_request({"input": [
            {"type": "function_call", "call_id": "s1", "name": "a", "arguments": "{}"},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "thinking"}]},
            {"type": "function_call", "call_id": "s2", "name": "b", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "s1", "output": "x"},
            {"type": "function_call_output", "call_id": "s2", "output": "y"}]}, "m")
        asst = [m for m in chat["messages"] if m.get("tool_calls")]
        self.assertEqual(len(asst), 2)
        self.assertEqual([tc["id"] for m in asst for tc in m["tool_calls"]], ["s1", "s2"])

    def test_single_call_turn_is_unchanged(self):
        chat, _ = T.translate_request({"input": [
            {"type": "function_call", "call_id": "o1", "name": "a", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "o1", "output": "x"}]}, "m")
        asst = [m for m in chat["messages"] if m.get("tool_calls")]
        self.assertEqual(len(asst), 1)
        self.assertEqual(len(asst[0]["tool_calls"]), 1)

    def test_merge_does_not_mutate_the_input_messages(self):
        """The merge copies before extending; aliasing would corrupt the list the
        caller still holds."""
        msgs = [{"role": "assistant", "content": None,
                 "tool_calls": [{"id": "m1", "type": "function",
                                 "function": {"name": "a", "arguments": "{}"}}]},
                {"role": "assistant", "content": None,
                 "tool_calls": [{"id": "m2", "type": "function",
                                 "function": {"name": "b", "arguments": "{}"}}]}]
        T._merge_tool_call_runs(msgs)
        self.assertEqual([len(m["tool_calls"]) for m in msgs], [1, 1])


class TestTransducer(unittest.TestCase):
    def _run(self, chunks):
        td = T.Transducer("m")
        frames = []
        for c in chunks:
            frames += td.feed(c)
        frames += td.close()
        return frames_to_events(frames)

    def test_text_stream(self):
        evs = self._run([
            {"choices": [{"index": 0, "delta": {"content": "Hel"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "stop"}]},
            {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}},
        ])
        types = [e for e, _ in evs]
        self.assertEqual(types[0], "response.created")
        self.assertIn("response.output_item.added", types)
        deltas = "".join(d["delta"] for e, d in evs if e == "response.output_text.delta")
        self.assertEqual(deltas, "Hello")
        self.assertEqual(types[-1], "response.completed")
        usage = next(d["response"]["usage"] for e, d in evs if e == "response.completed")
        self.assertEqual((usage["input_tokens"], usage["output_tokens"]), (10, 2))

    def test_streamed_tool_call_assembled(self):
        evs = self._run([
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "type": "function",
                 "function": {"name": "exec", "arguments": "{\"cmd\":"}}]}}]},
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"ls\"}"}}]}, "finish_reason": "tool_calls"}]},
        ])
        fc = next(d["item"] for e, d in evs if e == "response.output_item.done"
                  and d["item"].get("type") == "function_call")
        self.assertEqual(fc["name"], "exec")
        self.assertEqual(json.loads(fc["arguments"]), {"cmd": "ls"})
        self.assertEqual(fc["call_id"], "c1")

    def test_mid_stream_error_fails_cleanly(self):
        evs = self._run([{"error": {"message": "OOM on GPU", "code": "server_error"}}])
        self.assertEqual(evs[-1][0], "response.failed")
        self.assertIn("OOM", evs[-1][1]["response"]["error"]["message"])

    def test_empty_stream_fails_not_hangs(self):
        evs = self._run([])
        self.assertEqual(evs[-1][0], "response.failed")

    def test_full_nonstream_completion_handled(self):
        # server ignored stream:true and returned one full object
        td = T.Transducer("m")
        frames = td.feed({"object": "chat.completion", "choices": [
            {"index": 0, "message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})
        evs = frames_to_events(frames)
        self.assertEqual(evs[-1][0], "response.completed")
        self.assertIn("done", "".join(
            d["delta"] for e, d in evs if e == "response.output_text.delta"))

    def test_reasoning_content_buffered_not_streamed(self):
        # Reasoning models (gpt-oss/deepseek/glm on Fireworks) stream CoT in
        # reasoning_content; buffer it into an output_items-only reasoning item NEVER
        # streamed to the engine, while the visible answer (content) still renders.
        td = T.Transducer("m")
        frames = []
        for c in [
            {"choices": [{"index": 0, "delta": {"reasoning_content": "let me "}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {"reasoning_content": "think"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {"content": "answer"}, "finish_reason": "stop"}]},
        ]:
            frames += td.feed(c)
        frames += td.close()
        evs = frames_to_events(frames)
        # not streamed: no output_item.done frame carries a reasoning item
        for e, d in evs:
            if e == "response.output_item.done":
                self.assertNotEqual(d["item"].get("type"), "reasoning")
        # captured: output_items carry ONE reasoning item with the full CoT
        r = [i for i in td.output_items if i.get("type") == "reasoning"]
        self.assertEqual(len(r), 1)
        self.assertEqual(r[0]["summary"][0]["text"], "let me think")
        # the visible answer still renders to the client
        self.assertEqual("answer", "".join(
            d["delta"] for e, d in evs if e == "response.output_text.delta"))

    def test_output_items_captured_in_order(self):
        # The Transducer accumulates completed output items (assistant text THEN
        # tool calls) so a caller can read the model OUTPUT once the stream ends.
        # byok_sidecar never reads it.
        td = T.Transducer("m")
        for c in [
            {"choices": [{"index": 0, "delta": {"content": "Hi"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "type": "function",
                 "function": {"name": "exec", "arguments": "{\"cmd\": \"ls\"}"}}]},
                "finish_reason": "tool_calls"}]},
        ]:
            td.feed(c)
        td.close()
        self.assertEqual([i["type"] for i in td.output_items], ["message", "function_call"])
        self.assertEqual(td.output_items[0]["content"][0]["text"], "Hi")
        self.assertEqual(td.output_items[1]["name"], "exec")
        self.assertEqual(json.loads(td.output_items[1]["arguments"]), {"cmd": "ls"})


class TestIterSSE(unittest.TestCase):
    def test_parses_frames_and_skips_done(self):
        raw = (b'data: {"choices":[{"delta":{"content":"a"}}]}\n\n'
               b'data: {"choices":[{"delta":{"content":"b"}}]}\n\n'
               b'data: [DONE]\n\n')
        chunks = list(T.iter_sse(iter([raw[:20], raw[20:41], raw[41:]])))  # boundary-straddling
        self.assertEqual(len(chunks), 2)                      # [DONE] skipped
        self.assertEqual(chunks[0]["choices"][0]["delta"]["content"], "a")

    def test_tolerates_crlf(self):
        chunks = list(T.iter_sse(iter([b'data: {"choices":[]}\r\n\r\n'])))
        self.assertEqual(len(chunks), 1)


# --- e2e against a mock chat-completions SSE server -------------------------- #
_MOCK_SSE = (
    b'data: {"choices":[{"index":0,"delta":{"content":"Let me "},"finish_reason":null}]}\n\n'
    b'data: {"choices":[{"index":0,"delta":{"content":"edit."},"finish_reason":null}]}\n\n'
    b'data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function",'
    b'"function":{"name":"apply_patch","arguments":"{\\"patch\\":\\"x\\"}"}}]},"finish_reason":"tool_calls"}]}\n\n'
    b'data: {"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":8,"total_tokens":58}}\n\n'
    b'data: [DONE]\n\n'
)


class _MockChatServer(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(_MOCK_SSE)))
        self.end_headers()
        self.wfile.write(_MOCK_SSE)

    def log_message(self, *a):
        pass


class TestEndToEnd(unittest.TestCase):
    def setUp(self):
        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), _MockChatServer)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.port = self.srv.server_address[1]

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()

    def test_full_turn_translates_and_transduces(self):
        import http.client
        req = {"instructions": "code", "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "fix it"}]}],
            "tools": [{"type": "function", "name": "apply_patch",
                       "parameters": {"type": "object", "properties": {"patch": {"type": "string"}}}}]}
        chat_body, name_map = T.translate_request(req, "Qwen2.5-Coder-32B")
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.request("POST", "/v1/chat/completions", body=json.dumps(chat_body).encode(),
                     headers={"Content-Type": "application/json"})
        resp = conn.getresponse()
        self.assertEqual(resp.status, 200)
        td = T.Transducer("Qwen2.5-Coder-32B", name_map)
        frames = []
        for ev in T.iter_sse(iter(lambda: resp.read(2048), b"")):
            frames += td.feed(ev)
        frames += td.close()
        conn.close()
        evs = frames_to_events(frames)
        types = [e for e, _ in evs]
        self.assertEqual(types[0], "response.created")
        self.assertEqual(types[-1], "response.completed")
        text = "".join(d["delta"] for e, d in evs if e == "response.output_text.delta")
        self.assertEqual(text, "Let me edit.")
        fc = next(d["item"] for e, d in evs if e == "response.output_item.done"
                  and d["item"].get("type") == "function_call")
        self.assertEqual(fc["name"], "apply_patch")
        self.assertEqual(json.loads(fc["arguments"]), {"patch": "x"})
        usage = next(d["response"]["usage"] for e, d in evs if e == "response.completed")
        self.assertEqual((usage["input_tokens"], usage["output_tokens"]), (50, 8))


class _RecoveryBase(unittest.TestCase):
    """Helpers for the works-with-any-model hardening tests."""

    def run_chunks(self, chunks, known=None, custom=None, name_map=None):
        td = T.Transducer("m", name_map=name_map, known_tools=known,
                          custom_tools=custom)
        frames = []
        for c in chunks:
            frames += td.feed(c)
        frames += td.close()
        return frames_to_events(frames)

    def run_text(self, text, known=None, custom=None, split=1, finish="stop"):
        """Stream ``text`` as content deltas of ``split`` chars each."""
        parts = [text[i:i + split] for i in range(0, len(text), split)] or [""]
        chunks = [{"choices": [{"index": 0, "delta": {"content": p},
                                "finish_reason": None}]} for p in parts]
        chunks.append({"choices": [{"index": 0, "delta": {},
                                    "finish_reason": finish}]})
        return self.run_chunks(chunks, known=known, custom=custom)

    def calls(self, evs):
        return [d["item"] for e, d in evs if e == "response.output_item.done"
                and d["item"].get("type") in ("function_call", "custom_tool_call")]

    def text(self, evs):
        return "".join(d["delta"] for e, d in evs if e == "response.output_text.delta")


class TestTextLeakRecovery(_RecoveryBase):
    """Models that emit their tool call as plain text (observed in the wild) must
    still produce a real call — gated on the DECLARED tools."""

    def test_ollama_3b_fenced_json_leak(self):
        # EXACT shape observed from qwen2.5-coder:3b via Ollama.
        leak = '```json\n{\n  "name": "shell",\n  "arguments": {\n    "command": "ls"\n  }\n}\n```'
        evs = self.run_text(leak, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(len(c), 1)
        self.assertEqual(c[0]["name"], "shell")
        self.assertEqual(json.loads(c[0]["arguments"]), {"command": "ls"})
        self.assertEqual(self.text(evs), "")             # raw JSON never shown as text
        self.assertEqual(evs[-1][0], "response.completed")
        self.assertFalse(evs[-1][1]["response"]["end_turn"])   # tool call -> turn continues

    def test_vllm_xml_tag_leak(self):
        # EXACT shape observed from Qwen2.5-Coder-7B via vLLM (default template).
        leak = '<xml>\n  {\n    "name": "get_time",\n    "arguments": {}\n  }\n</xml>'
        evs = self.run_text(leak, known={"get_time"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["get_time"])

    def test_vllm_tool_tag_leak(self):
        # EXACT shape observed from Qwen2.5-Coder-7B via vLLM (hermes template).
        leak = '<tool>\n    {\n        "name": "get_time",\n        "arguments": {}\n    }\n</tool>'
        evs = self.run_text(leak, known={"get_time"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["get_time"])

    def test_hermes_tool_call_tag_leak(self):
        leak = '<tool_call>\n{"name": "shell", "arguments": {"command": "pwd"}}\n</tool_call>'
        evs = self.run_text(leak, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(c[0]["name"], "shell")

    def test_response_tag_leak_unclosed(self):
        # Streamed leak that never closes its tag (observed mid-stream shape).
        leak = '<response>\n{"name": "get_time", "arguments": {}}'
        evs = self.run_text(leak, known={"get_time"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["get_time"])

    def test_mistral_tool_calls_marker(self):
        leak = '[TOOL_CALLS] [{"name": "shell", "arguments": {"command": "ls"}}]'
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["shell"])

    def test_bare_json_whole_message(self):
        leak = '{"name": "shell", "arguments": {"command": "ls -la"}}'
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["shell"])

    def test_mixed_prose_then_tag(self):
        msg = 'Let me check the time. <tool_call>{"name": "get_time", "arguments": {}}</tool_call>'
        evs = self.run_text(msg, known={"get_time"}, split=7)
        self.assertEqual(self.text(evs), "Let me check the time. ")
        self.assertEqual([c["name"] for c in self.calls(evs)], ["get_time"])

    def test_two_tag_blocks_two_calls(self):
        msg = ('<tool_call>{"name": "shell", "arguments": {"command": "ls"}}</tool_call>'
               '<tool_call>{"name": "get_time", "arguments": {}}</tool_call>')
        evs = self.run_text(msg, known={"shell", "get_time"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["shell", "get_time"])

    def test_parameters_key_variant(self):
        leak = '{"name": "shell", "parameters": {"command": "ls"}}'
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual(json.loads(self.calls(evs)[0]["arguments"]), {"command": "ls"})

    def test_case_insensitive_name_match(self):
        leak = '{"name": "Shell", "arguments": {}}'
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["shell"])

    def test_vllm_fenced_pure_xml_functioncall(self):
        # EXACT class observed LIVE from Qwen2.5-Coder-7B via vLLM: a fenced xml
        # block with XML-STRUCTURED elements — no JSON anywhere.
        leak = ('```xml\n<FunctionCall>\n    <name>get_time</name>\n'
                '    <arguments></arguments>\n</FunctionCall>\n```')
        evs = self.run_text(leak, known={"get_time"})
        c = self.calls(evs)
        self.assertEqual([x["name"] for x in c], ["get_time"])
        self.assertEqual(json.loads(c[0]["arguments"]), {})

    def test_pure_xml_kv_arguments(self):
        leak = ('<FunctionCall>\n  <name>shell</name>\n'
                '  <command>ls -la</command>\n</FunctionCall>')
        evs = self.run_text(leak, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(c[0]["name"], "shell")
        self.assertEqual(json.loads(c[0]["arguments"]), {"command": "ls -la"})

    def test_invoke_name_attr_with_parameters(self):
        # Claude-style leak: <invoke name="shell"><parameter name="command">ls</parameter>
        leak = ('<invoke name="shell"><parameter name="command">ls</parameter></invoke>')
        evs = self.run_text(leak, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(c[0]["name"], "shell")
        self.assertEqual(json.loads(c[0]["arguments"]), {"command": "ls"})

    def test_xml_arguments_containing_json(self):
        leak = ('<tool_call><name>shell</name>'
                '<arguments>{"command": "pwd"}</arguments></tool_call>')
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual(json.loads(self.calls(evs)[0]["arguments"]), {"command": "pwd"})

    def test_xml_hallucinated_name_stays_text(self):
        leak = ('<FunctionCall><name>file_write</name>'
                '<filename>x.txt</filename></FunctionCall>')
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual(self.calls(evs), [])
        self.assertIn("file_write", self.text(evs))


class TestDegenerateCallEnvelope(_RecoveryBase):
    """A tool-call envelope with NO usable name is neither dispatchable nor prose.
    Live 2026-07-24: qwen2.5-coder:3b answered a plain "hi" (tools attached) with
    `{"name": null, "arguments": {}}`, which the gate rejected and the old fallback
    printed verbatim at the user. It must be dropped instead."""

    def _dropped(self, text, known={"shell", "get_time"}):
        evs = self.run_text(text, known=known)
        self.assertEqual(self.calls(evs), [], "must not dispatch a nameless call")
        return self.text(evs)

    def test_null_name_envelope_dropped(self):
        # The exact shape a small self-hosted model produced.
        self.assertEqual(self._dropped('{"name": null, "arguments": {}}'), "")

    def test_empty_name_envelope_dropped(self):
        self.assertEqual(self._dropped('{"name": "", "arguments": {}}'), "")

    def test_fenced_empty_name_envelope_dropped(self):
        leak = '```json\n{\n  "name": "",\n  "arguments": {}\n}\n```'
        self.assertEqual(self._dropped(leak), "")

    def test_name_only_envelope_dropped(self):
        self.assertEqual(self._dropped('{"name": null}'), "")

    def test_array_of_nameless_envelopes_dropped(self):
        self.assertEqual(self._dropped('[{"name": "", "arguments": {}}]'), "")

    def test_split_across_chunks_still_dropped(self):
        evs = self.run_text('{"name": null, "arguments": {}}', known={"shell"}, split=3)
        self.assertEqual(self.text(evs), "")

    # ---- the guards: dropping must stay narrow ---------------------------- #
    def test_hallucinated_named_tool_still_shown(self):
        # A REAL but undeclared name is honest signal — deliberately still text.
        out = self.run_text('{"name": "sayHello", "arguments": {"m": "hi"}}', known={"shell"})
        self.assertIn("sayHello", self.text(out))

    def test_legit_json_answer_with_name_field_kept(self):
        # A user asking for JSON must never have their answer swallowed.
        out = self.run_text('{"name": "Alice", "age": 30}', known={"shell"})
        self.assertIn('"Alice"', self.text(out))

    def test_empty_name_with_real_content_kept(self):
        # Empty name, but `value` is outside the envelope key set: it's data.
        out = self.run_text('{"name": "", "value": 3}', known={"shell"})
        self.assertIn('"value"', self.text(out))

    def test_bare_empty_object_kept(self):
        # No `name` key at all -> not an envelope; left alone (conservative).
        out = self.run_text('{}', known={"shell"})
        self.assertEqual(self.text(out).strip(), "{}")

    def test_declared_name_still_recovers_as_call(self):
        leak = '{"name": "shell", "arguments": {"command": "ls"}}'
        evs = self.run_text(leak, known={"shell"})
        self.assertEqual([c["name"] for c in self.calls(evs)], ["shell"])


class TestRecoveryFalsePositiveGuards(_RecoveryBase):
    def test_hallucinated_tool_stays_text(self):
        # The 3B hallucinated `file_write` (not declared): must NOT become a call.
        leak = '{"name": "file_write", "arguments": {"filename": "x", "content": "hi"}}'
        evs = self.run_text(leak, known={"shell", "apply_patch"})
        self.assertEqual(self.calls(evs), [])
        self.assertIn("file_write", self.text(evs))      # surfaced honestly as text

    def test_legit_json_answer_stays_text(self):
        evs = self.run_text('{"result": 42, "status": "ok"}', known={"shell"})
        self.assertEqual(self.calls(evs), [])
        self.assertIn('"result": 42', self.text(evs))

    def test_fenced_example_mid_prose_stays_text(self):
        msg = ('You can call it like this:\n```json\n{"name": "shell", "arguments": {}}\n```\n'
               'Hope that helps.')
        evs = self.run_text(msg, known={"shell"}, split=9)
        self.assertEqual(self.calls(evs), [])            # fence not at message start
        self.assertIn("Hope that helps.", self.text(evs))

    def test_structured_call_wins_over_leak(self):
        chunks = [
            {"choices": [{"index": 0, "delta": {"content": '{"name": "shell", "arguments": {}}'},
                          "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "shell",
                                                      "arguments": "{}"}}]},
                "finish_reason": "tool_calls"}]},
        ]
        evs = self.run_chunks(chunks, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(len(c), 1)                      # no duplicate from the leak
        self.assertEqual(c[0]["call_id"], "c1")

    def test_plain_text_still_streams(self):
        evs = self.run_text("Hello there, how can I help?", known={"shell"}, split=5)
        self.assertEqual(self.text(evs), "Hello there, how can I help?")
        self.assertEqual(self.calls(evs), [])


class TestThinkBlocks(_RecoveryBase):
    def test_think_block_stripped(self):
        evs = self.run_text("<think>I should greet politely.</think>Hello!", split=6)
        self.assertEqual(self.text(evs), "Hello!")

    def test_think_then_leaked_call(self):
        msg = '<think>need the time</think><tool_call>{"name": "get_time", "arguments": {}}</tool_call>'
        evs = self.run_text(msg, known={"get_time"}, split=8)
        self.assertEqual(self.text(evs), "")
        self.assertEqual([c["name"] for c in self.calls(evs)], ["get_time"])

    def test_unclosed_think_dropped(self):
        evs = self.run_text("<think>rambling forever", split=4)
        self.assertEqual(self.text(evs), "")             # think content never shown
        self.assertEqual(self.calls(evs), [])
        # the server DID finish the stream (finish_reason present) -> completed,
        # just with empty output (the engine tolerates an empty turn)
        self.assertEqual(evs[-1][0], "response.completed")


class TestStreamedCallRobustness(_RecoveryBase):
    def test_missing_index_args_continuation(self):
        # Some servers omit `index`: name-bearing delta starts the call, bare-args
        # deltas continue it. Must assemble ONE call, not two.
        chunks = [
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"id": "c1", "function": {"name": "shell", "arguments": '{"comm'}}]}}]},
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"function": {"arguments": 'and": "ls"}'}}]}, "finish_reason": "tool_calls"}]},
        ]
        evs = self.run_chunks(chunks, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(len(c), 1)
        self.assertEqual(json.loads(c[0]["arguments"]), {"command": "ls"})

    def test_legacy_function_call_delta(self):
        chunks = [
            {"choices": [{"index": 0, "delta": {"function_call": {"name": "shell",
                                                                  "arguments": '{"command":'}}}]},
            {"choices": [{"index": 0, "delta": {"function_call": {"arguments": ' "ls"}'}},
                          "finish_reason": "function_call"}]},
        ]
        evs = self.run_chunks(chunks, known={"shell"})
        c = self.calls(evs)
        self.assertEqual(len(c), 1)
        self.assertEqual(json.loads(c[0]["arguments"]), {"command": "ls"})
        self.assertFalse(evs[-1][1]["response"]["end_turn"])   # legacy finish honored


class TestCustomTools(_RecoveryBase):
    APPLY_PATCH_CUSTOM = {"type": "custom", "name": "apply_patch",
                          "description": "Edit files.",
                          "format": {"type": "grammar",
                                     "definition": "*** Begin Patch ... *** End Patch"}}

    def test_custom_tool_becomes_input_function(self):
        chat, nm = T.translate_request({"input": [], "tools": [self.APPLY_PATCH_CUSTOM]}, "m")
        f = chat["tools"][0]["function"]
        self.assertEqual(f["name"], "apply_patch")
        self.assertIn("input", f["parameters"]["properties"])
        self.assertIn("required", f["parameters"])
        self.assertIn("Begin Patch", f["description"])   # grammar folded into desc

    def test_custom_tool_names_helper(self):
        names = T.custom_tool_names([self.APPLY_PATCH_CUSTOM,
                                     {"type": "function", "name": "shell", "parameters": {}}])
        self.assertEqual(names, {"apply_patch"})

    def test_structured_call_comes_back_as_custom_tool_call(self):
        patch = "*** Begin Patch\n*** Add File: hi.txt\n+hi\n*** End Patch"
        chunks = [{"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "c1", "function": {
                "name": "apply_patch",
                "arguments": json.dumps({"input": patch})}}]},
            "finish_reason": "tool_calls"}]}]
        evs = self.run_chunks(chunks, known={"apply_patch"}, custom={"apply_patch"})
        c = self.calls(evs)
        self.assertEqual(c[0]["type"], "custom_tool_call")
        self.assertEqual(c[0]["input"], patch)           # raw input string, unwrapped

    def test_leaked_call_comes_back_as_custom_tool_call(self):
        patch = "*** Begin Patch\\n+hi\\n*** End Patch"
        leak = '<tool_call>{"name": "apply_patch", "arguments": {"input": "%s"}}</tool_call>' % patch
        evs = self.run_text(leak, known={"apply_patch"}, custom={"apply_patch"})
        c = self.calls(evs)
        self.assertEqual(c[0]["type"], "custom_tool_call")
        self.assertIn("Begin Patch", c[0]["input"])


# ============================ --output-schema ============================== #
# The engine serializes `--output-schema` into the Responses `text` param as
#   {"format": {"type":"json_schema","strict":bool,"schema":{...},
#               "name":"codex_output_schema"}}
# (codex-api::create_text_param_for_request). This translator used to ignore it,
# so the flag was accepted and discarded with no error -- on Fireworks AND on
# every `self/*` endpoint, since fireworks_translate.translate_request IS this one.
def text_param(schema, strict=True):
    return {"format": {"type": "json_schema", "strict": strict,
                       "schema": schema, "name": "codex_output_schema"}}


OSCHEMA = {"type": "object", "additionalProperties": False,
           "properties": {"answer": {"type": "string"}},
           "required": ["answer"]}


class TestOutputSchema(unittest.TestCase):
    def base(self, **kw):
        body = {"input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}]}
        body.update(kw)
        chat, _ = T.translate_request(body, "m")
        return chat

    def test_absent_when_not_requested(self):
        """The whole feature must be inert unless --output-schema was passed, so
        it cannot regress an existing self/* session."""
        self.assertNotIn("response_format", self.base())

    def test_schema_becomes_response_format(self):
        chat = self.base(text=text_param(OSCHEMA))
        self.assertEqual(chat["response_format"], {
            "type": "json_schema",
            "json_schema": {"name": T.OUTPUT_SCHEMA_NAME,
                            "schema": OSCHEMA, "strict": True}})

    def test_strict_false_is_forwarded_not_forced(self):
        chat = self.base(text=text_param(OSCHEMA, strict=False))
        self.assertIs(chat["response_format"]["json_schema"]["strict"], False)

    def test_schema_is_NOT_sanitized(self):
        """`_sanitize_schema` strips `additionalProperties` for picky vLLM *tool*
        parsers, but OpenAI-compatible strict mode REQUIRES it -- sanitizing the
        response schema would break the very thing being requested."""
        chat = self.base(text=text_param(OSCHEMA))
        self.assertIs(
            chat["response_format"]["json_schema"]["schema"]["additionalProperties"], False)

    def test_nested_schema_preserved_verbatim(self):
        nested = {"type": "object", "additionalProperties": False, "properties": {
            "rows": {"type": "array", "items": {
                "type": "object", "additionalProperties": False,
                "properties": {"f": {"type": "string"}}}}}}
        chat = self.base(text=text_param(nested))
        self.assertEqual(chat["response_format"]["json_schema"]["schema"], nested)

    def test_coexists_with_tools(self):
        chat = self.base(text=text_param(OSCHEMA), tools=[
            {"type": "function", "name": "t", "description": "d",
             "parameters": {"type": "object", "properties": {}}}])
        self.assertIn("response_format", chat)
        self.assertEqual(chat["tools"][0]["function"]["name"], "t")

    def test_malformed_format_is_ignored_not_raised(self):
        for bad in ({"format": {"type": "text"}},
                    {"format": {"type": "json_schema", "schema": "not-a-dict"}},
                    {"format": "nope"},
                    "nope"):
            self.assertNotIn("response_format", self.base(text=bad))

    def test_fireworks_shares_this_translator(self):
        """Fireworks routes through this exact function, so the fix covers it."""
        try:
            import fireworks_translate as F
        except Exception:
            self.skipTest("fireworks_translate unavailable")
        self.assertIs(F.translate_request, T.translate_request)

    def test_output_schema_of_reads_strict(self):
        self.assertEqual(T.output_schema_of({"text": text_param(OSCHEMA, strict=False)}),
                         (OSCHEMA, False))
        self.assertEqual(T.output_schema_of({"text": text_param(OSCHEMA)}), (OSCHEMA, True))
        self.assertEqual(T.output_schema_of({}), (None, False))


if __name__ == "__main__":
    unittest.main()
