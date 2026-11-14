#!/bin/sh
# Tests byteask.ps1 (the native Windows launcher) on Linux via PowerShell Core.
# Same shape as onboard.sh: stubs the gateway auth + a byteask-engine.exe, isolates
# $BYTEASK_HOME. Skips cleanly if pwsh isn't available.
#
# Run:  BYTEASK_PWSH=/path/to/pwsh sh tests/win-client.sh   (or just `pwsh` on PATH)
set -u

PWSH="${BYTEASK_PWSH:-$(command -v pwsh 2>/dev/null || true)}"
if [ -z "$PWSH" ] || [ ! -x "$PWSH" ]; then
  echo "SKIP win-client (no pwsh; set BYTEASK_PWSH=/path/to/pwsh)"; exit 0
fi

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask.ps1"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

TMP=$(mktemp -d); SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask.ps1"
# stub engine (named .exe so byteask.ps1 finds it); emits a marker if triggered.
cat > "$BIN/byteask-engine.exe" <<'ENGINE_EOF'
#!/bin/sh
if [ -f "$BYTEASK_HOME/.stub-emit" ]; then
  _m=$(cat "$BYTEASK_HOME/.stub-emit"); rm -f "$BYTEASK_HOME/.stub-emit"
  printf '%s\n' "$_m" > "$BYTEASK_HOME/.byteask-auth-request"
fi
# ChatGPT sign-in: install the auth.json the test staged; logout removes it.
case "${1:-}" in
  login)  cp "$BYTEASK_HOME/auth.json.next" "$BYTEASK_HOME/auth.json"; echo "ENGINE_LOGIN"; exit 0;;
  logout) rm -f "$BYTEASK_HOME/auth.json"; exit 0;;
esac
echo "ENGINE_LAUNCHED $*"
ENGINE_EOF
chmod +x "$BIN/byteask-engine.exe"

HOME_BX="$TMP/home/.byteask"; mkdir -p "$HOME_BX"
PORT=8815; GW="http://127.0.0.1:$PORT"

seed_unsigned()   { cat > "$HOME_BX/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
base_url = "$GW/byteask/v1"
EOF
}
seed_signed()     { seed_unsigned; printf 'experimental_bearer_token = "%s"\n' "${1:-SEEDTOKEN}" >> "$HOME_BX/config.toml"; }
signed()          { grep -q 'experimental_bearer_token' "$HOME_BX/config.toml" 2>/dev/null; }

