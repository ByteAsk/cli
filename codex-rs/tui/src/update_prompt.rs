#![cfg(not(debug_assertions))]

use crate::key_hint;
use crate::legacy_core::config::Config;
use crate::render::Insets;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;
use crate::render::renderable::RenderableExt as _;
use crate::selection_list::selection_option_row;
use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crate::update_action::UpdateAction;
use crate::updates;
use color_eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::WidgetRef;
use tokio_stream::StreamExt;

const RELEASE_NOTES_URL: &str = "https://github.com/ByteAsk/cli/releases/latest";

pub(crate) enum UpdatePromptOutcome {
    Continue,
    RunUpdate(UpdateAction),
}

pub(crate) async fn run_update_prompt_if_needed(
    tui: &mut Tui,
    config: &Config,
) -> Result<UpdatePromptOutcome> {
    let Some(latest_version) = updates::get_upgrade_version_for_popup(config) else {
        return Ok(UpdatePromptOutcome::Continue);
    };
    // ByteAsk has one update command (`byteask --update`) for every install method, so
    // an undetected install context must not silently skip a required update.
    let update_action = crate::update_action::get_update_action().unwrap_or(if cfg!(windows) {
        UpdateAction::StandaloneWindows
    } else {
        UpdateAction::StandaloneUnix
    });

    let mut screen =
        UpdatePromptScreen::new(tui.frame_requester(), latest_version.clone(), update_action);
    tui.draw(u16::MAX, |frame| {
        frame.render_widget_ref(&screen, frame.area());
    })?;

    tui.discard_pending_input_before_interactive_screen()?;
    let events = tui.event_stream();
    tokio::pin!(events);

    while !screen.is_done() {
        if let Some(event) = events.next().await {
            tui.screen_size_for_event(&event)?;
            match event {
                TuiEvent::Key(key_event) => screen.handle_key(key_event),
                TuiEvent::Paste(_) | TuiEvent::FocusLost => {}
                TuiEvent::Draw | TuiEvent::Resume | TuiEvent::Resize(_) | TuiEvent::FocusGained => {
                    tui.draw(u16::MAX, |frame| {
                        frame.render_widget_ref(&screen, frame.area());
                    })?;
                }
            }
        } else {
            break;
        }
    }

    // ByteAsk: this prompt is only reached when the installed version is below the
    // feed's `min_supported`, so there is no "skip until next version" — skipping
    // lasts for this launch only.
    match screen.selection() {
        Some(UpdateSelection::UpdateNow) => {
            tui.terminal.clear()?;
            Ok(UpdatePromptOutcome::RunUpdate(update_action))
        }
        Some(UpdateSelection::NotNow) | None => Ok(UpdatePromptOutcome::Continue),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateSelection {
    UpdateNow,
    NotNow,
}

struct UpdatePromptScreen {
    request_frame: FrameRequester,
    latest_version: String,
    current_version: String,
    update_action: UpdateAction,
    highlighted: UpdateSelection,
    selection: Option<UpdateSelection>,
}

impl UpdatePromptScreen {
    fn new(
        request_frame: FrameRequester,
        latest_version: String,
        update_action: UpdateAction,
    ) -> Self {
        Self {
            request_frame,
            latest_version,
            current_version: crate::version::display_version().to_string(),
            update_action,
            highlighted: UpdateSelection::UpdateNow,
            selection: None,
        }
    }

    fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Release {
            return;
        }
        if key_event.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key_event.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.select(UpdateSelection::NotNow);
            return;
        }
        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => self.set_highlight(self.highlighted.prev()),
            KeyCode::Down | KeyCode::Char('j') => self.set_highlight(self.highlighted.next()),
            KeyCode::Char('1') => self.select(UpdateSelection::UpdateNow),
            KeyCode::Char('2') => self.select(UpdateSelection::NotNow),
            KeyCode::Enter => self.select(self.highlighted),
            KeyCode::Esc => self.select(UpdateSelection::NotNow),
            _ => {}
        }
    }

    fn set_highlight(&mut self, highlight: UpdateSelection) {
        if self.highlighted != highlight {
            self.highlighted = highlight;
            self.request_frame.schedule_frame();
        }
    }

    fn select(&mut self, selection: UpdateSelection) {
        self.highlighted = selection;
        self.selection = Some(selection);
        self.request_frame.schedule_frame();
    }

    fn is_done(&self) -> bool {
        self.selection.is_some()
    }

    fn selection(&self) -> Option<UpdateSelection> {
        self.selection
    }
}

