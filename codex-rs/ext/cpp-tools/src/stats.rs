//! Shared statistics core for the performance tools.
//!
//! Turns raw timing samples (or an already-summarized mean + dispersion, e.g. a
//! Google Benchmark aggregate row) into a [`Measurement`], and compares two
//! measurements into a [`Verdict`] that NEVER claims a speedup inside
//! measurement noise. Every `cpp_perf` / `cpp_bench` "is it faster?" answer
//! routes through [`compare`], so the verdict is honest by construction rather
//! than by each caller remembering to hedge.
//!
//! ```text
//!   samples ─┐
//!            ├─► Measurement ─┐
//!   summary ─┘                ├─► compare(floor) ─► Verdict ─► render_verdict()
//!            (mean, stderr, n,│                     Faster | WithinNoise |
//!             failures)       │                     NoNoiseEstimate | InvalidComparison
//! ```
//!
//! Decision order in [`compare`]:
//! 1. any failed run on either side → `InvalidComparison` (a command
//!    that did not do the work is not "faster").
//! 2. no dispersion estimate on either side → `NoNoiseEstimate`.
//! 3. `Faster` iff `delta > gate`, where `gate` is the larger of
//!    - the half-width of the **99% confidence interval** of the difference of
//!      the two means (Welch's t-test: unequal variances, Welch–Satterthwaite
//!      degrees of freedom), and
//!    - the caller's relative `floor` × the larger mean — the smallest
//!      difference that kind of measurement can attribute to the code at all;
//!
//!    otherwise `WithinNoise`.
//!
//! This replaced the first gate (`delta > 2 * (noise_a +
//! noise_b)` plus a 1% floor), which had no degrees of freedom: with the
//! default 5 runs, a side whose few samples happened to agree got a tiny gate,
//! and an A/A simulation with unequal spreads called ~11% of identical pairs
//! "faster" (test `unequal_spread_aa_false_wins_stay_near_one_percent`). A test
//! is only as good as its samples, though: the gate assumes both sides were
//! measured under the same conditions, which is why `cpp_perf` interleaves its
//! A and B runs.

/// Student's t 0.995 quantile (the two-sided 99% critical value) for 1..=30
/// degrees of freedom. Checked against numeric integration of the t density.
const T_99: [f64; 30] = [
    63.657, 9.925, 5.841, 4.604, 4.032, 3.707, 3.499, 3.355, 3.250, 3.169, 3.106, 3.055, 3.012,
    2.977, 2.947, 2.921, 2.898, 2.878, 2.861, 2.845, 2.831, 2.819, 2.807, 2.797, 2.787, 2.779,
    2.771, 2.763, 2.756, 2.750,
];

/// Tabulated points beyond 30 degrees of freedom, interpolated linearly in
/// `1/df` (accurate to ~0.001), ending at the normal quantile as `df → ∞`.
const T_99_TAIL: [(f64, f64); 4] = [(30.0, 2.750), (40.0, 2.704), (60.0, 2.660), (120.0, 2.617)];
const Z_99: f64 = 2.576;

/// One side's timing summary. `noise` is the standard error of the mean, in
/// the same unit as `mean` (seconds for `cpp_perf`, ns for `cpp_bench`); it is
/// `None` when there is no way to estimate it (a single run).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub mean: f64,
    pub noise: Option<f64>,
    pub runs: u32,
    /// How many of `runs` exited non-zero. Any failure invalidates a comparison.
    pub failures: u32,
}

impl Measurement {
    /// Build a `Measurement` from raw wall-clock samples, computing the mean and
    /// a standard-error dispersion estimate (sample stddev / sqrt(n)). `noise`
    /// is `None` for fewer than two samples (no dispersion to estimate).
    pub fn from_samples(samples: &[f64], failures: u32) -> Self {
        let runs = samples.len() as u32;
        let mean = if samples.is_empty() {
            0.0
        } else {
            samples.iter().sum::<f64>() / samples.len() as f64
        };
        Measurement {
            mean,
            noise: stderr_of(samples),
            runs,
            failures,
        }
    }

