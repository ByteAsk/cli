#!/bin/sh
# First run and the moments around sign-in:
#   * "How do you want to run ByteAsk?" comes before any email, and only the ByteAsk
#     account row ever asks for one (own key, ChatGPT plan and own server never do);
#   * the line printed after a sign-in names the plan, the credit and the account site;
#   * doctor, completion, mcp and help work signed out; `byteask upgrade` explains
#     itself instead of reinstalling; --help ends with docs, account and support links.
# Self-contained: stubs the gateway, the engine and the providers' key checks, isolates
# $BYTEASK_HOME. No network, no real email.
#
# Run:  sh tests/first-run.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2" | head -c 3000; echo;; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2" | head -c 3000; echo;; *) ok "$3";; esac; }

for _need in python3 curl script; do
  command -v "$_need" >/dev/null 2>&1 || { echo "SKIP first-run: '$_need' not installed"; exit 0; }
done
script -qec 'true' /dev/null >/dev/null 2>&1 || { echo "SKIP first-run: no util-linux 'script -qec'"; exit 0; }
# An operator's local pin of the subscription switches would hide what section B tests.
unset BYTEASK_SUBSCRIPTION_FLAGS_LOCAL

TMP=$(mktemp -d); SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
# Stub engine: `login` writes a ChatGPT auth.json whose id_token carries $STUB_PLAN,
# `logout` removes it, the local commands answer, anything else is a launch.
cat > "$BIN/byteask-engine" <<'ENGINE_EOF'
#!/bin/sh
[ "${1:-}" = --sandbox ] && shift 2
case "${1:-}" in
  login)
    python3 - "$BYTEASK_HOME/auth.json" "${STUB_PLAN:-plus}" <<'PY'
import base64, json, sys
p = base64.urlsafe_b64encode(json.dumps(
    {"https://api.openai.com/auth": {"chatgpt_plan_type": sys.argv[2]}}).encode()).decode().rstrip("=")
json.dump({"tokens": {"id_token": "h.%s.s" % p}}, open(sys.argv[1], "w"))
PY
    echo "ENGINE_LOGIN"; exit 0;;
  logout) rm -f "$BYTEASK_HOME/auth.json"; echo "ENGINE_LOGOUT"; exit 0;;
  doctor) echo "DOCTOR_RAN"; exit 0;;
  completion) echo "COMPLETION_RAN $*"; exit 0;;
  mcp) echo "MCP_RAN $*"; exit 0;;
esac
_c="$BYTEASK_HOME/config.toml"
echo "ENGINE_LAUNCHED $* provider=$(sed -n 's/^model_provider = "\(.*\)"$/\1/p' "$_c" | head -1) model=$(sed -n 's/^model = "\(.*\)"$/\1/p' "$_c" | head -1)"
ENGINE_EOF
chmod +x "$BIN/byteask-engine"

# The providers' key checks (validate_key) are the only calls that would leave the
# machine: answer them 200 and pass everything else to the real curl.
REAL_CURL=$(command -v curl)
mkdir -p "$TMP/cbin"
cat > "$TMP/cbin/curl" <<EOF
#!/bin/sh
for a in "\$@"; do
  case "\$a" in
    *api.openai.com*|*api.anthropic.com*|*generativelanguage.googleapis.com*)
      for b in "\$@"; do case "\$b" in *http_code*) printf 200;; esac; done
      exit 0;;
  esac
