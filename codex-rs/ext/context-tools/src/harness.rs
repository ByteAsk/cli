//! The harness side of ByteAsk Context: the turn-0 pack before the
//! first model turn, and the finish check before the agent ends its turn. They do in every
//! session kind, `exec` included, what the engine's Claude Code hooks do there.
//!
//! Both are off unless `[byteask_harness] context_pack` / `context_finish_check` is set. The
//! engine is started the way the session already starts it: the installed engine whose
//! `engine.json` record covers the workspace (the record that turns the `context_*` tools on),
//! else the session's MCP server entry for the engine (`python -m bctx.cli --data D mcp --user U
//! --partition P ...`). Anything that goes wrong (no engine, an old engine, a timeout, a crash)
//! degrades silently to "no pack" / "no check": the session never waits on, or shows, a broken
//! Context.
//!
//! Interface with the Context engine: the adapters its Claude Code hooks call, on the same
//! command line with the subcommand replaced, in the session's environment plus the entry's.
//! - pack: `hook-card --user U --partition P [--setting S]`, a UserPromptSubmit event as JSON
//!   on stdin. It also keeps the task with the worktree, which the finish check reads. On stdout
//!   `{"hookSpecificOutput": {"additionalContext": "<card>"}}`.
//! - finish check: `hook-stop --user U --partition P [--setting S] --workspace W`, a Stop event
//!   as JSON on stdin; on stdout nothing or `{"decision": "block", "reason": "..."}`.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use crate::backend;
use crate::client::Launch;
use crate::engine::Workspace;

/// How long the pack may take before the first turn starts without it.
pub const PACK_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the finish check (a targeted build and test run) may take.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(600);
/// Largest pack passed to the model (the pack is designed for about 4K tokens).
const MAX_PACK_BYTES: usize = 24 * 1024;
/// Most times the finish check may send the agent back in one thread.
const MAX_FINISH_BLOCKS: u8 = 2;
/// What `hook-card` prints instead of a card when building one failed.
const CARD_UNAVAILABLE: &str = "(ByteAsk locator card unavailable";

/// The engine's command line for one workspace, without the subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineCli {
    program: PathBuf,
    /// Arguments before the subcommand (`-m bctx.cli --data D`, `-I serve.py --data D`).
    prefix: Vec<String>,
    /// Set on top of the session's environment.
    env: Vec<(String, String)>,
    /// Where the process runs.
    cwd: PathBuf,
    user: String,
    partition: String,
    setting: Option<String>,
    /// The agent's worktree.
    workspace: PathBuf,
}

impl EngineCli {
    /// The installed engine of `engine.json`, started like its query server.
    pub fn from_workspace(workspace: &Workspace, cwd: &Path) -> Self {
        let launch = backend::launch(workspace);
        let mut prefix = launch.args;
        if prefix.last().is_some_and(|arg| arg == "serve") {
            prefix.pop();
        }
        Self {
            program: launch.program,
            prefix,
            env: launch.env,
            cwd: launch.cwd,
            user: workspace.engine.user.clone(),
            partition: workspace.project.partition.clone(),
            setting: Some(workspace.project.setting.clone()).filter(|s| !s.is_empty()),
            workspace: cwd.to_path_buf(),
        }
    }

