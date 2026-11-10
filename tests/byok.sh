#!/bin/sh
# BYOK wrapper tests: `byteask byok set/status/remove/off`. Verifies key storage
# (0600, never in config.toml), the JWT migration into the sidecar store, the
# config.toml flip to/from the byok-local provider, and validation reject.
# Self-contained: stubs curl (provider /models validation) + the engine; skips the
# real sidecar spawn via BYOK_SKIP_SIDECAR. No network.
#
# Run:  sh tests/byok.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
SIDECAR="$HERE/../helpers/byok_sidecar.py"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')";; *) ok "$3";; esac; }

TMP=$(mktemp -d)
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
# Stub engine: exits 0 like the old no-op, but RECORDS its argv. Some behaviour we
# need to assert is a call we make TO the engine (e.g. discarding a ChatGPT credential
# with `logout`), and a no-op stub cannot show that happened.
cat > "$BIN/byteask-engine" <<'ENG_EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$ENGINE_LOG"
[ -n "${BYTEASK_REPORTED_VERSION_FLOOR:-}" ] && printf 'FLOOR=%s\n' "$BYTEASK_REPORTED_VERSION_FLOOR" >> "$ENGINE_LOG"
# Simulate the real `login`, which REPLACES auth.json with the newly signed-in
# account before anything can inspect it. Without this the credential-restore
# assertion would pass on a file nothing ever overwrote.
case "$1" in
  login) [ -f "$BYTEASK_HOME/auth.json.go" ] && cp "$BYTEASK_HOME/auth.json.go" "$BYTEASK_HOME/auth.json" ;;
  logout) rm -f "$BYTEASK_HOME/auth.json" ;;
esac
# A TUI launch can hand work back through the marker. The wrapper DELETES any stale
# marker before launching (a marker from a crashed session must not be dispatched), so
# a test cannot pre-seed one - the engine has to write it during the run, as the real
# engine does when the user picks a row in /login.
# ONE SHOT: the trigger file is consumed as it fires. An env-var trigger would be
# inherited by every relaunch, and since the wrapper relaunches after dispatching a
# marker, the engine would hand back the same work forever.
if [ -z "${1:-}" ] && [ -f "$BYTEASK_HOME/.stub-marker" ]; then
  mv -f "$BYTEASK_HOME/.stub-marker" "$BYTEASK_HOME/.byteask-auth-request"
fi
exit 0
ENG_EOF
chmod +x "$BIN/byteask-engine"

# curl shim: validate_key uses `-w %{http_code}` -> echo $CURL_CODE (default 200,
# override via the marker file to test reject). Everything else exits 0/1 harmlessly.
cat > "$BIN/curl" <<'CURL_EOF'
#!/bin/sh
for a in "$@"; do case "$a" in "%{http_code}") echo "$(cat "$CODE_FILE" 2>/dev/null || echo 200)"; exit 0;; esac; done
for a in "$@"; do case "$a" in */byteask/v1/subscription-flags) [ -f "${FLAGS_BODY:-}" ] && { cat "$FLAGS_BODY"; exit 0; };; esac; done
exit 1
CURL_EOF
chmod +x "$BIN/curl"

HOME_BX="$TMP/home/.byteask"; mkdir -p "$HOME_BX"
cp "$SIDECAR" "$HOME_BX/byok_sidecar.py"   # so the `-f "$BYOK_SIDECAR"` guard passes
export PATH="$BIN:$PATH"
export BYTEASK_HOME="$HOME_BX"
export BYTEASK_GATEWAY="https://gw.test"
export BYOK_SKIP_SIDECAR=1
export BYTEASK_NO_UPDATE_CHECK=1
export CODE_FILE="$TMP/curlcode"
export ENGINE_LOG="$TMP/engine-argv.log"; : > "$ENGINE_LOG"
CFG="$HOME_BX/config.toml"
BCFG="$HOME_BX/byok-config.json"

seed_signed() { cat > "$CFG" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
web_search = "live"

[model_providers.byteask]
name = "ByteAsk"
base_url = "https://gw.test/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "JWT-ORIGINAL"

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
EOF
}

echo "== byok status (off) =="
seed_signed; rm -f "$BCFG"
OUT=$(byteask byok status 2>&1)
has "BYOK: off" "$OUT" "status shows off with no keys"

echo "== byok set openai (piped key, validated 200) =="
echo 200 > "$CODE_FILE"
OUT=$(printf 'sk-USERKEY\n' | byteask byok set openai 2>&1)
has "Saved your openai key" "$OUT" "set reports saved"
[ -f "$BCFG" ] && ok "byok-config.json created" || bad "byok-config.json missing"
has '"openai": "sk-USERKEY"' "$(cat "$BCFG")" "key stored in byok-config.json"
PERM=$(stat -c '%a' "$BCFG" 2>/dev/null || stat -f '%Lp' "$BCFG" 2>/dev/null)
[ "$PERM" = 600 ] && ok "byok-config.json is 0600" || bad "byok-config.json perms=$PERM (want 600)"
has 'model_provider = "byok-local"' "$(cat "$CFG")" "config.toml flipped to byok-local"
has "127.0.0.1:8799" "$(cat "$CFG")" "config points at the loopback sidecar"
has "supports_websockets = false" "$(cat "$CFG")" "websockets disabled"

