#!/bin/sh
# Tests for install.sh's fetch_engine: the release marker (skip-if-current),
# SHA256SUMS verification, and the .zst/.xz/.gz/bare compressor ladder.
#
# fetch_engine had ZERO coverage before this file — update-check.sh replaces
# install.sh with a stub, so it tests the update *trigger* and never the download.
#
# Self-contained, two harness patterns reused from the existing suites:
#   * a local web root + python http.server (as in update-check.sh) serves the
#     engine variants, SHA256SUMS, the wrapper and a deliberately corrupt asset;
#   * a mirror-PATH with binaries deliberately excluded (as in onboard.sh) forces
#     the ladder down a rung without uninstalling zstd or xz from the host.
# The server's access log is the ground truth for "did it download?" — every
# assertion about which rung was chosen reads the actual GET lines.
#
# Run:  sh tests/install-fetch.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
INSTALL="$HERE/../install.sh"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$1"; }
has()   { case "$2" in *"$1"*) ok "$3";; *) bad "$3 (missing '$1')"; printf '       got: %s\n' "$2";; esac; }
hasnt() { case "$2" in *"$1"*) bad "$3 (unexpected '$1')";; *) ok "$3";; esac; }

for _need in python3 gzip zstd xz sha256sum curl; do
  command -v "$_need" >/dev/null 2>&1 || { echo "SKIP: '$_need' not installed"; exit 0; }
done

TMP=$(mktemp -d); SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

WEB="$TMP/web"; mkdir -p "$WEB"
LOG="$TMP/http.log"
# A free port and `exec`: a fixed port collides with a server orphaned by an earlier run
# (a `( cd && python3 ) &` subshell leaves python alive when cleanup kills the subshell),
# and then every fetch assertion fails on a server that is not ours.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
( cd "$WEB" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >"$LOG" 2>&1 ) &
SRV_PID=$!
sleep 1
URL="http://127.0.0.1:$PORT"
ASSET="byteask-engine-$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/darwin/')-$(uname -m | sed 's/aarch64/arm64/;s/amd64/x86_64/')"

# --- fake release assets ----------------------------------------------------
# _valid_engine demands >= 20 MB and an ELF/Mach-O magic, so the fixture has to
# be a real 20 MB file with a real magic. Zeros compress to a few KB, so all
# three variants are produced in well under a second.
make_engine() {  # $1 = filler byte tag (changes the bytes -> changes every digest)
  head -c 21000000 /dev/zero > "$TMP/raw"
  printf '\177ELF' | dd of="$TMP/raw" conv=notrunc bs=1 count=4 2>/dev/null
  printf '%s' "$1" | dd of="$TMP/raw" conv=notrunc bs=1 seek=1000 2>/dev/null
}
publish() {  # $* = variants to serve (zst xz gz bare); SHA256SUMS covers the compressed ones
  rm -f "$WEB/$ASSET" "$WEB/$ASSET".* "$WEB/SHA256SUMS"
  for _v in "$@"; do
    case "$_v" in
      gz)   gzip -9c  "$TMP/raw" > "$WEB/$ASSET.gz" ;;
      zst)  zstd -19 -qc "$TMP/raw" > "$WEB/$ASSET.zst" ;;
      xz)   xz -6c    "$TMP/raw" > "$WEB/$ASSET.xz" ;;
      bare) cp "$TMP/raw" "$WEB/$ASSET" ;;
    esac
  done
  ( cd "$WEB" && ls "$ASSET".gz "$ASSET".zst "$ASSET".xz 2>/dev/null | sort | xargs -r sha256sum > SHA256SUMS )
}
printf '#!/bin/sh\necho byteask-test\n' > "$WEB/byteask"      # the launcher install.sh fetches (fatal if missing)

