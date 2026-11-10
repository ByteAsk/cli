//! `byteask toolbox install --claude-code | --codex`: register the
//! toolbox as a user-level MCP server in the other agent, in one command.
//!
//! The entry runs the ByteAsk engine binary directly (`<engine> toolbox
//! serve`) with `BYTEASK_HOME` set:
//!
//! * not the `byteask` launcher, which can print an update prompt on stdout,
//!   and stdout is the MCP channel;
//! * `BYTEASK_HOME` explicitly, because the engine falls back to `CODEX_HOME`
//!   when it is unset, and a Codex user's `CODEX_HOME` would otherwise make the
//!   toolbox read Codex's own config.
//!
//! The same command also writes the hook entries:
//! SessionStart (remember the work tree), Stop (the finish gate, fast tier) and
//! UserPromptSubmit (the latest result, marked stale after an edit). Claude Code
//! reads them from its user `settings.json`, Codex from its `hooks.json`; both use
//! the same hook protocol. Entries are recognised by [`HOOK_MARKER`] and replaced,
//! so installing twice changes nothing.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;

/// MCP server name in the other agent's config.
pub const ENTRY_NAME: &str = "byteask";
/// Codex's own default tool timeout is 300 s, the same as our tools' internal
/// cap, which would cut a slow build at the boundary. Leave headroom.
pub const CODEX_TOOL_TIMEOUT_SECS: f64 = 900.0;

/// Every command we write contains this, so a reinstall replaces exactly our entries.
pub const HOOK_MARKER: &str = " toolbox hook ";
/// The hook events, the `toolbox hook` event each runs, and its timeout in seconds. The Stop
/// hook waits for the gate's fast tier (90 s), so its timeout sits above it; the prompt
/// hook spends at most 2 s itself.
pub const HOOK_EVENTS: [(&str, &str, u64); 3] = [
    ("SessionStart", "session-start", 60),
    ("Stop", "stop", 150),
    ("UserPromptSubmit", "prompt", 5),
];

/// What the other agent should run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    pub engine: PathBuf,
    pub byteask_home: PathBuf,
}

impl ServerCommand {
    pub fn args() -> [&'static str; 2] {
        ["toolbox", "serve"]
    }

    /// The shell command a hook runs (both agents run hook commands through a shell).
    pub fn hook_command(&self, event: &str, client: &str) -> String {
        format!(
            "BYTEASK_HOME={} {}{HOOK_MARKER}{event} --client {client}",
            shell_quote(&self.byteask_home.to_string_lossy()),
            shell_quote(&self.engine.to_string_lossy()),
        )
    }

    fn claude_entry(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "stdio",
            "command": self.engine.to_string_lossy(),
            "args": Self::args(),
            "env": { "BYTEASK_HOME": self.byteask_home.to_string_lossy() },
        })
    }
}

/// How the Claude Code entry is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeRoute {
    /// Claude Code's own CLI (`claude mcp add-json --scope user`): it owns the
    /// file and rewrites it itself, so this is the race-free path.
    Cli(PathBuf),
    /// No `claude` on PATH: edit the user config file directly.
    File(PathBuf),
}

/// `name` on `$PATH`, if present.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Claude Code's user config file: `$CLAUDE_CONFIG_DIR/.claude.json`, else
/// `~/.claude.json`.
pub fn claude_json_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join(".claude.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude.json"))
}

/// Codex's user config: `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`.
pub fn codex_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join("config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex").join("config.toml"))
}

