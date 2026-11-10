# Changelog

All notable changes to ByteAsk CLI are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the version is 0.y.z, anything may change
between releases.

## [Unreleased]

### Changed

- Merged upstream OpenAI Codex CLI `rust-v0.167.0` (from `rust-v0.153.4`). Upstream changes include a
  background clipboard worker, reworked context updates and new terminal tips; ByteAsk's tools, sign-in and
  panes are carried over. Tips that pointed to OpenAI's forum, Discord and desktop app are not shown.

### Added

- Source code published under the Apache License 2.0.
- `BYTEASK_NO_TELEMETRY=1` turns off the install report and the own-key usage record.

### Removed

- Anonymous use without signing in is removed; model calls need a ByteAsk account, your own provider key or your
  own hosted model.

## [0.2.4] - 2026-11-10

The macOS build of this release still uses the 0.2.3 engine; the changes below reach macOS in a later release.

### Added

- With ByteAsk Context connected (`byteask context connect`), ByteAsk follows a short set of working rules:
  reproduce the problem first, complete the change across related places (Context's `find_related_changes` and
  `get_symbol` list candidates), keep the repository's own helpers and names, and check the change with
  `cpp_finish_check` before finishing. Your own `developer_instructions` replace them.
- `cpp_finish_check` lists the places Context says may need the same change, right after its verdict.
- When a turn ends with a C/C++ change nobody checked, ByteAsk runs the check once and shows the result.
- The environment note states the memory limit and a build job count that fits it.

### Changed

- The recommended harness settings are on by default for new and updated installs; each can be changed in
  `~/.byteask/config.toml`.
- Context's code pack waits up to 2 minutes on large repositories, and a top-level `CMakeLists.txt`,
  `meson.build` or `configure.ac` marks a repository as C/C++.
- Headless runs (`byteask exec`) offer a smaller tool set; interactive sessions keep every tool.
- `cpp_finish_check` builds and runs the nearest tests' programs first and names the other tests the change
  reaches. A change that no compiled file reaches is reported as not checked, never as passed.
- C/C++ tools whose program is not installed are not offered.
- ByteAsk's background builds run at idle priority, give way to your builds in the same tree, and stop first
  under memory pressure.
- On a long-running command without a terminal, ByteAsk waits 30 s before the first check and up to 3 minutes
  between checks; every wait ends as soon as the command exits.

## [0.2.3] - 2026-11-03

Versions 0.2.1 and 0.2.2 were skipped so that every release channel carries the same version.

### Added

- With ByteAsk Context connected in a CMake project, ByteAsk finds the tests that cover the place Context points
  to, starts building them in the background, and gives the agent one command that builds and runs only those
  tests. This applies in full-access sessions.

### Changed

- On the Free plan, with your own provider key saved and BYOK active, that provider's Pro models are selectable in
  `/model` instead of showing "Available on ByteAsk Pro". Requests on your own key go directly to the provider.

## [0.2.0] - 2026-11-02

### Added

- Ultra plan on `/upgrade`, next to Pro and Max. A Max subscriber can renew Max or move up to Ultra from the same
  screen.
- `cpp_finish_check`: before saying a change works, the agent can build the affected code and run the project's
  tests within 90 seconds, and report PASS, FAIL or NOT RUN with the reason. It never deletes files and never
  sends the agent back on its own.
- Debugging tools `debug_core` (diagnose a core dump), `debug_reverse` (step backwards through an rr recording,
  Linux) and `cpp_kdb` (inspect a stopped Linux kernel over KGDB/KDB, Linux), plus `cpp_impact`,
  `cpp_test_select` and `cpp_repo_history`.
- `byteask context connect` adds ByteAsk Context (the hosted, build-aware code index) as an MCP server and signs
  you in; `byteask context disconnect` removes it.
- `byteask toolbox` brings ByteAsk's C/C++ and reference tools to Claude Code and Codex; `byteask policy` shows or resets the local policy file (`~/.byteask/policy.toml`).

### Changed

- Rarely used tools load on demand, so each request carries a smaller fixed prompt; interactive tools are always
  loaded. The always-on C/C++ guidance is now a short set of rules.
