//! `InfoPaneView` — the one bottom-pane vehicle every ByteAsk readout and form
//! uses (`/usage`, `/referral-code`, `/upgrade`, `/voucher`).
//!
//! Why it exists: those four commands used to render as **history cells**, i.e.
//! lines written into the terminal's real scrollback, which can never be
//! dismissed or re-rendered. Account chrome (plan, quota, referral, billing) is
//! not part of the conversation, so it belongs in a pane that `esc` closes —
//! the mechanism upstream already uses for `/model`, `/skills` and `/feedback`.
//!
//! Anatomy: upstream's tinted [`render_menu_surface`], **no
//! gutter glyph** — `▌ ` appears only on a row the user types into. Title row
//! (bold) with a dim right-aligned tag · body ≤ 8 rows at a 2-space inset ·
//! blank · dim footer hint. The title and footer rows are mandatory in every
//! state including loading: when the terminal background is unknown the tint is
//! absent and they are the pane's only boundary.
//!
//! Keys: `esc` closes, `ctrl + c` closes, **`enter` never
//! closes** — it performs only the action the footer names, `r` retries a
//! failed request when no text field has focus, and every other key is
//! swallowed rather than forwarded to the composer.
//!
//! The view is a pure render of a [`InfoPaneSpec`]; async results replace the
//! whole view (see `ChatWidget::update_byteask_pane`), so no interaction state
//! has to survive a network round trip.

use std::cell::RefCell;
use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::Widget;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::byteask_a11y::glyph;
use crate::key_hint::has_ctrl_or_alt;
use crate::keymap::KeymapContextSet;
use crate::keymap::RuntimeKeymap;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::bottom_pane_view::ViewCompletion;
use super::selection_popup_common::menu_surface_padding_height;
use super::selection_popup_common::render_menu_surface;
use super::textarea::TextArea;
use super::textarea::TextAreaState;

/// Narrower than this and the pane renders one line instead of its content.
/// State is kept, so typed text is never lost to a resize.
const MIN_PANE_WIDTH: u16 = 40;
/// At or above this width the footer keeps all of its segments; below it the
/// middle segments drop and `esc to close` survives.
const FULL_FOOTER_WIDTH: u16 = 60;
/// Loading text only appears after this long, so a fast gateway never flashes.
const LOADING_DELAY: Duration = Duration::from_millis(150);
/// Body rows are inset two columns from the title.
const BODY_INSET: u16 = 2;
/// A voucher code never exceeds this.
const FIELD_MAX_CHARS: usize = 32;

/// The four ByteAsk commands that render as an info pane. The `view_id` is what
/// routes an async result back to an open pane — and what makes a result for a
/// closed pane droppable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByteAskPane {
    Usage,
    Referral,
    Upgrade,
    Voucher,
    /// The ChatGPT-subscription sign-in: requesting a code, showing the code, and
    /// the refusal screen. A pane rather than shell output because a subprocess
    /// writes into scrollback, where nothing can clean it up afterwards.
    Subscription,
}

impl ByteAskPane {
    pub(crate) fn view_id(self) -> &'static str {
        match self {
            Self::Usage => "byteask_usage",
            Self::Referral => "byteask_referral",
            Self::Upgrade => "byteask_upgrade",
            Self::Subscription => "byteask_subscription",
            Self::Voucher => "byteask_voucher",
        }
    }

    /// True for any id this module owns. `BottomPane::push_view` uses it to keep
    /// info panes from stacking.
    pub(crate) fn is_info_pane_id(view_id: &str) -> bool {
        matches!(
            view_id,
            "byteask_usage" | "byteask_referral" | "byteask_upgrade" | "byteask_voucher"
        )
    }
}

/// What `enter` does. A pane with [`Primary::None`] does not mention `enter` in
/// its footer — a user trained by `/usage` must not create payment links by
/// reflex on `/upgrade`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Primary {
    None,
    /// Copy a literal string (never the rendered row) to the clipboard.
    Copy(String),
    /// Submit the currently selected [`BodyRow::Choice`].
    Choose,
    /// Submit the text field's contents.
    Submit,
}

/// A key action taken inside an info pane, handled by `ChatWidget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneAction {
    Copy(String),
    Choose(String),
    Submit(String),
    Retry,
}

