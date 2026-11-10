//! Rendering, against answers captured from the real context engine (an index
//! of rigtorp/SPSCQueue's SPSCQueueTest target, GCC 12, RelWithDebInfo).

use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn fixture(name: &str) -> serde_json::Value {
    let text = match name {
        "symbol_pass" => include_str!("testdata/symbol_pass.json"),
        "compile_line_pass" => include_str!("testdata/compile_line_pass.json"),
        "pack_pass" => include_str!("testdata/pack_pass.json"),
        "search_no_claim" => include_str!("testdata/search_no_claim.json"),
        "unknown_setting" => include_str!("testdata/unknown_setting.json"),
        "wrong_user" => include_str!("testdata/wrong_user.json"),
        "bad_arguments" => include_str!("testdata/bad_arguments.json"),
        "bundle_pass" => include_str!("testdata/bundle_pass.json"),
        "find_pass" => include_str!("testdata/find_pass.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).expect("fixture json")
}

fn render(tool: &str, args: &str, outcome: Outcome) -> (String, CheckResult) {
    text_and_result(tool, args, outcome)
}

/// The answer leads with its build setting and commit, then the call
/// that shows the compile line, and ends with the shared result.
#[test]
fn a_symbol_answer_shows_setting_commit_and_the_compile_line() {
    let (text, result) = render(
        "context_symbol",
        r#"{"name":"push"}"#,
        Outcome::Answer(fixture("symbol_pass")),
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🔎 context_symbol · push");
    assert_eq!(
        lines[1],
        "Index: build setting relwithdebinfo, commit 59a6a938513e, committed layer, 0 commits behind your worktree"
    );
    assert_eq!(
        lines[2],
        r#"Show the compile line: context_compile_line {"path":"include/rigtorp/SPSCQueue.h","setting":"relwithdebinfo"}"#
    );
    assert!(lines[3].starts_with('{'), "{}", lines[3]);
    assert_eq!(
        text.lines().last(),
        Some("Result: PASS: answered from completed index snapshots")
    );
    assert!(result.is_pass());
    // The result record is the last line, not repeated inside the JSON body.
    assert!(!lines[3].contains("check_result"));
}

#[test]
fn the_compile_line_answer_does_not_point_at_itself() {
    let (text, result) = render(
        "context_compile_line",
        r#"{"path":"src/SPSCQueueTest.cpp"}"#,
        Outcome::Answer(fixture("compile_line_pass")),
    );
    assert!(
        text.lines()
            .nth(1)
            .unwrap_or_default()
            .starts_with("Index: build setting relwithdebinfo, commit 59a6a938513e")
    );
    assert!(!text.contains("Show the compile line:"), "{text}");
    assert!(text.contains("production_command"));
    assert!(result.is_pass());
}

#[test]
fn a_pack_names_its_settings_and_commits() {
    let (text, result) = render(
        "context_pack",
        r#"{"task":"why can push block"}"#,
        Outcome::Answer(fixture("pack_pass")),
    );
    assert_eq!(
        text.lines().nth(1),
        Some("Index: build setting relwithdebinfo, commit 59a6a938513e")
    );
    assert!(
        text.lines()
            .nth(2)
            .unwrap_or_default()
            .starts_with("Show the compile line: context_compile_line {\"path\":"),
        "{text}"
    );
    assert!(result.is_pass());
}

#[test]
fn nothing_found_is_no_claim_and_still_names_the_index() {
    let (text, result) = render(
        "context_search",
        r#"{"query":"zzqqxxnotthere"}"#,
        Outcome::Answer(fixture("search_no_claim")),
    );
    assert_eq!(result.verdict, check_result::Verdict::NoClaim);
    assert!(text.contains("Index: build setting relwithdebinfo, commit 59a6a938513e"));
    // No file was named, so there is no compile line to show.
    assert!(!text.contains("Show the compile line:"), "{text}");
}

/// The engine's own fix names its internal command; the user gets ours.
#[test]
fn an_index_that_cannot_answer_is_not_run_with_our_fix() {
    let (text, result) = render(
        "context_symbol",
        r#"{"name":"push","setting":"nosuch"}"#,
        Outcome::Answer(fixture("unknown_setting")),
    );
    assert_eq!(result.verdict, check_result::Verdict::NotRun);
    assert_eq!(result.reason, Some(NotRunReason::ServiceUnavailable));
    assert_eq!(result.fix.as_deref(), Some(FIX_INDEX));
    assert!(text.contains("index unavailable"), "{text}");
    assert!(!text.contains("bctx"), "internal command leaked: {text}");
    assert_eq!(result.validate(), Ok(()));
}

#[test]
fn access_refused_by_the_walls_is_not_run_and_carries_no_data() {
    let (text, result) = render(
        "context_symbol",
        r#"{"name":"push"}"#,
        Outcome::Answer(fixture("wrong_user")),
    );
    assert_eq!(result.reason, Some(NotRunReason::NeedsApproval));
    assert!(!text.contains("SPSCQueue.h"), "{text}");
}

#[test]
fn the_servers_argument_error_is_bad_arguments() {
    let (_, result) = render(
        "context_symbol",
        r#"{"limit":3}"#,
        Outcome::Answer(fixture("bad_arguments")),
    );
    assert_eq!(result.reason, Some(NotRunReason::BadArguments));
}

#[test]
fn every_failure_is_not_run_with_a_reason_and_a_fix() {
    let cases = [
        (
            Outcome::Failed(ClientError::Start("no python3".into())),
            NotRunReason::ToolMissing,
        ),
        (
            Outcome::Failed(ClientError::Timeout(Duration::from_secs(60))),
            NotRunReason::TimeLimit,
        ),
        (
            Outcome::Failed(ClientError::Broken {
                what: "stopped".into(),
                stderr: "a\nb\nTraceback: boom".into(),
            }),
            NotRunReason::ToolError,
        ),
        (
            Outcome::BadArguments("unknown argument `x`".into()),
            NotRunReason::BadArguments,
        ),
        (
            Outcome::Mismatch("different tools".into()),
            NotRunReason::ToolError,
        ),
        (
            Outcome::Answer(
                serde_json::json!({"error": {"code": -32601, "message": "unknown method"}}),
            ),
            NotRunReason::ToolError,
        ),
        (
            Outcome::Answer(
                serde_json::json!({"content": [{"type": "text", "text": "{}"}], "isError": false}),
            ),
            NotRunReason::ToolError,
        ),
    ];
    for (outcome, reason) in cases {
        let (text, result) = render("context_symbol", r#"{"name":"push"}"#, outcome);
        assert_eq!(result.reason, Some(reason), "{text}");
        assert_eq!(result.validate(), Ok(()), "{text}");
        assert!(
            text.lines()
                .last()
                .unwrap_or_default()
                .starts_with("Result: NOT RUN"),
            "{text}"
        );
        assert!(!result.is_pass());
    }
}

#[test]
fn a_broken_server_shows_its_last_error_lines_as_evidence() {
    let (_, result) = render(
        "context_symbol",
        "{}",
        Outcome::Failed(ClientError::Broken {
            what: "stopped".into(),
            stderr: "line1\nTraceback (most recent call last):\nModuleNotFoundError: No module named 'bctx'".into(),
        }),
    );
    assert!(
        result
            .evidence
            .iter()
            .any(|line| line.contains("No module named 'bctx'"))
    );
}

/// A result record the server did not fill properly is never read as PASS.
#[test]
fn a_malformed_result_record_is_never_pass() {
    let mut answer = fixture("symbol_pass");
    answer["structuredContent"]["check_result"]["verdict"] = serde_json::json!("GREAT");
    let (_, result) = render("context_symbol", "{}", Outcome::Answer(answer));
    assert_eq!(result.verdict, check_result::Verdict::NotRun);
    let mut answer = fixture("symbol_pass");
    answer["structuredContent"]["check_result"]["summary"] = serde_json::json!("");
    let (_, result) = render("context_symbol", "{}", Outcome::Answer(answer));
    assert_eq!(result.verdict, check_result::Verdict::NotRun);
}

#[test]
fn approximate_answers_say_how_much_is_approximate() {
    let mut answer = fixture("symbol_pass");
    answer["structuredContent"]["answers"][0]["approximate_line_share"] = serde_json::json!(0.024);
    answer["structuredContent"]["check_result"] = serde_json::json!({
        "schema": 1, "check": "context_symbol", "verdict": "APPROXIMATE",
        "summary": "some results come from files the pinned clang could not parse exactly"});
    let (text, result) = render("context_symbol", "{}", Outcome::Answer(answer));
    assert_eq!(result.verdict, check_result::Verdict::Approximate);
    assert!(text.contains(", 2.4% of lines approximate"), "{text}");
}

#[test]
fn several_settings_get_one_index_line_each() {
    let body = serde_json::json!({"answers": [
        {"setting": "debug", "commit": "aaaaaaaaaaaaaaaa", "layer": "committed"},
        {"setting": "release", "commit": "bbbbbbbbbbbbbbbb", "layer": "committed+overlay", "commits_behind": 1},
    ]});
    assert_eq!(
        index_lines(&body),
        vec![
            "Index: build setting debug, commit aaaaaaaaaaaa, committed layer".to_string(),
            "Index: build setting release, commit bbbbbbbbbbbb, committed layer + your uncommitted changes, 1 commit behind your worktree".to_string(),
        ]
    );
}

/// The task-level tools answer from one setting: the index line still leads,
/// and the bundle's text reaches the model as text, not as an escaped string.
#[test]
fn a_bundle_answer_is_readable_text_after_the_index_line() {
    let (text, result) = render(
        "context_bundle",
        r#"{"task":"Reporter prints the wrong test count"}"#,
        Outcome::Answer(fixture("bundle_pass")),
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "🔎 context_bundle · Reporter prints the wrong test count"
    );
    assert_eq!(lines[1], "Index: build setting dev, commit 7ce35799767d");
    assert!(
        text.contains("\n## Where to look (most likely first)\n1. Catch::ConsoleReporter::testRunEnded"),
        "{text}"
    );
    // the JSON body after the text no longer carries it
    let body = lines
        .iter()
        .rev()
        .find(|line| line.starts_with('{'))
        .expect("json body");
    assert!(!body.contains("## Where to look"), "{body}");
    assert!(result.is_pass());
}

#[test]
fn a_find_answer_names_its_setting_and_commit() {
    let (text, result) = render(
        "context_find",
        r#"{"name":"StreamingReporterBase","kind":"class"}"#,
        Outcome::Answer(fixture("find_pass")),
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "🔎 context_find · StreamingReporterBase");
    assert_eq!(lines[1], "Index: build setting dev, commit 7ce35799767d");
    assert!(lines[2].starts_with("Show the compile line: context_compile_line"), "{}", lines[2]);
    assert!(result.is_pass());
}
