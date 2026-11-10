//! TMA level 1 + 2 for the measured region only: `perf stat -M
//! TopdownL1,TopdownL2 --control fifo:ctl,ack --delay=-1`, the benchmark
//! enabling the counters around its hot loop.

use std::collections::BTreeMap;

use serde::Serialize;

/// Level-1 and level-2 names as perf's metric groups print them, in report order.
pub const L1: [&str; 4] = ["tma_retiring", "tma_bad_speculation", "tma_frontend_bound", "tma_backend_bound"];
pub const L2: [&str; 8] = [
    "tma_light_operations",
    "tma_heavy_operations",
    "tma_branch_mispredicts",
    "tma_machine_clears",
    "tma_fetch_latency",
    "tma_fetch_bandwidth",
    "tma_memory_bound",
    "tma_core_bound",
];

/// Smallest share change (percentage points) that can count as a change.
pub const MIN_CHANGE_PP: f64 = 1.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmaRun {
    /// Percent of pipeline slots, by metric name.
    pub metrics: BTreeMap<String, f64>,
    pub slots: Option<f64>,
    /// Some counter ran only part of the time (multiplexed).
    pub multiplexed: bool,
}

/// Parse `perf stat -j` output. `Err` explains why there is nothing to report
/// (never enabled, no PMU, garbage).
pub fn parse_perf_json(text: &str) -> Result<TmaRun, String> {
    let mut run = TmaRun::default();
    let mut saw_slots = false;
    for line in text.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(ev) = v.get("event").and_then(|e| e.as_str()) {
            if ev.eq_ignore_ascii_case("TOPDOWN.SLOTS") || ev == "slots" {
                saw_slots = true;
                run.slots = v.get("counter-value").and_then(|c| c.as_str()).and_then(|c| c.parse::<f64>().ok());
            }
            if let Some(p) = v.get("pcnt-running").and_then(|p| p.as_f64())
                && p < 99.99 {
                    run.multiplexed = true;
                }
        }
        if let (Some(val), Some(unit)) = (v.get("metric-value").and_then(|m| m.as_str()), v.get("metric-unit").and_then(|m| m.as_str())) {
            let name = unit.trim_start_matches('%').trim();
            if let Ok(x) = val.parse::<f64>()
                && x.is_finite() && name.starts_with("tma_") {
                    // perf prints tiny negatives (rounding of subtracted terms).
                    run.metrics.insert(name.to_string(), x.max(0.0));
                }
        }
    }
    if !saw_slots {
        return Err("perf printed no TOPDOWN.SLOTS counter (no topdown support on this PMU?)".to_string());
    }
    match run.slots {
        Some(s) if s > 0.0 => {}
        _ => {
            return Err("the counters were never enabled: the benchmark did not write \"enable\" to the perf control fifo (does it support the {ctl} {ack} arguments?)".to_string());
        }
    }
    if !L1.iter().any(|m| run.metrics.contains_key(*m)) {
        return Err("perf reported no level-1 TMA metric".to_string());
    }
    Ok(run)
}

#[derive(Debug, Clone, Serialize)]
pub struct TmaReport {
    pub status: &'static str,
    pub reason: Option<String>,
    /// "perf-ctl" (the benchmark's one region) or the PERFLAB_REGION name.
    pub region: String,
    pub runs_per_side: usize,
    pub a: BTreeMap<String, f64>,
    pub b: BTreeMap<String, f64>,
    pub delta_pp: BTreeMap<String, f64>,
    /// Pipeline slots in the measured region (median per side). Same work on
    /// both sides, so slots are comparable in absolute terms.
    pub a_slots: Option<f64>,
    pub b_slots: Option<f64>,
    /// Change in the slots spent in each category, B vs A, percent:
    /// (share_B * slots_B) / (share_A * slots_A) - 1. A share can rise while
    /// the time in that category falls, when the whole region got shorter.
    pub delta_slots_pct: BTreeMap<String, f64>,
    /// [min, max] of each category's share over the runs of each side.
    pub a_range: BTreeMap<String, [f64; 2]>,
    pub b_range: BTreeMap<String, [f64; 2]>,
    /// Categories whose A and B ranges do not overlap AND whose share moved by
    /// at least [`MIN_CHANGE_PP`]: the only changes that count. For identical
    /// builds the ranges of n runs per side miss each other with probability
    /// 2 / C(2n, n) per category: 10% at n = 3, 0.8% at n = 5 (the default).
    /// Needs at least 2 runs per side; with 1 nothing is checked.
    pub changed: Vec<String>,
    pub changes_checked: bool,
    pub a_headline: Option<String>,
    pub b_headline: Option<String>,
    pub multiplexed: bool,
    pub level2_available: bool,
}

