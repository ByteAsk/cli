#!/usr/bin/env python3
"""A reply the model's output limit cut off continues the agent's turn instead of ending it, on every provider:
Anthropic (stop_reason max_tokens), Gemini (finishReason MAX_TOKENS), Chat Completions (finish_reason length) and
native Responses streams (response.incomplete, max_output_tokens). A tool call cut off mid-arguments is dropped, one
note says the reply was cut off, end_turn is false; the third cut-off in a row ends the turn. Stdlib unittest only."""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import anthropic_translate as A  # noqa: E402
import gemini_translate as G  # noqa: E402
import openai_compat_translate as O  # noqa: E402


def events_of(frames):
    out = []
    for f in frames:
        for part in f.split("\n\n"):
            for line in part.split("\n"):
                if line.startswith("data: "):
                    out.append(json.loads(line[6:]))
    return out


def terminal(evs):
    return next(e for e in reversed(evs) if e["type"].startswith("response.") and e["type"] in (
        "response.completed", "response.failed", "response.incomplete"))


def calls(evs):
    return [e["item"] for e in evs if e["type"] == "response.output_item.done"
            and e["item"].get("type") in ("function_call", "custom_tool_call")]


def notes(evs):
    return [e["item"]["content"][0]["text"] for e in evs if e["type"] == "response.output_item.done"
            and e["item"].get("type") == "message" and e["item"]["content"]
            and e["item"]["content"][0]["text"].startswith(A.CUTOFF_MARK)]


def user(text):
    return {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}


def note_msg(text=A.CUTOFF_MARK + ": my reply was cut off]"):
    return {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}


class TheBlockIsTheSameEverywhere(unittest.TestCase):
    def test_duplicated_blocks_agree(self):
        for mod in (G, O):
            self.assertEqual(mod.CUTOFF_MARK, A.CUTOFF_MARK)
            self.assertEqual(mod.CUTOFF_MAX_IN_A_ROW, A.CUTOFF_MAX_IN_A_ROW)
            for prior in range(4):
                self.assertEqual(mod.cutoff_note(prior), A.cutoff_note(prior))

    def test_prior_cutoffs_counts_notes_since_the_last_user_message(self):
        body = {"input": [user("fix it"), note_msg(), {"type": "function_call_output", "output": "x"}, note_msg(),
                          user("continue"), note_msg()]}
        self.assertEqual(A.prior_cutoffs(body), 1)  # the two before "continue" belong to the earlier request
        self.assertEqual(A.prior_cutoffs({"input": [user("x")]}), 0)
        self.assertEqual(A.prior_cutoffs({"input": "a string"}), 0)
        self.assertEqual(A.prior_cutoffs(None), 0)

    def test_the_third_in_a_row_stops(self):
        self.assertTrue(A.cutoff_note(0)[1] and A.cutoff_note(1)[1])
        text, go_on = A.cutoff_note(2)
        self.assertFalse(go_on)
        self.assertIn("3 times in a row", text)


class Anthropic(unittest.TestCase):
    def run_events(self, blocks, stop="max_tokens", prior=0):
        t = A.Transducer("claude-haiku-5-5", prior_cutoffs=prior)
        frames = t.feed({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10}}})
        for i, (kind, payload) in enumerate(blocks):
            if kind == "text":
                frames += t.feed({"type": "content_block_start", "index": i, "content_block": {"type": "text"}})
                frames += t.feed({"type": "content_block_delta", "index": i,
                                  "delta": {"type": "text_delta", "text": payload}})
            else:
                frames += t.feed({"type": "content_block_start", "index": i,
                                  "content_block": {"type": "tool_use", "id": "toolu_%d" % i, "name": "shell"}})
                frames += t.feed({"type": "content_block_delta", "index": i,
                                  "delta": {"type": "input_json_delta", "partial_json": payload}})
            frames += t.feed({"type": "content_block_stop", "index": i})
        frames += t.feed({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 64000}})
        frames += t.feed({"type": "message_stop"})
        return events_of(frames)

    def test_text_cut_off_continues(self):
        evs = self.run_events([("text", "Step 1 is done. Now step 2: I will")])
        self.assertEqual(len(notes(evs)), 1)
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_a_tool_call_cut_off_mid_arguments_is_dropped(self):
        evs = self.run_events([("tool", '{"command": ["bash", "-lc", "cat > big.py <<EOF\\nprint(')])
        self.assertEqual(calls(evs), [])
        self.assertEqual(len(notes(evs)), 1)
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_whole_calls_before_the_cut_still_run(self):
        evs = self.run_events([("tool", '{"command": ["ls"]}'), ("tool", '{"command": ["ca')])
        self.assertEqual([json.loads(c["arguments"]) for c in calls(evs)], [{"command": ["ls"]}])

    def test_an_ordinary_end_is_unchanged(self):
        evs = self.run_events([("text", "All done.")], stop="end_turn")
        self.assertEqual(notes(evs), [])
        self.assertIs(terminal(evs)["response"]["end_turn"], True)
        evs = self.run_events([("tool", '{"command": ["ls"]}')], stop="tool_use")
        self.assertEqual(notes(evs), [])
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_the_third_cut_off_in_a_row_ends_the_turn(self):
        evs = self.run_events([("text", "and then")], prior=2)
        self.assertIs(terminal(evs)["response"]["end_turn"], True)
        self.assertIn("3 times in a row", notes(evs)[0])

    def test_caps_cover_every_served_claude(self):
        for m in ("claude-haiku-5-5", "claude-fable-5-1", "claude-opus-5-5", "claude-sonnet-5-5"):
            self.assertGreaterEqual(A.MAX_OUTPUT_TOKENS[m], 64000, m)


