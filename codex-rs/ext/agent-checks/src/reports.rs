//! Test results from test drivers that write no JUnit, read after a test command whose JUnit
//! (if any) was not found, first that yields anything:
//! 1. automake's parallel harness: one `*.trs` per test (`:global-test-result:` / `:test-result:`),
//!    written under the command's directory; id = the `.trs` path relative to the repo, without
//!    `.trs` (`tests/basic.test` -> `tests/basic`), the same in baseline and verify;
//! 2. Meson: `meson-logs/testlog.json` (one JSON object per line) in the command's directory, its
//!    `-C` directory, or one level below (`build/`); covers `ninja test` and Meson versions
//!    without `testlog.junit.xml`; id = the test's name;
//! 3. automake's serial harness: `PASS: name` / `FAIL: name` lines in the command's output.
//!
//! Only files modified after the command started count (`since`), so a stale report from an
//! earlier run never reads as this run's result. Plain `make check` / `make test` with none of
//! these leaves it to the caller, which records the command's exit status as one test.

use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::TestStatus;
use crate::agent::read_fresh;

/// Most entries the `.trs` walk looks at, and how deep it goes.
const WALK_ENTRY_CAP: usize = 200_000;
const WALK_DEPTH_CAP: usize = 12;
/// Most `.trs` files read (a test suite with more is reported as far as it got).
const TRS_CAP: usize = 20_000;
const MAX_ID_CHARS: usize = 1024;

/// What a test-purpose command reported without JUnit, or empty when it wrote none of these.
pub(crate) fn read(
    root: &Path,
    cwd: &Path,
    cmd: &str,
    output: &str,
    since: SystemTime,
) -> Vec<(String, TestStatus)> {
    let trs = automake_trs(root, cwd, since);
    if !trs.is_empty() {
        return trs;
    }
    let meson = meson_testlog(cwd, cmd, since);
    if !meson.is_empty() {
        return meson;
    }
    automake_lines(output)
}

/// One test for a test command whose results nobody wrote down: its exit status. The id is the
/// command itself, so baseline and verify (same command) agree.
pub(crate) fn exit_status(
    cmd: &str,
    exit_code: Option<i32>,
    timed_out: bool,
) -> (String, TestStatus) {
    let status = match (exit_code, timed_out) {
        (Some(0), false) => TestStatus::Pass,
        (Some(_), false) => TestStatus::Fail,
        _ => TestStatus::Error,
    };
    (clip(cmd.trim()), status)
}

fn clip(id: &str) -> String {
    id.chars().take(MAX_ID_CHARS).collect()
}

fn automake_status(word: &str) -> Option<TestStatus> {
    match word {
        "PASS" | "XFAIL" => Some(TestStatus::Pass),
        "SKIP" => Some(TestStatus::Skip),
        "FAIL" | "XPASS" => Some(TestStatus::Fail),
        "ERROR" => Some(TestStatus::Error),
        _ => None,
    }
}

fn worse(a: TestStatus, b: TestStatus) -> TestStatus {
    fn rank(s: TestStatus) -> u8 {
        match s {
            TestStatus::Skip => 0,
            TestStatus::Pass => 1,
            TestStatus::Fail => 2,
            TestStatus::Error => 3,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

/// A `.trs` file's verdict: `:global-test-result:` when present (TAP drivers write one per
/// file), else the worst `:test-result:`.
fn trs_status(text: &str) -> Option<TestStatus> {
    let mut worst: Option<TestStatus> = None;
    for line in text.lines() {
        let value = |rest: &str| automake_status(rest.split_whitespace().next().unwrap_or(""));
        if let Some(global) = line.strip_prefix(":global-test-result:").and_then(value) {
            return Some(global);
        }
        if let Some(status) = line.strip_prefix(":test-result:").and_then(value) {
            worst = Some(worst.map_or(status, |w| worse(w, status)));
        }
    }
    worst
}

fn automake_trs(root: &Path, cwd: &Path, since: SystemTime) -> Vec<(String, TestStatus)> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(cwd.to_path_buf(), 0)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > WALK_ENTRY_CAP || out.len() >= TRS_CAP {
                return sorted(out);
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if depth < WALK_DEPTH_CAP && !matches!(name.as_ref(), ".git" | "node_modules") {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".trs") else {
                continue;
            };
            if stem.is_empty() || !kind.is_file() {
                continue;
            }
            let Some(status) = read_fresh(&path, since).as_deref().and_then(trs_status) else {
                continue;
            };
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            let rel = canonical
                .strip_prefix(&root)
                .map(Path::to_path_buf)
                .unwrap_or(canonical);
            let id = rel.with_file_name(stem).to_string_lossy().into_owned();
            out.push((clip(&id), status));
        }
    }
    sorted(out)
}

fn sorted(mut tests: Vec<(String, TestStatus)>) -> Vec<(String, TestStatus)> {
    tests.sort_by(|a, b| a.0.cmp(&b.0));
    tests
}

fn meson_status(result: &str) -> TestStatus {
    match result {
        "OK" | "EXPECTEDFAIL" => TestStatus::Pass,
        "SKIP" => TestStatus::Skip,
        "FAIL" | "UNEXPECTEDPASS" => TestStatus::Fail,
        _ => TestStatus::Error,
    }
}

/// `-C <dir>` of a `meson test` / `ninja test` / `muon test` command, when there is one.
fn dash_c(cmd: &str) -> Option<String> {
    let words = shlex::split(cmd)?;
    words
        .iter()
        .position(|w| w == "-C")
        .and_then(|i| words.get(i + 1).cloned())
        .or_else(|| {
            words.iter().find_map(|w| {
                w.strip_prefix("-C")
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
            })
        })
}

fn meson_testlog(cwd: &Path, cmd: &str, since: SystemTime) -> Vec<(String, TestStatus)> {
    let script = crate::classify::unwrap_shell(cmd);
    let mut dirs = vec![cwd.to_path_buf()];
    if let Some(dir) = dash_c(&script) {
        dirs.insert(0, cwd.join(dir));
    }
    if let Ok(entries) = std::fs::read_dir(cwd) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        subdirs.sort();
        dirs.extend(subdirs);
    }
    for dir in dirs {
        let Some(text) = read_fresh(&dir.join("meson-logs/testlog.json"), since) else {
            continue;
        };
        let tests: Vec<(String, TestStatus)> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.trim();
                let result = entry.get("result")?.as_str()?;
                (!name.is_empty()).then(|| (clip(name), meson_status(result)))
            })
            .collect();
        if !tests.is_empty() {
            return tests;
        }
    }
    Vec::new()
}

