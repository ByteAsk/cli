#!/usr/bin/env python3
"""Unit tests for the shared error taxonomy.

The behavioral half (does the engine actually stop retrying?) lives in
test_byok_errors.py. This file covers the pure functions."""
import json
import os
import ssl
import sys
import unittest
import urllib.error

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import byteask_errors as E


class TestTerminalFrame(unittest.TestCase):
    def _payload(self, frame):
        self.assertTrue(frame.startswith("event: response.failed\ndata: "))
        self.assertTrue(frame.endswith("\n\n"))
        return json.loads(frame.split("data: ", 1)[1])

    def test_shape_matches_what_the_engine_dispatches_on(self):
        p = self._payload(E.terminal_frame("boom"))
        # The engine matches on the JSON `type`, not the SSE `event:` line.
        self.assertEqual(p["type"], "response.failed")
        self.assertEqual(p["response"]["error"]["code"], "invalid_prompt")
        self.assertEqual(p["response"]["error"]["message"], "boom")
        self.assertEqual(p["response"]["status"], "failed")

    def test_message_survives_newlines_and_quotes(self):
        msg = 'line one\nFix: set SSL_CERT_FILE="$(python3 -m certifi)"'
        p = self._payload(E.terminal_frame(msg))
        self.assertEqual(p["response"]["error"]["message"], msg)

    def test_terminal_code_is_not_the_backoff_code(self):
        # Swapping these silently converts terminal errors back into retries.
        self.assertNotEqual(E.TERMINAL_SSE_CODE, E.BACKOFF_SSE_CODE)


class TestBackoffFrame(unittest.TestCase):
    def _msg(self, frame):
        return json.loads(frame.split("data: ", 1)[1])["response"]["error"]["message"]

    def test_message_matches_the_engine_retry_after_regex(self):
        # CRITICAL. codex-api/src/sse/responses.rs:659
        #   (?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)
        # Reword this and the delay is silently dropped.
        import re
        rx = re.compile(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)")
        m = rx.search(self._msg(E.backoff_frame("ByteAsk is busy", 12)))
        self.assertIsNotNone(m, "engine cannot parse a delay out of this message")
        self.assertEqual(m.group(1), "12")

    def test_code_is_retryable_not_terminal(self):
        p = json.loads(E.backoff_frame("busy", 5).split("data: ", 1)[1])
        self.assertEqual(p["response"]["error"]["code"], "rate_limit_exceeded")

    def test_zero_and_none_clamp_to_one_second(self):
        for bad in (0, None, -3):
            self.assertIn("try again in 1 seconds", self._msg(E.backoff_frame("busy", bad)))

    def test_no_double_punctuation(self):
        self.assertNotIn("., try again", self._msg(E.backoff_frame("ByteAsk is busy.", 4)))


class TestPlanLimitBody(unittest.TestCase):
    def test_type_is_exactly_what_the_engine_matches(self):
        # Anything other than this string renders "exceeded retry limit,
        # last status: 429" and discards our copy entirely.
        self.assertEqual(E.plan_limit_body(1000)["error"]["type"], "usage_limit_reached")

    def test_resets_at_is_an_epoch_int_not_a_duration(self):
        body = E.plan_limit_body(1784000000.9)
        self.assertEqual(body["error"]["resets_at"], 1784000000)
        self.assertIsInstance(body["error"]["resets_at"], int)

    def test_resets_at_omitted_when_unknown(self):
        self.assertNotIn("resets_at", E.plan_limit_body(None)["error"])

    def test_body_carries_no_upgrade_copy_of_its_own(self):
        # The visible copy comes from the promo header; a message here is dead
        # weight that could mislead the next reader into thinking it renders.
        self.assertNotIn("message", E.plan_limit_body(1, plan="free")["error"])


class TestPromoHeader(unittest.TestCase):
    def test_name_is_the_header_the_engine_reads(self):
        self.assertEqual(E.promo_header("x")[0], "x-codex-promo-message")

    def test_newlines_collapsed_since_headers_are_single_line(self):
        name, value = E.promo_header("one\ntwo   three\r\nfour")
        self.assertEqual(value, "one two three four")
        self.assertNotIn("\n", value)

    def test_our_copy_contains_no_competitor_url_or_brand(self):
        # CRITICAL, and the whole reason the promo header exists: with plan_type
        # alone the engine emits chatgpt.com links and the word "Codex".
        _, value = E.promo_header(
            "You've reached your 5-hour ByteAsk limit; email founders@byteask.ai to upgrade")
        self.assertNotIn("chatgpt.com", value)
        self.assertNotIn("Codex", value)


class TestIsTerminalStatus(unittest.TestCase):
    def test_client_errors_are_terminal(self):
        for s in (400, 401, 402, 403, 404, 413, 422):
            self.assertTrue(E.is_terminal_status(s), s)

    def test_429_is_not_terminal_engine_owns_rate_limits(self):
        self.assertFalse(E.is_terminal_status(429))

    def test_server_errors_stay_transient(self):
        # REGRESSION GUARD: the point of this change is correct classification,
        # not fewer retries. A 5xx must keep retrying.
        for s in (500, 502, 503, 504, 529):
            self.assertFalse(E.is_terminal_status(s), s)

    def test_success_and_garbage_are_not_terminal(self):
        for s in (200, 204, 302, None, "", "abc"):
            self.assertFalse(E.is_terminal_status(s), s)


