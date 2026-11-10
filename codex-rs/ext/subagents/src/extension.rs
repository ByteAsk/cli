//! Host-facing contributors. Only the root thread of a session can own an orchestration; the
//! sub-agents it starts are marked and never start one of their own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::spawner::AgentSpawner;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::TokenUsageContributor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolStartInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnGateContributor;
use codex_extension_api::TurnGateFragments;
use codex_extension_api::TurnGateInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::user_input::UserInput;
use tokio::sync::OwnedSemaphorePermit;

use crate::attempts::AttemptsMode;
use crate::attempts::AttemptsSettings;
use crate::host::ChildHost;
use crate::host::ThreadHost;
use crate::orchestrator::Orchestration;
use crate::orchestrator::Params;
use crate::orchestrator::StepCounters;
use crate::orchestrator::looks_like_issue;
use crate::runner;
use crate::settings::Settings;

/// Session-source labels of the sub-agents this extension starts.
const CHILD_LABEL_PREFIX: &str = "byteask_";

/// Per-root-thread state, seeded at thread start when a feature is enabled.
struct RootState {
    settings: Settings,
    /// Tier B mode for this session (off unless configured).
    attempts: (AttemptsSettings, AttemptsMode),
    config: Arc<Config>,
    started: AtomicBool,
    orchestration: StdMutex<Option<Arc<Orchestration>>>,
}

/// Marks a sub-agent thread started by this extension.
struct ChildMarker;

/// Marks the root turn that owns the orchestration.
struct OrchestrationTurn;

pub struct SubagentsExtension<S> {
    spawner: Arc<S>,
    thread_manager: Weak<ThreadManager>,
    counters: Arc<StepCounters>,
    /// Build/test slots held by sub-agent tool calls, keyed by (thread, call).
    held: StdMutex<HashMap<(String, String), OwnedSemaphorePermit>>,
}

fn child_label(source: &SessionSource) -> Option<&str> {
    match source {
        SessionSource::SubAgent(SubAgentSource::Other(label))
            if label.starts_with(CHILD_LABEL_PREFIX) =>
        {
            Some(label.as_str())
        }
        _ => None,
    }
}

fn task_text(input: &[UserInput]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Heuristic for "this shell command builds or runs tests" (sub-agent throttling only).
pub fn is_build_or_test_command(cmd: &str) -> bool {
    const TOOLS: &[&str] = &[
        "make", "gmake", "cmake", "ninja", "ctest", "meson", "scons", "bazel", "gcc", "g++", "cc",
        "c++", "clang", "clang++",
    ];
    cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|tok| !tok.is_empty())
        .any(|tok| {
            let base = tok.rsplit('/').next().unwrap_or(tok);
            TOOLS.contains(&base)
                || (base.contains("test") && (tok.starts_with("./") || tok.contains('/')))
        })
}

fn command_of(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    if let Some(cmd) = v.get("cmd").and_then(|c| c.as_str()) {
        return Some(cmd.to_string());
    }
    v.get("command").and_then(|c| c.as_array()).map(|parts| {
        parts
            .iter()
            .filter_map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

impl<S> ThreadLifecycleContributor<Config> for SubagentsExtension<S>
where
    S: Send + Sync,
{
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if child_label(input.session_source).is_some() {
                input.thread_store.insert(ChildMarker);
                return;
            }
            if input.session_source.is_non_root_agent() {
                return;
            }
            let effective = input.config.config_layer_stack.effective_config();
            let settings = Settings::from_env_and_config(effective.get("byteask"));
            let attempts = AttemptsSettings::from_env_and_config(effective.get("byteask"));
            let mode = attempts.effective_mode();
            if !settings.any_enabled() && mode == AttemptsMode::Off {
                return;
            }
            input.thread_store.insert(RootState {
                settings,
                attempts: (attempts, mode),
                config: Arc::new(input.config.clone()),
                started: AtomicBool::new(false),
                orchestration: StdMutex::new(None),
            });
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.thread_store.get::<ChildMarker>().is_some() {
                // Release build/test slots a stopped sub-agent still holds.
                let id = input.thread_store.level_id().to_string();
                if let Ok(mut held) = self.held.lock() {
                    held.retain(|(thread, _), _| *thread != id);
                }
                return;
            }
            let orch = input
                .thread_store
                .get::<RootState>()
                .and_then(|s| s.orchestration.lock().ok().and_then(|o| o.clone()));
            if let Some(orch) = orch {
                orch.abort().await;
            }
        })
    }
}

