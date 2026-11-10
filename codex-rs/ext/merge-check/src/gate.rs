//! `byteask checks gate`: the merge status check.
//!
//! It decides from signed CI results only: a result counts when an enrolled runner key of
//! the right kind signed it (the enterprise server applies retirement versus revocation)
//! and it is bound to this exact merge candidate. Anything else, a local result on
//! the developer's machine, an unsigned file, a payload edited after signing, a result for an
//! older base, is ignored and listed with the reason.
//!
//! - every required check PASS: merge allowed, shown as PASS;
//! - any FAIL: blocked;
//! - any NOT RUN: blocked, unless every NOT RUN reason is one the profile lets a reviewer
//!   approve and a person (not a bot, not the author) approved this head after the result was
//!   produced; then allowed, shown as NOT RUN with who approved it, never as PASS.
//! - no trusted result at all: NOT RUN (not checked), which nobody can approve.

use std::path::Path;
use std::path::PathBuf;

use check_result::CheckResult;
use check_result::NotRunReason;
use check_result::Verdict;
use checks_profile::PerfPolicy;
use serde::Deserialize;
use serde::Serialize;

use crate::binding::Binding;
use crate::binding::Candidate;
use crate::perf;
use crate::perf::PerfEvidence;
use crate::run::CHECK_BUILD;
use crate::run::CHECK_TESTS;
use crate::run::RunReport;
use crate::sign::Envelope;
use crate::sign::RunnerKind;
use crate::verify::Verifier;

pub const DECISION_SCHEMA: &str = "byteask.merge-check.decision/1";

/// A review on the code platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub reviewer: String,
    /// A bot or an app account (never counts).
    pub is_bot: bool,
    /// The change's own author (never counts).
    pub is_author: bool,
    /// The head commit the review was given on.
    pub head_commit: String,
    /// When it was given (Unix milliseconds).
    pub at_ms: u64,
}

/// One signed CI or bench result that counted.
#[derive(Clone, Debug)]
pub struct Accepted<T> {
    pub value: T,
    pub key_id: String,
    pub signed_ms: u64,
}

/// Every envelope sorted into what counts and what does not.
#[derive(Clone, Debug, Default)]
pub struct Evidence {
    pub ci: Vec<Accepted<RunReport>>,
    pub perf: Vec<Accepted<PerfEvidence>>,
    /// `<file>: <why it does not count>`.
    pub ignored: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub schema: String,
    /// The merge may go ahead.
    pub allowed: bool,
    /// PASS only when every required check passed; otherwise FAIL or NOT RUN.
    pub verdict: Verdict,
    /// One line for a status page.
    pub summary: String,
    /// The merge candidate the decision is for.
    pub candidate_key: String,
    /// The content key of the CI result it rests on (else this job's own).
    pub content_key: String,
    pub binding: Binding,
    pub checks: Vec<CheckResult>,
    /// The person whose approval let a NOT RUN through.
    pub approved_by: Option<String>,
    /// Results that did not count, and why.
    pub ignored: Vec<String>,
    /// The perf card, when the profile asks for one and a bench result exists.
    pub perf_card: Option<String>,
}

/// Read every `*.json` envelope in the given files and directories (a directory named after
/// the candidate key inside a cache directory is read too).
pub fn collect_envelopes(
    paths: &[PathBuf],
    candidate_key: &str,
) -> Vec<(String, Result<Envelope, String>)> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            for dir in [path.clone(), path.join(candidate_key)] {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    let mut found: Vec<PathBuf> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
                        .collect();
                    found.sort();
                    files.extend(found);
                }
            }
        } else {
            files.push(path.clone());
        }
    }
    files
        .into_iter()
        .map(|file| {
            let name = file.display().to_string();
            let parsed = std::fs::read_to_string(&file)
                .map_err(|err| format!("could not read it: {err}"))
                .and_then(|text| Envelope::parse(&text));
            (name, parsed)
        })
        .collect()
}

