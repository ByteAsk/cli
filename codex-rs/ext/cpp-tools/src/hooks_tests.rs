use super::*;
use crate::impact::tests::git;

fn repo() -> Option<tempfile::TempDir> {
    let dir = tempfile::tempdir().ok()?;
    std::fs::create_dir_all(dir.path().join("src")).ok()?;
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 1; }\n").ok()?;
    (git(dir.path(), &["init", "-q"])
        && git(dir.path(), &["add", "-A"])
        && git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]))
    .then_some(dir)
}

fn ctx(home: &Path, cwd: &Path) -> HookContext {
    // These tests exercise the gate, which is off by default since 0.2.0: switch it on for the
    // whole test process (nothing else in it reads the variable).
    static GATE_ON: std::sync::Once = std::sync::Once::new();
    // SAFETY: set once, before any hook in this process reads it.
    GATE_ON.call_once(|| unsafe { std::env::set_var(gate::ENV_GATE, "1") });
    HookContext {
        home: home.to_path_buf(),
        cwd: cwd.to_path_buf(),
        session_id: "s-1".to_string(),
        source: "claude-code".to_string(),
        native_exec: false,
        // The engine's sandbox helper when the test run provides one; without it the gate
        // answers NOT RUN (sandbox unavailable) before building.
        sandbox_helper: std::env::var_os("BYTEASK_TEST_SANDBOX_HELPER").map(PathBuf::from),
        // Without a helper the tests run programs unsandboxed, like every other unit test.
        relaxed: std::env::var_os("BYTEASK_TEST_SANDBOX_HELPER").is_none(),
        fast_tier: None,
    }
}

#[tokio::test]
async fn nothing_changed_or_no_session_start_means_no_claim() {
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    // No SessionStart snapshot: the agent's changes are unknown.
    assert_eq!(stop(&ctx).await, StopDecision::Allow { message: None, follow_up: false });
    session_start(&ctx, true).await;
    assert_eq!(stop(&ctx).await, StopDecision::Allow { message: None, follow_up: false }, "nothing changed");
}

#[tokio::test]
async fn a_failure_goes_back_to_the_agent_until_the_retry_limit_then_ends_as_fail() {
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    session_start(&ctx, true).await;
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 2; }\n").unwrap();
    // A file the change does not use: the gate's leftover check fails it.
    std::fs::write(dir.path().join("scratch.txt"), "notes").unwrap();

    let first = stop(&ctx).await;
    let StopDecision::Block { reason } = &first else {
        panic!("a FAIL must send the agent back: {first:?}");
    };
    assert!(reason.contains("FILES LEFT FROM THIS SESSION"), "{reason}");
    assert!(reason.contains("Result: FAIL"), "{reason}");
    // Unchanged code: the same failure again (not rebuilt), round 2 of 2.
    assert!(matches!(stop(&ctx).await, StopDecision::Block { .. }));
    // The retry limit: the turn ends, and it ends as FAIL, shown to the user.
    let StopDecision::Allow { message: Some(message), .. } = stop(&ctx).await else {
        panic!("the retry limit must let the turn end with a message");
    };
    assert!(message.contains("Result: FAIL"), "{message}");
    assert!(message.contains("Still failing"), "{message}");

    // The verdict is in the store, and the prompt hook reports it once.
    let entry = results_store::latest(home.path(), dir.path()).expect("stored");
    assert_eq!(entry.result.verdict, check_result::Verdict::Fail);
    assert_eq!(entry.source, "claude-code");
    let told = prompt(&ctx).await.expect("the latest result is reported");
    assert!(told.contains("Result: FAIL"), "{told}");
    assert!(prompt(&ctx).await.is_none(), "reported once");
}

