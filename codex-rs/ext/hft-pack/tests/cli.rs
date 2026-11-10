#![allow(clippy::expect_used)]
//! The `byteask-itch` CLI on the checked-in fixture.

use byteask_replay::Rules;
use byteask_replay::compare_records;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_byteask-itch");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/itch50")
        .join(name)
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

#[test]
fn decode_and_messages_print_exactly_the_golden_files() {
    for (cmd, golden) in [
        ("decode", "synthetic-20261018.events.jsonl"),
        ("messages", "synthetic-20261018.messages.jsonl"),
    ] {
        let out = Command::new(EXE)
            .arg(cmd)
            .arg(fixture("synthetic-20261018.itch"))
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let want = lines(&std::fs::read(fixture(golden)).expect("golden"));
        let c = compare_records(&want, &lines(&out.stdout), &Rules::exact()).expect("compare");
        assert!(
            c.same,
            "{cmd}: {}",
            byteask_replay::report::sentence(&c, "golden", "cli")
        );
    }
}

#[test]
fn synthetic_regenerates_the_fixture_bytes_and_a_truncated_file_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out_file = dir.path().join("s.itch");
    let st = Command::new(EXE)
        .args(["synthetic", "20261018", "400"])
        .arg(&out_file)
        .status()
        .expect("run");
    assert!(st.success());
    let bytes = std::fs::read(&out_file).expect("read");
    assert!(bytes == std::fs::read(fixture("synthetic-20261018.itch")).expect("fixture"));
    let cut = dir.path().join("cut.itch");
    std::fs::write(&cut, &bytes[..bytes.len() - 5]).expect("write");
    let out = Command::new(EXE)
        .arg("decode")
        .arg(&cut)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("truncated"));
}
