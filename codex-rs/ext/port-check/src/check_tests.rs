//! End-to-end tests of port_check with a real Python strategy and real g++ builds of its C++
//! port, including planted defects. These need `python3` and `g++` on PATH and fail (they do not
//! skip) when either is missing, because a skipped check here would read as a passing one.

use super::*;
use crate::check::DiagTolerance;
use crate::session::ProcessSpec;
use crate::sim::HarnessConfig;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tool(name: &str) {
    let ok = std::process::Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "{name} is required for the port_check end-to-end tests and was not found on PATH"
    );
}

/// Build the C++ port, optionally with planted defects (`-DPLANT_...`).
fn build_cpp(dir: &Path, defines: &[&str]) -> PathBuf {
    tool("g++");
    let out = dir.join(format!("imbalance{}", defines.join("_").replace('=', "-")));
    let mut cmd = std::process::Command::new("g++");
    cmd.args([
        "-std=c++17",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-pedantic",
    ])
    .arg("-I")
    .arg(manifest().join("adapters"))
    .arg(manifest().join("tests/fixtures/imbalance.cpp"))
    .arg("-o")
    .arg(&out);
    for d in defines {
        cmd.arg(format!("-D{d}"));
    }
    let o = cmd.output().expect("run g++");
    assert!(
        o.status.success(),
        "g++ failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    out
}

/// A synthetic, internally consistent market: two symbols, adds, partial cancels, deletes,
/// executions, replaces and trade prints; the book never crosses; some events share a time.
fn write_recording(dir: &Path, events: usize, seed: u64) -> PathBuf {
    let mut state = seed;
    let mut rnd = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    #[derive(Clone)]
    struct Live {
        id: u64,
        buy: bool,
        px: i64,
        qty: i64,
    }
    let syms = ["AAA", "BBB"];
    let mut books: Vec<Vec<Live>> = vec![Vec::new(), Vec::new()];
    let mut next_id = 1000u64;
    let mut ts = 34_200_000_000_000i64;
    let mut lines = Vec::new();
    let best = |b: &Vec<Live>, buy: bool| -> Option<i64> {
        let it = b.iter().filter(|o| o.buy == buy).map(|o| o.px);
        if buy { it.max() } else { it.min() }
    };
    for i in 0..events {
        if rnd(5) != 0 {
            ts += 1 + rnd(4000) as i64;
        }
        let s = rnd(2) as usize;
        let book = &mut books[s];
        let op = if book.len() < 6 || i < 20 {
            0
        } else {
            rnd(100)
        };
        let line = if op < 42 {
            let buy = rnd(2) == 0;
            let mut px = 1000 + rnd(9) as i64 - 4;
            if buy {
                if let Some(a) = best(book, false) {
                    px = px.min(a - 1);
                }
            } else if let Some(b) = best(book, true) {
                px = px.max(b + 1);
            }
            let qty = 1 + rnd(20) as i64;
            next_id += 1;
            book.push(Live {
                id: next_id,
                buy,
                px,
                qty,
            });
            json!({"ts": ts, "type": "add", "sym": syms[s], "id": next_id, "side": if buy {"B"} else {"S"}, "px": px, "qty": qty})
        } else if op < 56 {
            let k = rnd(book.len() as u64) as usize;
            if book[k].qty > 1 {
                let q = 1 + rnd(book[k].qty as u64 - 1) as i64;
                book[k].qty -= q;
                json!({"ts": ts, "type": "cancel", "id": book[k].id, "qty": q})
            } else {
                let o = book.swap_remove(k);
                json!({"ts": ts, "type": "delete", "id": o.id})
            }
        } else if op < 70 {
            let k = rnd(book.len() as u64) as usize;
            let o = book.swap_remove(k);
            json!({"ts": ts, "type": "delete", "id": o.id})
        } else if op < 85 {
            // Execute against the best order of a random side.
            let buy = rnd(2) == 0;
            let Some(px) = best(book, buy) else { continue };
            let k = book
                .iter()
                .position(|o| o.buy == buy && o.px == px)
                .unwrap_or(0);
            let q = 1 + rnd(book[k].qty as u64) as i64;
            let q = q.min(book[k].qty);
            book[k].qty -= q;
            let id = book[k].id;
            if book[k].qty == 0 {
                book.swap_remove(k);
            }
            json!({"ts": ts, "type": "exec", "id": id, "qty": q})
        } else if op < 95 {
            let k = rnd(book.len() as u64) as usize;
            let mut o = book.swap_remove(k);
            let mut px = o.px + rnd(3) as i64 - 1;
            if o.buy {
                if let Some(a) = best(book, false) {
                    px = px.min(a - 1);
                }
            } else if let Some(b) = best(book, true) {
                px = px.max(b + 1);
            }
            next_id += 1;
            let old = o.id;
            o.id = next_id;
            o.px = px;
            o.qty = 1 + rnd(20) as i64;
            let v = json!({"ts": ts, "type": "replace", "id": old, "new_id": o.id, "px": o.px, "qty": o.qty});
            book.push(o);
            v
        } else {
            json!({"ts": ts, "type": "trade", "sym": syms[s], "side": "B", "px": 1000, "qty": 1 + rnd(5)})
        };
        lines.push(line.to_string());
    }
    let path = dir.join("market.jsonl");
    let mut f = std::fs::File::create(&path).expect("create");
    for l in lines {
        writeln!(f, "{l}").expect("write");
    }
    path
}

fn request(dir: &Path, recording: &Path, cpp: &Path, params: &[(&str, &str)]) -> PortCheckRequest {
    tool("python3");
    let mut harness = HarnessConfig::standard();
    harness.seed = 20261018;
    harness.reject_every = 7;
    harness.max_position = Some(40);
    for (k, v) in params {
        harness.params.insert((*k).to_string(), (*v).to_string());
    }
    let mut py_env = BTreeMap::new();
    py_env.insert(
        "PYTHONPATH".to_string(),
        manifest().join("adapters").display().to_string(),
    );
    PortCheckRequest {
        recording: recording.to_path_buf(),
        recording_format: Default::default(),
        initial_book: None,
        python: ProcessSpec {
            argv: vec![
                "python3".into(),
                manifest()
                    .join("tests/fixtures/imbalance.py")
                    .display()
                    .to_string(),
            ],
            env: py_env,
            cwd: None,
            launch: None,
        },
        cpp: ProcessSpec {
            argv: vec![cpp.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            launch: None,
        },
        harness,
        diag_tolerance: DiagTolerance::default(),
        limits: Default::default(),
        python_runs: 2,
        cpp_runs: 2,
        workdir: Some(dir.join("work")),
    }
}

fn diff_of(r: &PortCheckResult) -> Value {
    r.first_difference.clone().expect("a first difference")
}

/// The step at which the Python run sent its Nth IOC order.
fn step_of_nth_ioc(transcript: &Path, n: usize) -> (u64, i64) {
    let text = std::fs::read_to_string(transcript).expect("transcript");
    let mut seen = 0;
    for line in text.lines() {
        let row: Value = serde_json::from_str(line).expect("row");
        for a in row
            .pointer("/out/actions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if a["op"] == "new" && a["tif"] == "ioc" {
                seen += 1;
                if seen == n {
                    return (
                        row["step"].as_u64().expect("step"),
                        a["qty"].as_i64().expect("qty"),
                    );
                }
            }
        }
    }
    panic!("fewer than {n} IOC orders in {}", transcript.display());
}

#[test]
fn an_honest_port_passes_and_says_exactly_what_was_exercised() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 600, 7);
    let cpp = build_cpp(dir.path(), &[]);
    let r = port_check(&request(dir.path(), &rec, &cpp, &[]));
    assert_eq!(
        r.status,
        Status::Pass,
        "{}\n{}",
        r.sentence,
        serde_json::to_string_pretty(&r.evidence).unwrap_or_default()
    );
    assert!(
        r.sentence
            .starts_with("PASS: on all 600 records of market.jsonl"),
        "{}",
        r.sentence
    );
    let cov = &r.evidence["coverage"];
    for k in [
        "orders",
        "fills",
        "rejects",
        "cancels",
        "timers",
        "decisions",
    ] {
        assert!(cov[k].as_u64().unwrap_or(0) > 0, "{k} not exercised: {cov}");
    }
    assert_eq!(
        r.evidence["self_check"]["python"],
        json!("reproduced exactly in 2 runs")
    );
    assert_eq!(
        r.evidence["self_check"]["cpp"],
        json!("reproduced exactly in 2 runs")
    );
    assert_eq!(r.evidence["recording"]["records"], json!(600));
    assert_eq!(
        r.evidence["cpp"]["executable_sha256"]
            .as_str()
            .map(str::len),
        Some(64)
    );
    assert_eq!(
        r.evidence["python"]["script_sha256"].as_str().map(str::len),
        Some(64)
    );
    let runs = r.evidence["python"]["runs"].as_array().expect("runs");
    assert_eq!(runs[0]["varied"]["PYTHONHASHSEED"], json!("0"));
    assert_eq!(runs[1]["varied"]["PYTHONHASHSEED"], json!("1234567"));
    assert_eq!(
        r.evidence["cpp"]["runs"][1]["varied"]["MALLOC_PERTURB_"],
        json!("165")
    );
    assert_eq!(r.perf["status"], json!("NOT RUN"));
}