# --- mirror-PATH: a PATH with chosen binaries subtracted --------------------
# `command -v` reads PATH and PATH cannot be subtracted from, so build a dir of
# symlinks to every executable EXCEPT the excluded ones. Stubs for rg/bwrap/sudo
# keep install.sh's best-effort package-manager blocks from touching the host.
mkpath() {  # $1 = dest dir, rest = basenames to exclude
  _dest="$1"; shift
  rm -rf "$_dest"; mkdir -p "$_dest"
  ( IFS=:
    for d in $PATH; do
      [ -d "$d" ] || continue
      for f in "$d"/*; do
        b=${f##*/}
        for x in "$@"; do [ "$b" = "$x" ] && continue 2; done
        [ -e "$_dest/$b" ] || ln -s "$f" "$_dest/$b" 2>/dev/null
      done
    done )
  # rm first: these are symlinked from the real PATH above, and writing through a
  # symlink would try to overwrite /usr/bin/sudo.
  for s in rg bwrap sudo; do
    rm -f "$_dest/$s"; printf '#!/bin/sh\nexit 0\n' > "$_dest/$s"; chmod +x "$_dest/$s"
  done
}
PATH_ALL="$TMP/p-all"; mkpath "$PATH_ALL"
PATH_NOZSTD="$TMP/p-nozstd"; mkpath "$PATH_NOZSTD" zstd unzstd
PATH_GZONLY="$TMP/p-gzonly"; mkpath "$PATH_GZONLY" zstd unzstd xz unxz
PATH_NOCOMP="$TMP/p-nocomp"; mkpath "$PATH_NOCOMP" zstd unzstd xz unxz gzip gunzip

BIN="$TMP/bin"; mkdir -p "$BIN"
MARKER="$BIN/.engine-release"

log_mark() { wc -l < "$LOG" | tr -d ' '; }
# GET lines the server saw since $1, asset requests only (200 or 404).
gets_since() { tail -n +"$(( $1 + 1 ))" "$LOG" | sed -n 's/.*"GET \([^ ]*\) HTTP.*/\1/p'; }

run_install() {  # $1 = PATH to run under; rest = extra env assignments
  _p="$1"; shift
  env -i PATH="$_p" HOME="$TMP/home" \
    PREFIX="$BIN" BYTEASK_HOME="$TMP/home/.byteask" \
    ENGINE_URL="$URL" BUNDLE_URL="$URL" GATEWAY_URL="$URL" \
    "$@" sh "$INSTALL" 2>&1
}

echo "== A. first install: downloads, verifies, writes the marker =="
make_engine A; publish zst xz gz bare
rm -rf "$BIN" "$TMP/home"; mkdir -p "$BIN"
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
has "sha256 verified"  "$OUT" "A1 verifies the download against SHA256SUMS"
[ -x "$BIN/byteask-engine" ] && ok "A2 engine installed + executable" || bad "A2 engine missing"
[ -f "$MARKER" ] && ok "A3 release marker written" || bad "A3 no marker"
grep -q "^asset=$ASSET.zst$" "$MARKER" 2>/dev/null && ok "A4 marker records the asset name" || { bad "A4 marker asset"; cat "$MARKER"; }
_want=$(awk -v a="$ASSET.zst" '$2==a{print $1}' "$WEB/SHA256SUMS")
grep -q "^sha256=$_want$" "$MARKER" 2>/dev/null && ok "A5 marker records the COMPRESSED asset digest" || bad "A5 marker digest"
case "$G" in *SHA256SUMS*) ok "A6 fetched the release manifest";; *) bad "A6 no SHA256SUMS fetch";; esac

echo "== B. skip-if-current: marker matches the release -> no engine download =="
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
has "already at this release" "$OUT" "B1 says it skipped"
hasnt "sha256 verified"       "$OUT" "B2 nothing was fetched to verify"
case "$G" in *"$ASSET".zst*|*"$ASSET".gz*|*"$ASSET".xz*) bad "B3 engine WAS re-downloaded: $G";; *) ok "B3 no engine asset requested (server log)";; esac
case "$G" in *SHA256SUMS*) ok "B4 only the small manifest was fetched";; *) bad "B4 manifest not fetched";; esac
[ -x "$BIN/byteask-engine" ] && ok "B5 engine still in place" || bad "B5 engine lost"

echo "== C. marker mismatch (new engine in the release) -> downloads again =="
make_engine C; publish zst xz gz bare
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
hasnt "already at this release" "$OUT" "C1 does not skip a changed release"
has   "sha256 verified"         "$OUT" "C2 re-downloaded and verified"
case "$G" in *"$ASSET".zst*) ok "C3 fetched the engine again";; *) bad "C3 no engine fetch: $G";; esac
_want=$(awk -v a="$ASSET.zst" '$2==a{print $1}' "$WEB/SHA256SUMS")
grep -q "^sha256=$_want$" "$MARKER" && ok "C4 marker updated to the new digest" || bad "C4 stale marker"