/// Verify each envelope and keep what counts for this merge candidate.
pub async fn sort_evidence(
    envelopes: Vec<(String, Result<Envelope, String>)>,
    verifier: &dyn Verifier,
    candidate: &Binding,
) -> Evidence {
    let mut evidence = Evidence::default();
    for (name, parsed) in envelopes {
        let envelope = match parsed {
            Ok(envelope) => envelope,
            Err(err) => {
                evidence.ignored.push(format!("{name}: {err}"));
                continue;
            }
        };
        let payload: serde_json::Value = match serde_json::from_str(&envelope.payload) {
            Ok(value) => value,
            Err(_) => {
                evidence
                    .ignored
                    .push(format!("{name}: its payload is not JSON"));
                continue;
            }
        };
        let want = match payload["schema"].as_str() {
            Some(crate::run::RUN_SCHEMA) => RunnerKind::Ci,
            Some(perf::PERF_SCHEMA) => RunnerKind::Bench,
            other => {
                evidence
                    .ignored
                    .push(format!("{name}: unknown payload schema {other:?}"));
                continue;
            }
        };
        // Bound to exactly this candidate, and its key is the one its binding gives.
        let bound: Option<Binding> = serde_json::from_value(payload["binding"].clone()).ok();
        let content_key = payload["content_key"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        match &bound {
            Some(binding)
                if binding.same_candidate(candidate) && binding.content_key() == content_key => {}
            _ => {
                evidence.ignored.push(format!(
                    "{name}: it is for another version (content key {}), not this merge candidate",
                    if content_key.is_empty() {
                        "none"
                    } else {
                        &content_key[..content_key.len().min(12)]
                    }
                ));
                continue;
            }
        }
        let content_key = content_key.as_str();
        let answer = match verifier.verify(&envelope).await {
            Ok(answer) => answer,
            Err(err) => {
                evidence
                    .ignored
                    .push(format!("{name}: its signature could not be checked: {err}"));
                continue;
            }
        };
        if let Err(why) = crate::verify::accept(&envelope, &answer, want) {
            evidence.ignored.push(format!("{name}: {why}"));
            continue;
        }
        match want {
            RunnerKind::Ci => match serde_json::from_value::<RunReport>(payload) {
                Ok(report)
                    if report
                        .results
                        .iter()
                        .all(|r| r.content_key.as_deref() == Some(content_key)) =>
                {
                    evidence.ci.push(Accepted {
                        value: report,
                        key_id: envelope.key_id.clone(),
                        signed_ms: envelope.signed_ms,
                    });
                }
                Ok(_) => evidence
                    .ignored
                    .push(format!("{name}: a result inside it is for another version")),
                Err(err) => evidence
                    .ignored
                    .push(format!("{name}: not a run report: {err}")),
            },
            RunnerKind::Bench => match serde_json::from_value::<PerfEvidence>(payload) {
                Ok(perf) => evidence.perf.push(Accepted {
                    value: perf,
                    key_id: envelope.key_id.clone(),
                    signed_ms: envelope.signed_ms,
                }),
                Err(err) => evidence
                    .ignored
                    .push(format!("{name}: not perf evidence: {err}")),
            },
        }
    }
    evidence
}

/// The decision. Pure: every rule is tested without a server.
pub fn decide(candidate: &Candidate, evidence: &Evidence, approvals: &[Approval]) -> Decision {
    let binding = &candidate.binding;
    let key = binding.content_key();
    let profile = candidate.profile.profile();
    let ci = evidence.ci.iter().max_by_key(|accepted| accepted.signed_ms);
    let mut checks: Vec<CheckResult> = match ci {
        Some(accepted) => accepted.value.results.clone(),
        None => [CHECK_BUILD, CHECK_TESTS]
            .into_iter()
            .map(|name| {
                CheckResult::not_run(
                    name,
                    NotRunReason::NotChecked,
                    "No result signed by an enrolled CI runner exists for this exact merge candidate.",
                    "",
                )
                .with_content_key(&key)
            })
            .collect(),
    };

    // The perf card.
    let marked = profile.is_some_and(|profile| {
        profile
            .file_classes()
            .touches_marked(candidate.changed.iter().map(String::as_str))
    });
    let perf_policy = profile.map_or(PerfPolicy::Off, |profile| profile.merge.perf);
    let bench = evidence
        .perf
        .iter()
        .max_by_key(|accepted| accepted.signed_ms);
    let mut perf_card = None;
    if perf_policy != PerfPolicy::Off {
        match bench {
            Some(accepted) => {
                perf_card = Some(perf::card(&accepted.value.ut));
                if perf_policy == PerfPolicy::Required && marked {
                    checks.push(perf::result(&accepted.value.ut).with_content_key(&key));
                }
            }
            None if perf_policy == PerfPolicy::Required && marked => checks.push(
                CheckResult::not_run(
                    perf::CHECK_PERF,
                    NotRunReason::NotChecked,
                    "The change touches a marked path and no signed bench result exists for this exact merge candidate.",
                    "run the perf check on the bench machine for this version.",
                )
                .with_content_key(&key),
            ),
            None => {}
        }
    }

    let failed: Vec<&CheckResult> = checks
        .iter()
        .filter(|c| c.verdict == Verdict::Fail)
        .collect();
    let not_run: Vec<&CheckResult> = checks
        .iter()
        .filter(|c| !matches!(c.verdict, Verdict::Pass | Verdict::Fail))
        .collect();
    let approvable = profile.map_or_else(Vec::new, |profile| {
        profile.merge.reviewer_may_approve.clone()
    });
    let (allowed, verdict, approved_by, summary) = if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|c| c.check.as_str()).collect();
        (
            false,
            Verdict::Fail,
            None,
            format!("FAIL: {} ({})", names.join(", "), failed[0].summary),
        )
    } else if !not_run.is_empty() {
        let reasons: Vec<NotRunReason> = not_run.iter().filter_map(|c| c.reason).collect();
        let all_approvable = ci.is_some()
            && reasons.len() == not_run.len()
            && reasons
                .iter()
                .all(|reason| *reason != NotRunReason::NotChecked && approvable.contains(reason));
        let label = not_run[0]
            .reason
            .map_or("not run".to_string(), |reason| reason.label().to_string());
        let approval = ci.filter(|_| all_approvable).and_then(|accepted| {
            approvals
                .iter()
                .filter(|a| a.counts_for(binding, accepted.signed_ms))
                .max_by_key(|a| a.at_ms)
        });
        match approval {
            Some(approval) => (
                true,
                Verdict::NotRun,
                Some(approval.reviewer.clone()),
                format!(
                    "NOT RUN ({label}), approved by {} to merge without it. Not a pass.",
                    approval.reviewer
                ),
            ),
            None => {
                let how = if all_approvable {
                    " A reviewer may approve this reason; no approval of this exact version yet."
                } else {
                    ""
                };
                (
                    false,
                    Verdict::NotRun,
                    None,
                    format!("NOT RUN ({label}): {}{how}", not_run[0].summary),
                )
            }
        }
    } else {
        (
            true,
            Verdict::Pass,
            None,
            format!(
                "PASS: {} required check(s) passed for this exact merge candidate.",
                checks.len()
            ),
        )
    };
    Decision {
        schema: DECISION_SCHEMA.to_string(),
        allowed,
        verdict,
        summary: one_line(&summary),
        candidate_key: binding.candidate_key(),
        content_key: ci.map_or(key, |accepted| accepted.value.content_key.clone()),
        binding: binding.clone(),
        checks,
        approved_by,
        ignored: evidence.ignored.clone(),
        perf_card,
    }
}

