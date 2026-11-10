#!/usr/bin/env python3
"""Shared user-facing error taxonomy for ByteAsk.

Why this module exists
----------------------
The engine decides BOTH whether to retry AND what text to show purely from the
wire shape we send, and most shapes DISCARD our message. Verified against
the engine source the 0.1.8-0.1.10 binaries were built from:

  status / frame we send            engine result        retried?  user reads
  --------------------------------  -------------------  --------  ------------------
  400                               InvalidRequest(body) no        the RAW body, braces
  401 / 402 / 403 / 404 / 413       UnexpectedStatus     YES, 5x   spinner, then + url
  429 type=usage_limit_reached      UsageLimitReached    no        engine copy, NOT ours
  429 anything else                 RetryLimit           no        "exceeded retry limit"
  500                               InternalServerError  YES, 5x   "high demand", NOT ours
  502 / other 5xx                   UnexpectedStatus     YES, 5x   spinner, then + url
  SSE failed code=invalid_prompt    InvalidRequest(msg)  NO        OUR MESSAGE, ALONE
  SSE failed code=rate_limit_exc..  Stream(msg, delay)   yes       OUR MESSAGE + backoff

Only the last two preserve what we wrote. A local fault that can never heal (no
CA bundle, a rejected key, an unregistered self/* alias) used to be sent as 502,
so the user waited ~6.2s of exponential backoff (200+400+800+1600+3200 ms) to be
shown a raw Python traceback with the LOOPBACK sidecar's url attached, and
concluded our servers were down.

So: terminal faults go out in-band as one `response.failed` frame with
TERMINAL_SSE_CODE. Transient faults keep their real status, because there the
engine's retry is exactly what we want.

Contract risk: `invalid_prompt` is UPSTREAM's string (codex-api/src/sse/
responses.rs, the `invalid_prompt | bio_policy` arm). Re-verify it against the
engine source after every upstream merge.

Pure stdlib. Served to user machines alongside byok_sidecar.py -- it is part of
that file's release unit, never served independently -- and imported by the
gateway server-side.
"""
import json
import ssl

# The engine maps this code to CodexErr::InvalidRequest: not retryable, and its
# Display is the bare message (no status prefix, no url, no spinner).
TERMINAL_SSE_CODE = "invalid_prompt"

# The engine maps this code to ApiError::Retryable: retried WITH the delay it
# parses out of the message, and our text stays on screen under the retry line.
BACKOFF_SSE_CODE = "rate_limit_exceeded"

# The terminal channel is gated to this exact path, where the engine is the
# known consumer. Every other route keeps REST semantics (401 stays 401) because
# the public gateway cannot assume who is calling it.
TURN_PATH = "/byteask/v1/responses"

# The engine parses a backoff out of the message with
#   (?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)
# so a backoff message MUST contain that literal phrasing or the delay is lost
# and the engine falls back to its own exponential backoff.
_BACKOFF_PHRASE = "try again in {secs} seconds"


def frame(event_type, payload):
    """One Responses SSE frame. The engine dispatches on the JSON ``type``; the
    ``event:`` line is redundant but standard. Same shape the three translators
    already emit, hoisted here so it stops being written four times."""
    payload = dict(payload, type=event_type)
    return f"event: {event_type}\ndata: {json.dumps(payload)}\n\n"


def terminal_frame(message, response_id="resp_byteask_error"):
    """A fault the user must act on. NOT retried; rendered verbatim and alone.

    The caller must follow this with the chunked-encoding terminator
    (``0\\r\\n\\r\\n``) or the engine sees a truncated stream and reports a
    disconnect instead of our message."""
    return frame("response.failed", {"response": {
        "id": response_id, "status": "failed",
        "error": {"code": TERMINAL_SSE_CODE, "message": message}}})


def backoff_frame(message, seconds, response_id="resp_byteask_busy"):
    """A transient capacity fault. Retried, honoring OUR delay.

    `message` should NOT already contain a retry phrase; the required
    "try again in N seconds" wording is appended here so the engine's regex can
    read the delay. Anything else and the delay is silently dropped."""
    secs = max(1, int(seconds or 1))
    text = f"{message.rstrip().rstrip('.')}, {_BACKOFF_PHRASE.format(secs=secs)}."
    return frame("response.failed", {"response": {
        "id": response_id, "status": "failed",
        "error": {"code": BACKOFF_SSE_CODE, "message": text}}})


def plan_limit_body(resets_at_epoch, plan=None):
    """The 429 body for a user who has exhausted THEIR ByteAsk budget.

    `type` must be exactly "usage_limit_reached" or the engine renders
    "exceeded retry limit, last status: 429" and drops our message entirely.

    NOTE the engine builds the visible copy from `plan_type` and the
    x-codex-promo-message HEADER -- never from a message in this body. With
    plan_type alone and no promo header it emits ChatGPT upgrade URLs and the
    word "Codex" to our users. Always send the header (see promo_header)."""
    err = {"type": "usage_limit_reached"}
    if resets_at_epoch is not None:
        # An EPOCH timestamp, not a duration. The gateway's rate limiter returns retry
        # SECONDS, so callers must add it to time.time() first.
        err["resets_at"] = int(resets_at_epoch)
    if plan:
        err["plan_type"] = str(plan)
    return {"error": err}


