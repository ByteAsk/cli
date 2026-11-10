//! Optional switch `BYTEASK_FINISH_REVIEW_OWN_TESTS=1` (off by default): with `finish_review`, the
//! agent's own test run counts as the automatic review's build and test when it ran on the final change, after a
//! build of that change, exited 0, and its output shows tests that ran and none that failed.
//!
//! The headless runner reports each of the agent's finished shell commands that builds or runs tests
//! ([`note_agent_command`], core's agent-command hook). One task per process takes them in order and records, in the
//! root thread's finish state, the content key of the change each successful build and each passing test run saw.
//! `finish_review` then runs no build and no test for that exact change: it sends only Context's related places
//! (nothing when Context names none). Any later edit changes the key, so the review checks again.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

use regex_lite::Regex;
use tokio::sync::mpsc;

use crate::workflow::Workflow;

pub(crate) const ENV_OWN_TESTS: &str = "BYTEASK_FINISH_REVIEW_OWN_TESTS";

/// The switch (read once per process).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var(ENV_OWN_TESTS).is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
    })
}

/// The root thread's workflow (one session per process), set at thread start when the switch is on.
static ROOT: std::sync::Mutex<Option<Weak<Workflow>>> = std::sync::Mutex::new(None);

pub(crate) fn set_root_workflow(workflow: &Arc<Workflow>) {
    if !on() {
        return;
    }
    if let Ok(mut root) = ROOT.lock()
        && root.is_none()
    {
        *root = Some(Arc::downgrade(workflow));
    }
}

struct Note {
    builds: bool,
    tests_passed: bool,
    command: String,
}

static NOTES: OnceLock<mpsc::UnboundedSender<Note>> = OnceLock::new();

/// Core's agent-command hook: one of the agent's shell commands that builds or runs tests finished.
pub fn note_agent_command(command: &str, builds: bool, tests: bool, exit_code: Option<i32>, output: &str) {
    if !on() || exit_code != Some(0) {
        return;
    }
    let tests_passed = tests && shows_passing_tests(output);
    if !builds && !tests_passed {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let notes = NOTES.get_or_init(|| {
        let (tx, mut rx) = mpsc::unbounded_channel::<Note>();
        runtime.spawn(async move {
            while let Some(note) = rx.recv().await {
                record(note).await;
            }
        });
        tx
    });
    let _ = notes.send(Note {
        builds,
        tests_passed,
        command: short(command),
    });
}

async fn record(note: Note) {
    let workflow = ROOT
        .lock()
        .ok()
        .and_then(|root| root.as_ref().and_then(Weak::upgrade));
    let Some(workflow) = workflow else {
        return;
    };
    let Some(snapshot) = workflow.snapshot().await else {
        return;
    };
    let key = crate::gate::agent_changes(&snapshot).await.content_key;
    let mut state = workflow.finish_state().await;
    if note.builds {
        state.built = Some(key.clone());
    }
    if note.tests_passed && state.built.as_deref() == Some(key.as_str()) {
        state.tested = Some((key, note.command));
    }
}

/// The command as one short line (the review quotes it).
fn short(command: &str) -> String {
    let line: String = command
        .chars()
        .map(|c| if c == '\n' || c == '`' { ' ' } else { c })
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 160 {
        line
    } else {
        format!("{}…", line.chars().take(160).collect::<String>())
    }
}

/// The output shows tests that ran and none that failed: a C/C++ test driver's summary of passing tests (CTest,
/// GoogleTest, Catch2, doctest, Boost.Test, Automake, Meson, CppUnit) and no failure, crash or build error in it.
pub(crate) fn shows_passing_tests(output: &str) -> bool {
    static PASS: OnceLock<Vec<Regex>> = OnceLock::new();
    static FAIL: OnceLock<Vec<Regex>> = OnceLock::new();
    let pass = PASS.get_or_init(|| {
        [
            r"\b100% tests passed, 0 tests failed out of [1-9]",
            r"\[  PASSED  \] [1-9]\d* tests?\b",
            r"All tests passed \(\d+ assertions? in [1-9]\d* test cases?\)",
            r"\[doctest\] Status: SUCCESS!",
            r"\*\*\* No errors detected",
            r"(?m)^# PASS: +[1-9]",
            r"(?m)^Ok: +[1-9]\d*\s*$",
            r"\bOK \([1-9]\d* tests?\)",
        ]
        .iter()
        .filter_map(|re| Regex::new(re).ok())
        .collect()
    });
    let fail = FAIL.get_or_init(|| {
        [
            r"\[  FAILED  \]",
            r"\b[1-9]\d* tests? failed out of",
            r"\*\*\*Failed|\*\*\*Exception|\*\*\*Timeout|Errors while running CTest",
            r"FAILED TESTS?\b",
            r"test cases?:[^\n]*\b[1-9]\d* failed",
            r"\[doctest\] Status: FAILURE",
            r"\*\*\* [1-9]\d* failures? (are|is) detected|failure(s)? detected in",
            r"(?m)^# (FAIL|ERROR|XPASS): +[1-9]",
            r"(?m)^(Fail|Unexpected Pass|Timeout): +[1-9]",
            r"!!!FAILURES!!!",
            r"(?m)^FAILED: ",
            r"\*\*\* \[[^\]]*\] Error [0-9]",
            r"(?m)^\S+:\d+:\d+: (fatal )?error: ",
            r"Segmentation fault|core dumped|Aborted|terminate called|AddressSanitizer|runtime error:",
        ]
        .iter()
        .filter_map(|re| Regex::new(re).ok())
        .collect()
    });
    pass.iter().any(|re| re.is_match(output)) && !fail.iter().any(|re| re.is_match(output))
}

#[cfg(test)]
mod tests {
    use super::shows_passing_tests;

    #[test]
    fn passing_and_failing_outputs() {
        assert!(shows_passing_tests("100% tests passed, 0 tests failed out of 406\n\nTotal Test time (real) = 1.99 sec\n"));
        assert!(!shows_passing_tests("90% tests passed, 1 tests failed out of 10\n"));
        assert!(!shows_passing_tests("No tests were found!!!\n"));
        assert!(shows_passing_tests("[==========] 12 tests ran.\n[  PASSED  ] 12 tests.\n"));
        assert!(!shows_passing_tests("[  PASSED  ] 11 tests.\n[  FAILED  ] 1 test, listed below:\n"));
        assert!(shows_passing_tests("All tests passed (42 assertions in 7 test cases)\n"));
        assert!(!shows_passing_tests("test cases: 7 | 6 passed | 1 failed\n"));
        assert!(shows_passing_tests("*** No errors detected\n"));
        assert!(!shows_passing_tests("FAILED: CMakeFiles/x.dir/a.cpp.o\n100% tests passed, 0 tests failed out of 3\n"));
        assert!(!shows_passing_tests("src/a.cpp:3:5: error: expected ';'\n100% tests passed, 0 tests failed out of 3\n"));
    }
}