#[tokio::test]
async fn a_check_that_cannot_run_lets_the_turn_end_and_says_why() {
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    session_start(&ctx, true).await;
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 3; }\n").unwrap();
    // Not trusted for native builds: NOT RUN, never a loop, never PASS.
    let decision = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), .. } = &decision else {
        panic!("NOT RUN must let the turn end: {decision:?}");
    };
    assert!(message.contains("Result: NOT RUN"), "{message}");
    // After an edit the stored result reads as stale in the next prompt.
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 4; }\n").unwrap();
    let told = prompt(&ctx).await.expect("reported");
    assert!(told.contains("STALE"), "{told}");
}

#[tokio::test]
async fn a_resumed_session_keeps_the_snapshot_it_started_with() {
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    session_start(&ctx, true).await;
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 5; }\n").unwrap();
    session_start(&ctx, false).await;
    // The edit made before the resume is still the agent's change.
    assert!(matches!(stop(&ctx).await, StopDecision::Allow { message: Some(_), .. }));
}

#[test]
fn the_hook_fast_tier_is_the_gate_default() {
    assert_eq!(FAST_TIER, crate::gate::GateSettings::default().fast_tier);
}

/// A CMake project whose one test takes a few seconds: it builds, but not within 1 s.
fn slow_cmake_repo() -> Option<tempfile::TempDir> {
    let have = |tool: &str| crate::tools::binary_on_path(tool);
    if !have("cmake") || !(have("c++") || have("g++")) || !have("git") {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).ok()?;
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.5)\nproject(t CXX)\nenable_testing()\n\
         add_executable(t_test src/t.cpp)\nadd_test(NAME works COMMAND t_test)\n",
    )
    .ok()?;
    std::fs::write(root.join("src/t.cpp"), "int main() { return 0; }\n").ok()?;
    (git(root, &["init", "-q"])
        && git(root, &["add", "-A"])
        && git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]))
    .then_some(dir)
}

#[tokio::test]
async fn a_check_that_does_not_fit_the_fast_tier_is_finished_by_the_follow_up() {
    let Some(dir) = slow_cmake_repo() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    if std::env::var_os("BYTEASK_TEST_SANDBOX_HELPER").is_none() {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set (the hook gate builds in the sandbox)");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut ctx = ctx(home.path(), dir.path());
    ctx.native_exec = true;
    ctx.fast_tier = Some(Duration::from_millis(500));
    session_start(&ctx, true).await;
    std::fs::write(dir.path().join("src/t.cpp"), "int main() { return 0; } // edited\n").unwrap();

    let first = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), follow_up: start } = &first else {
        panic!("{first:?}");
    };
    assert!(message.contains("NOT RUN (time limit)"), "{message}");
    assert!(start, "the caller must start the follow-up");

    follow_up(&ctx).await;
    let stored = results_store::all(home.path(), dir.path());
    let full = stored.iter().find(|entry| entry.tier == "full").expect("the full result is stored");
    assert_eq!(full.result.verdict, check_result::Verdict::Pass, "{:?}", full.result);

    // The next Stop for the same code takes the full result: PASS, no follow-up, no rebuild.
    let next = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), follow_up: start } = &next else {
        panic!("{next:?}");
    };
    assert!(message.contains("Result: PASS"), "{message}");
    assert!(!start);
}

