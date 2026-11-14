#!/bin/sh
# ByteAsk CLI installer.   curl -fsSL https://code.byteask.ai/install.sh | sh
set -eu

BUNDLE_URL="${BUNDLE_URL:-https://code.byteask.ai}"
# Engine binaries are served from GitHub Releases (latest published release); code.byteask.ai
# stays a fallback so a GitHub hiccup or a not-yet-uploaded asset can't break installs.
ENGINE_URL="${ENGINE_URL:-https://github.com/ByteAsk/cli/releases/latest/download}"; ENGINE_URL="${ENGINE_URL%/}"
GATEWAY_URL="${GATEWAY_URL:-https://code.byteask.ai}"; GATEWAY_URL="${GATEWAY_URL%/}"
MODEL="${MODEL:-auto}"

# Optional referral code: `curl ... | sh -s -- --ref=CODE` (passed as a positional
# arg) or BYTEASK_REF=CODE. Persisted below so the first `byteask login` credits the
# referrer. Best-effort: an absent/invalid ref just means a normal install.
REF="${BYTEASK_REF:-}"
for _arg in "$@"; do
  case "$_arg" in
    --ref=*) REF="${_arg#--ref=}" ;;
  esac
done
REF_GIVEN="$REF"                                  # kept so a REJECTED code can be reported
case "$REF" in *[!A-Za-z0-9_-]*) REF="" ;; esac   # keep only a sane token (alnum/-/_)
[ "${#REF}" -le 64 ] || REF=""                    # ... of reasonable length

# Detect platform -> engine asset name (byteask-engine-<os>-<arch>).
os="$(uname -s 2>/dev/null || echo unknown)"
arch="$(uname -m 2>/dev/null || echo unknown)"
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=darwin ;;
  MINGW*|MSYS*|CYGWIN*)
    # Git Bash, MSYS2 and Cygwin on Windows: the native Windows build is the one to use.
    echo "[byteask] This installer is for macOS and Linux. On Windows, run this in PowerShell:" >&2
    echo "            irm https://code.byteask.ai/install.ps1 | iex" >&2
    echo "          or: pip install byteask. Inside WSL, this script works as it is." >&2
    exit 1 ;;
  *) echo "[byteask] unsupported OS: $os (Linux/macOS supported; Windows: use WSL)" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64)   arch=x86_64 ;;
  aarch64|arm64)  arch=arm64 ;;
  *) echo "[byteask] unsupported CPU arch: $arch" >&2; exit 1 ;;
esac
ASSET="byteask-engine-$os-$arch"

# Pick a writable install dir (no sudo needed).
if [ -n "${PREFIX:-}" ]; then BIN_DIR="$PREFIX"
elif [ -w /usr/local/bin ]; then BIN_DIR="/usr/local/bin"
else BIN_DIR="$HOME/.local/bin"; fi
mkdir -p "$BIN_DIR"

