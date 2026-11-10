#!/usr/bin/env python3
"""Fetch OpenAI's prebuilt rusty_v8 artifacts and emit the two cargo env vars.

The engine builds V8 with pointer compression plus the V8 sandbox (profile
``ptrcomp_sandbox_release``).  denoland publishes no
prebuilt archive for that profile, so the ``v8`` crate's own downloader gets a real
HTTP 404 and the build dies in its build script.  Upstream builds its own artifacts
and publishes them on ``openai/codex`` releases under the tag
``rusty-v8-v<crate-version>``; their CI wires them in via
``.github/actions/setup-rusty-v8``, which this fork cannot use because its
upstream workflows are not kept.

This is a *wrapper*, not a reimplementation: all fetching, caching and checksum
verification is done by ``scripts/codex_package/v8.py``, which is upstream code we
inherit on every merge.  What this script adds is an explicit ``--target``, a
sensible cache location, and the three output modes a build needs.

WHY THIS FILE LIVES IN THE ENGINE REPO
    ``.github/workflows/byteask-release.yml`` checks out only this repository, so the
    single implementation lives here and every build host (local machines and the
    Linux, macOS and Windows CI runners) runs the same code.

USAGE
    # bash / zsh.  Safe despite eval's usual status-swallowing: see
    # FAIL CLOSED below.
    eval "$(python3 scripts/fetch_rusty_v8.py --eval --target x86_64-unknown-linux-gnu)"

    # or write an env file once and source it from several shells:
    source "$(python3 scripts/fetch_rusty_v8.py --source)"

    # GitHub Actions:
    python3 scripts/fetch_rusty_v8.py --target "$TARGET" --github-env

FAIL CLOSED
    Every failure path -- unreachable release, missing/short/garbled checksum
    manifest, a truncated download, a corrupt cached artifact -- raises and exits
    non-zero without printing usable env vars.  In ``--eval`` mode a literal
    ``false`` is printed on stdout as well, so ``eval "$(...)"`` -- which would
    otherwise swallow our exit status, since an empty substitution makes ``eval ""``
    succeed -- returns non-zero instead of silently building with no override.

    Measured under ``set -e`` with a bad target:
        eval "$(cmd)"                        -> exit 1  (safe; the `false` fires)
        v8env="$(cmd)" || exit 1; eval ...   -> exit 1  (safe; real status)
        v8env="$(cmd)" && eval "$v8env"      -> exit 0  FAIL-OPEN, never use this:
            a failing command in a && list is exempt from `set -e`, and the
            short-circuit means eval never runs, so the caller sails into the build.
"""

from __future__ import annotations

import argparse
import os
import platform
import shlex
import sys
from pathlib import Path

if sys.version_info < (3, 11):
    raise SystemExit(
        "fetch_rusty_v8: needs Python >= 3.11 (codex_package.v8 parses Cargo.lock "
        f"with tomllib); running {platform.python_version()}"
    )

_SCRIPTS_DIR = Path(__file__).resolve().parent
_ENGINE_ROOT = _SCRIPTS_DIR.parent

# codex_package.targets reads CODEX_REPO_ROOT at *import* time and raises without
# it.  setdefault so a CI job that already exported it (upstream's setup-ci action
# does) keeps its own value.
os.environ.setdefault("CODEX_REPO_ROOT", str(_ENGINE_ROOT))
# PYTHONSAFEPATH=1 keeps the script dir off sys.path; add it explicitly, as
# scripts/build_codex_package.py does for the same reason.
sys.path.insert(0, str(_SCRIPTS_DIR))

from codex_package.targets import TARGET_SPECS, normalize_machine  # noqa: E402
from codex_package.v8 import (  # noqa: E402
    V8_ARTIFACT_PROFILE,
    RustyV8ArtifactPair,
    default_cache_root,
    fetch_codex_v8_artifacts,
    resolved_v8_crate_version,
)

PROG = "fetch-rusty-v8"

# Build targets ByteAsk actually ships, keyed by (platform.system(), machine).
#
# Deliberately NOT codex_package.targets.default_target(): that maps every Linux
# host to *-unknown-linux-musl (upstream's own release target).  ByteAsk builds
# Linux gnu with vendored OpenSSL, so borrowing
# upstream's host map would silently fetch the wrong artifact and fail at link time.
HOST_BUILD_TARGETS: dict[tuple[str, str], str] = {
    ("linux", "x86_64"): "x86_64-unknown-linux-gnu",
    ("linux", "aarch64"): "aarch64-unknown-linux-gnu",
    ("darwin", "x86_64"): "x86_64-apple-darwin",
    ("darwin", "aarch64"): "aarch64-apple-darwin",
    ("windows", "x86_64"): "x86_64-pc-windows-msvc",
    ("windows", "aarch64"): "aarch64-pc-windows-msvc",
}

ENV_FILE_NAME = "rusty-v8-env.sh"


class FetchError(RuntimeError):
    """Anything that must stop the build."""


def host_target() -> str:
    system = platform.system().lower()
    machine = normalize_machine(platform.machine())
    target = HOST_BUILD_TARGETS.get((system, machine))
    if target is None:
        raise FetchError(
            f"unsupported host {platform.system()}/{platform.machine()}; "
            f"pass --target explicitly (supported: {', '.join(sorted(TARGET_SPECS))})"
        )
    return target


