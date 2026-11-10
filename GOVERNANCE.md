# Governance

ByteAsk CLI is maintained by ByteAsk, Inc. and developed in the open. This document describes who makes decisions,
how changes are reviewed and released, and how contributors can take on more responsibility.

## Roles

**Users** run ByteAsk CLI and report problems or ideas in issues.

**Contributors** open issues, review pull requests, improve documentation or submit code. Anyone can be a
contributor; see [CONTRIBUTING.md](CONTRIBUTING.md).

**Maintainers** review and merge pull requests, triage issues, cut releases and merge upstream changes. The current
maintainers are listed in [MAINTAINERS.md](MAINTAINERS.md).

## Decisions

- Most decisions are made in pull requests and issues, by the people doing the work, with a maintainer's review.
- A change is merged when at least one maintainer approves it, CI passes, and no maintainer has an open objection.
- Larger changes (new tools, new configuration surface, changes to what is sent over the network, changes to the
  licence or to this document) start as an issue labelled `proposal` and stay open for at least five working days
  before work is merged, so others can comment.
- When maintainers disagree, they try to reach consensus in the issue. If that fails, ByteAsk, Inc. makes the
  final call and records the reason in the issue.

## Reviews

- Maintainers aim to give a first response to new issues and pull requests within five working days.
- Reviews focus on correctness, tests, compatibility with existing configuration, and keeping the difference from
  upstream Codex CLI small.
- A maintainer does not merge their own pull request without another maintainer's approval, except for release
  housekeeping (version bumps, changelog) and reverts of a change that broke `main`.

## Releases

- Releases follow [Semantic Versioning](https://semver.org). While the version is 0.y.z, a minor release may
  contain breaking changes; the changelog says so.
- Every release has an entry in [CHANGELOG.md](CHANGELOG.md), a git tag `vX.Y.Z`, and GitHub release assets for
  each supported platform with a `SHA256SUMS` file.
- Security fixes are released as soon as they are ready, outside the normal schedule.

## Upstream

ByteAsk CLI is a fork of [OpenAI Codex CLI](https://github.com/openai/codex). Maintainers merge upstream stable
releases on a regular basis, in a pull request of their own, and record the upstream version in
[UPSTREAM.md](UPSTREAM.md). Fixes that belong upstream are offered upstream where the upstream project accepts
them.

## Becoming a maintainer

Contributors who have made sustained, high-quality contributions (code, reviews or triage) over several months can
be invited to become maintainers by the existing maintainers. Maintainers who are inactive for six months may be
moved to an emeritus list, with thanks.

## Code of conduct

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md). Maintainers enforce it.

## Changes to this document

Changes to this document are proposed in a pull request, labelled `proposal`, and need approval from ByteAsk, Inc.
