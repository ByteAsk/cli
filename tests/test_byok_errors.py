#!/usr/bin/env python3
"""BYOK sidecar — error classification and rendering.

These are behavioral tests: they assert what the ENGINE would do with what the
sidecar puts on the wire, not just that a function returned a string. Every one
of them was proven to fail against the pre-fix sidecar (which answered 502 for a
TLS fault, 401 for a rejected key and 404 for an unknown alias, all of which the
engine retries five times behind "Reconnecting... N/5").

The wire contract is described in helpers/byteask_errors.py.
"""
import http.client
import json
import os
import shutil
import ssl
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "helpers"))
import byteask_errors as E


def _mock(record, spec):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def _go(self):
            n = int(self.headers.get("Content-Length", "0") or "0")
            body = self.rfile.read(n) if n else b""
            record.append({"path": self.path, "body": body})
            s = spec(self.path)
            self.send_response(s["status"])
            self.send_header("Content-Type", s.get("ctype", "application/json"))
            self.send_header("Content-Length", str(len(s["body"])))
            self.end_headers()
            self.wfile.write(s["body"])

        do_GET = do_POST = _go

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


class SidecarErrorBase(unittest.TestCase):
    managed_spec = staticmethod(lambda p: {"status": 200, "ctype": "text/event-stream",
                                           "body": b"data: {\"type\":\"response.completed\"}\n\n"})
    anthropic_spec = staticmethod(lambda p: {"status": 200, "ctype": "text/event-stream",
                                             "body": b"event: message_stop\ndata: {}\n\n"})

    def setUp(self):
        import byok_sidecar as sc
        self.sc = sc
        self.tmp = tempfile.mkdtemp()
        self.rec_managed, self.rec_anthropic = [], []
        self.managed_srv, mp = _mock(self.rec_managed, self.managed_spec)
        self.anthropic_srv, ap = _mock(self.rec_anthropic, self.anthropic_spec)
        self._saved = (sc.ANTHROPIC_URL, sc._CONFIG_PATH, sc._conn_for)
        sc.ANTHROPIC_URL = f"http://127.0.0.1:{ap}"
        self.cfg_path = os.path.join(self.tmp, "byok-config.json")
        sc._CONFIG_PATH = self.cfg_path
        self.managed_url = f"http://127.0.0.1:{mp}"
        self.srv = sc._Server(("127.0.0.1", 0), sc.Handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.port = self.srv.server_address[1]

    def tearDown(self):
        for s in (self.managed_srv, self.anthropic_srv, self.srv):
            s.shutdown()
            s.server_close()
        (self.sc.ANTHROPIC_URL, self.sc._CONFIG_PATH, self.sc._conn_for) = self._saved
        shutil.rmtree(self.tmp, ignore_errors=True)

    def write_config(self, keys=None, endpoints=None):
        cfg = {"keys": keys or {}, "jwt": "jwt-abc", "local_token": "tok",
               "gateway": self.managed_url}
        if endpoints is not None:
            cfg["endpoints"] = endpoints
        with open(self.cfg_path, "w") as f:
            json.dump(cfg, f)

    def call(self, model="gpt-5.4", token="tok", path="/byteask/v1/responses", method="POST"):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        hdrs = {"Content-Type": "application/json"}
        if token is not None:
            hdrs["X-BYOK-Token"] = token
        body = json.dumps({"model": model, "input": []})
        conn.request(method, path, body=body if method == "POST" else None, headers=hdrs)
        r = conn.getresponse()
        data = r.read()
        conn.close()
        return r.status, data

    # -- assertions ------------------------------------------------------- #
    def assert_terminal(self, status, data):
        """The engine renders this verbatim and does NOT retry it."""
        self.assertEqual(status, 200, "a non-200 is retried 5x by the engine")
        text = data.decode()
        payload = None
        for blk in text.split("\n\n"):
            if "data: " in blk:
                payload = json.loads(blk.split("data: ", 1)[1])
        self.assertIsNotNone(payload, f"no SSE frame in {text!r}")
        self.assertEqual(payload["type"], "response.failed")
        self.assertEqual(payload["response"]["error"]["code"], E.TERMINAL_SSE_CODE)
        return payload["response"]["error"]["message"]

    def force_tls_failure(self):
        """Deterministic CERTIFICATE_VERIFY_FAILED, so the test exercises our
        classification rather than the host's actual trust store.

        The failure is raised from request(), not from _conn_for: HTTPSConnection
        construction does no I/O, so the real handshake -- and the real exception
        -- happens when the request is sent. Raising it earlier would test a code
        path that cannot occur in production."""
        real = self.sc._conn_for
        exc = ssl.SSLCertVerificationError(
            "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: unable "
            "to get local issuer certificate (_ssl.c:992)")

        class _BoomConn:
            def request(self, *a, **k):
                raise exc

            def getresponse(self):
                raise exc

            def close(self):
                pass

        def boom(base_url, ssl_ctx=None):
            if base_url.startswith(self.managed_url):
                import urllib.parse as up
                return _BoomConn(), up.urlparse(base_url)
            return real(base_url, ssl_ctx)
        self.sc._conn_for = boom


class TestTlsIsTerminal(SidecarErrorBase):
    def test_tls_failure_is_terminal_not_a_retryable_502(self):
        # The bug this pins: a 502 here, which the engine retries
        # five times (~6.2s) before printing a raw traceback plus the loopback url.
        self.write_config()
        self.force_tls_failure()
        msg = self.assert_terminal(*self.call())
        # Names the host that actually failed (the gateway), not our own port.
        self.assertIn(self.managed_url.split("//", 1)[1], msg)
        self.assertIn("Fix:", msg)

    def test_message_names_the_failing_host_not_this_sidecar(self):
        # The engine used to append `url:`, which for a sidecar error is the
        # loopback address of THIS process, so users blamed our servers.
        self.write_config()
        self.force_tls_failure()
        msg = self.assert_terminal(*self.call())
        self.assertNotIn(f":{self.port}", msg, "the sidecar's own port leaked into the message")

    def test_message_carries_no_internals(self):
        self.write_config()
        self.force_tls_failure()
        msg = self.assert_terminal(*self.call())
        for leak in ("_ssl.c", "Traceback", "SSLCertVerificationError", "('"):
            self.assertNotIn(leak, msg, f"{leak} leaked into a user-facing message")

    def test_stream_is_terminated_so_the_engine_does_not_hang(self):
        # Without the chunked terminator the engine reports a stream disconnect
        # instead of rendering the message. http.client decodes chunking away, so
        # read the raw socket to see what actually goes on the wire.
        import socket
        self.write_config()
        self.force_tls_failure()
        s = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        body = json.dumps({"model": "gpt-5.4", "input": []}).encode()
        host = f"127.0.0.1:{self.port}".encode()   # the real engine's loopback Host
        s.sendall(b"POST /byteask/v1/responses HTTP/1.1\r\nHost: " + host + b"\r\n"
                  b"X-BYOK-Token: tok\r\nContent-Type: application/json\r\n"
                  b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
        raw = b""
        s.settimeout(5)
        try:
            while not raw.endswith(b"0\r\n\r\n"):
                chunk = s.recv(4096)
                if not chunk:
                    break
                raw += chunk
        except socket.timeout:
            pass
        s.close()
        self.assertTrue(raw.endswith(b"0\r\n\r\n"),
                        f"stream not terminated; engine would report a disconnect: {raw[-80:]!r}")

    def test_exactly_one_failure_frame(self):
        self.write_config()
        self.force_tls_failure()
        _, data = self.call()
        # One frame. NB "response.failed" appears twice per frame by design (the
        # SSE event: line and the JSON type), so count frames, not substrings.
        self.assertEqual(data.decode().count("event: response.failed"), 1)


class TestTransientStaysRetryable(SidecarErrorBase):
    managed_spec = staticmethod(lambda p: {"status": 503, "body": b'{"error":{"message":"busy"}}'})

    def test_upstream_503_still_returns_a_retryable_status(self):
        # REGRESSION GUARD. The goal is correct classification, not fewer
        # retries. If this ever returns 200 we have killed legitimate retries.
        self.write_config()
        status, _ = self.call()
        self.assertNotEqual(status, 200)
        self.assertGreaterEqual(status, 500)


class TestRejectedKeyIsTerminal(SidecarErrorBase):
    anthropic_spec = staticmethod(lambda p: {
        "status": 401, "body": b'{"error":{"message":"invalid x-api-key"}}'})

    def test_bad_user_key_is_terminal_and_names_the_fix(self):
        # Pre-fix this forwarded 401, which the engine retries five times.
        self.write_config(keys={"anthropic": "sk-ant-bad"})
        msg = self.assert_terminal(*self.call(model="claude-opus-4-8"))
        self.assertIn("invalid x-api-key", msg)
        self.assertIn("byteask login", msg)


class TestUnknownSelfModel(SidecarErrorBase):
    def test_terminal_and_never_managed_forwards_a_private_prompt(self):
        # The fail-closed privacy invariant, preserved from the old 404 tests.
        self.write_config(endpoints={})
        msg = self.assert_terminal(*self.call(model="self/ghost"))
        self.assertIn("ghost", msg)
        self.assertIn("byteask models add", msg)
        turns = [r for r in self.rec_managed if "responses" in r["path"]]
        self.assertEqual(turns, [], "a self/* prompt leaked to the managed gateway")


class TestDegradeNotBrick(SidecarErrorBase):
    def test_missing_shared_module_degrades_to_the_old_behavior(self):
        # CRITICAL. byteask_errors.py ships to user machines; install.sh can fail
        # to place it. A module-scope import would stop the sidecar from starting
        # and kill ALL of BYOK, not just error handling.
        self.write_config()
        self.force_tls_failure()
        saved = self.sc._err
        try:
            self.sc._err = None
            status, data = self.call()
            self.assertEqual(status, 502)                    # legacy shape
            self.assertIn(b"error", data)
        finally:
            self.sc._err = saved

    def test_healthz_still_answers_without_the_shared_module(self):
        saved = self.sc._err
        try:
            self.sc._err = None
            status, _ = self.call(path="/healthz", method="GET")
            self.assertEqual(status, 200)
        finally:
            self.sc._err = saved


class TestNonTurnKeepsRestSemantics(SidecarErrorBase):
    def test_bad_token_on_a_non_turn_path_stays_403(self):
        # The terminal channel is gated to POST /byteask/v1/responses. The
        # public surface keeps REST status codes for non-engine callers.
        self.write_config()
        status, _ = self.call(path="/byteask/v1/usage", token="WRONG", method="GET")
        self.assertEqual(status, 403)

    def test_bad_token_on_the_turn_path_is_terminal(self):
        self.write_config()
        msg = self.assert_terminal(*self.call(token="WRONG"))
        self.assertIn("byteask", msg.lower())


class TestObservability(SidecarErrorBase):
    def test_terminal_errors_are_logged_to_stderr(self):
        # A 200 makes byok-sidecar.log indistinguishable from a healthy
        # turn, and that log is the artifact we ask users to send us.
        import io
        self.write_config()
        self.force_tls_failure()
        saved, buf = sys.stderr, io.StringIO()
        try:
            sys.stderr = buf
            self.call()
        finally:
            sys.stderr = saved
        self.assertIn("TERMINAL", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
