<h1 align="center">ByteAsk CLI</h1>

<p align="center">An AI coding agent for your terminal, with tools for C and C++ work: build, debug, sanitize and profile.</p>

<p align="center">
  <a href="https://github.com/ByteAsk/cli/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/ByteAsk/cli"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-blue"></a>
</p>

ByteAsk CLI is a fork of [OpenAI Codex CLI](https://github.com/openai/codex). It keeps Codex's terminal agent and
adds native tools for C and C++ projects, a hosted model gateway with sign-in by email, support for your own
provider keys and self-hosted models, and an optional connection to ByteAsk Context, a hosted code index.
ByteAsk is not affiliated with, sponsored by, or endorsed by OpenAI. See [UPSTREAM.md](UPSTREAM.md).

## Install

**macOS and Linux**

```sh
curl -fsSL https://code.byteask.ai/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://code.byteask.ai/install.ps1 | iex
```

**pip**

```sh
pip install byteask
```

The installers download the engine for your platform from this repository's
[releases](https://github.com/ByteAsk/cli/releases/latest) and check it against the published `SHA256SUMS`.
Supported platforms: Linux x86_64 and arm64, macOS on Apple silicon, Windows x86_64.
Release binaries up to v0.2.4 were built before the source was published; this repository holds that source,
cleaned for publication. To build from source, see [docs/byteask/building.md](docs/byteask/building.md).

## Quick start

```sh
cd your-project
byteask
```

On first run ByteAsk asks how you want to run models:

- **ByteAsk account:** sign in with your email address; models run through the ByteAsk gateway.
- **Your own provider key:** OpenAI, Anthropic or Gemini; requests go from your machine to the provider.
- **Your own hosted model:** any OpenAI-compatible endpoint, for example vLLM or Ollama.

Then describe the task. ByteAsk reads the code, runs commands in a sandbox, and shows each change for approval.

## Usage

```text
byteask                           start an interactive session
byteask "<prompt>"                start with a prompt
byteask exec "<prompt>"           one answer, no UI, for scripts and CI
byteask review                    review the working tree
byteask resume | fork             continue an earlier session
byteask login | logout            sign in or out
byteask byok <set|status|remove>  use your own provider key
byteask models <add|test|list>    use your own hosted model
byteask context connect           connect ByteAsk Context
byteask mcp <add|list|remove>     manage MCP servers
byteask doctor                    check the install
byteask --update                  update ByteAsk
```

Common options: `-m <model>`, `--effort <low|medium|high|xhigh>`, `-C <dir>`. Run `byteask --help` for the full
list, and see [docs/byteask/usage.md](docs/byteask/usage.md) for examples.

## C and C++ tools

When ByteAsk detects a C or C++ project it adds tools the agent can call on your code, for example:

- build and test with the project's own build system, and select the tests a change affects;
- run clang-tidy and cppcheck, and read the compilation database;
- run AddressSanitizer, UndefinedBehaviorSanitizer and ThreadSanitizer builds;
- debug with gdb or lldb, including core files, and join a gdb session you already have open;
- record and replay with rr on Linux;
- profile with perf and run benchmarks with noise checks;
- show struct layout and padding, symbolize stack traces, and look up compiler output on Compiler Explorer.

Each tool reports the command it ran and what it found.

## Configuration

Settings live in `~/.byteask/config.toml`. The format follows Codex CLI's configuration, with ByteAsk's
additions documented in [docs/byteask/configuration.md](docs/byteask/configuration.md).

## What ByteAsk sends

- **Signed in with a ByteAsk account:** your requests, including the code context the agent sends, go to the
  ByteAsk gateway, which forwards them to the model provider.
- **Your own key or your own hosted model:** requests go from your machine to that provider. ByteAsk receives a
  usage record after each turn (provider, model and token counts; never the key or the prompt).
- **Install:** the installer reports the platform it installed for.
- **Update check:** the launcher asks `code.byteask.ai` for the latest version.

The launcher in this repository honours `BYTEASK_NO_TELEMETRY=1`, which turns off the install report and the usage
record; it reaches installed copies with the next release. The [privacy policy](https://byteask.ai/privacy)
covers what the hosted service does with requests.

## Relationship to Codex CLI

The engine under `codex-rs/` is Codex CLI's Rust workspace. ByteAsk's own code lives mostly under
`codex-rs/ext/`, plus changes to upstream files, all listed in [UPSTREAM.md](UPSTREAM.md). The launcher
(`byteask`, `byteask.ps1`) and installers (`install.sh`, `install.ps1`) are ByteAsk's. We merge upstream stable
releases regularly. The directories `codex-cli/`, `sdk/` and the upstream `docs/` are kept so merges stay small;
nothing is published from them.

## Contributing

Bug reports and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; everyone taking part
follows the [Code of Conduct](CODE_OF_CONDUCT.md). Report security problems privately as described in
[SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party components are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