impl<S> TurnInputContributor for SubagentsExtension<S>
where
    S: AgentSpawner<StartThreadOptions, Spawned = NewThread, Error = CodexErr> + 'static,
{
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        _extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(state) = thread_store.get::<RootState>() else {
                return Vec::new();
            };
            // One orchestration per session, owned by its first turn.
            if state.started.swap(true, Ordering::SeqCst) {
                return Vec::new();
            }
            let task = task_text(&input.user_input);
            let started = Instant::now();
            let config = state.config.clone();
            // The work tree containing the workspace, or the one below it when the workspace
            // is a parent of the repository.
            let Some(repo) = codex_cpp_tools::primary_repo_root(config.cwd.as_path()).await else {
                tracing::info!(target: "byteask_subagents", "not a git work tree; off");
                return Vec::new();
            };
            if !codex_cpp_tools::native_exec_allowed(&config) {
                tracing::info!(target: "byteask_subagents", "native execution not allowed in this workspace; off");
                return Vec::new();
            }
            let is_cpp = codex_cpp_tools::is_cpp_workspace(&repo);
            let issue = looks_like_issue(&task);
            let run_repro = state.settings.repro_tests && is_cpp && issue;
            // Tier B: adaptive multi-attempt, same workspace conditions as the writer.
            let run_attempts = state.attempts.1 != AttemptsMode::Off && is_cpp && issue;
            if !run_repro && !run_attempts {
                tracing::info!(target: "byteask_subagents", is_cpp, issue, "conditions not met; off");
                return Vec::new();
            }
            let Ok(root_id) = ThreadId::from_string(thread_store.level_id()) else {
                return Vec::new();
            };
            let host: Arc<dyn ChildHost> = Arc::new(ThreadHost::new(
                self.spawner.clone(),
                self.thread_manager.clone(),
                root_id,
                config.clone(),
            ));
            let params = Params {
                settings: state.settings.clone(),
                repo,
                task,
                root_id: root_id.to_string(),
                codex_home: config.codex_home.to_path_buf(),
                run_repro,
            };
            match Orchestration::start(params, host, self.counters.clone()).await {
                Ok(orch) => {
                    if run_attempts {
                        orch.attempts
                            .configure(state.attempts.0.clone(), state.attempts.1);
                    }
                    if let Ok(mut slot) = state.orchestration.lock() {
                        *slot = Some(orch);
                    }
                    turn_store.insert(OrchestrationTurn);
                    tracing::info!(target: "byteask_subagents", ms = started.elapsed().as_millis() as u64, "orchestration started");
                }
                Err(err) => {
                    tracing::warn!(target: "byteask_subagents", %err, "orchestration not started");
                }
            }
            Vec::new()
        })
    }
}

impl<S> TurnLifecycleContributor for SubagentsExtension<S>
where
    S: Send + Sync,
{
    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.turn_store.get::<OrchestrationTurn>().is_none() {
                return;
            }
            let orch = input
                .thread_store
                .get::<RootState>()
                .and_then(|s| s.orchestration.lock().ok().and_then(|o| o.clone()));
            if let Some(orch) = orch {
                orch.finalize().await;
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.turn_store.get::<OrchestrationTurn>().is_none() {
                return;
            }
            let orch = input
                .thread_store
                .get::<RootState>()
                .and_then(|s| s.orchestration.lock().ok().and_then(|o| o.clone()));
            if let Some(orch) = orch {
                orch.abort().await;
            }
        })
    }
}

