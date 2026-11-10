//! The independent reproduction-test writer end to end with a stub child host (no model):
//! a scripted writer produces tests in its worktree; the real qualification, freezing,
//! delivery, `run-repro.sh`, leak removal and cleanup run against a temp git repository.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codex_subagents::host::ChildSpec;
use codex_subagents::orchestrator::Orchestration;
use codex_subagents::orchestrator::Params;
use codex_subagents::orchestrator::StepCounters;
use codex_subagents::settings::Settings;
use common::*;
use pretty_assertions::assert_eq;

fn params(repo: &Path, home: &Path, settings: Settings, id: &str) -> Params {
    Params {
        run_repro: settings.repro_tests,
        settings,
        repo: repo.to_path_buf(),
        task: TASK.to_string(),
        root_id: id.to_string(),
        codex_home: home.to_path_buf(),
    }
}

fn report_json(home: &Path, id: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(home.join("byteask-subagents").join(format!("{id}.json")))
        .expect("report written");
    serde_json::from_str(&text).expect("report is json")
}

fn assert_clean(repo: &Path, scratch_root: &Path, id: &str) {
    assert_eq!(
        extra_worktrees(repo),
        Vec::<String>::new(),
        "worktrees left behind"
    );
    assert_eq!(byteask_refs(repo), "", "snapshot ref left behind");
    assert!(
        !scratch_root.join("byteask-subagents").join(id).exists(),
        "scratch dir left behind"
    );
}

