//! Minimal client for the ByteAsk Embedded MCP server (streamable-HTTP,
//! JSON-RPC 2.0, SSE-framed responses). Mirrors the reference Python harness:
//! `initialize` -> `notifications/initialized` -> `tools/call`, reusing one
//! `Mcp-Session-Id` for the process. Deliberately hand-rolled (not the engine's
//! full MCP-client machinery) so the reference tools stay a thin, standalone
//! extension with our own banner/logging and no per-user server setup.

use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_extension_api::FunctionCallError;
use codex_login::default_client::create_client;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Mutex;

/// Public ByteAsk Embedded MCP endpoint. Override with `BYTEASK_REF_URL` (tests
/// point this at a local mock).
const DEFAULT_URL: &str = "https://mcp.byteask.ai/mcp";
/// Sent on every request. Cloudflare 403s reqwest/urllib default agents, so we
/// set an explicit ByteAsk agent (any non-default value is accepted).
const USER_AGENT: &str = concat!("byteask-engine/", env!("CARGO_PKG_VERSION"));
/// MCP protocol version we speak in `initialize`.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Cap the upstream error body echoed back to the model.
const ERROR_CAP: usize = 800;
/// Whole-request cap (connect, send, and read the body). Measured on the
/// production server: search latency p99 15.3 s, max 62.8 s. Without a cap a
/// hung server or an SSE stream that never closes held the tool — and the
/// user's turn — until the turn was cancelled by hand.
/// (2 s under test, so the timeout test does not wait 90 s.)
const REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(if cfg!(test) { 2 } else { 90 });
/// Start of the message a timed-out request produces; `call_tool` does not
/// retry it (a retry is for an expired session, and would double the wait).
const TIMEOUT_MESSAGE: &str = "ByteAsk reference server did not answer within";

fn endpoint() -> String {
    std::env::var("BYTEASK_REF_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// Process-wide cached session id (established once by the handshake, reused by
/// every `tools/call`). Held behind an async mutex so concurrent first-calls do
/// not race the handshake.
fn session_cell() -> &'static Mutex<Option<String>> {
    static SESSION: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

fn next_id() -> u64 {
    static ID: AtomicU64 = AtomicU64::new(0);
    ID.fetch_add(1, Ordering::Relaxed) + 1
}

/// Call an MCP tool by name and return its first text content block.
///
/// Establishes the session on first use; on failure (e.g. the server restarted
/// and the session expired) it re-handshakes and retries exactly once.
pub async fn call_tool(name: &str, arguments: Value) -> Result<String, FunctionCallError> {
    let mut last: Option<FunctionCallError> = None;
    for attempt in 0..2u8 {
        let session = match acquire_session(attempt == 1).await {
            Ok(session) => session,
            Err(err) => {
                let timed_out = is_timeout(&err);
                last = Some(err);
                if timed_out {
                    break;
                }
                continue;
            }
        };
        let params = json!({ "name": name, "arguments": arguments.clone() });
        match rpc("tools/call", Some(params), false, Some(&session)).await {
            Ok((_, Some(value))) => return extract_text(&value),
            Ok((_, None)) => {
                last = Some(FunctionCallError::RespondToModel(
                    "ByteAsk reference server returned an empty response".to_string(),
                ));
            }
            Err(err) => {
                let timed_out = is_timeout(&err);
                last = Some(err);
                if timed_out {
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        FunctionCallError::RespondToModel("ByteAsk reference lookup failed".to_string())
    }))
}

/// Return the current session id, performing the handshake if none is cached.
/// `force_new` clears any cached id first (used for the one retry after a
/// suspected session expiry).
async fn acquire_session(force_new: bool) -> Result<String, FunctionCallError> {
    let mut guard = session_cell().lock().await;
    if force_new {
        *guard = None;
    }
    if let Some(session) = guard.as_ref() {
        return Ok(session.clone());
    }
    let session = handshake().await?;
    *guard = Some(session.clone());
    Ok(session)
}

/// `initialize` (grab the `Mcp-Session-Id` header) then `notifications/initialized`.
async fn handshake() -> Result<String, FunctionCallError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "byteask-engine", "version": "1.0" },
    });
    let (session, _) = rpc("initialize", Some(params), false, None).await?;
    let session = session.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "ByteAsk reference server did not return a session id".to_string(),
        )
    })?;
    rpc("notifications/initialized", None, true, Some(&session)).await?;
    Ok(session)
}

