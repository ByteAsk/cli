use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncRead;
use tokio::io::BufReader;
use tokio::time::Instant;

use crate::lsp::ReadFail;
use crate::lsp::next_message_raw;
use crate::lsp::notification;
use crate::lsp::path_to_uri;
use crate::lsp::read_until_id;
use crate::lsp::request;
use crate::lsp::send_message;
use crate::lsp::uri_to_path;

use crate::finding::Finding;
use crate::finding::Severity;
use crate::finding::render_findings;

const TOOL_NAME: &str = "cpp_nav";
/// Cap on diagnostics handed back to the model.
const FINDINGS_CAP: usize = 50;
/// Overall wall-clock guard around the whole clangd exchange.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);
/// How long we wait for clangd to push the first `publishDiagnostics`.
const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(10);

const ID_INITIALIZE: i64 = 1;
const ID_QUERY: i64 = 2;
const ID_SHUTDOWN: i64 = 3;

pub struct ClangdNavTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Action {
    Diagnostics,
    Hover,
    Definition,
    References,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NavArgs {
    file: String,
    #[serde(default)]
    action: Option<Action>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    column: Option<u32>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for ClangdNavTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "file".to_string(),
            JsonSchema::string(Some(
                "C/C++ source file to analyze, relative to the workspace or absolute.".to_string(),
            )),
        );
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("diagnostics"),
                    json!("hover"),
                    json!("definition"),
                    json!("references"),
                ],
                Some(
                    "Navigation action (default: diagnostics). hover/definition/references \
                     require line+column."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "line".to_string(),
            JsonSchema::integer(Some(
                "1-based line of the symbol (required for hover/definition/references)."
                    .to_string(),
            )),
        );
        properties.insert(
            "column".to_string(),
            JsonSchema::integer(Some(
                "1-based column of the symbol (required for hover/definition/references)."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Compiler-accurate C/C++ code navigation via clangd (LSP): diagnostics, \
                hover (types/signatures), go-to-definition, find-references — resolves \
                overloads/templates/macros that grep can't. Best with a compile_commands.json \
                (run cpp_compile_db first). Use for understanding/navigating real C/C++ code. \
                C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["file".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl ClangdNavTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: NavArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let action = args.action.unwrap_or(Action::Diagnostics);

        // Position-based actions need a 1-based line and column.
        let position = position_for_action(action, args.line, args.column)?;

        // Resolve and canonicalize so the file:// URIs match clangd's own.
        let file_path = resolve_path(&self.cwd, &args.file);
        let file_abs = std::fs::canonicalize(&file_path).unwrap_or(file_path);
        // A regular file only: reading a FIFO blocked here, outside the
        // OVERALL_TIMEOUT that covers the clangd exchange.
        if !file_abs.is_file() {
            let what = if file_abs.exists() {
                "not a regular file"
            } else {
                "no such file"
            };
            return Err(FunctionCallError::RespondToModel(format!(
                "{what}: {} (pass a C/C++ source file). Nothing was analyzed.",
                file_abs.display()
            )));
        }
        let text = std::fs::read_to_string(&file_abs).map_err(|err| {
            FunctionCallError::RespondToModel(format!("cannot read {}: {err}", file_abs.display()))
        })?;
        let root_abs = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());

        let file_uri = path_to_uri(&file_abs);
        let root_uri = path_to_uri(&root_abs);

        // One-shot session: spawn clangd, handshake, query, exit.
        //
        // This is an interactive LSP exchange, so it cannot use
        // `run_capped` (which spawns, captures to EOF and waits) — it has its
        // own `OVERALL_TIMEOUT` deadline instead. What it was missing is the
        // other half of run_capped's contract: its own process group, so the
        // teardown below kills clangd's index/build children too rather than
        // just clangd.
        let mut argv = vec![
            "--log=error".to_string(),
            // Nothing written into the user's project or the temp dir: the
            // background indexer wrote `.cache/clangd/index/` into the
            // workspace, and every call left its preamble PCH (3-10 MB) in
            // TMPDIR because the group is killed right after `exit`.
            "--background-index=false".to_string(),
            "--pch-storage=memory".to_string(),
        ];
        // clangd looks for compile_commands.json only in the file's
        // parent directories (and their `build/`), not in `out/` where
        // cpp_compile_db / cpp_tidy / cpp_check find it — on its own it would analyze
        // with fallback flags and report false errors. Point it at the same DB.
        if let Some(db) = crate::tools::compile_db::find_compile_db(&self.cwd)
            && let Some(dir) = db.parent()
        {
            argv.push(format!("--compile-commands-dir={}", dir.display()));
        }
        // Inside this tool's sandbox profile.
        let wrapped = crate::sandbox::wrap("clangd", &argv, &self.cwd)?;
        let mut command = tokio::process::Command::new(&wrapped.program);
        command
            .args(&wrapped.args)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        // Never `Fatal`: that ends the user's whole turn.
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                FunctionCallError::RespondToModel("clangd not installed".to_string())
            } else {
                FunctionCallError::RespondToModel(format!(
                    "could not start clangd ({err}); nothing was analyzed."
                ))
            }
        })?;
        let clangd_pid = child.id();

        let mut stdin = child.stdin.take().ok_or_else(|| {
            FunctionCallError::RespondToModel("clangd stdin unavailable".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            FunctionCallError::RespondToModel("clangd stdout unavailable".to_string())
        })?;
        let mut reader = BufReader::new(stdout);

        let deadline = Instant::now() + OVERALL_TIMEOUT;

        // Run the exchange; on any error still tear the child down below.
        let result = run_exchange(
            &mut stdin,
            &mut reader,
            deadline,
            action,
            position,
            &root_uri,
            &file_uri,
            &file_abs,
            &text,
            &args.file,
        )
        .await;

        // Best-effort graceful shutdown, then make sure the whole group is gone.
        let _ = send_message(&mut stdin, &request(ID_SHUTDOWN, "shutdown", json!(null))).await;
        let _ = send_message(&mut stdin, &notification("exit", json!(null))).await;
        drop(stdin);
        crate::run::kill_process_group(clangd_pid);
        let _ = child.kill().await;

        result
    }
}

