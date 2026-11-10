//! The Cloud Agent run contract in headless `exec`:
//!
//! - with `BYTEASK_CHECKS_OUT` set, the configure/build/test/lint commands the agent runs
//!   through its shell tools are recorded and written there as `checks.json` (`mode: agent`)
//!   when the run ends;
//! - on SIGTERM the running turn is interrupted, `-o` gets the last agent message so far (or a
//!   short "stopped" note), `checks.json` is written, and exec exits 124. A watchdog does the
//!   same after a grace period if the interrupt does not finish, so both files are flushed
//!   well within the runner's 20 s.
//!
//! Nothing here changes exec when the variable is unset and no SIGTERM arrives.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use codex_agent_checks::AgentRecorder;
use codex_agent_checks::CHECKS_OUT_ENV_VAR;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use tokio::sync::mpsc;

/// Exit code after SIGTERM (same as `timeout(1)`).
pub(crate) const SIGTERM_EXIT_CODE: i32 = 124;

/// How long the interrupted turn may take to wind down before the watchdog writes the files
/// and exits anyway.
#[cfg(unix)]
const SIGTERM_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

const STOPPED_NOTE: &str = "Stopped before finishing: the run was cancelled (SIGTERM) before \
the agent wrote a final message.\n";

#[derive(Clone)]
pub(crate) struct AgentRun {
    state: Arc<Mutex<State>>,
}

struct State {
    checks_out: Option<PathBuf>,
    recorder: Option<AgentRecorder>,
    last_message_file: Option<PathBuf>,
    last_agent_message: Option<String>,
    stopped_written: bool,
}

impl AgentRun {
    pub(crate) fn new(cwd: &Path, last_message_file: Option<PathBuf>) -> Self {
        let checks_out = std::env::var_os(CHECKS_OUT_ENV_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let recorder = checks_out
            .as_ref()
            .and_then(|_| match AgentRecorder::new(cwd) {
                Ok(recorder) => Some(recorder),
                Err(err) => {
                    eprintln!(
                        "Warning: {CHECKS_OUT_ENV_VAR} is set but no checks.json can be written: {err}"
                    );
                    None
                }
            });
        Self {
            state: Arc::new(Mutex::new(State {
                checks_out,
                recorder,
                last_message_file,
                last_agent_message: None,
                stopped_written: false,
            })),
        }
    }

    /// Track commands (any thread: they all ran in this workspace) and the primary thread's
    /// latest agent message.
    pub(crate) fn observe(&self, notification: &ServerNotification, primary_thread_id: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match notification {
            ServerNotification::ItemStarted(started) => {
                if let ThreadItem::CommandExecution {
                    id,
                    command,
                    cwd,
                    source,
                    ..
                } = &started.item
                    && is_agent_command(source)
                    && let Some(recorder) = state.recorder.as_mut()
                {
                    recorder.command_started(id, command, Path::new(cwd.as_str()));
                }
            }
            ServerNotification::ItemCompleted(completed) => match &completed.item {
                ThreadItem::AgentMessage { text, .. }
                    if completed.thread_id == primary_thread_id && !text.trim().is_empty() =>
                {
                    state.last_agent_message = Some(text.clone());
                }
                ThreadItem::CommandExecution {
                    id,
                    command,
                    cwd,
                    source,
                    status,
                    aggregated_output,
                    exit_code,
                    duration_ms,
                    ..
                } if is_agent_command(source) => {
                    // The C/C++ tools' finish review learns which of the agent's commands built
                    // and ran tests (the hook does nothing unless BYTEASK_FINISH_REVIEW_OWN_TESTS is on).
                    if matches!(
                        status,
                        CommandExecutionStatus::Completed | CommandExecutionStatus::Failed
                    ) {
                        let (builds, tests) = codex_agent_checks::command_builds_and_tests(command);
                        if builds || tests {
                            codex_core::byteask_context::note_agent_command(
                                command,
                                builds,
                                tests,
                                *exit_code,
                                aggregated_output.as_deref().unwrap_or_default(),
                            );
                        }
                    }
                    let Some(recorder) = state.recorder.as_mut() else {
                        return;
                    };
                    match status {
                        CommandExecutionStatus::Declined => recorder.command_dropped(id),
                        CommandExecutionStatus::Completed
                        | CommandExecutionStatus::Failed
                        | CommandExecutionStatus::InProgress => recorder.command_finished(
                            id,
                            command,
                            Path::new(cwd.as_str()),
                            *exit_code,
                            *duration_ms,
                            aggregated_output.as_deref(),
                        ),
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Write checks.json (if requested) for the run so far.
    pub(crate) fn write_checks(&self) {
        if let Ok(state) = self.state.lock() {
            write_checks_locked(&state);
        }
    }

    /// After SIGTERM: `-o` gets the last agent message (or the stopped note) and checks.json is
    /// written. Runs once, whichever of the main loop and the watchdog gets here first.
    pub(crate) fn write_stopped(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.stopped_written {
            return;
        }
        state.stopped_written = true;
        if let Some(path) = state.last_message_file.as_ref() {
            let text = state.last_agent_message.as_deref().unwrap_or(STOPPED_NOTE);
            if let Err(err) = std::fs::write(path, text) {
                eprintln!(
                    "Failed to write last message file {}: {err}",
                    path.display()
                );
            }
        }
        write_checks_locked(&state);
    }
}

fn write_checks_locked(state: &State) {
    if let (Some(path), Some(recorder)) = (state.checks_out.as_ref(), state.recorder.as_ref())
        && let Err(err) = recorder.checks().write_to(path)
    {
        eprintln!(
            "Failed to write {CHECKS_OUT_ENV_VAR} file {}: {err}",
            path.display()
        );
    }
}

/// Commands the model ran: a shell-tool call or a unified-exec session start. Not the user's
/// own `!` shell commands, and not later writes into an already running session.
fn is_agent_command(source: &CommandExecutionSource) -> bool {
    match source {
        CommandExecutionSource::Agent | CommandExecutionSource::UnifiedExecStartup => true,
        CommandExecutionSource::UserShell | CommandExecutionSource::UnifiedExecInteraction => false,
    }
}

/// Listen for SIGTERM. The returned channel fires once so the main loop can interrupt the
/// turn; independently, after [`SIGTERM_GRACE`] the files are written and the process exits
/// 124 even if the loop is stuck.
#[cfg(unix)]
pub(crate) fn watch_sigterm(run: AgentRun) -> mpsc::UnboundedReceiver<()> {
    use tokio::signal::unix::SignalKind;
    use tokio::signal::unix::signal;

    let (tx, rx) = mpsc::unbounded_channel();
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::spawn(async move {
                if term.recv().await.is_some() {
                    tracing::debug!("SIGTERM: stopping the run");
                    let _ = tx.send(());
                    tokio::time::sleep(SIGTERM_GRACE).await;
                    run.write_stopped();
                    std::process::exit(SIGTERM_EXIT_CODE);
                }
            });
        }
        Err(err) => tracing::warn!("could not install the SIGTERM handler: {err}"),
    }
    rx
}

#[cfg(not(unix))]
pub(crate) fn watch_sigterm(_run: AgentRun) -> mpsc::UnboundedReceiver<()> {
    let (_tx, rx) = mpsc::unbounded_channel();
    rx
}
