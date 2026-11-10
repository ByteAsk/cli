#![allow(clippy::expect_used)]
//! The `byteask-replay` CLI: JSON out, exit 0 same / 1 different / 2 could not compare.

use serde_json::Value;
use std::process::Command;

fn cli(args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_byteask-replay")).args(args).output().expect("run");
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    (out.status.code().unwrap_or(-1), v)
}

#[test]
fn compare_reports_the_first_difference_and_exits_by_outcome() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.jsonl");
    let b = dir.path().join("b.jsonl");
    std::fs::write(&a, "{\"seq\":0,\"px\":1,\"wall\":5}\n{\"seq\":1,\"px\":2,\"wall\":6}\n").expect("write");
    std::fs::write(&b, "{\"seq\":0,\"px\":1,\"wall\":7}\n{\"seq\":1,\"px\":2,\"wall\":8}\n").expect("write");
    let (code, v) = cli(&["compare", a.to_str().expect("utf8"), b.to_str().expect("utf8")]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["outcome"], "different");
    assert_eq!(v["comparison"]["first_difference"]["path"], "/wall");
    let rules = dir.path().join("rules.json");
    std::fs::write(&rules, "{\"ignore\":[\"/wall\"],\"key\":\"/seq\"}").expect("write");
    let (code, v) = cli(&["compare", "--rules", rules.to_str().expect("utf8"), a.to_str().expect("utf8"), b.to_str().expect("utf8")]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["outcome"], "same");
    std::fs::write(&rules, "{\"tolerance\":1}").expect("write");
    let (code, v) = cli(&["compare", "--rules", rules.to_str().expect("utf8"), a.to_str().expect("utf8"), b.to_str().expect("utf8")]);
    assert_eq!(code, 2, "a typo in the rules is refused: {v}");
    assert_eq!(v["outcome"], "not_run");
    let (code, _) = cli(&["compare", a.to_str().expect("utf8")]);
    assert_eq!(code, 2);
}

#[test]
fn run_feeds_one_input_to_two_programs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.jsonl");
    std::fs::write(&input, "{\"x\":1}\n{\"x\":2}\n").expect("write");
    let spec = dir.path().join("spec.json");
    let spec_text = serde_json::json!({
        "input": input,
        "a": {"name": "old", "argv": ["/bin/sh", "-c", "cat"]},
        "b": {"name": "new", "argv": ["/bin/sh", "-c", "sed 's/2/3/'"]},
        "timeout_ms": 10000
    });
    std::fs::write(&spec, spec_text.to_string()).expect("write");
    let work = dir.path().join("work");
    let (code, v) = cli(&["run", spec.to_str().expect("utf8"), "--workdir", work.to_str().expect("utf8")]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["sentence"], "First difference at record 1: field /x is 2 in old and 3 in new (rule: exact).");
}
