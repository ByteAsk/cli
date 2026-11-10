//! `debug_attach` — AI pair-debugging on the user's LIVE gdb.
//!
//! Unlike [`super::GdbSessionTool`] (which owns a headless gdb), this tool
//! attaches to a gdb the *human* is already running. The human arms the bridge
//! in their gdb (`byteask-bridge`, defined by `byteask_gdb_bridge.py`), which
//! opens an owner-only Unix socket under `~/.byteask/gdb-agent/`. This tool is a
//! thin async client of that socket.
//!
//! Safety model (pair mode): the agent drives a *shared* session, so we use an
//! ALLOWLIST — read-only / navigation commands ([`crate::gdb_common::classify`]
//! → `Auto`) run automatically; state-changing / code-running / unrecognized
//! commands are PROPOSED for the human to approve (`approved=true` runs it after
//! they OK). Every command the bridge runs is echoed to the user's terminal.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(unix)]
use std::time::Duration;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::json;

use crate::gdb_common::OUTPUT_CAP;
use crate::gdb_common::Verdict;
use crate::gdb_common::classify;
use crate::gdb_common::truncate;
use crate::report::report;

const TOOL_NAME: &str = "debug_attach";
#[cfg(unix)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Read window: the bridge caps a command at its own 30s post_event wait, so
/// give it a little more before we give up on a reply.
#[cfg(unix)]
const REPLY_TIMEOUT: Duration = Duration::from_secs(35);

/// The gdb session this thread is currently attached to (chosen by `attach`).
/// Read out (cloned) before any `.await`, so no lock is held across a suspend.
pub struct AttachState {
    current: Mutex<Option<Attachment>>,
    /// Commands proposed in the current attachment and not yet run,
    /// whitespace-normalised. `approved` may run only one of these, once.
    proposed: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Attachment {
    session_id: String,
    socket: PathBuf,
}

impl AttachState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(None),
            proposed: Mutex::new(Vec::new()),
        })
    }

    fn remember_proposal(&self, command: &str) {
        if let Ok(mut proposed) = self.proposed.lock() {
            proposed.push(normalize(command));
        }
    }

    /// Consume a proposal of `command`, if one is outstanding.
    fn take_proposal(&self, command: &str) -> bool {
        let wanted = normalize(command);
        let Ok(mut proposed) = self.proposed.lock() else {
            return false;
        };
        match proposed.iter().position(|p| *p == wanted) {
            Some(at) => {
                proposed.remove(at);
                true
            }
            None => false,
        }
    }

    fn get(&self) -> Option<Attachment> {
        self.current.lock().ok().and_then(|g| g.clone())
    }

    fn set(&self, attachment: Attachment) {
        if let Ok(mut g) = self.current.lock() {
            *g = Some(attachment);
        }
        self.forget_proposals();
    }

    fn clear(&self) {
        if let Ok(mut g) = self.current.lock() {
            *g = None;
        }
        self.forget_proposals();
    }

    fn forget_proposals(&self) {
        if let Ok(mut proposed) = self.proposed.lock() {
            proposed.clear();
        }
    }
}

