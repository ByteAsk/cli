//! `byteask toolbox ...` and `byteask policy ...`.
//!
//! Only `toolbox serve` speaks MCP; it never prints to stdout. The other
//! commands are for people and print plain text.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::ArgGroup;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_utils_cli::CliConfigOverrides;
use rmcp::ServiceExt as _;

use crate::entitlement;
use crate::install;
use crate::registry::ToolboxThread;
use crate::server;
use crate::server::ToolboxServer;

#[derive(Debug, clap::Parser)]
pub struct ToolboxCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub command: ToolboxCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ToolboxCommand {
    /// Serve ByteAsk's C/C++ and reference tools to another agent over MCP (stdio).
    Serve(WorkspaceArgs),
    /// Add ByteAsk's tools to Claude Code or Codex for this user.
    Install(InstallArgs),
    /// List the tools the toolbox serves in a directory.
    List(ListArgs),
    /// Run one agent hook (Claude Code / Codex call this; `toolbox install` writes the entries).
    /// Reads the hook's JSON on stdin and answers in the hook protocol on stdout.
    Hook(HookArgs),
    /// Show what ByteAsk's checks found on this machine: counts from the local
    /// results store only. Nothing is sent anywhere.
    Report(crate::report::ReportArgs),
}

#[derive(Debug, clap::Args)]
pub struct HookArgs {
    /// Which hook: session-start, stop or prompt.
    #[arg(value_enum)]
    pub event: HookEvent,
    /// The agent: claude-code or codex (recorded with each result).
    #[arg(long, default_value = "claude-code")]
    pub client: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    Stop,
    Prompt,
    /// Started by `stop` itself (never by an agent): finish, in the background, a check that
    /// did not fit the fast tier.
    FollowUp,
}

