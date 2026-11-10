//! Frame and key tests for [`InfoPaneView`].
//!
//! Every frame test renders into a real `Buffer` and asserts the *text* of each
//! row plus its width, which is what each pane's design
//! fixes. Assertions rather than insta snapshots, so a `.snap.new`
//! can never be left behind and so the reason a row exists is written down next
//! to it.

use super::*;
use crate::app_event_sender::AppEventSender;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

/// The width every pane is designed at. `/referral-code` uses 80.
pub(crate) const FRAME_WIDTH: u16 = 72;

pub(crate) fn sender() -> (AppEventSender, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    (AppEventSender::new(tx), rx)
}

/// Render at `width` and return the rows, trailing blanks trimmed. The two
/// columns `render_menu_surface` insets on each side are part of the frame, so
/// they are kept: an assertion on the row text is an assertion on the layout.
pub(crate) fn rows(view: &InfoPaneView, width: u16) -> Vec<String> {
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    let mut out: Vec<String> = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

fn view_of(spec: InfoPaneSpec) -> InfoPaneView {
    let (tx, _rx) = sender();
    InfoPaneView::new(spec, tx)
}

/// A spec shaped like `/usage` on a Pro plan.
fn usage_like_spec() -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    spec.tag = "Pro plan".to_string();
    spec.body = vec![
        BodyRow::Text(Line::from(format!(
            "{:<8}{}{} {:>3}%   resets in 47m (15:06)",
            "5-hour",
            "█".repeat(16),
            "░".repeat(4),
            82
        ))),
        BodyRow::Blank,
        BodyRow::Text(Line::from("Higher limits with /upgrade")),
    ];
    spec
}

#[test]
fn every_row_fits_the_frame_and_the_title_and_footer_are_always_drawn() {
    let view = view_of(usage_like_spec());
    let rendered = rows(&view, FRAME_WIDTH);
    for row in &rendered {
        assert!(
            row.chars().count() <= FRAME_WIDTH as usize,
            "row wider than the frame: {row:?}"
        );
    }
    // Title left, tag right-aligned, both on the first content row.
    assert!(rendered[1].starts_with("  Usage"), "{rendered:?}");
    assert!(rendered[1].ends_with("Pro plan"), "{rendered:?}");
    assert_eq!(rendered[1].chars().count(), FRAME_WIDTH as usize - 2);
    // Body rows sit two columns in from the title.
    assert!(rendered[3].starts_with("    5-hour"), "{rendered:?}");
    // The footer is the last row and always offers `esc`.
    assert_eq!(rendered.last().map(String::as_str), Some("  esc to close"));
}

#[test]
fn a_choice_row_marks_the_cursor_with_a_glyph_not_only_colour() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, vec!["Upgrade your plan".bold()]);
    spec.body = vec![
        BodyRow::Choice {
            id: "pro".to_string(),
            line: Line::from("Pro"),
            selectable: true,
        },
        BodyRow::Choice {
            id: "max".to_string(),
            line: Line::from("Max"),
            selectable: true,
        },
    ];
    spec.primary = Primary::Choose;
    let mut view = view_of(spec);
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[3], "  › Pro", "{rendered:?}");
    assert_eq!(rendered[4], "    Max", "{rendered:?}");

    view.handle_key_event(KeyEvent::from(KeyCode::Down));
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[3], "    Pro", "{rendered:?}");
    assert_eq!(rendered[4], "  › Max", "{rendered:?}");
}

#[test]
fn a_current_plan_row_is_not_selectable_and_the_cursor_starts_below_it() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, vec!["Upgrade your plan".bold()]);
    spec.body = vec![
        BodyRow::Choice {
            id: "pro".to_string(),
            line: Line::from("Pro    current plan"),
            selectable: false,
        },
        BodyRow::Choice {
            id: "max".to_string(),
            line: Line::from("Max"),
            selectable: true,
        },
    ];
    spec.primary = Primary::Choose;
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[3], "    Pro    current plan", "{rendered:?}");
    assert_eq!(rendered[4], "  › Max", "{rendered:?}");

    // `enter` can only pick the selectable row.
    view.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ByteAskPaneAction {
            pane: ByteAskPane::Upgrade,
            action: PaneAction::Choose(plan),
        }) if plan == "max"
    ));
}

#[test]
fn enter_performs_the_footer_action_and_never_closes_a_readout() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.body = vec![BodyRow::Text(Line::from("REFCODE1"))];
    spec.footer = vec!["enter to copy the curl line".to_string(), close_hint()];
    spec.primary = Primary::Copy("curl ... | sh".to_string());
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);

    view.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(!view.is_complete(), "enter must never close a pane");
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ByteAskPaneAction {
            action: PaneAction::Copy(text),
            ..
        }) if text == "curl ... | sh"
    ));
}

