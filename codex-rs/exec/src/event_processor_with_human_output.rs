use std::io::IsTerminal;
use std::path::PathBuf;

use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::McpToolCallResult;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::Config;
use codex_model_provider_info::WireApi;
use codex_protocol::num_format::format_with_separators;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_utils_sandbox_summary::summarize_permission_profile;
use owo_colors::OwoColorize;
use owo_colors::Style;
use serde_json::Value as JsonValue;

use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;
use crate::event_processor::describe_image_url;
use crate::event_processor::handle_last_message;

/// Maximum bytes of a single tool text block that the human renderer prints.
///
/// **Truncation ownership.** Three independent layers can shorten
/// tool output; they are ordered, not redundant:
///
/// 1. The *tool* trims by meaning (cpp-tools drops the sanitizer shadow-byte
///    block) and then applies its own payload cap (`STDOUT_CAP`, 2 KiB today).
/// 2. This renderer cap is a terminal-flood backstop only. It is set an order
///    of magnitude above any tool-owned cap precisely so it can never become
///    the reason a sanitizer trace goes unseen (a renderer cap below the trace
///    size would leave the trace invisible).
/// 3. `--json` has no cap at all.
///
/// If a tool-owned cap is ever raised above this value, raise this one first.
const TOOL_OUTPUT_TEXT_CAP: usize = 16 * 1024;

/// Maximum bytes of the one-line tool arguments preview.
const TOOL_ARGUMENTS_PREVIEW_CAP: usize = 200;

pub(crate) struct EventProcessorWithHumanOutput {
    bold: Style,
    cyan: Style,
    dimmed: Style,
    green: Style,
    italic: Style,
    magenta: Style,
    red: Style,
    yellow: Style,
    show_agent_reasoning: bool,
    show_raw_agent_reasoning: bool,
    last_message_path: Option<PathBuf>,
    final_message: Option<String>,
    final_message_rendered: bool,
    emit_final_message_on_shutdown: bool,
    last_total_token_usage: Option<ThreadTokenUsage>,
    /// Whether this session's shell commands run under a read-only sandbox.
    /// Captured from the effective config so the sandbox-denial hint
    /// is not printed when writes were actually allowed and the failure is a
    /// genuine filesystem error.
    sandbox_is_read_only: bool,
    /// The sandbox-denial hint is advice, not an error: print it once.
    sandbox_hint_shown: bool,
}

impl EventProcessorWithHumanOutput {
    pub(crate) fn create_with_ansi(
        with_ansi: bool,
        config: &Config,
        last_message_path: Option<PathBuf>,
    ) -> Self {
        let style = |styled: Style, plain: Style| if with_ansi { styled } else { plain };
        Self {
            bold: style(Style::new().bold(), Style::new()),
            cyan: style(Style::new().cyan(), Style::new()),
            dimmed: style(Style::new().dimmed(), Style::new()),
            green: style(Style::new().green(), Style::new()),
            italic: style(Style::new().italic(), Style::new()),
            magenta: style(Style::new().magenta(), Style::new()),
            red: style(Style::new().red(), Style::new()),
            yellow: style(Style::new().yellow(), Style::new()),
            show_agent_reasoning: !config.hide_agent_reasoning,
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            last_message_path,
            final_message: None,
            final_message_rendered: false,
            emit_final_message_on_shutdown: false,
            last_total_token_usage: None,
            sandbox_is_read_only: false,
            sandbox_hint_shown: false,
        }
    }

    fn render_item_started(&self, item: &ThreadItem) {
        for line in self.item_started_lines(item) {
            eprintln!("{line}");
        }
    }

