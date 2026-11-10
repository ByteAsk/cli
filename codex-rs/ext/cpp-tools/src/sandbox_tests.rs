use super::*;
use std::time::Duration;

use check_result::Verdict;
use pretty_assertions::assert_eq;

use crate::run::RunOutcome;
use crate::run::run_capped;

// Without the tool sandbox, every program a tool started ran as a direct child of the
// engine with the user's full permissions: it could write anywhere in their
// home and open any network connection. There was no profile per tool and no
// NOT RUN when a sandbox could not start, because there was no sandbox.

fn settings(workspace: &Path) -> Settings {
    Settings {
        helper: Some(PathBuf::from("/opt/byteask/sandbox-helper")),
        workspace: workspace.to_path_buf(),
        relaxed: false,
        required: false,
    }
}

#[test]
fn a_tool_without_a_profile_starts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let err = decide(
        "cpp_new_tool",
        &settings(dir.path()),
        "true",
        &[],
        dir.path(),
        &Backend::Bubblewrap,
    )
    .expect_err("refused");
    let FunctionCallError::RespondToModel(text) = err else {
        panic!("must not end the turn");
    };
    let result = CheckResult::from_tool_text("cpp_new_tool", &text).expect("result line");
    assert_eq!(result.verdict, Verdict::NotRun);
    assert_eq!(result.reason, Some(NotRunReason::SandboxUnavailable));
}

/// No usable sandbox (Docker, old kernels, WSL) = the tool runs without one
/// and its answer says so in one line; it never runs unboxed silently.
#[test]
fn no_sandbox_runs_unsandboxed_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    for tool in ["cpp_sanitize", "cpp_check", "cpp_finish_check"] {
        let (wrapped, note) = decide(
            tool,
            &settings(dir.path()),
            "true",
            &[],
            dir.path(),
            &Backend::Unavailable("bwrap: setting up uid map: Permission denied".to_string()),
        )
        .expect("runs without the sandbox");
        assert!(!wrapped.sandboxed, "{tool}");
        assert_eq!(wrapped, unchanged("true", &[]));
        let note = note.expect("the answer says so");
        assert!(note.starts_with("sandbox: unavailable"), "{note}");
        assert!(
            note.contains("Permission denied"),
            "the real reason is shown: {note}"
        );
    }
    // No helper at all, and a landlock-only machine that cannot fence ptrace: the same.
    let no_helper = Settings {
        helper: None,
        ..settings(dir.path())
    };
    let (wrapped, note) = decide(
        "cpp_sanitize",
        &no_helper,
        "true",
        &[],
        dir.path(),
        &Backend::Bubblewrap,
    )
    .expect("runs");
    assert!(!wrapped.sandboxed);
    assert!(note.expect("note").contains("helper was not found"));
}

/// ...unless the protected policy requires the sandbox: then NOT RUN with both fixes.
#[test]
fn a_policy_that_requires_the_sandbox_keeps_failing_closed() {
    let dir = tempfile::tempdir().unwrap();
    let required = Settings {
        required: true,
        ..settings(dir.path())
    };
    for tool in ["cpp_sanitize", "cpp_check"] {
        let err = decide(
            tool,
            &required,
            "true",
            &[],
            dir.path(),
            &Backend::Unavailable("bwrap: setting up uid map: Permission denied".to_string()),
        )
        .expect_err("refused");
        let FunctionCallError::RespondToModel(text) = err else {
            panic!("must not end the turn");
        };
        assert!(
            text.contains("Permission denied"),
            "the real reason is shown: {text}"
        );
        assert!(
            text.contains("[sandbox] required = true"),
            "the reason names the policy: {text}"
        );
        let result = CheckResult::from_tool_text(tool, &text).expect("result line");
        assert_eq!(result.verdict, Verdict::NotRun);
        assert_eq!(result.reason, Some(NotRunReason::SandboxUnavailable));
        let fix = result.fix.expect("fix");
        assert!(fix.contains("user namespaces"), "{fix}");
        assert!(fix.contains("byteask policy relax"), "{fix}");
    }
    // A working sandbox is used as before.
    assert!(
        decide(
            "cpp_check",
            &required,
            "sh",
            &[],
            dir.path(),
            &Backend::Bubblewrap
        )
        .expect("runs")
        .0
        .sandboxed
    );
}

