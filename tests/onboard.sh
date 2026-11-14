#!/bin/sh
# Auth UX tests for the byteask wrapper: interactive onboarding of an unsigned
# user (like claude/codex) + `byteask logout`. Exhaustive edge coverage.
# Self-contained: stubs the gateway's /auth/{start,poll} + magic-link, isolates
# $BYTEASK_HOME, stubs the engine. No network, no real email.
#
# Run:  sh tests/onboard.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

TMP=$(mktemp -d); SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
# Stub engine. If a one-shot trigger `.stub-emit` exists, it writes that word to the
# auth-request marker (simulating the TUI /login|/logout) and deletes the trigger, so
# a relaunch doesn't loop forever.
cat > "$BIN/byteask-engine" <<'ENGINE_EOF'
#!/bin/sh
if [ -f "$BYTEASK_HOME/.stub-emit" ]; then
  _m=$(cat "$BYTEASK_HOME/.stub-emit"); rm -f "$BYTEASK_HOME/.stub-emit"
  printf '%s\n' "$_m" > "$BYTEASK_HOME/.byteask-auth-request"
fi
echo "ENGINE_LAUNCHED $* token=$(sed -n 's/^experimental_bearer_token = "\(.*\)"$/\1/p' "$BYTEASK_HOME/config.toml" 2>/dev/null | head -1)"
ENGINE_EOF
chmod +x "$BIN/byteask-engine"

HOME_BX="$TMP/home/.byteask"; mkdir -p "$HOME_BX"
PORT=8814
GW="http://127.0.0.1:$PORT"

# --- config seeders ---------------------------------------------------------
seed_unsigned()   { cat > "$HOME_BX/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
base_url = "$GW/byteask/v1"
EOF
}
seed_signed()     { seed_unsigned; printf 'experimental_bearer_token = "%s"\n' "${1:-SEEDTOKEN}" >> "$HOME_BX/config.toml"; }
seed_nonbyteask() { cat > "$HOME_BX/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "openai"
EOF
}
seed_none()       { rm -f "$HOME_BX/config.toml"; }
signed()          { grep -q 'experimental_bearer_token' "$HOME_BX/config.toml" 2>/dev/null; }

