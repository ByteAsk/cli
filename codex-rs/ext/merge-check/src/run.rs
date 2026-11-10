//! `byteask checks run`: the required checks for one merge candidate, on the company's CI
//! runner.
//!
//! The profile and the expected-test manifest come from the protected base branch;
//! the code checked is the tree head merged into base would produce, checked out into a
//! scratch worktree. This step runs the candidate's code, so it holds no signing key: its
//! report is signed afterwards by `byteask checks sign`, in a job that never runs the branch.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use check_result::CheckResult;
use check_result::NotRunReason;
use checks_profile::Profile;
use serde::Deserialize;
use serde::Serialize;

use crate::binding::BaseProfile;
use crate::binding::Binding;
use crate::binding::Candidate;
use crate::git;
use crate::junit;
use crate::step;

pub const RUN_SCHEMA: &str = "byteask.merge-check.run/1";

/// Check names the merge check reports under.
pub const CHECK_BUILD: &str = "build";
pub const CHECK_TESTS: &str = "tests";
pub const CHECK_MANIFEST: &str = "manifest";

/// What `checks run` produces, and what the CI runner key signs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub schema: String,
    pub binding: Binding,
    pub content_key: String,
    pub results: Vec<CheckResult>,
    /// Paths that differ between base and the candidate.
    pub changed_files: usize,
    /// The change touches a marked (hot-path) path.
    pub marked: bool,
    pub host: String,
    pub started_ms: u64,
    pub finished_ms: u64,
}

pub struct RunOptions {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    /// Where scratch worktrees and reports go (default: the system temp dir).
    pub work_dir: Option<PathBuf>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn setup_fix(base: &str) -> String {
    format!(
        "run `byteask checks profile propose`, review it, and commit it as {} on {base} through code review.",
        checks_profile::PATH
    )
}

/// The results when nothing can be built: the same NOT RUN for every required check.
fn not_run_all(
    profile: Option<&Profile>,
    reason: NotRunReason,
    summary: &str,
    fix: &str,
) -> Vec<CheckResult> {
    let mut names = vec![CHECK_BUILD, CHECK_TESTS];
    if profile
        .and_then(|profile| profile.test.as_ref())
        .is_some_and(|test| test.manifest.is_some())
    {
        names.push(CHECK_MANIFEST);
    }
    names
        .into_iter()
        .map(|name| CheckResult::not_run(name, reason, summary, fix))
        .collect()
}

/// Run the required checks for `head` merged into `base`.
pub async fn run_checks(options: &RunOptions) -> Result<RunReport, String> {
    let started_ms = now_ms();
    let candidate = Candidate::compute(&options.repo, &options.base, &options.head)?;
    let key = candidate.binding.content_key();
    let profile = candidate.profile.profile();
    let classes = profile
        .map(Profile::file_classes)
        .unwrap_or_else(checks_profile::FileClasses::defaults);
    let marked = classes.touches_marked(candidate.changed.iter().map(String::as_str));
    let results = match &candidate.profile {
        BaseProfile::Missing => not_run_all(
            None,
            NotRunReason::NoBuildProfile,
            &format!(
                "The base branch has no {}, so nothing defines the checks.",
                checks_profile::PATH
            ),
            &setup_fix(&options.base),
        ),
        BaseProfile::Invalid(err) => not_run_all(
            None,
            NotRunReason::NoBuildProfile,
            &format!("The base branch's build profile does not parse: {err}"),
            &format!(
                "fix {} on {} through code review.",
                checks_profile::PATH,
                options.base
            ),
        ),
        BaseProfile::Valid(profile) => {
            if let Some(paths) = &candidate.conflict {
                not_run_all(
                    Some(profile),
                    NotRunReason::BuildFailed,
                    &format!(
                        "The head does not merge cleanly into {} ({} conflicting path(s)), so it cannot be built.",
                        options.base,
                        paths.len()
                    ),
                    &format!(
                        "merge or rebase onto {} and resolve the conflicts.",
                        options.base
                    ),
                )
            } else if candidate.changed.is_empty()
                || classes.only_non_code(candidate.changed.iter().map(String::as_str))
            {
                // Never PASS for a change that was not built or tested.
                not_run_all(
                    Some(profile),
                    NotRunReason::NoCodeChange,
                    &format!(
                        "The change touches no code or build files ({} path(s), all in the profile's non-code list).",
                        candidate.changed.len()
                    ),
                    "",
                )
            } else {
                check_candidate(options, &candidate, profile, marked).await?
            }
        }
    };
    Ok(RunReport {
        schema: RUN_SCHEMA.to_string(),
        content_key: key.clone(),
        results: results
            .into_iter()
            .map(|result| result.with_content_key(&key))
            .collect(),
        changed_files: candidate.changed.len(),
        marked,
        binding: candidate.binding,
        host: gethostname::gethostname().to_string_lossy().into_owned(),
        started_ms,
        finished_ms: now_ms(),
    })
}

/// A scratch worktree holding the merge candidate; removed on drop.
struct Checkout {
    repo: PathBuf,
    dir: tempfile::TempDir,
}

impl Checkout {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let path = self.dir.path().to_string_lossy().into_owned();
        let _ = git::git(&self.repo, &["worktree", "remove", "--force", &path]);
        let _ = git::git(&self.repo, &["worktree", "prune"]);
    }
}

