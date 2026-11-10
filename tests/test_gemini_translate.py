#!/usr/bin/env python3
"""Tests for the Responses <-> Gemini generateContent translator."""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import gemini_translate as G


def frame_json(frame):
    return json.loads(frame.split("data: ", 1)[1])


def run(chunks, model="gemini-2.5-flash"):
    t = G.Transducer(model)
    frames = []
    for c in chunks:
        frames += t.feed(c)
    frames += t.close()
    return t, [frame_json(f) for f in frames]


# ============================ routing / request ============================ #
class TestRoute(unittest.TestCase):
    def test_is_gemini_model(self):
        self.assertTrue(G.is_gemini_model("gemini-2.5-pro"))
        self.assertTrue(G.is_gemini_model("gemini-3.5-flash"))
        self.assertFalse(G.is_gemini_model("gpt-5.4"))
        self.assertFalse(G.is_gemini_model(None))

    def test_route_path_and_key_header(self):
        path, headers = G.route("gemini-2.5-flash", "AIza-KEY")
        self.assertEqual(path, "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse")
        self.assertEqual(headers["x-goog-api-key"], "AIza-KEY")
        self.assertNotIn("Authorization", headers)


class TestTranslateRequest(unittest.TestCase):
    def base(self, **kw):
        body = {"model": "gemini-2.5-flash", "input": []}
        body.update(kw)
        return G.translate_request(body)

    def test_system_and_maxtokens(self):
        r = self.base(model="gemini-2.5-pro", instructions="be terse", input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        self.assertEqual(r["systemInstruction"]["parts"][0]["text"], "be terse")
        self.assertEqual(r["generationConfig"]["maxOutputTokens"], G.MAX_OUTPUT_TOKENS["gemini-2.5-pro"])

    def test_empty_instructions_no_system(self):
        r = self.base(instructions="  ", input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        self.assertNotIn("systemInstruction", r)

    def test_roles_user_and_model(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]}])
        self.assertEqual([c["role"] for c in r["contents"]], ["user", "model"])
        self.assertEqual(r["contents"][1]["parts"][0], {"text": "a"})

    def test_function_call_carries_name_args_id(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "shell", "arguments": '{"command":["ls"]}', "call_id": "c1"}])
        fc = r["contents"][1]["parts"][0]["functionCall"]
        self.assertEqual(fc, {"name": "shell", "args": {"command": ["ls"]}, "id": "c1"})

    def test_function_response_recovers_name(self):
        # The KEY Gemini quirk: functionResponse REQUIRES the name, recovered via call_id map.
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": "done"}])
        fr = r["contents"][2]["parts"][0]["functionResponse"]
        self.assertEqual(fr, {"name": "shell", "response": {"output": "done"}, "id": "c1"})

    def test_function_response_object_wrap_array(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "s", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_text", "text": "a"}, {"type": "input_text", "text": "b"}]}])
        self.assertEqual(r["contents"][2]["parts"][0]["functionResponse"]["response"], {"output": "ab"})

    def test_parallel_calls_and_outputs_merge(self):
        r = self.base(input=[
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "name": "a", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call", "name": "b", "arguments": "{}", "call_id": "c2"},
            {"type": "function_call_output", "call_id": "c1", "output": "r1"},
            {"type": "function_call_output", "call_id": "c2", "output": "r2"}])
        model_turn = r["contents"][1]
        self.assertEqual(model_turn["role"], "model")
        self.assertEqual([p["functionCall"]["name"] for p in model_turn["parts"]], ["a", "b"])
        user_turn = r["contents"][2]
        self.assertEqual([p["functionResponse"]["name"] for p in user_turn["parts"]], ["a", "b"])

    def test_image_data_url_inline(self):
        r = self.base(input=[{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,QUJD"}]}])
        self.assertEqual(r["contents"][0]["parts"][0],
                         {"inlineData": {"mimeType": "image/png", "data": "QUJD"}})

    def test_image_http_url_filedata(self):
        r = self.base(input=[{"type": "message", "role": "user", "content": [
            {"type": "input_image", "image_url": "https://x/y.png"}]}])
        self.assertEqual(r["contents"][0]["parts"][0]["fileData"]["fileUri"], "https://x/y.png")

    def test_tools_function_declarations(self):
        r = self.base(
            input=[{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]}],
            tools=[{"type": "function", "name": "shell", "description": "run",
                    "parameters": {"type": "object", "properties": {"c": {"type": "string"}}}}])
        decl = r["tools"][0]["functionDeclarations"][0]
        self.assertEqual(decl["name"], "shell")
        self.assertEqual(decl["parameters"], {"type": "object", "properties": {"c": {"type": "string"}}})
        self.assertEqual(r["toolConfig"]["functionCallingConfig"]["mode"], "AUTO")

    def test_tool_schema_strips_gemini_unsupported_keywords(self):
        # Gemini's OpenAPI-subset schema hard-rejects (400) JSON-Schema-only keywords
        # that Codex/OpenAI send (esp. additionalProperties, set to false for strict
        # mode). They must be stripped recursively at every nesting level.
        r = self.base(
            input=[{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]}],
            tools=[{"type": "function", "name": "apply_patch", "description": "p",
                    "parameters": {"type": "object", "additionalProperties": False,
                                   "$schema": "http://json-schema.org/draft-07/schema#",
                                   "properties": {
                                       "input": {"type": "string"},
                                       "files": {"type": "array", "items": {
                                           "type": "object", "additionalProperties": False,
                                           "properties": {"path": {"type": "string"}}}}},
                                   "required": ["input"]}}])
        params = r["tools"][0]["functionDeclarations"][0]["parameters"]
        blob = json.dumps(params)
        self.assertNotIn("additionalProperties", blob)   # stripped at top + nested
        self.assertNotIn("$schema", blob)
        # real schema content survives
        self.assertEqual(params["properties"]["input"], {"type": "string"})
        self.assertEqual(params["required"], ["input"])
        self.assertEqual(
            params["properties"]["files"]["items"]["properties"]["path"], {"type": "string"})

    def test_reasoning_dropped_compaction_kept(self):
        r = self.base(input=[
            {"type": "reasoning", "summary": [], "encrypted_content": "blob"},
            {"type": "compaction", "encrypted_content": "blob"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "now"}]}])
        blob = json.dumps(r["contents"])
        self.assertNotIn("blob", blob)               # reasoning + encrypted dropped
        self.assertIn("compacted", blob)             # compaction preserved as text

    def test_unknown_item_hard_fails(self):
        with self.assertRaises(G.UnsupportedItemError):
            self.base(input=[
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x"}]},
                {"type": "some_future_item"}])

    def test_first_content_forced_user(self):
        r = self.base(input=[
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]}])
        self.assertEqual(r["contents"][0]["role"], "user")


