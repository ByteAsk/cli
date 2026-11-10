//! A context-engine answer as the model reads it.
//!
//! ```text
//! 🔎 context_symbol · push
//! Index: build setting relwithdebinfo, commit 59a6a938513e, committed layer, 0 commits behind
//! Show the compile line: context_compile_line {"path":"src/feed.cpp","setting":"relwithdebinfo"}
//! {...the answer as JSON...}
//!
//! Result: PASS: answered from completed index snapshots
//! ```
//!
//! Every answer leads with the build setting and commit it comes from (one
//! line per setting), and with the call that shows the compile line behind it.
//! The last line is the shared result: PASS, APPROXIMATE (some files
//! could only be parsed approximately), NO CLAIM (nothing matched), or NOT RUN
//! with its reason and a one-line fix.

use check_result::CheckResult;
use check_result::NotRunReason;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;

use crate::backend::Outcome;
use crate::client::ClientError;

/// The fix for an index that cannot answer.
pub const FIX_INDEX: &str =
    "run `byteask company trial` in this project to build or update its index, then ask again";
/// The fix for a context engine that is missing, broken or from another release.
pub const FIX_INSTALL: &str =
    "run `byteask company trial` in this project again; it reinstalls the context engine";

struct Answer {
    tool: String,
    text: String,
    passed: bool,
}

impl ToolOutput for Answer {
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

/// The tool's answer for this outcome.
pub fn output(tool: &str, raw_args: &str, outcome: Outcome) -> Box<dyn ToolOutput> {
    let (text, result) = text_and_result(tool, raw_args, outcome);
    Box::new(Answer {
        tool: tool.to_string(),
        passed: result.is_pass(),
        text,
    })
}

/// The full text (ending with the result line) and the result itself.
pub fn text_and_result(tool: &str, raw_args: &str, outcome: Outcome) -> (String, CheckResult) {
    let banner = banner(tool, raw_args);
    let (body, result) = body_and_result(tool, outcome);
    let text = if body.is_empty() {
        result.append_to(&banner)
    } else {
        result.append_to(&format!("{banner}\n{body}"))
    };
    (text, result)
}

fn banner(tool: &str, raw_args: &str) -> String {
    let target = serde_json::from_str::<serde_json::Value>(raw_args)
        .ok()
        .and_then(|args| {
            ["name", "query", "path", "task", "code"]
                .iter()
                .find_map(|key| {
                    args.get(*key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
        })
        .unwrap_or_default();
    let collapsed = target.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = collapsed.chars().take(56).collect();
    if collapsed.chars().count() > 56 {
        short.push('…');
    }
    if short.is_empty() {
        format!("🔎 {tool}")
    } else {
        format!("🔎 {tool} · {short}")
    }
}

fn body_and_result(tool: &str, outcome: Outcome) -> (String, CheckResult) {
    match outcome {
        Outcome::Answer(result) => answer(tool, result),
        Outcome::BadArguments(problem) => (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::BadArguments,
                &problem,
                &format!("call {tool} again with the arguments its schema lists"),
            ),
        ),
        Outcome::Mismatch(problem) => (
            String::new(),
            CheckResult::not_run(tool, NotRunReason::ToolError, &problem, FIX_INSTALL),
        ),
        Outcome::Failed(ClientError::Start(why)) => (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::ToolMissing,
                &format!("the context engine could not start: {why}"),
                FIX_INSTALL,
            ),
        ),
        Outcome::Failed(ClientError::Timeout(after)) => (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::TimeLimit,
                &format!("the index did not answer within {} s", after.as_secs()),
                "ask a narrower question: pass setting=, a more specific name, or a smaller limit",
            ),
        ),
        Outcome::Failed(ClientError::Broken { what, stderr }) => (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::ToolError,
                &format!("the context engine failed: {what}"),
                FIX_INSTALL,
            )
            .with_evidence(last_lines(&stderr, 5)),
        ),
    }
}