    /// The lines `render_item_started` prints, so the render contract is
    /// testable without capturing stderr.
    fn item_started_lines(&self, item: &ThreadItem) -> Vec<String> {
        match item {
            ThreadItem::CommandExecution { command, cwd, .. } => vec![
                format!("{}", "exec".style(self.italic).style(self.magenta)),
                format!("{} in {cwd}", command.style(self.bold)),
            ],
            ThreadItem::McpToolCall { server, tool, .. } => vec![format!(
                "{} {} {}",
                "mcp:".style(self.bold),
                format!("{server}/{tool}").style(self.cyan),
                "started".style(self.dimmed)
            )],
            ThreadItem::DynamicToolCall {
                namespace,
                tool,
                arguments,
                ..
            } => {
                let mut lines = vec![format!(
                    "{} {} {}",
                    "tool:".style(self.bold),
                    dynamic_tool_label(namespace.as_deref(), tool).style(self.cyan),
                    "started".style(self.dimmed)
                )];
                if let Some(preview) = tool_arguments_preview(arguments) {
                    lines.push(format!("{}", preview.style(self.dimmed)));
                }
                lines
            }
            ThreadItem::WebSearch(item) => {
                vec![format!("{} {}", "web search:".style(self.bold), item.query)]
            }
            ThreadItem::FileChange { .. } => {
                vec![format!("{}", "apply patch".style(self.bold))]
            }
            ThreadItem::CollabAgentToolCall { tool, .. } => {
                vec![format!("{} {tool:?}", "collab:".style(self.bold))]
            }
            // Enumerated on purpose: a `_ => {}` arm silently hides a variant
            // it does not name (`DynamicToolCall` was hidden that way). A new
            // upstream variant must be a compile error, not a silent drop.
            ThreadItem::UserMessage { .. }
            | ThreadItem::HookPrompt { .. }
            | ThreadItem::AgentMessage { .. }
            | ThreadItem::Plan { .. }
            | ThreadItem::Reasoning { .. }
            | ThreadItem::SubAgentActivity { .. }
            | ThreadItem::ImageView { .. }
            | ThreadItem::Sleep { .. }
            | ThreadItem::ImageGeneration(_)
            | ThreadItem::EnteredReviewMode { .. }
            | ThreadItem::ExitedReviewMode { .. }
            | ThreadItem::ContextCompaction { .. }
            | ThreadItem::FunctionCallOutput { .. } => Vec::new(),
        }
    }

    fn render_item_completed(&mut self, item: ThreadItem) {
        if let ThreadItem::AgentMessage { text, .. } = &item {
            self.final_message = Some(text.clone());
            self.final_message_rendered = true;
        }
        for line in self.item_completed_lines(&item) {
            eprintln!("{line}");
        }
        if !self.sandbox_hint_shown
            && let Some(hint) = self.sandbox_denial_hint(&item)
        {
            self.sandbox_hint_shown = true;
            eprintln!("{hint}");
        }
    }

    /// One actionable line after a shell command that a read-only sandbox
    /// denied. Without it the run dead-ends: the agent retries
    /// with `TMPDIR=/tmp`, fails again, and reports that the product cannot
    /// compile.
    fn sandbox_denial_hint(&self, item: &ThreadItem) -> Option<String> {
        if !self.sandbox_is_read_only {
            return None;
        }
        let ThreadItem::CommandExecution {
            aggregated_output,
            exit_code,
            status,
            ..
        } = item
        else {
            return None;
        };
        if !matches!(status, CommandExecutionStatus::Failed) {
            return None;
        }
        if !looks_like_sandbox_write_denial(*exit_code, aggregated_output.as_deref()?) {
            return None;
        }
        Some(format!(
            "{}",
            concat!(
                "hint: shell commands run in a read-only sandbox, so this write was denied. ",
                "Re-run with `--sandbox workspace-write` to let the agent build and write. ",
                "Note that cpp_sanitize / cpp_perf build and run outside the sandbox and do not need it."
            )
            .style(self.yellow)
        ))
    }