/// Drive the full initialize → didOpen → query handshake.
#[allow(clippy::too_many_arguments)]
async fn run_exchange<R>(
    stdin: &mut tokio::process::ChildStdin,
    reader: &mut R,
    deadline: Instant,
    action: Action,
    position: Option<(u32, u32)>,
    root_uri: &str,
    file_uri: &str,
    file_abs: &Path,
    text: &str,
    file_label: &str,
) -> Result<Box<dyn ToolOutput>, FunctionCallError>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    // 1. initialize (id 1) → wait for its response.
    let init = request(
        ID_INITIALIZE,
        "initialize",
        json!({
            "processId": null,
            "rootUri": root_uri,
            // Positions in UTF-8 bytes — the unit cpp_tidy, cpp_check and
            // compilers report, and what this tool's `column` promises.
            // clangd's default is UTF-16 code units, so on a line with a
            // non-ASCII character the columns disagreed with every other
            // tool. `offsetEncoding` is clangd's extension (clangd 14 honours
            // it); `positionEncodings` is the LSP 3.17 spelling.
            "capabilities": {
                "offsetEncoding": ["utf-8"],
                "general": { "positionEncodings": ["utf-8"] },
            },
        }),
    );
    send_message(stdin, &init).await?;
    read_until_id(reader, deadline, ID_INITIALIZE).await?;

    // 2. initialized notification.
    send_message(stdin, &notification("initialized", json!({}))).await?;

    // 3. didOpen the document.
    let did_open = notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": file_uri,
                "languageId": "cpp",
                "version": 1,
                "text": text,
            }
        }),
    );
    send_message(stdin, &did_open).await?;

    match action {
        Action::Diagnostics => {
            let diag_deadline = (Instant::now() + DIAGNOSTICS_TIMEOUT).min(deadline);
            let (body, result) = match collect_diagnostics(
                reader,
                diag_deadline,
                file_uri,
                file_abs,
            )
            .await?
            {
                Some(findings) => (
                    render_findings(&findings, FINDINGS_CAP),
                    crate::results::findings_result(TOOL_NAME, &findings, false),
                ),
                None => (
                    format!(
                        "clangd sent no diagnostics for this file within {}s — NO ANSWER, not a \
                         clean result (a large translation unit can take longer; retry, or use \
                         cpp_tidy / a compile to check it).",
                        DIAGNOSTICS_TIMEOUT.as_secs()
                    ),
                    crate::results::timed_out(
                        TOOL_NAME,
                        "Waiting for clangd's diagnostics",
                        DIAGNOSTICS_TIMEOUT.as_secs(),
                        "call cpp_nav again, or check the file with cpp_tidy instead.",
                    ),
                ),
            };
            Ok(crate::report::report(TOOL_NAME, file_label, body, result))
        }
        Action::Hover | Action::Definition | Action::References => {
            let (line0, char0) = position.expect("position validated for this action");
            let pos = json!({ "line": line0, "character": char0 });
            let (method, params) = match action {
                Action::Hover => (
                    "textDocument/hover",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": pos,
                    }),
                ),
                Action::Definition => (
                    "textDocument/definition",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": pos,
                    }),
                ),
                Action::References => (
                    "textDocument/references",
                    json!({
                        "textDocument": { "uri": file_uri },
                        "position": pos,
                        "context": { "includeDeclaration": true },
                    }),
                ),
                Action::Diagnostics => unreachable!(),
            };

            send_message(stdin, &request(ID_QUERY, method, params)).await?;
            let response = read_until_id(reader, deadline, ID_QUERY).await?;
            let result = response.get("result").unwrap_or(&JsonValue::Null);
            let nothing = result.is_null() || result.as_array().is_some_and(Vec::is_empty);
            let verdict = match action {
                _ if nothing => check_result::CheckResult::no_claim(
                    TOOL_NAME,
                    "clangd returned nothing for this position in this translation unit.",
                ),
                Action::References => check_result::CheckResult::approximate(
                    TOOL_NAME,
                    "References in this translation unit only; other files were not searched.",
                ),
                _ => check_result::CheckResult::pass(TOOL_NAME, "clangd answered the query."),
            };
            let text = match action {
                Action::Hover => format_hover(result),
                // One clangd, one opened file, no index — so this is not
                // the whole project, and must not read as if it were.
                Action::References => format!(
                    "{}\n(references in this translation unit only — clangd ran without a project \
                     index, so uses in other source files are NOT listed; grep for the rest)",
                    format_locations(result)
                ),
                _ => format!(
                    "{}\n(resolved from this translation unit only: a definition in a file it does \
                     not include cannot be found)",
                    format_locations(result)
                ),
            };
            Ok(crate::report::report(TOOL_NAME, file_label, text, verdict))
        }
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Convert a 1-based (line, column) into LSP's 0-based (line, character).
fn to_lsp_position(line: u32, column: u32) -> (u32, u32) {
    (line.saturating_sub(1), column.saturating_sub(1))
}

