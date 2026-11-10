//! Shared tool-output wrapper providing (1) **labeling** — every C/C++ tool
//! result is prefixed with a `🔧 <tool> · <target>` banner naming which tool ran
//! on what — and (2) **observability** — every call is appended to a JSONL log
//! for the usage dashboard.
//!
//! All `cpp_*` tools build their success output through [`report`] (or
//! [`report_plain`]) so labeling and logging are guaranteed and centralized,
//! never left to the individual tool author to remember.
//!
//! ⚠️ **The banner is MODEL-facing, not user-facing.** It is prepended to the
//! `function_call_output` the model receives; it is not printed to the user's
//! terminal. Surfacing a tool call to the *user* is the host renderer's job —
//! the TUI does it via `ThreadItem::DynamicToolCall`, and `exec` renders tool
//! calls itself. The banner reaches the user only when the model happens to
//! quote it back, so do not rely on it for user-visible transparency.

use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use check_result::CheckResult;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;

/// Max characters of the body recorded in the JSONL preview field.
const LOG_PREVIEW_CHARS: usize = 600;

/// A labeled, logged tool result. The banner is prepended to the body so both
/// the model and the user-facing harness see exactly which tool produced this.
pub(crate) struct ToolReport {
    tool: String,
    text: String,
    /// Only a PASS counts as success, for the TUI and for telemetry.
    passed: bool,
}

impl ToolOutput for ToolReport {
    fn log_output(&self) -> String {
        format!("[{} output]", self.tool)
    }

    fn success_for_logging(&self) -> bool {
        self.passed
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: self.text.clone(),
                },
            ]),
        }
    }
}

/// Build a transparency-bannered, logged tool result.
///
/// `tool` is the tool name (e.g. `"cpp_check"`), `target` is what it ran on
/// (a path, symbol, or short summary; may be empty), and `body` is the rendered
/// result. The returned output is prefixed with `🔧 <tool> · <target>`, and
/// ends with `result` as its last line: the body itself is unchanged.
/// Taking the result here means no success path can be written without
/// deciding its verdict.
pub(crate) fn report(tool: &str, target: &str, body: String, result: CheckResult) -> Box<dyn ToolOutput> {
    log_call(tool, target, &body, result.is_pass(), result.verdict.word());
    let banner = banner_line(tool, target);
    // Whether this call's programs ran sandboxed, and if not, why.
    let note = crate::sandbox::current_note()
        .map(|note| format!("\n{note}"))
        .unwrap_or_default();
    Box::new(ToolReport {
        tool: tool.to_string(),
        text: result.append_to(&format!("{banner}\n{body}{note}")),
        passed: result.is_pass(),
    })
}

/// [`report`]'s text, logged the same way, for a check the engine runs itself and sends as a message rather than
/// as a tool result (the `finish_review` switch).
pub(crate) fn report_text(tool: &str, target: &str, body: String, result: CheckResult) -> String {
    log_call(tool, target, &body, result.is_pass(), result.verdict.word());
    let banner = banner_line(tool, target);
    let note = crate::sandbox::current_note()
        .map(|note| format!("\n{note}"))
        .unwrap_or_default();
    result.append_to(&format!("{banner}\n{body}{note}"))
}

/// Like [`report`] but for results with no meaningful target (banner is just
/// `🔧 <tool>`).
pub(crate) fn report_plain(tool: &str, body: String, result: CheckResult) -> Box<dyn ToolOutput> {
    report(tool, "", body, result)
}

fn banner_line(tool: &str, target: &str) -> String {
    if target.is_empty() {
        format!("🔧 {tool}")
    } else {
        format!("🔧 {tool} · {target}")
    }
}