echo "== JWT migration: key NEVER in config.toml; JWT moved to sidecar store =="
hasnt "sk-USERKEY" "$(cat "$CFG")" "the API key is NOT in config.toml"
hasnt "experimental_bearer_token" "$(cat "$CFG")" "no managed JWT left in config.toml (engine holds no secret)"
has 'JWT-ORIGINAL' "$(cat "$BCFG")" "JWT migrated into the sidecar store"
has 'X-BYOK-Token' "$(cat "$CFG")" "loopback token header present"

echo "== byok status (on) =="
OUT=$(byteask byok status 2>&1)
has "BYOK: on" "$OUT" "status shows on"
has "openai: your key" "$OUT" "openai shown as your key"
has "anthropic: managed" "$OUT" "anthropic still managed"

echo "== byok set anthropic (second key) =="
OUT=$(printf 'sk-ant-USER\n' | byteask byok set anthropic 2>&1)
has '"anthropic": "sk-ant-USER"' "$(cat "$BCFG")" "anthropic key added"
has '"openai": "sk-USERKEY"' "$(cat "$BCFG")" "openai key retained"

echo "== byok remove openai (one of two) =="
OUT=$(byteask byok remove openai 2>&1)
hasnt '"openai"' "$(cat "$BCFG")" "openai key removed"
has '"anthropic": "sk-ant-USER"' "$(cat "$BCFG")" "anthropic key retained"
has 'model_provider = "byok-local"' "$(cat "$CFG")" "still byok-local (keys remain)"

echo "== byok off (restore managed, JWT round-trips back) =="
OUT=$(byteask byok off 2>&1)
has "ByteAsk managed" "$OUT" "off reports success"
has 'model_provider = "byteask"' "$(cat "$CFG")" "config.toml restored to managed"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "JWT restored into managed config"
KN=$(printf '%s' "$(cat "$BCFG" 2>/dev/null)" | grep -c 'sk-ant-USER' || true)
[ "$KN" = 0 ] && ok "keys cleared from byok-config on off" || bad "keys not cleared on off"

echo "== validation reject (provider returns 401) =="
seed_signed; rm -f "$BCFG"; echo 401 > "$CODE_FILE"
OUT=$(printf 'sk-BAD\n' | byteask byok set openai 2>&1)
has "rejected by the provider" "$OUT" "bad key rejected with a clear message"
[ ! -f "$BCFG" ] || hasnt "sk-BAD" "$(cat "$BCFG")" "rejected key not stored"
has 'experimental_bearer_token' "$(cat "$CFG")" "managed config untouched after reject"

echo "== unknown provider =="
OUT=$(printf 'x\n' | byteask byok set bogus 2>&1 || true)
has "usage: byteask byok set" "$OUT" "unknown provider shows usage"

# ---- ChatGPT subscription switch (atomic + identity-preserving) ----------
# The stub engine (no-op) makes `"$ENGINE" login` succeed, so we exercise the
# config rewrite. The regression this pins: a bare `cat >` drops the
# ByteAsk JWT entirely, which silently signs the user out.
echo "== ChatGPT subscription: switch is atomic and keeps the ByteAsk identity =="
seed_signed; rm -f "$BCFG"
# Seed a catalog line so "no model_catalog_json after the switch" proves the switch
# DROPS it, not that it was never there.
printf 'model_catalog_json = "/x/models.json"\n' >> "$CFG"
# Seed the managed catalog the subscription copy is derived FROM. It deliberately holds
# a non-OpenAI model and `auto`, so the filtering is exercised rather than assumed.
cat > "$BYTEASK_HOME/models-catalog.json" <<'MCAT'
{"models":[
 {"slug":"auto","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-6-astra","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-5.6-sol","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-5.4","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gemini-3.8-flash","base_instructions":"You are ByteAsk, a coding agent."}
]}
MCAT
OUT=$(byteask byok set openai --subscription 2>&1)
has "Switched to your ChatGPT subscription" "$OUT" "subscription switch reports success"
has "Use ByteAsk managed" "$OUT" "success line names the way back (/login), not the retired 'byteask byok off'"
has 'model_provider = "openai"' "$(cat "$CFG")" "config flipped to the openai provider"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "ByteAsk JWT retained across the switch [regression: config-wipe]"
has '[model_providers.byteask]' "$(cat "$CFG")" "retained byteask block (provider-independent identity)"
# Dropping the catalog would hand the picker, the default AND the
# identity to upstream's bundled list: 11 models the account may not have, a default of
# gpt-6-astra that a ChatGPT account cannot use (so the FIRST message 400s), and
# "You are Codex" in the prompt. We now ship a filtered, OpenAI-only copy of our own.
has 'models-catalog-subscription.json' "$(cat "$CFG")" "a subscription session gets our FILTERED catalog, not upstream's bundled list"
SUBCAT="$BYTEASK_HOME/models-catalog-subscription.json"
if [ -f "$SUBCAT" ]; then
  ok "the subscription catalog was written"
  python3 - "$SUBCAT" <<'SUBCHK'