#[test]
fn a_relaxed_project_runs_unchanged_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let relaxed = Settings {
        relaxed: true,
        required: false,
        ..settings(dir.path())
    };
    let (wrapped, note) = decide(
        "cpp_sanitize",
        &relaxed,
        "true",
        &["-x".to_string()],
        dir.path(),
        &Backend::Unavailable("none".to_string()),
    )
    .expect("runs");
    assert_eq!(wrapped, unchanged("true", &["-x".to_string()]));
    assert!(note.expect("note").contains("byteask policy relax"));
}

#[test]
fn uncovered_profiles_and_platforms_run_as_before_and_say_so() {
    let dir = tempfile::tempdir().unwrap();
    for (tool, backend) in [
        ("debug_session", Backend::Bubblewrap),
        ("cpp_kdb", Backend::Bubblewrap),
        ("cpp_sanitize", Backend::NotWired),
    ] {
        let (wrapped, note) = decide(
            tool,
            &settings(dir.path()),
            "gdb",
            &[],
            dir.path(),
            &backend,
        )
        .expect("runs");
        assert!(!wrapped.sandboxed, "{tool}");
        assert!(
            note.expect("note").starts_with("sandbox: not applied"),
            "{tool}"
        );
    }
}

#[test]
fn a_covered_tool_runs_inside_the_helper_with_its_profile() {
    let dir = tempfile::tempdir().unwrap();
    let (wrapped, note) = decide(
        "cpp_sanitize",
        &settings(dir.path()),
        "sh",
        &["-c".to_string(), "true".to_string()],
        dir.path(),
        &Backend::Bubblewrap,
    )
    .expect("runs");
    assert!(wrapped.sandboxed);
    assert_eq!(wrapped.program, "/opt/byteask/sandbox-helper");
    assert!(
        wrapped.args.contains(&"--permission-profile".to_string()),
        "{:?}",
        wrapped.args
    );
    assert!(!wrapped.args.contains(&"--use-legacy-landlock".to_string()));
    let tail = &wrapped.args[wrapped.args.len() - 3..];
    assert!(
        tail[0].ends_with("/sh"),
        "the program is resolved to a full path: {tail:?}"
    );
    assert_eq!(&tail[1..], &["-c".to_string(), "true".to_string()]);
    assert!(note.expect("note").contains("bubblewrap"));

    let (wrapped, _) = decide(
        "cpp_check",
        &settings(dir.path()),
        "sh",
        &[],
        dir.path(),
        &Backend::Landlock,
    )
    .expect("runs");
    assert!(wrapped.args.contains(&"--use-legacy-landlock".to_string()));
}

#[test]
fn a_missing_program_is_left_to_fail_as_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let (wrapped, _) = decide(
        "cpp_check",
        &settings(dir.path()),
        "byteask-no-such-tool-t4",
        &[],
        dir.path(),
        &Backend::Bubblewrap,
    )
    .expect("decided");
    assert_eq!(wrapped.program, "byteask-no-such-tool-t4");
}

#[test]
fn a_build_under_landlock_needs_yama_to_fence_ptrace() {
    let dir = tempfile::tempdir().unwrap();
    let decision = decide(
        "cpp_sanitize",
        &settings(dir.path()),
        "sh",
        &[],
        dir.path(),
        &Backend::Landlock,
    );
    // Without a Yama fence the build runs without the sandbox and says so.
    let (wrapped, note) = decision.expect("runs");
    assert_eq!(wrapped.sandboxed, ptrace_limited_to_children(), "{note:?}");
    if !ptrace_limited_to_children() {
        assert!(note.expect("note").starts_with("sandbox: unavailable"));
    }
    // An analysis has no network, so the helper's filter already blocks ptrace.
    assert!(
        decide(
            "cpp_check",
            &settings(dir.path()),
            "sh",
            &[],
            dir.path(),
            &Backend::Landlock
        )
        .is_ok()
    );
}

#[test]
fn a_signal_exit_from_bubblewrap_reads_as_a_crash() {
    assert_eq!(signal_exit(true, Some(139)), None);
    assert_eq!(signal_exit(true, Some(1)), Some(1));
    assert_eq!(signal_exit(false, Some(139)), Some(139));
    assert_eq!(signal_exit(true, None), None);
}

