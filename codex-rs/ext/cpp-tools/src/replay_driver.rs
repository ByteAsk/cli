//! Test-only replay driver: runs engine hooks (session-start snapshot, the forced checklist,
//! the finish gate) and C/C++ tools on a recorded state of a repository, with no model, for
//! offline evaluation of engine changes. Not compiled into the engine (`#[cfg(test)]` only).
//!
//! ```text
//! BYTEASK_REPLAY=cases.jsonl BYTEASK_REPLAY_OUT=out.jsonl BYTEASK_REPLAY_CWD=/workspace \
//!   <test-binary> replay_driver::replay_driver --ignored --exact
//! ```
//!
//! Each input line is `{"id": "...", "hook": "...", ...}`; lines run in order on one thread
//! state, like one session:
//! - `{"hook": "snapshot", "settings": {...}}` starts the workflow (session-start snapshot) for
//!   the workspace; `settings` may set `gate`, `impact`, `tests`, `critic`, `budget_secs`,
//!   `max_loops`, `session_cap_secs`, `native_exec`;
//! - `{"hook": "apply", "diff": "/in/patch.diff"}` applies a patch to the snapshot's repository
//!   (`git apply`), as the agent's edits;
//! - `{"hook": "checklist"}` is the step boundary after an edit (the automatic checklist);
//! - `{"hook": "gate", "assistant_text": "..."}` is the model ending its turn: the finish gate,
//!   with its feedback text and the gate's own record (problems, notes);
//! - `{"hook": "tool", "tool": "cpp_impact", "args": {...}}` calls a C/C++ tool by name;
//! - `{"hook": "jobs"}` reports the memory-aware job count and the memory reading;
//! - `{"hook": "status"}` is `git status --porcelain` of the repository (side-effect check).
//!
//! Output lines: `{"id", "hook", "ok", "ms", "text", "data"}`. New engine hooks (failure
//! packets, reviewers) are added here as further `hook` values.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use codex_extension_api::ConversationHistory;
use codex_extension_api::ConversationHistorySnapshot;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolPayload;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use serde_json::Value;
use serde_json::json;

use crate::gate::GateSettings;
use crate::gdbmi::GdbRegistry;
use crate::tools::AttachState;
use crate::workflow::Workflow;

const TURN: &str = "replay";

struct History(Vec<ResponseItem>);

impl ConversationHistorySnapshot for History {
    fn history_version(&self) -> u64 {
        0
    }

    fn user_message_revision(&self) -> u64 {
        0
    }

    fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
        Box::new(self.0.iter())
    }
}

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![if role == "assistant" {
            ContentItem::OutputText {
                text: text.to_string(),
            }
        } else {
            ContentItem::InputText {
                text: text.to_string(),
            }
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn settings_from(value: &Value) -> GateSettings {
    let key_for = |key: &str| -> String {
        match key {
            "finish_gate" => "gate",
            "gate_budget_secs" => "budget_secs",
            "gate_max_loops" => "max_loops",
            "gate_session_cap_secs" => "session_cap_secs",
            other => other,
        }
        .to_string()
    };
    GateSettings::resolve(
        |key| value.get(key_for(key)).and_then(Value::as_bool),
        |key| value.get(key_for(key)).and_then(Value::as_i64),
        |_| None,
    )
}

/// The last record the gate wrote for this session (problems and notes, unrendered).
fn last_gate_record(scratch: &std::path::Path) -> Value {
    std::fs::read_to_string(scratch.join("gate-runs.jsonl"))
        .ok()
        .and_then(|text| text.lines().last().map(str::to_string))
        .and_then(|line| serde_json::from_str(&line).ok())
        .unwrap_or(Value::Null)
}

async fn git_text(root: &std::path::Path, args: &[&str]) -> (bool, String) {
    match tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
    {
        Ok(out) => (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        ),
        Err(err) => (false, err.to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual replay driver; set BYTEASK_REPLAY"]
async fn replay_driver() {
    let Ok(cases) = std::env::var("BYTEASK_REPLAY") else {
        return;
    };
    let cwd = std::env::var("BYTEASK_REPLAY_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("cwd"));
    let out_path = std::env::var("BYTEASK_REPLAY_OUT").expect("BYTEASK_REPLAY_OUT");
    replay(
        std::path::Path::new(&cases),
        std::path::Path::new(&out_path),
        &cwd,
    )
    .await;
}

/// Run the hooks in `cases` (JSONL) for the workspace at `cwd`; write one JSON line per hook
/// to `out_path`.
async fn replay(cases: &std::path::Path, out_path: &std::path::Path, cwd: &std::path::Path) {
    let cwd = cwd.to_path_buf();
    let mut out = std::fs::File::create(out_path).expect("create out");
    let sessions = GdbRegistry::new();
    let attach = AttachState::new();
    let kernel = crate::gdbmi::remote::KernelRegistry::new();
    let mut workflow: Option<Arc<Workflow>> = None;
    // The session's conversation as far as the hooks see it.
    let mut history: Vec<ResponseItem> = Vec::new();

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
        let hook = case["hook"].as_str().unwrap_or("tool").to_string();
        let started = Instant::now();
        let (ok, text, data): (bool, String, Value) = match hook.as_str() {
            "snapshot" => {
                let settings_value = case.get("settings").cloned().unwrap_or(Value::Null);
                let settings = settings_from(&settings_value);
                let native_exec = settings_value
                    .get("native_exec")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let started_workflow =
                    Workflow::start(settings.clone(), native_exec, true, cwd.clone());
                let snapshot = started_workflow.snapshot().await;
                let data = match &snapshot {
                    Some(s) => json!({
                        "root": s.root.display().to_string(),
                        "head": s.head,
                        "dirty": s.dirty.len(),
                        "untracked": s.untracked.len(),
                        "build_trees": s.build_trees.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                        "scratch": s.scratch.display().to_string(),
                        "settings": format!("{settings:?}"),
                    }),
                    None => Value::Null,
                };
                workflow = Some(started_workflow);
                (snapshot.is_some(), String::new(), data)
            }
            "apply" => {
                let diff = case["diff"].as_str().unwrap_or_default().to_string();
                let root = match &workflow {
                    Some(w) => w.snapshot().await.map(|s| s.root.clone()),
                    None => crate::gitx::primary_root(&cwd).await,
                }
                .unwrap_or_else(|| cwd.clone());
                if std::fs::metadata(&diff).map(|m| m.len()).unwrap_or(0) == 0 {
                    (true, "empty diff: nothing applied".to_string(), Value::Null)
                } else {
                    let (ok, text) =
                        git_text(&root, &["apply", "--binary", "--whitespace=nowarn", &diff]).await;
                    if let Some(w) = &workflow {
                        w.note_tool_call();
                    }
                    (ok, text, json!({"root": root.display().to_string()}))
                }
            }
            "checklist" => match &workflow {
                Some(w) => {
                    w.note_tool_call();
                    let fragments = w.after_step(TURN).await;
                    let text: String = fragments
                        .iter()
                        .map(|f| f.body())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        history.push(message("developer", &text));
                    }
                    (true, text, Value::Null)
                }
                None => (false, "no snapshot hook ran first".to_string(), Value::Null),
            },
            "gate" => match &workflow {
                Some(w) => {
                    if let Some(answer) = case.get("assistant_text").and_then(Value::as_str) {
                        history.push(message("assistant", answer));
                    }
                    let snapshot_history: Arc<dyn ConversationHistorySnapshot> =
                        Arc::new(History(history.clone()));
                    let fragments = w.before_stop(TURN, snapshot_history).await;
                    let text: String = fragments
                        .iter()
                        .map(|f| f.body())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        history.push(message("developer", &text));
                    }
                    let record = match w.snapshot().await {
                        Some(s) => last_gate_record(&s.scratch),
                        None => Value::Null,
                    };
                    (
                        true,
                        text,
                        json!({"feedback": !fragments.is_empty(), "record": record}),
                    )
                }
                None => (false, "no snapshot hook ran first".to_string(), Value::Null),
            },
            "jobs" => {
                let available = crate::resources::available_memory();
                (
                    true,
                    String::new(),
                    json!({
                        "jobs": crate::resources::jobs(),
                        "cpus": crate::resources::cpus(),
                        "available_bytes": available.as_ref().map(|a| a.bytes),
                        "source": available.map(|a| a.source),
                    }),
                )
            }
            "status" => {
                let root = match &workflow {
                    Some(w) => w.snapshot().await.map(|s| s.root.clone()),
                    None => crate::gitx::primary_root(&cwd).await,
                }
                .unwrap_or_else(|| cwd.clone());
                let (ok, text) =
                    git_text(&root, &["status", "--porcelain", "--untracked-files=all"]).await;
                (ok, text, Value::Null)
            }
            "tool" => {
                let name = case["tool"].as_str().expect("tool").to_string();
                let arguments = case["args"].to_string();
                let tool_cwd = case["cwd"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| cwd.clone());
                let tools = crate::probe_driver::tools_for(tool_cwd, &sessions, &attach, &kernel);
                match tools.iter().find(|t| t.tool_name().to_string() == name) {
                    None => (false, format!("unknown tool {name}"), Value::Null),
                    Some(tool) => {
                        let payload = ToolPayload::Function { arguments };
                        let result = tool
                            .handle(ToolCall {
                                turn_id: TURN.to_string(),
                                call_id: id.clone(),
                                tool_name: tool.tool_name(),
                                model: "replay".to_string(),
                                codex_turn_metadata: None,
                                truncation_policy: TruncationPolicy::Bytes(1 << 20),
                                source: ToolCallSource::Direct,
                                conversation_history: ConversationHistory::default(),
                                turn_item_emitter: Arc::new(NoopTurnItemEmitter),
                                environments: Vec::new(),
                                payload: payload.clone(),
                            })
                            .await;
                        match result {
                            Ok(output) => (
                                true,
                                crate::probe_driver::output_text(
                                    output.to_response_item(&id, &payload),
                                ),
                                Value::Null,
                            ),
                            Err(err) => (false, err.to_string(), Value::Null),
                        }
                    }
                }
            }
            other => (false, format!("unknown hook {other}"), Value::Null),
        };
        let record = json!({
            "id": id,
            "hook": hook,
            "ok": ok,
            "ms": started.elapsed().as_millis() as u64,
            "text": text,
            "data": data,
        });
        writeln!(out, "{record}").expect("write out");
        out.flush().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_settings_map_to_gate_settings() {
        let settings = settings_from(
            &json!({"gate": true, "impact": false, "budget_secs": 120, "max_loops": 1}),
        );
        assert!(settings.gate && !settings.impact);
        assert_eq!(settings.budget, std::time::Duration::from_secs(120));
        assert_eq!(settings.max_loops, 1);
        assert_eq!(settings_from(&Value::Null), GateSettings::default());
    }

    /// The driver end to end on a fixture: snapshot, the agent's patch, checklist, gate.
    #[tokio::test]
    async fn the_driver_replays_a_session() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            eprintln!("skipped: git not available");
            return;
        };
        let work = tempfile::tempdir().expect("tempdir");
        // The patch: the fixture's edits as a diff, then the tree back at its base.
        crate::impact::tests::apply_edits(dir.path()).expect("edits");
        let (_, diff) = git_text(dir.path(), &["diff"]).await;
        let (reset, _) = git_text(dir.path(), &["checkout", "--", "."]).await;
        assert!(reset);
        let patch = work.path().join("agent.diff");
        std::fs::write(&patch, diff).expect("write patch");
        let cases = work.path().join("cases.jsonl");
        std::fs::write(
            &cases,
            format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n",
                json!({"id": "s", "hook": "snapshot", "settings": {"gate": true, "impact": true, "tests": false}}),
                json!({"id": "a", "hook": "apply", "diff": patch.display().to_string()}),
                json!({"id": "c", "hook": "checklist"}),
                json!({"id": "g", "hook": "gate", "assistant_text": "[1] edited"}),
                json!({"id": "j", "hook": "jobs"}),
                json!({"id": "t", "hook": "tool", "tool": "cpp_impact", "args": {}}),
            ),
        )
        .expect("write cases");
        let out = work.path().join("out.jsonl");
        replay(&cases, &out, dir.path()).await;
        let lines: Vec<Value> = std::fs::read_to_string(&out)
            .expect("out")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(lines.len(), 6, "{lines:#?}");
        assert!(lines.iter().all(|l| l["ok"] == true), "{lines:#?}");
        assert!(
            lines[2]["text"]
                .as_str()
                .unwrap_or("")
                .starts_with(crate::workflow::IMPACT_TITLE)
        );
        assert!(
            lines[3]["text"]
                .as_str()
                .unwrap_or("")
                .contains("GENERATED COPIES OUT OF SYNC")
        );
        assert!(
            lines[3]["data"]["record"]["problems"].is_array(),
            "{:#?}",
            lines[3]
        );
        assert!(lines[4]["data"]["jobs"].as_u64().unwrap_or(0) >= 1);
        assert!(
            lines[5]["text"]
                .as_str()
                .unwrap_or("")
                .contains("cpp_impact")
        );
    }
}
