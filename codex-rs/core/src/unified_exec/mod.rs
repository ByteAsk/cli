//! Unified Exec: interactive process execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive processes (create, reuse, buffer output with caps).
//! - Supports completion-only calls that terminate on timeout or cancellation.
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox-transformed `ExecRequest`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open process)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `SandboxTransformRequest` -> `ExecRequest` -> spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Process handle is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/process
//! concerns remain isolated here. The implementation is split between:
//! - `process.rs`: PTY process lifecycle + output buffering.
//! - `process_state.rs`: shared exit/failure state for local and remote processes.
//! - `process_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_network_proxy::NetworkProxy;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::unified_exec::preparation::PreparedExecCommand;
use crate::tools::network_approval::DeferredNetworkApproval;
use codex_core_plugins::PluginMetricsSidecar;

mod async_watcher;
mod errors;
mod oneshot;
pub(crate) mod output;
mod process;
mod process_manager;
pub(crate) use process_manager::exec_env_policy_from_shell_policy;
mod process_state;
mod shell_snapshot;
mod stdin_approval;

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use errors::UnifiedExecError;
pub(crate) use process::UnifiedExecProcess;
pub(crate) use stdin_approval::TerminalPermissions;
pub(crate) use stdin_approval::TerminalSandboxSource;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 10_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
/// ByteAsk ("wait instead of poll"): the default floor for an EMPTY `write_stdin` poll
/// of a session started WITHOUT a TTY. Such a session cannot take input (a non-empty write
/// is refused), so a short poll only returns the same "still running" answer sooner, and
/// every extra poll re-sends the whole conversation. The wait still ends as soon as the
/// command exits. Policy number `empty_poll_min_ms` (5,000-300,000); the brake, a TTY
/// session, or a non-empty write keep upstream's behaviour.
/// ByteAsk 0.2.4: 180 s (was 60 s).
pub(crate) const DEFAULT_EMPTY_POLL_MIN_MS: u64 = 180_000;
/// ByteAsk: the default floor for the FIRST wait of an `exec_command` started without a
/// TTY (upstream already applies 10 s on Windows). Policy number `first_yield_min_ms`
/// (250-30,000). Ends as soon as the command exits, so short commands are unaffected.
/// ByteAsk 0.2.4: 30 s (was 10 s).
pub(crate) const DEFAULT_FIRST_YIELD_MIN_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;

const MAX_TRACE_ID_BYTES: usize = 256;

fn trace_id(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= MAX_TRACE_ID_BYTES).then_some(id)
}

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub step_context: Arc<StepContext>,
    pub cancellation_token: CancellationToken,
    pub call_id: String,
}

impl UnifiedExecContext {
    pub fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        call_id: String,
    ) -> Self {
        Self {
            session,
            step_context,
            cancellation_token,
            call_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub prepared: PreparedExecCommand,
    pub process_id: i32,
    pub network: Option<NetworkProxy>,
}

/// The process and environment behind a terminal session ID at the time of lookup.
#[derive(Clone, Debug)]
pub(crate) struct TerminalSession {
    pub(crate) process_id: i32,
    process: Arc<UnifiedExecProcess>,
    call_id: String,
    pub(crate) environment_id: String,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub terminal: &'a TerminalSession,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
    pub interaction_event: Option<WriteStdinInteractionEvent<'a>>,
}

pub(crate) struct WriteStdinInteractionEvent<'a> {
    pub session: &'a Arc<Session>,
    pub turn: &'a Arc<TurnContext>,
}

impl std::fmt::Debug for WriteStdinInteractionEvent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WriteStdinInteractionEvent")
    }
}

#[derive(Default)]
pub(crate) struct ProcessStore {
    processes: HashMap<i32, ProcessEntry>,
    reserved_process_ids: HashSet<i32>,
}

