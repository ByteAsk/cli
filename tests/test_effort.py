#!/usr/bin/env python3
"""Reasoning-effort mapping in the provider translators.

These are mostly GUARD tests: each one pins a fact about the engine or a provider
that, if it silently changed, would either brick engine startup, move every
user's default effort, or over-bill.
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "helpers"))

import anthropic_translate as A          # noqa: E402
import effort                            # noqa: E402
import gemini_translate as G             # noqa: E402

TOOL = {"type": "function", "name": "t", "description": "d",
        "parameters": {"type": "object", "properties": {}}}
# Every value the engine's ReasoningEffort enum can put on the wire
# (protocol/src/openai_models.rs), plus an unknown future one.
ALL_EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]


def body(model, eff=None, tools=True):
    b = {"model": model, "instructions": "x",
         "input": [{"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]}]}
    if tools:
        b["tools"] = [TOOL]
    if eff is not None:
        b["reasoning"] = {"effort": eff}
    return b


class TestEffortExtraction(unittest.TestCase):
    def test_absent_or_malformed_yields_none(self):
        for b in ({}, {"reasoning": None}, {"reasoning": "high"}, {"reasoning": {}},
                  {"reasoning": {"effort": None}}, {"reasoning": {"effort": 7}},
                  {"reasoning": {"effort": "  "}}, "not-a-dict", None):
            self.assertIsNone(effort.effort_of(b), repr(b))

    def test_case_and_whitespace_normalized(self):
        self.assertEqual(effort.effort_of({"reasoning": {"effort": " HIGH "}}), "high")

    def test_unknown_effort_clamps_to_none_not_a_guess(self):
        # Guessing a rung for a future level would be a silent cost change.
        self.assertIsNone(effort.clamp_effort({"reasoning": {"effort": "turbo"}},
                                              A.EFFORT_MAP))


class TestAnthropicEffort(unittest.TestCase):
    def test_omitted_effort_sends_no_effort_and_no_thinking(self):
        out = A.translate_request(body("claude-opus-5"))
        self.assertNotIn("output_config", out)
        self.assertNotIn("thinking", out)

    def test_every_engine_level_maps_into_anthropics_vocabulary(self):
        # Anthropic accepts ONLY low/medium/high/xhigh/max. Anything we emit
        # outside that set is a 400 on a real request.
        allowed = {"low", "medium", "high", "xhigh", "max"}
        for eff in ALL_EFFORTS:
            out = A.translate_request(body("claude-opus-5", eff))
            self.assertIn(out["output_config"]["effort"], allowed, eff)

    def test_thinking_adaptive_accompanies_effort(self):
        out = A.translate_request(body("claude-opus-4-8", "high"))
        self.assertEqual(out["thinking"], {"type": "adaptive"})

    def test_never_emits_disabled_thinking(self):
        # fable-5 400s on disabled outright; opus-5 400s on it above effort high.
        for eff in ALL_EFFORTS:
            for m in ("claude-fable-5", "claude-opus-5"):
                out = A.translate_request(body(m, eff))
                self.assertNotEqual(out.get("thinking", {}).get("type"), "disabled")

    def test_xhigh_clamps_down_not_up(self):
        # sonnet-4-6 rejects xhigh (verified live). Clamping UP to `max` would be
        # a silent upgrade to the most expensive rung.
        self.assertEqual(A.EFFORT_MAP["xhigh"], "high")


class TestGeminiEffort(unittest.TestCase):
    def test_omitted_effort_sends_no_thinking_config(self):
        out = G.translate_request(body("gemini-3.6-flash"))
        self.assertNotIn("thinkingConfig", out["generationConfig"])

    def test_every_engine_level_maps_into_geminis_vocabulary(self):
        # Probed live: only minimal/low/medium/high are accepted; the rest 400.
        allowed = {"minimal", "low", "medium", "high"}
        for eff in ALL_EFFORTS:
            out = G.translate_request(body("gemini-3.6-flash", eff))
            self.assertIn(out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                          allowed, eff)

    def test_no_xhigh_or_max_rung_is_ever_emitted(self):
        for eff in ("xhigh", "max", "ultra"):
            self.assertEqual(G.EFFORT_MAP[eff], "high")

    def test_include_thoughts_survives_alongside_thinking_level(self):
        # REGRESSION GUARD: assigning a fresh thinkingConfig dict for the level
        # would silently delete includeThoughts, so the thought summary would
        # stop being returned, with no error and no other failing test.
        saved = G._REASONING_CAPTURE
        try:
            G._REASONING_CAPTURE = True
            out = G.translate_request(body("gemini-3.6-flash", "high"))
            tc = out["generationConfig"]["thinkingConfig"]
            self.assertIs(tc.get("includeThoughts"), True)
            self.assertEqual(tc.get("thinkingLevel"), "high")
        finally:
            G._REASONING_CAPTURE = saved

    def test_new_flash_models_have_output_headroom(self):
        # Gemini thinking tokens come out of the output budget; the 16K default
        # can return MAX_TOKENS with empty text once thinkingLevel is high.
        for slug in ("gemini-3.6-flash", "gemini-3.5-flash-lite"):
            self.assertGreaterEqual(G.MAX_OUTPUT_TOKENS.get(slug, 0), 32768, slug)


class TestHaikuGetsNoEffort(unittest.TestCase):
    """claude-haiku-4-5 400s on output_config.effort AND thinking:adaptive."""
    def test_haiku_request_carries_neither(self):
        out = A.translate_request(body("claude-haiku-4-5", "high"))
        self.assertNotIn("thinking", out)
        self.assertNotIn("effort", out.get("output_config") or {})

    def test_haiku_5_5_carries_both(self):
        # Unlike 4.5: probed live 2026-10-09, 5.5 accepts effort AND adaptive thinking.
        out = A.translate_request(body("claude-haiku-5-5", "high"))
        self.assertEqual(out["thinking"], {"type": "adaptive"})
        self.assertIn("effort", out.get("output_config") or {})

    def test_sonnet_still_carries_both(self):
        out = A.translate_request(body("claude-sonnet-5", "high"))
        self.assertEqual(out["thinking"], {"type": "adaptive"})
        self.assertEqual(out["output_config"]["effort"], "high")


if __name__ == "__main__":
    unittest.main()
