//! The local value report.
//!
//! Counts and measured results only, from the local results store only.
//! Every count lives under `local` in the JSON and under a "(local)" heading
//! in the text (CI counts are never added to these). It
//! never says ByteAsk caused an improvement.
//!
//! Definitions, per change:
//!
//! * A version **passes every check** when at least one code check
//!   ([`crate::CODE_CHECKS`]) ran on it, every code-check result on it is
//!   PASS, and every code check that did not pass on another version before
//!   this one first appeared has passed on it. (So a version where the failing
//!   check was simply not run again does not count.)
//! * **Problem caught**: after the AI's first edit, a code check FAILED on one
//!   version, and the same check later passed on a different version that
//!   passes every check, while the change was still in the session (a change
//!   ends when it is pushed). Counted once per change.
//! * **Time to review-ready**: from the AI's first edit to the moment the
//!   first version of the change passed every check. Inside another agent the
//!   first edit is seen by its Stop hook, so it is timed at the start of that
//!   turn: never later than the real edit, so the time is never understated.
//! * **The base version** (the code when an agent session started, or as just
//!   pushed) is never a version of the change, whatever its time: a failure on
//!   it is the code as it was, not a problem the change had.
//! * A change with no AI edit seen has neither (not measured); results with
//!   no version count only under "checks run".
//! * **Perf results**: `cpp_perf` comparisons the noise gate marked
//!   significant, with the measured ratio.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use byteask_policy::ResultsStoreLimits;
use check_result::Verdict;
use serde::Serialize;

use crate::CODE_CHECKS;
use crate::store::ChangeLog;
use crate::store::CloseHow;
use crate::store::Loaded;
use crate::store::Record;

/// Version of the JSON export. Add fields, never rename them.
pub const SCHEMA: u32 = 1;

/// Results by verdict, and NOT RUN by reason.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VerdictCounts {
    pub pass: u64,
    pub fail: u64,
    pub not_run: u64,
    pub no_claim: u64,
    pub approximate: u64,
    /// NOT RUN results by reason (`time_limit`, ...).
    pub not_run_reasons: BTreeMap<String, u64>,
}

