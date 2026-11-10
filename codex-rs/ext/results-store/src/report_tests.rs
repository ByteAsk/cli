use check_result::CheckResult;
use check_result::NotRunReason;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::store;
use crate::test_support::T;
use crate::test_support::scripted_session;
use crate::test_support::store_root;

const NOW: u64 = T + 1_000;

fn limits() -> ResultsStoreLimits {
    ResultsStoreLimits::default()
}

/// The metrics, computed on a scripted session written through
/// the real recorder and real git (see `test_support::scripted_session`).
#[test]
fn metrics_are_computed_correctly_on_a_scripted_session() {
    let scripted = scripted_session();
    let report = ValueReport::build(&store::load(&store_root(&scripted)), NOW, None);
    assert_eq!(
        report.to_json(),
        json!({
            "schema": 1,
            "source": "local",
            "generated_at": NOW,
            "period": {"since": null, "first_record": T},
            "complete": true,
            "incomplete_since": null,
            "local": {
                "checks": {
                    "cpp_check": {"pass": 5, "fail": 1, "not_run": 0, "no_claim": 0, "approximate": 0, "not_run_reasons": {}},
                    "cpp_format": {"pass": 1, "fail": 0, "not_run": 0, "no_claim": 0, "approximate": 0, "not_run_reasons": {}},
                    "cpp_sanitize": {"pass": 1, "fail": 2, "not_run": 1, "no_claim": 0, "approximate": 0, "not_run_reasons": {"time_limit": 1}},
                    "cpp_tidy": {"pass": 0, "fail": 1, "not_run": 0, "no_claim": 0, "approximate": 0, "not_run_reasons": {}},
                },
                "other_tools": {
                    "cpp_perf": {"pass": 1, "fail": 0, "not_run": 0, "no_claim": 0, "approximate": 0, "not_run_reasons": {}},
                },
                "results_without_version": 1,
                "changes": {"total": 4, "with_ai_edit": 2, "pushed": 1},
                "problems_caught": {"changes": 1, "by_check": {"cpp_sanitize": 1}},
                "review_ready": {
                    "measured": 1,
                    "median_secs": 30,
                    "fastest_secs": 30,
                    "slowest_secs": 30,
                    "not_reached": 1,
                    "no_ai_edit": 2,
                },
                "perf": {"significant": 1, "smallest_ratio": 2.39, "median_ratio": 2.39, "largest_ratio": 2.39},
                "unreadable_records": 0,
                "capped_changes": 0,
            }
        })
    );
}

