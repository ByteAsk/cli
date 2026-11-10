#!/bin/sh
# Tests for the byteask wrapper's auto-update check (check_for_update / version_gt).
# Self-contained: stubs the engine, isolates $BYTEASK_HOME, and serves a local
# /version + /install.sh so nothing touches the network or the real install.
#
# Run:  sh tests/update-check.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
WRAP="$HERE/../byteask"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()    { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt()  { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

TMP=$(mktemp -d)
SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

# --- sandbox: fake engine + isolated home + config with a token (passes the sign-in nudge) ---
BIN="$TMP/bin"; mkdir -p "$BIN"
cp "$WRAP" "$BIN/byteask"; chmod +x "$BIN/byteask"
printf '#!/bin/sh\necho "ENGINE_LAUNCHED $* CLIENTVER=${BYTEASK_CLIENT_VERSION:-none}"\n' > "$BIN/byteask-engine"
chmod +x "$BIN/byteask-engine"
HOME_BX="$TMP/home/.byteask"; mkdir -p "$HOME_BX"
cat > "$HOME_BX/config.toml" <<'EOF'
model = "gpt-5.4"
model_provider = "byteask"
[model_providers.byteask]
experimental_bearer_token = "test"
EOF

# --- local web root + server for /version and /install.sh ---
WEB="$TMP/web"; mkdir -p "$WEB"
printf 'echo UPDATER_RAN\n' > "$WEB/install.sh"
# A free port, never a fixed one: a fixed port collides with whatever already listens
# there (8799 is also the BYOK sidecar's default), and then /version is never served and
# every "silent" assertion passes on nothing.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
( cd "$WEB" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
for _i in 1 2 3 4 5 6 7 8 9 10; do curl -fsS "http://127.0.0.1:$PORT/install.sh" >/dev/null 2>&1 && break; sleep 0.3; done
GW="http://127.0.0.1:$PORT"

set_version() { printf '%s\n' "$1" > "$WEB/version"; }
reset_state()  { rm -f "$HOME_BX/update-check"; }
# run non-interactively (stdin /dev/null -> not a TTY -> notice path, deterministic)
run() { BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" "$BIN/byteask" </dev/null 2>&1; }

echo "== version_gt =="
# Define just the version_gt function (do NOT source the whole wrapper — it execs).
eval "$(sed -n '/^version_gt() {/,/^}/p' "$BIN/byteask")"
vg() { if version_gt "$1" "$2"; then echo GT; else echo no; fi; }
[ "$(vg 0.2.0 0.1.0)" = GT ] && ok "0.2.0 > 0.1.0" || bad "0.2.0 > 0.1.0"
[ "$(vg 0.1.0 0.2.0)" = no ] && ok "0.1.0 !> 0.2.0" || bad "0.1.0 !> 0.2.0"
[ "$(vg 0.10.0 0.9.0)" = GT ] && ok "0.10.0 > 0.9.0 (numeric)" || bad "0.10.0 > 0.9.0"
[ "$(vg 0.1.0 0.1.0)" = no ] && ok "equal !> equal" || bad "equal"

# Versions relative to the launcher's own, so the test does not go stale on a release.
WVER=$(sed -n 's/^VERSION="\(.*\)"/\1/p' "$BIN/byteask" | head -1)
NEW=$(echo "$WVER" | awk -F. '{print $1"."$2+1".0"}')     # newer than the launcher
NEWER=$(echo "$WVER" | awk -F. '{print $1"."$2+2".0"}')   # newer still

echo "== update-check paths =="
# 1. newer available -> notice + engine still launches
set_version "$NEW"; reset_state; OUT=$(run)
has "$NEW is available" "$OUT" "1 newer: notice shown"
has "ENGINE_LAUNCHED"    "$OUT" "1 newer: engine still launches"

# 2. same version -> silent
set_version "$WVER"; reset_state; OUT=$(run)
hasnt "is available" "$OUT" "2 same: silent"
has   "ENGINE_LAUNCHED" "$OUT" "2 same: launches"

# 3. older on server -> silent
set_version 0.0.9; reset_state; OUT=$(run)
hasnt "is available" "$OUT" "3 older: silent"

# 4. offline (dead port) -> silent, fail-open, launches
reset_state
OUT=$(BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="http://127.0.0.1:1" "$BIN/byteask" </dev/null 2>&1)
hasnt "is available" "$OUT" "4 offline: silent"
has   "ENGINE_LAUNCHED" "$OUT" "4 offline: still launches (fail-open)"

# 5. malformed /version -> ignored
printf 'not-a-version\n' > "$WEB/version"; reset_state; OUT=$(run)
hasnt "is available" "$OUT" "5 garbage: ignored"
has   "ENGINE_LAUNCHED" "$OUT" "5 garbage: launches"

# 6. cache: first run caches $NEW; bump server to $NEWER; within 1h still $NEW
set_version "$NEW"; reset_state; run >/dev/null 2>&1
set_version "$NEWER"; OUT=$(run)
has   "$NEW is available" "$OUT" "6 cache: uses cached $NEW"
hasnt "$NEWER"              "$OUT" "6 cache: did NOT re-fetch $NEWER within 1h"

# 7. re-offer: a previously-declined version is shown AGAIN (no dismiss memory)
set_version "$NEW"; reset_state
printf 'last_check=%s\nlatest=%s\n' "$(date +%s)" "$NEW" > "$HOME_BX/update-check"
OUT=$(run)
has "$NEW is available" "$OUT" "7 re-offer: declined version nudges again"
has "ENGINE_LAUNCHED"    "$OUT" "7 re-offer: launches"

# 8. opt-out env -> silent even when newer
set_version "$NEW"; reset_state
OUT=$(BYTEASK_NO_UPDATE_CHECK=1 BYTEASK_HOME="$HOME_BX" BYTEASK_GATEWAY="$GW" "$BIN/byteask" </dev/null 2>&1)
hasnt "is available" "$OUT" "8 opt-out: silent"
has   "ENGINE_LAUNCHED" "$OUT" "8 opt-out: launches"

# 9*. interactive runs need a pty; via `script`.
if command -v script >/dev/null 2>&1; then
  set_version "$NEW"; reset_state
  # 9. interactive TUI launch -> the [Y/n] prompt. "y" runs the
  #    updater, then relaunches the fresh wrapper, which launches the engine.
  OUT=$(printf 'y\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' '$BIN/byteask'" /dev/null 2>&1 || true)
  has   "Update now? [Y/n]" "$OUT" "9 interactive TUI: asks [Y/n]"
  has   "UPDATER_RAN"       "$OUT" "9 interactive TUI: y runs the updater"
  has   "ENGINE_LAUNCHED"   "$OUT" "9 interactive TUI: engine launches after the update"

  # 9-enter. Enter alone is the default (yes).
  set_version "$NEW"; reset_state
  OUT=$(printf '\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' '$BIN/byteask'" /dev/null 2>&1 || true)
  has   "UPDATER_RAN"       "$OUT" "9-enter: Enter defaults to yes"

  # 9-no. "n" continues on this version, without the updater.
  set_version "$NEW"; reset_state
  OUT=$(printf 'n\n' | script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' '$BIN/byteask'" /dev/null 2>&1 || true)
  has   "Update now? [Y/n]" "$OUT" "9-no: asks"
  hasnt "UPDATER_RAN"       "$OUT" "9-no: n does not update"
  has   "ENGINE_LAUNCHED"   "$OUT" "9-no: engine launches on the current version"

  # 9b. a terminal, but a non-TUI subcommand (exec): the engine shows no TUI notice there,
  #     so the wrapper keeps its ONE line -- and never a [Y/n] prompt.
  set_version "$NEW"; reset_state
  OUT=$(script -qec "BYTEASK_HOME='$HOME_BX' BYTEASK_GATEWAY='$GW' '$BIN/byteask' exec hi" /dev/null </dev/null 2>&1 || true)
  has   "$NEW is available" "$OUT" "9b interactive exec: one-line notice"
  hasnt "Update now"         "$OUT" "9b interactive exec: no [Y/n] prompt"
  hasnt "UPDATER_RAN"        "$OUT" "9b interactive exec: never runs the updater itself"
else
  printf '  SKIP 9 interactive (no `script` for a pty)\n'
fi

echo "== client version export =="
# 10. the wrapper exports its VERSION as BYTEASK_CLIENT_VERSION so the engine
#     header shows the released version, not the engine's own crate version.
set_version "$WVER"; reset_state; OUT=$(run)   # same version -> silent, engine runs
has "CLIENTVER=$WVER" "$OUT" "10 exports BYTEASK_CLIENT_VERSION=$WVER to engine"

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
