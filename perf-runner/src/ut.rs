//! `perf-runner ut`: preregistered performance unit tests for a
//! merge: base vs head, each ref built once, every test a full interleaved
//! comparison, then the customer's limits. Produces the pull-request perf
//! card and one JSON report.
//!
//! ```toml
//! # .byteask/perf-ut.toml
//! [defaults]
//! build = "cmake --preset prod && cmake --build --preset prod --target bench"
//! confidence = 0.99          # anything that gates a merge: A/A false claims <= 1%
//! rounds = 16
//! floor_pct = 2
//!
//! [[test]]
//! name = "feed handler hot path"
//! bench = "build/prod/bench --feed /data/feed.cap"
//! primary = "p50_ns"
//! guards = ["allocs_per_msg"]
//! check = "build/prod/replay_test"
//! max_regression_pct = { p50_ns = 2, p99_ns = 5 }   # relative limits
//! max = { p99_ns = 600 }                           # absolute limits (head median)
//! min = { throughput = 4.0e6 }
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::campaign;
use crate::gitbuild;
use crate::report::Report;
use crate::report::SideInfo;
use crate::report::Verdict;
use crate::stats::Better;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub build: Option<String>,
    pub confidence: Option<f64>,
    pub rounds: Option<usize>,
    pub floor_pct: Option<f64>,
    pub tma: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtTest {
    pub name: String,
    pub bench: String,
    pub build: Option<String>,
    pub primary: Option<String>,
    #[serde(default)]
    pub guards: Vec<String>,
    pub check: Option<String>,
    pub rounds: Option<usize>,
    pub confidence: Option<f64>,
    pub floor_pct: Option<f64>,
    #[serde(default)]
    pub metric_floor: BTreeMap<String, f64>,
    #[serde(default)]
    pub max_regression_pct: BTreeMap<String, f64>,
    #[serde(default)]
    pub max: BTreeMap<String, f64>,
    #[serde(default)]
    pub min: BTreeMap<String, f64>,
    pub tma: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UtFile {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(rename = "test", default)]
    pub tests: Vec<UtTest>,
}

pub fn parse(text: &str) -> Result<UtFile, String> {
    let f: UtFile = toml::from_str(text).map_err(|e| format!("perf-ut file: {e}"))?;
    if f.tests.is_empty() {
        return Err("perf-ut file has no [[test]]".to_string());
    }
    for t in &f.tests {
        if t.build.is_none() && f.defaults.build.is_none() {
            return Err(format!("test {:?} has no build command (and no [defaults] build)", t.name));
        }
    }
    Ok(f)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Gate {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "FAIL")]
    Fail,
    #[serde(rename = "NOT_RUN")]
    NotRun,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestOutcome {
    pub name: String,
    pub gate: Gate,
    pub reasons: Vec<String>,
    pub report: Report,
}

/// Apply the customer's limits to one comparison. Pure, so every rule is tested.
///
/// - not measured -> NOT_RUN; a failed run, changed output or a guard -> FAIL;
/// - a measured regression (the metric's own verdict SLOWER, which already
///   includes the floor) fails, unless the customer set a relative limit for
///   that metric, in which case only a measured regression beyond the limit
///   fails;
/// - absolute limits apply to the head's median whatever the statistics say;
/// - a limit on a metric the benchmark does not report fails (never silently passes).
pub fn gate(test: &UtTest, r: &Report) -> (Gate, Vec<String>) {
    let mut fails = Vec::new();
    match r.verdict {
        Verdict::NotRun => return (Gate::NotRun, vec![format!("not measured: {}", r.verdict_reason)]),
        Verdict::Invalid => return (Gate::Fail, vec![format!("invalid: {}", r.verdict_reason)]),
        Verdict::Rejected => return (Gate::Fail, vec![format!("rejected: {}", r.verdict_reason)]),
        _ => {}
    }
    // Every directional metric can fail the merge, so the A/A false-FAIL rate
    // is held at alpha for the whole family (Bonferroni): each metric's test
    // must reach alpha / k. Whole-process wall time never gates.
    let gated: Vec<&crate::stats::MetricResult> = r.metrics.iter().filter(|m| m.better != Better::Info && m.name != "wall_s").collect();
    let alpha = (1.0 - r.confidence) / gated.len().max(1) as f64;
    for m in &gated {
        let worse_pct = match m.better {
            Better::Lower => m.change_pct,
            _ => -m.change_pct,
        };
        let measured_worse = m.p_value < alpha
            && match m.better {
                Better::Lower => m.ci[0] > 1.0,
                _ => m.ci[1] < 1.0,
            };
        let ci = format!("CI {:+.1}% to {:+.1}%, p={:.4} < {alpha:.4}", (m.ci[0] - 1.0) * 100.0, (m.ci[1] - 1.0) * 100.0, m.p_value);
        let floor = test.metric_floor.get(&m.name).copied().unwrap_or(r.floor_pct);
        match test.max_regression_pct.get(&m.name) {
            Some(limit) => {
                if measured_worse && worse_pct > *limit {
                    fails.push(format!("{} regressed {worse_pct:.1}% (limit {limit}%; {ci})", m.name));
                }
            }
            None => {
                if measured_worse && worse_pct >= floor {
                    fails.push(format!("{} regressed {worse_pct:.1}% ({ci}); no limit set, so any measured regression of at least {floor}% fails", m.name));
                }
            }
        }
    }
    for m in &r.metrics {
        if let Some(max) = test.max.get(&m.name)
            && m.b_median > *max {
                fails.push(format!("{} = {:.1} on head, above the limit {max}", m.name, m.b_median));
            }
        if let Some(min) = test.min.get(&m.name)
            && m.b_median < *min {
                fails.push(format!("{} = {:.1} on head, below the limit {min}", m.name, m.b_median));
            }
    }
    for key in test.max_regression_pct.keys().chain(test.max.keys()).chain(test.min.keys()) {
        if !r.metrics.iter().any(|m| &m.name == key) {
            fails.push(format!("limit on {key} could not be checked: the benchmark does not report it"));
        }
    }
    if fails.is_empty() { (Gate::Pass, Vec::new()) } else { (Gate::Fail, fails) }
}

