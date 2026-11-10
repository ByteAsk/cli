//! `byteask-engine verify`: build and test without a model.
//!
//! - no `--checks` (`mode: baseline`): the environment recipe's `build.run` then `test.run`;
//! - `--checks FILE` (`mode: verify`): exactly that file's commands, same ids, cwd, purpose
//!   and order.
//!
//! `repo.tree_sha` is `git write-tree` of the index before any command runs (the runner stages
//! the patch with `git apply --index` and records the same tree; untracked files never count). Exit 0 when it ran (results are in the file, even if
//! tests failed), 2 when it could not run.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use clap::Parser;

use crate::Checks;
use crate::CommandEntry;
use crate::MAX_COMMANDS;
use crate::MAX_JUNIT_PER_COMMAND;
use crate::Mode;
use crate::Purpose;
use crate::RepoInfo;
use crate::TestStatus;
use crate::agent::read_fresh;
use crate::classify;
use crate::dedupe_tests;
use crate::duration_s;
use crate::git;
use crate::junit;
use crate::log_tail;
use crate::reports;
use crate::run;

const EXIT_RAN: i32 = 0;
const EXIT_COULD_NOT_RUN: i32 = 2;
const DEFAULT_TIMEOUT_S: u64 = 3600;
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// Re-run build and test checks without a model and write checks.json (Cloud Agent harness).
#[derive(Debug, Parser)]
pub struct VerifyCli {
    /// The repository to check, as its working tree is now.
    #[arg(short = 'C', long = "cd", value_name = "REPO")]
    pub repo: PathBuf,

    /// Where to write checks.json.
    #[arg(long = "out", value_name = "FILE")]
    pub out: PathBuf,

    /// Re-run exactly the commands of this checks.json (mode verify). Without it, build and
    /// test from the environment recipe (mode baseline).
    #[arg(long = "checks", value_name = "FILE")]
    pub checks: Option<PathBuf>,

    /// The environment recipe, relative to the repository unless absolute.
    #[arg(
        long = "env-spec",
        value_name = "FILE",
        default_value = ".byteask/environment.toml"
    )]
    pub env_spec: PathBuf,

    /// Per-command timeout in seconds (default: the recipe's `timeout_s`, else 3600).
    #[arg(long = "timeout-s", value_name = "N")]
    pub timeout_s: Option<u64>,
}

#[derive(Debug)]
enum JunitSource {
    /// Recipe `test.junit` patterns, relative to the repo root.
    Globs(Vec<String>),
    /// The input checks.json's `junit` paths, relative to the repo root.
    Paths(Vec<String>),
    /// Whatever the command asks its test runner to write (`ctest --output-junit`, ...).
    Detect,
}

#[derive(Debug)]
struct Planned {
    id: String,
    purpose: Purpose,
    cmd: String,
    cwd: String,
    timeout: Duration,
    junit: JunitSource,
    /// verify: the input's tests for this command, reported as `error` if the re-run wrote no
    /// readable report (a missing result must never read as "no failures").
    prior_tests: Vec<(String, TestStatus)>,
}

/// The recipe fields verify uses (the runner validates the whole file against the schema).
#[derive(Debug, Default)]
struct Recipe {
    build_run: Option<String>,
    build_timeout_s: Option<u64>,
    test_run: Option<String>,
    test_timeout_s: Option<u64>,
    test_junit: Option<Vec<String>>,
}

fn read_recipe(path: &Path) -> Result<Recipe, String> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        format!(
            "cannot read the environment recipe {}: {err}",
            path.display()
        )
    })?;
    let doc: toml::Table = toml::from_str(&text)
        .map_err(|err| format!("{} is not valid TOML: {err}", path.display()))?;
    if doc.get("version").and_then(toml::Value::as_integer) != Some(1) {
        return Err(format!("{}: `version = 1` is required", path.display()));
    }
    let table = |name: &str| doc.get(name).and_then(toml::Value::as_table);
    let string = |section: &str, key: &str| {
        table(section)
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    let seconds = |section: &str| {
        table(section)
            .and_then(|t| t.get("timeout_s"))
            .and_then(toml::Value::as_integer)
            .and_then(|n| u64::try_from(n).ok())
    };
    let junit = table("test").and_then(|t| t.get("junit")).and_then(|v| {
        v.as_array().map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
    });
    Ok(Recipe {
        build_run: string("build", "run"),
        build_timeout_s: seconds("build"),
        test_run: string("test", "run"),
        test_timeout_s: seconds("test"),
        test_junit: junit,
    })
}

