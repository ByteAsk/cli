#!/bin/sh
# What install.sh asks and says about changes outside its own files:
#   * it names bubblewrap and ripgrep, shows the sudo command and asks before running it
#     (Enter = yes); with no terminal it only tries passwordless sudo (`sudo -n`);
#   * the summary says when it changed ~/.gdbinit, and only on the run that did;
#   * an own-key or ChatGPT setup survives an update (only a ByteAsk-models config is
#     rewritten);
#   * Git Bash / MSYS gets the PowerShell command instead of only "use WSL".
# Same harness as install-fetch.sh: a local web root serves a fake engine, and a PATH
# mirror stubs sudo and the package managers so nothing touches the host.
#
# Run:  sh tests/install-prompts.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
INSTALL="$HERE/../install.sh"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')"; printf '       got: %s\n' "$2";; *) ok "$3";; esac; }

for _need in python3 gzip curl script setsid; do
  command -v "$_need" >/dev/null 2>&1 || { echo "SKIP install-prompts: '$_need' not installed"; exit 0; }
done
[ "$(id -u)" = 0 ] && { echo "SKIP install-prompts: running as root (no sudo involved)"; exit 0; }
[ "$(uname -s)" = Linux ] || { echo "SKIP install-prompts: Linux only (bubblewrap)"; exit 0; }

TMP=$(mktemp -d); SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

WEB="$TMP/web"; mkdir -p "$WEB"
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
( cd "$WEB" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
URL="http://127.0.0.1:$PORT"
ASSET="byteask-engine-linux-$(uname -m | sed 's/aarch64/arm64/;s/amd64/x86_64/')"
# _valid_engine wants >= 20 MB and an ELF magic.
head -c 21000000 /dev/zero > "$TMP/raw"
printf '\177ELF' | dd of="$TMP/raw" conv=notrunc bs=1 count=4 2>/dev/null
gzip -1c "$TMP/raw" > "$WEB/$ASSET.gz"
printf '#!/bin/sh\necho byteask-test\n' > "$WEB/byteask"
for _i in 1 2 3 4 5 6 7 8 9 10; do curl -fsS "$URL/byteask" >/dev/null 2>&1 && break; sleep 0.3; done

# A PATH with bubblewrap, ripgrep, sudo and every package manager removed, then stubs:
# `sudo` and `apt-get` only record how they were called.
mkpath() {  # $1 = dest dir
  rm -rf "$1"; mkdir -p "$1"
  ( IFS=:
    for d in $PATH; do
      [ -d "$d" ] || continue
      for f in "$d"/*; do
        b=${f##*/}
        case "$b" in bwrap|rg|sudo|apt-get|dnf|yum|pacman|zypper|apk|brew|zstd|unzstd|xz|unxz) continue;; esac
        [ -e "$1/$b" ] || ln -s "$f" "$1/$b" 2>/dev/null
      done
    done )
  printf '#!/bin/sh\necho "$*" >> "%s/sudo.log"\nexit 0\n' "$TMP" > "$1/sudo"
  printf '#!/bin/sh\necho "$*" >> "%s/apt.log"\nexit 0\n' "$TMP" > "$1/apt-get"
  chmod +x "$1/sudo" "$1/apt-get"
}
P_MISSING="$TMP/p-missing"; mkpath "$P_MISSING"
P_HAVE="$TMP/p-have"; mkpath "$P_HAVE"
for s in bwrap rg; do printf '#!/bin/sh\nexit 0\n' > "$P_HAVE/$s"; chmod +x "$P_HAVE/$s"; done

BIN="$TMP/bin"; H="$TMP/home"
envs() { printf "env -i PATH='%s' HOME='%s' PREFIX='%s' BYTEASK_HOME='%s/.byteask' ENGINE_URL='%s' BUNDLE_URL='%s' GATEWAY_URL='%s' BYTEASK_NO_TELEMETRY=1" "$1" "$H" "$BIN" "$H" "$URL" "$URL" "$URL"; }
fresh() { rm -rf "$BIN" "$H" "$TMP/sudo.log" "$TMP/apt.log"; mkdir -p "$BIN" "$H"; }
# On a terminal, answering $2 to any question.
tty_install() {   # $1 = PATH, $2 = answer line
  printf '%s\n' "$2" | timeout 120 script -qec "$(envs "$1") sh '$INSTALL'" /dev/null 2>&1 || true
}
# With no controlling terminal at all (CI, Docker, `curl | sh` from a cron job).
notty_install() {   # $1 = PATH
  eval "$(envs "$1")" setsid -w sh "'$INSTALL'" </dev/null 2>&1 || true
}
sudo_calls() { cat "$TMP/sudo.log" 2>/dev/null; }