#[derive(Debug, Clone, Serialize)]
pub struct UtReport {
    pub schema: &'static str,
    pub gate: Gate,
    pub base: SideInfo,
    pub head: SideInfo,
    pub tests: Vec<TestOutcome>,
    pub started_utc: String,
    pub finished_utc: String,
    pub runner_version: &'static str,
}

pub struct UtArgs {
    pub file: PathBuf,
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub keep: bool,
}

fn short(c: &Option<String>) -> String {
    c.as_deref().map(|c| c[..c.len().min(10)].to_string()).unwrap_or_default()
}

pub fn run(args: &UtArgs, runner_exe: &Path) -> Result<UtReport, String> {
    let started_utc = crate::host::now_utc();
    let (creds, env) = campaign::job_context();
    // The caller's file, read with the caller's rights.
    let text = crate::fsutil::read_as_caller(&args.file, creds.as_ref())?;
    let file = parse(&text)?;
    let stamp = format!("ut-{}-{}", started_utc.replace([':', '-'], ""), std::process::id());
    let ws = crate::fsutil::workspace(&stamp, creds.as_ref())?;
    let run_dir = ws.run_dir.clone();
    // Build each (ref, build command) once.
    let mut trees: BTreeMap<(String, String), (SideInfo, PathBuf)> = BTreeMap::new();
    let mut worktrees = Vec::new();
    let mut builds: Vec<String> = file.tests.iter().map(|t| t.build.clone().or(file.defaults.build.clone()).unwrap_or_default()).collect();
    builds.dedup();
    for (bi, build) in builds.iter().enumerate() {
        for (slot, reference) in [("base", &args.base), ("head", &args.head)] {
            let dest = ws.trees.join(format!("{slot}-{bi}"));
            let commit = gitbuild::add_worktree(&args.repo, reference, &dest, creds.as_ref(), &env)?;
            worktrees.push(dest.clone());
            eprintln!("perf-runner ut: building {reference} ({})", &commit[..commit.len().min(12)]);
            let info = gitbuild::build(&dest, build, creds.as_ref(), &env, &run_dir.join(format!("build-{slot}-{bi}.log")));
            let side = SideInfo {
                label: format!("{slot} {reference}"),
                reference: Some(reference.clone()),
                commit: Some(commit),
                build: Some(info),
                ..Default::default()
            };
            trees.insert((slot.to_string(), build.clone()), (side, dest));
        }
    }
    let mut outcomes = Vec::new();
    for t in &file.tests {
        let build = t.build.clone().or(file.defaults.build.clone()).unwrap_or_default();
        let (a, a_dir) = trees[&("base".to_string(), build.clone())].clone();
        let (b, b_dir) = trees[&("head".to_string(), build)].clone();
        let d = campaign::Options::default();
        let opts = campaign::Options {
            source: campaign::Source::Prepared { a, a_dir, b, b_dir, bench: t.bench.clone() },
            rounds: t.rounds.or(file.defaults.rounds).unwrap_or(16),
            confidence: t.confidence.or(file.defaults.confidence).unwrap_or(0.99),
            floor_pct: t.floor_pct.or(file.defaults.floor_pct).unwrap_or(d.floor_pct),
            metric_floors: t.metric_floor.clone(),
            primary: t.primary.clone(),
            guards: t.guards.clone(),
            check: t.check.clone(),
            tma: t.tma.or(file.defaults.tma).unwrap_or(true),
            ..d
        };
        eprintln!("perf-runner ut: test {:?}", t.name);
        let report = campaign::run(&opts, runner_exe);
        let (g, reasons) = gate(t, &report);
        outcomes.push(TestOutcome { name: t.name.clone(), gate: g, reasons, report });
    }
    if !args.keep {
        for w in &worktrees {
            gitbuild::remove_worktree(&args.repo, w, creds.as_ref(), &env);
        }
        let _ = std::fs::remove_dir_all(&run_dir);
        let _ = std::fs::remove_dir_all(&ws.scratch);
    }
    let gate = if outcomes.iter().any(|o| o.gate == Gate::Fail) {
        Gate::Fail
    } else if outcomes.iter().any(|o| o.gate == Gate::NotRun) {
        Gate::NotRun
    } else {
        Gate::Pass
    };
    let first = &outcomes[0].report;
    Ok(UtReport {
        schema: "byteask.perflab.ut/1",
        gate,
        base: first.a.clone(),
        head: first.b.clone(),
        tests: outcomes,
        started_utc,
        finished_utc: crate::host::now_utc(),
        runner_version: env!("CARGO_PKG_VERSION"),
    })
}

