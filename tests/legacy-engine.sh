#!/bin/sh
# Legacy-engine shim: a new wrapper paired with an OLD engine must not look signed out.
#
# Engines before this release read only the legacy home/brand variable names. When an update
# keeps the old engine (its engine fetch failed, say), the wrapper must hand that engine the
# legacy names pointing at the SAME ByteAsk home -- and hand a new engine nothing legacy.
# A new engine is recognised by its home-dir error text "BYTEASK_HOME points to ", which no
# older engine carries. The scan is cached per engine file (path + size + mtime).
#
# Run:  sh tests/legacy-engine.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
FAKEHOME="$TMP/home"; BX="$FAKEHOME/.byteask"; mkdir -p "$BX"
cat > "$BX/config.toml" <<'EOF'
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
experimental_bearer_token = "test"
EOF
CACHE="$BX/engine-compat"

# Two stubs of IDENTICAL size: the legacy one carries a same-length placeholder where the new
# one carries the marker, so a stub can be flipped without changing its size.
engine() {  # $1 = marker text for the comment line
  cat > "$BIN/byteask-engine" <<EOF
#!/bin/sh
# $1
echo "ENGINE cx=\${CODEX_HOME:-unset} cb=\${CODEX_BRAND:-unset}"
EOF
  chmod +x "$BIN/byteask-engine"
}
LEGACY='XYTEASK_HOME points to '
NEW='BYTEASK_HOME points to '

run() {
  env -u CODEX_HOME -u CODEX_BRAND -u BYTEASK_HOME -u BYTEASK_BRAND \
    HOME="$FAKEHOME" BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_GATEWAY=http://127.0.0.1:1 \
    "$BIN/byteask" "$@" </dev/null 2>&1
}

echo "== legacy engine gets the legacy names, pointing at the ByteAsk home =="
engine "$LEGACY"; rm -f "$CACHE"
OUT=$(run)
has "cx=$BX"      "$OUT" "1 legacy engine: legacy home var = the ByteAsk home"
has "cb=ByteAsk"  "$OUT" "1 legacy engine: legacy brand var = ByteAsk"
grep -q '|legacy$' "$CACHE" 2>/dev/null && ok "1 verdict cached as legacy" || bad "1 verdict cached as legacy ($(cat "$CACHE" 2>/dev/null))"

echo "== new engine gets nothing legacy =="
engine "$NEW"; touch -d '2001-01-01 00:00:00' "$BIN/byteask-engine"
OUT=$(run)
has "cx=unset"    "$OUT" "2 new engine: legacy home var not exported"
has "cb=unset"    "$OUT" "2 new engine: legacy brand var not exported"
grep -q '|new$' "$CACHE" 2>/dev/null && ok "2 verdict cached as new" || bad "2 verdict cached as new ($(cat "$CACHE" 2>/dev/null))"

echo "== a cache hit does not rescan =="
engine "$LEGACY"; touch -d '2001-01-03 00:00:00' "$BIN/byteask-engine"; rm -f "$CACHE"
run >/dev/null                                    # caches legacy for this path+size+mtime
cp -p "$BIN/byteask-engine" "$TMP/ref"
sed -i "s/$LEGACY/$NEW/" "$BIN/byteask-engine"    # now carries the marker, same size...
touch -r "$TMP/ref" "$BIN/byteask-engine"         # ...and the same mtime
OUT=$(run)
has "cx=$BX"      "$OUT" "3 same path+size+mtime: cached legacy verdict used, file not rescanned"

echo "== the cache invalidates when the engine file changes =="
touch -d '2001-01-04 00:00:00' "$BIN/byteask-engine"
OUT=$(run)
has "cx=unset"    "$OUT" "4 changed mtime: rescanned, now recognised as a new engine"
grep -q '|new$' "$CACHE" 2>/dev/null && ok "4 cache rewritten as new" || bad "4 cache rewritten ($(cat "$CACHE" 2>/dev/null))"

echo "== --version and --help never scan =="
rm -f "$CACHE"
run --version >/dev/null; run --help >/dev/null
[ ! -f "$CACHE" ] && ok "5 no scan for --version/--help" || bad "5 --version/--help wrote the cache"

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