/// The hooks write the agent session's change in the results store: the code at
/// SessionStart as its base, the agent's edit timed at the start of the turn (the Stop hook
/// sees it only at the end), and the gate's verdict with the version it checked, all under
/// the agent's own session id (the same id the toolbox records its tool results under).
#[tokio::test]
async fn the_session_s_change_gets_the_base_the_edit_and_the_verdict() {
    use ::results_store::store::Record;
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    session_start(&ctx, true).await;
    let turn = results_store::now_unix();
    let _ = prompt(&ctx).await;
    // The agent works for a while before the turn ends.
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
    std::fs::write(
        dir.path().join("src/tool.c"),
        "int answer(void) { return 6; }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("scratch.txt"), "notes").unwrap();
    assert!(
        matches!(stop(&ctx).await, StopDecision::Block { .. }),
        "the leftover file fails"
    );
    let stopped = results_store::now_unix();

    let changes = ::results_store::store::load(&::results_store::store::root(home.path())).changes;
    assert_eq!(changes.len(), 1, "one change for the session");
    let change = &changes[0];
    assert_eq!(
        change.opened().map(|(_, _, _, session)| session),
        Some("s-1")
    );
    let base = change.base_version().map(str::to_string);
    assert!(base.is_some(), "the code at SessionStart is the base");
    let edits: Vec<u64> = change
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Edit { at, .. } => Some(*at),
            _ => None,
        })
        .collect();
    assert_eq!(edits.len(), 1, "{:?}", change.records);
    assert!(
        edits[0] >= turn && edits[0] <= turn + 1 && edits[0] + 2 <= stopped,
        "timed at the turn's start, not when the Stop hook saw it: {} vs turn {turn}, stop {stopped}",
        edits[0]
    );
    let verdicts: Vec<(check_result::Verdict, Option<String>)> = change
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Result {
                result, version, ..
            } if result.check == crate::gate::CHECK => Some((result.verdict, version.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts.len(), 1, "{:?}", change.records);
    assert_eq!(verdicts[0].0, check_result::Verdict::Fail);
    assert!(
        verdicts[0].1.is_some() && verdicts[0].1 != base,
        "the edited version"
    );

    // The same failure again (unchanged code) records no second edit.
    assert!(matches!(stop(&ctx).await, StopDecision::Block { .. }));
    let again =
        &::results_store::store::load(&::results_store::store::root(home.path())).changes[0];
    let edits_after = again
        .records
        .iter()
        .filter(|record| matches!(record, Record::Edit { .. }))
        .count();
    assert_eq!(edits_after, 1);
}

/// A person's skip of exactly this version ends the turn with NOT RUN
/// (skipped: reason, who), never PASS and never a loop, even when the change would FAIL; an
/// edit ends the skip and the gate runs again.
#[tokio::test]
async fn a_persons_skip_ends_the_turn_as_not_run_skipped_never_pass() {
    let Some(dir) = repo() else {
        eprintln!("skipped: git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let ctx = ctx(home.path(), dir.path());
    session_start(&ctx, true).await;
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 7; }\n").unwrap();
    // Would FAIL (leftover file) without the skip.
    std::fs::write(dir.path().join("scratch.txt"), "notes").unwrap();
    crate::skip::record(home.path(), dir.path(), crate::gate::CHECK, "rig is down", "alice")
        .await
        .expect("recorded");
    let decision = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), follow_up: false } = &decision else {
        panic!("a skip lets the turn end: {decision:?}");
    };
    assert!(
        message.contains("Result: NOT RUN (skipped): Skipped by alice: rig is down"),
        "{message}"
    );
    assert!(!message.contains("PASS"), "{message}");
    // An edit ends the skip: the gate runs and the leftover file fails it again.
    std::fs::write(dir.path().join("src/tool.c"), "int answer(void) { return 8; }\n").unwrap();
    assert!(matches!(stop(&ctx).await, StopDecision::Block { .. }));
}