/// Resolve the tool-call log path: `$BYTEASK_TOOL_LOG` if set, else
/// `$BYTEASK_HOME/logs/tool-calls.jsonl`, else `~/.byteask/logs/tool-calls.jsonl`.
fn log_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("BYTEASK_TOOL_LOG") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    let home = std::env::var("BYTEASK_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".byteask")))?;
    Some(home.join("logs").join("tool-calls.jsonl"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one JSONL record to the tool-call log. Best-effort: any failure
/// (no home dir, unwritable path, clock error) is swallowed — logging must never
/// break a tool call.
fn append_log(record: serde_json::Value) {
    let Some(path) = log_path() else { return };
    append_to(&path, &record);
}

/// The record and its newline go out in ONE write. `writeln!` issues two,
/// so concurrent tool calls (they run in parallel) interleave them — measured,
/// 3.9% of 1,800 records came back glued together or empty.
fn append_to(path: &std::path::Path, record: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut line) = serde_json::to_string(record) {
        use std::io::Write as _;
        line.push('\n');
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Log a tool call that produced a normal output, with a preview (called from
/// [`report`]). `success` means PASS only.
fn log_call(tool: &str, target: &str, body: &str, success: bool, verdict: &str) {
    let preview: String = body.chars().take(LOG_PREVIEW_CHARS).collect();
    append_log(serde_json::json!({
        "ts": now_secs(),
        "kind": "tool_call",
        "tool": tool,
        "target": target,
        "success": success,
        "verdict": verdict,
        "bytes": body.len(),
        "preview": preview,
    }));
}

/// Log a tool call that did NOT produce a normal output (error / blocked /
/// cancelled). Called from the lifecycle backstop so the dashboard captures
/// every invocation, not just successful ones. `tool` is the (possibly
/// namespaced) tool name; `outcome` is a short label like `failed`/`blocked`.
/// One line per automatic review when a finish-review switch is on: which path ran, whether a message
/// was sent, how long it took, and the agent's test command it counted (own-tests path).
pub(crate) fn log_review(target: &str, sent: bool, ms: u128, command: &str) {
    append_log(serde_json::json!({
        "ts": now_secs(),
        "kind": "review",
        "tool": "cpp_finish_check",
        "target": target,
        "sent": sent,
        "ms": u64::try_from(ms).unwrap_or(u64::MAX),
        "command": command,
    }));
}

/// With `BYTEASK_FINISH_REVIEW_LOG=1` (off unless set): one JSON line per automatic review in
/// finish-review.jsonl beside the tool-call log: the path that ran, whether the message was sent, its duration, the
/// full message the model got and its related-places section.
pub(crate) fn log_review_message(path_kind: &str, sent: bool, ms: u128, message: &str, related: &str) {
    if !std::env::var("BYTEASK_FINISH_REVIEW_LOG")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
    {
        return;
    }
    let Some(tool_log) = log_path() else { return };
    append_to(
        &tool_log.with_file_name("finish-review.jsonl"),
        &serde_json::json!({
            "ts": now_secs(),
            "path": path_kind,
            "sent": sent,
            "ms": u64::try_from(ms).unwrap_or(u64::MAX),
            "message": message,
            "related": related,
        }),
    );
}

pub(crate) fn log_tool_outcome(tool: &str, outcome: &str) {
    append_log(serde_json::json!({
        "ts": now_secs(),
        "kind": "tool_call",
        "tool": tool,
        "target": "",
        "success": false,
        "outcome": outcome,
    }));
}

/// Log one git command a tool ran on its own (not through the shell), with its
/// full argv, for the leak audit: history reads must stay on ancestors of the
/// session-start HEAD, and this record is how an auditor checks that.
pub(crate) fn log_git_call(tool: &str, argv: &[String], ok: bool, ms: u128) {
    append_log(serde_json::json!({
        "ts": now_secs(),
        "kind": "git_call",
        "tool": tool,
        "argv": argv,
        "ok": ok,
        "ms": ms,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_log_records_never_interleave() {
        // With two writes per record, 6 writers x 300 calls lost 3.9% of records to interleaving.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool-calls.jsonl");
        let filler = "x".repeat(600);
        std::thread::scope(|scope| {
            for t in 0..8 {
                let (path, filler) = (&path, &filler);
                scope.spawn(move || {
                    for i in 0..250 {
                        append_to(path, &serde_json::json!({"t": t, "i": i, "pad": filler}));
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2000);
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).expect("every record intact");
        }
    }

    #[test]
    fn only_a_pass_is_success() {
        // No env change here: the banner test owns BYTEASK_TOOL_LOG, and tests
        // run in parallel.
        for (result, success) in [
            (CheckResult::pass("cpp_check", "No findings."), true),
            (CheckResult::no_claim("cpp_perf", "Inside the noise."), false),
            (CheckResult::approximate("cpp_layout", "Partial."), false),
            (
                CheckResult::not_run(
                    "cpp_check",
                    check_result::NotRunReason::TimeLimit,
                    "Stopped.",
                    "narrow it.",
                ),
                false,
            ),
        ] {
            let out = report("cpp_check", "", "body".to_string(), result);
            assert_eq!(out.success_for_logging(), success);
        }
    }

    #[test]
    fn banner_includes_target_when_present() {
        assert_eq!(banner_line("cpp_check", "a.cpp"), "🔧 cpp_check · a.cpp");
    }

    #[test]
    fn banner_omits_target_when_empty() {
        assert_eq!(banner_line("cpp_perf", ""), "🔧 cpp_perf");
    }

    #[test]
    fn report_prepends_banner_to_body() {
        // Isolate logging to a temp file so the test never touches a real home.
        let dir = std::env::temp_dir().join(format!("baxlog-{}", std::process::id()));
        let log = dir.join("t.jsonl");
        // SAFETY: single-threaded test; set + restore the env var.
        unsafe { std::env::set_var("BYTEASK_TOOL_LOG", &log) };
        let out = report(
            "cpp_check",
            "a.cpp",
            "2 findings".to_string(),
            CheckResult::fail("cpp_check", "2 findings."),
        );
        let item = out.to_response_item("call-1", &ToolPayload::Function { arguments: "{}".to_string() });
        // Serialize the response item and assert the banner + body landed in it,
        // unchanged, with the shared result as the last line.
        let serialized = serde_json::to_string(&item).expect("response item serializes");
        assert!(
            serialized.contains("🔧 cpp_check · a.cpp\\n2 findings\\n\\nResult: FAIL: 2 findings."),
            "banner+body+result missing from: {serialized}"
        );
        assert!(!out.success_for_logging(), "a FAIL is never shown as success");
        // A JSONL line was written.
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(logged.contains("\"tool\":\"cpp_check\""));
        assert!(logged.contains("\"target\":\"a.cpp\""));
        assert!(logged.contains("\"verdict\":\"FAIL\""), "{logged}");
        unsafe { std::env::remove_var("BYTEASK_TOOL_LOG") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
