# Building ByteAsk CLI from source

ByteAsk CLI has two parts:

- the **engine**, a Rust workspace in `codex-rs/` (forked from OpenAI Codex CLI), built into one binary;
- the **launcher**, the shell script `byteask` (macOS and Linux) or `byteask.ps1` (Windows), which signs you in,
  sets up configuration and starts the engine.

## Requirements

- [rustup](https://rustup.rs). The toolchain version is pinned in `codex-rs/rust-toolchain.toml` and is installed
  automatically on the first build.
- Python 3.10 or newer (used by the build helper below).
- A C and C++ compiler. On Linux, use clang; some dependencies do not build with older gcc versions.
- Disk space: a full release build needs about 40 GB for the `target/` directory.
- Memory: linking the engine is memory-hungry. On machines with less than 16 GB of RAM, limit parallel jobs
  with `CARGO_BUILD_JOBS=2`.

## 1. Fetch the prebuilt V8 library

The engine embeds V8 built with pointer compression and the V8 sandbox. No prebuilt archive for that profile is
published by the `v8` crate, so the build uses the archives that OpenAI Codex publishes on its own releases. A
helper downloads and verifies them and prints the two environment variables the build needs:

```sh
# from the repository root; pass the target you are building for
eval "$(python3 scripts/fetch_rusty_v8.py --target x86_64-unknown-linux-gnu)"
```

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`,
`x86_64-pc-windows-msvc`. If the helper fails, it prints `false`, so the `eval` fails too instead of continuing
without the variables. Set `BYTEASK_RUSTY_V8_CACHE` to choose where the archives are cached.

## 2. Build the engine

```sh
cd codex-rs
cargo build --release -p codex-cli
```

The binary is `codex-rs/target/release/codex-byteask` (`codex-byteask.exe` on Windows).

## 3. Run it with the launcher

The launcher expects the engine next to it, named `byteask-engine`:

```sh
mkdir -p ~/.local/share/byteask-dev
cp byteask ~/.local/share/byteask-dev/
cp codex-rs/target/release/codex-byteask ~/.local/share/byteask-dev/byteask-engine
~/.local/share/byteask-dev/byteask --version
```

Your existing configuration in `~/.byteask/` is used. Set `BYTEASK_HOME` to a different directory to keep a
development build separate.

## Running tests

Run the tests of the crate you changed rather than the whole workspace:

```sh
cd codex-rs
cargo test -p codex-cpp-tools
```

The engine's terminal UI uses snapshot tests. When a change alters rendered output, review and accept new
snapshots with [cargo-insta](https://insta.rs):

```sh
cargo insta test -p codex-tui
cargo insta review
```

Launcher and installer tests are shell scripts in `tests/`. Run one with `sh tests/<name>.sh`.

## Formatting and lints

```sh
cd codex-rs
cargo fmt
cargo clippy -p <crate> --tests
```
