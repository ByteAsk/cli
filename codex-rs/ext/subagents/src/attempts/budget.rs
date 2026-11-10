//! Time budget for the attempts phase. Pure arithmetic over explicit instants, so the rules are
//! unit-tested without a clock.
//!
//! - The session ends at `start + task timeout` when the environment gives one
//!   (`BYTEASK_TASK_TIMEOUT_SECS`, measured from process start), else at the sub-agent session
//!   deadline.
//! - A margin of at least 10% of the budget (and at least `apply_margin`) is kept free at the end
//!   for applying the selected change and cleaning up; nothing of the attempts phase is planned
//!   into it.
//! - A new attempt starts only if, after keeping a reserve for evaluating it, at least
//!   `min_run` of working time is left; its deadline is the end minus that reserve.

use std::time::Duration;
use std::time::Instant;

use serde::Serialize;

/// Time kept for one pairwise judge comparison (both presentation orders).
pub const JUDGE_RESERVE: Duration = Duration::from_secs(240);

#[derive(Clone, Debug)]
pub struct Budget {
    pub start: Instant,
    pub end: Instant,
    pub margin: Duration,
    pub source: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct BudgetReport {
    pub source: &'static str,
    pub total_secs: u64,
    pub margin_secs: u64,
    pub left_at_phase_start_secs: u64,
}

impl Budget {
    pub fn new(
        session_start: Instant,
        session_deadline: Instant,
        task_timeout: Option<Duration>,
        apply_margin: Duration,
    ) -> Self {
        let (end, source) = match task_timeout {
            Some(t) => (session_start + t, "task timeout"),
            None => (session_deadline, "sub-agent session deadline"),
        };
        let total = end.saturating_duration_since(session_start);
        Self {
            start: session_start,
            end,
            margin: (total / 10).max(apply_margin),
            source,
        }
    }

    pub fn total(&self) -> Duration {
        self.end.saturating_duration_since(self.start)
    }

    /// End of everything the attempts phase plans: the session end minus the margin.
    pub fn attempts_end(&self) -> Instant {
        self.end
            .checked_sub(self.margin)
            .unwrap_or(self.start)
            .max(self.start)
    }

    pub fn left(&self, now: Instant) -> Duration {
        self.attempts_end().saturating_duration_since(now)
    }

    /// Time kept back after a new attempt for evaluating it and judging. Before anything was
    /// measured: a quarter of what is left, 2-20 minutes. Afterwards: twice the measured
    /// evaluation time of one candidate plus a judge comparison, 2-30 minutes.
    pub fn eval_reserve(&self, now: Instant, measured: Option<Duration>, judge: bool) -> Duration {
        let judge_time = if judge { JUDGE_RESERVE } else { Duration::ZERO };
        match measured {
            Some(m) => {
                (m * 2 + judge_time).clamp(Duration::from_secs(120), Duration::from_secs(1800))
            }
            None => (self.left(now) / 4).clamp(Duration::from_secs(120), Duration::from_secs(1200)),
        }
    }

    /// Deadline for an attempt started at `now`, or why none can start.
    pub fn child_deadline(
        &self,
        now: Instant,
        reserve: Duration,
        min_run: Duration,
    ) -> Result<Instant, String> {
        let end = self.attempts_end();
        let child_end = end.checked_sub(reserve).unwrap_or(now);
        let run = child_end.saturating_duration_since(now);
        if run < min_run {
            return Err(format!(
                "only {}s of working time left for a new attempt (need {}s; {}s kept for evaluation, {}s margin before the session ends)",
                run.as_secs(),
                min_run.as_secs(),
                reserve.as_secs(),
                self.margin.as_secs()
            ));
        }
        Ok(child_end)
    }

    pub fn report(&self, now: Instant) -> BudgetReport {
        BudgetReport {
            source: self.source,
            total_secs: self.total().as_secs(),
            margin_secs: self.margin.as_secs(),
            left_at_phase_start_secs: self.left(now).as_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const MIN: Duration = Duration::from_secs(300);

    #[test]
    fn margin_is_at_least_ten_percent_and_the_apply_floor() {
        let t0 = Instant::now();
        let b = Budget::new(
            t0,
            t0 + Duration::from_secs(3600),
            Some(Duration::from_secs(5400)),
            Duration::from_secs(120),
        );
        assert_eq!(b.source, "task timeout");
        assert_eq!(b.margin, Duration::from_secs(540));
        assert_eq!(b.attempts_end(), t0 + Duration::from_secs(5400 - 540));
        // Short budgets keep the floor.
        let b = Budget::new(
            t0,
            t0 + Duration::from_secs(600),
            None,
            Duration::from_secs(120),
        );
        assert_eq!(b.source, "sub-agent session deadline");
        assert_eq!(b.margin, Duration::from_secs(120));
        assert_eq!(b.attempts_end(), t0 + Duration::from_secs(480));
    }

    #[test]
    fn a_new_attempt_needs_working_time_after_the_reserve() {
        let t0 = Instant::now();
        let b = Budget::new(
            t0,
            t0,
            Some(Duration::from_secs(5400)),
            Duration::from_secs(120),
        );
        // 20 minutes in: 4860 - 1200 = 3660 s left; reserve a quarter (clamped to 1200).
        let now = t0 + Duration::from_secs(1200);
        let reserve = b.eval_reserve(now, None, true);
        assert_eq!(reserve, Duration::from_secs(915));
        let deadline = b.child_deadline(now, reserve, MIN).unwrap_or(now);
        assert_eq!(deadline, t0 + Duration::from_secs(4860 - 915));
        // 75 minutes in: 360 s left; the 120 s minimum reserve leaves 240 s < 300 s.
        let late = t0 + Duration::from_secs(4500);
        let reserve = b.eval_reserve(late, None, true);
        assert_eq!(reserve, Duration::from_secs(120));
        let err = b
            .child_deadline(late, reserve, MIN)
            .err()
            .unwrap_or_default();
        assert!(err.contains("only 240s"), "{err}");
        // Past the margin nothing starts.
        assert!(
            b.child_deadline(t0 + Duration::from_secs(5000), Duration::ZERO, MIN)
                .is_err()
        );
    }

    #[test]
    fn measured_evaluation_time_sizes_the_reserve() {
        let t0 = Instant::now();
        let b = Budget::new(
            t0,
            t0,
            Some(Duration::from_secs(5400)),
            Duration::from_secs(120),
        );
        assert_eq!(
            b.eval_reserve(t0, Some(Duration::from_secs(200)), true),
            Duration::from_secs(640)
        );
        assert_eq!(
            b.eval_reserve(t0, Some(Duration::from_secs(10)), false),
            Duration::from_secs(120)
        );
        assert_eq!(
            b.eval_reserve(t0, Some(Duration::from_secs(5000)), true),
            Duration::from_secs(1800)
        );
    }
}