/// `PASS: name` lines (automake's console output, serial or parallel harness). Lines that only
/// look alike (`# PASS:  3` totals) do not match: the name must follow one space.
fn automake_lines(output: &str) -> Vec<(String, TestStatus)> {
    output
        .lines()
        .filter_map(|line| {
            let (word, rest) = line.split_once(": ")?;
            let status = automake_status(word)?;
            let name = rest.trim();
            (!name.is_empty() && !rest.starts_with(' ')).then(|| (clip(name), status))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;

    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn ids(tests: &[(String, TestStatus)]) -> Vec<(&str, TestStatus)> {
        tests.iter().map(|(id, s)| (id.as_str(), *s)).collect()
    }

    #[test]
    fn automake_trs_files_are_one_test_each() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let since = SystemTime::now() - Duration::from_secs(5);
        write(
            &root.join("tests/basic.trs"),
            ":test-result: PASS\n:global-test-result: PASS\n:recheck: no\n",
        );
        write(
            &root.join("tests/cdecl.trs"),
            ":test-result: FAIL\n:copy-in-global-log: yes\n",
        );
        write(&root.join("tests/xfail.trs"), ":test-result: XFAIL\n");
        write(&root.join("tests/xpass.trs"), ":test-result: XPASS\n");
        write(&root.join("tests/skipped.trs"), ":test-result: SKIP\n");
        // TAP: several results, one global verdict.
        write(
            &root.join("t/tap.trs"),
            ":test-result: PASS\n:test-result: FAIL\n:global-test-result: FAIL\n",
        );
        write(&root.join("tests/garbage.trs"), "not a trs\n");
        let tests = read(root, root, "make check", "", since);
        assert_eq!(
            ids(&tests),
            vec![
                ("t/tap", TestStatus::Fail),
                ("tests/basic", TestStatus::Pass),
                ("tests/cdecl", TestStatus::Fail),
                ("tests/skipped", TestStatus::Skip),
                ("tests/xfail", TestStatus::Pass),
                ("tests/xpass", TestStatus::Fail),
            ]
        );
        // A .trs older than the command (an earlier run's) is not this run's result.
        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(read(root, root, "make check", "", later).is_empty());
    }

    #[test]
    fn meson_testlog_json_in_the_dash_c_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let since = SystemTime::now() - Duration::from_secs(5);
        write(
            &root.join("builddir/meson-logs/testlog.json"),
            concat!(
                "{\"name\": \"unit\", \"result\": \"OK\", \"suite\": [\"p\"]}\n",
                "{\"name\": \"parse\", \"result\": \"FAIL\"}\n",
                "{\"name\": \"slow\", \"result\": \"TIMEOUT\"}\n",
                "{\"name\": \"xf\", \"result\": \"EXPECTEDFAIL\"}\n",
                "{\"name\": \"sk\", \"result\": \"SKIP\"}\n",
                "not json\n"
            ),
        );
        let tests = read(
            root,
            root,
            "/bin/bash -lc 'ninja -C builddir test'",
            "",
            since,
        );
        assert_eq!(
            ids(&tests),
            vec![
                ("unit", TestStatus::Pass),
                ("parse", TestStatus::Fail),
                ("slow", TestStatus::Error),
                ("xf", TestStatus::Pass),
                ("sk", TestStatus::Skip),
            ]
        );
        // Found one level below the command's directory too (`build/`), without -C.
        assert_eq!(read(root, root, "meson test", "", since).len(), 5);
    }

    #[test]
    fn automake_console_lines_when_there_is_no_trs() {
        let output = "make  check-TESTS\nPASS: test_basic\nFAIL: test_cdecl\nXFAIL: known\n\
                      SKIP: needs_x\nERROR: crashed\n# PASS:  3\n# FAIL:  1\nPASS:\n";
        let dir = tempfile::tempdir().unwrap();
        let tests = read(
            dir.path(),
            dir.path(),
            "make check",
            output,
            SystemTime::now(),
        );
        assert_eq!(
            ids(&tests),
            vec![
                ("test_basic", TestStatus::Pass),
                ("test_cdecl", TestStatus::Fail),
                ("known", TestStatus::Pass),
                ("needs_x", TestStatus::Skip),
                ("crashed", TestStatus::Error),
            ]
        );
    }

    #[test]
    fn plain_make_check_is_its_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read(
                dir.path(),
                dir.path(),
                "make check",
                "all good\n",
                SystemTime::now()
            )
            .is_empty()
        );
        assert_eq!(
            exit_status(" make check ", Some(0), false),
            ("make check".to_string(), TestStatus::Pass)
        );
        assert_eq!(exit_status("make test", Some(2), false).1, TestStatus::Fail);
        assert_eq!(exit_status("make test", None, true).1, TestStatus::Error);
    }
}
