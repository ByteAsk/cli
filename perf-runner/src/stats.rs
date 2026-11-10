//! Statistics for interleaved A/B rounds.
//!
//! The unit of replication is a ROUND: one run of A and one run of B, in an
//! order drawn at random per round. Iterations inside a run are never pooled
//! across runs (layout/ASLR effects make whole runs differ; pooling iterations
//! makes a confidence interval look far tighter than it is).
//!
//! Per metric we report:
//! - `ratio` = median(B rounds) / median(A rounds);
//! - a paired percentile bootstrap CI of that ratio (resampling whole rounds);
//! - an exact sign-flip randomisation test on the per-round log ratios. Under
//!   A/A the sign of `ln(B_i / A_i)` is symmetric because the order inside each
//!   round is random, whatever the noise distribution, drift or carry-over, so
//!   the test's false-positive rate is at most its level by construction.
//!
//! A verdict needs BOTH (CI excludes 1.0 and p < alpha) plus the floor.

/// Deterministic xorshift64* generator (no external crates; reproducible
/// bootstrap CIs from the same rounds).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, n).
    pub fn below(&mut self, n: usize) -> usize {
        (((self.next_u64() >> 11) as u128 * n as u128) >> 53) as usize
    }

    /// Uniform in (0, 1).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit(), self.unit());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        0.5 * (s[n / 2 - 1] + s[n / 2])
    }
}

pub fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// Sample standard deviation (n - 1).
pub fn sd(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 2 {
        return f64::NAN;
    }
    let m = mean(v);
    (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n as f64 - 1.0)).sqrt()
}

/// Empirical quantile with linear interpolation (type 7), `q` in [0, 1].
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let h = (sorted.len() - 1) as f64 * q.clamp(0.0, 1.0);
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    sorted[lo] + (h - lo as f64) * (sorted[hi] - sorted[lo])
}

/// Paired percentile bootstrap CI of median(B)/median(A), resampling whole
/// rounds (index i keeps A_i with B_i, so drift cancels as in the test).
pub fn bootstrap_ratio_ci(a: &[f64], b: &[f64], level: f64, resamples: usize, seed: u64) -> (f64, f64) {
    assert_eq!(a.len(), b.len());
    let n = a.len();
    let mut rng = Rng::new(seed);
    let mut ratios = Vec::with_capacity(resamples);
    let mut ra = vec![0.0; n];
    let mut rb = vec![0.0; n];
    for _ in 0..resamples {
        for k in 0..n {
            let i = rng.below(n);
            ra[k] = a[i];
            rb[k] = b[i];
        }
        let ma = median(&ra);
        let mb = median(&rb);
        if ma > 0.0 {
            ratios.push(mb / ma);
        }
    }
    ratios.sort_by(f64::total_cmp);
    let tail = (1.0 - level) / 2.0;
    (quantile(&ratios, tail), quantile(&ratios, 1.0 - tail))
}

/// Two-sided sign-flip randomisation test of "the d_i are symmetric about 0",
/// statistic |sum d_i|. Exact (full enumeration) for n <= 20, Monte Carlo
/// with the +1 correction above that. Returns 1.0 for n == 0.
pub fn sign_flip_p(d: &[f64], seed: u64) -> f64 {
    let n = d.len();
    if n == 0 {
        return 1.0;
    }
    let obs = d.iter().sum::<f64>().abs();
    // Ties: count permutations whose statistic is >= observed minus a relative
    // epsilon, so floating-point summation order never makes p smaller.
    let eps = 1e-12 * d.iter().map(|x| x.abs()).sum::<f64>().max(1e-300);
    if n <= 20 {
        let total: u64 = 1 << n;
        let mut extreme: u64 = 0;
        for mask in 0..total {
            let mut s = 0.0;
            for (i, x) in d.iter().enumerate() {
                if mask >> i & 1 == 1 {
                    s -= x;
                } else {
                    s += x;
                }
            }
            if s.abs() >= obs - eps {
                extreme += 1;
            }
        }
        extreme as f64 / total as f64
    } else {
        const DRAWS: u64 = 200_000;
        let mut rng = Rng::new(seed);
        let mut extreme: u64 = 0;
        for _ in 0..DRAWS {
            let mut s = 0.0;
            let mut bits = 0u64;
            for (i, x) in d.iter().enumerate() {
                if i % 64 == 0 {
                    bits = rng.next_u64();
                }
                if bits >> (i % 64) & 1 == 1 {
                    s -= x;
                } else {
                    s += x;
                }
            }
            if s.abs() >= obs - eps {
                extreme += 1;
            }
        }
        (extreme + 1) as f64 / (DRAWS + 1) as f64
    }
}