done
exec "$REAL_CURL" "\$@"
EOF
chmod +x "$TMP/cbin/curl"

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
GW="http://127.0.0.1:$PORT"
USAGE_FILE="$TMP/usage.json"; FLAGS_FILE="$TMP/flags.json"; REQ_LOG="$TMP/requests.log"
: > "$USAGE_FILE"; : > "$FLAGS_FILE"; : > "$REQ_LOG"
cat > "$TMP/stub.py" <<'PY'
import json, os, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
PORT = int(sys.argv[1])
def body_of(env):
    try: return open(os.environ[env]).read().strip()
    except Exception: return ""
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _s(self, code, body, ct="application/json"):
        b = body.encode(); self.send_response(code); self.send_header("Content-Type", ct)
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def _rec(self):
        with open(os.environ["REQ_LOG"], "a") as f: f.write("%s %s\n" % (self.command, self.path))
    def do_GET(self):
        self._rec()
        p = self.path
        if p.startswith("/auth/account-exists"): return self._s(200, '{"exists": false, "blocked": false}')
        if p.startswith("/auth/verify"): return self._s(200, "ok", "text/plain")
        if p.startswith("/byteask/v1/usage"):
            b = body_of("USAGE_FILE"); return self._s(200, b) if b else self._s(404, "{}")
        if p.startswith("/byteask/v1/subscription-flags"):
            b = body_of("FLAGS_FILE"); return self._s(200, b) if b else self._s(404, "{}")
        if p.endswith("/models"): return self._s(200, '{"data": [{"id": "local-model"}]}')
        return self._s(404, "{}")
    def do_POST(self):
        self._rec()
        n = int(self.headers.get("Content-Length") or 0); self.rfile.read(n)
        if self.path == "/auth/start": return self._s(200, '{"poll_token": "pt1"}')
        if self.path == "/auth/poll": return self._s(200, '{"status": "approved", "access_token": "FIRSTRUNTOKEN"}')
        return self._s(404, "{}")
ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
PY
USAGE_FILE="$USAGE_FILE" FLAGS_FILE="$FLAGS_FILE" REQ_LOG="$REQ_LOG" python3 "$TMP/stub.py" "$PORT" & SRV_PID=$!
for _i in 1 2 3 4 5 6 7 8 9 10; do curl -fsS "$GW/auth/verify" >/dev/null 2>&1 && break; sleep 0.3; done
: > "$REQ_LOG"