#[test]
fn a_planted_order_divergence_is_found_at_the_exact_step_and_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 600, 7);
    let cpp = build_cpp(dir.path(), &["PLANT_QTY_AT=3"]);
    let r = port_check(&request(dir.path(), &rec, &cpp, &[]));
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    let (step, py_qty) = step_of_nth_ioc(&dir.path().join("work/python-1.transcript.jsonl"), 3);
    let d = diff_of(&r);
    assert_eq!(d["comparison"], json!("python-1 vs cpp-1"));
    assert_eq!(d["difference"]["key"], json!(step), "{d}");
    assert_eq!(d["difference"]["path"], json!("/out/actions/0/qty"));
    assert_eq!(d["difference"]["a"], json!(py_qty));
    assert_eq!(d["difference"]["b"], json!(py_qty + 1));
    assert!(
        r.sentence.contains(&format!("at step {step} (md event")),
        "{}",
        r.sentence
    );
    assert!(r.sentence.contains("recording record"), "{}", r.sentence);
}

#[test]
fn no_tolerance_reaches_orders_however_large_the_diag_tolerance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 600, 7);
    let cpp = build_cpp(dir.path(), &["PLANT_QTY_AT=3"]);
    let mut req = request(dir.path(), &rec, &cpp, &[]);
    req.diag_tolerance = DiagTolerance {
        abs: 1e12,
        rel: 0.5,
        ulps: 1 << 40,
    };
    let r = port_check(&req);
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    assert_eq!(
        diff_of(&r)["difference"]["path"],
        json!("/out/actions/0/qty")
    );
}

