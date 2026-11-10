//! Runtime evidence about attempt 1 (the normal session) and the triggers for attempt 2.
//!
//! Attempt 2 starts only on weak runtime evidence. A trigger fires when:
//! - the change is empty;
//! - the finish gate's last verdict still reports problems (build, tests, edited tests, empty
//!   change, stale generated copies), or says it could not check (did not build or run tests);
//! - the agent's own final message carries uncertainty markers ("could not verify", ...);
//! - the independent writer delivered qualified tests (they fail on the starting code) in
//!   this session and none of them passes on the change;
//! - those tests could not be built or run.
//!
//! A session without qualified tests (the writer off, its default, or a writer that
//! produced none) is not evidence against the change: then only the empty change, the gate's
//! verdict (open problems, or what it could not check) and the agent's own hedging decide, so a
//! task with clean evidence gets one attempt.
//!
//! Everything here is a pure function of recorded data, so the rules are unit-tested.

use std::path::Path;

use serde::Serialize;

/// The finish gate's last verdict for the root's turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum GateVerdict {
    Passed,
    /// Problems that were still open when the turn ended.
    Problems(Vec<String>),
    /// The gate could not build or run the tests (or had nothing to build).
    NotChecked(Vec<String>),
    /// No gate record for this turn (gate off, not a git work tree, ...): no evidence either way.
    Unknown(String),
}