# A downloaded engine must be a real, whole executable — not a KB-sized proxy/block
# page, captive-portal HTML, or a truncated transfer that still returned 200. The
# engine is hundreds of MB and starts with an ELF (Linux) / Mach-O (macOS) magic, so a
# size floor + magic check reliably rejects junk that would otherwise "install" and
# then fail to run.
_valid_engine() {  # $1 = candidate file
  [ -f "$1" ] || return 1
  _sz=$(wc -c < "$1" 2>/dev/null || echo 0)
  [ "$_sz" -ge 20000000 ] 2>/dev/null || return 1            # >= ~20 MB (junk is tiny)
  case "$(od -An -tx1 -N4 "$1" 2>/dev/null | tr -d ' \n')" in
    7f454c46|cffaedfe|feedfacf|cefaedfe|cafebabe) return 0 ;; # ELF / Mach-O variants
    *) return 1 ;;
  esac
}
# sha256 of a file, using whatever the host actually has (Linux: sha256sum;
# macOS: shasum; last resort: openssl). Prints the bare hex digest, or nothing at
# all when no tool exists — callers treat empty as "cannot verify here".
# Digests are lowercased on both sides so a manifest written in upper hex can never
# read as "tampered".
_sha256() {  # $1 = file
  if   command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" 2>/dev/null | cut -d' ' -f1 | tr 'A-F' 'a-f'
  elif command -v shasum    >/dev/null 2>&1; then shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1 | tr 'A-F' 'a-f'
  elif command -v openssl   >/dev/null 2>&1; then openssl dgst -sha256 "$1" 2>/dev/null | sed 's/.*= *//' | tr 'A-F' 'a-f'
  fi
}
# Look up one asset's expected digest in a downloaded SHA256SUMS. The file is GNU
# coreutils format, "<hex>  <name>", where a leading '*' on the name marks binary
# mode. Prints nothing (and still succeeds) when the asset isn't listed, so the
# caller can distinguish "no claim was made" from "the claim was violated".
_manifest_hash() {  # $1 = manifest file, $2 = asset name
  [ -f "$1" ] || return 0
  awk -v want="$2" '{ n = $2; sub(/^\*/, "", n); if (n == want) { print tolower($1); exit } }' "$1" 2>/dev/null
  return 0
}

# ---------------------------------------------------------------------------
# The release marker.
#
# Records WHICH release asset produced the engine currently on disk: the asset
# name plus the sha256 of the COMPRESSED asset, i.e. exactly the artifact
# SHA256SUMS makes a claim about. Skip-if-current compares this marker against
# the release manifest — it deliberately does NOT hash the installed binary,
# because SHA256SUMS covers the compressed assets while the installed file is
# the decompressed one (different artifacts, different digests), and because
# re-hashing the ~324 MB engine costs ~2 s on every `byteask --update`.
# A missing/garbled marker only ever costs one redundant download.
MARKER="$BIN_DIR/.engine-release"

# ---------------------------------------------------------------------------
# The compressor ladder.
#
# Measured on the real 324 MB linux-x86_64 engine (compress once at release,
# decompress once per install):
#     gzip -9   116.7 MB   2.1 s to decompress
#     zstd -19   81.2 MB   0.6 s      <- 30% smaller than gzip AND 3.5x faster
#     xz -6      75.1 MB   4.8 s      <- smallest, but slow to unpack
# So: zstd first, xz second, gzip third, bare (uncompressed) last. Only variants
# whose decompressor is actually installed are attempted. `.gz` is published
# FOREVER so wrappers installed today keep working against future releases, and
# the bare asset stays as the final rung for hosts with no compressor at all.
VARIANTS=""
if command -v zstd   >/dev/null 2>&1 || command -v unzstd >/dev/null 2>&1; then VARIANTS="$VARIANTS zst"; fi
if command -v xz     >/dev/null 2>&1 || command -v unxz   >/dev/null 2>&1; then VARIANTS="$VARIANTS xz"; fi
if command -v gunzip >/dev/null 2>&1 || command -v gzip   >/dev/null 2>&1; then VARIANTS="$VARIANTS gz"; fi
VARIANTS="$VARIANTS bare"
case "$VARIANTS" in
  *zst*|*xz*|*gz*) ;;
  *) echo "[byteask] note: no gzip/zstd/xz found; the uncompressed engine (~3x bigger) will be fetched. Install 'gzip' to avoid that." >&2 ;;
esac
_decompress() {  # $1 = variant, $2 = downloaded file, $3 = destination
  case "$1" in
    zst)  if command -v zstd >/dev/null 2>&1; then zstd -dcq "$2" > "$3" 2>/dev/null
          else unzstd -c "$2" > "$3" 2>/dev/null; fi ;;
    xz)   if command -v xz   >/dev/null 2>&1; then xz -dc   "$2" > "$3" 2>/dev/null
          else unxz   -c "$2" > "$3" 2>/dev/null; fi ;;
    gz)   gunzip -c "$2" > "$3" 2>/dev/null ;;
    bare) mv -f "$2" "$3" ;;
  esac
}

