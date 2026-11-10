//! Session headers, onboarding guidance, and transcript cards.

use super::*;
use crate::line_truncation::line_width;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::width::display_width;

pub(crate) const SESSION_HEADER_MAX_INNER_WIDTH: usize = 56; // Just an eyeballed value

pub(crate) fn card_inner_width(width: u16, max_inner_width: usize) -> Option<usize> {
    if width < 4 {
        return None;
    }
    let inner_width = std::cmp::min(width.saturating_sub(4) as usize, max_inner_width);
    Some(inner_width)
}

/// Render `lines` inside a border sized to the widest span in the content.
pub(crate) fn with_border(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    with_border_internal(lines, /*forced_inner_width*/ None)
}

/// Render `lines` inside a border whose inner width is at least `inner_width`.
///
/// This is useful when callers have already clamped their content to a
/// specific width and want the border math centralized here instead of
/// duplicating padding logic in the TUI widgets themselves.
pub(crate) fn with_border_with_inner_width(
    lines: Vec<Line<'static>>,
    inner_width: usize,
) -> Vec<Line<'static>> {
    with_border_internal(lines, Some(inner_width))
}

fn with_border_internal(
    lines: Vec<Line<'static>>,
    forced_inner_width: Option<usize>,
) -> Vec<Line<'static>> {
    let max_line_width = lines.iter().map(line_width).max().unwrap_or(0);
    let content_width = forced_inner_width
        .unwrap_or(max_line_width)
        .max(max_line_width);

    let mut out = Vec::with_capacity(lines.len() + 2);
    let border_inner_width = content_width + 2;
    out.push(vec![format!("╭{}╮", "─".repeat(border_inner_width)).dim()].into());

    for line in lines.into_iter() {
        let used_width = line_width(&line);
        let span_count = line.spans.len();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(span_count + 4);
        spans.push(Span::from("│ ").dim());
        spans.extend(line);
        if used_width < content_width {
            spans.push(Span::from(" ".repeat(content_width - used_width)).dim());
        }
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }

    out.push(vec![format!("╰{}╯", "─".repeat(border_inner_width)).dim()].into());

    out
}

#[derive(Debug)]
struct TooltipHistoryCell {
    tip: String,
    cwd: PathBuf,
}

impl TooltipHistoryCell {
    fn new(tip: String, cwd: &Path) -> Self {
        Self {
            tip,
            cwd: cwd.to_path_buf(),
        }
    }
}

impl HistoryCell for TooltipHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let indent = "  ";
        let indent_width = display_width(indent);
        let wrap_width = usize::from(width.max(1))
            .saturating_sub(indent_width)
            .max(1);
        let mut lines: Vec<Line<'static>> = Vec::new();
        // `tip` may be several newline-separated tips (byteask multi-tip startup
        // block). Render each as its own markdown block; only the first is prefixed
        // with "Tip:" so they stack as a clean list.
        let mut first = true;
        for tip in self.tip.split('\n') {
            let tip = tip.trim();
            if tip.is_empty() {
                continue;
            }
            let md = if first {
                format!("**Tip:** {tip}")
            } else {
                tip.to_string()
            };
            first = false;
            append_markdown(&md, Some(wrap_width), Some(self.cwd.as_path()), &mut lines);
        }

        prefix_lines(lines, indent.into(), indent.into())
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut first = true;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for tip in self.tip.split('\n') {
            let tip = tip.trim();
            if tip.is_empty() {
                continue;
            }
            lines.push(Line::from(if first {
                format!("Tip: {tip}")
            } else {
                tip.to_string()
            }));
            first = false;
        }
        lines
    }
}

#[derive(Debug)]
pub struct SessionInfoCell(CompositeHistoryCell);

impl HistoryCell for SessionInfoCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.display_lines(width)
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.transcript_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.0.raw_lines()
    }
}

/// The signed-in account email for a mode-C gateway session: the `sub` claim of
/// the per-user bearer JWT in `experimental_bearer_token`. The payload is decoded
/// but NOT verified (display-only; it is the user's own token). Returns None for
/// non-gateway sessions (BYOK / ChatGPT / signed-out) or an undecodable token, in
/// which case the header simply omits the account row.
pub(crate) fn byteask_account_email(config: &Config) -> Option<String> {
    use base64::Engine as _;
    let token = config
        .model_provider
        .experimental_bearer_token
        .as_deref()
        .map(|token| token.trim())
        .filter(|s| !s.is_empty())?;
    let payload_b64 = token.split('.').nth(1)?; // header.PAYLOAD.signature
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let sub = claims.get("sub")?.as_str()?.trim().to_string();
    (!sub.is_empty()).then_some(sub)
}

