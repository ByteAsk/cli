# Configuration

ByteAsk reads its settings from `~/.byteask/config.toml`. Set `BYTEASK_HOME` to use a different directory.

The file format is the one Codex CLI uses, so the
[Codex configuration reference](https://github.com/openai/codex/blob/main/docs/config.md) applies for models,
sandboxing, approvals, MCP servers and profiles. Run `byteask -c key=value` to override one setting for a single
run, for example `byteask -c model_reasoning_effort=high`.

This page covers what ByteAsk adds.

## How models are reached

Choose one with `byteask login`; the launcher writes the matching section of `config.toml`.

| Mode | Set up with | Where requests go |
|---|---|---|
| ByteAsk account | `byteask login` | the ByteAsk gateway at `code.byteask.ai`, which forwards them to the model provider |
| Your own provider key | `byteask byok set` | from your machine to OpenAI, Anthropic or Gemini |
| Your own hosted model | `byteask models add <alias> --url <endpoint>` | from your machine to your endpoint |

`byteask byok status` shows the active mode. `byteask models test <alias>` checks that a hosted model answers.

## ByteAsk Context

`byteask context connect` adds ByteAsk Context, a hosted code index, as an MCP server and signs you in.
`byteask context status` and `byteask context disconnect` show or remove it.

```toml
[byteask_harness]
# Index a repository Context has not seen yet without asking first.
context_auto_index = true
```

The launcher sets this for you with `byteask context auto-index on|off`.

## Agent and C/C++ defaults

These defaults are on in every install. Set any of them to `false` in `config.toml` to turn it off.

```toml
[byteask_harness]
context_keep_prefix = true      # keep the usual tool set when Context is connected
context_pack_timeout_secs = 120 # how long to wait for Context's first answer on large repositories
context_build_markers = true    # a top-level CMakeLists.txt, meson.build or configure.ac marks a C/C++ repo
context_check_timeout_secs = 15 # how long to wait for the local check after Context's answer
finish_related = true           # cpp_finish_check also lists related places Context finds
context_lean_tools = true       # headless runs with Context leave out interactive and debugger tools

[byteask_cpp]
hide_missing_tools = true       # do not offer a C/C++ tool whose program is not installed
tree_owner = true               # one build at a time per build tree
finish_review = true            # a turn that ends with an unchecked C/C++ change gets one check
```

## Environment variables

| Variable | Effect |
|---|---|
| `BYTEASK_HOME` | Configuration and session directory (default `~/.byteask`). |
| `BYTEASK_NO_TELEMETRY=1` | Turn off the install report and the own-key usage record. See the README. |
| `BYTEASK_NO_UPDATE_CHECK=1` | Do not check for a newer version at launch. |
| `BYTEASK_FINISH_CHECK_SECS` | Time limit in seconds for `cpp_finish_check`. |