# ---------------------------------------------------------------------------
# SHA256SUMS policy: FAIL-CLOSED on a mismatch, FAIL-OPEN on absence.
#
#  * The manifest names an asset and the digest DISAGREES  -> refuse it outright,
#    delete it, and drop to the next rung. Never install a binary the release
#    itself says is not what we asked for.
#  * The manifest is unreachable, or doesn't list this asset -> install anyway,
#    with a printed note, after the existing size+magic check.
#
# Fail-open on absence is deliberate. The manifest and the asset travel the same
# TLS connection from the same host, so an adversary who can suppress the
# manifest can already rewrite the asset — the digest's real value is catching
# corruption: truncated transfers, caching proxies, mangled CDN objects. Against
# those, absence carries no signal. Failing closed instead would brick installs
# on every flaky/filtering network, break the code.byteask.ai fallback (which
# serves no SHA256SUMS) and break every already-published release, which is a far
# larger and far likelier harm than the one it would prevent.
# Set BYTEASK_REQUIRE_SHA256=1 to make absence fatal too (enterprise/paranoid).
ENGINE_CURRENT=0      # 1 = the on-disk engine already matches the release
ENGINE_BADHASH=0      # 1 = at least one asset was refused on a digest mismatch
ENGINE_ASSET_NAME=""  # what we actually installed, ->
ENGINE_ASSET_SHA=""   #    ... and its digest; both go into the marker

_engine_is_current() {  # $1 = manifest file; 0 = on-disk engine matches the release
  if [ "${BYTEASK_FORCE_ENGINE_DOWNLOAD:-0}" = 1 ]; then return 1; fi
  [ -f "$MARKER" ] || return 1
  _valid_engine "$BIN_DIR/byteask-engine" || return 1
  _m_asset=$(sed -n 's/^asset=//p'  "$MARKER" 2>/dev/null | head -n1)
  _m_sha=$(  sed -n 's/^sha256=//p' "$MARKER" 2>/dev/null | head -n1)
  [ -n "$_m_asset" ] && [ -n "$_m_sha" ] || return 1
  _r_sha=$(_manifest_hash "$1" "$_m_asset")
  [ -n "$_r_sha" ] || return 1          # release doesn't list it -> can't claim current
  [ "$_r_sha" = "$_m_sha" ]
}

