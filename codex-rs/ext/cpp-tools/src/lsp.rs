//! LSP (JSON-RPC over stdio) plumbing shared by `cpp_nav` and `cpp_impact`.
//!
//! The framing helpers were `cpp_nav`'s private functions (moved here unchanged). What is new
//! is [`LspSession`]: one clangd process serving several requests, with incrementing request
//! ids, a per-request deadline and the whole process group torn down on drop. `cpp_nav` keeps
//! its one-shot exchange (ids 1/2/3); `cpp_impact` uses the session to confirm textual
//! candidates.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use codex_extension_api::FunctionCallError;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::time::Instant;
use tokio::time::timeout;

/// `file://` URI for `path`, percent-encoded. Unencoded, a path with a
/// space, `#`, `%` or a non-ASCII character was not the URI clangd uses, so
/// its diagnostics never matched (a 10 s wait reported as "No findings"), and
/// `%20` in a real directory name was decoded by clangd into a false
/// "file not found".
pub(crate) fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.to_string_lossy().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The filesystem path of an LSP uri, percent-decoded (otherwise paths come back as
/// `/usr/include/c%2B%2B/12/...`).
pub(crate) fn uri_to_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri).as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if raw[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                raw.get(i + 1).copied().and_then(hex),
                raw.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn request(id: i64, method: &str, params: JsonValue) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

pub(crate) fn notification(method: &str, params: JsonValue) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Encode a JSON-RPC payload with the LSP `Content-Length` framing.
pub(crate) fn encode_message(payload: &JsonValue) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Parse a `Content-Length` header line, case-insensitively. Returns the body
/// byte length, or `None` for any other header.
pub(crate) fn parse_content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if name.trim().eq_ignore_ascii_case("content-length") {
        value.trim().parse().ok()
    } else {
        None
    }
}

pub(crate) async fn send_message(
    stdin: &mut tokio::process::ChildStdin,
    payload: &JsonValue,
) -> Result<(), FunctionCallError> {
    let bytes = encode_message(payload);
    stdin
        .write_all(&bytes)
        .await
        .map_err(|err| clangd_gone(&format!("write error: {err}")))?;
    stdin
        .flush()
        .await
        .map_err(|err| clangd_gone(&format!("flush error: {err}")))
}

/// Read one framed JSON-RPC message. Returns `Ok(None)` on EOF.
pub(crate) async fn read_message<R>(reader: &mut R) -> std::io::Result<Option<JsonValue>>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(len) = parse_content_length(trimmed) {
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidData, "LSP message missing Content-Length")
    })?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf)
        .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err))?;
    Ok(Some(value))
}

/// Read messages until the response with `want_id` arrives, skipping
/// notifications and server-to-client requests.
pub(crate) async fn read_until_id<R>(
    reader: &mut R,
    deadline: Instant,
    want_id: i64,
) -> Result<JsonValue, FunctionCallError>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    loop {
        let msg = next_message(reader, deadline).await?;
        if msg.get("id") == Some(&json!(want_id))
            && (msg.get("result").is_some() || msg.get("error").is_some())
        {
            return Ok(msg);
        }
    }
}

/// Why no message came back from clangd.
pub(crate) enum ReadFail {
    TimedOut,
    Failed(String),
}

impl ReadFail {
    pub(crate) fn into_error(self) -> FunctionCallError {
        match self {
            ReadFail::TimedOut => FunctionCallError::RespondToModel(
                "clangd timed out without answering; nothing was analyzed.".to_string(),
            ),
            ReadFail::Failed(what) => clangd_gone(&what),
        }
    }
}

/// clangd exited, crashed or broke the protocol. Not `Fatal`: core turns
/// a tool's `Fatal` into the end of the user's whole turn, and a broken clangd
/// is a fact the model can report and work around (cpp_tidy, a compile).
pub(crate) fn clangd_gone(what: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "clangd stopped responding ({what}) before it answered — it exited or crashed; \
         nothing was analyzed. Check that `clangd --version` runs, or use cpp_tidy instead."
    ))
}