impl Approval {
    /// A review counts for the version it was given on, after the result it approves
    /// existed, and only from a person who is not the author.
    pub fn counts_for(&self, binding: &Binding, result_signed_ms: u64) -> bool {
        !self.is_bot
            && !self.is_author
            && self.head_commit == binding.head_commit
            && self.at_ms >= result_signed_ms
    }
}

fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    line.chars().take(300).collect()
}

/// The decision as a Markdown report (GitHub check output, Jenkins summary).
pub fn report_markdown(decision: &Decision) -> String {
    let mut out = format!(
        "**{}**\n\n| check | result |\n|---|---|\n",
        decision.summary
    );
    for check in &decision.checks {
        out.push_str(&format!(
            "| {} | {} |\n",
            check.check.replace('|', "/"),
            check
                .render_line()
                .trim_start_matches(check_result::LINE_PREFIX)
                .replace('|', "/")
        ));
    }
    out.push_str(&format!(
        "\nMerge candidate: base `{}` + head `{}` (tree `{}`), candidate key `{}`.\n",
        short(&decision.binding.base_commit),
        short(&decision.binding.head_commit),
        short(&decision.binding.merge_tree),
        short(&decision.candidate_key)
    ));
    if !decision.ignored.is_empty() {
        out.push_str("\nResults that did not count:\n");
        for why in &decision.ignored {
            out.push_str(&format!("- {why}\n"));
        }
    }
    if let Some(card) = &decision.perf_card {
        out.push('\n');
        out.push_str(card);
    }
    out
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Where a signed result for the candidate `candidate_key` belongs in a CI-side cache.
pub fn cache_path(cache: &Path, candidate_key: &str, envelope: &Envelope) -> PathBuf {
    cache.join(candidate_key).join(format!(
        "{}-{}.json",
        envelope.kind.as_str(),
        &envelope.digest()[..16]
    ))
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
