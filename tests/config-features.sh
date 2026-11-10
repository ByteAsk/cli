#!/bin/sh
# A user's [features] settings survive every full config.toml rewrite: sign-in, sign-out,
# BYOK on/off and a subscription switch. A writer that rebuilds
# the file from scratch drops them, `features.plugins = true` and the
# `commit_attribution` opt-out included.
# Self-contained, no network.   Run:  sh tests/config-features.sh
# (WRAP=<path> runs it against another copy of the launcher, e.g. the pre-fix one.)
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
W="${WRAP:-$REPO/byteask}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

# The launcher runs its launch loop on load, so pull out just the writers.
for f in _cfg_features_block write_managed_config _install_config _wmc_fail \
         write_byok_config write_subscription_config; do
  awk "/^$f\\(\\) \\{/,/^\\}/" "$W" >> "$TMP/fns.sh"
done
grep -q '^write_subscription_config()' "$TMP/fns.sh" || { echo "config-features.sh: could not extract the writers"; exit 1; }
BYTEASK_HOME="$TMP/home"; export BYTEASK_HOME; mkdir -p "$BYTEASK_HOME"
BYOK_PORT=18923
. "$TMP/fns.sh"
CFG="$BYTEASK_HOME/config.toml"

# Print the parsed [features] table as JSON, or ERR when the file is not valid TOML.
features_of() {
  python3 - "$CFG" <<'PY'
import json, sys, tomllib
try:
    with open(sys.argv[1], "rb") as f:
        doc = tomllib.load(f)
except Exception as e:
    print("ERR", e); sys.exit(0)
print(json.dumps(doc.get("features"), sort_keys=True))
PY
}

TABLE='model = "gpt-5.4"
model_provider = "byteask"

[model_providers.byteask]
name = "ByteAsk"

[features]
commit_attribution = false
plugins = true

[projects."/tmp/x"]
trust_level = "trusted"
'
DOTTED='model = "gpt-5.4"
features.commit_attribution = false
features.plugins = true

[model_providers.byteask]
name = "ByteAsk"
'
NESTED='model = "gpt-5.4"

[features]
# the user wrote this comment
commit_attribution = false

[features.multi_agent_v2]
enabled = true

[model_providers.byteask]
name = "ByteAsk"
'
NONE='model = "gpt-5.4"

[model_providers.byteask]
name = "ByteAsk"
'
WANT_FLAT='{"commit_attribution": false, "plugins": true}'
WANT_NESTED='{"commit_attribution": false, "multi_agent_v2": {"enabled": true}}'

# Each writer, as the launcher calls it.
w_signin()   { write_managed_config gpt-5.4 'model_catalog_json = "/x/models-catalog.json"' https://gw.test tok1; }
w_signout()  { write_managed_config gpt-5.4 'model_catalog_json = "/x/models-catalog.json"' https://gw.test ""; }
w_byok()     { write_byok_config gpt-5.4 'model_catalog_json = "/x/models-catalog.json"' lt1; }
w_sub()      { write_subscription_config "" 'model_catalog_json = "/x/sub.json"' https://gw.test jwt1; }
w_sub_signedout() { write_subscription_config "" 'model_catalog_json = "/x/sub.json"' https://gw.test ""; }

for writer in w_signin w_signout w_byok w_sub w_sub_signedout; do
  echo "== $writer =="
  for seed in TABLE DOTTED NESTED NONE; do
    eval "printf '%s' \"\$$seed\"" > "$CFG"
    case "$seed" in TABLE|DOTTED) want=$WANT_FLAT;; NESTED) want=$WANT_NESTED;; NONE) want=null;; esac
    if $writer >/dev/null 2>&1; then :; else bad "$writer/$seed: the writer failed"; continue; fi
    got=$(features_of)
    [ "$got" = "$want" ] && ok "$writer keeps $seed features" || bad "$writer/$seed: want $want, got $got"
    # A second rewrite must not duplicate the table (it reads the first rewrite's output).
    $writer >/dev/null 2>&1
    got2=$(features_of); n=$(grep -c '^\[features\]' "$CFG")
    { [ "$got2" = "$want" ] && [ "$n" -le 1 ]; } && ok "$writer/$seed: stable over a second rewrite" \
      || bad "$writer/$seed: second rewrite gave $got2 with $n [features] headers"
  done
done

echo "== the writers still do their own job =="
printf '%s' "$TABLE" > "$CFG"; w_signin >/dev/null 2>&1
grep -q '^experimental_bearer_token = "tok1"$' "$CFG" && ok "sign-in token landed" || bad "sign-in token missing"
grep -q '^model_provider = "byteask"$' "$CFG" && ok "managed provider set" || bad "managed provider missing"
grep -q 'trust_level' "$CFG" && bad "carried a non-features table (scope creep)" || ok "only [features] is carried"
printf '%s' "$TABLE" > "$CFG"; w_byok >/dev/null 2>&1
grep -q '^model_provider = "byok-local"$' "$CFG" && ok "byok provider set" || bad "byok provider missing"
printf '%s' "$TABLE" > "$CFG"; w_sub >/dev/null 2>&1
grep -q '^model_provider = "openai"$' "$CFG" && ok "subscription provider set" || bad "subscription provider missing"
rm -f "$CFG"; w_signin >/dev/null 2>&1 && [ "$(features_of)" = null ] \
  && ok "no previous config: works, no features" || bad "no previous config broke the writer"
ls -a "$BYTEASK_HOME" | grep -q 'tmp' && bad "left a temp file behind" || ok "no temp files left"

echo
echo "config-features: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
