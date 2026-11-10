#!/bin/sh
# `byteask terse` is RETIRED: token efficiency is built into the engine and gateway, with
# no user setting. This verifies the retirement is clean:
#   * the command is a harmless no-op that says so, and writes nothing;
#   * at launch the wrapper removes what the old feature left behind (the persist file,
#     the x-byteask-terse line in config.toml, OUR installed terse skill) ...
#   * ... without losing the bearer token line or the file's 0600 mode, and without
#     touching a user's OWN skill that happens to be called terse.
# Self-contained, no network.   Run:  sh tests/terse-cli.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
: > "$BIN/byteask-engine"; chmod +x "$BIN/byteask-engine"
export PATH="$BIN:$PATH"
HOME_BX="$TMP/home/.byteask"; mkdir -p "$HOME_BX"
export BYTEASK_HOME="$HOME_BX" BYTEASK_NO_UPDATE_CHECK=1
CFG="$HOME_BX/config.toml"

seed() {
  cat > "$CFG" <<EOT
model = "gpt-5.4"
model_provider = "byteask"

[model_providers.byteask]
name = "ByteAsk"
experimental_bearer_token = "SECRET-TOKEN"

[model_providers.byteask.http_headers]
x-byteask-terse = "full"
x-openai-actor-authorization = "byteask"
EOT
  chmod 600 "$CFG"
  printf 'full' > "$HOME_BX/terse"
}

echo "== the command is a no-op that writes nothing =="
seed
before=$(cksum < "$CFG")
out=$(byteask terse full 2>&1); rc=$?
[ "$rc" -eq 0 ] && ok "exit 0" || bad "exit $rc"
case "$out" in *"built in"*) ok "says efficiency is built in";; *) bad "unexpected output: $out";; esac
[ "$before" = "$(cksum < "$CFG")" ] && ok "config.toml untouched by the command" || bad "the command rewrote config.toml"

echo "== launch-time cleanup (_retire_terse) =="
run_retire() { sh -c ". /dev/stdin; _retire_terse" <<EOT
$(sed -n '/^_retire_terse() {/,/^}/p' "$WRAP")
EOT
}
mkdir -p "$HOME_BX/skills/terse"
printf -- '---\nname: terse\n---\nrun: byteask terse <level>\n' > "$HOME_BX/skills/terse/SKILL.md"
run_retire
grep -q 'x-byteask-terse' "$CFG" && bad "terse header line survived" || ok "terse header line removed"
grep -q 'experimental_bearer_token = "SECRET-TOKEN"' "$CFG" && ok "bearer token line kept" || bad "bearer token LOST"
grep -q 'x-openai-actor-authorization = "byteask"' "$CFG" && ok "other headers kept" || bad "another header was lost"
[ "$(stat -c %a "$CFG" 2>/dev/null || stat -f %Lp "$CFG")" = "600" ] && ok "config.toml is still 0600" || bad "config.toml mode changed"
[ ! -e "$HOME_BX/terse" ] && ok "persist file removed" || bad "persist file survived"
[ ! -e "$HOME_BX/skills/terse" ] && ok "our installed terse skill removed" || bad "our terse skill survived"

echo "== idempotent, and a user's own 'terse' skill is left alone =="
c1=$(cksum < "$CFG"); run_retire
[ "$c1" = "$(cksum < "$CFG")" ] && ok "second run changes nothing" || bad "second run rewrote config.toml"
mkdir -p "$HOME_BX/skills/terse"; printf -- '---\nname: terse\n---\nmy own skill\n' > "$HOME_BX/skills/terse/SKILL.md"
run_retire
[ -f "$HOME_BX/skills/terse/SKILL.md" ] && ok "a user's own terse skill is untouched" || bad "deleted a USER's skill"

echo "== a config with no terse line is never rewritten =="
rm -f "$HOME_BX/terse"; i1=$(ls -i "$CFG" | cut -d' ' -f1); run_retire
[ "$i1" = "$(ls -i "$CFG" | cut -d' ' -f1)" ] && ok "same inode: file not replaced" || bad "file replaced needlessly"

echo
echo "terse-cli: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
