//! The `byteask-port-check` CLI: describe, adapters, and check's NOT RUN paths (the full check
//! is covered by the library's end-to-end tests).

use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

const EXE: &str = env!("CARGO_BIN_EXE_byteask-port-check");

#[test]
fn describe_prints_the_tool_spec() {
    let out = Command::new(EXE).arg("describe").output().expect("run");
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["name"], "port_check");
    assert_eq!(v, byteask_port_check::tool::tool_spec());
}

#[test]
fn adapters_writes_both_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(EXE)
        .arg("adapters")
        .arg(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("port_harness.py")).expect("py"),
        byteask_port_check::PYTHON_ADAPTER
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("port_harness.hpp")).expect("hpp"),
        byteask_port_check::CPP_ADAPTER
    );
}

#[test]
fn check_answers_not_run_with_exit_2_for_a_bad_or_missing_request() {
    let mut child = Command::new(EXE)
        .args(["check", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{not json")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["status"], "NOT RUN");
    let out = Command::new(EXE)
        .args(["check", "/nonexistent/request.json"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(
        v["sentence"]
            .as_str()
            .unwrap_or("")
            .contains("cannot read the request"),
        "{v}"
    );
    let out = Command::new(EXE).arg("nonsense").output().expect("run");
    assert_eq!(out.status.code(), Some(2));
}
