#!/bin/sh
# ByteAsk CLI installer.   curl -fsSL https://code.byteask.ai/install.sh | sh
set -eu

BUNDLE_URL="${BUNDLE_URL:-https://code.byteask.ai}"
# Engine binaries are served from GitHub Releases (latest published release); code.byteask.ai
# stays a fallback so a GitHub hiccup or a not-yet-uploaded asset can't break installs.
ENGINE_URL="${ENGINE_URL:-https://github.com/ByteAsk/cli/releases/latest/download}"; ENGINE_URL="${ENGINE_URL%/}"
GATEWAY_URL="${GATEWAY_URL:-https://code.byteask.ai}"; GATEWAY_URL="${GATEWAY_URL%/}"
MODEL="${MODEL:-gpt-5.4}"

# Optional referral code: `curl ... | sh -s -- --ref=CODE` (passed as a positional
# arg) or BYTEASK_REF=CODE. Persisted below so the first `byteask login` credits the
# referrer. Best-effort: an absent/invalid ref just means a normal install.
REF="${BYTEASK_REF:-}"
for _arg in "$@"; do
  case "$_arg" in
    --ref=*) REF="${_arg#--ref=}" ;;
  esac
done
case "$REF" in *[!A-Za-z0-9_-]*) REF="" ;; esac   # keep only a sane token (alnum/-/_)
[ "${#REF}" -le 64 ] || REF=""                    # ... of reasonable length

# Detect platform -> engine asset name (byteask-engine-<os>-<arch>).
os="$(uname -s 2>/dev/null || echo unknown)"
arch="$(uname -m 2>/dev/null || echo unknown)"
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=darwin ;;
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

echo "[byteask] downloading ByteAsk CLI ($os/$arch)…"
# Try GitHub Releases first (primary), then code.byteask.ai (fallback). For each
# base: <asset>.gz (gunzip) then the bare <asset>.
fetch_engine() {  # $1 = base url -> writes $BIN_DIR/byteask-engine.tmp ; 0 on success
  if curl -fsSL "$1/$ASSET.gz" -o "$BIN_DIR/byteask-engine.gz.tmp" 2>/dev/null; then
    if gunzip -c "$BIN_DIR/byteask-engine.gz.tmp" > "$BIN_DIR/byteask-engine.tmp" 2>/dev/null; then
      rm -f "$BIN_DIR/byteask-engine.gz.tmp"; return 0
    fi
  fi
  rm -f "$BIN_DIR/byteask-engine.gz.tmp"
  if curl -fsSL "$1/$ASSET" -o "$BIN_DIR/byteask-engine.tmp" 2>/dev/null; then return 0; fi
  rm -f "$BIN_DIR/byteask-engine.tmp"
  return 1
}
if fetch_engine "$ENGINE_URL" || fetch_engine "$BUNDLE_URL"; then :; else
  echo "" >&2
  echo "  No ByteAsk build for $os/$arch yet." >&2
  echo "  Supported: Linux x86_64/arm64, macOS arm64, Windows x86_64." >&2
  exit 1
fi
chmod +x "$BIN_DIR/byteask-engine.tmp"; mv "$BIN_DIR/byteask-engine.tmp" "$BIN_DIR/byteask-engine"
curl -fsSL "$BUNDLE_URL/byteask" -o "$BIN_DIR/byteask.tmp"
chmod +x "$BIN_DIR/byteask.tmp"; mv "$BIN_DIR/byteask.tmp" "$BIN_DIR/byteask"

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
if ! { [ -f "$GDBINIT" ] && grep -qF "$GDB_MARKER" "$GDBINIT" 2>/dev/null; }; then
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
cat > "$HOME_DIR/config.toml" <<EOF
model = "$MODEL"
model_provider = "byteask"
web_search = "live"

[model_providers.byteask]
name = "ByteAsk"
base_url = "$GATEWAY_URL/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
$TOKEN_LINE

[model_providers.byteask.http_headers]
x-openai-actor-authorization = "byteask"
EOF

VER="$("$BIN_DIR/byteask" --version 2>/dev/null || echo byteask)"
NEED_PATH=0; case ":$PATH:" in *":$BIN_DIR:"*) ;; *) NEED_PATH=1 ;; esac
printf '\n  \033[1;38;2;134;174;165m✓\033[0m %s installed  →  %s\n\n' "$VER" "$BIN_DIR/byteask"
echo "  To start, just run:"
printf '\n      \033[1;38;2;134;174;165mbyteask\033[0m\n\n'
echo "  and you're in interactive mode — like  claude  or  codex."
echo
if [ "$NEED_PATH" = 1 ]; then
  printf '  First add it to your PATH:  export PATH="%s:$PATH"\n' "$BIN_DIR"
  echo "  (append that to ~/.bashrc or ~/.zshrc so it sticks)"
  echo
fi
echo "  New here? Sign in first:  byteask login --email you@company.com"
echo