cat > "$TMP/stub.py" <<PY
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
PORT=$PORT
class H(BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def _s(self,c,b,ct="application/json"):
        b=b.encode(); self.send_response(c); self.send_header("Content-Type",ct)
        self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path.startswith("/version"): return self._s(200,"0.1.1","text/plain")
        if self.path.startswith("/auth/verify"): return self._s(200,"ok","text/plain")
        if self.path.startswith("/byteask/v1/usage"):
            return self._s(200,json.dumps({"plan":"pro","credit_left_usd":12.5}))
        if self.path.startswith("/byteask/v1/subscription-flags"):
            return self._s(200,json.dumps({"chatgpt":True}))
        return self._s(404,"{}")
    def do_POST(self):
        if self.path=="/auth/start":
            return self._s(200,json.dumps({"poll_token":"pt1","dev_magic_link":"http://127.0.0.1:%d/auth/verify?t=x"%PORT}))
        if self.path=="/auth/poll":
            return self._s(200,json.dumps({"status":"approved","access_token":"WINTOKEN123"}))
        return self._s(404,"{}")
ThreadingHTTPServer(("127.0.0.1",PORT),H).serve_forever()
PY
python3 "$TMP/stub.py" & SRV_PID=$!
sleep 1
printf '%s' "$GW" > "$HOME_BX/gateway"

# run byteask.ps1 non-interactively (stdin from arg or /dev/null)
psrun() { BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$PWSH" -NoProfile -File "$BIN/byteask.ps1" "$@" </dev/null 2>&1; }

echo "== A. --version =="
has "byteask $(sed -n "s/^\$VERSION = '\(.*\)'/\1/p" "$BIN/byteask.ps1" | head -1)" "$(psrun --version)" "A1 --version"

echo "== B. unsigned + non-console -> nudge, no launch =="
seed_unsigned; OUT=$(psrun)
has   "not signed in"   "$OUT" "B1 nudge shown"
hasnt "ENGINE_LAUNCHED" "$OUT" "B2 engine not launched"

echo "== C. explicit login (piped email) writes the token + launches nothing =="
seed_unsigned
OUT=$(printf 'dev@x.com\n' | BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$PWSH" -NoProfile -File "$BIN/byteask.ps1" login 2>&1)
has "Signed in as dev@x.com" "$OUT" "C1 login confirms"
grep -q 'experimental_bearer_token = "WINTOKEN123"' "$HOME_BX/config.toml" && ok "C2 token written" || bad "C2 token not written"

echo "== D. signed -> engine launches, no onboarding =="
seed_signed "TOK_D"; OUT=$(psrun)
has   "ENGINE_LAUNCHED"    "$OUT" "D1 launches"
hasnt "Welcome to ByteAsk" "$OUT" "D2 no onboarding when signed in"

echo "== E. logout removes the token, keeps the rest =="
seed_signed "TOK_E"; OUT=$(psrun logout)
has "Logged out" "$OUT" "E1 logout confirms"
signed && bad "E2 token still present" || ok "E2 token removed"
grep -q '^model_provider = "byteask"' "$HOME_BX/config.toml" && ok "E3 provider preserved" || bad "E3 config clobbered"

echo "== F. logout when not signed in =="
has "not signed in" "$(psrun logout)" "F1 not-signed-in message"

echo "== G. TUI /logout marker -> wrapper clears token =="
seed_signed "TOK_G"; printf 'logout\n' > "$HOME_BX/.stub-emit"; OUT=$(psrun)
has "Logged out" "$OUT" "G1 marker logout"
signed && bad "G2 token still present after /logout marker" || ok "G2 token cleared"

echo "== H. TUI /login marker -> wrapper re-auths (piped email) + relaunches =="
seed_signed "OLD"; printf 'login\n' > "$HOME_BX/.stub-emit"
OUT=$(printf 'dev@x.com\n' | BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$PWSH" -NoProfile -File "$BIN/byteask.ps1" 2>&1)
grep -q 'experimental_bearer_token = "WINTOKEN123"' "$HOME_BX/config.toml" && ok "H1 /login wrote new token" || bad "H1 new token not written"
has "ENGINE_LAUNCHED" "$OUT" "H2 relaunched after /login"
[ -f "$HOME_BX/.byteask-auth-request" ] && bad "H3 marker not consumed" || ok "H3 marker consumed"

echo "== I. --help ends with docs, account and support links =="
OUT=$(psrun --help)
has "Docs:              https://byteask.ai/docs" "$OUT" "I1 docs link"
has "Account and plan:  https://app.byteask.ai"  "$OUT" "I2 account link"
has "Help:              support@byteask.ai"      "$OUT" "I3 support address"

echo "== L. the sign-in line: plan, credit, the account site =="
seed_unsigned
OUT=$(printf 'dev@x.com\n' | BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$PWSH" -NoProfile -File "$BIN/byteask.ps1" login 2>&1)
has "Signed in as dev@x.com - Pro plan - \$12.50 of credit left - Plan and billing: https://app.byteask.ai (same account)" "$OUT" "L1 the full line"

echo "== M. first run: the question comes before any email =="
# Drives the function directly: the launch loop only reaches it on a real console.
cat > "$TMP/fr.ps1" <<'PS'
$env:BYTEASK_PS_TEST = '1'
. (Join-Path $args[0] 'byteask.ps1')
Invoke-FirstRun
Write-Host "FIRSTRUN_RETURNED"
PS
frrun() { printf "$1" | BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 BYOK_ASSUME_TTY=1 "$PWSH" -NoProfile -File "$TMP/fr.ps1" "$BIN" 2>&1; }
seed_unsigned
OUT=$(frrun '0\n'); rc=$?
has   "How do you want to run ByteAsk?" "$OUT" "M1 asks the question"
_order=$(printf '%s\n' "$OUT" | grep -o 'ByteAsk account\|Your own API key\|Your ChatGPT plan\|Your own server' | tr '\n' '|')
[ "$_order" = "ByteAsk account|Your own API key|Your ChatGPT plan|Your own server|" ] \
  && ok "M2 order: account, own key, ChatGPT, own server" || bad "M2 wrong order: $_order"
has   'Free, $5 of credit a month. Same account as app.byteask.ai' "$OUT" "M3 the account row says free, credit, same account"
hasnt "Email"                 "$OUT" "M4 cancelling never asks for an email"
has   "Nothing was set up"    "$OUT" "M5 cancelling says nothing was set up"
[ "$rc" = 1 ] && ok "M6 cancelling exits 1" || bad "M6 exit $rc"
seed_unsigned
OUT=$(frrun '1\ndev@x.com\n\n')
has "We email you a sign-in link" "$OUT" "M7 the account row says what happens next"
grep -q 'experimental_bearer_token = "WINTOKEN123"' "$HOME_BX/config.toml" && ok "M8 the account row signs in" || bad "M8 token not written"
has "FIRSTRUN_RETURNED" "$OUT" "M9 returns to launch"

echo "== N. first run, ChatGPT: Go is refused and offered the rest; Plus is used =="
mk_auth() { python3 -c 'import base64,json,sys; p=base64.urlsafe_b64encode(json.dumps({"https://api.openai.com/auth":{"chatgpt_plan_type":sys.argv[1]}}).encode()).decode().rstrip("="); print(json.dumps({"tokens":{"id_token":"h.%s.s" % p}}))' "$1" > "$HOME_BX/auth.json.next"; }
seed_unsigned; mk_auth go; CFG_BEFORE=$(cat "$HOME_BX/config.toml")
OUT=$(frrun '3\n0\n')
has   'is on the "go" plan'                     "$OUT" "N1 the refusal names the plan"
has   "Choose another way to run ByteAsk below" "$OUT" "N2 it points at the other choices"
[ "$(printf '%s' "$OUT" | grep -c 'How do you want to run ByteAsk')" -ge 2 ] && ok "N3 the choices are shown again" || bad "N3 choices not shown again"
[ "$CFG_BEFORE" = "$(cat "$HOME_BX/config.toml")" ] && ok "N4 config.toml unchanged" || bad "N4 config.toml rewritten"
[ -f "$HOME_BX/auth.json" ] && bad "N5 the refused credential was kept" || ok "N5 the refused credential is removed"
seed_unsigned; mk_auth plus
OUT=$(frrun '3\n')
has "Using your ChatGPT subscription" "$OUT" "N6 Plus is used"
grep -q '^model_provider = "openai"$' "$HOME_BX/config.toml" && ok "N7 provider is the ChatGPT session" || bad "N7 provider not switched"
grep -q '^model = ' "$HOME_BX/config.toml" && bad "N8 a managed model was carried into the ChatGPT session" || ok "N8 no managed model carried over"
rm -f "$HOME_BX/auth.json" "$HOME_BX/auth.json.next"

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
