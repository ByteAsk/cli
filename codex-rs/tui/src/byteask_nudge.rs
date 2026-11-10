//! ByteAsk occasional in-chat nudge — after a completed turn we sometimes show a
//! tiny prompt in the composer's bottom row, randomly either a session-feedback
//! rating (1 Bad / 2 Fine / 3 Good / 0 Dismiss) or a referral nudge. Named to avoid
//! colliding with upstream's unrelated `codex_feedback` (a diagnostics uploader).
//!
//! The cadence is pure + injectable (rolls passed in) so it unit-tests deterministically;
//! the composer supplies real rolls from `rand`. Cadence: at least
//! MIN_GAP turns between prompts (so it never gets annoying), ~50% chance per eligible turn
//! so it typically lands around turn 7-8, and FORCE-shown once FORCE_GAP turns have passed —
//! which GUARANTEES at least one prompt in any 10 consecutive messages. No session cap.

/// Which nudge to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nudge {
    Feedback,
    Referral,
}

/// A prompt never appears sooner than this many turns after the last one (or after the
/// session start) — keeps it from getting annoying, and sets when the FIRST prompt can show.
pub(crate) const MIN_GAP_TURNS: u32 = 7;
/// Force a prompt once this many turns have passed since the last (or since start). With
/// FORCE_GAP = 10 the longest prompt-free run is 9 turns, so ANY window of 10 consecutive
/// messages contains at least one prompt (the "always within 10" guarantee).
pub(crate) const FORCE_GAP_TURNS: u32 = 10;
/// Chance to show on an eligible turn (between MIN_GAP and FORCE_GAP) — tuned so a prompt
/// typically lands around turn 7-8.
pub(crate) const SHOW_PROBABILITY: f64 = 0.5;

/// Per-session rotation state (lives on the composer).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct NudgeState {
    /// Completed turns so far this session.
    pub(crate) turn: u32,
    /// Turn index at which the last prompt was shown (None => never).
    pub(crate) last_shown_turn: Option<u32>,
}