fn is_command_id(id: &str) -> bool {
    let mut chars = id.chars();
    let lower_or_digit = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    id.len() <= 32
        && chars.next().is_some_and(lower_or_digit)
        && chars.all(|c| lower_or_digit(c) || c == '_' || c == '-')
}

/// A path checks.json may name: relative, inside the repo, no `..`.
fn is_relpath(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains('\0')
        && !path.contains('\n')
        && !path.split('/').any(|part| part == "..")
}

fn timeout_for(cli: &VerifyCli, recipe: Option<&Recipe>, purpose: Purpose) -> Duration {
    let from_recipe = recipe.and_then(|r| match purpose {
        Purpose::Test => r.test_timeout_s,
        Purpose::Configure | Purpose::Build | Purpose::Lint | Purpose::Other => r.build_timeout_s,
    });
    Duration::from_secs(cli.timeout_s.or(from_recipe).unwrap_or(DEFAULT_TIMEOUT_S))
}

fn plan_baseline(cli: &VerifyCli, recipe: &Recipe) -> Result<Vec<Planned>, String> {
    let build = recipe
        .build_run
        .clone()
        .ok_or("the environment recipe has no [build] run")?;
    let test = recipe
        .test_run
        .clone()
        .ok_or("the environment recipe has no [test] run")?;
    Ok(vec![
        Planned {
            id: "build".to_string(),
            purpose: Purpose::Build,
            cmd: build,
            cwd: ".".to_string(),
            timeout: timeout_for(cli, Some(recipe), Purpose::Build),
            junit: JunitSource::Detect,
            prior_tests: Vec::new(),
        },
        Planned {
            id: "test".to_string(),
            purpose: Purpose::Test,
            cmd: test,
            cwd: ".".to_string(),
            timeout: timeout_for(cli, Some(recipe), Purpose::Test),
            junit: match &recipe.test_junit {
                Some(globs) => JunitSource::Globs(globs.clone()),
                None => JunitSource::Detect,
            },
            prior_tests: Vec::new(),
        },
    ])
}