impl ProcessStore {
    fn remove(&mut self, process_id: i32) -> Option<ProcessEntry> {
        if !process_manager::should_use_deterministic_process_ids() {
            self.reserved_process_ids.remove(&process_id);
        }
        self.processes.remove(&process_id)
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Mutex<ProcessStore>,
    max_write_stdin_yield_time_ms: u64,
}

impl UnifiedExecProcessManager {
    pub(crate) fn new(max_write_stdin_yield_time_ms: u64) -> Self {
        Self {
            process_store: Mutex::new(ProcessStore::default()),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
        }
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    plugin_metrics_sidecar: Option<SharedPluginMetricsSidecar>,
    call_id: String,
    process_id: i32,
    cwd: PathUri,
    initial_exec_command_active: Arc<std::sync::atomic::AtomicBool>,
    hook_command: String,
    tty: bool,
    environment_id: String,
    permissions: TerminalPermissions,
    network_approval: Option<DeferredNetworkApproval>,
    session: Weak<Session>,
    last_used: tokio::time::Instant,
}

type SharedPluginMetricsSidecar = Arc<std::sync::Mutex<Option<PluginMetricsSidecar>>>;

fn take_plugin_metrics_sidecar(
    sidecar: &SharedPluginMetricsSidecar,
) -> Option<PluginMetricsSidecar> {
    sidecar
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    clamp_yield_time_with(&byteask_output_reducers::policy::active(), yield_time_ms)
}

fn clamp_yield_time_with(policy: &byteask_output_reducers::Policy, yield_time_ms: u64) -> u64 {
    let yield_time_ms = if cfg!(windows) {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    // ByteAsk: the server-driven policy may raise the cap on the FIRST wait, so a
    // 30-120 s build returns in one call instead of costing extra polling calls (each of
    // which re-reads the whole context). Output still streams to the user meanwhile.
    let max = policy
        .number("first_yield_ms")
        .map_or(MAX_YIELD_TIME_MS, |ms| ms.max(MAX_YIELD_TIME_MS));
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, max)
}

/// The first wait of an `exec_command`: [`clamp_yield_time`], raised to the wait floor
/// ([`DEFAULT_FIRST_YIELD_MIN_MS`]) when the command runs without a TTY.
pub(crate) fn exec_yield_time(yield_time_ms: u64, tty: bool) -> u64 {
    exec_yield_time_with(
        &byteask_output_reducers::policy::active(),
        yield_time_ms,
        tty,
    )
}

pub(crate) fn exec_yield_time_with(
    policy: &byteask_output_reducers::Policy,
    yield_time_ms: u64,
    tty: bool,
) -> u64 {
    let floor = if tty || policy.brake {
        MIN_YIELD_TIME_MS
    } else {
        policy
            .number("first_yield_min_ms")
            .unwrap_or(DEFAULT_FIRST_YIELD_MIN_MS)
    };
    clamp_yield_time_with(policy, yield_time_ms.max(floor))
}

/// The floor of an EMPTY `write_stdin` poll: upstream's [`MIN_EMPTY_YIELD_TIME_MS`] for a TTY
/// session (it may be an interactive program the model is typing into), the ByteAsk floor
/// ([`DEFAULT_EMPTY_POLL_MIN_MS`]) for a session without one.
pub(crate) fn empty_poll_floor_ms(tty: bool) -> u64 {
    empty_poll_floor_ms_with(&byteask_output_reducers::policy::active(), tty)
}

pub(crate) fn empty_poll_floor_ms_with(policy: &byteask_output_reducers::Policy, tty: bool) -> u64 {
    if tty || policy.brake {
        MIN_EMPTY_YIELD_TIME_MS
    } else {
        policy
            .number("empty_poll_min_ms")
            .unwrap_or(DEFAULT_EMPTY_POLL_MIN_MS)
    }
}

/// Told to the model when a wait floor made it wait longer than it asked and the command
/// is still running, so the longer wall time does not read as a hang and the next wait is
/// not a short poll or a clock check.
pub(crate) fn wait_floor_note(wall_time: Duration) -> String {
    format!(
        "Waited {:.0}s: a running command without a TTY is waited on at least that long; \
         every wait ends as soon as the command exits.",
        wall_time.as_secs_f64()
    )
}

pub(crate) fn resolve_max_tokens(max_tokens: Option<usize>) -> usize {
    max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

#[cfg(test)]
#[cfg(unix)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[cfg(unix)]
#[path = "mod_tests.rs"]
mod tests;
