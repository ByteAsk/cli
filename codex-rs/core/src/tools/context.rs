use crate::original_image_detail::sanitize_original_image_detail;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::output::CapturedStream;
use crate::unified_exec::resolve_max_tokens;
use codex_protocol::ResponseItemId;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolName;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_function_output_payload;
use codex_utils_output_truncation::with_serialization_allowance;
use codex_utils_path_uri::PathUri;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub use codex_tools::ToolOutput;
pub use codex_tools::ToolPayload;

pub(crate) fn boxed_tool_output<T>(output: T) -> Box<dyn ToolOutput>
where
    T: ToolOutput + 'static,
{
    Box::new(output)
}

pub type SharedTurnDiffTracker = Arc<Mutex<TurnDiffTracker>>;

/// Host-observed state for one call, owned outside its abortable dispatch task.
/// Delivery does not finish the call: post-tool hooks can still be cancelled.
#[derive(Default)]
pub(crate) struct ToolCallState {
    pub(crate) terminal_outcome_reached: AtomicBool,
    pub(crate) delivered_assistant_message: OnceLock<String>,
    pub(crate) invocation: crate::agent::host::InvocationReservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCallSource {
    Direct,
    DirectPlaintextMessage,
    CodeMode {
        /// Runtime cell that issued the nested tool request.
        cell_id: String,
        /// Code-mode's per-cell tool invocation id. This is useful for
        /// debugging the JS/runtime bridge, but it is not the Codex tool call id
        /// because the runtime id only needs to be unique within one cell.
        runtime_tool_call_id: String,
    },
}

/// Targets captured from the original call. Environments follow the order declared by the tool.
#[derive(Clone, Default)]
pub(crate) enum ToolEnvironments {
    #[default]
    All,
    Terminal(crate::unified_exec::TerminalSession),
    Arguments(Vec<(&'static str, Arc<TurnEnvironment>)>),
    Mcp {
        server: Option<String>,
        owner: Option<String>,
    },
}

impl ToolEnvironments {
    pub(crate) fn single_executor(env: &TurnEnvironment) -> Self {
        Self::Arguments(vec![("environment_id", Arc::new(env.clone()))])
    }

    pub(crate) fn single_id(&self) -> Option<&str> {
        match self {
            Self::Arguments(args) => match args.as_slice() {
                [(_, env)] => Some(&env.selection.environment_id),
                _ => None,
            },
            Self::Terminal(terminal) => Some(&terminal.environment_id),
            Self::Mcp { owner, .. } => owner.as_deref(),
            Self::All => None,
        }
    }

    pub(crate) fn hook_ids(&self) -> Vec<&str> {
        match self {
            Self::Arguments(args) => args
                .iter()
                .map(|(_, env)| env.selection.environment_id.as_str())
                .collect(),
            _ => self.single_id().into_iter().collect(),
        }
    }
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub session: Arc<Session>,
    // TODO(sayan): Remove this compatibility field once handlers use `step_context.turn`.
    pub turn: Arc<TurnContext>,
    pub(crate) step_context: Arc<StepContext>,
    pub cancellation_token: CancellationToken,
    pub tracker: SharedTurnDiffTracker,
    pub call_id: String,
    pub tool_name: ToolName,
    pub source: ToolCallSource,
    pub payload: ToolPayload,
    /// Original function-call evidence owned by this invocation, independent of live history.
    pub native_call: Option<Arc<ResponseItem>>,
    /// Captured before PreToolUse and retained across input rewrites.
    pub(crate) environments: ToolEnvironments,
}

/// Identity of the model call that started a tool invocation, retained across code-mode waits.
#[derive(Clone)]
pub(crate) struct ToolCallOrigin {
    /// Best-effort lookup: invocations without a matching history item still
    /// retain their known window ID.
    pub(crate) item_id: Option<ResponseItemId>,
    pub(crate) window_id: String,
}

impl ToolInvocation {
    /// Returns the item and window that requested this call or started its code-mode cell.
    pub(crate) async fn originating_call(&self) -> Option<ToolCallOrigin> {
        if let ToolCallSource::CodeMode { cell_id, .. } = &self.source {
            return self
                .session
                .services
                .code_mode_service
                .cell_originating_call(&codex_code_mode::CellId::new(cell_id.clone()));
        }

        let item_id = self
            .session
            .clone_history()
            .await
            .raw_items()
            .rev()
            .find_map(|item| match item {
                ResponseItem::FunctionCall { id, call_id, .. }
                | ResponseItem::CustomToolCall { id, call_id, .. }
                    if call_id == &self.call_id =>
                {
                    id.clone()
                }
                _ => None,
            });
        Some(ToolCallOrigin {
            item_id,
            window_id: self.session.current_window_id().await,
        })
    }
}

#[derive(Clone, Debug)]
pub struct McpToolOutput {
    pub result: CallToolResult,
    pub tool_input: JsonValue,
    // Keep the original metadata for hooks; this flag only controls analytics capture.
    pub(crate) result_metadata_capture_allowed: bool,
    pub wall_time: Duration,
    pub original_image_detail_supported: bool,
    pub truncation_policy: TruncationPolicy,
}

impl ToolOutput for McpToolOutput {
    fn log_output(&self) -> String {
        // Logging has its own budget; do not first apply the model-context budget.
        let output = self.result.log_output();
        let wall_time_seconds = self.wall_time.as_secs_f64();
        let header = format!("Wall time: {wall_time_seconds:.4} seconds\nOutput:");
        if output.is_empty() {
            header
        } else {
            format!("{header}\n{output}")
        }
    }

    fn success_for_logging(&self) -> bool {
        self.result.success()
    }

    fn fallback_token_limit_override(&self) -> Option<usize> {
        Some(with_serialization_allowance(self.truncation_policy).token_budget())
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.response_payload(),
        }
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        self.result.code_mode_result(payload)
    }

    fn tool_result_metadata(&self) -> Option<&JsonValue> {
        if !self.result_metadata_capture_allowed {
            return None;
        }
        self.result.meta.as_ref()
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.tool_input.clone())
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        serde_json::to_value(&self.result).ok()
    }
}

impl McpToolOutput {
    fn response_payload(&self) -> FunctionCallOutputPayload {
        let mut payload = self.result.as_function_call_output_payload();
        if let Some(items) = payload.content_items_mut() {
            sanitize_original_image_detail(self.original_image_detail_supported, items);
        }

        let wall_time_seconds = self.wall_time.as_secs_f64();
        let header = format!("Wall time: {wall_time_seconds:.4} seconds\nOutput:");

        match &mut payload.body {
            FunctionCallOutputBody::Text(text) => {
                if text.is_empty() {
                    *text = header;
                } else {
                    *text = format!("{header}\n{text}");
                }
            }
            FunctionCallOutputBody::ContentItems(items) => {
                items.insert(0, FunctionCallOutputContentItem::InputText { text: header });
            }
        }

        // History receives this budget in tokens. Code Mode keeps the raw result.
        truncate_function_output_payload(
            &mut payload,
            with_serialization_allowance(self.truncation_policy),
            estimate_audio_token_count,
        );
        payload
    }
}

#[derive(Clone)]
pub struct ToolSearchOutput {
    pub tools: Vec<LoadableToolSpec>,
}

impl ToolOutput for ToolSearchOutput {
    fn log_output(&self) -> String {
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                serde_json::to_value(tool).unwrap_or_else(|err| {
                    JsonValue::String(format!("failed to serialize tool_search output: {err}"))
                })
            })
            .collect();
        JsonValue::Array(tools).to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::ToolSearchOutput {
            call_id: call_id.to_string(),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: self
                .tools
                .iter()
                .map(|tool| {
                    serde_json::to_value(tool).unwrap_or_else(|err| {
                        JsonValue::String(format!("failed to serialize tool_search output: {err}"))
                    })
                })
                .collect(),
        }
    }
}