def promo_header(message):
    """The header the engine interpolates into its limit copy. It renders as
    "You've hit your usage limit. {message}, try again in ...", so `message`
    should be a fragment that reads correctly mid-sentence and must not contain
    a newline (headers are single-line)."""
    return ("x-codex-promo-message", " ".join(str(message).split()))


def is_terminal_status(status):
    """True when an upstream status will still be failing on the fifth retry: a
    4xx means the request itself is wrong (bad key, bad model id, bad payload).
    429 is excluded because the engine owns rate limits on its own path."""
    try:
        status = int(status)
    except (TypeError, ValueError):
        return False
    return 400 <= status < 500 and status != 429


def tls_advice(exc, host):
    """An actionable message when `exc` is a TLS trust failure, else None.

    CERTIFICATE_VERIFY_FAILED never recovers on retry -- the request did not even
    leave the machine. It is almost always a Python with no usable CA store (the
    python.org macOS builds ship none until their installer script is run), or a
    TLS-inspecting corporate proxy whose root certificate is not trusted.

    Matches the exception TYPE or its signature STRING: http.client raises
    ssl.SSLCertVerificationError directly, but urllib wraps the same failure in
    urllib.error.URLError, which is not an ssl.SSLError."""
    text = str(exc)
    if not isinstance(exc, ssl.SSLError) and "SSL" not in text \
       and "CERTIFICATE_VERIFY_FAILED" not in text:
        return None
    if "CERTIFICATE_VERIFY_FAILED" not in text:
        # Deliberately NOT interpolating `exc`: str() of an ssl.SSLError renders
        # as a Python tuple repr, and a raw exception in user-facing text is the
        # exact defect this module exists to remove. The raw detail still lands
        # in the sidecar log via the TERMINAL stderr line.
        return (f"ByteAsk could not negotiate a secure connection to {host}, so nothing "
                f"was sent.\n"
                f"Fix: if you are behind a proxy that inspects TLS, trust its root "
                f"certificate, then restart byteask.")
    # python3 -m pip, NOT pip3: pip3 may install into a different interpreter
    # than the one running this process, which is the one that needs the certs.
    return (f"ByteAsk could not verify the TLS certificate for {host}, so the request "
            f"never left this machine.\n"
            f"Your Python has no usable CA certificates, or a network appliance is "
            f"re-signing TLS traffic with a root it does not trust.\n"
            f"Fix: python3 -m pip install --upgrade certifi, then set "
            f'SSL_CERT_FILE="$(python3 -m certifi)" in your shell profile and restart '
            f"byteask.\n"
            f"macOS python.org build: run "
            f'"/Applications/Python 3.x/Install Certificates.command".\n'
            f"Corporate network: append your company root CA to that same file.")


def upstream_detail(raw):
    """The provider's own error sentence when the body is the usual JSON error
    envelope, else the trimmed raw body. Keeps the useful half of an upstream
    error out of a wall of JSON in the user's terminal."""
    if isinstance(raw, (bytes, bytearray)):
        text = raw[:800].decode("utf-8", "replace").strip()
    else:
        text = str(raw or "").strip()
    try:
        err = (json.loads(text) or {}).get("error")
        if isinstance(err, dict):
            msg = err.get("message")
            if isinstance(msg, str) and msg.strip():
                return msg.strip()
        elif isinstance(err, str) and err.strip():
            return err.strip()
    except Exception:
        pass
    return text or "(empty response body)"


def key_rejected(label, status, detail, own_key=True):
    """A user-actionable auth failure: their key, their fix.

    Kept distinct from ours_broken() on purpose -- a message telling the user to
    "check your key" when the fault is our missing server-side key is worse than
    no message at all."""
    if own_key:
        fix = (f"Fix: re-enter your {label} key with `byteask login` (choose \"API keys\"), "
               f"or switch to a ByteAsk-managed model with /model.")
    else:
        fix = (f"Fix: check the API key on this endpoint with "
               f"`byteask models add <alias> --url <url> --key <key>`.")
    return (f"{label} rejected the request (HTTP {status}): {detail}\n{fix}")


def ours_broken(family, alternatives="another model"):
    """An operator-side fault. Never hand the user a `Fix:` they cannot perform;
    say plainly that it is ours and give them a way around it."""
    return (f"{family} models are temporarily unavailable on ByteAsk. This is on our "
            f"side, not yours.\n"
            f"Fix: switch to {alternatives} with /model, or retry in a few minutes.\n"
            f"Still broken? founders@byteask.ai")