fn normalize(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct GdbAttachTool {
    pub state: Arc<AttachState>,
    /// The session runs with full permission ([`crate::trust::full_access`]): commands that
    /// would be proposed for the user's approval run directly, and the model-facing text says
    /// so instead of asking it to wait for a yes.
    pub full_access: bool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Action {
    Attach,
    Cmd,
    Snapshot,
    History,
    List,
    Interrupt,
    Detach,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachArgs {
    action: Action,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    command: Option<String>,
    /// Set to true ONLY after the user approves a previously-proposed
    /// state-changing command; bypasses the allowlist for that one call.
    #[serde(default)]
    approved: bool,
}

/// A discovered bridge session (from a `~/.byteask/gdb-agent/*.json` file whose
/// socket still exists).
struct Session {
    session_id: String,
    socket: PathBuf,
    pid: u64,
    program: String,
}

impl<'call> ToolExecutor<ToolCall<'call>> for GdbAttachTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("attach"),
                    json!("cmd"),
                    json!("snapshot"),
                    json!("history"),
                    json!("list"),
                    json!("interrupt"),
                    json!("detach"),
                ],
                Some(if self.full_access {
                    "attach: connect to the user's armed gdb (returns state + history); \
                     cmd: run one gdb command (full access is on: every command runs \
                     directly); snapshot: current frame/\
                     backtrace/breakpoints; history: commands the user + you have run; \
                     list: show armed sessions; interrupt: pause a running program; \
                     detach: stop targeting the session (does NOT close the user's gdb)."
                        .to_string()
                } else {
                    "attach: connect to the user's armed gdb (returns state + history); \
                     cmd: run one gdb command (read-only runs automatically; anything \
                     else is proposed for the user to approve); snapshot: current frame/\
                     backtrace/breakpoints; history: commands the user + you have run; \
                     list: show armed sessions; interrupt: pause a running program; \
                     detach: stop targeting the session (does NOT close the user's gdb)."
                        .to_string()
                }),
            ),
        );
        properties.insert(
            "session".to_string(),
            JsonSchema::string(Some(
                "Session id (from attach/list). Optional when exactly one gdb is armed."
                    .to_string(),
            )),
        );
        properties.insert(
            "command".to_string(),
            JsonSchema::string(Some(
                "cmd: the gdb command to run (e.g. \"bt\", \"print x\", \"info locals\")."
                    .to_string(),
            )),
        );
        properties.insert(
            "approved".to_string(),
            JsonSchema::boolean(Some(if self.full_access {
                "cmd: not needed while full access is on (every command runs directly).".to_string()
            } else {
                "cmd: set true ONLY after the user explicitly approves a command you \
                 previously proposed (bypasses the read-only allowlist for that call)."
                    .to_string()
            })),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: if self.full_access {
                "AI pair-debugging on the user's LIVE gdb. The user arms a bridge in \
                their own gdb (you tell them to run `byteask-bridge`), then you attach to help \
                while they watch every command echoed on their terminal — unlike debug_session \
                (which runs its own private gdb) and debug (one-shot). Full access is on: every \
                command runs directly, including ones that change state or run code (call, set, \
                run/continue/step, breakpoints); do not stop to ask the user, and say what you ran. \
                `action=\"attach\"` first; `action=\"interrupt\"` pauses a running program. \
                C/C++ only."
                    .to_string()
            } else {
                "AI pair-debugging on the user's LIVE gdb. The user arms a bridge in \
                their own gdb (you tell them to run `byteask-bridge`), then you attach to help \
                while they watch every command echoed on their terminal — unlike debug_session \
                (which runs its own private gdb) and debug (one-shot). Read-only/navigation \
                commands (bt, info, print of a plain variable, list, frame) run automatically; \
                commands that change state or run code (call, set, run/continue/step, breakpoints, \
                or anything unrecognized) are PROPOSED for the user to approve — relay the proposal \
                and only re-send with approved=true once they say yes. `action=\"attach\"` first; \
                `action=\"interrupt\"` pauses a running program. C/C++ only."
                    .to_string()
            },
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["action".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // A shared, stateful gdb: commands must serialize.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl GdbAttachTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: AttachArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        match args.action {
            Action::Attach => self.attach(args).await,
            Action::Cmd => self.cmd(args).await,
            Action::Snapshot => self.meta(args, "__snapshot__", "snapshot").await,
            Action::History => {
                let target = self.resolve_target(args.session.as_deref())?;
                let reply = send_to_bridge(&target.socket, "__history__")
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                Ok(report(
                    TOOL_NAME,
                    &format!("{} history", target.session_id),
                    keep_tail(reply, OUTPUT_CAP),
                    check_result::CheckResult::pass(
                        TOOL_NAME,
                        "Read the session's recent history.",
                    ),
                ))
            }
            Action::Interrupt => self.meta(args, "__interrupt__", "interrupt").await,
            Action::List => Ok(self.list()),
            Action::Detach => {
                self.state.clear();
                Ok(report(
                    TOOL_NAME,
                    "",
                    "Detached (the user's gdb is untouched).".to_string(),
                    check_result::CheckResult::pass(TOOL_NAME, "Detached from the user's gdb."),
                ))
            }
        }
    }

    async fn attach(&self, args: AttachArgs) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let session = self.select(args.session.as_deref())?;
        self.state.set(Attachment {
            session_id: session.session_id.clone(),
            socket: session.socket.clone(),
        });
        let snapshot = send_to_bridge(&session.socket, "__snapshot__").await;
        let history = send_to_bridge(&session.socket, "__history__").await;
        // Each section gets its own budget. One cap over the whole body
        // keeps its head, so a long snapshot would evict the history entirely,
        // and a long history would lose its NEWEST commands — the ones that matter.
        let snapshot = truncate(
            snapshot.unwrap_or_else(|e| format!("(no state: {e})")),
            OUTPUT_CAP * 5 / 8,
        );
        let history = keep_tail(
            history.unwrap_or_else(|e| format!("(no history: {e})")),
            OUTPUT_CAP * 3 / 8,
        );
        let body = format!(
            "Attached to gdb session {} (pid {}) on {}.\n\n=== CURRENT STATE ===\n{snapshot}\n\n=== SESSION HISTORY ===\n{history}",
            session.session_id, session.pid, session.program,
        );
        Ok(report(
            TOOL_NAME,
            &session.session_id,
            body,
            check_result::CheckResult::pass(TOOL_NAME, "Attached to the user's gdb session."),
        ))
    }

    async fn cmd(&self, args: AttachArgs) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let command = args.command.unwrap_or_default().trim().to_string();
        if command.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "cmd requires a non-empty `command`.".to_string(),
            ));
        }
        let target = self.resolve_target(args.session.as_deref())?;

        // Allowlist gate (pair-mode safety). `approved` bypasses it for one call
        // after the user OKs a proposal — but only for a command that WAS
        // proposed in this attachment. The flag is the model's own word; it
        // used to run any command at all, so one call with `approved: true`
        // skipped the gate the design relies on against a prompt-injected
        // agent. Whether the human really said yes still rests on the model.
        if args.approved {
            if !self.state.take_proposal(&command) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "approved=true runs only a command proposed earlier in this attachment, once, \
                     after the user agrees; `{command}` has not been proposed. Send it without \
                     `approved` first and show the proposal to the user."
                )));
            }
        } else if self.full_access {
            // Full permission: nothing waits for the user's yes. The command runs, and the
            // answer says that it changed state without asking.
            if let Verdict::Propose(reason) = classify(&command) {
                let reply = send_to_bridge(&target.socket, &command)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                let label = format!("{} {}", target.session_id, first_token(&command));
                let result = cut_or_pass(reply.len(), "The command ran in the user's gdb.");
                let body = format!(
                    "Ran without asking (full access is on): `{command}` ({reason}).\n{}",
                    truncate(reply, OUTPUT_CAP)
                );
                return Ok(report(TOOL_NAME, &label, body, result));
            }
        } else {
            if let Verdict::Propose(reason) = classify(&command) {
                self.state.remember_proposal(&command);
                let body = format!(
                    "Proposed (needs your approval): `{command}`\n\
                     Reason: {reason}.\n\
                     I did NOT run it. Run it yourself in gdb, or tell me to proceed \
                     (I'll re-issue it as approved)."
                );
                let result = check_result::CheckResult::not_run(
                    TOOL_NAME,
                    check_result::NotRunReason::NeedsApproval,
                    "The command can change the program's state, so it was proposed and not run.",
                    "show the proposal to the user; if they agree, send the same command with approved: true.",
                );
                return Ok(report(
                    TOOL_NAME,
                    &format!("{} propose", target.session_id),
                    body,
                    result,
                ));
            }
        }

        let reply = send_to_bridge(&target.socket, &command)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let label = format!("{} {}", target.session_id, first_token(&command));
        let result = cut_or_pass(reply.len(), "The command ran in the user's gdb.");
        Ok(report(
            TOOL_NAME,
            &label,
            truncate(reply, OUTPUT_CAP),
            result,
        ))
    }

    async fn meta(
        &self,
        args: AttachArgs,
        message: &str,
        label: &str,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let target = self.resolve_target(args.session.as_deref())?;
        let reply = send_to_bridge(&target.socket, message)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let result = cut_or_pass(reply.len(), "The user's gdb answered.");
        Ok(report(
            TOOL_NAME,
            &format!("{} {label}", target.session_id),
            truncate(reply, OUTPUT_CAP),
            result,
        ))
    }

    fn list(&self) -> Box<dyn ToolOutput> {
        let sessions = discover();
        let body = if sessions.is_empty() {
            no_bridge_message()
        } else {
            let mut out = String::from("Armed gdb sessions (ask the user which, if unsure):");
            for s in &sessions {
                out.push_str(&format!(
                    "\n  session {}  (pid {})  {}",
                    s.session_id,
                    s.pid,
                    s.program.lines().next().unwrap_or("").trim()
                ));
            }
            out
        };
        report(
            TOOL_NAME,
            "",
            body,
            check_result::CheckResult::pass(TOOL_NAME, "Listed the armed gdb sessions."),
        )
    }

    /// Pick a session for `attach`: explicit id, else the only one, else error
    /// listing choices.
    fn select(&self, wanted: Option<&str>) -> Result<Session, FunctionCallError> {
        let mut sessions = discover();
        if sessions.is_empty() {
            return Err(FunctionCallError::RespondToModel(no_bridge_message()));
        }
        if let Some(id) = wanted {
            if let Some(pos) = sessions.iter().position(|s| s.session_id == id) {
                return Ok(sessions.swap_remove(pos));
            }
            return Err(FunctionCallError::RespondToModel(format!(
                "No armed gdb session with id `{id}`. Use action=\"list\" to see them."
            )));
        }
        if sessions.len() == 1 {
            return Ok(sessions.swap_remove(0));
        }
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        if self.full_access {
            return Err(FunctionCallError::RespondToModel(format!(
                "Multiple gdb sessions are armed ({}). Pick the one that fits the task \
                 (action=\"list\" shows each program) and pass `session`.",
                ids.join(", ")
            )));
        }
        Err(FunctionCallError::RespondToModel(format!(
            "Multiple gdb sessions are armed ({}). Ask the user which, then pass `session`.",
            ids.join(", ")
        )))
    }

    /// Resolve the target for a non-attach action: explicit id, else the stored
    /// attachment, else the only armed session.
    fn resolve_target(&self, wanted: Option<&str>) -> Result<Attachment, FunctionCallError> {
        if let Some(id) = wanted {
            let session = self.select(Some(id))?;
            return Ok(Attachment {
                session_id: session.session_id,
                socket: session.socket,
            });
        }
        if let Some(existing) = self.state.get() {
            // Confirm it's still alive; otherwise fall through to rediscovery.
            if existing.socket.exists() {
                return Ok(existing);
            }
        }
        let session = self.select(None)?;
        let attachment = Attachment {
            session_id: session.session_id,
            socket: session.socket,
        };
        self.state.set(attachment.clone());
        Ok(attachment)
    }
}

