//! Remaining-time notes: with `[byteask_harness] time_budget_secs = N`, the model is told
//! once when half of the run's wall-clock budget is gone and once at three quarters. It is a
//! fact plus the one decision it changes; nothing is forced (no best-guess edit at a
//! fixed deadline).

use std::time::Duration;

/// The notes already given, as a level: 0 none, 1 the 50% note, 2 the 75% note.
pub type Level = u8;

/// The note due after `elapsed` of `total`, and the level it moves to. When both thresholds
/// passed at once (one very long step), only the later note is given.
pub fn due(elapsed: Duration, total: Duration, given: Level) -> Option<(Level, String)> {
    if total.is_zero() {
        return None;
    }
    let fraction = elapsed.as_secs_f64() / total.as_secs_f64();
    let level = if fraction >= 0.75 {
        2
    } else if fraction >= 0.5 {
        1
    } else {
        0
    };
    if level <= given {
        return None;
    }
    let left = total.saturating_sub(elapsed);
    let text = if level == 2 {
        format!(
            "Time budget: 75% of this run's {} has been used; {} left. Finish now: keep the change \
             you have, run only the most relevant targeted test, and end your turn. Do not start \
             full rebuilds, whole test suites, fuzzing or new exploration.",
            human(total),
            human(left)
        )
    } else {
        format!(
            "Time budget: half of this run's {} has been used; {} left. If the fix is not in place \
             yet, make the change you think is right now and verify it with a targeted build and \
             test rather than exploring further.",
            human(total),
            human(left)
        )
    };
    Some((level, text))
}

fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 120 {
        format!("{} min", (secs + 30) / 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn notes_come_once_at_half_and_three_quarters() {
        assert_eq!(due(Duration::from_secs(1700), HOUR, 0), None);
        let (level, half) = due(Duration::from_secs(1800), HOUR, 0).expect("50%");
        assert_eq!(level, 1);
        assert!(
            half.contains("half of this run's 60 min has been used; 30 min left"),
            "{half}"
        );
        assert_eq!(due(Duration::from_secs(2000), HOUR, 1), None);
        let (level, late) = due(Duration::from_secs(2700), HOUR, 1).expect("75%");
        assert_eq!(level, 2);
        assert!(
            late.contains("75% of this run's 60 min has been used; 15 min left"),
            "{late}"
        );
        assert_eq!(due(Duration::from_secs(3500), HOUR, 2), None);
    }

    #[test]
    fn a_long_step_past_both_thresholds_gives_only_the_later_note() {
        let (level, text) = due(Duration::from_secs(3000), HOUR, 0).expect("note");
        assert_eq!(level, 2);
        assert!(text.starts_with("Time budget: 75%"), "{text}");
    }

    #[test]
    fn short_budgets_are_in_seconds_and_zero_means_none() {
        let (_, text) = due(Duration::from_secs(50), Duration::from_secs(100), 0).expect("note");
        assert!(text.contains("100 s has been used; 50 s left"), "{text}");
        assert_eq!(due(Duration::from_secs(5), Duration::ZERO, 0), None);
    }
}