    /// Build a `Measurement` from an already-summarized mean + standard error
    /// (the Google-Benchmark-aggregate path, which hands us the mean and a
    /// `_stddev` row directly).
    pub fn from_summary(mean: f64, noise: Option<f64>, runs: u32, failures: u32) -> Self {
        Measurement {
            mean,
            noise,
            runs,
            failures,
        }
    }

    /// One-line per-command summary, e.g.
    /// `A: mean 0.0123s over 5 run(s) (±0.0001s stderr)`.
    pub fn summary(&self, label: &str, unit: &str, precision: usize) -> String {
        let mean = format!("{mean:.precision$}", mean = self.mean);
        let mut s = format!("{label}: mean {mean}{unit} over {} run(s)", self.runs);
        if let Some(noise) = self.noise {
            let noise = format!("{noise:.precision$}");
            s.push_str(&format!(" (±{noise}{unit} stderr)"));
        }
        if self.failures > 0 {
            s.push_str(&format!(" ({} run(s) exited non-zero)", self.failures));
        }
        s
    }
}

/// Sample standard error (stddev / sqrt(n)) using the n-1 (Bessel) sample
/// variance. Returns `None` for fewer than two samples.
fn stderr_of(samples: &[f64]) -> Option<f64> {
    let n = samples.len();
    if n < 2 {
        return None;
    }
    let mean = samples.iter().sum::<f64>() / n as f64;
    let var = samples
        .iter()
        .map(|x| {
            let d = x - mean;
            d * d
        })
        .sum::<f64>()
        / (n as f64 - 1.0);
    Some((var / n as f64).sqrt())
}

/// Welch–Satterthwaite degrees of freedom of the difference of two means with
/// standard errors `se_a` / `se_b` over `n_a` / `n_b` samples. Always at least
/// `min(n_a, n_b) - 1`; NaN only when both standard errors are zero.
fn welch_df(se_a: f64, n_a: u32, se_b: f64, n_b: u32) -> f64 {
    let (va, vb) = (se_a * se_a, se_b * se_b);
    let term = |v: f64, n: u32| v * v / f64::from(n.saturating_sub(1).max(1));
    (va + vb).powi(2) / (term(va, n_a) + term(vb, n_b))
}

/// Two-sided 99% critical value of Student's t with `df` degrees of freedom.
/// Fractional `df` below 30 rounds DOWN: fewer degrees of freedom is the wider,
/// safer gate.
fn t_critical_99(df: f64) -> f64 {
    if df.is_nan() || df < 1.0 {
        // Welch–Satterthwaite never yields this with n >= 2 per side; answer
        // with the most conservative value rather than index out of bounds.
        return T_99[0];
    }
    if df < 30.0 {
        return T_99[df as usize - 1];
    }
    let x = 1.0 / df;
    for pair in T_99_TAIL.windows(2) {
        let ((df_lo, t_lo), (df_hi, t_hi)) = (pair[0], pair[1]);
        if df <= df_hi {
            let (x_lo, x_hi) = (1.0 / df_lo, 1.0 / df_hi);
            return t_hi + (t_lo - t_hi) * (x - x_hi) / (x_lo - x_hi);
        }
    }
    let (df_last, t_last) = T_99_TAIL[T_99_TAIL.len() - 1];
    Z_99 + (t_last - Z_99) * x * df_last
}

/// The honest answer to "is A vs B faster?".
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// A real, above-noise difference. `winner` is `'A'` or `'B'`; `gate` is
    /// the smallest difference that would have counted.
    Faster {
        winner: char,
        ratio: f64,
        delta: f64,
        gate: f64,
    },
    /// A measured difference that is inside the noise band — do NOT claim a
    /// win. `floor_bound` says the caller's floor, not the measured spread, set
    /// the gate, so more runs would not help.
    WithinNoise {
        delta_pct: f64,
        gate_pct: f64,
        floor_bound: bool,
    },
    /// No dispersion estimate available (single run, or unparseable dispersion).
    NoNoiseEstimate,
    /// One side failed; the timings are meaningless.
    InvalidComparison { side: char, failed: u32, total: u32 },
}

