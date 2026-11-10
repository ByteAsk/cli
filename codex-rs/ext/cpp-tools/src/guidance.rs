/// The C/C++ guidance: one short note instead of the old
/// tool catalog. The specialised tools reach the model through `tool_search` when a task needs
/// one; `cpp_finish_check` is the optional, model-called finish check (the automatic finish gate
/// is off by default).
pub const CPP_GUIDANCE: &str = "\
This is a C/C++ workspace. Build and test with the project's own build system, flags and test framework. After a behaviour change, build the affected targets and run the tests that cover it. Keep useful regression tests unless the user says otherwise; do not edit existing tests unless the request changes what they assert.
- Reuse an existing build tree incrementally; never reconfigure or delete a build directory you did not create. Preserve the user's files and edits; remove only your own scratch files.
- Crash, sanitizer report, or suspected memory error/UB: run the failing program or test under ASan+UBSan and read the real report. Suspected race: TSan; intermittent failure: rr where supported. A tool that did not run verifies nothing.
- Core dump: analyse it read-only; get the user's consent before running the program or changing a live process.
- Claim a speedup only after a measurement shows a significant difference.
- Specialist tools (debuggers, profilers, layout, change impact) load through tool_search when needed. Before saying a change works, use cpp_finish_check when extra verification helps, and report what you checked and what is still unverified.";

/// The approved guidance's core-dump line (default modes).
const CORE_DUMP_LINE: &str = "- Core dump: analyse it read-only; get the user's consent before running the program or changing a live process.";
/// Its replacement in a session with full permission ([`crate::trust::full_access`]).
const FULL_ACCESS_CORE_DUMP_LINE: &str = "- Core dump: analyse it read-only first; full access is on, so run the program or change a live process when the task needs it, without asking.";
/// The line appended in a session with full permission.
pub(crate) const FULL_ACCESS_LINE: &str = "Full access is on: do not stop to ask the user for confirmation; proceed and report what you did.";

/// The guidance for a session with full permission (approval `never` and full access): the
/// core-dump line no longer asks for consent, and one line says not to stop and ask. Default
/// modes keep the approved text unchanged.
pub(crate) fn full_access_guidance(text: &str) -> String {
    format!(
        "{}\n{FULL_ACCESS_LINE}",
        text.replacen(CORE_DUMP_LINE, FULL_ACCESS_CORE_DUMP_LINE, 1)
    )
}

/// The fast profile's C/C++ note (`[byteask_harness] fast = true`): the same approved text as
/// [`CPP_GUIDANCE`] (one guidance for every profile since 0.2.0).
pub const FAST_CPP_GUIDANCE: &str = CPP_GUIDANCE;

/// What the engine enforces in a thread (the workflow: the change-impact checklist and the
/// finish gate's full suite).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Enforcement {
    /// The change-impact checklist is injected after the first C/C++ edit.
    pub checklist: bool,
    /// The finish gate builds every target and runs the full test suite.
    pub finish_suite: bool,
}

impl Enforcement {
    pub const NONE: Self = Self {
        checklist: false,
        finish_suite: false,
    };
}

/// The guidance for a thread: [`CPP_GUIDANCE`], byte for byte, for every enforcement. There
/// is no variant per enforcement; the parameter is kept so one can be added without changing
/// the callers.
pub(crate) fn cpp_guidance(_enforcement: Enforcement) -> &'static str {
    CPP_GUIDANCE
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// Both variants pinned: the approved text for the default modes, and the full-access
    /// variant (core-dump line replaced, one line appended).
    #[test]
    fn the_full_access_variant_replaces_the_consent_line_and_adds_one_line() {
        assert!(
            CPP_GUIDANCE.contains(CORE_DUMP_LINE),
            "the approved text carries the line"
        );
        let full = full_access_guidance(CPP_GUIDANCE);
        let expected = "\
This is a C/C++ workspace. Build and test with the project's own build system, flags and test framework. After a behaviour change, build the affected targets and run the tests that cover it. Keep useful regression tests unless the user says otherwise; do not edit existing tests unless the request changes what they assert.
- Reuse an existing build tree incrementally; never reconfigure or delete a build directory you did not create. Preserve the user's files and edits; remove only your own scratch files.
- Crash, sanitizer report, or suspected memory error/UB: run the failing program or test under ASan+UBSan and read the real report. Suspected race: TSan; intermittent failure: rr where supported. A tool that did not run verifies nothing.
- Core dump: analyse it read-only first; full access is on, so run the program or change a live process when the task needs it, without asking.
- Claim a speedup only after a measurement shows a significant difference.
- Specialist tools (debuggers, profilers, layout, change impact) load through tool_search when needed. Before saying a change works, use cpp_finish_check when extra verification helps, and report what you checked and what is still unverified.
Full access is on: do not stop to ask the user for confirmation; proceed and report what you did.";
        assert_eq!(full, expected);
        assert!(!full.contains("consent"));
        assert_eq!(FAST_CPP_GUIDANCE, CPP_GUIDANCE);
    }
}
