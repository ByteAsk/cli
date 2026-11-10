//! Building the selected targets and running the selected tests.
//!
//! Every command's output goes to one log file per run (kept on disk for
//! `rg`); the model gets failures and a summary. Commands run in their own
//! process group with a wall-clock cap and can be cancelled. A build that
//! fails is retried target by target, so a test whose program did not build is
//! reported as NOT BUILT instead of running an old binary. Failing tests are
//! re-run once to tell flaky from deterministic failures.

use std::collections::BTreeMap;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::watch;

use super::Model;
use super::System;
use super::parse;
use super::parse::Parsed;
use super::symbols::Framework;
use crate::base_run_cache::TestStatus;

/// Longest a single build command may run.
pub(crate) const BUILD_TIMEOUT: Duration = Duration::from_secs(3600);
/// Most of a log read back for parsing.
const READ_BACK_CAP: u64 = 16 * 1024 * 1024;
/// Most names in one `ctest -R` alternation.
const CTEST_CHUNK: usize = 200;

/// Shared state of one run.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub log: PathBuf,
    pub jobs: usize,
    pub test_timeout: Duration,
    pub cancel: watch::Receiver<bool>,
    pub progress: Arc<Mutex<String>>,
}

impl Ctx {
    pub(crate) fn progress(&self, text: impl Into<String>) {
        *self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text.into();
    }

    fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
}

/// How one command ended.
#[derive(Debug)]
pub(crate) struct CmdResult {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output: String,
}

impl CmdResult {
    pub(crate) fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }
}