/// Compare two measurements. Check order is load-bearing: failure, then
/// missing-noise, then the gate. `floor` is the caller's relative floor (e.g.
/// `0.03` = never call a difference under 3% of the larger mean).
pub fn compare(a: &Measurement, b: &Measurement, floor: f64) -> Verdict {
    if a.failures > 0 {
        return Verdict::InvalidComparison {
            side: 'A',
            failed: a.failures,
            total: a.runs,
        };
    }
    if b.failures > 0 {
        return Verdict::InvalidComparison {
            side: 'B',
            failed: b.failures,
            total: b.runs,
        };
    }
    let (na, nb) = match (a.noise, b.noise) {
        (Some(na), Some(nb)) if a.runs >= 2 && b.runs >= 2 => (na, nb),
        _ => return Verdict::NoNoiseEstimate,
    };
    if a.mean <= 0.0 || b.mean <= 0.0 {
        return Verdict::NoNoiseEstimate;
    }
    let delta = (a.mean - b.mean).abs();
    let max_mean = a.mean.max(b.mean);
    let se = (na * na + nb * nb).sqrt();
    // Zero spread on both sides (a deterministic count, a coarse clock): any
    // difference is statistically certain, so only the floor can gate it.
    let noise_gate = if se > 0.0 {
        t_critical_99(welch_df(na, a.runs, nb, b.runs)) * se
    } else {
        0.0
    };
    let floor_gate = floor * max_mean;
    let gate = noise_gate.max(floor_gate);
    if delta > gate {
        let (winner, ratio) = if b.mean < a.mean {
            ('B', a.mean / b.mean)
        } else {
            ('A', b.mean / a.mean)
        };
        Verdict::Faster {
            winner,
            ratio,
            delta,
            gate,
        }
    } else {
        Verdict::WithinNoise {
            delta_pct: delta / max_mean * 100.0,
            gate_pct: gate / max_mean * 100.0,
            floor_bound: floor_gate >= noise_gate,
        }
    }
}

