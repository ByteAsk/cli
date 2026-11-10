//! `byteask company trial`: the whole pipeline on
//! one laptop, with no server, so an engineer can try ByteAsk before their
//! security review.
//!
//! One run, re-runnable at any time:
//!
//! 1. install the context engine from the trial bundle (sha256-checked, no download);
//! 2. switch this ByteAsk home to local-only: ByteAsk's own online tools and
//!    reference lookups answer NOT RUN (offline), the toolbox never checks the
//!    plan online, and the CLI's update check, web search and telemetry are off;
//!    the agent's model calls go only to the provider the engineer chose;
//! 3. configure the CMake project in a build tree ByteAsk keeps outside the
//!    project, and build one target;
//! 4. index that target (its build closure) with the context engine, plus an
//!    overlay of the worktree's uncommitted changes;
//! 5. run the checks (cppcheck, clang-tidy, the sanitizers) on that target's
//!    code as it is, in the tools' sandbox;
//! 6. write the local value report: this run's index and checks, then what
//!    the local results store recorded from AI sessions.
//!
//! `byteask company trial report` writes the report again from what is on
//! this machine (no indexing, no checks). The report holds counts only.

mod build;
mod checks;
pub mod install;
mod local_only;
mod state;
mod summary;

use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context as _;
use check_result::CheckResult;
use check_result::NotRunReason;
use codex_utils_cli::CliConfigOverrides;

pub use state::Step;
pub use state::TrialState;

/// The index partition the trial writes.
pub const PARTITION: &str = "trial";

#[derive(Debug, clap::Parser)]
pub struct CompanyCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub command: CompanyCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum CompanyCommand {
    /// Try ByteAsk on this laptop: index one target of a CMake project, run the
    /// checks on it and write a local value report. Nothing is sent anywhere
    /// except your agent's own model calls.
    Trial(TrialCli),
}

#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct TrialCli {
    #[command(subcommand)]
    pub action: Option<TrialAction>,
    #[command(flatten)]
    pub run: TrialArgs,
}

#[derive(Debug, clap::Subcommand)]
pub enum TrialAction {
    /// Write the local value report again from what is on this machine (no
    /// indexing, no checks).
    Report(ReportOnlyArgs),
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct ReportOnlyArgs {
    /// Print the report as JSON (counts only) instead of text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct TrialArgs {
    /// The project: a git checkout with a CMakeLists.txt at its top. Default:
    /// the current directory.
    #[arg(long, value_name = "DIR")]
    pub project: Option<PathBuf>,
    /// The CMake target to index and check. Needed when the project has more
    /// than one candidate; a test target is the best choice.
    #[arg(long, value_name = "NAME")]
    pub target: Option<String>,
    /// The executable the sanitizer check builds and runs. Default: the target
    /// itself when it is an executable, else a test that links it.
    #[arg(long, value_name = "NAME")]
    pub run_target: Option<String>,
    /// CMAKE_BUILD_TYPE of the build ByteAsk configures.
    #[arg(long, value_name = "TYPE", default_value = "RelWithDebInfo")]
    pub build_type: String,
    /// An extra CMake setting for that build, e.g. --cmake-arg=-DFEED_SIMD=ON
    /// (repeatable).
    #[arg(long = "cmake-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub cmake_args: Vec<String>,
    /// The trial bundle (default: the bundle this byteask runs from).
    #[arg(long, value_name = "DIR")]
    pub bundle: Option<PathBuf>,
    /// Print the report as JSON (counts only) instead of text.
    #[arg(long)]
    pub json: bool,
}

/// `name` on `PATH` (as given, or this process's), as an absolute path.
pub fn which(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_var = path_var
        .map(std::ffi::OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::metadata(candidate)
                    .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
}

/// The person running the trial, as the index's access list names them. There
/// is no sign-in in a laptop trial: the operating-system user is the reader.
pub fn local_user() -> String {
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "me".to_string());
    let clean: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect();
    format!("local:{}", if clean.is_empty() { "me" } else { &clean })
}

/// The top of the git checkout `dir` is in.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!top.is_empty()).then(|| std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(top)))
}

/// One step's line on the terminal while the trial runs.
fn print_step(step: &Step) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!(
            "  {:<16} {} ({:.1} s)",
            step.name,
            step.result
                .render_line()
                .trim_start_matches(check_result::LINE_PREFIX),
            step.seconds
        );
    }
}