HOME_BX="$TMP/home/.byteask"
# A fresh install: what install.sh leaves behind (managed provider, no token), plus the
# helpers own-key routing needs and a small catalog (best model first per provider).
seed_fresh() {
  rm -rf "$TMP/home"; mkdir -p "$HOME_BX"
  cp "$HERE"/../helpers/*.py "$HOME_BX/"
  printf '%s' "$GW" > "$HOME_BX/gateway"
  cat > "$HOME_BX/models-catalog.json" <<'EOF'
{"models": [
 {"slug": "auto", "visibility": "list", "priority": 0},
 {"slug": "gpt-5.4", "visibility": "hide", "priority": 1},
 {"slug": "gpt-6-astra", "visibility": "list", "priority": 2},
 {"slug": "gpt-6-sol", "visibility": "list", "priority": 4},
 {"slug": "claude-fable-5.1", "visibility": "list", "priority": 12},
 {"slug": "claude-opus-5-5", "visibility": "list", "priority": 13},
 {"slug": "gemini-3.8-flash", "visibility": "list", "priority": 20}
]}
EOF
  cat > "$HOME_BX/config.toml" <<EOF
model = "auto"
model_provider = "byteask"
web_search = "live"
model_catalog_json = "$HOME_BX/models-catalog.json"

[model_providers.byteask]
name = "ByteAsk"
base_url = "$GW/byteask/v1"
wire_api = "responses"
requires_openai_auth = false

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
EOF
  : > "$REQ_LOG"
}
cfg() { sed -n "s/^$1 = \"\(.*\)\"\$/\1/p" "$HOME_BX/config.toml" | head -1; }

# Bare `byteask` on a pseudo-terminal, fed the given keystrokes (printf %b escapes).
fr_run() {   # $1 = feed, $2 = extra env assignments (optional)
  # timeout: a screen still waiting for input fails its assertions instead of hanging.
  printf '%b' "$1" | timeout 60 script -qec "PATH='$TMP/cbin:$PATH' LANG=C.UTF-8 LC_ALL=C.UTF-8 TERM=xterm BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 BYOK_SKIP_SIDECAR=1 ${2:-} '$BIN/byteask'" /dev/null 2>&1 || true
}
nt_run() { PATH="$TMP/cbin:$PATH" BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 BYOK_SKIP_SIDECAR=1 "$BIN/byteask" "$@" </dev/null 2>&1; }
count() { printf '%s' "$2" | grep -o "$1" | grep -c .; }

echo "== A. the first run asks how to run ByteAsk before any email =="
seed_fresh; CFG_BEFORE=$(cat "$HOME_BX/config.toml")
OUT=$(fr_run 'q')
has   "How do you want to run ByteAsk?"   "$OUT" "A1 asks the question"
has   "ByteAsk account"                   "$OUT" "A2 offers the ByteAsk account"
has   'Free, $5 of credit a month. Same account as app.byteask.ai' "$OUT" "A3 says the account is free, its credit, and that it is the app.byteask.ai account"
has   "Your own API key"                  "$OUT" "A4 offers your own API key"
has   "Your own server"                   "$OUT" "A5 offers your own server"
hasnt "Email:"                            "$OUT" "A6 no email prompt before a choice"
hasnt "Your ChatGPT plan"                 "$OUT" "A7 the ChatGPT row follows its server switch (off here)"
hasnt "ENGINE_LAUNCHED"                   "$OUT" "A8 cancelling launches nothing"
has   "Nothing was set up"                "$OUT" "A9 cancelling says nothing was set up"
[ "$CFG_BEFORE" = "$(cat "$HOME_BX/config.toml")" ] && ok "A10 cancelling leaves config.toml as it was" || bad "A10 cancelling rewrote config.toml"
grep -q '^POST /auth/start' "$REQ_LOG" && bad "A11 an email sign-in was started" || ok "A11 no sign-in request was sent"

echo "== B. with the ChatGPT switch on, the plan is the third choice =="
seed_fresh; printf '{"chatgpt": true}' > "$FLAGS_FILE"
OUT=$(fr_run 'q')
has "Your ChatGPT plan  Use your Plus or Pro subscription" "$OUT" "B1 offers the ChatGPT Plus or Pro subscription"
_order=$(printf '%s\n' "$OUT" | grep -o 'ByteAsk account\|Your own API key\|Your ChatGPT plan\|Your own server' | tr '\n' '|')
[ "$_order" = "ByteAsk account|Your own API key|Your ChatGPT plan|Your own server|" ] \
  && ok "B2 order: account, own key, ChatGPT, own server" || bad "B2 wrong order: $_order"
has "type 1-4" "$OUT" "B3 the hint counts four rows"

echo "== C. your own OpenAI key: no email, no account, launches on an OpenAI model =="
seed_fresh; : > "$FLAGS_FILE"
OUT=$(fr_run '21sk-test-openai\n')
hasnt "Email:"                 "$OUT" "C1 never asks for an email"
grep -q '^POST /auth/start' "$REQ_LOG" && bad "C2 a sign-in was started" || ok "C2 no sign-in request was sent"
[ "$(cfg model_provider)" = byok-local ] && ok "C3 provider is the local own-key route" || bad "C3 provider is '$(cfg model_provider)'"
[ "$(cfg model)" = gpt-6-astra ] && ok "C4 model is the first listed OpenAI model" || bad "C4 model is '$(cfg model)'"
has "No account needed"        "$OUT" "C5 says no account is needed"
has "ENGINE_LAUNCHED"          "$OUT" "C6 the engine starts"
grep -q 'sk-test-openai' "$HOME_BX/byok-config.json" 2>/dev/null && ok "C7 the key is saved in byok-config.json" || bad "C7 key not saved"
grep -q 'sk-test-openai' "$HOME_BX/config.toml" && bad "C8 the key leaked into config.toml" || ok "C8 the key is not in config.toml"
OUT=$(nt_run)
has   "ENGINE_LAUNCHED" "$OUT" "C9 the next launch starts with no sign-in"
hasnt "not signed in"   "$OUT" "C10 and never asks for one"

echo "== D. your own Anthropic key starts on a Claude model =="
seed_fresh
OUT=$(fr_run '22sk-ant-test\n')
[ "$(cfg model)" = claude-fable-5.1 ] && ok "D1 model is the first listed Claude model" || bad "D1 model is '$(cfg model)'"
hasnt "Email:" "$OUT" "D2 never asks for an email"

echo "== E. the ByteAsk account: the question, then the email =="
seed_fresh
OUT=$(fr_run '\ndev@x.com\n\n')
has   "We email you a sign-in link"   "$OUT" "E1 says what happens next"
has   "Email:"                        "$OUT" "E2 asks for the email after the choice"
has   "Signed in as dev@x.com"        "$OUT" "E3 confirms the sign-in"
has   "ENGINE_LAUNCHED"               "$OUT" "E7 the engine starts"
hasnt "API keys"                      "$OUT" "E8 no second settings screen after choosing the account"
[ "$(cfg experimental_bearer_token)" = FIRSTRUNTOKEN ] && ok "E9 the session is stored" || bad "E9 token not stored"
_q=$(printf '%s' "$OUT" | grep -n 'How do you want to run ByteAsk' | head -1 | cut -d: -f1)
_e=$(printf '%s' "$OUT" | grep -n 'Email:' | head -1 | cut -d: -f1)
[ -n "$_q" ] && [ -n "$_e" ] && [ "$_q" -lt "$_e" ] && ok "E10 the question comes before the email prompt" || bad "E10 question/email order ($_q/$_e)"

echo "== F. after sign-in: one line with the plan, the credit and the account site =="
seed_fresh; printf '{"plan": "free", "used_5h_pct": 3}' > "$USAGE_FILE"
OUT=$(fr_run '\ndev@x.com\n\n')
has   "Signed in as dev@x.com · Free plan · Plan and billing: https://app.byteask.ai (same account)" "$OUT" "F0 names the plan and the account site, and says it is the same account"
hasnt "credit left" "$OUT" "F0b no credit figure while the gateway sends none"
# Call the real function; the wrapper's test seam loads it without running anything.
sl() {   # $1 = /usage body
  printf '%s' "$1" > "$USAGE_FILE"
  BYTEASK_SOURCE_ONLY=1 BYTEASK_HOME="$HOME_BX" LANG=C.UTF-8 LC_ALL=C.UTF-8 sh -c '. "$1"; _scr_tokens; _signin_line "$2" tok you@x.com' sh "$BIN/byteask" "$GW" 2>&1
}
OUT=$(sl '{"plan": "pro", "credit_left_usd": 3.1}')
has "Signed in as you@x.com · Pro plan · \$3.10 of credit left · Plan and billing: https://app.byteask.ai (same account)" "$OUT" "F1 the full line with credit"
has '$5 of credit left'  "$(sl '{"plan": "free", "credit_left_usd": 5}')"    "F2 whole dollars print without cents"
has '$0 of credit left'  "$(sl '{"plan": "free", "credit_left_usd": -2.5}')" "F3 a negative balance prints as \$0"
OUT=$(sl "$(printf '{"plan": "pro\033[2J"}')")
hasnt "$(printf '\033')" "$OUT" "F4 a control character from the gateway never reaches the terminal"
has   "Pro2J plan"       "$OUT" "F4b only the plain characters are kept"
: > "$USAGE_FILE"
OUT=$(sl '')
has "Signed in as you@x.com · Plan and billing: https://app.byteask.ai (same account)" "$OUT" "F5 no /usage answer: the line still names the account site"

echo "== G. your ChatGPT Plus plan: no email, an OpenAI session =="
seed_fresh; printf '{"chatgpt": true}' > "$FLAGS_FILE"
OUT=$(fr_run '3' "STUB_PLAN=plus")
has   "Plus or higher. Free and Go accounts are not supported" "$OUT" "G1 states the plan requirement before sign-in"
hasnt "Email:"           "$OUT" "G2 never asks for an email"
[ "$(cfg model_provider)" = openai ] && ok "G3 provider is the ChatGPT session" || bad "G3 provider is '$(cfg model_provider)'"
has   "Using your ChatGPT subscription" "$OUT" "G4 confirms the subscription"
has   "ENGINE_LAUNCHED"  "$OUT" "G5 the engine starts"

echo "== H. a ChatGPT Go account is refused and offered the other choices =="
seed_fresh; printf '{"chatgpt": true}' > "$FLAGS_FILE"; CFG_BEFORE=$(cat "$HOME_BX/config.toml")
OUT=$(fr_run '3q' "STUB_PLAN=go")
has   'is on the "go" plan'                         "$OUT" "H1 the refusal names the plan"
has   "Choose another way to run ByteAsk below"     "$OUT" "H2 it points at the other choices"
[ "$(count 'How do you want to run ByteAsk' "$OUT")" -ge 2 ] && ok "H3 the choices are shown again" || bad "H3 the choices were not shown again"
hasnt "ENGINE_LAUNCHED"                             "$OUT" "H4 nothing launches on a refused plan"
[ "$CFG_BEFORE" = "$(cat "$HOME_BX/config.toml")" ] && ok "H5 config.toml unchanged" || bad "H5 a refused plan rewrote config.toml"
[ -f "$HOME_BX/auth.json" ] && bad "H6 the refused ChatGPT credential was kept" || ok "H6 the refused ChatGPT credential is removed"

echo "== I. your own server: no email, starts on the server's model =="
seed_fresh; : > "$FLAGS_FILE"
OUT=$(fr_run "3$GW\n\n\n\nn\n")
hasnt "Email:" "$OUT" "I1 never asks for an email"
[ "$(cfg model)" = self/my-server ] && ok "I2 model is the new server (default name my-server)" || bad "I2 model is '$(cfg model)'"
has "ENGINE_LAUNCHED" "$OUT" "I3 the engine starts"

echo "== J. an expired session goes straight back to sign-in, without the choice =="
seed_fresh
_exp=$(python3 -c 'import base64,json; b=lambda o: base64.urlsafe_b64encode(json.dumps(o).encode()).decode().rstrip("="); print("%s.%s.sig" % (b({"alg":"HS256"}), b({"sub":"old@x.com","exp":1000000000})))')
printf 'model = "auto"\nmodel_provider = "byteask"\n\n[model_providers.byteask]\nbase_url = "%s/byteask/v1"\nexperimental_bearer_token = "%s"\n' "$GW" "$_exp" > "$HOME_BX/config.toml"
OUT=$(fr_run 'old@x.com\n')
hasnt "How do you want to run ByteAsk?" "$OUT" "J1 a returning user is not asked again"
has   "has expired"                     "$OUT" "J2 says why it asks"

echo "== K. byteask byok set, signed out: the key is enough =="
seed_fresh
OUT=$(printf 'sk-piped\n' | PATH="$TMP/cbin:$PATH" BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 BYOK_SKIP_SIDECAR=1 "$BIN/byteask" byok set gemini 2>&1)
[ "$(cfg model)" = gemini-3.8-flash ] && ok "K1 model switched to the key's provider" || bad "K1 model is '$(cfg model)'"
has   "No account needed" "$OUT" "K2 says no account is needed"
has   "ENGINE_LAUNCHED"   "$OUT" "K3 launches"
hasnt "not signed in"     "$OUT" "K4 no sign-in nudge"

echo "== M. --help ends with docs, account and support links =="
OUT=$(nt_run --help); rc=$?
[ "$rc" = 0 ] && ok "M1 --help exits 0 signed out" || bad "M1 --help exit $rc"
_tail=$(printf '%s\n' "$OUT" | tail -3)
has "Docs:              https://byteask.ai/docs" "$_tail" "M2 docs link"
has "Account and plan:  https://app.byteask.ai"  "$_tail" "M3 account link"
has "Help:              support@byteask.ai"      "$_tail" "M4 support address"

echo "== O. the sign-in timeout points at the own-key command =="
grep -q "Or skip email: 'byteask login --with-api-key'" "$WRAP" "$HERE/../byteask.ps1" && bad "O1 the timeout still suggests login --with-api-key" || ok "O1 no login --with-api-key hint"
grep -q 'use your own key, no account: byteask byok set openai' "$WRAP" && ok "O2 the timeout suggests byteask byok set" || bad "O2 own-key hint missing"
grep -q 'use your own key, no account: byteask byok set openai' "$HERE/../byteask.ps1" && ok "O3 byteask.ps1 says the same" || bad "O3 own-key hint missing in byteask.ps1"

echo "== P. no price claim on the models row =="
grep -q '20% off' "$WRAP" "$HERE/../byteask.ps1" && bad "P1 '20% off' is still in a launcher" || ok "P1 no '20% off' in either launcher"
grep -q 'GPT, Claude, Gemini and open models, paid from your credit' "$WRAP" && ok "P2 the row says what pays for it" || bad "P2 new row text missing"

echo
echo "first-run: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