impl TmaReport {
    pub fn not_run(reason: String) -> TmaReport {
        TmaReport {
            status: "NOT_RUN",
            reason: Some(reason),
            region: "perf-ctl".to_string(),
            runs_per_side: 0,
            a: BTreeMap::new(),
            b: BTreeMap::new(),
            delta_pp: BTreeMap::new(),
            a_slots: None,
            b_slots: None,
            delta_slots_pct: BTreeMap::new(),
            a_range: BTreeMap::new(),
            b_range: BTreeMap::new(),
            changed: Vec::new(),
            changes_checked: false,
            a_headline: None,
            b_headline: None,
            multiplexed: false,
            level2_available: false,
        }
    }
}

fn medians(runs: &[TmaRun]) -> BTreeMap<String, f64> {
    let mut names: Vec<&String> = runs.iter().flat_map(|r| r.metrics.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|n| {
            let v: Vec<f64> = runs.iter().filter_map(|r| r.metrics.get(n).copied()).collect();
            (v.len() == runs.len()).then(|| (n.clone(), crate::stats::median(&v)))
        })
        .collect()
}

/// "backend_bound (memory_bound 40%)": the largest level-1 class and, under
/// it, the largest level-2 class.
pub fn headline(m: &BTreeMap<String, f64>) -> Option<String> {
    let (top, val) = L1.iter().filter_map(|k| m.get(*k).map(|v| (*k, *v))).max_by(|a, b| a.1.total_cmp(&b.1))?;
    let children: &[&str] = match top {
        "tma_retiring" => &["tma_light_operations", "tma_heavy_operations"],
        "tma_bad_speculation" => &["tma_branch_mispredicts", "tma_machine_clears"],
        "tma_frontend_bound" => &["tma_fetch_latency", "tma_fetch_bandwidth"],
        _ => &["tma_memory_bound", "tma_core_bound"],
    };
    let short = |s: &str| s.trim_start_matches("tma_").to_string();
    let child = children.iter().filter_map(|k| m.get(*k).map(|v| (*k, *v))).max_by(|a, b| a.1.total_cmp(&b.1));
    Some(match child {
        Some((c, cv)) => format!("{} {val:.1}% ({} {cv:.1}%)", short(top), short(c)),
        None => format!("{} {val:.1}%", short(top)),
    })
}

