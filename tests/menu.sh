#!/bin/sh
# Persistent settings screen — PTY-driven tests.
# Drives the REAL wrapper through a pseudo-terminal (`script`), feeds keystrokes
# with real timing, then REPLAYS the raw typescript through vtreplay.py to assert
# on the screen the user actually saw (the only honest way to test in-place
# repainting). Sequential/numbered-path behavior is covered by byok.sh + onboard.sh.
set -u
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$HERE/.." && pwd)"
VTREPLAY="$HERE/vtreplay.py"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok  %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf 'FAIL  %s\n' "$1"; }
has()  { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')";; esac; }
hasnt(){ case "$2" in *"$1"*) bad "$3 (unexpected '$1')";; *) ok "$3";; esac; }

# ---- platform guard: needs util-linux `script -qec` (Linux). macOS differs. ----
if ! script -qec 'true' /dev/null >/dev/null 2>&1; then
  echo "menu.sh: no util-linux 'script -qec' on this platform - skipping PTY suite"
  echo "menu.sh: 0 passed, 0 failed (SKIPPED)"
  exit 0
fi
command -v python3 >/dev/null 2>&1 || { echo "menu.sh: needs python3"; exit 1; }

echo "== T14: vtreplay interpreter self-test =="
if python3 "$VTREPLAY" --selftest >/dev/null 2>&1; then ok "vtreplay selftest"; else bad "vtreplay selftest"; fi

# ---- sandbox -----------------------------------------------------------------
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
export BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME"
export BYTEASK_NO_UPDATE_CHECK=1 BYOK_SKIP_SIDECAR=1
export TERM=xterm LANG=C.UTF-8 LC_ALL=C.UTF-8
unset NO_COLOR BYTEASK_PLAIN_MENU 2>/dev/null || true
export CODE_FILE="$TMP/curlcode"; echo 200 > "$CODE_FILE"

# stub curl: -w %{http_code} answers from CODE_FILE; everything else no-op/empty
mkdir -p "$TMP/bin"
cat > "$TMP/bin/curl" <<'EOF'
#!/bin/sh
for a in "$@"; do case "$a" in "%{http_code}") cat "$CODE_FILE" 2>/dev/null || echo 200; exit 0;; esac; done
exit 0
EOF
chmod +x "$TMP/bin/curl"
cp "$ROOT/byteask" "$TMP/bin/byteask"; chmod +x "$TMP/bin/byteask"
export PATH="$TMP/bin:$PATH"
# Keep in sync with install.sh's fetch list. effort.py is a module-level dependency of
# the translators; without it `import anthropic_translate` raises, byteask_models.py
# cannot run, and the self-hosted / provider-picker screens render empty (which also
# desynchronises the seeded arrow-key input in T30).
for f in byok_sidecar.py byteask_models.py openai_compat_translate.py \
         anthropic_translate.py gemini_translate.py effort.py; do
  cp "$ROOT/helpers/$f" "$BYTEASK_HOME/$f" 2>/dev/null || true
done

# a decodable JWT so the header shows a real email (sub claim, no exp)
JWT=$(python3 - <<'PY'
import base64, json
seg = lambda o: base64.urlsafe_b64encode(json.dumps(o).encode()).rstrip(b"=").decode()
print(seg({"alg":"HS256"}) + "." + seg({"sub":"tester@example.com"}) + ".sig")
PY
)
seed_signed() {   # managed, signed-in config (fresh each test)
  rm -f "$BYTEASK_HOME/byok-config.json" "$BYTEASK_HOME/.byteask-auth-request" 2>/dev/null || true
  cat > "$BYTEASK_HOME/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
web_search = "live"

[model_providers.byteask]
name = "ByteAsk"
base_url = "https://gw.invalid/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "$JWT"

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
EOF
  printf 'https://gw.invalid' > "$BYTEASK_HOME/gateway"
}
seed_key() {   # drop a provider key straight into the byok store
  python3 - "$BYTEASK_HOME/byok-config.json" "$1" "$2" <<'PY'
import json, sys, os
p, prov, key = sys.argv[1:4]
try: cfg = json.load(open(p))
except Exception: cfg = {}
cfg.setdefault("keys", {})[prov] = key
cfg.setdefault("local_token", "tok-test")
open(p, "w").write(json.dumps(cfg))
PY
}

