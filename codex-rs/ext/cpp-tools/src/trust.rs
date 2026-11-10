//! Trust gate for the `cpp_*` tools that execute code **outside** the
//! filesystem sandbox.
//!
//! # Why this exists
//!
//! [`crate::run::run_capped`] spawns a plain `tokio::process::Command` as a
//! direct child of the engine. The engine is not sandboxed, so those children
//! are not either: under `--sandbox read-only` the `shell` tool is refused with
//! `Read-only file system` while `cpp_sanitize` compiles and runs a binary in
//! the same turn. That is a real capability gap between what the user asked for
//! and what runs.
//!
//! # Why a gate alone is not enough
//!
//! The engine already has per-project trust
//! (`[projects."…"] trust_level = "trusted"` in config.toml, surfaced as
//! `Config::active_project`). Gating on it is right — but a first-time evaluator
//! on a fresh clone has **no trust entry at all**, which is precisely the
//! situation of the user whose report prompted this work. Gating on trust alone
//! would make their headline claim ("ByteAsk cannot run sanitizers") *true*.
//!
//! So the gate has an explicit escape hatch: the [`ALLOW_ENV`] environment
//! variable, named verbatim in the refusal text so a dead end is self-healing.
//! It is an env var rather than a CLI flag deliberately — it keeps the gate
//! self-contained inside this crate, with no edit to `exec/src/cli.rs`.
//!
//! An explicit env opt-in **wins over** a stored `trust_level = "untrusted"`:
//! the environment is a deliberate act by the person at the keyboard for this
//! run, config.toml is a remembered default.

use std::path::Path;

use codex_core::config::Config;
use codex_extension_api::FunctionCallError;

/// Explicit opt-in for the native-execution tools. Set to `1`/`true`/`yes`/`on`.
pub(crate) const ALLOW_ENV: &str = "BYTEASK_ALLOW_NATIVE_TOOLS";

/// Tools refused by this gate, for the "what still works" half of the message.
const STATIC_TOOLS: &str =
    "cpp_check, cpp_tidy, cpp_nav, cpp_impact, cpp_layout, cpp_godbolt, cpp_binary, cpp_symbolize, cpp_repo_history";

/// Parse a truthy env value. Anything else (including unset, `0`, empty) is not
/// an opt-in — the gate fails closed.
pub(crate) fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn env_opt_in() -> bool {
    truthy(std::env::var(ALLOW_ENV).ok().as_deref())
}

/// Resolve the gate once, at thread start, from the engine's own per-project
/// trust plus the env opt-in. A session with full permission ([`full_access`]) is
/// allowed too: the user already let everything run unsandboxed and unasked, so a
/// trust refusal would only make the model stop and ask.
pub fn native_exec_allowed(config: &Config) -> bool {
    decide(config.active_project.is_trusted(), env_opt_in()) || full_access(config)
}

/// The session runs with full permission: approval policy `never` AND no sandbox
/// (`danger-full-access`, or a permission profile that disables the sandbox) — the
/// "full access" preset. Then nothing in the C/C++ and reference tools may stop to
/// ask the user (trust refusals, proposals waiting for approval, ask-first wording).
pub fn full_access(config: &Config) -> bool {
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::SandboxPolicy;
    let never = matches!(
        config.permissions.approval_policy.value(),
        AskForApproval::Never
    );
    let unrestricted = matches!(
        config.permissions.permission_profile(),
        PermissionProfile::Disabled
    ) || matches!(config.legacy_sandbox_policy(), SandboxPolicy::DangerFullAccess);
    is_full_access(never, unrestricted)
}

/// [`full_access`]'s rule, separate from `Config` so it can be tested directly.
pub(crate) fn is_full_access(approval_never: bool, unrestricted: bool) -> bool {
    approval_never && unrestricted
}

/// The policy itself, separated from `Config` so it can be tested directly.
/// `Config` resolves `active_project` to `ProjectConfig { trust_level: None }`
/// when config.toml has no `[projects."…"]` entry, so an unlisted directory is
/// `trusted = false` — the fresh-clone case.
fn decide(trusted: bool, opted_in: bool) -> bool {
    trusted || opted_in
}

/// Guard the entry of a tool that compiles or runs code natively.
pub(crate) fn require_native_exec(
    allowed: bool,
    tool: &str,
    cwd: &Path,
) -> Result<(), FunctionCallError> {
    if allowed {
        return Ok(());
    }
    Err(FunctionCallError::RespondToModel(refusal_message(
        tool, cwd,
    )))
}