import json, sys
d = json.load(open(sys.argv[1]))
slugs = [m["slug"] for m in d["models"]]
bad = [s for s in slugs if not s.startswith("gpt-")]
assert not bad, f"non-OpenAI models would 400 on the ChatGPT backend: {bad}"
assert "auto" not in slugs, "auto is a gateway-side chain; there is no gateway here"
dflt = [m["slug"] for m in d["models"] if m.get("is_default")]
assert len(dflt) == 1, f"exactly one default required, got {dflt}"
assert dflt[0] in slugs
SUBCHK
  if [ $? -eq 0 ]; then ok "it is OpenAI-only, has no 'auto', and has exactly one default"
  else bad "the subscription catalog is malformed"; fi
  # REGRESSION: gpt-5.4 shipped as the DEFAULT of this list and is refused outright on a
  # ChatGPT account, so every Plus session failed on its first message. Measured
  # 2026-09-22: a ChatGPT account gets the 5.6 family and astra, never 5.4 or codex-*.
  for m in gpt-5.4 gpt-5.4-mini gpt-5.4-nano gpt-4.1 gpt-5.3-codex gpt-5.2; do
    if grep -q "\"$m\"" "$SUBCAT" 2>/dev/null; then
      bad "offers $m, which a ChatGPT account is not entitled to"
    fi
  done
  ok "offers no model a ChatGPT account is refused on"
else
  bad "no subscription catalog was written"
fi
[ ! -f "$CFG".tmp."$$" ] && ok "no temp config left behind" || bad "temp config leaked"

# A plan that serves NOTHING must be refused at sign-in, with the config untouched.
# Measured 2026-09-22: on a real `go` account all 11 offered models were refused, so
# switching would hand the user a session that fails on their first message.
# An EXISTING subscription session (config written before the filtered catalog existed)
# carries no model_catalog_json, so it would keep using upstream's bundled list until the
# user happened to sign in again. The launch path repairs it once, in place.
# The switches are server-fetched and default OFF. Without a local pin, a dormant
# feature cannot be exercised on one machine without enabling it for every user.
# The engine now signs in itself, in a pane, and hands over only the provider swap.
# The wrapper must do that swap and print NOTHING: anything it echoes lands in
# scrollback, which is the whole reason the flow moved into the engine.
echo "== subscription-ready: the wrapper finishes silently, drawing nothing =="
seed_signed; rm -f "$BCFG" "$BYTEASK_HOME/models-catalog-subscription.json"
cat > "$BYTEASK_HOME/models-catalog.json" <<'MCAT3'
{"models":[
 {"slug":"gpt-6-astra","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-5.6-sol","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gemini-3.8-flash","base_instructions":"You are ByteAsk, a coding agent."}
]}
MCAT3
printf 'login\nsubscription-ready chatgpt\n' > "$BYTEASK_HOME/.stub-marker"
OUT=$(byteask 2>&1 || true)
has 'model_provider = "openai"' "$(cat "$CFG")" "the wrapper completed the provider swap"
has 'models-catalog-subscription.json' "$(cat "$CFG")" "and pointed at the filtered catalog"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "keeping the ByteAsk identity"
hasnt 'Sign in with your ChatGPT subscription' "$OUT" "it does NOT redraw the sign-in screen"
hasnt 'one-time code' "$OUT" "it prints no code"
hasnt 'Requires ChatGPT Plus' "$OUT" "it prints no requirement line"
byteask byok off >/dev/null 2>&1 || true

# gpt-6-sol / gpt-6-luna are always offered: the launcher has no client-version floor.
# They are synthesized from astra (keeping the ByteAsk identity) because the managed
# catalog does not carry them.
echo "== subscription catalog: gpt-6-sol/luna are offered; no version floor is read, stored or exported =="
seed_signed; rm -f "$BCFG"
cat > "$BYTEASK_HOME/models-catalog.json" <<'MCAT4'
{"models":[
 {"slug":"gpt-6-astra","base_instructions":"You are ByteAsk, a coding agent.",
  "supported_reasoning_levels":[{"effort":"low","description":"l"},{"effort":"medium","description":"m"},
    {"effort":"high","description":"h"},{"effort":"max","description":"x"},{"effort":"ultra","description":"u"}],
  "availability_nux":{"message":"astra launch banner"}},
 {"slug":"gpt-5.6-sol","base_instructions":"You are ByteAsk, a coding agent."}
]}
MCAT4
SUBCAT="$BYTEASK_HOME/models-catalog-subscription.json"
sub_slugs() { python3 -c "import json;print(' '.join(m['slug'] for m in json.load(open('$SUBCAT'))['models']))" 2>/dev/null; }

