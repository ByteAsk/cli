//! Shared tool-output wrapper for the reference tools: (1) **transparency** —
//! every result is prefixed with a `📚 <tool> · <target>` banner so the user
//! sees which grounded lookup ran on what — and (2) **observability** — every
//! call is appended to the same `tool-calls.jsonl` the C/C++ tools use.

use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use check_result::CheckResult;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;

const LOG_PREVIEW_CHARS: usize = 600;

struct ToolReport {
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

/// Build a `📚`-bannered, logged tool result. `target` is the query or ref id
/// (may be truncated by the caller; empty is allowed). `result` becomes
/// the last line; the body itself is unchanged.
pub(crate) fn report(
    tool: &str,
    target: &str,
    body: String,
    result: CheckResult,
) -> Box<dyn ToolOutput> {
    log_call(tool, target, &body, &result);
    let banner = banner_line(tool, target);
    Box::new(ToolReport {
        tool: tool.to_string(),
        text: result.append_to(&format!("{banner}\n{body}")),
        passed: result.is_pass(),
    })
}

fn banner_line(tool: &str, target: &str) -> String {
    if target.is_empty() {
        format!("📚 {tool}")
    } else {
        format!("📚 {tool} · {target}")
    }
}

/// A short, single-line label for the banner: collapse whitespace and cap length
/// so a multi-line query renders as one tidy banner segment.
pub(crate) fn short_target(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(56).collect();
    if collapsed.chars().count() > 56 {
        out.push('…');
    }
    out
}

fn log_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("BYTEASK_TOOL_LOG")
        && !explicit.is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("BYTEASK_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".byteask"))
        })?;
    Some(home.join("logs").join("tool-calls.jsonl"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_log(record: serde_json::Value) {
    let Some(path) = log_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut line) = serde_json::to_string(&record) {
        use std::io::Write as _;
        // One write per record: two (`writeln!`) interleave under concurrent
        // tool calls.
        line.push('\n');
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

fn log_call(tool: &str, target: &str, body: &str, result: &CheckResult) {
    let preview: String = body.chars().take(LOG_PREVIEW_CHARS).collect();
    append_log(serde_json::json!({
        "ts": now_secs(),
        "kind": "tool_call",
        "tool": tool,
        "target": target,
        "success": result.is_pass(),
        "verdict": result.verdict.word(),
        "bytes": body.len(),
        "preview": preview,
    }));
}

/// Log a reference tool call that did NOT produce a normal output (error /
/// blocked / aborted). Called from the lifecycle backstop.
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn banner_includes_target_when_present() {
        assert_eq!(
            banner_line("autosar_ref", "A5-2-2"),
            "📚 autosar_ref · A5-2-2"
        );
    }

    #[test]
    fn banner_omits_target_when_empty() {
        assert_eq!(banner_line("ref_request", ""), "📚 ref_request");
    }

    #[test]
    fn short_target_collapses_and_caps() {
        assert_eq!(
            short_target("  is  a\nC-style   cast allowed "),
            "is a C-style cast allowed"
        );
        let long = "x".repeat(80);
        let out = short_target(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 57); // 56 + ellipsis
    }

    #[test]
    fn report_prepends_banner_and_logs() {
        let dir = std::env::temp_dir().join(format!("reflog-{}", std::process::id()));
        let log = dir.join("t.jsonl");
        // SAFETY: single-threaded test; set + restore the env var.
        unsafe { std::env::set_var("BYTEASK_TOOL_LOG", &log) };
        let out = report(
            "autosar_ref",
            "A5-2-2",
            "no C-style casts".to_string(),
            CheckResult::pass("autosar_ref", "Returned page-cited excerpts."),
        );
        let item = out.to_response_item(
            "call-1",
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        let serialized = serde_json::to_string(&item).expect("serializes");
        assert!(
            serialized.contains(
                "📚 autosar_ref · A5-2-2\\nno C-style casts\\n\\nResult: PASS: Returned page-cited excerpts."
            ),
            "banner+body+result missing from: {serialized}"
        );
        assert!(out.success_for_logging());
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(logged.contains("\"tool\":\"autosar_ref\""));
        unsafe { std::env::remove_var("BYTEASK_TOOL_LOG") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