/// First whitespace token, for labeling.
fn first_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

fn no_bridge_message() -> String {
    "No armed gdb bridge found. Ask the user to run this in their gdb:\n  \
     (gdb) source ~/.byteask/byteask_gdb_bridge.py   (skip if auto-loaded)\n  \
     (gdb) byteask-bridge\nThen retry attach."
        .to_string()
}

/// `~/.byteask/gdb-agent` (honoring `$BYTEASK_HOME`).
fn agent_dir() -> Option<PathBuf> {
    let home = std::env::var("BYTEASK_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".byteask"))
        })?;
    Some(home.join("gdb-agent"))
}

/// Discover armed sessions under the real agent dir (honoring `$BYTEASK_HOME`).
fn discover() -> Vec<Session> {
    match agent_dir() {
        Some(dir) => discover_in(&dir),
        None => Vec::new(),
    }
}

/// Discover armed sessions in `dir`: parse each `*.json`, keep those whose
/// Is the gdb that armed this bridge still running?
///
/// ⚠️ `socket.exists()` is NOT a liveness test, though the old code used it as
/// one. An AF_UNIX socket file is an ordinary directory entry that outlives its
/// process; the bridge unlinks it only in `byteask-bridge-off`, and nothing
/// hooks gdb exit. Measured: after the owning process is killed the file is
/// still there, while `connect` gives ECONNREFUSED. So every session ever armed
/// stayed "armed" forever — `~/.byteask/gdb-agent/` accumulated one pair per
/// session, `attach` started asking the user to choose between corpses, and
/// with exactly one stale entry it attached to a dead one. That gets worse the
/// more the tool is used.
///
/// `kill(pid, 0)` sends no signal; it only asks the kernel whether the pid is
/// there. `EPERM` means it exists under another uid, which is still alive.
fn pid_is_alive(pid: u64) -> bool {
    // A descriptor written by a bridge that recorded no pid: fall back to the
    // old behaviour rather than hiding a session that may well be live.
    if pid == 0 {
        return true;
    }
    // `kill` does not exist on Windows (E0425 broke the 0.1.13 Windows build). The bridge
    // is an AF_UNIX socket armed from inside a Unix gdb, so off Unix keep the old behaviour.
    #[cfg(not(unix))]
    {
        true
    }
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return true;
        };
        // SAFETY: signal 0 performs only the existence/permission check.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// socket exists, ordered by session id for stable output.