/// Render a [`Verdict`] as one model-facing line. `unit` is the value suffix
/// (`"s"`, `"ns"`), `precision` the decimal places for absolute values.
/// `resolve_hint` is the tool's "how to get a cleaner answer" clause for a
/// noise-bound result (`cpp_perf` says "increase repeat"); `floor_hint` is the
/// one for a floor-bound result, where more runs cannot help.
pub fn render_verdict(
    v: &Verdict,
    unit: &str,
    precision: usize,
    resolve_hint: &str,
    floor_hint: &str,
) -> String {
    match v {
        Verdict::Faster {
            winner,
            ratio,
            delta,
            gate,
        } => {
            let loser = if *winner == 'A' { 'B' } else { 'A' };
            let delta = format!("{delta:.precision$}");
            let gate = format!("{gate:.precision$}");
            format!(
                "{winner} is {ratio:.2}x faster than {loser} \
                 (delta {delta}{unit} > noise gate {gate}{unit}; significant)."
            )
        }
        Verdict::WithinNoise {
            delta_pct,
            gate_pct,
            floor_bound,
        } => {
            let resolve = if *floor_bound { floor_hint } else { resolve_hint };
            format!(
                "A and B differ by {delta_pct:.1}% which is INSIDE the measurement noise \
                 (gate {gate_pct:.1}%). Do NOT claim a speedup from this data. \
                 To resolve: {resolve}."
            )
        }
        Verdict::NoNoiseEstimate => format!(
            "No noise estimate (single run or unmeasured dispersion) — the difference \
             cannot be verified. To resolve: {resolve_hint}."
        ),
        Verdict::InvalidComparison {
            side,
            failed,
            total,
        } => format!(
            "INVALID COMPARISON: {side} exited non-zero on {failed}/{total} run(s) — \
             timings of a failing command are meaningless; fix the command first."
        ),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const HINT: &str = "increase repeat, enlarge the workload, or close background load";
    const FLOOR_HINT: &str = "benchmark it in-process";

    fn m(mean: f64, noise: Option<f64>) -> Measurement {
        Measurement::from_summary(mean, noise, 5, 0)
    }

    /// xorshift64* + Box–Muller: a deterministic normal source, so the A/A
    /// simulations below give the same answer on every run.
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ((x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            let (u, v) = (self.unit(), self.unit());
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    /// Share of `trials` comparisons of `n` samples per side, drawn from
    /// normals around 100 (+ `shift_b` on B) with the given spreads, that
    /// `compare` calls a win.
    fn win_rate(n: usize, sd_a: f64, sd_b: f64, shift_b: f64, floor: f64, seed: u64) -> f64 {
        const TRIALS: usize = 20_000;
        let mut rng = Rng(seed);
        let mut wins = 0;
        for _ in 0..TRIALS {
            let a: Vec<f64> = (0..n).map(|_| 100.0 + sd_a * rng.normal()).collect();
            let b: Vec<f64> = (0..n).map(|_| 100.0 + shift_b + sd_b * rng.normal()).collect();
            let verdict = compare(
                &Measurement::from_samples(&a, 0),
                &Measurement::from_samples(&b, 0),
                floor,
            );
            if matches!(verdict, Verdict::Faster { .. }) {
                wins += 1;
            }
        }
        wins as f64 / TRIALS as f64
    }

    #[test]
    fn from_samples_computes_mean_and_stderr() {
        let stats = Measurement::from_samples(&[0.2, 0.4, 0.3], 0);
        assert_eq!(stats.runs, 3);
        assert!((stats.mean - 0.3).abs() < 1e-9);
        // sample stddev of {0.2,0.4,0.3} = 0.1; stderr = 0.1/sqrt(3).
        let expected = 0.1_f64 / 3.0_f64.sqrt();
        assert!((stats.noise.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn from_samples_single_has_no_noise() {
        let stats = Measurement::from_samples(&[0.5], 0);
        assert_eq!(stats.runs, 1);
        assert!(stats.noise.is_none());
    }

    #[test]
    fn from_samples_empty_is_zero_no_noise() {
        let stats = Measurement::from_samples(&[], 0);
        assert_eq!(stats.runs, 0);
        assert_eq!(stats.mean, 0.0);
        assert!(stats.noise.is_none());
    }

    #[test]
    fn failure_on_a_invalidates_before_anything_else() {
        // Even a huge, clean-looking delta must not produce a winner if A failed.
        let a = Measurement::from_summary(0.001, Some(0.0), 5, 2);
        let b = m(1.0, Some(0.001));
        assert_eq!(
            compare(&a, &b, 0.01),
            Verdict::InvalidComparison {
                side: 'A',
                failed: 2,
                total: 5
            }
        );
    }

    #[test]
    fn failure_on_b_invalidates() {
        let a = m(1.0, Some(0.001));
        let b = Measurement::from_summary(0.001, Some(0.0), 5, 1);
        assert_eq!(
            compare(&a, &b, 0.01),
            Verdict::InvalidComparison {
                side: 'B',
                failed: 1,
                total: 5
            }
        );
    }

    #[test]
    fn missing_noise_on_either_side_is_no_estimate() {
        assert_eq!(compare(&m(2.0, None), &m(1.0, Some(0.01)), 0.01), Verdict::NoNoiseEstimate);
        assert_eq!(compare(&m(2.0, Some(0.01)), &m(1.0, None), 0.01), Verdict::NoNoiseEstimate);
    }

    #[test]
    fn clear_win_above_gate_names_winner() {
        // A=2.0, B=1.0, tiny noise → gate ~0.005, delta 1.0 ≫ gate, rel 50%.
        let v = compare(&m(2.0, Some(0.001)), &m(1.0, Some(0.001)), 0.01);
        match v {
            Verdict::Faster { winner, ratio, .. } => {
                assert_eq!(winner, 'B');
                assert!((ratio - 2.0).abs() < 1e-9);
            }
            other => panic!("expected Faster, got {other:?}"),
        }
    }

    #[test]
    fn delta_below_gate_is_within_noise() {
        // delta 0.02; se 0.0707, df 8 → gate 3.355 * 0.0707 ≈ 0.237.
        let v = compare(&m(1.00, Some(0.05)), &m(1.02, Some(0.05)), 0.01);
        match v {
            Verdict::WithinNoise {
                gate_pct,
                floor_bound,
                ..
            } => {
                assert!((gate_pct - 23.26).abs() < 0.05, "{gate_pct}");
                assert!(!floor_bound);
            }
            other => panic!("expected WithinNoise, got {other:?}"),
        }
    }

    #[test]
    fn delta_above_noise_but_below_floor_is_within_noise_and_floor_bound() {
        // Ultra-quiet machine: noise ~0, delta 0.5% of mean. Above the noise gate
        // but below a 1% floor → still WithinNoise, and more runs cannot help.
        match compare(&m(1.000, Some(0.00001)), &m(1.005, Some(0.00001)), 0.01) {
            Verdict::WithinNoise {
                floor_bound,
                gate_pct,
                ..
            } => {
                assert!(floor_bound);
                assert!((gate_pct - 1.0).abs() < 1e-9);
            }
            other => panic!("expected WithinNoise, got {other:?}"),
        }
    }

    #[test]
    fn zero_spread_is_gated_by_the_floor_alone() {
        // Deterministic samples: no noise gate at all, only the floor decides.
        assert!(matches!(
            compare(&m(100.0, Some(0.0)), &m(102.0, Some(0.0)), 0.01),
            Verdict::Faster { .. }
        ));
        assert!(matches!(
            compare(&m(100.0, Some(0.0)), &m(102.0, Some(0.0)), 0.03),
            Verdict::WithinNoise {
                floor_bound: true,
                ..
            }
        ));
    }

    #[test]
    fn non_positive_mean_is_no_estimate() {
        assert_eq!(compare(&m(0.0, Some(0.1)), &m(1.0, Some(0.1)), 0.01), Verdict::NoNoiseEstimate);
    }

    #[test]
    fn t_critical_follows_the_table_and_rounds_df_down() {
        assert_eq!(t_critical_99(1.0), 63.657);
        assert_eq!(t_critical_99(8.0), 3.355);
        // Fractional df rounds DOWN (the wider gate).
        assert_eq!(t_critical_99(8.9), 3.355);
        assert_eq!(t_critical_99(29.99), 2.756);
        assert_eq!(t_critical_99(30.0), 2.750);
        assert!((t_critical_99(40.0) - 2.704).abs() < 1e-9);
        assert!((t_critical_99(120.0) - 2.617).abs() < 1e-9);
        // Between tabulated points (exact t(0.995, 50) = 2.678).
        assert!((t_critical_99(50.0) - 2.678).abs() < 0.002);
        // Toward the normal quantile.
        assert!((t_critical_99(1e9) - 2.576).abs() < 1e-3);
        // Never NaN / out of bounds on nonsense input.
        assert_eq!(t_critical_99(f64::NAN), 63.657);
        assert_eq!(t_critical_99(0.5), 63.657);
        // Monotone non-increasing in df.
        let mut prev = f64::INFINITY;
        for df in 1..400 {
            let t = t_critical_99(f64::from(df));
            assert!(t <= prev, "t({df}) = {t} > {prev}");
            prev = t;
        }
    }

    #[test]
    fn welch_df_matches_hand_computation() {
        // Equal spreads and sizes: df = 2(n-1).
        assert!((welch_df(1.0, 5, 1.0, 5) - 8.0).abs() < 1e-9);
        // One side with no spread: df = the other side's n-1.
        assert!((welch_df(1.0, 5, 0.0, 9) - 4.0).abs() < 1e-9);
        // se 1 and 2 over 5 and 10 samples: (1+4)^2 / (1/4 + 16/9).
        assert!((welch_df(1.0, 5, 2.0, 10) - 25.0 / (0.25 + 16.0 / 9.0)).abs() < 1e-9);
    }

    #[test]
    fn aa_false_wins_stay_near_one_percent() {
        // Identical distributions, the default 5 runs a side, no floor: the
        // gate's own false-win rate. 99% confidence → ~1%.
        let rate = win_rate(5, 1.0, 1.0, 0.0, 0.0, 0x5eed);
        assert!(rate < 0.015, "false-win rate {rate}");
    }

    #[test]
    fn unequal_spread_aa_false_wins_stay_near_one_percent() {
        // THE regression: identical means, but B's samples barely vary (a
        // quiet side) while A's do. The old `2 * (se_a + se_b)` gate collapsed
        // to ~2 * se_a and called ~11% of these identical pairs a win.
        let rate = win_rate(5, 1.0, 0.01, 0.0, 0.0, 0xa11ce);
        assert!(rate < 0.02, "false-win rate {rate}");
    }

    #[test]
    fn a_real_difference_is_still_found() {
        // B 5% slower with a 1% spread per run: found every time at n = 5.
        assert!(win_rate(5, 1.0, 1.0, 5.0, 0.03, 0xb0b) > 0.99);
        // A 2x difference cannot be missed.
        assert_eq!(win_rate(5, 1.0, 1.0, 100.0, 0.03, 0xb0b), 1.0);
    }

    #[test]
    fn render_faster_exact_string() {
        let v = Verdict::Faster {
            winner: 'B',
            ratio: 1.42,
            delta: 4.2,
            gate: 0.31,
        };
        assert_eq!(
            render_verdict(&v, "s", 2, HINT, FLOOR_HINT),
            "B is 1.42x faster than A (delta 4.20s > noise gate 0.31s; significant)."
        );
    }

    #[test]
    fn render_within_noise_exact_string() {
        let v = Verdict::WithinNoise {
            delta_pct: 0.8,
            gate_pct: 2.1,
            floor_bound: false,
        };
        assert_eq!(
            render_verdict(&v, "s", 4, HINT, FLOOR_HINT),
            "A and B differ by 0.8% which is INSIDE the measurement noise (gate 2.1%). \
             Do NOT claim a speedup from this data. To resolve: increase repeat, \
             enlarge the workload, or close background load."
        );
    }

    #[test]
    fn render_floor_bound_within_noise_does_not_ask_for_more_runs() {
        // More repeats cannot shrink a floor: the advice must change with it.
        let v = Verdict::WithinNoise {
            delta_pct: 2.1,
            gate_pct: 3.0,
            floor_bound: true,
        };
        assert_eq!(
            render_verdict(&v, "s", 4, HINT, FLOOR_HINT),
            "A and B differ by 2.1% which is INSIDE the measurement noise (gate 3.0%). \
             Do NOT claim a speedup from this data. To resolve: benchmark it in-process."
        );
    }

    #[test]
    fn render_no_noise_estimate_exact_string() {
        assert_eq!(
            render_verdict(&Verdict::NoNoiseEstimate, "s", 4, HINT, FLOOR_HINT),
            "No noise estimate (single run or unmeasured dispersion) — the difference \
             cannot be verified. To resolve: increase repeat, enlarge the workload, \
             or close background load."
        );
    }

    #[test]
    fn render_invalid_comparison_exact_string() {
        let v = Verdict::InvalidComparison {
            side: 'B',
            failed: 3,
            total: 5,
        };
        assert_eq!(
            render_verdict(&v, "s", 4, HINT, FLOOR_HINT),
            "INVALID COMPARISON: B exited non-zero on 3/5 run(s) — timings of a failing \
             command are meaningless; fix the command first."
        );
    }

    #[test]
    fn render_ns_precision() {
        let v = Verdict::Faster {
            winner: 'A',
            ratio: 3.0,
            delta: 4200.0,
            gate: 120.0,
        };
        assert_eq!(
            render_verdict(&v, "ns", 1, "increase repetitions", FLOOR_HINT),
            "A is 3.00x faster than B (delta 4200.0ns > noise gate 120.0ns; significant)."
        );
    }

    #[test]
    fn summary_with_and_without_noise() {
        let a = Measurement::from_summary(0.0123, Some(0.0001), 5, 0);
        assert_eq!(a.summary("A", "s", 4), "A: mean 0.0123s over 5 run(s) (±0.0001s stderr)");
        let b = Measurement::from_summary(0.5, None, 1, 0);
        assert_eq!(b.summary("B", "s", 4), "B: mean 0.5000s over 1 run(s)");
    }

    #[test]
    fn summary_notes_failures() {
        let a = Measurement::from_summary(0.5, Some(0.01), 5, 2);
        assert_eq!(
            a.summary("A", "s", 4),
            "A: mean 0.5000s over 5 run(s) (±0.0100s stderr) (2 run(s) exited non-zero)"
        );
    }
}
