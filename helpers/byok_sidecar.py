#!/usr/bin/env python3
"""ByteAsk BYOK local sidecar — a loopback "local gateway" that runs on the USER's
machine so their own-key traffic never routes through ByteAsk's servers.

The engine speaks ONLY the OpenAI Responses API and resolves ONE provider (one
base_url) per session, so it cannot itself reach Anthropic/Gemini (no Responses
endpoint) nor mix providers. This sidecar is that missing router: the engine points
at http://127.0.0.1:<port>, and per request we route by the model's provider and by
whether the user has set a key for it:

    ┌─ user HAS a key for this provider ─► call the provider DIRECT with the user's
    │                                       key (translate for claude/gemini, pass
    │                                       through for openai). The key and the
    │                                       prompt never reach ByteAsk. After the
    │                                       turn, if the user is signed in, we send
    │                                       a usage BEACON: provider, model and
    │                                       token counts, with the user's ByteAsk
    │                                       token.
    └─ NO user key ─────────────────────► transparently forward to the managed
                                            ByteAsk gateway with the user's JWT
                                            (billed + /usage exactly as today).

Design invariants:
  * The engine holds NO secret. Keys AND the managed JWT live here, in a 0600 file.
    A repo re-pointing base_url can only break the connection, never exfiltrate.
  * Provider hosts are PINNED (hardcoded). base_url overrides exist only for tests.
  * Loopback bind only; a per-user token gates access (a same-box hardening).
  * Reuses anthropic_translate / gemini_translate verbatim — one translator codebase.
  * Best-effort everywhere: a beacon/log fault never breaks the user's request.

Pure stdlib. Materialized to ~/.byteask alongside the translators and run as
`python3 byok_sidecar.py` by the wrapper.
"""
import hmac
import http.client
import http.server
import json
import os
import socketserver
import ssl
import sys
import threading
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import anthropic_translate
import gemini_translate
import openai_compat_translate

# Bumped whenever the sidecar's wire/behavior changes so the wrapper's health probe
# can detect a stale daemon after `byteask --update` and restart it.
# v2 (2026-07-16): self/* custom-endpoint routing + /healthz identity.
# v3 (2026-07-18): any-model hardening — text-leaked tool-call recovery, think-block
#                  strip, custom/freeform (apply_patch) tool support.
# v4 (2026-07-25): terminal-vs-transient error classification. NB the wrapper
#                  detects a stale sidecar by /healthz + file mtime, NOT by this
#                  number (ensure_sidecar in the wrapper), so bumping it documents
#                  the wire change but does not by itself detect helper skew.
PROTOCOL_VERSION = 4

# The one route where the engine is the known consumer, and therefore the only
# route allowed to answer a failure with 200 + an in-band SSE error frame.
TURN_PATH = "/byteask/v1/responses"
# Hostnames a genuine client (the engine/wrapper, always http://127.0.0.1:<port>)
# can present in the Host header. Anything else is a browser DNS-rebinding attempt.
_LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1", "[::1]"})

# PINNED provider hosts. The env overrides exist ONLY so tests can point at a mock
# upstream; in production these are the hardcoded, trusted destinations.
ANTHROPIC_URL = os.environ.get("BYOK_ANTHROPIC_URL", "https://api.anthropic.com").rstrip("/")
GEMINI_URL = os.environ.get("BYOK_GEMINI_URL", "https://generativelanguage.googleapis.com").rstrip("/")
OPENAI_URL = os.environ.get("BYOK_OPENAI_URL", "https://api.openai.com").rstrip("/")

_CODEX_HOME = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.byteask")
_CONFIG_PATH = os.environ.get("BYOK_CONFIG_PATH") or os.path.join(_CODEX_HOME, "byok-config.json")

_UPSTREAM_TIMEOUT = 600