echo "== D. BYTEASK_FORCE_ENGINE_DOWNLOAD=1 overrides skip-if-current =="
M=$(log_mark); OUT=$(run_install "$PATH_ALL" BYTEASK_FORCE_ENGINE_DOWNLOAD=1); G=$(gets_since "$M")
hasnt "already at this release" "$OUT" "D1 forced re-download ignores the marker"
case "$G" in *"$ASSET".zst*) ok "D2 engine re-fetched on demand";; *) bad "D2 no fetch: $G";; esac

echo "== E. compressor ladder: best available variant wins =="
make_engine E; publish zst xz gz bare
rm -rf "$BIN"; mkdir -p "$BIN"
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
has "fetched $ASSET.zst" "$OUT" "E1 zstd host picks .zst (smallest+fastest)"
case "$G" in *"$ASSET".xz*|*"$ASSET".gz*) bad "E2 fetched a worse rung too: $G";; *) ok "E2 never touched .xz/.gz";; esac

rm -rf "$BIN"; mkdir -p "$BIN"
M=$(log_mark); OUT=$(run_install "$PATH_NOZSTD"); G=$(gets_since "$M")
has "fetched $ASSET.xz" "$OUT" "E3 no zstd -> falls to .xz"
case "$G" in *"$ASSET".zst*) bad "E4 requested .zst without a decompressor";; *) ok "E4 skips .zst when zstd is absent";; esac

rm -rf "$BIN"; mkdir -p "$BIN"
OUT=$(run_install "$PATH_GZONLY")
has "fetched $ASSET.gz" "$OUT" "E5 no zstd/xz -> falls to .gz"
[ -x "$BIN/byteask-engine" ] && ok "E6 gz install produced a working engine" || bad "E6 no engine"

rm -rf "$BIN"; mkdir -p "$BIN"
OUT=$(run_install "$PATH_NOCOMP")
has "no gzip/zstd/xz found" "$OUT" "E7 warns when no decompressor exists"
has "fetched $ASSET"        "$OUT" "E8 falls back to the bare binary (D3d)"
[ -x "$BIN/byteask-engine" ] && ok "E9 bare fallback still installs" || bad "E9 no engine"

echo "== F. back-compat: a release that publishes ONLY .gz (as today) =="
make_engine F; publish gz bare
rm -rf "$BIN"; mkdir -p "$BIN"
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
has "fetched $ASSET.gz" "$OUT" "F1 zstd host installs from a gz-only release"
[ -x "$BIN/byteask-engine" ] && ok "F2 engine installed" || bad "F2 no engine"
case "$G" in *"$ASSET".zst*) ok "F3 probed .zst first (404, then fell through)";; *) bad "F3 ladder did not probe";; esac
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
has "already at this release" "$OUT" "F4 skip-if-current works on a gz-only release"

echo "== G. checksum mismatch -> refuses that asset =="
make_engine G; publish zst xz gz
# Corrupt ONLY .zst, leaving its SHA256SUMS line describing the honest bytes.
zstd -19 -qc "$TMP/raw" > "$WEB/$ASSET.zst"; printf 'TAMPERED' >> "$WEB/$ASSET.zst"
rm -rf "$BIN"; mkdir -p "$BIN"
OUT=$(run_install "$PATH_ALL")
has "REFUSING $ASSET.zst" "$OUT" "G1 names the refused asset"
has "sha256 mismatch"     "$OUT" "G2 says why"
has "expected"            "$OUT" "G3 prints both digests"
has "fetched $ASSET.xz"   "$OUT" "G4 falls to the next rung, which verifies clean"
[ -x "$BIN/byteask-engine" ] && ok "G5 a good rung still installs" || bad "G5 no engine"
grep -q "^asset=$ASSET.xz$" "$MARKER" && ok "G6 marker records the rung actually used" || bad "G6 wrong marker"

