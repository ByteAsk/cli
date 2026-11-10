//! Only a person may skip a check.
//!
//! The skip command is not a tool, so no agent can call it through MCP; this guard refuses it
//! when it is run from an agent's own shell. Three signals, any one refuses:
//! - the environment markers agents put on the commands they run (Claude Code `CLAUDECODE`,
//!   Codex and the ByteAsk CLI `CODEX_THREAD_ID` / `CODEX_SANDBOX` / `CODEX_CI`);
//! - an agent process among the command's ancestors (catches a marker someone unset);
//! - no terminal on stdin (agents run commands without one; Codex gives commands a PTY, which
//!   is why this alone is not enough).
//!
//! Even a skip that gets past this is only ever NOT RUN (skipped), which the merge check blocks
//! unless a human reviewer approves.

use std::ffi::OsString;

/// Environment variables that mark a command an agent started.
pub const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_THREAD_ID",
    "CODEX_SANDBOX",
    "CODEX_CI",
];

/// Process names of agents (the command's ancestors are compared against these).
pub const AGENT_PROCESSES: &[&str] = &[
    "claude",
    "codex",
    "codex-byteask",
    "byteask-engine",
    "cursor-agent",
    "gemini",
    "aider",
];

/// Why a skip must be refused here, if it must.
pub fn skip_refusal(
    stdin_is_terminal: bool,
    env: impl Fn(&str) -> Option<OsString>,
    ancestors: &[String],
) -> Option<String> {
    if let Some(marker) = AGENT_MARKERS.iter().find(|key| env(key).is_some()) {
        return Some(format!(
            "this runs inside an AI agent's session ({marker} is set), and only a person may \
             skip a check. Run the same command yourself in a terminal."
        ));
    }
    if let Some(agent) = ancestors.iter().find(|name| {
        let name = name.rsplit('/').next().unwrap_or(name);
        AGENT_PROCESSES.contains(&name)
    }) {
        return Some(format!(
            "this was started by an AI agent ({agent}), and only a person may skip a check. \
             Run the same command yourself in a terminal outside the agent."
        ));
    }
    if !stdin_is_terminal {
        return Some(
            "skipping a check needs a person at a terminal. Run the same command yourself in a \
             terminal."
                .to_string(),
        );
    }
    None
}

/// The names of this process's ancestors, nearest first (Linux: /proc; elsewhere: `ps`).
pub fn ancestor_names() -> Vec<String> {
    let mut names = Vec::new();
    let mut pid = parent_of(std::process::id());
    let mut guard = 0;
    while let Some(current) = pid {
        if current <= 1 || guard > 64 {
            break;
        }
        guard += 1;
        if let Some(name) = name_of(current) {
            names.push(name);
        }
        pid = parent_of(current);
    }
    names
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid ...: comm may contain spaces and parentheses, so split after the
    // last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn name_of(pid: u32) -> Option<String> {
    // The executable's file name (comm is cut at 15 bytes: "codex-byteask" fits, longer
    // names would not); fall back to comm.
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
        })
        .map(|name| name.trim_end_matches(" (deleted)").to_string())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps(pid: u32, field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_of(pid: u32) -> Option<u32> {
    ps(pid, "ppid")?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn name_of(pid: u32) -> Option<String> {
    ps(pid, "comm").map(|comm| comm.rsplit('/').next().unwrap_or(&comm).to_string())
}

#[cfg(not(unix))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(unix))]
fn name_of(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_at_a_terminal_may_skip() {
        assert_eq!(
            skip_refusal(true, |_| None, &["bash".into(), "sshd".into()]),
            None
        );
    }

    #[test]
    fn every_agent_marker_refuses() {
        for marker in AGENT_MARKERS {
            let env = move |key: &str| (key == *marker).then(|| OsString::from("1"));
            let why = skip_refusal(true, env, &[]).expect(marker);
            assert!(why.contains(marker), "{why}");
        }
    }

    /// Codex runs commands in a PTY: a terminal is not proof of a person.
    #[test]
    fn an_agent_ancestor_refuses_even_with_a_terminal_and_no_marker() {
        let ancestors = vec!["bash".to_string(), "/usr/local/bin/codex".to_string()];
        let why = skip_refusal(true, |_| None, &ancestors).expect("refused");
        assert!(why.contains("codex"), "{why}");
        let ancestors = vec!["zsh".to_string(), "claude".to_string()];
        assert!(skip_refusal(true, |_| None, &ancestors).is_some());
    }

    #[test]
    fn no_terminal_refuses() {
        assert!(skip_refusal(false, |_| None, &[]).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestors_are_read_from_proc() {
        // The test binary's parent is cargo or a shell; at least one ancestor has a name.
        let names = ancestor_names();
        assert!(!names.is_empty());
    }
}