# ============================ SSE transduction ============================= #
class TestTransducer(unittest.TestCase):
    def _usage(self, prompt=10, cached=0, cand=1, thoughts=0):
        u = {"promptTokenCount": prompt, "candidatesTokenCount": cand}
        if cached:
            u["cachedContentTokenCount"] = cached
        if thoughts:
            u["thoughtsTokenCount"] = thoughts
        return u

    def test_text_turn_added_before_delta(self):
        _t, evs = run([
            {"candidates": [{"content": {"role": "model", "parts": [{"text": "Hi"}]}}],
             "usageMetadata": self._usage(prompt=10, cand=1)},
            {"candidates": [{"content": {"role": "model", "parts": [{"text": " there"}]},
                             "finishReason": "STOP"}], "usageMetadata": self._usage(prompt=10, cand=5)}])
        types = [e["type"] for e in evs]
        self.assertEqual(types[0], "response.created")
        self.assertLess(types.index("response.output_item.added"), types.index("response.output_text.delta"))
        self.assertEqual(types[-1], "response.completed")
        done = [e["item"] for e in evs if e["type"] == "response.output_item.done"][0]
        self.assertEqual(done["content"][0]["text"], "Hi there")

    def test_function_call_buffered_with_id(self):
        _t, evs = run([
            {"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "shell", "args": {"command": ["ls"]}, "id": "fc9"}}]},
                "finishReason": "STOP"}], "usageMetadata": self._usage(prompt=10, cand=20)}])
        fc = [e["item"] for e in evs if e["type"] == "response.output_item.done"][0]
        # Even when Gemini sends its own id ("fc9") we synthesize a fresh uuid call_id --
        # fc["id"] is not unique across responses, which would collide in the persistent
        # signature store and echo the wrong thoughtSignature.
        self.assertEqual(fc["type"], "function_call")
        self.assertEqual(fc["name"], "shell")
        self.assertEqual(fc["arguments"], '{"command": ["ls"]}')
        self.assertTrue(fc["call_id"].startswith("shell_") and len(fc["call_id"]) > len("shell_"))
        self.assertFalse(evs[-1]["response"]["end_turn"])   # tool_use -> keep going

    def test_function_call_synthesizes_id_when_absent(self):
        _t, evs = run([{"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "shell", "args": {}}}]}, "finishReason": "STOP"}],
            "usageMetadata": self._usage()}])
        fc = [e["item"] for e in evs if e["type"] == "response.output_item.done"][0]
        self.assertTrue(fc["call_id"].startswith("shell_"))   # synthesized, non-empty

    def test_text_then_functioncall_closes_text_first(self):
        _t, evs = run([
            {"candidates": [{"content": {"parts": [{"text": "let me"}]}}], "usageMetadata": self._usage()},
            {"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "a", "args": {}, "id": "c1"}}]}, "finishReason": "STOP"}],
             "usageMetadata": self._usage(cand=9)}])
        done = [e for e in evs if e["type"] == "response.output_item.done"]
        self.assertEqual(done[0]["item"]["type"], "message")        # text closed first
        self.assertEqual(done[1]["item"]["type"], "function_call")

    def test_usage_synth_openai_shaped(self):
        _t, evs = run([
            {"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "STOP"}],
             "usageMetadata": self._usage(prompt=1200, cached=1000, cand=20, thoughts=5)}])
        u = evs[-1]["response"]["usage"]
        self.assertEqual(u["input_tokens"], 1200)                   # promptTokenCount incl cached
        self.assertEqual(u["input_tokens_details"]["cached_tokens"], 1000)
        self.assertEqual(u["output_tokens"], 25)                    # candidates 20 + thoughts 5
        self.assertEqual(u["cache_write_tokens"], 0)                # Gemini implicit cache: no write
        self.assertEqual(u["total_tokens"], 1225)

    def test_usage_cumulative_last_wins(self):
        _t, evs = run([
            {"candidates": [{"content": {"parts": [{"text": "a"}]}}], "usageMetadata": self._usage(prompt=100, cand=1)},
            {"candidates": [{"content": {"parts": [{"text": "b"}]}, "finishReason": "STOP"}],
             "usageMetadata": self._usage(prompt=100, cand=50)}])
        self.assertEqual(evs[-1]["response"]["usage"]["output_tokens"], 50)   # last, not summed

    def test_error_chunk_emits_failed(self):
        _t, evs = run([{"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota"}}])
        self.assertEqual(evs[-1]["type"], "response.failed")
        self.assertEqual(evs[-1]["response"]["error"]["code"], "RESOURCE_EXHAUSTED")

    def test_drop_before_content_failed(self):
        # stream ends with nothing -> terminal failed, never hangs
        _t, evs = run([])
        self.assertEqual(evs[-1]["type"], "response.failed")
        self.assertEqual(evs[-1]["response"]["error"]["code"], "stream_interrupted")

    def test_thought_buffered_not_streamed(self):
        # A Gemini "thought" part is NOT streamed to the engine, but IS buffered
        # into output_items as a reasoning item.
        t, evs = run([{"candidates": [{"content": {"parts": [
            {"thought": True, "text": "internal"}, {"text": "answer"}]}, "finishReason": "STOP"}],
            "usageMetadata": self._usage()}])
        done = [e["item"] for e in evs if e["type"] == "response.output_item.done"]
        msg = [d for d in done if d.get("type") == "message"][0]
        self.assertEqual(msg["content"][0]["text"], "answer")       # answer streams
        self.assertNotIn("reasoning", [d.get("type") for d in done])  # thought not streamed
        reasoning = [i for i in t.output_items if i.get("type") == "reasoning"]
        self.assertEqual(len(reasoning), 1)
        self.assertEqual(reasoning[0]["summary"][0]["text"], "internal")  # buffered

    def test_reasoning_capture_flag_adds_thinkingconfig(self):
        import importlib
        os.environ["BYTEASK_REASONING_CAPTURE"] = "1"
        try:
            importlib.reload(G)
            req = G.translate_request({"model": "gemini-2.5-flash", "input": []})
            self.assertTrue(req["generationConfig"]["thinkingConfig"]["includeThoughts"])
        finally:
            os.environ.pop("BYTEASK_REASONING_CAPTURE", None)
            importlib.reload(G)


