//! Test-only probe driver: replays tool calls from a JSONL file against the
//! REAL tool executors, so a broad edge-case sweep needs no model and no
//! rebuild per case. Not compiled into the engine (`#[cfg(test)]` only).
//!
//! ```text
//! BYTEASK_PROBE=cases.jsonl BYTEASK_PROBE_OUT=out.jsonl BYTEASK_TOOL_LOG=/dev/null \
//!   <test-binary> probe_driver::probe_driver --ignored --exact
//! ```
//!
//! Each input line is `{"id": "...", "tool": "cpp_sanitize", "args": {...}}`,
//! optionally with `"cwd"` (defaults to `BYTEASK_PROBE_CWD`, else the current
//! dir). Lines run in order and share one gdb session registry and one
//! `debug_attach` state, so a multi-call `debug_session` script works. Each
//! output line is `{"id", "tool", "ok", "text", "ms"}`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use codex_extension_api::ConversationHistory;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_utils_output_truncation::TruncationPolicy;
use serde_json::Value;
use serde_json::json;

use crate::gdbmi::GdbRegistry;
use crate::gdbmi::remote::KernelRegistry;
use crate::memsafety::FixVerifyTool;
use crate::tools::*;

pub(crate) type Tool = Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>;

pub(crate) fn tools_for(
    cwd: PathBuf,
    sessions: &Arc<GdbRegistry>,
    attach: &Arc<AttachState>,
    kernel: &Arc<KernelRegistry>,
) -> Vec<Tool> {
    let native_exec = true;
    vec![
        Arc::new(CppCheckTool { cwd: cwd.clone() }),
        Arc::new(ClangTidyTool { cwd: cwd.clone() }),
        Arc::new(SanitizeTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(FixVerifyTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(SymbolizeTool { cwd: cwd.clone() }),
        Arc::new(CompileDbTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(KdbTool {
            cwd: cwd.clone(),
            sessions: kernel.clone(),
            native_exec,
        }),
        Arc::new(GdbTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(GdbSessionTool {
            cwd: cwd.clone(),
            sessions: sessions.clone(),
            native_exec,
        }),
        Arc::new(GdbAttachTool {
            state: attach.clone(),
            full_access: false,
        }),
        Arc::new(PerfTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(InsightsTool { cwd: cwd.clone() }),
        Arc::new(ClangdNavTool { cwd: cwd.clone() }),
        Arc::new(ImpactTool {
            cwd: cwd.clone(),
            workflow: None,
        }),
        Arc::new(FormatTool { cwd: cwd.clone() }),
        Arc::new(BinaryTool { cwd: cwd.clone() }),
        Arc::new(BenchTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(RecordTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(ReverseTool {
            cwd: cwd.clone(),
            native_exec,
        }),
        Arc::new(CoreTriageTool { cwd: cwd.clone() }),
        Arc::new(CppLayoutTool { cwd: cwd.clone() }),
        Arc::new(DecompileTool { cwd: cwd.clone() }),
        Arc::new(GodboltTool { cwd: cwd.clone() }),
        Arc::new(IntrinsicTool {
            cwd: cwd.clone(),
            gateway_base: String::new(),
            bearer: None,
        }),
        Arc::new(TestSelectTool {
            cwd: cwd.clone(),
            native_exec,
            head: crate::session_head::SessionHead::spawn(cwd.clone()),
            jobs: crate::testsel::jobs::Jobs::new(),
            finish_gate_suite: false,
        }),
        Arc::new(RepoHistoryTool {
            head: crate::session_head::SessionHead::spawn(cwd),
        }),
    ]
}

pub(crate) fn output_text(item: ResponseInputItem) -> String {
    match item {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            if let Some(text) = output.text_content() {
                return text.to_string();
            }
            output
                .content_items()
                .unwrap_or_default()
                .iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        other => format!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual probe driver; set BYTEASK_PROBE"]
async fn probe_driver() {
    let Ok(cases) = std::env::var("BYTEASK_PROBE") else {
        return;
    };
    let default_cwd = std::env::var("BYTEASK_PROBE_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("cwd"));
    let out_path = std::env::var("BYTEASK_PROBE_OUT").expect("BYTEASK_PROBE_OUT");
    let mut out = std::fs::File::create(out_path).expect("create out");
    let sessions = GdbRegistry::new();
    let attach = AttachState::new();
    let kernel = KernelRegistry::new();

    let text = std::fs::read_to_string(cases).expect("read cases");
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with("//") {
            continue;
        }
        let case: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("line {n}: {e}"));
        let id = case["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| n.to_string());
        let name = case["tool"].as_str().expect("tool").to_string();
        // `args_raw` sends a literal argument string (to probe malformed JSON).
        let arguments = match case.get("args_raw").and_then(Value::as_str) {
            Some(raw) => raw.to_string(),
            None => case["args"].to_string(),
        };
        let cwd = case["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_cwd.clone());
        let tools = tools_for(cwd, &sessions, &attach, &kernel);
        let Some(tool) = tools.iter().find(|t| t.tool_name().to_string() == name) else {
            panic!("line {n}: unknown tool {name}");
        };
        let payload = ToolPayload::Function { arguments };
        let started = Instant::now();
        let result = tool
            .handle(ToolCall {
                turn_id: "probe".to_string(),
                call_id: id.clone(),
                tool_name: tool.tool_name(),
                model: "probe".to_string(),
                codex_turn_metadata: None,
                truncation_policy: TruncationPolicy::Bytes(1 << 20),
                source: ToolCallSource::Direct,
                conversation_history: ConversationHistory::default(),
                turn_item_emitter: Arc::new(NoopTurnItemEmitter),
                environments: Vec::new(),
                payload: payload.clone(),
            })
            .await;
        let (ok, text) = match result {
            Ok(output) => (true, output_text(output.to_response_item(&id, &payload))),
            Err(err) => (false, err.to_string()),
        };
        let record = json!({
            "id": id,
            "tool": name,
            "ok": ok,
            "text": text,
            "ms": started.elapsed().as_millis() as u64,
        });
        writeln!(out, "{record}").expect("write out");
        out.flush().ok();
    }
}