# ---- PTY driver --------------------------------------------------------------
# feed <spec>... : leads with a settle so the wrapper's startup (config load +
# the first ACCOUNT frame) completes BEFORE key 1 — otherwise the fed bytes pile
# into the pty buffer faster than the raw-mode reader drains them, and the
# burst-Enter guard misreads an Enter-with-arrows-behind-it as a paste. Each key
# then prints with a 0.30s gap (past the 100ms Esc window AND the per-key fork
# overhead, incl. the python `models list` on the KEYS transition), so keys
# register individually. A real human types after the screen paints and slower
# than this — the gaps only matter for the automated feed. Specs:
#   up|down|enter|esc|ctrlc  ·  t:<text+Enter>  ·  raw:<printf-arg>  ·  w:<secs>
FEED_SETTLE=${FEED_SETTLE:-1.0}   # startup + first-frame paint before key 1
FEED_GAP=${FEED_GAP:-0.30}        # inter-key gap
feed() {
  sleep "$FEED_SETTLE"
  for _it in "$@"; do
    case "$_it" in
      up)    printf '\033[A';;
      down)  printf '\033[B';;
      enter) printf '\r';;
      esc)   printf '\033';;
      ctrlc) printf '\003';;
      t:*)   printf '%s\r' "${_it#t:}";;
      raw:*) printf '%b' "${_it#raw:}";;
      w:*)   sleep "${_it#w:}"; continue;;
    esac
    sleep "$FEED_GAP"
  done
  sleep 0.4
}
TS="$TMP/ts"; RC_F="$TMP/rc"
run_pty() {   # run_pty [rows cols] -- feedspec...   (drives `byteask login`)
  _rows=40; _cols=100
  if [ "$1" != "--" ]; then _rows=$1; _cols=$2; shift 2; fi
  shift   # the --
  : > "$TS"
  { feed "$@"; } | script -qec \
    "stty rows $_rows cols $_cols 2>/dev/null; byteask login; echo RC=\$? >'$RC_F'" \
    "$TS" >/dev/null 2>&1
  SCRIPT_RC=$?   # for a signal exit (Ctrl-C), the inner `echo RC=$?` never runs
                 # (SIGINT kills the sh -c), so script's own code is the wrapper's.
}
screen() { python3 "$VTREPLAY" "$TS"; }
flags()  { python3 "$VTREPLAY" --flags "$TS"; }
rc()     { sed -n 's/^RC=//p' "$RC_F" 2>/dev/null | head -n1; }
scrc()   { echo "${SCRIPT_RC:-}"; }
# widest rendered line in DISPLAY columns (chars, not bytes — a middot is 1 col
# but 2 bytes; awk length() would count bytes under C.UTF-8 and over-report).
# Skips util-linux `script`'s own "Script started/done on ..." banner lines,
# which it writes verbatim into the typescript (not wrapper output).
maxwidth() { python3 "$VTREPLAY" "$TS" | python3 -c 'import sys
ls=[l for l in sys.stdin.read().splitlines() if not l.startswith(("Script started on","Script done on"))]
print(max((len(l) for l in ls), default=0))'; }

# =============================================================================
echo "== T1/T2/T7: add a key — one persistent region, arrows, status line =="
seed_signed; echo 200 > "$CODE_FILE"
# ACCOUNT: enter (Manage keys & models) -> KEYS (no keys: cursor on managed, row 1)
# down -> OpenAI -> enter -> paste key -> back on KEYS w/ status -> Done
run_pty -- enter down enter t:sk-PTY-KEY w:0.5 enter
S=$(screen); F=$(flags)
has "ByteAsk · Settings" "$S" "header rendered"
[ "$(printf '%s' "$S" | grep -c 'ByteAsk · Settings')" = 1 ] \
  && ok "header appears EXACTLY once (the bug this exists to kill)" \
  || bad "header repeated in final screen: $(printf '%s' "$S" | grep -c 'ByteAsk · Settings')x"
has "Saved your OpenAI key" "$S" "status line shows the action result"
has "cursor_up_used" "$F" "in-place repaint machinery active"
has "clear_eol_used" "$F" "clear-to-EOL used"
has "mode_2004_on" "$F" "bracketed paste enabled during menu"
has "mode_2004_off" "$F" "bracketed paste disabled on exit"
has '"openai": "sk-PTY-KEY"' "$(cat "$BYTEASK_HOME/byok-config.json")" "key stored"
has 'model_provider = "byok-local"' "$(cat "$BYTEASK_HOME/config.toml")" "config flipped to byok-local"
RAW=$(cat "$TS")
has "ByteAsk settings" "$RAW" "final scrollback summary line printed"
has "tester@example.com" "$S" "signed-in email in the header"

echo "== printable shortcuts are DEAD in the raw menu =="
seed_signed
run_pty -- enter raw:3 raw:q w:0.3 esc esc
TSRAW=$(cat "$TS")
has "API keys" "$TSRAW" "KEYS screen rendered"
hasnt "Paste your" "$TSRAW" "'3' opened no prompt (digit shortcut dead)"
has 'model_provider = "byteask"' "$(cat "$BYTEASK_HOME/config.toml")" "'q'/'3' fired nothing (still managed)"
[ "$(rc)" = 0 ] && ok "clean exit via Esc (q did not cancel mid-way)" || bad "exit rc=$(rc)"

echo "== T4: bracketed paste burst swallowed with explanation =="
seed_signed
run_pty -- enter "raw:\033[200~sk-fake4q99\r\033[201~" w:0.4 esc esc
# The message is transient (KEYS-screen status), and the trailing "esc esc" navigates
# away (KEYS->ACCOUNT->exit) before the run ends — the screen correctly clears it on
# that navigation (a stale message must NOT survive onto ACCOUNT), so it's checked in
# the raw stream (where it appears at least once, mid-session) rather than the final
# replayed screen (where a status message from a DIFFERENT, earlier screen is gone by
# design).
has "Paste ignored here" "$(cat "$TS")" "paste-ignored message shown"
hasnt "byok-local" "$(cat "$BYTEASK_HOME/config.toml")" "paste fired NO action"

echo "== T20/T21: UNMARKED paste (incl. trailing newline) fires nothing =="
seed_signed
run_pty -- enter "raw:sk-unmarked-4q\r" w:0.4 esc esc
has "Paste ignored here" "$(cat "$TS")" "unmarked burst detected + ignored"
has 'model_provider = "byteask"' "$(cat "$BYTEASK_HOME/config.toml")" "no selection fired (still managed)"

echo "== T18: bare Esc = back (100ms window, no hang) =="
seed_signed
run_pty -- enter esc esc
S=$(screen)
has "Account" "$S" "Esc from KEYS returned to ACCOUNT"
[ "$(rc)" = 0 ] && ok "second Esc exited cleanly" || bad "rc=$(rc)"

echo "== T19: modified arrows / Home / F-keys drained, no stray action =="
seed_signed
run_pty -- enter "raw:\033[1;5A" "raw:\033[H" "raw:\033OP" w:0.3 enter
# cursor never moved -> Enter selects the default (managed, 0 keys) -> exits
[ "$(rc)" = 0 ] && ok "unknown CSI drained; default select still clean" || bad "rc=$(rc)"
has 'model_provider = "byteask"' "$(cat "$BYTEASK_HOME/config.toml")" "managed selected (cursor unmoved by CSI junk)"

echo "== T5: Remove submenu — only keyed providers + Cancel =="
seed_signed; seed_key gemini sk-GEM
run_pty -- enter up up enter w:0.3 down up enter w:0.3 enter
# KEYS w/ 1 key: cursor Done(7); up x2 -> remove(5); submenu: Gemini/Cancel; enter removes
S=$(screen)
has "Gemini      (your key)" "$(cat "$TS")" "submenu listed the keyed provider"
hasnt "OpenAI      (your key)" "$(cat "$TS")" "unkeyed providers NOT listed"
has "no keys left" "$S" "last-key removal says the whole truth (switched to managed)"
hasnt '"gemini"' "$(cat "$BYTEASK_HOME/byok-config.json" 2>/dev/null || echo '')" "key actually removed"

echo "== T15/T25: managed-wipe confirm — cancel keeps, y wipes, self/* model resets =="
seed_signed; seed_key openai sk-KEEP
sed -i 's|^model = .*|model = "self/mybox"|' "$BYTEASK_HOME/config.toml"
# KEYS w/ key: Done(7); down wraps -> managed(1); confirm 'n' cancels; managed again; 'y' wipes+exits
run_pty -- enter down enter w:0.3 raw:n w:0.3 down enter w:0.3 raw:y
S=$(screen)
has "This removes your 1 saved key" "$(cat "$TS")" "confirm frame shown with count"
has "Cancelled" "$(cat "$TS")" "'n' cancelled the first attempt"
has "Switched to ByteAsk managed" "$S" "'y' completed the wipe"
hasnt '"openai"' "$(cat "$BYTEASK_HOME/byok-config.json" 2>/dev/null || echo '')" "keys wiped after y"
has 'model = "auto"' "$(cat "$BYTEASK_HOME/config.toml")" "active self/* model reset to default"

echo "== T6/T16: self-hosted screen + wizard hand-off failure re-entry =="
seed_signed; seed_key openai sk-X   # a key so KEYS cursor starts on Done(7)
python3 "$BYTEASK_HOME/byteask_models.py" add "$BYTEASK_HOME/byok-config.json" demo \
  >/dev/null 2>&1 <<'EOF'
{"base_url": "http://127.0.0.1:9", "model_id": "demo-model", "wire": "chat"}
EOF
# add -> provider picker (cursor defaults to "custom") -> Enter picks custom ->
# name -> URL -> Enter (skip key). t:<text> already carries its own trailing
# Enter, so no extra bare `enter` between the name/URL steps.
run_pty -- enter up enter w:0.4 enter enter t:newbox t:http://127.0.0.1:9 enter w:1.2 esc esc esc
TSRAW=$(cat "$TS")
has "self/demo" "$TSRAW" "registered model listed on the self-hosted screen"
has "never billed" "$TSRAW" "self-hosted explainer shown"
has "Choose a provider" "$TSRAW" "navigable provider picker shown in the add flow"
has "openrouter" "$TSRAW" "a known preset listed in the picker"
has "Custom server" "$TSRAW" "custom-URL option listed in the picker"
has "API key, if needed" "$TSRAW" "optional key prompt shown in the add flow"
has "Couldn't add it" "$TSRAW" "failed wizard reported in the status line after re-entry"
F=$(flags)
has "mode_2004_on" "$F" "region resumed (bracketed paste re-enabled) after hand-off"
S=$(screen)
[ "$(printf '%s' "$S" | grep -c 'ByteAsk · Settings')" = 1 ] \
  && ok "header appears EXACTLY once after the wizard hand-off (T29)" \
  || bad "header repeated after wizard hand-off: $(printf '%s' "$S" | grep -c 'ByteAsk · Settings')x"

echo "== T23: rejected key (401) + unverifiable key (000) =="
seed_signed; echo 401 > "$CODE_FILE"
run_pty -- enter down enter t:sk-BAD w:0.5 esc esc
# Raw stream, not the final screen: the trailing "esc esc" navigates KEYS->ACCOUNT->exit,
# and the screen correctly clears a KEYS-screen status on that navigation (same rationale as T4).
has "rejected that key (HTTP 401)" "$(cat "$TS")" "401 shows the exact rejection"
hasnt '"openai"' "$(cat "$BYTEASK_HOME/byok-config.json" 2>/dev/null || echo '')" "rejected key NOT stored"
seed_signed; echo 000 > "$CODE_FILE"
run_pty -- enter down enter t:sk-MAYBE w:0.5 enter
S=$(screen)
has "saved it anyway" "$S" "unverifiable key saved with honest warning"
has '"openai": "sk-MAYBE"' "$(cat "$BYTEASK_HOME/byok-config.json")" "unverified key stored"
echo 200 > "$CODE_FILE"

echo "== T24: sidecar helper missing fails soft into the status line =="
seed_signed; mv "$BYTEASK_HOME/byok_sidecar.py" "$BYTEASK_HOME/byok_sidecar.py.away"
run_pty -- enter down enter w:0.3 esc esc
# Raw stream, not the final screen — same navigation-clears-status rationale as T4/T23.
has "run 'byteask --update'" "$(cat "$TS")" "missing-helper guidance shown; screen alive"
mv "$BYTEASK_HOME/byok_sidecar.py.away" "$BYTEASK_HOME/byok_sidecar.py"

echo "== T17: stderr redirected — screen still renders (via /dev/tty) =="
seed_signed
: > "$TS"
{ feed enter esc esc; } | script -qec \
  "stty rows 40 cols 100 2>/dev/null; byteask login 2>/dev/null; echo RC=\$? >'$RC_F'" \
  "$TS" >/dev/null 2>&1
S=$(screen)
has "ByteAsk · Settings" "$S" "region rendered with stderr redirected"

echo "== T11: rows too small -> sequential fallback =="
seed_signed
run_pty 10 100 -- t:1 t:7 raw:
S_RAW=$(cat "$TS")
has "type a number" "$S_RAW" "tiny terminal fell back to the numbered flow"
hasnt "$(printf '\033[?2004h')" "$S_RAW" "no region machinery on the fallback path"

echo "== T22: narrow terminal truncates — nothing wraps =="
seed_signed
run_pty 40 45 -- enter esc esc
MW=$(maxwidth)
[ "$MW" -le 45 ] && ok "all rendered lines fit 45 cols (widest=$MW, no wrap-drift)" || bad "widest line $MW cols > 45"
hasnt "$(printf '\357\277\275')" "$(screen)" "no  from a mid-glyph cut on the narrow terminal"

echo "== T13/T26: ACCOUNT screen + failed sign-in re-enters cleanly =="
seed_signed
# down->email row, enter->sign-in wizard; type a NEW email, Enter past the
# "referral code?" prompt (skip), the stub gateway fails auth/start -> do_login
# exits 1 -> screen re-enters ACCOUNT with the honest status; Esc closes.
run_pty -- down enter w:1.0 t:someone@example.com enter w:1.5 esc
TSRAW=$(cat "$TS")
has "Manage keys & models" "$TSRAW" "ACCOUNT screen rendered"
has "Sign-in didn't complete" "$TSRAW" "failed wizard re-entered with an honest status"
[ "$(rc)" = 0 ] && ok "clean exit after failed sign-in" || bad "rc=$(rc)"
S=$(screen)
[ "$(printf '%s' "$S" | grep -c 'ByteAsk · Settings')" = 1 ] \
  && ok "header appears EXACTLY once after the do_login hand-off (T29)" \
  || bad "header repeated after do_login hand-off: $(printf '%s' "$S" | grep -c 'ByteAsk · Settings')x"

echo "== T8: Ctrl-C mid-menu restores the terminal =="
seed_signed
run_pty -- enter ctrlc
F=$(flags)
has "mode_2004_off" "$F" "bracketed paste turned off by the INT trap"
has "mode_25_show" "$F" "cursor re-shown by the INT trap"
# The INT trap does `exit 130`, but SIGINT also kills the harness `sh -c` before
# its `echo RC=$?` runs, so read the exit through `script` itself (== 130).
[ "$(scrc)" = 130 ] && ok "exit code 130 (SIGINT)" || bad "script rc=$(scrc) (want 130)"

echo "== T9: SIGWINCH mid-menu — repaint survives, selection still lands =="
# Uses the standard feed|script driver (the feed ALWAYS supplies the exit keys,
# so this can never hang). A background helper delivers WINCH to the byteask
# child while the menu is up; the trailing esc/esc guarantee termination.
seed_signed
( _w=0; while [ "$_w" -lt 30 ]; do
    _cp=$(pgrep -f "$TMP/bin/byteask login" 2>/dev/null | head -n1 || true)
    if [ -n "$_cp" ]; then sleep 0.5; kill -WINCH "$_cp" 2>/dev/null; break; fi
    sleep 0.1; _w=$((_w + 1))
  done ) &
WPID=$!
run_pty -- enter w:0.6 esc esc
kill "$WPID" 2>/dev/null || true
S=$(screen)
[ "$(rc)" = 0 ] && ok "survived WINCH; exited cleanly" || bad "rc=$(rc) after WINCH"
has "Account" "$S" "screen intact after resize signal"

echo "== T30: provider picker — arrow-select a preset skips the URL prompt =="
seed_signed; seed_key openai sk-X   # a key so KEYS cursor starts on Done(7)
# add -> provider picker (cursor defaults to "custom"); "up" once lands on the
# last local preset (vllm) -> Enter picks it -> name -> key (no URL prompt at all).
# t:myvllm carries its own trailing Enter, so only ONE more bare enter (skip key).
run_pty -- enter up enter w:0.4 enter up enter t:myvllm enter w:1.2 esc esc esc
TSRAW=$(cat "$TS")
has "Choose a provider" "$TSRAW" "provider picker rendered"
has "vllm" "$TSRAW" "a local preset listed and selectable via arrows"
hasnt "Server URL" "$TSRAW" "picking a preset skips the URL prompt entirely"
has "API key, if needed" "$TSRAW" "key prompt still shown after picking a preset"

echo "== T31: unsigned self-served config -> settings screen, honest header (no account) =="
# A byok-local self/* config needs no ByteAsk account, so `byteask login`
# must open the keys/models screen directly (not force email onboarding) and the header
# must NOT falsely claim "signed in".
rm -f "$BYTEASK_HOME/byok-config.json" "$BYTEASK_HOME/.byteask-auth-request" 2>/dev/null || true
cat > "$BYTEASK_HOME/config.toml" <<EOF
model = "self/mybox"
model_provider = "byok-local"
[model_providers.byok-local]
base_url = "https://gw.invalid/byteask/v1"
EOF
run_pty -- w:0.4 esc esc
S=$(screen); TSRAW=$(cat "$TS")
has "API keys" "$TSRAW" "T31 keys/models screen opens (no forced sign-in)"
has "not signed in" "$S" "T31 header states not-signed-in"
hasnt "· signed in" "$S" "T31 header does NOT falsely claim signed in"

echo "== T10: EOF mid-menu finalizes as Done (defensive arm — static check) =="
# NOT PTY-driveable: the wrapper reads /dev/tty (fd 3); under `script` the pty
# master stays open until the child exits, so a tty EOF can't be delivered
# without the process already having gone (a real terminal only EOFs on fd close,
# which coincides with SIGHUP). This is correct behavior — a live-but-quiet
# terminal must NOT exit the menu. So we assert the defensive `eof -> break` arm
# is present rather than fake an EOF the harness can't produce.
WRAP="$ROOT/byteask"
has "_KEY=eof" "$(cat "$WRAP")" "read loop detects a real fd EOF"
has "eof)   break" "$(cat "$WRAP")" "EOF finalizes the screen (break -> summary)"

echo ""
echo "== managed leads the list, says recommended, and a first-run cursor starts on it =="
# "Use ByteAsk managed (recommended)" is on top and selected.
seed_signed
run_pty -- enter w:0.3 esc esc
# Read the raw stream: the final screen is ACCOUNT again after `esc esc`.
verdict=$(python3 - "$TS" <<'PYV'
import re, sys
raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
raw = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", raw)
keys = raw[raw.find("API keys"):]
m, o = keys.find("Use ByteAsk managed (recommended)"), keys.find("OpenAI ")
sel = "> Use ByteAsk managed (recommended)" in keys
print("ok" if 0 <= m < o and sel else "bad managed=%d openai=%d selected=%s" % (m, o, sel))
PYV
)
[ "$verdict" = ok ] && ok "first row is managed, marked recommended, and selected" \
                   || bad "managed row: $verdict"

echo "menu.sh: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
