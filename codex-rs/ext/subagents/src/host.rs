//! How the orchestrator starts and drives sub-agents. The orchestrator only sees the
//! [`ChildHost`] / [`ChildHandle`] traits, so tests substitute a stub host with scripted
//! children. The real host uses the spawner injected by the app-server (the same pattern as
//! guardian-v2's injected `AgentSpawner`).
//!
//! Every child is a *fresh* session (no inherited transcript) attributed to the root thread:
//! its rollout carries `parent_thread_id` = the root and `thread_source` = `subagent`, and is
//! written to the same `CODEX_HOME/sessions` tree, so the harness can sum every rollout and
//! still find the root.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_core::CodexThread;
use codex_core::NewThread;
use codex_core::StartIfIdleSubmission;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::AgentSpawner;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Role {
    /// The independent reproduction-test writer.
    Repro,
    /// Tier B: attempt k (>= 2; attempt 1 is the root session itself).
    Attempt(usize),
    /// Tier B: the fresh-context pairwise judge.
    Judge,
}

impl Role {
    /// `SubAgentSource::Other` label recorded in the child's rollout.
    pub fn source_label(self) -> &'static str {
        match self {
            Role::Repro => "byteask_repro",
            Role::Attempt(_) => "byteask_attempt",
            Role::Judge => "byteask_judge",
        }
    }
}

pub struct ChildSpec {
    pub role: Role,
    pub cwd: PathBuf,
    pub prompt: String,
    /// Use a higher reasoning effort than the root.
    pub high_effort: bool,
    /// Extra environment for the child's shell commands (ccache, job limits).
    pub env: Vec<(String, String)>,
    /// Directories outside `cwd` the child must be able to write (its report files).
    pub extra_writable: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum ChildState {
    Running,
    /// The turn completed; the final assistant message, if any.
    Done(Option<String>),
    /// The turn was interrupted (budget or deadline).
    Interrupted,
    Failed(String),
}

impl ChildState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, ChildState::Running)
    }
}

pub trait ChildHandle: Send + Sync {
    fn id(&self) -> String;
    fn state(&self) -> BoxFut<'_, ChildState>;
    fn interrupt(&self) -> BoxFut<'_, ()>;
    /// Add a message to the child's running turn. False when it has no running turn.
    fn inject(&self, text: String) -> BoxFut<'_, bool>;
    fn shutdown(&self) -> BoxFut<'_, ()>;
}

pub trait ChildHost: Send + Sync {
    fn spawn(&self, spec: ChildSpec) -> BoxFut<'_, Result<Arc<dyn ChildHandle>, String>>;
    /// Add a message to the root thread's running turn. False when no turn is running.
    fn notify_root(&self, text: String) -> BoxFut<'_, bool>;
}

fn context_item(text: String) -> ResponseItem {
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("byteask_repro"),
        text,
    ))
}

fn raised_effort(current: Option<&ReasoningEffort>) -> ReasoningEffort {
    match current {
        Some(
            e @ (ReasoningEffort::High
            | ReasoningEffort::XHigh
            | ReasoningEffort::Max
            | ReasoningEffort::Ultra
            | ReasoningEffort::Persistent),
        ) => e.clone(),
        _ => ReasoningEffort::High,
    }
}

/// The production host: fresh child sessions through the injected spawner.
pub struct ThreadHost<S> {
    spawner: Arc<S>,
    thread_manager: Weak<ThreadManager>,
    root_id: ThreadId,
    root_config: Arc<Config>,
}

impl<S> ThreadHost<S> {
    pub fn new(
        spawner: Arc<S>,
        thread_manager: Weak<ThreadManager>,
        root_id: ThreadId,
        root_config: Arc<Config>,
    ) -> Self {
        Self {
            spawner,
            thread_manager,
            root_id,
            root_config,
        }
    }

