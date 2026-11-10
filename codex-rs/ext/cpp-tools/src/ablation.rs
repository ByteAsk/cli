//! Per-part off switches, for measuring what each part contributes.
//!
//! `BYTEASK_CPP_ABLATE=part,part,...` turns the named parts off for one engine process, so
//! a run can measure the strict gate with exactly one part removed. Nothing is off unless the
//! variable is set; it is an evaluation switch, not a product setting.
//!
//! | part           | what goes                                                     |
//! |----------------|---------------------------------------------------------------|
//! | `checklist`    | the automatic change-impact checklist (and the gate's re-ask)  |
//! | `gate`         | the finish gate (and so `gate_tests` and `critic`)             |
//! | `gate_tests`   | the gate's build and test run                                  |
//! | `critic`       | the gate's patch-shape critic in crash-fix sessions            |
//! | `impact_tool`  | the `cpp_impact` tool and its guidance lines                   |
//! | `test_select`  | the `cpp_test_select` tool and its guidance lines              |
//! | `repo_history` | the `cpp_repo_history` tool and its guidance lines             |
//! | `fix_verify`   | the `cpp_fix_verify` tool and its guidance lines               |
//! | `env_note`     | the session-start environment note                             |
//!
//! A second evaluation switch, also not a product setting: `BYTEASK_CPP_GUIDANCE_FILE=<path>`.
//! When set and the file is readable UTF-8, its contents replace the C/C++ guidance text (the
//! approved `CPP_GUIDANCE`, or its full-access variant) for that engine process, so one binary
//! can be measured with several guidance texts. Unset or unreadable = the approved text. The
//! reference guidance and the session-start environment note are not affected.
//!
//! The sub-agent parts have their own switches (`BYTEASK_ATTEMPTS`, `BYTEASK_REPRO_TESTS`), and
//! those are set directly. Dependencies between parts: see [`DEPENDS_ON`].

use std::collections::BTreeSet;

use crate::gate::GateSettings;

pub(crate) const ENV: &str = "BYTEASK_CPP_ABLATE";
/// Evaluation switch: a file whose contents replace the C/C++ guidance text (see the module doc).
pub(crate) const GUIDANCE_FILE_ENV: &str = "BYTEASK_CPP_GUIDANCE_FILE";