#[derive(Debug, clap::Args)]
pub struct WorkspaceArgs {
    /// Project directory (default: $CLAUDE_PROJECT_DIR, else the current directory).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
#[command(group(ArgGroup::new("agent").required(true).multiple(true).args(["claude_code", "codex"])))]
pub struct InstallArgs {
    /// Add the tools to Claude Code (user scope).
    #[arg(long)]
    pub claude_code: bool,
    /// Add the tools to OpenAI's Codex CLI (its user config.toml).
    #[arg(long)]
    pub codex: bool,
    /// Show what would change and change nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Print the MCP tool list as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Parser)]
pub struct PolicyCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub command: PolicyCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum PolicyCommand {
    /// Show the policy file and what it allows.
    Show,
    /// Check the signed-in plan with ByteAsk now and store it for offline use.
    RefreshPlan,
    /// Move the policy file aside so every setting returns to its safe default.
    Reset,
    /// Run a project's ByteAsk tools WITHOUT the tool sandbox. Type this
    /// yourself in a terminal: it refuses to run from an AI agent's session.
    Relax {
        /// The project directory.
        path: PathBuf,
    },
    /// Turn the tool sandbox back on for a project.
    Unrelax {
        /// The project directory.
        path: PathBuf,
    },
    /// Keep ByteAsk's own tools off the network (`on`), or allow them again
    /// (`off`; type it yourself in a terminal). `byteask company trial` turns it on.
    LocalOnly {
        #[arg(value_enum)]
        state: OnOff,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

/// Why `byteask policy relax` must refuse here, if it must: only
/// the person at the keyboard may loosen the sandbox, never an agent. Agents
/// run commands without a terminal and mark their environment.
pub(crate) fn relax_refusal(
    stdin_is_terminal: bool,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<&'static str> {
    const AGENT_MARKERS: [&str; 3] = ["CLAUDECODE", "CODEX_THREAD_ID", "CODEX_SANDBOX"];
    if AGENT_MARKERS.iter().any(|key| env(key).is_some()) {
        return Some(
            "this looks like an AI agent's session, and an agent may not turn the sandbox off. \
             Run the same command yourself in a terminal.",
        );
    }
    if !stdin_is_terminal {
        return Some(
            "relaxing the sandbox needs a person at a terminal. Run the same command yourself in \
             a terminal.",
        );
    }
    None
}

fn resolve_cwd(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_PROJECT_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    std::env::current_dir().context("could not read the current directory")
}

pub(crate) async fn load_config(
    overrides: &CliConfigOverrides,
    cwd: PathBuf,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<Config> {
    let cli_overrides = overrides
        .parse_overrides()
        .map_err(|err| anyhow::anyhow!("error parsing -c overrides: {err}"))?;
    ConfigBuilder::default()
        .cli_overrides(cli_overrides)
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd),
            codex_linux_sandbox_exe: linux_sandbox_exe,
            ..Default::default()
        })
        .build()
        .await
        .context("could not load the ByteAsk config")
}

/// The signed-in gateway and token, else the managed gateway and no token.
fn identity(config: &Config) -> (String, Option<String>) {
    match config.byteask_identity() {
        Some((base, token)) => (base, Some(token.into_inner())),
        None => (entitlement::DEFAULT_GATEWAY_BASE.to_string(), None),
    }
}

pub async fn run_toolbox(
    cli: ToolboxCli,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    match cli.command {
        ToolboxCommand::Serve(args) => serve(&cli.config_overrides, args, linux_sandbox_exe).await,
        ToolboxCommand::Install(args) => install(args),
        ToolboxCommand::List(args) => list(&cli.config_overrides, args, linux_sandbox_exe).await,
        ToolboxCommand::Report(args) => report(&args),
        ToolboxCommand::Hook(args) => {
            hook(&cli.config_overrides, args, linux_sandbox_exe).await;
            Ok(())
        }
    }
}

/// Start `toolbox hook follow-up` detached (its own process group, no terminal, output
/// discarded) so the agent's turn ends now and the full check finishes on its own.
fn start_follow_up(client: &str, input: &serde_json::Value) {
    use std::io::Write as _;
    let Ok(engine) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(engine);
    command
        .args(["toolbox", "hook", "follow-up", "--client", client])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    if let Ok(mut child) = command.spawn()
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(input.to_string().as_bytes());
    }
}

/// The hook's stdin, as much as the hooks send (a few KB); bounded so a runaway writer
/// cannot make the hook use unbounded memory.
const HOOK_INPUT_LIMIT: u64 = 1 << 20;

/// One agent hook. Never fails and never prints anything but its answer: a broken hook must
/// not trap the user's agent, so every problem means "no answer".
#[allow(clippy::print_stdout)]
async fn hook(overrides: &CliConfigOverrides, args: HookArgs, linux_sandbox_exe: Option<PathBuf>) {
    use std::io::Read as _;
    let mut raw = String::new();
    let _ = std::io::stdin().take(HOOK_INPUT_LIMIT).read_to_string(&mut raw);
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let answer = hook_answer(overrides, &args, &input, linux_sandbox_exe);
    #[cfg(unix)]
    let answer = {
        // The agent stops the hook when the user interrupts (SIGTERM, or SIGINT /
        // SIGHUP from a terminal). Dropping the answer stops the check and every program it
        // started; the version it was checking is recorded NOT RUN (cancelled).
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) else {
            return;
        };
        tokio::select! {
            answer = answer => answer,
            _ = term.recv() => { hook_cancelled(&args, &input).await; None }
            _ = int.recv() => { hook_cancelled(&args, &input).await; None }
            _ = hup.recv() => { hook_cancelled(&args, &input).await; None }
        }
    };
    #[cfg(not(unix))]
    let answer = answer.await;
    let Some(answer) = answer else {
        return;
    };
    println!("{answer}");
}

/// The hook was stopped while checking: record NOT RUN (cancelled) for what it was checking.
async fn hook_cancelled(args: &HookArgs, input: &serde_json::Value) {
    if !matches!(args.event, HookEvent::Stop | HookEvent::FollowUp) {
        return;
    }
    use codex_cpp_tools::hooks;
    let Ok(home) = codex_utils_home_dir::find_codex_home() else {
        return;
    };
    let Some(cwd) = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| resolve_cwd(None).ok())
    else {
        return;
    };
    let ctx = hooks::HookContext {
        home: home.to_path_buf(),
        cwd,
        session_id: input["session_id"].as_str().unwrap_or("session").to_string(),
        source: args.client.clone(),
        native_exec: false,
        sandbox_helper: None,
        relaxed: false,
        fast_tier: None,
    };
    let how = "the agent stopped the check (the user interrupted)";
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(1_500),
        hooks::record_cancelled(&ctx, how),
    )
    .await;
}