#[test]
fn a_one_ulp_diagnostic_difference_fails_exact_and_passes_within_one_ulp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 300, 11);
    let cpp = build_cpp(dir.path(), &["PLANT_DIAG_ULP"]);
    let mut req = request(dir.path(), &rec, &cpp, &[]);
    let r = port_check(&req);
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    assert_eq!(diff_of(&r)["difference"]["path"], json!("/out/diag/fair"));
    assert_eq!(diff_of(&r)["difference"]["rule"], json!("exact"));
    req.diag_tolerance.ulps = 1;
    req.workdir = Some(dir.path().join("work2"));
    let r = port_check(&req);
    assert_eq!(r.status, Status::Pass, "{}", r.sentence);
    assert!(
        r.sentence.contains("diagnostics compared within 1 ulps"),
        "{}",
        r.sentence
    );
}

#[test]
fn python_that_depends_on_hash_order_is_not_reproducible_so_nothing_is_claimed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 100, 3);
    let cpp = build_cpp(dir.path(), &[]);
    let r = port_check(&request(
        dir.path(),
        &rec,
        &cpp,
        &[("variant", "hash_order")],
    ));
    assert_eq!(r.status, Status::NotRun, "{}", r.sentence);
    assert!(r.sentence.contains("not reproducible"), "{}", r.sentence);
    assert!(r.sentence.contains("PYTHONHASHSEED"), "{}", r.sentence);
    assert_eq!(diff_of(&r)["comparison"], json!("python-1 vs python-2"));
    assert_eq!(
        diff_of(&r)["difference"]["key"],
        json!(0),
        "it decides at init (step 0)"
    );
    assert!(
        r.evidence.get("cpp").and_then(|c| c.get("runs")).is_none(),
        "C++ never ran"
    );
}