    /// The lines `render_item_completed` prints, so the render contract is
    /// testable without capturing stderr.
    fn item_completed_lines(&self, item: &ThreadItem) -> Vec<String> {
        match item {
            ThreadItem::AgentMessage { text, .. } => vec![
                format!("{}", "ByteAsk".style(self.italic).style(self.magenta)),
                text.clone(),
            ],
            ThreadItem::Reasoning {
                summary, content, ..
            } => match reasoning_text(summary, content, self.show_raw_agent_reasoning) {
                Some(text) if self.show_agent_reasoning && !text.trim().is_empty() => {
                    vec![format!("{}", text.style(self.dimmed))]
                }
                _ => Vec::new(),
            },
            ThreadItem::CommandExecution {
                command: _,
                aggregated_output,
                exit_code,
                status,
                duration_ms,
                ..
            } => {
                let duration_suffix = duration_ms
                    .map(|duration_ms| format!(" in {duration_ms}ms"))
                    .unwrap_or_default();
                let mut lines = vec![match status {
                    CommandExecutionStatus::Completed => {
                        format!(
                            "{}",
                            format!(" succeeded{duration_suffix}:").style(self.green)
                        )
                    }
                    CommandExecutionStatus::Failed => {
                        let exit_code = exit_code.unwrap_or(1);
                        format!(
                            "{}",
                            format!(" exited {exit_code}{duration_suffix}:").style(self.red)
                        )
                    }
                    CommandExecutionStatus::Declined => {
                        format!(
                            "{}",
                            format!(" declined{duration_suffix}:").style(self.yellow)
                        )
                    }
                    CommandExecutionStatus::InProgress => {
                        format!(
                            "{}",
                            format!(" in progress{duration_suffix}:").style(self.dimmed)
                        )
                    }
                }];
                if let Some(output) = aggregated_output
                    && !output.trim().is_empty()
                {
                    lines.push(output.clone());
                }
                lines
            }
            ThreadItem::FileChange {
                changes, status, ..
            } => {
                let status_text = match status {
                    PatchApplyStatus::Completed => "completed",
                    PatchApplyStatus::Failed => "failed",
                    PatchApplyStatus::Declined => "declined",
                    PatchApplyStatus::InProgress => "in_progress",
                };
                let mut lines = vec![format!("{} {status_text}", "patch:".style(self.bold))];
                lines.extend(
                    changes
                        .iter()
                        .map(|change| format!("{}", change.path.style(self.dimmed))),
                );
                lines
            }
            ThreadItem::McpToolCall {
                server,
                tool,
                status,
                result,
                error,
                duration_ms,
                ..
            } => {
                let status_text = match status {
                    McpToolCallStatus::Completed => "completed".style(self.green).to_string(),
                    McpToolCallStatus::Failed => "failed".style(self.red).to_string(),
                    McpToolCallStatus::InProgress => "in_progress".style(self.dimmed).to_string(),
                };
                let duration_suffix = duration_ms
                    .map(|duration_ms| format!(" in {duration_ms}ms"))
                    .unwrap_or_default();
                let mut lines = vec![format!(
                    "{} {} {}",
                    "mcp:".style(self.bold),
                    format!("{server}/{tool}").style(self.cyan),
                    format!("({status_text}{duration_suffix})").style(self.dimmed)
                )];
                if let Some(error) = error {
                    lines.push(format!("{}", error.message.style(self.red)));
                }
                // The output body used to be dropped here: exec printed that an
                // MCP tool ran and whether it failed, but never what it said —
                // the same defect as the missing `DynamicToolCall` arm.
                if let Some(result) = result {
                    lines.extend(mcp_result_lines(result));
                }
                lines
            }
            ThreadItem::DynamicToolCall {
                namespace,
                tool,
                status,
                content_items,
                success,
                duration_ms,
                ..
            } => {
                let failed =
                    matches!(status, DynamicToolCallStatus::Failed) || *success == Some(false);
                let status_text = if failed {
                    "failed".style(self.red).to_string()
                } else {
                    match status {
                        DynamicToolCallStatus::InProgress => {
                            "in_progress".style(self.dimmed).to_string()
                        }
                        DynamicToolCallStatus::Completed | DynamicToolCallStatus::Failed => {
                            "completed".style(self.green).to_string()
                        }
                    }
                };
                let duration_suffix = duration_ms
                    .map(|duration_ms| format!(" in {duration_ms}ms"))
                    .unwrap_or_default();
                let mut lines = vec![format!(
                    "{} {} {}",
                    "tool:".style(self.bold),
                    dynamic_tool_label(namespace.as_deref(), tool).style(self.cyan),
                    format!("({status_text}{duration_suffix})").style(self.dimmed)
                )];
                lines.extend(dynamic_tool_output_lines(content_items.as_deref()));
                lines
            }
            ThreadItem::WebSearch(item) => {
                vec![format!("{} {}", "web search:".style(self.bold), item.query)]
            }
            ThreadItem::ContextCompaction { .. } => {
                vec![format!("{}", "context compacted".style(self.dimmed))]
            }
            // Enumerated on purpose; see `item_started_lines`.
            ThreadItem::UserMessage { .. }
            | ThreadItem::HookPrompt { .. }
            | ThreadItem::Plan { .. }
            | ThreadItem::CollabAgentToolCall { .. }
            | ThreadItem::SubAgentActivity { .. }
            | ThreadItem::ImageView { .. }
            | ThreadItem::Sleep { .. }
            | ThreadItem::ImageGeneration(_)
            | ThreadItem::EnteredReviewMode { .. }
            | ThreadItem::ExitedReviewMode { .. }
            | ThreadItem::FunctionCallOutput { .. } => Vec::new(),
        }
    }
}

