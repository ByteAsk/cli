use std::path::Path;

use codex_app_server_protocol::ServerNotification;
use codex_core::config::Config;
use codex_protocol::protocol::SessionConfiguredEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexStatus {
    Running,
    InitiateShutdown,
}

pub(crate) trait EventProcessor {
    /// Print summary of effective configuration and user prompt.
    fn print_config_summary(
        &mut self,
        config: &Config,
        prompt: &str,
        session_configured: &SessionConfiguredEvent,
    );

    /// Handle a single typed app-server notification emitted by the agent.
    fn process_server_notification(&mut self, notification: ServerNotification) -> CodexStatus;

    /// Handle a local exec warning that is not represented as an app-server notification.
    fn process_warning(&mut self, message: String) -> CodexStatus;

    fn print_final_output(&mut self) {}
}

/// Describe a dynamic tool's image output without carrying its payload.
///
/// Extension tools (image generation in particular) return `image_url` as a
/// `data:image/png;base64,…` URL that can be several megabytes. Both renderers
/// need the *shape* of that payload and neither may reproduce it: the human
/// renderer would flood the terminal, and `--json` would balloon the stream.
/// Only this description is shared — the truncation policies deliberately are
/// not: the human renderer caps text, `--json` never does.
///
/// Returns `(mime_type, decoded_byte_len)`. For a non-`data:` URL the byte
/// length is unknown and reported as `0` with the URL as the "type".
pub(crate) fn describe_image_url(image_url: &str) -> (String, u64) {
    let Some(rest) = image_url.strip_prefix("data:") else {
        return (image_url.to_string(), 0);
    };
    let (meta, payload) = rest.split_once(',').unwrap_or((rest, ""));
    let mime = meta.split(';').next().unwrap_or("").trim();
    let mime = if mime.is_empty() { "image" } else { mime };
    let bytes = if meta.contains("base64") {
        let padding = payload.bytes().rev().take_while(|b| *b == b'=').count();
        (payload.len() / 4 * 3).saturating_sub(padding) as u64
    } else {
        payload.len() as u64
    };
    (mime.to_string(), bytes)
}

pub(crate) fn handle_last_message(last_agent_message: Option<&str>, output_file: &Path) {
    let message = last_agent_message.unwrap_or_default();
    write_last_message_file(message, Some(output_file));
    if last_agent_message.is_none() {
        eprintln!(
            "Warning: no last agent message; wrote empty content to {}",
            output_file.display()
        );
    }
}

fn write_last_message_file(contents: &str, last_message_path: Option<&Path>) {
    if let Some(path) = last_message_path
        && let Err(e) = std::fs::write(path, contents)
    {
        eprintln!("Failed to write last message file {path:?}: {e}");
    }
}