#[test]
fn the_text_report_for_the_scripted_session() {
    let scripted = scripted_session();
    let report = ValueReport::build(&store::load(&store_root(&scripted)), NOW, None);
    assert_eq!(
        report.render_text(&limits()),
        "\
ByteAsk value report: local results from this machine only (not from your CI)
Covers 2027-01-15 to 2027-01-15 (everything kept on this machine).

Checks run (local)
  cpp_check     5 passed, 1 failed
  cpp_format    1 passed
  cpp_sanitize  1 passed, 2 failed, 1 not run (time limit: 1)
  cpp_tidy      1 failed

Other ByteAsk tools (local)
  cpp_perf  1 passed

Changes (local)
  4 changes (one session's work on one branch), 2 with an AI edit ByteAsk saw, 1 pushed.

Problems caught (local)
  1 change: a check failed on a version of the change, and the same check passed on a later version that passed every check, before the change was pushed.
  By check: cpp_sanitize 1.

Time to review-ready (local)
  Measured for 1 change: median 30 s, fastest 30 s, slowest 30 s.
  From the AI's first edit to the first version that passed every check (inside another agent, such as Claude Code: from the start of the turn in which it first changed code).
  1 change never reached a version that passed every check.
  2 changes not measured: no AI edit was seen (inside another agent, such as Claude Code, ByteAsk sees edits through its hooks: run `byteask toolbox install` there).

Speed comparisons (local)
  1 comparison where one side was faster beyond the measurement noise: 2.39x to 2.39x (median 2.39x).
  Measured with cpp_perf on this machine. This says which side was faster, not what made it faster.

Results not tied to a version of the code (local)
  1 result (not in a git repository, or the code could not be read in time): counted under the tables above only.

This machine keeps results for 90 days and up to 2 GB of tool output (policy.toml, [results_store]). Nothing in this report is sent to ByteAsk; it holds counts only, no code, file names or branch names.
"
    );
}

/// A change log built in memory: (time, record) pairs after an open at 0.
fn log(records: Vec<Record>) -> ChangeLog {
    let mut all = vec![Record::Open {
        at: 0,
        repo: "/work/repo".to_string(),
        branch: "main".to_string(),
        session: "s".to_string(),
        head: Some("c0".to_string()),
    }];
    all.extend(records);
    ChangeLog {
        stem: "x".to_string(),
        records: all,
        unreadable: Vec::new(),
    }
}

fn edit(at: u64, version: &str) -> Record {
    Record::Edit {
        at,
        version: Some(version.to_string()),
    }
}

fn res(at: u64, version: &str, result: CheckResult) -> Record {
    Record::Result {
        at,
        version: Some(version.to_string()),
        result,
        payload: None,
    }
}

fn report_for(changes: Vec<ChangeLog>) -> ValueReport {
    ValueReport::build(
        &Loaded {
            changes,
            unreadable_files: Vec::new(),
        },
        NOW,
        None,
    )
}

fn base(at: u64, version: &str) -> Record {
    Record::Base {
        at,
        version: Some(version.to_string()),
    }
}

/// Inside another agent the edit is timed at the start of the turn, so a check the agent ran
/// on the untouched code later in that turn comes AFTER the recorded edit. The base version
/// (the code at session start) keeps it from counting as the change's failure.
#[test]
fn a_failure_on_the_session_start_code_never_counts_even_after_the_turn_start() {
    let report = report_for(vec![log(vec![
        base(1, "v0"),
        edit(10, "v1"),
        res(15, "v0", CheckResult::fail("cpp_sanitize", "overflow")),
        res(30, "v1", CheckResult::pass("cpp_sanitize", "clean")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 0);
    assert_eq!(report.local.review_ready.median_secs, Some(20));
}

/// An agent session with ByteAsk's hooks: the Stop hook's verdicts and the edit it saw make
/// both metrics measurable.
#[test]
fn an_agent_session_with_hooks_is_measured() {
    let report = report_for(vec![log(vec![
        base(0, "v0"),
        edit(1, "v1"),
        res(
            30,
            "v1",
            CheckResult::fail("cpp_finish_gate", "1 test failed"),
        ),
        edit(40, "v2"),
        res(
            90,
            "v2",
            CheckResult::pass("cpp_finish_gate", "every check passed"),
        ),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 1);
    assert_eq!(
        report.local.problems_caught.by_check.get("cpp_finish_gate"),
        Some(&1)
    );
    assert_eq!(report.local.review_ready.median_secs, Some(89));
    assert_eq!(report.local.review_ready.no_ai_edit, 0);
}

/// A session that checked and changed nothing is not a change at all.
#[test]
fn a_session_with_nothing_in_it_is_not_counted() {
    let report = report_for(vec![log(vec![base(0, "v0")])]);
    assert_eq!(report.local.changes.total, 0);
    assert_eq!(report.local.review_ready.no_ai_edit, 0);
}

#[test]
fn fixing_a_bug_that_was_there_before_the_first_edit_is_not_a_problem_caught() {
    // The "reproduce, then fix" task: the failure is on the code as it was.
    let report = report_for(vec![log(vec![
        res(5, "v0", CheckResult::fail("cpp_sanitize", "overflow")),
        edit(10, "v1"),
        res(20, "v1", CheckResult::pass("cpp_sanitize", "clean")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 0);
    // ...but the fixed version is review-ready: the failing check passed on it.
    assert_eq!(report.local.review_ready.median_secs, Some(10));
}

#[test]
fn a_version_where_the_failing_check_was_not_run_again_is_not_review_ready() {
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", CheckResult::fail("cpp_sanitize", "overflow")),
        edit(30, "v2"),
        res(40, "v2", CheckResult::pass("cpp_format", "formatted")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 0);
    assert_eq!(report.local.review_ready.not_reached, 1);
    assert_eq!(report.local.review_ready.measured, 0);
}

#[test]
fn a_check_that_did_not_run_must_pass_before_a_version_is_review_ready() {
    let not_run = CheckResult::not_run("cpp_sanitize", NotRunReason::TimeLimit, "cut", "");
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", not_run),
        edit(30, "v2"),
        res(40, "v2", CheckResult::pass("cpp_check", "clean")),
        res(55, "v2", CheckResult::pass("cpp_sanitize", "clean")),
    ])]);
    // Ready only once the sanitizer passed (55), not at the first pass (40).
    assert_eq!(report.local.review_ready.median_secs, Some(45));
    assert_eq!(
        report.local.problems_caught.changes, 0,
        "NOT RUN is not a FAIL"
    );
}

#[test]
fn a_flaky_check_on_the_same_version_is_not_a_problem_caught() {
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", CheckResult::fail("cpp_check", "race")),
        res(30, "v1", CheckResult::pass("cpp_check", "clean")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 0);
    assert_eq!(report.local.review_ready.not_reached, 1, "v1 also failed");
}

#[test]
fn a_later_failure_on_the_passing_version_takes_its_pass_back() {
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", CheckResult::fail("cpp_tidy", "warning")),
        edit(30, "v2"),
        res(40, "v2", CheckResult::pass("cpp_tidy", "clean")),
        res(50, "v2", CheckResult::fail("cpp_check", "null deref")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 0);
    assert_eq!(report.local.review_ready.not_reached, 1);
}

#[test]
fn several_problems_in_one_change_count_once_per_change_and_once_per_check() {
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", CheckResult::fail("cpp_tidy", "w")),
        res(21, "v1", CheckResult::fail("cpp_sanitize", "o")),
        edit(30, "v2"),
        res(40, "v2", CheckResult::fail("cpp_tidy", "w")),
        edit(50, "v3"),
        res(60, "v3", CheckResult::pass("cpp_tidy", "clean")),
        res(61, "v3", CheckResult::pass("cpp_sanitize", "clean")),
    ])]);
    assert_eq!(
        report.local.problems_caught,
        ProblemsCaught {
            changes: 1,
            by_check: [("cpp_sanitize".to_string(), 1), ("cpp_tidy".to_string(), 1)]
                .into_iter()
                .collect(),
        }
    );
    assert_eq!(report.local.review_ready.median_secs, Some(51));
}

#[test]
fn other_tools_cannot_make_or_break_a_version() {
    let report = report_for(vec![log(vec![
        edit(10, "v1"),
        res(20, "v1", CheckResult::fail("cpp_check", "bug")),
        edit(30, "v2"),
        res(
            35,
            "v2",
            CheckResult::no_claim("cpp_perf", "inside the noise"),
        ),
        res(40, "v2", CheckResult::pass("cpp_check", "clean")),
        res(45, "v2", CheckResult::approximate("cpp_nav", "partial")),
    ])]);
    assert_eq!(report.local.problems_caught.changes, 1);
    assert_eq!(report.local.review_ready.median_secs, Some(30));
}

/// The export contains no source text. Plant marked text in
/// every place a record can carry text and look for it in the output.
#[test]
fn the_export_and_the_text_contain_no_source_text() {
    const MARK: &str = "PLANTED_SOURCE";
    let source = format!("int {MARK}_order() {{ return book[{MARK}]; }}");
    let home = tempfile::tempdir().unwrap();
    let root = store::root(home.path());
    store::append(
        &root,
        "a",
        &Record::Open {
            at: 1,
            repo: format!("/home/trader/{MARK}_strategy"),
            branch: format!("feature/{MARK}-alpha"),
            session: MARK.to_string(),
            head: Some(MARK.to_string()),
        },
    )
    .unwrap();
    store::append(
        &root,
        "a",
        &Record::Edit {
            at: 2,
            version: Some(MARK.to_string()),
        },
    )
    .unwrap();
    let planted = [
        CheckResult::fail("cpp_sanitize", &source).with_evidence([source.clone()]),
        CheckResult::not_run("cpp_check", NotRunReason::BuildFailed, &source, &source),
        CheckResult::pass(&format!("cpp_check; {source}"), &source),
        CheckResult::pass(
            "cpp_perf",
            &format!("{MARK} is 9.99x faster, beyond the noise gate."),
        ),
        CheckResult::pass("cpp_sanitize", &source).with_content_key(MARK),
        CheckResult::pass("cpp_check", &source),
    ];
    for (n, result) in planted.iter().enumerate() {
        store::write_payload(&root, "a", &format!("{n}.txt"), &source).unwrap();
        store::append(
            &root,
            "a",
            &Record::Result {
                at: 3 + n as u64,
                version: Some(format!("{MARK}{}", n.min(2))),
                result: result.clone(),
                payload: Some(format!("{n}.txt")),
            },
        )
        .unwrap();
    }
    let report = ValueReport::build(&store::load(&root), NOW, None);
    let export = serde_json::to_string(&report.to_json()).unwrap();
    let text = report.render_text(&limits());
    for output in [&export, &text] {
        assert!(!output.contains(MARK), "{output}");
        assert!(!output.contains("return book"), "{output}");
        assert!(!output.contains("trader"), "{output}");
    }
    // The planted records were read and counted, so this is not vacuous.
    assert_eq!(report.local.checks["cpp_sanitize"].total(), 2);
    assert_eq!(report.local.checks["cpp_check"].not_run, 1);
    assert_eq!(report.local.checks["cpp_check"].pass, 1);
    assert_eq!(
        report.local.other_tools["other"].pass, 1,
        "a bad name shows as `other`"
    );
    assert_eq!(report.local.problems_caught.changes, 1);
}

/// Every count is labelled local.
#[test]
fn every_count_is_labelled_local() {
    let scripted = scripted_session();
    let report = ValueReport::build(&store::load(&store_root(&scripted)), NOW, None);

    // JSON: the counts live under `local`, and `source` says so. Outside
    // `local` only these numbers exist, and none of them is a count.
    let value = report.to_json();
    assert_eq!(value["source"], "local");
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "complete",
            "generated_at",
            "incomplete_since",
            "local",
            "period",
            "schema",
            "source"
        ]
    );
    fn numbers(value: &serde_json::Value, path: String, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Number(_) => out.push(path),
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key != "local" || !path.is_empty() {
                        numbers(child, format!("{path}/{key}"), out);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    numbers(child, format!("{path}/[]"), out);
                }
            }
            _ => {}
        }
    }
    let mut outside = Vec::new();
    numbers(&value, String::new(), &mut outside);
    outside.sort();
    assert_eq!(
        outside,
        vec!["/generated_at", "/period/first_record", "/schema"]
    );

    // Text: every line that carries a count sits under a "(local)" heading.
    let text = report.render_text(&limits());
    let mut heading = "";
    let mut counted_lines = 0;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            heading = line;
        }
        if line.starts_with("  ") && line.chars().any(|c| c.is_ascii_digit()) {
            counted_lines += 1;
            assert!(
                heading.ends_with("(local)"),
                "{line:?} is under {heading:?}"
            );
        }
    }
    assert!(counted_lines >= 10, "{text}");
    assert!(text.starts_with("ByteAsk value report: local results from this machine only"));
}

