#!/bin/sh
# Catalog refresh on launch. The base catalog used to be
# fetched only at sign-in, so a changed budget or a ROLLBACK reached nobody who stayed
# signed in. The per-launch `model-access` request now carries `catalog_version`; when it
# differs from the local one the wrapper re-fetches the static catalog. It must NEVER make
# a launch worse: any failure keeps the local catalog byte-for-byte.
# Self-contained, no network.   Run:  sh tests/catalog-refresh.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="${WRAP:-$HERE/../byteask}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export BYTEASK_HOME="$TMP/home"; mkdir -p "$BYTEASK_HOME" "$TMP/bin" "$TMP/srv"
CAT="$BYTEASK_HOME/models-catalog.json"

# stub curl: serves $TMP/srv/access.json and $TMP/srv/catalog.json; `down` file = fail.
# The static catalog answers a conditional GET like nginx: 304 when If-None-Match equals
# $TMP/srv/etag, else 200 with an ETag header (written to the -D file).
cat > "$TMP/bin/curl" <<EOT
#!/bin/sh
[ -e "$TMP/srv/down" ] && exit 7
_out=""; _url=""; _hdr=""; _fmt=""; _inm=""
while [ \$# -gt 0 ]; do case "\$1" in
  -o) _out="\$2"; shift 2;;
  -D) _hdr="\$2"; shift 2;;
  -w) _fmt="\$2"; shift 2;;
  -H) case "\$2" in If-None-Match:*) _inm="\${2#If-None-Match: }";; esac; shift 2;;
  --max-time|-m|--connect-timeout) shift 2;;
  -*) shift;;
  *) _url="\$1"; shift;;
esac; done
echo "\$_url inm=\$_inm" >> "$TMP/srv/requests.log"
case "\$_url" in
  */byteask/v1/model-access) cp "$TMP/srv/access.json" "\$_out";;
  */models-catalog.json)
    _tag=\$(cat "$TMP/srv/etag" 2>/dev/null)
    if [ -n "\$_inm" ] && [ "\$_inm" = "\$_tag" ]; then
      : > "\$_out"; [ -n "\$_hdr" ] && printf 'HTTP/1.1 304 Not Modified\r\n\r\n' > "\$_hdr"; _code=304
    else
      cp "$TMP/srv/catalog.json" "\$_out"
      [ -n "\$_hdr" ] && printf 'HTTP/1.1 200 OK\r\nETag: %s\r\n\r\n' "\$_tag" > "\$_hdr"; _code=200
    fi
    if [ -n "\$_fmt" ]; then printf '%s' "\$_code"; fi;;
  *) exit 22;;
esac
EOT
chmod +x "$TMP/bin/curl"; PATH="$TMP/bin:$PATH"; export PATH

