use pretty_assertions::assert_eq;

use super::*;

fn every_kind() -> Vec<CheckResult> {
    let mut all = vec![
        CheckResult::pass("cpp_check", "No findings in 3 files."),
        CheckResult::fail(
            "cpp_sanitize",
            "AddressSanitizer: heap-use-after-free at main.cpp:12.",
        ),
        CheckResult::no_claim(
            "cpp_perf",
            "B is 0.8% faster, inside the measurement noise.",
        ),
        CheckResult::approximate(
            "cpp_layout",
            "Offsets from clang's record layout; 2 members unknown.",
        ),
    ];
    for reason in NotRunReason::ALL {
        all.push(CheckResult::not_run(
            "cpp_sanitize",
            reason,
            "The program did not finish.",
            "",
        ));
    }
    all
}

#[test]
fn only_pass_is_success() {
    for result in every_kind() {
        assert_eq!(
            result.is_pass(),
            result.verdict == Verdict::Pass,
            "{result:?}"
        );
    }
}

/// No NOT RUN without a fix, for every reason, even when the caller
/// supplied none.
#[test]
fn every_not_run_reason_carries_a_fix() {
    for reason in NotRunReason::ALL {
        assert!(!reason.generic_fix().trim().is_empty(), "{reason:?}");
        let result = CheckResult::not_run("x", reason, "Did not run.", "   ");
        let fix = result.fix.as_deref().unwrap_or_default();
        assert!(!fix.trim().is_empty(), "{reason:?}");
        assert!(result.render_line().contains(" Fix: "), "{reason:?}");
    }
}

#[test]
fn every_reason_label_is_distinct_and_parses() {
    let mut labels: Vec<&str> = NotRunReason::ALL.iter().map(|r| r.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), NotRunReason::ALL.len());
    for label in labels {
        assert!(!label.contains(')') && !label.contains(':'), "{label}");
    }
}

#[test]
fn rendered_lines_look_like_this() {
    assert_eq!(
        CheckResult::pass("cpp_check", "No findings in 3 files.").render_line(),
        "Result: PASS: No findings in 3 files."
    );
    assert_eq!(
        CheckResult::not_run(
            "cpp_sanitize",
            NotRunReason::TimeLimit,
            "The program was still running after 300 s, so nothing was checked.",
            "call cpp_sanitize again with \"timeout_secs\": 900.",
        )
        .render_line(),
        "Result: NOT RUN (time limit): The program was still running after 300 s, so nothing \
         was checked. Fix: call cpp_sanitize again with \"timeout_secs\": 900."
    );
}

#[test]
fn every_kind_round_trips_through_tool_text() {
    for result in every_kind() {
        let text = result.append_to("existing tool output\nline two\n");
        assert!(
            text.starts_with("existing tool output\nline two\n\nResult: "),
            "{text}"
        );
        let parsed = CheckResult::from_tool_text(&result.check, &text).expect("parses back");
        assert_eq!(parsed, result);
    }
}

#[test]
fn the_existing_text_is_never_changed() {
    let original = "🔧 cpp_check · a.cpp\n2 findings\n  a.cpp:3: warning";
    let text = CheckResult::fail("cpp_check", "2 findings.").append_to(original);
    assert!(text.starts_with(original));
}

#[test]
fn only_the_last_line_counts() {
    // A program's own output can contain a line that looks like ours.
    let text = "Result: PASS: printed by the user's program\nreal output";
    assert_eq!(CheckResult::from_tool_text("cpp_sanitize", text), None);
    let text = "real output\nResult: PASS: printed by the program\n\nmore output";
    assert_eq!(CheckResult::from_tool_text("cpp_sanitize", text), None);
}

#[test]
fn malformed_lines_are_not_results() {
    for line in [
        "Result: OK: fine",
        "Result: NOT RUN: no reason given Fix: x",
        "Result: NOT RUN (made up): x Fix: y",
        "Result: NOT RUN (time limit): no fix given",
        "Result: PASS (time limit): x",
        "Result: PASS no colon",
        "result: PASS: lower case prefix",
    ] {
        assert_eq!(CheckResult::from_tool_text("x", line), None, "{line}");
    }
}

#[test]
fn text_that_would_break_the_line_is_flattened() {
    let result = CheckResult::not_run(
        "cpp_perf",
        NotRunReason::Unsupported,
        "No PMU.\nThe VM has no counters. Fix: nothing",
        "use a machine\nwith counters",
    );
    let line = result.render_line();
    assert!(!line.contains('\n'), "{line}");
    assert_eq!(line.matches(" Fix: ").count(), 1, "{line}");
    assert_eq!(CheckResult::from_tool_text("cpp_perf", &line), Some(result));
}

#[test]
fn the_json_record_has_stable_field_names() {
    let result = CheckResult::not_run(
        "cpp_sanitize",
        NotRunReason::OutputTruncated,
        "The report was cut.",
        "run it on one file.",
    )
    .with_evidence(["main.cpp".to_string()])
    .with_content_key("sha256:abc");
    assert_eq!(
        result.to_json(),
        serde_json::json!({
            "schema": 1,
            "check": "cpp_sanitize",
            "verdict": "NOT_RUN",
            "reason": "output_truncated",
            "fix": "run it on one file.",
            "summary": "The report was cut.",
            "evidence": ["main.cpp"],
            "content_key": "sha256:abc",
        })
    );
    assert_eq!(
        CheckResult::pass("cpp_check", "Clean.").to_json(),
        serde_json::json!({"schema": 1, "check": "cpp_check", "verdict": "PASS", "summary": "Clean."})
    );
}

#[test]
fn validate_rejects_records_that_break_the_rules() {
    let mut no_fix = CheckResult::not_run("x", NotRunReason::TimeLimit, "s", "f");
    no_fix.fix = None;
    assert!(no_fix.validate().is_err());
    let mut empty_fix = CheckResult::not_run("x", NotRunReason::TimeLimit, "s", "f");
    empty_fix.fix = Some(" ".to_string());
    assert!(empty_fix.validate().is_err());
    let mut pass_with_reason = CheckResult::pass("x", "s");
    pass_with_reason.reason = Some(NotRunReason::TimeLimit);
    assert!(pass_with_reason.validate().is_err());
    let mut future = CheckResult::pass("x", "s");
    future.schema = 2;
    assert!(future.validate().is_err());
    for result in every_kind() {
        assert_eq!(result.validate(), Ok(()), "{result:?}");
    }
}

#[test]
fn json_round_trips() {
    for result in every_kind() {
        let json = serde_json::to_string(&result).unwrap();
        let back: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }
}
