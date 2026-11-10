//! `port_check` (the crate `byteask-port-check`) as an engine tool.
//!
//! The check itself, its model-facing description and its input schema belong to that crate
//! (`byteask_port_check::tool`); this file only hosts it like the other tools that run the
//! user's programs: the native-exec trust gate first, every strategy process started inside
//! the tool sandbox (profile `Analyze`: read-only except temp dirs, no network, so a strategy
//! under replay can never reach a live venue), paths resolved against the thread's
//! workspace, and the harness's PASS / FAIL / NOT RUN / NO CLAIM mapped onto the shared
//! result line. The harness blocks for minutes, so it runs off the async runtime.

use std::path::Path;
use std::path::PathBuf;

use byteask_port_check::PortCheckRequest;
use byteask_port_check::PortCheckResult;
use byteask_port_check::Status;
use byteask_port_check::tool::TOOL_DESCRIPTION;
use byteask_port_check::tool::TOOL_NAME;
use check_result::CheckResult;
use check_result::NotRunReason;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;

/// The evidence block is shown up to this size; the verdict never depends on it.
const EVIDENCE_CAP: usize = 24 * 1024;

pub struct PortCheckTool {
    pub cwd: PathBuf,
    pub native_exec: bool,
}

/// The port-check crate's JSON Schema as the engine's schema type. Keywords the engine's subset does not
/// model (`minimum`, `maximum`, `exclusiveMaximum`) are dropped from what the model sees; the
/// harness still enforces every limit when it reads the request.
pub(crate) fn parameters() -> JsonSchema {
    serde_json::from_value(byteask_port_check::tool::input_schema()).unwrap_or_default()
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Relative paths in the request mean the thread's workspace, not the engine's own cwd.
fn resolve_paths(req: &mut PortCheckRequest, cwd: &Path) {
    req.recording = absolute(cwd, &req.recording);
    if let Some(book) = &req.initial_book {
        req.initial_book = Some(absolute(cwd, book));
    }
    if let Some(workdir) = &req.workdir {
        req.workdir = Some(absolute(cwd, workdir));
    }
    for spec in [&mut req.python, &mut req.cpp] {
        spec.cwd = Some(match &spec.cwd {
            Some(dir) => absolute(cwd, dir),
            None => cwd.to_path_buf(),
        });
    }
}

/// Start both versions through the tool sandbox. Returns the protected-name placeholders the
/// sandbox may leave behind, to remove after the run.
fn sandbox_versions(req: &mut PortCheckRequest) -> Result<Vec<PathBuf>, FunctionCallError> {
    let mut placeholders = Vec::new();
    for spec in [&mut req.python, &mut req.cpp] {
        let Some((program, args)) = spec.argv.split_first() else {
            continue; // the harness answers NOT RUN for an empty argv
        };
        let cwd = spec.cwd.clone().unwrap_or_default();
        let wrapped = crate::sandbox::wrap(program, args, &cwd)?;
        if wrapped.sandboxed {
            placeholders.extend(wrapped.placeholders.iter().cloned());
            let mut launch = vec![wrapped.program.clone()];
            launch.extend(wrapped.args.iter().cloned());
            spec.launch = Some(launch);
        }
    }
    Ok(placeholders)
}

/// The harness's verdict as the shared result.
pub(crate) fn verdict(result: &PortCheckResult) -> CheckResult {
    let sentence = result.sentence.lines().next().unwrap_or_default().trim();
    // The harness's sentence starts with its own verdict word; the result line already says it.
    let sentence = ["PASS:", "FAIL:", "NOT RUN:", "NO CLAIM:"]
        .iter()
        .find_map(|word| sentence.strip_prefix(word))
        .map_or(sentence, str::trim_start);
    match result.status {
        Status::Pass => CheckResult::pass(TOOL_NAME, sentence),
        Status::Fail => CheckResult::fail(TOOL_NAME, sentence),
        Status::NoClaim => CheckResult::no_claim(TOOL_NAME, sentence),
        Status::NotRun => {
            let reason_text = result.reason.as_deref().unwrap_or(sentence);
            let lower = reason_text.to_ascii_lowercase();
            let reason = if lower.contains("timed out") || lower.contains("timeout") {
                NotRunReason::TimeLimit
            } else if lower.contains("request is not valid") {
                NotRunReason::BadArguments
            } else {
                NotRunReason::ToolError
            };
            CheckResult::not_run(TOOL_NAME, reason, sentence, "")
        }
    }
}

fn body(result: &PortCheckResult) -> String {
    let mut text = format!("{}\n", result.sentence.trim_end());
    if let Some(diff) = &result.first_difference {
        text.push_str("\nfirst difference:\n");
        text.push_str(&serde_json::to_string_pretty(diff).unwrap_or_default());
        text.push('\n');
    }
    let mut evidence = serde_json::to_string_pretty(&result.evidence).unwrap_or_default();
    if evidence.len() > EVIDENCE_CAP {
        let mut end = EVIDENCE_CAP;
        while !evidence.is_char_boundary(end) {
            end -= 1;
        }
        evidence.truncate(end);
        evidence
            .push_str("\n... [evidence cut for length; the verdict above covers the whole run]");
    }
    text.push_str("\nevidence:\n");
    text.push_str(&evidence);
    text.push('\n');
    text
}

impl<'call> ToolExecutor<ToolCall<'call>> for PortCheckTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            strict: false,
            parameters: parameters(),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl PortCheckTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let mut req: PortCheckRequest = serde_json::from_str(arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!("the request is not valid: {err}"))
        })?;
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
        resolve_paths(&mut req, &self.cwd);
        let placeholders = sandbox_versions(&mut req)?;
        let target = req.recording.display().to_string();
        let ran = tokio::task::spawn_blocking(move || byteask_port_check::port_check(&req)).await;
        crate::sandbox::remove_placeholders(&placeholders);
        let result = ran.unwrap_or_else(|err| {
            byteask_port_check::tool::not_run_result(format!(
                "the check stopped unexpectedly: {err}"
            ))
        });
        Ok(crate::report::report(
            TOOL_NAME,
            &target,
            body(&result),
            verdict(&result),
        ))
    }
}

#[cfg(test)]
#[path = "port_check_tests.rs"]
mod tests;