impl UpdateSelection {
    fn next(self) -> Self {
        match self {
            UpdateSelection::UpdateNow => UpdateSelection::NotNow,
            UpdateSelection::NotNow => UpdateSelection::UpdateNow,
        }
    }

    fn prev(self) -> Self {
        self.next()
    }
}

impl WidgetRef for &UpdatePromptScreen {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let mut column = ColumnRenderable::new();

        let update_command = self.update_action.command_str();

        column.push("");
        column.push(Line::from(vec![
            "  \u{2728}\u{200A}".bold().fg(crate::style::accent_color()),
            "Update available!".bold(),
            " ".into(),
            format!(
                "{current} -> {latest}",
                current = self.current_version,
                latest = self.latest_version
            )
            .dim(),
        ]));
        column.push("");
        column.push(
            Line::from(vec![
                "Release notes: ".dim(),
                RELEASE_NOTES_URL.dim().underlined(),
            ])
            .inset(Insets::tlbr(0, 2, 0, 0)),
        );
        column.push("");
        column.push(selection_option_row(
            0,
            format!("Update now (runs `{update_command}`)"),
            self.highlighted == UpdateSelection::UpdateNow,
        ));
        column.push(selection_option_row(
            1,
            "Skip".to_string(),
            self.highlighted == UpdateSelection::NotNow,
        ));
        column.push("");
        column.push(
            Line::from(vec![
                "Press ".dim(),
                key_hint::plain(KeyCode::Enter).into(),
                " to continue".dim(),
            ])
            .inset(Insets::tlbr(0, 2, 0, 0)),
        );
        column.render(area, buf);
        crate::terminal_hyperlinks::mark_underlined_hyperlink(buf, area, RELEASE_NOTES_URL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use crate::tui::FrameRequester;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::widgets::FrameExt;

    fn new_prompt() -> UpdatePromptScreen {
        UpdatePromptScreen::new(
            FrameRequester::test_dummy(),
            "9.9.9".into(),
            UpdateAction::NpmGlobalLatest,
        )
    }

    #[test]
    fn update_prompt_snapshot() {
        let screen = new_prompt();
        let mut terminal = Terminal::new(VT100Backend::new(80, 12)).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget_ref(&screen, frame.area()))
            .expect("render update prompt");
        insta::assert_snapshot!("update_prompt_modal", terminal.backend());
    }

    #[test]
    fn update_prompt_confirm_selects_update() {
        let mut screen = new_prompt();
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(screen.is_done());
        assert_eq!(screen.selection(), Some(UpdateSelection::UpdateNow));
    }

    #[test]
    fn update_prompt_dismiss_option_leaves_prompt_in_normal_state() {
        let mut screen = new_prompt();
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(screen.is_done());
        assert_eq!(screen.selection(), Some(UpdateSelection::NotNow));
    }

    #[test]
    fn update_prompt_ctrl_c_skips_update() {
        let mut screen = new_prompt();
        screen.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(screen.is_done());
        assert_eq!(screen.selection(), Some(UpdateSelection::NotNow));
    }

    #[test]
    fn update_prompt_navigation_wraps_between_entries() {
        let mut screen = new_prompt();
        screen.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(screen.highlighted, UpdateSelection::NotNow);
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(screen.highlighted, UpdateSelection::UpdateNow);
    }
}