fn last_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// The server answered: a normal answer, or its own error.
fn answer(tool: &str, result: serde_json::Value) -> (String, CheckResult) {
    if let Some(error) = result.get("error") {
        let message = error["message"].as_str().unwrap_or("unknown error");
        return (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::ToolError,
                &format!("the context engine refused the request: {message}"),
                FIX_INSTALL,
            ),
        );
    }
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let structured = result.get("structuredContent").cloned();
    if result["isError"].as_bool().unwrap_or(false) {
        return (
            String::new(),
            error_result(tool, &text, structured.as_ref()),
        );
    }
    let Some(serde_json::Value::Object(mut body)) = structured else {
        return (
            String::new(),
            CheckResult::not_run(
                tool,
                NotRunReason::ToolError,
                "the context engine answered without structured content",
                FIX_INSTALL,
            ),
        );
    };
    let record = body.remove("check_result");
    // A bundle's text is written for the model to read: print it as text, not
    // as one escaped JSON string.
    let readable = match body.get("text") {
        Some(serde_json::Value::String(_)) if tool == "context_bundle" => body
            .remove("text")
            .and_then(|text| text.as_str().map(str::to_string)),
        _ => None,
    };
    let body = serde_json::Value::Object(body);
    let result = match record.and_then(|record| verdict_of(tool, &record)) {
        Some(result) => result,
        None => {
            return (
                String::new(),
                CheckResult::not_run(
                    tool,
                    NotRunReason::ToolError,
                    "the context engine's answer carried no valid result record",
                    FIX_INSTALL,
                ),
            );
        }
    };
    let mut lines = index_lines(&body);
    if let Some(hint) = compile_line_hint(tool, &body) {
        lines.push(hint);
    }
    if let Some(readable) = readable {
        lines.push(readable.trim_end().to_string());
    }
    lines.push(serde_json::to_string(&body).unwrap_or_default());
    (lines.join("\n"), result)
}

/// The context engine's own result record, as a shared result. Only the three
/// answered verdicts are accepted here; a NOT RUN arrives as an error.
fn verdict_of(tool: &str, record: &serde_json::Value) -> Option<CheckResult> {
    let summary = record["summary"]
        .as_str()
        .filter(|s| !s.trim().is_empty())?;
    match record["verdict"].as_str()? {
        "PASS" => Some(CheckResult::pass(tool, summary)),
        "APPROXIMATE" => Some(CheckResult::approximate(tool, summary)),
        "NO_CLAIM" => Some(CheckResult::no_claim(tool, summary)),
        _ => None,
    }
}

/// The server's error answers: index unavailable, access refused, bad arguments.
fn error_result(tool: &str, text: &str, structured: Option<&serde_json::Value>) -> CheckResult {
    let summary = text.lines().next().unwrap_or("the index could not answer");
    if let Some(record) = structured.and_then(|value| value.get("check_result"))
        && record["verdict"].as_str() == Some("NOT_RUN")
    {
        // The engine's own fix names its internal command; ours names the one
        // a ByteAsk user runs.
        let reason = record["reason"]
            .as_str()
            .and_then(|reason| {
                serde_json::from_value::<NotRunReason>(serde_json::json!(reason)).ok()
            })
            .unwrap_or(NotRunReason::ServiceUnavailable);
        let fix = if reason == NotRunReason::ServiceUnavailable {
            FIX_INDEX
        } else {
            ""
        };
        return CheckResult::not_run(tool, reason, summary, fix);
    }
    if text.starts_with("bad arguments") {
        return CheckResult::not_run(
            tool,
            NotRunReason::BadArguments,
            summary,
            &format!("call {tool} again with the arguments its schema lists"),
        );
    }
    if text.contains("returning nothing") {
        return CheckResult::not_run(
            tool,
            NotRunReason::NeedsApproval,
            summary,
            "ask the owner of the index to add you as a reader",
        );
    }
    CheckResult::not_run(tool, NotRunReason::ToolError, summary, FIX_INSTALL)
}