#[test]
fn python_that_reads_the_wall_clock_is_not_reproducible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 100, 3);
    let cpp = build_cpp(dir.path(), &[]);
    let r = port_check(&request(
        dir.path(),
        &rec,
        &cpp,
        &[("variant", "wall_clock")],
    ));
    assert_eq!(r.status, Status::NotRun, "{}", r.sentence);
    assert!(r.sentence.contains("wall-clock"), "{}", r.sentence);
}

#[test]
fn a_port_that_reads_uninitialised_memory_fails_its_own_reproducibility_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 300, 7);
    let cpp = build_cpp(dir.path(), &["PLANT_UNINIT"]);
    let r = port_check(&request(dir.path(), &rec, &cpp, &[]));
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    assert_eq!(
        diff_of(&r)["comparison"],
        json!("cpp-1 vs cpp-2"),
        "{}",
        r.sentence
    );
    assert!(
        r.sentence.contains("uninitialised memory"),
        "{}",
        r.sentence
    );
}

#[test]
fn a_port_that_crashes_fails_at_the_step_it_crashed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 200, 7);
    let cpp = build_cpp(dir.path(), &["PLANT_CRASH_AT=40"]);
    let r = port_check(&request(dir.path(), &rec, &cpp, &[]));
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    let d = diff_of(&r);
    assert_eq!(d["difference"]["key"], json!(40), "{d}");
    assert!(r.sentence.contains("killed by signal 6"), "{}", r.sentence);
}

#[test]
fn a_port_that_stops_answering_is_not_run_not_a_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 200, 7);
    let cpp = build_cpp(dir.path(), &["PLANT_HANG_AT=30"]);
    let mut req = request(dir.path(), &rec, &cpp, &[]);
    req.limits.response_timeout_ms = Some(500);
    let r = port_check(&req);
    assert_eq!(r.status, Status::NotRun, "{}", r.sentence);
    assert!(
        r.sentence.contains("timed out at step 30"),
        "{}",
        r.sentence
    );
}

#[test]
fn a_strategy_that_never_trades_is_no_claim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 150, 7);
    let cpp = build_cpp(dir.path(), &[]);
    let r = port_check(&request(
        dir.path(),
        &rec,
        &cpp,
        &[("variant", "no_orders")],
    ));
    assert_eq!(r.status, Status::NoClaim, "{}", r.sentence);
    assert!(
        r.sentence.contains("neither sent a single order"),
        "{}",
        r.sentence
    );
}

#[test]
fn an_inconsistent_recording_is_refused_before_anything_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = dir.path().join("bad.jsonl");
    std::fs::write(
        &rec,
        "{\"ts\":1,\"type\":\"add\",\"sym\":\"A\",\"id\":1,\"side\":\"B\",\"px\":10,\"qty\":5}\n{\"ts\":2,\"type\":\"cancel\",\"id\":2,\"qty\":1}\n",
    )
    .expect("write");
    let req = request(dir.path(), &rec, Path::new("/nonexistent"), &[]);
    let r = port_check(&req);
    assert_eq!(r.status, Status::NotRun);
    assert!(
        r.sentence
            .contains("record 1: order id 2 is not in the book"),
        "{}",
        r.sentence
    );
    assert!(
        r.sentence.contains("initial_book"),
        "the fix is named: {}",
        r.sentence
    );
    assert!(!dir.path().join("work").exists(), "nothing ran");
}