class TestIterSSE(unittest.TestCase):
    def test_parses_data_frames(self):
        raw = (b'data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}\n\n'
               b'data: {"usageMetadata":{"promptTokenCount":5}}\n\n')
        evs = list(G.iter_sse(iter([raw[i:i + 9] for i in range(0, len(raw), 9)])))
        self.assertEqual(evs[0]["candidates"][0]["content"]["parts"][0]["text"], "hi")
        self.assertEqual(evs[1]["usageMetadata"]["promptTokenCount"], 5)

    def test_parses_crlf_frames(self):
        # Gemini REALLY terminates SSE frames with CRLF (\r\n\r\n), not LF. Before the
        # fix, splitting on \n\n never fired for CRLF -> every frame piled into the final
        # flush, got concatenated into one bad json.loads -> ZERO events -> the transducer
        # reported "gemini stream ended before any content". Tiny 7-byte chunks also make
        # a \r\n straddle a read boundary, which the buffer re-normalization must heal.
        raw = (b'data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}\r\n\r\n'
               b'data: {"candidates":[{"content":{"parts":[{"text":"!"}]},'
               b'"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5}}\r\n\r\n')
        evs = list(G.iter_sse(iter([raw[i:i + 7] for i in range(0, len(raw), 7)])))
        self.assertEqual(len(evs), 2)          # both frames parsed, NOT concatenated
        self.assertEqual(evs[0]["candidates"][0]["content"]["parts"][0]["text"], "Hello")
        self.assertEqual(evs[1]["candidates"][0]["finishReason"], "STOP")
        self.assertEqual(evs[1]["usageMetadata"]["promptTokenCount"], 5)