/// The guidance text from [`GUIDANCE_FILE_ENV`], read from the environment `env` describes:
/// `None` when it is unset, empty, or not a readable UTF-8 file.
pub(crate) fn guidance_override(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<String> {
    let path = env(GUIDANCE_FILE_ENV).filter(|path| !path.is_empty())?;
    std::fs::read_to_string(path).ok()
}

/// Every part name this build knows, so a caller can validate the names it passes.
pub(crate) const PARTS: &[&str] = &[
    "checklist",
    "gate",
    "gate_tests",
    "critic",
    "impact_tool",
    "test_select",
    "repo_history",
    "fix_verify",
    "env_note",
];

/// `(part, the part it needs)`: turning the second off also removes the first's effect, so a run
/// that removes the second measures both.
pub(crate) const DEPENDS_ON: &[(&str, &str)] = &[
    ("gate_tests", "gate"),
    ("critic", "gate"),
    // The gate re-asks unanswered checklist items; without the checklist there are none.
    ("checklist", "gate"),
    // cpp_test_select defers the full suite to the gate's run when the gate runs it.
    ("test_select", "gate_tests"),
];

/// Parts the fast profile (`[byteask_harness] fast = true`) leaves out: the automatic checklist, the
/// finish gate and the tools whose work it moves elsewhere (targeted tests come from the Context
/// engine's `context_verify` when it is configured, the project's own build otherwise).
/// The environment note goes too: it names per-workspace paths, and anything that differs between sessions in
/// the leading developer block stops the provider's prompt cache from being reused across sessions.
pub(crate) const FAST_OFF: &[&str] = &[
    "checklist",
    "gate",
    "impact_tool",
    "test_select",
    "repo_history",
    "fix_verify",
    "env_note",
];

/// Tools each part removes.
const TOOLS: &[(&str, &str)] = &[
    ("impact_tool", "cpp_impact"),
    ("test_select", "cpp_test_select"),
    ("repo_history", "cpp_repo_history"),
    ("fix_verify", "cpp_fix_verify"),
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ablation {
    off: BTreeSet<&'static str>,
}

impl Ablation {
    /// From [`ENV`]. An unknown name is reported and ignored, never guessed at.
    pub(crate) fn from_env() -> Self {
        let (ablation, unknown) = Self::parse(&std::env::var(ENV).unwrap_or_default());
        if !unknown.is_empty() {
            tracing::error!(
                "{ENV}: unknown part(s) {unknown:?}; known parts: {}",
                PARTS.join(", ")
            );
        }
        let implied = ablation.implied();
        if !implied.is_empty() {
            tracing::info!("{ENV}: these parts have no effect in this run either: {implied:?}");
        }
        ablation
    }

    pub(crate) fn parse(value: &str) -> (Self, Vec<String>) {
        let mut off = BTreeSet::new();
        let mut unknown = Vec::new();
        for name in value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            match PARTS.iter().find(|part| **part == name) {
                Some(part) => {
                    off.insert(*part);
                }
                None => unknown.push(name.to_string()),
            }
        }
        (Self { off }, unknown)
    }

    pub(crate) fn is_off(&self, part: &str) -> bool {
        self.off.contains(part)
    }

    /// Parts still on that cannot act because a part they need is off ([`DEPENDS_ON`]); a
    /// run that removes the second measures both.
    pub(crate) fn implied(&self) -> Vec<&'static str> {
        DEPENDS_ON
            .iter()
            .filter(|(part, needs)| self.is_off(needs) && !self.is_off(part))
            .map(|(part, _)| *part)
            .collect()
    }

    /// Apply the workflow parts to the gate settings.
    pub(crate) fn apply(&self, settings: &mut GateSettings) {
        if self.is_off("checklist") {
            settings.impact = false;
        }
        if self.is_off("gate") {
            settings.gate = false;
        }
        if self.is_off("gate_tests") {
            settings.tests = false;
        }
        if self.is_off("critic") {
            settings.critic = false;
        }
    }

    /// Is this tool removed?
    pub(crate) fn tool_off(&self, tool: &str) -> bool {
        TOOLS
            .iter()
            .any(|(part, name)| *name == tool && self.is_off(part))
    }

    /// The guidance without the lines that name a removed tool, so the guidance never tells the model
    /// to use a tool it does not have.
    /// These switches plus [`FAST_OFF`] when the thread runs the fast profile.
    pub(crate) fn with_fast(mut self, fast: bool) -> Self {
        if fast {
            self.off.extend(FAST_OFF.iter().copied());
        }
        self
    }

    pub(crate) fn guidance(&self, text: &str) -> String {
        let removed: Vec<&str> = TOOLS
            .iter()
            .filter(|(part, _)| self.is_off(part))
            .map(|(_, name)| *name)
            .collect();
        if removed.is_empty() {
            return text.to_string();
        }
        text.lines()
            .filter(|line| !removed.iter().any(|name| line.contains(name)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_guidance_file_switch_replaces_the_text_only_when_readable() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("guidance.txt");
        std::fs::write(&file, "Alternative guidance for an eval arm.").expect("write");
        let env = |path: Option<std::path::PathBuf>| {
            move |key: &str| {
                (key == GUIDANCE_FILE_ENV)
                    .then(|| path.clone().map(std::ffi::OsString::from))
                    .flatten()
            }
        };
        assert_eq!(
            guidance_override(env(Some(file.clone()))).as_deref(),
            Some("Alternative guidance for an eval arm.")
        );
        assert_eq!(
            guidance_override(env(None)),
            None,
            "unset = the approved text"
        );
        assert_eq!(
            guidance_override(env(Some(dir.path().join("missing")))),
            None
        );
        std::fs::write(&file, [0xff_u8, 0xfe, 0x00]).expect("write");
        assert_eq!(
            guidance_override(env(Some(file))),
            None,
            "not UTF-8 = the approved text"
        );
    }

    #[test]
    fn nothing_is_off_unless_named() {
        let (ablation, unknown) = Ablation::parse("");
        assert_eq!(ablation, Ablation::default());
        assert!(unknown.is_empty());
        let mut settings = GateSettings::default();
        ablation.apply(&mut settings);
        assert_eq!(settings, GateSettings::default());
        assert!(!ablation.tool_off("cpp_test_select"));
        assert_eq!(
            ablation.guidance("a\ncpp_test_select b"),
            "a\ncpp_test_select b"
        );
    }

    #[test]
    fn named_parts_go_and_unknown_names_are_reported() {
        let (ablation, unknown) = Ablation::parse(" gate_tests, test_select ,typo");
        assert_eq!(unknown, vec!["typo".to_string()]);
        let mut settings = GateSettings::enabled();
        ablation.apply(&mut settings);
        assert!(!settings.tests);
        assert!(settings.gate && settings.impact && settings.critic);
        assert!(ablation.tool_off("cpp_test_select"));
        assert!(!ablation.tool_off("cpp_impact"));
        assert_eq!(
            ablation.guidance("keep\nuse cpp_test_select for x\nalso keep"),
            "keep\nalso keep"
        );
    }

    #[test]
    fn every_part_does_something() {
        for part in PARTS {
            let (ablation, unknown) = Ablation::parse(part);
            assert!(unknown.is_empty());
            // From the opted-in workflow: the gate and checklist are off by default since 0.2.0.
            let mut settings = GateSettings::enabled();
            ablation.apply(&mut settings);
            let tools = TOOLS.iter().any(|(p, _)| p == part);
            let workflow = settings != GateSettings::enabled();
            assert!(
                tools || workflow || *part == "env_note",
                "{part} switches nothing off"
            );
        }
        for (part, needs) in DEPENDS_ON {
            assert!(PARTS.contains(part) && PARTS.contains(needs));
        }
        let (gate_off, _) = Ablation::parse("gate");
        assert_eq!(
            gate_off.implied(),
            vec!["gate_tests", "critic", "checklist"]
        );
    }
}
