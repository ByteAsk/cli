//! The shared result for every C/C++ tool.
//!
//! Every tool keeps its own text unchanged and gets one last line,
//! `Result: <VERDICT> ...`, from [`check_result::CheckResult`]:
//!
//! * a normal output: [`crate::report::report`] takes the result its caller
//!   decided, so a success path cannot be added without choosing a verdict;
//! * an error: [`WithResult`] wraps every tool and gives an error message
//!   without a result line a `NOT RUN` with the most specific reason it can
//!   tell ([`error_result`]). An error is never anything but `NOT RUN`.
//!
//! Only `PASS` is success: a check that did not finish, was cut, or could not
//! run is never shown as one.

use std::sync::Arc;

use check_result::CheckResult;
use check_result::NotRunReason;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_tools::ToolExposure;
use codex_tools::ToolSearchInfo;

use crate::finding::Finding;
use crate::finding::Severity;

/// The verdict for a static checker's findings (cppcheck, clang-tidy, clangd).
///
/// An error or a warning is a `FAIL`; notes alone are a `PASS` that says how
/// many notes there were. `truncated` with no finding is `NOT RUN`: the cut
/// part may have held the findings, so "none" cannot be claimed.
pub(crate) fn findings_result(tool: &str, findings: &[Finding], truncated: bool) -> CheckResult {
    let problems = findings
        .iter()
        .filter(|finding| matches!(finding.severity, Severity::Error | Severity::Warning))
        .count();
    let notes = findings.len() - problems;
    if problems > 0 {
        let first = findings
            .iter()
            .find(|finding| matches!(finding.severity, Severity::Error | Severity::Warning))
            .map(|finding| format!("{}:{}", finding.file, finding.line))
            .unwrap_or_default();
        let more = if truncated {
            " The output was cut, so there may be more."
        } else {
            ""
        };
        return CheckResult::fail(
            tool,
            &format!("{problems} error(s) or warning(s), the first at {first}.{more}"),
        );
    }
    if truncated {
        return CheckResult::not_run(
            tool,
            NotRunReason::OutputTruncated,
            "The output was cut before any finding was read, so no result can be claimed.",
            &format!("call {tool} again on a smaller path so the whole report fits."),
        );
    }
    if notes > 0 {
        CheckResult::pass(tool, &format!("No errors or warnings ({notes} note(s))."))
    } else {
        CheckResult::pass(tool, "No findings.")
    }
}

/// A `NOT RUN` because a time limit cut the work.
pub(crate) fn timed_out(tool: &str, what: &str, secs: u64, fix: &str) -> CheckResult {
    CheckResult::not_run(
        tool,
        NotRunReason::TimeLimit,
        &format!("{what} was stopped after {secs} s, so it says nothing."),
        fix,
    )
}

/// The `NOT RUN` for an error message that carries no result line. The
/// reason comes from the shapes our own helpers and serde produce; anything
/// unrecognised is a `tool error` with the generic fix.
pub(crate) fn error_result(tool: &str, message: &str) -> CheckResult {
    let lower = message.to_ascii_lowercase();
    let (reason, fix) = if message.contains(crate::trust::ALLOW_ENV) {
        (
            NotRunReason::NotTrusted,
            format!(
                "set {}=1 for this run, or mark the project trusted in config.toml (both shown above).",
                crate::trust::ALLOW_ENV
            ),
        )
    } else if lower.contains("is not installed") || lower.contains("not on path") {
        (
            NotRunReason::ToolMissing,
            "install the program named above, then call the tool again.".to_string(),
        )
    } else if lower.contains("missing field")
        || lower.contains("unknown field")
        || lower.contains("invalid type")
        || lower.contains("invalid value")
        || lower.contains("expected one of")
        || lower.contains("needs either")
        || lower.contains("must not be empty")
    {
        (
            NotRunReason::BadArguments,
            format!("call {tool} again with the arguments its schema lists; the message above names the wrong one."),
        )
    } else if lower.contains("timed out") || lower.contains("did not finish") {
        (
            NotRunReason::TimeLimit,
            "call the tool again with a longer timeout, or on less code.".to_string(),
        )
    } else if lower.contains("unavailable") || lower.contains("unreachable") || lower.contains("request failed") {
        (
            NotRunReason::ServiceUnavailable,
            "check the network, then call the tool again.".to_string(),
        )
    } else {
        (NotRunReason::ToolError, String::new())
    };
    CheckResult::not_run(tool, reason, "The tool stopped with the message above; nothing was checked.", &fix)
}

/// `err` with a result line, unless it already ends with one.
pub(crate) fn with_result(tool: &str, err: FunctionCallError) -> FunctionCallError {
    let (message, fatal) = match err {
        FunctionCallError::RespondToModel(message) => (message, false),
        FunctionCallError::Fatal(message) => (message, true),
    };
    let message = if CheckResult::from_tool_text(tool, &message).is_some() {
        message
    } else {
        error_result(tool, &message).append_to(&message)
    };
    if fatal {
        FunctionCallError::Fatal(message)
    } else {
        FunctionCallError::RespondToModel(message)
    }
}