/// `namespace/tool` when the extension declares a namespace, else `tool`.
fn dynamic_tool_label(namespace: Option<&str>, tool: &str) -> String {
    match namespace {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}/{tool}"),
        _ => tool.to_string(),
    }
}

/// A one-line, byte-capped preview of a tool call's arguments, so the reader
/// can see *what* was analysed (`cpp_sanitize` on which file) and not just that
/// something ran. `None` when there is nothing worth showing.
fn tool_arguments_preview(arguments: &JsonValue) -> Option<String> {
    match arguments {
        JsonValue::Null => return None,
        JsonValue::Object(map) if map.is_empty() => return None,
        _ => {}
    }
    let rendered = serde_json::to_string(arguments).ok()?;
    let rendered = rendered.replace(['\n', '\r'], " ");
    Some(truncate_to_bytes(&rendered, TOOL_ARGUMENTS_PREVIEW_CAP))
}

/// The printable lines of a dynamic tool's output.
///
/// Text is byte-capped with an explicit elision marker and images are reduced
/// to a one-line placeholder naming type and size. The image case
/// is not hypothetical: the image-generation extension registers into the same
/// tool registry and returns `image_url: "data:image/png;base64,…"`, so echoing
/// content items verbatim would dump megabytes of base64 into the terminal.
fn dynamic_tool_output_lines(items: Option<&[DynamicToolCallOutputContentItem]>) -> Vec<String> {
    items
        .unwrap_or_default()
        .iter()
        .filter_map(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text } => {
                (!text.trim().is_empty())
                    .then(|| truncate_to_bytes(&printable(text), TOOL_OUTPUT_TEXT_CAP))
            }
            DynamicToolCallOutputContentItem::InputImage { image_url } => {
                Some(image_placeholder(image_url))
            }
            DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                Some(audio_placeholder(audio_url))
            }
        })
        .collect()
}

/// The printable lines of an MCP tool result, under the same capping rule.
fn mcp_result_lines(result: &McpToolCallResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|block| {
            let kind = block.get("type").and_then(JsonValue::as_str).unwrap_or("");
            match kind {
                "text" => {
                    let text = block.get("text").and_then(JsonValue::as_str)?;
                    (!text.trim().is_empty())
                        .then(|| truncate_to_bytes(&printable(text), TOOL_OUTPUT_TEXT_CAP))
                }
                "image" => {
                    let mime = block
                        .get("mimeType")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("image");
                    let bytes = block
                        .get("data")
                        .and_then(JsonValue::as_str)
                        .map(|data| data.len() / 4 * 3)
                        .unwrap_or(0);
                    Some(format!("<image: {mime}, ~{bytes} bytes>"))
                }
                "" => None,
                other => Some(format!("<{other}>")),
            }
        })
        .collect()
}

fn audio_placeholder(audio_url: &str) -> String {
    let (mime, bytes) = describe_image_url(audio_url);
    let mime = if mime == "image" { "audio".to_string() } else { mime };
    if bytes == 0 {
        format!(
            "<audio: {}>",
            truncate_to_bytes(&mime, TOOL_ARGUMENTS_PREVIEW_CAP)
        )
    } else {
        format!("<audio: {mime}, ~{bytes} bytes>")
    }
}

fn image_placeholder(image_url: &str) -> String {
    let (mime, bytes) = describe_image_url(image_url);
    if bytes == 0 {
        format!(
            "<image: {}>",
            truncate_to_bytes(&mime, TOOL_ARGUMENTS_PREVIEW_CAP)
        )
    } else {
        format!("<image: {mime}, ~{bytes} bytes>")
    }
}