/// The failure-mode rule: an unreadable store says INCOMPLETE, never zero.
#[test]
fn an_unreadable_record_makes_the_report_incomplete_with_its_date() {
    let home = tempfile::tempdir().unwrap();
    let root = store::root(home.path());
    store::append(
        &root,
        "a",
        &Record::Open {
            at: 951_782_400,
            repo: "/r".to_string(),
            branch: "main".to_string(),
            session: "s".to_string(),
            head: None,
        },
    )
    .unwrap();
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(store::change_path(&root, "a"))
        .unwrap()
        .write_all(b"{\"t\":\"result\",\"at\":9,\n")
        .unwrap();
    let report = ValueReport::build(&store::load(&root), NOW, None);
    assert!(!report.complete);
    assert_eq!(report.incomplete_since, Some(951_782_400));
    assert_eq!(report.local.unreadable_records, 1);
    let text = report.render_text(&limits());
    assert!(
        text.contains("INCOMPLETE since 2000-02-29: part of the store could not be read, so each count below is at least that number."),
        "{text}"
    );
    assert!(text.contains("1 unreadable record"), "{text}");
}

#[test]
fn an_unreadable_store_directory_is_incomplete_not_empty() {
    let home = tempfile::tempdir().unwrap();
    let root = store::root(home.path());
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("changes"), "a file where the directory should be").unwrap();
    let report = ValueReport::build(&store::load(&root), NOW, None);
    assert!(!report.complete);
    assert!(report.render_text(&limits()).contains("INCOMPLETE"));
}