pub struct FunctionToolOutput {
    pub body: Vec<FunctionCallOutputContentItem>,
    pub success: Option<bool>,
    pub post_tool_use_response: Option<JsonValue>,
}

impl FunctionToolOutput {
    pub fn from_text(text: String, success: Option<bool>) -> Self {
        Self {
            body: vec![FunctionCallOutputContentItem::InputText { text }],
            success,
            post_tool_use_response: None,
        }
    }

    pub fn from_content(
        content: Vec<FunctionCallOutputContentItem>,
        success: Option<bool>,
    ) -> Self {
        Self {
            body: content,
            success,
            post_tool_use_response: None,
        }
    }

    pub fn into_text(self) -> String {
        function_call_output_content_items_to_text(&self.body).unwrap_or_default()
    }
}

impl ToolOutput for FunctionToolOutput {
    fn log_output(&self) -> String {
        function_call_output_content_items_to_text(&self.body).unwrap_or_default()
    }

    fn success_for_logging(&self) -> bool {
        self.success.unwrap_or(true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(call_id, payload, self.body.clone(), self.success)
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        self.post_tool_use_response.clone()
    }
}

pub struct ApplyPatchToolOutput {
    pub text: String,
}

impl ApplyPatchToolOutput {
    pub fn from_text(text: String) -> Self {
        Self { text }
    }
}

impl ToolOutput for ApplyPatchToolOutput {
    fn log_output(&self) -> String {
        self.text.clone()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.text.clone(),
            }],
            Some(true),
        )
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(JsonValue::String(self.text.clone()))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        JsonValue::Object(serde_json::Map::new())
    }
}