/// True when the user has disabled all nudges via `BYTEASK_NO_PROMPTS`.
pub(crate) fn opt_out() -> bool {
    std::env::var("BYTEASK_NO_PROMPTS")
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Decide whether to show a nudge after a completed turn, and which one.
///
/// Pure: `show_roll` and `pick_roll` are uniform draws in `[0, 1)` (the composer
/// supplies real ones from `rand`; tests inject fixed values). Returns `None` to show
/// nothing. Order: opt-out, min-gap (too soon), then probability unless the force
/// ceiling is reached.
pub(crate) fn decide(state: &NudgeState, show_roll: f64, pick_roll: f64) -> Option<Nudge> {
    if opt_out() {
        return None;
    }
    // Turns since the last prompt, or since the session start if never shown.
    let since_last = match state.last_shown_turn {
        Some(last) => state.turn.saturating_sub(last),
        None => state.turn,
    };
    if since_last < MIN_GAP_TURNS {
        return None; // too soon -> never annoy
    }
    // Below the force ceiling, show probabilistically (typically lands ~7-8). At/after
    // FORCE_GAP_TURNS always show -> guarantees a prompt within every 10 consecutive turns.
    if since_last < FORCE_GAP_TURNS && show_roll >= SHOW_PROBABILITY {
        return None;
    }
    Some(if pick_roll < 0.5 {
        Nudge::Feedback
    } else {
        Nudge::Referral
    })
}

/// The composer bottom-row copy for a nudge (plain text; the composer renders it in
/// place of the rotating marketing hint while a prompt is pending).
pub(crate) fn prompt_line(nudge: Nudge) -> &'static str {
    match nudge {
        Nudge::Feedback => {
            "How is ByteAsk doing this session?   [1] Bad   [2] Fine   [3] Good   [0] Dismiss"
        }
        Nudge::Referral => {
            "Get $10 of usage (30 days) each for you + a friend — /referral-code   [0] Dismiss"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(turn: u32, last: Option<u32>) -> NudgeState {
        NudgeState {
            turn,
            last_shown_turn: last,
        }
    }

    #[test]
    fn no_prompt_before_min_gap() {
        // Fresh session: nothing shows before MIN_GAP turns, even on the lowest roll.
        for t in 0..MIN_GAP_TURNS {
            assert_eq!(decide(&st(t, None), 0.0, 0.0), None, "turn {t}");
        }
        assert!(decide(&st(MIN_GAP_TURNS, None), 0.0, 0.0).is_some());
    }

    #[test]
    fn min_gap_after_a_prompt() {
        // last at 5: blocked until since_last >= MIN_GAP (turn 5 + MIN_GAP).
        for t in 6..(5 + MIN_GAP_TURNS) {
            assert_eq!(decide(&st(t, Some(5)), 0.0, 0.0), None, "turn {t} too soon");
        }
        assert!(decide(&st(5 + MIN_GAP_TURNS, Some(5)), 0.0, 0.0).is_some());
    }

    #[test]
    fn probability_gate_between_min_and_force() {
        // since_last = 8 (in [MIN_GAP, FORCE_GAP)): low roll shows, high roll doesn't.
        assert!(decide(&st(8, None), SHOW_PROBABILITY - 0.01, 0.0).is_some());
        assert_eq!(decide(&st(8, None), SHOW_PROBABILITY, 0.0), None);
        assert_eq!(decide(&st(8, None), 0.99, 0.0), None);
    }

    #[test]
    fn force_shows_at_ceiling_regardless_of_roll() {
        // At/after FORCE_GAP the prompt shows even on the highest roll — the guarantee.
        assert!(decide(&st(FORCE_GAP_TURNS, None), 0.999, 0.0).is_some());
        assert!(decide(&st(25, Some(15)), 0.999, 0.0).is_some()); // since_last = 10
    }

    #[test]
    fn pick_splits_feedback_vs_referral() {
        assert_eq!(decide(&st(8, None), 0.0, 0.0), Some(Nudge::Feedback));
        assert_eq!(decide(&st(8, None), 0.0, 0.49), Some(Nudge::Feedback));
        assert_eq!(decide(&st(8, None), 0.0, 0.5), Some(Nudge::Referral));
        assert_eq!(decide(&st(8, None), 0.0, 0.99), Some(Nudge::Referral));
    }

    #[test]
    fn always_at_least_one_prompt_per_ten_consecutive() {
        // Worst case: always roll "don't show" (0.99) so ONLY the force fires. Then assert
        // every window of 10 consecutive turns contains at least one prompt.
        let mut state = NudgeState::default();
        let mut shown = std::collections::HashSet::new();
        for turn in 1..=200 {
            state.turn = turn;
            if decide(&state, 0.99, 0.0).is_some() {
                shown.insert(turn);
                state.last_shown_turn = Some(turn);
            }
        }
        for k in 1..=190u32 {
            assert!(
                (k..k + 10).any(|t| shown.contains(&t)),
                "no prompt in the 10-turn window starting at {k}"
            );
        }
    }

    #[test]
    fn typical_gap_is_min_gap_on_a_show_roll() {
        // With a roll under the probability, prompts fire as soon as MIN_GAP allows —
        // so the steady cadence is one every MIN_GAP (7) turns.
        let mut state = NudgeState::default();
        let mut shown = Vec::new();
        for turn in 1..=60 {
            state.turn = turn;
            if decide(&state, 0.1, 0.0).is_some() {
                shown.push(turn);
                state.last_shown_turn = Some(turn);
            }
        }
        assert_eq!(shown[0], MIN_GAP_TURNS);
        for w in shown.windows(2) {
            assert_eq!(w[1] - w[0], MIN_GAP_TURNS);
        }
    }

    #[test]
    fn copy_is_present_and_actionable() {
        assert!(prompt_line(Nudge::Feedback).contains("[1] Bad"));
        assert!(prompt_line(Nudge::Feedback).contains("[0] Dismiss"));
        assert!(prompt_line(Nudge::Referral).contains("/referral-code"));
        assert!(prompt_line(Nudge::Referral).contains("$10"));
    }
}