# Try one base URL, walking the ladder. curl: follow redirects, fail on HTTP
# errors, connect-timeout so a blocked/slow host fails fast, retry transient
# errors (5xx / resets / timeouts — NOT 404, so probing a variant an older
# release never published costs one quick 404). Every result is integrity-checked.
fetch_engine() {  # $1 = base url -> writes $BIN_DIR/byteask-engine.tmp ; 0 on success
  _base="$1"
  _dl="$BIN_DIR/byteask-engine.dl.tmp"
  _man="$BIN_DIR/.byteask-sha256sums.tmp"
  rm -f "$_dl" "$BIN_DIR/byteask-engine.tmp" "$_man"

  # The release manifest, fetched once per base. Absence is not fatal (see above).
  curl -fL --connect-timeout 20 --retry 2 --retry-delay 2 "$_base/SHA256SUMS" \
    -o "$_man" 2>/dev/null || rm -f "$_man"

  if _engine_is_current "$_man"; then
    ENGINE_CURRENT=1; rm -f "$_man"; return 0
  fi

  for _v in $VARIANTS; do
    if [ "$_v" = bare ]; then _name="$ASSET"; else _name="$ASSET.$_v"; fi
    if ! curl -fL --connect-timeout 20 --retry 3 --retry-delay 2 "$_base/$_name" \
         -o "$_dl" 2>/dev/null; then
      rm -f "$_dl"; continue
    fi
    _want=$(_manifest_hash "$_man" "$_name")
    _got=$(_sha256 "$_dl")
    if [ -n "$_want" ] && [ -n "$_got" ] && [ "$_want" != "$_got" ]; then
      echo "[byteask] REFUSING $_name: sha256 mismatch against the release SHA256SUMS." >&2
      echo "            expected  $_want" >&2
      echo "            got       $_got" >&2
      echo "          The download is corrupt or was tampered with; it will NOT be installed." >&2
      rm -f "$_dl"; ENGINE_BADHASH=1; continue
    fi
    if [ -z "$_want" ]; then
      if [ "${BYTEASK_REQUIRE_SHA256:-0}" = 1 ]; then
        echo "[byteask] REFUSING $_name: BYTEASK_REQUIRE_SHA256=1 and it has no SHA256SUMS entry." >&2
        rm -f "$_dl"; ENGINE_BADHASH=1; continue
      fi
      echo "[byteask] note: no SHA256SUMS entry for $_name — installing unverified (size+magic checked)." >&2
    elif [ -z "$_got" ]; then
      echo "[byteask] note: no sha256 tool on this host — installing unverified (size+magic checked)." >&2
    fi
    if _decompress "$_v" "$_dl" "$BIN_DIR/byteask-engine.tmp" \
       && _valid_engine "$BIN_DIR/byteask-engine.tmp"; then
      rm -f "$_dl" "$_man"
      ENGINE_ASSET_NAME="$_name"; ENGINE_ASSET_SHA="$_got"
      if [ -n "$_want" ] && [ -n "$_got" ]; then
        echo "[byteask] fetched $_name (sha256 verified)"
      else
        echo "[byteask] fetched $_name"
      fi
      return 0
    fi
    rm -f "$_dl" "$BIN_DIR/byteask-engine.tmp"
  done
  rm -f "$_man"
  return 1
}
echo "[byteask] downloading the ByteAsk engine ($os/$arch)…"
if fetch_engine "$ENGINE_URL"; then :
else
  echo "[byteask] primary source (GitHub Releases) unreachable/blocked; trying fallback ($BUNDLE_URL)…" >&2
  if fetch_engine "$BUNDLE_URL"; then :
  else
    echo "" >&2
    if [ "$ENGINE_BADHASH" = 1 ]; then
      echo "[byteask] Every engine download for $os/$arch failed its checksum (or was" >&2
      echo "  unverifiable while BYTEASK_REQUIRE_SHA256=1). Nothing was installed." >&2
      echo "  This usually means a caching proxy or VPN is rewriting the download." >&2
    else
      echo "[byteask] Couldn't download the engine for $os/$arch. The build IS published —" >&2
      echo "  this is a network issue on THIS machine (a proxy/firewall blocking GitHub or" >&2
      echo "  code.byteask.ai, no internet/DNS, or a VPN intercepting HTTPS)." >&2
    fi
    echo "  Tried:  $ENGINE_URL/$ASSET.{zst,xz,gz}" >&2
    echo "     and  $BUNDLE_URL/$ASSET.{zst,xz,gz}" >&2
    echo "  Retry, fix the proxy, or download one of the archives above by hand, unpack it" >&2
    echo "  to \"$BIN_DIR/byteask-engine\" (chmod +x), then re-run this installer." >&2
    exit 1
  fi
fi
if [ "$ENGINE_CURRENT" = 1 ]; then
  echo "[byteask] engine already at this release — skipped the download."
else
  chmod +x "$BIN_DIR/byteask-engine.tmp"; mv "$BIN_DIR/byteask-engine.tmp" "$BIN_DIR/byteask-engine"
  # Record what produced it, for the next update's skip-if-current. Best-effort:
  # a marker we fail to write only costs one redundant download later.
  printf 'asset=%s\nsha256=%s\n' "$ENGINE_ASSET_NAME" "$ENGINE_ASSET_SHA" > "$MARKER" 2>/dev/null || true
