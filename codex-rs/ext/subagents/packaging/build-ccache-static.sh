#!/bin/sh
# Build the ccache binary shipped in the ByteAsk package as <package>/bin/ccache
# (e.g. /opt/byteask/bin/ccache). The engine's sub-agent builds (reproduction-test writer) find it
# next to the engine executable (see ext/subagents/src/ccache.rs).
#
# Why ccache 3.7.12: plain C99 + autotools, so it links fully statically against musl and runs on
# any x86-64 Linux image (checked: Ubuntu 16.04 / glibc 2.23, Alpine/musl) with no shared-library
# dependency. It supports everything the engine uses: CCACHE_DIR, CCACHE_BASEDIR, CCACHE_NOHASHDIR,
# CCACHE_SLOPPINESS, masquerade symlinks, and use as a CMake compiler launcher.
#
# Requirements on the build host: curl, tar, make, musl-gcc (Debian/Ubuntu: apt install musl-tools).
# Usage: build-ccache-static.sh OUT_DIR      -> OUT_DIR/bin/ccache (+ SHA256SUMS)
set -eu
OUT=${1:?usage: build-ccache-static.sh OUT_DIR}
VER=3.7.12
SHA=a02f4e8360dc6618bc494ca35b0ae21cea080f804a4898eab1ad3fcd108eb400
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
curl -sSL -o ccache.tar.xz "https://github.com/ccache/ccache/releases/download/v$VER/ccache-$VER.tar.xz"
echo "$SHA  ccache.tar.xz" | sha256sum -c -
tar xf ccache.tar.xz
cd "ccache-$VER"
./configure CC=musl-gcc CFLAGS=-O2 LDFLAGS=-static --with-bundled-zlib >/dev/null
make -j2 >/dev/null
strip ccache
file ccache | grep -q "statically linked" || { echo "not static" >&2; exit 1; }
mkdir -p "$OUT/bin"
# The binary must be called exactly "ccache" (masquerade links resolve through its name).
cp ccache "$OUT/bin/ccache"
chmod 755 "$OUT/bin/ccache"
(cd "$OUT" && sha256sum bin/ccache > SHA256SUMS)
"$OUT/bin/ccache" --version | head -1