/// A tool whose every error carries a result line. Everything else is
/// delegated unchanged, so the spec the model sees is byte-identical.
pub(crate) struct WithResult {
    inner: Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>,
}

impl WithResult {
    pub(crate) fn wrap(
        inner: Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>,
    ) -> Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>> {
        Arc::new(Self { inner })
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for WithResult {
    fn tool_name(&self) -> ToolName {
        self.inner.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.inner.search_info()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.inner.supports_parallel_tool_calls()
    }

    fn manages_turn_items(&self) -> bool {
        self.inner.manages_turn_items()
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let tool = self.inner.tool_name().name;
            self.inner
                .handle(call)
                .await
                .map_err(|err| with_result(&tool, err))
        })
    }
}

#[cfg(test)]
mod tests {
    use check_result::Verdict;
    use pretty_assertions::assert_eq;

    use super::*;

    fn finding(severity: Severity, line: u32) -> Finding {
        Finding {
            source: "cppcheck".to_string(),
            severity,
            file: "a.cpp".to_string(),
            line,
            column: 1,
            message: "m".to_string(),
            rule: None,
        }
    }

    #[test]
    fn findings_errors_and_warnings_fail_notes_alone_pass() {
        assert_eq!(findings_result("cpp_check", &[], false).verdict, Verdict::Pass);
        let notes = [finding(Severity::Note, 1)];
        assert_eq!(findings_result("cpp_check", &notes, false).verdict, Verdict::Pass);
        for severity in [Severity::Error, Severity::Warning] {
            let found = [finding(Severity::Note, 1), finding(severity, 7)];
            let result = findings_result("cpp_check", &found, false);
            assert_eq!(result.verdict, Verdict::Fail);
            assert!(result.summary.contains("a.cpp:7"), "{}", result.summary);
        }
    }

    /// The contract: a cut output never reads as clean.
    #[test]
    fn a_truncated_report_with_no_finding_is_not_run() {
        let result = findings_result("cpp_tidy", &[], true);
        assert_eq!(result.verdict, Verdict::NotRun);
        assert_eq!(result.reason, Some(NotRunReason::OutputTruncated));
        let notes_only = findings_result("cpp_tidy", &[finding(Severity::Note, 1)], true);
        assert_eq!(notes_only.verdict, Verdict::NotRun);
        let real = findings_result("cpp_tidy", &[finding(Severity::Error, 2)], true);
        assert_eq!(real.verdict, Verdict::Fail, "a real finding stays a FAIL");
    }

    #[test]
    fn a_time_limit_is_never_a_pass() {
        let result = timed_out("cpp_check", "cppcheck", 300, "narrow `path`.");
        assert_eq!(result.verdict, Verdict::NotRun);
        assert_eq!(result.reason, Some(NotRunReason::TimeLimit));
        assert!(result.summary.contains("300 s"));
    }

    #[test]
    fn errors_get_the_most_specific_reason() {
        let cases = [
            (
                crate::trust::refusal_message("cpp_sanitize", std::path::Path::new("/w")),
                NotRunReason::NotTrusted,
            ),
            (
                "'cppcheck' is not installed / not on PATH.".to_string(),
                NotRunReason::ToolMissing,
            ),
            (
                "missing field `source` at line 1 column 2".to_string(),
                NotRunReason::BadArguments,
            ),
            (
                "unknown field `args`, expected one of `source`, `run_args`".to_string(),
                NotRunReason::BadArguments,
            ),
            (
                "Quick Bench (quick-bench.com) is unavailable: request failed: dns".to_string(),
                NotRunReason::ServiceUnavailable,
            ),
            ("something unexpected".to_string(), NotRunReason::ToolError),
        ];
        for (message, reason) in cases {
            let result = error_result("cpp_x", &message);
            assert_eq!(result.verdict, Verdict::NotRun, "{message}");
            assert_eq!(result.reason, Some(reason), "{message}");
            assert!(result.fix.as_deref().is_some_and(|fix| !fix.is_empty()));
        }
    }

    #[test]
    fn with_result_appends_once_and_keeps_the_message_and_kind() {
        let err = with_result(
            "cpp_check",
            FunctionCallError::RespondToModel("no such file: a.cpp".to_string()),
        );
        let FunctionCallError::RespondToModel(text) = err else {
            panic!("a soft error must stay soft");
        };
        assert!(text.starts_with("no such file: a.cpp"));
        assert_eq!(
            CheckResult::from_tool_text("cpp_check", &text).map(|r| r.verdict),
            Some(Verdict::NotRun)
        );
        let again = with_result("cpp_check", FunctionCallError::RespondToModel(text.clone()));
        let FunctionCallError::RespondToModel(again) = again else {
            panic!();
        };
        assert_eq!(again, text, "a message that already has a result is left alone");
        assert!(matches!(
            with_result("cpp_check", FunctionCallError::Fatal("x".to_string())),
            FunctionCallError::Fatal(_)
        ));
    }
}
