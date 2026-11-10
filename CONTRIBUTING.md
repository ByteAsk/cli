# Contributing to ByteAsk CLI

Thanks for helping. Bug reports, fixes, documentation and reviews are all welcome. This guide covers how to
propose a change, how to build and test it, and what reviewers look for.

## Ways to contribute

- **Report a bug** with the [bug report form](https://github.com/ByteAsk/cli/issues/new/choose). Include the
  version (`byteask --version`), your operating system, the steps, and what you saw.
- **Suggest a feature** with the feature request form. Describe the problem first, then the change you want.
- **Fix something.** Issues labelled [`good first issue`](https://github.com/ByteAsk/cli/labels/good%20first%20issue)
  are small and self-contained. Issues labelled [`help wanted`](https://github.com/ByteAsk/cli/labels/help%20wanted)
  are ones maintainers would welcome a pull request for.
- **Review pull requests.** Reviews from contributors are valued and count towards becoming a maintainer
  ([GOVERNANCE.md](GOVERNANCE.md)).
- **Improve the docs** in `README.md` and `docs/byteask/`.

Security problems go through [SECURITY.md](SECURITY.md), never through public issues.

## Before you start

- Comment on the issue you want to work on, so two people do not fix the same thing.
- For a larger change (a new tool, a new setting, anything that changes what is sent over the network), open an
  issue labelled `proposal` first and agree on the approach. Pull requests for larger changes without a prior
  issue may be closed.

## Repository layout

| Path | What it is |
|---|---|
| `codex-rs/` | The engine: a Rust workspace forked from OpenAI Codex CLI. ByteAsk's own crates are under `codex-rs/ext/`. |
| `byteask`, `byteask.ps1` | The launcher for macOS/Linux and Windows. |
| `install.sh`, `install.ps1` | The installers. |
| `helpers/` | Python helpers the installer places next to the launcher (own-key routing, provider translators). |
| `tests/` | Launcher, installer and helper tests. |
| `docs/byteask/` | ByteAsk's guides: usage, configuration, building from source. |
| `codex-cli/`, `sdk/`, upstream `docs/` | Kept from upstream so merges stay small. Nothing is published from them. |

This project is a fork. Edits to files that come from upstream make future merges harder, so prefer adding code
under `codex-rs/ext/` over editing upstream files, and keep edits to upstream files small and clearly separated.
See [UPSTREAM.md](UPSTREAM.md).

## Development setup

Build and run instructions are in [docs/byteask/building.md](docs/byteask/building.md). In short:

```sh
eval "$(python3 scripts/fetch_rusty_v8.py --target <your-target>)"
cd codex-rs
cargo build -p codex-cli
```

## Tests

Every change in behaviour needs a test that would fail without it.

- **Engine:** run the tests of the crate you changed, for example `cargo test -p codex-cpp-tools`. Terminal UI
  changes use snapshot tests; review new snapshots with `cargo insta review` and commit them.
- **Launcher and installers:** `sh tests/<name>.sh`. Windows scripts have PowerShell tests in `tests/`.
- **Helpers:** `python3 -m unittest discover -s tests -p 'test_*.py'`.

CI runs formatting, a compile check, the ByteAsk crate tests, the launcher tests and a secret scan on every pull
request.

## Style

- Rust: `cargo fmt`; no new `clippy` warnings in the crates you touched.
- Shell: POSIX `sh`, checked with ShellCheck. Keep `byteask` and `byteask.ps1` in step: a change to one usually
  needs the same change in the other.
- Python: standard library only in `helpers/`.
- Comments explain why the code does what it does. Do not reference private documents, tickets or people.
- User-visible text: short, plain sentences.

## Commits and pull requests

- One topic per pull request. Describe what changed, why, and how you tested it.
- Commit subjects in the imperative, with the area first, for example `cpp_check: report the failing target name`.
- Sign off each commit (`git commit -s`). The sign-off states that you wrote the change or have the right to
  submit it, under the [Developer Certificate of Origin](https://developercertificate.org/).
- Keep pull requests up to date with `main` by rebasing; maintainers squash or rebase when merging.
- Do not include keys, tokens, internal URLs or private code in commits, test data or screenshots.

## Review

- A maintainer aims to respond within five working days. If a pull request has had no response after that, a
  polite ping in the pull request is welcome.
- Reviewers check correctness, tests, compatibility with existing configuration, and the size of the change to
  upstream files.
- A pull request is merged when a maintainer approves it, CI passes and review comments are resolved.
- See [GOVERNANCE.md](GOVERNANCE.md) for how decisions are made.

## Issue triage

Maintainers label new issues within a few working days:

| Label | Meaning |
|---|---|
| `bug`, `enhancement`, `documentation`, `install` | the kind of issue |
| `needs info` | waiting for details from the reporter; closed after 30 days without a reply |
| `good first issue` | small, self-contained, with enough context to start |
| `help wanted` | maintainers would welcome a pull request |
| `proposal` | a larger change under discussion |
| `upstream` | the problem is in OpenAI Codex CLI and belongs there |

## Licence of contributions

ByteAsk CLI is licensed under the Apache License, Version 2.0. By submitting a contribution you agree that it is
licensed under the same terms, as described in section 5 of the licence.

## Conduct

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