/// One rendered row of a pane body.
#[derive(Debug, Clone)]
pub(crate) enum BodyRow {
    Blank,
    /// Plain content at the 2-space body inset.
    Text(Line<'static>),
    /// Shimmered while animations are on, static text otherwise.
    Loading(String),
    /// A selectable row; the cursor renders as `› ` in the two columns the body
    /// inset would otherwise occupy, so labels still line up with `Text` rows.
    Choice {
        id: String,
        line: Line<'static>,
        selectable: bool,
    },
    /// The `▌ ` row the user types into (the `CustomPromptView` idiom).
    Field,
}

/// Everything needed to draw one state of one pane. Built by
/// `chatwidget::byteask_account`, which owns the copy.
///
/// `Clone` is load-bearing: `BottomPane` keeps the open pane's spec so a
/// clipboard result can retint the footer without re-running the request that
/// produced the pane.
#[derive(Clone)]
pub(crate) struct InfoPaneSpec {
    pub(crate) pane: ByteAskPane,
    pub(crate) title: Vec<Span<'static>>,
    pub(crate) tag: String,
    /// The readouts put a blank line under the title; the voucher form does not.
    pub(crate) blank_after_title: bool,
    pub(crate) body: Vec<BodyRow>,
    /// Footer segments, joined with ` · `. `esc to close` is always last.
    pub(crate) footer: Vec<String>,
    /// Minimum body height, so a loading pane reserves the height its result
    /// will need and the composer does not jump twice.
    pub(crate) reserve_rows: u16,
    /// Initial text for the [`BodyRow::Field`], when the spec has one.
    pub(crate) field: Option<String>,
    pub(crate) primary: Primary,
    /// Binds `r`. Never set on a spec that has a focused field.
    pub(crate) retry: bool,
    pub(crate) animations: bool,
    /// Which [`BodyRow::Choice`] the cursor starts on (index among selectable
    /// choices, not among body rows).
    pub(crate) cursor: usize,
}

impl InfoPaneSpec {
    pub(crate) fn new(pane: ByteAskPane, title: Vec<Span<'static>>) -> Self {
        Self {
            pane,
            title,
            tag: String::new(),
            blank_after_title: true,
            body: Vec::new(),
            footer: vec![close_hint()],
            reserve_rows: 0,
            field: None,
            primary: Primary::None,
            retry: false,
            animations: true,
            cursor: 0,
        }
    }
}

pub(crate) fn close_hint() -> String {
    "esc to close".to_string()
}

pub(crate) struct InfoPaneView {
    spec: InfoPaneSpec,
    /// Present exactly when the spec asked for a field.
    field: Option<TextArea>,
    field_state: RefCell<TextAreaState>,
    /// Indices into `spec.body` of the selectable choices, in row order.
    choice_rows: Vec<usize>,
    cursor: usize,
    completion: Option<ViewCompletion>,
    opened_at: Instant,
    /// A transient footer (`✓ Copied`) and what to show once it expires.
    footer_revert: Option<(Instant, Vec<String>)>,
    tx: AppEventSender,
}

impl InfoPaneView {
    pub(crate) fn new(spec: InfoPaneSpec, tx: AppEventSender) -> Self {
        let field = spec.field.as_ref().map(|initial| {
            let mut textarea = TextArea::new();
            if !initial.is_empty() {
                textarea.set_text_clearing_elements(initial);
                textarea.set_cursor(initial.len());
            }
            textarea
        });
        let choice_rows: Vec<usize> = spec
            .body
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                BodyRow::Choice { selectable, .. } if *selectable => Some(index),
                _ => None,
            })
            .collect();
        let cursor = spec.cursor.min(choice_rows.len().saturating_sub(1));
        Self {
            spec,
            field,
            field_state: RefCell::new(TextAreaState::default()),
            choice_rows,
            cursor,
            completion: None,
            opened_at: Instant::now(),
            footer_revert: None,
            tx,
        }
    }

    pub(crate) fn pane(&self) -> ByteAskPane {
        self.spec.pane
    }

    /// Apply the composer's editing preferences to the text field, exactly as
    /// `BottomPane::show_text_prompt` does for `CustomPromptView`.
    pub(crate) fn apply_editor_prefs(&mut self, keymap: &RuntimeKeymap, vim_enabled: bool) {
        if let Some(field) = self.field.as_mut() {
            field.set_keymap_bindings(keymap);
            if vim_enabled {
                field.set_vim_enabled(/*enabled*/ true);
                field.enter_vim_insert_mode();
            }
        }
    }

    /// Show a transient footer for `duration`, then fall back to `after`.
    pub(crate) fn set_transient_footer(
        &mut self,
        footer: Vec<String>,
        duration: Duration,
        after: Vec<String>,
    ) {
        self.footer_revert = Instant::now().checked_add(duration).map(|at| (at, after));
        self.spec.footer = footer;
    }

    fn has_focused_field(&self) -> bool {
        self.field.is_some()
    }

    #[cfg(test)]
    pub(crate) fn field_text(&self) -> String {
        self.field
            .as_ref()
            .map(|field| field.text().to_string())
            .unwrap_or_default()
    }

    /// Pretend the pane has been open for `elapsed`, so the 150 ms loading delay
    /// can be tested without sleeping.
    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self, elapsed: Duration) {
        if let Some(earlier) = self.opened_at.checked_sub(elapsed) {
            self.opened_at = earlier;
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_transient_footer_for_test(&mut self) {
        if let Some((at, _)) = self.footer_revert.as_mut()
            && let Some(earlier) = at.checked_sub(Duration::from_secs(60))
        {
            *at = earlier;
        }
    }

    /// A voucher code is at most 32 characters. The cap is applied after
    /// every edit rather than per keystroke so a paste is bounded too.
    fn clamp_field(&mut self) {
        let Some(field) = self.field.as_mut() else {
            return;
        };
        let text = field.text();
        if text.chars().count() <= FIELD_MAX_CHARS {
            return;
        }
        let trimmed: String = text.chars().take(FIELD_MAX_CHARS).collect();
        field.set_text_clearing_elements(&trimmed);
        field.set_cursor(trimmed.len());
    }

    fn footer_now(&self) -> &[String] {
        match &self.footer_revert {
            Some((at, after)) if Instant::now() >= *at => after,
            _ => &self.spec.footer,
        }
    }

    fn send(&self, action: PaneAction) {
        self.tx.send(AppEvent::ByteAskPaneAction {
            pane: self.spec.pane,
            action,
        });
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.choice_rows.len() < 2 {
            return;
        }
        let len = self.choice_rows.len() as isize;
        let next = (self.cursor as isize + delta).rem_euclid(len);
        self.cursor = next as usize;
    }

    /// `enter` performs the footer's action and never closes the pane.
    fn activate_primary(&mut self) {
        match &self.spec.primary {
            Primary::None => {}
            Primary::Copy(text) => self.send(PaneAction::Copy(text.clone())),
            Primary::Choose => {
                if let Some(id) = self.selected_choice_id() {
                    self.send(PaneAction::Choose(id));
                }
            }
            Primary::Submit => {
                let text = self
                    .field
                    .as_ref()
                    .map(|field| field.text().trim().to_string())
                    .unwrap_or_default();
                // An empty submit does nothing and keeps focus.
                if !text.is_empty() {
                    self.send(PaneAction::Submit(text));
                }
            }
        }
    }

    fn selected_choice_id(&self) -> Option<String> {
        let row = *self.choice_rows.get(self.cursor)?;
        match self.spec.body.get(row) {
            Some(BodyRow::Choice { id, .. }) => Some(id.clone()),
            _ => None,
        }
    }

    /// Body rows to draw, padded to `reserve_rows` so the pane keeps its height
    /// between the loading state and the result.
    fn body_height(&self) -> u16 {
        let rows = self.spec.body.len() as u16;
        rows.max(self.spec.reserve_rows)
    }

    fn content_height(&self) -> u16 {
        1 // title
            + u16::from(self.spec.blank_after_title)
            + self.body_height()
            + 1 // blank
            + 1 // footer
    }

    fn is_loading(&self) -> bool {
        self.spec
            .body
            .iter()
            .any(|row| matches!(row, BodyRow::Loading(_)))
    }

    fn loading_visible(&self) -> bool {
        self.opened_at.elapsed() >= LOADING_DELAY
    }

    fn title_line(&self, width: u16) -> Line<'static> {
        let title_width: usize = self.spec.title.iter().map(Span::width).sum();
        let mut spans = self.spec.title.clone();
        if !self.spec.tag.is_empty() {
            let tag = self.spec.tag.clone();
            let tag_width = tag.chars().count();
            // One space minimum between the title and its tag; drop the tag
            // rather than let it collide.
            if title_width + 1 + tag_width <= width as usize {
                let gap = width as usize - title_width - tag_width;
                spans.push(" ".repeat(gap).into());
                spans.push(tag.dim());
            }
        }
        truncate_line_with_ellipsis_if_overflow(Line::from(spans), width as usize)
    }

    fn footer_line(&self, width: u16) -> Line<'static> {
        let mut segments: Vec<String> = self.footer_now().to_vec();
        if width < FULL_FOOTER_WIDTH {
            // Drop segments from the middle; `esc to close` is last and stays.
            while segments.len() > 1 && joined_width(&segments) > width as usize {
                segments.remove(segments.len().saturating_sub(2));
            }
        }
        let text = segments.join(&format!(" {} ", glyph("·")));
        truncate_line_with_ellipsis_if_overflow(Line::from(text.dim()), width as usize)
    }

    /// Rows the body renders at `width`, already inset and truncated. `None`
    /// marks the field row, which is drawn by the textarea rather than as text.
    fn body_lines(&self, width: u16) -> Vec<Option<Line<'static>>> {
        let inset = " ".repeat(BODY_INSET as usize);
        let mut out: Vec<Option<Line<'static>>> = Vec::with_capacity(self.spec.body.len());
        for (index, row) in self.spec.body.iter().enumerate() {
            let line = match row {
                BodyRow::Blank => Line::from(""),
                BodyRow::Text(line) => {
                    let mut spans = vec![Span::from(inset.clone())];
                    spans.extend(line.spans.clone());
                    truncate_line_with_ellipsis_if_overflow(Line::from(spans), width as usize)
                }
                BodyRow::Loading(label) => {
                    if !self.loading_visible() {
                        Line::from("")
                    } else {
                        let mut spans = vec![Span::from(inset.clone())];
                        spans.extend(crate::motion::shimmer_text(
                            label,
                            crate::motion::MotionMode::from_animations_enabled(
                                self.spec.animations,
                            ),
                        ));
                        truncate_line_with_ellipsis_if_overflow(Line::from(spans), width as usize)
                    }
                }
                BodyRow::Choice {
                    line, selectable, ..
                } => {
                    let selected = *selectable
                        && self
                            .choice_rows
                            .get(self.cursor)
                            .is_some_and(|row| *row == index);
                    let marker: Span<'static> = if selected {
                        format!("{} ", glyph("›")).fg(crate::style::accent_color())
                    } else {
                        Span::from(inset.clone())
                    };
                    let mut spans = vec![marker];
                    spans.extend(line.spans.clone());
                    truncate_line_with_ellipsis_if_overflow(Line::from(spans), width as usize)
                }
                BodyRow::Field => {
                    out.push(None);
                    continue;
                }
            };
            out.push(Some(line));
        }
        while (out.len() as u16) < self.spec.reserve_rows {
            out.push(Some(Line::from("")));
        }
        out
    }

    /// The one-line pane shown below [`MIN_PANE_WIDTH`]: state is kept and
    /// nothing is written to history.
    fn render_too_narrow(&self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        Paragraph::new(truncate_line_with_ellipsis_if_overflow(
            Line::from("Widen the terminal to 40 columns".bold()),
            width,
        ))
        .render(Rect { height: 1, ..area }, buf);
        if area.height >= 3 {
            Paragraph::new(truncate_line_with_ellipsis_if_overflow(
                Line::from(close_hint().dim()),
                width,
            ))
            .render(
                Rect {
                    y: area.y.saturating_add(2),
                    height: 1,
                    ..area
                },
                buf,
            );
        }
    }

    fn field_row_y(&self, area: Rect) -> Option<u16> {
        let field_index = self
            .spec
            .body
            .iter()
            .position(|row| matches!(row, BodyRow::Field))?;
        let top = area.y + 1 + u16::from(self.spec.blank_after_title);
        Some(top + field_index as u16)
    }
}

