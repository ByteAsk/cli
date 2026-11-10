#!/usr/bin/env python3
"""Repository rules for changes to ByteAsk CLI.

Checks the lines added since --base (default: origin/main) for content that must not be published:
private keys, ByteAsk service keys, and paths or host names of ByteAsk's own machines.

Usage: check_repo_rules.py [--base <rev>] [--messages]
  --messages  also check commit messages since --base for AI co-author trailers (maintainer pre-push hook).
"""
import argparse
import re
import subprocess
import sys

CONTENT_RULES = [
    ("private key block", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |)PRIVATE KEY-----\s*$")),
    ("ByteAsk service key", re.compile(r"\b(?:bc[ks]|bx[khs])_[A-Za-z0-9][-A-Za-z0-9_]{15,}")),
    ("internal path", re.compile(r"/mnt/disks/queries|/home/pratyushsaini|/opt/byteask/")),
    ("internal host", re.compile(r"\b(?:prod-1|byteask-index-1|byteask-run-1|clinqai-prod)\b")),
]
MESSAGE_RULES = [
    ("AI co-author trailer", re.compile(r"(?im)^co-authored-by:.*(?:claude|anthropic|openai|codex|copilot|gpt)")),
    ("generated-with line", re.compile(r"(?i)generated with \[?claude")),
]
# Files where the rules themselves are written down.
EXEMPT = {"scripts/check_repo_rules.py"}


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True, errors="replace").stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="origin/main")
    ap.add_argument("--messages", action="store_true")
    args = ap.parse_args()
    base = args.base
    if not base or set(base) == {"0"}:
        base = "HEAD~1"

    failures = []
    path = None
    for line in git("diff", "-U0", "--no-color", f"{base}...HEAD").splitlines():
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else None
            continue
        if not line.startswith("+") or path is None or path in EXEMPT:
            continue
        for name, rx in CONTENT_RULES:
            if rx.search(line[1:]):
                failures.append(f"{path}: {name}")

    if args.messages:
        for sha in git("rev-list", f"{base}..HEAD").split():
            msg = git("log", "-1", "--format=%B", sha)
            for name, rx in MESSAGE_RULES:
                if rx.search(msg):
                    failures.append(f"commit {sha[:10]}: {name}")

    if failures:
        print("Repository rules failed:")
        for f in sorted(set(failures)):
            print(f"  {f}")
        return 1
    print("Repository rules passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