/// Register the toolbox in Claude Code at user scope. Returns what was done.
pub fn install_claude_code(
    command: &ServerCommand,
    route: &ClaudeRoute,
    dry_run: bool,
) -> anyhow::Result<String> {
    let entry = command.claude_entry();
    match route {
        ClaudeRoute::Cli(claude) => {
            let json = entry.to_string();
            if dry_run {
                return Ok(format!(
                    "Would run: {} mcp add-json --scope user {ENTRY_NAME} '{json}'",
                    claude.display()
                ));
            }
            // An existing entry makes `add` fail, so remove ours first; a
            // missing one is not an error.
            let _ = std::process::Command::new(claude)
                .args(["mcp", "remove", "--scope", "user", ENTRY_NAME])
                .output();
            let output = std::process::Command::new(claude)
                .args(["mcp", "add-json", "--scope", "user", ENTRY_NAME, &json])
                .output()
                .with_context(|| format!("could not run {}", claude.display()))?;
            if !output.status.success() {
                bail!(
                    "Claude Code refused the entry ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(format!(
                "Added the ByteAsk tools to Claude Code for this user (MCP server \"{ENTRY_NAME}\").\n\
                 Restart Claude Code to load them; `claude mcp get {ENTRY_NAME}` shows the entry."
            ))
        }
        ClaudeRoute::File(path) => {
            let mut doc = match std::fs::read_to_string(path) {
                Ok(text) if text.trim().is_empty() => serde_json::json!({}),
                Ok(text) => {
                    serde_json::from_str::<serde_json::Value>(&text).with_context(|| {
                        format!(
                            "{} is not valid JSON, so it was left untouched",
                            path.display()
                        )
                    })?
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
                Err(err) => return Err(err).context(format!("could not read {}", path.display())),
            };
            let Some(root) = doc.as_object_mut() else {
                bail!(
                    "{} is not a JSON object, so it was left untouched",
                    path.display()
                );
            };
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}));
            let Some(servers) = servers.as_object_mut() else {
                bail!(
                    "mcpServers in {} is not an object, so it was left untouched",
                    path.display()
                );
            };
            servers.insert(ENTRY_NAME.to_string(), entry);
            let text = format!("{}\n", serde_json::to_string_pretty(&doc)?);
            if dry_run {
                return Ok(format!("Would write {}:\n{text}", path.display()));
            }
            write_replacing(path, text.as_bytes())?;
            Ok(format!(
                "Added the ByteAsk tools to {} (MCP server \"{ENTRY_NAME}\").\n\
                 Restart Claude Code to load them. Close Claude Code first if it is running: it \
                 rewrites this file itself.",
                path.display()
            ))
        }
    }
}

/// Register the toolbox in Codex's user config. Returns what was done.
pub fn install_codex(
    command: &ServerCommand,
    config: &Path,
    dry_run: bool,
) -> anyhow::Result<String> {
    // If CODEX_HOME points at ByteAsk's own home, the entry would make ByteAsk
    // load itself as a tool server.
    if let Some(dir) = config.parent()
        && same_dir(dir, &command.byteask_home)
    {
        bail!(
            "the Codex config resolved to ByteAsk's own home ({}). Set CODEX_HOME to your Codex \
             directory (usually ~/.codex) and run this again.",
            dir.display()
        );
    }
    let mut doc = match std::fs::read_to_string(config) {
        Ok(text) => text.parse::<toml_edit::DocumentMut>().with_context(|| {
            format!(
                "{} is not valid TOML, so it was left untouched",
                config.display()
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(err) => return Err(err).context(format!("could not read {}", config.display())),
    };
    if !doc.contains_key("mcp_servers") {
        let mut servers = toml_edit::Table::new();
        servers.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(servers);
    }
    let Some(servers) = doc["mcp_servers"].as_table_mut() else {
        bail!(
            "mcp_servers in {} is not a table, so it was left untouched",
            config.display()
        );
    };
    let mut entry = toml_edit::Table::new();
    entry["command"] = toml_edit::value(command.engine.to_string_lossy().as_ref());
    let mut args = toml_edit::Array::new();
    for arg in ServerCommand::args() {
        args.push(arg);
    }
    entry["args"] = toml_edit::value(args);
    let mut env = toml_edit::InlineTable::new();
    env.insert(
        "BYTEASK_HOME",
        command.byteask_home.to_string_lossy().as_ref().into(),
    );
    entry["env"] = toml_edit::value(env);
    entry["tool_timeout_sec"] = toml_edit::value(CODEX_TOOL_TIMEOUT_SECS);
    servers.insert(ENTRY_NAME, toml_edit::Item::Table(entry));
    let text = doc.to_string();
    if dry_run {
        return Ok(format!("Would write {}:\n{text}", config.display()));
    }
    if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_replacing(config, text.as_bytes())?;
    Ok(format!(
        "Added the ByteAsk tools to {} ([mcp_servers.{ENTRY_NAME}]).\n\
         Start a new Codex session to load them.",
        config.display()
    ))
}

/// Claude Code's user settings: `$CLAUDE_CONFIG_DIR/settings.json`, else
/// `~/.claude/settings.json`.
pub fn claude_settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join("settings.json"))
}

/// Codex's user hooks file: `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`.
pub fn codex_hooks_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join("hooks.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex").join("hooks.json"))
}

