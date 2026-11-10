//! The trial's local value report.
//!
//! Written to `$BYTEASK_HOME/trial/report.txt` and `report.json`. Counts and
//! measured results only, like the value report it embeds: no code, no file,
//! branch, project or target names, no tool text (a tool's summary can quote a
//! file name, so only verdicts and reason labels appear). Every number is
//! labelled as measured on this machine, and the trial's own checks are kept
//! apart from AI sessions: a failure in code nobody had changed yet is never a
//! problem caught.

use std::path::Path;
use std::path::PathBuf;

use check_result::Verdict;

use super::state::CheckTally;
use super::state::TrialState;

pub struct Written {
    pub text: String,
    pub json: String,
    pub text_path: PathBuf,
}

fn verdict_word(result: &check_result::CheckResult) -> String {
    match (result.verdict, result.reason) {
        (Verdict::NotRun, Some(reason)) => format!("NOT RUN ({})", reason.label()),
        (verdict, _) => verdict.word().to_string(),
    }
}

fn tally_line(tally: &CheckTally) -> String {
    let unit = if tally.check == "cpp_sanitize" {
        "run"
    } else {
        "file"
    };
    let plural = if tally.runs == 1 { "" } else { "s" };
    let mut parts = Vec::new();
    for (n, word) in [
        (tally.pass, "passed"),
        (tally.fail, "failed"),
        (tally.approximate, "approximate"),
        (tally.no_claim, "no claim"),
        (tally.not_run, "not run"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    let mut line = format!("{} {unit}{plural}: {}", tally.runs, parts.join(", "));
    if !tally.not_run_reasons.is_empty() {
        let reasons: Vec<&str> = tally
            .not_run_reasons
            .iter()
            .map(|(r, _)| r.as_str())
            .collect();
        line.push_str(&format!(" (not run: {})", reasons.join(", ")));
    }
    if tally.not_checked > 0 {
        line.push_str(&format!("; {} more files not checked", tally.not_checked));
    }
    line
}

/// The report as text and JSON.
pub fn render(
    state: Option<&TrialState>,
    sessions: &results_store::ValueReport,
    sessions_text: &str,
    now: u64,
) -> (String, serde_json::Value) {
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line("ByteAsk company trial: local value report");
    line(&format!(
        "Written {} on this machine. Counts only: no code and no file, branch, project or target \
         names. Nothing in it has been sent anywhere.",
        results_store::report::date(now)
    ));
    let mut json = serde_json::json!({
        "schema": 1,
        "kind": "byteask company trial",
        "source": "local",
        "written_at": now,
    });
    match state {
        None => {
            line("");
            line(
                "No trial has run on this machine yet: run `byteask company trial` in a CMake project.",
            );
        }
        Some(state) => {
            line("");
            line(&format!(
                "Trial run of {} (local)",
                results_store::report::date(state.updated_at)
            ));
            for step in &state.steps {
                line(&format!(
                    "  {:<16} {}",
                    step.name,
                    verdict_word(&step.result)
                ));
            }
            json["trial"] = serde_json::json!({
                "run_at": state.updated_at,
                "build_type": state.build_type,
                "steps": state.steps.iter().map(|step| serde_json::json!({
                    "step": step.name,
                    "verdict": step.result.verdict,
                    "reason": step.result.reason,
                    "seconds": step.seconds,
                })).collect::<Vec<_>>(),
            });
            line("");
            line("Index of one target and the first-party targets it builds with (local)");
            match &state.index {
                Some(index) => {
                    let lines = match (index.lines, index.files) {
                        (Some(lines), Some(files)) => format!("{lines} in {files} files"),
                        _ => "no count yet (no completed index)".to_string(),
                    };
                    line(&format!(
                        "  translation units indexed    {}",
                        index.translation_units
                    ));
                    line(&format!("  targets in the build closure {}", index.targets));
                    line(&format!("  first-party lines indexed    {lines}"));
                    line(&format!(
                        "  indexed only approximately   {:.1}% of lines",
                        index.approximate_share * 100.0
                    ));
                    if let Some(versions) = index.template_versions {
                        line(&format!("  compiled template versions   {versions}"));
                    }
                    line(&format!(
                        "  production compiler          {} (parsed by the pinned clang 21.1.8)",
                        index.compiler
                    ));
                    line(&format!(
                        "  index time                   {:.1} s",
                        index.seconds
                    ));
                    if index.committed == "kept" {
                        line(
                            "  this run kept the last committed index (the checkout has uncommitted changes)",
                        );
                    }
                    line(&format!("  uncommitted changes          {}", index.overlay));
                    line(
                        "  Every context_* answer names its build setting and commit, with the call that",
                    );
                    line("  shows the compile line.");
                    json["index"] = serde_json::to_value(index).unwrap_or_default();
                }
                None => line("  not built in this run (see the steps above)"),
            }
            line("");
            line("Checks on the code as it was, before any AI session (local; never counted as");
            line("problems caught)");
            if state.checks.is_empty() {
                line("  none ran (see the steps above)");
            }
            for tally in &state.checks {
                line(&format!("  {:<13} {}", tally.check, tally_line(tally)));
            }
            json["checks_before_ai"] = serde_json::json!(
                state.checks.iter().map(|tally| serde_json::json!({
                    "check": tally.check,
                    "runs": tally.runs,
                    "pass": tally.pass,
                    "fail": tally.fail,
                    "approximate": tally.approximate,
                    "no_claim": tally.no_claim,
                    "not_run": tally.not_run,
                    "not_run_reasons": tally.not_run_reasons.iter().map(|(r, _)| r).collect::<Vec<_>>(),
                    "not_checked": tally.not_checked,
                })).collect::<Vec<_>>()
            );
        }
    }
    line("");
    line("AI sessions on this machine, from the local results store");
    for text in sessions_text.lines() {
        line(&format!("  {text}"));
    }
    json["ai_sessions"] = sessions.to_json();
    (out, json)
}

/// Write `report.txt` and `report.json` under `$BYTEASK_HOME/trial/`.
pub fn write(home: &Path, state: Option<&TrialState>, now: u64) -> anyhow::Result<Written> {
    let (limits, _) = byteask_policy::results_store_limits(home);
    let loaded = results_store::store::load(&results_store::store::root(home));
    let sessions = results_store::ValueReport::build(&loaded, now, None);
    let sessions_text = sessions.render_text(&limits);
    let (text, json) = render(state, &sessions, &sessions_text, now);
    let json = format!("{}\n", serde_json::to_string_pretty(&json)?);
    let dir = super::state::dir(home);
    context_tools::engine::create_private_dir(&dir)?;
    let text_path = dir.join("report.txt");
    std::fs::write(&text_path, &text)?;
    std::fs::write(dir.join("report.json"), &json)?;
    Ok(Written {
        text,
        json,
        text_path,
    })
}