async fn wait_for_delivery(host: &FakeHost) -> String {
    for _ in 0..300 {
        if let Some(m) = host.root_messages.lock().unwrap().first().cloned() {
            return m;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("frozen tests were never delivered");
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    text.split(start)
        .nth(1)
        .and_then(|s| s.split(end).next())
        .expect("marker present")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tests_are_qualified_frozen_delivered_and_kept_out_of_the_diff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let home = tmp.path().join("home");
    let scratch = tmp.path().join("scratch");
    let host = Arc::new(FakeHost::new(Box::new(repro_writer)));
    let orch = Orchestration::start(
        params(&repo, &home, settings(&scratch), "r1"),
        host.clone(),
        Arc::new(StepCounters::default()),
    )
    .await
    .expect("start");

    // The main agent keeps working; the tests arrive as a message while it runs.
    let msg = wait_for_delivery(&host).await;
    assert!(
        msg.contains("independent reproduction tests are ready"),
        "{msg}"
    );
    assert!(msg.contains("they fail when run"), "{msg}");
    assert!(msg.contains("Tests: add"), "{msg}");
    let script = between(&msg, "`sh ", "`").to_string();
    let patch = between(&msg, "your final diff): ", "\n").trim().to_string();

    // Against the buggy tree the frozen tests fail; after the fix they pass. The real tree
    // is never touched by the script.
    let run = |label: &str| {
        let out = std::process::Command::new("sh")
            .arg(&script)
            .output()
            .expect("run script");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(text.contains(label), "expected {label}: {text}");
    };
    run("BYTEASK RESULT: FAIL");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    fix(&repo, FIX_GOOD);
    run("BYTEASK RESULT: PASS");
    assert_eq!(git(&repo, &["status", "--porcelain"]).trim(), "M calc.sh");

    // The main agent (wrongly) leaves the frozen tests in its tree.
    git(&repo, &["apply", &patch]);
    assert!(repo.join("tests/repro_add.sh").exists());
    orch.finalize().await;

    assert!(
        !repo.join("tests/repro_add.sh").exists(),
        "frozen tests removed from the real tree"
    );
    assert_eq!(
        read(&repo, "calc.sh"),
        CALC.replace(BUG_LINE, FIX_GOOD),
        "root's fix kept"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]).trim(), "M calc.sh");
    let r = report_json(&home, "r1");
    assert_eq!(r["leaked_tests_removed"], true);
    assert_eq!(r["repro"]["note"], "qualified and frozen");
    assert_eq!(r["repro"]["delivered_to_root"], true);
    assert_eq!(r["repro"]["frozen"]["fails_because"], "they fail when run");
    assert_clean(&repo, &scratch, "r1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tests_that_pass_on_the_starting_code_are_dropped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let home = tmp.path().join("home");
    let scratch = tmp.path().join("scratch");
    let host = Arc::new(FakeHost::new(Box::new(|spec: &ChildSpec| {
        // Checks existing behaviour (mul), so it already passes: not discriminating. No patch
        // file either: the tests are taken from the writer's worktree.
        write(&spec.cwd, "tests/repro_mul.sh", SUITE);
        Some(
            "BYTEASK_REPORT {\"build\": \"sh build.sh\", \"run\": \"sh tests/repro_mul.sh\"}"
                .to_string(),
        )
    })));
    let orch = Orchestration::start(
        params(&repo, &home, settings(&scratch), "r2"),
        host.clone(),
        Arc::new(StepCounters::default()),
    )
    .await
    .expect("start");
    // Give the writer time to finish and be qualified before the root ends.
    tokio::time::sleep(Duration::from_secs(3)).await;
    orch.finalize().await;
    let r = report_json(&home, "r2");
    assert!(
        r["repro"]["note"]
            .as_str()
            .unwrap_or("")
            .contains("not discriminating"),
        "{}",
        r["repro"]["note"]
    );
    assert!(
        host.root_messages.lock().unwrap().is_empty(),
        "nothing delivered"
    );
    assert_clean(&repo, &scratch, "r2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tests_that_do_not_compile_on_base_qualify() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let home = tmp.path().join("home");
    let scratch = tmp.path().join("scratch");
    let host = Arc::new(FakeHost::new(Box::new(|spec: &ChildSpec| {
        // The test needs something the starting code lacks, so "building" with it fails.
        write(&spec.cwd, "tests/needs_api.sh", "#!/bin/sh\nexit 0\n");
        Some(
            "BYTEASK_REPORT {\"build\": \"sh build.sh && { [ ! -f tests/needs_api.sh ] || grep -q sum calc.sh; }\", \
\"run\": \"sh tests/needs_api.sh\"}"
                .to_string(),
        )
    })));
    let orch = Orchestration::start(
        params(&repo, &home, settings(&scratch), "r3"),
        host.clone(),
        Arc::new(StepCounters::default()),
    )
    .await
    .expect("start");
    let msg = wait_for_delivery(&host).await;
    assert!(msg.contains("they do not compile against it"), "{msg}");
    orch.finalize().await;
    assert_clean(&repo, &scratch, "r3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_step_cap_interrupts_the_writer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let home = tmp.path().join("home");
    let scratch = tmp.path().join("scratch");
    let mut host = FakeHost::new(Box::new(repro_writer));
    host.delay = Duration::from_secs(3600); // would run for an hour
    let host = Arc::new(host);
    let counters = Arc::new(StepCounters::default());
    let mut s = settings(&scratch);
    s.repro_max_steps = 5;
    let orch = Orchestration::start(
        params(&repo, &home, s, "r4"),
        host.clone(),
        counters.clone(),
    )
    .await
    .expect("start");
    // Simulate model requests reported by the token-usage hook.
    for _ in 0..50 {
        counters.bump("child-1");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    orch.finalize().await;
    let r = report_json(&home, "r4");
    // Interrupted by the cap (not by the root finishing): the writer's work was still
    // examined; without a final report there is no command to verify its tests with.
    assert_eq!(r["repro"]["state"], "Interrupted", "{}", r["repro"]);
    assert_eq!(
        r["repro"]["note"], "no build/run command reported; tests cannot be verified",
        "{}",
        r["repro"]
    );
    assert_clean(&repo, &scratch, "r4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finishing_the_root_never_waits_for_the_writer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let home = tmp.path().join("home");
    let scratch = tmp.path().join("scratch");
    let mut host = FakeHost::new(Box::new(repro_writer));
    host.delay = Duration::from_secs(3600);
    let host = Arc::new(host);
    let orch = Orchestration::start(
        params(&repo, &home, settings(&scratch), "r5"),
        host.clone(),
        Arc::new(StepCounters::default()),
    )
    .await
    .expect("start");
    fix(&repo, FIX_WRONG);
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(20), orch.finalize())
        .await
        .expect("finalize returns promptly with the writer still running");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(read(&repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_WRONG));
    let r = report_json(&home, "r5");
    assert_eq!(r["repro"]["note"], "cancelled before qualification");
    assert_clean(&repo, &scratch, "r5");
}