// ---------------------------------------------------------------------------
// Live: the engine's real sandbox helper. The batch that runs this suite sets
// BYTEASK_TEST_SANDBOX_HELPER to a built `codex-linux-sandbox`.

fn live_helper() -> Option<PathBuf> {
    let helper = PathBuf::from(std::env::var_os("BYTEASK_TEST_SANDBOX_HELPER")?);
    if !cfg!(target_os = "linux") || !helper.is_file() {
        return None;
    }
    Some(helper)
}

fn live(workspace: &Path, helper: PathBuf) -> Settings {
    Settings {
        helper: Some(helper),
        workspace: workspace.to_path_buf(),
        relaxed: false,
        required: false,
    }
}

async fn sh(tool: &str, settings: Settings, cwd: &Path, script: &str) -> RunOutcome {
    scope_for_test(
        tool,
        settings,
        run_capped(
            "sh",
            &["-c".to_string(), script.to_string()],
            cwd,
            Duration::from_secs(30),
        ),
    )
    .await
    .expect("spawned")
}

fn outside_dir() -> tempfile::TempDir {
    // Under $HOME, never a temp dir: temp dirs are writable by design.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    tempfile::tempdir_in(home).expect("a dir in HOME")
}

#[tokio::test]
async fn live_a_build_writes_its_workspace_and_nothing_outside_it() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let elsewhere = outside_dir();
    let script = format!(
        "echo ok > '{ws}/inside'; echo bad > '{other}/escape'; echo tmp > \"${{TMPDIR:-/tmp}}/byteask-t4-$$\"; exit 0",
        ws = workspace.path().display(),
        other = elsewhere.path().display(),
    );
    let outcome = sh(
        "cpp_sanitize",
        live(workspace.path(), helper),
        workspace.path(),
        &script,
    )
    .await;
    assert!(
        matches!(outcome, RunOutcome::Finished { .. }),
        "{outcome:?}"
    );
    assert!(
        workspace.path().join("inside").exists(),
        "the workspace is writable for a build"
    );
    assert!(
        !elsewhere.path().join("escape").exists(),
        "a build wrote outside its workspace"
    );
}

#[tokio::test]
async fn live_an_analysis_cannot_write_the_workspace() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let script = format!("echo bad > '{}/x'", workspace.path().display());
    let outcome = sh(
        "cpp_check",
        live(workspace.path(), helper),
        workspace.path(),
        &script,
    )
    .await;
    assert!(
        matches!(outcome, RunOutcome::Finished { success: false, .. }),
        "{outcome:?}"
    );
    assert!(!workspace.path().join("x").exists());
}

#[tokio::test]
async fn live_no_network_inside_the_sandbox() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let workspace = outside_dir();
    // Control first: the same connect works unsandboxed, so the sandboxed
    // failure below is the sandbox and not a broken probe.
    let probe = format!(
        "import socket; socket.create_connection(('127.0.0.1', {port}), timeout=3); print('connected')"
    );
    let control = std::process::Command::new("python3")
        .args(["-c", &probe])
        .output()
        .expect("python3");
    assert!(control.status.success(), "control connect failed");
    // An analysis has no network. (A build keeps it; see the module docs.)
    let outcome = scope_for_test(
        "cpp_check",
        live(workspace.path(), helper),
        run_capped(
            "python3",
            &["-c".to_string(), probe.clone()],
            workspace.path(),
            Duration::from_secs(30),
        ),
    )
    .await
    .expect("spawned");
    let RunOutcome::Finished {
        success, stdout, ..
    } = outcome
    else {
        panic!("timed out");
    };
    assert!(
        !success && !stdout.contains("connected"),
        "a sandboxed program reached the network"
    );
}

#[tokio::test]
async fn live_a_crash_inside_the_sandbox_is_still_a_crash() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let outcome = sh(
        "cpp_sanitize",
        live(workspace.path(), helper),
        workspace.path(),
        "kill -SEGV $$",
    )
    .await;
    let RunOutcome::Finished { success, code, .. } = outcome else {
        panic!("timed out");
    };
    assert!(!success);
    assert_eq!(
        code, None,
        "a signal death must read as 'killed by a signal'"
    );
}

