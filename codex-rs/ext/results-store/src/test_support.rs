//! A scripted session against real git repositories, shared by the recorder
//! and report tests.

use std::path::Path;
use std::path::PathBuf;

use byteask_policy::ResultsStoreLimits;
use check_result::CheckResult;
use check_result::NotRunReason;

use crate::recorder::Settings;
use crate::recorder::ThreadRecorder;
use crate::workspace::tests::repo_with_upstream;
use crate::workspace::tests::run_git;

pub(crate) const T: u64 = 1_800_000_000;

pub(crate) fn recorder(home: &Path, cwd: &Path) -> ThreadRecorder {
    ThreadRecorder::with_limits(
        Settings {
            home: home.to_path_buf(),
            cwd: cwd.to_path_buf(),
        },
        ResultsStoreLimits::default(),
    )
}

fn result(recorder: &ThreadRecorder, at: u64, result: CheckResult) {
    let probe = recorder.probe();
    recorder.record_result(at, &probe, &result, Some("full tool answer"));
}

fn edit(recorder: &ThreadRecorder, at: u64, file: &Path, text: &str) {
    std::fs::write(file, text).unwrap();
    let probe = recorder.probe();
    recorder.record_edit(at, &probe);
}

/// What [`scripted_session`] leaves behind.
pub(crate) struct Scripted {
    pub home: tempfile::TempDir,
    pub _repos: Vec<tempfile::TempDir>,
    pub _plain: tempfile::TempDir,
}

/// Three sessions, each written through [`ThreadRecorder`] with real git:
///
/// * A (repo 1, `main`): a pre-existing sanitizer failure before any edit; the
///   AI edits (T+10), the sanitizer still fails (T+20), a significant perf
///   result (T+25), the AI edits again (T+30), the sanitizer passes (T+40) and
///   cpp_check passes (T+45); the AI commits and pushes; one more check (T+60)
///   lands in a NEW change that has no edit. Expected: 1 problem caught (by
///   cpp_sanitize), review-ready 30 s, change pushed.
/// * B (repo 2, `main`): edit (T+100), sanitizer NOT RUN (time limit), tidy
///   FAILS (T+110); edit (T+120) and only cpp_format runs and passes (T+130):
///   the failing checks were never re-run, so that version does not pass
///   every check; edit (T+150), cpp_check fails then passes on the SAME
///   version (flaky). Expected: nothing caught, never review-ready.
/// * C (a plain directory): one cpp_check pass with no version and no edit.
pub(crate) fn scripted_session() -> Scripted {
    let home = tempfile::tempdir().unwrap();

    // Session A.
    let (dir1, work1) = repo_with_upstream();
    let a = recorder(home.path(), &work1);
    let book = work1.join("book.cpp");
    result(&a, T, CheckResult::pass("cpp_check", "No findings."));
    result(
        &a,
        T + 5,
        CheckResult::fail(
            "cpp_sanitize",
            "AddressSanitizer: heap-buffer-overflow at book.cpp:1.",
        ),
    );
    edit(&a, T + 10, &book, "int depth() { return 2; }\n");
    result(
        &a,
        T + 20,
        CheckResult::fail(
            "cpp_sanitize",
            "AddressSanitizer: heap-buffer-overflow at book.cpp:1.",
        ),
    );
    result(
        &a,
        T + 25,
        CheckResult::pass("cpp_perf", "B is 2.39x faster, beyond the noise gate."),
    );
    edit(&a, T + 30, &book, "int depth() { return 3; }\n");
    result(
        &a,
        T + 40,
        CheckResult::pass("cpp_sanitize", "No sanitizer errors."),
    );
    result(&a, T + 45, CheckResult::pass("cpp_check", "No findings."));
    run_git(&work1, &["commit", "-q", "-am", "fix the overflow"]);
    run_git(&work1, &["push", "-q"]);
    result(&a, T + 60, CheckResult::pass("cpp_check", "No findings."));

    // Session B.
    let (dir2, work2) = repo_with_upstream();
    let b = recorder(home.path(), &work2);
    let book = work2.join("book.cpp");
    edit(&b, T + 100, &book, "int depth() { return 20; }\n");
    result(
        &b,
        T + 105,
        CheckResult::not_run(
            "cpp_sanitize",
            NotRunReason::TimeLimit,
            "The program was still running after 300 s.",
            "",
        ),
    );
    result(
        &b,
        T + 110,
        CheckResult::fail("cpp_tidy", "1 warning, the first at book.cpp:1."),
    );
    edit(&b, T + 120, &book, "int depth() { return 21; }\n");
    result(
        &b,
        T + 130,
        CheckResult::pass("cpp_format", "Already formatted."),
    );
    edit(&b, T + 150, &book, "int depth() { return 22; }\n");
    result(
        &b,
        T + 160,
        CheckResult::fail("cpp_check", "1 error, the first at book.cpp:1."),
    );
    result(&b, T + 170, CheckResult::pass("cpp_check", "No findings."));

    // Session C.
    let plain = tempfile::tempdir().unwrap();
    let c = recorder(home.path(), plain.path());
    result(&c, T + 200, CheckResult::pass("cpp_check", "No findings."));

    Scripted {
        home,
        _repos: vec![dir1, dir2],
        _plain: plain,
    }
}

pub(crate) fn store_root(scripted: &Scripted) -> PathBuf {
    crate::store::root(scripted.home.path())
}