    fn child_config(&self, spec: &ChildSpec) -> Result<Config, String> {
        let mut config = (*self.root_config).clone();
        config.cwd = AbsolutePathBuf::try_from(spec.cwd.clone()).map_err(|e| e.to_string())?;
        if !config.workspace_roots_explicit {
            let mut roots = vec![config.cwd.clone()];
            for dir in &spec.extra_writable {
                if let Ok(abs) = AbsolutePathBuf::try_from(dir.clone()) {
                    roots.push(abs);
                }
            }
            config.workspace_roots = roots;
        }
        if spec.high_effort {
            config.model_reasoning_effort = Some(raised_effort(config.model_reasoning_effort.as_ref()));
        }
        for (k, v) in &spec.env {
            config
                .permissions
                .shell_environment_policy
                .r#set
                .insert(k.clone(), v.clone());
        }
        Ok(config)
    }
}

impl<S> ChildHost for ThreadHost<S>
where
    S: AgentSpawner<StartThreadOptions, Spawned = NewThread, Error = CodexErr> + 'static,
{
    fn spawn(&self, spec: ChildSpec) -> BoxFut<'_, Result<Arc<dyn ChildHandle>, String>> {
        Box::pin(async move {
            let config = self.child_config(&spec)?;
            let options = StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                    spec.role.source_label().to_string(),
                ))),
                thread_source: Some(ThreadSource::Subagent),
                ..StartThreadOptions::new(config)
            };
            let NewThread {
                thread_id, thread, ..
            } = self
                .spawner
                .spawn_subagent(self.root_id, options)
                .await
                .map_err(|e| format!("spawn failed: {e}"))?;
            let child = Arc::new(ThreadChild {
                id: thread_id,
                thread,
                thread_manager: self.thread_manager.clone(),
            });
            let request = TurnInputRequest::user_input(vec![UserInput::Text {
                text: spec.prompt,
                text_elements: Vec::new(),
            }]);
            match child.thread.start_turn_if_idle(request).await {
                Ok(StartIfIdleSubmission::Started { .. }) => Ok(child as Arc<dyn ChildHandle>),
                Ok(StartIfIdleSubmission::NotSubmitted { reason }) => {
                    child.shutdown().await;
                    Err(format!("first turn not started: {reason:?}"))
                }
                Err(err) => {
                    child.shutdown().await;
                    Err(format!("first turn not started: {err}"))
                }
            }
        })
    }

    fn notify_root(&self, text: String) -> BoxFut<'_, bool> {
        Box::pin(async move {
            let Some(tm) = self.thread_manager.upgrade() else {
                return false;
            };
            let Ok(root) = tm.get_thread(self.root_id).await else {
                return false;
            };
            root.inject_if_running(vec![context_item(text)]).await.is_ok()
        })
    }
}

struct ThreadChild {
    id: ThreadId,
    thread: Arc<CodexThread>,
    thread_manager: Weak<ThreadManager>,
}

impl ChildHandle for ThreadChild {
    fn id(&self) -> String {
        self.id.to_string()
    }

    fn state(&self) -> BoxFut<'_, ChildState> {
        Box::pin(async move {
            match self.thread.agent_status().await {
                AgentStatus::PendingInit | AgentStatus::Running => ChildState::Running,
                AgentStatus::Completed(message) => ChildState::Done(message),
                AgentStatus::Interrupted => ChildState::Interrupted,
                AgentStatus::Errored(err) => ChildState::Failed(err),
                AgentStatus::Shutdown => ChildState::Failed("shut down".to_string()),
                AgentStatus::NotFound => ChildState::Failed("not found".to_string()),
            }
        })
    }

    fn interrupt(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            let _ = self.thread.submit(Op::Interrupt).await;
        })
    }

    fn inject(&self, text: String) -> BoxFut<'_, bool> {
        Box::pin(async move {
            self.thread
                .inject_if_running(vec![context_item(text)])
                .await
                .is_ok()
        })
    }

    fn shutdown(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            let _ = tokio::time::timeout(Duration::from_secs(30), self.thread.shutdown_and_wait()).await;
            if let Some(tm) = self.thread_manager.upgrade() {
                let _ = tm.remove_thread(&self.id).await;
            }
        })
    }
}