fn checkout(options: &RunOptions, binding: &Binding) -> Result<Checkout, String> {
    let commit = git::commit_tree(
        &options.repo,
        &binding.merge_tree,
        &[&binding.base_commit, &binding.head_commit],
    )?;
    let parent = options.work_dir.clone().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&parent)
        .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    let dir = tempfile::Builder::new()
        .prefix("byteask-merge-")
        .tempdir_in(&parent)
        .map_err(|err| format!("could not create a scratch directory: {err}"))?;
    let path = dir.path().to_string_lossy().into_owned();
    // `worktree add` wants a missing or empty directory; the temp dir is empty.
    git::git(
        &options.repo,
        &["worktree", "add", "--force", "--detach", &path, &commit],
    )?;
    Ok(Checkout {
        repo: options.repo.clone(),
        dir,
    })
}

fn limit(timeout: Option<u64>) -> Duration {
    Duration::from_secs(timeout.unwrap_or(checks_profile::DEFAULT_TIMEOUT_SECS))
}

/// Run a list of commands in order; stop at the first that does not succeed.
async fn run_steps(
    commands: &[String],
    cwd: &Path,
    env: &[(String, String)],
    total: Duration,
) -> Vec<step::StepOutcome> {
    let deadline = std::time::Instant::now() + total;
    let mut out = Vec::new();
    for command in commands {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let outcome = if left.is_zero() {
            step::StepOutcome {
                command: command.clone(),
                end: step::StepEnd::TimedOut,
                seconds: 0.0,
                tail: Vec::new(),
            }
        } else {
            step::run(command, cwd, env, left).await
        };
        let stop = !outcome.succeeded();
        out.push(outcome);
        if stop {
            break;
        }
    }
    out
}

fn evidence(outcomes: &[step::StepOutcome]) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(last) = outcomes.last() {
        lines.push(format!("$ {}", last.command));
        lines.extend(last.tail.iter().rev().take(20).rev().cloned());
    }
    lines
}

/// The verdict of a list of steps: PASS, FAIL (non-zero exit), or NOT RUN (time limit, could
/// not start).
fn steps_result(
    name: &str,
    what: &str,
    outcomes: &[step::StepOutcome],
    timeout: Duration,
) -> CheckResult {
    let Some(last) = outcomes.last() else {
        return CheckResult::not_run(
            name,
            NotRunReason::NoBuildProfile,
            &format!("The profile lists no {what} commands."),
            "",
        );
    };
    match &last.end {
        step::StepEnd::Exited(Some(0)) => CheckResult::pass(
            name,
            &format!("{} {what} command(s) exited 0.", outcomes.len()),
        ),
        step::StepEnd::Exited(code) => CheckResult::fail(
            name,
            &format!(
                "`{}` exited with {}.",
                short(&last.command),
                code.map_or("a signal".to_string(), |code| format!("status {code}"))
            ),
        )
        .with_evidence(evidence(outcomes)),
        step::StepEnd::TimedOut => CheckResult::not_run(
            name,
            NotRunReason::TimeLimit,
            &format!(
                "`{}` was stopped at the {} s limit.",
                short(&last.command),
                timeout.as_secs()
            ),
            &format!("raise timeout_secs for [{what}] in the profile, or make the {what} faster."),
        )
        .with_evidence(evidence(outcomes)),
        step::StepEnd::NotStarted(err) => CheckResult::not_run(
            name,
            NotRunReason::ToolMissing,
            &format!("`{}` could not start: {err}", short(&last.command)),
            "install bash on the CI runner.",
        ),
    }
}

