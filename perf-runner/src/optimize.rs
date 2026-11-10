//! `perf-runner optimize`: the verified-optimisation loop.
//!
//! An agent proposes several patches; picking the best of several noisy
//! measurements manufactures winners (the winner's curse). So:
//! 1. explore: each candidate vs base, a few rounds, no claims made;
//! 2. select the best by its explore estimate and FREEZE it (commit + hash of
//!    the benchmark binary);
//! 3. confirm the frozen choice on fresh measurements (new process, new
//!    random order) at the gate's confidence. Only a confirmed FASTER counts.
//!
//! `--calibrate-aa N` runs the whole loop N times with every candidate being
//! the base itself, built separately: the share that "confirms" is the loop's
//! false-win rate.

use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;

use crate::campaign;
use crate::gitbuild;
use crate::report::Report;
use crate::report::SideInfo;
use crate::report::Verdict;
use crate::stats::Better;

pub struct OptArgs {
    pub repo: PathBuf,
    pub base: String,
    pub candidates: Vec<String>,
    pub build: String,
    pub bench: String,
    pub explore_rounds: usize,
    pub confirm_rounds: usize,
    pub confidence: f64,
    pub floor_pct: f64,
    pub primary: Option<String>,
    pub calibrate_aa: Option<usize>,
    pub aa_candidates: usize,
    pub keep: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Explored {
    pub candidate: String,
    pub commit: Option<String>,
    pub verdict: Verdict,
    /// Primary metric, signed so that positive = better.
    pub improvement_pct: Option<f64>,
    pub report: Report,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoopResult {
    pub outcome: &'static str,
    pub reason: String,
    pub explored: Vec<Explored>,
    pub chosen: Option<String>,
    pub frozen_commit: Option<String>,
    pub frozen_binary_sha256: Option<String>,
    /// The chosen candidate's improvement as exploration saw it, and as the
    /// fresh confirmation measured it (the gap is the winner's curse).
    pub explore_improvement_pct: Option<f64>,
    pub confirm_improvement_pct: Option<f64>,
    pub confirm: Option<Report>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OptReport {
    pub schema: &'static str,
    pub mode: &'static str,
    pub base: SideInfo,
    pub loops: Vec<LoopResult>,
    /// Calibration only: confirmed wins / loops, with the exact 95% upper bound.
    pub false_win_rate: Option<f64>,
    pub false_win_upper95: Option<f64>,
    pub mean_explore_improvement_pct: Option<f64>,
    pub mean_confirm_improvement_pct: Option<f64>,
    pub started_utc: String,
    pub finished_utc: String,
}

/// The primary metric's change in the good direction (positive = better).
pub fn improvement(r: &Report) -> Option<f64> {
    let p = r.primary_metric.as_ref()?;
    let m = r.metrics.iter().find(|m| &m.name == p)?;
    if !m.change_pct.is_finite() {
        return None;
    }
    match m.better {
        Better::Lower => Some(-m.change_pct),
        Better::Higher => Some(m.change_pct),
        Better::Info => None,
    }
}

/// Exact one-sided 95% upper bound of a binomial rate (Clopper-Pearson).
pub fn upper95(k: usize, n: usize) -> f64 {
    if n == 0 {
        return 1.0;
    }
    if k >= n {
        return 1.0;
    }
    // Bisection on P(X <= k | p) = 0.05.
    let cdf = |p: f64| {
        let mut term = (1.0 - p).powi(n as i32);
        let mut sum = term;
        for i in 0..k {
            term *= (n - i) as f64 / (i + 1) as f64 * p / (1.0 - p);
            sum += term;
        }
        sum
    };
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if cdf(mid) > 0.05 { lo = mid } else { hi = mid }
    }
    hi
}

fn sha256_of(path: &Path) -> Option<String> {
    let out = std::process::Command::new("sha256sum").arg(path).output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(str::to_string)
}

fn bench_binary(dir: &Path, bench: &str) -> Option<PathBuf> {
    let argv = crate::spawn::split_command(bench).ok()?;
    let p = PathBuf::from(&argv[0]);
    Some(if p.is_absolute() { p } else { dir.join(p) })
}

struct Tree {
    info: SideInfo,
    dir: PathBuf,
}

fn one_loop(a: &OptArgs, base: &Tree, cands: &[Tree], exe: &Path) -> LoopResult {
    let d = campaign::Options::default();
    let opts = |b: &Tree, rounds: usize, confidence: f64, tma: bool| campaign::Options {
        source: campaign::Source::Prepared { a: base.info.clone(), a_dir: base.dir.clone(), b: b.info.clone(), b_dir: b.dir.clone(), bench: a.bench.clone() },
        rounds,
        confidence,
        floor_pct: a.floor_pct,
        primary: a.primary.clone(),
        tma,
        ..campaign::Options::default()
    };
    let mut explored = Vec::new();
    for c in cands {
        eprintln!("perf-runner optimize: explore {}", c.info.label);
        // Exploration makes no claims; its confidence only shapes the report.
        let r = campaign::run(&opts(c, a.explore_rounds, d.confidence, false), exe);
        explored.push(Explored { candidate: c.info.label.clone(), commit: c.info.commit.clone(), verdict: r.verdict, improvement_pct: improvement(&r), report: r });
    }
    let usable = |e: &&Explored| !matches!(e.verdict, Verdict::NotRun | Verdict::Invalid | Verdict::Rejected) && e.improvement_pct.is_some();
    let best = explored.iter().enumerate().filter(|(_, e)| usable(e)).max_by(|x, y| x.1.improvement_pct.unwrap().total_cmp(&y.1.improvement_pct.unwrap()));
    let Some((bi, best)) = best else {
        let why = explored.iter().map(|e| format!("{}: {}", e.candidate, e.report.verdict_reason)).collect::<Vec<_>>().join("; ");
        return LoopResult { outcome: "NOT_RUN", reason: format!("no candidate could be measured ({why})"), explored, chosen: None, frozen_commit: None, frozen_binary_sha256: None, explore_improvement_pct: None, confirm_improvement_pct: None, confirm: None };
    };
    let explore_imp = best.improvement_pct;
    if explore_imp.unwrap_or(0.0) <= 0.0 {
        return LoopResult {
            outcome: "NO_CANDIDATE",
            reason: format!("no candidate looked better than base in exploration (best {:+.2}%)", explore_imp.unwrap_or(0.0)),
            chosen: None,
            frozen_commit: None,
            frozen_binary_sha256: None,
            explore_improvement_pct: explore_imp,
            confirm_improvement_pct: None,
            confirm: None,
            explored,
        };
    }
    let chosen = &cands[bi];
    // Freeze: the commit is immutable; the binary hash proves nothing was
    // rebuilt or swapped between exploration and confirmation.
    let bin = bench_binary(&chosen.dir, &a.bench);
    let frozen = bin.as_deref().and_then(sha256_of);
    eprintln!("perf-runner optimize: confirm {} on fresh measurements", chosen.info.label);
    let conf = campaign::run(&opts(chosen, a.confirm_rounds, a.confidence, true), exe);
    let again = bin.as_deref().and_then(sha256_of);
    let confirm_imp = improvement(&conf);
    let (outcome, reason) = if frozen.is_none() || frozen != again {
        ("NOT_CONFIRMED", "the chosen binary changed between exploration and confirmation".to_string())
    } else {
        match conf.verdict {
            Verdict::Faster => ("CONFIRMED", format!("confirmed on fresh measurements: {}", conf.verdict_reason)),
            _ => ("NOT_CONFIRMED", format!("exploration said {:+.2}%, confirmation: {} ({})", explore_imp.unwrap_or(0.0), conf.verdict.word(), conf.verdict_reason)),
        }
    };
    LoopResult {
        outcome,
        reason,
        chosen: Some(chosen.info.label.clone()),
        frozen_commit: chosen.info.commit.clone(),
        frozen_binary_sha256: frozen,
        explore_improvement_pct: explore_imp,
        confirm_improvement_pct: confirm_imp,
        confirm: Some(conf),
        explored,
    }
}

pub fn run(a: &OptArgs, exe: &Path) -> Result<OptReport, String> {
    let started_utc = crate::host::now_utc();
    let (creds, env) = campaign::job_context();
    let stamp = format!("opt-{}-{}", started_utc.replace([':', '-'], ""), std::process::id());
    let ws = crate::fsutil::workspace(&stamp, creds.as_ref())?;
    let run_dir = ws.run_dir.clone();
    let calibrating = a.calibrate_aa.is_some();
    let refs: Vec<String> = if calibrating { vec![a.base.clone(); a.aa_candidates.max(2)] } else { a.candidates.clone() };
    if refs.is_empty() {
        return Err("give at least one --candidate (or --calibrate-aa N)".to_string());
    }
    let mut worktrees = Vec::new();
    let mut build = |slot: String, reference: &str| -> Result<Tree, String> {
        let dest = ws.trees.join(&slot);
        let commit = gitbuild::add_worktree(&a.repo, reference, &dest, creds.as_ref(), &env)?;
        worktrees.push(dest.clone());
        eprintln!("perf-runner optimize: building {slot} = {reference} ({})", &commit[..commit.len().min(12)]);
        let info = gitbuild::build(&dest, &a.build, creds.as_ref(), &env, &run_dir.join(format!("build-{slot}.log")));
        if !info.ok {
            return Err(format!("build of {reference} failed:\n{}", info.log_tail));
        }
        Ok(Tree {
            info: SideInfo { label: if calibrating { format!("{slot} (= base)") } else { reference.to_string() }, reference: Some(reference.to_string()), commit: Some(commit), build: Some(info), ..Default::default() },
            dir: dest,
        })
    };
    let result = (|| -> Result<(Tree, Vec<Tree>), String> {
        let base = build("base".to_string(), &a.base)?;
        let mut cands = Vec::new();
        for (i, r) in refs.iter().enumerate() {
            cands.push(build(format!("cand{}", i + 1), r)?);
        }
        Ok((base, cands))
    })();
    let cleanup = |w: &[PathBuf]| {
        if !a.keep {
            for d in w {
                gitbuild::remove_worktree(&a.repo, d, creds.as_ref(), &env);
            }
            let _ = std::fs::remove_dir_all(&run_dir);
            let _ = std::fs::remove_dir_all(&ws.scratch);
        }
    };
    let (base, cands) = match result {
        Ok(x) => x,
        Err(e) => {
            cleanup(&worktrees);
            return Err(e);
        }
    };
    let n = a.calibrate_aa.unwrap_or(1).max(1);
    let mut loops = Vec::new();
    for i in 0..n {
        if calibrating {
            eprintln!("perf-runner optimize: A/A loop {}/{n}", i + 1);
        }
        loops.push(one_loop(a, &base, &cands, exe));
    }
    cleanup(&worktrees);
    let (mut fwr, mut up, mut me, mut mc) = (None, None, None, None);
    if calibrating {
        let measured: Vec<&LoopResult> = loops.iter().filter(|l| l.outcome != "NOT_RUN").collect();
        let wins = measured.iter().filter(|l| l.outcome == "CONFIRMED").count();
        fwr = Some(wins as f64 / measured.len().max(1) as f64);
        up = Some(upper95(wins, measured.len()));
        let ex: Vec<f64> = measured.iter().filter_map(|l| l.explore_improvement_pct).collect();
        let co: Vec<f64> = measured.iter().filter_map(|l| l.confirm_improvement_pct).collect();
        me = (!ex.is_empty()).then(|| crate::stats::mean(&ex));
        mc = (!co.is_empty()).then(|| crate::stats::mean(&co));
    }
    Ok(OptReport {
        schema: "byteask.perflab.optimize/1",
        mode: if calibrating { "calibrate-aa" } else { "optimize" },
        base: base.info,
        loops,
        false_win_rate: fwr,
        false_win_upper95: up,
        mean_explore_improvement_pct: me,
        mean_confirm_improvement_pct: mc,
        started_utc,
        finished_utc: crate::host::now_utc(),
    })
}

pub fn summary(r: &OptReport) -> Vec<String> {
    let mut s = Vec::new();
    if let (Some(f), Some(u)) = (r.false_win_rate, r.false_win_upper95) {
        let measured = r.loops.iter().filter(|l| l.outcome != "NOT_RUN").count();
        s.push(format!(
            "A/A calibration of the loop: {} confirmed false wins in {measured} loops ({:.1}%, 95% upper bound {:.1}%)",
            r.loops.iter().filter(|l| l.outcome == "CONFIRMED").count(),
            f * 100.0,
            u * 100.0
        ));
        if let (Some(e), Some(c)) = (r.mean_explore_improvement_pct, r.mean_confirm_improvement_pct) {
            s.push(format!("winner's curse: the chosen (identical) candidate looked {e:+.2}% better in exploration, {c:+.2}% on confirmation"));
        }
    }
    for (i, l) in r.loops.iter().enumerate().take(if r.loops.len() > 3 { 0 } else { 3 }) {
        s.push(format!("loop {}: {} — {}", i + 1, l.outcome, l.reason));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clopper_pearson_upper_bounds() {
        // 0 of 60 -> about 4.9%; 0 of 100 -> about 2.95%; 1 of 100 -> about 4.7%.
        assert!((upper95(0, 60) - 0.0487).abs() < 0.001, "{}", upper95(0, 60));
        assert!((upper95(0, 100) - 0.0295).abs() < 0.001);
        assert!((upper95(1, 100) - 0.0466).abs() < 0.002, "{}", upper95(1, 100));
        assert_eq!(upper95(5, 5), 1.0);
    }

    #[test]
    fn improvement_sign_follows_direction() {
        let opts = campaign::Options::default();
        let mut r = campaign::empty_report(&opts);
        r.primary_metric = Some("p50_ns".into());
        r.metrics = vec![crate::stats::analyse("p50_ns", Better::Lower, None, &[100.0; 8], &[90.0; 8], 0.95, 2.0)];
        assert!((improvement(&r).unwrap() - 10.0).abs() < 1e-9);
        r.primary_metric = Some("throughput".into());
        r.metrics = vec![crate::stats::analyse("throughput", Better::Higher, None, &[100.0; 8], &[90.0; 8], 0.95, 2.0)];
        assert!((improvement(&r).unwrap() + 10.0).abs() < 1e-9);
    }
}