def resolve_cache_root(explicit: Path | None) -> Path:
    if explicit is not None:
        return explicit
    from_env = os.environ.get("BYTEASK_RUSTY_V8_CACHE")
    if from_env:
        return Path(from_env)
    return default_cache_root()


def resolve_target(target: str | None) -> str:
    resolved = target or host_target()
    if resolved not in TARGET_SPECS:
        raise FetchError(
            f"unknown target {resolved!r}; supported: {', '.join(sorted(TARGET_SPECS))}"
        )
    return resolved


def archive_name(target: str) -> str:
    """Windows ships a MSVC import lib, every other target a static archive."""
    spec = TARGET_SPECS[target]
    if spec.is_windows:
        return f"rusty_v8_{V8_ARTIFACT_PROFILE}_{target}.lib.gz"
    return f"librusty_v8_{V8_ARTIFACT_PROFILE}_{target}.a.gz"


def fetch(
    target: str,
    cache_root: Path,
    *,
    version: str | None = None,
) -> RustyV8ArtifactPair:
    """Download-if-needed + checksum-verify.  Raises on any problem."""
    spec = TARGET_SPECS[target]
    try:
        return fetch_codex_v8_artifacts(spec, version=version, cache_root=cache_root)
    except FetchError:
        raise
    except Exception as exc:  # noqa: BLE001 - re-raised as one fail-closed type
        raise FetchError(f"{type(exc).__name__}: {exc}") from exc


def env_pairs(pair: RustyV8ArtifactPair) -> list[tuple[str, str]]:
    return [
        ("RUSTY_V8_ARCHIVE", str(pair.archive)),
        ("RUSTY_V8_SRC_BINDING_PATH", str(pair.binding)),
    ]


def render_eval(pair: RustyV8ArtifactPair) -> str:
    return "".join(
        f"export {key}={shlex.quote(value)}\n" for key, value in env_pairs(pair)
    )


def write_source_file(pair: RustyV8ArtifactPair, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"# generated by {PROG} -- safe to source; regenerate when the v8 "
        "crate version moves\n" + render_eval(pair),
        encoding="utf-8",
    )
    return path


def append_github_env(pair: RustyV8ArtifactPair, github_env: Path) -> None:
    # GitHub's env file is KEY=VALUE per line; these are single-line paths.
    with github_env.open("a", encoding="utf-8") as handle:
        for key, value in env_pairs(pair):
            handle.write(f"{key}={value}\n")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog=PROG,
        description=(
            "Fetch OpenAI's prebuilt rusty_v8 artifacts "
            f"(profile {V8_ARTIFACT_PROFILE}) and emit RUSTY_V8_ARCHIVE + "
            "RUSTY_V8_SRC_BINDING_PATH."
        ),
    )
    parser.add_argument(
        "--target",
        help=(
            "rust target triple to fetch for. Defaults to this host's ByteAsk build "
            "target, but release/CI builds cross-target -- pass it explicitly there."
        ),
    )
    parser.add_argument(
        "--cache-root",
        type=Path,
        help=(
            "where artifacts are cached. Default: $BYTEASK_RUSTY_V8_CACHE, else "
            "the system temp dir."
        ),
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--eval",
        dest="mode",
        action="store_const",
        const="eval",
        help="(default) print `export KEY=VALUE` lines for `eval`.",
    )
    mode.add_argument(
        "--source",
        dest="mode",
        action="store_const",
        const="source",
        help="write an env file and print its path, for `source \"$(...)\"`.",
    )
    mode.add_argument(
        "--github-env",
        dest="mode",
        action="store_const",
        const="github-env",
        help="append KEY=VALUE to $GITHUB_ENV (GitHub Actions).",
    )
    parser.add_argument(
        "--out",
        type=Path,
        help=f"--source only: env file path (default: <cache dir>/{ENV_FILE_NAME}).",
    )
    parser.set_defaults(mode="eval")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        target = resolve_target(args.target)
        cache_root = resolve_cache_root(args.cache_root)
        version = resolved_v8_crate_version()
        print(
            f"{PROG}: v8 {version} / {V8_ARTIFACT_PROFILE} / {target} "
            f"({archive_name(target)}) -> {cache_root}",
            file=sys.stderr,
        )
        pair = fetch(target, cache_root, version=version)

        if args.mode == "eval":
            sys.stdout.write(render_eval(pair))
        elif args.mode == "source":
            out = args.out or (pair.archive.parent / ENV_FILE_NAME)
            print(write_source_file(pair, out))
        else:
            github_env = os.environ.get("GITHUB_ENV")
            if not github_env:
                raise FetchError("--github-env used outside GitHub Actions ($GITHUB_ENV unset)")
            append_github_env(pair, Path(github_env))
            for key, value in env_pairs(pair):
                print(f"{PROG}: {key}={value}", file=sys.stderr)
    except Exception as exc:  # noqa: BLE001 - one fail-closed exit for every path
        print(f"{PROG}: FAILED: {exc}", file=sys.stderr)
        if args.mode == "eval":
            # Poison the eval so a caller that ignored our exit status still fails.
            print(f"false # {PROG} failed; see stderr")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