fn short(command: &str) -> String {
    let first = command.lines().next().unwrap_or_default();
    if first.chars().count() > 80 {
        format!("{}...", first.chars().take(80).collect::<String>())
    } else {
        first.to_string()
    }
}

async fn check_candidate(
    options: &RunOptions,
    candidate: &Candidate,
    profile: &Profile,
    marked: bool,
) -> Result<Vec<CheckResult>, String> {
    let tree = checkout(options, &candidate.binding)?;
    let reports = tempfile::Builder::new()
        .prefix("byteask-results-")
        .tempdir_in(options.work_dir.clone().unwrap_or_else(std::env::temp_dir))
        .map_err(|err| format!("could not create a results directory: {err}"))?;
    let env = vec![
        (
            "BYTEASK_RESULTS_DIR".to_string(),
            reports.path().to_string_lossy().into_owned(),
        ),
        ("BYTEASK_MERGE_CHECK".to_string(), "1".to_string()),
        (
            "BYTEASK_CONTENT_KEY".to_string(),
            candidate.binding.content_key(),
        ),
    ];
    let mut results = Vec::new();

    // Build.
    let build_ok = match &profile.build {
        Some(build) => {
            let timeout = limit(build.timeout_secs);
            let outcomes = run_steps(&build.run, tree.path(), &env, timeout).await;
            let result = steps_result(CHECK_BUILD, "build", &outcomes, timeout);
            let ok = result.is_pass();
            results.push(result);
            ok
        }
        None => {
            results.push(CheckResult::not_run(
                CHECK_BUILD,
                NotRunReason::NoBuildProfile,
                "The profile has no [build] section.",
                &format!(
                    "add [build] to {} on the base branch.",
                    checks_profile::PATH
                ),
            ));
            false
        }
    };
    let blocked = |name: &str| {
        CheckResult::not_run(
            name,
            NotRunReason::BuildFailed,
            "The build did not pass, so this did not run.",
            "fix the build first.",
        )
    };

    // Tests and the expected-test manifest.
    match &profile.test {
        None => results.push(CheckResult::not_run(
            CHECK_TESTS,
            NotRunReason::NoBuildProfile,
            "The profile has no [test] section.",
            &format!("add [test] to {} on the base branch.", checks_profile::PATH),
        )),
        Some(_) if !build_ok => {
            results.push(blocked(CHECK_TESTS));
            if profile
                .test
                .as_ref()
                .is_some_and(|test| test.manifest.is_some())
            {
                results.push(blocked(CHECK_MANIFEST));
            }
        }
        Some(test) => {
            let timeout = limit(test.timeout_secs);
            let outcomes = run_steps(&test.run, tree.path(), &env, timeout).await;
            let mut result = steps_result(CHECK_TESTS, "test", &outcomes, timeout);
            let report = test.junit.as_ref().map(|path| {
                std::fs::read_to_string(reports.path().join(path))
                    .map_err(|_| format!("the test report {path} was not written"))
                    .and_then(|xml| junit::parse(&xml))
            });
            let ran = outcomes
                .last()
                .is_some_and(|last| matches!(last.end, step::StepEnd::Exited(_)));
            if let Some(report) = &report
                && ran
            {
                match report {
                    Ok(cases) => {
                        let failed: Vec<&str> = cases
                            .iter()
                            .filter(|case| case.status == junit::CaseStatus::Failed)
                            .map(|case| case.name.as_str())
                            .collect();
                        if !failed.is_empty() {
                            result = CheckResult::fail(
                                CHECK_TESTS,
                                &format!("{} test(s) failed: {}.", failed.len(), list(&failed)),
                            )
                            .with_evidence(evidence(&outcomes));
                        } else if result.is_pass() {
                            let passed = cases
                                .iter()
                                .filter(|case| case.status == junit::CaseStatus::Passed)
                                .count();
                            result = CheckResult::pass(
                                CHECK_TESTS,
                                &format!(
                                    "{passed} test(s) passed ({} in the report).",
                                    cases.len()
                                ),
                            );
                        }
                    }
                    Err(err) => {
                        result = CheckResult::fail(
                            CHECK_TESTS,
                            &format!("The profile says the tests write a report, but {err}."),
                        )
                        .with_evidence(evidence(&outcomes));
                    }
                }
            }
            let tests_ran_to_the_end = ran;
            results.push(result.clone());
            if let Some(path) = &test.manifest {
                results.push(manifest_result(
                    &options.repo,
                    &candidate.binding.base_commit,
                    path,
                    tests_ran_to_the_end,
                    &result,
                    report.as_ref(),
                ));
            }
        }
    }

    // Slow checks: only for a change touching a marked path.
    for slow in &profile.slow {
        if !marked {
            continue;
        }
        if !build_ok {
            results.push(blocked(&slow.name));
            continue;
        }
        let timeout = limit(slow.timeout_secs);
        let outcomes = run_steps(&slow.run, tree.path(), &env, timeout).await;
        results.push(steps_result(&slow.name, &slow.name, &outcomes, timeout));
    }
    drop(tree);
    Ok(results)
}