/// Smallest p-value the sign-flip test can produce with `n` rounds.
pub fn sign_flip_min_p(n: usize) -> f64 {
    2.0 / 2f64.powi(n as i32)
}

/// Mann-Whitney U two-sided p-value (normal approximation with tie and
/// continuity correction). Reported only; the decision uses the paired test.
pub fn mann_whitney_p(a: &[f64], b: &[f64]) -> f64 {
    let (n1, n2) = (a.len(), b.len());
    if n1 == 0 || n2 == 0 {
        return 1.0;
    }
    let mut all: Vec<(f64, usize)> = a.iter().map(|&x| (x, 0)).chain(b.iter().map(|&x| (x, 1))).collect();
    all.sort_by(|x, y| x.0.total_cmp(&y.0));
    let n = all.len();
    let mut ranks = vec![0.0; n];
    let mut tie_term = 0.0;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && all[j + 1].0 == all[i].0 {
            j += 1;
        }
        let r = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            ranks[k] = r;
        }
        let t = (j - i + 1) as f64;
        tie_term += t * t * t - t;
        i = j + 1;
    }
    let r1: f64 = all.iter().zip(&ranks).filter(|(x, _)| x.1 == 0).map(|(_, r)| r).sum();
    let (n1f, n2f) = (n1 as f64, n2 as f64);
    let u1 = r1 - n1f * (n1f + 1.0) / 2.0;
    let mu = n1f * n2f / 2.0;
    let nf = n as f64;
    let var = n1f * n2f / 12.0 * ((nf + 1.0) - tie_term / (nf * (nf - 1.0)));
    if var <= 0.0 {
        return 1.0;
    }
    let z = ((u1 - mu).abs() - 0.5).max(0.0) / var.sqrt();
    (2.0 * (1.0 - normal_cdf(z))).clamp(0.0, 1.0)
}

pub fn normal_cdf(z: f64) -> f64 {
    0.5 * erfc(-z / std::f64::consts::SQRT_2)
}