async fn hook_answer(
    overrides: &CliConfigOverrides,
    args: &HookArgs,
    input: &serde_json::Value,
    linux_sandbox_exe: Option<PathBuf>,
) -> Option<serde_json::Value> {
    use codex_cpp_tools::hooks;
    let home = codex_utils_home_dir::find_codex_home().ok()?.to_path_buf();
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| resolve_cwd(None).ok())?;
    let session_id = input["session_id"].as_str().unwrap_or("session").to_string();
    let mut ctx = hooks::HookContext {
        home: home.clone(),
        cwd: cwd.clone(),
        session_id,
        source: args.client.clone(),
        native_exec: false,
        sandbox_helper: linux_sandbox_exe.clone(),
        relaxed: false,
        fast_tier: None,
    };
    match args.event {
        HookEvent::SessionStart => {
            let source = input["source"].as_str().unwrap_or("startup");
            hooks::session_start(&ctx, matches!(source, "startup" | "clear")).await;
            None
        }
        HookEvent::Prompt => {
            let text = hooks::prompt(&ctx).await?;
            Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": text,
                }
            }))
        }
        HookEvent::Stop | HookEvent::FollowUp => {
            // The same trust rule as the CLI decides whether the gate may build and test.
            let config = load_config(overrides, cwd.clone(), linux_sandbox_exe).await.ok()?;
            ctx.native_exec = codex_cpp_tools::native_exec_allowed(&config);
            ctx.relaxed = byteask_policy::load(&home, std::slice::from_ref(&cwd))
                .policy
                .is_relaxed(&cwd);
            if args.event == HookEvent::FollowUp {
                hooks::follow_up(&ctx).await;
                return None;
            }
            match hooks::stop(&ctx).await {
                hooks::StopDecision::Block { reason } => {
                    Some(serde_json::json!({ "decision": "block", "reason": reason }))
                }
                hooks::StopDecision::Allow { message, follow_up } => {
                    if follow_up {
                        start_follow_up(&args.client, input);
                    }
                    message.map(|message| serde_json::json!({ "systemMessage": message }))
                }
            }
        }
    }
}

async fn serve(
    overrides: &CliConfigOverrides,
    args: WorkspaceArgs,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    // First, before anything can print or spawn: the MCP channel moves off
    // fds 0 and 1.
    #[cfg(unix)]
    let transport = server::take_stdio().context("could not take stdio for MCP")?;
    #[cfg(not(unix))]
    let transport = (tokio::io::stdin(), tokio::io::stdout());

    let cwd = resolve_cwd(args.cwd)?;
    let config = load_config(overrides, cwd, linux_sandbox_exe).await?;
    let home = config.codex_home.to_path_buf();
    let (base, token) = identity(&config);
    let state = entitlement::cached_state(&home, token.as_deref());
    let thread = ToolboxThread::start(&config).await;
    let toolbox = ToolboxServer::new(thread, state);

    // The plan check runs in the background: an offline machine must not wait
    // on the network before its first tool call (the cached answer applies).
    // A local-only home never asks: the cached answer applies.
    if byteask_policy::local_only(&home).0 {
        let running = toolbox.serve(transport).await?;
        running.waiting().await?;
        return Ok(());
    }
    let gate = toolbox.gate_handle();
    tokio::spawn(async move {
        let fresh = entitlement::refresh(&home, &base, token.as_deref(), server::now_unix()).await;
        match gate.write() {
            Ok(mut current) => *current = fresh,
            Err(poisoned) => *poisoned.into_inner() = fresh,
        }
    });

    let running = toolbox.serve(transport).await?;
    running.waiting().await?;
    Ok(())
}

/// Reads only the results store: no config, no sign-in, no network.
#[allow(clippy::print_stdout)]
fn report(args: &crate::report::ReportArgs) -> anyhow::Result<()> {
    let home = codex_utils_home_dir::find_codex_home()
        .context("could not find the ByteAsk home directory")?;
    print!("{}", crate::report::render(&home, args, results_store::now_unix()));
    Ok(())
}