impl<S> TokenUsageContributor for SubagentsExtension<S>
where
    S: Send + Sync,
{
    fn on_token_usage<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        token_usage: &'a TokenUsageInfo,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if thread_store.get::<ChildMarker>().is_some() {
                self.counters.bump(thread_store.level_id());
                let total = token_usage.total_token_usage.input_tokens
                    + token_usage.total_token_usage.output_tokens;
                crate::attempts::meter::record(
                    thread_store.level_id(),
                    u64::try_from(total).unwrap_or(0),
                );
            }
        })
    }
}

impl<S> TurnGateContributor for SubagentsExtension<S>
where
    S: Send + Sync,
{
    /// Tier B observes (never adds anything): the root turn's id and its final message each
    /// time the model ends the turn; the last one before the turn ends is attempt 1's.
    fn before_stop<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            if input.turn_store.get::<OrchestrationTurn>().is_some()
                && let Some(orch) = input
                    .thread_store
                    .get::<RootState>()
                    .and_then(|s| s.orchestration.lock().ok().and_then(|o| o.clone()))
            {
                orch.attempts
                    .observe_turn_end(input.turn_id, input.last_agent_message);
            }
            Vec::new()
        })
    }
}

impl<S> ToolLifecycleContributor for SubagentsExtension<S>
where
    S: Send + Sync,
{
    /// Sub-agent builds/test runs share the build/test slots with the engine's own checks.
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.thread_store.get::<ChildMarker>().is_none() {
                return;
            }
            let Some(cmd) = command_of(input.payload) else {
                return;
            };
            if !is_build_or_test_command(&cmd) {
                return;
            }
            let slots = runner::run_slots(2);
            if let Ok(permit) = slots.acquire_owned().await
                && let Ok(mut held) = self.held.lock()
            {
                held.insert(
                    (
                        input.thread_store.level_id().to_string(),
                        input.call_id.to_string(),
                    ),
                    permit,
                );
            }
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.thread_store.get::<ChildMarker>().is_none() {
                return;
            }
            if let Ok(mut held) = self.held.lock() {
                held.remove(&(
                    input.thread_store.level_id().to_string(),
                    input.call_id.to_string(),
                ));
            }
        })
    }
}

/// Installs the independent reproduction-test writer and Tier B (adaptive multi-attempt). Each
/// stays inert unless enabled (see [`Settings`] and [`AttemptsSettings`]). `child_spawner`
/// starts fresh sub-agent sessions attributed to a parent thread (host:
/// `ThreadManager::spawn_fresh_subagent`).
pub fn install<S>(
    registry: &mut ExtensionRegistryBuilder<Config>,
    child_spawner: S,
    thread_manager: Weak<ThreadManager>,
) where
    S: AgentSpawner<StartThreadOptions, Spawned = NewThread, Error = CodexErr> + 'static,
{
    crate::attempts::meter::note_process_start();
    let extension = Arc::new(SubagentsExtension {
        spawner: Arc::new(child_spawner),
        thread_manager,
        counters: Arc::new(StepCounters::default()),
        held: StdMutex::new(HashMap::new()),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_input_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.turn_gate_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_test_commands() {
        for cmd in [
            "cmake --build build -j4",
            "cd build && make -j2",
            "ninja -C out",
            "ctest --output-on-failure",
            "./build/tests/unit_tests --filter x",
            "g++ -std=c++17 a.cpp -o a",
            "/usr/bin/cc -c x.c",
        ] {
            assert!(is_build_or_test_command(cmd), "{cmd}");
        }
        for cmd in [
            "ls -la",
            "grep -rn test src/",
            "cat tests/a.cpp",
            "git diff",
        ] {
            assert!(!is_build_or_test_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn command_extraction() {
        let p = ToolPayload::Function {
            arguments: "{\"cmd\": \"make -j2\", \"yield_time_ms\": 1000}".to_string(),
        };
        assert_eq!(command_of(&p), Some("make -j2".to_string()));
        let p = ToolPayload::Function {
            arguments: "{\"command\": [\"bash\", \"-lc\", \"ctest\"]}".to_string(),
        };
        assert_eq!(command_of(&p), Some("bash -lc ctest".to_string()));
    }
}
