//! The machine-readable block every sub-agent ends its final message with:
//! a line `BYTEASK_REPORT` followed by one JSON object. Parsing is tolerant: fenced blocks,
//! extra text and missing fields are fine; unknown fields are ignored.

use serde::Deserialize;
use serde::Serialize;

pub const REPORT_MARKER: &str = "BYTEASK_REPORT";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct TestNote {
    pub name: String,
    /// Why it fails on the starting code, in the writer's words.
    pub fails_on_base_because: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct SubagentReport {
    /// Command (from the worktree root) that builds the project, including the tests.
    pub build: String,
    /// Command that runs the project's existing tests.
    #[serde(alias = "test_suite")]
    pub test: String,
    /// Command that builds and runs this agent's own new tests (after their patch is applied).
    #[serde(alias = "tests_run")]
    pub run: String,
    pub tests: Vec<TestNote>,
    pub tests_patch: String,
}

impl SubagentReport {
    pub fn has_recipe(&self) -> bool {
        !self.build.trim().is_empty()
    }
}

/// Find and parse the report block in a final message.
pub fn parse_report(message: &str) -> Option<SubagentReport> {
    let idx = message.rfind(REPORT_MARKER)?;
    let rest = &message[idx + REPORT_MARKER.len()..];
    let start = rest.find('{')?;
    let body = &rest[start..];
    // Take the first balanced JSON object (strings may contain braces).
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut end = None;
    for (i, ch) in body.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let json = &body[..end?];
    serde_json::from_str::<SubagentReport>(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_block_with_fence_and_noise() {
        let msg = "Done. Summary...\n\nBYTEASK_REPORT\n```json\n{\"build\": \"make -j2 all\", \"test\": \"make check\", \"run\": \"./t/run.sh {x}\", \"tests\": [{\"name\": \"a\", \"fails_on_base_because\": \"compile: missing f()\"}], \"extra\": 1}\n```\n";
        let report = parse_report(msg).expect("report");
        assert_eq!(report.build, "make -j2 all");
        assert_eq!(report.test, "make check");
        assert_eq!(report.run, "./t/run.sh {x}");
        assert_eq!(report.tests.len(), 1);
        assert!(report.has_recipe());
    }

    #[test]
    fn aliases_and_missing() {
        let report =
            parse_report("BYTEASK_REPORT {\"test_suite\": \"ctest\", \"tests_run\": \"./a\"}")
                .expect("report");
        assert_eq!(report.test, "ctest");
        assert_eq!(report.run, "./a");
        assert!(!report.has_recipe());
        assert_eq!(parse_report("no block here"), None);
        assert_eq!(parse_report("BYTEASK_REPORT {not json"), None);
    }
}
