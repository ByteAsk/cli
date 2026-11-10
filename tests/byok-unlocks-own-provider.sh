#!/bin/sh
# A user's own provider key unlocks that provider's paywalled /model rows.
# Without this a Free user who saves an Anthropic key still sees "Available on ByteAsk Pro":
# the per-account overlay locks premium models by plan and knows nothing of the key,
# although own-key turns go direct and cost us nothing. The unlock applies ONLY while
# BYOK is the active provider: keys can outlive it, and on managed those turns reach
# the gateway, whose gate refuses them.
# Runs the wrapper's real _models_access_merge against the real helper. No network.
#   Run:  sh tests/byok-unlocks-own-provider.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="${WRAP:-$HERE/../byteask}"
HELPER="$HERE/../helpers/byteask_models.py"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME"

# provider in config.toml, keys in byok-config.json; the gateway locked both premium rows.
setup() {   # $1=model_provider  $2=keys JSON object
  printf 'model = "gpt-5.4"\nmodel_provider = "%s"\n' "$1" > "$BYTEASK_HOME/config.toml"
  printf '{"keys": %s}\n' "$2" > "$BYTEASK_HOME/byok-config.json"
  echo '{"models":[{"slug":"gpt-5.4"},{"slug":"gpt-6.1-sol"},{"slug":"claude-opus-5-5"}]}' \
    > "$BYTEASK_HOME/models-catalog.json"
  echo '{"locked":["claude-opus-5-5","gpt-6.1-sol"],"inactive":[]}' > "$BYTEASK_HOME/.model-access.json"
}
run() {
  _MA_FETCHED=1 BYOK_MODELS="$HELPER" BYOK_CFG="$BYTEASK_HOME/byok-config.json" sh -c '
. /dev/stdin
_models_access_merge' <<EOT
$(sed -n '/^_models_access_merge() {$/,/^}$/p' "$WRAP")
EOT
}
locked() {  # prints "yes"/"no" for slug $1
  python3 -c 'import json,sys
m={x["slug"]:x for x in json.load(open(sys.argv[1]))["models"]}
print("yes" if m[sys.argv[2]].get("locked_reason") else "no")' "$BYTEASK_HOME/models-catalog.json" "$1"
}

echo "own key unlocks its provider's rows"
setup byok-local '{"anthropic":"sk-ant-test"}'; run
[ "$(locked claude-opus-5-5)" = no ] && ok "BYOK + Anthropic key: Claude selectable" \
  || bad "BYOK + Anthropic key: Claude still locked"
[ "$(locked gpt-6.1-sol)" = yes ] && ok "BYOK + Anthropic key: OpenAI premium still locked" \
  || bad "BYOK + Anthropic key unlocked an OpenAI model"

setup byteask '{"anthropic":"sk-ant-test"}'; run
[ "$(locked claude-opus-5-5)" = yes ] && ok "managed provider + leftover key: Claude stays locked" \
  || bad "managed provider + leftover key unlocked Claude (gateway would refuse the turn)"

setup byok-local '{}'; run
[ "$(locked claude-opus-5-5)" = yes ] && ok "BYOK without an Anthropic key: Claude stays locked" \
  || bad "BYOK without a key unlocked Claude"

printf '%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