    /// The session's MCP server entry for the engine: its arguments up to `mcp` are the
    /// prefix, and `--user`, `--partition`, `--setting` and `--workspace` after it are kept.
    /// `None` without a user and a partition (the hooks need both).
    pub fn from_mcp_server(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Option<Self> {
        let at = args.iter().position(|arg| arg == "mcp")?;
        let rest = &args[at + 1..];
        let value = |key: &str| {
            let flag = format!("--{key}");
            let inline = format!("--{key}=");
            rest.iter().enumerate().find_map(|(i, arg)| {
                if *arg == flag {
                    rest.get(i + 1).cloned()
                } else {
                    arg.strip_prefix(&inline).map(str::to_string)
                }
            })
        };
        Some(Self {
            program: PathBuf::from(command),
            prefix: args[..at].to_vec(),
            env: env.to_vec(),
            cwd: cwd.to_path_buf(),
            user: value("user")?,
            partition: value("partition")?,
            setting: value("setting").filter(|s| !s.is_empty()),
            workspace: value("workspace")
                .map(PathBuf::from)
                .unwrap_or_else(|| cwd.to_path_buf()),
        })
    }

    fn launch(&self, subcommand: &str) -> Launch {
        let mut args = self.prefix.clone();
        args.extend([
            subcommand.to_string(),
            "--user".to_string(),
            self.user.clone(),
            "--partition".to_string(),
            self.partition.clone(),
        ]);
        if let Some(setting) = &self.setting {
            args.extend(["--setting".to_string(), setting.clone()]);
        }
        // `hook-card` takes the worktree from the event's `cwd`.
        if subcommand != "hook-card" {
            args.extend([
                "--workspace".to_string(),
                self.workspace.to_string_lossy().into_owned(),
            ]);
        }
        Launch {
            program: self.program.clone(),
            args,
            env: self.env.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

/// What the harness may ask the Context engine in one root thread.
pub struct ContextHarness {
    cli: EngineCli,
    pack: bool,
    finish_check: bool,
    pack_done: AtomicBool,
    finish_blocks: AtomicU8,
}

impl ContextHarness {
    /// `None` when neither switch is on.
    pub fn new(cli: EngineCli, pack: bool, finish_check: bool) -> Option<Self> {
        (pack || finish_check).then(|| Self {
            cli,
            pack,
            finish_check,
            pack_done: AtomicBool::new(false),
            finish_blocks: AtomicU8::new(0),
        })
    }

    /// The pack for `prompt`, once per thread (the first turn). `None` when off, already
    /// given, or unavailable.
    pub async fn turn0_pack(&self, session_id: &str, prompt: &str) -> Option<String> {
        if !self.pack || self.pack_done.swap(true, Ordering::AcqRel) || prompt.trim().is_empty() {
            return None;
        }
        let event = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session_id,
            "cwd": self.cli.workspace.to_string_lossy(),
            "prompt": prompt,
        });
        let out = run(
            &self.cli.launch("hook-card"),
            event.to_string().as_bytes(),
            PACK_TIMEOUT,
        )
        .await?;
        let text = card_text(&out)?;
        Some(render_pack(&text))
    }

    /// The finish check's reason to continue, or `None` to let the turn end.
    pub async fn finish_check(
        &self,
        session_id: &str,
        last_message: Option<&str>,
    ) -> Option<String> {
        if !self.finish_check || self.finish_blocks.load(Ordering::Acquire) >= MAX_FINISH_BLOCKS {
            return None;
        }
        let event = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": session_id,
            "cwd": self.cli.workspace.to_string_lossy(),
            "stop_hook_active": self.finish_blocks.load(Ordering::Acquire) > 0,
            "last_assistant_message": last_message.unwrap_or_default(),
        });
        let out = run(
            &self.cli.launch("hook-stop"),
            event.to_string().as_bytes(),
            FINISH_TIMEOUT,
        )
        .await?;
        let reason = block_reason(&out)?;
        self.finish_blocks.fetch_add(1, Ordering::AcqRel);
        Some(format!("ByteAsk Context finish check:\n{reason}"))
    }
}