/// Read the next framed message, enforcing the wall-clock deadline.
pub(crate) async fn next_message<R>(
    reader: &mut R,
    deadline: Instant,
) -> Result<JsonValue, FunctionCallError>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    next_message_raw(reader, deadline)
        .await
        .map_err(ReadFail::into_error)
}

pub(crate) async fn next_message_raw<R>(
    reader: &mut R,
    deadline: Instant,
) -> Result<JsonValue, ReadFail>
where
    R: AsyncBufRead + AsyncRead + Unpin,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ReadFail::TimedOut);
    }
    match timeout(remaining, read_message(reader)).await {
        Err(_) => Err(ReadFail::TimedOut),
        Ok(Err(err)) => Err(ReadFail::Failed(format!("read error: {err}"))),
        Ok(Ok(None)) => Err(ReadFail::Failed("it closed its output".to_string())),
        Ok(Ok(Some(value))) => Ok(value),
    }
}

/// One `file:line:col` location from an LSP `Location`/`LocationLink` result (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LspLocation {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

/// Every location in a definition/references/implementation `result`.
pub(crate) fn locations(result: &JsonValue) -> Vec<LspLocation> {
    let mut out = Vec::new();
    collect_lsp_locations(result, &mut out);
    out
}

fn collect_lsp_locations(value: &JsonValue, out: &mut Vec<LspLocation>) {
    match value {
        JsonValue::Array(items) => {
            for item in items {
                collect_lsp_locations(item, out);
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
                    .map(|l| l as u32 + 1)
                    .unwrap_or(1);
                let column = range
                    .pointer("/start/character")
                    .and_then(JsonValue::as_u64)
                    .map(|c| c as u32 + 1)
                    .unwrap_or(1);
                out.push(LspLocation {
                    path: PathBuf::from(uri_to_path(uri)),
                    line,
                    column,
                });
            }
        }
        _ => {}
    }
}

/// A clangd process serving several requests.
pub(crate) struct LspSession {
    child: tokio::process::Child,
    pid: Option<u32>,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    /// `clangd --version` major, when known.
    pub major: Option<u32>,
}