# BYTEASK_NO_TELEMETRY=1 (also true/yes, any case): never send the usage beacon.
_NO_TELEMETRY = os.environ.get("BYTEASK_NO_TELEMETRY", "").strip().lower() in ("1", "true", "yes")


def _load_config():
    """Read the 0600 config each request so key/JWT edits by the wrapper take effect
    without a restart. Shape: {keys:{openai,anthropic,gemini}, jwt, local_token,
    gateway}. Fail-safe to empty (the sidecar then rejects on the token check)."""
    try:
        with open(_CONFIG_PATH, "rb") as f:
            cfg = json.loads(f.read() or b"{}")
        return cfg if isinstance(cfg, dict) else {}
    except Exception:
        return {}


# Slug prefixes that really are OpenAI models. Anything else that is not Claude or
# Gemini (kimi, deepseek, glm, qwen, a model added next month) is served by ByteAsk
# on its own provider key, so it must NEVER be sent to api.openai.com with the
# user's OpenAI key. Unknown -> "managed" is the safe direction.
_OPENAI_PREFIXES = ("gpt-", "chatgpt-", "codex-mini")


def _is_openai_model(model):
    m = (model or "").lower()
    if m.startswith(_OPENAI_PREFIXES):
        return True
    # o-series reasoning models: o1, o3, o3-mini, o4-mini, ...
    return len(m) > 1 and m[0] == "o" and m[1].isdigit()


def _provider_of(model):
    if anthropic_translate.is_anthropic_model(model):
        return "anthropic"
    if gemini_translate.is_gemini_model(model):
        return "gemini"
    if _is_openai_model(model):
        return "openai"
    return "managed"


def _body_model(body_bytes):
    try:
        m = json.loads(body_bytes or b"{}").get("model")
        return m if isinstance(m, str) and m else ""
    except Exception:
        return ""


# The shared user-facing error taxonomy. GUARDED import on purpose: this file
# runs on END USER machines and is fetched by install.sh, so byteask_errors.py
# can legitimately be absent (a partial update, an installer whose fetch guard
# rejected it). A module-scope `import` would raise here, /healthz would never
# answer, ensure_sidecar would give up and ALL of BYOK would be dead -- far worse
# than the error-message bug this module fixes. Absent -> `_err` is None and
# _terminal() falls back to the legacy 502 shape. One choke point, not 12.
try:
    import byteask_errors as _err
except Exception:                                   # pragma: no cover - degraded path
    _err = None


def _conn_for(base_url, ssl_ctx=None):
    up = urllib.parse.urlparse(base_url)
    if up.scheme == "https":
        conn = http.client.HTTPSConnection(up.netloc, timeout=_UPSTREAM_TIMEOUT, context=ssl_ctx)
    else:
        conn = http.client.HTTPConnection(up.netloc, timeout=_UPSTREAM_TIMEOUT)
    return conn, up


# --------------------------------------------------------------------------- #
# self/* custom endpoints (user's own OpenAI-compatible model server)         #
# --------------------------------------------------------------------------- #
def _normalize_base(url):
    """A user pastes anything: `http://dgx:8000`, `.../v1`, `.../openai/v1`. Strip a
    trailing slash + a trailing `/v1` (we append the version path ourselves), keeping
    any other path prefix. Returns the root; callers build `<prefix>/v1/...`."""
    u = (url or "").strip().rstrip("/")
    if u.endswith("/v1"):
        u = u[:-3]
    return u


def _self_path(base, suffix):
    """`<any path prefix on base>` + `/v1/<suffix>` (e.g. chat/completions)."""
    prefix = urllib.parse.urlparse(base).path.rstrip("/")
    return f"{prefix}/v1/{suffix}"


def _ssl_ctx_for(base, ep):
    """Honor a per-endpoint private CA / self-signed cert. Only for https."""
    if not base.lower().startswith("https"):
        return None
    if ep.get("insecure"):
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        return ctx
    ca = (ep.get("ca_bundle") or "").strip()
    if ca:
        return ssl.create_default_context(cafile=ca)
    return None


