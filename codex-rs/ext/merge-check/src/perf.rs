//! The perf card on the pull request: the noise-gated change and its
//! confidence interval, measured on the company's bench machine by `perf-runner ut`
//! and signed there by its enrolled bench key.

use check_result::CheckResult;
use check_result::NotRunReason;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::binding::Binding;

pub const PERF_SCHEMA: &str = "byteask.merge-check.perf/1";
/// The report schema of `perf-runner ut --json`.
pub const UT_SCHEMA: &str = "byteask.perflab.ut/1";
pub const CHECK_PERF: &str = "perf";

/// What a bench runner signs: the `perf-runner ut` report, bound to one merge candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerfEvidence {
    pub schema: String,
    pub binding: Binding,
    pub content_key: String,
    /// The `perf-runner ut` report, as the runner wrote it.
    pub ut: Value,
}

impl PerfEvidence {
    /// Wrap a `ut` report for a candidate, checking that it measured exactly that candidate:
    /// its base is the candidate's base commit and its head's tree is the merged tree.
    pub fn bind(
        ut: Value,
        binding: &Binding,
        head_tree: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        if ut["schema"].as_str() != Some(UT_SCHEMA) {
            return Err(format!(
                "not a perf-runner ut report (schema {:?}, want {UT_SCHEMA})",
                ut["schema"].as_str().unwrap_or("none")
            ));
        }
        let base = ut["base"]["commit"].as_str().unwrap_or_default();
        if base != binding.base_commit {
            return Err(format!(
                "the bench measured base {} but the candidate's base is {}",
                short(base),
                short(&binding.base_commit)
            ));
        }
        let head = ut["head"]["commit"].as_str().unwrap_or_default();
        match head_tree(head) {
            Some(tree) if tree == binding.merge_tree => {}
            Some(_) => {
                return Err(format!(
                    "the bench measured {} whose tree is not the merge candidate's",
                    short(head)
                ));
            }
            None => {
                return Err(format!(
                    "the bench measured {}, which this repository does not have",
                    short(head)
                ));
            }
        }
        Ok(Self {
            schema: PERF_SCHEMA.to_string(),
            binding: binding.clone(),
            content_key: binding.content_key(),
            ut,
        })
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

/// The perf check's verdict: the merge gate of `perf-runner ut` (it applies the company's limits).
pub fn result(ut: &Value) -> CheckResult {
    let reasons: Vec<String> = ut["tests"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|test| {
            let name = test["name"].as_str().unwrap_or("?").to_string();
            test["reasons"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|reason| reason.as_str())
                .map(move |reason| format!("{name}: {reason}"))
                .collect::<Vec<_>>()
        })
        .collect();
    match ut["gate"].as_str() {
        Some("PASS") => CheckResult::pass(
            CHECK_PERF,
            "No measured regression beyond the profile's limits on the bench machine.",
        ),
        Some("FAIL") => CheckResult::fail(
            CHECK_PERF,
            &format!(
                "Measured regression on the bench machine: {}",
                reasons.first().cloned().unwrap_or_else(|| "see the perf card".to_string())
            ),
        )
        .with_evidence(reasons),
        _ => CheckResult::not_run(
            CHECK_PERF,
            NotRunReason::ServiceUnavailable,
            &format!(
                "The bench machine did not measure it: {}",
                reasons.first().cloned().unwrap_or_else(|| "no reason given".to_string())
            ),
            "fix what the bench names (host checks, build) and run the perf check again.",
        ),
    }
}

/// What a comparison found, in words (inside the noise is never a speedup).
fn finding(verdict: &str) -> &'static str {
    match verdict {
        "FASTER" => "faster",
        "SLOWER" => "slower",
        "MIXED" => "mixed (better on one metric, worse on another)",
        "NO_CLAIM" => "no significant change",
        "REJECTED" => "rejected (output changed or a guard metric rose)",
        "INVALID" => "not measured (a run failed)",
        _ => "not measured",
    }
}

fn metric<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report["metrics"]
        .as_array()?
        .iter()
        .find(|m| m["name"].as_str() == Some(name))
}

fn cell(report: &Value, name: &str) -> String {
    let Some(m) = metric(report, name) else {
        return "-".to_string();
    };
    let change = m["change_pct"].as_f64();
    let lo = m["ci"][0].as_f64();
    let hi = m["ci"][1].as_f64();
    match (change, lo, hi) {
        (Some(c), Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() => format!(
            "{c:+.1}% [{:+.1}%, {:+.1}%]",
            (lo - 1.0) * 100.0,
            (hi - 1.0) * 100.0
        ),
        (Some(c), _, _) => format!("{c:+.1}%"),
        _ => "-".to_string(),
    }
}

/// The card, in Markdown (GitHub check output, GitLab note, Jenkins summary).
pub fn card(ut: &Value) -> String {
    let tests = ut["tests"].as_array().cloned().unwrap_or_default();
    let first = tests.first().map(|t| &t["report"]).cloned().unwrap_or(Value::Null);
    let host = &first["host"];
    let confidence = first["confidence"].as_f64().unwrap_or(0.99);
    let gate = match ut["gate"].as_str() {
        Some("PASS") => "PASS",
        Some("FAIL") => "FAIL",
        _ => "NOT RUN",
    };
    let mut out = format!(
        "### ByteAsk perf check: {gate}\n\n\
         Measured on {} (cpu {} isolated, {}), base `{}` against this change, {} rounds, \
         {:.0}% confidence. Each cell is the change in the candidate's median against the base, \
         with its confidence interval; a change inside the noise is reported as no change.\n\n\
         | test | finding | p50 | p99 | p99.9 | throughput |\n|---|---|---|---|---|---|\n",
        host["hostname"].as_str().unwrap_or("the bench machine"),
        host["measured_cpu"]
            .as_u64()
            .map_or("?".to_string(), |c| c.to_string()),
        host["freq_ghz"]
            .as_f64()
            .map_or("frequency unknown".to_string(), |f| format!("{f:.2} GHz pinned")),
        short(ut["base"]["commit"].as_str().unwrap_or("?")),
        first["rounds"].as_u64().unwrap_or(0),
        confidence * 100.0,
    );
    for test in &tests {
        let report = &test["report"];
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            test["name"].as_str().unwrap_or("?").replace('|', "/"),
            finding(report["verdict"].as_str().unwrap_or("NOT_RUN")),
            cell(report, "p50_ns"),
            cell(report, "p99_ns"),
            cell(report, "p999_ns"),
            cell(report, "throughput"),
        ));
    }
    let mut notes = Vec::new();
    for test in &tests {
        let name = test["name"].as_str().unwrap_or("?");
        let report = &test["report"];
        if report["verdict"].as_str() == Some("NO_CLAIM") {
            let primary = report["primary_metric"].as_str().unwrap_or("p50_ns");
            let detectable = metric(report, primary)
                .and_then(|m| m["min_detectable_pct"].as_f64())
                .map(|pct| format!("; this run would have caught a change of {pct:.1}% or more in {primary}"))
                .unwrap_or_default();
            notes.push(format!("- {name}: no significant change{detectable}."));
        }
        for reason in test["reasons"].as_array().into_iter().flatten() {
            if let Some(reason) = reason.as_str() {
                notes.push(format!("- {name}: {reason}"));
            }
        }
    }
    if !notes.is_empty() {
        out.push('\n');
        out.push_str(&notes.join("\n"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[path = "perf_tests.rs"]
mod tests;