class TestThoughtSignatureRoundTrip(unittest.TestCase):
    """Gemini thinking models (esp. 3.x) HARD-400 ("Function call is missing a thought
    signature") if a follow-up turn drops the emitted `thoughtSignature`. The Transducer
    stashes it (keyed by the emitted call_id); translate_request re-attaches it when the
    client resends that functionCall."""

    def _emit_fc_with_signature(self, sig="SIG-ABC-123"):
        td = G.Transducer("gemini-3.1-pro-preview")
        frames = td.feed({"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "shell", "args": {"command": ["echo", "9"]}},
             "thoughtSignature": sig}]}, "finishReason": "STOP"}]})
        for f in frames:
            for line in f.split("\n"):
                if line.startswith("data:"):
                    obj = json.loads(line[5:])
                    if (obj.get("type") == "response.output_item.done"
                            and obj["item"]["type"] == "function_call"):
                        return obj["item"]["call_id"]
        self.fail("no function_call emitted")

    def test_signature_captured_then_echoed_on_followup(self):
        call_id = self._emit_fc_with_signature("SIG-ABC-123")
        out = G.translate_request({"model": "gemini-3.1-pro-preview", "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run echo 9"}]},
            {"type": "function_call", "call_id": call_id, "name": "shell",
             "arguments": json.dumps({"command": ["echo", "9"]})},
            {"type": "function_call_output", "call_id": call_id, "output": "9\n"}]})
        fc_part = next(p for c in out["contents"] if c["role"] == "model"
                       for p in c["parts"] if "functionCall" in p)
        self.assertEqual(fc_part.get("thoughtSignature"), "SIG-ABC-123")

    def test_unique_call_ids_prevent_cross_conversation_collision(self):
        a = self._emit_fc_with_signature("SIG-A")
        b = self._emit_fc_with_signature("SIG-B")
        self.assertNotEqual(a, b)                       # globally-unique synthesized ids
        self.assertEqual(G._recall_signature(a), "SIG-A")
        self.assertEqual(G._recall_signature(b), "SIG-B")

    def test_placeholder_signature_attached_when_unknown_call_id(self):
        # An unknown call_id (cross-provider switch / evicted sig) gets the accepted
        # placeholder sentinel instead of nothing, because omitting 400s Gemini 3.x
        # ("Function call is missing a thought_signature") in real conversations.
        out = G.translate_request({"model": "gemini-2.5-flash", "input": [
            {"type": "function_call", "call_id": "never-seen-xyz", "name": "shell", "arguments": "{}"}]})
        fc_part = next(p for c in out["contents"] for p in c["parts"] if "functionCall" in p)
        self.assertEqual(fc_part.get("thoughtSignature"), G._PLACEHOLDER_SIG)