fn discover_in(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let socket = value
            .get("socket")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let Some(socket) = socket else { continue };
        if !socket.exists() {
            continue; // the descriptor outlived its socket — skip
        }
        let pid = value.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
        if !pid_is_alive(pid) {
            continue; // gdb is gone; the socket file left behind is just litter
        }
        sessions.push(Session {
            session_id: value
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            socket,
            pid,
            program: value
                .get("program")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    sessions
}

/// On Windows there is no tokio `UnixStream` (gdb pair-debugging uses a
/// Unix-domain socket), so the tool still registers for a uniform tool list but
/// reports that it is unsupported there.
#[cfg(not(unix))]
async fn send_to_bridge(_socket: &Path, _message: &str) -> Result<String, String> {
    Err(
        "debug_attach (gdb pair-debugging over a Unix socket) is only available on Linux/macOS."
            .to_string(),
    )
}

/// Send one line to the bridge and read its reply. Framing mirrors the Python
/// server: write `message\n`, half-close the write side, read to EOF.
#[cfg(unix)]
async fn send_to_bridge(socket: &Path, message: &str) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::timeout;
    let mut stream = match timeout(CONNECT_TIMEOUT, UnixStream::connect(socket)).await {
        Err(_) => return Err("timed out connecting to the gdb bridge".to_string()),
        Ok(Err(err)) => {
            return Err(format!(
                "gdb bridge not reachable ({err}); is `byteask-bridge` still armed in gdb?"
            ));
        }
        Ok(Ok(stream)) => stream,
    };
    stream
        .write_all(message.as_bytes())
        .await
        .map_err(|e| format!("write to bridge failed: {e}"))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|e| format!("write to bridge failed: {e}"))?;
    // Half-close the write side so the server sees end-of-request.
    let _ = stream.shutdown().await;

    let mut buf = Vec::new();
    match timeout(REPLY_TIMEOUT, stream.read_to_end(&mut buf)).await {
        // Not "did not run": the bridge may have handed the command to gdb,
        // which can still execute it (an older bridge queues it with no way to
        // cancel). Re-sending a state change could apply it twice.
        Err(_) => Err(
            "the gdb bridge did not reply in time. The command MAY STILL RUN in gdb — do NOT \
             re-send it; check `history` and the program state first."
                .to_string(),
        ),
        Ok(Err(err)) => Err(format!("read from bridge failed: {err}")),
        Ok(Ok(_)) => Ok(String::from_utf8_lossy(&buf).trim().to_string()),
    }
}

/// Cap `text` at `cap` bytes keeping its END (the newest history lines), cut
/// at a line boundary, with a count of the lines left out.
fn keep_tail(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let start = text[start..].find('\n').map_or(start, |nl| start + nl + 1);
    let omitted = text[..start].lines().count();
    format!("({omitted} older lines omitted)\n{}", &text[start..])
}