/// Validate and derive the LSP query position for an `action`. Position-based
/// actions (hover/definition/references) require a 1-based `line` and `column`;
/// `diagnostics` needs neither. Returns the 0-based LSP position, or `None` for
/// diagnostics.
fn position_for_action(
    action: Action,
    line: Option<u32>,
    column: Option<u32>,
) -> Result<Option<(u32, u32)>, FunctionCallError> {
    match action {
        Action::Diagnostics => Ok(None),
        Action::Hover | Action::Definition | Action::References => {
            let line = line.unwrap_or(0);
            let column = column.unwrap_or(0);
            if line == 0 || column == 0 {
                return Err(FunctionCallError::RespondToModel(
                    "hover/definition/references require 1-based `line` and `column`.".to_string(),
                ));
            }
            Ok(Some(to_lsp_position(line, column)))
        }
    }
}

/// Whether an incoming `publishDiagnostics` uri refers to the file we opened.
fn uri_matches(diag_uri: &str, file_uri: &str, file_abs: &Path) -> bool {
    diag_uri == file_uri || uri_to_path(diag_uri) == file_abs.to_string_lossy()
}

/// Collect the first `publishDiagnostics` notification matching `file_uri`;
/// `None` when none arrived before `deadline`.
///
/// A timeout is never returned as an empty list: that would render as "No
/// findings." — a false clean whenever clangd is slow, or never matches the
/// file (an unencoded URI with a space or non-ASCII character).
async fn collect_diagnostics<R>(
    reader: &mut R,
    deadline: Instant,
    file_uri: &str,
    file_abs: &Path,
) -> Result<Option<Vec<Finding>>, FunctionCallError>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    loop {
        let msg = match next_message_raw(reader, deadline).await {
            Ok(msg) => msg,
            Err(ReadFail::TimedOut) => return Ok(None),
            Err(fail) => return Err(fail.into_error()),
        };

        if msg.get("method").and_then(JsonValue::as_str) == Some("textDocument/publishDiagnostics")
        {
            let params = msg.get("params").unwrap_or(&JsonValue::Null);
            let uri = params.get("uri").and_then(JsonValue::as_str).unwrap_or("");
            if uri_matches(uri, file_uri, file_abs) {
                return Ok(Some(parse_diagnostics(params, file_abs)));
            }
        }
    }
}