echo "== H. every variant corrupt -> nothing is installed =="
make_engine H; publish zst xz gz
for _v in zst xz gz; do printf 'TAMPERED' >> "$WEB/$ASSET.$_v"; done
rm -rf "$BIN" ; mkdir -p "$BIN"
OUT=$(run_install "$PATH_ALL"); RC=$?
[ "$RC" != 0 ] && ok "H1 installer exits non-zero" || bad "H1 exit 0 despite bad hashes"
has "failed its checksum" "$OUT" "H2 error message explains the checksum failure"
[ -e "$BIN/byteask-engine" ] && bad "H3 installed a bad binary" || ok "H3 no engine installed"
[ -e "$MARKER" ] && bad "H4 wrote a marker for a failed install" || ok "H4 no marker written"

echo "== I. SHA256SUMS unreachable -> FAIL-OPEN (documented policy) =="
make_engine I; publish zst xz gz bare; rm -f "$WEB/SHA256SUMS"
rm -rf "$BIN"; mkdir -p "$BIN"
OUT=$(run_install "$PATH_ALL")
has "no SHA256SUMS entry" "$OUT" "I1 says the download is unverified"
has "fetched $ASSET.zst"  "$OUT" "I2 installs anyway (fail-open)"
[ -x "$BIN/byteask-engine" ] && ok "I3 install completes without a manifest" || bad "I3 no engine"
M=$(log_mark); OUT=$(run_install "$PATH_ALL"); G=$(gets_since "$M")
hasnt "already at this release" "$OUT" "I4 cannot claim current without a manifest -> re-downloads"

echo "== J. BYTEASK_REQUIRE_SHA256=1 -> FAIL-CLOSED opt-in =="
rm -rf "$BIN"; mkdir -p "$BIN"
OUT=$(run_install "$PATH_ALL" BYTEASK_REQUIRE_SHA256=1); RC=$?
[ "$RC" != 0 ] && ok "J1 refuses to install without a manifest" || bad "J1 installed unverified"
has "BYTEASK_REQUIRE_SHA256=1" "$OUT" "J2 names the flag that caused the refusal"
[ -e "$BIN/byteask-engine" ] && bad "J3 installed anyway" || ok "J3 nothing installed"

echo "== K. a skipped engine survives a failed launcher fetch =="
make_engine K; publish zst gz
rm -rf "$BIN"; mkdir -p "$BIN"
run_install "$PATH_ALL" >/dev/null 2>&1
mv "$WEB/byteask" "$WEB/byteask.hidden"                       # launcher now 404s
OUT=$(run_install "$PATH_ALL"); RC=$?
mv "$WEB/byteask.hidden" "$WEB/byteask"
has "already at this release" "$OUT" "K1 skipped the engine download"
[ "$RC" != 0 ] && ok "K2 still fails on the missing launcher" || bad "K2 masked a launcher failure"
[ -x "$BIN/byteask-engine" ] && ok "K3 pre-existing engine NOT rolled back" || bad "K3 deleted a good engine"

echo "== L. install ping: sent by default, skipped with BYTEASK_NO_TELEMETRY =="
make_engine L; publish zst gz
rm -rf "$BIN"; mkdir -p "$BIN"
M=$(log_mark); run_install "$PATH_ALL" >/dev/null 2>&1; G=$(gets_since "$M")
case "$G" in */byteask/dl/*) ok "L1 default: the install ping is sent";; *) bad "L1 no install ping by default: $G";; esac
_n=2
for _v in 1 true TRUE Yes; do
  M=$(log_mark); OUT=$(run_install "$PATH_ALL" BYTEASK_NO_TELEMETRY="$_v"); RC=$?; G=$(gets_since "$M")
  case "$G" in */byteask/dl/*) bad "L$_n BYTEASK_NO_TELEMETRY=$_v still pinged: $G";; *) ok "L$_n BYTEASK_NO_TELEMETRY=$_v: no install ping";; esac
  _n=$((_n+1))
done
[ "$RC" = 0 ] && ok "L6 the install itself still completes" || bad "L6 install failed with the opt-out set"
case "$G" in *SHA256SUMS*) ok "L7 the other fetches are unchanged";; *) bad "L7 manifest not fetched: $G";; esac
M=$(log_mark); run_install "$PATH_ALL" BYTEASK_NO_TELEMETRY=0 >/dev/null 2>&1; G=$(gets_since "$M")
case "$G" in */byteask/dl/*) ok "L8 BYTEASK_NO_TELEMETRY=0 does not opt out";; *) bad "L8 '0' was treated as an opt-out";; esac

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
