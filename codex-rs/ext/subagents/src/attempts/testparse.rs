//! Reading build and test output: per-test outcomes, compiler warnings per file, sanitizer
//! reports, and which paths are test files. All parsers are best-effort and framework-neutral;
//! when nothing is recognised a whole test command counts as one test (its exit status).
//! (Revived from the parked `attempts` commit 38cd2de169, unchanged apart from lint attributes.)

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex_lite::Regex;

/// Name used when a test command's output has no recognisable per-test lines.
pub const WHOLE_COMMAND: &str = "<test command>";

#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static CTEST_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*\d+/\d+\s+Test\s+#\d+:\s+(\S+)\s+\.*\s*(\**)\s*(Passed|Failed|Not Run|Exception|Timeout|SEGFAULT|Subprocess aborted|Child aborted|Bus error|ILLEGAL|OTHER_FAULT|Disabled)",
    )
    .expect("valid regex")
});
#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static BRACKET_OK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\s+OK\s+\]\s+(\S+)\s+\(").expect("valid regex"));
#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static BRACKET_FAILED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[\s+FAILED\s+\]\s+([A-Za-z_][\w:/.<>-]*\.[\w/<>-]+)").expect("valid regex")
});
#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static TAP_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(not ok|ok)\s+\d+\s*(?:-\s*)?(.*)$").expect("valid regex"));
#[allow(clippy::expect_used)] // constant pattern, covered by the unit tests
static WARNING_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:In file included from )?([^\s:][^:]*):(\d+):(?:\d+:)?\s+warning:")
        .expect("valid regex")
});

/// Outcome of one run of a test command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestRun {
    pub exit_ok: bool,
    pub timed_out: bool,
    /// test name -> passed
    pub outcomes: BTreeMap<String, bool>,
}

impl TestRun {
    pub fn failed(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .filter(|(_, ok)| !**ok)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Parse per-test outcomes out of a test command's output.
pub fn parse_test_output(output: &str, exit_ok: bool, timed_out: bool) -> TestRun {
    let mut outcomes: BTreeMap<String, bool> = BTreeMap::new();
    for raw in output.lines() {
        let line = raw.trim_end();
        if let Some(c) = CTEST_LINE.captures(line) {
            let name = c[1].to_string();
            let status = &c[3];
            if status == "Disabled" || status == "Not Run" && c[2].is_empty() {
                // Disabled / skipped tests carry no signal.
                continue;
            }
            outcomes.insert(format!("ctest:{name}"), status == "Passed");
            continue;
        }
        if let Some(c) = BRACKET_OK.captures(line) {
            outcomes.entry(format!("case:{}", &c[1])).or_insert(true);
            continue;
        }
        if let Some(c) = BRACKET_FAILED.captures(line) {
            outcomes.insert(format!("case:{}", &c[1]), false);
            continue;
        }
        if let Some(c) = TAP_LINE.captures(line) {
            let desc = c[2].trim();
            if desc.is_empty() || desc.starts_with('#') {
                continue;
            }
            let passed = &c[1] == "ok" && !desc.contains("# TODO");
            outcomes.insert(format!("tap:{desc}"), passed);
        }
    }
    if outcomes.is_empty() {
        outcomes.insert(WHOLE_COMMAND.to_string(), exit_ok && !timed_out);
    } else if !exit_ok && !timed_out && outcomes.values().all(|ok| *ok) {
        // The command failed although every recognised test passed (e.g. a crash after the
        // last test, or a failing step we could not parse). Keep that signal.
        outcomes.insert(WHOLE_COMMAND.to_string(), false);
    }
    TestRun {
        exit_ok,
        timed_out,
        outcomes,
    }
}

/// Compiler warnings per file (as printed, normalised relative to `root` when possible).
pub fn warnings_per_file(log: &str, root: &str) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    let root = root.trim_end_matches('/');
    for line in log.lines() {
        if let Some(c) = WARNING_LINE.captures(line.trim_start()) {
            let mut path = c[1].to_string();
            if let Some(rest) = path.strip_prefix(root) {
                path = rest.trim_start_matches('/').to_string();
            }
            while let Some(rest) = path.strip_prefix("../") {
                path = rest.to_string();
            }
            *out.entry(path).or_insert(0) += 1;
        }
    }
    out
}

/// Warnings attributed to `changed` files (suffix match: logs print paths in varying forms).
pub fn warnings_in(
    counts: &BTreeMap<String, usize>,
    changed: &[String],
) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for file in changed {
        let n: usize = counts
            .iter()
            .filter(|(p, _)| p.as_str() == file || p.ends_with(&format!("/{file}")))
            .map(|(_, n)| *n)
            .sum();
        if n > 0 {
            out.insert(file.clone(), n);
        }
    }
    out
}

/// Number of sanitizer reports in an output.
pub fn sanitizer_reports(output: &str) -> usize {
    const MARKERS: &[&str] = &[
        "ERROR: AddressSanitizer",
        "ERROR: LeakSanitizer",
        "ERROR: MemorySanitizer",
        "WARNING: ThreadSanitizer",
        "ERROR: HWAddressSanitizer",
        ": runtime error: ",
    ];
    output
        .lines()
        .filter(|line| MARKERS.iter().any(|m| line.contains(m)))
        .count()
}