/// The command's stdout when it exits 0 within `timeout`; anything else is `None`.
async fn run(launch: &Launch, stdin: &[u8], timeout: Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(&launch.program);
    command
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    if let Some(mut pipe) = child.stdin.take() {
        // A command that does not read its input must not block the session.
        let _ = tokio::time::timeout(Duration::from_secs(5), pipe.write_all(stdin)).await;
        drop(pipe);
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The card in `hook-card`'s output; `None` when there is none or it failed.
fn card_text(out: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let text = value
        .get("hookSpecificOutput")?
        .get("additionalContext")?
        .as_str()?
        .trim();
    (!text.is_empty() && !text.starts_with(CARD_UNAVAILABLE)).then(|| text.to_string())
}

fn render_pack(text: &str) -> String {
    let mut body = text.to_string();
    if body.len() > MAX_PACK_BYTES {
        let mut cut = MAX_PACK_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str("\n[pack cut at 24 KB]");
    }
    format!("ByteAsk Context, turn-0 pack for this task (from the build-aware index):\n{body}")
}

/// `Some(reason)` when the check's output asks to continue.
pub fn block_reason(out: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    if value.get("decision").and_then(|d| d.as_str()) != Some("block") {
        return None;
    }
    let reason = value.get("reason").and_then(|r| r.as_str())?.trim();
    (!reason.is_empty()).then(|| reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineFile;
    use crate::engine::Project;
    use crate::engine::SCHEMA;
    use pretty_assertions::assert_eq;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_block_needs_a_reason() {
        assert_eq!(
            block_reason(r#"{"decision": "block", "reason": " test fails "}"#),
            Some("test fails".to_string())
        );
        assert_eq!(block_reason(""), None);
        assert_eq!(block_reason(r#"{"decision": "approve"}"#), None);
        assert_eq!(block_reason(r#"{"decision": "block", "reason": ""}"#), None);
        assert_eq!(block_reason("not json"), None);
    }

    #[test]
    fn a_failed_card_is_no_card() {
        let out = |text: &str| {
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": text}})
                .to_string()
        };
        assert_eq!(card_text(&out(" Ring::push ")), Some("Ring::push".to_string()));
        assert_eq!(card_text(&out("(ByteAsk locator card unavailable: no index)")), None);
        assert_eq!(card_text(&out("")), None);
        assert_eq!(card_text(""), None);
    }

    fn workspace(install: &Path, python: &Path) -> Workspace {
        Workspace {
            engine: EngineFile {
                schema: SCHEMA,
                version: "test".to_string(),
                install: install.to_path_buf(),
                python: python.to_path_buf(),
                data: install.join("data"),
                user: "local:me".to_string(),
                projects: Vec::new(),
            },
            project: Project {
                root: PathBuf::from("/src/feed"),
                partition: "feed".to_string(),
                setting: "debug".to_string(),
                target: "feed_tests".to_string(),
            },
        }
    }

    #[test]
    fn an_installed_engine_is_started_like_the_query_server() {
        let ws = workspace(Path::new("/opt/ctx"), Path::new("/usr/bin/python3"));
        let cli = EngineCli::from_workspace(&ws, Path::new("/src/feed/sub"));
        let launch = cli.launch("hook-stop");
        assert_eq!(
            launch.args,
            strings(&[
                "-I",
                "/opt/ctx/serve.py",
                "--data",
                "/opt/ctx/data",
                "hook-stop",
                "--user",
                "local:me",
                "--partition",
                "feed",
                "--setting",
                "debug",
                "--workspace",
                "/src/feed/sub"
            ])
        );
        assert_eq!(launch.program, PathBuf::from("/usr/bin/python3"));
        // hook-card reads the worktree from its event
        assert!(!cli.launch("hook-card").args.contains(&"--workspace".to_string()));
    }

    #[test]
    fn an_mcp_server_entry_gives_the_same_command_line() {
        let args = strings(&[
            "-m",
            "bctx.cli",
            "--data",
            "/idx",
            "mcp",
            "--user",
            "bench",
            "--partition=md4c",
            "--setting",
            "debug",
            "--workspace",
            "/clones/md4c",
            "--tools",
            "context_card,context_verify",
        ]);
        let env = vec![("PYTHONPATH".to_string(), "/frozen/v6".to_string())];
        let cli = EngineCli::from_mcp_server("/venv/python", &args, &env, Path::new("/elsewhere"))
            .expect("cli");
        let launch = cli.launch("hook-stop");
        assert_eq!(launch.program, PathBuf::from("/venv/python"));
        assert_eq!(
            launch.args,
            strings(&[
                "-m",
                "bctx.cli",
                "--data",
                "/idx",
                "hook-stop",
                "--user",
                "bench",
                "--partition",
                "md4c",
                "--setting",
                "debug",
                "--workspace",
                "/clones/md4c"
            ])
        );
        assert_eq!(launch.env, env);
        // without --workspace the session's directory is the worktree; without a partition
        // there is nothing to ask
        let short = strings(&["-m", "bctx.cli", "mcp", "--user", "u", "--partition", "p"]);
        let cli = EngineCli::from_mcp_server("py", &short, &[], Path::new("/w")).expect("cli");
        assert_eq!(
            cli.launch("hook-card").args,
            strings(&["-m", "bctx.cli", "hook-card", "--user", "u", "--partition", "p"])
        );
        assert_eq!(cli.workspace, PathBuf::from("/w"));
        let no_partition = strings(&["-m", "bctx.cli", "mcp", "--user", "u"]);
        assert_eq!(EngineCli::from_mcp_server("py", &no_partition, &[], Path::new("/w")), None);
    }

    /// A fake engine: "python" is a shell script that sees the subcommand as `$1` (the prefix
    /// is empty) and the event on stdin.
    #[cfg(unix)]
    fn fake_engine(script: &str) -> (tempfile::TempDir, EngineCli) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("dir");
        let program = dir.path().join("py");
        std::fs::write(&program, format!("#!/bin/sh\n{script}")).expect("script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let args = strings(&["mcp", "--user", "u", "--partition", "p", "--workspace", "/src/feed"]);
        let cli = EngineCli::from_mcp_server(
            &program.to_string_lossy(),
            &args,
            &[("FAKE_SUFFIX".to_string(), " <- card".to_string())],
            dir.path(),
        )
        .expect("cli");
        (dir, cli)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_pack_comes_from_the_card_hook_once() {
        let (_dir, cli) = fake_engine(
            "case \"$1\" in\n  hook-card) printf '{\"hookSpecificOutput\": {\"additionalContext\": \"%s%s\"}}' \"$(grep -o 'Ring::push overflows' -)\" \"$FAKE_SUFFIX\";;\nesac\n",
        );
        let harness = ContextHarness::new(cli, true, false).expect("on");
        let pack = harness
            .turn0_pack("thread-1", "Rules: none\n\nISSUE:\nRing::push overflows")
            .await
            .expect("pack from the card hook");
        assert!(pack.contains("Ring::push overflows <- card"), "{pack}");
        assert!(pack.starts_with("ByteAsk Context, turn-0 pack"), "{pack}");
        assert_eq!(harness.turn0_pack("thread-1", "again").await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_finish_check_blocks_on_a_reason_at_most_twice_and_is_silent_on_failure() {
        let (_dir, cli) = fake_engine(
            "case \"$1\" in\n  hook-stop) cat >/dev/null; echo '{\"decision\": \"block\", \"reason\": \"tests fail\"}';;\nesac\n",
        );
        let harness = ContextHarness::new(cli, false, true).expect("on");
        for _ in 0..2 {
            let reason = harness
                .finish_check("thread-1", Some("done"))
                .await
                .expect("block");
            assert_eq!(reason, "ByteAsk Context finish check:\ntests fail");
        }
        assert_eq!(harness.finish_check("thread-1", Some("done")).await, None);
        // the pack switch is off: no pack even though an engine exists
        assert_eq!(harness.turn0_pack("thread-1", "task").await, None);

        // an engine that crashes: no check, no pack, no error
        let (_dir2, broken) = fake_engine("exit 1\n");
        let quiet = ContextHarness::new(broken, true, true).expect("on");
        assert_eq!(quiet.finish_check("t", None).await, None);
        assert_eq!(quiet.turn0_pack("t", "task").await, None);
    }

    #[test]
    fn both_switches_off_means_no_harness() {
        let ws = workspace(Path::new("/opt/ctx"), Path::new("/usr/bin/python3"));
        let cli = EngineCli::from_workspace(&ws, Path::new("/src"));
        assert!(ContextHarness::new(cli, false, false).is_none());
    }
}