fi
# The launcher itself (a small POSIX sh). Harden this fetch too so a network blip here
# can't leave a half-install (engine but no wrapper); verify it's a real script, not an
# HTML block page. On failure, roll back the engine so a re-run starts clean.
if curl -fL --connect-timeout 20 --retry 3 --retry-delay 2 "$BUNDLE_URL/byteask" -o "$BIN_DIR/byteask.tmp" 2>/dev/null \
   && head -n1 "$BIN_DIR/byteask.tmp" 2>/dev/null | grep -q '^#!'; then
  chmod +x "$BIN_DIR/byteask.tmp"; mv "$BIN_DIR/byteask.tmp" "$BIN_DIR/byteask"
else
  rm -f "$BIN_DIR/byteask.tmp"
  # Roll the engine back only if THIS run installed it. When skip-if-current fired
  # the engine on disk predates this run and is still good — deleting it would turn
  # a failed launcher fetch into a broken install.
  [ "$ENGINE_CURRENT" = 1 ] || rm -f "$BIN_DIR/byteask-engine" "$MARKER"
  echo "[byteask] Couldn't download the launcher from $BUNDLE_URL/byteask (network issue). Retry, or check your proxy/VPN." >&2
  exit 1
fi

# Two OS packages make ByteAsk work well:
#   bubblewrap (`bwrap`, Linux only)  the sandbox for the shell commands the model runs
#       (macOS has a native one). This build ships no working bundled bwrap. Without it
#       the `byteask` launcher runs commands WITHOUT the sandbox (approval prompts still
#       gate them) and says so; a later install is picked up automatically.
#   ripgrep (`rg`)  fast code search. Without it search falls back to find/grep, which is
#       slower and costs the model extra turns.
# Installing them usually needs sudo, and a bare password prompt with no reason given is
# exactly what a careful user should refuse. So: name the packages and why, show the
# command, and ask (Enter = yes). Root and Homebrew need no sudo and just install. With
# no terminal to ask on (CI, Docker) only passwordless sudo is tried (`sudo -n`), so it
# can never stop at a prompt. Never fatal: a missing package is reported below.
_pkgs=""; _why=""
if [ "$os" = linux ] && ! command -v bwrap >/dev/null 2>&1; then
  _pkgs="bubblewrap"; _why="bubblewrap (a sandbox for the commands it runs)"
fi
if ! command -v rg >/dev/null 2>&1; then
  _pkgs="${_pkgs:+$_pkgs }ripgrep"; _why="${_why:+$_why and }ripgrep (fast code search)"
fi
_pm=""
if   command -v apt-get >/dev/null 2>&1; then _pm="apt-get install -y"
elif command -v dnf     >/dev/null 2>&1; then _pm="dnf install -y"
elif command -v yum     >/dev/null 2>&1; then _pm="yum install -y"
elif command -v pacman  >/dev/null 2>&1; then _pm="pacman -S --noconfirm"
elif command -v zypper  >/dev/null 2>&1; then _pm="zypper install -y"
elif command -v apk     >/dev/null 2>&1; then _pm="apk add"
elif command -v brew    >/dev/null 2>&1; then _pm="brew install"
fi
# Homebrew has no bubblewrap; on a Linux box that only has brew, install ripgrep alone.
[ "$_pm" = "brew install" ] && case " $_pkgs " in *" ripgrep "*) _pkgs="ripgrep";; *) _pkgs="";; esac
if [ -n "$_pkgs" ] && [ -n "$_pm" ]; then
  # shellcheck disable=SC2086 -- $_pm and $_pkgs are word lists built above
  if [ "$(id -u 2>/dev/null || echo 0)" = 0 ] || [ "$_pm" = "brew install" ]; then
    echo "[byteask] installing $_why…"
    $_pm $_pkgs >/dev/null 2>&1 || true
  elif ( : </dev/tty ) 2>/dev/null; then
    echo "[byteask] ByteAsk uses $_why."
    echo "[byteask] This needs: sudo $_pm $_pkgs"
    printf '[byteask] Run it now with sudo? [Y/n] '
    read -r _ans </dev/tty || _ans=n
    case "$_ans" in
      ""|[yY]*) sudo $_pm $_pkgs >/dev/null || true ;;
      *) echo "[byteask] Skipped. Nothing was installed with sudo." ;;
    esac
  else
    sudo -n $_pm $_pkgs >/dev/null 2>&1 || true
  fi