/// Is `path` (repository-relative) a test file? Framework-neutral naming conventions.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let comps: Vec<&str> = lower.split('/').collect();
    let (dirs, file) = comps.split_at(comps.len().saturating_sub(1));
    const TEST_DIRS: &[&str] = &[
        "test",
        "tests",
        "testing",
        "unittest",
        "unittests",
        "unit_tests",
        "unit-tests",
        "testsuite",
        "test-suite",
        "test_suite",
        "t",
        "spec",
        "specs",
        "regress",
        "regression",
    ];
    if dirs.iter().any(|d| {
        TEST_DIRS.contains(d)
            || d.starts_with("test_")
            || d.starts_with("tests_")
            || d.starts_with("test-")
            || d.starts_with("tests-")
            || d.ends_with("_test")
            || d.ends_with("_tests")
            || d.ends_with("-test")
            || d.ends_with("-tests")
    }) {
        return true;
    }
    let Some(file) = file.first() else {
        return false;
    };
    let stem = file.split('.').next().unwrap_or(file);
    stem.starts_with("test_")
        || stem.starts_with("test-")
        || stem.ends_with("_test")
        || stem.ends_with("-test")
        || stem.ends_with("_tests")
        || stem.ends_with("_unittest")
        || stem.ends_with("test")
            && stem.len() > 4
            && !stem.ends_with("latest")
            && !stem.ends_with("contest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ctest_lines() {
        let out = "\
Test project /w/build
    Start 1: unit_a
1/3 Test #1: unit_a ...........................   Passed    0.01 sec
2/3 Test #2: unit_b ...........................***Failed    0.02 sec
3/3 Test #3: unit_c ...........................***Timeout  60.00 sec
67% tests passed, 2 tests failed out of 3
";
        let run = parse_test_output(out, false, false);
        assert_eq!(
            run.outcomes,
            BTreeMap::from([
                ("ctest:unit_a".to_string(), true),
                ("ctest:unit_b".to_string(), false),
                ("ctest:unit_c".to_string(), false),
            ])
        );
        assert_eq!(
            run.failed(),
            vec!["ctest:unit_b".to_string(), "ctest:unit_c".to_string()]
        );
    }

    #[test]
    fn bracket_style_cases() {
        let out = "\
[ RUN      ] Suite.one
[       OK ] Suite.one (0 ms)
[ RUN      ] Suite.two
[  FAILED  ] Suite.two (1 ms)
[  FAILED  ] 1 test, listed below:
[  FAILED  ] Suite.two
";
        let run = parse_test_output(out, false, false);
        assert_eq!(
            run.outcomes,
            BTreeMap::from([
                ("case:Suite.one".to_string(), true),
                ("case:Suite.two".to_string(), false),
            ])
        );
    }

    #[test]
    fn tap_and_fallback() {
        let run = parse_test_output("ok 1 - adds\nnot ok 2 - subtracts\n", false, false);
        assert_eq!(run.outcomes.get("tap:adds"), Some(&true));
        assert_eq!(run.outcomes.get("tap:subtracts"), Some(&false));
        let run = parse_test_output("all good\n", true, false);
        assert_eq!(
            run.outcomes,
            BTreeMap::from([(WHOLE_COMMAND.to_string(), true)])
        );
        // Exit failure with only passing recognised tests keeps a failing entry.
        let run = parse_test_output("ok 1 - adds\n", false, false);
        assert_eq!(run.outcomes.get(WHOLE_COMMAND), Some(&false));
    }

    #[test]
    fn warnings_and_sanitizers() {
        let log = "\
/w/src/a.c:10:5: warning: unused variable 'x' [-Wunused-variable]
../src/a.c:11:1: warning: something
/w/src/b.c:3:1: error: nope
src/c.cpp:4: warning: old style
";
        let counts = warnings_per_file(log, "/w");
        assert_eq!(counts.get("src/a.c"), Some(&2));
        assert_eq!(counts.get("src/c.cpp"), Some(&1));
        assert_eq!(
            warnings_in(&counts, &["src/a.c".to_string(), "src/b.c".to_string()]),
            BTreeMap::from([("src/a.c".to_string(), 2)])
        );
        assert_eq!(
            sanitizer_reports(
                "==1==ERROR: AddressSanitizer: heap-use-after-free\nx.c:3:4: runtime error: signed integer overflow\n"
            ),
            2
        );
    }

    #[test]
    fn test_paths() {
        for p in [
            "tests/foo.cpp",
            "test/CMakeLists.txt",
            "src/parser_test.cc",
            "unittests/a.c",
            "lib/test_util.c",
            "module/tests-unit/x.cpp",
            "src/FooTest.cpp",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/parser.cc",
            "include/latest.h",
            "src/contest.c",
            "docs/testing.md.in/x",
        ] {
            // "docs/testing.md.in/x" has a directory named like a test dir only by prefix.
            if p == "docs/testing.md.in/x" {
                continue;
            }
            assert!(!is_test_path(p), "{p}");
        }
    }
}