/// `byteask checks cancel` stops a running check within 2 s, every program it
/// started is gone, and the result is NOT RUN (cancelled), never PASS.
#[tokio::test]
async fn a_cancel_request_stops_the_check_within_two_seconds() {
    let have = |tool: &str| crate::tools::binary_on_path(tool);
    if !have("cmake") || !(have("c++") || have("g++")) || !have("git") {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = root.join("build-step-ran-to-the-end");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A build that takes 60 s (a custom command every build runs).
    std::fs::write(
        root.join("CMakeLists.txt"),
        format!(
            "cmake_minimum_required(VERSION 3.5)\nproject(t CXX)\nenable_testing()\n\
             add_executable(t_test src/t.cpp)\nadd_test(NAME works COMMAND t_test)\n\
             add_custom_target(slow ALL COMMAND sleep 61.5 COMMAND touch {})\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(root.join("src/t.cpp"), "int main() { return 0; }\n").unwrap();
    assert!(git(root, &["init", "-q"]) && git(root, &["add", "-A"]));
    assert!(git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]));
    let home = tempfile::tempdir().unwrap();
    let mut ctx = ctx(home.path(), root);
    ctx.native_exec = true;
    ctx.fast_tier = Some(Duration::from_secs(120));
    session_start(&ctx, true).await;
    std::fs::write(root.join("src/t.cpp"), "int main() { return 0; } // edited\n").unwrap();

    let home_path = home.path().to_path_buf();
    let root_path = root.to_path_buf();
    let requested_at = std::sync::Arc::new(std::sync::Mutex::new(None));
    let at = requested_at.clone();
    let canceller = tokio::spawn(async move {
        // Cancel once the slow step is running.
        for _ in 0..600 {
            let running = slow_step_running();
            if running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        *at.lock().unwrap() = Some(std::time::Instant::now());
        crate::cancel::request(&home_path, &root_path).await.unwrap();
    });
    let decision = stop(&ctx).await;
    let returned = std::time::Instant::now();
    canceller.await.unwrap();
    let asked = requested_at.lock().unwrap().expect("the cancel was sent");
    assert!(
        returned.duration_since(asked) < Duration::from_secs(2),
        "stopped {:?} after the request",
        returned.duration_since(asked)
    );
    let StopDecision::Allow { message: Some(message), follow_up: false } = &decision else {
        panic!("a cancel lets the turn end: {decision:?}");
    };
    assert!(message.contains("Result: NOT RUN (cancelled)"), "{message}");
    let stored = results_store::latest(home.path(), root).expect("stored");
    assert_eq!(stored.result.reason, Some(check_result::NotRunReason::Cancelled));
    // Nothing the check started keeps running.
    assert!(!slow_step_running(), "the build's slow step is still running");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!marker.exists());
}

/// The fixture's slow build step (`sleep 61.5`, a duration nothing else on the host uses).
fn slow_step_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "^sleep 61.5$"])
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false)
}

/// A check that runs out of the fast tier names the slowest files and heaviest
/// templates, from clang -ftime-trace data in the check's build tree, once per session.
#[tokio::test]
async fn a_check_that_runs_out_of_time_says_why_the_build_is_slow() {
    let Some(dir) = slow_cmake_repo() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let mut ctx = ctx(home.path(), dir.path());
    ctx.native_exec = true;
    ctx.fast_tier = Some(Duration::from_millis(300));
    session_start(&ctx, true).await;
    // A clang trace of the project's file in the gate's build tree (clang writes
    // `<object>.json` next to `<object>` when the project builds with -ftime-trace).
    let scratch = crate::gate::scratch_for(dir.path());
    let traces = scratch.join("cur-build/CMakeFiles/t_test.dir/src");
    std::fs::create_dir_all(&traces).unwrap();
    std::fs::write(
        traces.join("t.cpp.json"),
        serde_json::json!({"traceEvents": [
            {"name": "Total Frontend", "ph": "X", "ts": 0, "dur": 61_000_000},
            {"name": "InstantiateClass", "ph": "X", "ts": 5, "dur": 12_000_000,
             "args": {"detail": "std::map<int, Order>"}}
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("src/t.cpp"), "int main() { return 0; } // edited\n").unwrap();
    let first = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), .. } = &first else {
        panic!("{first:?}");
    };
    assert!(message.contains("NOT RUN (time limit)"), "{message}");
    assert!(message.contains("Why the build is slow"), "{message}");
    assert!(message.contains("61.0 s  CMakeFiles/t_test.dir/src/t.cpp"), "{message}");
    assert!(message.contains("12.0 s  class std::map<int, Order> (in 1 file(s))"), "{message}");
    // Once per session.
    std::fs::write(dir.path().join("src/t.cpp"), "int main() { return 0; } // again\n").unwrap();
    let second = stop(&ctx).await;
    let StopDecision::Allow { message: Some(message), .. } = &second else {
        panic!("{second:?}");
    };
    assert!(!message.contains("Why the build is slow"), "{message}");
    let _ = std::fs::remove_dir_all(&scratch);
}
