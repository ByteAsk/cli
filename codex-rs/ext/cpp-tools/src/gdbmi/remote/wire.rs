//! Bounded GDB/MI framing shared with the existing MI record parser.

use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use super::MiRecord;
use super::StreamKind;
use super::parse_mi_line;

pub(super) fn quote(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

pub(crate) fn monitor(command: &str) -> String {
    format!(
        "-interpreter-exec console {}",
        quote(&format!("monitor {command}"))
    )
}

pub(super) async fn exchange(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncBufRead + Unpin),
    token: u64,
    command: &str,
) -> Result<String, String> {
    input
        .write_all(format!("{token}{command}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    input.flush().await.map_err(|e| e.to_string())?;
    let mut body = String::new();
    let mut consumed = 0;
    let mut truncated = false;
    // Every layer is bounded: individual records, records per exchange, bytes
    // processed, and model-facing text. Never buffer a missing newline forever.
    for _ in 0..8192 {
        let mut line = Vec::new();
        loop {
            let buf = output.fill_buf().await.map_err(|e| e.to_string())?;
            if buf.is_empty() {
                return Err("GDB connection closed; remote target state unknown.".to_string());
            }
            let count = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |n| n + 1);
            if line.len() + count > 64 * 1024 {
                return Err("Oversized MI record; connection retired.".to_string());
            }
            line.extend_from_slice(&buf[..count]);
            output.consume(count);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        consumed += line.len();
        if consumed > 2 * 1024 * 1024 {
            return Err("Kernel output exceeds capture budget; connection retired.".to_string());
        }
        match parse_mi_line(&String::from_utf8_lossy(&line)) {
            MiRecord::Stream {
                kind: StreamKind::Console | StreamKind::Target,
                text,
            } => {
                let room = crate::gdb_common::OUTPUT_CAP.saturating_sub(body.len());
                let mut keep = room.min(text.len());
                while !text.is_char_boundary(keep) {
                    keep -= 1;
                }
                body.push_str(&text[..keep]);
                truncated |= keep != text.len();
            }
            MiRecord::Result {
                token: Some(t),
                class,
                payload,
            } if t == token => {
                if class == "error" {
                    return Err(payload
                        .get("msg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("GDB command failed")
                        .to_string());
                }
                if !matches!(class.as_str(), "done" | "connected") {
                    return Err(format!(
                        "Unexpected GDB result {class}; connection retired."
                    ));
                }
                if truncated {
                    body.push_str("\n[output truncated]");
                }
                return Ok(body);
            }
            MiRecord::Result { .. } => {
                return Err(
                    "Unexpected MI token; connection retired to avoid stale results.".to_string(),
                );
            }
            MiRecord::Async {
                kind: super::super::parser::AsyncKind::Exec,
                class,
                ..
            } if class == "running" => {
                return Err("Remote target resumed unexpectedly; connection retired.".to_string());
            }
            _ => {}
        }
    }
    Err("Too many MI records; connection retired.".to_string())
}