#[test]
fn a_readout_with_no_enter_action_does_nothing_on_enter() {
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(usage_like_spec(), tx);
    view.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(!view.is_complete());
    assert!(
        rx.try_recv().is_err(),
        "a footer with no enter action must not fire one"
    );
}

#[test]
fn esc_and_ctrl_c_both_close_the_pane() {
    for key in [KeyCode::Esc, KeyCode::Char('c')] {
        let mut view = view_of(usage_like_spec());
        assert!(!view.is_complete());
        if key == KeyCode::Esc {
            // `esc` reaches the view through `on_ctrl_c` (bottom_pane/mod.rs).
            assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
        } else {
            assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
        }
        assert!(view.is_complete());
        assert_eq!(view.completion(), Some(ViewCompletion::Cancelled));
    }
}

#[test]
fn printable_keys_are_swallowed_by_a_readout() {
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(usage_like_spec(), tx);
    for ch in "hello /model".chars() {
        view.handle_key_event(KeyEvent::from(KeyCode::Char(ch)));
    }
    assert!(!view.is_complete(), "typing must not close the pane");
    assert!(
        rx.try_recv().is_err(),
        "typed keys must never leave the pane"
    );
}

#[test]
fn r_retries_only_when_the_state_binds_it_and_no_field_has_focus() {
    let mut spec = usage_like_spec();
    spec.retry = true;
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);
    view.handle_key_event(KeyEvent::from(KeyCode::Char('r')));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ByteAskPaneAction {
            action: PaneAction::Retry,
            ..
        })
    ));

    // With a focused field the same key is text, never a retry.
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, vec!["Redeem a voucher".bold()]);
    spec.blank_after_title = false;
    spec.body = vec![BodyRow::Field];
    spec.field = Some(String::new());
    spec.primary = Primary::Submit;
    spec.retry = true;
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);
    view.handle_key_event(KeyEvent::from(KeyCode::Char('r')));
    assert!(
        rx.try_recv().is_err(),
        "`r` is never bound while a field has focus"
    );
}

#[test]
fn the_voucher_field_takes_text_uppercases_nothing_until_submit_and_caps_at_32() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, vec!["Redeem a voucher".bold()]);
    spec.blank_after_title = false;
    spec.body = vec![
        BodyRow::Field,
        BodyRow::Text(Line::from("Credit shows as Extra in /usage.")),
    ];
    spec.field = Some(String::new());
    spec.footer = vec!["enter to redeem".to_string(), close_hint()];
    spec.primary = Primary::Submit;
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);

    for ch in "gift5".chars() {
        view.handle_key_event(KeyEvent::from(KeyCode::Char(ch)));
    }
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[1], "  Redeem a voucher", "{rendered:?}");
    assert_eq!(rendered[2], "  ▌ gift5", "{rendered:?}");

    // Paste is accepted, and 32 characters is the ceiling.
    assert!(view.handle_paste("X".repeat(64)));
    assert_eq!(view.field_text().chars().count(), 32);

    view.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(!view.is_complete(), "submitting must not close the pane");
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::ByteAskPaneAction {
            action: PaneAction::Submit(_),
            ..
        })
    ));
}

#[test]
fn an_empty_submit_does_nothing_and_keeps_focus() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, vec!["Redeem a voucher".bold()]);
    spec.blank_after_title = false;
    spec.body = vec![BodyRow::Field];
    spec.field = Some(String::new());
    spec.primary = Primary::Submit;
    let (tx, mut rx) = sender();
    let mut view = InfoPaneView::new(spec, tx);
    view.handle_key_event(KeyEvent::from(KeyCode::Enter));
    assert!(rx.try_recv().is_err(), "an empty submit must do nothing");
    assert!(!view.is_complete(), "focus stays in the field");
}

#[test]
fn below_forty_columns_the_pane_is_one_line_and_keeps_its_state() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, vec!["Redeem a voucher".bold()]);
    spec.blank_after_title = false;
    spec.body = vec![BodyRow::Field];
    spec.field = Some("GIFT5".to_string());
    spec.primary = Primary::Submit;
    let view = view_of(spec);
    let rendered = rows(&view, 39);
    assert_eq!(rendered[1], "  Widen the terminal to 40 columns");
    assert_eq!(rendered.last().map(String::as_str), Some("  esc to close"));
    // The typed code is still there; a resize never loses it.
    assert_eq!(view.field_text(), "GIFT5");
    // …and it comes back at a usable width.
    let wide = rows(&view, FRAME_WIDTH);
    assert!(wide.iter().any(|row| row.contains("GIFT5")), "{wide:?}");
}

