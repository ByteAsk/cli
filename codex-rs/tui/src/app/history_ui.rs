//! Terminal history and clear-screen UI helpers for the TUI app.
//!
//! This module owns rendering the fresh session header, clearing inline or alternate-screen UI
//! state, and resetting transcript-related app state after `/clear` or Ctrl-L.

use super::*;
use crate::terminal_hyperlinks::HyperlinkLine;
use std::sync::Weak;

pub(super) struct RenderedHistoryTail {
    pub(super) cell: Weak<dyn HistoryCell>,
    pub(super) lines: Vec<HyperlinkLine>,
}

pub(super) struct ThreadUsageStatusHistory {
    thread_id: ThreadId,
    pub(super) cell: Weak<dyn HistoryCell>,
    pub(super) lines: Vec<HyperlinkLine>,
}

impl App {
    pub(super) fn insert_history_cell(&mut self, tui: &mut tui::Tui, cell: Box<dyn HistoryCell>) {
        let cell: Arc<dyn HistoryCell> = cell.into();
        if let Some(Overlay::Transcript(t)) = &mut self.overlay {
            t.insert_cell(cell.clone());
            tui.frame_requester().schedule_frame();
        }
        self.transcript_cells.push(cell.clone());
        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        let lines =
            cell.display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode());
        if cell.as_any().is::<history_cell::CompositeHistoryCell>()
            && lines.first().is_some_and(|line| {
                line.line.spans.len() == 1 && line.line.spans[0].content.as_ref() == "/status"
            })
            && let Some(thread_id) = self.chat_widget.thread_id()
        {
            self.last_thread_usage_status_cell = Some(ThreadUsageStatusHistory {
                thread_id,
                cell: Arc::downgrade(&cell),
                lines: lines.clone(),
            });
        }
        if self.initial_history_replay_buffer.as_ref().is_some() {
            self.insert_history_cell_lines_with_initial_replay_buffer(tui, cell.as_ref(), width);
            self.last_rendered_history_tail = None;
        } else {
            self.insert_history_cell_lines(tui, cell.as_ref(), width);
            self.last_rendered_history_tail = if self.overlay.is_none() && !lines.is_empty() {
                Some(RenderedHistoryTail {
                    cell: Arc::downgrade(&cell),
                    lines,
                })
            } else {
                None
            };
        }
        // A committed cell can unblock a settled /usage card that was waiting
        // behind a transient active cell or a provisional stream tail.
        self.chat_widget.request_pending_usage_output_insertion();
    }

    pub(super) fn finish_thread_usage_refresh(
        &mut self,
        tui: &mut tui::Tui,
        thread_id: ThreadId,
        request_id: u64,
        result: std::result::Result<crate::chatwidget::ThreadUsageOutcome, String>,
    ) -> Result<()> {
        let may_update_status = match &result {
            Ok(crate::chatwidget::ThreadUsageOutcome::Available(usage)) => {
                usage.thread_id == thread_id.to_string()
            }
            Ok(crate::chatwidget::ThreadUsageOutcome::Disabled) => true,
            Err(_) => false,
        };
        if !self
            .chat_widget
            .finish_thread_usage_refresh(thread_id, request_id, result)
            || !may_update_status
        {
            return Ok(());
        }

        if self.overlay.is_some() || self.initial_history_replay_buffer.is_some() {
            self.pending_thread_usage_history_refresh = true;
            return Ok(());
        }
        self.refresh_thread_usage_history_tail(tui)
    }

    pub(crate) fn refresh_thread_usage_history_tail(&mut self, tui: &mut tui::Tui) -> Result<()> {
        let Some(status_history) = self.last_thread_usage_status_cell.as_ref() else {
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        };
        if self.chat_widget.thread_id() != Some(status_history.thread_id) {
            self.last_thread_usage_status_cell = None;
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        }
        let Some(status_cell) = status_history.cell.upgrade() else {
            self.last_thread_usage_status_cell = None;
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        };

        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        let updated_lines = status_cell
            .display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode());
        if updated_lines == status_history.lines {
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        }
        let Some(rendered_tail) = self.last_rendered_history_tail.as_ref() else {
            self.insert_history_cell_lines(tui, status_cell.as_ref(), width);
            self.last_rendered_history_tail = Some(RenderedHistoryTail {
                cell: Arc::downgrade(&status_cell),
                lines: updated_lines.clone(),
            });
            if let Some(status_history) = self.last_thread_usage_status_cell.as_mut() {
                status_history.lines = updated_lines;
            }
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        };
        if !rendered_tail
            .cell
            .upgrade()
            .is_some_and(|cell| Arc::ptr_eq(&cell, &status_cell))
        {
            self.insert_history_cell_lines(tui, status_cell.as_ref(), width);
            self.last_rendered_history_tail = Some(RenderedHistoryTail {
                cell: Arc::downgrade(&status_cell),
                lines: updated_lines.clone(),
            });
            if let Some(status_history) = self.last_thread_usage_status_cell.as_mut() {
                status_history.lines = updated_lines;
            }
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        }

        let prefix_len = rendered_tail
            .lines
            .iter()
            .zip(&updated_lines)
            .take_while(|(previous, updated)| previous == updated)
            .count();
        if prefix_len == rendered_tail.lines.len() && prefix_len == updated_lines.len() {
            self.pending_thread_usage_history_refresh = false;
            return Ok(());
        }

        let wrap_policy = self.history_line_wrap_policy();
        if !tui.replace_visible_history_tail(
            &rendered_tail.lines[prefix_len..],
            &updated_lines[prefix_len..],
            wrap_policy,
        )? {
            self.insert_history_cell_lines(tui, status_cell.as_ref(), width);
        }
        self.last_rendered_history_tail = Some(RenderedHistoryTail {
            cell: Arc::downgrade(&status_cell),
            lines: updated_lines.clone(),
        });
        if let Some(status_history) = self.last_thread_usage_status_cell.as_mut() {
            status_history.lines = updated_lines;
        }
        self.pending_thread_usage_history_refresh = false;
        Ok(())
    }

    pub(super) fn pending_usage_output_insertion_blocked(&self) -> bool {
        self.chat_widget.usage_history_insertion_blocked()
            || self
                .transcript_cells
                .last()
                .is_some_and(|cell| cell.as_any().is::<history_cell::AgentMessageCell>())
    }

    fn insert_pending_usage_output(&mut self, tui: &mut tui::Tui) {
        if let Some(cell) = self.chat_widget.take_completed_token_activity_output() {
            self.insert_history_cell(tui, Box::new(cell));
        }
        if let Some(cell) = self.chat_widget.take_pending_rate_limit_reset_hint() {
            self.insert_history_cell(tui, Box::new(cell));
        }
    }

    pub(super) fn insert_pending_usage_output_if_ready(&mut self, tui: &mut tui::Tui) {
        if self.pending_usage_output_insertion_blocked() {
            return;
        }
        self.insert_pending_usage_output(tui);
    }

    pub(super) fn insert_pending_usage_output_after_stream_shutdown(&mut self, tui: &mut tui::Tui) {
        if self.chat_widget.usage_history_insertion_blocked() {
            return;
        }
        self.insert_pending_usage_output(tui);
    }

    pub(super) fn open_url_in_browser(&mut self, url: String) {
        if let Err(err) = webbrowser::open(&url) {
            self.chat_widget
                .add_error_message(format!("Failed to open browser for {url}: {err}"));
            return;
        }

        self.chat_widget
            .add_info_message(format!("Opened {url} in your browser."), /*hint*/ None);
    }

    pub(super) fn clear_ui_header_lines_with_version(
        &self,
        width: u16,
        version: &'static str,
    ) -> Vec<Line<'static>> {
        history_cell::SessionHeaderHistoryCell::new(
            self.chat_widget.current_model().to_string(),
            self.chat_widget.current_reasoning_effort(),
            self.chat_widget.should_show_fast_status(
                self.chat_widget.current_model(),
                self.chat_widget.current_service_tier(),
            ),
            self.config.cwd.to_path_buf(),
            version,
        )
        .with_account(history_cell::byteask_account_email(&self.config))
        .with_yolo_mode(history_cell::is_yolo_mode(&self.config))
        .display_lines(width)
    }

    pub(super) fn clear_ui_header_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.clear_ui_header_lines_with_version(width, crate::version::display_version())
    }

    pub(super) fn queue_clear_ui_header(&mut self, tui: &mut tui::Tui) {
        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        let header_lines = self.clear_ui_header_lines(width);
        if !header_lines.is_empty() {
            tui.insert_history_lines(header_lines);
            self.has_emitted_history_lines = true;
        }
    }

    pub(super) fn clear_terminal_ui(
        &mut self,
        tui: &mut tui::Tui,
        redraw_header: bool,
    ) -> Result<()> {
        let is_alt_screen_active = tui.is_alt_screen_active();

        // Drop queued history insertions so stale transcript lines cannot be flushed after /clear.
        tui.clear_pending_history_lines();

        if is_alt_screen_active {
            tui.terminal.clear_visible_screen()?;
        } else {
            // Some terminals (Terminal.app, Warp) do not reliably drop scrollback when purge and
            // clear are emitted as separate backend commands. Prefer a single ANSI sequence.
            tui.terminal.clear_scrollback_and_visible_screen_ansi()?;
        }

        let mut area = tui.terminal.viewport_area;
        if area.y > 0 {
            // After a full clear, anchor the inline viewport at the top and redraw a fresh header
            // box. `insert_history_lines()` will shift the viewport down by the rendered height.
            area.y = 0;
            tui.terminal.set_viewport_area(area);
        }
        self.has_emitted_history_lines = false;

        if redraw_header {
            self.queue_clear_ui_header(tui);
        }
        Ok(())
    }

    pub(super) fn reset_app_ui_state_after_clear(&mut self) {
        self.reset_transcript_state_after_clear();
    }

    pub(super) fn reset_transcript_state_after_clear(&mut self) {
        self.overlay = None;
        self.transcript_cells.clear();
        self.last_rendered_history_tail = None;
        self.last_thread_usage_status_cell = None;
        self.pending_thread_usage_history_refresh = false;
        self.deferred_history_lines.clear();
        self.has_emitted_history_lines = false;
        self.transcript_reflow.clear();
        self.chat_widget.clear_pending_token_activity_refreshes();
        self.chat_widget.clear_pending_rate_limit_reset_hint();
        self.initial_history_replay_buffer = None;
        self.scrollback_has_older_history = false;
        self.backtrack = BacktrackState::default();
        self.backtrack_render_pending = false;
        self.skill_load_warnings.clear();
    }
}
