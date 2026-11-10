//! Test-only helpers shared by the tool tests: call a tool through its real
//! `handle` entry point, and a tiny HTTP/1.1 mock for the online tools.

use std::io::Read as _;
use std::io::Write as _;
use std::sync::Arc;

use codex_extension_api::ConversationHistory;
use codex_extension_api::FunctionCallError;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_utils_output_truncation::TruncationPolicy;

/// Run `tool` on `args` exactly as core does; the model-facing text on
/// success, the error on failure.
pub(crate) async fn call<T>(tool: &T, args: serde_json::Value) -> Result<String, FunctionCallError>
where
    T: ?Sized + for<'call> ToolExecutor<ToolCall<'call>>,
{
    let payload = ToolPayload::Function {
        arguments: args.to_string(),
    };
    let output = tool
        .handle(ToolCall {
            turn_id: "test".to_string(),
            call_id: "call".to_string(),
            tool_name: tool.tool_name(),
            model: "test".to_string(),
            codex_turn_metadata: None,
            truncation_policy: TruncationPolicy::Bytes(1 << 20),
            source: ToolCallSource::Direct,
            conversation_history: ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await?;
    Ok(match output.to_response_item("call", &payload) {
        ResponseInputItem::FunctionCallOutput { output, .. } => match output.text_content() {
            Some(text) => text.to_string(),
            None => output
                .content_items()
                .unwrap_or_default()
                .iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        other => format!("{other:?}"),
    })
}

/// A base URL nothing listens on (connection refused).
pub(crate) fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}")
}

/// A body of more than `cap` bytes whose byte `cap` falls INSIDE a 3-byte
/// character, the shape that makes `String::truncate(cap)` panic.
pub(crate) fn body_straddling(cap: usize) -> String {
    let body = format!("{}— and more", "x".repeat(cap - 1));
    assert!(!body.is_char_boundary(cap));
    body
}

/// Serve `routes` — `(path prefix, status, content type, body)` — on a
/// loopback port for the rest of the test process; the first route whose
/// prefix matches the request path answers. Returns the base URL.
pub(crate) fn serve_http(routes: Vec<(&'static str, u16, &'static str, String)>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start = loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(at + 4);
                }
            };
            let Some(body_start) = body_start else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..body_start]).to_string();
            let length: usize = head
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while request.len() < body_start + length {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, content_type, body) = routes
                .iter()
                .find(|(prefix, ..)| path.starts_with(prefix))
                .map(|(_, status, content_type, body)| (*status, *content_type, body.clone()))
                .unwrap_or((404, "text/plain", "no route".to_string()));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// Create a FIFO at `path` (nothing ever writes to it: opening it for reading
/// blocks forever, the shape that hung the tools that read their input first).
#[cfg(unix)]
pub(crate) fn make_fifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt as _;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    // SAFETY: mkfifo(3) on a valid NUL-terminated path.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {}", path.display());
}

/// Keys sorted at every level, so a pinned rendering does not depend on whether this build
/// also compiles a crate that turns on serde_json's `preserve_order`
/// (the pinned files were written with sorted keys).
pub(crate) fn sorted_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sorted_keys(v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted_keys).collect())
        }
        other => other,
    }
}