fn cell(r: &Report, name: &str) -> String {
    match r.metrics.iter().find(|m| m.name == name) {
        Some(m) if m.ci[0].is_finite() => format!(
            "{:+.1}% [{:+.1}, {:+.1}]",
            m.change_pct,
            (m.ci[0] - 1.0) * 100.0,
            (m.ci[1] - 1.0) * 100.0
        ),
        Some(m) => format!("{:+.1}%", m.change_pct),
        None => "-".to_string(),
    }
}

/// The pull-request perf card: one line per test, the noise-gated
/// delta with its confidence interval, from this bench machine.
pub fn card(u: &UtReport) -> String {
    let mut o = String::new();
    let word = match u.gate {
        Gate::Pass => "PASS",
        Gate::Fail => "FAIL",
        Gate::NotRun => "NOT RUN",
    };
    let r0 = &u.tests[0].report;
    o.push_str(&format!(
        "### ByteAsk perf check: {word}\n\nbase `{}` -> head `{}` on {} (cpu {} isolated, {}), {} rounds, {:.0}% confidence. Change is head vs base, [CI].\n\n",
        short(&u.base.commit),
        short(&u.head.commit),
        r0.host.hostname,
        r0.host.measured_cpu.map_or("?".into(), |c| c.to_string()),
        r0.host.freq_ghz.map_or("frequency unknown".into(), |f| format!("{f:.2} GHz pinned")),
        r0.rounds,
        r0.confidence * 100.0
    ));
    o.push_str("| test | gate | verdict | p50 | p99 | p99.9 | throughput |\n|---|---|---|---|---|---|---|\n");
    for t in &u.tests {
        let g = match t.gate {
            Gate::Pass => "PASS",
            Gate::Fail => "**FAIL**",
            Gate::NotRun => "NOT RUN",
        };
        o.push_str(&format!(
            "| {} | {g} | {} | {} | {} | {} | {} |\n",
            t.name,
            t.report.verdict.word(),
            cell(&t.report, "p50_ns"),
            cell(&t.report, "p99_ns"),
            cell(&t.report, "p999_ns"),
            cell(&t.report, "throughput")
        ));
    }
    for t in &u.tests {
        for why in &t.reasons {
            o.push_str(&format!("\n- {}: {why}", t.name));
        }
        if t.report.verdict == Verdict::NoClaim
            && let Some(m) = t.report.primary_metric.as_ref().and_then(|p| t.report.metrics.iter().find(|m| &m.name == p)) {
                o.push_str(&format!(
                    "\n- {}: no significant change; this run would have caught a change of {:.1}% or more in {}",
                    t.name,
                    m.min_detectable_pct,
                    crate::report::short_name(&m.name)
                ));
            }
    }
    o.push('\n');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::MetricResult;
    use crate::stats::MetricVerdict;

    const FILE: &str = r#"
[defaults]
build = "make bench"
confidence = 0.99

[[test]]
name = "hot path"
bench = "./bench"
guards = ["allocs_per_msg"]
max_regression_pct = { p50_ns = 2, throughput = 3 }
max = { p99_ns = 600 }
min = { throughput = 4.0e6 }
"#;

    fn metric(name: &str, better: Better, a: f64, b: f64, ci: [f64; 2]) -> MetricResult {
        MetricResult {
            name: name.into(),
            better,
            unit: None,
            n: 16,
            a_median: a,
            b_median: b,
            ratio: b / a,
            change_pct: (b / a - 1.0) * 100.0,
            ci,
            ci_level: 0.99,
            p_value: 0.001,
            mwu_p_value: 0.001,
            noise_pct: 1.0,
            min_detectable_pct: 1.0,
            rounds_needed_for_floor: None,
            verdict: MetricVerdict::NoClaim,
            reason: String::new(),
        }
    }

    fn report(verdict: Verdict, metrics: Vec<MetricResult>) -> Report {
        let opts = campaign::Options { confidence: 0.99, ..campaign::Options::default() };
        let mut r = campaign::empty_report(&opts);
        r.verdict = verdict;
        r.metrics = metrics;
        r
    }

    #[test]
    fn parses_and_rejects_bad_files() {
        let f = parse(FILE).unwrap();
        assert_eq!(f.tests.len(), 1);
        assert_eq!(f.tests[0].max["p99_ns"], 600.0);
        assert!(parse("[defaults]\nbuild='x'\n").is_err());
        assert!(parse("[[test]]\nname='a'\nbench='b'\n").is_err(), "no build anywhere");
        assert!(parse("[[test]]\nname='a'\nbench='b'\nbuild='c'\ntypo=1\n").is_err(), "unknown key");
    }

    #[test]
    fn gate_rules() {
        let t = &parse(FILE).unwrap().tests[0];
        // p_value 0.001 < 0.01 in every fixture; the CI decides "measured".
        let set = |p50: f64, p99: f64, thr: f64| {
            vec![
                metric("p50_ns", Better::Lower, 100.0, p50, [p50 / 100.0 - 0.005, p50 / 100.0 + 0.005]),
                metric("p99_ns", Better::Lower, 500.0, p99, [0.98, 1.02]),
                metric("throughput", Better::Higher, 5e6, thr, [thr / 5e6 - 0.005, thr / 5e6 + 0.005]),
            ]
        };
        assert_eq!(gate(t, &report(Verdict::NoClaim, set(100.0, 500.0, 5e6))).0, Gate::Pass);
        // A measured 5% p50 regression beats the 2% limit.
        let (g, why) = gate(t, &report(Verdict::Slower, set(105.0, 500.0, 5e6)));
        assert_eq!(g, Gate::Fail);
        assert!(why.iter().any(|w| w.contains("p50_ns regressed")), "{why:?}");
        // A measured 1.5% regression inside the customer's 2% limit passes.
        assert_eq!(gate(t, &report(Verdict::Slower, set(101.5, 500.0, 5e6))).0, Gate::Pass);
        // A measured regression on a metric with NO limit fails (MIXED tail).
        let mut m = set(90.0, 500.0, 5e6);
        m[1].ci = [1.03, 1.08];
        m[1].change_pct = 5.0;
        m[1].verdict = MetricVerdict::Slower;
        let (g, why) = gate(t, &report(Verdict::Mixed, m.clone()));
        assert_eq!(g, Gate::Fail);
        assert!(why.iter().any(|w| w.contains("p99_ns regressed") && w.contains("no limit set")), "{why:?}");
        // Family-wise: 3 gated metrics at 99% -> each needs p < 0.0033.
        m[1].p_value = 0.004;
        assert_eq!(gate(t, &report(Verdict::Mixed, m.clone())).0, Gate::Pass, "p=0.004 is not measured for a family of 3");
        m[1].p_value = 0.003;
        assert_eq!(gate(t, &report(Verdict::Mixed, m)).0, Gate::Fail);
        // Absolute limits on the head, whatever the statistics say.
        let (g, why) = gate(t, &report(Verdict::NoClaim, set(100.0, 650.0, 5e6)));
        assert_eq!(g, Gate::Fail);
        assert!(why[0].contains("p99_ns = 650.0"), "{why:?}");
        assert_eq!(gate(t, &report(Verdict::NoClaim, set(100.0, 500.0, 3.9e6))).0, Gate::Fail);
        // An unmeasured regression (CI includes no change) does not fail.
        let mut m = set(103.0, 500.0, 5e6);
        m[0].ci = [0.99, 1.07];
        assert_eq!(gate(t, &report(Verdict::NoClaim, m)).0, Gate::Pass);
        // Not measured / invalid / rejected.
        assert_eq!(gate(t, &report(Verdict::NotRun, vec![])).0, Gate::NotRun);
        assert_eq!(gate(t, &report(Verdict::Invalid, vec![])).0, Gate::Fail);
        assert_eq!(gate(t, &report(Verdict::Rejected, vec![])).0, Gate::Fail);
        // A limit on a metric the benchmark never reports cannot pass silently.
        let (g, why) = gate(t, &report(Verdict::NoClaim, vec![metric("p50_ns", Better::Lower, 100.0, 100.0, [0.99, 1.01])]));
        assert_eq!(g, Gate::Fail);
        assert!(why.iter().any(|w| w.contains("could not be checked")), "{why:?}");
    }
}