run() { sh -c '
_byok_current_jwt() { echo tok; }
resolve_gateway() { echo http://gw.test; }
. /dev/stdin
_models_access_fetch; _catalog_refresh' <<EOT
$(sed -n '/^_MA_FETCHED=0$/,/^_models_access_merge() {$/p' "$WRAP" | sed '$d')
EOT
}
nreq() { n=$(grep -c "$1" "$TMP/srv/requests.log" 2>/dev/null); echo "${n:-0}"; }

echo '{"models":[{"slug":"old"}]}' > "$CAT"
echo '{"models":[{"slug":"new"}],"byteask_efficiency":{"policy_version":"p2"}}' > "$TMP/srv/catalog.json"
echo '{"locked":[],"inactive":[],"catalog_version":"aaaaaaaaaaaa"}' > "$TMP/srv/access.json"

echo "== a new version is fetched and recorded =="
run
grep -q '"new"' "$CAT" && ok "catalog replaced" || bad "catalog not replaced"
grep -q 'byteask_efficiency' "$CAT" && ok "the policy object arrived with it" || bad "policy object missing"
[ "$(cat "$BYTEASK_HOME/.catalog-version")" = "aaaaaaaaaaaa" ] && ok "version recorded" || bad "version not recorded"

echo "== the same version costs no catalog download =="
: > "$TMP/srv/requests.log"; run
[ "$(nreq models-catalog.json)" = "0" ] && ok "no catalog request" || bad "re-downloaded an unchanged catalog"
[ "$(nreq model-access)" = "1" ] && ok "exactly one request per launch" || bad "unexpected request count"

echo "== a ROLLBACK reaches a signed-in client =="
echo '{"models":[{"slug":"rolled-back"}]}' > "$TMP/srv/catalog.json"
echo '{"locked":[],"inactive":[],"catalog_version":"bbbbbbbbbbbb"}' > "$TMP/srv/access.json"
run
grep -q 'rolled-back' "$CAT" && ok "rolled-back catalog in place" || bad "rollback did not land"

echo "== failures keep the local catalog byte-for-byte =="
keep=$(cksum < "$CAT")
echo '{"locked":[],"inactive":[],"catalog_version":"cccccccccccc"}' > "$TMP/srv/access.json"
echo 'THIS IS NOT JSON' > "$TMP/srv/catalog.json"; run
[ "$keep" = "$(cksum < "$CAT")" ] && ok "a corrupt download is discarded" || bad "a corrupt catalog was installed"
[ "$(cat "$BYTEASK_HOME/.catalog-version")" = "bbbbbbbbbbbb" ] && ok "version NOT advanced, so it retries next launch" || bad "version advanced past a failed fetch"
echo '{"models":[]}' > "$TMP/srv/catalog.json"; run
[ "$keep" = "$(cksum < "$CAT")" ] && ok "an empty model list is discarded" || bad "an empty catalog was installed"
touch "$TMP/srv/down"; run
[ "$keep" = "$(cksum < "$CAT")" ] && ok "server down: untouched, launch proceeds" || bad "server down changed the catalog"
rm -f "$TMP/srv/down"
echo '{"locked":[],"inactive":[]}' > "$TMP/srv/access.json"; run
[ "$keep" = "$(cksum < "$CAT")" ] && ok "an OLD gateway (no catalog_version) changes nothing" || bad "old gateway broke the catalog"
ls "$BYTEASK_HOME" | grep -q 'refresh\.' && bad "left a temp file behind" || ok "no temp files left"


# ---- a launch with NO ByteAsk sign-in (own key or own model): a conditional GET on the ETag.
# A served change, a kill switch included, must reach
# these launches too.
run_unsigned() { sh -c '
_byok_current_jwt() { echo "${JWT:-}"; }
resolve_gateway() { echo http://gw.test; }
. /dev/stdin
_catalog_refresh_unsigned' <<EOT
$(sed -n '/^_MA_FETCHED=0$/,/^_models_access_merge() {$/p' "$WRAP" | sed '$d')
EOT
}
TAGF="$BYTEASK_HOME/.catalog-etag"
echo '{"models":[{"slug":"unsigned-old"}]}' > "$CAT"; rm -f "$TAGF"
echo '{"models":[{"slug":"unsigned-new"}],"byteask_attribution":{"enabled":true}}' > "$TMP/srv/catalog.json"
printf '"6ab8dac0-e1490"' > "$TMP/srv/etag"

echo "== unsigned: a signed-in launch never takes this path =="
: > "$TMP/srv/requests.log"; JWT=tok run_unsigned
[ "$(nreq models-catalog.json)" = "0" ] && ok "no request when signed in" || bad "the unsigned path ran for a signed-in launch"

echo "== unsigned: the first launch fetches and remembers the ETag =="
out=$(run_unsigned 2>&1)
[ -z "$out" ] && ok "silent" || bad "printed: $out"
grep -q 'unsigned-new' "$CAT" && ok "catalog replaced" || bad "catalog not replaced"
grep -q '"enabled":true' "$CAT" && ok "the attribution switch arrived (on)" || bad "attribution switch missing"
[ "$(cat "$TAGF" 2>/dev/null)" = '"6ab8dac0-e1490"' ] && ok "ETag stored" || bad "ETag not stored: $(cat "$TAGF" 2>/dev/null)"

echo "== unsigned: an unchanged catalog costs a 304 and no download =="
keep=$(cksum < "$CAT"); : > "$TMP/srv/requests.log"; run_unsigned
grep -q 'inm="6ab8dac0-e1490"' "$TMP/srv/requests.log" && ok "sent If-None-Match" || bad "no If-None-Match sent"
[ "$keep" = "$(cksum < "$CAT")" ] && ok "catalog untouched on 304" || bad "a 304 changed the catalog"

echo "== unsigned: a served kill switch (enabled:false) reaches the launch =="
echo '{"models":[{"slug":"unsigned-new"}],"byteask_attribution":{"enabled":false}}' > "$TMP/srv/catalog.json"
printf '"6ab8dac0-ffff"' > "$TMP/srv/etag"; run_unsigned
grep -q '"enabled":false' "$CAT" && ok "switch turned off locally" || bad "the off switch did not land"
[ "$(cat "$TAGF")" = '"6ab8dac0-ffff"' ] && ok "new ETag stored" || bad "ETag not advanced"

echo "== unsigned: failures keep the local catalog byte-for-byte =="
keep=$(cksum < "$CAT")
echo 'THIS IS NOT JSON' > "$TMP/srv/catalog.json"; printf '"x1"' > "$TMP/srv/etag"; run_unsigned
[ "$keep" = "$(cksum < "$CAT")" ] && ok "a corrupt download is discarded" || bad "a corrupt catalog was installed"
[ "$(cat "$TAGF")" = '"6ab8dac0-ffff"' ] && ok "ETag NOT advanced, so it retries next launch" || bad "ETag advanced past a failed fetch"
echo '{"models":[]}' > "$TMP/srv/catalog.json"; run_unsigned
[ "$keep" = "$(cksum < "$CAT")" ] && ok "an empty model list is discarded" || bad "an empty catalog was installed"
touch "$TMP/srv/down"; out=$(run_unsigned 2>&1)
{ [ "$keep" = "$(cksum < "$CAT")" ] && [ -z "$out" ]; } && ok "server down: untouched and silent" || bad "server down changed the catalog or printed"
rm -f "$TMP/srv/down"

echo "== unsigned: an ETag that is not header-safe is never stored =="
echo '{"models":[{"slug":"odd"}]}' > "$TMP/srv/catalog.json"; printf 'bad tag; x' > "$TMP/srv/etag"; run_unsigned
grep -q '"odd"' "$CAT" && ok "catalog still refreshed" || bad "catalog not refreshed"
[ -e "$TAGF" ] && bad "stored an unsafe ETag: $(cat "$TAGF")" || ok "unsafe ETag dropped"
ls -a "$BYTEASK_HOME" | grep -q 'refresh\.\|headers\.' && bad "left a temp file behind" || ok "no temp or header files left"
echo
echo "catalog-refresh: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