#[test]
fn one_python_run_is_refused_because_reproduction_comes_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 50, 7);
    let mut req = request(dir.path(), &rec, Path::new("/nonexistent"), &[]);
    req.python_runs = 1;
    let r = port_check(&req);
    assert_eq!(r.status, Status::NotRun);
    assert!(
        r.sentence.contains("python_runs must be 2-5"),
        "{}",
        r.sentence
    );
}

#[test]
fn a_cpp_binary_that_cannot_start_is_not_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 50, 7);
    let r = port_check(&request(
        dir.path(),
        &rec,
        Path::new("/nonexistent/strategy"),
        &[],
    ));
    assert_eq!(r.status, Status::NotRun, "{}", r.sentence);
    assert!(r.sentence.contains("could not start"), "{}", r.sentence);
}

#[test]
fn the_json_entry_point_runs_the_same_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 200, 7);
    let cpp = build_cpp(dir.path(), &[]);
    let req = request(dir.path(), &rec, &cpp, &[]);
    let text = crate::tool::run_json(&serde_json::to_string(&req).expect("encode"));
    let v: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(v["tool"], json!("port_check"));
    assert_eq!(v["status"], json!("PASS"), "{text}");
}

#[test]
fn an_honest_port_passes_on_the_hft_pack_itch_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let itch = manifest().join("../hft-pack/fixtures/itch50/synthetic-20261018.itch");
    let cpp = build_cpp(dir.path(), &[]);
    let mut req = request(dir.path(), &itch, &cpp, &[("max_pos", "900")]);
    req.recording_format = crate::check::RecordingFormat::Itch50;
    req.harness.tick = 100;
    let r = port_check(&req);
    assert_eq!(r.status, Status::Pass, "{}", r.sentence);
    assert_eq!(r.evidence["recording"]["format"], json!("itch50"));
    assert!(
        r.evidence["coverage"]["fills"].as_u64().unwrap_or(0) > 0,
        "{}",
        r.evidence["coverage"]
    );
    // The same capture through the planted port fails, at the step of its third IOC order.
    let bad = build_cpp(dir.path(), &["PLANT_QTY_AT=3"]);
    let mut req = request(dir.path(), &itch, &bad, &[("max_pos", "900")]);
    req.recording_format = crate::check::RecordingFormat::Itch50;
    req.harness.tick = 100;
    req.workdir = Some(dir.path().join("work-bad"));
    let r = port_check(&req);
    assert_eq!(r.status, Status::Fail, "{}", r.sentence);
    let (step, _) = step_of_nth_ioc(&dir.path().join("work-bad/python-1.transcript.jsonl"), 3);
    assert_eq!(diff_of(&r)["difference"]["key"], json!(step));
}

