//! The shared result for the reference tools.
//!
//! A normal answer gets its verdict from [`search_result`] / [`context_result`];
//! an error gets a `NOT RUN` from [`WithResult`]. The same rules as the C/C++
//! tools (`ext/cpp-tools/src/results.rs`): only PASS is success, and an error
//! is never anything but NOT RUN. Kept separate rather than shared because
//! this crate does not depend on the C/C++ crate, and the shared part of the
//! contract lives in `check_result` itself.

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

/// The marker every search tool's description promises for a miss.
const NO_MATCH: &str = "no confident match";

pub(crate) fn search_result(tool: &str, text: &str) -> CheckResult {
    if text.to_ascii_lowercase().contains(NO_MATCH) {
        CheckResult::no_claim(
            tool,
            "No confident match in the indexed documents, so nothing is claimed from them.",
        )
    } else {
        CheckResult::pass(
            tool,
            "Returned page-cited excerpts from the indexed documents.",
        )
    }
}

pub(crate) fn context_result(text: &str) -> CheckResult {
    if text.trim().is_empty() || text.contains("_Not found_") {
        CheckResult::no_claim("ref_context", "That reference id was not found.")
    } else {
        CheckResult::pass("ref_context", "Returned the full section.")
    }
}

/// The NOT RUN for an error message without a result line.
pub(crate) fn error_result(tool: &str, message: &str) -> CheckResult {
    let lower = message.to_ascii_lowercase();
    let (reason, fix) = if lower.contains("timed out") || lower.contains("did not answer within") {
        (
            NotRunReason::TimeLimit,
            format!(
                "call {tool} again in a moment; if it keeps timing out, answer without it and say so."
            ),
        )
    } else if lower.contains("unreachable")
        || lower.contains("lookup failed")
        || lower.contains("reference server")
        || lower.contains("reference response")
    {
        (
            NotRunReason::ServiceUnavailable,
            format!(
                "check the network, then call {tool} again; until then, say the answer is not from the documents."
            ),
        )
    } else if lower.contains("missing field")
        || lower.contains("unknown field")
        || lower.contains("invalid type")
        || lower.contains("must not be empty")
    {
        (
            NotRunReason::BadArguments,
            format!(
                "call {tool} again with the arguments its schema lists; the message above names the wrong one."
            ),
        )
    } else {
        (NotRunReason::ToolError, String::new())
    };
    CheckResult::not_run(
        tool,
        reason,
        "The lookup stopped with the message above; nothing was retrieved.",
        &fix,
    )
}

fn with_result(tool: &str, err: FunctionCallError) -> FunctionCallError {
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

/// A tool whose every error carries a result line; everything else delegated.
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
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_miss_is_no_claim_and_a_hit_is_pass() {
        let miss = search_result("hft_ref", "_no confident match for 'x'_");
        assert_eq!(miss.verdict, check_result::Verdict::NoClaim);
        let hit = search_result("hft_ref", "### ITCH 5.0 §1.3.1 p.13\nAdd Order ...");
        assert!(hit.is_pass());
        assert_eq!(
            context_result("_Not found_").verdict,
            check_result::Verdict::NoClaim
        );
        assert!(context_result("## 1.3.1 Add Order\nbody").is_pass());
    }

    #[test]
    fn every_error_is_not_run_with_a_fix() {
        for message in [
            "ByteAsk reference server unreachable: connection refused",
            "ByteAsk reference lookup timed out after 30s",
            "missing field `query` at line 1 column 2",
            "something nobody anticipated",
        ] {
            let err = with_result(
                "hft_ref",
                FunctionCallError::RespondToModel(message.to_string()),
            );
            let FunctionCallError::RespondToModel(text) = err else {
                panic!("stays model-facing");
            };
            assert!(text.starts_with(message), "the message is kept: {text}");
            let result = CheckResult::from_tool_text("hft_ref", &text).expect("a result line");
            assert_eq!(result.verdict, check_result::Verdict::NotRun, "{text}");
            assert!(result.fix.is_some_and(|fix| !fix.is_empty()), "{text}");
        }
    }

    #[test]
    fn an_error_that_already_has_a_result_is_left_alone() {
        let line = CheckResult::not_run("hft_ref", NotRunReason::Offline, "Offline.", "connect.")
            .append_to("no network");
        let err = with_result("hft_ref", FunctionCallError::RespondToModel(line.clone()));
        let FunctionCallError::RespondToModel(text) = err else {
            panic!();
        };
        assert_eq!(text, line);
    }
}