/// Byte-cap `text`, appending an explicit elision marker that states how much
/// was dropped. Never splits a UTF-8 code point.
/// A tool result with its control characters dropped, lines and tabs kept.
/// The text is untrusted — what a debugged program or an MCP server printed —
/// and goes to the terminal as-is: its escape sequences would reach it (OSC 52
/// writes the clipboard, OSC 0 retitles the window, CSI 2J clears the screen).
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn truncate_to_bytes(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let elided = text.len() - end;
    format!("{}\n… [truncated, {elided} more bytes]", &text[..end])
}

/// Whether a failed shell command looks like a sandbox write denial.
///
/// Deliberately narrower than `codex_sandboxing::is_likely_sandbox_denied`
/// (which exec cannot call — it needs an `ExecToolCallOutput`): this only
/// drives an advisory hint, so `permission denied` is excluded because it fires
/// routinely on a non-executable file with no sandbox involved.
fn looks_like_sandbox_write_denial(exit_code: Option<i32>, output: &str) -> bool {
    if exit_code == Some(0) {
        return false;
    }
    const SIGNATURES: [&str; 3] = [
        // Linux (bubblewrap / landlock read-only bind).
        "read-only file system",
        // macOS Seatbelt denies writes with EPERM.
        "operation not permitted",
        // Compiler drivers surface the denial before naming the errno.
        "unable to make temporary file",
    ];
    let lower = output.to_lowercase();
    SIGNATURES.iter().any(|needle| lower.contains(needle))
}

impl EventProcessor for EventProcessorWithHumanOutput {
    fn print_config_summary(
        &mut self,
        config: &Config,
        prompt: &str,
        session_configured_event: &SessionConfiguredEvent,
    ) {
        // Prefer the ByteAsk wrapper's exported release version (the engine crate version
        // lags the release train, so a bare CARGO_PKG_VERSION prints a stale "v0.1.0" in
        // exec-mode's header). Mirrors codex-tui's display_version(); falls back to the
        // crate version for direct/dev runs.
        let version = std::env::var("BYTEASK_CLIENT_VERSION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        eprintln!(
            "{} v{version}\n--------",
            std::env::var("BYTEASK_BRAND").or_else(|_| std::env::var("CODEX_BRAND")).unwrap_or_else(|_| "ByteAsk".to_string())
        );
        self.sandbox_is_read_only = sandbox_is_read_only(config);
        for (key, value) in config_summary_entries(config, session_configured_event) {
            eprintln!("{} {}", format!("{key}:").style(self.bold), value);
        }
        eprintln!("--------");
        eprintln!("{}\n{}", "user".style(self.cyan), prompt);
    }

