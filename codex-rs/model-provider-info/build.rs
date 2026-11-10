//! ByteAsk: stamp the REAL upstream Codex version this
//! binary was built from into the compiled artifact.
//!
//! ByteAsk reports its own `0.1.0` everywhere, but the ChatGPT/Codex backend
//! gates its model list on the genuine Codex version — at `0.1.0` it returns an
//! empty list and 400s every model. A subscription session therefore reports the
//! upstream version instead (via the `auto` keyword; see
//! `default_client::resolve_reported_version`). Deriving it here — from the
//! `rust-v<x.y.z>` tag the engine was merged from — means it tracks upstream
//! automatically on every merge + rebuild, with no wrapper literal to keep in
//! sync and nothing to tune by hand.
//!
//! Resolution order:
//!   1. `BYTEASK_UPSTREAM_CODEX_VERSION` in the build env (explicit lever), if it
//!      parses;
//!   2. `git describe --tags --match 'rust-v*' --abbrev=0` (the normal path —
//!      works from a detached worktree too, since tags are repo-global);
//!   3. `FALLBACK` below (only reached when git is unavailable, e.g. a source
//!      build with no tags; keep it in sync with the merged tag at merge time).

use std::process::Command;

/// Only used when neither the build env nor git can supply the version. Bump it
/// to the merged `rust-v<x.y.z>` at each upstream merge so a git-less build (some
/// CI/source paths) still reports a real version.
const FALLBACK: &str = "0.153.4";

fn main() {
    println!("cargo:rerun-if-env-changed=BYTEASK_UPSTREAM_CODEX_VERSION");
    println!("cargo:rerun-if-changed=build.rs");

    let version = std::env::var("BYTEASK_UPSTREAM_CODEX_VERSION")
        .ok()
        .and_then(|v| normalize(&v))
        .or_else(git_upstream_version)
        .unwrap_or_else(|| FALLBACK.to_string());

    println!("cargo:rustc-env=BYTEASK_UPSTREAM_CODEX_VERSION={version}");
}

fn git_upstream_version() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--match", "rust-v*", "--abbrev=0"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tag = String::from_utf8(out.stdout).ok()?;
    normalize(&tag)
}

/// Accept a `rust-vX.Y.Z` tag or a bare `X.Y.Z`; return the numeric `X.Y.Z`.
/// Rejects anything non-numeric so a stray ref can never poison the version.
fn normalize(raw: &str) -> Option<String> {
    let v = raw.trim();
    let v = v.strip_prefix("rust-v").unwrap_or(v);
    let parts: Vec<&str> = v.split('.').collect();
    let ok = parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    ok.then(|| v.to_string())
}