class Gemini(unittest.TestCase):
    def test_max_tokens_continues(self):
        t = G.Transducer("gemini-3.1-pro-preview", prior_cutoffs=0)
        frames = t.feed({"responseId": "r1", "candidates": [{"content": {"parts": [{"text": "Writing the"}]},
                                                             "finishReason": "MAX_TOKENS"}]})
        frames += t.close()
        evs = events_of(frames)
        self.assertEqual(len(notes(evs)), 1)
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_stop_is_unchanged(self):
        t = G.Transducer("gemini-3.1-pro-preview")
        frames = t.feed({"responseId": "r1", "candidates": [{"content": {"parts": [{"text": "Done."}]},
                                                             "finishReason": "STOP"}]})
        frames += t.close()
        evs = events_of(frames)
        self.assertEqual(notes(evs), [])
        self.assertIs(terminal(evs)["response"]["end_turn"], True)


class ChatCompletions(unittest.TestCase):
    def stream(self, chunks, prior=0):
        t = O.Transducer("kimi-k3", known_tools={"shell"}, prior_cutoffs=prior)
        frames = []
        for c in chunks:
            frames += t.feed(c)
        frames += t.close()
        return events_of(frames)

    def test_length_drops_the_cut_call_and_continues(self):
        evs = self.stream([
            {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c0", "function": {
                "name": "shell", "arguments": '{"command": ["ls"]}'}}]}}]},
            {"choices": [{"delta": {"tool_calls": [{"index": 1, "id": "c1", "function": {
                "name": "shell", "arguments": '{"command": ["cat > a.py <<EOF'}}]}, "finish_reason": "length"}]}])
        self.assertEqual([json.loads(c["arguments"]) for c in calls(evs)], [{"command": ["ls"]}])
        self.assertEqual(len(notes(evs)), 1)
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_text_cut_off_continues(self):
        evs = self.stream([{"choices": [{"delta": {"content": "First I"}, "finish_reason": "length"}]}])
        self.assertEqual(len(notes(evs)), 1)
        self.assertIs(terminal(evs)["response"]["end_turn"], False)

    def test_stop_is_unchanged(self):
        evs = self.stream([{"choices": [{"delta": {"content": "Done."}, "finish_reason": "stop"}]}])
        self.assertEqual(notes(evs), [])
        self.assertIs(terminal(evs)["response"]["end_turn"], True)


class NativeResponses(unittest.TestCase):
    INCOMPLETE = {"type": "response.incomplete", "response": {
        "id": "resp_1", "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
        "usage": {"input_tokens": 5, "output_tokens": 128000},
        "output": [{"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "a"}]},
                   {"type": "function_call", "status": "incomplete", "name": "shell", "arguments": '{"c'}]}}

    def stream_bytes(self, events):
        return "".join("event: %s\ndata: %s\n\n" % (e["type"], json.dumps(e)) for e in events).encode()

    def test_the_incomplete_event_becomes_a_completed_one_that_continues(self):
        raw = self.stream_bytes([{"type": "response.created", "response": {"id": "resp_1"}}, self.INCOMPLETE])
        rw = O.IncompleteRewriter(0)
        out = b""
        for i in range(0, len(raw), 7):  # in small pieces, as a socket delivers them
            out += rw.feed(raw[i:i + 7])
        out += rw.flush()
        evs = events_of([out.decode()])
        term = terminal(evs)
        self.assertEqual(term["type"], "response.completed")
        self.assertIs(term["response"]["end_turn"], False)
        self.assertEqual(term["response"]["usage"]["output_tokens"], 128000)  # billing reads the same usage
        self.assertNotIn("incomplete", [o.get("status") for o in term["response"]["output"]])
        self.assertEqual(len(notes(evs)), 1)

    def test_other_incomplete_reasons_and_events_pass_untouched(self):
        other = json.loads(json.dumps(self.INCOMPLETE))
        other["response"]["incomplete_details"]["reason"] = "content_filter"
        raw = self.stream_bytes([{"type": "response.output_text.delta", "delta": "hi"}, other])
        rw = O.IncompleteRewriter(0)
        self.assertEqual(rw.feed(raw) + rw.flush(), raw)

    def test_the_third_in_a_row_ends_the_turn(self):
        line = b"data: " + json.dumps(self.INCOMPLETE).encode() + b"\n"
        evs = events_of([O.rewrite_incomplete_line(line, 2).decode()])
        self.assertIs(terminal(evs)["response"]["end_turn"], True)


if __name__ == "__main__":
    unittest.main()
