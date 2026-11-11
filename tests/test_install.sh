#!/bin/sh
# Regression tests for install.sh: verify that fresh install creates config.toml
# and update preserves an existing config.toml (features, models, provider).
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

setup_test_env() {
  TEST_DIR="$(mktemp -d)"
  BIN_DIR="$TEST_DIR/bin"
  HOME_DIR="$TEST_DIR/home"
  mkdir -p "$BIN_DIR" "$HOME_DIR"

  # Stub rg to avoid triggering distro package manager sudo prompts
  touch "$BIN_DIR/rg" && chmod +x "$BIN_DIR/rg"

  # Stub byteask-engine & release marker to satisfy skip-if-current
  printf '\177ELF' > "$BIN_DIR/byteask-engine"
  dd if=/dev/zero bs=1M count=20 >> "$BIN_DIR/byteask-engine" 2>/dev/null
  chmod +x "$BIN_DIR/byteask-engine"
  printf 'asset=byteask-engine-linux-x86_64.zst\nsha256=b6178e6cd4ff5cb3b84aad28cb6540b76c73e624ec5dbffd03f8fa40897a4869\n' > "$BIN_DIR/.engine-release"
}

cleanup_test_env() {
  rm -rf "$TEST_DIR"
}

test_fresh_install() {
  setup_test_env
  PATH="$BIN_DIR:$PATH" PREFIX="$BIN_DIR" BYTEASK_HOME="$HOME_DIR" sh "$REPO_ROOT/install.sh" >/dev/null 2>&1

  [ -f "$HOME_DIR/config.toml" ] || { echo "FAIL: config.toml not created on fresh install" >&2; return 1; }
  grep -q '^model = "gpt-5.4"' "$HOME_DIR/config.toml" || { echo "FAIL: default model missing" >&2; return 1; }
  grep -q '^model_provider = "byteask"' "$HOME_DIR/config.toml" || { echo "FAIL: default provider missing" >&2; return 1; }
  cleanup_test_env
  echo "test_fresh_install: PASS"
}

test_update_preserves_config() {
  setup_test_env
  cat > "$HOME_DIR/config.toml" <<'EOF'
model = "claude-3-7-sonnet"
model_provider = "byok-local"
web_search = "live"

[features]
plugins = true
commit_attribution = false

[model_providers.byok-local]
name = "ByteAsk (your key)"
base_url = "http://127.0.0.1:8799/byteask/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[model_providers.byok-local.http_headers]
X-BYOK-Token = "secret123"
x-openai-actor-authorization = "byteask"
EOF

  PATH="$BIN_DIR:$PATH" PREFIX="$BIN_DIR" BYTEASK_HOME="$HOME_DIR" sh "$REPO_ROOT/install.sh" >/dev/null 2>&1

  grep -q '^model = "claude-3-7-sonnet"' "$HOME_DIR/config.toml" \
    || { echo "FAIL: custom model was overwritten on update" >&2; return 1; }
  grep -q '^model_provider = "byok-local"' "$HOME_DIR/config.toml" \
    || { echo "FAIL: model_provider was clobbered on update" >&2; return 1; }
  grep -q '\[features\]' "$HOME_DIR/config.toml" \
    || { echo "FAIL: [features] table was wiped on update" >&2; return 1; }
  grep -q '^plugins = true' "$HOME_DIR/config.toml" \
    || { echo "FAIL: features.plugins was wiped on update" >&2; return 1; }
  grep -q '^commit_attribution = false' "$HOME_DIR/config.toml" \
    || { echo "FAIL: features.commit_attribution was wiped on update" >&2; return 1; }
  grep -q '^X-BYOK-Token = "secret123"' "$HOME_DIR/config.toml" \
    || { echo "FAIL: byok-local headers were wiped on update" >&2; return 1; }

  cleanup_test_env
  echo "test_update_preserves_config: PASS"
}

test_update_adds_catalog_if_missing() {
  setup_test_env
  cat > "$HOME_DIR/config.toml" <<'EOF'
model = "gpt-5.4"
model_provider = "byteask"

[model_providers.byteask]
name = "ByteAsk"
EOF

  PATH="$BIN_DIR:$PATH" PREFIX="$BIN_DIR" BYTEASK_HOME="$HOME_DIR" sh "$REPO_ROOT/install.sh" >/dev/null 2>&1

  grep -q '^model_catalog_json = ' "$HOME_DIR/config.toml" \
    || { echo "FAIL: model_catalog_json was not added to config lacking it" >&2; return 1; }

  cleanup_test_env
  echo "test_update_adds_catalog_if_missing: PASS"
}

test_fresh_install
test_update_preserves_config
test_update_adds_catalog_if_missing
echo "All tests passed."