/// What tests that exercise the change say about attempt 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum TestEvidence {
    /// Qualified tests (failing on the starting code) that pass on the change.
    Passed(Vec<String>),
    /// Qualified tests that fail on the change (or no longer apply/build).
    Failed(Vec<String>),
    /// No qualified test exists in this session (the writer off, or none qualified): neutral.
    NoTests,
    /// They could not be built or run (why).
    NotRun(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    pub diff_empty: bool,
    pub gate: GateVerdict,
    pub uncertainty: Vec<String>,
    pub tests: TestEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Trigger {
    EmptyDiff,
    GateProblems,
    GateNotChecked,
    UncertaintyMarkers,
    NoPassingTest,
    TestsCouldNotRun,
}

/// The weak-evidence triggers that fire for `e` (empty: the evidence is strong, stop at 1).
pub fn triggers(e: &Evidence) -> Vec<Trigger> {
    let mut out = Vec::new();
    if e.diff_empty {
        out.push(Trigger::EmptyDiff);
    }
    match &e.gate {
        GateVerdict::Problems(_) => out.push(Trigger::GateProblems),
        GateVerdict::NotChecked(_) => out.push(Trigger::GateNotChecked),
        GateVerdict::Passed | GateVerdict::Unknown(_) => {}
    }
    if !e.uncertainty.is_empty() {
        out.push(Trigger::UncertaintyMarkers);
    }
    match &e.tests {
        // Delivered qualified tests that fail on the change.
        TestEvidence::Failed(_) => out.push(Trigger::NoPassingTest),
        TestEvidence::NotRun(_) => out.push(Trigger::TestsCouldNotRun),
        // Passing qualified tests, or none in this session: no trigger from tests.
        TestEvidence::Passed(_) | TestEvidence::NoTests => {}
    }
    out
}

/// Phrases with which an agent says it is not sure its change works. Matched case-insensitively
/// in the final message.
const UNCERTAINTY: &[&str] = &[
    "not sure",
    "unsure",
    "uncertain",
    "not confident",
    "could not verify",
    "couldn't verify",
    "unable to verify",
    "not verified",
    "unverified",
    "could not test",
    "couldn't test",
    "unable to test",
    "not tested",
    "untested",
    "could not build",
    "couldn't build",
    "unable to build",
    "could not compile",
    "couldn't compile",
    "could not run",
    "couldn't run",
    "unable to run",
    "could not reproduce",
    "couldn't reproduce",
    "unable to reproduce",
    "did not reproduce",
    "cannot confirm",
    "can't confirm",
    "could not confirm",
    "may not fully",
    "partial fix",
    "workaround",
    "best guess",
    "speculative",
];

pub fn uncertainty_markers(message: &str) -> Vec<String> {
    let lower = message.to_lowercase().replace('\u{2019}', "'");
    UNCERTAINTY
        .iter()
        .filter(|m| lower.contains(*m))
        .map(|m| (*m).to_string())
        .collect()
}

/// Problem kinds of the finish gate that count as "still reports problems". Hygiene items
/// (leftover files, unanswered checklist items, advisories) do not.
const GATE_PROBLEM_KINDS: &[&str] = &["Build", "Tests", "EmptyChange", "TestEdits", "Copies"];
const GATE_NOT_CHECKED_KINDS: &[&str] = &["SuiteNotChecked"];
/// Note fragments with which the gate says it did not build or run the tests.
const GATE_NOT_CHECKED_NOTES: &[&str] = &[
    "did not run",
    "not run",
    "no test target",
    "build/tests: off",
    "build/tests: skipped",
];

/// Classify one gate record (`problems` as `Kind: text`, `notes` free text; `not_checked` the
/// gate's own list of what it could not check, whether or not it was shown this round).
pub fn classify_gate_record(problems: &[String], notes: &[String], not_checked: &[String]) -> GateVerdict {
    let kind = |p: &String| p.split(':').next().unwrap_or("").trim().to_string();
    let open: Vec<String> = problems
        .iter()
        .filter(|p| GATE_PROBLEM_KINDS.contains(&kind(p).as_str()))
        .cloned()
        .collect();
    if !open.is_empty() {
        return GateVerdict::Problems(open);
    }
    let mut not_checked: Vec<String> = problems
        .iter()
        .filter(|p| GATE_NOT_CHECKED_KINDS.contains(&kind(p).as_str()))
        .cloned()
        .chain(not_checked.iter().map(|n| format!("not checked: {n}")))
        .collect();
    not_checked.extend(
        notes
            .iter()
            .filter(|n| {
                let lower = n.to_lowercase();
                GATE_NOT_CHECKED_NOTES.iter().any(|f| lower.contains(f))
            })
            .cloned(),
    );
    if !not_checked.is_empty() {
        return GateVerdict::NotChecked(not_checked);
    }
    GateVerdict::Passed
}

/// The gate's last verdict for `turn_id` from its run log (one JSON object per line:
/// `{"turn", "round", "problems": [..], "notes": [..], "still_open": [..], "not_checked": [..]}`;
/// `still_open` are build/test problems of an unchanged change set that the gate already
/// showed and does not repeat, so they still count as open).
pub fn gate_verdict(log: &Path, turn_id: Option<&str>) -> GateVerdict {
    let Ok(text) = std::fs::read_to_string(log) else {
        return GateVerdict::Unknown("no finish-gate record".to_string());
    };
    let strings = |v: &serde_json::Value, key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let last = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .rfind(|v| match turn_id {
            Some(id) => v.get("turn").and_then(|t| t.as_str()) == Some(id),
            None => true,
        });
    match last {
        Some(v) => {
            let mut problems = strings(&v, "problems");
            problems.extend(strings(&v, "still_open"));
            classify_gate_record(&problems, &strings(&v, "notes"), &strings(&v, "not_checked"))
        }
        None => GateVerdict::Unknown("no finish-gate record for this turn".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn evidence(diff_empty: bool, gate: GateVerdict, uncertainty: &[&str], tests: TestEvidence) -> Evidence {
        Evidence {
            diff_empty,
            gate,
            uncertainty: uncertainty.iter().map(|s| (*s).to_string()).collect(),
            tests,
        }
    }

    #[test]
    fn strong_evidence_fires_nothing() {
        let e = evidence(false, GateVerdict::Passed, &[], TestEvidence::Passed(vec!["t".into()]));
        assert_eq!(triggers(&e), Vec::<Trigger>::new());
        // An unknown gate is not evidence against the change.
        let e = evidence(false, GateVerdict::Unknown("off".into()), &[], TestEvidence::Passed(vec!["t".into()]));
        assert_eq!(triggers(&e), Vec::<Trigger>::new());
    }

    #[test]
    fn each_weak_signal_fires() {
        let pass = || TestEvidence::Passed(vec!["t".into()]);
        assert_eq!(triggers(&evidence(true, GateVerdict::Passed, &[], pass())), vec![Trigger::EmptyDiff]);
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Problems(vec!["Build: x".into()]), &[], pass())),
            vec![Trigger::GateProblems]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::NotChecked(vec!["x".into()]), &[], pass())),
            vec![Trigger::GateNotChecked]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Passed, &["not sure"], pass())),
            vec![Trigger::UncertaintyMarkers]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Passed, &[], TestEvidence::Failed(vec!["t".into()]))),
            vec![Trigger::NoPassingTest]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Passed, &[], TestEvidence::NotRun("no build".into()))),
            vec![Trigger::TestsCouldNotRun]
        );
        let all = evidence(
            true,
            GateVerdict::Problems(vec!["Tests: y".into()]),
            &["untested"],
            TestEvidence::Failed(vec!["t".into()]),
        );
        assert_eq!(
            triggers(&all),
            vec![Trigger::EmptyDiff, Trigger::GateProblems, Trigger::UncertaintyMarkers, Trigger::NoPassingTest]
        );
    }

    /// The writer off (no qualified tests in the session) and a clean gate: one attempt. Only the other
    /// runtime evidence escalates then.
    #[test]
    fn without_qualified_tests_clean_evidence_fires_nothing() {
        let clean = evidence(false, GateVerdict::Passed, &[], TestEvidence::NoTests);
        assert_eq!(triggers(&clean), Vec::<Trigger>::new());
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Unknown("off".into()), &[], TestEvidence::NoTests)),
            Vec::<Trigger>::new()
        );
        assert_eq!(triggers(&evidence(true, GateVerdict::Passed, &[], TestEvidence::NoTests)), vec![Trigger::EmptyDiff]);
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Problems(vec!["Tests: x".into()]), &[], TestEvidence::NoTests)),
            vec![Trigger::GateProblems]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::NotChecked(vec!["x".into()]), &[], TestEvidence::NoTests)),
            vec![Trigger::GateNotChecked]
        );
        assert_eq!(
            triggers(&evidence(false, GateVerdict::Passed, &["could not verify"], TestEvidence::NoTests)),
            vec![Trigger::UncertaintyMarkers]
        );
    }

    #[test]
    fn markers_are_found_case_insensitively() {
        assert_eq!(
            uncertainty_markers("Done. I couldn\u{2019}t verify the fix because the build is BROKEN; NOT TESTED."),
            vec!["couldn't verify".to_string(), "not tested".to_string()]
        );
        assert!(uncertainty_markers("Fixed add(); all 12 tests pass.").is_empty());
    }

    #[test]
    fn gate_records_are_classified() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(classify_gate_record(&[], &s(&["build: ok", "tests: 12 passed"]), &[]), GateVerdict::Passed);
        assert_eq!(
            classify_gate_record(&s(&["Tests: FAILING TESTS a", "Impact: items"]), &[], &[]),
            GateVerdict::Problems(s(&["Tests: FAILING TESTS a"]))
        );
        // Hygiene and advisories alone are not open problems.
        assert_eq!(classify_gate_record(&s(&["Leftovers: x", "ShapeAdvisory: y"]), &[], &[]), GateVerdict::Passed);
        assert_eq!(
            classify_gate_record(&s(&["SuiteNotChecked: COULD NOT CHECK"]), &[], &[]),
            GateVerdict::NotChecked(s(&["SuiteNotChecked: COULD NOT CHECK"]))
        );
        assert_eq!(
            classify_gate_record(&[], &s(&["build/tests: skipped (no C/C++ or build file changed)"]), &[]),
            GateVerdict::NotChecked(s(&["build/tests: skipped (no C/C++ or build file changed)"]))
        );
        // The gate's own not-checked list counts even after its note was shown once.
        assert_eq!(
            classify_gate_record(&[], &s(&["build: ok"]), &s(&["2 test(s) hit the per-test time limit"])),
            GateVerdict::NotChecked(s(&["not checked: 2 test(s) hit the per-test time limit"]))
        );
    }

    #[test]
    fn the_last_record_of_the_turn_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("gate-runs.jsonl");
        assert!(matches!(gate_verdict(&log, Some("t1")), GateVerdict::Unknown(_)));
        std::fs::write(
            &log,
            "{\"turn\":\"t0\",\"round\":0,\"problems\":[],\"notes\":[]}\n\
             {\"turn\":\"t1\",\"round\":0,\"problems\":[\"Build: BUILD FAILS\"],\"notes\":[]}\n\
             not json\n\
             {\"turn\":\"t1\",\"round\":1,\"problems\":[],\"notes\":[\"tests: 3 passed\"]}\n",
        )
        .expect("write");
        assert_eq!(gate_verdict(&log, Some("t1")), GateVerdict::Passed);
        assert_eq!(gate_verdict(&log, Some("t0")), GateVerdict::Passed);
        assert!(matches!(gate_verdict(&log, Some("t9")), GateVerdict::Unknown(_)));
        std::fs::write(
            &log,
            "{\"turn\":\"t1\",\"round\":1,\"problems\":[\"Tests: FAILING TESTS x\"],\"notes\":[]}\n",
        )
        .expect("write");
        assert!(matches!(gate_verdict(&log, Some("t1")), GateVerdict::Problems(_)));
        // A problem shown in an earlier round and not repeated for the unchanged change set is
        // still open at the end of the turn.
        std::fs::write(
            &log,
            "{\"turn\":\"t1\",\"round\":1,\"problems\":[],\"notes\":[],\"still_open\":[\"Tests: TESTS THAT PASS ON THE UNMODIFIED TREE NOW FAIL x\"],\"not_checked\":[]}\n",
        )
        .expect("write");
        assert!(matches!(gate_verdict(&log, Some("t1")), GateVerdict::Problems(_)));
    }
}
