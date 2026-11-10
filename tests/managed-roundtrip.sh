#!/bin/sh
# "Use ByteAsk managed" round trip — the way BACK from a subscription session.
#
# Regression test for a bug that left users stuck: the engine's
# `use_byteask_managed()` returned early whenever `keyed_providers.is_empty()`,
# which is ALWAYS true in a subscription session, so the row that exists to be the
# way back was a silent no-op — and the wrapper had no `managed` marker verb at all.
#
# Also pins the rule: restore the PREVIOUS model, not a default.
#
#   sh tests/managed-roundtrip.sh
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
W="$REPO/byteask"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

# The wrapper runs its launch loop on load, so exercise the two functions in
# isolation rather than sourcing the whole script.
awk '/^_save_managed_session\(\) \{/,/^\}/' "$W"  > "$TMP/fns.sh"
awk '/^byok_use_managed\(\) \{/,/^\}/'      "$W" >> "$TMP/fns.sh"
[ -s "$TMP/fns.sh" ] || { echo "managed-roundtrip.sh: could not extract the functions"; exit 1; }
grep -q '^byok_use_managed()' "$TMP/fns.sh" || { echo "managed-roundtrip.sh: byok_use_managed missing"; exit 1; }

BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME"
. "$TMP/fns.sh"
_byok_current_jwt() { echo "JWT123"; }
resolve_gateway()   { echo "https://code.byteask.ai"; }
write_managed_config() {   # record the call instead of writing a real config
  printf 'model=%s\ncatalog=%s\ngw=%s\njwt=%s\n' "$1" "$2" "$3" "$4" > "$BYTEASK_HOME/wmc.called"
}

echo "== a saved managed session is restored exactly =="
_save_managed_session "gpt-5.6-terra" 'model_catalog_json = "/home/u/.byteask/models-catalog.json"'
[ -f "$BYTEASK_HOME/managed-session.prev" ] && ok "the managed session was remembered" \
                                            || bad "the managed session was remembered"
byok_use_managed >/dev/null
grep -q '^model=gpt-5.6-terra$' "$BYTEASK_HOME/wmc.called" \
  && ok "restores the PREVIOUS model, not a default" || bad "restores the PREVIOUS model"
grep -q 'models-catalog.json' "$BYTEASK_HOME/wmc.called" \
  && ok "restores model_catalog_json (subscription had dropped it)" || bad "restores model_catalog_json"
grep -q '^jwt=JWT123$' "$BYTEASK_HOME/wmc.called" \
  && ok "carries the ByteAsk identity forward" || bad "carries the ByteAsk identity forward"
[ -f "$BYTEASK_HOME/managed-session.prev" ] && bad "the remembered session is consumed" \
                                            || ok "the remembered session is consumed"

echo "== with NO remembered session it still returns a WORKING managed config =="
rm -f "$BYTEASK_HOME/wmc.called"
byok_use_managed >/dev/null
grep -q '^model=auto$' "$BYTEASK_HOME/wmc.called" \
  && ok "falls back to the default model" || bad "falls back to the default model"
grep -q 'models-catalog.json' "$BYTEASK_HOME/wmc.called" \
  && ok "never restores managed with an EMPTY catalog line" || bad "never restores an empty catalog"

echo "== the engine's marker verb is actually dispatched by the wrapper =="
grep -q "'managed')" "$W" && ok "the wrapper handles the 'managed' marker verb" \
                          || bad "the wrapper handles the 'managed' marker verb"

echo
echo "managed-roundtrip.sh: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