pub fn summarise(a_runs: &[TmaRun], b_runs: &[TmaRun]) -> TmaReport {
    let a = medians(a_runs);
    let b = medians(b_runs);
    let delta_pp = a.iter().filter_map(|(k, va)| b.get(k).map(|vb| (k.clone(), vb - va))).collect();
    let level2_available = L2.iter().any(|k| a.contains_key(*k));
    let slots = |runs: &[TmaRun]| {
        let v: Vec<f64> = runs.iter().filter_map(|r| r.slots).collect();
        (v.len() == runs.len() && !v.is_empty()).then(|| crate::stats::median(&v))
    };
    let (a_slots, b_slots) = (slots(a_runs), slots(b_runs));
    let mut delta_slots_pct = BTreeMap::new();
    if let (Some(sa), Some(sb)) = (a_slots, b_slots) {
        for (k, va) in &a {
            if let Some(vb) = b.get(k)
                && *va > 0.0 {
                    delta_slots_pct.insert(k.clone(), ((vb * sb) / (va * sa) - 1.0) * 100.0);
                }
        }
        delta_slots_pct.insert("total".to_string(), (sb / sa - 1.0) * 100.0);
    }
    let range = |runs: &[TmaRun]| -> BTreeMap<String, [f64; 2]> {
        let mut m: BTreeMap<String, [f64; 2]> = BTreeMap::new();
        for r in runs {
            for (k, v) in &r.metrics {
                let e = m.entry(k.clone()).or_insert([f64::INFINITY, f64::NEG_INFINITY]);
                e[0] = e[0].min(*v);
                e[1] = e[1].max(*v);
            }
        }
        m
    };
    let (a_range, b_range) = (range(a_runs), range(b_runs));
    let changes_checked = a_runs.len() >= 2 && b_runs.len() >= 2;
    let changed = if changes_checked {
        a_range
            .iter()
            .filter_map(|(k, ra)| {
                let moved = (b.get(k).copied().unwrap_or(0.0) - a.get(k).copied().unwrap_or(0.0)).abs() >= MIN_CHANGE_PP;
                b_range.get(k).filter(|rb| moved && (rb[0] > ra[1] || rb[1] < ra[0])).map(|_| k.clone())
            })
            .collect()
    } else {
        Vec::new()
    };
    TmaReport {
        a_range,
        b_range,
        changed,
        changes_checked,
        a_slots,
        b_slots,
        delta_slots_pct,
        status: "OK",
        reason: None,
        region: "perf-ctl".to_string(),
        runs_per_side: a_runs.len().min(b_runs.len()),
        a_headline: headline(&a),
        b_headline: headline(&b),
        a,
        b,
        delta_pp,
        multiplexed: a_runs.iter().chain(b_runs).any(|r| r.multiplexed),
        level2_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENABLED: &str = r#"{"counter-value" : "7118410290.000000", "unit" : "", "event" : "TOPDOWN.SLOTS", "event-runtime" : 389549102, "pcnt-running" : 100.00, "metric-value" : "0.042770", "metric-unit" : "%  tma_bad_speculation", "metric-threshold" : "good"}
{"metric-value" : "-0.000339", "metric-unit" : "%  tma_frontend_bound", "metric-threshold" : "good"}
{"counter-value" : "614137358.000000", "unit" : "", "event" : "topdown-retiring", "event-runtime" : 389549102, "pcnt-running" : 100.00, "metric-value" : "91.015625", "metric-unit" : "%  tma_backend_bound", "metric-threshold" : "bad"}
{"counter-value" : "4615335005.000000", "unit" : "", "event" : "topdown-mem-bound", "event-runtime" : 389272205, "pcnt-running" : 100.00, "metric-value" : "27.559055", "metric-unit" : "%  tma_core_bound", "metric-threshold" : "bad"}
{"metric-value" : "64.173228", "metric-unit" : "%  tma_memory_bound", "metric-threshold" : "bad"}
{"counter-value" : "0.000000", "unit" : "", "event" : "topdown-heavy-ops", "event-runtime" : 389272205, "pcnt-running" : 100.00, "metric-value" : "8.267717", "metric-unit" : "%  tma_retiring", "metric-threshold" : "good"}
"#;

    const NEVER: &str = r#"{"counter-value" : "<not counted>", "unit" : "", "event" : "TOPDOWN.SLOTS", "event-runtime" : 0, "pcnt-running" : 100.00, "metric-value" : "0.000000", "metric-unit" : "%  tma_bad_speculation", "metric-threshold" : "good"}
{"metric-value" : "nan", "metric-unit" : "%  tma_frontend_bound"}
"#;

    #[test]
    fn parses_region_metrics() {
        let r = parse_perf_json(ENABLED).unwrap();
        assert_eq!(r.metrics["tma_backend_bound"], 91.015625);
        assert_eq!(r.metrics["tma_frontend_bound"], 0.0);
        assert_eq!(r.metrics["tma_memory_bound"], 64.173228);
        assert!(!r.multiplexed);
        let h = headline(&r.metrics).unwrap();
        assert!(h.starts_with("backend_bound 91.0% (memory_bound 64.2%)"), "{h}");
    }

    #[test]
    fn never_enabled_is_an_error_not_zeros() {
        let e = parse_perf_json(NEVER).unwrap_err();
        assert!(e.contains("never enabled"), "{e}");
        assert!(parse_perf_json("").is_err());
    }

    #[test]
    fn summary_deltas() {
        let a = parse_perf_json(ENABLED).unwrap();
        let mut b = a.clone();
        b.metrics.insert("tma_memory_bound".to_string(), 30.0);
        let s = summarise(&[a.clone(), a], &[b.clone(), b]);
        assert!((s.delta_pp["tma_memory_bound"] + 34.173228).abs() < 1e-9);
        // Same slots on both sides: the slot change equals the share change.
        assert!((s.delta_slots_pct["tma_memory_bound"] - (30.0 / 64.173228 - 1.0) * 100.0).abs() < 1e-9);
        assert!(s.delta_slots_pct["total"].abs() < 1e-12);
        assert_eq!(s.runs_per_side, 2);
    }

    #[test]
    fn changes_count_only_beyond_run_to_run_spread() {
        let run = |mem: f64, ret: f64| TmaRun { metrics: [("tma_memory_bound".to_string(), mem), ("tma_retiring".to_string(), ret)].into(), slots: Some(1000.0), multiplexed: false };
        // One run per side: nothing can be checked.
        let s = summarise(&[run(30.0, 20.0)], &[run(20.0, 21.0)]);
        assert!(!s.changes_checked && s.changed.is_empty());
        // Three runs: memory_bound separated, retiring overlapping.
        let s = summarise(&[run(30.0, 20.0), run(31.0, 22.0), run(29.5, 21.0)], &[run(20.0, 21.5), run(21.0, 20.5), run(19.0, 22.5)]);
        assert!(s.changes_checked);
        assert_eq!(s.changed, vec!["tma_memory_bound".to_string()]);
        // Separated but tiny (0.3 pp): not a change.
        let s = summarise(&[run(30.0, 20.0), run(30.1, 20.0)], &[run(30.3, 20.0), run(30.4, 20.0)]);
        assert!(s.changed.is_empty(), "{:?}", s.changed);
    }

    #[test]
    fn a_share_can_rise_while_the_time_falls() {
        let a = TmaRun { metrics: [("tma_memory_bound".to_string(), 29.3), ("tma_retiring".to_string(), 20.0)].into(), slots: Some(1000.0), multiplexed: false };
        let b = TmaRun { metrics: [("tma_memory_bound".to_string(), 37.0), ("tma_retiring".to_string(), 12.0)].into(), slots: Some(590.0), multiplexed: false };
        let s = summarise(&[a], &[b]);
        assert!(s.delta_pp["tma_memory_bound"] > 0.0);
        assert!(s.delta_slots_pct["tma_memory_bound"] < -20.0, "{:?}", s.delta_slots_pct);
        assert_eq!(s.runs_per_side, 1);
    }
}