fn step(name: &str, started: Instant, result: CheckResult) -> Step {
    let step = Step {
        name: name.to_string(),
        result,
        seconds: (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    };
    print_step(&step);
    step
}

/// A later step that cannot run because an earlier one did not finish.
fn skipped(name: &str, because: &str) -> Step {
    step(
        name,
        Instant::now(),
        CheckResult::not_run(
            name,
            NotRunReason::Skipped,
            &format!("not run: {because} did not finish"),
            &format!("fix the {because} step above, then run `byteask company trial` again"),
        ),
    )
}

pub async fn run_company(
    cli: CompanyCli,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    match cli.command {
        CompanyCommand::Trial(trial) => match trial.action {
            Some(TrialAction::Report(args)) => report_only(&args),
            None => run_trial(&cli.config_overrides, trial.run, linux_sandbox_exe).await,
        },
    }
}

fn home() -> anyhow::Result<PathBuf> {
    Ok(codex_utils_home_dir::find_codex_home()
        .context("could not find the ByteAsk home directory")?
        .to_path_buf())
}

#[allow(clippy::print_stdout)]
fn report_only(args: &ReportOnlyArgs) -> anyhow::Result<()> {
    let home = home()?;
    let state = state::load(&home);
    let now = results_store::now_unix();
    let written = summary::write(&home, state.as_ref(), now)?;
    if args.json {
        print!("{}", written.json);
    } else {
        print!("{}", written.text);
    }
    Ok(())
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn run_trial(
    overrides: &CliConfigOverrides,
    args: TrialArgs,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    let home = home()?;
    if !home.is_dir() {
        std::fs::create_dir_all(&home)
            .with_context(|| format!("could not create {}", home.display()))?;
    }
    let started_dir = match &args.project {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("could not read the current directory")?,
    };
    let Some(root) = git_toplevel(&started_dir) else {
        anyhow::bail!(
            "{} is not inside a git checkout; the index is built from a commit. Run the trial in \
             a git clone of your project (or pass --project).",
            started_dir.display()
        );
    };
    if !root.join("CMakeLists.txt").is_file() {
        anyhow::bail!(
            "{} has no CMakeLists.txt at its top; the trial supports CMake projects. Pass \
             --project with the directory that holds the top CMakeLists.txt.",
            root.display()
        );
    }
    eprintln!("ByteAsk company trial in {}", root.display());
    let mut steps: Vec<Step> = Vec::new();
    let now = results_store::now_unix();

    // 1. The context engine, from the bundle.
    let t = Instant::now();
    let exe = std::env::current_exe().ok();
    let python = install::find_python(None);
    let installed = match (
        install::find_bundle(args.bundle.as_deref(), exe.as_deref()),
        &python,
    ) {
        (_, Err(why)) => {
            steps.push(step(
                "context engine",
                t,
                CheckResult::not_run(
                    "context engine",
                    NotRunReason::ToolMissing,
                    why,
                    "install Python 3.9 or newer (python3 on PATH), then run `byteask company trial` again",
                ),
            ));
            None
        }
        (None, _) => {
            steps.push(step(
                "context engine",
                t,
                CheckResult::not_run(
                    "context engine",
                    NotRunReason::ToolMissing,
                    "no trial bundle found next to this byteask",
                    "run the byteask from the unpacked trial bundle, or pass --bundle <dir>",
                ),
            ));
            None
        }
        (Some(bundle), Ok(_)) => match install::install(&home, &bundle) {
            Ok(installed) => {
                let what = if installed.fresh {
                    "installed"
                } else {
                    "already installed"
                };
                steps.push(step(
                    "context engine",
                    t,
                    CheckResult::pass(
                        "context engine",
                        &format!(
                            "{what}: {} (every file matched the bundle's manifest)",
                            installed.version
                        ),
                    ),
                ));
                Some(installed)
            }
            Err(why) => {
                steps.push(step(
                    "context engine",
                    t,
                    CheckResult::not_run(
                        "context engine",
                        NotRunReason::ToolError,
                        &why,
                        "get the trial bundle again and run `byteask company trial` from it",
                    ),
                ));
                None
            }
        },
    };

    // 2. Local only.
    let t = Instant::now();
    steps.push(step("local only", t, local_only::apply(&home)));

    // 3. Configure and build one target.
    let t = Instant::now();
    let built = build::configure_and_build(&root, &args).await;
    let built = match built {
        Ok(built) => {
            steps.push(step(
                "build",
                t,
                CheckResult::pass(
                    "build",
                    &format!(
                        "configured with CMake ({} build) outside the project and built the target",
                        args.build_type
                    ),
                ),
            ));
            Some(built)
        }
        Err(result) => {
            steps.push(step("build", t, result));
            None
        }
    };

    // 4. Index the target.
    let indexed = match (&installed, &built, &python) {
        (Some(installed), Some(built), Ok(python)) => {
            let t = Instant::now();
            let outcome = state::index(&home, installed, python, &root, built).await;
            let result = outcome.result.clone();
            steps.push(step("index", t, result));
            outcome.facts.map(|facts| (facts, outcome.compdb))
        }
        (None, _, _) => {
            steps.push(skipped("index", "context engine"));
            None
        }
        _ => {
            steps.push(skipped("index", "build"));
            None
        }
    };

    // The context tools now find this project (CLI, Claude Code, Codex).
    if let (Some(installed), Some(built), Some(_), Ok(python)) =
        (&installed, &built, &indexed, &python)
        && let Err(err) = state::record_engine(&home, installed, python, &root, built)
    {
        eprintln!("  could not record the index for the context tools: {err}");
    }

    // 5. The checks, on the target's code as it is.
    let tallies = match (&built, &indexed) {
        (Some(built), Some((_, compdb))) => {
            checks::run(
                overrides,
                &root,
                built,
                compdb,
                linux_sandbox_exe.clone(),
                &mut steps,
            )
            .await
        }
        (Some(built), None) => {
            let compdb = built.build_dir.join("compile_commands.json");
            checks::run(
                overrides,
                &root,
                built,
                &compdb,
                linux_sandbox_exe.clone(),
                &mut steps,
            )
            .await
        }
        (None, _) => {
            for name in checks::CHECKS {
                steps.push(skipped(name, "build"));
            }
            Vec::new()
        }
    };

    // 6. The report.
    let trial = TrialState {
        schema: state::SCHEMA,
        project_root: root.clone(),
        target: built.as_ref().map(|b| b.target.clone()).unwrap_or_default(),
        build_type: args.build_type.clone(),
        steps,
        index: indexed.map(|(facts, _)| facts),
        checks: tallies,
        updated_at: now,
    };
    state::save(&home, &trial)?;
    let written = summary::write(&home, Some(&trial), results_store::now_unix())?;
    if args.json {
        print!("{}", written.json);
    } else {
        print!("{}", written.text);
        eprintln!(
            "\nThe report is in {} (and report.json beside it).",
            written.text_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "trial_tests.rs"]
mod tests;