/// One line per build setting the answer draws on: setting, commit, layer and
/// how far behind the worktree the index is.
pub fn index_lines(body: &serde_json::Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    collect_index_lines(body, &mut lines);
    let mut seen = std::collections::BTreeSet::new();
    lines.retain(|line| seen.insert(line.clone()));
    lines
}

fn collect_index_lines(body: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(answers) = body.get("answers").and_then(serde_json::Value::as_array) {
        for answer in answers {
            out.push(answer_line(answer));
        }
    } else if let Some(results) = body.get("results").and_then(serde_json::Value::as_array) {
        for result in results {
            collect_index_lines(result, out);
        }
    } else if let (Some(settings), Some(commits)) = (
        body.get("settings").and_then(serde_json::Value::as_array),
        body.get("commits").and_then(serde_json::Value::as_array),
    ) {
        // A context pack draws on several settings at once.
        let words = |items: &Vec<serde_json::Value>, short: bool| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|item| {
                    if short {
                        short_commit(item)
                    } else {
                        item.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push(format!(
            "Index: build setting {}, commit {}",
            words(settings, false),
            words(commits, true)
        ));
    } else if let (Some(setting), Some(commit)) = (
        body.get("setting").and_then(serde_json::Value::as_str),
        body.get("commit").and_then(serde_json::Value::as_str),
    ) {
        // The task-level tools (bundle, locate, find, calls, ...) answer from one setting.
        out.push(format!(
            "Index: build setting {setting}, commit {}",
            short_commit(commit)
        ));
    }
}

fn short_commit(commit: &str) -> String {
    commit.chars().take(12).collect()
}

fn answer_line(answer: &serde_json::Value) -> String {
    let setting = answer["setting"].as_str().unwrap_or("unknown");
    let commit = short_commit(answer["commit"].as_str().unwrap_or("unknown"));
    let layer = match answer["layer"].as_str().unwrap_or("committed") {
        "committed" => "committed layer".to_string(),
        "committed+overlay" => "committed layer + your uncommitted changes".to_string(),
        other => format!("{other} layer"),
    };
    let mut line = format!("Index: build setting {setting}, commit {commit}, {layer}");
    if let Some(behind) = answer["commits_behind"].as_i64() {
        let word = if behind == 1 { "commit" } else { "commits" };
        line.push_str(&format!(", {behind} {word} behind your worktree"));
    }
    if let Some(share) = answer["approximate_line_share"].as_f64()
        && share > 0.0
    {
        line.push_str(&format!(", {:.1}% of lines approximate", share * 100.0));
    }
    line
}

/// "Show the compile line" for the first file the answer names.
pub fn compile_line_hint(tool: &str, body: &serde_json::Value) -> Option<String> {
    if tool == "context_compile_line" {
        return None;
    }
    let first = first_answer(body)?;
    let path = first_path(first)?;
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(path));
    let setting = first["setting"]
        .as_str()
        .or_else(|| first["settings"][0].as_str())
        .or_else(|| body["settings"][0].as_str());
    if let Some(setting) = setting {
        args.insert("setting".to_string(), serde_json::json!(setting));
    }
    Some(format!(
        "Show the compile line: context_compile_line {}",
        serde_json::Value::Object(args)
    ))
}

fn first_answer(body: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(answer) = body["answers"]
        .as_array()
        .and_then(|answers| answers.first())
    {
        return Some(answer);
    }
    if let Some(result) = body["results"]
        .as_array()
        .and_then(|results| results.first())
    {
        return first_answer(result);
    }
    if body.get("items").is_some() {
        return Some(body);
    }
    None
}

fn first_path(answer: &serde_json::Value) -> Option<String> {
    if let Some(path) = answer["path"].as_str() {
        return Some(path.to_string());
    }
    for list in ["definitions", "references", "hits", "items"] {
        if let Some(path) = answer[list][0]["path"].as_str() {
            return Some(path.to_string());
        }
    }
    answer["versions"][0]["instantiated_in"][0]
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