pub struct AbortedToolOutput {
    pub message: String,
}

impl ToolOutput for AbortedToolOutput {
    fn log_output(&self) -> String {
        self.message.clone()
    }

    fn success_for_logging(&self) -> bool {
        false
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        match payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call_id.to_string(),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
            },
            _ => function_tool_response(
                call_id,
                payload,
                vec![FunctionCallOutputContentItem::InputText {
                    text: self.message.clone(),
                }],
                /*success*/ None,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecCommandToolOutput {
    pub event_call_id: String,
    pub chunk_id: String,
    pub wall_time: Duration,
    /// Output retained by collection, before model-facing truncation.
    pub(crate) raw_output: CapturedStream,
    pub truncation_policy: TruncationPolicy,
    pub max_output_tokens: Option<usize>,
    pub process_id: Option<i32>,
    pub exit_code: Option<i32>,
    pub original_token_count: Option<usize>,
    pub hook_command: Option<String>,
    /// Local execution cwd retained for PostToolUse, independent of the collecting
    /// turn. Remote commands leave this unset to retain the host hook cwd.
    pub tool_cwd: Option<PathUri>,
    /// ByteAsk: set when a wait floor, not the model's `yield_time_ms`, decided how long
    /// this call waited on a still-running command. Shown after the session line.
    pub wait_note: Option<String>,
}

impl ToolOutput for ExecCommandToolOutput {
    fn log_output(&self) -> String {
        // The telemetry budget must not inherit the model's output-token limit.
        let output = self
            .raw_output
            .render(/*policy*/ None, self.original_token_count);
        format!("{}\n{output}", self.response_header())
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.response_text(),
            }],
            Some(true),
        )
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        if self.event_call_id.is_empty() {
            call_id.to_string()
        } else {
            self.event_call_id.clone()
        }
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        self.hook_command
            .as_ref()
            .map(|command| serde_json::json!({ "command": command }))
    }

    fn post_tool_use_cwd(&self) -> Option<PathUri> {
        self.tool_cwd.clone()
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        if self.process_id.is_some() || self.hook_command.is_none() {
            return None;
        }

        Some(JsonValue::String(
            self.truncated_output_with_policy(self.model_output_policy()),
        ))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        #[derive(Serialize)]
        struct UnifiedExecCodeModeResult {
            #[serde(skip_serializing_if = "Option::is_none")]
            chunk_id: Option<String>,
            wall_time_seconds: f64,
            #[serde(skip_serializing_if = "Option::is_none")]
            exit_code: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            original_token_count: Option<usize>,
            output: String,
        }

        let result = UnifiedExecCodeModeResult {
            chunk_id: (!self.chunk_id.is_empty()).then(|| self.chunk_id.clone()),
            wall_time_seconds: self.wall_time.as_secs_f64(),
            exit_code: self.exit_code,
            session_id: self.process_id,
            original_token_count: self.original_token_count,
            output: match self.max_output_tokens {
                Some(max_tokens) => self.truncated_output(max_tokens),
                None => self.raw_output.text.clone(),
            },
        };

        serde_json::to_value(result).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize exec result: {err}"))
        })
    }
}

impl ExecCommandToolOutput {
    fn model_output_policy(&self) -> TruncationPolicy {
        let requested_policy = TruncationPolicy::Tokens(resolve_max_tokens(self.max_output_tokens));
        if requested_policy.byte_budget() < self.truncation_policy.byte_budget() {
            requested_policy
        } else {
            self.truncation_policy
        }
    }

    pub(crate) fn truncated_output(&self, max_tokens: usize) -> String {
        self.truncated_output_with_policy(TruncationPolicy::Tokens(max_tokens))
    }

