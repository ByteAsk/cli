//! The checks on the target's code as it is (a local runner over the toolbox).
//!
//! The tools run exactly as an agent's calls do (same registry entries, same
//! sandbox, same result line), through a toolbox thread that holds the C/C++
//! tools only. That thread records nothing in the results store: the store's
//! counts are evidence from AI sessions, and the trial's own run is reported
//! once, in its own section, so nobody can mistake it for a session's work.
//!
//! Running the project's code (the sanitizer check) needs trust. The engineer
//! typed `byteask company trial` for this project, which is consent for this
//! run only: the trust is set on this run's in-memory config and never written
//! to `config.toml`.

use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use check_result::CheckResult;
use check_result::NotRunReason;
use check_result::Verdict;
use codex_protocol::config_types::TrustLevel;
use codex_utils_cli::CliConfigOverrides;

use super::Step;
use super::build::Built;
use super::state::CheckTally;
use crate::registry::ToolboxThread;

/// The checks the trial runs, in order.
pub const CHECKS: [&str; 3] = ["cpp_check", "cpp_tidy", "cpp_sanitize"];
/// At most this many of the target's files are checked one by one.
pub const MAX_FILES: usize = 10;

/// One translation unit to check: its path relative to the project, and the
/// project's own include directories from its compile command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub path: String,
    pub include_dirs: Vec<String>,
}

/// The target's translation units inside the project, from the indexed
/// compile DB, sorted; and how many more there were than the cap.
pub fn units(compdb: &Path, root: &Path) -> (Vec<Unit>, usize) {
    let text = std::fs::read_to_string(compdb).unwrap_or_default();
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    let mut units: Vec<Unit> = Vec::new();
    for entry in &entries {
        let directory = PathBuf::from(entry["directory"].as_str().unwrap_or_default());
        let Some(file) = entry["file"].as_str() else {
            continue;
        };
        let file = directory.join(file);
        let Ok(rel) = file.strip_prefix(root) else {
            continue;
        };
        let args: Vec<String> = match entry["arguments"].as_array() {
            Some(args) => args
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect(),
            None => entry["command"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        };
        let mut include_dirs = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            let dir = if arg == "-I" || arg == "-iquote" {
                i += 1;
                args.get(i).cloned()
            } else if let Some(dir) = arg.strip_prefix("-I") {
                Some(dir.to_string())
            } else {
                arg.strip_prefix("-iquote")
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
            };
            if let Some(dir) = dir {
                let dir = directory.join(dir);
                let shown = dir
                    .strip_prefix(root)
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| dir.to_string_lossy().into_owned());
                let shown = if shown.is_empty() {
                    ".".to_string()
                } else {
                    shown
                };
                if !include_dirs.contains(&shown) {
                    include_dirs.push(shown);
                }
            }
            i += 1;
        }
        let path = rel.to_string_lossy().into_owned();
        if !units.iter().any(|unit| unit.path == path) {
            units.push(Unit { path, include_dirs });
        }
    }
    units.sort_by(|a, b| a.path.cmp(&b.path));
    let over = units.len().saturating_sub(MAX_FILES);
    units.truncate(MAX_FILES);
    (units, over)
}

/// Add one result to a tally.
pub fn count(tally: &mut CheckTally, result: &CheckResult) {
    tally.runs += 1;
    match result.verdict {
        Verdict::Pass => tally.pass += 1,
        Verdict::Fail => tally.fail += 1,
        Verdict::NoClaim => tally.no_claim += 1,
        Verdict::Approximate => tally.approximate += 1,
        Verdict::NotRun => {
            tally.not_run += 1;
            let reason = result
                .reason
                .map(|r| r.label().to_string())
                .unwrap_or_default();
            let fix = result.fix.clone().unwrap_or_default();
            if !tally.not_run_reasons.iter().any(|(r, _)| *r == reason) {
                tally.not_run_reasons.push((reason, fix));
            }
        }
    }
}