fn list(names: &[&str]) -> String {
    let shown: Vec<&str> = names.iter().take(8).copied().collect();
    let more = names.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Every test the base branch's manifest names must have run and passed.
fn manifest_result(
    repo: &Path,
    base_commit: &str,
    path: &str,
    tests_ran: bool,
    tests: &CheckResult,
    report: Option<&Result<Vec<junit::Case>, String>>,
) -> CheckResult {
    let Some(text) = git::show(repo, base_commit, path) else {
        return CheckResult::not_run(
            CHECK_MANIFEST,
            NotRunReason::NoBuildProfile,
            &format!(
                "The profile names the test manifest {path}, but the base branch has no such file."
            ),
            &format!("commit {path} to the base branch, or remove `manifest` from the profile."),
        );
    };
    let wanted = junit::manifest_names(&text);
    if !tests_ran {
        let reason = tests.reason.unwrap_or(NotRunReason::ToolError);
        return CheckResult::not_run(
            CHECK_MANIFEST,
            reason,
            "The tests did not run to the end, so the required tests cannot be confirmed.",
            "",
        );
    }
    let cases = match report {
        None => {
            return CheckResult::fail(
                CHECK_MANIFEST,
                "The profile lists required tests but no test report (`junit`), so none can be confirmed.",
            );
        }
        Some(Err(err)) => {
            return CheckResult::fail(
                CHECK_MANIFEST,
                &format!("The required tests cannot be confirmed: {err}."),
            );
        }
        Some(Ok(cases)) => cases,
    };
    let mut missing = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    for name in &wanted {
        let found: Vec<&junit::Case> = cases.iter().filter(|case| case.matches(name)).collect();
        if found.is_empty() {
            missing.push(name.as_str());
        } else if found
            .iter()
            .any(|case| case.status == junit::CaseStatus::Failed)
        {
            failed.push(name.as_str());
        } else if found
            .iter()
            .all(|case| case.status == junit::CaseStatus::Skipped)
        {
            skipped.push(name.as_str());
        }
    }
    if missing.is_empty() && skipped.is_empty() && failed.is_empty() {
        return CheckResult::pass(
            CHECK_MANIFEST,
            &format!("All {} required test(s) ran and passed.", wanted.len()),
        );
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("did not run: {}", list(&missing)));
    }
    if !skipped.is_empty() {
        parts.push(format!("skipped: {}", list(&skipped)));
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", list(&failed)));
    }
    CheckResult::fail(
        CHECK_MANIFEST,
        &format!(
            "Required tests from the base branch's manifest {}.",
            parts.join("; ")
        ),
    )
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