class TestTlsAdvice(unittest.TestCase):
    def test_direct_verify_failure_names_host_cause_and_fix(self):
        exc = ssl.SSLCertVerificationError(
            "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: unable to get "
            "local issuer certificate (_ssl.c:992)")
        msg = E.tls_advice(exc, "code.byteask.ai")
        self.assertIn("code.byteask.ai", msg)
        self.assertIn("Fix:", msg)
        self.assertIn("certifi", msg)

    def test_advice_leaks_no_internals_to_the_user(self):
        exc = ssl.SSLCertVerificationError(
            "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed (_ssl.c:992)")
        msg = E.tls_advice(exc, "code.byteask.ai")
        self.assertNotIn("_ssl.c", msg)
        self.assertNotIn("127.0.0.1", msg)
        self.assertNotIn("Traceback", msg)

    def test_uses_python3_m_pip_not_pip3(self):
        # pip3 may target a different interpreter than the one that needs certs.
        exc = ssl.SSLCertVerificationError("[SSL: CERTIFICATE_VERIFY_FAILED] x")
        msg = E.tls_advice(exc, "h")
        self.assertIn("python3 -m pip", msg)
        self.assertNotIn("pip3 install", msg)

    def test_urllib_wrapped_failure_is_still_diagnosed(self):
        # byok_sidecar.py:673 uses urllib, which wraps the SSL error in URLError.
        # URLError is not an ssl.SSLError, so an isinstance-only check misses it.
        inner = ssl.SSLCertVerificationError("[SSL: CERTIFICATE_VERIFY_FAILED] nope")
        msg = E.tls_advice(urllib.error.URLError(inner), "code.byteask.ai")
        self.assertIsNotNone(msg)
        self.assertIn("certifi", msg)

    def test_tls_error_that_is_not_verify_failure_gets_different_advice(self):
        msg = E.tls_advice(ssl.SSLError("[SSL] internal error"), "h")
        self.assertIsNotNone(msg)
        # Not the CA-bundle remedy: this is not a trust-store problem.
        # NB "certifi" is a substring of "certificate", so assert on the command.
        self.assertNotIn("python3 -m pip", msg)
        self.assertIn("Fix:", msg)

    def test_no_raw_python_exception_repr_leaks_into_the_message(self):
        # str(ssl.SSLError("x")) renders as the tuple repr "('x',)". Interpolating
        # an exception into user-facing text is the defect this module removes.
        msg = E.tls_advice(ssl.SSLError("[SSL] internal error"), "code.byteask.ai")
        self.assertNotIn("('", msg)
        self.assertNotIn("[SSL]", msg)

    def test_non_tls_exception_returns_none(self):
        for exc in (ConnectionRefusedError("refused"), TimeoutError("slow"),
                    ValueError("nope")):
            self.assertIsNone(E.tls_advice(exc, "h"), exc)


class TestUpstreamDetail(unittest.TestCase):
    def test_json_envelope_yields_the_provider_sentence(self):
        raw = b'{"error": {"message": "invalid x-api-key", "type": "authentication_error"}}'
        self.assertEqual(E.upstream_detail(raw), "invalid x-api-key")

    def test_string_error_field(self):
        self.assertEqual(E.upstream_detail(b'{"error": "not found"}'), "not found")

    def test_non_json_body_passes_through_trimmed(self):
        self.assertEqual(E.upstream_detail(b"  502 Bad Gateway  "), "502 Bad Gateway")

    def test_empty_body_is_labelled_not_blank(self):
        for raw in (b"", b"   ", None):
            self.assertEqual(E.upstream_detail(raw), "(empty response body)")

    def test_long_body_is_truncated(self):
        self.assertLessEqual(len(E.upstream_detail(b"x" * 5000)), 800)

    def test_undecodable_bytes_do_not_raise(self):
        self.assertIsInstance(E.upstream_detail(b"\xff\xfe bad"), str)


class TestMessageTemplates(unittest.TestCase):
    def test_user_key_fault_tells_them_how_to_re_enter_it(self):
        msg = E.key_rejected("Anthropic", 401, "invalid x-api-key", own_key=True)
        self.assertIn("byteask login", msg)
        self.assertIn("invalid x-api-key", msg)

    def test_self_hosted_key_fault_points_at_models_add(self):
        msg = E.key_rejected("your endpoint", 401, "no", own_key=False)
        self.assertIn("byteask models add", msg)

    def test_our_fault_never_hands_the_user_a_fix_they_cannot_perform(self):
        msg = E.ours_broken("Claude", "a GPT or Gemini model")
        self.assertIn("on our side", msg)
        self.assertIn("/model", msg)
        # Must not blame the user's credentials for our missing server-side key.
        self.assertNotIn("your key", msg.lower())
        self.assertNotIn("byteask login", msg)


if __name__ == "__main__":
    unittest.main()
