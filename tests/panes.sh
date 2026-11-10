#!/bin/sh
# Pane surfaces -- end-to-end in a real terminal.
#
# What is tested is a behaviour, not a rendering: /usage output must not stay on the
# screen; pressing escape makes it disappear. Only a real terminal emulator
# can prove that, so this drives the REAL engine inside tmux against a stub gateway and reads
# the rendered screen back with `capture-pane`.
#
# The esc assertions are the non-vacuity proof: they FAIL against any engine that renders
# these commands as history cells (every engine before this change), because a line written
# into scrollback cannot be dismissed. Measured against the 0.1.11 engine on 2026-09-08:
# P2a/P2b/P3b fail, exactly as designed.
#
#   sh tests/panes.sh                        # uses ~/.local/bin/byteask-engine
#   BYTEASK_ENGINE=/path/to/engine sh tests/panes.sh
set -u
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO="$(CDPATH= cd -- "$HERE/.." && pwd)"
ENGINE="${BYTEASK_ENGINE:-$HOME/.local/bin/byteask-engine}"
SESSION="byteask-panes-$$"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok  %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf 'FAIL  %s\n' "$1"; }
has()  { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')";; esac; }
hasnt(){ case "$2" in *"$1"*) bad "$3 (still on screen: '$1')";; *) ok "$3";; esac; }

command -v tmux >/dev/null 2>&1 || { echo "panes.sh: needs tmux - skipping"; echo "panes.sh: 0 passed, 0 failed (SKIPPED)"; exit 0; }
command -v python3 >/dev/null 2>&1 || { echo "panes.sh: needs python3"; exit 1; }
[ -x "$ENGINE" ] || { echo "panes.sh: no engine at $ENGINE - skipping"; echo "panes.sh: 0 passed, 0 failed (SKIPPED)"; exit 0; }

TMP="$(mktemp -d)"
cleanup() { tmux kill-session -t "$SESSION" 2>/dev/null; kill "${GW_PID:-0}" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT
mkdir -p "$TMP/home" "$TMP/work"

# ---- stub gateway: only the account endpoints these panes read ---------------
cat > "$TMP/gw.py" <<'PY'
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _j(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path.endswith("/usage"):
            # Field names below are the gateway's real ones; a stub that invents
            # names tests nothing. The percentages are i64
            # in the engine's UsageInfo: send integers, or serde
            # rejects the whole payload and the pane shows "couldn't reach ByteAsk".
            return self._j({"plan": "free", "used_5h_pct": 1, "used_week_pct": 0,
                            "reset_5h_sec": 17940, "reset_week_sec": 601140,
                            "extra_granted_usd": 0.0, "extra_spent_usd": 0.0,
                            "extra_balance_usd": 0.0})
        if self.path.endswith("/referral-code"):
            return self._j({"code": "TESTCODE", "successful_referrals": 0,
                            "pro_reward_target": 3, "pro_reward_granted": False,
                            "share_command": "curl -fsSL https://code.byteask.ai/install.sh | BYTEASK_REF=TESTCODE sh",
                            "share_pip": "pip install byteask && BYTEASK_REF=TESTCODE byteask",
                            "share_npm": "npm install -g @byteask/cli && BYTEASK_REF=TESTCODE byteask"})
        if self.path.endswith("/alerts"):
            return self._j({"alerts": []})
        return self._j({}, 404)
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if self.path.endswith("/upgrade"):
            return self._j({"pay_url": "https://code.byteask.ai/pay/TESTLINK"})
        if self.path.endswith("/voucher"):
            return self._j({"ok": True, "amount_added_usd": 5.0, "balance_usd": 12.0})
        return self._j({}, 404)
srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
print(srv.server_address[1], flush=True)
srv.serve_forever()
PY
python3 "$TMP/gw.py" > "$TMP/port" 2>/dev/null &
GW_PID=$!
i=0; PORT=""
while [ $i -lt 60 ]; do PORT="$(head -n1 "$TMP/port" 2>/dev/null || true)"; [ -n "$PORT" ] && break; i=$((i+1)); sleep 0.1; done
[ -n "$PORT" ] || { echo "panes.sh: stub gateway did not start"; exit 1; }

cat > "$TMP/home/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
model_catalog_json = "$REPO/dist/models-catalog.json"

[tui]
animations = false

[model_providers.byteask]
name = "ByteAsk"
base_url = "http://127.0.0.1:$PORT/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "test-token"

[projects."$TMP/work"]
trust_level = "trusted"
EOF

screen() { tmux capture-pane -p -t "$SESSION" 2>/dev/null; }
# The composer's slash popup needs a beat before Enter, or the command is only completed.
type_cmd() { tmux send-keys -t "$SESSION" "$1"; sleep 2; tmux send-keys -t "$SESSION" Enter; sleep "${2:-6}"; }

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 100 -y 30 -c "$TMP/work" \
  "BYTEASK_HOME=$TMP/home CODEX_HOME=$TMP/home TERM=xterm-256color NO_COLOR= '$ENGINE' -C '$TMP/work'"
sleep 7
START="$(screen)"
has "ByteAsk" "$START" "P0 the session header rendered"

echo "== P1: /usage opens a pane =="
type_cmd '/usage'
A="$(screen)"
has "5-hour" "$A" "P1a the 5-hour row is on screen"
has "Weekly" "$A" "P1b the weekly row is on screen"

echo "== P2: esc dismisses it (fails on a history-cell engine) =="
tmux send-keys -t "$SESSION" Escape; sleep 3
B="$(screen)"
hasnt "5-hour" "$B" "P2a the usage readout is gone after esc"
hasnt "Weekly" "$B" "P2b no row of it survives"
has "ByteAsk" "$B" "P2c the transcript above is untouched"

echo "== P3: /referral-code, same contract =="
type_cmd '/referral-code'
C="$(screen)"
has "TESTCODE" "$C" "P3a the code is on screen"
tmux send-keys -t "$SESSION" Escape; sleep 3
D="$(screen)"
hasnt "TESTCODE" "$D" "P3b the code is gone after esc"

echo "== P4: nothing the panes drew was written into the transcript =="
FINAL="$(screen)"
hasnt "usage unavailable" "$FINAL" "P4a no error cell left behind"
hasnt "Share any of these" "$FINAL" "P4b no referral block left behind"

printf '%s\n' "--- final screen ---"
printf '%s\n' "$FINAL" | grep -n . | sed -n '1,20p'
printf 'panes.sh: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