rm -f "$BYTEASK_HOME/subscription-flags" "$SUBCAT"
BYTEASK_SUBSCRIPTION_FLAGS_LOCAL=1 byteask byok set openai --subscription >/dev/null 2>&1 || true
has 'gpt-6-sol' "$(sub_slugs)" "no flags file: gpt-6-sol is offered"
has 'gpt-6-luna' "$(sub_slugs)" "no flags file: gpt-6-luna is offered"
[ "$(sub_slugs)" = "gpt-6-astra gpt-6-sol gpt-6-luna gpt-5.6-sol" ] \
  && ok "the catalog is the plain slug list, in order" || bad "unexpected subscription catalog: $(sub_slugs)"
SYN=$(python3 -c "
import json; ms={m['slug']:m for m in json.load(open('$SUBCAT'))['models']}
s=ms['gpt-6-sol']
print('identity=%s nux=%s levels=%s' % (s['base_instructions'].startswith('You are ByteAsk'),
      'availability_nux' in s, ','.join(l['effort'] for l in s['supported_reasoning_levels'])))" 2>/dev/null)
has 'identity=True' "$SYN" "the synthesized model keeps the ByteAsk identity"
has 'nux=False' "$SYN" "and does not inherit astra's launch banner"

# a floor left in the file by an older launcher changes nothing and is not exported
printf 'chatgpt=1\nreported_version_floor=0.154.9\n' > "$BYTEASK_HOME/subscription-flags"
rm -f "$SUBCAT"
BYTEASK_SUBSCRIPTION_FLAGS_LOCAL=1 byteask byok set openai --subscription >/dev/null 2>&1 || true
has 'gpt-6-sol' "$(sub_slugs)" "an old floor line in the flags file does not withhold gpt-6-sol"
printf 'chatgpt=1\nreported_version_floor=0.155.1\n' > "$BYTEASK_HOME/subscription-flags"
: > "$ENGINE_LOG"; BYTEASK_SUBSCRIPTION_FLAGS_LOCAL=1 byteask >/dev/null 2>&1 || true
[ -s "$ENGINE_LOG" ] && ok "the engine was launched" || bad "the engine was not launched"
hasnt 'FLOOR=' "$(cat "$ENGINE_LOG")" "no floor is exported to the engine"
# a floor sent by the server is not stored: the refresh rewrites the file without it
printf '{"chatgpt": true, "claude": false, "claude_launcher": false, "reported_version_floor": "0.155.1"}' > "$TMP/flags.json"
BYTEASK_SUBSCRIPTION_FLAGS_LOCAL=0 FLAGS_BODY="$TMP/flags.json" byteask >/dev/null 2>&1 || true
_i=0; while grep -q floor "$BYTEASK_HOME/subscription-flags" 2>/dev/null && [ "$_i" -lt 30 ]; do sleep 0.1; _i=$((_i+1)); done
[ "$(cat "$BYTEASK_HOME/subscription-flags")" = "chatgpt=1
claude=0
claude_launcher=0" ] && ok "the refreshed flags file holds the three switches and no floor" \
  || bad "unexpected flags file: $(cat "$BYTEASK_HOME/subscription-flags")"
rm -f "$BYTEASK_HOME/subscription-flags"
byteask byok off >/dev/null 2>&1 || true

echo "== subscription flags: a local pin survives the launch refresh =="
printf 'chatgpt=1\nclaude=0\nclaude_launcher=0\n' > "$BYTEASK_HOME/subscription-flags"
BYTEASK_SUBSCRIPTION_FLAGS_LOCAL=1 byteask >/dev/null 2>&1 || true
has 'chatgpt=1' "$(cat "$BYTEASK_HOME/subscription-flags")" "the pinned flag survives a launch"
rm -f "$BYTEASK_HOME/subscription-flags"

echo "== ChatGPT subscription: an OLD config is healed at launch, keeping identity =="
seed_signed
cat > "$BYTEASK_HOME/models-catalog.json" <<'MCAT2'
{"models":[
 {"slug":"auto","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-6-astra","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-5.6-sol","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gpt-5.4","base_instructions":"You are ByteAsk, a coding agent."},
 {"slug":"gemini-3.8-flash","base_instructions":"You are ByteAsk, a coding agent."}
]}
MCAT2
# Build a pre-change subscription config the way one really existed: do the real switch,
# then strip the catalog line and the derived file, as a config written before this change.
byteask byok set openai --subscription >/dev/null 2>&1 || true
sed -i '/^model_catalog_json = /d' "$CFG"
rm -f "$BYTEASK_HOME/models-catalog-subscription.json"
hasnt 'model_catalog_json' "$(cat "$CFG")" "precondition: the old config has no catalog line"
has 'model_provider = "openai"' "$(cat "$CFG")" "precondition: it is a subscription config"
byteask >/dev/null 2>&1 || true   # a launch, which is where the repair happens
has 'models-catalog-subscription.json' "$(cat "$CFG")" "launch healed the old config"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "healing kept the ByteAsk identity"
has 'model_provider = "openai"' "$(cat "$CFG")" "healing kept the subscription provider"

echo "== ChatGPT subscription: no managed catalog -> falls back, never a broken one =="
seed_signed; rm -f "$BCFG" "$BYTEASK_HOME/models-catalog.json" "$BYTEASK_HOME/models-catalog-subscription.json"
OUT=$(byteask byok set openai --subscription 2>&1)
has "Switched to your ChatGPT subscription" "$OUT" "the switch still succeeds with no catalog to filter"
hasnt 'model_catalog_json' "$(cat "$CFG")" "and writes NO catalog line rather than a broken one"

echo "== ChatGPT subscription: a `go` plan is refused and changes nothing =="
seed_signed
CFG_BEFORE="$(cat "$CFG")"
# An id_token whose payload carries chatgpt_plan_type=go (header/signature are dummies;
# _chatgpt_plan_type only base64-decodes the payload, it does not verify).
GO_PAYLOAD=$(python3 -c "
import base64, json
p = json.dumps({'https://api.openai.com/auth': {'chatgpt_plan_type': 'go'}}).encode()
print(base64.urlsafe_b64encode(p).decode().rstrip('='))")
# A credential that was already there and working. The stub engine's `login` does not
# rewrite auth.json, so seed the REFUSED one and mark it, then prove the pre-existing
# file comes back. (Real `login` overwrites it; the wrapper backs it up either way.)
printf '{"tokens":{"id_token":"PREVIOUS-PLUS-TOKEN"}}\n' > "$BYTEASK_HOME/auth.json.working"
cp "$BYTEASK_HOME/auth.json.working" "$BYTEASK_HOME/auth.json"
printf '{\"tokens\":{\"id_token\":\"h.%s.s\"}}\n' "$GO_PAYLOAD" > "$BYTEASK_HOME/auth.json.go"
: > "$ENGINE_LOG"
OUT=$(byteask byok set openai --subscription 2>&1 || true)
has "Plus or higher. Free and Go accounts are not supported" "$OUT" "the requirement is stated BEFORE sign-in"
has "go" "$OUT" "the refusal names the plan"
has "Plus or higher" "$OUT" "the refusal says what to do"
hasnt "Switched to your ChatGPT subscription" "$OUT" "it does NOT claim success"
has "logout" "$(cat "$ENGINE_LOG")" "the unusable ChatGPT credential is discarded via the engine"
# A refusal must not destroy a subscription session that was already working: `login`
# replaces auth.json BEFORE the plan can be read, so a bare logout loses the good one.
has "PREVIOUS-PLUS-TOKEN" "$(cat "$BYTEASK_HOME/auth.json" 2>/dev/null)" "the previous credential is restored, not destroyed"
if [ "$CFG_BEFORE" = "$(cat "$CFG")" ]; then ok "config untouched by a refused switch"
else bad "a refused switch rewrote config.toml"; fi
# Clear the simulated sign-in identity too, or every later subscription switch in this
# file signs in as the same `go` account and is refused.
rm -f "$BYTEASK_HOME/auth.json" "$BYTEASK_HOME/auth.json.go" "$BYTEASK_HOME/auth.json.working"

echo "== ChatGPT subscription: byok off restores managed, JWT round-trips =="
OUT=$(byteask byok off 2>&1)
has 'model_provider = "byteask"' "$(cat "$CFG")" "byok off restores the managed provider"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "JWT round-trips back after a subscription switch"

echo "== ChatGPT subscription: --sub alias works too =="
seed_signed; rm -f "$BCFG"
OUT=$(byteask byok set openai --sub 2>&1)
has 'model_provider = "openai"' "$(cat "$CFG")" "--sub alias also flips to openai"

echo "== ChatGPT subscription: headless (SSH) session uses device-code sign-in =="
seed_signed; rm -f "$BCFG"
# SSH_CONNECTION set + no DISPLAY => _subscription_is_headless true => device-code path.
OUT=$(env SSH_CONNECTION="1.2.3.4 5 6.7.8.9 22" DISPLAY= WAYLAND_DISPLAY= byteask byok set openai --subscription 2>&1)
has "device-code sign-in" "$OUT" "headless SSH session announces device-code sign-in"
has 'model_provider = "openai"' "$(cat "$CFG")" "headless subscription still flips to openai"

echo "== ChatGPT subscription: --browser forces the browser path even on SSH (no device msg) =="
seed_signed; rm -f "$BCFG"
OUT=$(env SSH_CONNECTION="1.2.3.4 5 6.7.8.9 22" DISPLAY= byteask byok set openai --subscription --browser 2>&1)
hasnt "device-code sign-in" "$OUT" "--browser overrides headless auto-detection"

# ---- login/source-menu flow (do_account + do_source_menu) -------------------
# BYOK_ASSUME_TTY forces the interactive menu so we can pipe selections.
echo "== source menu: set OpenAI THEN Anthropic (keys mix) =="
seed_signed; rm -f "$BCFG"; echo 200 > "$CODE_FILE"
# account: 1 (change source); menu: 1 openai + key, 2 anthropic + key, Enter (done)
OUT=$(printf '1\n1\nsk-OA\n2\nsk-ANT\n\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
has '"openai": "sk-OA"' "$(cat "$BCFG")" "menu set openai key"
has '"anthropic": "sk-ANT"' "$(cat "$BCFG")" "menu set anthropic key (both coexist)"
has 'model_provider = "byok-local"' "$(cat "$CFG")" "menu flips config to byok-local"
has "Current:" "$OUT" "menu shows the Current per-provider state line"

echo "== source menu: remove one key, the other remains =="
OUT=$(printf '1\n4\nopenai\n\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
hasnt '"openai"' "$(cat "$BCFG")" "menu removed openai key"
has '"anthropic": "sk-ANT"' "$(cat "$BCFG")" "anthropic key retained"

echo "== source menu: option 6 back to fully managed (confirms when keys exist) =="
# (Option 5 is 'Use your own hosted model'; 'Use ByteAsk managed' is 6. With a key
# saved, 6 now asks 'Type y to confirm' before wiping — feed the y.)
OUT=$(printf '1\n6\ny\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
case "$OUT" in *"Type y to confirm"*) ok "managed switch asks for confirmation (keys present)";;
  *) bad "managed switch missing the confirm prompt";; esac
has 'model_provider = "byteask"' "$(cat "$CFG")" "menu option 6 restores managed (after y)"
has 'experimental_bearer_token = "JWT-ORIGINAL"' "$(cat "$CFG")" "JWT restored to managed config"

echo "== option 6 confirm: anything-but-y cancels, keys kept =="
seed_signed; rm -f "$BCFG"; echo 200 > "$CODE_FILE"
# account:1 -> add openai key -> pick managed(6) -> 'n' cancels -> Done(7)
OUT=$(printf '1\n1\nsk-KEEP\n6\nn\n7\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
case "$OUT" in *"(cancelled)"*) ok "confirm 'n' cancels the wipe";;
  *) bad "confirm cancel path missing";; esac
has '"openai": "sk-KEEP"' "$(cat "$BCFG")" "key survives a cancelled managed switch"

echo "== [BUG] option 6 'Use ByteAsk managed' EXITS the menu, no loop =="
# NO trailing input: with the loop bug (missing `return`) the menu re-renders after
# byok_off and waits for another key, so 'Set up your API keys' would print TWICE. The fix
# makes managed a terminal choice (act + return), so the header prints exactly ONCE.
seed_signed; rm -f "$BCFG"; echo 200 > "$CODE_FILE"
OUT=$(printf '1\n6\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
_menus=$(printf '%s\n' "$OUT" | grep -c "Set up your API keys")
[ "$_menus" = 1 ] && ok "option 6 exits after one render (no loop)" \
  || bad "option 6 LOOPED: menu rendered $_menus times (expected 1)"
has 'model_provider = "byteask"' "$(cat "$CFG")" "option 6 still restores managed"

echo "== source menu: option 5 shows the self-hosted how-to (keeps menu open) =="
seed_signed; rm -f "$BCFG"
OUT=$(printf '1\n5\n\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1)
case "$OUT" in *"Use your own hosted model"*) ok "option 5 shows the self-hosted how-to" ;;
  *) bad "option 5 missing the self-hosted how-to" ;; esac
_m5=$(printf '%s\n' "$OUT" | grep -c "Set up your API keys")
[ "$_m5" -ge 2 ] && ok "self-hosted how-to keeps the menu open (re-renders)" \
  || bad "self-hosted how-to did not loop (rendered $_m5)"

echo "== signed-in login shows the two-choice (change source / different email) =="
seed_signed; rm -f "$BCFG"
OUT=$(printf '\n\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1 || true)
has "Change your key or plan" "$OUT" "signed-in login shows the two-choice prompt"
has "Sign in with a different email" "$OUT" "two-choice offers a different email"

echo "== non-interactive login is a no-op for a signed-in user (stays managed) =="
seed_signed; rm -f "$BCFG"
OUT=$(printf '1\nsk-X\n' | byteask login 2>&1 || true)   # NO BYOK_ASSUME_TTY -> not interactive
has 'model_provider = "byteask"' "$(cat "$CFG")" "non-interactive signed-in login stays managed"
[ ! -f "$BCFG" ] && ok "non-interactive: no key written" || bad "non-interactive wrote a key file"

echo "== byok status discloses the managed fallback =="
printf '{"keys":{"openai":"sk-OA2"},"jwt":"JWT-ORIGINAL","local_token":"t"}' > "$BCFG"
OUT=$(byteask byok status 2>&1)
has "counts toward your usage" "$OUT" "status discloses managed fallback for un-keyed providers"

# ---- arrow-key navigation (real tty via a `script` pty) ---------------------
if command -v script >/dev/null 2>&1; then
  echo "== arrow keys: Down+Enter selects option 2 (different email) =="
  seed_signed; rm -f "$BCFG"
  OUT=$(printf '\033[B\r' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='https://gw.test' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask' login" /dev/null 2>&1 || true)
  has "Email:" "$OUT" "Down+Enter picks 'different email' (arrow navigation works)"

  echo "== arrow keys: Enter selects the default (change source -> source menu) =="
  seed_signed; rm -f "$BCFG"
  OUT=$(printf '\rq' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='https://gw.test' BYTEASK_NO_UPDATE_CHECK=1 '$BIN/byteask' login" /dev/null 2>&1 || true)
  has   "Set up your API keys" "$OUT" "Enter picks 'change source' -> source menu"
  hasnt "Email:"           "$OUT" "Enter on default does NOT trigger email re-auth"
else printf '  SKIP arrow tests (no pty/script)\n'; fi

echo "== account menu shows the signed-in email (decoded from JWT) =="
_JWT="$(python3 -c 'import base64,json;print("h."+base64.urlsafe_b64encode(json.dumps({"sub":"demo@x.com"}).encode()).decode().rstrip("=")+".s")')"
cat > "$CFG" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
experimental_bearer_token = "$_JWT"
EOF
rm -f "$BCFG"
OUT=$(printf '\n\n' | BYOK_ASSUME_TTY=1 byteask login 2>&1 || true)
has "Signed in as demo@x.com" "$OUT" "account menu shows the decoded email"

# ---- AUTH / LOGOUT RCA regression tests (logout must clear EVERY auth store) ----
_mkjwt() { python3 -c "import base64,json,time,sys;p=base64.urlsafe_b64encode(json.dumps({'sub':'u@x.com','exp':int(time.time())+int(sys.argv[1])}).encode()).decode().rstrip('=');print('h.'+p+'.s')" "$1"; }
seed_byok_local() {  # $1=jwt (empty = no byok-config)
  cat > "$CFG" <<EOF
model = "gpt-5.4"
model_provider = "byok-local"
[model_providers.byok-local]
base_url = "http://127.0.0.1:8799/byteask/v1"
[model_providers.byok-local.http_headers]
X-BYOK-Token = "tok"
EOF
  if [ -n "$1" ]; then printf '{"keys":{"openai":"sk-X","gemini":"g"},"jwt":"%s","local_token":"tok"}' "$1" > "$BCFG"; else rm -f "$BCFG"; fi
}

echo "== [BUG] logout from BYOK-local clears keys+JWT, stops sidecar, resets config =="
seed_byok_local "$(_mkjwt 99999)"
sleep 120 & _SC=$!; echo "$_SC" > "$HOME_BX/byok-sidecar.pid"
OUT=$(byteask logout 2>&1)
has "Logged out" "$OUT" "logout reports success from BYOK mode (was: 'not signed in')"
[ ! -f "$BCFG" ] && ok "byok-config.json (keys+JWT) removed" || bad "byok-config.json NOT removed"
has   'model_provider = "byteask"' "$(cat "$CFG")" "config reset to managed provider"
hasnt 'experimental_bearer_token'  "$(cat "$CFG")" "no token line -> unsigned"
if kill -0 "$_SC" 2>/dev/null; then bad "sidecar STILL running after logout"; kill "$_SC" 2>/dev/null; else ok "sidecar stopped on logout"; fi
[ ! -f "$HOME_BX/byok-sidecar.pid" ] && ok "sidecar pidfile removed" || bad "pidfile left behind"

echo "== [BUG] after BYOK logout, launch is BLOCKED until re-login =="
OUT=$(byteask </dev/null 2>&1 || true)
has "not signed in" "$OUT" "logged-out user forced to sign in (no unauthenticated launch)"

echo "== leftover byok-local config with NO JWT -> forced sign-in (launch-check bug) =="
seed_byok_local ""
OUT=$(byteask </dev/null 2>&1 || true)
has "not signed in" "$OUT" "byok-local + no JWT detected as signed-out"

echo "== expired JWT is treated as signed-out =="
cat > "$CFG" <<EOF
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
experimental_bearer_token = "$(_mkjwt -100)"
EOF
rm -f "$BCFG"
OUT=$(byteask </dev/null 2>&1 || true)
has "not signed in" "$OUT" "expired token -> onboard, not a stale 401 session"

echo "== managed logout still works + double-logout is graceful (regression) =="
seed_signed
OUT=$(byteask logout 2>&1)
has  "Logged out" "$OUT" "managed logout reports success"
hasnt 'experimental_bearer_token' "$(cat "$CFG")" "managed token removed"
OUT=$(byteask logout 2>&1)
has "not signed in" "$OUT" "second logout is graceful (already logged out)"

echo "== telemetry opt-out: the sidecar is started (or restarted once) with BYTEASK_NO_TELEMETRY =="
# ensure_sidecar on its own, with the real curl: a web server stands in for /healthz
# and a fake sidecar records the BYTEASK_NO_TELEMETRY it was started with.
SCT="$TMP/sct"; mkdir -p "$SCT/web" "$SCT/home"; : > "$SCT/web/healthz"
SCPORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
( cd "$SCT/web" && exec python3 -m http.server "$SCPORT" --bind 127.0.0.1 >/dev/null 2>&1 ) & SCSRV=$!
cat > "$SCT/sidecar.py" <<'SC_EOF'
import os
open(os.environ["SC_LOG"], "a").write("started NO_TELEMETRY=%s\n" % os.environ.get("BYTEASK_NO_TELEMETRY", ""))
SC_EOF
touch -d '2020-01-01' "$SCT/sidecar.py"
for f in _no_telemetry ensure_sidecar; do awk "/^$f\\(\\) \\{/,/^\\}/" "$WRAP"; done > "$SCT/fns.sh"
sc_run() {  # $1 = BYTEASK_NO_TELEMETRY for this launch ('' = not set)
  ( PATH="${PATH#"$BIN:"}"; unset BYOK_SKIP_SIDECAR BYTEASK_NO_TELEMETRY
    if [ -n "$1" ]; then BYTEASK_NO_TELEMETRY="$1"; export BYTEASK_NO_TELEMETRY; fi
    BYTEASK_HOME="$SCT/home"; BYOK_PORT="$SCPORT"; BYOK_SIDECAR="$SCT/sidecar.py"
    SC_LOG="$SCT/log"; export SC_LOG
    . "$SCT/fns.sh"; ensure_sidecar )
}
sc_starts() {  # wait until the log holds $1 lines (the fake sidecar writes it in the background)
  _i=0; while [ "$(wc -l < "$SCT/log" | tr -d ' ')" -lt "$1" ] && [ "$_i" -lt 30 ]; do sleep 0.1; _i=$((_i+1)); done
  wc -l < "$SCT/log" | tr -d ' '
}
_i=0; while ! PATH="${PATH#"$BIN:"}" curl -fsS "http://127.0.0.1:$SCPORT/healthz" >/dev/null 2>&1 && [ "$_i" -lt 50 ]; do sleep 0.1; _i=$((_i+1)); done
: > "$SCT/log"
sleep 120 & _OLD=$!; echo "$_OLD" > "$SCT/home/byok-sidecar.pid"
sc_run ""
{ kill -0 "$_OLD" 2>/dev/null && [ "$(sc_starts 0)" = 0 ]; } \
  && ok "variable not set: a healthy, current sidecar is left running" || bad "the sidecar was restarted without the opt-out"
sc_run 1
[ "$(sc_starts 1)" = 1 ] && has 'NO_TELEMETRY=1' "$(cat "$SCT/log")" "opt-out set: the sidecar is restarted with the variable in its environment" \
  || bad "the sidecar was not restarted with the opt-out"
kill -0 "$_OLD" 2>/dev/null && { bad "the old sidecar is still running"; kill "$_OLD" 2>/dev/null; } || ok "the old sidecar was stopped"
[ -f "$SCT/home/byok-sidecar.notelemetry" ] && ok "the opt-out marker is written" || bad "no opt-out marker"
sc_run TRUE; sleep 0.3
[ "$(sc_starts 1)" = 1 ] && ok "opt-out already in effect: no second restart" || bad "restarted again although already opted out"
sc_run ""; sleep 0.3
[ "$(sc_starts 1)" = 1 ] && [ -f "$SCT/home/byok-sidecar.notelemetry" ] \
  && ok "variable dropped later: the opted-out sidecar is left as it is" || bad "the opted-out sidecar was restarted"
touch -d '2099-01-01' "$SCT/sidecar.py"   # an update: the sidecar file is newer than the running one
sc_run ""
[ "$(sc_starts 2)" = 2 ] && [ "$(tail -n1 "$SCT/log")" = "started NO_TELEMETRY=" ] && [ ! -f "$SCT/home/byok-sidecar.notelemetry" ] \
  && ok "a restart without the variable starts it with telemetry on and clears the marker" \
  || bad "restart without the variable: log '$(tail -n1 "$SCT/log")'"
kill "$SCSRV" 2>/dev/null

echo
echo "byok wrapper: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
