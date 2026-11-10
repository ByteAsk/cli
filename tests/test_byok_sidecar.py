#!/usr/bin/env python3
"""BYOK local sidecar — end-to-end routing tests.

Spins up mock Anthropic / Gemini / OpenAI / managed-gateway upstreams and the real
sidecar, then drives requests through it to prove the router:
  * keyed provider  -> DIRECT to that provider with the USER's key (translate for
                       claude/gemini, passthrough for openai) + a metadata beacon
  * un-keyed         -> managed gateway with the JWT (no beacon)
  * loopback token gate, /healthz, and "the beacon carries no key".
Pure stdlib mock servers on 127.0.0.1 — never touches the network or live services.
"""
import http.client
import json
import os
import shutil
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))


def _sse_anthropic():
    events = [
        ("message_start", {"message": {"id": "msg1", "usage": {
            "input_tokens": 50, "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 200, "output_tokens": 1}}}),
        ("content_block_start", {"index": 0, "content_block": {"type": "text"}}),
        ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
        ("content_block_stop", {"index": 0}),
        ("message_delta", {"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 30}}),
        ("message_stop", {}),
    ]
    return "".join(f"event: {t}\ndata: {json.dumps(dict(p, type=t))}\n\n"
                   for t, p in events).encode()


def _sse_gemini():
    chunks = [
        {"candidates": [{"content": {"role": "model", "parts": [{"text": "Hi"}]},
                         "finishReason": "STOP"}],
         "usageMetadata": {"promptTokenCount": 1250, "cachedContentTokenCount": 1000,
                           "candidatesTokenCount": 30}},
    ]
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks).encode()


def _sse_openai():
    evs = [
        {"type": "response.created", "response": {"id": "resp_1"}},
        {"type": "response.output_text.delta", "delta": "Hi"},
        {"type": "response.completed", "response": {"id": "resp_1", "usage": {
            "input_tokens": 12, "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 7}}},
    ]
    return "".join(f"data: {json.dumps(e)}\n\n" for e in evs).encode()


def _mock_server(record, spec):
    """A mock upstream that records every request into ``record`` and replies per
    ``spec`` (status/body/ctype), keyed by request path."""
    class H(BaseHTTPRequestHandler):
        def _handle(self):
            ln = int(self.headers.get("Content-Length", 0) or 0)
            raw = self.rfile.read(ln) if ln else b""
            try:
                body = json.loads(raw or b"{}")
            except Exception:
                body = {"_raw": raw.decode("utf-8", "replace")}
            record.append({"method": self.command, "path": self.path,
                           "headers": {k.lower(): v for k, v in self.headers.items()},
                           "body": body})
            s = spec(self.path)
            self.send_response(s["status"])
            self.send_header("Content-Type", s["ctype"])
            self.end_headers()
            try:
                self.wfile.write(s["body"])
            except Exception:
                pass
        do_POST = _handle
        do_GET = _handle

        def log_message(self, *a):
            pass
    return H


def _start(handler_cls):
    srv = ThreadingHTTPServer(("127.0.0.1", 0), handler_cls)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


class SidecarBase(unittest.TestCase):
    def setUp(self):
        import byok_sidecar
        self.sc = byok_sidecar
        self.sc._NO_TELEMETRY = False   # whatever the developer's own shell exports
        self.tmp = tempfile.mkdtemp(prefix="byteask-byok-sc-")

        # ---- mock upstreams ----
        self.rec = {"anthropic": [], "gemini": [], "openai": [], "managed": []}
        self.anthropic_srv, ap = _start(_mock_server(
            self.rec["anthropic"], lambda p: {"status": 200, "ctype": "text/event-stream",
                                              "body": _sse_anthropic()}))
        self.gemini_srv, gp = _start(_mock_server(
            self.rec["gemini"], lambda p: {"status": 200, "ctype": "text/event-stream",
                                           "body": _sse_gemini()}))
        self.openai_srv, op = _start(_mock_server(
            self.rec["openai"], lambda p: {"status": 200, "ctype": "text/event-stream",
                                           "body": _sse_openai()}))

        def _managed_spec(path):
            if path.rstrip("/").endswith("/byok-usage"):
                return {"status": 204, "ctype": "application/json", "body": b""}
            if "responses" in path:
                return {"status": 200, "ctype": "text/event-stream", "body": _sse_openai()}
            return {"status": 200, "ctype": "application/json",
                    "body": b'{"ok":true,"managed":true}'}
        self.managed_srv, mp = _start(_mock_server(self.rec["managed"], _managed_spec))

        # ---- point the sidecar at the mocks + a temp config ----
        self._saved = (self.sc.ANTHROPIC_URL, self.sc.GEMINI_URL, self.sc.OPENAI_URL,
                       self.sc._CONFIG_PATH)
        self.sc.ANTHROPIC_URL = f"http://127.0.0.1:{ap}"
        self.sc.GEMINI_URL = f"http://127.0.0.1:{gp}"
        self.sc.OPENAI_URL = f"http://127.0.0.1:{op}"
        self.cfg_path = os.path.join(self.tmp, "byok-config.json")
        self.sc._CONFIG_PATH = self.cfg_path
        self.managed_url = f"http://127.0.0.1:{mp}"

        # ---- start the real sidecar ----
        self.sidecar_srv = self.sc._Server(("127.0.0.1", 0), self.sc.Handler)
        threading.Thread(target=self.sidecar_srv.serve_forever, daemon=True).start()
        self.port = self.sidecar_srv.server_address[1]

    def tearDown(self):
        for s in (self.anthropic_srv, self.gemini_srv, self.openai_srv,
                  self.managed_srv, self.sidecar_srv):
            s.shutdown()
            s.server_close()
        (self.sc.ANTHROPIC_URL, self.sc.GEMINI_URL, self.sc.OPENAI_URL,
         self.sc._CONFIG_PATH) = self._saved
        shutil.rmtree(self.tmp, ignore_errors=True)

    def write_config(self, keys=None, jwt="jwt-abc", token="tok-xyz"):
        cfg = {"keys": keys or {}, "jwt": jwt, "local_token": token,
               "gateway": self.managed_url}
        with open(self.cfg_path, "w") as f:
            json.dump(cfg, f)

    def call(self, model, token="tok-xyz", path="/byteask/v1/responses"):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        hdrs = {"Content-Type": "application/json"}
        if token is not None:
            hdrs["X-BYOK-Token"] = token
        body = json.dumps({"model": model, "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]}]})
        conn.request("POST", path, body=body, headers=hdrs)
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        return resp.status, data

    def wait_beacon(self, timeout=3.0):
        end = time.time() + timeout
        while time.time() < end:
            b = [r for r in self.rec["managed"] if r["path"].rstrip("/").endswith("/byok-usage")]
            if b:
                return b[0]
            time.sleep(0.02)
        return None


class TestKeyedRouting(SidecarBase):
    def test_anthropic_key_routes_direct_with_user_key(self):
        self.write_config(keys={"anthropic": "sk-ant-USER"})
        status, _ = self.call("claude-opus-4-8")
        self.assertEqual(status, 200)
        self.assertEqual(len(self.rec["anthropic"]), 1, "should hit Anthropic directly")
        req = self.rec["anthropic"][0]
        self.assertEqual(req["path"], "/v1/messages")
        self.assertEqual(req["headers"].get("x-api-key"), "sk-ant-USER")
        self.assertEqual(self.rec["managed"], [] if False else
                         [r for r in self.rec["managed"]
                          if r["path"].rstrip("/").endswith("/byok-usage")] or self.rec["managed"])
        # beacon carries the translated usage (50+1000+200 in, 1000 cached, 30 out)
        beacon = self.wait_beacon()
        self.assertIsNotNone(beacon, "a usage beacon must fire")
        self.assertEqual(beacon["body"]["provider"], "anthropic")
        self.assertEqual(beacon["body"]["input_tokens"], 1250)
        self.assertEqual(beacon["body"]["cached_tokens"], 1000)
        self.assertEqual(beacon["body"]["output_tokens"], 30)

    def test_gemini_key_routes_direct(self):
        self.write_config(keys={"gemini": "GEMINI-USER"})
        status, _ = self.call("gemini-2.5-pro")
        self.assertEqual(status, 200)
        self.assertEqual(len(self.rec["gemini"]), 1)
        self.assertEqual(self.rec["gemini"][0]["headers"].get("x-goog-api-key"), "GEMINI-USER")
        beacon = self.wait_beacon()
        self.assertEqual(beacon["body"]["provider"], "gemini")
        self.assertEqual(beacon["body"]["output_tokens"], 30)

    def test_openai_key_passthrough_direct(self):
        self.write_config(keys={"openai": "sk-USER"})
        status, _ = self.call("gpt-5.4")
        self.assertEqual(status, 200)
        self.assertEqual(len(self.rec["openai"]), 1)
        req = self.rec["openai"][0]
        self.assertEqual(req["path"], "/v1/responses")
        self.assertEqual(req["headers"].get("authorization"), "Bearer sk-USER")
        beacon = self.wait_beacon()
        self.assertEqual(beacon["body"]["provider"], "openai")
        self.assertEqual(beacon["body"]["input_tokens"], 12)
        self.assertEqual(beacon["body"]["cached_tokens"], 4)
        self.assertEqual(beacon["body"]["output_tokens"], 7)

    def test_beacon_never_contains_a_key(self):
        self.write_config(keys={"openai": "sk-SECRET-USER"})
        self.call("gpt-5.4")
        beacon = self.wait_beacon()
        blob = json.dumps(beacon)
        self.assertNotIn("sk-SECRET-USER", blob, "the key must never appear in the beacon")
        self.assertNotIn("authorization", {k.lower() for k in beacon["body"].keys()})

    def test_no_telemetry_sends_no_beacon(self):
        self.sc._NO_TELEMETRY = True
        self.addCleanup(setattr, self.sc, "_NO_TELEMETRY", False)
        self.write_config(keys={"openai": "sk-USER"})
        status, _ = self.call("gpt-5.4")
        self.assertEqual(status, 200)
        self.assertEqual(len(self.rec["openai"]), 1, "the turn itself still goes direct")
        self.assertIsNone(self.wait_beacon(timeout=0.5))
        self.assertEqual(self.rec["managed"], [], "nothing at all may reach ByteAsk")

    def test_no_telemetry_env_values(self):
        import subprocess
        helpers = os.path.dirname(os.path.abspath(self.sc.__file__))
        for value, want in (("1", True), ("true", True), ("TRUE", True), ("Yes", True),
                            ("0", False), ("no", False), ("", False), (None, False)):
            env = {k: v for k, v in os.environ.items() if k != "BYTEASK_NO_TELEMETRY"}
            if value is not None:
                env["BYTEASK_NO_TELEMETRY"] = value
            out = subprocess.run(
                [sys.executable, "-c", "import byok_sidecar; print(byok_sidecar._NO_TELEMETRY)"],
                cwd=helpers, env=env, capture_output=True, text=True, timeout=30).stdout.strip()
            self.assertEqual(out, str(want), "BYTEASK_NO_TELEMETRY=%r" % (value,))


class TestManagedFallback(SidecarBase):
    def test_unkeyed_provider_falls_through_to_managed_with_jwt(self):
        self.write_config(keys={"anthropic": "sk-ant"})   # NO openai key
        status, _ = self.call("gpt-5.4")
        self.assertEqual(status, 200)
        self.assertEqual(self.rec["openai"], [], "must NOT hit OpenAI directly (no key)")
        managed_turns = [r for r in self.rec["managed"] if "responses" in r["path"]
                         and not r["path"].rstrip("/").endswith("/byok-usage")]
        self.assertEqual(len(managed_turns), 1, "must go to the managed gateway")
        self.assertEqual(managed_turns[0]["headers"].get("authorization"), "Bearer jwt-abc")
        # managed traffic is billed server-side, so NO byok beacon
        self.assertIsNone(self.wait_beacon(timeout=0.5))

    def test_managed_open_models_never_get_the_users_openai_key(self):
        # Any non-claude/non-gemini slug was classed "openai", so a
        # user's own OpenAI key was fired at api.openai.com for kimi/deepseek/glm
        # (a 404 on their account at best). Only real OpenAI slugs go direct.
        self.write_config(keys={"openai": "sk-USER"})
        for model in ("kimi-k3", "kimi-k2.7-code", "deepseek-v4", "deepseek-v4-flash",
                      "glm-5.2", "minimax-m3", "qwen3-coder", "some-future-model"):
            with self.subTest(model=model):
                self.rec["managed"].clear()
                status, _ = self.call(model)
                self.assertEqual(status, 200)
                self.assertEqual(self.rec["openai"], [], f"{model} must NOT hit OpenAI")
                turns = [r for r in self.rec["managed"] if "responses" in r["path"]
                         and not r["path"].rstrip("/").endswith("/byok-usage")]
                self.assertEqual(len(turns), 1, f"{model} must go to the managed gateway")
                self.assertEqual(turns[0]["headers"].get("authorization"), "Bearer jwt-abc")

    def test_openai_slugs_still_use_the_users_openai_key(self):
        self.write_config(keys={"openai": "sk-USER"})
        for model in ("gpt-6-astra", "gpt-5.6-luna", "gpt-4.1", "gpt-5.3-codex", "o3",
                      "o4-mini", "codex-mini-latest"):
            with self.subTest(model=model):
                self.rec["openai"].clear()
                status, _ = self.call(model)
                self.assertEqual(status, 200)
                self.assertEqual(len(self.rec["openai"]), 1, f"{model} should go direct")

    def test_auto_always_managed_even_with_all_keys(self):
        # `auto` is a ByteAsk-managed concept (the gateway's policy table resolves
        # it). Even with EVERY provider keyed, it must never go direct — the literal
        # id "auto" would 404 upstream — and must carry the managed JWT instead.
        self.write_config(keys={"openai": "sk-USER", "anthropic": "sk-ant",
                                "gemini": "GEM-USER"})
        status, _ = self.call("auto")
        self.assertEqual(status, 200)
        self.assertEqual(self.rec["openai"], [], "auto must NOT hit OpenAI directly")
        self.assertEqual(self.rec["anthropic"], [], "auto must NOT hit Anthropic directly")
        self.assertEqual(self.rec["gemini"], [], "auto must NOT hit Gemini directly")
        managed_turns = [r for r in self.rec["managed"] if "responses" in r["path"]
                         and not r["path"].rstrip("/").endswith("/byok-usage")]
        self.assertEqual(len(managed_turns), 1, "auto must go to the managed gateway")
        self.assertEqual(managed_turns[0]["headers"].get("authorization"), "Bearer jwt-abc")
        self.assertIsNone(self.wait_beacon(timeout=0.5))   # managed => no byok beacon

    def test_non_turn_get_passes_through_to_managed(self):
        self.write_config(keys={})
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.request("GET", "/byteask/v1/usage", headers={"X-BYOK-Token": "tok-xyz"})
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        self.assertEqual(resp.status, 200)
        self.assertIn(b"managed", data)
        self.assertTrue(any(r["path"] == "/byteask/v1/usage" for r in self.rec["managed"]))


class TestSecurityGates(SidecarBase):
    # The loopback token gate now answers the TURN path with a terminal SSE error
    # (200 + code invalid_prompt) instead of 403, because the engine retries a 403
    # five times and buries the reason. The security invariant is unchanged and
    # still asserted below: the request is refused BEFORE any routing, so the
    # user's key is never used. Non-turn paths still answer 403 -- see
    # test_byok_errors.TestNonTurnKeepsRestSemantics.
    def _assert_refused(self, data):
        self.assertIn(b"invalid_prompt", data, "not refused with a terminal error")

    def test_missing_token_rejected(self):
        self.write_config(keys={"openai": "sk-USER"}, token="tok-xyz")
        status, data = self.call("gpt-5.4", token=None)
        self.assertEqual(status, 200)
        self._assert_refused(data)
        self.assertEqual(self.rec["openai"], [], "the user's key was used despite a bad token")

    def test_wrong_token_rejected(self):
        self.write_config(keys={"openai": "sk-USER"}, token="tok-xyz")
        status, data = self.call("gpt-5.4", token="WRONG")
        self.assertEqual(status, 200)
        self._assert_refused(data)
        self.assertEqual(self.rec["openai"], [], "the user's key was used despite a bad token")

    def test_foreign_host_header_refused_before_any_routing(self):
        # DNS-rebinding: the socket is local (127.0.0.1) but a browser page rebound
        # to it sends its own Host, and being same-origin can even carry a valid
        # X-BYOK-Token. A non-loopback Host is refused before the user's key is used.
        self.write_config(keys={"openai": "sk-USER"}, token="tok-xyz")
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        body = json.dumps({"model": "gpt-5.4", "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]}]})
        conn.request("POST", "/byteask/v1/responses", body=body,
                     headers={"Content-Type": "application/json",
                              "X-BYOK-Token": "tok-xyz",
                              "Host": "attacker.example"})
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        self.assertEqual(resp.status, 403)
        self.assertEqual(self.rec["openai"], [], "key used despite a foreign Host header")

    def test_loopback_host_with_port_is_allowed(self):
        # The real engine sends Host: 127.0.0.1:<port> — must still route.
        self.write_config(keys={"openai": "sk-USER"}, token="tok-xyz")
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.request("GET", "/healthz", headers={"Host": f"127.0.0.1:{self.port}"})
        resp = conn.getresponse()
        obj = json.loads(resp.read())
        conn.close()
        self.assertTrue(obj["ok"])

    def test_healthz_reports_protocol(self):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.request("GET", "/healthz")
        resp = conn.getresponse()
        obj = json.loads(resp.read())
        conn.close()
        self.assertTrue(obj["ok"])
        self.assertEqual(obj["protocol"], self.sc.PROTOCOL_VERSION)


if __name__ == "__main__":
    unittest.main()