#[tokio::test]
async fn live_a_timeout_kills_everything_the_program_started() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let beat = workspace.path().join("beat");
    // A detached grandchild (its own session) that keeps writing: the
    // process-group kill alone cannot reach it; the sandbox's PID namespace
    // must.
    let script = format!(
        "setsid sh -c 'while true; do date +%s%N >> \"{beat}\"; sleep 0.1; done' & sleep 100",
        beat = beat.display()
    );
    let outcome = scope_for_test(
        "cpp_sanitize",
        live(workspace.path(), helper),
        run_capped(
            "sh",
            &["-c".to_string(), script],
            workspace.path(),
            Duration::from_secs(2),
        ),
    )
    .await
    .expect("spawned");
    assert!(
        matches!(outcome, RunOutcome::TimedOut { .. }),
        "{outcome:?}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = std::fs::read_to_string(&beat).unwrap_or_default().len();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = std::fs::read_to_string(&beat).unwrap_or_default().len();
    assert!(before > 0, "the program never ran");
    assert_eq!(
        before, after,
        "something the program started is still running"
    );
}

#[tokio::test]
async fn live_the_note_reaches_the_call() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let note = scope_for_test("cpp_sanitize", live(workspace.path(), helper), async {
        let _ = run_capped("true", &[], workspace.path(), Duration::from_secs(30)).await;
        current_note()
    })
    .await;
    assert!(note.expect("note").starts_with("sandbox: on"));
}

#[tokio::test]
async fn live_leak_sanitizer_still_works_inside_a_build() {
    // The reason a build keeps ptrace: LeakSanitizer stops the program's
    // threads with it for every leak check.
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    std::fs::write(
        workspace.path().join("leak.cpp"),
        "#include <cstdlib>\nint main(){ void* p = std::malloc(77); p = nullptr; return 0; }\n",
    )
    .unwrap();
    let built = std::process::Command::new("c++")
        .args(["-g", "-fsanitize=address", "leak.cpp", "-o", "leak"])
        .current_dir(workspace.path())
        .status()
        .expect("c++");
    assert!(built.success());
    let outcome = scope_for_test(
        "cpp_sanitize",
        live(workspace.path(), helper),
        crate::run::run_capped_env(
            "./leak",
            &[],
            workspace.path(),
            Duration::from_secs(60),
            &[("ASAN_OPTIONS".to_string(), "detect_leaks=1".to_string())],
        ),
    )
    .await
    .expect("spawned");
    let RunOutcome::Finished { stderr, .. } = outcome else {
        panic!("timed out");
    };
    assert!(stderr.contains("detected memory leaks"), "{stderr}");
    assert!(
        !stderr.contains("LeakSanitizer has encountered a fatal error"),
        "{stderr}"
    );
}

#[tokio::test]
async fn live_a_killed_sandbox_leaves_no_placeholder_behind() {
    // Found by running: a timed-out (killed) helper could not remove the empty `.git` it
    // creates in each writable root, and an empty `.git` in /tmp or a workspace makes it look
    // like a repository (other tests failed because of /tmp/.git).
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    let workspace = outside_dir();
    let outcome = scope_for_test(
        "cpp_sanitize",
        live(workspace.path(), helper),
        run_capped(
            "sh",
            &["-c".to_string(), "sleep 100".to_string()],
            workspace.path(),
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("spawned");
    assert!(
        matches!(outcome, RunOutcome::TimedOut { .. }),
        "{outcome:?}"
    );
    for name in PROTECTED_NAMES {
        assert!(
            !workspace.path().join(name).exists(),
            "{name} was left in the workspace by a killed sandbox"
        );
    }
}

#[test]
fn only_empty_placeholders_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let empty_dir = dir.path().join(".git");
    let full_dir = dir.path().join(".codex");
    let empty_file = dir.path().join(".agents");
    std::fs::create_dir(&empty_dir).unwrap();
    std::fs::create_dir(&full_dir).unwrap();
    std::fs::write(full_dir.join("config.toml"), "x").unwrap();
    std::fs::write(&empty_file, "").unwrap();
    remove_placeholders(&[empty_dir.clone(), full_dir.clone(), empty_file.clone()]);
    assert!(!empty_dir.exists());
    assert!(
        full_dir.join("config.toml").exists(),
        "real content is never removed"
    );
    assert!(!empty_file.exists());
}
