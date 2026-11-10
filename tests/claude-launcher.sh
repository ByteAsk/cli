#!/bin/sh
# `byteask claude` launcher tests. Stubs `claude`
# so nothing hits Anthropic. Verifies: clean failure when claude is absent;
# interactive launch passes ByteAsk's method + the user's args through; --headless
# uses -p and warns about metered credits; and the wrapper NEVER reads the Claude
# credential file (compliance: ByteAsk must not touch/intermediate it).
#
# Run:  sh tests/claude-launcher.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')";; *) ok "$3";; esac; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
: > "$BIN/byteask-engine"; chmod +x "$BIN/byteask-engine"   # stub engine (_engine_compat)
# Pin PATH to $BIN + system dirs ONLY. This EXCLUDES the real ~/.local/bin/claude
# so a test can never launch the actual Claude Code (it would run on the user's
# real subscription); `claude` resolves solely to the stub we add below.
# `claude` is installed system-wide here (/usr/bin, /bin, ~/.local/bin), so the
# "not installed" case needs a curated tools dir with the coreutils the wrapper
# uses and NO claude.
TOOLS="$TMP/tools"; mkdir -p "$TOOLS"
for t in sh dash bash sed grep cat mkdir rm printf cut head tail tr wc stat env date awk expr basename dirname sleep chmod ln cp test curl python3; do
  src=$(command -v "$t" 2>/dev/null) && ln -sf "$src" "$TOOLS/$t" 2>/dev/null
done
cp "$WRAP" "$TOOLS/byteask"; chmod +x "$TOOLS/byteask"
: > "$TOOLS/byteask-engine"; chmod +x "$TOOLS/byteask-engine"
export PATH="$BIN:$TOOLS"
export BYTEASK_HOME="$TMP/home/.byteask"; mkdir -p "$BYTEASK_HOME"
export BYTEASK_GATEWAY="https://gw.test" BYTEASK_NO_UPDATE_CHECK=1

echo "== claude not installed -> clean failure, no crash =="
OUT=$(PATH="$TOOLS" byteask claude "hi" 2>&1); RC=$?
has "Claude Code isn't installed" "$OUT" "missing claude gives a clear message"
[ "$RC" = 127 ] && ok "exit 127 when claude absent" || bad "expected exit 127, got $RC"

# Stub claude that echoes how it was invoked, so we can inspect the launch.
cat > "$BIN/claude" <<'CLAUDE'
#!/bin/sh
echo "CLAUDE_INVOKED args=[$*]"
CLAUDE
chmod +x "$BIN/claude"

echo "== interactive launch: passes ByteAsk method + user args to the real claude =="
OUT=$(byteask claude "fix the leak" 2>&1)
has "CLAUDE_INVOKED" "$OUT" "the user's own claude is launched"
has "append-system-prompt" "$OUT" "ByteAsk method is injected via --append-system-prompt"
has "fix the leak" "$OUT" "the user's prompt is passed through"
# The stub prints args=[...]; headless puts -p FIRST, interactive never does.
hasnt "args=[-p" "$OUT" "interactive by default (no headless -p)"

echo "== --headless: uses -p and warns about metered credits =="
OUT=$(byteask claude --headless "audit this" 2>&1)
has "args=[-p" "$OUT" "headless uses claude -p"
has "metered" "$OUT" "headless warns the credits are metered (not the flat subscription)"
has "audit this" "$OUT" "headless still passes the prompt"

echo "== compliance: the launcher NEVER reads the Claude credential file =="
# Static guard on the launcher region: ByteAsk must not read ~/.claude/.credentials.
N=$(sed -n '/^do_claude_launcher()/,/^}/p' "$WRAP" | grep -c 'credentials' || true)
[ "$N" = 0 ] && ok "do_claude_launcher never references the Claude credential" \
             || bad "do_claude_launcher references the credential ($N times)"
# And the method file we injected exists + carries the discipline, not secrets.
has "sanitizer" "$(cat "$BYTEASK_HOME/claude-launcher-method.md" 2>/dev/null)" "method file carries the C/C++ discipline"

echo
echo "claude launcher: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