fn parse_diagnostics(params: &JsonValue, file_abs: &Path) -> Vec<Finding> {
    let file = file_abs.to_string_lossy().to_string();
    let Some(diagnostics) = params.get("diagnostics").and_then(JsonValue::as_array) else {
        return Vec::new();
    };

    diagnostics
        .iter()
        .map(|diag| {
            let start = diag.pointer("/range/start");
            let line = start
                .and_then(|s| s.get("line"))
                .and_then(JsonValue::as_u64)
                .map(|l| l as u32 + 1)
                .unwrap_or(1);
            let column = start
                .and_then(|s| s.get("character"))
                .and_then(JsonValue::as_u64)
                .map(|c| c as u32 + 1)
                .unwrap_or(1);
            let severity = severity_from_lsp(diag.get("severity").and_then(JsonValue::as_u64));
            let source = diag
                .get("source")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("clangd")
                .to_string();
            let message = diag
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let rule = diag.get("code").and_then(|code| match code {
                JsonValue::String(s) if !s.is_empty() => Some(s.clone()),
                JsonValue::Number(n) => Some(n.to_string()),
                _ => None,
            });

            Finding {
                source,
                severity,
                file: file.clone(),
                line,
                column,
                message,
                rule,
            }
        })
        .collect()
}

/// LSP severity: 1=error, 2=warning, 3=information, 4=hint.
fn severity_from_lsp(severity: Option<u64>) -> Severity {
    match severity {
        Some(1) => Severity::Error,
        Some(2) => Severity::Warning,
        _ => Severity::Note,
    }
}

/// Format a hover `result` into readable text. clangd returns a `MarkupContent`
/// `{ kind, value }`; older shapes (string, `MarkedString[]`) are handled too.
fn format_hover(result: &JsonValue) -> String {
    if result.is_null() {
        return "No hover information.".to_string();
    }
    let Some(contents) = result.get("contents") else {
        return "No hover information.".to_string();
    };

    let mut parts = Vec::new();
    collect_marked_strings(contents, &mut parts);
    if parts.is_empty() {
        "No hover information.".to_string()
    } else {
        parts.join("\n")
    }
}

fn collect_marked_strings(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::String(s) => {
            if !s.trim().is_empty() {
                out.push(s.clone());
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                collect_marked_strings(item, out);
            }
        }
        JsonValue::Object(map) => {
            if let Some(JsonValue::String(value)) = map.get("value") {
                if !value.trim().is_empty() {
                    out.push(value.clone());
                }
            }
        }
        _ => {}
    }
}

/// Format a definition/references `result` (`Location`, `Location[]`, or
/// `LocationLink[]`) into one `file:line:col` per line.
fn format_locations(result: &JsonValue) -> String {
    let mut out = Vec::new();
    collect_locations(result, &mut out);
    if out.is_empty() {
        "No results.".to_string()
    } else {
        out.join("\n")
    }
}