    fn truncated_output_with_policy(&self, policy: TruncationPolicy) -> String {
        self.raw_output
            .render(Some(policy), self.original_token_count)
    }

    fn truncated_text_with_policy(&self, text: &str, policy: TruncationPolicy) -> String {
        CapturedStream {
            text: text.to_string(),
            omitted_bytes: self.raw_output.omitted_bytes,
        }
        .render(Some(policy), self.original_token_count)
    }

    fn response_header(&self) -> String {
        if byteask_output_reducers::policy::active().flag("compact_header") {
            return self.byteask_compact_header();
        }
        let mut sections = Vec::new();

        if !self.chunk_id.is_empty() {
            sections.push(format!("Chunk ID: {}", self.chunk_id));
        }

        let wall_time_seconds = self.wall_time.as_secs_f64();
        sections.push(format!("Wall time: {wall_time_seconds:.4} seconds"));

        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }

        if let Some(process_id) = &self.process_id {
            sections.push(format!("Process running with session ID {process_id}"));
        }

        if let Some(note) = &self.wait_note {
            sections.push(note.clone());
        }

        if let Some(original_token_count) = self.original_token_count {
            sections.push(format!("Original token count: {original_token_count}"));
        }

        sections.push("Output:".to_string());
        sections.join("\n")
    }

    /// `[exit 0 | 1.2s]`, the session id only while the process is still running (the
    /// model needs it for `write_stdin`), the token count only when something was cut.
    /// Drops `Chunk ID` (an id no tool accepts back) and sub-second precision: a removal,
    /// so class N and off until the policy turns it on.
    pub(crate) fn byteask_compact_header(&self) -> String {
        let secs = self.wall_time.as_secs_f64();
        let mut first = match self.exit_code {
            Some(code) => format!("[exit {code} | {secs:.1}s]"),
            None => format!("[running | {secs:.1}s]"),
        };
        if let Some(process_id) = &self.process_id {
            first.push_str(&format!("\nProcess running with session ID {process_id}"));
        }
        if let Some(note) = &self.wait_note {
            first.push('\n');
            first.push_str(note);
        }
        if let Some(count) = self.original_token_count
            && (self.raw_output.omitted_bytes > 0
                || count > self.model_output_policy().token_budget())
        {
            first.push_str(&format!("\nOriginal token count: {count}"));
        }
        first
    }

    /// ByteAsk token efficiency: shrink what the model READS at
    /// the moment it enters the conversation. Only this model-facing text is affected;
    /// hooks, telemetry and the UI keep the raw output. Returns the text to truncate, the
    /// budget to truncate it to, and the footer to append (empty when nothing happened).
    fn byteask_reduce(&self, upstream: TruncationPolicy) -> (String, TruncationPolicy, String) {
        self.byteask_reduce_with(upstream, &byteask_output_reducers::policy::active())
    }

    pub(crate) fn byteask_reduce_with(
        &self,
        upstream: TruncationPolicy,
        eff: &byteask_output_reducers::Policy,
    ) -> (String, TruncationPolicy, String) {
        let raw = self.raw_output.text.clone();
        // `path:line:text` must really start with a path before it may be regrouped. An
        // absolute path is checked on disk; a relative one only structurally, because the
        // command's working directory is not known here. The family match (a search
        // command run with line numbers) is what makes that enough: without a file name
        // the first field is a number and the parse refuses.
        let path_exists = |p: &str| {
            let path = std::path::Path::new(p);
            if path.is_absolute() {
                path.exists()
            } else {
                !p.contains("://") && !p.contains(char::is_whitespace)
            }
        };
        let ctx = byteask_output_reducers::Ctx {
            command: self.hook_command.as_deref(),
            path_exists: Some(&path_exists),
        };
        let reduced = byteask_output_reducers::reduce(&raw, &ctx, eff);

        // A lower budget than upstream's is allowed ONLY when the original was spilled.
        let wanted = eff
            .budget_for(reduced.family)
            .map(TruncationPolicy::Tokens)
            .filter(|p| p.byte_budget() < upstream.byte_budget())
            .filter(|p| reduced.text.len() > p.byte_budget());
        let collection_cut = self.raw_output.omitted_bytes > 0;
        if !reduced.changed() && wanted.is_none() && !collection_cut {
            return (raw, upstream, String::new());
        }
        let spill_id = format!("{}_{}", self.event_call_id, self.chunk_id);
        let spilled =
            byteask_output_reducers::spill::write(&spill_id, self.raw_output.text.as_bytes());
        // Content was removed by a reducer: without the original on disk, undo it.
        let (text, fired) = if reduced.lossy && spilled.is_none() {
            let safe = byteask_output_reducers::reduce_terminal(&raw, eff.ansi, eff.cr);
            (safe.text, safe.fired)
        } else {
            (reduced.text, reduced.fired)
        };
        let policy = match (&spilled, wanted) {
            (Some(_), Some(p)) => p,
            _ => upstream,
        };
        let mut notes = fired.join(" ");
        if policy.byte_budget() < upstream.byte_budget() {
            if !notes.is_empty() {
                notes.push(' ');
            }
            notes.push_str("trimmed");
        }
        if notes.is_empty() {
            // Nothing of ours happened. Still point at the complete original when the
            // 1 MiB collector dropped the middle and a tee kept it.
            return match spilled {
                Some(path) if collection_cut && path.to_string_lossy().ends_with(".full.txt") => {
                    let footer = self.byteask_footer("complete output kept", &spill_id, &path, eff);
                    (raw, upstream, footer)
                }
                _ => (raw, upstream, String::new()),
            };
        }
        let footer = match spilled {
            Some(path) => self.byteask_footer(&notes, &spill_id, &path, eff),
            None => format!("\n[byteask: {notes}; original not retained]"),
        };
        (text, policy, footer)
    }

    fn byteask_footer(
        &self,
        notes: &str,
        spill_id: &str,
        path: &std::path::Path,
        eff: &byteask_output_reducers::Policy,
    ) -> String {
        let bytes = std::fs::metadata(path)
            .map(|m| m.len())
            .unwrap_or(self.raw_output.text.len() as u64);
        let lines = self.raw_output.text.lines().count();
        let approx = if self.raw_output.omitted_bytes > 0 {
            "more than "
        } else {
            ""
        };
        if eff.flag("native_read") {
            // A handle, not a path: fewer tokens, and the home directory stays local.
            format!(
                "\n[byteask: {notes}; full output ({approx}{lines} lines, {bytes} bytes) is spill:{spill_id}; read a range with read_file or search it with search, do not re-run the command]"
            )
        } else {
            format!(
                "\n[byteask: {notes}; full output ({approx}{lines} lines, {bytes} bytes) saved at {}; read a part of it with sed -n 'A,Bp' or search it with rg, do not re-run the command]",
                path.display()
            )
        }
    }

    fn response_text(&self) -> String {
        let header = self.response_header();
        let upstream_policy = self.model_output_policy();
        let (text, reduced_policy, footer) = self.byteask_reduce(upstream_policy);
        // The footer is written INSIDE the budget, so no later pass can cut it off.
        let output_budget = with_serialization_allowance(self.truncation_policy)
            .byte_budget()
            .saturating_sub(header.len().saturating_add(/*rhs*/ 1))
            .saturating_sub(footer.len());
        let mut policy = reduced_policy;
        let mut output = self.truncated_text_with_policy(&text, policy);

        // History applies this same serialization budget to the complete response.
        // Reserve room for metadata, warning headers, and the truncation marker so
        // it does not truncate an already-truncated output a second time.
        while output.len() > output_budget && policy.byte_budget() > 0 {
            let excess_bytes = output.len() - output_budget;
            policy = match policy {
                TruncationPolicy::Bytes(bytes) => {
                    TruncationPolicy::Bytes(bytes.saturating_sub(excess_bytes))
                }
                TruncationPolicy::Tokens(tokens) => TruncationPolicy::Tokens(
                    tokens.saturating_sub(TruncationPolicy::Bytes(excess_bytes).token_budget()),
                ),
            };
            output = self.truncated_text_with_policy(&text, policy);
        }

        format!("{header}\n{output}{footer}")
    }
}

fn function_tool_response(
    call_id: &str,
    payload: &ToolPayload,
    body: Vec<FunctionCallOutputContentItem>,
    success: Option<bool>,
) -> ResponseInputItem {
    let body = match body.as_slice() {
        [FunctionCallOutputContentItem::InputText { text }] => {
            FunctionCallOutputBody::Text(text.clone())
        }
        _ => FunctionCallOutputBody::ContentItems(body),
    };

    if matches!(payload, ToolPayload::Custom { .. }) {
        return ResponseInputItem::CustomToolCallOutput {
            call_id: call_id.to_string(),
            name: None,
            output: FunctionCallOutputPayload { body, success },
        };
    }

    ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload { body, success },
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