#[test]
fn an_empty_store_says_nothing_was_recorded_and_is_complete() {
    let home = tempfile::tempdir().unwrap();
    let report = ValueReport::build(&store::load(&store::root(home.path())), NOW, None);
    assert!(report.complete);
    let text = report.render_text(&limits());
    assert!(
        text.contains("Nothing recorded on this machine yet (2027-01-15)."),
        "{text}"
    );
    assert!(!text.contains("INCOMPLETE"));
}

#[test]
fn the_period_limits_results_and_changes() {
    let report = ValueReport::build(
        &Loaded {
            changes: vec![
                log(vec![
                    edit(10, "v1"),
                    res(20, "v1", CheckResult::fail("cpp_check", "bug")),
                ]),
                log(vec![
                    edit(100, "v1"),
                    res(110, "v1", CheckResult::pass("cpp_check", "ok")),
                ]),
            ],
            unreadable_files: Vec::new(),
        },
        NOW,
        Some(50),
    );
    assert_eq!(report.local.checks["cpp_check"].pass, 1);
    assert_eq!(report.local.checks["cpp_check"].fail, 0);
    assert_eq!(report.local.changes.total, 1);
    assert_eq!(report.period.since, Some(50));
}

#[test]
fn speed_ratios_are_read_from_significant_cpp_perf_comparisons_only() {
    use check_result::Verdict;
    let gate = "x faster, beyond the noise gate.";
    assert_eq!(
        speed_ratio("cpp_perf", Verdict::Pass, &format!("B is 2.39{gate}")),
        Some(2.39)
    );
    assert_eq!(
        speed_ratio("cpp_perf", Verdict::Pass, &format!("A is 1.05{gate}")),
        Some(1.05)
    );
    assert_eq!(
        speed_ratio("cpp_bench", Verdict::Pass, &format!("B is 2.39{gate}")),
        None
    );
    assert_eq!(
        speed_ratio("cpp_perf", Verdict::NoClaim, &format!("B is 2.39{gate}")),
        None
    );
    assert_eq!(
        speed_ratio(
            "cpp_perf",
            Verdict::Pass,
            "Measured over 10 runs with a noise estimate."
        ),
        None
    );
    assert_eq!(
        speed_ratio("cpp_perf", Verdict::Pass, &format!("B is NaN{gate}")),
        None
    );
    assert_eq!(
        speed_ratio("cpp_perf", Verdict::Pass, &format!("B is 0.50{gate}")),
        None
    );
}

