#!/bin/sh
# Which token the per-launch `model-access` request sends. It used to read
# byok-config.json's `jwt` first; a stale one there got 401 on every launch while the
# engine's turns (config.toml's token) worked, so the account never got its overlay and
# /model showed every model. The engine's own credential must win; byok-config.json is
# only the fallback for a BYOK setup that has no managed token.
# Self-contained, no network.   Run:  sh tests/model-access-token.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="${WRAP:-$HERE/../byteask}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME" "$TMP/bin"
echo '{"models":[{"slug":"gpt-5.4"}]}' > "$BYTEASK_HOME/models-catalog.json"

# stub curl: records the Authorization header it was given, answers the overlay.
# The wrapper hands the header over on stdin (-H @-) so the token never appears on
# the command line; a header given on argv is still recorded.
cat > "$TMP/bin/curl" <<EOT
#!/bin/sh
_out=""; _auth=""
while [ \$# -gt 0 ]; do case "\$1" in
  -o) _out="\$2"; shift 2;;
  -H) case "\$2" in
        @-) _auth="\$(grep '^Authorization:' | head -n1)";;
        Authorization:*) _auth="\$2";;
      esac; shift 2;;
  --max-time|-m) shift 2;;
  *) shift;;
esac; done
echo "\$_auth" >> "$TMP/auth.log"
echo '{"locked":[],"inactive":[]}' > "\$_out"
EOT
chmod +x "$TMP/bin/curl"; PATH="$TMP/bin:$PATH"; export PATH

# The real _managed_token and _models_access_fetch from the wrapper; only the byok lookup
# and the gateway are stubbed. BYOK_JWT is what byok-config.json would hold.
run() { BYOK_JWT="$1" sh -c '
_byok_current_jwt() { [ -n "$BYOK_JWT" ] && echo "$BYOK_JWT" && return; _managed_token; }
resolve_gateway() { echo http://gw.test; }
. /dev/stdin
_models_access_fetch' <<EOT
$(sed -n '/^_managed_token() {$/,/^}$/p' "$WRAP")
$(sed -n '/^_MA_FETCHED=0$/,/^}$/p' "$WRAP")
EOT
}
last_auth() { tail -n1 "$TMP/auth.log" 2>/dev/null; }
cfg() { printf 'model = "gpt-5.4"\n%s' "$1" > "$BYTEASK_HOME/config.toml"; }

echo "model-access token choice"
cfg 'experimental_bearer_token = "live-managed"
'
run stale-byok
[ "$(last_auth)" = "Authorization: Bearer live-managed" ] \
  && ok "managed token wins over a byok-config.json jwt" \
  || bad "sent [$(last_auth)], want the managed token"

cfg ''
run byok-only
[ "$(last_auth)" = "Authorization: Bearer byok-only" ] \
  && ok "no managed token: falls back to byok-config.json (BYOK)" \
  || bad "sent [$(last_auth)], want the byok fallback"

: > "$TMP/auth.log"; cfg ''
run ''
[ ! -s "$TMP/auth.log" ] && ok "no token anywhere: no request" || bad "requested with no token"

printf '%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