fi
case "$_pm" in
  "")             _bw_hint="your package manager's bubblewrap package"
                  _rg_hint="https://github.com/BurntSushi/ripgrep#installation" ;;
  "brew install") _bw_hint="your package manager's bubblewrap package"; _rg_hint="brew install ripgrep" ;;
  *)              _bw_hint="sudo $_pm bubblewrap"; _rg_hint="sudo $_pm ripgrep" ;;
esac
if [ "$os" = linux ] && ! command -v bwrap >/dev/null 2>&1; then
  echo "[byteask] note: bubblewrap is not installed, so ByteAsk runs commands WITHOUT its sandbox until you install it: $_bw_hint"
fi
if ! command -v rg >/dev/null 2>&1; then
  echo "[byteask] note: ripgrep is not installed, so code search falls back to find/grep (slower). Install it: $_rg_hint"
fi

# Per-platform install beacon (fire-and-forget; never blocks or fails the run).
# The engine now downloads from GitHub, so the gateway can't observe it directly —
# this ping is the per-platform install/update signal. Best-effort, ≤3s, fail-open.
# BYTEASK_NO_TELEMETRY=1 (also true/yes, any case) skips it.
case "$(printf '%s' "${BYTEASK_NO_TELEMETRY:-}" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes) ;;
  *) curl -fsS -m 3 "$BUNDLE_URL/byteask/dl/$os-$arch" >/dev/null 2>&1 || true ;;
esac

HOME_DIR="${BYTEASK_HOME:-$HOME/.byteask}"; mkdir -p "$HOME_DIR"
printf '%s' "$GATEWAY_URL" > "$HOME_DIR/gateway"
# Persist an optional referral code for the first `byteask login` (one-shot; the
# wrapper deletes it after a successful sign-in). Best-effort.
if [ -n "$REF" ]; then
  printf '%s' "$REF" > "$HOME_DIR/referral" 2>/dev/null || true
fi

# Install the gdb pair-debugging hook into ~/.gdbinit (idempotent). It only
# DEFINES the `byteask-bridge` command in every gdb; nothing is armed until the
# user runs it. Existence-guarded + fail-open so it can never break the user's gdb.
GDBINIT="$HOME/.gdbinit"
GDB_MARKER="# ===== ByteAsk GDB bridge (added by the byteask installer) ====="
GDB_CHANGED=0   # reported in the summary: a file of the user's that this run edited
if ! { [ -f "$GDBINIT" ] && grep -qF "$GDB_MARKER" "$GDBINIT" 2>/dev/null; }; then
  GDB_CHANGED=1
  cat >> "$GDBINIT" <<'GDBINIT_BLOCK'
# ===== ByteAsk GDB bridge (added by the byteask installer) =====
# Defines the `byteask-bridge` command so the `byteask` AI assistant can attach to
# THIS gdb when you ask for debugging help. Nothing is armed until you run
# `byteask-bridge`; every command the AI runs is echoed to this terminal, and it
# never arms in batch/non-interactive gdb. Engine-owned, refreshed on byteask
# startup. To disable: delete this block (or run `byteask --uninstall-gdb-bridge`).
python
import os as _o
_p = _o.path.expanduser("~/.byteask/byteask_gdb_bridge.py")
if _o.path.exists(_p):
    try:
        gdb.execute("source " + _p)
    except Exception:
        pass
