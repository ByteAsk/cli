//! Reading build and test output: CTest status lines, GoogleTest, Catch XML,
//! doctest console output, automake test-suite lines, and compiler errors.
//! Each parser keeps the first lines that explain a failure, not the log.

use std::collections::BTreeMap;

use crate::base_run_cache::TestStatus;

/// Lines of explanation kept per failure.
pub(crate) const DETAIL_LINES: usize = 3;
const DETAIL_CHARS: usize = 220;

/// Per-name results plus the first explanatory lines of each failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub results: Vec<(String, TestStatus)>,
    pub details: BTreeMap<String, Vec<String>>,
}

impl Parsed {
    fn set(&mut self, name: &str, status: TestStatus) {
        match self.results.iter_mut().find(|(n, _)| n == name) {
            Some(entry) => entry.1 = status,
            None => self.results.push((name.to_string(), status)),
        }
    }
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= DETAIL_CHARS {
        line.to_string()
    } else {
        let cut: String = line.chars().take(DETAIL_CHARS).collect();
        format!("{cut}…")
    }
}

/// Lines worth quoting from a failure's output: assertion/error lines first,
/// else the last non-empty lines.
pub(crate) fn explain(lines: &[&str]) -> Vec<String> {
    const MARKERS: &[&str] = &[
        "Failure",
        "FAILED",
        "failed",
        "error",
        "Error",
        "ERROR",
        "Assertion",
        "assert",
        "Expected",
        "expected",
        "Which is",
        "Segmentation",
        "Abort",
        "abort",
        "exception",
        "Exception",
        "REQUIRE",
        "CHECK",
        "panic",
        "Sanitizer",
        "runtime error",
    ];
    let mut out: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if MARKERS.iter().any(|m| line.contains(m)) && !line.trim().is_empty() {
            out.push(clip(line));
            // The value lines that follow an assertion header.
            for next in lines.iter().skip(i + 1).take(DETAIL_LINES - 1) {
                if out.len() >= DETAIL_LINES || next.trim().is_empty() {
                    break;
                }
                out.push(clip(next));
            }
            break;
        }
    }
    if out.is_empty() {
        let tail: Vec<&&str> = lines
            .iter()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(DETAIL_LINES)
            .collect();
        out = tail.into_iter().rev().map(|l| clip(l)).collect();
    }
    out.truncate(DETAIL_LINES);
    out
}

// ---------------------------------------------------------------------------
// CTest
// ---------------------------------------------------------------------------

/// `N/M Test #K: name ....   Passed   0.01 sec` → (name, status).
pub(crate) fn ctest_status_line(line: &str) -> Option<(String, TestStatus)> {
    let at = line.find("Test ")?;
    let rest = &line[at + 5..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('#')?;
    let colon = rest.find(':')?;
    if !rest[..colon].trim().chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = rest[colon + 1..].trim_start();
    let name_end = rest.find(" .").or_else(|| rest.find("***"))?;
    let name = rest[..name_end].trim().to_string();
    let status_text = rest[name_end..].trim_start_matches([' ', '.']);
    if !status_text.contains(" sec") {
        return None;
    }
    let status = if status_text.contains("Passed") {
        TestStatus::Pass
    } else if status_text.contains("Timeout") {
        TestStatus::Timeout
    } else if status_text.contains("Disabled") {
        TestStatus::Skip
    } else if status_text.contains("Not Run") {
        // The program was not found: it was not built.
        TestStatus::NotBuilt
    } else if status_text.contains("Skipped") {
        TestStatus::Skip
    } else {
        TestStatus::Fail
    };
    Some((name, status))
}

/// All tests in a `ctest --output-on-failure` transcript, with the first lines
/// of each failing test's output.
pub(crate) fn ctest(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some((name, status)) = ctest_status_line(lines[i]) else {
            i += 1;
            continue;
        };
        parsed.set(&name, status);
        i += 1;
        if status == TestStatus::Pass || status == TestStatus::Skip {
            continue;
        }
        let start = i;
        while i < lines.len()
            && ctest_status_line(lines[i]).is_none()
            && !lines[i].trim_start().starts_with("Start ")
            && !lines[i].contains("tests passed")
            && !lines[i].starts_with("The following tests")
        {
            i += 1;
        }
        let details = explain(&lines[start..i]);
        if !details.is_empty() {
            parsed.details.insert(name, details);
        }
    }
    parsed
}