/// Complementary error function (Numerical Recipes erfcc, |error| < 1.2e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// Inverse standard normal CDF (Acklam's rational approximation, ~1e-9).
pub fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let p = p.clamp(1e-300, 1.0 - 1e-16);
    let plow = 0.02425;
    if p < plow {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - plow {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

/// Smallest relative change (percent) a paired test at level `alpha` would
/// detect with 80% power, given the round-to-round SD of the log ratios.
pub fn min_detectable_pct(sd_log: f64, n: usize, alpha: f64) -> f64 {
    if !(sd_log.is_finite()) || n < 2 {
        return f64::NAN;
    }
    let z = normal_quantile(1.0 - alpha / 2.0) + normal_quantile(0.8);
    ((z * sd_log / (n as f64).sqrt()).exp() - 1.0) * 100.0
}

/// Rounds needed so that `floor_pct` is detectable with 80% power.
pub fn rounds_needed(sd_log: f64, floor_pct: f64, alpha: f64) -> Option<usize> {
    if !(sd_log.is_finite()) || floor_pct <= 0.0 {
        return None;
    }
    let z = normal_quantile(1.0 - alpha / 2.0) + normal_quantile(0.8);
    let eff = (1.0 + floor_pct / 100.0).ln();
    Some(((z * sd_log / eff).powi(2)).ceil().max(2.0) as usize)
}

/// Which way is better for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    Lower,
    Higher,
    /// Reported only, no verdict.
    Info,
}

impl MetricVerdict {
    pub fn word(self) -> &'static str {
        match self {
            MetricVerdict::Faster => "FASTER",
            MetricVerdict::Slower => "SLOWER",
            MetricVerdict::NoClaim => "NO CLAIM",
            MetricVerdict::NotRun => "NOT RUN",
            MetricVerdict::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum MetricVerdict {
    #[serde(rename = "FASTER")]
    Faster,
    #[serde(rename = "SLOWER")]
    Slower,
    #[serde(rename = "NO_CLAIM")]
    NoClaim,
    #[serde(rename = "NOT_RUN")]
    NotRun,
    #[serde(rename = "INFO")]
    Info,
}

/// Everything the report says about one metric.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricResult {
    pub name: String,
    pub better: Better,
    pub unit: Option<String>,
    pub n: usize,
    pub a_median: f64,
    pub b_median: f64,
    pub ratio: f64,
    pub change_pct: f64,
    pub ci: [f64; 2],
    pub ci_level: f64,
    pub p_value: f64,
    pub mwu_p_value: f64,
    pub noise_pct: f64,
    pub min_detectable_pct: f64,
    pub rounds_needed_for_floor: Option<usize>,
    pub verdict: MetricVerdict,
    pub reason: String,
}

pub const BOOTSTRAP_RESAMPLES: usize = 10_000;
pub const BOOTSTRAP_SEED: u64 = 0x5EED_1E55;

/// Analyse one metric over paired rounds. `a[i]`/`b[i]` are round i.
pub fn analyse(name: &str, better: Better, unit: Option<String>, a: &[f64], b: &[f64], confidence: f64, floor_pct: f64) -> MetricResult {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let alpha = 1.0 - confidence;
    let a_med = median(a);
    let b_med = median(b);
    let ratio = b_med / a_med;
    if better == Better::Info {
        // Reported only: medians and their ratio, no test.
        let ratio = if a_med == b_med { 1.0 } else { b_med / a_med };
        return MetricResult {
            name: name.to_string(),
            better,
            unit,
            n,
            a_median: a_med,
            b_median: b_med,
            ratio,
            change_pct: (ratio - 1.0) * 100.0,
            ci: [f64::NAN, f64::NAN],
            ci_level: confidence,
            p_value: f64::NAN,
            mwu_p_value: f64::NAN,
            noise_pct: f64::NAN,
            min_detectable_pct: f64::NAN,
            rounds_needed_for_floor: None,
            verdict: MetricVerdict::Info,
            reason: "reported only (no direction from its name)".to_string(),
        };
    }
    let logs: Vec<f64> = a.iter().zip(b).map(|(x, y)| (y / x).ln()).collect();
    let valid = n >= 2 && a.iter().chain(b).all(|v| v.is_finite() && *v > 0.0);
    let mut r = MetricResult {
        name: name.to_string(),
        better,
        unit,
        n,
        a_median: a_med,
        b_median: b_med,
        ratio,
        change_pct: (ratio - 1.0) * 100.0,
        ci: [f64::NAN, f64::NAN],
        ci_level: confidence,
        p_value: f64::NAN,
        mwu_p_value: f64::NAN,
        noise_pct: f64::NAN,
        min_detectable_pct: f64::NAN,
        rounds_needed_for_floor: None,
        verdict: MetricVerdict::NotRun,
        reason: String::new(),
    };
    if !valid {
        r.reason = if n < 2 {
            format!("only {n} round(s) carry this metric")
        } else {
            "a value is zero, negative or not a number, so ratios are undefined".to_string()
        };
        return r;
    }
    let (lo, hi) = bootstrap_ratio_ci(a, b, confidence, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED);
    r.ci = [lo, hi];
    r.p_value = sign_flip_p(&logs, BOOTSTRAP_SEED);
    r.mwu_p_value = mann_whitney_p(a, b);
    let sd_log = sd(&logs);
    r.noise_pct = (sd_log.exp() - 1.0) * 100.0;
    r.min_detectable_pct = min_detectable_pct(sd_log, n, alpha);
    r.rounds_needed_for_floor = rounds_needed(sd_log, floor_pct, alpha);
    let (v, reason) = decide(better, ratio, (lo, hi), r.p_value, alpha, floor_pct, n);
    r.verdict = v;
    r.reason = reason;
    r
}

/// The verdict rule, kept pure so every branch is tested.
pub fn decide(better: Better, ratio: f64, ci: (f64, f64), p: f64, alpha: f64, floor_pct: f64, n: usize) -> (MetricVerdict, String) {
    let change = (ratio - 1.0) * 100.0;
    let level = (1.0 - alpha) * 100.0;
    let ci_txt = format!(
        "{level:.0}% CI {:+.1}% to {:+.1}%",
        (ci.0 - 1.0) * 100.0,
        (ci.1 - 1.0) * 100.0
    );
    if better == Better::Info {
        return (MetricVerdict::Info, format!("B/A {change:+.1}% ({ci_txt}); reported only"));
    }
    if sign_flip_min_p(n) >= alpha {
        return (
            MetricVerdict::NoClaim,
            format!("{n} rounds cannot reach p < {alpha} with a sign-flip test; add rounds"),
        );
    }
    let excludes_one = ci.0 > 1.0 || ci.1 < 1.0;
    let significant = excludes_one && p < alpha;
    let big_enough = change.abs() >= floor_pct;
    if !significant {
        return (
            MetricVerdict::NoClaim,
            format!("inside the noise: B/A {change:+.1}% ({ci_txt}, p={p:.3})"),
        );
    }
    if !big_enough {
        return (
            MetricVerdict::NoClaim,
            format!(
                "below the minimum-effect floor: B/A {change:+.1}% is significant ({ci_txt}, p={p:.4}) but smaller than {floor_pct}%"
            ),
        );
    }
    let b_better = match better {
        Better::Lower => ratio < 1.0,
        Better::Higher => ratio > 1.0,
        Better::Info => unreachable!(),
    };
    let pct = if better == Better::Lower {
        (1.0 - ratio) * 100.0
    } else {
        (ratio - 1.0) * 100.0
    };
    if b_better {
        (
            MetricVerdict::Faster,
            format!("B is {:.1}% better ({ci_txt}, p={p:.4}, floor {floor_pct}%)", pct.abs()),
        )
    } else {
        (
            MetricVerdict::Slower,
            format!("B is {:.1}% worse ({ci_txt}, p={p:.4}, floor {floor_pct}%)", pct.abs()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_quantile() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        let s = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(quantile(&s, 0.5), 3.0);
        assert_eq!(quantile(&s, 0.0), 1.0);
        assert_eq!(quantile(&s, 1.0), 5.0);
        assert!((quantile(&s, 0.9) - 4.6).abs() < 1e-12);
    }

    #[test]
    fn sign_flip_exact_small_cases() {
        // All 4 positive: only all-same-sign flips (2 of 16) are as extreme.
        assert!((sign_flip_p(&[1.0, 1.0, 1.0, 1.0], 1) - 2.0 / 16.0).abs() < 1e-12);
        // Symmetric data: p = 1.
        assert!((sign_flip_p(&[1.0, -1.0], 1) - 1.0).abs() < 1e-12);
        assert_eq!(sign_flip_min_p(12), 2.0 / 4096.0);
    }

    #[test]
    fn sign_flip_monte_carlo_agrees_with_exact() {
        let mut rng = Rng::new(7);
        let d: Vec<f64> = (0..20).map(|_| rng.normal() + 0.3).collect();
        let exact = sign_flip_p(&d, 3);
        // Same data padded to 21 with a zero changes nothing in distribution.
        let mut d21 = d.clone();
        d21.push(0.0);
        let mc = sign_flip_p(&d21, 3);
        assert!((exact - mc).abs() < 0.01, "exact {exact} mc {mc}");
    }

    #[test]
    fn normal_functions() {
        assert!((normal_quantile(0.975) - 1.959964).abs() < 1e-5);
        assert!((normal_quantile(0.5)).abs() < 1e-9);
        assert!((normal_cdf(1.959964) - 0.975).abs() < 1e-6);
        assert!((normal_quantile(0.995) - 2.575829).abs() < 1e-5);
    }

    #[test]
    fn mann_whitney_separated_and_identical() {
        let a: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect();
        let b: Vec<f64> = (0..12).map(|i| 200.0 + i as f64).collect();
        assert!(mann_whitney_p(&a, &b) < 1e-4);
        assert!(mann_whitney_p(&a, &a) > 0.9);
    }

    #[test]
    fn decide_branches() {
        let a = 0.05;
        // Clear win on a lower-is-better metric.
        let (v, _) = decide(Better::Lower, 0.8, (0.78, 0.82), 0.0005, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::Faster);
        // Clear loss.
        let (v, _) = decide(Better::Lower, 1.1, (1.08, 1.12), 0.0005, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::Slower);
        // Higher-is-better: ratio > 1 is a win.
        let (v, _) = decide(Better::Higher, 1.25, (1.2, 1.3), 0.0005, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::Faster);
        // CI includes 1.
        let (v, why) = decide(Better::Lower, 0.97, (0.93, 1.01), 0.04, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::NoClaim);
        assert!(why.starts_with("inside the noise"), "{why}");
        // CI excludes 1 but the randomisation test does not agree.
        let (v, _) = decide(Better::Lower, 0.97, (0.95, 0.99), 0.08, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::NoClaim);
        // Significant but under the floor.
        let (v, why) = decide(Better::Lower, 0.99, (0.985, 0.995), 0.001, a, 2.0, 12);
        assert_eq!(v, MetricVerdict::NoClaim);
        assert!(why.starts_with("below the minimum-effect floor"), "{why}");
        // Too few rounds to ever be significant.
        let (v, _) = decide(Better::Lower, 0.5, (0.4, 0.6), 0.0, a, 2.0, 5);
        assert_eq!(v, MetricVerdict::NoClaim);
        // Info metrics never get a verdict.
        assert_eq!(decide(Better::Info, 0.5, (0.4, 0.6), 0.0, a, 2.0, 12).0, MetricVerdict::Info);
    }

    /// Simulate interleaved rounds: per-round machine drift (shared by A and
    /// B in that round), a carry-over penalty on whichever side runs first,
    /// per-run noise, and a true effect `shift` on B. Returns the share of
    /// trials whose verdict is FASTER, and the share SLOWER.
    fn campaign(rounds: usize, shift: f64, trials: usize, seed: u64, heavy_tail: bool) -> (f64, f64) {
        let mut rng = Rng::new(seed);
        let (mut faster, mut slower) = (0, 0);
        for _ in 0..trials {
            let mut order: Vec<bool> = (0..rounds).map(|i| i % 2 == 0).collect();
            rng.shuffle(&mut order);
            let mut a = Vec::new();
            let mut b = Vec::new();
            let mut drift = 0.0;
            for &a_first in &order {
                drift += 0.004 * rng.normal();
                let noise = |rng: &mut Rng| {
                    let base = 0.01 * rng.normal();
                    if heavy_tail && rng.unit() < 0.05 { base + 0.2 * rng.unit() } else { base }
                };
                let first_penalty = 0.02;
                let pa = if a_first { first_penalty } else { 0.0 };
                let pb = if a_first { 0.0 } else { first_penalty };
                a.push(100.0 * (drift + pa + noise(&mut rng)).exp());
                b.push(100.0 * (shift + drift + pb + noise(&mut rng)).exp());
            }
            let r = analyse("p50_ns", Better::Lower, None, &a, &b, 0.95, 2.0);
            match r.verdict {
                MetricVerdict::Faster => faster += 1,
                MetricVerdict::Slower => slower += 1,
                _ => {}
            }
        }
        (faster as f64 / trials as f64, slower as f64 / trials as f64)
    }

    #[test]
    fn aa_false_claims_stay_under_alpha_even_with_floor_zero() {
        // Floor 0 isolates the statistics: the sign-flip test alone must
        // keep the claim rate <= 5% at 95%, despite drift, carry-over and a
        // heavy tail.
        let mut rng = Rng::new(11);
        let trials = 1500;
        let mut claims = 0;
        for _ in 0..trials {
            let mut order: Vec<bool> = (0..12).map(|i| i % 2 == 0).collect();
            rng.shuffle(&mut order);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let mut drift = 0.0;
            for &a_first in &order {
                drift += 0.01 * rng.normal();
                let pen = 0.03;
                let tail = |rng: &mut Rng| if rng.unit() < 0.05 { 0.3 } else { 0.0 };
                a.push(100.0 * (drift + if a_first { pen } else { 0.0 } + 0.01 * rng.normal() + tail(&mut rng)).exp());
                b.push(100.0 * (drift + if a_first { 0.0 } else { pen } + 0.01 * rng.normal() + tail(&mut rng)).exp());
            }
            let r = analyse("x_ns", Better::Lower, None, &a, &b, 0.95, 0.0);
            if matches!(r.verdict, MetricVerdict::Faster | MetricVerdict::Slower) {
                claims += 1;
            }
        }
        let rate = claims as f64 / trials as f64;
        assert!(rate <= 0.05, "A/A claim rate {rate}");
    }

    #[test]
    fn aa_with_default_floor_never_claims() {
        let (f, s) = campaign(12, 0.0, 1000, 21, true);
        assert!(f + s <= 0.01, "faster {f} slower {s}");
    }

    #[test]
    fn real_changes_are_found() {
        // 8% slower, 20% faster: found essentially every time at 12 rounds.
        let (_, s) = campaign(12, (1.08f64).ln(), 400, 31, false);
        assert!(s > 0.97, "slower found {s}");
        let (f, _) = campaign(12, (0.80f64).ln(), 400, 41, false);
        assert!(f > 0.97, "faster found {f}");
    }

    #[test]
    fn pooling_iterations_would_have_lied() {
        // The reason runs are the unit: with a per-run offset (layout),
        // a naive comparison of pooled iterations calls identical code
        // different most of the time. Document it with numbers.
        let mut rng = Rng::new(5);
        let mut naive_claims = 0;
        let trials = 200;
        for _ in 0..trials {
            let mut pa = Vec::new();
            let mut pb = Vec::new();
            for _ in 0..6 {
                let off_a = 0.03 * rng.normal();
                let off_b = 0.03 * rng.normal();
                for _ in 0..200 {
                    pa.push(100.0 * (off_a + 0.01 * rng.normal()).exp());
                    pb.push(100.0 * (off_b + 0.01 * rng.normal()).exp());
                }
            }
            if mann_whitney_p(&pa, &pb) < 0.05 {
                naive_claims += 1;
            }
        }
        assert!(naive_claims as f64 / trials as f64 > 0.5);
    }

    #[test]
    fn bootstrap_ci_is_deterministic_and_brackets_the_ratio() {
        let a: Vec<f64> = (0..12).map(|i| 100.0 + (i % 3) as f64).collect();
        let b: Vec<f64> = a.iter().map(|x| x * 0.8).collect();
        let (lo, hi) = bootstrap_ratio_ci(&a, &b, 0.95, 2000, 9);
        assert!(lo <= 0.8 + 1e-9 && hi >= 0.8 - 1e-9, "{lo} {hi}");
        assert_eq!(bootstrap_ratio_ci(&a, &b, 0.95, 2000, 9), (lo, hi));
    }

    #[test]
    fn min_detectable_and_rounds_needed() {
        // sd 1% per round, 12 rounds, alpha 0.05: (1.96+0.84)*0.01/sqrt(12).
        let m = min_detectable_pct(0.01, 12, 0.05);
        assert!((m - 0.812).abs() < 0.02, "{m}");
        let n = rounds_needed(0.01, 2.0, 0.05).unwrap();
        assert!((2..=3).contains(&n), "{n}");
        assert!(rounds_needed(0.05, 2.0, 0.05).unwrap() > 40);
    }

    #[test]
    fn info_metrics_report_medians_even_at_zero() {
        let r = analyse("allocs_per_msg", Better::Info, None, &[0.0; 6], &[0.0; 6], 0.95, 2.0);
        assert_eq!(r.verdict, MetricVerdict::Info);
        assert_eq!(r.change_pct, 0.0);
    }

    #[test]
    fn analyse_rejects_nonpositive_values() {
        let r = analyse("x_ns", Better::Lower, None, &[1.0, 0.0, 1.0], &[1.0, 1.0, 1.0], 0.95, 2.0);
        assert_eq!(r.verdict, MetricVerdict::NotRun);
    }
}