/// A reply longer than the tool shows is cut, so it is not the whole answer.
fn cut_or_pass(len: usize, summary: &str) -> check_result::CheckResult {
    if len > OUTPUT_CAP {
        check_result::CheckResult::approximate(
            TOOL_NAME,
            "gdb's reply was longer than this tool shows, so it was cut.",
        )
    } else {
        check_result::CheckResult::pass(TOOL_NAME, summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn keep_tail_keeps_the_newest_lines() {
        let text: String = (0..500).map(|i| format!("entry {i:04}\n")).collect();
        let out = keep_tail(text, 200);
        assert!(out.contains("entry 0499"), "{out}");
        assert!(!out.contains("entry 0000"), "{out}");
        assert!(
            out.starts_with('(') && out.contains("older lines omitted"),
            "{out}"
        );
        assert_eq!(keep_tail("short".to_string(), 200), "short");
    }

    #[test]
    fn a_misnamed_field_is_an_error_not_silently_ignored() {
        // A field outside the schema used to be dropped without a word.
        let err =
            serde_json::from_str::<AttachArgs>(r#"{"action":"cmd","command":"bt","approve":true}"#)
                .err()
                .expect("unknown field refused");
        assert!(err.to_string().contains("unknown field `approve`"), "{err}");
    }

    #[test]
    fn first_token_labels() {
        assert_eq!(first_token("  info locals"), "info");
        assert_eq!(first_token(""), "");
    }

    #[test]
    fn attach_state_set_get_clear() {
        let st = AttachState::new();
        assert!(st.get().is_none());
        st.set(Attachment {
            session_id: "abc123".to_string(),
            socket: PathBuf::from("/tmp/x.sock"),
        });
        assert_eq!(st.get().unwrap().session_id, "abc123");
        st.clear();
        assert!(st.get().is_none());
    }

    #[test]
    fn discover_in_empty_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_in(dir.path()).is_empty());
    }

    #[test]
    fn discover_in_reads_valid_session_and_skips_stale() {
        let agent = tempfile::tempdir().unwrap();
        // A live session: socket file present.
        let live_sock = agent.path().join("gdb-1-aa.sock");
        std::fs::write(&live_sock, b"").unwrap();
        std::fs::write(
            agent.path().join("gdb-1-aa.json"),
            serde_json::to_vec(&json!({
                "session_id": "aa", "pid": 1, "socket": live_sock, "program": "demo"
            }))
            .unwrap(),
        )
        .unwrap();
        // A stale session: socket path does not exist.
        std::fs::write(
            agent.path().join("gdb-2-bb.json"),
            serde_json::to_vec(&json!({
                "session_id": "bb", "pid": 2, "socket": agent.path().join("missing.sock"), "program": "x"
            }))
            .unwrap(),
        )
        .unwrap();

        let found = discover_in(agent.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "aa");
        assert_eq!(found[0].program, "demo");
    }

    /// CATCHES the stale-session leak: a gdb that has exited leaves its socket
    /// FILE behind (the bridge unlinks it only in `byteask-bridge-off`, and
    /// nothing hooks gdb exit), so the old `socket.exists()` check reported
    /// every session ever armed as still live. In real use that meant
    /// `~/.byteask/gdb-agent/` grew one pair per session until `attach` asked
    /// the user to choose between dead sessions — and with exactly one stale
    /// entry it attached to the corpse and failed with "not reachable".
    ///
    /// NON-VACUITY: the socket file here DOES exist, so the only thing that can
    /// exclude this session is the pid check. Reverting `discover_in` to the
    /// old `socket.exists()`-only form makes this assert 1 instead of 0.
    #[cfg(unix)]
    #[test]
    fn discover_in_skips_a_session_whose_gdb_has_exited() {
        // A pid that is genuinely gone: spawn, reap, reuse its number.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn /bin/true");
        let dead_pid = u64::from(child.id());
        child.wait().expect("reap");
        assert!(!pid_is_alive(dead_pid), "pid {dead_pid} should be gone");

        let agent = tempfile::tempdir().unwrap();
        let sock = agent.path().join("gdb-9-zz.sock");
        std::fs::write(&sock, b"").unwrap(); // the file the dead gdb left behind
        std::fs::write(
            agent.path().join("gdb-9-zz.json"),
            serde_json::to_vec(&json!({
                "session_id": "zz", "pid": dead_pid, "socket": sock, "program": "gone"
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            discover_in(agent.path()).is_empty(),
            "a session whose gdb has exited must not be offered for attach"
        );
    }

    /// An unreadable pid (0, from an older bridge that recorded none) must not
    /// hide a session that may be live — the check fails OPEN, like the rest of
    /// this crate's environment probes.
    #[cfg(unix)]
    #[test]
    fn a_missing_pid_is_treated_as_alive() {
        assert!(pid_is_alive(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_to_bridge_round_trips() {
        // Verify the client framing (write `cmd\n`, half-close, read to EOF)
        // against a minimal echo server — the same framing the Python bridge uses.
        use tokio::io::AsyncBufReadExt;
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = conn.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            assert_eq!(line.trim(), "bt");
            wr.write_all(b"#0 add (a=41)\n").await.unwrap();
            // wr dropped here -> client's read_to_end sees EOF
        });
        let reply = send_to_bridge(&sock, "bt").await.unwrap();
        assert_eq!(reply, "#0 add (a=41)");
        server.await.unwrap();
    }

    /// CATCHES: the request framing drifting from what `send_to_bridge`'s own
    /// doc comment promises — in particular a dropped `\n` terminator.
    ///
    /// Nothing else can. `send_to_bridge_round_trips` says it verifies the
    /// framing, but its `read_line` also returns at EOF and the client
    /// half-closes, so it reads `bt` with or without the newline. The real
    /// Python bridge is just as tolerant (`_handle` breaks its read loop on an
    /// empty `recv` as well as on `\n`) — measured against a live gdb, every
    /// verb answers identically either way. So the only way to hold the wire
    /// contract is to look at the bytes.
    #[cfg(unix)]
    #[tokio::test]
    async fn send_to_bridge_writes_one_newline_terminated_line() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.unwrap().0.into_split();
            let mut raw = Vec::new();
            // Reading to EOF only returns because the client half-closed its
            // write side, so this pins that half of the contract as well.
            tokio::time::timeout(Duration::from_secs(10), rd.read_to_end(&mut raw))
                .await
                .expect("client never half-closed its write side")
                .unwrap();
            wr.write_all(b"ok\n").await.unwrap();
            raw
        });
        assert_eq!(send_to_bridge(&sock, "info locals").await.unwrap(), "ok");
        assert_eq!(
            String::from_utf8(server.await.unwrap()).unwrap(),
            "info locals\n",
            "the request must be exactly one newline-terminated line"
        );
    }

    #[tokio::test]
    async fn send_to_bridge_errors_when_socket_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.sock");
        assert!(send_to_bridge(&missing, "bt").await.is_err());
    }

    // ───────────────────── live bridge, against a REAL gdb ──────────────────
    //
    // Everything above this line talks to our own mock, so it can only confirm
    // assumptions we already made. The two tests below start a real gdb, arm the
    // real `byteask_gdb_bridge.py` inside it, and drive the actual socket.

    /// A real gdb, stopped inside `byteask_add`, with the shipped bridge armed
    /// in a private `$BYTEASK_HOME`. `Drop` kills gdb and removes the home — on
    /// a panic too, so a failing test can never leak a gdb onto the machine.
    #[cfg(unix)]
    struct LiveGdb {
        child: std::process::Child,
        /// Removed after `Drop` has killed gdb (our `drop` body runs before the
        /// fields'), which is what takes the socket + descriptor with it.
        _home: tempfile::TempDir,
        agent: PathBuf,
        log: PathBuf,
        socket: PathBuf,
        session_id: String,
    }

    #[cfg(unix)]
    impl LiveGdb {
        /// gdb's own transcript (every `[byteask] (gdb) …` echo lands here).
        /// Folded into assertion messages so an arming failure is diagnosable.
        fn transcript(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for LiveGdb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Point the JSONL tool log at a throwaway file so driving the real tool in
    /// a test never appends to the developer's `~/.byteask/logs`. Both live
    /// tests write the SAME path, so running them in parallel is not a race.
    #[cfg(unix)]
    fn isolate_tool_log() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("byteask-attach-test-{}.jsonl", std::process::id()));
        // SAFETY: the only value any test ever sets is this one fixed per-process
        // temp path, so concurrent setters cannot disagree.
        unsafe { std::env::set_var("BYTEASK_TOOL_LOG", &path) };
        path
    }

    /// Compile a tiny program, start a real gdb stopped inside `byteask_add`,
    /// and arm the shipped bridge in a private home.
    ///
    /// `None` means this machine cannot run the test (no C compiler, no gdb, or
    /// a gdb built without Python) and the caller returns. A machine that *has*
    /// python-enabled gdb and still fails to arm is a real defect, so that path
    /// panics instead of skipping.
    #[cfg(unix)]
    async fn start_live_gdb(test: &str) -> Option<LiveGdb> {
        let Some(cc) = ["cc", "gcc", "clang"].into_iter().find(|c| {
            std::process::Command::new(c)
                .arg("--version")
                .output()
                .is_ok()
        }) else {
            eprintln!("skip: no C compiler");
            return None;
        };
        // `-nx` everywhere: a developer's own ~/.gdbinit must not perturb this.
        match std::process::Command::new("gdb")
            .args([
                "--batch",
                "-nx",
                "-ex",
                "python print('byteask-gdb-python-ok')",
            ])
            .output()
        {
            Err(err) => {
                eprintln!("skip: gdb unavailable ({err})");
                return None;
            }
            Ok(out) if !String::from_utf8_lossy(&out.stdout).contains("byteask-gdb-python-ok") => {
                eprintln!("skip: this gdb has no Python (the bridge is a gdb Python script)");
                return None;
            }
            Ok(_) => {}
        }

        let home = tempfile::Builder::new()
            .prefix(&format!("byteask-attach-{test}-{}-", std::process::id()))
            .tempdir()
            .unwrap();
        // Exactly what the engine does on startup, so this drives the bytes we
        // actually ship rather than a copy that happens to sit in the tree.
        crate::helper::materialize_helper(home.path());
        let bridge = home.path().join("byteask_gdb_bridge.py");
        assert!(bridge.exists(), "materialize_helper wrote no bridge script");

        let src = home.path().join("demo.c");
        let bin = home.path().join("demo");
        std::fs::write(
            &src,
            "int byteask_marker = 424242;\n\
             int byteask_scratch = 111;\n\
             int byteask_add(int a,int b){ return a+b; }\n\
             int main(void){ return byteask_add(byteask_marker,0) - byteask_marker; }\n",
        )
        .unwrap();
        let compiled = std::process::Command::new(cc)
            .args(["-g", "-O0", "-o"])
            .arg(&bin)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(compiled, "compile demo.c");

        // gdb's transcript goes to a FILE, not a pipe: the bridge echoes every
        // command it runs to gdb's stdout, and a pipe nobody drains would wedge
        // gdb once that output filled the kernel buffer.
        let log = home.path().join("gdb-session.log");
        let out = std::fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = std::process::Command::new("gdb")
            .args([
                "-q",
                "-nx",
                // ⚠️ LANDMINE: pagination MUST go off before `byteask-bridge`.
                // If gdb's pager engages while the bridge is serving, the bridge
                // freezes — and interrupting the pager aborts gdb outright.
                "-ex",
                "set pagination off",
                "-ex",
                "set confirm off",
            ])
            .arg("-ex")
            .arg(format!("source {}", bridge.display()))
            // Stop inside a real frame, so a state-changing command is something
            // this gdb could genuinely carry out (see the positive control in
            // `live_bridge_proposes_state_changing_commands`). Arming last means
            // the run/stop already happened and the bridge starts at "stopped".
            .args([
                "-ex",
                "break byteask_add",
                "-ex",
                "run",
                "-ex",
                "byteask-bridge",
            ])
            .arg(&bin)
            // Keep gdb's stdin an open pipe we never write to and never close:
            // gdb then sits at its prompt servicing the bridge thread's posted
            // events. Let it see EOF and it quits out from under the test.
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .env("BYTEASK_HOME", home.path())
            // The bridge refuses to arm unless gdb's fd 0 is a tty; this is its
            // own documented test hook for exactly that.
            .env("BYTEASK_BRIDGE_FORCE", "1")
            .spawn()
            .expect("spawn gdb");

        let agent = home.path().join("gdb-agent");
        // Built immediately so every panic below still runs `Drop` and reaps gdb.
        let mut live = LiveGdb {
            child,
            _home: home,
            agent: agent.clone(),
            log,
            socket: PathBuf::new(),
            session_id: String::new(),
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(session) = discover_in(&agent).pop() {
                live.socket = session.socket;
                live.session_id = session.session_id;
                return Some(live);
            }
            if let Ok(Some(status)) = live.child.try_wait() {
                panic!(
                    "gdb exited ({status}) before the bridge armed — transcript:\n{}",
                    live.transcript()
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the bridge never armed in {} within 20s — transcript:\n{}",
                agent.display(),
                live.transcript()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The real tool, pre-pointed at `live`'s socket. Bypasses `discover()` on
    /// purpose: that reads the *developer's* `$HOME/.byteask/gdb-agent`, and a
    /// test must never touch it. `resolve_target` still runs for real.
    #[cfg(unix)]
    fn tool_attached_to(live: &LiveGdb) -> GdbAttachTool {
        let tool = GdbAttachTool {
            state: AttachState::new(),
            full_access: false,
        };
        tool.state.set(Attachment {
            session_id: live.session_id.clone(),
            socket: live.socket.clone(),
        });
        tool
    }

    #[cfg(unix)]
    fn cmd_args(command: &str, approved: bool) -> AttachArgs {
        AttachArgs {
            action: Action::Cmd,
            session: None,
            command: Some(command.to_string()),
            approved,
        }
    }

    /// Wait (bounded) for `needle` to reach gdb's transcript. gdb block-buffers
    /// its stdout when it is a file and flushes around its prompt, which can
    /// land just *after* the bridge has already answered us — so reading the log
    /// the instant a reply arrives is a race. Polling makes it deterministic.
    #[cfg(unix)]
    async fn await_transcript(live: &LiveGdb, needle: &str) {
        for _ in 0..50 {
            if live.transcript().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "gdb never echoed {needle:?} to the user within 5s — transcript:\n{}",
            live.transcript()
        );
    }

    /// Serialize a tool result so a test can assert on its body text (mirrors
    /// the `report.rs` / `compile_db.rs` approach).
    #[cfg(unix)]
    fn output_text(out: &dyn ToolOutput) -> String {
        let item = out.to_response_item(
            "call-test",
            &codex_extension_api::ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        serde_json::to_string(&item).expect("serialize response item")
    }

    /// CATCHES: any break in the real socket path — the bridge failing to arm
    /// under `$BYTEASK_HOME`, `discover_in` not finding or not parsing the
    /// descriptor the Python actually writes, `send_to_bridge`'s framing
    /// (write, half-close, read to EOF) drifting from the Python server's, or
    /// `cmd`'s `Auto` branch not reaching gdb. The echo-server test above cannot
    /// catch any of them: both halves of it are ours.
    ///
    /// NON-VACUITY: every assertion is on a value gdb computed. `424242` and
    /// `byteask_add` appear in no request we send, so they can only come back
    /// from gdb reading the program's DWARF and memory. An empty reply, a bridge
    /// that never armed, or a gdb that exited all fail rather than pass.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_bridge_round_trips_against_real_gdb() {
        let log = isolate_tool_log();
        let Some(live) = start_live_gdb("roundtrip").await else {
            return;
        };

        // 1. Discovery sees exactly OUR gdb — pid-matched, not just "a session".
        let sessions = discover_in(&live.agent);
        assert_eq!(
            sessions.len(),
            1,
            "expected one armed session in {:?}",
            live.agent
        );
        assert_eq!(sessions[0].session_id, live.session_id);
        assert_eq!(sessions[0].pid, u64::from(live.child.id()));
        assert!(
            sessions[0].program.contains("demo"),
            "the descriptor should carry `info inferiors`: {:?}",
            sessions[0].program
        );

        // 2. A read-only command really executes: gdb reads the global's value.
        let value = send_to_bridge(&live.socket, "print byteask_marker")
            .await
            .unwrap_or_else(|e| panic!("print over the real bridge: {e}"));
        assert!(
            value.trim().ends_with("= 424242"),
            "gdb should have printed the global's value, got {value:?}"
        );

        // 3. ... on a LIVE inferior stopped in our frame, not a static symbol
        //    read: the backtrace names both frames with the argument gdb read
        //    off the stopped process.
        let bt = send_to_bridge(&live.socket, "bt")
            .await
            .unwrap_or_else(|e| panic!("bt over the real bridge: {e}"));
        assert!(
            bt.contains("byteask_add") && bt.contains("main") && bt.contains("424242"),
            "backtrace should show the stopped frame, got {bt:?}"
        );

        // 4. The bridge's own meta verbs answer, and the snapshot `attach` shows
        //    the model is built from real gdb output.
        assert_eq!(
            send_to_bridge(&live.socket, "__runstate__").await.unwrap(),
            "stopped"
        );
        let snapshot = send_to_bridge(&live.socket, "__snapshot__")
            .await
            .unwrap_or_else(|e| panic!("__snapshot__ over the real bridge: {e}"));
        assert!(
            snapshot.contains("[backtrace]") && snapshot.contains("byteask_add"),
            "snapshot should carry the real backtrace, got {snapshot:?}"
        );

        // 5. The production path: `cmd` classifies `print` as Auto and delivers
        //    it to the same gdb, labelled with the session.
        let tool = tool_attached_to(&live);
        let out = tool
            .cmd(cmd_args("print byteask_marker", false))
            .await
            .expect("cmd should succeed against a live bridge");
        let text = output_text(out.as_ref());
        assert!(
            text.contains("424242"),
            "cmd must return gdb's answer, got {text}"
        );
        assert!(
            text.contains(&live.session_id),
            "cmd should label the result with the session id, got {text}"
        );
        assert!(
            !text.contains("Proposed"),
            "a plain `print` must not be gated, got {text}"
        );

        // 6. The user saw every command we ran (the pair-mode transparency
        //    promise), echoed on their own terminal.
        await_transcript(&live, "[byteask] (gdb) print byteask_marker").await;

        let _ = std::fs::remove_file(&log);
    }

    /// CATCHES: the pair-mode allowlist failing open — `cmd` handing a
    /// state-changing command to the human's live gdb without their approval.
    ///
    /// NON-VACUITY, four ways. (a) A POSITIVE CONTROL first writes a sibling
    /// global through the same socket and reads the new value back, so this gdb
    /// provably does honour `set var` right now — "unchanged" is therefore a
    /// real negative, not a command gdb could never have carried out anyway.
    /// (b) The gated variable is read back off the live process afterwards and
    /// must still be `424242`. (c) The bridge's own `__history__` (which it
    /// appends to *before* running anything) must not mention the proposed
    /// command at all, while still showing the control — so the bridge never
    /// even received it. (d) Re-sending it with `approved=true` must actually
    /// change the live process, otherwise (b)/(c) could be passing because
    /// `cmd` is broken for every command rather than because the gate works.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_bridge_proposes_state_changing_commands() {
        let log = isolate_tool_log();
        let Some(live) = start_live_gdb("propose").await else {
            return;
        };
        let tool = tool_attached_to(&live);

        // (a) Positive control, on a variable nothing else in this test touches.
        let wrote = send_to_bridge(&live.socket, "set var byteask_scratch = 7")
            .await
            .unwrap_or_else(|e| panic!("positive control over the real bridge: {e}"));
        assert!(
            !wrote.starts_with("error:"),
            "this gdb must be able to write memory for the control to mean anything: {wrote:?}"
        );
        let scratch = send_to_bridge(&live.socket, "print byteask_scratch")
            .await
            .unwrap();
        assert!(
            scratch.trim().ends_with("= 7"),
            "positive control: `set var` must really take effect here, got {scratch:?}"
        );

        // (b) The gate. Same shape of command, through the real tool. The value
        //     99 is used nowhere else, so it identifies this command uniquely.
        let out = tool
            .cmd(cmd_args("set var byteask_marker = 99", false))
            .await
            .expect("a proposal is a normal result, not an error");
        let text = output_text(out.as_ref());
        assert!(
            text.contains("Proposed (needs your approval)"),
            "a state change must be proposed, got {text}"
        );
        assert!(
            text.contains("I did NOT run it"),
            "the proposal must say plainly that it was not run, got {text}"
        );

        // ... and the live process still holds the old value.
        let marker = send_to_bridge(&live.socket, "print byteask_marker")
            .await
            .unwrap();
        assert!(
            marker.trim().ends_with("= 424242"),
            "the proposed command must NOT have run, got {marker:?}"
        );

        // (c) The bridge never even received it. Read straight out of the
        //     bridge's own history, which it appends to before it runs anything
        //     — unlike gdb's buffered transcript, this cannot race us.
        let history = send_to_bridge(&live.socket, "__history__").await.unwrap();
        assert!(
            history.contains("set var byteask_scratch = 7"),
            "history must show the control, or its silence proves nothing: {history}"
        );
        assert!(
            !history.contains("byteask_marker = 99"),
            "the proposed command reached the bridge: {history}"
        );

        // (d) Approving a command that was never proposed is refused: the
        //     flag is the model's own word, not proof the human agreed.
        let refused = tool.cmd(cmd_args("set var byteask_marker = 7", true)).await;
        assert!(refused.is_err(), "an unproposed command must not run");
        let marker = send_to_bridge(&live.socket, "print byteask_marker")
            .await
            .unwrap();
        assert!(!marker.trim().ends_with("= 7"), "{marker:?}");

        // (e) With the user's approval the proposed command goes through — so
        //     the checks above are the gate working, not `cmd` being broken.
        let out = tool
            .cmd(cmd_args("set var  byteask_marker = 99", true))
            .await
            .expect("approved cmd should reach the bridge");
        let text = output_text(out.as_ref());
        assert!(
            !text.contains("Proposed"),
            "approved=true must bypass the gate, got {text}"
        );
        let marker = send_to_bridge(&live.socket, "print byteask_marker")
            .await
            .unwrap();
        assert!(
            marker.trim().ends_with("= 99"),
            "approved=true must actually change the live process, got {marker:?}"
        );
        // A proposal is used once.
        assert!(
            tool.cmd(cmd_args("set var byteask_marker = 99", true))
                .await
                .is_err()
        );

        let _ = std::fs::remove_file(&log);
    }

    /// With full permission nothing waits for the user's yes. The same
    /// state-changing command runs directly (no proposal, no `approved` round trip) and the
    /// answer says it ran without asking; the live process really changed.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_bridge_runs_state_changing_commands_under_full_access() {
        let log = isolate_tool_log();
        let Some(live) = start_live_gdb("fullaccess").await else {
            return;
        };
        let mut tool = tool_attached_to(&live);
        tool.full_access = true;
        let out = tool
            .cmd(cmd_args("set var byteask_marker = 77", false))
            .await
            .expect("runs directly");
        let text = output_text(out.as_ref());
        assert!(!text.contains("Proposed"), "{text}");
        assert!(
            text.contains("Ran without asking (full access is on)"),
            "{text}"
        );
        let marker = send_to_bridge(&live.socket, "print byteask_marker")
            .await
            .unwrap();
        assert!(
            marker.trim().ends_with("= 77"),
            "the command must have run: {marker:?}"
        );
        let _ = std::fs::remove_file(&log);
    }

    /// The model-facing text follows the mode: the default asks to propose and wait, full
    /// access says every command runs directly and never mentions waiting for approval.
    #[test]
    fn the_spec_says_propose_by_default_and_run_directly_under_full_access() {
        let text = |full_access: bool| {
            let tool = GdbAttachTool {
                state: AttachState::new(),
                full_access,
            };
            serde_json::to_string(&<GdbAttachTool as ToolExecutor<ToolCall<'_>>>::spec(&tool))
                .expect("spec")
        };
        let normal = text(false);
        assert!(
            normal.contains("PROPOSED for the user to approve"),
            "{normal}"
        );
        let full = text(true);
        assert!(
            !full.contains("PROPOSED") && !full.contains("approve a command"),
            "{full}"
        );
        assert!(full.contains("do not stop to ask the user"), "{full}");
    }

    /// CATCHES: the bridge never learning that the program EXITED. A program
    /// that runs to its end fires gdb's `cont` event and then `exited` — never
    /// `stop` (measured on gdb 13.1) — and the bridge listened only for `stop`.
    /// So after the program finished, the bridge said "running" forever: `bt`,
    /// `print $_exitcode` and the snapshot were all refused with "Program is
    /// running", and `interrupt` signalled a pid that no longer existed
    /// ("interrupt failed: [Errno 3] No such process").
    ///
    /// NON-VACUITY: the bridge reads "stopped" at the breakpoint first, and
    /// gdb's own transcript must show the exit, so the program really ran to its
    /// end in between. `$_exitcode` is a value only gdb can supply, and only
    /// after an exit (it is `void` before one). `bt` goes through the real tool,
    /// so the model-facing answer is checked, not just the bridge's.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_bridge_sees_the_program_exit() {
        let log = isolate_tool_log();
        let Some(live) = start_live_gdb("exited").await else {
            return;
        };
        assert_eq!(
            send_to_bridge(&live.socket, "__runstate__").await.unwrap(),
            "stopped"
        );

        // Let the program run to its end: `main` returns 0 right after the
        // breakpoint. Sent straight to the socket, as the positive control in
        // `live_bridge_proposes_state_changing_commands` does.
        send_to_bridge(&live.socket, "continue")
            .await
            .unwrap_or_else(|e| panic!("continue over the real bridge: {e}"));
        await_transcript(&live, "exited normally").await;

        let mut runstate = String::new();
        for _ in 0..50 {
            runstate = send_to_bridge(&live.socket, "__runstate__").await.unwrap();
            if runstate == "stopped" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            runstate, "stopped",
            "the program has exited, so the bridge must not report it running"
        );

        let exit_code = send_to_bridge(&live.socket, "print $_exitcode")
            .await
            .unwrap();
        assert!(
            exit_code.trim().ends_with("= 0"),
            "gdb should report the exit code, got {exit_code:?}"
        );
        let snapshot = send_to_bridge(&live.socket, "__snapshot__").await.unwrap();
        assert!(
            snapshot.contains("[inferior]") && !snapshot.contains("Program is running"),
            "the snapshot must describe the exited program, got {snapshot:?}"
        );

        // The production path: the model asks for a backtrace and hears gdb's
        // own answer (there is no stack), not a claim that the program runs.
        let tool = tool_attached_to(&live);
        let out = tool
            .cmd(cmd_args("bt", false))
            .await
            .expect("bt should reach the bridge");
        let text = output_text(out.as_ref());
        assert!(
            text.contains("No stack") && !text.contains("Program is running"),
            "bt after the exit must be answered by gdb, got {text}"
        );

        // Nothing is left to interrupt, and no signal may go to the dead pid.
        let interrupt = send_to_bridge(&live.socket, "__interrupt__").await.unwrap();
        assert!(
            !interrupt.contains("No such process") && !interrupt.contains("Interrupt sent"),
            "interrupt after the exit must not signal the exited program, got {interrupt:?}"
        );

        let _ = std::fs::remove_file(&log);
    }
}
