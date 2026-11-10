//! Local-only: when `policy.toml` says
//! `[network] local_only = true`, a tool that would send the question off this
//! machine answers NOT RUN (offline) without sending anything. Its spec and
//! everything else are delegated unchanged, so the tool list and its pinned
//! text do not change.

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

type SharedTool = Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>;

pub(crate) struct Offline {
    inner: SharedTool,
    destination: &'static str,
}

impl Offline {
    /// `tool` answering NOT RUN (offline) instead of reaching `destination`.
    pub(crate) fn wrap(inner: SharedTool, destination: &'static str) -> SharedTool {
        Arc::new(Self { inner, destination })
    }
}

/// The answer an offline tool gives.
pub(crate) fn offline_answer(tool: &str, destination: &str) -> String {
    let summary = byteask_policy::local_only_summary(tool, destination);
    CheckResult::not_run(
        tool,
        NotRunReason::Offline,
        &summary,
        byteask_policy::LOCAL_ONLY_FIX,
    )
    .append_to(&format!("{summary}."))
}

impl<'call> ToolExecutor<ToolCall<'call>> for Offline {
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

    fn handle<'a>(&'a self, _call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        let tool = self.inner.tool_name().name;
        let answer = offline_answer(&tool, self.destination);
        Box::pin(async move { Err(FunctionCallError::RespondToModel(answer)) })
    }
}

/// Where each online C/C++ tool sends its request, or `None` for a tool that
/// works on this machine.
pub(crate) fn destination(tool: &str) -> Option<&'static str> {
    match tool {
        "cpp_godbolt" => Some("Compiler Explorer"),
        "cpp_insights" => Some("C++ Insights"),
        "cpp_bench" => Some("Quick C++ Benchmark"),
        "cpp_intrinsic" => Some("ByteAsk's intrinsics service"),
        _ => None,
    }
}