impl VerdictCounts {
    pub fn total(&self) -> u64 {
        self.pass + self.fail + self.not_run + self.no_claim + self.approximate
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ChangeCounts {
    pub total: u64,
    pub with_ai_edit: u64,
    pub pushed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProblemsCaught {
    /// Changes with a problem caught.
    pub changes: u64,
    /// For each check, how many of those changes it caught a problem in.
    pub by_check: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReviewReady {
    /// Changes where it was measured.
    pub measured: u64,
    pub median_secs: Option<u64>,
    pub fastest_secs: Option<u64>,
    pub slowest_secs: Option<u64>,
    /// Changes with an AI edit that never had a version passing every check.
    pub not_reached: u64,
    /// Changes where no AI edit was seen (not measured).
    pub no_ai_edit: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PerfResults {
    /// Comparisons the noise gate marked significant.
    pub significant: u64,
    pub smallest_ratio: Option<f64>,
    pub median_ratio: Option<f64>,
    pub largest_ratio: Option<f64>,
}

/// Every count in the report; all of it is local.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LocalCounts {
    /// Code checks ([`CODE_CHECKS`]) by check.
    pub checks: BTreeMap<String, VerdictCounts>,
    /// Every other ByteAsk tool's results by tool.
    pub other_tools: BTreeMap<String, VerdictCounts>,
    /// Results not tied to a version of the code.
    pub results_without_version: u64,
    pub changes: ChangeCounts,
    pub problems_caught: ProblemsCaught,
    pub review_ready: ReviewReady,
    pub perf: PerfResults,
    /// Records or files that could not be read.
    pub unreadable_records: u64,
    /// Changes that hit the per-change result cap.
    pub capped_changes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Period {
    /// Start of the period asked for; `None` = everything kept.
    pub since: Option<u64>,
    /// The first record counted.
    pub first_record: Option<u64>,
}

/// The report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ValueReport {
    pub schema: u32,
    /// Always `"local"`: these counts come from this machine's store only.
    pub source: &'static str,
    pub generated_at: u64,
    pub period: Period,
    /// False when part of the store could not be read: the counts are then
    /// at least these numbers, never a reason to read zero.
    pub complete: bool,
    pub incomplete_since: Option<u64>,
    pub local: LocalCounts,
}

/// A check name that is safe to show: lower-case letters, digits and `_`,
/// at most 40 characters. Anything else (a damaged record) shows as `other`,
/// so no text from a record other than a tool name can reach the report.
fn safe_name(name: &str) -> String {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        name.to_string()
    } else {
        "other".to_string()
    }
}

fn reason_key(reason: check_result::NotRunReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "tool_error".to_string())
}

/// The measured speed ratio of a significant `cpp_perf` comparison, read
/// from the summary `cpp_perf` writes: `"B is 2.39x faster, beyond the noise gate."`.
pub fn speed_ratio(check: &str, verdict: Verdict, summary: &str) -> Option<f64> {
    if check != "cpp_perf" || verdict != Verdict::Pass {
        return None;
    }
    let rest = summary
        .strip_prefix("A is ")
        .or_else(|| summary.strip_prefix("B is "))?;
    let ratio: f64 = rest
        .strip_suffix("x faster, beyond the noise gate.")?
        .parse()
        .ok()?;
    (ratio.is_finite() && ratio >= 1.0).then_some(ratio)
}

struct CheckRow<'a> {
    at: u64,
    version: Option<&'a str>,
    check: &'a str,
    verdict: Verdict,
}

/// What one change contributes.
#[derive(Debug, Default, PartialEq, Eq)]
struct ChangeOutcome {
    edited: bool,
    /// Check names that caught a problem (empty = none caught).
    caught_by: BTreeSet<String>,
    /// Seconds from the first AI edit to review-ready.
    ready_secs: Option<u64>,
}

fn analyse(change: &ChangeLog) -> ChangeOutcome {
    let first_edit = change
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Edit { at, .. } => Some(*at),
            _ => None,
        })
        .min();
    let Some(first_edit) = first_edit else {
        return ChangeOutcome::default();
    };
    let base = change.base_version();
    let mut rows: Vec<CheckRow<'_>> = change
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Result {
                at,
                version,
                result,
                ..
            } if CODE_CHECKS.contains(&result.check.as_str()) => Some(CheckRow {
                at: *at,
                version: version.as_deref(),
                check: result.check.as_str(),
                verdict: result.verdict,
            }),
            _ => None,
        })
        .collect();
    rows.sort_by_key(|row| row.at);

    let mut first_seen: HashMap<&str, u64> = HashMap::new();
    for row in &rows {
        if let Some(version) = row.version {
            first_seen.entry(version).or_insert(row.at);
        }
    }
    // Versions that pass every check, with the moment each became review-ready.
    let mut ready: Vec<(u64, &str)> = Vec::new();
    for (&version, &seen) in &first_seen {
        if seen < first_edit || Some(version) == base {
            continue;
        }
        let on_version: Vec<&CheckRow<'_>> = rows
            .iter()
            .filter(|row| row.version == Some(version))
            .collect();
        if on_version.iter().any(|row| row.verdict != Verdict::Pass) {
            continue;
        }
        let required: BTreeSet<&str> = rows
            .iter()
            .filter(|row| {
                row.version != Some(version) && row.at < seen && row.verdict != Verdict::Pass
            })
            .map(|row| row.check)
            .collect();
        let mut covered: BTreeSet<&str> = BTreeSet::new();
        for row in &on_version {
            covered.insert(row.check);
            if required.is_subset(&covered) {
                ready.push((row.at, version));
                break;
            }
        }
    }
    ready.sort();

    let mut caught_by = BTreeSet::new();
    for failed in rows
        .iter()
        .filter(|row| row.verdict == Verdict::Fail && row.at >= first_edit)
    {
        if failed.version.is_none() || failed.version == base {
            continue;
        }
        // A version that holds this FAIL can never pass every check, so the
        // version that fixed it is always a different one.
        let fixed = ready.iter().any(|&(ready_at, version)| {
            ready_at > failed.at
                && rows.iter().any(|row| {
                    row.version == Some(version) && row.check == failed.check && row.at > failed.at
                })
        });
        if fixed {
            caught_by.insert(safe_name(failed.check));
        }
    }
    ChangeOutcome {
        edited: true,
        caught_by,
        ready_secs: ready.first().map(|(at, _)| at.saturating_sub(first_edit)),
    }
}