#[test]
fn an_initial_book_is_applied_before_the_first_event_and_shown_as_a_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let generated = write_recording(dir.path(), 200, 7);
    let book = dir.path().join("book.jsonl");
    std::fs::write(
        &book,
        "{\"ts\":0,\"type\":\"add\",\"sym\":\"AAA\",\"id\":1,\"side\":\"B\",\"px\":990,\"qty\":7}\n\
         {\"ts\":0,\"type\":\"add\",\"sym\":\"AAA\",\"id\":2,\"side\":\"S\",\"px\":1010,\"qty\":9}\n",
    )
    .expect("write book");
    // The recording starts by deleting an order only the initial book holds.
    let rec = dir.path().join("with-book.jsonl");
    let mut text = String::from("{\"ts\":34199999999999,\"type\":\"delete\",\"id\":1}\n");
    text.push_str(&std::fs::read_to_string(&generated).expect("read"));
    std::fs::write(&rec, text).expect("write");
    let cpp = build_cpp(dir.path(), &[]);
    let mut req = request(dir.path(), &rec, &cpp, &[]);
    let refused = port_check(&req);
    assert_eq!(refused.status, Status::NotRun, "{}", refused.sentence);
    assert!(
        refused
            .sentence
            .contains("record 0: order id 1 is not in the book"),
        "{}",
        refused.sentence
    );
    req.initial_book = Some(book);
    req.workdir = Some(dir.path().join("work-book"));
    let r = port_check(&req);
    assert_eq!(r.status, Status::Pass, "{}", r.sentence);
    assert_eq!(r.evidence["recording"]["records"], json!(201));
    let first_rows: Vec<Value> =
        std::fs::read_to_string(dir.path().join("work-book/python-1.transcript.jsonl"))
            .expect("transcript")
            .lines()
            .take(3)
            .map(|l| serde_json::from_str(l).expect("row"))
            .collect();
    assert_eq!(first_rows[1]["in"]["kind"], json!("snapshot"));
    assert_eq!(first_rows[1]["in"]["sym"], json!("AAA"));
    assert_eq!(first_rows[1]["in"]["bids"], json!([[990, 7]]));
    assert_eq!(first_rows[1]["in"]["asks"], json!([[1010, 9]]));
}

#[test]
fn the_skills_freeze_check_python_in_both_slots_passes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 300, 7);
    let mut req = request(dir.path(), &rec, Path::new("/unused"), &[]);
    req.cpp = req.python.clone();
    let r = port_check(&req);
    assert_eq!(r.status, Status::Pass, "{}", r.sentence);
}

#[test]
fn the_adapters_own_unit_tests_pass() {
    tool("python3");
    let py = std::process::Command::new("python3")
        .arg("test_port_harness.py")
        .current_dir(manifest().join("adapters"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stderr)
    );
    tool("g++");
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("tph");
    let cc = std::process::Command::new("g++")
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-pedantic"])
        .arg(manifest().join("adapters/test_port_harness.cpp"))
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("run g++");
    assert!(
        cc.status.success(),
        "{}",
        String::from_utf8_lossy(&cc.stderr)
    );
    let run = std::process::Command::new(&exe).output().expect("run");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn the_shipped_adapters_are_the_files_in_the_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = write_adapters(dir.path()).expect("write");
    assert_eq!(paths.len(), 2);
    for (p, name) in paths.iter().zip(["port_harness.py", "port_harness.hpp"]) {
        assert_eq!(
            std::fs::read_to_string(p).expect("read"),
            std::fs::read_to_string(manifest().join("adapters").join(name)).expect("read repo")
        );
    }
}

/// Integration (the engine's tool sandbox): the host may start each version through a wrapper
/// (`launch`), while `argv` stays what is fingerprinted and reported. Here the C++ `argv`
/// names a program that does not exist and `launch` starts the real binary through `env`, so
/// the check passes only if `launch` is what was started; the evidence still names `argv`.
#[test]
fn the_host_launch_command_is_started_and_argv_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = write_recording(dir.path(), 200, 7);
    let cpp = build_cpp(dir.path(), &[]);
    let mut req = request(dir.path(), &rec, Path::new("/nonexistent/strategy"), &[]);
    req.cpp.launch = Some(vec!["/usr/bin/env".to_string(), cpp.display().to_string()]);
    let r = port_check(&req);
    assert_eq!(r.status, Status::Pass, "{}", r.sentence);
    assert_eq!(
        r.evidence["cpp"]["argv"],
        json!(["/nonexistent/strategy"]),
        "{}",
        r.evidence
    );
}

/// A request can never choose the launch command: only the host sets it.
#[test]
fn a_request_cannot_set_the_launch_command() {
    let text =
        r#"{"recording":"r","python":{"argv":["python3"],"launch":["sh"]},"cpp":{"argv":["x"]}}"#;
    let r = crate::tool::run_request_json(text);
    assert_eq!(r.status, Status::NotRun, "{}", r.sentence);
    assert!(r.sentence.contains("launch"), "{}", r.sentence);
}
