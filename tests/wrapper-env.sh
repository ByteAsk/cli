#!/bin/sh
# The environment the wrapper hands the engine, and the "no codex on the user's machine"
# guard for every file the install puts there.
#
# The engine reads BYTEASK_HOME (CODEX_HOME only as a silent legacy fallback) and
# BYTEASK_BRAND, so the wrapper must export those and must NOT export the legacy CODEX_*
# names. A user's own CODEX_HOME belongs to a real Codex install: the wrapper never
# adopted it (it used to overwrite it) and must not start now.
#
# Run:  sh tests/wrapper-env.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT="$HERE/.."
WRAP="$ROOT/byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
cat > "$BIN/byteask-engine" <<'EOF'
#!/bin/sh
# BYTEASK_HOME points to (the new-engine marker, so the legacy shim stays out of these cases)
echo "ENGINE bx=${BYTEASK_HOME:-unset} cx=${CODEX_HOME:-unset} brand=${BYTEASK_BRAND:-unset} cbrand=${CODEX_BRAND:-unset}"
EOF
chmod +x "$BIN/byteask-engine"

seed() {
  mkdir -p "$1"
  cat > "$1/config.toml" <<'EOF'
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
experimental_bearer_token = "test"
EOF
}
FAKEHOME="$TMP/home"; seed "$FAKEHOME/.byteask"
# Extra NAME=VALUE assignments are passed as arguments; stdin is not a TTY.
run() {
  env -u BYTEASK_HOME -u CODEX_HOME -u BYTEASK_BRAND -u CODEX_BRAND \
    HOME="$FAKEHOME" BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_GATEWAY=http://127.0.0.1:1 \
    "$@" "$BIN/byteask" </dev/null 2>&1
}

echo "== engine environment =="
OUT=$(run)
has   "bx=$FAKEHOME/.byteask" "$OUT" "1 default: BYTEASK_HOME=~/.byteask reaches the engine"
has   "cx=unset"              "$OUT" "1 default: CODEX_HOME is not exported"
has   "brand=ByteAsk"         "$OUT" "1 default: BYTEASK_BRAND=ByteAsk reaches the engine"
has   "cbrand=unset"          "$OUT" "1 default: CODEX_BRAND is not exported"

mkdir -p "$TMP/codex-install"
OUT=$(run CODEX_HOME="$TMP/codex-install")
has   "bx=$FAKEHOME/.byteask" "$OUT" "2 user CODEX_HOME: ByteAsk still uses ~/.byteask, never a Codex install"

seed "$TMP/custom"
OUT=$(run BYTEASK_HOME="$TMP/custom")
has   "bx=$TMP/custom"        "$OUT" "3 user BYTEASK_HOME is honoured"
hasnt "cx="$TMP/custom        "$OUT" "3 user BYTEASK_HOME is not mirrored into CODEX_HOME"

OUT=$(run BYTEASK_BRAND=Acme)
has   "brand=Acme"            "$OUT" "4 user BYTEASK_BRAND is honoured"

echo "== no 'codex' in anything the install puts on a user's machine =="
for f in byteask byteask.ps1 install.sh install.ps1 \
         packaging/README.md packaging/pip/README.md packaging/npm/README.md \
         packaging/npm/bin/byteask.js packaging/pip/src/byteask/__init__.py \
         packaging/pip/src/byteask/__main__.py \
         packaging/npm-platforms/cli-linux-x64/byteask packaging/npm-platforms/cli-linux-arm64/byteask \
         packaging/npm-platforms/cli-darwin-arm64/byteask packaging/npm-platforms/cli-win32-x64/byteask.ps1; do
  # The ONE allowed exception is the legacy-engine shim line (legacy-engine.sh): an old
  # engine reads only the legacy names. Anything else mentioning codex fails.
  # Only the two wrappers (and their npm copies) may carry it; installers never.
  case "$f" in
    byteask.ps1|*/cli-win32-x64/byteask.ps1) allow='$env:CODEX_HOME = $BYTEASK_HOME; $env:CODEX_BRAND = $env:BYTEASK_BRAND' ;;
    byteask|packaging/npm-platforms/*/byteask) allow='export CODEX_HOME="$BYTEASK_HOME" CODEX_BRAND="$BYTEASK_BRAND"' ;;
    *) allow='' ;;
  esac
  lines=$(grep -i codex "$ROOT/$f" 2>/dev/null | sed 's/^[[:space:]]*//' || true)
  if [ -z "$lines" ]; then ok "5 $f: no 'codex'"
  elif [ -n "$allow" ] && [ "$lines" = "$allow" ]; then ok "5 $f: only the legacy-engine shim line"
  else bad "5 $f: codex beyond the shim"; printf '%s\n' "$lines" | head -3 | sed 's/^/       /'; fi
done

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