/// One check's step: the worst verdict over its runs (FAIL, then NOT RUN,
/// then APPROXIMATE, then NO CLAIM; PASS only when every run passed).
pub fn step_result(tally: &CheckTally) -> CheckResult {
    let check = tally.check.as_str();
    let files = if check == "cpp_sanitize" {
        "run"
    } else {
        "file"
    };
    let plural = if tally.runs == 1 { "" } else { "s" };
    let mut parts = vec![];
    for (n, word) in [
        (tally.pass, "passed"),
        (tally.fail, "failed"),
        (tally.not_run, "not run"),
        (tally.approximate, "approximate"),
        (tally.no_claim, "no claim"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    let mut summary = format!("{} {files}{plural}: {}", tally.runs, parts.join(", "));
    if tally.not_checked > 0 {
        summary.push_str(&format!(" ({} more files not checked)", tally.not_checked));
    }
    if tally.fail > 0 {
        CheckResult::fail(check, &summary)
    } else if tally.not_run > 0 || tally.runs == 0 {
        let (reason, fix) = tally.not_run_reasons.first().cloned().unwrap_or_default();
        let reason = NotRunReason::ALL
            .into_iter()
            .find(|r| r.label() == reason)
            .unwrap_or(NotRunReason::ToolError);
        CheckResult::not_run(check, reason, &summary, &fix)
    } else if tally.approximate > 0 {
        CheckResult::approximate(check, &summary)
    } else if tally.no_claim > 0 && tally.pass == 0 {
        CheckResult::no_claim(check, &summary)
    } else {
        CheckResult::pass(check, &summary)
    }
}

async fn call(thread: &ToolboxThread, check: &str, args: serde_json::Value) -> CheckResult {
    match thread.call(check, args.to_string()).await {
        Some(outcome) => CheckResult::from_tool_text(check, &outcome.text).unwrap_or_else(|| {
            CheckResult::not_run(
                check,
                NotRunReason::ToolError,
                "the tool answered without a result line",
                "run `byteask company trial` again",
            )
        }),
        None => CheckResult::not_run(
            check,
            NotRunReason::ToolMissing,
            "this ByteAsk has no such tool",
            "use the byteask from the trial bundle",
        ),
    }
}

#[allow(clippy::print_stderr)]
pub async fn run(
    overrides: &CliConfigOverrides,
    root: &Path,
    built: &Built,
    compdb: &Path,
    linux_sandbox_exe: Option<PathBuf>,
    steps: &mut Vec<Step>,
) -> Vec<CheckTally> {
    let mut config =
        match crate::cli::load_config(overrides, root.to_path_buf(), linux_sandbox_exe).await {
            Ok(config) => config,
            Err(err) => {
                for check in CHECKS {
                    steps.push(super::step(
                        check,
                        Instant::now(),
                        CheckResult::not_run(
                            check,
                            NotRunReason::ToolError,
                            &format!("the ByteAsk config could not be loaded: {err}"),
                            "fix config.toml in your ByteAsk home, then run the trial again",
                        ),
                    ));
                }
                return Vec::new();
            }
        };
    // Consent for this run only (see the module docs).
    config.active_project.trust_level = Some(TrustLevel::Trusted);
    let thread = ToolboxThread::start_with(&config, codex_cpp_tools::install).await;
    let (units, over) = units(compdb, root);
    let mut tallies = Vec::new();

    for check in ["cpp_check", "cpp_tidy"] {
        let started = Instant::now();
        let mut tally = CheckTally {
            check: check.to_string(),
            not_checked: over as u64,
            ..CheckTally::default()
        };
        for unit in &units {
            let args = if check == "cpp_check" {
                serde_json::json!({"path": unit.path, "include_dirs": unit.include_dirs})
            } else {
                serde_json::json!({"path": unit.path, "build_dir": built.build_dir.to_string_lossy()})
            };
            let result = call(&thread, check, args).await;
            eprintln!(
                "      {check} {}: {}",
                unit.path,
                result
                    .render_line()
                    .trim_start_matches(check_result::LINE_PREFIX)
            );
            count(&mut tally, &result);
        }
        steps.push(super::step(check, started, step_result(&tally)));
        tallies.push(tally);
    }

    let started = Instant::now();
    let mut tally = CheckTally {
        check: "cpp_sanitize".to_string(),
        ..CheckTally::default()
    };
    let result = match &built.run_target {
        Some(run_target) => {
            call(
                &thread,
                "cpp_sanitize",
                serde_json::json!({"project": true, "target": run_target}),
            )
            .await
        }
        None => CheckResult::not_run(
            "cpp_sanitize",
            NotRunReason::Unsupported,
            "the target is a library and no test links it, so there is no program to run",
            "pass --run-target with an executable that links the target",
        ),
    };
    count(&mut tally, &result);
    steps.push(super::step("cpp_sanitize", started, step_result(&tally)));
    tallies.push(tally);
    tallies
}