end
# ===== end ByteAsk GDB bridge =====
GDBINIT_BLOCK
fi
# Carry an existing login token across updates: config.toml holds it
# (`byteask login` writes it here), and we rewrite config.toml below — so
# without this an update would silently log the user out.
PREV_TOKEN=""
if [ -f "$HOME_DIR/config.toml" ]; then
  PREV_TOKEN="$(sed -n 's/^experimental_bearer_token = "\(.*\)"$/\1/p' "$HOME_DIR/config.toml" | head -n1)"
fi
TOKEN_LINE=""
[ -n "$PREV_TOKEN" ] && TOKEN_LINE="experimental_bearer_token = \"$PREV_TOKEN\""
# Model catalog: adds Claude (opus/sonnet) to /model with correct metadata. It
# REPLACES the engine's bundled catalog, so only reference it after the download
# validates. Fail-safe: skip on any failure (Claude still routes via the gateway).
CATALOG_LINE=""
if curl -fsSL -m 20 "$GATEWAY_URL/models-catalog.json" -o "$HOME_DIR/models-catalog.json.tmp" 2>/dev/null \
   && grep -q '"models"' "$HOME_DIR/models-catalog.json.tmp" 2>/dev/null; then
  mv "$HOME_DIR/models-catalog.json.tmp" "$HOME_DIR/models-catalog.json"
  CATALOG_LINE="model_catalog_json = \"$HOME_DIR/models-catalog.json\""
else
  rm -f "$HOME_DIR/models-catalog.json.tmp" 2>/dev/null || true
fi

# BYOK sidecar + translators (own-key routing). Fetched from the gateway; fail-soft
# so a managed install is never affected if they aren't served yet (BYOK simply stays
# unavailable — `byteask byok set` tells the user to update). `byteask --update`
# re-runs this and refreshes a stale sidecar (the wrapper auto-restarts it).
# effort.py is a DEPENDENCY of the translators (they `import effort` at module
# level), so it has to land BEFORE them. The per-file fetch below is fail-soft --
# a failure leaves the previous working copy in place -- but effort.py has no
# previous copy on a first upgrade, so a translator updated without it would be an
# ImportError that kills the whole sidecar. Gate the group on it.
_EFFORT_OK=0
if curl -fsSL -m 20 "$GATEWAY_URL/effort.py" -o "$HOME_DIR/effort.py.tmp" 2>/dev/null \
   && grep -q 'def clamp_effort' "$HOME_DIR/effort.py.tmp" 2>/dev/null; then
  mv "$HOME_DIR/effort.py.tmp" "$HOME_DIR/effort.py"
  _EFFORT_OK=1
else
  rm -f "$HOME_DIR/effort.py.tmp" 2>/dev/null || true
  [ -f "$HOME_DIR/effort.py" ] && _EFFORT_OK=1   # already present from a prior run
fi
# byteask_errors.py is fetched FIRST and is part of byok_sidecar.py's release
# unit -- the sidecar imports it (guarded, so a miss degrades rather than bricks).
# NB the validation grep below must contain a symbol from EVERY file in this list
# or the file is downloaded and silently discarded on every install.
for _f in byteask_errors.py byok_sidecar.py anthropic_translate.py gemini_translate.py \
          openai_compat_translate.py byteask_models.py; do
  [ "$_EFFORT_OK" = 1 ] || break                 # missing dependency: keep the old set
  if curl -fsSL -m 20 "$GATEWAY_URL/$_f" -o "$HOME_DIR/$_f.tmp" 2>/dev/null \
     && grep -q 'def translate_request\|PROTOCOL_VERSION\|is_anthropic_model\|is_gemini_model\|SELF_BASE_INSTRUCTIONS\|TERMINAL_SSE_CODE' \
        "$HOME_DIR/$_f.tmp" 2>/dev/null; then
    mv "$HOME_DIR/$_f.tmp" "$HOME_DIR/$_f"
  else
    rm -f "$HOME_DIR/$_f.tmp" 2>/dev/null || true
  fi