fn median_u64(sorted: &[u64]) -> Option<u64> {
    let n = sorted.len();
    match n {
        0 => None,
        _ if !n.is_multiple_of(2) => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2),
    }
}

fn median_f64(sorted: &[f64]) -> Option<f64> {
    let n = sorted.len();
    match n {
        0 => None,
        _ if !n.is_multiple_of(2) => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

impl ValueReport {
    /// Build the report from what [`crate::store::load`] read. `since` limits
    /// it to results (and changes whose first edit) at or after that time.
    pub fn build(loaded: &Loaded, now: u64, since: Option<u64>) -> Self {
        let in_period = |at: u64| since.is_none_or(|since| at >= since);
        let mut local = LocalCounts::default();
        let mut first_record: Option<u64> = None;
        let mut incomplete_since: Option<u64> = None;
        let mut incomplete = !loaded.unreadable_files.is_empty();
        local.unreadable_records = loaded.unreadable_files.len() as u64;
        let mut ready_times: Vec<u64> = Vec::new();
        let mut ratios: Vec<f64> = Vec::new();

        for change in &loaded.changes {
            for unreadable in &change.unreadable {
                incomplete = true;
                local.unreadable_records += 1;
                if let Some(at) = unreadable {
                    incomplete_since = Some(incomplete_since.map_or(*at, |old| old.min(*at)));
                }
            }
            if let Some(capped_at) = change.records.iter().find_map(|record| match record {
                Record::Capped { at } => Some(*at),
                _ => None,
            }) {
                incomplete = true;
                local.capped_changes += 1;
                incomplete_since =
                    Some(incomplete_since.map_or(capped_at, |old| old.min(capped_at)));
            }
            let Some((opened_at, ..)) = change.opened() else {
                continue;
            };
            if !change.has_activity() {
                // An agent session that changed and checked nothing.
                continue;
            }

            for record in &change.records {
                let Record::Result {
                    at,
                    version,
                    result,
                    ..
                } = record
                else {
                    continue;
                };
                if !in_period(*at) {
                    continue;
                }
                first_record = Some(first_record.map_or(*at, |old| old.min(*at)));
                let name = safe_name(&result.check);
                let table = if CODE_CHECKS.contains(&result.check.as_str()) {
                    &mut local.checks
                } else {
                    &mut local.other_tools
                };
                let counts = table.entry(name).or_default();
                match result.verdict {
                    Verdict::Pass => counts.pass += 1,
                    Verdict::Fail => counts.fail += 1,
                    Verdict::NoClaim => counts.no_claim += 1,
                    Verdict::Approximate => counts.approximate += 1,
                    Verdict::NotRun => {
                        counts.not_run += 1;
                        let reason = result
                            .reason
                            .map(reason_key)
                            .unwrap_or_else(|| "tool_error".to_string());
                        *counts.not_run_reasons.entry(reason).or_default() += 1;
                    }
                }
                if version.is_none() {
                    local.results_without_version += 1;
                }
                if let Some(ratio) = speed_ratio(&result.check, result.verdict, &result.summary) {
                    ratios.push(ratio);
                }
            }

            let outcome = analyse(change);
            let first_edit = change
                .records
                .iter()
                .filter_map(|record| match record {
                    Record::Edit { at, .. } => Some(*at),
                    _ => None,
                })
                .min();
            if !in_period(first_edit.unwrap_or(opened_at)) {
                continue;
            }
            local.changes.total += 1;
            if matches!(change.closed(), Some((_, CloseHow::Pushed))) {
                local.changes.pushed += 1;
            }
            if !outcome.edited {
                local.review_ready.no_ai_edit += 1;
                continue;
            }
            local.changes.with_ai_edit += 1;
            if !outcome.caught_by.is_empty() {
                local.problems_caught.changes += 1;
                for check in outcome.caught_by {
                    *local.problems_caught.by_check.entry(check).or_default() += 1;
                }
            }
            match outcome.ready_secs {
                Some(secs) => ready_times.push(secs),
                None => local.review_ready.not_reached += 1,
            }
        }

        ready_times.sort_unstable();
        local.review_ready.measured = ready_times.len() as u64;
        local.review_ready.median_secs = median_u64(&ready_times);
        local.review_ready.fastest_secs = ready_times.first().copied();
        local.review_ready.slowest_secs = ready_times.last().copied();
        ratios.sort_by(f64::total_cmp);
        local.perf.significant = ratios.len() as u64;
        local.perf.smallest_ratio = ratios.first().copied();
        local.perf.median_ratio = median_f64(&ratios);
        local.perf.largest_ratio = ratios.last().copied();

        ValueReport {
            schema: SCHEMA,
            source: "local",
            generated_at: now,
            period: Period {
                since,
                first_record,
            },
            complete: !incomplete,
            incomplete_since,
            local,
        }
    }

    /// The JSON export: the file a customer may share.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// The plain-language report.
    pub fn render_text(&self, limits: &ResultsStoreLimits) -> String {
        let mut out = String::new();
        let line = |out: &mut String, text: &str| {
            out.push_str(text);
            out.push('\n');
        };
        line(
            &mut out,
            "ByteAsk value report: local results from this machine only (not from your CI)",
        );
        let to = date(self.generated_at);
        match (self.period.since, self.period.first_record) {
            (Some(since), _) => line(&mut out, &format!("Covers {} to {to}.", date(since))),
            (None, Some(first)) => line(
                &mut out,
                &format!(
                    "Covers {} to {to} (everything kept on this machine).",
                    date(first)
                ),
            ),
            (None, None) => line(
                &mut out,
                &format!("Nothing recorded on this machine yet ({to})."),
            ),
        }
        if !self.complete {
            let since = self
                .incomplete_since
                .map(|at| format!(" since {}", date(at)))
                .unwrap_or_default();
            line(
                &mut out,
                &format!(
                    "INCOMPLETE{since}: part of the store could not be read, so each count below is at least that number."
                ),
            );
        }
        let local = &self.local;

        out.push('\n');
        line(&mut out, "Checks run (local)");
        render_table(&mut out, &local.checks);
        out.push('\n');
        line(&mut out, "Other ByteAsk tools (local)");
        render_table(&mut out, &local.other_tools);

        out.push('\n');
        line(&mut out, "Changes (local)");
        line(
            &mut out,
            &format!(
                "  {} {} (one session's work on one branch), {} with an AI edit ByteAsk saw, {} pushed.",
                local.changes.total,
                plural(local.changes.total, "change", "changes"),
                local.changes.with_ai_edit,
                local.changes.pushed
            ),
        );

        out.push('\n');
        line(&mut out, "Problems caught (local)");
        line(
            &mut out,
            &format!(
                "  {} {}: a check failed on a version of the change, and the same check passed on a later version that passed every check, before the change was pushed.",
                local.problems_caught.changes,
                plural(local.problems_caught.changes, "change", "changes"),
            ),
        );
        if !local.problems_caught.by_check.is_empty() {
            let by = local
                .problems_caught
                .by_check
                .iter()
                .map(|(check, count)| format!("{check} {count}"))
                .collect::<Vec<_>>()
                .join(", ");
            line(&mut out, &format!("  By check: {by}."));
        }

        out.push('\n');
        line(&mut out, "Time to review-ready (local)");
        let ready = &local.review_ready;
        match (ready.median_secs, ready.fastest_secs, ready.slowest_secs) {
            (Some(median), Some(fastest), Some(slowest)) => line(
                &mut out,
                &format!(
                    "  Measured for {} {}: median {}, fastest {}, slowest {}.",
                    ready.measured,
                    plural(ready.measured, "change", "changes"),
                    duration(median),
                    duration(fastest),
                    duration(slowest)
                ),
            ),
            _ => line(&mut out, "  Measured for 0 changes."),
        }
        line(
            &mut out,
            "  From the AI's first edit to the first version that passed every check (inside another agent, such as Claude Code: from the start of the turn in which it first changed code).",
        );
        if ready.not_reached > 0 {
            line(
                &mut out,
                &format!(
                    "  {} {} never reached a version that passed every check.",
                    ready.not_reached,
                    plural(ready.not_reached, "change", "changes")
                ),
            );
        }
        if ready.no_ai_edit > 0 {
            line(
                &mut out,
                &format!(
                    "  {} {} not measured: no AI edit was seen (inside another agent, such as Claude Code, ByteAsk sees edits through its hooks: run `byteask toolbox install` there).",
                    ready.no_ai_edit,
                    plural(ready.no_ai_edit, "change", "changes")
                ),
            );
        }

        out.push('\n');
        line(&mut out, "Speed comparisons (local)");
        let perf = &local.perf;
        match (perf.smallest_ratio, perf.median_ratio, perf.largest_ratio) {
            (Some(smallest), Some(median), Some(largest)) => line(
                &mut out,
                &format!(
                    "  {} {} where one side was faster beyond the measurement noise: {smallest:.2}x to {largest:.2}x (median {median:.2}x).",
                    perf.significant,
                    plural(perf.significant, "comparison", "comparisons"),
                ),
            ),
            _ => line(
                &mut out,
                "  0 comparisons where one side was faster beyond the measurement noise.",
            ),
        }
        line(
            &mut out,
            "  Measured with cpp_perf on this machine. This says which side was faster, not what made it faster.",
        );

        if local.results_without_version > 0 {
            out.push('\n');
            line(
                &mut out,
                "Results not tied to a version of the code (local)",
            );
            line(
                &mut out,
                &format!(
                    "  {} {} (not in a git repository, or the code could not be read in time): counted under the tables above only.",
                    local.results_without_version,
                    plural(local.results_without_version, "result", "results")
                ),
            );
        }
        if local.unreadable_records > 0 || local.capped_changes > 0 {
            out.push('\n');
            line(&mut out, "Not counted (local)");
            line(
                &mut out,
                &format!(
                    "  {} unreadable {}, {} {} that stopped recording at the result limit.",
                    local.unreadable_records,
                    plural(local.unreadable_records, "record", "records"),
                    local.capped_changes,
                    plural(local.capped_changes, "change", "changes")
                ),
            );
        }

        out.push('\n');
        line(
            &mut out,
            &format!(
                "This machine keeps results for {} days and up to {} of tool output (policy.toml, [results_store]). Nothing in this report is sent to ByteAsk; it holds counts only, no code, file names or branch names.",
                limits.max_days,
                bytes(limits.max_bytes)
            ),
        );
        out
    }
}

fn plural<'a>(count: u64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn render_table(out: &mut String, table: &BTreeMap<String, VerdictCounts>) {
    if table.is_empty() {
        out.push_str("  none\n");
        return;
    }
    let width = table.keys().map(String::len).max().unwrap_or(0);
    for (check, counts) in table {
        let mut parts = Vec::new();
        for (count, word) in [
            (counts.pass, "passed"),
            (counts.fail, "failed"),
            (counts.no_claim, "no claim"),
            (counts.approximate, "approximate"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {word}"));
            }
        }
        if counts.not_run > 0 {
            let reasons = counts
                .not_run_reasons
                .iter()
                .map(|(reason, count)| format!("{}: {count}", reason_label(reason)))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("{} not run ({reasons})", counts.not_run));
        }
        out.push_str(&format!("  {check:<width$}  {}\n", parts.join(", ")));
    }
}

fn reason_label(key: &str) -> String {
    serde_json::from_value::<check_result::NotRunReason>(serde_json::Value::String(key.to_string()))
        .map(|reason| reason.label().to_string())
        .unwrap_or_else(|_| key.replace('_', " "))
}

fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

fn bytes(count: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    if count >= GB && count.is_multiple_of(GB) {
        format!("{} GB", count / GB)
    } else if count >= MB {
        format!("{} MB", count / MB)
    } else {
        format!("{count} bytes")
    }
}

/// `YYYY-MM-DD` (UTC) for unix seconds.
pub fn date(unix: u64) -> String {
    // Howard Hinnant's civil-from-days.
    let days = (unix / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