fn collect_locations(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::Array(items) => {
            for item in items {
                collect_locations(item, out);
            }
        }
        JsonValue::Object(map) => {
            let uri = map
                .get("uri")
                .or_else(|| map.get("targetUri"))
                .and_then(JsonValue::as_str);
            let range = map
                .get("range")
                .or_else(|| map.get("targetSelectionRange"))
                .or_else(|| map.get("targetRange"));
            if let (Some(uri), Some(range)) = (uri, range) {
                let line = range
                    .pointer("/start/line")
                    .and_then(JsonValue::as_u64)
                    .map(|l| l + 1)
                    .unwrap_or(1);
                let col = range
                    .pointer("/start/character")
                    .and_then(JsonValue::as_u64)
                    .map(|c| c + 1)
                    .unwrap_or(1);
                out.push(format!("{}:{line}:{col}", uri_to_path(uri)));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::lsp::encode_message;
    use crate::lsp::next_message;
    use crate::lsp::parse_content_length;
    use crate::lsp::read_message;

    #[test]
    fn encodes_with_content_length_framing() {
        let payload = json!({ "jsonrpc": "2.0", "id": 1, "method": "x" });
        let body = serde_json::to_vec(&payload).unwrap();
        let encoded = encode_message(&payload);

        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(encoded.starts_with(header.as_bytes()));
        assert_eq!(&encoded[header.len()..], body.as_slice());
        // Header length must be the UTF-8 byte length of the body.
        assert_eq!(encoded.len(), header.len() + body.len());
    }

    #[test]
    fn parses_content_length_header() {
        assert_eq!(parse_content_length("Content-Length: 42"), Some(42));
        assert_eq!(parse_content_length("content-length:7"), Some(7));
        assert_eq!(parse_content_length("Content-Type: application/json"), None);
        assert_eq!(parse_content_length("garbage"), None);
    }

    #[tokio::test]
    async fn round_trips_a_framed_message() {
        let payload = json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } });
        let bytes = encode_message(&payload);
        let mut reader = BufReader::new(&bytes[..]);
        let decoded = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn reads_two_concatenated_messages() {
        let first = json!({ "id": 1, "result": 1 });
        let second = json!({ "method": "note" });
        let mut bytes = encode_message(&first);
        bytes.extend_from_slice(&encode_message(&second));

        let mut reader = BufReader::new(&bytes[..]);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), first);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), second);
        // Third read hits EOF.
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[test]
    fn converts_one_based_to_zero_based_position() {
        assert_eq!(to_lsp_position(1, 1), (0, 0));
        assert_eq!(to_lsp_position(10, 5), (9, 4));
        // Defensive: never underflow below zero.
        assert_eq!(to_lsp_position(0, 0), (0, 0));
    }

    #[test]
    fn maps_lsp_severity() {
        assert_eq!(severity_from_lsp(Some(1)), Severity::Error);
        assert_eq!(severity_from_lsp(Some(2)), Severity::Warning);
        assert_eq!(severity_from_lsp(Some(3)), Severity::Note);
        assert_eq!(severity_from_lsp(None), Severity::Note);
    }

    #[test]
    fn parses_diagnostics_with_zero_based_to_one_based() {
        let params = json!({
            "uri": "file:///work/main.cpp",
            "diagnostics": [{
                "range": { "start": { "line": 9, "character": 4 } },
                "severity": 1,
                "message": "use of undeclared identifier 'foo'",
                "source": "clang",
                "code": "undeclared_var"
            }]
        });
        let findings = parse_diagnostics(&params, Path::new("/work/main.cpp"));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0],
            Finding {
                source: "clang".to_string(),
                severity: Severity::Error,
                file: "/work/main.cpp".to_string(),
                line: 10,
                column: 5,
                message: "use of undeclared identifier 'foo'".to_string(),
                rule: Some("undeclared_var".to_string()),
            }
        );
    }

    #[test]
    fn diagnostic_source_defaults_to_clangd() {
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "severity": 2,
                "message": "unused variable"
            }]
        });
        let findings = parse_diagnostics(&params, Path::new("/a.cpp"));
        assert_eq!(findings[0].source, "clangd");
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[0].column, 1);
        assert_eq!(findings[0].rule, None);
    }

    #[test]
    fn formats_hover_markup_content() {
        let result = json!({ "contents": { "kind": "markdown", "value": "int foo()" } });
        assert_eq!(format_hover(&result), "int foo()");
        assert_eq!(format_hover(&JsonValue::Null), "No hover information.");
    }

    #[test]
    fn formats_locations_from_array() {
        let result = json!([
            { "uri": "file:///work/a.cpp", "range": { "start": { "line": 4, "character": 2 } } },
            { "targetUri": "file:///work/b.h", "targetRange": { "start": { "line": 0, "character": 0 } } }
        ]);
        assert_eq!(format_locations(&result), "/work/a.cpp:5:3\n/work/b.h:1:1");
        assert_eq!(format_locations(&json!([])), "No results.");
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path(cwd, "src/a.cpp"),
            PathBuf::from("/work/src/a.cpp")
        );
        assert_eq!(resolve_path(cwd, "/abs/b.cpp"), PathBuf::from("/abs/b.cpp"));
    }

    #[test]
    fn matches_uri_by_scheme_or_path() {
        let abs = Path::new("/work/main.cpp");
        assert!(uri_matches(
            "file:///work/main.cpp",
            "file:///work/main.cpp",
            abs
        ));
        assert!(uri_matches("file:///work/main.cpp", "file://other", abs));
        assert!(!uri_matches("file:///work/other.cpp", "file://x", abs));
    }

    // --- Content-Length framing: multibyte byte-length correctness ---

    #[tokio::test]
    async fn content_length_counts_utf8_bytes_not_chars() {
        // Body contains multibyte chars: é (2 bytes), — (3), λ (2), 🔧 (4).
        let payload = json!({ "s": "café—λ🔧" });
        let body = serde_json::to_vec(&payload).unwrap();
        let encoded = encode_message(&payload);

        // The advertised Content-Length must be the UTF-8 *byte* length.
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(encoded.starts_with(header.as_bytes()));
        assert_eq!(encoded.len(), header.len() + body.len());

        // Sanity: the multibyte content genuinely has more bytes than chars,
        // so a char-count framing would have been wrong.
        let s = "café—λ🔧";
        assert!(s.len() > s.chars().count());

        // And the byte-accurate frame round-trips back to the same value.
        let mut reader = BufReader::new(&encoded[..]);
        let decoded = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, payload);
    }

    // --- Frame decode: EOF / partial / malformed ---

    #[tokio::test]
    async fn read_message_returns_none_on_immediate_eof() {
        let empty: &[u8] = b"";
        let mut reader = BufReader::new(empty);
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_message_returns_none_on_eof_mid_headers() {
        // Header line present but the stream ends before the blank separator.
        let bytes = b"Content-Length: 5\r\n";
        let mut reader = BufReader::new(&bytes[..]);
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_message_errors_on_truncated_body() {
        // Header promises 100 bytes but only a few arrive → read_exact fails.
        let mut bytes = b"Content-Length: 100\r\n\r\n".to_vec();
        bytes.extend_from_slice(b"{\"a\":1}");
        let mut reader = BufReader::new(&bytes[..]);
        assert!(read_message(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn read_message_errors_without_content_length() {
        // Headers end (blank line) but no Content-Length was supplied.
        let bytes = b"Content-Type: application/json\r\n\r\n{}";
        let mut reader = BufReader::new(&bytes[..]);
        let err = read_message(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    // --- Position conversion: saturating edges ---

    #[test]
    fn to_lsp_position_saturates_each_axis_independently() {
        assert_eq!(to_lsp_position(0, 5), (0, 4));
        assert_eq!(to_lsp_position(5, 0), (4, 0));
        assert_eq!(to_lsp_position(1, 1), (0, 0));
    }

    // --- LSP severity mapping: hint + unknown fall through to Note ---

    #[test]
    fn maps_lsp_severity_hint_and_unknown_to_note() {
        assert_eq!(severity_from_lsp(Some(4)), Severity::Note);
        assert_eq!(severity_from_lsp(Some(99)), Severity::Note);
    }

    // --- Diagnostic parsing: numeric code, empty code, missing range ---

    #[test]
    fn parses_diagnostic_numeric_code_into_rule() {
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "severity": 1,
                "message": "boom",
                "code": 1234
            }]
        });
        let findings = parse_diagnostics(&params, Path::new("/a.cpp"));
        assert_eq!(findings[0].rule, Some("1234".to_string()));
    }

    #[test]
    fn parses_diagnostic_empty_code_yields_no_rule() {
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "severity": 1,
                "message": "boom",
                "code": ""
            }]
        });
        let findings = parse_diagnostics(&params, Path::new("/a.cpp"));
        assert_eq!(findings[0].rule, None);
    }

    #[test]
    fn parses_diagnostic_missing_range_defaults_to_one_one() {
        let params = json!({
            "diagnostics": [{
                "severity": 2,
                "message": "no range here"
            }]
        });
        let findings = parse_diagnostics(&params, Path::new("/a.cpp"));
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[0].column, 1);
    }

    #[test]
    fn parses_no_diagnostics_array_as_empty() {
        let findings = parse_diagnostics(&json!({}), Path::new("/a.cpp"));
        assert!(findings.is_empty());
    }

    // --- Action validation (extracted, behavior-preserving) ---

    #[test]
    fn diagnostics_action_needs_no_position() {
        // Even with line/column absent, diagnostics yields Ok(None).
        match position_for_action(Action::Diagnostics, None, None) {
            Ok(p) => assert_eq!(p, None),
            _ => panic!("diagnostics should not require a position"),
        }
        // And ignores any provided line/column.
        match position_for_action(Action::Diagnostics, Some(3), Some(4)) {
            Ok(p) => assert_eq!(p, None),
            _ => panic!("diagnostics should ignore position"),
        }
    }

    #[test]
    fn position_actions_accept_line_and_column() {
        for action in [Action::Hover, Action::Definition, Action::References] {
            match position_for_action(action, Some(10), Some(5)) {
                // 1-based 10:5 → 0-based 9:4.
                Ok(Some(pos)) => assert_eq!(pos, (9, 4)),
                _ => panic!("expected Ok(Some(position)) for {action:?}"),
            }
        }
    }

    #[test]
    fn position_actions_reject_missing_column() {
        match position_for_action(Action::Hover, Some(10), None) {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("require 1-based"));
            }
            _ => panic!("missing column should be a RespondToModel error"),
        }
    }

    #[test]
    fn position_actions_reject_missing_line() {
        match position_for_action(Action::Definition, None, Some(5)) {
            Err(FunctionCallError::RespondToModel(_)) => {}
            _ => panic!("missing line should be a RespondToModel error"),
        }
    }

    #[test]
    fn position_actions_reject_zero_values() {
        match position_for_action(Action::References, Some(0), Some(5)) {
            Err(FunctionCallError::RespondToModel(_)) => {}
            _ => panic!("zero line should be rejected"),
        }
        match position_for_action(Action::References, Some(5), Some(0)) {
            Err(FunctionCallError::RespondToModel(_)) => {}
            _ => panic!("zero column should be rejected"),
        }
    }

    // --- Hover result formatting: string, array, empty-object shapes ---

    #[test]
    fn formats_hover_from_plain_string_contents() {
        let result = json!({ "contents": "int x" });
        assert_eq!(format_hover(&result), "int x");
    }

    #[test]
    fn formats_hover_joins_marked_string_array() {
        let result = json!({
            "contents": [
                { "language": "cpp", "value": "int foo()" },
                "Documentation line"
            ]
        });
        assert_eq!(format_hover(&result), "int foo()\nDocumentation line");
    }

    #[test]
    fn formats_hover_empty_contents_is_placeholder() {
        let result = json!({ "contents": { "kind": "markdown", "value": "   " } });
        assert_eq!(format_hover(&result), "No hover information.");
        let no_contents = json!({ "foo": 1 });
        assert_eq!(format_hover(&no_contents), "No hover information.");
    }

    // --- Location formatting: single Location + LocationLink selection range ---

    #[test]
    fn formats_single_location_object() {
        let result = json!({
            "uri": "file:///work/a.cpp",
            "range": { "start": { "line": 0, "character": 0 } }
        });
        assert_eq!(format_locations(&result), "/work/a.cpp:1:1");
    }

    #[test]
    fn formats_location_link_prefers_target_selection_range() {
        // LocationLink carries targetUri + targetSelectionRange (and targetRange).
        let result = json!([{
            "targetUri": "file:///work/b.h",
            "targetSelectionRange": { "start": { "line": 6, "character": 1 } },
            "targetRange": { "start": { "line": 0, "character": 0 } }
        }]);
        assert_eq!(format_locations(&result), "/work/b.h:7:2");
    }

    #[test]
    fn formats_null_location_result_as_no_results() {
        assert_eq!(format_locations(&JsonValue::Null), "No results.");
    }

    // --- URI construction / stripping ---

    #[test]
    fn path_to_uri_prefixes_file_scheme() {
        assert_eq!(path_to_uri(Path::new("/work/a.cpp")), "file:///work/a.cpp");
    }

    #[test]
    fn uri_to_path_strips_file_scheme() {
        assert_eq!(uri_to_path("file:///work/a.cpp"), "/work/a.cpp");
        // A bare path with no scheme passes through unchanged.
        assert_eq!(uri_to_path("/work/a.cpp"), "/work/a.cpp");
    }

    #[test]
    fn uris_are_percent_encoded_and_decoded() {
        // Spaces, non-ASCII, `#`, `%` and `+` in real paths.
        for path in [
            "/w/sp ace/a.cpp",
            "/w/é日本/a.cpp",
            "/w/hash#dir/a.cpp",
            "/w/pct%20inc/a.cpp",
            "/usr/include/c++/12/vector",
        ] {
            let uri = path_to_uri(Path::new(path));
            assert!(
                !uri[7..].contains([' ', '#']) && uri.is_ascii(),
                "{path} -> {uri}"
            );
            assert_eq!(uri_to_path(&uri), path);
        }
        assert_eq!(
            path_to_uri(Path::new("/w/sp ace/a.cpp")),
            "file:///w/sp%20ace/a.cpp"
        );
        assert_eq!(
            uri_to_path("file:///usr/include/c%2B%2B/12/vector"),
            "/usr/include/c++/12/vector"
        );
    }

    #[tokio::test]
    async fn a_clangd_that_exits_is_an_error_not_the_end_of_the_turn() {
        // EOF from clangd must not be `Fatal` — that ends the user's whole turn.
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = next_message(&mut reader, deadline).await.expect_err("EOF");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("must go back to the model: {err:?}");
        };
        assert!(message.contains("stopped responding"), "{message}");
    }

    async fn nav(dir: &Path, args: JsonValue) -> Result<String, FunctionCallError> {
        let tool = ClangdNavTool {
            cwd: dir.to_path_buf(),
        };
        crate::test_support::call(&tool, args).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_instead_of_blocking_forever() {
        // The file is read before the OVERALL_TIMEOUT starts.
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::make_fifo(&dir.path().join("pipe.cpp"));
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            nav(dir.path(), json!({"file": "pipe.cpp"})),
        )
        .await
        .expect("must not block on a FIFO");
        assert!(
            matches!(out, Err(FunctionCallError::RespondToModel(_))),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn diagnostics_come_back_for_a_path_with_spaces_and_non_ascii() {
        // End to end on real clangd: an unencoded URI never matches, and
        // the 10 s wait would come back as "No findings." for a file with an error.
        if !crate::program_args::fixture::have("clangd") {
            eprintln!("skip: clangd not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("my src é");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            sub.join("bad.cpp"),
            "int main() { return undeclared_name; }\n",
        )
        .unwrap();
        let out = nav(dir.path(), json!({"file": "my src é/bad.cpp"}))
            .await
            .expect("a report");
        assert!(out.contains("undeclared_name"), "{out}");
        assert!(
            !out.contains("No findings") && !out.contains("NO ANSWER"),
            "{out}"
        );
        assert!(
            !dir.path().join(".cache").exists(),
            "the background indexer must not write into the project"
        );
    }

    #[tokio::test]
    async fn columns_are_bytes_like_every_other_tool() {
        // clangd's default unit is UTF-16, where after a non-ASCII character
        // cpp_nav's column would disagree with cpp_tidy's and the compiler's.
        if !crate::program_args::fixture::have("clangd") {
            eprintln!("skip: clangd not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let line = "const char *s = \"日本語\"; int y = undeclared_thing;";
        std::fs::write(dir.path().join("u.cpp"), format!("{line}\n")).unwrap();
        let byte_col = line.find("undeclared_thing").unwrap() + 1;
        let out = nav(dir.path(), json!({"file": "u.cpp"}))
            .await
            .expect("a report");
        assert!(
            out.contains(&format!("u.cpp:1:{byte_col}")),
            "want byte column {byte_col}: {out}"
        );
    }

    #[tokio::test]
    async fn references_say_they_cover_one_translation_unit() {
        // A TU-only answer must not read as the complete list.
        if !crate::program_args::fixture::have("clangd") {
            eprintln!("skip: clangd not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("r.cpp"),
            "int v;\nint main() { return v; }\n",
        )
        .unwrap();
        let out = nav(
            dir.path(),
            json!({"file": "r.cpp", "action": "references", "line": 1, "column": 5}),
        )
        .await
        .expect("a report");
        assert!(out.contains("r.cpp:2:"), "{out}");
        assert!(out.contains("translation unit only"), "{out}");
    }

    #[tokio::test]
    async fn clangd_uses_a_compile_db_in_out() {
        // cpp_compile_db / cpp_tidy find a DB in out/; clangd alone does not,
        // and analyzes with fallback flags: a false "unknown type" error.
        if !crate::program_args::fixture::have("clangd") {
            eprintln!("skip: clangd not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("out")).unwrap();
        std::fs::write(
            root.join("a.cpp"),
            "#ifndef FROM_DB\n#error FROM_DB missing\n#endif\nint main() { return 0; }\n",
        )
        .unwrap();
        let db = json!([{
            "directory": root.display().to_string(),
            "file": root.join("a.cpp").display().to_string(),
            "arguments": ["clang++", "-std=c++17", "-DFROM_DB=1", "-c", "a.cpp"],
        }]);
        std::fs::write(root.join("out/compile_commands.json"), db.to_string()).unwrap();
        let out = nav(root, json!({"file": "a.cpp"})).await.expect("a report");
        assert!(!out.contains("FROM_DB missing"), "{out}");
        assert!(out.contains("No findings"), "{out}");
    }
}
