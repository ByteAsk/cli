#!/bin/sh
# /login subscription + launcher row render test — drives the REAL installed engine
# in tmux and reads the screen back with capture-pane (like panes.sh). Proves the
# rows are gated correctly on the server-fetched kill switches (the local
# $BYTEASK_HOME/subscription-flags file) and on claude-on-PATH.
#   BYTEASK_ENGINE=/path/to/engine sh tests/subscription-panes.sh
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
ENGINE="${BYTEASK_ENGINE:-$HOME/.local/bin/byteask-engine}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')";; *) ok "$3";; esac; }

command -v tmux >/dev/null 2>&1 || { echo "subscription-panes.sh: needs tmux - skipping"; echo "subscription-panes.sh: 0 passed, 0 failed (SKIPPED)"; exit 0; }
[ -x "$ENGINE" ] || { echo "subscription-panes.sh: no engine at $ENGINE - skipping"; echo "subscription-panes.sh: 0 passed, 0 failed (SKIPPED)"; exit 0; }

TMP=$(mktemp -d); SESSION="subpanes-$$"
cleanup() { tmux kill-session -t "$SESSION" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT
mkdir -p "$TMP/home" "$TMP/work"

cat > "$TMP/home/config.toml" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
model_catalog_json = "$REPO/dist/models-catalog.json"

[tui]
animations = false

[model_providers.byteask]
name = "ByteAsk"
base_url = "http://127.0.0.1:9/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "test-token"

[projects."$TMP/work"]
trust_level = "trusted"
EOF

screen() { tmux capture-pane -p -t "$SESSION" 2>/dev/null; }
open_login() {
  tmux send-keys -t "$SESSION" '/login'; sleep 2; tmux send-keys -t "$SESSION" Enter; sleep 5
}
close_pane() { tmux send-keys -t "$SESSION" Escape; sleep 2; }
launch() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  tmux new-session -d -s "$SESSION" -x 110 -y 34 -c "$TMP/work" \
    "BYTEASK_HOME=$TMP/home CODEX_HOME=$TMP/home TERM=xterm-256color NO_COLOR= '$ENGINE' -C '$TMP/work'"
  sleep 7
}

echo "== flags OFF (no file): NO subscription rows AND no launcher row (fail-safe) =="
rm -f "$TMP/home/subscription-flags"
launch
open_login
S="$(screen)"
has "ByteAsk settings" "$S" "the /login root pane opened"
hasnt "Use my ChatGPT subscription" "$S" "no ChatGPT row when the flag is off"
hasnt "Use my Claude subscription" "$S" "no Claude row when the flag is off"
# The launcher row is now gated on its own flag too — off by default, so it must
# NOT show even though claude is on this box's PATH.
hasnt "Use my Claude Code" "$S" "no launcher row when claude_launcher is off"
close_pane

echo "== flags ON incl. claude_launcher: all three rows appear =="
printf 'chatgpt=1\nclaude=1\nclaude_launcher=1\n' > "$TMP/home/subscription-flags"
launch
open_login
S2="$(screen)"
has "ByteAsk settings" "$S2" "the /login root pane opened"
has "Use my ChatGPT subscription" "$S2" "ChatGPT row appears when chatgpt=1"
has "Use my Claude subscription" "$S2" "Claude row appears when claude=1"
has "Use my Claude Code" "$S2" "launcher row appears when claude_launcher=1 (claude on PATH)"
close_pane

echo "== launcher flag is independent: claude_launcher=1 but subscriptions off =="
printf 'chatgpt=0\nclaude=0\nclaude_launcher=1\n' > "$TMP/home/subscription-flags"
launch
open_login
S3="$(screen)"
hasnt "Use my ChatGPT subscription" "$S3" "ChatGPT row hidden when chatgpt=0"
hasnt "Use my Claude subscription" "$S3" "Claude row hidden when claude=0"
has "Use my Claude Code" "$S3" "launcher row shows on its own flag, no subscription needed"
close_pane

echo "== claude subscription on but launcher off: launcher stays hidden =="
printf 'chatgpt=0\nclaude=1\nclaude_launcher=0\n' > "$TMP/home/subscription-flags"
launch
open_login
S4="$(screen)"
has "Use my Claude subscription" "$S4" "Claude subscription row shows when claude=1"
hasnt "Use my Claude Code" "$S4" "launcher hidden when its own flag is off (claude=1 does not imply it)"
close_pane

echo
echo "subscription-panes.sh: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