fn plan_verify(
    cli: &VerifyCli,
    input: &Path,
    recipe: Option<&Recipe>,
) -> Result<Vec<Planned>, String> {
    let text = std::fs::read_to_string(input)
        .map_err(|err| format!("cannot read {}: {err}", input.display()))?;
    let checks: Checks = serde_json::from_str(&text)
        .map_err(|err| format!("{} is not a checks.json v1: {err}", input.display()))?;
    if checks.version != 1 {
        return Err(format!(
            "{}: unsupported version {}",
            input.display(),
            checks.version
        ));
    }
    if checks.commands.len() > MAX_COMMANDS {
        return Err(format!(
            "{}: more than {MAX_COMMANDS} commands",
            input.display()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut plan = Vec::new();
    for command in checks.commands {
        if !is_command_id(&command.id) || !seen.insert(command.id.clone()) {
            return Err(format!(
                "{}: bad or repeated command id {:?}",
                input.display(),
                command.id
            ));
        }
        if !is_relpath(&command.cwd) || command.cmd.is_empty() {
            return Err(format!(
                "{}: command {:?} has a bad cwd or empty cmd",
                input.display(),
                command.id
            ));
        }
        let prior_tests = checks
            .tests
            .iter()
            .filter(|t| t.command_id == command.id)
            .map(|t| (t.id.clone(), t.status))
            .collect();
        plan.push(Planned {
            timeout: timeout_for(cli, recipe, command.purpose),
            junit: match command.junit {
                Some(paths) if !paths.is_empty() => JunitSource::Paths(paths),
                Some(_) | None => JunitSource::Detect,
            },
            id: command.id,
            purpose: command.purpose,
            cmd: command.cmd,
            cwd: command.cwd,
            prior_tests,
        });
    }
    Ok(plan)
}

fn report_paths(root: &Path, cwd: &Path, planned: &Planned) -> Vec<PathBuf> {
    match &planned.junit {
        JunitSource::Globs(patterns) => patterns
            .iter()
            .filter(|p| is_relpath(p))
            .flat_map(|pattern| {
                let full = root.join(pattern).to_string_lossy().into_owned();
                glob::glob(&full)
                    .map(|paths| paths.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect(),
        JunitSource::Paths(paths) => paths
            .iter()
            .filter(|p| is_relpath(p))
            .map(|p| root.join(p))
            .collect(),
        JunitSource::Detect => classify::junit_outputs(&planned.cmd, cwd),
    }
}

fn run_one(root: &Path, planned: &Planned) -> (CommandEntry, Vec<(String, TestStatus)>) {
    let cwd = root.join(&planned.cwd);
    let started_wall = SystemTime::now();
    let outcome = run::run_shell(&planned.cmd, &cwd, planned.timeout);

    let since = started_wall
        .checked_sub(MTIME_SLACK)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let root_canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut junit_rel = Vec::new();
    let mut tests = Vec::new();
    let mut read_any = false;
    for path in report_paths(root, &cwd, planned) {
        let Some(text) = read_fresh(&path, since) else {
            continue;
        };
        let Ok(cases) = junit::parse(&text) else {
            continue;
        };
        read_any = true;
        tests.extend(cases);
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        if let Ok(rel) = canonical.strip_prefix(&root_canonical)
            && junit_rel.len() < MAX_JUNIT_PER_COMMAND
        {
            let rel = rel.to_string_lossy().into_owned();
            if !junit_rel.contains(&rel) {
                junit_rel.push(rel);
            }
        }
    }
    // No JUnit from a test command: automake .trs, Meson testlog.json, automake console lines,
    // else its exit status as one test (only when that is what the input had for it too, so a
    // re-run that lost its per-test results still reports them as errors below).
    if !read_any && planned.purpose == Purpose::Test {
        let found = reports::read(root, &cwd, &planned.cmd, &outcome.output, since);
        let exit = reports::exit_status(&planned.cmd, outcome.exit_code, outcome.timed_out);
        if !found.is_empty() {
            read_any = true;
            tests = found;
            // A test the input had that this run did not report (the suite stopped early, e.g. a
            // test program no longer builds) is an error, never a silent drop.
            for (id, _) in &planned.prior_tests {
                if !tests.iter().any(|(t, _)| t == id) {
                    tests.push((id.clone(), TestStatus::Error));
                }
            }
        } else if planned.prior_tests.iter().all(|(id, _)| *id == exit.0) {
            read_any = true;
            tests = vec![exit];
        }
    }
    if !read_any && !planned.prior_tests.is_empty() {
        tests = planned
            .prior_tests
            .iter()
            .map(|(id, _)| (id.clone(), TestStatus::Error))
            .collect();
    }

    let entry = CommandEntry {
        id: planned.id.clone(),
        purpose: planned.purpose,
        cmd: planned.cmd.clone(),
        cwd: planned.cwd.clone(),
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        duration_s: duration_s(outcome.duration_ms),
        log_tail: log_tail(&outcome.output),
        junit: (!junit_rel.is_empty()).then_some(junit_rel),
    };
    (entry, tests)
}

/// Run `byteask-engine verify` and return its exit code.
pub fn run_verify(cli: VerifyCli) -> i32 {
    match verify(&cli) {
        Ok(checks) => {
            eprintln!(
                "byteask-engine verify: {} commands, build {}, {} tests, {} failed -> {}",
                checks.commands.len(),
                if checks.summary.build_ok {
                    "ok"
                } else {
                    "FAILED"
                },
                checks.summary.tests_total,
                checks.summary.tests_failed,
                cli.out.display()
            );
            EXIT_RAN
        }
        Err(message) => {
            eprintln!("byteask-engine verify: could not run: {message}");
            EXIT_COULD_NOT_RUN
        }
    }
}

fn verify(cli: &VerifyCli) -> Result<Checks, String> {
    let root = git::repo_root(&cli.repo)?;
    let base_sha = git::head_sha(&root)?;
    let recipe_path = if cli.env_spec.is_absolute() {
        cli.env_spec.clone()
    } else {
        root.join(&cli.env_spec)
    };
    let (mode, plan) = match &cli.checks {
        None => {
            let recipe = read_recipe(&recipe_path)?;
            (Mode::Baseline, plan_baseline(cli, &recipe)?)
        }
        Some(input) => {
            // The recipe only supplies timeouts here; a missing or broken one is not fatal.
            let recipe = recipe_path
                .is_file()
                .then(|| read_recipe(&recipe_path))
                .and_then(Result::ok);
            (Mode::Verify, plan_verify(cli, input, recipe.as_ref())?)
        }
    };
    let tree_sha = git::tree_sha(&root)?;

    let mut commands = Vec::new();
    let mut tests = Vec::new();
    for planned in &plan {
        let (entry, cases) = run_one(&root, planned);
        tests.extend(dedupe_tests(&entry.id, cases));
        commands.push(entry);
    }
    let checks = Checks::new(
        mode,
        RepoInfo {
            base_sha,
            tree_sha: Some(tree_sha),
        },
        commands,
        tests,
    );
    checks
        .write_to(&cli.out)
        .map_err(|err| format!("cannot write {}: {err}", cli.out.display()))?;
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned_test(cmd: &str, prior: &[(&str, TestStatus)]) -> Planned {
        Planned {
            id: "test".to_string(),
            purpose: Purpose::Test,
            cmd: cmd.to_string(),
            cwd: ".".to_string(),
            timeout: Duration::from_secs(30),
            junit: JunitSource::Detect,
            prior_tests: prior.iter().map(|(id, s)| (id.to_string(), *s)).collect(),
        }
    }

    /// automake: per-test results from the .trs files the run wrote; a baseline test the re-run
    /// did not report (the suite stopped early) is an error, not dropped.
    #[test]
    fn automake_results_and_missing_tests_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cmd = "mkdir -p tests && printf ':test-result: PASS\\n' > tests/a.trs && exit 2";
        let (entry, tests) = run_one(root, &planned_test(cmd, &[]));
        assert_eq!(entry.exit_code, Some(2));
        assert_eq!(tests, vec![("tests/a".to_string(), TestStatus::Pass)]);

        let prior = [("tests/a", TestStatus::Pass), ("tests/b", TestStatus::Pass)];
        let (_, tests) = run_one(root, &planned_test(cmd, &prior));
        assert_eq!(
            tests,
            vec![
                ("tests/a".to_string(), TestStatus::Pass),
                ("tests/b".to_string(), TestStatus::Error),
            ]
        );
    }

    /// A test command with no per-test report is one test, its exit status, in baseline and in
    /// verify (same id); a re-run that lost the per-test reports the input had is all errors.
    #[test]
    fn plain_test_command_is_its_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (_, tests) = run_one(root, &planned_test("exit 0", &[]));
        assert_eq!(tests, vec![("exit 0".to_string(), TestStatus::Pass)]);
        let (_, tests) = run_one(
            root,
            &planned_test("exit 1", &[("exit 1", TestStatus::Pass)]),
        );
        assert_eq!(tests, vec![("exit 1".to_string(), TestStatus::Fail)]);
        let (_, tests) = run_one(
            root,
            &planned_test("exit 0", &[("tests/a", TestStatus::Pass)]),
        );
        assert_eq!(tests, vec![("tests/a".to_string(), TestStatus::Error)]);
    }

    #[test]
    fn command_ids_and_relpaths() {
        assert!(is_command_id("test-2") && is_command_id("build") && is_command_id("0_a"));
        assert!(!is_command_id("Build") && !is_command_id("-x") && !is_command_id(""));
        assert!(!is_command_id(&"a".repeat(33)));
        assert!(is_relpath(".") && is_relpath("build/sub") && is_relpath("a..b"));
        assert!(!is_relpath("/abs") && !is_relpath("../up") && !is_relpath("a/../b"));
    }
}