pub(crate) fn new_session_info(
    config: &Config,
    requested_model: &str,
    session: &ThreadSessionState,
    is_first_event: bool,
    tooltip_override: Option<String>,
    auth_plan: Option<PlanType>,
    show_fast_status: bool,
) -> SessionInfoCell {
    // Header box rendered as history (so it appears at the very top)
    let header = SessionHeaderHistoryCell::new(
        session.model.clone(),
        session.reasoning_effort.clone(),
        show_fast_status,
        config.cwd.to_path_buf(),
        crate::version::display_version(),
    )
    .with_account(byteask_account_email(config))
    .with_yolo_mode(has_yolo_permissions(
        session.approval_policy,
        &session.permission_profile,
    ));
    let mut parts: Vec<Box<dyn HistoryCell>> = vec![Box::new(header)];

    if is_first_event {
        // Help lines below the header (new copy and list)
        let help_lines: Vec<Line<'static>> = vec![
            "  To get started, describe a task or try one of these commands:"
                .dim()
                .into(),
            Line::from(""),
            Line::from(vec![
                "  ".into(),
                "/init".into(),
                " - create an AGENTS.md file with instructions for ByteAsk".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/status".into(),
                " - show current session configuration".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/permissions".into(),
                " - choose what ByteAsk is allowed to do".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/model".into(),
                " - choose what model and reasoning effort to use".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/review".into(),
                " - review any changes and find issues".dim(),
            ]),
        ];

        parts.push(Box::new(PlainHistoryCell { lines: help_lines }));
    } else {
        if config.show_tooltips
            && let Some(tooltips) = tooltip_override
                .filter(|s| !s.trim().is_empty()) // ignore empty model-NUX so the byteask tip shows
                .or_else(|| tooltips::get_tooltip(auth_plan, show_fast_status))
                .filter(|s| !s.trim().is_empty())
                .map(|tip| TooltipHistoryCell::new(tip, &config.cwd))
        {
            parts.push(Box::new(tooltips));
        }
        if requested_model != session.model.as_str() {
            let lines = vec![
                "model changed:".magenta().bold().into(),
                format!("requested: {requested_model}").into(),
                format!("used: {}", session.model).into(),
            ];
            parts.push(Box::new(PlainHistoryCell { lines }));
        }
    }

    SessionInfoCell(CompositeHistoryCell { parts })
}

pub(crate) fn is_yolo_mode(config: &Config) -> bool {
    has_yolo_permissions(
        AskForApproval::from(config.permissions.approval_policy.value()),
        &config.permissions.effective_permission_profile(),
    )
}

pub(crate) fn has_yolo_permissions(
    approval_policy: AskForApproval,
    permission_profile: &PermissionProfile,
) -> bool {
    approval_policy == AskForApproval::Never
        && matches!(
            permission_profile,
            PermissionProfile::Disabled
                | PermissionProfile::Managed {
                    file_system: ManagedFileSystemPermissions::Unrestricted,
                    network: NetworkSandboxPolicy::Enabled,
                }
        )
}
#[derive(Debug)]
pub(crate) struct SessionHeaderHistoryCell {
    version: &'static str,
    model: String,
    model_style: Style,
    reasoning_effort: Option<ReasoningEffortConfig>,
    show_fast_status: bool,
    directory: PathBuf,
    account: Option<String>,
    yolo_mode: bool,
}

impl SessionHeaderHistoryCell {
    pub(crate) fn new(
        model: String,
        reasoning_effort: Option<ReasoningEffortConfig>,
        show_fast_status: bool,
        directory: PathBuf,
        version: &'static str,
    ) -> Self {
        Self::new_with_style(
            model,
            Style::default(),
            reasoning_effort,
            show_fast_status,
            directory,
            version,
        )
    }

    pub(crate) fn new_with_style(
        model: String,
        model_style: Style,
        reasoning_effort: Option<ReasoningEffortConfig>,
        show_fast_status: bool,
        directory: PathBuf,
        version: &'static str,
    ) -> Self {
        Self {
            version,
            model,
            model_style,
            reasoning_effort,
            show_fast_status,
            directory,
            account: None,
            yolo_mode: false,
        }
    }

    pub(crate) fn with_yolo_mode(mut self, yolo_mode: bool) -> Self {
        self.yolo_mode = yolo_mode;
        self
    }

    /// Attach the signed-in account email (mode-C gateway sessions). `None` for
    /// BYOK / ChatGPT / signed-out sessions — the account row is then omitted.
    pub(crate) fn with_account(mut self, account: Option<String>) -> Self {
        self.account = account.filter(|s| !s.trim().is_empty());
        self
    }

    fn format_directory(&self, max_width: Option<usize>) -> String {
        Self::format_directory_inner(&self.directory, max_width)
    }

