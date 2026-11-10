//! Memory-safety remediation support: root-cause evidence from sanitizer
//! reports, a patch-shape critic, and the `cpp_fix_verify` fix verifier.
//!
//! Nothing here is tuned to any benchmark: it is generic C/C++ crash-repair
//! practice.

pub(crate) mod critic;
pub(crate) mod diff;
pub(crate) mod evidence;
mod runner;
mod verify;

pub(crate) use verify::FixVerifyTool;

/// The patch-shape critic as a plain function, for a finish
/// gate in another crate to call cheaply when the model ends a crash-fix turn: no build, no
/// execution. `diff_text` is a unified diff; `crash_functions` are the crash
/// stack's functions (innermost first), or empty when unknown. Returns the
/// rendered advisory findings.
pub fn critique_patch(diff_text: &str, crash_functions: &[String]) -> String {
    critic::render(&critic::critique(diff_text, crash_functions))
}

/// Root-cause evidence rendered from a sanitizer report's stderr (empty when it
/// holds no report), for a finish gate to attach to a crash-fix review.
pub fn root_cause_evidence(sanitizer_stderr: &str) -> String {
    evidence::render_from(sanitizer_stderr)
}