impl LspSession {
    /// Spawn clangd (no background index, nothing written to the project), handshake, and
    /// return the session. `compile_commands_dir` points clangd at the project's database.
    pub(crate) async fn start_clangd(
        cwd: &Path,
        compile_commands_dir: Option<&Path>,
        handshake_timeout: Duration,
    ) -> Result<Self, String> {
        let mut command = tokio::process::Command::new("clangd");
        command
            .arg("--log=error")
            .arg("--background-index=false")
            .arg("--pch-storage=memory")
            .arg("-j=2");
        if let Some(dir) = compile_commands_dir {
            command.arg(format!("--compile-commands-dir={}", dir.display()));
        }
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                "clangd is not installed".to_string()
            } else {
                format!("could not start clangd: {err}")
            }
        })?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "clangd stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "clangd stdout unavailable".to_string())?;
        let mut session = Self {
            child,
            pid,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            opened: HashSet::new(),
            major: crate::toolver::tool_version("clangd", cwd)
                .await
                .map(|(major, _)| major),
        };
        let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let params = json!({
            "processId": null,
            "rootUri": path_to_uri(&root),
            "capabilities": {
                "offsetEncoding": ["utf-8"],
                "general": { "positionEncodings": ["utf-8"] },
            },
        });
        session
            .request("initialize", params, handshake_timeout)
            .await
            .map_err(|err| format!("clangd did not initialize: {err:?}"))?;
        send_message(&mut session.stdin, &notification("initialized", json!({})))
            .await
            .map_err(|err| format!("{err:?}"))?;
        Ok(session)
    }

    /// `textDocument/didOpen` once per file.
    pub(crate) async fn open(&mut self, path: &Path) -> Result<(), FunctionCallError> {
        if self.opened.contains(path) {
            return Ok(());
        }
        let text = std::fs::read_to_string(path).map_err(|err| {
            FunctionCallError::RespondToModel(format!("cannot read {}: {err}", path.display()))
        })?;
        let language = match path.extension().and_then(|ext| ext.to_str()) {
            Some("c") => "c",
            _ => "cpp",
        };
        let did_open = notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": language,
                    "version": 1,
                    "text": text,
                }
            }),
        );
        send_message(&mut self.stdin, &did_open).await?;
        self.opened.insert(path.to_path_buf());
        Ok(())
    }

    /// Send one request with a fresh id and wait (at most `limit`) for its result.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: JsonValue,
        limit: Duration,
    ) -> Result<JsonValue, FunctionCallError> {
        let id = self.next_id;
        self.next_id += 1;
        send_message(&mut self.stdin, &request(id, method, params)).await?;
        let deadline = Instant::now() + limit;
        let response = read_until_id(&mut self.reader, deadline, id).await?;
        if let Some(error) = response.get("error") {
            return Err(FunctionCallError::RespondToModel(format!(
                "clangd answered {method} with an error: {error}"
            )));
        }
        Ok(response.get("result").cloned().unwrap_or(JsonValue::Null))
    }

    /// Position request (`textDocument/definition`, `…/implementation`, …) at a 1-based
    /// line/column of `path`.
    pub(crate) async fn at_position(
        &mut self,
        method: &str,
        path: &Path,
        line: u32,
        column: u32,
        limit: Duration,
    ) -> Result<Vec<LspLocation>, FunctionCallError> {
        self.open(path).await?;
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line.saturating_sub(1), "character": column.saturating_sub(1) },
        });
        let result = self.request(method, params, limit).await?;
        Ok(locations(&result))
    }

    /// Best-effort graceful shutdown, then kill the whole process group.
    pub(crate) async fn shutdown(mut self) {
        let id = self.next_id;
        let _ = send_message(&mut self.stdin, &request(id, "shutdown", json!(null))).await;
        let _ = send_message(&mut self.stdin, &notification("exit", json!(null))).await;
        crate::run::kill_process_group(self.pid);
        let _ = self.child.kill().await;
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        crate::run::kill_process_group(self.pid);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_location_and_location_link_results() {
        let result = json!([
            { "uri": "file:///a/b.h", "range": { "start": { "line": 9, "character": 4 } } },
            { "targetUri": "file:///a/c%2B%2B.h",
              "targetSelectionRange": { "start": { "line": 0, "character": 0 } } }
        ]);
        assert_eq!(
            locations(&result),
            vec![
                LspLocation {
                    path: PathBuf::from("/a/b.h"),
                    line: 10,
                    column: 5
                },
                LspLocation {
                    path: PathBuf::from("/a/c++.h"),
                    line: 1,
                    column: 1
                },
            ]
        );
        assert!(locations(&JsonValue::Null).is_empty());
    }

    #[tokio::test]
    async fn request_ids_increment_across_requests() {
        // A fake server that echoes each request's id back as a result.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-lsp.py");
        std::fs::write(
            &script,
            "import sys, json\n\
             def read():\n\
             \tlength = 0\n\
             \twhile True:\n\
             \t\tline = sys.stdin.buffer.readline()\n\
             \t\tif not line: sys.exit(0)\n\
             \t\tline = line.strip()\n\
             \t\tif not line: break\n\
             \t\tif line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])\n\
             \treturn json.loads(sys.stdin.buffer.read(length))\n\
             while True:\n\
             \tmsg = read()\n\
             \tif 'id' in msg:\n\
             \t\tbody = json.dumps({'jsonrpc': '2.0', 'id': msg['id'], 'result': {'echo': msg['id']}}).encode()\n\
             \t\tsys.stdout.buffer.write(b'Content-Length: %d\\r\\n\\r\\n' % len(body) + body)\n\
             \t\tsys.stdout.buffer.flush()\n",
        )
        .unwrap();
        let Ok(mut child) = tokio::process::Command::new("python3")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        else {
            eprintln!("skipped: python3 not available");
            return;
        };
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pid = child.id();
        let mut session = LspSession {
            child,
            pid,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            opened: HashSet::new(),
            major: None,
        };
        for want in 1..=3 {
            let result = session
                .request("x", json!({}), Duration::from_secs(10))
                .await
                .unwrap();
            assert_eq!(result, json!({ "echo": want }));
        }
        session.shutdown().await;
    }
}