# --- auth stub --------------------------------------------------------------
# Mirrors the real gateway closely enough to exercise the referral-suppression path:
# tracks emails that have completed a sign-in (like the seen_users table) and answers
# GET /auth/account-exists?email= with {"exists": bool} (pure read, set only at poll).
cat > "$TMP/stub.py" <<PY
import json
import os
from urllib.parse import urlparse, parse_qs
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
PORT=$PORT
SEEN=set(); POLL_EMAIL={}
_BLOCK_MSG="This email address is not supported. Please sign in with a different email."
def _blocked(em): return em.rsplit("@",1)[-1]=="blocked.example"
class H(BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def _s(self,c,b,ct="application/json"):
        b=b.encode(); self.send_response(c); self.send_header("Content-Type",ct)
        self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def _body(self):
        try: n=int(self.headers.get("Content-Length") or 0)
        except Exception: n=0
        try: return json.loads(self.rfile.read(n) or b"{}")
        except Exception: return {}
    def _rec(self):
        with open(os.environ["STUB_REQ_LOG"],"a") as f:
            f.write("%s %s anon=%s\\n" % (self.command,self.path,self.headers.get("X-Anon-Id") or "-"))
    def do_GET(self):
        self._rec()
        if self.path.startswith("/version"): return self._s(200,"0.1.1","text/plain")
        if self.path.startswith("/auth/verify"): return self._s(200,"ok","text/plain")
        if self.path.startswith("/auth/account-exists"):
            em=(parse_qs(urlparse(self.path).query).get("email",[""])[0] or "").strip().lower()
            if "@" not in em: return self._s(400,json.dumps({"error":"bad email"}))
            if _blocked(em): return self._s(200,json.dumps({"exists":False,"blocked":True,"error":_BLOCK_MSG}))
            return self._s(200,json.dumps({"exists": em in SEEN,"blocked":False}))
        return self._s(404,"{}")
    def do_POST(self):
        self._rec()
        if self.path=="/auth/start":
            b=self._body(); _em=(b.get("email") or "").strip().lower()
            if _blocked(_em): return self._s(200,json.dumps({"error":_BLOCK_MSG}))
            POLL_EMAIL["pt1"]=_em
            return self._s(200,json.dumps({"poll_token":"pt1","dev_magic_link":"http://127.0.0.1:%d/auth/verify?t=x"%PORT}))
        if self.path=="/auth/refresh":
            return self._s(200,json.dumps({"access_token":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzQHguY29tIn0.refreshed","token_type":"Bearer","expires_in":2592000,"email":"s@x.com"}))
        if self.path=="/auth/poll":
            b=self._body(); em=POLL_EMAIL.get(b.get("poll_token") or "pt1","")
            if em: SEEN.add(em)     # first_seen: sign-in complete -> now a returning account
            r={"status":"approved","access_token":"TESTTOKEN123"}
            # A partner-deal grant claimed on THIS sign-in. The gateway sends only a
            # finished sentence; the wrapper decides whether there is one and prints it.
            # Read per REQUEST from a file so each case can vary it.
            try:
                _n=open(os.environ["STUB_NOTICE_FILE"],"rb").read().decode("utf-8","replace").strip("\n")
                if _n: r["plan_notice"]=_n
            except Exception: pass
            return self._s(200,json.dumps(r))
        return self._s(404,"{}")
ThreadingHTTPServer(("127.0.0.1",PORT),H).serve_forever()
PY
STUB_NOTICE_FILE="$TMP/plan_notice"; : > "$STUB_NOTICE_FILE"
export STUB_NOTICE_FILE
# Every request the stub gateway receives: "<METHOD> <path> anon=<X-Anon-Id or ->".
STUB_REQ_LOG="$TMP/requests.log"; : > "$STUB_REQ_LOG"
export STUB_REQ_LOG
python3 "$TMP/stub.py" & SRV_PID=$!
sleep 1
printf '%s' "$GW" > "$HOME_BX/gateway"

# run bare byteask on a pty. Feeds: Enter (the first-run choice: its default row is the
# ByteAsk account), email (onboarding prompt), a blank (referral prompt), and 'q' (left
# over for anything that still asks).
tty_run() { # $1=extra gateway override, $2..=byteask args
  _gw="$1"; shift
  printf '\ndev@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$_gw' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask' $*" /dev/null 2>&1 || true
}
# like tty_run but with a caller-chosen email (feeds: Enter, email, blank, 'q').
tty_run_email() { # $1=gateway $2=email
  printf '\n%s\n\nq\n' "$2" | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$1' BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 '$BIN/byteask'" /dev/null 2>&1 || true
}
# an EXPIRED session skips the first-run choice and goes straight to the email.
tty_run_expired() { # $1=gateway $2=email
  printf '%s\n\nq\n' "$2" | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$1' BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 '$BIN/byteask'" /dev/null 2>&1 || true
}
# run non-interactively (stdin /dev/null -> not a TTY)
nt_run() { BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$BIN/byteask" "$@" </dev/null 2>&1; }

HAVE_PTY=0; command -v script >/dev/null 2>&1 && HAVE_PTY=1

echo "== A. unsigned + non-TTY -> nudge, no launch, stays unsigned =="
seed_unsigned; OUT=$(nt_run)
has   "not signed in"   "$OUT" "A1 shows the nudge"
hasnt "ENGINE_LAUNCHED" "$OUT" "A2 does NOT launch the engine"
signed && bad "A3 unexpectedly signed in" || ok "A3 stays unsigned"

echo "== B. unsigned + TTY -> onboards, persists token, launches =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_unsigned; OUT=$(tty_run "$GW")
  has "Welcome to ByteAsk" "$OUT" "B1 shows onboarding welcome"
  has "ENGINE_LAUNCHED"    "$OUT" "B2 launches engine after sign-in"
  grep -q 'experimental_bearer_token = "TESTTOKEN123"' "$HOME_BX/config.toml" && ok "B3 token persisted" || bad "B3 token NOT persisted"
else printf '  SKIP B (no pty)\n'; fi

echo "== C. unsigned + TTY + a prompt arg -> onboards THEN forwards the prompt =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_unsigned; OUT=$(tty_run "$GW" "hello")
  has "ENGINE_LAUNCHED hello" "$OUT" "C1 original prompt survives onboarding (POSIX \$@ preserved)"
  hasnt "unknown option"      "$OUT" "C2 do_login did NOT choke on the inherited prompt"
else printf '  SKIP C (no pty)\n'; fi

echo "== D. unsigned + TTY + sign-in FAILS (dead gateway) -> no launch =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_unsigned; OUT=$(tty_run "http://127.0.0.1:1")
  hasnt "ENGINE_LAUNCHED" "$OUT" "D1 does NOT launch when sign-in fails"
  signed && bad "D2 wrote a token despite failure" || ok "D2 no token on failed sign-in"
else printf '  SKIP D (no pty)\n'; fi

echo "== E. non-auth subcommands work while unsigned =="
seed_unsigned
OUT=$(nt_run --version); has "byteask $(sed -n 's/^VERSION="\(.*\)"/\1/p' "$WRAP" | head -1)" "$OUT" "E1 --version works unsigned"; hasnt "not signed in" "$OUT" "E2 --version doesn't nudge"

echo "== F. non-byteask provider -> no onboarding, launches directly =="
seed_nonbyteask; OUT=$(nt_run)
has   "ENGINE_LAUNCHED" "$OUT" "F1 launches (mode A/B not forced to onboard)"
hasnt "not signed in"   "$OUT" "F2 no byteask nudge for a non-byteask provider"

echo "== G. already signed in -> launches directly, no onboarding =="
seed_signed; OUT=$(nt_run)
has   "ENGINE_LAUNCHED"    "$OUT" "G1 launches"
hasnt "Welcome to ByteAsk" "$OUT" "G2 no onboarding when already signed in"
# Non-TTY + a leading prompt arg would hit the terminal-only TUI -> point at `exec` first.
seed_signed; OUT=$(nt_run "explain this repo")
has   "byteask exec" "$OUT" "G3 non-TTY prompt hints the headless exec mode"
# `byteask exec ...` is already headless -> no hint, passes straight through.
seed_signed; OUT=$(nt_run exec "do a thing")
hasnt "For non-interactive" "$OUT" "G4 exec subcommand is not hinted (passes through)"

echo "== H. --help lists the headless one-shot (exec) mode =="
seed_signed; OUT=$(nt_run --help)
has "byteask exec" "$OUT" "H1 --help documents byteask exec"

echo "== T. ripgrep dependency is surfaced when missing (and silent when present) =="
# The model shells out to `rg`; without it the engine falls back to find/grep and
# just feels slow, with nothing on screen explaining why. install.sh installs it
# best-effort, so the wrapper warns when that failed.
#
# BOTH branches are forced, because neither can be inferred from the host: a dev
# box may or may not have ripgrep, and `command -v rg` reads PATH, which cannot be
# subtracted from. So:
#   * missing -> PATH points at a mirror of every OTHER executable (rg excluded).
#   * present -> a stub `rg` is PREPENDED to that same mirror.
# Prepending works no matter what the host has, so this is deterministic anywhere.
RGSHIM="$TMP/rgshim"; RGSTUB="$TMP/rgstub"
rm -rf "$RGSHIM" "$RGSTUB"; mkdir -p "$RGSHIM" "$RGSTUB"
( IFS=:
  for d in $PATH; do
    [ -d "$d" ] || continue
    for f in "$d"/*; do
      b=${f##*/}
      [ "$b" = rg ] && continue
      [ -e "$RGSHIM/$b" ] || ln -s "$f" "$RGSHIM/$b" 2>/dev/null
    done
  done )
printf '#!/bin/sh\nexit 0\n' > "$RGSTUB/rg"; chmod +x "$RGSTUB/rg"
# NB: probe inside a subshell. `VAR=x command ...` on a BUILTIN leaks the assignment
# into the rest of the script (no fork happens), which silently left PATH pointing at
# the shim for every later test.
if [ -x "$RGSHIM/sh" ] && ! ( PATH="$RGSHIM"; command -v rg >/dev/null 2>&1 ); then
  rg_run() { # $1 = PATH to launch under
    seed_signed
    PATH="$1" BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" \
      BYTEASK_NO_UPDATE_CHECK=1 "$BIN/byteask" </dev/null 2>&1
  }
  OUT=$(rg_run "$RGSHIM")
  has "ripgrep (rg) not found" "$OUT" "T1 warns when rg is missing"
  has "ENGINE_LAUNCHED"        "$OUT" "T2 still launches (warning is not fatal)"
  # Must name a command the user can actually run, not just complain.
  case "$OUT" in
    *"install ripgrep"*|*"add ripgrep"*|*"ripgrep#installation"*)
      ok "T3 warning names an actionable install command" ;;
    *) bad "T3 warning has no install hint" ;;
  esac
  # rg present (the normal case) -> no noise on every launch.
  OUT=$(rg_run "$RGSTUB:$RGSHIM")
  hasnt "ripgrep (rg) not found" "$OUT" "T4 silent when rg is installed"
  has   "ENGINE_LAUNCHED"        "$OUT" "T5 launches normally when rg is present"
else
  printf '  SKIP T (could not build a PATH without rg)\n'
fi

echo "== Q. fresh signed-out launch -> the sign-in prompt; no anonymous session, no anonymous request =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_unsigned; rm -f "$HOME_BX/anon-id" "$HOME_BX/byok-config.json"   # clean managed-unsigned state
  : > "$STUB_REQ_LOG"
  # No email is given (the input ends at the prompt). BYTEASK_ANON_FORCE=1 used to
  # start an anonymous session here; it must now do nothing.
  OUT=$(: | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_ANON_FORCE=1 BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask'" /dev/null 2>&1 || true)
  has   "Welcome to ByteAsk" "$OUT" "Q1 a signed-out launch reaches the sign-in prompt"
  hasnt "ENGINE_LAUNCHED"    "$OUT" "Q2 no engine session without a sign-in"
  signed && bad "Q3 unexpectedly signed" || ok "Q3 stays signed out"
  # Then sign in for real and look at everything the gateway saw across both runs.
  OUT=$(tty_run "$GW")
  has   "ENGINE_LAUNCHED"    "$OUT" "Q4 after sign-in the engine launches"
  grep -q '^POST /auth/start ' "$STUB_REQ_LOG" && ok "Q5 the gateway saw the sign-in" || bad "Q5 no sign-in request recorded"
  grep -q 'anon-status' "$STUB_REQ_LOG" && bad "Q6 the anonymous-trial status was requested" || ok "Q6 no anon-status request"
  grep -v ' anon=-$' "$STUB_REQ_LOG" | grep -q . && bad "Q7 a request carried X-Anon-Id" || ok "Q7 no request carries X-Anon-Id"
  grep -qi 'X-Anon-Id' "$HOME_BX/config.toml" && bad "Q8 config.toml carries X-Anon-Id" || ok "Q8 no X-Anon-Id header in config.toml"
  [ -e "$HOME_BX/anon-id" ] && bad "Q9 a device id file was written" || ok "Q9 no device id file"
else printf '  SKIP Q (no pty)\n'; fi

echo "== H. logout removes the mode-C token (config.toml), keeps the rest =="
seed_signed "TOK_H"; printf 'x' > "$HOME_BX/auth.json"
OUT=$(BYTEASK_HOME="$HOME_BX" "$BIN/byteask" logout 2>&1)
has "Logged out" "$OUT" "H1 confirms sign-out"
signed && bad "H2 token STILL present (the bug)" || ok "H2 bearer token removed"
grep -q '^model_provider = "byteask"' "$HOME_BX/config.toml" && ok "H3 provider config preserved" || bad "H3 config clobbered"
grep -q "$GW" "$HOME_BX/config.toml" && ok "H4 base_url preserved" || bad "H4 base_url lost"
[ -f "$HOME_BX/auth.json" ] && bad "H5 auth.json NOT removed" || ok "H5 auth.json cleared too"

echo "== I. logout when already signed out / no config =="
OUT=$(BYTEASK_HOME="$HOME_BX" "$BIN/byteask" logout 2>&1)
has "not signed in" "$OUT" "I1 second logout: not signed in"
seed_none; OUT=$(BYTEASK_HOME="$HOME_BX" "$BIN/byteask" logout 2>&1); rc=$?
has "not signed in" "$OUT" "I2 logout with no config.toml: not signed in"
[ "$rc" -eq 0 ] && ok "I3 no crash (exit 0)" || bad "I3 crashed (exit $rc)"

echo "== J. round-trip: onboard -> logout -> unsigned again =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_unsigned; tty_run "$GW" >/dev/null; signed && ok "J1 signed in" || bad "J1 sign-in failed"
  BYTEASK_HOME="$HOME_BX" "$BIN/byteask" logout >/dev/null 2>&1
  signed && bad "J2 still signed in after logout" || ok "J2 logged out"
  OUT=$(nt_run); has "not signed in" "$OUT" "J3 back to unsigned"
else printf '  SKIP J (no pty)\n'; fi

echo "== K. TUI /logout marker -> wrapper clears token, back to shell =="
seed_signed "TOK_K"; printf 'logout\n' > "$HOME_BX/.stub-emit"
OUT=$(nt_run)
has "Logged out" "$OUT" "K1 /logout marker -> wrapper logs out"
signed && bad "K2 token STILL present after /logout" || ok "K2 token cleared by /logout marker"
[ -f "$HOME_BX/.byteask-auth-request" ] && bad "K3 marker not consumed" || ok "K3 marker consumed"

echo "== L. TUI /login marker -> account menu; 'different email' re-auths + relaunches =="
if [ "$HAVE_PTY" = 1 ]; then
  seed_signed "OLDTOK"; printf 'login\n' > "$HOME_BX/.stub-emit"
  # /login opens the account menu (signed in). Feed: '2' = a single keystroke selecting
  # 'different email' (the raw-mode menu reads one byte, so NO newline after it), then the
  # email line, a blank referral, and 'q' to dismiss the source menu. Writes new token + relaunch.
  OUT=$(printf '2dev@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask'" /dev/null 2>&1 || true)
  grep -q 'experimental_bearer_token = "TESTTOKEN123"' "$HOME_BX/config.toml" && ok "L1 /login (different email) wrote the new token" || bad "L1 new token not written"
  has "ENGINE_LAUNCHED" "$OUT" "L2 relaunched the TUI after /login"
  [ -f "$HOME_BX/.byteask-auth-request" ] && bad "L3 marker not consumed (loop risk)" || ok "L3 marker consumed"
else printf '  SKIP L (no pty)\n'; fi

echo "== M. no marker -> single run, engine exit code propagates =="
seed_signed "TOK_M"; rm -f "$HOME_BX/.stub-emit" "$HOME_BX/.byteask-auth-request"
OUT=$(nt_run); has "ENGINE_LAUNCHED" "$OUT" "M1 normal launch (no relaunch, no marker)"

echo "== N. referral prompt: shown on FIRST signup, SKIPPED for a returning email =="
if [ "$HAVE_PTY" = 1 ]; then
  NEW="newuser@x.com"                            # a fresh email neither the server nor cache has seen
  rm -f "$HOME_BX/.known-emails"
  seed_unsigned; OUT=$(tty_run_email "$GW" "$NEW")
  has "Have a referral code" "$OUT" "N1 first signup IS offered a referral prompt"
  grep -qixF "$NEW" "$HOME_BX/.known-emails" 2>/dev/null && ok "N2 email recorded in local cache on sign-in" || bad "N2 email NOT recorded locally"
  # Cross-machine proof: wipe the LOCAL cache so ONLY the gateway's account-exists
  # answer can suppress the prompt (the server marked $NEW seen at the poll above).
  rm -f "$HOME_BX/.known-emails"
  seed_unsigned; OUT=$(tty_run_email "$GW" "$NEW")
  hasnt "Have a referral code" "$OUT" "N3 returning email (server-detected, empty local cache) is NOT re-asked"
  # Fail-open: with a DEAD gateway the server can't answer; an unknown email with an
  # empty local cache must still be treated as new (prompt shown), never crash.
  rm -f "$HOME_BX/.known-emails"
  seed_unsigned; OUT=$(tty_run_email "http://127.0.0.1:1" "$NEW")
  has "Have a referral code" "$OUT" "N4 dead gateway -> fail-open to local (unknown => prompt shown)"
else printf '  SKIP N (no pty)\n'; fi

echo "== P. unsupported email: message shown, session re-prompts (no close), validated BEFORE referral =="
if [ "$HAVE_PTY" = 1 ]; then
  rm -f "$HOME_BX/.known-emails"; seed_unsigned
  # feed: a blocked email (must be rejected + re-asked), then a good NEW email,
  # then blank (referral skip for the good email), then 'q' (post-login source menu).
  OUT=$(printf '\nx@blocked.example\ngood@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask'" /dev/null 2>&1 || true)
  has   "not supported"                         "$OUT" "P1 unsupported email shows the gateway message"
  hasnt "Signing in to ByteAsk as x@blocked.example" "$OUT" "P2 blocked email never reached the sign-in attempt (validated up front)"
  has   "Signing in to ByteAsk as good@x.com"   "$OUT" "P3 re-prompted + proceeded with a different email (session did NOT close)"
  grep -q 'experimental_bearer_token = "TESTTOKEN123"' "$HOME_BX/config.toml" && ok "P4 sign-in completed after the re-prompt" || bad "P4 sign-in did not complete"
  # Non-interactive (piped, not a TTY): a blocked --email must fail cleanly, not loop.
  seed_unsigned
  OUT=$(BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$BIN/byteask" login --email x@blocked.example </dev/null 2>&1 || true)
  has "not supported" "$OUT" "P5 non-interactive blocked --email shows the message and exits (no loop)"
  # Crash regression: a non-tty do_login run must never hit "unbound
  # variable" (a bug in the first draft of the
  # do_login styling fix — gating the _scr_tokens CALL on [ -t 1 ] left every
  # $_C_*/$_G_* var completely unset under set -eu). Also: no raw ESC byte leaks
  # into non-tty stdout+stderr (NO_COLOR-equivalent behavior for a non-tty run).
  hasnt "unbound variable" "$OUT" "P6 non-tty do_login never crashes on unset color vars"
  hasnt "$(printf '\033')" "$OUT" "P7 non-tty do_login emits no raw ESC byte (stdout+stderr)"
else printf '  SKIP P (no pty)\n'; fi

echo "== S. managed config that lost its bearer token (401 'Sign in to continue') =="
# A managed config with no experimental_bearer_token authenticates NOTHING: the engine
# sends no Authorization and the gateway 401s every turn. _is_signed_in reads the JWT
# out of byok-config.json first, so the wrapper AND the settings screen both said
# "signed in" and the launch loop happily launched the broken config.
mk_jwt() {   # $1 = seconds from now for the exp claim
  python3 - "$1" <<'PY'
import base64, json, sys, time
def b(o): return base64.urlsafe_b64encode(json.dumps(o).encode()).rstrip(b"=").decode()
print("%s.%s.sig" % (b({"alg":"HS256"}), b({"sub":"s@x.com","exp":int(time.time())+int(sys.argv[1])})))
PY
}
seed_byok_jwt() { printf '{"jwt":"%s","keys":{"gemini":"g"}}\n' "$1" > "$HOME_BX/byok-config.json"; }
# BYTEASK_NO_REFRESH: these sections assert on what happens to a session that is
# NOT renewed (heal-from-BYOK, the expired-session message). Renewal is exercised
# on its own in section X; leaving it on here would quietly heal every fixture and
# the assertions would pass for the wrong reason.
run_launch()    { BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 "$BIN/byteask" </dev/null 2>&1 || true; }
run_launch_rf() { BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" BYTEASK_NO_UPDATE_CHECK=1 "$BIN/byteask" </dev/null 2>&1 || true; }

LIVE_JWT=$(mk_jwt 86400)
seed_unsigned; seed_byok_jwt "$LIVE_JWT"
OUT=$(run_launch)
has "ENGINE_LAUNCHED" "$OUT" "S1 launches instead of dead-ending on a broken managed config"
if grep -q "experimental_bearer_token = \"$LIVE_JWT\"" "$HOME_BX/config.toml" 2>/dev/null; then
  ok "S2 token self-healed into config.toml from the BYOK store"
else
  bad "S2 token NOT restored"; printf '       got: %s\n' "$(cat "$HOME_BX/config.toml")"
fi

# Expired JWT: nothing valid to heal with, so it must NOT silently launch a config that
# will 401 -- non-interactive gets the sign-in nudge and a non-zero exit.
seed_unsigned; seed_byok_jwt "$(mk_jwt -600)"
OUT=$(run_launch)
hasnt "ENGINE_LAUNCHED"     "$OUT" "S3 expired JWT does not launch a config that would 401"
has   "not signed in"       "$OUT" "S4 expired JWT falls through to sign-in"

# No-op guarantees: a healthy signed config and a byok-local config (which legitimately
# carries no bearer token) must both be left exactly as they are.
seed_signed GOODTOKEN; seed_byok_jwt "$LIVE_JWT"
BEFORE=$(cat "$HOME_BX/config.toml"); OUT=$(run_launch)
has "ENGINE_LAUNCHED" "$OUT" "S5 healthy signed config still launches"
[ "$BEFORE" = "$(cat "$HOME_BX/config.toml")" ] && ok "S6 healthy config left untouched" || bad "S6 healthy config was rewritten"

cat > "$HOME_BX/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byok-local"
[model_providers.byok-local]
base_url = "http://127.0.0.1:8799/byteask/v1"
EOF
BEFORE=$(cat "$HOME_BX/config.toml"); BYOK_SKIP_SIDECAR=1 run_launch >/dev/null
[ "$BEFORE" = "$(cat "$HOME_BX/config.toml")" ] && ok "S7 byok-local config untouched (its token lives in the sidecar)" || bad "S7 byok-local config was rewritten"
rm -f "$HOME_BX/byok-config.json"

echo "== U. an EXPIRED session says so instead of silently asking for a magic link =="
# The symptom: "I am already signed in but got a magic link poll". Only
# _is_signed_in checks `exp`; _current_email reads `sub` and ignores it, so an expired
# user still sees their address everywhere while /login silently restarts sign-in.
mk_jwt() {  # $1=sub $2=exp-epoch -> a decodable JWT (the wrapper only reads the payload)
  python3 - "$1" "$2" <<'PY'
import base64, json, sys
b = lambda o: base64.urlsafe_b64encode(json.dumps(o).encode()).decode().rstrip("=")
print("%s.%s.sig" % (b({"alg": "HS256", "typ": "JWT"}), b({"sub": sys.argv[1], "exp": int(sys.argv[2])})))
PY
}
if [ "$HAVE_PTY" = 1 ] && command -v python3 >/dev/null 2>&1; then
  # No .stub-emit marker here: an expired token already routes the LAUNCH path through
  # do_account. (Adding the marker fires a SECOND settings screen after sign-in, which
  # outruns the input feed and hangs the suite — that is why this reads as a plain run.)
  rm -f "$HOME_BX/.stub-emit"
  seed_signed "$(mk_jwt stale@x.com 1000000000)"   # exp = 2001 -> long expired
  OUT=$(tty_run_expired "$GW" stale@x.com)
  has "session for stale@x.com has expired" "$OUT" "U1 expired session is named and explained"
  # A genuine first run must NOT claim an expiry (no stored JWT at all).
  seed_unsigned
  OUT=$(tty_run "$GW")
  hasnt "has expired" "$OUT" "U2 first sign-in is not mislabelled as expired"
else printf '  SKIP U (no pty or no python3)\n'; fi

echo "== V. a SELF-SERVED config (own server / own key) launches with NO ByteAsk account =="
# A config whose traffic never touches us must not force a
# sign-in. self/* and cloud-with-own-key go DIRECT; auto and un-keyed cloud need managed.
seed_selfonly() {   # byok-local + a self/* model, no JWT anywhere
  rm -f "$HOME_BX/byok-config.json" 2>/dev/null || true
  cat > "$HOME_BX/config.toml" <<EOF
model = "self/mybox"
model_provider = "byok-local"
[model_providers.byok-local]
base_url = "http://127.0.0.1:8799/byteask/v1"
EOF
}
# self/* -> pure-shell decision, no python/translators needed.
seed_selfonly
OUT=$(BYOK_SKIP_SIDECAR=1 nt_run)
has   "ENGINE_LAUNCHED" "$OUT" "V1 self/* config launches unsigned (no account forced)"
hasnt "not signed in"   "$OUT" "V2 no sign-in nudge for a self-served config"
signed && bad "V3 unexpectedly wrote a token" || ok "V3 stays unsigned (no account created)"

# auto on byok-local still needs managed -> must NOT launch unsigned.
sed 's|self/mybox|auto|' "$HOME_BX/config.toml" > "$HOME_BX/config.toml.n" && mv "$HOME_BX/config.toml.n" "$HOME_BX/config.toml"
OUT=$(BYOK_SKIP_SIDECAR=1 nt_run)
hasnt "ENGINE_LAUNCHED" "$OUT" "V4 auto (managed-only) still gates: no unsigned launch"

# cloud model + the user's OWN key for its provider -> direct -> launches unsigned.
if command -v python3 >/dev/null 2>&1; then
  # effort.py too: the translators import it at module level, so without it
  # _needs_byteask_signin's python probe raises and fails closed to "needs an
  # account" -- which looks exactly like a V5 regression.
  cp "$HERE/../helpers/anthropic_translate.py" "$HERE/../helpers/gemini_translate.py" \
     "$HERE/../helpers/effort.py" "$HOME_BX/" 2>/dev/null || true
  printf 'model = "gpt-5.4"\nmodel_provider = "byok-local"\n[model_providers.byok-local]\nbase_url = "http://127.0.0.1:8799/byteask/v1"\n' > "$HOME_BX/config.toml"
  printf '{"keys":{"openai":"sk-mine"}}' > "$HOME_BX/byok-config.json"
  OUT=$(BYOK_SKIP_SIDECAR=1 nt_run)
  has   "ENGINE_LAUNCHED" "$OUT" "V5 cloud model with own key launches unsigned"
  # same model, NO key -> managed-forward -> must gate.
  printf '{"keys":{}}' > "$HOME_BX/byok-config.json"
  OUT=$(BYOK_SKIP_SIDECAR=1 nt_run)
  hasnt "ENGINE_LAUNCHED" "$OUT" "V6 same cloud model without a key still needs an account"
  rm -f "$HOME_BX/byok-config.json" "$HOME_BX/anthropic_translate.py" \
        "$HOME_BX/gemini_translate.py" "$HOME_BX/effort.py"
else printf '  SKIP V5/V6 (no python3)\n'; fi

# W. The engine's in-TUI settings surface finishes some actions in the wrapper by
# writing a TWO-line marker (`login\n<detail>`). Each hand-off must run ONLY that
# action — never the wrapper's own account/keys menu, which is the exact screen the
# in-TUI surface exists to replace.
#
# The regression this pins: a wrapper that matches `login*` and runs do_account
# unconditionally drops the user from every row of the new native menu back into
# the old one. The cases below seed a 2-line marker to catch that. These
# assertions are PTY-only on purpose: a non-TTY do_account returns early for a
# signed-in user, so it cannot tell the broken wrapper from the fixed one.
echo "== W. TUI settings hand-off: a 2-line marker must NOT open the wrapper's own menu =="
# Either string proves the legacy account UI painted: the persistent screen's row
# label, or the numbered fallback's first option (the harness gets the fallback,
# since `script ... /dev/null` gives no usable terminal size).
LEGACY1="Manage keys & models"; LEGACY2="Change your key or plan"
w_run() { # $1=stdin feed
  printf '%b' "$1" | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask'" /dev/null 2>&1 || true
}
no_legacy() { # $1=output $2=label
  case "$1" in *"$LEGACY1"*|*"$LEGACY2"*) bad "$2 (legacy wrapper menu painted)";; *) ok "$2";; esac
}
if [ "$HAVE_PTY" = 1 ]; then
  # W1 `signin` -> the linear magic-link wizard, nothing else.
  seed_signed "TOK_W1"; printf 'login\nsignin\n' > "$HOME_BX/.stub-emit"
  OUT=$(w_run 'dev@x.com\n\nq\n')
  no_legacy "$OUT" "W1 signin marker does not open the legacy menu"
  has "Email:"          "$OUT" "W1b signin marker goes straight to the sign-in wizard"
  has "ENGINE_LAUNCHED" "$OUT" "W1c relaunches the TUI after signing in"

  # W2 `relaunch` -> restart only: no prompt, no menu, token untouched.
  seed_signed "TOK_W2"; printf 'login\nrelaunch\n' > "$HOME_BX/.stub-emit"
  OUT=$(w_run 'q\n')
  no_legacy "$OUT" "W2 relaunch marker does not open the legacy menu"
  has "ENGINE_LAUNCHED" "$OUT" "W2b relaunch marker restarts the engine"
  hasnt "Email:"        "$OUT" "W2c relaunch marker does not re-ask for an email"
  grep -q 'experimental_bearer_token = "TOK_W2"' "$HOME_BX/config.toml" \
    && ok "W2d relaunch left the session token alone" || bad "W2d token changed on a plain relaunch"
  [ -f "$HOME_BX/.byteask-auth-request" ] && bad "W2e marker not consumed (loop risk)" || ok "W2e marker consumed"

  # W3 `add-model <provider>` -> the registration prompts, not a menu.
  seed_signed "TOK_W3"; printf 'login\nadd-model custom\n' > "$HOME_BX/.stub-emit"
  OUT=$(w_run '\nq\n')
  no_legacy "$OUT" "W3 add-model marker does not open the legacy menu"
  has "Name it" "$OUT" "W3b add-model marker goes straight to the model prompts"

  # W4 the documented degrade: an engine OLDER than this wrapper writes a bare
  # `login`, and must still get a working menu rather than a silent quit.
  seed_signed "TOK_W4"; printf 'login\n' > "$HOME_BX/.stub-emit"
  OUT=$(w_run 'q\n')
  case "$OUT" in
    *"$LEGACY1"*|*"$LEGACY2"*) ok "W4 bare login marker still falls back to the legacy menu";;
    *) bad "W4 bare login marker lost its fallback (an old engine would strand the user)";;
  esac

  # W5 `signin <email>`: the address was typed into the in-TUI form, so the shell must
  # NOT draw an email field of its own — it goes straight to the magic-link wait.
  seed_signed "TOK_W5"; printf 'login\nsignin typed@x.com\n' > "$HOME_BX/.stub-emit"
  OUT=$(w_run '\nq\n')
  no_legacy "$OUT" "W5 signin-with-email does not open the legacy menu"
  hasnt "Email:"      "$OUT" "W5b signin-with-email does not re-ask for the email"
  has   "typed@x.com" "$OUT" "W5c signs in as the address collected in the TUI"

  # W6 `register-model`: every field came from the in-TUI form, so no shell prompt may
  # appear — and the staged key must be consumed, never left on disk.
  seed_signed "TOK_W6"
  printf 'login\nregister-model custom mybox http://127.0.0.1:9/v1\n' > "$HOME_BX/.stub-emit"
  printf 'STAGED-KEY' > "$HOME_BX/.byteask-auth-secret"; chmod 600 "$HOME_BX/.byteask-auth-secret"
  OUT=$(w_run 'q\n')
  no_legacy "$OUT" "W6 register-model does not open the legacy menu"
  hasnt "Name it"    "$OUT" "W6b register-model does not re-prompt for a name"
  hasnt "Server URL" "$OUT" "W6c register-model does not re-prompt for a URL"
  hasnt "STAGED-KEY" "$OUT" "W6d the staged key is never echoed to the terminal"
  [ -f "$HOME_BX/.byteask-auth-secret" ] \
    && bad "W6e staged API key left on disk after the hand-off" \
    || ok "W6e staged API key consumed and removed"

  # W7 a key staged but never handed off (engine crash) must not survive a relaunch.
  seed_signed "TOK_W7"; rm -f "$HOME_BX/.stub-emit"
  printf 'ORPHAN-KEY' > "$HOME_BX/.byteask-auth-secret"; chmod 600 "$HOME_BX/.byteask-auth-secret"
  OUT=$(w_run 'q\n')
  [ -f "$HOME_BX/.byteask-auth-secret" ] \
    && bad "W7 orphaned API key survived into the next session" \
    || ok "W7 orphaned API key cleared at launch"
else printf '  SKIP W (no pty)\n'; fi

echo "== X. a sign-in that cannot be stored must never report success =="
# The failure this pins: a 30-day token expires; every /login completes against
# the gateway (start -> verify -> approved poll -> catalog) and the wrapper says
# "Signed in as ..." -- but the token never reaches config.toml, so the engine
# relaunches on the dead one every time. Two separate defects produce it:
# config.toml truncated in place (fails outright when the file is root-owned,
# e.g. after one `sudo byteask`), and a write whose exit status is never checked
# -- and `( do_login ) || true` on the marker path suppresses `set -e` for the
# whole subshell, so nothing aborts.
_is_root() { [ "$(id -u)" = 0 ]; }   # chmod means nothing to root; skip those cases
x_login() {   # drive a TUI /login hand-off; echoes the combined output
  printf 'login\nsignin\n' > "$HOME_BX/.stub-emit"
  printf 'x@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask'" /dev/null 2>&1 || true
}
if [ "$HAVE_PTY" = 1 ]; then
  # X1: an unwritable config.toml inside a writable ~/.byteask is the real-world
  # trigger. The rename-based write must SUCCEED here (rename needs the directory,
  # not the file) -- that is what makes this class of lockout impossible, not just loud.
  seed_signed "STALE_EXPIRED"; chmod 444 "$HOME_BX/config.toml"
  OUT=$(x_login); chmod 644 "$HOME_BX/config.toml" 2>/dev/null || true
  grep -q 'experimental_bearer_token = "TESTTOKEN123"' "$HOME_BX/config.toml" \
    && ok "X1 sign-in persists even when config.toml itself is unwritable" \
    || { bad "X1 fresh token did NOT land"; printf '       got: %s\n' "$(cat "$HOME_BX/config.toml")"; }
  case "$OUT" in *"token=TESTTOKEN123"*) ok "X1b engine relaunched on the NEW token";;
    *) bad "X1b engine did not get the new token"; printf '       got: %s\n' "$OUT";; esac

  # X2: when the write genuinely cannot happen (unwritable directory), the wrapper
  # must say so instead of congratulating the user. This is the assertion the old
  # code failed: it printed "Signed in as ..." and relaunched on the stale token.
  # Freeze the whole directory so even the atomic rename has nowhere to land, and
  # drive `byteask login` directly (the marker itself needs a writable dir, so the
  # TUI hand-off cannot be the vehicle for this one).
  if _is_root; then printf '  SKIP X2 (running as root, chmod is inert)\n'; else
  seed_signed "STALE_EXPIRED"; rm -f "$HOME_BX/.stub-emit"; chmod 555 "$HOME_BX"
  OUT=$(printf 'x@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask' login" /dev/null 2>&1 || true)
  chmod 755 "$HOME_BX"
  hasnt "Signed in as"                "$OUT" "X2 does NOT claim success when nothing was stored"
  has   "could not save your sign-in" "$OUT" "X2b says what failed"
  has   "chown"                       "$OUT" "X2c says how to fix it"
  grep -q 'experimental_bearer_token = "STALE_EXPIRED"' "$HOME_BX/config.toml" \
    && ok "X2d leaves the old config intact (no half-written config.toml)" \
    || bad "X2d config.toml was damaged by the failed write"
  fi

  # X8: a config.toml that is a SYMLINK (dotfile managers) must stay one. A rename
  # would replace the link with a plain file and silently detach the dotfiles repo.
  rm -f "$HOME_BX/config.toml" "$HOME_BX/config.real"
  : > "$HOME_BX/config.real"; ln -s "$HOME_BX/config.real" "$HOME_BX/config.toml"
  seed_signed "STALE_EXPIRED"                       # writes THROUGH the link
  OUT=$(x_login)
  [ -L "$HOME_BX/config.toml" ] && ok "X8 symlinked config.toml is still a symlink after sign-in" \
    || bad "X8 the symlink was replaced by a plain file"
  grep -q 'experimental_bearer_token = "TESTTOKEN123"' "$HOME_BX/config.real" \
    && ok "X8b the link TARGET received the new token" || bad "X8b link target not updated"
  rm -f "$HOME_BX/config.toml" "$HOME_BX/config.real"
else printf '  SKIP X1-X2, X8 (no pty)\n'; fi

# X3: the managed provider authenticates with config.toml's token and NOTHING else.
# _is_signed_in used to read byok-config.json first, so any live BYOK jwt made an
# expired managed token report "signed in" -- forever, with every turn 401ing.
seed_signed "$(mk_jwt s@x.com $(( $(date +%s) - 600 )))"
seed_byok_jwt "$(mk_jwt s@x.com $(( $(date +%s) + 86400 )))"
OUT=$(run_launch)
hasnt "ENGINE_LAUNCHED" "$OUT" "X3 expired managed token is not masked by a live BYOK jwt"
has   "not signed in"   "$OUT" "X3b falls through to sign-in"
rm -f "$HOME_BX/byok-config.json"

# X3c: and with renewal on, that same dead session repairs itself at launch. This
# is the property that makes the lockout unreachable rather than merely visible.
seed_signed "$(mk_jwt s@x.com $(( $(date +%s) - 600 )))"
OUT=$(run_launch_rf)
has "ENGINE_LAUNCHED" "$OUT" "X3c an expired session is renewed at launch instead of 401ing"
grep -q 'experimental_bearer_token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzQHguY29tIn0.refreshed"' "$HOME_BX/config.toml" \
  && ok "X3c2 the renewed token replaced the dead one" || bad "X3c2 dead token still in config"

# X4: and it must not go the other way either: an expired token is never silently
# deleted and the launch carried on signed out.
if [ "$HAVE_PTY" = 1 ]; then
  seed_signed "$(mk_jwt s@x.com $(( $(date +%s) - 600 )))"; rm -f "$HOME_BX/byok-config.json"
  OUT=$(printf 'dev@x.com\n\nq\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_NO_REFRESH=1 '$BIN/byteask'" /dev/null 2>&1 || true)
  # `auto` is the default model for EVERYONE, so the model line no
  # longer tells the two apart; the downgrade deleted the account's token, so check that.
  ! grep -q '^experimental_bearer_token = ' "$HOME_BX/config.toml" \
    && bad "X4 an expired account was left signed out" \
    || ok "X4 an account with a stale token is never left signed out"
  has "Email:" "$OUT" "X4b it asks them to sign in instead"
else printf '  SKIP X4 (no pty)\n'; fi

# X5/X6: renewal. A session inside its last week is traded for a fresh token at
# launch, so a user who launches at least monthly never reaches `exp` at all.
seed_signed "$(mk_jwt s@x.com $(( $(date +%s) + 3600 )))"; rm -f "$HOME_BX/byok-config.json"
run_launch_rf >/dev/null
grep -q 'experimental_bearer_token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzQHguY29tIn0.refreshed"' "$HOME_BX/config.toml" \
  && ok "X5 a nearly-expired session is renewed at launch" \
  || { bad "X5 session was NOT renewed"; printf '       got: %s\n' "$(cat "$HOME_BX/config.toml")"; }

FRESH_JWT=$(mk_jwt s@x.com $(( $(date +%s) + 2592000 )))
seed_signed "$FRESH_JWT"
run_launch_rf >/dev/null
grep -q "experimental_bearer_token = \"$FRESH_JWT\"" "$HOME_BX/config.toml" \
  && ok "X6 a healthy session is left alone (no needless refresh round trip)" \
  || bad "X6 a healthy session was rewritten"

# X7: a renewal replaces the token line and NOTHING else. The engine persists the
# user's trusted projects and reasoning effort into this same file; a renewal that
# regenerated it would silently take those away every few weeks.
seed_signed "$(mk_jwt s@x.com $(( $(date +%s) + 3600 )))"
printf '\nmodel_reasoning_effort = "high"\n\n[projects."/home/u/proj"]\ntrust_level = "trusted"\n' >> "$HOME_BX/config.toml"
run_launch_rf >/dev/null
grep -q 'experimental_bearer_token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzQHguY29tIn0.refreshed"' "$HOME_BX/config.toml" \
  && ok "X7 renewal installed the new token" || bad "X7 renewal did not install the new token"
grep -q '^trust_level = "trusted"' "$HOME_BX/config.toml" \
  && ok "X7b renewal kept the user's trusted project" || bad "X7b renewal DROPPED [projects] trust_level"
grep -q '^model_reasoning_effort = "high"' "$HOME_BX/config.toml" \
  && ok "X7c renewal kept the user's reasoning effort" || bad "X7c renewal DROPPED model_reasoning_effort"
[ "$(grep -c '^experimental_bearer_token = ' "$HOME_BX/config.toml")" = 1 ] \
  && ok "X7d exactly one token line after renewal" || bad "X7d duplicate/missing token line"

# X9: BYOK mode keeps its session in byok-config.json (the sidecar re-reads it per
# request); it gets the same renewal, in that store.
cat > "$HOME_BX/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byok-local"
[model_providers.byok-local]
base_url = "http://127.0.0.1:8799/byteask/v1"
EOF
seed_byok_jwt "$(mk_jwt s@x.com $(( $(date +%s) + 3600 )))"
BYOK_SKIP_SIDECAR=1 run_launch_rf >/dev/null
grep -q '"jwt": *"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzQHguY29tIn0.refreshed"' "$HOME_BX/byok-config.json" \
  && ok "X9 BYOK-mode session renewed in byok-config.json" || { bad "X9 BYOK jwt NOT renewed"; printf '       got: %s\n' "$(cat "$HOME_BX/byok-config.json")"; }
grep -q 'experimental_bearer_token' "$HOME_BX/config.toml" \
  && bad "X9b BYOK renewal leaked a bearer into config.toml" || ok "X9b BYOK renewal left config.toml alone"
rm -f "$HOME_BX/byok-config.json"

# X10: logout must not report success while ANY token is still on disk -- an expired
# one is still renewable inside the gateway's grace window.
if _is_root; then printf '  SKIP X10 (running as root, chmod is inert)\n'; else
seed_signed "$(mk_jwt s@x.com $(( $(date +%s) - 600 )))"; chmod 555 "$HOME_BX"
OUT=$(BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" "$BIN/byteask" logout 2>&1 || true)
chmod 755 "$HOME_BX"
hasnt "Logged out"            "$OUT" "X10 does not claim to be logged out while a token remains"
has   "Couldn't fully log out" "$OUT" "X10b says the logout did not complete"
fi

echo "== Y. a partner-deal grant claimed at sign-in is SAID OUT LOUD =="
# Without this a user who redeems a partner deal is told "sign in and your plan switches on",
# signs in, sees nothing, and has to know to type /usage. The gateway sends a finished
# sentence; the wrapper only decides whether there is one and prints it.
if [ "$HAVE_PTY" = 1 ]; then
  printf 'ByteAsk Pro is active until 9 Dec 2026.' > "$STUB_NOTICE_FILE"
  seed_unsigned; OUT=$(tty_run "$GW")
  has "Signed in as dev@x.com"              "$OUT" "Y1 still confirms the sign-in itself"
  has "ByteAsk Pro is active until 9 Dec"   "$OUT" "Y2 prints the plan the deal granted"

  # Everyone else signs in every day. An unconditional line would be noise for them.
  : > "$STUB_NOTICE_FILE"
  seed_unsigned; OUT=$(tty_run "$GW")
  has   "Signed in as dev@x.com" "$OUT" "Y3 an ordinary sign-in still confirms"
  hasnt "is active until"        "$OUT" "Y4 an ordinary sign-in adds NO extra line"

  # The value crosses the network into a terminal, which is exactly where trusting a
  # remote string gets you escape injection. The wrapper strips every control byte.
  printf 'Pro active\033[2J\033[1;31mFAKE ERROR' > "$STUB_NOTICE_FILE"
  seed_unsigned; OUT=$(tty_run "$GW")
  has   "Pro active"      "$OUT" "Y5 the printable part still prints"
  hasnt "$(printf '\033[2J')" "$OUT" "Y6 the ANSI escape is stripped, not executed"
  : > "$STUB_NOTICE_FILE"
else printf '  SKIP Y (no pty)\n'; fi

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
