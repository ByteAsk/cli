use super::*;
use clap::CommandFactory;
use pretty_assertions::assert_eq;

#[test]
fn resume_parses_prompt_after_global_flags() {
    const PROMPT: &str = "echo resume-with-global-flags-after-subcommand";
    let cli = Cli::parse_from([
        "codex-exec",
        "resume",
        "--last",
        "--json",
        "--model",
        "gpt-5.2-codex",
        "--cyber-access-program",
        "daybreak_blue",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        PROMPT,
    ]);

    assert!(cli.ephemeral);
    assert!(cli.ignore_user_config);
    assert!(cli.ignore_rules);
    assert_eq!(
        cli.cyber_access_program,
        Some(CyberAccessProgramCliArg::DaybreakBlue)
    );
    let Some(Command::Resume(args)) = cli.command else {
        panic!("expected resume command");
    };
    let effective_prompt = args.prompt.clone().or_else(|| {
        if args.last {
            args.session_id.clone()
        } else {
            None
        }
    });
    assert_eq!(effective_prompt.as_deref(), Some(PROMPT));
}

#[test]
fn resume_accepts_output_flags_after_subcommand() {
    const PROMPT: &str = "echo resume-with-output-file";
    let cli = Cli::parse_from([
        "codex-exec",
        "resume",
        "session-123",
        "-o",
        "/tmp/resume-output.md",
        "--output-schema",
        "/tmp/schema.json",
        PROMPT,
    ]);

    assert_eq!(
        cli.last_message_file,
        Some(PathBuf::from("/tmp/resume-output.md"))
    );
    assert_eq!(cli.output_schema, Some(PathBuf::from("/tmp/schema.json")));
    let Some(Command::Resume(args)) = cli.command else {
        panic!("expected resume command");
    };
    assert_eq!(args.session_id.as_deref(), Some("session-123"));
    assert_eq!(args.prompt.as_deref(), Some(PROMPT));
}

#[test]
fn fork_parses_prompt_after_global_flags() {
    const PROMPT: &str = "continue on the fork";
    let cli = Cli::parse_from([
        "codex-exec",
        "fork",
        "session-123",
        "--json",
        "--model",
        "gpt-5.2-codex",
        "--thread-source",
        "automated_review",
        "--skip-git-repo-check",
        "--ephemeral",
        PROMPT,
    ]);

    assert!(cli.json);
    assert!(cli.ephemeral);
    assert_eq!(
        cli.thread_source,
        Some(ThreadSource::Feature("automated_review".to_string()))
    );
    let Some(Command::Fork(args)) = cli.command else {
        panic!("expected fork command");
    };
    assert_eq!(args.session_id, "session-123");
    assert_eq!(args.prompt.as_deref(), Some(PROMPT));
}

#[test]
fn parses_config_isolation_flags() {
    let cli = Cli::parse_from([
        "codex-exec",
        "--ignore-user-config",
        "--ignore-rules",
        "summarize",
    ]);

    assert!(cli.ignore_user_config);
    assert!(cli.ignore_rules);
}

#[test]
fn parses_max_turns_budget() {
    let cli = Cli::parse_from(["codex-exec", "--max-turns", "12", "summarize"]);

    assert_eq!(cli.max_turns, Some(12));
}

#[test]
fn max_turns_defaults_to_unlimited() {
    let cli = Cli::parse_from(["codex-exec", "summarize"]);

    assert_eq!(cli.max_turns, None);
}

#[test]
fn max_turns_is_global_and_accepted_after_a_subcommand() {
    let cli = Cli::parse_from(["codex-exec", "resume", "--last", "--max-turns", "3"]);

    assert_eq!(cli.max_turns, Some(3));
}

/// `-s/--sandbox` was always documented; its *default* never was, so a headless
/// evaluator could not know their shell commands were confined to a read-only
/// filesystem.
#[test]
fn help_states_the_sandbox_default_and_the_escape_hatch() {
    let help = Cli::command().render_help().to_string();

    assert!(
        help.contains("READ-ONLY by default"),
        "help must state the sandbox default:\n{help}"
    );
    assert!(
        help.contains("--sandbox workspace-write"),
        "help must name the flag that allows writes:\n{help}"
    );
    assert!(
        help.contains("cpp_sanitize"),
        "help must say the native tools run outside the sandbox:\n{help}"
    );
}

#[test]
fn approve_for_me_flag_applies_to_resume_when_passed_at_exec_root() {
    for flag in ["--approve-for-me", "--not-so-yolo"] {
        let cli = Cli::parse_from(["codex-exec", flag, "resume", "--last"]);

        assert!(cli.auto_review);
    }
}

#[test]
fn approve_for_me_flag_conflicts_with_other_sandbox_modes() {
    for conflicting_args in [
        vec!["--sandbox", "read-only"],
        vec!["--dangerously-bypass-approvals-and-sandbox"],
    ] {
        let mut args = vec!["codex-exec", "--approve-for-me"];
        args.extend(conflicting_args);
        args.push("summarize");

        let error = Cli::try_parse_from(args).expect_err("flags should conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}

#[test]
fn worktree_flag_is_accepted_after_fork_subcommand() {
    let cli = Cli::try_parse_from(["codex-exec", "fork", "session-id", "--worktree"])
        .expect("worktree should be a global exec argument");

    assert!(cli.worktree);
    assert!(matches!(cli.command, Some(Command::Fork(_))));
}

#[test]
fn worktree_flag_is_accepted_before_fork_subcommand() {
    let cli = Cli::try_parse_from(["codex-exec", "--worktree", "fork", "session-id"])
        .expect("worktree should be accepted before the fork subcommand");

    assert!(cli.worktree);
    assert!(matches!(cli.command, Some(Command::Fork(_))));
}