    pub(crate) fn format_directory_inner(directory: &Path, max_width: Option<usize>) -> String {
        let formatted = if let Some(rel) = relativize_to_home(directory) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display())
            }
        } else {
            directory.display().to_string()
        };

        if let Some(max_width) = max_width {
            if max_width == 0 {
                return String::new();
            }
            if display_width(formatted.as_str()) > max_width {
                return crate::text_formatting::center_truncate_path(&formatted, max_width);
            }
        }

        formatted
    }

    fn reasoning_label(&self) -> Option<&str> {
        self.reasoning_effort
            .as_ref()
            .map(ReasoningEffortConfig::as_str)
    }
}

impl HistoryCell for SessionHeaderHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(inner_width) = card_inner_width(width, SESSION_HEADER_MAX_INNER_WIDTH) else {
            return Vec::new();
        };

        let make_row = |spans: Vec<Span<'static>>| Line::from(spans);

        // Title line rendered inside the box: ">_ OpenAI Codex (vX)"
        let title_spans: Vec<Span<'static>> = vec![
            Span::from(">_ ").dim(),
            Span::from(std::env::var("BYTEASK_BRAND").or_else(|_| std::env::var("CODEX_BRAND")).unwrap_or_else(|_| "ByteAsk".to_string())).bold(),
            Span::from(" ").dim(),
            Span::from(format!("(v{})", self.version)).dim(),
        ];

        const CHANGE_MODEL_HINT_COMMAND: &str = "/model";
        const CHANGE_MODEL_HINT_EXPLANATION: &str = " to change";
        const ACCOUNT_LABEL: &str = "account:";
        const DIR_LABEL: &str = "directory:";
        const PERMISSIONS_LABEL: &str = "permissions:";
        let label_width = if self.yolo_mode {
            DIR_LABEL.len().max(PERMISSIONS_LABEL.len())
        } else {
            DIR_LABEL.len()
        };

        let model_label = format!(
            "{model_label:<label_width$}",
            model_label = "model:",
            label_width = label_width
        );
        let reasoning_label = self.reasoning_label();
        let model_spans: Vec<Span<'static>> = {
            let mut spans = vec![
                Span::from(format!("{model_label} ")).dim(),
                Span::styled(self.model.clone(), self.model_style),
            ];
            if let Some(reasoning) = reasoning_label {
                spans.push(Span::from(" "));
                spans.push(Span::from(reasoning.to_owned()));
            }
            if self.show_fast_status {
                spans.push("   ".into());
                spans.push(Span::styled("fast", self.model_style.magenta()));
            }
            spans.push("   ".dim());
            spans.push(CHANGE_MODEL_HINT_COMMAND.fg(crate::style::accent_color()));
            spans.push(CHANGE_MODEL_HINT_EXPLANATION.dim());
            spans
        };

        let dir_label = format!("{DIR_LABEL:<label_width$}");
        let dir_prefix = format!("{dir_label} ");
        let dir_prefix_width = display_width(dir_prefix.as_str());
        let dir_max_width = inner_width.saturating_sub(dir_prefix_width);
        let dir = self.format_directory(Some(dir_max_width));
        let dir_spans = vec![Span::from(dir_prefix).dim(), Span::from(dir)];

        let mut lines = vec![make_row(title_spans), make_row(Vec::new())];

        // Signed-in account (mode-C gateway sessions) — the first data row, above
        // model/directory. Omitted when not signed in to a gateway account.
        if let Some(email) = &self.account {
            let acct_label = format!("{ACCOUNT_LABEL:<label_width$}");
            let acct_prefix = format!("{acct_label} ");
            let acct_max_width = inner_width.saturating_sub(display_width(acct_prefix.as_str()));
            let shown = crate::text_formatting::truncate_text(email, acct_max_width);
            lines.push(make_row(vec![
                Span::from(acct_prefix).dim(),
                Span::from(shown),
            ]));
        }

        lines.push(make_row(model_spans));
        lines.push(make_row(dir_spans));

        if self.yolo_mode {
            let permissions_label = format!("{PERMISSIONS_LABEL:<label_width$}");
            lines.push(make_row(vec![
                Span::from(format!("{permissions_label} ")).dim(),
                "YOLO mode".magenta().bold(),
            ]));
        }

        let lines = lines
            .into_iter()
            .map(|line| truncate_line_with_ellipsis_if_overflow(line, inner_width))
            .collect();
        with_border(lines)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(format!("{} (v{})", std::env::var("BYTEASK_BRAND").or_else(|_| std::env::var("CODEX_BRAND")).unwrap_or_else(|_| "ByteAsk".to_string()), self.version)),
        ];
        if let Some(email) = &self.account {
            lines.push(Line::from(format!("account: {email}")));
        }
        lines.extend([
            Line::from(format!(
                "model: {}{}",
                self.model,
                self.reasoning_label()
                    .map(|reasoning| format!(" {reasoning}"))
                    .unwrap_or_default()
            )),
            Line::from(format!(
                "directory: {}",
                self.format_directory(/*max_width*/ None)
            )),
        ]);
        if self.yolo_mode {
            lines.push(Line::from("permissions: YOLO mode"));
        }
        lines
    }
}
