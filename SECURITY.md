# Security policy

## Supported versions

Security fixes are released for the latest version of ByteAsk CLI. Older versions are not patched; update with
`byteask --update`.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

- Preferred: use GitHub's private reporting form on this repository (Security tab, "Report a vulnerability").
- Or email security@byteask.ai with "security" in the subject.

Include the version (`byteask --version`), your operating system, what you did, what happened, and what you
expected. A proof of concept helps but is not required.

What to expect:

- We acknowledge a report within 3 working days.
- We tell you whether we can reproduce it and what we plan to do, normally within 10 working days.
- We credit reporters in the release notes unless you ask us not to.

There is no paid bounty program.

## Scope

In scope: the code in this repository (the engine, the launcher and the installers) and the binaries published
in this repository's releases.

Problems in upstream OpenAI Codex that also affect this fork are welcome here; where the fix belongs upstream we
will say so and coordinate.

The hosted ByteAsk services are not part of this repository. Report problems with them to the same address.