- A long wait on a running command ends as soon as you send a message.
- With full permission (approval policy `never` and no sandbox), ByteAsk no longer stops to ask: C/C++ tools run
  without consent prompts and the agent works around changes it did not make.
- `cpp_compile_db` never writes into your project, and `cpp_format` only formats the lines you changed, only
  where the project has a `.clang-format`.
- More accurate `cpp_check`, `cpp_tidy` and `cpp_sanitize` (MemorySanitizer mode, allocation and free stacks in
  sanitizer reports, builds sized to the machine's memory).
- `/model` shows locked rows for models your plan cannot use; `/upgrade` plan rows show each plan's own limits;
  `/usage` shows when extra credit expires.

### Security

- On Linux the C/C++ tools run in their own sandbox; where none can start, they run without one and say so.

## [0.1.16] - 2026-10-09

### Fixed

- `/model` reliably shows what your plan includes: models you cannot use are marked Pro with a lock, and retired
  models are hidden.
- With your own Anthropic key, Claude Fable 5.1 and Claude Haiku 5.5 work, and Claude Haiku 4.5 no longer fails on
  effort settings.

## [0.1.15] - 2026-09-27

### Added

- `debug_session` runs on lldb, so interactive debugging works on Apple Silicon Macs. lldb is the default on macOS.
- Pass `sysroot` to `debug` or `debug_session` to open a crash dump from another machine with that machine's
  libraries.
- After a crash, ByteAsk runs the post-mortem commands first and tells you what failed and where.

### Changed

- `/model` is three short steps: provider, then model, then reasoning effort.
- When your free messages run out, the sign-in prompt opens by itself; type your email, no `/login` needed.
- The first-run setup menu lists *Use ByteAsk managed (recommended)* first, already selected.
- An interactive launch asks *Update now? [Y/n]* again when a new version is out, and the model list refreshes on
  every launch, including before you sign in.
- Conversation titles and catch-up summaries use a small, fast model.
- `cpp_perf`, `cpp_bench`, `cpp_sanitize` and `cpp_layout` give more precise results.

### Fixed

- Program arguments reach your program exactly as written, including empty and quoted ones.
- C/C++ tools no longer report a run as clean when it never ran, was cut short or was only partly instrumented; no
  C/C++ tool can end or hang your turn; a misnamed argument is refused with a clear message.
- Settings set with `byteask features` survive signing in, signing out and switching between ByteAsk, your own
  keys and a subscription.

### Security

- The debug tools refuse commands that could run programs on your machine without your approval, and a function
  name passed to `cpp_decompile` can no longer run commands.

## [0.1.14] - 2026-09-23

### Added

- ByteAsk can run on a ChatGPT Plus (or higher) subscription; choose it from `/login`. Sign-in happens in a pane
  that closes on finish or `esc` and leaves nothing behind in your terminal.
- ChatGPT Free and Go plans are refused at sign-in with a clear message, before anything is saved, so an existing
  sign-in is kept.
- On a ChatGPT subscription the model picker lists only the models it can use: `gpt-6-astra`, `gpt-6-sol`,
  `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra` and `gpt-5.6-luna`. To switch back, use `/login` and choose
  *Use ByteAsk managed*.

### Changed

- Questions about ByteAsk itself are answered from ByteAsk's documentation.

### Fixed

- ByteAsk identifies itself as ByteAsk on every connection type: your own API key, self-hosted models and a
  ChatGPT subscription.
- The `Token usage: …` line no longer prints when a session ends.
- The background app-server no longer tries to install or update another product.
- The browser pages at the end of a sign-in, and the device-code screen, show ByteAsk branding.

## [0.1.13] - 2026-09-20

### Added

- ByteAsk can read several files, or several ranges of one file, in a single step, and search with context lines.
- `cpp_sanitize` accepts `sanitizer: "tsan"` to find data races, and can build your project's own CMake or Make
  target with sanitizers on.
- `debug_record` (rr) can record under rr's chaos scheduler until a run fails, then replay that run, including
  backwards. When rr cannot work on a machine it says why and what to use instead.
- `cpp_binary` can explain a link failure caused by a libstdc++ ABI mismatch and shows NX and RELRO state;
  `cpp_compile_db` can generate `compile_commands.json` for Makefile projects.

### Changed

- Command output is cleaned before the model reads it: colour codes and progress-bar redraws are removed,
  repeated lines are folded, and long logs keep every warning, error and summary line. The full output is saved
  on your machine and the model can open it; your terminal output does not change.
- After the history is compacted, the agent keeps a note of the files it had open and its most recent results.
- The update check reads ByteAsk's own feed and is on.
- The plugin marketplace is off by default; `features.plugins = true` in `~/.byteask/config.toml` turns it on.
- Old state from before the rename is cleared from `~/.byteask` on launch; sign-in, settings and history are kept.

### Fixed

- A Python project with a Makefile no longer gets the C/C++ guidance, and a repository whose C++ sits several
  folders down now does.
- `cpp_symbolize` caller-frame lines, `cpp_layout` padding for nested members of unknown size, `cpp_godbolt`
  compiler choice for requests like "clang 18", and `debug_attach` offering a gdb session that had exited. On
  macOS a clean sanitizer run says that leaks were not checked.
- `/upgrade` arrow keys work on a Pro plan, and Pro can be renewed from the picker; `/usage` shows your plan in
  its title; `/export` suggests `byteask-session.md`.

## [0.1.12] - 2026-09-08

### Added

- `/recap`, `/cd`, `/pwd` and `/agents` commands; `/copy` can target the last code block or quote; `/status`
  shows this thread's token usage.
- `byteask queue --thread <session> --message "…"` sends a message into a running session from another
  terminal; `byteask agents` browses sessions across the local daemon.
- `/keymap` can rebind keys, including two-key chords like `g g`; vim mode gained undo and redo, dot-repeat,
  search motions and character find; links in model output, including local file paths, are clickable where the
  terminal supports it.
- The model can ask structured questions with options.
- Screen-reader mode: `--ax-screen-reader`, the `BYTEASK_AX_SCREEN_READER` environment variable, or
  `tui.screen_reader` in your config.

### Changed

- `/usage`, `/referral-code`, `/upgrade` and `/voucher` open as panes over the composer instead of printing into
  scrollback; `esc` closes them and `enter` never does. `/usage` meters show when each window resets.
- The model picker has more models, `auto` falls through across several models and providers, and model
  descriptions use "fast", "balanced" and "deep" consistently.
- An auto-reviewer can approve some risky actions without asking, on by default. The permission menu offers
  "Approve for me" or "Ask for approval", and auto-denied actions are labelled in your approval history.

### Fixed

- Sessions renew themselves at launch before they expire, and expired sessions recover on the next launch, so you
  are no longer stuck at `Reconnecting…`. A credential that cannot be saved is reported as an error. Sessions
  reconnect themselves when a connection drops, keeping your draft.
- Only server errors and rate limits are retried; other errors, such as needing to sign in, are shown at once.
- `/referral-code` prints all share commands in full, and the installer reports a rejected referral code, where
  it installed and whether your PATH needs a change.

### Removed

- `byteask exec --full-auto`; use `--sandbox workspace-write`. `byteask mcp-server` now warns that it is
  deprecated; it still works.
- The announcement banner at the top of sessions.

### Security

- ByteAsk's C/C++ and reference tools are no longer available to the internal thread that summarizes your
  conversation.

## [0.1.11] - 2026-07-30

### Added

- Service announcements show at the top of the session.
- `byteask exec` shows ByteAsk tool calls (`cpp_*`, `ref_*`).

### Changed

- `/login` is a settings pane at the bottom of the screen.
- C/C++ tools show real sanitizer output, build your project, run with time limits and respect project trust.
- Performance checks take the machine's hardware into account.
- Remaining user-facing "Codex" strings now say ByteAsk, and `byteask --update` updates ByteAsk.

## [0.1.10] - 2026-07-23

### Changed

- Sign-in and self-hosted model wizard prompts match the style of the rest of the settings UI.

### Fixed

- Settings and menu screens no longer look duplicated when a wizard starts (self-hosted model add, sign-in).
- Status messages, such as key-add errors, no longer appear on unrelated screens.
- `install.sh` and `install.ps1` say when the optional ripgrep install did not work.

## [0.1.9] - 2026-07-18

### Added

- Self-hosted and custom models: point ByteAsk at your own OpenAI-compatible server (vLLM, TGI, SGLang, Ollama,
  LM Studio, llama.cpp, LiteLLM) with `byteask models add <alias> --url ...`, then pick it in `/model` under
  `self/<alias>`.

### Security

- General security hardening.

## [0.1.8] - 2026-07-15

### Added

- Every ByteAsk tool call (`autosar_ref`, `mcu_ref`, `cpp_godbolt`, `debug`, …) shows a live cell with the tool
  name, arguments and result.
- `/referral-code` shows your referral progress.

### Fixed

- Interrupted tool calls show cleanly, and retryable tool replies no longer show as failures.
- Aborting a turn no longer leaves stuck in-progress items in the thread history.

## [0.1.7] - 2026-07-15

### Added

- Reference tools `autosar_ref`, `mcu_ref`, `fpga_ref`, `hft_ref`, `grid_ref`, `scpi_ref` and `x86_ref`:
  page-cited lookups for embedded, firmware, hardware and safety-critical C++ (MISRA, AUTOSAR, Intel SDM,
  IEEE 1547, SCPI, Xilinx, ITCH and more).

### Changed

- Native debugger tools renamed: `cpp_debug`, `cpp_debug_session`, `cpp_debug_attach` and `cpp_record` are now
  `debug`, `debug_session`, `debug_attach` and `debug_record`; they debug any native binary, including Rust.

### Fixed

- More reliable `auto` model selection and Gemini and Anthropic models.

### Removed

- The `/app` slash command.

## [0.1.6] - 2026-07-13

### Fixed

- Pasting an API key no longer hangs in macOS Terminal.

## [0.1.5] - 2026-07-13

### Changed

- Merged upstream Codex CLI 0.144.1.
- The default model is `gpt-5.4`.
- ByteAsk occasionally asks for feedback.

## [0.1.4] - 2026-07-07

### Added

- The Linux installer installs bubblewrap automatically, and the sandbox falls back when it cannot start.
- Referral codes work with every installer.

### Fixed

- Gemini and Anthropic models: CRLF streaming, tool schemas and thought signatures.

### Removed

- Claude Sonnet 4.5 from the model list.

## [0.1.3] - 2026-07-06

### Added

- First public release of ByteAsk CLI, the AI coding agent for your terminal.
- Install on macOS and Linux with `curl -fsSL https://code.byteask.ai/install.sh | sh`, on Windows with
  `irm https://code.byteask.ai/install.ps1 | iex`, or with `pip install byteask`.
- `SHA256SUMS` to verify downloads.

[Unreleased]: https://github.com/ByteAsk/cli/compare/v0.2.4...HEAD
[0.2.4]: https://github.com/ByteAsk/cli/releases/tag/v0.2.4
[0.2.3]: https://github.com/ByteAsk/cli/releases/tag/v0.2.3
[0.2.0]: https://github.com/ByteAsk/cli/releases/tag/v0.2.0
[0.1.16]: https://github.com/ByteAsk/cli/releases/tag/v0.1.16
[0.1.15]: https://github.com/ByteAsk/cli/releases/tag/v0.1.15
[0.1.14]: https://github.com/ByteAsk/cli/releases/tag/v0.1.14
[0.1.13]: https://github.com/ByteAsk/cli/releases/tag/v0.1.13
[0.1.12]: https://github.com/ByteAsk/cli/releases/tag/v0.1.12
[0.1.11]: https://github.com/ByteAsk/cli/releases/tag/v0.1.11
[0.1.10]: https://github.com/ByteAsk/cli/releases/tag/v0.1.10
[0.1.9]: https://github.com/ByteAsk/cli/releases/tag/v0.1.9
[0.1.8]: https://github.com/ByteAsk/cli/releases/tag/v0.1.8
[0.1.7]: https://github.com/ByteAsk/cli/releases/tag/v0.1.7
[0.1.6]: https://github.com/ByteAsk/cli/releases/tag/v0.1.6
[0.1.5]: https://github.com/ByteAsk/cli/releases/tag/v0.1.5
[0.1.4]: https://github.com/ByteAsk/cli/releases/tag/v0.1.4
[0.1.3]: https://github.com/ByteAsk/cli/releases/tag/v0.1.3