# ============================ --output-schema ============================== #
# The engine serializes `--output-schema` into the Responses `text` param as
#   {"format": {"type":"json_schema","strict":bool,"schema":{...},
#               "name":"codex_output_schema"}}
# (codex-api::create_text_param_for_request). This translator used to ignore it
# entirely, so the flag was accepted and discarded with no error.
def text_param(schema, strict=True):
    return {"format": {"type": "json_schema", "strict": strict,
                       "schema": schema, "name": "codex_output_schema"}}


SCHEMA = {"type": "object",
          "properties": {"answer": {"type": "string"}},
          "required": ["answer"]}


class TestOutputSchema(unittest.TestCase):
    def base(self, **kw):
        body = {"model": "gemini-2.5-flash", "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]}]}
        body.update(kw)
        return G.translate_request(body)

    def test_absent_when_not_requested(self):
        """The whole feature must be inert unless --output-schema was passed."""
        r = self.base()
        self.assertNotIn("responseSchema", r["generationConfig"])
        self.assertNotIn("responseMimeType", r["generationConfig"])

    def test_schema_reaches_generation_config(self):
        r = self.base(text=text_param(SCHEMA))
        self.assertEqual(r["generationConfig"]["responseSchema"], SCHEMA)
        self.assertEqual(r["generationConfig"]["responseMimeType"], "application/json")

    def test_does_not_clobber_maxoutputtokens_or_thinking(self):
        """generationConfig is shared -- adding the schema must merge, not replace."""
        r = self.base(text=text_param(SCHEMA), reasoning={"effort": "low"})
        gc = r["generationConfig"]
        self.assertIn("maxOutputTokens", gc)
        self.assertIn("responseSchema", gc)
        self.assertEqual(gc["thinkingConfig"]["thinkingLevel"], "low")

    def test_schema_is_sanitized_for_gemini(self):
        """Gemini's dialect is an OpenAPI 3.0 subset: `additionalProperties` is a
        hard 400. Verified live 2026-07-27."""
        r = self.base(text=text_param(dict(SCHEMA, additionalProperties=False)))
        self.assertNotIn("additionalProperties", r["generationConfig"]["responseSchema"])
        self.assertEqual(r["generationConfig"]["responseSchema"]["properties"], SCHEMA["properties"])

    def test_nested_schema_sanitized(self):
        nested = {"type": "object", "additionalProperties": False, "properties": {
            "rows": {"type": "array", "items": {
                "type": "object", "additionalProperties": False,
                "properties": {"f": {"type": "string"}}}}}}
        r = self.base(text=text_param(nested))
        blob = json.dumps(r["generationConfig"]["responseSchema"])
        self.assertNotIn("additionalProperties", blob)
        self.assertIn("rows", blob)

    def test_coexists_with_tools(self):
        """PROBED LIVE 2026-07-27: responseSchema + tools returns 200, and a
        tool-forcing prompt still yields a functionCall -- the agent loop holds."""
        r = self.base(text=text_param(SCHEMA), tools=[
            {"type": "function", "name": "t", "description": "d",
             "parameters": {"type": "object", "properties": {}}}])
        self.assertIn("responseSchema", r["generationConfig"])
        self.assertEqual(r["tools"][0]["functionDeclarations"][0]["name"], "t")

    def test_malformed_format_is_ignored_not_raised(self):
        for bad in ({"format": {"type": "text"}},
                    {"format": {"type": "json_schema", "schema": "not-a-dict"}},
                    {"format": "nope"},
                    "nope"):
            r = self.base(text=bad)
            self.assertNotIn("responseSchema", r["generationConfig"])

    def test_dropped_keys_are_reported(self):
        """A silently-flattened $ref is the class of bug this task exists to kill."""
        found = G._dropped_schema_keys(
            {"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}},
             "additionalProperties": False})
        self.assertEqual(found, {"$ref", "additionalProperties"})
        self.assertEqual(G._dropped_schema_keys(SCHEMA), set())

    def test_output_schema_of_reads_strict(self):
        self.assertEqual(G.output_schema_of({"text": text_param(SCHEMA, strict=False)}),
                         (SCHEMA, False))
        self.assertEqual(G.output_schema_of({"text": text_param(SCHEMA)}), (SCHEMA, True))
        self.assertEqual(G.output_schema_of({}), (None, False))


if __name__ == "__main__":
    unittest.main()
