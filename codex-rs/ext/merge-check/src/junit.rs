//! The JUnit XML report the profile's test commands write (CTest `--output-junit`, GoogleTest
//! `--gtest_output=xml`, Catch2 `-r junit`), read for the expected-test manifest.

use quick_xml::Reader;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub classname: String,
    pub name: String,
    pub status: CaseStatus,
}

impl Case {
    /// The names a manifest line may use for this test: `name` and `classname.name`.
    pub fn matches(&self, wanted: &str) -> bool {
        wanted == self.name
            || (!self.classname.is_empty() && wanted == format!("{}.{}", self.classname, self.name))
    }
}

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

fn status_from_attrs(element: &BytesStart<'_>) -> CaseStatus {
    // CTest: status="notrun"/"disabled"; GoogleTest: result="skipped"/"suppressed".
    let status = attr(element, b"status")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let result = attr(element, b"result")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(status.as_str(), "notrun" | "disabled" | "skipped")
        || matches!(result.as_str(), "skipped" | "suppressed")
    {
        CaseStatus::Skipped
    } else if matches!(status.as_str(), "fail" | "failed") {
        CaseStatus::Failed
    } else {
        CaseStatus::Passed
    }
}

/// Every test case in the report, or why it could not be read.
pub fn parse(xml: &str) -> Result<Vec<Case>, String> {
    let mut reader = Reader::from_str(xml);
    let mut cases = Vec::new();
    let mut open: Option<Case> = None;
    let mut saw_root = false;
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Err(err) => return Err(format!("the test report is not valid XML: {err}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(element))
                if element.name().as_ref() == b"testsuites"
                    || element.name().as_ref() == b"testsuite" =>
            {
                saw_root = true;
                depth += 1;
            }
            Ok(Event::Empty(element))
                if element.name().as_ref() == b"testsuites"
                    || element.name().as_ref() == b"testsuite" =>
            {
                saw_root = true;
            }
            Ok(Event::End(element))
                if element.name().as_ref() == b"testsuites"
                    || element.name().as_ref() == b"testsuite" =>
            {
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Start(element)) if element.name().as_ref() == b"testcase" => {
                open = Some(Case {
                    classname: attr(&element, b"classname").unwrap_or_default(),
                    name: attr(&element, b"name").unwrap_or_default(),
                    status: status_from_attrs(&element),
                });
            }
            Ok(Event::Empty(element)) if element.name().as_ref() == b"testcase" => {
                cases.push(Case {
                    classname: attr(&element, b"classname").unwrap_or_default(),
                    name: attr(&element, b"name").unwrap_or_default(),
                    status: status_from_attrs(&element),
                });
            }
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if let Some(case) = open.as_mut() {
                    match element.name().as_ref() {
                        b"failure" | b"error" => case.status = CaseStatus::Failed,
                        b"skipped" if case.status != CaseStatus::Failed => {
                            case.status = CaseStatus::Skipped;
                        }
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
        return Err("the test report has no <testsuites> or <testsuite> element".to_string());
    }
    if open.is_some() || depth > 0 {
        // A report cut off mid-write must not read as "these were all the tests".
        return Err("the test report was cut off before its end".to_string());
    }
    Ok(cases)
}

/// The manifest's test names: one per line, `#` starts a comment, blank lines ignored.
pub fn manifest_names(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn reads_ctest_and_gtest_reports() {
        let ctest = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="Linux" tests="4" failures="1" disabled="1" skipped="1">
  <testcase name="book_add" classname="book_add" time="0.01" status="run"/>
  <testcase name="book_cross" classname="book_cross" time="0.01" status="fail">
    <failure message="Failed"/>
  </testcase>
  <testcase name="feed_gap" classname="feed_gap" status="notrun"><skipped message="Not Run"/></testcase>
  <testcase name="old" classname="old" status="disabled"/>
</testsuite>"#;
        let cases = parse(ctest).expect("parses");
        let status: Vec<(&str, CaseStatus)> =
            cases.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            status,
            vec![
                ("book_add", CaseStatus::Passed),
                ("book_cross", CaseStatus::Failed),
                ("feed_gap", CaseStatus::Skipped),
                ("old", CaseStatus::Skipped),
            ]
        );
        let gtest = r#"<testsuites><testsuite name="Book">
  <testcase name="Adds" status="run" result="completed" classname="Book"/>
  <testcase name="Later" status="run" result="skipped" classname="Book"><skipped message="x"/></testcase>
  <testcase name="Crosses" status="run" result="completed" classname="Book"><failure message="boom">x</failure></testcase>
</testsuite></testsuites>"#;
        let cases = parse(gtest).expect("parses");
        assert!(cases[0].matches("Book.Adds") && cases[0].matches("Adds"));
        assert_eq!(cases[1].status, CaseStatus::Skipped);
        assert_eq!(cases[2].status, CaseStatus::Failed);
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
        assert_eq!(
            parse("<testsuite/>"),
            Ok(Vec::new()),
            "an empty suite is a valid report"
        );
    }

    #[test]
    fn manifest_lines() {
        assert_eq!(
            manifest_names("# required\nbook_add\n\n  Book.Crosses  # the cross test\n"),
            vec!["book_add".to_string(), "Book.Crosses".to_string()]
        );
    }
}
