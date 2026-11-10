//! `byteask checks ...`: the merge check's CI commands and a person's skip.
//!
//! `skip` is a command, never a tool: no agent can call it over MCP, and it refuses to run from
//! an agent's own shell or without a terminal. It records NOT RUN (skipped: reason, who) for the
//! work tree exactly as it is now; it never produces PASS, and the merge check never reads it.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::Context as _;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, clap::Parser)]
pub struct ChecksCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub command: ChecksCommand,
}

// Parsed once per process; boxing the larger argument sets buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, clap::Subcommand)]
pub enum ChecksCommand {
    #[command(flatten)]
    Merge(merge_check::cli::MergeCommand),
    /// Skip a check for your work exactly as it is now, with a reason. Type this yourself in
    /// a terminal: an AI agent cannot skip a check. The check reports NOT RUN (skipped), never
    /// PASS, and runs again after any edit.
    Skip(SkipArgs),
    /// Stop the automatic check running for this project now (within 2 s). It reports NOT
    /// RUN (cancelled), never PASS.
    Cancel(CancelArgs),
}

#[derive(Debug, clap::Args)]
pub struct CancelArgs {
    /// The project directory (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct SkipArgs {
    /// Why you are skipping it (shown with the result).
    #[arg(long)]
    pub reason: String,
    /// The check to skip.
    #[arg(long, default_value = "cpp_finish_gate")]
    pub check: String,
    /// The project directory (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

/// Who is skipping: the login name.
fn person() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "unknown user".to_string())
}

/// Run `byteask checks ...`; the process exit status.
#[allow(clippy::print_stdout)]
pub async fn run_checks(cli: ChecksCli) -> anyhow::Result<i32> {
    match cli.command {
        ChecksCommand::Merge(command) => merge_check::cli::run_command(command).await,
        ChecksCommand::Cancel(args) => {
            let home = codex_utils_home_dir::find_codex_home()
                .context("could not find the ByteAsk home directory")?
                .to_path_buf();
            let cwd = match args.cwd {
                Some(dir) => dir,
                None => std::env::current_dir().context("could not read the current directory")?,
            };
            let root = codex_cpp_tools::cancel::request(&home, &cwd)
                .await
                .map_err(anyhow::Error::msg)?;
            println!(
                "Asked the check running for {} to stop. It reports NOT RUN (cancelled), never PASS.",
                root.display()
            );
            Ok(0)
        }
        ChecksCommand::Skip(args) => {
            let home = codex_utils_home_dir::find_codex_home()
                .context("could not find the ByteAsk home directory")?
                .to_path_buf();
            let result = skip(
                args,
                &home,
                std::io::stdin().is_terminal(),
                |key| std::env::var_os(key),
                &merge_check::human::ancestor_names(),
            )
            .await?;
            println!("{}", result.render_line());
            println!(
                "This is NOT RUN, never PASS. It applies to your work exactly as it is now; the \
                 next edit ends it. At merge the required checks run on CI regardless."
            );
            Ok(0)
        }
    }
}

/// Refuse first (agent session, agent ancestor, no terminal), then record.
async fn skip(
    args: SkipArgs,
    home: &std::path::Path,
    stdin_is_terminal: bool,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ancestors: &[String],
) -> anyhow::Result<check_result::CheckResult> {
    if let Some(why) = merge_check::human::skip_refusal(stdin_is_terminal, env, ancestors) {
        anyhow::bail!("not skipped: {why}");
    }
    let cwd = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().context("could not read the current directory")?,
    };
    codex_cpp_tools::skip::record(home, &cwd, &args.check, &args.reason, &person())
        .await
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn skip_needs_a_reason_and_the_ci_commands_are_there() {
        assert!(ChecksCli::try_parse_from(["checks", "skip"]).is_err(), "no reason");
        let cli = ChecksCli::try_parse_from(["checks", "skip", "--reason", "rig down"]).expect("parses");
        let ChecksCommand::Skip(args) = cli.command else {
            panic!("skip");
        };
        assert_eq!(args.check, "cpp_finish_gate");
        for sub in ["run", "sign", "gate", "candidate", "keygen", "profile", "cancel"] {
            let err = ChecksCli::try_parse_from(["checks", sub, "--help"]).expect_err("help exits");
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "{sub}");
        }
    }

    fn args(dir: &std::path::Path) -> SkipArgs {
        SkipArgs {
            reason: "the agent wants to finish".to_string(),
            check: "cpp_finish_gate".to_string(),
            cwd: Some(dir.to_path_buf()),
        }
    }

    /// An agent's skip is refused before anything is recorded, whether the agent
    /// marks its shell, runs it under a PTY (Codex) or without a terminal.
    #[tokio::test]
    async fn an_agent_skip_is_refused_and_records_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let home = tempfile::tempdir().expect("home");
        let marked = |key: &str| (key == "CLAUDECODE").then(|| std::ffi::OsString::from("1"));
        for (tty, env_marked, ancestors) in [
            (true, true, vec!["bash".to_string()]),
            (true, false, vec!["bash".to_string(), "codex".to_string()]),
            (false, false, vec!["bash".to_string()]),
        ] {
            let env = move |key: &str| if env_marked { marked(key) } else { None };
            let err = skip(args(dir.path()), home.path(), tty, env, &ancestors)
                .await
                .expect_err("refused");
            assert!(err.to_string().starts_with("not skipped:"), "{err}");
        }
        assert!(std::fs::read_dir(home.path()).expect("dir").next().is_none(), "nothing recorded");
    }

    /// A person at a terminal can skip; the record is NOT RUN (skipped), never PASS.
    #[tokio::test]
    async fn a_person_skip_is_recorded_as_not_run() {
        let dir = tempfile::tempdir().expect("tmp");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.cpp"), "int a;\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "b"]);
        let home = tempfile::tempdir().expect("home");
        let result = skip(args(dir.path()), home.path(), true, |_| None, &["bash".to_string()])
            .await
            .expect("recorded");
        assert!(!result.is_pass());
        assert_eq!(result.reason, Some(check_result::NotRunReason::Skipped));
    }
}