#[test]
fn a_narrow_footer_drops_middle_segments_and_keeps_esc() {
    let mut spec = usage_like_spec();
    spec.footer = vec![
        "enter to get a payment link".to_string(),
        "r to retry".to_string(),
        close_hint(),
    ];
    let view = view_of(spec);
    let wide = rows(&view, FRAME_WIDTH);
    assert_eq!(
        wide.last().map(String::as_str),
        Some("  enter to get a payment link · r to retry · esc to close")
    );
    let narrow = rows(&view, 45);
    let footer = narrow.last().cloned().unwrap_or_default();
    assert!(footer.contains("esc to close"), "{footer:?}");
    assert!(
        footer.chars().count() <= 45,
        "footer must fit the frame: {footer:?}"
    );
}

#[test]
fn a_body_row_wider_than_the_frame_is_truncated_with_an_ellipsis() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.body = vec![BodyRow::Text(Line::from("x".repeat(200)))];
    let view = view_of(spec);
    let rendered = rows(&view, FRAME_WIDTH);
    assert!(rendered[3].ends_with('…'), "{rendered:?}");
    assert!(rendered[3].chars().count() <= FRAME_WIDTH as usize);
}

#[test]
fn loading_reserves_the_result_height_so_the_composer_does_not_jump() {
    let mut loading = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    loading.body = vec![BodyRow::Loading("Loading usage".to_string())];
    loading.reserve_rows = 4;
    let loading_height = view_of(loading).desired_height(FRAME_WIDTH);

    let mut result = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    result.body = vec![
        BodyRow::Text(Line::from("5-hour")),
        BodyRow::Text(Line::from("Weekly")),
        BodyRow::Text(Line::from("Extra")),
        BodyRow::Blank,
    ];
    assert_eq!(loading_height, view_of(result).desired_height(FRAME_WIDTH));
}

#[test]
fn loading_text_appears_only_after_the_delay_but_the_frame_is_there_immediately() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    spec.body = vec![BodyRow::Loading("Loading usage".to_string())];
    spec.reserve_rows = 4;
    spec.animations = false;
    let mut view = view_of(spec);
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[1], "  Usage", "the title is drawn immediately");
    assert!(
        !rendered.iter().any(|row| row.contains("Loading usage")),
        "no flash of loading text before 150 ms: {rendered:?}"
    );
    // The view asks to be redrawn when the delay is up.
    assert!(view.next_frame_delay().is_some());

    view.age_for_test(LOADING_DELAY);
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(rendered[3], "    Loading usage", "{rendered:?}");
    // Animations off: static text, so no repaint is scheduled.
    assert_eq!(view.next_frame_delay(), None);
}

#[test]
fn a_transient_footer_reverts_to_the_panes_own_footer() {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.footer = vec!["enter to copy the curl line".to_string(), close_hint()];
    let mut view = view_of(spec);
    view.set_transient_footer(
        vec!["✓ Copied".to_string(), close_hint()],
        Duration::from_secs(2),
        vec!["enter to copy the curl line".to_string(), close_hint()],
    );
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(
        rendered.last().map(String::as_str),
        Some("  ✓ Copied · esc to close")
    );
    assert!(view.next_frame_delay().is_some());

    view.expire_transient_footer_for_test();
    let rendered = rows(&view, FRAME_WIDTH);
    assert_eq!(
        rendered.last().map(String::as_str),
        Some("  enter to copy the curl line · esc to close")
    );
}

#[test]
fn the_view_id_is_stable_per_command_and_only_ours_count_as_info_panes() {
    assert_eq!(ByteAskPane::Usage.view_id(), "byteask_usage");
    assert_eq!(ByteAskPane::Referral.view_id(), "byteask_referral");
    assert_eq!(ByteAskPane::Upgrade.view_id(), "byteask_upgrade");
    assert_eq!(ByteAskPane::Voucher.view_id(), "byteask_voucher");
    for pane in [
        ByteAskPane::Usage,
        ByteAskPane::Referral,
        ByteAskPane::Upgrade,
        ByteAskPane::Voucher,
    ] {
        assert!(ByteAskPane::is_info_pane_id(pane.view_id()));
    }
    assert!(!ByteAskPane::is_info_pane_id("model"));
}