/// One JSON-RPC POST. Returns `(new Mcp-Session-Id header if any, parsed response
/// or None for a notification)`. SSE-framed bodies are unwrapped from their
/// `data:` line.
async fn rpc(
    method: &str,
    params: Option<Value>,
    notify: bool,
    session: Option<&str>,
) -> Result<(Option<String>, Option<Value>), FunctionCallError> {
    let mut body = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(params) = params {
        body["params"] = params;
    }
    if !notify {
        body["id"] = json!(next_id());
    }

    let mut request = create_client()
        .post(endpoint())
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .json(&body);
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }

    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            timed_out()
        } else {
            FunctionCallError::RespondToModel(format!(
                "ByteAsk reference server unreachable: {err}"
            ))
        }
    })?;

    let new_session = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let status = response.status();
    let raw = response.text().await.map_err(|err| {
        if err.is_timeout() {
            timed_out()
        } else {
            FunctionCallError::RespondToModel(format!("reading reference response failed: {err}"))
        }
    })?;

    if !status.is_success() {
        let snippet = error_snippet(&raw);
        return Err(FunctionCallError::RespondToModel(format!(
            "ByteAsk reference server returned HTTP {}: {snippet}",
            status.as_u16()
        )));
    }
    if notify {
        return Ok((new_session, None));
    }

    let value = parse_rpc_body(&raw)?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(FunctionCallError::RespondToModel(format!(
            "ByteAsk reference server error: {message}"
        )));
    }
    Ok((new_session, Some(value)))
}

fn is_timeout(err: &FunctionCallError) -> bool {
    matches!(err, FunctionCallError::RespondToModel(message) if message.starts_with(TIMEOUT_MESSAGE))
}

fn timed_out() -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "{TIMEOUT_MESSAGE} {} s, so no documents came back. This is a server-side delay, not a \
         miss: the lookup can be retried; do not treat it as \"not covered\".",
        REQUEST_TIMEOUT.as_secs()
    ))
}

/// The head of an upstream error body, at most `ERROR_CAP` bytes, cut on a
/// char boundary.
///
/// ⚠️ `String::truncate` PANICS when the cap lands inside a multi-byte
/// character, and a panic in a tool task is not an error the model sees: core
/// turns the failed join into `FunctionCallError::Fatal`, which ends the user's
/// whole turn. The realistic trigger is a proxy's HTML error page (a Cloudflare
/// interstitial) with any non-ASCII character straddling byte 800.
fn error_snippet(raw: &str) -> &str {
    let mut end = raw.len().min(ERROR_CAP);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    &raw[..end]
}

/// Parse a JSON-RPC response body that may be SSE-framed (one or more
/// `data: {json}` lines) or a bare JSON object. Returns the first parseable
/// JSON payload.
fn parse_rpc_body(raw: &str) -> Result<Value, FunctionCallError> {
    for line in raw.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.trim();
            if rest.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(rest) {
                return Ok(value);
            }
        }
    }
    serde_json::from_str::<Value>(raw.trim()).map_err(|err| {
        FunctionCallError::RespondToModel(format!("invalid JSON from reference server: {err}"))
    })
}