/// Put our hook entries into `doc` (`{"hooks": {...}}`), replacing any earlier ones of ours and
/// keeping everything else as it was.
pub fn merge_hooks(doc: &mut serde_json::Value, command: &ServerCommand, client: &str) -> anyhow::Result<()> {
    let Some(root) = doc.as_object_mut() else {
        bail!("not a JSON object");
    };
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("`hooks` is not an object");
    };
    for (event, verb, timeout) in HOOK_EVENTS {
        let groups = hooks.entry(event).or_insert_with(|| serde_json::json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("`hooks.{event}` is not a list");
        };
        groups.retain(|group| {
            !group["hooks"].as_array().is_some_and(|handlers| {
                handlers.iter().any(|handler| {
                    handler["command"].as_str().is_some_and(|c| c.contains(HOOK_MARKER))
                })
            })
        });
        groups.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command.hook_command(verb, client),
                "timeout": timeout,
            }]
        }));
    }
    Ok(())
}

fn read_json_object(path: &Path) -> anyhow::Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .with_context(|| format!("{} is not valid JSON, so it was left untouched", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(err) => Err(err).context(format!("could not read {}", path.display())),
    }
}

/// Write the hook entries into `path` (Claude Code's settings.json or Codex's hooks.json).
pub fn install_hooks(
    command: &ServerCommand,
    path: &Path,
    client: &str,
    dry_run: bool,
) -> anyhow::Result<String> {
    let mut doc = read_json_object(path)?;
    merge_hooks(&mut doc, command, client)
        .with_context(|| format!("{} was left untouched", path.display()))?;
    let text = format!("{}\n", serde_json::to_string_pretty(&doc)?);
    if dry_run {
        return Ok(format!("Would write {}:\n{text}", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_replacing(path, text.as_bytes())?;
    Ok(format!(
        "Added the ByteAsk check hooks to {} (SessionStart, Stop: the finish gate, \
         UserPromptSubmit: the latest result).",
        path.display()
    ))
}

/// POSIX single-quoting.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Replace `path` atomically, keeping its permissions when it exists.
fn write_replacing(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("byteask-tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("could not write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn command() -> ServerCommand {
        ServerCommand {
            engine: PathBuf::from("/opt/byteask/bin/byteask-engine"),
            byteask_home: PathBuf::from("/home/u/.byteask"),
        }
    }

    #[test]
    fn claude_file_route_adds_the_entry_and_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"numStartups": 7, "mcpServers": {"other": {"type": "stdio", "command": "x"}}}"#,
        )
        .unwrap();
        install_claude_code(&command(), &ClaudeRoute::File(path.clone()), false).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["numStartups"], 7);
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            doc["mcpServers"]["byteask"],
            serde_json::json!({
                "type": "stdio",
                "command": "/opt/byteask/bin/byteask-engine",
                "args": ["toolbox", "serve"],
                "env": { "BYTEASK_HOME": "/home/u/.byteask" },
            })
        );
    }

    #[test]
    fn claude_file_route_refuses_to_clobber_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = install_claude_code(&command(), &ClaudeRoute::File(path.clone()), false)
            .expect_err("must refuse");
        assert!(err.to_string().contains("left untouched"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn claude_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let said = install_claude_code(&command(), &ClaudeRoute::File(path.clone()), true).unwrap();
        assert!(said.contains("Would write"), "{said}");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn claude_cli_route_calls_claude_with_user_scope() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        install_claude_code(&command(), &ClaudeRoute::Cli(fake), false).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines[0], "mcp remove --scope user byteask");
        assert!(
            lines[1].starts_with("mcp add-json --scope user byteask {"),
            "{}",
            lines[1]
        );
        let json: serde_json::Value =
            serde_json::from_str(lines[1].trim_start_matches("mcp add-json --scope user byteask "))
                .unwrap();
        assert_eq!(json["args"], serde_json::json!(["toolbox", "serve"]));
        assert_eq!(json["env"]["BYTEASK_HOME"], "/home/u/.byteask");
    }

    #[cfg(unix)]
    #[test]
    fn claude_cli_failure_is_reported_not_swallowed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'managed settings forbid this' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err =
            install_claude_code(&command(), &ClaudeRoute::Cli(fake), false).expect_err("must fail");
        assert!(
            err.to_string().contains("managed settings forbid this"),
            "{err}"
        );
    }

    #[test]
    fn codex_route_adds_the_table_and_keeps_comments_and_other_servers() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "model = \"gpt-5\" # my model\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        install_codex(&command(), &config, false).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("model = \"gpt-5\" # my model"), "{text}");
        let doc: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(doc["mcp_servers"]["other"]["command"].as_str(), Some("x"));
        let entry = &doc["mcp_servers"]["byteask"];
        assert_eq!(
            entry["command"].as_str(),
            Some("/opt/byteask/bin/byteask-engine")
        );
        assert_eq!(entry["args"].as_array().map(Vec::len), Some(2), "{text}");
        assert_eq!(
            entry["env"]["BYTEASK_HOME"].as_str(),
            Some("/home/u/.byteask")
        );
        assert_eq!(entry["tool_timeout_sec"].as_float(), Some(900.0));
    }

    #[test]
    fn codex_route_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        install_codex(&command(), &config, false).unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        install_codex(&command(), &config, false).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), first);
    }

    #[test]
    fn codex_route_refuses_byteasks_own_home() {
        let home = tempfile::tempdir().unwrap();
        let mut cmd = command();
        cmd.byteask_home = home.path().to_path_buf();
        let err =
            install_codex(&cmd, &home.path().join("config.toml"), false).expect_err("must refuse");
        assert!(err.to_string().contains("ByteAsk's own home"), "{err}");
        assert!(!home.path().join("config.toml").exists());
    }

    #[test]
    fn codex_route_refuses_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[broken\n").unwrap();
        let err = install_codex(&command(), &config, false).expect_err("must refuse");
        assert!(err.to_string().contains("left untouched"), "{err}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[broken\n");
    }

    #[test]
    fn hook_entries_are_ours_once_and_leave_everything_else_alone() {
        let mut doc = serde_json::json!({
            "model": "x",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "my-own-stop-hook"}]}]
            }
        });
        merge_hooks(&mut doc, &command(), "claude-code").unwrap();
        merge_hooks(&mut doc, &command(), "claude-code").unwrap();
        assert_eq!(doc["model"], "x");
        for (event, verb, timeout) in HOOK_EVENTS {
            let ours: Vec<&serde_json::Value> = doc["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|group| group["hooks"][0]["command"].as_str().unwrap().contains(HOOK_MARKER))
                .collect();
            assert_eq!(ours.len(), 1, "{event}: installed twice must leave one entry");
            let handler = &ours[0]["hooks"][0];
            assert_eq!(handler["timeout"], timeout);
            let cmd = handler["command"].as_str().unwrap();
            assert!(cmd.contains(&format!("toolbox hook {verb} --client claude-code")), "{cmd}");
            assert!(cmd.starts_with("BYTEASK_HOME='"), "{cmd}");
        }
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"], "my-own-stop-hook",
            "the user's own hook stays, first"
        );
    }

    #[test]
    fn the_stop_hook_waits_longer_than_the_fast_tier() {
        let stop = HOOK_EVENTS.iter().find(|(event, _, _)| *event == "Stop").unwrap();
        assert!(stop.2 > codex_cpp_tools::hooks::FAST_TIER.as_secs(), "the Stop hook outlasts the fast tier");
        let prompt = HOOK_EVENTS.iter().find(|(event, _, _)| *event == "UserPromptSubmit").unwrap();
        assert!(prompt.2 > codex_cpp_tools::hooks::PROMPT_BUDGET.as_secs(), "the prompt hook outlasts its budget");
    }

    #[test]
    fn paths_with_quotes_are_quoted_for_the_shell() {
        let command = ServerCommand {
            engine: PathBuf::from("/opt/it's here/byteask-engine"),
            byteask_home: PathBuf::from("/home/u/.byteask"),
        };
        let cmd = command.hook_command("stop", "codex");
        assert!(cmd.contains("'/opt/it'\\''s here/byteask-engine'"), "{cmd}");
    }

    #[test]
    fn hooks_are_written_to_the_file_and_a_broken_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install_hooks(&command(), &path, "claude-code", false).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc["hooks"]["SessionStart"].is_array());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(install_hooks(&command(), &path, "claude-code", false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