// ---------------------------------------------------------------------------
// GoogleTest
// ---------------------------------------------------------------------------

/// GoogleTest console output → per-case results (`Suite.Name`).
pub(crate) fn gtest(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some(name) = bracket_tag(line, "RUN") {
            current = Some((name, Vec::new()));
            continue;
        }
        let closing = [
            ("OK", TestStatus::Pass),
            ("FAILED", TestStatus::Fail),
            ("SKIPPED", TestStatus::Skip),
        ]
        .into_iter()
        .find_map(|(tag, status)| bracket_tag(line, tag).map(|n| (n, status)));
        if let Some((name, status)) = closing {
            // Summary lines repeat FAILED names and "N tests, listed below".
            if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Some((running, body)) = current.take() {
                if running == name && status == TestStatus::Fail {
                    parsed.details.insert(name.clone(), explain(&body));
                }
            }
            if !parsed.results.iter().any(|(n, _)| *n == name) || status == TestStatus::Fail {
                parsed.set(&name, status);
            }
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((name, body)) = current {
        // Started, never finished: the program died in this case.
        parsed.set(&name, TestStatus::Fail);
        let mut details =
            vec!["(no result line: the program crashed or was killed in this case)".to_string()];
        details.extend(explain(&body).into_iter().take(DETAIL_LINES - 1));
        parsed.details.insert(name, details);
    }
    parsed
}

/// `[   TAG    ] name (12 ms)` → name.
fn bracket_tag(line: &str, tag: &str) -> Option<String> {
    let line = line.trim_start();
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    if rest[..close].trim() != tag {
        return None;
    }
    let name = rest[close + 1..].trim();
    let name = match name.rfind(" (") {
        Some(p) if name.ends_with(')') => &name[..p],
        _ => name,
    };
    // `Suite.Name, where <parameter> = …` for parameterised cases.
    let name = name.split(", where ").next().unwrap_or(name);
    (!name.is_empty()).then(|| name.trim().to_string())
}

// ---------------------------------------------------------------------------
// Catch (XML reporter)
// ---------------------------------------------------------------------------

pub(crate) fn catch_xml(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut rest = text;
    while let Some(start) = rest.find("<TestCase ") {
        let block_start = &rest[start..];
        let name = attr(block_start, "name")
            .map(|n| xml_unescape(&n))
            .unwrap_or_default();
        let end = block_start.find("</TestCase>").unwrap_or(block_start.len());
        let block = &block_start[..end];
        let status = match overall_result(block) {
            Some(p) => {
                if attr(&block[p..], "success").as_deref() == Some("true") {
                    TestStatus::Pass
                } else {
                    TestStatus::Fail
                }
            }
            // No overall result: the program died inside this case.
            None => TestStatus::Fail,
        };
        if status == TestStatus::Fail {
            parsed.details.insert(name.clone(), catch_failure(block));
        }
        if !name.is_empty() {
            parsed.set(&name, status);
        }
        rest = &block_start[end.min(block_start.len())..];
        if end == block_start.len() {
            break;
        }
    }
    parsed
}

/// The case's own `<OverallResult success=…>` element (not the plural
/// group-level `<OverallResults …>`).
fn overall_result(block: &str) -> Option<usize> {
    block.find("<OverallResult ")
}

fn catch_failure(block: &str) -> Vec<String> {
    if let Some(p) = block.find("<Expression success=\"false\"") {
        let expr = &block[p..];
        let file = attr(expr, "filename").unwrap_or_default();
        let line = attr(expr, "line").unwrap_or_default();
        let kind = attr(expr, "type").unwrap_or_default();
        let original = element_text(expr, "Original").unwrap_or_default();
        let expanded = element_text(expr, "Expanded").unwrap_or_default();
        return vec![
            clip(&format!(
                "{}:{line}: {kind}( {} ) failed",
                short_path(&file),
                original
            )),
            clip(&format!("  with expansion: {expanded}")),
        ];
    }
    for tag in ["Exception", "FatalErrorCondition", "Failure"] {
        if let Some(p) = block.find(&format!("<{tag}")) {
            let el = &block[p..];
            let file = attr(el, "filename").unwrap_or_default();
            let line = attr(el, "line").unwrap_or_default();
            let text = element_text(el, tag).unwrap_or_default();
            return vec![clip(&format!(
                "{}:{line}: {tag}: {text}",
                short_path(&file)
            ))];
        }
    }
    vec!["(no assertion recorded: the program may have crashed in this case)".to_string()]
}

fn short_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn attr(tag_text: &str, name: &str) -> Option<String> {
    let head_end = tag_text.find('>').unwrap_or(tag_text.len());
    let head = &tag_text[..head_end];
    let key = format!(" {name}=\"");
    let p = head.find(&key)? + key.len();
    let end = head[p..].find('"')?;
    Some(xml_unescape(&head[p..p + end]))
}

fn element_text(text: &str, tag: &str) -> Option<String> {
    let open = text.find(&format!("<{tag}"))?;
    let after = &text[open..];
    let content_start = after.find('>')? + 1;
    let close = after.find(&format!("</{tag}>"))?;
    Some(
        xml_unescape(after[content_start..close].trim())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// doctest (console reporter)
// ---------------------------------------------------------------------------

/// doctest console output. Passing cases are not printed, so each requested
/// case not reported as failing passed if the run reports any passes.
pub(crate) fn doctest(text: &str, requested: &[String]) -> Parsed {
    let mut parsed = Parsed::default();
    let mut current: Option<String> = None;
    let mut body: Vec<&str> = Vec::new();
    let mut failed: Vec<(String, Vec<String>)> = Vec::new();
    let flush =
        |name: Option<String>, body: &mut Vec<&str>, failed: &mut Vec<(String, Vec<String>)>| {
            if let Some(name) = name {
                if body
                    .iter()
                    .any(|l| l.contains("ERROR") || l.contains("crashed") || l.contains("FATAL"))
                {
                    failed.push((name, explain(body)));
                }
            }
            body.clear();
        };
    for line in text.lines() {
        if let Some(name) = line.trim_start().strip_prefix("TEST CASE:") {
            flush(current.take(), &mut body, &mut failed);
            current = Some(name.trim().to_string());
            continue;
        }
        if line.starts_with("=====") {
            flush(current.take(), &mut body, &mut failed);
            continue;
        }
        if current.is_some() {
            body.push(line);
        }
    }
    flush(current.take(), &mut body, &mut failed);
    let summary_passed = text
        .lines()
        .find(|l| l.contains("[doctest] test cases:"))
        .and_then(|l| l.split('|').nth(1))
        .and_then(|p| p.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    for (name, details) in &failed {
        parsed.set(name, TestStatus::Fail);
        parsed.details.insert(name.clone(), details.clone());
    }
    for name in requested {
        if !failed
            .iter()
            .any(|(n, _)| super::symbols::glob_match(name, n))
        {
            parsed.set(
                name,
                if summary_passed > 0 {
                    TestStatus::Pass
                } else {
                    TestStatus::NotRun
                },
            );
        }
    }
    parsed
}

// ---------------------------------------------------------------------------
// automake test-suite driver
// ---------------------------------------------------------------------------

/// `PASS: t1` / `FAIL: t2` / `SKIP:` / `XFAIL:` / `XPASS:` / `ERROR:` lines.
pub(crate) fn automake(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in text.lines() {
        for (prefix, status) in [
            ("PASS: ", TestStatus::Pass),
            ("FAIL: ", TestStatus::Fail),
            ("ERROR: ", TestStatus::Fail),
            ("XPASS: ", TestStatus::Fail),
            ("SKIP: ", TestStatus::Skip),
            ("XFAIL: ", TestStatus::Pass),
        ] {
            if let Some(name) = line.strip_prefix(prefix) {
                let name = name.split(" - ").next().unwrap_or(name).trim();
                if !name.is_empty() {
                    parsed.set(name, status);
                }
            }
        }
    }
    parsed
}

// ---------------------------------------------------------------------------
// Build errors
// ---------------------------------------------------------------------------

/// The first `cap` distinct compiler/linker errors, each with up to 3 lines of
/// context, file paths shown relative to `root` when inside it.
pub(crate) fn build_errors(text: &str, root: &str, cap: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let rel = |line: &str| -> String {
        let prefix = format!("{}/", root.trim_end_matches('/'));
        clip(&line.replace(&prefix, ""))
    };
    for (i, line) in lines.iter().enumerate() {
        let is_error = line.contains(": error:")
            || line.contains(": fatal error:")
            || line.contains("undefined reference to")
            || line.contains("error: ld returned")
            || line.contains("No rule to make target")
            || line.starts_with("CMake Error");
        if !is_error {
            continue;
        }
        let key = rel(line);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let mut block = key;
        for next in lines.iter().skip(i + 1).take(3) {
            if next.contains(": error:")
                || next.contains(": warning:")
                || next.trim().is_empty()
                || next.starts_with("make")
                || next.starts_with("ninja")
                || next.starts_with('[')
            {
                break;
            }
            block.push('\n');
            block.push_str(&format!("    {}", rel(next).trim_start()));
        }
        out.push(block);
        if out.len() >= cap {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn reads_ctest_transcripts() {
        let text = "Test project /b\n\
      Start  1: core_test\n\
 1/4 Test  #1: core_test ........................***Failed    0.01 sec\n\
[ RUN      ] Core.Twice\n\
/r/tests/core_test.cpp:5: Failure\n\
Expected equality of these values:\n\
  twice(2)\n\
      Start  2: api_test\n\
 2/4 Test  #2: api_test .........................   Passed    0.00 sec\n\
 3/4 Test  #3: slow .............................***Timeout   5.02 sec\n\
 4/4 Test  #4: gone .............................***Not Run   0.00 sec\n\
\n\
50% tests passed, 2 tests failed out of 4\n";
        let parsed = ctest(text);
        assert_eq!(
            parsed.results,
            vec![
                ("core_test".to_string(), TestStatus::Fail),
                ("api_test".to_string(), TestStatus::Pass),
                ("slow".to_string(), TestStatus::Timeout),
                ("gone".to_string(), TestStatus::NotBuilt),
            ]
        );
        assert_eq!(
            parsed.details["core_test"],
            vec![
                "/r/tests/core_test.cpp:5: Failure".to_string(),
                "Expected equality of these values:".to_string(),
                "twice(2)".to_string()
            ]
        );
        // CMake 3.5 prints the same shape.
        assert_eq!(
            ctest_status_line(
                "1/1 Test #1: unit .............................   Passed    0.00 sec"
            ),
            Some(("unit".to_string(), TestStatus::Pass))
        );
        assert_eq!(ctest_status_line("Test project /b"), None);
    }

    #[test]
    fn reads_gtest_output() {
        let text = "[==========] Running 3 tests from 1 test suite.\n\
[ RUN      ] Core.Twice\n\
tests/core_test.cpp:5: Failure\n\
Expected equality of these values:\n\
  twice(2)\n\
    Which is: 5\n\
[  FAILED  ] Core.Twice (0 ms)\n\
[ RUN      ] Core.Add\n\
[       OK ] Core.Add (0 ms)\n\
[ RUN      ] P/Param.Works/0\n\
[       OK ] P/Param.Works/0 (0 ms)\n\
[  PASSED  ] 2 tests.\n\
[  FAILED  ] 1 test, listed below:\n\
[  FAILED  ] Core.Twice\n";
        let parsed = gtest(text);
        assert_eq!(
            parsed.results,
            vec![
                ("Core.Twice".to_string(), TestStatus::Fail),
                ("Core.Add".to_string(), TestStatus::Pass),
                ("P/Param.Works/0".to_string(), TestStatus::Pass),
            ]
        );
        assert_eq!(
            parsed.details["Core.Twice"][0],
            "tests/core_test.cpp:5: Failure"
        );
        let crashed = gtest("[ RUN      ] Core.Crash\nSegmentation fault\n");
        assert_eq!(
            crashed.results,
            vec![("Core.Crash".to_string(), TestStatus::Fail)]
        );
    }

    #[test]
    fn reads_catch_xml() {
        let text = r#"<?xml version="1.0"?><Catch name="t"><Group name="t">
<TestCase name="parses &quot;x&quot;" filename="/r/t.cc" line="2">
  <Expression success="false" type="REQUIRE" filename="/r/t.cc" line="3">
    <Original>f() == 2</Original>
    <Expanded>1 == 2</Expanded>
  </Expression>
  <OverallResult success="false"/>
</TestCase>
<TestCase name="ok" filename="/r/t.cc" line="9"><OverallResult success="true"/></TestCase>
<TestCase name="dies" filename="/r/t.cc" line="12">"#;
        let parsed = catch_xml(text);
        assert_eq!(
            parsed.results,
            vec![
                ("parses \"x\"".to_string(), TestStatus::Fail),
                ("ok".to_string(), TestStatus::Pass),
                ("dies".to_string(), TestStatus::Fail),
            ]
        );
        assert_eq!(
            parsed.details["parses \"x\""][0],
            "t.cc:3: REQUIRE( f() == 2 ) failed"
        );
    }

    #[test]
    fn reads_doctest_output() {
        let text = "[doctest] doctest version is \"2.4.11\"\n\
===============================================================================\n\
tests/x.cpp:12:\n\
TEST CASE:  second\n\
\n\
tests/x.cpp:14: ERROR: CHECK( a == b ) is NOT correct!\n\
  values: CHECK( 1 == 2 )\n\
\n\
===============================================================================\n\
[doctest] test cases: 2 | 1 passed | 1 failed | 0 skipped\n";
        let parsed = doctest(text, &["first".to_string(), "second".to_string()]);
        assert_eq!(
            parsed.results,
            vec![
                ("second".to_string(), TestStatus::Fail),
                ("first".to_string(), TestStatus::Pass)
            ]
        );
        assert!(parsed.details["second"][0].contains("CHECK( a == b ) is NOT correct"));
    }

    #[test]
    fn reads_automake_and_build_errors() {
        let parsed =
            automake("PASS: tests/a.test\nFAIL: tests/b.test\nSKIP: c - no x\n# TOTAL: 3\n");
        assert_eq!(
            parsed.results,
            vec![
                ("tests/a.test".to_string(), TestStatus::Pass),
                ("tests/b.test".to_string(), TestStatus::Fail),
                ("c".to_string(), TestStatus::Skip),
            ]
        );
        let log = "[ 50%] Building CXX object CMakeFiles/core.dir/src/core.cpp.o\n\
/r/src/core.cpp:3:10: error: 'y' was not declared in this scope\n\
    3 |   return y;\n\
      |          ^\n\
/r/src/core.cpp:3:10: error: 'y' was not declared in this scope\n\
make[2]: *** [CMakeFiles/core.dir/build.make:76: x] Error 1\n\
/usr/bin/ld: core.o: undefined reference to `twice(int)'\n";
        let errors = build_errors(log, "/r", 3);
        assert_eq!(errors.len(), 2);
        assert_eq!(
            errors[0],
            "src/core.cpp:3:10: error: 'y' was not declared in this scope\n    3 |   return y;\n    |          ^"
        );
        assert!(errors[1].contains("undefined reference"));
    }
}