/// Drift guard: the summary cpp_perf writes for a significant comparison is
/// the text [`speed_ratio`] reads. If cpp_perf changes it, this fails.
#[test]
fn cpp_perf_still_writes_the_summary_the_report_reads() {
    let perf = include_str!("../../cpp-tools/src/tools/perf.rs");
    assert!(
        perf.contains(r#""{winner} is {ratio:.2}x faster, beyond the noise gate.""#),
        "cpp_perf's significant-comparison summary changed; update report::speed_ratio"
    );
    // And `winner` is only ever 'A' or 'B'.
    let stats = include_str!("../../cpp-tools/src/stats.rs");
    assert!(stats.contains("`winner` is `'A'` or `'B'`"));
}

#[test]
fn nothing_the_user_sees_names_another_product_brand() {
    let scripted = scripted_session();
    let text =
        ValueReport::build(&store::load(&store_root(&scripted)), NOW, None).render_text(&limits());
    assert!(!text.to_ascii_lowercase().contains("codex"));
}

#[test]
fn dates_and_durations_read_plainly() {
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(951_782_400), "2000-02-29");
    assert_eq!(date(T), "2027-01-15");
    assert_eq!(duration(35), "35 s");
    assert_eq!(duration(250), "4 min 10 s");
    assert_eq!(duration(3_900), "1 h 05 min");
    assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2 GB");
    assert_eq!(bytes(5 * 1024 * 1024), "5 MB");
}