done

# Engine skills: dropped into the user-skills dir (~/.byteask/skills/<name>/) that the
# engine's `/skills` menu + mention discovery read. No rebuild. Add new skills here.
for _sk in compress; do
  mkdir -p "$HOME_DIR/skills/$_sk"
  if curl -fsSL -m 20 "$GATEWAY_URL/skills/$_sk/SKILL.md" -o "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null \
     && grep -q "^name: $_sk" "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null; then
    mv "$HOME_DIR/skills/$_sk/SKILL.md.tmp" "$HOME_DIR/skills/$_sk/SKILL.md"
  else
    rm -f "$HOME_DIR/skills/$_sk/SKILL.md.tmp" 2>/dev/null || true
  fi
done

# Own key, own server and ChatGPT setups are written by the launcher itself (provider
# byok-local or openai). Rewriting them here made every update a silent switch back to
# ByteAsk's models, and for someone without an account that meant a sign-in prompt.
# Only a missing or a ByteAsk-models config is (re)written.
PREV_PROVIDER=""
if [ -f "$HOME_DIR/config.toml" ]; then
  PREV_PROVIDER="$(sed -n 's/^model_provider = "\(.*\)"$/\1/p' "$HOME_DIR/config.toml" | head -n1)"
fi
case "$PREV_PROVIDER" in
  byok-local|openai) : ;;
  *) cat > "$HOME_DIR/config.toml" <<EOF
model = "$MODEL"
model_provider = "byteask"
web_search = "live"
$CATALOG_LINE

[model_providers.byteask]
name = "ByteAsk"
base_url = "$GATEWAY_URL/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
$TOKEN_LINE

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
EOF
     ;;
esac

# Two lines on the happy path. The success text names only ByteAsk and does not
# say "New here? Sign in first: byteask login --email ...", which would
# contradict the wrapper: `byteask` signs you in on first run.
VER="$("$BIN_DIR/byteask" --version 2>/dev/null || true)"   # "byteask 0.1.11"
VER="${VER##* }"
case "$VER" in [0-9]*) ;; *) VER="" ;; esac                 # unknown -> just omit it
printf '\n  \033[1;38;2;134;174;165m✓\033[0m ByteAsk %sinstalled\n' "${VER:+$VER }"
# Say which of the user's own files this run edited (clig.dev: if you change state, say so).
if [ "$GDB_CHANGED" = 1 ]; then
  printf '  Changed: ~/.gdbinit (adds the byteask-bridge gdb command; undo: byteask --uninstall-gdb-bridge)\n'
fi
printf '  Run:  \033[1;38;2;134;174;165mbyteask\033[0m\n'
# The one failure worth two extra lines: `command not found` in the first minute.
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    case "${SHELL:-}" in
      */zsh)  _rc="~/.zshrc" ;;
      */bash) _rc="~/.bashrc" ;;
      *)      _rc="~/.profile" ;;
    esac
    printf '  Add %s to your PATH, then run: byteask\n' "$BIN_DIR"
    printf '  echo '\''export PATH="%s:$PATH"'\'' >> %s\n' "$BIN_DIR" "$_rc"
    ;;
esac
# A referral code was silently accepted and, worse, silently DISCARDED when it did
# not validate -- so a friend who mistyped it credited nobody and neither side was
# told. REF_GIVEN is the raw input; REF is what survived validation.
if [ -n "$REF" ]; then
  printf '  \033[1;38;2;134;174;165m✓\033[0m Referral code %s applied · $10 of usage after you sign in\n' "$REF"
elif [ -n "$REF_GIVEN" ]; then
  printf '  Referral code ignored · it can only contain letters, digits, - and _\n'
fi
echo