    fn process_server_notification(&mut self, notification: ServerNotification) -> CodexStatus {
        match notification {
            ServerNotification::ConfigWarning(notification) => {
                let details = notification
                    .details
                    .map(|details| format!(" ({details})"))
                    .unwrap_or_default();
                eprintln!(
                    "{} {}{}",
                    "warning:".style(self.yellow).style(self.bold),
                    notification.summary,
                    details
                );
                CodexStatus::Running
            }
            ServerNotification::Warning(notification) => self.process_warning(notification.message),
            ServerNotification::AuthRecoveryStarted(notification) => {
                eprintln!("{}", notification.message);
                CodexStatus::Running
            }
            ServerNotification::AuthRecoveryCompleted(notification) => {
                eprintln!("{}", notification.message.style(self.green));
                CodexStatus::Running
            }
            ServerNotification::Error(notification) => {
                eprintln!(
                    "{} {}",
                    "ERROR:".style(self.red).style(self.bold),
                    notification.error
                );
                CodexStatus::Running
            }
            ServerNotification::DeprecationNotice(notification) => {
                eprintln!(
                    "{} {}",
                    "deprecated:".style(self.yellow).style(self.bold),
                    notification.summary
                );
                if let Some(details) = notification.details {
                    eprintln!("{}", details.style(self.dimmed));
                }
                CodexStatus::Running
            }
            ServerNotification::HookStarted(notification) => {
                eprintln!(
                    "{} {}",
                    "hook:".style(self.bold),
                    format!("{:?}", notification.run.event_name).style(self.dimmed)
                );
                CodexStatus::Running
            }
            ServerNotification::HookCompleted(notification) => {
                eprintln!(
                    "{} {} {:?}",
                    "hook:".style(self.bold),
                    format!("{:?}", notification.run.event_name).style(self.dimmed),
                    notification.run.status
                );
                CodexStatus::Running
            }
            ServerNotification::ItemStarted(notification) => {
                self.render_item_started(&notification.item);
                CodexStatus::Running
            }
            ServerNotification::ItemCompleted(notification) => {
                self.render_item_completed(notification.item);
                CodexStatus::Running
            }
            ServerNotification::ModelRerouted(notification) => {
                eprintln!(
                    "{} {} -> {}",
                    "model rerouted:".style(self.yellow).style(self.bold),
                    notification.from_model,
                    notification.to_model
                );
                CodexStatus::Running
            }
            ServerNotification::ModelVerification(_) => CodexStatus::Running,
            // ByteAsk Context's turn-0 pack: one stderr line when it could not be added.
            ServerNotification::ContextPack(notification) => {
                if let Some(message) = notification.message {
                    eprintln!("{} {message}", "context:".style(self.dimmed));
                }
                CodexStatus::Running
            }
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                self.last_total_token_usage = Some(notification.token_usage);
                CodexStatus::Running
            }
            ServerNotification::TurnCompleted(notification) => match notification.turn.status {
                TurnStatus::Completed => {
                    let rendered_message = self
                        .final_message_rendered
                        .then(|| self.final_message.clone())
                        .flatten();
                    if let Some(final_message) =
                        final_message_from_turn_items(notification.turn.items.as_slice())
                    {
                        self.final_message_rendered =
                            rendered_message.as_deref() == Some(final_message.as_str());
                        self.final_message = Some(final_message);
                    }
                    self.emit_final_message_on_shutdown = true;
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::Failed => {
                    self.final_message = None;
                    self.final_message_rendered = false;
                    self.emit_final_message_on_shutdown = false;
                    if let Some(error) = notification.turn.error {
                        eprintln!("{} {}", "ERROR:".style(self.red).style(self.bold), error);
                    }
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::Interrupted => {
                    self.final_message = None;
                    self.final_message_rendered = false;
                    self.emit_final_message_on_shutdown = false;
                    eprintln!("{}", "turn interrupted".style(self.dimmed));
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::InProgress => CodexStatus::Running,
            },
            ServerNotification::TurnDiffUpdated(notification) => {
                if !notification.diff.trim().is_empty() {
                    eprintln!("{}", notification.diff);
                }
                CodexStatus::Running
            }
            ServerNotification::TurnPlanUpdated(notification) => {
                if let Some(explanation) = notification.explanation {
                    eprintln!("{}", explanation.style(self.italic));
                }
                for step in notification.plan {
                    match step.status {
                        codex_app_server_protocol::TurnPlanStepStatus::Completed => {
                            eprintln!("  {} {}", "✓".style(self.green), step.step);
                        }
                        codex_app_server_protocol::TurnPlanStepStatus::InProgress => {
                            eprintln!("  {} {}", "→".style(self.cyan), step.step);
                        }
                        codex_app_server_protocol::TurnPlanStepStatus::Pending => {
                            eprintln!(
                                "  {} {}",
                                "•".style(self.dimmed),
                                step.step.style(self.dimmed)
                            );
                        }
                    }
                }
                CodexStatus::Running
            }
            ServerNotification::TurnStarted(_) => CodexStatus::Running,
            _ => CodexStatus::Running,
        }
    }

    fn process_warning(&mut self, message: String) -> CodexStatus {
        eprintln!(
            "{} {message}",
            "warning:".style(self.yellow).style(self.bold)
        );
        CodexStatus::Running
    }

    fn print_final_output(&mut self) {
        if self.emit_final_message_on_shutdown
            && let Some(path) = self.last_message_path.as_deref()
        {
            handle_last_message(self.final_message.as_deref(), path);
        }

        if let Some(usage) = &self.last_total_token_usage {
            eprintln!(
                "{}\n{}",
                "tokens used".style(self.dimmed),
                format_with_separators(blended_total(usage))
            );
        }

        #[allow(clippy::print_stdout)]
        if should_print_final_message_to_stdout(
            self.emit_final_message_on_shutdown
                .then_some(self.final_message.as_deref())
                .flatten(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ) && let Some(message) = self.final_message.as_deref()
        {
            println!("{message}");
        } else if should_print_final_message_to_tty(
            self.emit_final_message_on_shutdown
                .then_some(self.final_message.as_deref())
                .flatten(),
            self.final_message_rendered,
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ) && let Some(message) = self.final_message.as_deref()
        {
            eprintln!(
                "{}\n{}",
                "ByteAsk".style(self.italic).style(self.magenta),
                message
            );
        }
    }
}

/// Whether the effective permission profile forbids writes outside the
/// workspace, i.e. whether the sandbox-denial hint applies.
fn sandbox_is_read_only(config: &Config) -> bool {
    summarize_permission_profile(
        &config.permissions.effective_permission_profile(),
        &config.cwd,
        config.effective_workspace_roots().as_slice(),
    ) == "read-only"
}

fn config_summary_entries(
    config: &Config,
    session_configured_event: &SessionConfiguredEvent,
) -> Vec<(&'static str, String)> {
    let permission_profile = config.permissions.effective_permission_profile();
    let mut entries = vec![
        ("workdir", config.cwd.display().to_string()),
        ("model", session_configured_event.model.clone()),
        (
            "provider",
            session_configured_event.model_provider_id.clone(),
        ),
        (
            "approval",
            config.permissions.approval_policy.value().to_string(),
        ),
        (
            "sandbox",
            summarize_permission_profile(
                &permission_profile,
                &config.cwd,
                config.effective_workspace_roots().as_slice(),
            ),
        ),
    ];
    if config.model_provider.wire_api == WireApi::Responses {
        entries.push((
            "reasoning effort",
            config
                .model_reasoning_effort
                .as_ref()
                .map(std::string::ToString::to_string)
                // An unset `model_reasoning_effort` means "not configured, the
                // provider picks" — not "reasoning disabled". Printing "none"
                // reads as a session fact, which it is not.
                .unwrap_or_else(|| "default (provider-side)".to_string()),
        ));
        entries.push((
            "reasoning summaries",
            config
                .model_reasoning_summary
                .map(|summary| summary.to_string())
                // Same unset-is-not-disabled distinction as `reasoning effort`
                // above: an absent value means "not configured, provider
                // decides", not "summaries are off". Printing a bare `none` got
                // published as a session fact in a public report.
                .unwrap_or_else(|| "default (provider-side)".to_string()),
        ));
    }
    entries.push((
        "session id",
        session_configured_event.session_id.to_string(),
    ));
    entries
}

fn reasoning_text(
    summary: &[String],
    content: &[String],
    show_raw_agent_reasoning: bool,
) -> Option<String> {
    let entries = if show_raw_agent_reasoning && !content.is_empty() {
        content
    } else {
        summary
    };
    if entries.is_empty() {
        None
    } else {
        Some(entries.join("\n"))
    }
}

fn final_message_from_turn_items(items: &[ThreadItem]) -> Option<String> {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .or_else(|| {
            items.iter().rev().find_map(|item| match item {
                ThreadItem::Plan { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
}

fn blended_total(usage: &ThreadTokenUsage) -> i64 {
    let cached_input = usage.total.cached_input_tokens.max(0);
    let non_cached_input = (usage.total.input_tokens - cached_input).max(0);
    (non_cached_input + usage.total.output_tokens.max(0)).max(0)
}

fn should_print_final_message_to_stdout(
    final_message: Option<&str>,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
) -> bool {
    final_message.is_some() && !(stdout_is_terminal && stderr_is_terminal)
}

fn should_print_final_message_to_tty(
    final_message: Option<&str>,
    final_message_rendered: bool,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
) -> bool {
    final_message.is_some() && !final_message_rendered && stdout_is_terminal && stderr_is_terminal
}

#[cfg(test)]
#[path = "event_processor_with_human_output_tests.rs"]
mod tests;
