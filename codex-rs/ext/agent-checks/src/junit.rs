//! JUnit XML reports (CTest `--output-junit`, GoogleTest `--gtest_output=xml`, pytest
//! `--junitxml`, Catch2 `-r junit`, meson) to `(id, status)` pairs. The id is
//! `classname::name` (just `name` when there is no classname): stable across runs, which is
//! what `verify` compares by.

use quick_xml::Reader;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;

use crate::TestStatus;

const MAX_ID_CHARS: usize = 1024;

fn attr(element: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(std::borrow::Cow::into_owned)
        })
}

fn open_case(element: &BytesStart<'_>) -> (String, TestStatus) {
    let classname = attr(element, b"classname").unwrap_or_default();
    let name = attr(element, b"name").unwrap_or_default();
    let name = if name.is_empty() {
        "(unnamed)".to_string()
    } else {
        name
    };
    let id = if classname.is_empty() {
        name
    } else {
        format!("{classname}::{name}")
    };
    let id: String = id.chars().take(MAX_ID_CHARS).collect();
    // CTest: status="notrun"/"disabled"/"fail"; GoogleTest: result="skipped"/"suppressed".
    let status = attr(element, b"status")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let result = attr(element, b"result")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let status = if matches!(status.as_str(), "notrun" | "disabled" | "skipped")
        || matches!(result.as_str(), "skipped" | "suppressed")
    {
        TestStatus::Skip
    } else if matches!(status.as_str(), "fail" | "failed") {
        TestStatus::Fail
    } else {
        TestStatus::Pass
    };
    (id, status)
}

fn is_suite(name: &[u8]) -> bool {
    name == b"testsuites" || name == b"testsuite"
}

/// Every test case in the report, or why it could not be read. A report cut off mid-write is
/// an error, never "these were all the tests".
pub(crate) fn parse(xml: &str) -> Result<Vec<(String, TestStatus)>, String> {
    let mut reader = Reader::from_str(xml);
    let mut cases = Vec::new();
    let mut open: Option<(String, TestStatus)> = None;
    let mut saw_root = false;
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Err(err) => return Err(format!("not valid XML: {err}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(element)) if is_suite(element.name().as_ref()) => {
                saw_root = true;
                depth += 1;
            }
            Ok(Event::Empty(element)) if is_suite(element.name().as_ref()) => saw_root = true,
            Ok(Event::End(element)) if is_suite(element.name().as_ref()) => {
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Start(element)) if element.name().as_ref() == b"testcase" => {
                open = Some(open_case(&element));
            }
            Ok(Event::Empty(element)) if element.name().as_ref() == b"testcase" => {
                cases.push(open_case(&element));
            }
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if let Some((_, status)) = open.as_mut() {
                    match element.name().as_ref() {
                        b"error" => *status = TestStatus::Error,
                        b"failure" if *status != TestStatus::Error => *status = TestStatus::Fail,
                        b"skipped" if *status == TestStatus::Pass => *status = TestStatus::Skip,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(element)) if element.name().as_ref() == b"testcase" => {
                if let Some(case) = open.take() {
                    cases.push(case);
                }
            }
            Ok(_) => {}
        }
    }
    if !saw_root {
        return Err("no <testsuites> or <testsuite> element".to_string());
    }
    if open.is_some() || depth > 0 {
        return Err("cut off before its end".to_string());
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn reads_ctest_gtest_and_pytest_reports() {
        let ctest = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="Linux" tests="4" failures="1" disabled="1" skipped="1">
  <testcase name="book_add" classname="book_add" time="0.01" status="run"/>
  <testcase name="book_cross" classname="book_cross" time="0.01" status="fail">
    <failure message="Failed"/>
  </testcase>
  <testcase name="feed_gap" classname="feed_gap" status="notrun"><skipped message="Not Run"/></testcase>
  <testcase name="old" classname="old" status="disabled"/>
</testsuite>"#;
        assert_eq!(
            parse(ctest).expect("parses"),
            vec![
                ("book_add::book_add".to_string(), TestStatus::Pass),
                ("book_cross::book_cross".to_string(), TestStatus::Fail),
                ("feed_gap::feed_gap".to_string(), TestStatus::Skip),
                ("old::old".to_string(), TestStatus::Skip),
            ]
        );
        let gtest = r#"<testsuites><testsuite name="BookTest">
  <testcase name="Adds" status="run" result="completed" classname="BookTest"/>
  <testcase name="Later" status="run" result="skipped" classname="BookTest"><skipped message="x"/></testcase>
  <testcase name="Crosses" status="run" result="completed" classname="BookTest"><failure message="boom">x</failure></testcase>
</testsuite></testsuites>"#;
        assert_eq!(
            parse(gtest).expect("parses"),
            vec![
                ("BookTest::Adds".to_string(), TestStatus::Pass),
                ("BookTest::Later".to_string(), TestStatus::Skip),
                ("BookTest::Crosses".to_string(), TestStatus::Fail),
            ]
        );
        let pytest = r#"<testsuites><testsuite name="pytest">
  <testcase classname="tests.test_feed" name="test_gap"><error message="fixture"/></testcase>
  <testcase name="bare"/>
</testsuite></testsuites>"#;
        assert_eq!(
            parse(pytest).expect("parses"),
            vec![
                ("tests.test_feed::test_gap".to_string(), TestStatus::Error),
                ("bare".to_string(), TestStatus::Pass),
            ]
        );
    }

    #[test]
    fn a_broken_report_is_an_error_not_an_empty_pass() {
        for broken in [
            "<testsuite><testcase name='a'>",
            "<testsuites><testsuite name='x'><testcase name='a'/>",
            "<html></html>",
            "",
        ] {
            assert!(parse(broken).is_err(), "{broken:?}");
        }
        assert_eq!(parse("<testsuite/>"), Ok(Vec::new()));
    }
}