/// Pull `result.content[0].text` out of an MCP `tools/call` response.
///
/// A failed tool call is NOT a JSON-RPC error: the server answers HTTP 200 with
/// `result.isError: true` and puts the error in the text block (validation
/// errors, and — seen in real use — "embedding failed: 429
/// RESOURCE_EXHAUSTED"). Returned as text, that error reached the model under
/// a normal `📚` banner, as if it were what the documents say.
fn extract_text(value: &Value) -> Result<String, FunctionCallError> {
    let text = value
        .get("result")
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find_map(|item| item.get("text")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "ByteAsk reference server returned no text content".to_string(),
            )
        })?;
    let is_error = value
        .get("result")
        .and_then(|result| result.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        return Err(FunctionCallError::RespondToModel(format!(
            "The ByteAsk reference server FAILED this lookup, so no documents came back — this is \
             an error, not an answer and not a \"not covered\" miss. Server error: {}",
            error_snippet(text.trim())
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Serializes the tests that set `BYTEASK_REF_URL`: it is process-global
    /// and the test harness runs tests on parallel threads.
    fn ref_url_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn endpoint_defaults_and_honors_override() {
        let _ref_url = ref_url_lock();
        // SAFETY: the only other test that sets this variable holds the same lock.
        unsafe { std::env::remove_var("BYTEASK_REF_URL") };
        assert_eq!(endpoint(), "https://mcp.byteask.ai/mcp");
        unsafe { std::env::set_var("BYTEASK_REF_URL", "http://127.0.0.1:9/mcp") };
        assert_eq!(endpoint(), "http://127.0.0.1:9/mcp");
        unsafe { std::env::remove_var("BYTEASK_REF_URL") };
    }

    #[test]
    fn parse_rpc_body_reads_sse_data_line() {
        let raw =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let value = parse_rpc_body(raw).unwrap();
        assert_eq!(value["result"]["ok"], json!(true));
    }

    #[test]
    fn parse_rpc_body_reads_plain_json() {
        let raw = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":2}}";
        assert_eq!(parse_rpc_body(raw).unwrap()["result"]["n"], json!(2));
    }

    #[test]
    fn parse_rpc_body_errors_on_garbage() {
        assert!(parse_rpc_body("not json at all").is_err());
    }

    #[test]
    fn extract_text_pulls_first_content_block() {
        let value = json!({
            "result": { "content": [{ "type": "text", "text": "MISRA 5-2-4: ..." }] }
        });
        assert_eq!(extract_text(&value).unwrap(), "MISRA 5-2-4: ...");
    }

    #[test]
    fn a_tool_error_is_an_error_not_an_answer() {
        // CATCHES: `isError: true` results (HTTP 200) reaching the model as
        // document text. Seen in real use: an embedding quota failure.
        let value = json!({
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": "RuntimeError: embedding failed: 429 RESOURCE_EXHAUSTED" }]
            }
        });
        let message = match extract_text(&value) {
            Err(FunctionCallError::RespondToModel(message)) => message,
            other => panic!("isError must be an error, got {other:?}"),
        };
        assert!(message.contains("FAILED"), "{message}");
        assert!(message.contains("429 RESOURCE_EXHAUSTED"), "{message}");
        // `isError: false` is an ordinary answer.
        let ok = json!({ "result": { "isError": false, "content": [{ "type": "text", "text": "hit" }] } });
        assert_eq!(extract_text(&ok).unwrap(), "hit");
    }

    /// CATCHES: no request timeout — a server that accepts the connection and
    /// never answers held the tool until the user cancelled the turn. And a
    /// timeout must not be retried (the retry is for an expired session).
    #[test]
    fn a_server_that_never_answers_times_out_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            // Hold every connection open without answering.
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        let _ref_url = ref_url_lock();
        // SAFETY: the only other tests that set this variable hold the same lock.
        unsafe { std::env::set_var("BYTEASK_REF_URL", format!("http://{addr}/mcp")) };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let start = std::time::Instant::now();
        let result = runtime.block_on(call_tool("search_docs", json!({ "query": "x" })));
        let elapsed = start.elapsed();
        unsafe { std::env::remove_var("BYTEASK_REF_URL") };
        let message = match result {
            Err(FunctionCallError::RespondToModel(message)) => message,
            other => panic!("a hung server must be a model-facing error, got {other:?}"),
        };
        assert!(message.starts_with(TIMEOUT_MESSAGE), "{message}");
        assert!(
            elapsed < REQUEST_TIMEOUT * 2 - std::time::Duration::from_millis(300),
            "a timeout must not be retried: took {elapsed:?}"
        );
    }

    #[test]
    fn extract_text_errors_when_absent() {
        assert!(extract_text(&json!({ "result": { "content": [] } })).is_err());
        assert!(extract_text(&json!({ "result": {} })).is_err());
    }

    /// A body whose byte `ERROR_CAP` sits inside `—` (3 bytes, starting one
    /// byte before the cap), shaped like the HTML page a proxy serves.
    fn error_page_straddling_the_cap() -> String {
        let head = "<!DOCTYPE html><html><head><title>Just a moment...</title></head><body>";
        let pad = "x".repeat(ERROR_CAP - 1 - head.len());
        let page = format!("{head}{pad}— retry later</body></html>");
        assert!(
            !page.is_char_boundary(ERROR_CAP),
            "the fixture must put a multi-byte character across the cap"
        );
        page
    }

    #[test]
    fn error_snippet_cuts_on_a_char_boundary() {
        let page = error_page_straddling_the_cap();
        let snippet = error_snippet(&page);
        // The character that straddles the cap is dropped whole.
        assert_eq!(snippet.len(), ERROR_CAP - 1);
        assert!(snippet.ends_with('x'), "{snippet}");
        assert!(page.starts_with(snippet));
        // Short bodies come back whole.
        assert_eq!(error_snippet("Bad Gateway"), "Bad Gateway");
        assert_eq!(error_snippet(""), "");
    }

    /// CATCHES: a non-2xx reply whose body puts a multi-byte character across
    /// the `ERROR_CAP` cut PANICKING the tool. `rpc` used `String::truncate`,
    /// which panics off a char boundary, and core turns a panicked tool task
    /// into `FunctionCallError::Fatal` — the user's whole turn ends instead of
    /// the model hearing "HTTP 503".
    ///
    /// NON-VACUITY: this drives the real `call_tool` (handshake, retry, `rpc`)
    /// over real HTTP. The fixture asserts byte `ERROR_CAP` is inside a
    /// character, and the mock reads each request in full before it answers,
    /// so the client receives the 503 body (not a connection reset) and reaches
    /// the cut. Measured: with the old `truncate` this test panics.
    #[test]
    fn an_error_page_with_a_multibyte_char_at_the_cap_is_an_error_not_a_panic() {
        use std::io::Read as _;
        use std::io::Write as _;

        let page = error_page_straddling_the_cap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let response = format!(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/html; charset=UTF-8\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{page}",
            page.len()
        );
        // Answers every connection (the handshake is retried once) and lives
        // until the test process exits.
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
                let headers = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
                let length: usize = headers
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
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let _ref_url = ref_url_lock();
        // SAFETY: the only other test that sets this variable holds the same lock.
        unsafe { std::env::set_var("BYTEASK_REF_URL", format!("http://{addr}/mcp")) };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = runtime.block_on(call_tool("search_docs", json!({ "query": "RM0090 RCC" })));
        unsafe { std::env::remove_var("BYTEASK_REF_URL") };

        let message = match result {
            Err(FunctionCallError::RespondToModel(message)) => message,
            other => panic!("a 503 must reach the model as an error it can act on, got {other:?}"),
        };
        assert!(
            message.starts_with("ByteAsk reference server returned HTTP 503: <!DOCTYPE html>"),
            "{message}"
        );
        assert!(
            message.ends_with('x'),
            "cut before the straddling character: {message}"
        );
    }

    // Live end-to-end tests against the real ByteAsk MCP server. Ignored by
    // default (network); run with `cargo test -p codex-ref-tools -- --ignored`.
    // These exercise the actual handshake + session reuse + SSE parse of THIS
    // client, not a replica.
    #[tokio::test]
    #[ignore = "hits the live mcp.byteask.ai"]
    async fn live_search_returns_grounded_page_cited_result() {
        let text = call_tool(
            "search_docs",
            json!({ "query": "Does MISRA C++ allow C-style casts?", "limit": 2 }),
        )
        .await
        .expect("live search_docs should succeed");
        let lower = text.to_lowercase();
        assert!(
            (lower.contains("5-2-4") || lower.contains("5–2–4") || lower.contains("c-style"))
                && lower.contains("misra"),
            "expected a grounded MISRA C-style-cast hit, got:\n{}",
            text.chars().take(300).collect::<String>()
        );
    }

    #[tokio::test]
    #[ignore = "hits the live mcp.byteask.ai"]
    async fn live_miss_reports_no_confident_match_and_session_is_reused() {
        // A first call establishes the session; a second (nonsense) call must
        // reuse it and come back with the corpus's not-found sentinel.
        let _ = call_tool("search_docs", json!({ "query": "STM32F4 RCC register" }))
            .await
            .expect("first live call establishes session");
        let miss = call_tool(
            "search_docs",
            json!({ "query": "zxqw nonexistent flibberflab protocol 9999" }),
        )
        .await
        .expect("second live call reuses the session");
        assert!(
            miss.to_lowercase().contains("no confident match"),
            "expected the not-found sentinel, got:\n{}",
            miss.chars().take(200).collect::<String>()
        );
    }
}