#[allow(clippy::print_stdout)]
fn install(args: InstallArgs) -> anyhow::Result<()> {
    let command = install::ServerCommand {
        engine: std::env::current_exe().context("could not find the ByteAsk engine binary")?,
        byteask_home: codex_utils_home_dir::find_codex_home()
            .context("could not find the ByteAsk home directory")?
            .to_path_buf(),
    };
    if args.claude_code {
        let route = match install::find_on_path("claude") {
            Some(claude) => install::ClaudeRoute::Cli(claude),
            None => install::ClaudeRoute::File(
                install::claude_json_path().context("could not find the home directory")?,
            ),
        };
        println!(
            "{}",
            install::install_claude_code(&command, &route, args.dry_run)?
        );
        let settings = install::claude_settings_path().context("could not find the home directory")?;
        println!(
            "{}",
            install::install_hooks(&command, &settings, "claude-code", args.dry_run)?
        );
    }
    if args.codex {
        let config = install::codex_config_path().context("could not find the home directory")?;
        println!(
            "{}",
            install::install_codex(&command, &config, args.dry_run)?
        );
        let hooks = install::codex_hooks_path().context("could not find the home directory")?;
        println!(
            "{}",
            install::install_hooks(&command, &hooks, "codex", args.dry_run)?
        );
        println!("Codex asks you to trust new hooks before it runs them; accept the ByteAsk ones.");
    }
    Ok(())
}

#[allow(clippy::print_stdout)]
async fn list(
    overrides: &CliConfigOverrides,
    args: ListArgs,
    linux_sandbox_exe: Option<PathBuf>,
) -> anyhow::Result<()> {
    let cwd = resolve_cwd(args.workspace.cwd)?;
    let config = load_config(overrides, cwd, linux_sandbox_exe).await?;
    let thread = ToolboxThread::start(&config).await;
    let toolbox = ToolboxServer::new(thread, crate::gate::GateState::off());
    if args.json {
        println!("{}", serde_json::to_string_pretty(toolbox.listed_tools())?);
        return Ok(());
    }
    for tool in toolbox.listed_tools() {
        let first_line = tool
            .description
            .as_deref()
            .and_then(|text| text.lines().next())
            .unwrap_or_default();
        println!("{:<16} {first_line}", tool.name);
    }
    println!("\n{} tools.", toolbox.listed_tools().len());
    Ok(())
}