/// Run a command with its output appended to the run log; return its exit
/// and its own part of the log.
pub(crate) async fn run_logged(
    ctx: &Ctx,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> CmdResult {
    let fail = |msg: String| CmdResult {
        code: None,
        timed_out: false,
        cancelled: false,
        output: msg,
    };
    if ctx.cancelled() {
        return CmdResult {
            cancelled: true,
            ..fail("cancelled".to_string())
        };
    }
    if let Some(parent) = ctx.log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ctx.log)
    {
        Ok(f) => f,
        Err(e) => return fail(format!("cannot open log {}: {e}", ctx.log.display())),
    };
    let quoted: Vec<String> = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .map(|a| shlex::try_quote(&a).map(|q| q.to_string()).unwrap_or(a))
        .collect();
    let _ = writeln!(file, "\n$ cd {} && {}", cwd.display(), quoted.join(" "));
    let start = file.seek(std::io::SeekFrom::End(0)).unwrap_or(0);
    let (Ok(out), Ok(err)) = (file.try_clone(), file.try_clone()) else {
        return fail("cannot duplicate the log handle".to_string());
    };
    // One engine-started build or test run at a time per session (the finish gate, a
    // sub-agent check and this run never compile at once); the wait counts against `timeout`.
    let waited_from = Instant::now();
    let mut cancel_wait = ctx.cancel.clone();
    let slot = tokio::select! {
        slot = crate::resources::build_slot(timeout) => slot,
        _ = cancel_wait.wait_for(|c| *c) => {
            return CmdResult { cancelled: true, ..fail("cancelled".to_string()) };
        }
    };
    let Some(_slot) = slot else {
        let _ = writeln!(
            file,
            "timed out waiting for another ByteAsk build or test run to finish"
        );
        return CmdResult {
            timed_out: true,
            ..fail("timed out waiting for another ByteAsk build or test run to finish".to_string())
        };
    };
    let timeout = timeout.saturating_sub(waited_from.elapsed());
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for (k, v) in env {
        command.env(k, v);
    }
    #[cfg(unix)]
    command.process_group(0);
    // Inside an automatic check, low priority on the spare CPUs.
    #[cfg(unix)]
    crate::priority::apply(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return fail(format!("could not run {program}: {e}")),
    };
    let pid = child.id();
    let _ours = pid.map(super::tree::register_group);
    let mut cancel = ctx.cancel.clone();
    let (code, timed_out, cancelled) = tokio::select! {
        status = child.wait() => (status.ok().and_then(|s| s.code()), false, false),
        () = tokio::time::sleep(timeout) => (None, true, false),
        _ = cancel.wait_for(|c| *c) => (None, false, true),
    };
    crate::run::kill_process_group(pid);
    if timed_out || cancelled {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    drop(file);
    CmdResult {
        code,
        timed_out,
        cancelled,
        output: read_from(&ctx.log, start),
    }
}

fn read_from(path: &Path, start: u64) -> String {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let from = start.max(len.saturating_sub(READ_BACK_CAP));
    if file.seek(std::io::SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parallel jobs for builds and test runs: memory-aware (see [`crate::resources::jobs`]).
pub(crate) fn default_jobs() -> usize {
    crate::resources::jobs()
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct BuildResult {
    pub ok: bool,
    pub secs: f64,
    /// Targets that failed to build.
    pub failed: Vec<String>,
    pub errors: Vec<String>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// `tree_owner`: the build first waited for another process building in its tree.
    pub waited: Option<String>,
}

fn build_command(
    model: &Model,
    dir: &Path,
    targets: &[String],
    jobs: usize,
) -> (String, Vec<String>) {
    let jobs_s = jobs.to_string();
    match model.system {
        System::CMake => {
            let mut args = vec!["--build".to_string(), dir.display().to_string()];
            if !targets.is_empty() {
                args.push("--target".to_string());
                args.extend(targets.iter().cloned());
            }
            if crate::toolver::at_least(model.version, (3, 12)) {
                args.extend(["--parallel".to_string(), jobs_s]);
            } else {
                args.extend(["--".to_string(), format!("-j{jobs}")]);
            }
            ("cmake".to_string(), args)
        }
        System::Meson => {
            let mut args = vec![
                "-C".to_string(),
                dir.display().to_string(),
                "-j".to_string(),
                jobs_s,
            ];
            args.extend(targets.iter().cloned());
            ("ninja".to_string(), args)
        }
        System::Make => {
            let mut args = vec![
                "-C".to_string(),
                dir.display().to_string(),
                format!("-j{jobs}"),
            ];
            args.extend(targets.iter().cloned());
            ("make".to_string(), args)
        }
    }
}

/// The names a build tool builds `targets` (model target names) by.
pub(crate) fn tool_targets(model: &Model, targets: &[String]) -> Vec<String> {
    match model.system {
        System::CMake | System::Make => targets.to_vec(),
        // Ninja builds outputs by path relative to the build directory (a base
        // tree mirrors the layout of the build tree).
        System::Meson => targets
            .iter()
            .filter_map(|t| model.target(t))
            .flat_map(|t| t.artifacts.iter())
            .map(|a| {
                a.strip_prefix(&model.build_dir)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| a.clone())
                    .display()
                    .to_string()
            })
            .collect(),
    }
}

/// Build `targets` (all when empty) in `dir`. When the build fails, each
/// target is built alone to find the ones that failed (the others are quick
/// no-ops by then), so no test runs a program left over from an older build.
pub(crate) async fn build(ctx: &Ctx, model: &Model, dir: &Path, targets: &[String]) -> BuildResult {
    let started = Instant::now();
    let names = tool_targets(model, targets);
    let together = names.len() <= 1
        || model.system != System::CMake
        || crate::toolver::at_least(model.version, (3, 15));
    let mut result = BuildResult {
        ok: true,
        ..BuildResult::default()
    };
    result.waited = wait_for_foreign(ctx, dir).await;
    let mut outputs = String::new();
    let label = |group: &[String]| {
        if group.is_empty() {
            "everything".to_string()
        } else {
            group.join(" ")
        }
    };
    if together {
        ctx.progress(format!("building {}", label(&names)));
        let (program, args) = build_command(model, dir, &names, ctx.jobs);
        let res = run_logged(ctx, &program, &args, dir, &[], BUILD_TIMEOUT).await;
        outputs.push_str(&res.output);
        result.cancelled = res.cancelled;
        result.timed_out = res.timed_out;
        if !res.ok() {
            result.ok = false;
            if names.len() > 1 && !res.cancelled && !res.timed_out {
                for (name, tool_name) in targets.iter().zip(names.iter()) {
                    ctx.progress(format!("building {tool_name} alone to locate the failure"));
                    let (program, args) =
                        build_command(model, dir, std::slice::from_ref(tool_name), ctx.jobs);
                    let one = run_logged(ctx, &program, &args, dir, &[], BUILD_TIMEOUT).await;
                    if one.cancelled {
                        result.cancelled = true;
                        break;
                    }
                    if !one.ok() {
                        result.failed.push(name.clone());
                    }
                }
            } else {
                result.failed.extend(targets.iter().cloned());
            }
        }
    } else {
        for (name, tool_name) in targets.iter().zip(names.iter()) {
            ctx.progress(format!("building {tool_name}"));
            let (program, args) =
                build_command(model, dir, std::slice::from_ref(tool_name), ctx.jobs);
            let res = run_logged(ctx, &program, &args, dir, &[], BUILD_TIMEOUT).await;
            outputs.push_str(&res.output);
            result.timed_out |= res.timed_out;
            if res.cancelled {
                result.cancelled = true;
                result.ok = false;
                break;
            }
            if !res.ok() {
                result.ok = false;
                result.failed.push(name.clone());
            }
        }
    }
    result.errors = parse::build_errors(&outputs, &model.root.display().to_string(), 3);
    if !result.ok && result.errors.is_empty() {
        let lines: Vec<&str> = outputs.lines().collect();
        result.errors = parse::explain(&lines);
    }
    result.secs = started.elapsed().as_secs_f64();
    result
}

/// `tree_owner`: wait (until cancelled) while another process builds in `dir`; the note says so.
async fn wait_for_foreign(ctx: &Ctx, dir: &Path) -> Option<String> {
    if !super::tree::owner_mode(dir) {
        return None;
    }
    super::tree::foreign_process(dir)?;
    let started = Instant::now();
    let mut cancel = ctx.cancel.clone();
    // Counted from the second sighting on (see `tree::foreign_build`).
    let mut seen: Option<(u32, String)> = None;
    loop {
        tokio::select! {
            () = tokio::time::sleep(super::tree::FOREIGN_POLL) => {}
            _ = cancel.wait_for(|c| *c) => break,
        }
        let Some(found) = super::tree::foreign_process(dir) else {
            break;
        };
        if seen.is_none() {
            ctx.progress(format!(
                "waiting for a build already running in {}",
                dir.display()
            ));
            seen = Some(found);
        }
    }
    let (pid, cmd) = seen?;
    Some(format!(
        "waited {:.0}s for a build already running in its tree (pid {pid}: {}), so the two never build at once",
        started.elapsed().as_secs_f64(),
        super::tree::short_command(&cmd)
    ))
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// One thing to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunItem {
    /// `ctest -R '^(names)$'` in `dir` (all tests when `names` is empty).
    Ctest { dir: PathBuf, names: Vec<String> },
    /// A test program run directly, optionally with a case filter.
    Direct {
        test: String,
        argv: Vec<String>,
        workdir: PathBuf,
        env: Vec<(String, String)>,
        framework: Option<Framework>,
        cases: Vec<String>,
    },
    /// `make <target>` (the project's own suite runner).
    MakeCheck { dir: PathBuf, target: String },
}

/// Everything a run produced.
#[derive(Clone, Debug, Default)]
pub(crate) struct RunResult {
    /// `test` or `test::case` → status.
    pub results: BTreeMap<String, TestStatus>,
    pub details: BTreeMap<String, Vec<String>>,
    pub secs: f64,
    pub notes: Vec<String>,
    pub cancelled: bool,
}

/// Run `items`, re-running failures once.
pub(crate) async fn run(ctx: &Ctx, items: &[RunItem]) -> RunResult {
    let started = Instant::now();
    let mut out = RunResult::default();
    for item in items {
        if ctx.cancelled() {
            out.cancelled = true;
            break;
        }
        let first = run_item(ctx, item).await;
        merge(&mut out, first.clone());
        // Re-run what failed, once.
        let failed: Vec<String> = first
            .results
            .iter()
            .filter(|(_, s)| matches!(s, TestStatus::Fail | TestStatus::Timeout))
            .map(|(n, _)| n.clone())
            .collect();
        if failed.is_empty() || matches!(item, RunItem::MakeCheck { .. }) {
            continue;
        }
        if let Some(retry) = narrowed(item, &failed) {
            ctx.progress(format!("re-running {} failure(s) once", failed.len()));
            let second = run_item(ctx, &retry).await;
            for name in &failed {
                if second.results.get(name).is_some_and(|s| s.passing()) {
                    out.results.insert(name.clone(), TestStatus::Flaky);
                }
            }
        }
    }
    out.secs = started.elapsed().as_secs_f64();
    out
}

fn merge(into: &mut RunResult, from: RunResult) {
    into.results.extend(from.results);
    into.details.extend(from.details);
    into.notes.extend(from.notes);
    into.cancelled |= from.cancelled;
}

/// `item` restricted to the `failed` result names.
fn narrowed(item: &RunItem, failed: &[String]) -> Option<RunItem> {
    match item {
        RunItem::Ctest { dir, .. } => Some(RunItem::Ctest {
            dir: dir.clone(),
            names: failed.to_vec(),
        }),
        RunItem::Direct {
            test,
            argv,
            workdir,
            env,
            framework,
            cases,
        } => {
            let prefix = format!("{test}::");
            let failed_cases: Vec<String> = failed
                .iter()
                .filter_map(|f| f.strip_prefix(&prefix).map(str::to_string))
                .collect();
            Some(RunItem::Direct {
                test: test.clone(),
                argv: argv.clone(),
                workdir: workdir.clone(),
                env: env.clone(),
                framework: *framework,
                cases: if cases.is_empty() || failed_cases.is_empty() {
                    cases.clone()
                } else {
                    failed_cases
                },
            })
        }
        RunItem::MakeCheck { .. } => None,
    }
}

async fn run_item(ctx: &Ctx, item: &RunItem) -> RunResult {
    match item {
        RunItem::Ctest { dir, names } => run_ctest(ctx, dir, names).await,
        RunItem::Direct {
            test,
            argv,
            workdir,
            env,
            framework,
            cases,
        } => run_direct(ctx, test, argv, workdir, env, *framework, cases).await,
        RunItem::MakeCheck { dir, target } => run_make_check(ctx, dir, target).await,
    }
}

async fn run_ctest(ctx: &Ctx, dir: &Path, names: &[String]) -> RunResult {
    let mut out = RunResult::default();
    let chunks: Vec<&[String]> = if names.is_empty() {
        vec![&[][..]]
    } else {
        names.chunks(CTEST_CHUNK).collect()
    };
    for chunk in chunks {
        let mut args = vec![
            "--output-on-failure".to_string(),
            "--timeout".to_string(),
            ctx.test_timeout.as_secs().max(1).to_string(),
            "-j".to_string(),
            ctx.jobs.to_string(),
        ];
        if !chunk.is_empty() {
            args.push("-R".to_string());
            args.push(format!(
                "^({})$",
                chunk
                    .iter()
                    .map(|n| regex_escape(n))
                    .collect::<Vec<_>>()
                    .join("|")
            ));
        }
        let count = if chunk.is_empty() { 1000 } else { chunk.len() };
        ctx.progress(format!(
            "running {} test(s) with ctest",
            if chunk.is_empty() {
                "all".to_string()
            } else {
                count.to_string()
            }
        ));
        let rounds = u32::try_from(count.div_ceil(ctx.jobs.max(1))).unwrap_or(u32::MAX);
        let timeout = (ctx.test_timeout * rounds.max(1) + Duration::from_secs(60))
            .min(Duration::from_secs(6 * 3600));
        let res = run_logged(ctx, "ctest", &args, dir, &[], timeout).await;
        let parsed = parse::ctest(&res.output);
        absorb(&mut out, parsed, None);
        for name in chunk {
            out.results
                .entry(name.clone())
                .or_insert(TestStatus::NotRun);
        }
        if res.cancelled {
            out.cancelled = true;
            break;
        }
        if res.timed_out {
            out.notes.push("ctest hit the overall time cap".to_string());
        }
    }
    out
}

fn absorb(out: &mut RunResult, parsed: Parsed, prefix: Option<&str>) {
    let key = |n: &str| match prefix {
        Some(p) => format!("{p}::{n}"),
        None => n.to_string(),
    };
    for (name, status) in parsed.results {
        out.results.insert(key(&name), status);
    }
    for (name, details) in parsed.details {
        out.details.insert(key(&name), details);
    }
}

/// Escape a test name for a CTest (CMake) regular expression.
pub(crate) fn regex_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The argv for running `argv` with only `cases` selected.
pub(crate) fn filtered_argv(
    argv: &[String],
    framework: Option<Framework>,
    cases: &[String],
) -> Vec<String> {
    let mut out = argv.to_vec();
    match framework {
        Some(Framework::GTest) => {
            if !cases.is_empty() {
                out.push(format!("--gtest_filter={}", cases.join(":")));
            }
        }
        Some(Framework::Catch) => {
            if !cases.is_empty() {
                out.push(
                    cases
                        .iter()
                        .map(|c| catch_escape(c))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            out.extend(["-r".to_string(), "xml".to_string()]);
        }
        Some(Framework::Doctest) => {
            if !cases.is_empty() {
                out.push(format!(
                    "--test-case={}",
                    cases
                        .iter()
                        .map(|c| c.replace(',', "\\,"))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
        }
        Some(Framework::Boost) | None => {}
    }
    out
}

fn catch_escape(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if matches!(c, ',' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn run_direct(
    ctx: &Ctx,
    test: &str,
    argv: &[String],
    workdir: &Path,
    env: &[(String, String)],
    framework: Option<Framework>,
    cases: &[String],
) -> RunResult {
    let mut out = RunResult::default();
    let Some(program) = argv.first() else {
        out.results.insert(test.to_string(), TestStatus::NotRun);
        return out;
    };
    if !Path::new(program).exists() && program.contains('/') {
        out.results.insert(test.to_string(), TestStatus::NotBuilt);
        out.details
            .insert(test.to_string(), vec![format!("{program} does not exist")]);
        return out;
    }
    ctx.progress(format!(
        "running {test}{}",
        if cases.is_empty() {
            String::new()
        } else {
            format!(" ({} case(s))", cases.len())
        }
    ));
    let full = filtered_argv(argv, framework, cases);
    let n = u32::try_from(cases.len().max(1).div_ceil(8)).unwrap_or(1);
    let timeout = (ctx.test_timeout * n).min(Duration::from_secs(1800));
    let res = run_logged(ctx, program, &full[1..], workdir, env, timeout).await;
    if res.cancelled {
        out.cancelled = true;
        return out;
    }
    let parsed = match framework {
        Some(Framework::GTest) => parse::gtest(&res.output),
        Some(Framework::Catch) => parse::catch_xml(&res.output),
        Some(Framework::Doctest) => parse::doctest(&res.output, cases),
        _ if res.output.contains("[==========]") => parse::gtest(&res.output),
        _ => Parsed::default(),
    };
    if !cases.is_empty() && framework.is_some() {
        if parsed.results.is_empty() {
            // The filter matched nothing, or the program died first.
            let status = if res.ok() {
                TestStatus::NotRun
            } else if res.timed_out {
                TestStatus::Timeout
            } else {
                TestStatus::Fail
            };
            out.results.insert(test.to_string(), status);
            let lines: Vec<&str> = res.output.lines().collect();
            if status != TestStatus::NotRun {
                out.details.insert(test.to_string(), parse::explain(&lines));
            } else {
                out.notes
                    .push(format!("{test}: the case filter matched no test case"));
            }
        } else {
            absorb(&mut out, parsed, Some(test));
            if res.timed_out {
                out.results
                    .insert(format!("{test}::(timeout)"), TestStatus::Timeout);
            }
        }
        return out;
    }
    // A whole program: its exit status decides; name failing cases if it
    // printed them in a known format.
    let status = if res.timed_out {
        TestStatus::Timeout
    } else if res.code == Some(0) {
        TestStatus::Pass
    } else if res.code == Some(77) {
        TestStatus::Skip
    } else {
        TestStatus::Fail
    };
    out.results.insert(test.to_string(), status);
    if status == TestStatus::Fail || status == TestStatus::Timeout {
        let failing: Vec<&String> = parsed
            .results
            .iter()
            .filter(|(_, s)| s.failing())
            .map(|(n, _)| n)
            .collect();
        let mut details = Vec::new();
        if !failing.is_empty() {
            details.push(format!(
                "failing cases: {}",
                failing
                    .iter()
                    .take(5)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if let Some(first) = failing.first().and_then(|f| parsed.details.get(*f)) {
                details.extend(first.iter().take(parse::DETAIL_LINES - 1).cloned());
            }
        } else {
            let lines: Vec<&str> = res.output.lines().collect();
            details = parse::explain(&lines);
            if res.code.is_none() && !res.timed_out {
                details.insert(0, "(killed by a signal)".to_string());
            }
        }
        out.details.insert(test.to_string(), details);
    }
    out
}

async fn run_make_check(ctx: &Ctx, dir: &Path, target: &str) -> RunResult {
    let mut out = RunResult::default();
    ctx.progress(format!("running `make {target}`"));
    let args = vec![
        "-C".to_string(),
        dir.display().to_string(),
        format!("-j{}", ctx.jobs),
        target.to_string(),
    ];
    let res = run_logged(ctx, "make", &args, dir, &[], Duration::from_secs(4 * 3600)).await;
    if res.cancelled {
        out.cancelled = true;
        return out;
    }
    let mut parsed = parse::automake(&res.output);
    if parsed.results.is_empty() && res.output.contains("[==========]") {
        parsed = parse::gtest(&res.output);
    }
    let name = format!("make {target}");
    if parsed.results.is_empty() {
        out.results.insert(
            name.clone(),
            if res.ok() {
                TestStatus::Pass
            } else {
                TestStatus::Fail
            },
        );
    } else {
        absorb(&mut out, parsed, None);
        if !res.ok() && out.results.values().all(|s| !s.failing()) {
            out.results.insert(name.clone(), TestStatus::Fail);
        }
    }
    if !res.ok() {
        let lines: Vec<&str> = res.output.lines().collect();
        let errors = parse::build_errors(&res.output, &dir.display().to_string(), 2);
        out.details.entry(name).or_insert(if errors.is_empty() {
            parse::explain(&lines)
        } else {
            errors
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn ctx(dir: &Path) -> (Ctx, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        (
            Ctx {
                log: dir.join("run.log"),
                jobs: 2,
                test_timeout: Duration::from_secs(20),
                cancel: rx,
                progress: Arc::new(Mutex::new(String::new())),
            },
            tx,
        )
    }

    #[test]
    fn escapes_and_filters() {
        assert_eq!(regex_escape("a.b(c)+"), "a\\.b\\(c\\)\\+");
        let argv = vec!["/b/t".to_string()];
        assert_eq!(
            filtered_argv(
                &argv,
                Some(Framework::GTest),
                &["A.B".to_string(), "*P.C/*".to_string()]
            ),
            vec!["/b/t".to_string(), "--gtest_filter=A.B:*P.C/*".to_string()]
        );
        assert_eq!(
            filtered_argv(
                &argv,
                Some(Framework::Catch),
                &["x, y".to_string(), "[z]".to_string()]
            ),
            vec![
                "/b/t".to_string(),
                "x\\, y,\\[z\\]".to_string(),
                "-r".to_string(),
                "xml".to_string()
            ]
        );
        assert_eq!(
            filtered_argv(&argv, Some(Framework::Doctest), &["a,b".to_string()]),
            vec!["/b/t".to_string(), "--test-case=a\\,b".to_string()]
        );
        assert!(default_jobs() >= 1);
    }

    #[tokio::test]
    async fn logs_output_times_out_and_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, cancel) = ctx(dir.path());
        let res = run_logged(
            &ctx,
            "sh",
            &["-c".to_string(), "echo hello; exit 3".to_string()],
            dir.path(),
            &[],
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(res.code, Some(3));
        assert_eq!(res.output.trim(), "hello");
        let log = std::fs::read_to_string(dir.path().join("run.log")).unwrap();
        assert!(log.contains("$ cd ") && log.contains("hello"), "{log}");

        let started = Instant::now();
        let res = run_logged(
            &ctx,
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            dir.path(),
            &[],
            Duration::from_millis(300),
        )
        .await;
        assert!(res.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));

        let ctx2 = ctx.clone();
        let task = tokio::spawn(async move {
            run_logged(
                &ctx2,
                "sh",
                &["-c".to_string(), "sleep 30".to_string()],
                Path::new("/"),
                &[],
                Duration::from_secs(60),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.send(true).unwrap();
        let res = task.await.unwrap();
        assert!(res.cancelled);
    }

    #[tokio::test]
    async fn whole_programs_are_judged_by_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _cancel) = ctx(dir.path());
        let script = dir.path().join("t.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'check failed: expected 1 got 2'\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let item = RunItem::Direct {
            test: "t".to_string(),
            argv: vec![script.display().to_string()],
            workdir: dir.path().to_path_buf(),
            env: Vec::new(),
            framework: None,
            cases: Vec::new(),
        };
        let res = run(&ctx, &[item]).await;
        assert_eq!(res.results.get("t"), Some(&TestStatus::Fail));
        assert_eq!(
            res.details["t"],
            vec!["check failed: expected 1 got 2".to_string()]
        );
    }
}
