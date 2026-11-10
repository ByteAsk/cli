use std::fmt::Write as _;

use serde::Deserialize;
use serde::Serialize;

/// Severity of a single finding, normalized across every C/C++ tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// A normalized diagnostic produced by any C/C++ tool (cppcheck, clang-tidy,
/// the compiler, ...). Every future tool emits this same shape so findings can
/// be rendered and ranked uniformly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Tool that produced the finding, e.g. "cppcheck".
    pub source: String,
    pub severity: Severity,
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
    /// Tool-specific rule/check id, when available.
    pub rule: Option<String>,
}

/// Render findings as a compact one-line-per-finding table, capping output at
/// `cap` rows and noting how many were elided.
///
/// Each row is `source severity file:line:col message [rule]`.
pub fn render_findings(findings: &[Finding], cap: usize) -> String {
    if findings.is_empty() {
        return "No findings.".to_string();
    }

    let mut out = String::new();
    for finding in findings.iter().take(cap) {
        let _ = write!(
            out,
            "{} {} {}:{}:{} {}",
            finding.source,
            finding.severity.as_str(),
            finding.file,
            finding.line,
            finding.column,
            finding.message,
        );
        if let Some(rule) = &finding.rule {
            let _ = write!(out, " [{rule}]");
        }
        out.push('\n');
    }

    if findings.len() > cap {
        let elided = findings.len() - cap;
        let _ = write!(out, "... {elided} more finding(s) elided (showing {cap}).");
    } else {
        // Drop the trailing newline for a tidy block.
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn finding(severity: Severity, line: u32, message: &str, rule: Option<&str>) -> Finding {
        Finding {
            source: "cppcheck".to_string(),
            severity,
            file: "main.cpp".to_string(),
            line,
            column: 4,
            message: message.to_string(),
            rule: rule.map(str::to_string),
        }
    }

    #[test]
    fn renders_empty_findings() {
        assert_eq!(render_findings(&[], 50), "No findings.");
    }

    #[test]
    fn renders_one_line_per_finding_with_rule() {
        let findings = vec![
            finding(Severity::Error, 10, "null pointer", Some("nullPointer")),
            finding(Severity::Warning, 20, "unused variable", None),
        ];
        assert_eq!(
            render_findings(&findings, 50),
            "cppcheck error main.cpp:10:4 null pointer [nullPointer]\n\
             cppcheck warning main.cpp:20:4 unused variable"
        );
    }

    #[test]
    fn caps_and_notes_elided_findings() {
        let findings = vec![
            finding(Severity::Note, 1, "a", None),
            finding(Severity::Note, 2, "b", None),
            finding(Severity::Note, 3, "c", None),
        ];
        let rendered = render_findings(&findings, 2);
        assert_eq!(
            rendered,
            "cppcheck note main.cpp:1:4 a\n\
             cppcheck note main.cpp:2:4 b\n\
             ... 1 more finding(s) elided (showing 2)."
        );
    }
}