fn joined_width(segments: &[String]) -> usize {
    let separators = segments.len().saturating_sub(1) * 3;
    segments.iter().map(|s| s.chars().count()).sum::<usize>() + separators
}

impl BottomPaneView for InfoPaneView {
    fn keymap_contexts(&self) -> KeymapContextSet {
        match &self.field {
            Some(field) => KeymapContextSet::new(field.keymap_context()),
            None => KeymapContextSet::default(),
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        // Vim operator-pending swallows everything, exactly as CustomPromptView does.
        if let Some(field) = self.field.as_mut()
            && field.is_vim_operator_pending()
        {
            field.input(key_event);
            self.clamp_field();
            return;
        }

        match key_event {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                // Only reached when `prefer_esc_to_handle_key_event` diverted it,
                // i.e. vim insert mode: let the field leave insert mode instead.
                if let Some(field) = self.field.as_mut()
                    && field.should_handle_vim_insert_escape(key_event)
                {
                    field.input(key_event);
                } else {
                    self.on_ctrl_c();
                }
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            } if modifiers == KeyModifiers::NONE => self.activate_primary(),
            KeyEvent {
                code: KeyCode::Up, ..
            } => self.move_cursor(-1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => self.move_cursor(1),
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers,
                ..
            } if self.spec.retry && !self.has_focused_field() && !has_ctrl_or_alt(modifiers) => {
                self.send(PaneAction::Retry);
            }
            other => {
                // A focused field takes the rest; otherwise unbound keys do
                // nothing and are never forwarded to the composer.
                if let Some(field) = self.field.as_mut() {
                    field.input(other);
                    self.clamp_field();
                }
            }
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completion = Some(ViewCompletion::Cancelled);
        CancellationEvent::Handled
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        self.field
            .as_ref()
            .is_some_and(|field| field.uses_vim_insert_cursor() || field.is_vim_operator_pending())
    }

    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }

    fn view_id(&self) -> Option<&'static str> {
        Some(self.spec.pane.view_id())
    }

    fn selected_index(&self) -> Option<usize> {
        (!self.choice_rows.is_empty()).then_some(self.cursor)
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        let Some(field) = self.field.as_mut() else {
            return false;
        };
        if pasted.is_empty() {
            return false;
        }
        field.insert_str(&pasted);
        self.clamp_field();
        true
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        if let Some((at, _)) = &self.footer_revert {
            let remaining = at.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                return Some(remaining);
            }
        }
        if !self.is_loading() {
            return None;
        }
        if !self.loading_visible() {
            return Some(LOADING_DELAY.saturating_sub(self.opened_at.elapsed()));
        }
        self.spec.animations.then(|| Duration::from_millis(100))
    }
}

impl Renderable for InfoPaneView {
    fn desired_height(&self, width: u16) -> u16 {
        if width < MIN_PANE_WIDTH {
            return 3 + menu_surface_padding_height();
        }
        self.content_height() + menu_surface_padding_height()
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let content = render_menu_surface(area, buf);
        if content.is_empty() {
            return;
        }
        if area.width < MIN_PANE_WIDTH {
            self.render_too_narrow(content, buf);
            return;
        }

        let width = content.width;
        let mut y = content.y;
        let bottom = content.y.saturating_add(content.height);
        let row = |y: u16| Rect {
            x: content.x,
            y,
            width,
            height: 1,
        };

        if y < bottom {
            Paragraph::new(self.title_line(width)).render(row(y), buf);
        }
        y = y.saturating_add(1);
        if self.spec.blank_after_title {
            y = y.saturating_add(1);
        }

        for line in self.body_lines(width) {
            if y >= bottom {
                break;
            }
            match line {
                Some(line) => Paragraph::new(line).render(row(y), buf),
                None => {
                    // The `▌ ` input row: gutter, then the textarea.
                    if width > BODY_INSET {
                        Paragraph::new(Line::from(
                            format!("{} ", glyph("▌")).fg(crate::style::accent_color()),
                        ))
                        .render(row(y), buf);
                        if let Some(field) = self.field.as_ref() {
                            let rect = Rect {
                                x: content.x.saturating_add(BODY_INSET),
                                y,
                                width: width.saturating_sub(BODY_INSET),
                                height: 1,
                            };
                            let mut state = self.field_state.borrow_mut();
                            StatefulWidgetRef::render_ref(&field, rect, buf, &mut state);
                        }
                    }
                }
            }
            y = y.saturating_add(1);
        }

        // Blank, then the footer, pinned to the bottom of the content area.
        let footer_y = bottom.saturating_sub(1);
        if footer_y >= content.y {
            Paragraph::new(self.footer_line(width)).render(row(footer_y), buf);
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if area.width < MIN_PANE_WIDTH {
            return None;
        }
        let field = self.field.as_ref()?;
        let content = super::selection_popup_common::menu_surface_inset(area);
        let y = self.field_row_y(content)?;
        let rect = Rect {
            x: content.x.saturating_add(BODY_INSET),
            y,
            width: content.width.saturating_sub(BODY_INSET),
            height: 1,
        };
        let state = *self.field_state.borrow();
        field.cursor_pos_with_state(rect, state)
    }

    fn cursor_style(&self, _area: Rect) -> crossterm::cursor::SetCursorStyle {
        match self.field.as_ref() {
            Some(field) if field.uses_vim_insert_cursor() => {
                crossterm::cursor::SetCursorStyle::SteadyBar
            }
            _ => crossterm::cursor::SetCursorStyle::DefaultUserShape,
        }
    }
}

#[cfg(test)]
#[path = "info_pane_view_tests.rs"]
mod tests;
