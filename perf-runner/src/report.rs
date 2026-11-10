//! The stable JSON report (`byteask.perflab.compare/1`) and its Markdown view.
//! Fields are only ever added, never renamed or removed.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::gitbuild::BuildInfo;
use crate::preflight::Preflight;
use crate::stats::MetricResult;
use crate::stats::MetricVerdict;
use crate::tma::TmaReport;

pub const SCHEMA: &str = "byteask.perflab.compare/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "FASTER")]
    Faster,
    #[serde(rename = "SLOWER")]
    Slower,
    #[serde(rename = "NO_CLAIM")]
    NoClaim,
    /// Better at the primary metric, measurably worse at another.
    #[serde(rename = "MIXED")]
    Mixed,
    #[serde(rename = "REJECTED")]
    Rejected,
    #[serde(rename = "INVALID")]
    Invalid,
    #[serde(rename = "NOT_RUN")]
    NotRun,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Faster => "FASTER",
            Verdict::Slower => "SLOWER",
            Verdict::NoClaim => "NO CLAIM",
            Verdict::Mixed => "MIXED",
            Verdict::Rejected => "REJECTED",
            Verdict::Invalid => "INVALID",
            Verdict::NotRun => "NOT RUN",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Faster | Verdict::Slower | Verdict::NoClaim | Verdict::Mixed | Verdict::Rejected => 0,
            Verdict::NotRun => 2,
            Verdict::Invalid => 3,
        }
    }

    pub fn from_metric(v: MetricVerdict) -> Verdict {
        match v {
            MetricVerdict::Faster => Verdict::Faster,
            MetricVerdict::Slower => Verdict::Slower,
            MetricVerdict::NoClaim | MetricVerdict::Info => Verdict::NoClaim,
            MetricVerdict::NotRun => Verdict::NotRun,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub cpu_model: String,
    pub kernel: String,
    pub measured_cpu: Option<usize>,
    pub sibling_cpu: Option<usize>,
    pub housekeeping_cpus: Option<String>,
    pub freq_ghz: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SideInfo {
    pub label: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub commit: Option<String>,
    pub command: Vec<String>,
    pub cwd: String,
    pub build: Option<BuildInfo>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RunRecord {
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub wall_s: f64,
    pub user_s: f64,
    pub sys_s: f64,
    pub freq_ghz: Option<f64>,
    pub cycles: Option<u64>,
    pub instructions: Option<u64>,
    pub involuntary_switches: i64,
    pub metrics: BTreeMap<String, f64>,
    pub perflab_error: Option<String>,
    pub disturbed: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoundRecord {
    pub round: usize,
    pub order: &'static str,
    pub rerun: usize,
    pub discarded: Vec<String>,
    pub a: RunRecord,
    pub b: RunRecord,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardResult {
    pub metric: String,
    pub a_median: Option<f64>,
    pub b_median: Option<f64>,
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CheckResult {
    pub ran: bool,
    pub command: Option<String>,
    pub ok: Option<bool>,
    pub first_diff_line: Option<usize>,
    pub a: Option<String>,
    pub b: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub mode: &'static str,
    pub verdict: Verdict,
    pub verdict_reason: String,
    pub summary: Vec<String>,
    pub primary_metric: Option<String>,
    pub confidence: f64,
    pub floor_pct: f64,
    pub rounds: usize,
    pub host: HostInfo,
    pub preflight: Option<Preflight>,
    pub a: SideInfo,
    pub b: SideInfo,
    pub metrics: Vec<MetricResult>,
    pub regressions: Vec<String>,
    pub guards: Vec<GuardResult>,
    pub check: CheckResult,
    pub tma: TmaReport,
    /// Further regions when `--tma-regions` named more than one (the first is `tma`).
    pub tma_regions: Vec<TmaReport>,
    pub rounds_detail: Vec<RoundRecord>,
    pub order_seed: u64,
    pub measure_lock: Option<String>,
    pub notes: Vec<String>,
    pub started_utc: String,
    pub finished_utc: String,
    pub runner_version: &'static str,
}

fn fmt_num(x: f64) -> String {
    if !x.is_finite() {
        return "-".to_string();
    }
    let a = x.abs();
    if a >= 1e9 {
        format!("{:.3}G", x / 1e9)
    } else if a >= 1e6 {
        format!("{:.3}M", x / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", x / 1e3)
    } else if a >= 100.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.4}")
    }
}

pub fn short_name(m: &str) -> String {
    match m {
        "p50_ns" => "p50".into(),
        "p90_ns" => "p90".into(),
        "p99_ns" => "p99".into(),
        "p999_ns" => "p99.9".into(),
        "p9999_ns" => "p99.99".into(),
        other => other.to_string(),
    }
}

impl Report {
    /// The short header lines (verdict, latency, TMA, guards, host).
    pub fn build_summary(&mut self) {
        let mut s = Vec::new();
        s.push(format!("{}: {}", self.verdict.word(), self.verdict_reason));
        let lat: Vec<String> = self
            .metrics
            .iter()
            .filter(|m| m.name != "wall_s" && m.name != "cpu_cycles")
            .map(|m| {
                format!(
                    "{} {} -> {}{} ({:+.1}%)",
                    short_name(&m.name),
                    fmt_num(m.a_median),
                    fmt_num(m.b_median),
                    m.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default(),
                    m.change_pct
                )
            })
            .collect();
        if !lat.is_empty() {
            s.push(lat.join("; "));
        }
        if self.mode == "tma" {
            for t in std::iter::once(&self.tma).chain(self.tma_regions.iter()) {
                if t.status != "OK" {
                    s.push(format!("TMA {}: NOT RUN ({})", t.region, t.reason.as_deref().unwrap_or("-")));
                    continue;
                }
                let l1 = crate::tma::L1.iter().filter_map(|k| t.a.get(*k).map(|v| format!("{} {v:.1}%", k.trim_start_matches("tma_")))).collect::<Vec<_>>().join(", ");
                s.push(format!("TMA {} ({} run(s), median): {l1}", t.region, t.runs_per_side));
                let l2 = crate::tma::L2.iter().filter_map(|k| t.a.get(*k).map(|v| format!("{} {v:.1}%", k.trim_start_matches("tma_")))).collect::<Vec<_>>().join(", ");
                if !l2.is_empty() {
                    s.push(format!("  level 2: {l2}"));
                }
            }
        } else if self.tma.status == "OK" {
            let mut line = format!(
                "TMA (hot loop): {} -> {}",
                self.tma.a_headline.as_deref().unwrap_or("-"),
                self.tma.b_headline.as_deref().unwrap_or("-")
            );
            if let Some(total) = self.tma.delta_slots_pct.get("total") {
                line.push_str(&format!("; slots in the hot loop {total:+.1}%"));
            }
            if !self.tma.changes_checked {
                line.push_str(&format!(
                    "; {} TMA run(s) per side, so category changes are NOT checked against run-to-run spread (use --tma-runs 5)",
                    self.tma.runs_per_side
                ));
            } else {
                // The level-2 category whose time moved most, among those that
                // moved beyond the run-to-run spread and hold >= 5% of A's slots.
                let big = self
                    .tma
                    .delta_slots_pct
                    .iter()
                    .filter(|(k, _)| crate::tma::L2.contains(&k.as_str()) && self.tma.changed.contains(k) && self.tma.a.get(*k).copied().unwrap_or(0.0) >= 5.0)
                    .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
                match big {
                    Some((k, d)) => line.push_str(&format!(
                        ", {} time {d:+.1}% (share {:.1}% -> {:.1}%)",
                        k.trim_start_matches("tma_"),
                        self.tma.a.get(k).copied().unwrap_or(0.0),
                        self.tma.b.get(k).copied().unwrap_or(0.0)
                    )),
                    None => line.push_str("; no level-2 category moved beyond its run-to-run spread"),
                }
            }
            s.push(line);
        } else {
            s.push(format!("TMA: NOT RUN ({})", self.tma.reason.as_deref().unwrap_or("-")));
        }
        let mut g: Vec<String> = self
            .guards
            .iter()
            .map(|g| {
                format!(
                    "{} {} -> {} {}",
                    g.metric,
                    g.a_median.map(fmt_num).unwrap_or("-".into()),
                    g.b_median.map(fmt_num).unwrap_or("-".into()),
                    match g.ok {
                        Some(true) => "OK",
                        Some(false) => "VIOLATED",
                        None => "NOT CHECKED",
                    }
                )
            })
            .collect();
        if self.check.ran {
            g.push(match self.check.ok {
                Some(true) => "output identical".to_string(),
                Some(false) => format!("OUTPUT CHANGED at line {}", self.check.first_diff_line.unwrap_or(0)),
                None => "output check failed to run".to_string(),
            });
        }
        if !g.is_empty() {
            s.push(g.join("; "));
        }
        let pf = self.preflight.as_ref().map(|p| {
            let pass = p.checks.iter().filter(|c| c.status == crate::preflight::Status::Pass).count();
            format!("preflight {pass}/{} PASS", p.checks.len())
        });
        let noise = self
            .primary_metric
            .as_ref()
            .and_then(|pm| self.metrics.iter().find(|m| &m.name == pm))
            .filter(|m| m.noise_pct.is_finite())
            .map(|m| format!("noise {:.2}%/round, smallest detectable {:.1}%", m.noise_pct, m.min_detectable_pct));
        s.push(
            [
                Some(format!(
                    "cpu {} isolated, {}",
                    self.host.measured_cpu.map_or("?".into(), |c| c.to_string()),
                    self.host.freq_ghz.map_or("frequency unknown".into(), |f| format!("{f:.2} GHz pinned"))
                )),
                pf,
                Some(format!("{} rounds", self.rounds)),
                noise,
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; "),
        );
        self.summary = s;
    }

    pub fn markdown(&self) -> String {
        let mut o = String::new();
        let title = if self.mode == "aa" { "A/A noise check" } else { "Performance comparison" };
        o.push_str(&format!("# {title}: {} vs {}\n\n", self.a.label, self.b.label));
        for l in &self.summary {
            o.push_str(&format!("- {l}\n"));
        }
        o.push_str(&format!(
            "\nRule: {}% bootstrap CI of median(B)/median(A) over rounds AND an exact sign-flip test, plus a {}% minimum effect.\n\n",
            (self.confidence * 100.0).round(),
            self.floor_pct
        ));
        o.push_str("| metric | A median | B median | B/A | CI | p | noise/round | verdict |\n|---|---|---|---|---|---|---|---|\n");
        for m in &self.metrics {
            o.push_str(&format!(
                "| {} | {} | {} | {:+.2}% | {:+.2}% .. {:+.2}% | {:.4} | {:.2}% | {} |\n",
                short_name(&m.name),
                fmt_num(m.a_median),
                fmt_num(m.b_median),
                m.change_pct,
                (m.ci[0] - 1.0) * 100.0,
                (m.ci[1] - 1.0) * 100.0,
                m.p_value,
                m.noise_pct,
                m.verdict.word()
            ));
        }
        for t in &self.tma_regions {
            if t.status == "OK" {
                o.push_str(&format!("\n## TMA, region `{}`\n\n| category | A | B |\n|---|---|---|\n", t.region));
                for k in crate::tma::L1.iter().chain(crate::tma::L2.iter()) {
                    if let Some(a) = t.a.get(*k) {
                        let b = t.b.get(*k).map_or("-".to_string(), |b| format!("{b:.1}%"));
                        o.push_str(&format!("| {} | {a:.1}% | {b} |\n", k.trim_start_matches("tma_")));
                    }
                }
            }
        }
        if self.tma.status == "OK" {
            o.push_str(&format!(
                "\n## TMA, measured region only ({} run(s) per side, median)\n\nShares are of the region's pipeline slots; \"time\" is the change in slots spent in that category (same work on both sides). * = moved beyond the run-to-run spread of both sides (needs 2+ runs per side).\n\n| category | A share | B share | share change | time change |\n|---|---|---|---|---|\n",
                self.tma.runs_per_side
            ));
            for k in crate::tma::L1.iter().chain(crate::tma::L2.iter()) {
                if let (Some(a), Some(b)) = (self.tma.a.get(*k), self.tma.b.get(*k)) {
                    let indent = if crate::tma::L1.contains(k) { "" } else { "&nbsp;&nbsp;" };
                    let mark = if self.tma.changed.iter().any(|c| c == k) { " *" } else { "" };
                    let t = self.tma.delta_slots_pct.get(*k).map(|d| format!("{d:+.1}%{mark}")).unwrap_or("-".into());
                    o.push_str(&format!("| {indent}{} | {a:.1}% | {b:.1}% | {:+.1} pp | {t} |\n", k.trim_start_matches("tma_"), b - a));
                }
            }
            if let Some(t) = self.tma.delta_slots_pct.get("total") {
                o.push_str(&format!("| all slots | | | | {t:+.1}% |\n"));
            }
        } else {
            o.push_str(&format!("\nTMA: NOT RUN ({})\n", self.tma.reason.as_deref().unwrap_or("-")));
        }
        if let Some(p) = &self.preflight {
            o.push_str("\n## Preflight\n\n| check | status | detail |\n|---|---|---|\n");
            for c in &p.checks {
                o.push_str(&format!("| {} | {} | {}{} |\n", c.name, c.status.word(), c.detail.replace('|', "/"), c.fix.as_deref().map(|f| format!(" — fix: {f}")).unwrap_or_default()));
            }
        }
        if !self.rounds_detail.is_empty() {
            let pm = self.primary_metric.clone().unwrap_or_else(|| "wall_s".into());
            o.push_str(&format!("\n## Rounds ({})\n\n| round | order | A | B | reruns |\n|---|---|---|---|---|\n", short_name(&pm)));
            let get = |r: &RunRecord| if pm == "wall_s" { Some(r.wall_s) } else { r.metrics.get(&pm).copied() };
            for r in &self.rounds_detail {
                o.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    r.round,
                    r.order,
                    get(&r.a).map(fmt_num).unwrap_or("-".into()),
                    get(&r.b).map(fmt_num).unwrap_or("-".into()),
                    r.rerun
                ));
            }
        }
        if !self.notes.is_empty() {
            o.push_str("\n## Notes\n\n");
            for n in &self.notes {
                o.push_str(&format!("- {n}\n"));
            }
        }
        o
    }
}