#[allow(clippy::print_stdout)]
pub async fn run_policy(cli: PolicyCli) -> anyhow::Result<()> {
    let home = codex_utils_home_dir::find_codex_home()
        .context("could not find the ByteAsk home directory")?
        .to_path_buf();
    match cli.command {
        PolicyCommand::Relax { path } => {
            use std::io::IsTerminal as _;
            if let Some(why) = relax_refusal(std::io::stdin().is_terminal(), |key| {
                std::env::var_os(key)
            }) {
                anyhow::bail!("not relaxed: {why}");
            }
            let project = byteask_policy::set_relaxed(&home, &path, true)
                .with_context(|| format!("could not relax {}", path.display()))?;
            println!(
                "ByteAsk's tools now run WITHOUT the sandbox in {} and everything under it: \
                 builds and the programs they start can write anywhere you can. Turn it back on \
                 with `byteask policy unrelax {}`. New sessions pick this up.",
                project.display(),
                project.display()
            );
        }
        PolicyCommand::Unrelax { path } => {
            let project = byteask_policy::set_relaxed(&home, &path, false)
                .with_context(|| format!("could not update the policy for {}", path.display()))?;
            println!(
                "The tool sandbox is on again for {}. New sessions pick this up.",
                project.display()
            );
        }
        PolicyCommand::LocalOnly { state } => {
            use std::io::IsTerminal as _;
            if state == OnOff::Off
                && let Some(why) = relax_refusal(std::io::stdin().is_terminal(), |key| {
                    std::env::var_os(key)
                })
            {
                anyhow::bail!("local only stays on: {why}");
            }
            byteask_policy::set_local_only(&home, state == OnOff::On)
                .context("could not update the policy file")?;
            if state == OnOff::On {
                println!(
                    "Local only is on: ByteAsk's online tools and reference lookups answer NOT RUN \
                     and send nothing, and the toolbox does not check your plan online. Your \
                     agent's own model calls are not affected. New sessions pick this up."
                );
            } else {
                println!(
                    "Local only is off: ByteAsk's online tools and reference lookups may reach \
                     their services again. New sessions pick this up."
                );
            }
        }
        PolicyCommand::Show => {
            let cwd = std::env::current_dir().context("could not read the current directory")?;
            let loaded = byteask_policy::load(&home, &[cwd]);
            println!("{}", describe(&home, &loaded, server::now_unix()));
        }
        PolicyCommand::RefreshPlan => {
            let cwd = std::env::current_dir().context("could not read the current directory")?;
            let config = load_config(&cli.config_overrides, cwd, None).await?;
            let (base, token) = identity(&config);
            if token.is_none() {
                println!(
                    "Not signed in to ByteAsk, so there is no plan to check. Run `byteask login`."
                );
                return Ok(());
            }
            let state =
                entitlement::refresh(&home, &base, token.as_deref(), server::now_unix()).await;
            match (&state.identity, &state.cache) {
                (crate::gate::Identity::Expired, _) => {
                    println!("Your ByteAsk sign-in has expired. Run `byteask` once to renew it.")
                }
                (_, Some(cache)) if cache.checked_at + 60 >= server::now_unix() => {
                    println!("Plan: {}. Stored for offline use for 7 days.", cache.plan)
                }
                _ => println!(
                    "Could not reach ByteAsk to check your plan; the last stored answer still applies."
                ),
            }
        }
        PolicyCommand::Reset => {
            if byteask_policy::reset(&home)? {
                println!(
                    "Moved {} to {}. Every setting is back to its safe default.",
                    byteask_policy::path(&home).display(),
                    home.join(byteask_policy::CORRUPT_BACKUP_NAME).display()
                );
            } else {
                println!("There is no policy file; every setting is already at its safe default.");
            }
        }
    }
    Ok(())
}

fn describe(home: &std::path::Path, loaded: &byteask_policy::Loaded, now: u64) -> String {
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "none".to_string()
        } else {
            items.join(", ")
        }
    };
    let policy = &loaded.policy;
    let plan = match &policy.plan {
        Some(cache) => {
            let age_hours = now.saturating_sub(cache.checked_at) / 3600;
            let gate = if cache.gate { "on" } else { "off" };
            format!(
                "{} (checked {age_hours} h ago; the toolbox plan gate was {gate})",
                cache.plan
            )
        }
        None => "not checked yet".to_string(),
    };
    let mut out = format!(
        "Policy file: {}\n\
         Plan: {plan}\n\
         Sandbox relaxed for: {}\n\
         Source uploads allowed for: {}\n\
         Debugger hosts allowed: {}",
        byteask_policy::path(home).display(),
        list(
            policy
                .relaxed_projects
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        ),
        list(
            policy
                .upload_projects
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        ),
        list(policy.debugger_hosts.clone()),
    );
    let (local_only, network_problems) = byteask_policy::local_only(home);
    out.push_str(&format!(
        "\nLocal only (ByteAsk's own tools stay off the network): {}",
        if local_only { "on" } else { "off" }
    ));
    for problem in network_problems {
        out.push_str(&format!("\nProblem: {problem}"));
    }
    for problem in &loaded.problems {
        out.push_str(&format!("\nProblem: {problem}"));
    }
    out
}

#[cfg(test)]
mod relax_tests {
    use super::relax_refusal;

    #[test]
    fn only_a_person_at_a_terminal_may_relax_the_sandbox() {
        let clean = |_: &str| None;
        assert_eq!(relax_refusal(true, clean), None);
        assert!(relax_refusal(false, clean).is_some(), "no terminal");
        for marker in ["CLAUDECODE", "CODEX_THREAD_ID", "CODEX_SANDBOX"] {
            let env = move |key: &str| (key == marker).then(|| std::ffi::OsString::from("1"));
            assert!(relax_refusal(true, env).is_some(), "{marker} marks an agent session");
        }
    }
}
