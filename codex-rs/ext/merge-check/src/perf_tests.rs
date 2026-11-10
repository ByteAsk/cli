//! The perf card, from comparisons `perf-runner` recorded on a bench machine:
//! an A/A of a feed handler against itself (NO CLAIM) and one commit against its
//! faster rewrite (FASTER, p50 -36.7%). The `ut` wrapper around them is the documented
//! report shape (`perf-runner/src/ut.rs` `UtReport`).

use pretty_assertions::assert_eq;
use serde_json::Value;

use super::*;

const AA: &str = include_str!("../testdata/aa-recorded.json");
const FASTER: &str = include_str!("../testdata/faster-recorded.json");

fn ut(gate: &str, tests: Vec<(&str, &str, Vec<&str>)>) -> Value {
    serde_json::json!({
        "schema": UT_SCHEMA,
        "gate": gate,
        "base": {"label": "base main", "reference": "main", "commit": "b".repeat(40)},
        "head": {"label": "head cand", "reference": "cand", "commit": "c".repeat(40)},
        "tests": tests.into_iter().map(|(name, report, reasons)| serde_json::json!({
            "name": name,
            "gate": gate,
            "reasons": reasons,
            "report": serde_json::from_str::<Value>(report).expect("recorded"),
        })).collect::<Vec<_>>(),
        "started_utc": "2026-10-18T17:00:00Z",
        "finished_utc": "2026-10-18T17:45:00Z",
        "runner_version": "0.1.0",
    })
}

/// An A/A pair posts "no significant change", with what the run could have detected.
#[test]
fn an_aa_pair_posts_no_significant_change() {
    let report = ut("PASS", vec![("feed handler", AA, vec![])]);
    let card = card(&report);
    assert!(card.starts_with("### ByteAsk perf check: PASS\n"), "{card}");
    assert!(
        card.contains("| feed handler | no significant change | -0.5% [-0.5%, +0.0%] |"),
        "{card}"
    );
    assert!(
        card.contains("- feed handler: no significant change; this run would have caught a change of 0.5% or more in p50_ns."),
        "{card}"
    );
    assert!(card.contains("bench-host-1 (cpu 2 isolated, 3.09 GHz pinned)"), "{card}");
    assert!(card.contains("12 rounds, 95% confidence"), "{card}");
    assert!(!card.contains("faster"), "an A/A must never read as a speedup: {card}");
    assert_eq!(result(&report).verdict, check_result::Verdict::Pass);
}

/// A real measured change posts the noise-gated delta with its confidence interval.
#[test]
fn a_measured_change_posts_the_delta_with_its_confidence_interval() {
    let report = ut("PASS", vec![("feed handler", FASTER, vec![])]);
    let card = card(&report);
    assert!(card.contains("| feed handler | faster | -36.7% [-37.8%, -33.6%] |"), "{card}");
    assert!(!card.contains("no significant change"), "{card}");
}

#[test]
fn a_regression_beyond_the_limit_fails_the_perf_check_and_says_why() {
    let reason = "p50_ns regressed 6.4% (limit 2%; CI +6.2% to +6.7%, p=0.0005 < 0.0100)";
    let report = ut("FAIL", vec![("feed handler", FASTER, vec![reason])]);
    let result = result(&report);
    assert_eq!(result.verdict, check_result::Verdict::Fail);
    assert!(result.summary.contains("p50_ns regressed 6.4%"), "{result:?}");
    assert!(card(&report).contains(&format!("- feed handler: {reason}")));
}

#[test]
fn a_bench_that_did_not_measure_is_not_run() {
    let report = ut("NOT_RUN", vec![("feed handler", AA, vec!["not measured: host refused"])]);
    let result = result(&report);
    assert_eq!(result.verdict, check_result::Verdict::NotRun);
    assert!(card(&report).starts_with("### ByteAsk perf check: NOT RUN"));
}

/// A bench report counts only for the candidate it measured.
#[test]
fn bind_refuses_a_report_for_another_candidate() {
    let binding = Binding {
        base_commit: "b".repeat(40),
        head_commit: "h".repeat(40),
        merge_tree: "t".repeat(40),
        profile_rev: "p".into(),
        manifest_rev: "none".into(),
        baseline_rev: "x".into(),
        env: String::new(),
    };
    let report = ut("PASS", vec![("feed handler", AA, vec![])]);
    let right_tree = |_: &str| Some("t".repeat(40));
    let bound = PerfEvidence::bind(report.clone(), &binding, right_tree).expect("binds");
    assert_eq!(bound.content_key, binding.content_key());
    let wrong_tree = |_: &str| Some("w".repeat(40));
    assert!(PerfEvidence::bind(report.clone(), &binding, wrong_tree).is_err());
    assert!(PerfEvidence::bind(report.clone(), &binding, |_: &str| None).is_err());
    let mut other_base = report.clone();
    other_base["base"]["commit"] = Value::from("a".repeat(40));
    assert!(PerfEvidence::bind(other_base, &binding, right_tree).is_err());
    let mut not_ut = report;
    not_ut["schema"] = Value::from("byteask.perflab.compare/1");
    assert!(PerfEvidence::bind(not_ut, &binding, right_tree).is_err());
}