/// The refusal the model (and, once exec renders tool calls, the user) reads.
/// It must name both ways forward, or the dead end is not self-healing.
pub(crate) fn refusal_message(tool: &str, cwd: &Path) -> String {
    let project = cwd.display();
    format!(
        "{tool} compiles and/or runs code natively on this machine, OUTSIDE the filesystem \
         sandbox, so it is gated on project trust. This project ({project}) is not marked \
         trusted.\n\
         \n\
         Two ways to allow it:\n\
         1. One-off, for this run: set {ALLOW_ENV}=1 in the environment, e.g.\n\
         \x20     {ALLOW_ENV}=1 byteask exec \"…\"\n\
         2. Permanently, for this project: add to your byteask config.toml\n\
         \x20     [projects.\"{project}\"]\n\
         \x20     trust_level = \"trusted\"\n\
         \n\
         Analysis-only tools are NOT gated and work right now: {STATIC_TOOLS}. cpp_compile_db \
         can still DETECT an existing compile_commands.json, but generating one runs the \
         project's own build, so that half needs the same permission. Do not try to work around \
         this with the shell tool — under the default read-only sandbox that will fail too. Tell \
         the user which of the two options above to take."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_accepts_the_documented_spellings() {
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(truthy(Some(v)), "{v} should be truthy");
        }
    }

    #[test]
    fn truthy_fails_closed_on_everything_else() {
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("2"),
        ] {
            assert!(!truthy(v), "{v:?} must not open the gate");
        }
    }

    #[test]
    fn the_fresh_clone_case_is_refused_but_openable() {
        // A fresh clone (for example in /tmp/hft) has no `[projects."/tmp/hft"]`
        // entry, so `trusted` is false. Without the opt-in escape hatch the
        // trust gate would leave the sanitizers unusable for exactly the
        // people most likely to try them first.
        assert!(!decide(false, false), "fresh clone, no opt-in: refused");
        assert!(decide(false, true), "fresh clone + env opt-in: allowed");
        assert!(decide(true, false), "trusted project: allowed");
        // An explicit opt-in from the environment beats a remembered
        // `trust_level = "untrusted"`: it is a deliberate act for this run.
        assert!(decide(true, true));
    }

    #[test]
    fn the_env_var_is_actually_read() {
        // SAFETY: single-threaded within this test; the variable is read by no
        // other test in the crate.
        unsafe { std::env::remove_var(ALLOW_ENV) };
        assert!(!env_opt_in());
        unsafe { std::env::set_var(ALLOW_ENV, "1") };
        assert!(env_opt_in());
        unsafe { std::env::set_var(ALLOW_ENV, "0") };
        assert!(!env_opt_in(), "an explicit 0 must not open the gate");
        unsafe { std::env::remove_var(ALLOW_ENV) };
    }

    #[test]
    fn full_access_needs_both_approval_never_and_no_sandbox() {
        assert!(is_full_access(true, true));
        // `byteask exec` runs with approval `never` inside a sandbox: not full access.
        assert!(!is_full_access(true, false));
        // No sandbox but approvals on: the user is still asked, so keep asking.
        assert!(!is_full_access(false, true));
        assert!(!is_full_access(false, false));
    }

    #[test]
    fn allowed_passes_through() {
        assert!(require_native_exec(true, "cpp_sanitize", Path::new("/tmp/hft")).is_ok());
    }

    #[test]
    fn refusal_names_the_escape_hatch_and_the_config_stanza() {
        let err = require_native_exec(false, "cpp_sanitize", Path::new("/tmp/hft"))
            .expect_err("must refuse");
        let FunctionCallError::RespondToModel(msg) = err else {
            panic!("refusal must be model-facing, not fatal");
        };
        // The dead end has to be self-healing: both routes must be spelled out.
        assert!(msg.contains(ALLOW_ENV), "must name the env var: {msg}");
        assert!(msg.contains("BYTEASK_ALLOW_NATIVE_TOOLS=1"), "{msg}");
        assert!(msg.contains("[projects.\"/tmp/hft\"]"), "{msg}");
        assert!(msg.contains("trust_level = \"trusted\""), "{msg}");
        assert!(msg.contains("cpp_sanitize"), "must name the tool: {msg}");
    }

    #[test]
    fn refusal_points_away_from_the_shell_workaround() {
        // The reported failure mode was the model burning turns shelling out
        // after a refusal. Say up front that it will not work.
        let msg = refusal_message("debug", Path::new("/w"));
        assert!(msg.contains("shell tool"), "{msg}");
        assert!(msg.contains("read-only"), "{msg}");
        assert!(
            msg.contains("cpp_check"),
            "must list what still works: {msg}"
        );
    }
}