echo "== A. a missing package: it asks before sudo, and 'n' means no sudo =="
fresh; OUT=$(tty_install "$P_MISSING" n)
has   "ByteAsk uses bubblewrap (a sandbox for the commands it runs) and ripgrep (fast code search)." "$OUT" "A1 names the packages and why"
has   "This needs: sudo apt-get install -y bubblewrap ripgrep" "$OUT" "A2 shows the exact command"
has   "Run it now with sudo? [Y/n]" "$OUT" "A3 asks first"
has   "Skipped. Nothing was installed with sudo." "$OUT" "A4 'n' is confirmed"
[ -z "$(sudo_calls)" ] && ok "A5 'n' never runs sudo" || bad "A5 sudo ran: $(sudo_calls)"
has   "bubblewrap is not installed, so ByteAsk runs commands WITHOUT its sandbox until you install it: sudo apt-get install -y bubblewrap" "$OUT" "A6 says what is off and how to fix it"
has   "Install it: sudo apt-get install -y ripgrep" "$OUT" "A7 the ripgrep note has the command too"

echo "== B. 'y' and a bare Enter run the shown command, once =="
fresh; tty_install "$P_MISSING" y >/dev/null
[ "$(sudo_calls)" = "apt-get install -y bubblewrap ripgrep" ] && ok "B1 'y' runs exactly the shown command" || bad "B1 sudo calls: $(sudo_calls)"
fresh; tty_install "$P_MISSING" "" >/dev/null
[ "$(sudo_calls)" = "apt-get install -y bubblewrap ripgrep" ] && ok "B2 Enter defaults to yes" || bad "B2 sudo calls: $(sudo_calls)"

echo "== C. no terminal: never a prompt, only passwordless sudo =="
fresh; OUT=$(notty_install "$P_MISSING")
hasnt "[Y/n]" "$OUT" "C1 no question without a terminal"
[ "$(sudo_calls)" = "-n apt-get install -y bubblewrap ripgrep" ] && ok "C2 uses sudo -n (cannot prompt)" || bad "C2 sudo calls: $(sudo_calls)"

echo "== D. nothing missing: no question, no sudo =="
fresh; OUT=$(tty_install "$P_HAVE" y)
hasnt "Run it now with sudo" "$OUT" "D1 no question"
[ -z "$(sudo_calls)" ] && ok "D2 no sudo" || bad "D2 sudo ran: $(sudo_calls)"

echo "== E. the summary says when ~/.gdbinit changed =="
fresh; OUT=$(notty_install "$P_HAVE")
has "Changed: ~/.gdbinit (adds the byteask-bridge gdb command; undo: byteask --uninstall-gdb-bridge)" "$OUT" "E1 first install names the change"
grep -q 'ByteAsk GDB bridge' "$H/.gdbinit" 2>/dev/null && ok "E2 the block is in ~/.gdbinit" || bad "E2 no block in ~/.gdbinit"
OUT=$(notty_install "$P_HAVE")
hasnt "Changed: ~/.gdbinit" "$OUT" "E3 an update that changes nothing says nothing"
[ "$(grep -c 'ByteAsk GDB bridge (added' "$H/.gdbinit")" = 1 ] && ok "E4 the block is added once" || bad "E4 the block was added twice"

echo "== G. Git Bash, MSYS2 and Cygwin get the PowerShell installer =="
mkdir -p "$TMP/p-win"
printf '#!/bin/sh\n[ "${1:-}" = -s ] && echo MINGW64_NT-10.0-26100 || echo x86_64\n' > "$TMP/p-win/uname"; chmod +x "$TMP/p-win/uname"
fresh; OUT=$(env PATH="$TMP/p-win:$P_HAVE" HOME="$H" PREFIX="$BIN" sh "$INSTALL" </dev/null 2>&1); rc=$?
[ "$rc" = 1 ] && ok "G1 stops (exit 1)" || bad "G1 exit $rc"
has   "irm https://code.byteask.ai/install.ps1 | iex" "$OUT" "G2 gives the PowerShell command"
has   "pip install byteask"                           "$OUT" "G3 mentions pip"
hasnt "unsupported OS"                                "$OUT" "G4 not a bare 'unsupported OS'"

echo
echo "install-prompts: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