def _endpoint_headers(ep, stream=True):
    """Bearer api_key + arbitrary per-endpoint headers (custom auth schemes)."""
    h = {"Content-Type": "application/json"}
    if stream:
        h["Accept"] = "text/event-stream"
    key = (ep.get("api_key") or "").strip()
    if key:
        h["Authorization"] = "Bearer " + key
    for k, v in (ep.get("headers") or {}).items():
        if isinstance(k, str) and isinstance(v, str) and k.lower() != "content-type":
            h[k] = v
    return h


def _scan_completed_usage(sse_text, acc):
    """Extract usage from a Responses `response.completed` SSE event (for the openai
    passthrough beacon). Accumulates partial JSON across chunks in ``acc`` (a list of
    one str). Returns a usage dict or None. Never raises."""
    acc[0] += sse_text
    if '"response.completed"' not in acc[0] and '"response.incomplete"' not in acc[0]:
        # Bound memory on a long stream: the terminal usage event lives at the END,
        # so keep only the tail until we see it. (64KB dwarfs a usage frame.)
        if len(acc[0]) > 65536:
            acc[0] = acc[0][-65536:]
        return None
    usage = None
    for raw in acc[0].split("\n"):
        raw = raw.strip()
        if not raw.startswith("data:"):
            continue
        payload = raw[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            obj = json.loads(payload)
        except Exception:
            continue
        u = (obj.get("response") or {}).get("usage") if isinstance(obj, dict) else None
        if isinstance(u, dict):
            usage = u
    return usage


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # Quiet by default (no request logging that could echo anything sensitive).
    def log_message(self, *a):
        pass

    # ---- small helpers ----
    def _read_body(self):
        raw = (self.headers.get("Content-Length") or "0").strip()
        if not raw.isdigit():
            # read(-1) reads until EOF and swallows the next request.
            self.close_connection = True
            return b""
        n = int(raw)
        return self.rfile.read(n) if n else b""

    def _json(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _host_is_loopback(self):
        """Defeat browser DNS-rebinding against this loopback service: an attacker
        page at attacker.com rebound to 127.0.0.1 reaches us over a LOCAL socket but
        with `Host: attacker.com` (and, being same-origin to itself, no Origin/CORS
        preflight — so it can even set X-BYOK-Token). The engine and wrapper always
        reach the sidecar at http://127.0.0.1:<port>, so a non-loopback Host is never
        a real client. This holds even in the fresh-install window where _authed
        fails open (no local_token yet), which the token gate alone cannot cover."""
        host = (self.headers.get("Host") or "").strip()
        if not host:
            return True  # a direct local socket (HTTP/1.0 / no Host) is not a browser
        name = host.rsplit(":", 1)[0] if host.rsplit(":", 1)[-1].isdigit() else host
        return name.lower() in _LOOPBACK_HOSTS

    def _authed(self, cfg):
        """Loopback token gate: reject any request lacking the per-user token, so a
        different local user on a shared box can't drive the user's keys. Returns
        True iff allowed (also True if no token is configured yet, to fail-open on a
        fresh install where only the wrapper reaches the sidecar)."""
        want = (cfg.get("local_token") or "").strip()
        if not want:
            # Keys on disk with no local token is the shared-machine hole.
            # A fresh install that has not saved a key yet still has nothing to steal.
            keys = cfg.get("keys") or {}
            if isinstance(keys, dict) and any(str(v or "").strip() for v in keys.values()):
                return False
            return True
        got = (self.headers.get("X-BYOK-Token") or "").strip()
        if len(got) != len(want):
            return False
        return hmac.compare_digest(got, want)

    # Write-state only: "have the SSE headers gone out yet". NOT a classifier --
    # a socket drop or read error mid-stream stays TRANSIENT and must keep being
    # retried; only sites that have already decided a fault is terminal call
    # _terminal().
    _sse_open = False

    def _sse_headers(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self._sse_open = True

    def _is_turn(self):
        """The terminal channel is gated to the one route where the engine is the
        known consumer. Everything else (/usage, /models, ...) keeps REST
        semantics, because the public surface cannot assume who is calling."""
        return (self.command == "POST"
                and urllib.parse.urlparse(self.path).path.rstrip("/") == TURN_PATH)

    def _terminal(self, message):
        """Surface a fault the user must act on.

        200 + one `response.failed` frame with code `invalid_prompt`, which the
        engine maps to CodexErr::InvalidRequest: NOT retried, and its Display is
        the bare message -- no status prefix, no url, no "Reconnecting... N/5".
        Sending a real status here instead would cost the user ~6.2s of backoff
        before they see anything (core/src/util.rs: 200+400+800+1600+3200 ms).

        Safe mid-stream: headers are only sent if they have not gone out already.
        """
        # A 200 makes byok-sidecar.log indistinguishable from a healthy turn, and
        # that log is the artifact we ask users to send us. Always record it.
        sys.stderr.write(f"[byok] TERMINAL {self.path}: "
                         f"{(message or '').splitlines()[0] if message else ''}\n")
        if _err is None or not self._is_turn():
            # Degraded (helper missing) or a non-turn caller: keep the legacy
            # JSON shape rather than inventing an SSE stream nobody asked for.
            code = 502 if _err is None else 400
            return self._json(code, {"error": {
                "message": message, "type": "invalid_request"}})
        try:
            if not self._sse_open:
                self._sse_headers()
            self._emit(_err.terminal_frame(message))
            # Without the chunked terminator the engine sees a truncated stream
            # and reports a disconnect instead of rendering the message.
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _connect_fault(self, exc, host, label):
        """A connection that never completed.

        A TLS trust failure is TERMINAL: the request did not leave the machine
        and will fail identically on the fifth retry, so the user gets the CA
        diagnosis instead of a spinner. Everything else (refused, DNS, reset) is
        TRANSIENT -- the engine's retry is the right response there and this
        change must not take it away."""
        advice = _err.tls_advice(exc, host) if _err else None
        if advice:
            return self._terminal(advice)
        return self._json(502, {"error": {
            "message": f"ByteAsk could not reach {label} ({host}).", "type": "upstream"}})

    def _upstream_status_fault(self, label, status, raw, own_key=True):
        """Classify a non-200 from an upstream.

        4xx other than 429 means the request itself is wrong (bad key, bad model
        id, bad payload) and is terminal. 429 and 5xx keep their real status so
        the engine's rate-limit and retry handling still fire."""
        if _err is None:
            detail = (raw or b"")[:300].decode("utf-8", "replace")
            return self._json(status if status in (429, 500, 502, 503, 529) else 502,
                              {"error": {"message": f"{label} error {status}: {detail}",
                                         "type": "upstream"}})
        detail = _err.upstream_detail(raw)
        if _err.is_terminal_status(status):
            if status in (401, 403):
                return self._terminal(_err.key_rejected(label, status, detail, own_key))
            return self._terminal(
                f"{label} rejected the request (HTTP {status}): {detail}\n"
                f"Fix: check the model id, or pick another model with /model.")
        code = status if status in (429, 500, 502, 503, 529) else 502
        return self._json(code, {"error": {
            "message": f"{label} error {status}: {detail}", "type": "upstream"}})

    def _emit(self, frame):
        b = frame.encode() if isinstance(frame, str) else frame
        self.wfile.write(f"{len(b):X}\r\n".encode() + b + b"\r\n")
        self.wfile.flush()

    # ---- routing ----
    def do_GET(self):
        if not self._host_is_loopback():
            return self._json(403, {"error": {"message": "byok sidecar: refused"}})
        path = urllib.parse.urlparse(self.path).path
        if path == "/healthz":
            # Identity: the wrapper verifies this sidecar serves THIS install's
            # CODEX_HOME/config, so a stale :8799 daemon (old keys/registry) is detected
            # and restarted instead of silently serving the wrong endpoint.
            return self._json(200, {"ok": True, "protocol": PROTOCOL_VERSION,
                                    "codex_home": _CODEX_HOME, "config": _CONFIG_PATH})
        cfg = _load_config()
        if not self._authed(cfg):
            return self._json(403, {"error": {"message": "byok sidecar: bad local token"}})
        # Non-turn GETs (/usage, /models, /referral-code, ...) → managed gateway.
        return self._passthrough_managed(cfg, "GET", self._read_body())

    def do_POST(self):
        if not self._host_is_loopback():
            return self._json(403, {"error": {"message": "byok sidecar: refused"}})
        path = urllib.parse.urlparse(self.path).path
        cfg = _load_config()
        if not self._authed(cfg):
            if self._is_turn():
                return self._terminal(
                    "ByteAsk: this byteask install and its local model sidecar disagree on "
                    "their access token, so the request was refused before it went anywhere.\n"
                    "Fix: quit byteask and start it again, which restarts the sidecar. If it "
                    "keeps happening, run `byteask --update`.")
            return self._json(403, {"error": {"message": "byok sidecar: bad local token"}})
        body = self._read_body()
        if path.rstrip("/") == TURN_PATH:
            return self._route_turn(cfg, body)
        # Non-turn POSTs (/responses/compact, voucher, upgrade, ...) → managed gateway.
        return self._passthrough_managed(cfg, "POST", body)

    # ---- the per-model router ----
    def _route_turn(self, cfg, body):
        model = _body_model(body)
        # self/* = the user's OWN hosted model. Resolve from the registry and route
        # DIRECT to their box. Checked FIRST and FAIL-CLOSED: a self/* prompt is
        # private and must NEVER be managed-forwarded to ByteAsk.
        if model.startswith("self/"):
            return self._route_self(cfg, model, body)
        # `auto` is a ByteAsk-MANAGED concept: the gateway's policy table resolves
        # it to a real cheap model. It must NEVER go direct-to-provider on a user
        # key (the literal id "auto" would 404 upstream), so it always managed-
        # forwards, even with keys set. Managed-billed by design (stated in the
        # model's picker description).
        if model == "auto":
            return self._passthrough_managed(cfg, "POST", body)
        provider = _provider_of(model)
        keys = cfg.get("keys") or {}
        user_key = (keys.get(provider) or "").strip()
        if not user_key:
            # No own-key for this provider → managed pipeline (billed + /usage today).
            return self._passthrough_managed(cfg, "POST", body)
        # Own-key path: nothing touches ByteAsk; beacon usage after the stream.
        if provider == "anthropic":
            return self._stream_translated(cfg, anthropic_translate, model, body,
                                           user_key, "anthropic")
        if provider == "gemini":
            return self._stream_translated(cfg, gemini_translate, model, body,
                                           user_key, "gemini")
        return self._stream_openai(cfg, model, body, user_key)

    # ---- own-key: translated providers (claude / gemini) ----
    def _stream_translated(self, cfg, mod, model, body, user_key, provider):
        try:
            req = json.loads(body or b"{}")
            translated = mod.translate_request(req)
            provider_body = json.dumps(translated).encode()
        except mod.UnsupportedItemError as e:
            return self._terminal(
                f"ByteAsk cannot send this conversation to {provider}: {e}\n"
                f"Fix: switch back to a GPT model with /model, or start a new session.")
        except Exception as e:
            sys.stderr.write(f"[byok] translate error ({provider}): {e}\n")
            return self._terminal(
                f"ByteAsk could not convert this request for {provider}.\n"
                f"Fix: start a new session, or pick another model with /model.")

        if provider == "anthropic":
            base, path, headers = ANTHROPIC_URL, "/v1/messages", mod.route_headers(user_key)
        else:
            base = GEMINI_URL
            path, headers = mod.route(model, user_key)

        conn, _ = _conn_for(base)
        try:
            conn.request("POST", path, body=provider_body, headers=headers)
            resp = conn.getresponse()
        except Exception as e:
            conn.close()
            return self._connect_fault(e, urllib.parse.urlparse(base).netloc, provider)

        if resp.status != 200:
            err = resp.read()
            conn.close()
            return self._upstream_status_fault(provider, resp.status, err, own_key=True)

        self._sse_headers()
        # translated.name_map maps wire tool names back to the engine's (namespace, name)
        # (MCP / sub-agent tools loaded by tool_search); an older translator has none.
        name_map = getattr(translated, "name_map", None)
        transducer = (mod.Transducer(model, name_map=name_map) if name_map is not None
                      else mod.Transducer(model))
        try:
            for ev in mod.iter_sse(iter(lambda: resp.read(2048), b"")):
                for frame in transducer.feed(ev):
                    self._emit(frame)
            for frame in transducer.close():
                self._emit(frame)
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            sys.stderr.write(f"[byok] {provider} stream error: {e}\n")
        finally:
            conn.close()
            tu = transducer.usage
            if provider == "anthropic":
                inp = tu["input"] + tu["cache_read"] + tu["cache_write"]
                cached, out = tu["cache_read"], tu["output"]
            else:
                inp, cached, out = tu["input"], tu["cached"], tu["output"]
            self._beacon(cfg, provider, model, inp, cached, out)

    # ---- own-key: openai (native Responses passthrough, no translation) ----
    def _stream_openai(self, cfg, model, body, user_key):
        conn, _ = _conn_for(OPENAI_URL)
        headers = {"Authorization": "Bearer " + user_key,
                   "Content-Type": "application/json",
                   "Accept": "text/event-stream"}
        try:
            conn.request("POST", "/v1/responses", body=body, headers=headers)
            resp = conn.getresponse()
        except Exception as e:
            conn.close()
            return self._connect_fault(e, urllib.parse.urlparse(OPENAI_URL).netloc, "OpenAI")
        if resp.status != 200:
            err = resp.read()
            conn.close()
            return self._upstream_status_fault("OpenAI", resp.status, err, own_key=True)

        self._sse_headers()
        acc, usage = [""], None
        try:
            while True:
                chunk = resp.read(2048)
                if not chunk:
                    break
                self._emit(chunk)
                try:
                    found = _scan_completed_usage(chunk.decode("utf-8", "replace"), acc)
                    if found:
                        usage = found
                except Exception:
                    pass
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            sys.stderr.write(f"[byok] openai stream error: {e}\n")
        finally:
            conn.close()
            u = usage or {}
            det = u.get("input_tokens_details") or {}
            self._beacon(cfg, "openai", model,
                         u.get("input_tokens"), det.get("cached_tokens"),
                         u.get("output_tokens"))

    # ---- self/*: the user's own OpenAI-compatible endpoint (their compute) ----
    def _route_self(self, cfg, model, body):
        alias = model[5:]                                   # strip "self/"
        ep = (cfg.get("endpoints") or {}).get(alias)
        if not isinstance(ep, dict) or not str(ep.get("base_url") or "").strip():
            # FAIL-CLOSED: a self/* prompt is private — never
            # managed-forward it to ByteAsk. An unknown alias is an explicit error.
            return self._terminal(
                f"ByteAsk does not know the self-hosted model '{alias}', and will not send "
                f"a private prompt anywhere else.\n"
                f"Fix: register it with  byteask models add {alias} --url <your endpoint>")
        base = _normalize_base(ep["base_url"])
        model_id = str(ep.get("model_id") or alias)
        wire = str(ep.get("wire") or "auto").lower()
        try:
            ctx = _ssl_ctx_for(base, ep)
        except Exception as e:
            return self._terminal(
                f"ByteAsk could not load the TLS settings for '{alias}': {e}\n"
                f"Fix: re-add the endpoint with a readable --ca-bundle path.")
        try:
            req = json.loads(body or b"{}")
            if not isinstance(req, dict):
                req = {}
        except Exception:
            req = {}
        # Responses passthrough (wire=responses; also auto's first attempt).
        if wire in ("responses", "auto"):
            conn = resp = None
            try:
                conn, resp = self._open_responses(base, model_id, req, ep, ctx)
            except Exception as e:
                if conn:
                    conn.close()
                if wire == "responses":
                    return self._connect_fault(
                        e, urllib.parse.urlparse(base).netloc, f"your endpoint '{alias}'")
                conn = resp = None                          # auto: fall through to chat
            if resp is not None and resp.status == 200:
                return self._relay_responses(cfg, alias, ep, conn, resp)
            if resp is not None:
                err = resp.read()
                conn.close()
                if wire == "responses":
                    return self._upstream_status_fault(
                        f"your endpoint '{alias}'", resp.status, err, own_key=False)
                # auto: this server has no /v1/responses -> translate to chat below.
        return self._self_chat(cfg, alias, base, model_id, req, ep, ctx)

    def _open_responses(self, base, model_id, req, ep, ctx):
        out = dict(req, model=model_id)
        conn, _ = _conn_for(base, ctx)
        conn.request("POST", _self_path(base, "responses"),
                     body=json.dumps(out).encode(), headers=_endpoint_headers(ep))
        return conn, conn.getresponse()

    def _relay_responses(self, cfg, alias, ep, conn, resp):
        self._sse_headers()
        acc, usage = [""], None
        try:
            while True:
                chunk = resp.read(2048)
                if not chunk:
                    break
                self._emit(chunk)
                try:
                    found = _scan_completed_usage(chunk.decode("utf-8", "replace"), acc)
                    if found:
                        usage = found
                except Exception:
                    pass
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            sys.stderr.write(f"[byok] self responses stream error: {e}\n")
        finally:
            conn.close()
            u = usage or {}
            det = u.get("input_tokens_details") or {}
            self._beacon_self(cfg, alias, u.get("input_tokens"),
                              det.get("cached_tokens"), u.get("output_tokens"))

    def _self_chat(self, cfg, alias, base, model_id, req, ep, ctx):
        try:
            chat_body, name_map = openai_compat_translate.translate_request(req, model_id)
        except openai_compat_translate.UnsupportedItemError as e:
            return self._terminal(
                f"ByteAsk cannot send this conversation to '{alias}': {e}\n"
                f"Fix: start a new session for this model.")
        except Exception as e:
            sys.stderr.write(f"[byok] translate error (self/{alias}): {e}\n")
            return self._terminal(
                f"ByteAsk could not convert this request for '{alias}'.\n"
                f"Fix: start a new session, or pick another model with /model.")
        conn, _ = _conn_for(base, ctx)
        try:
            conn.request("POST", _self_path(base, "chat/completions"),
                         body=json.dumps(chat_body).encode(), headers=_endpoint_headers(ep))
            resp = conn.getresponse()
        except Exception as e:
            conn.close()
            return self._connect_fault(
                e, urllib.parse.urlparse(base).netloc, f"your endpoint '{alias}'")
        if resp.status != 200:
            err = resp.read()
            conn.close()
            return self._upstream_status_fault(
                f"your endpoint '{alias}'", resp.status, err, own_key=False)
        self._sse_headers()
        transducer = openai_compat_translate.Transducer(
            model_id, name_map,
            known_tools={t["function"]["name"] for t in chat_body.get("tools") or []},
            custom_tools=openai_compat_translate.custom_tool_names(req.get("tools")))
        try:
            for ev in openai_compat_translate.iter_sse(iter(lambda: resp.read(2048), b"")):
                for frame in transducer.feed(ev):
                    self._emit(frame)
            for frame in transducer.close():
                self._emit(frame)
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:
            sys.stderr.write(f"[byok] self chat stream error: {e}\n")
        finally:
            conn.close()
            tu = transducer.usage
            self._beacon_self(cfg, alias, tu["input"], tu["cached"], tu["output"])

    def _beacon_self(self, cfg, alias, inp, cached, out):
        # The custom-endpoint beacon is DEFAULT-OFF (explicit opt-in via
        # `beacon_self` in the config) so a private model_id/URL never leaks. When
        # on it sends the ALIAS (user-chosen), never the raw model_id or URL.
        if cfg.get("beacon_self"):
            self._beacon(cfg, "self", alias, inp, cached, out)

    # ---- managed passthrough (no user key) ----
    def _passthrough_managed(self, cfg, method, body):
        gateway = (os.environ.get("BYOK_MANAGED_URL")
                   or cfg.get("gateway") or "https://code.byteask.ai").rstrip("/")
        jwt = (cfg.get("jwt") or "").strip()
        conn, _ = _conn_for(gateway)
        # Forward the original path (so /byteask/v1/responses, /usage, etc. all work),
        # attach the user's JWT, strip the loopback token. The managed gateway does the
        # translation + billing + /usage exactly as today.
        fwd = {"Content-Type": "application/json"}
        if jwt:
            fwd["Authorization"] = "Bearer " + jwt
        accept = self.headers.get("Accept")
        if accept:
            fwd["Accept"] = accept
        try:
            conn.request(method, self.path, body=body or None, headers=fwd)
            resp = conn.getresponse()
        except Exception as e:
            conn.close()
            return self._connect_fault(
                e, urllib.parse.urlparse(gateway).netloc or gateway, "ByteAsk")

        ctype = resp.getheader("Content-Type", "")
        if "text/event-stream" in ctype:
            self._sse_headers()
            try:
                while True:
                    chunk = resp.read(2048)
                    if not chunk:
                        break
                    self._emit(chunk)
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as e:
                sys.stderr.write(f"[byok] managed stream error: {e}\n")
            finally:
                conn.close()
        else:
            data = resp.read()
            conn.close()
            self.send_response(resp.status)
            self.send_header("Content-Type", ctype or "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            try:
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                pass

    # ---- usage beacon (metadata only; never the key, never the prompt) ----
    def _beacon(self, cfg, provider, model, inp, cached, out):
        if _NO_TELEMETRY or not (inp or out):
            return
        gateway = (os.environ.get("BYOK_MANAGED_URL")
                   or cfg.get("gateway") or "https://code.byteask.ai").rstrip("/")
        jwt = (cfg.get("jwt") or "").strip()
        if not jwt:
            return
        payload = json.dumps({
            "provider": provider, "model": model,
            "input_tokens": int(inp or 0), "cached_tokens": int(cached or 0),
            "output_tokens": int(out or 0)}).encode()

        def _send():
            try:
                req = urllib.request.Request(
                    gateway + "/byteask/v1/byok-usage", data=payload, method="POST",
                    headers={"Authorization": "Bearer " + jwt,
                             "Content-Type": "application/json"})
                urllib.request.urlopen(req, timeout=5).read()
            except Exception:
                pass   # fire-and-forget: tracking must never affect the user's turn

        threading.Thread(target=_send, daemon=True).start()


class _Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    port = int(os.environ.get("BYOK_SIDECAR_PORT", "8799"))
    # LOOPBACK ONLY — never bind a routable interface (the sidecar holds keys + JWT).
    srv = _Server(("127.0.0.1", port), Handler)
    sys.stderr.write(f"[byok] sidecar listening on 127.0.0.1:{port} (protocol {PROTOCOL_VERSION})\n")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
