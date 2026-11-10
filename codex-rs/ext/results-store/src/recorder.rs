//! Writing into the store from a running agent thread.
//!
//! * [`ThreadRecorder`] is one session of one thread: its tool results and the
//!   AI's edits go into the session's change (one per repository and branch)
//!   through the shared writer in [`crate::changes`], which every process uses
//!   (the engine, the toolbox serving another agent, that agent's hooks). Every
//!   method is synchronous and swallows its own errors: recording must never
//!   break or slow a tool call.
//! * [`Recorded`] wraps a tool: it probes the workspace while the tool runs
//!   (so the version is the code the check started on) and records the
//!   shared result line the tool's answer ends with.
//! * [`install`] registers the contributors that create a recorder at thread
//!   start, record the AI's edits (the `apply_patch` tool finishing) and end
//!   the session at thread stop.
//!
//! The session: an engine thread (TUI, `byteask exec`) is its thread id. The
//! toolbox serving Claude Code takes `CLAUDE_CODE_SESSION_ID`, which Claude
//! Code also gives its hooks, so the tool results, the hooks' edits and the
//! finish gate's verdicts of one Claude Code session meet in one change
//! (`CODEX_THREAD_ID` likewise, else a random id).

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use byteask_policy::ResultsStoreLimits;
use check_result::CheckResult;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolExposure;
use codex_tools::ToolSearchInfo;

use crate::changes;
use crate::store;
use crate::workspace;
use crate::workspace::Probe;

/// The tool whose successful finish is an AI edit.
pub const EDIT_TOOL: &str = "apply_patch";
/// Clean up at most this often per recorder...
const GC_EVERY_SECS: u64 = 3_600;
/// ...or after this many payload bytes written since the last clean-up.
const GC_AFTER_BYTES: u64 = 64 << 20;
/// The agents' own session ids, in the order they are tried.
pub const AGENT_SESSION_VARS: [&str; 2] = ["CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID"];

/// What a thread needs to record: where the store is and which directory the
/// thread works in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `$BYTEASK_HOME`.
    pub home: PathBuf,
    /// The thread's working directory.
    pub cwd: PathBuf,
}

#[derive(Debug, Default)]
struct State {
    /// The repository top for `cwd`: `None` = not looked up yet.
    toplevel: Option<Option<PathBuf>>,
    /// Change keys this session wrote to (closed when the session ends).
    keys: BTreeSet<String>,
    bytes_since_gc: u64,
    last_gc_check: u64,
}

/// One session's writer. See the module docs.
#[derive(Debug)]
pub struct ThreadRecorder {
    home: PathBuf,
    cwd: PathBuf,
    session: String,
    limits: ResultsStoreLimits,
    max_results: u64,
    state: Mutex<State>,
}

/// The agent's own session id from its environment, if it gives one.
pub fn agent_session(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    AGENT_SESSION_VARS.iter().find_map(|name| {
        env(name)
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && id.len() <= 200)
    })
}

impl ThreadRecorder {
    /// A recorder with a session id of its own.
    pub fn new(settings: Settings) -> Self {
        Self::for_session(settings, uuid::Uuid::new_v4().simple().to_string())
    }

    /// A recorder for a known session id.
    pub fn for_session(settings: Settings, session: String) -> Self {
        let (limits, _problems) = byteask_policy::results_store_limits(&settings.home);
        Self::with_limits(settings, limits).with_session(session)
    }

    pub fn with_limits(settings: Settings, limits: ResultsStoreLimits) -> Self {
        Self {
            home: settings.home,
            cwd: settings.cwd,
            session: uuid::Uuid::new_v4().simple().to_string(),
            limits,
            max_results: store::MAX_RESULTS_PER_CHANGE,
            state: Mutex::new(State::default()),
        }
    }

    pub fn with_session(mut self, session: String) -> Self {
        self.session = session;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_max_results(mut self, max_results: u64) -> Self {
        self.max_results = max_results;
        self
    }

    /// This session's id.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The store directory this recorder writes.
    pub fn root(&self) -> PathBuf {
        store::root(&self.home)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Look at the workspace now: repository, branch, version.
    pub fn probe(&self) -> Probe {
        let cached = self.state().toplevel.clone();
        let top = match cached {
            Some(top) => top,
            None => {
                let top = workspace::toplevel(&self.cwd, workspace::PROBE_BUDGET);
                self.state().toplevel = Some(top.clone());
                top
            }
        };
        workspace::probe(&self.cwd, top.as_deref(), workspace::PROBE_BUDGET)
    }

    fn written(&self, probe: &Probe) {
        let key = changes::change_key(&probe.repo, &probe.branch, &self.session);
        self.state().keys.insert(key);
    }

    /// Record that the AI changed files, at time `at`, in the workspace `probe` saw.
    pub fn record_edit(&self, at: u64, probe: &Probe) {
        if changes::append(&self.home, &self.session, probe, at, changes::Event::Edit).is_ok() {
            self.written(probe);
        }
    }

    /// Record one result, found at time `at`, for the code `probe` saw when
    /// the check started. `answer` is the tool's full text, kept as a
    /// disposable payload.
    pub fn record_result(
        &self,
        at: u64,
        probe: &Probe,
        result: &CheckResult,
        answer: Option<&str>,
    ) {
        let event = changes::Event::Result { result, answer };
        if changes::append_capped(
            &self.home,
            &self.session,
            probe,
            at,
            event,
            self.max_results,
        )
        .is_err()
        {
            return;
        }
        self.written(probe);
        let written = answer.map_or(0, |text| text.len().min(store::MAX_PAYLOAD_BYTES) as u64);
        let mut state = self.state();
        state.bytes_since_gc += written;
        let due = state.bytes_since_gc > GC_AFTER_BYTES
            || at.saturating_sub(state.last_gc_check) > GC_EVERY_SECS;
        if due {
            state.last_gc_check = at;
            let root = store::root(&self.home);
            let stale = store::since_last_gc(&root).is_none_or(|age| age > GC_EVERY_SECS);
            if stale || state.bytes_since_gc > GC_AFTER_BYTES {
                state.bytes_since_gc = 0;
                drop(state);
                let _ = store::gc(&root, at, &self.limits);
            }
        }
    }

    /// [`Self::record_result`] for the code as it is now (for callers that
    /// did not probe before their check started).
    pub fn record_result_now(&self, result: &CheckResult, answer: Option<&str>) {
        let probe = self.probe();
        self.record_result(crate::now_unix(), &probe, result, answer);
    }

    /// The session ended: close every change it still has open.
    pub fn end_session(&self, at: u64) {
        let keys: Vec<String> = self.state().keys.iter().cloned().collect();
        changes::end_session(&self.home, &keys, at);
    }
}

/// The model-facing text of a tool's response item.
fn response_text(item: ResponseInputItem) -> String {
    match item {
        ResponseInputItem::FunctionCallOutput { output, .. } => match output.text_content() {
            Some(text) => text.to_string(),
            None => output
                .content_items()
                .unwrap_or_default()
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        _ => String::new(),
    }
}

type SharedTool = Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>;

/// A tool whose results are recorded in the store. Spec, exposure and every
/// other property are delegated unchanged, so the model sees the same tool.
pub struct Recorded {
    inner: SharedTool,
    recorder: Arc<ThreadRecorder>,
}

impl Recorded {
    /// Wrap `tool` if this thread records results; otherwise return it as it is.
    pub fn wrap(tool: SharedTool, thread_store: &ExtensionData) -> SharedTool {
        match thread_store.get::<ThreadRecorder>() {
            Some(recorder) => Arc::new(Self {
                inner: tool,
                recorder,
            }),
            None => tool,
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for Recorded {
    fn tool_name(&self) -> ToolName {
        self.inner.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.inner.search_info()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.inner.supports_parallel_tool_calls()
    }

    fn manages_turn_items(&self) -> bool {
        self.inner.manages_turn_items()
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let tool = self.inner.tool_name().name;
            let call_id = call.call_id.clone();
            let payload = call.payload.clone();
            // The version is the code the check starts on; the probe runs
            // beside the tool so it adds no time to the call.
            let recorder = Arc::clone(&self.recorder);
            let probe = tokio::task::spawn_blocking(move || recorder.probe());
            let outcome = self.inner.handle(call).await;
            let text = match &outcome {
                Ok(output) => response_text(output.to_response_item(&call_id, &payload)),
                Err(FunctionCallError::RespondToModel(message))
                | Err(FunctionCallError::Fatal(message)) => message.clone(),
            };
            if let Some(result) = CheckResult::from_tool_text(&tool, &text)
                && let Ok(probe) = probe.await
            {
                let recorder = Arc::clone(&self.recorder);
                let _ = tokio::task::spawn_blocking(move || {
                    recorder.record_result(crate::now_unix(), &probe, &result, Some(&text));
                })
                .await;
            }
            outcome
        })
    }
}

/// The contributors behind [`install`].
struct RecorderExtension<C> {
    settings: fn(&C) -> Option<Settings>,
    _config: PhantomData<fn(&C)>,
}

impl<C: Sync + 'static> ThreadLifecycleContributor<C> for RecorderExtension<C> {
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(settings) = (self.settings)(input.config) {
                let session = session_for(
                    input.session_source,
                    input.thread_store.level_id(),
                    |name| std::env::var(name).ok(),
                );
                input
                    .thread_store
                    .insert(ThreadRecorder::for_session(settings, session));
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        let recorder = input.thread_store.get::<ThreadRecorder>();
        Box::pin(async move {
            if let Some(recorder) = recorder {
                let _ =
                    tokio::task::spawn_blocking(move || recorder.end_session(crate::now_unix()))
                        .await;
            }
        })
    }
}

/// The session a thread records under: an engine thread is its thread id; the toolbox
/// (serving another agent over MCP) is that agent's session id, else a random id.
pub fn session_for(
    source: &SessionSource,
    thread_id: &str,
    env: impl Fn(&str) -> Option<String>,
) -> String {
    if matches!(source, SessionSource::Mcp) {
        agent_session(env).unwrap_or_else(|| format!("toolbox-{}", uuid::Uuid::new_v4().simple()))
    } else {
        thread_id.to_string()
    }
}

/// Is this finished tool call an edit by the AI?
pub fn is_ai_edit(tool_name: &ToolName, outcome: ToolCallOutcome) -> bool {
    // Core reports its own tools in the default namespace (`functions`); an
    // MCP server's tool of the same name is in its own namespace.
    tool_name.is_default_namespace()
        && tool_name.name == EDIT_TOOL
        && outcome == (ToolCallOutcome::Completed { success: true })
}

impl<C: Sync + 'static> ToolLifecycleContributor for RecorderExtension<C> {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let recorder = is_ai_edit(input.tool_name, input.outcome)
            .then(|| input.thread_store.get::<ThreadRecorder>())
            .flatten();
        Box::pin(async move {
            if let Some(recorder) = recorder {
                let _ = tokio::task::spawn_blocking(move || {
                    let probe = recorder.probe();
                    recorder.record_edit(crate::now_unix(), &probe);
                })
                .await;
            }
        })
    }
}

/// Register the recorder: `settings` says, from a thread's config, whether
/// the thread records and where (`None` = it does not, e.g. a thread that
/// gets no ByteAsk tools).
pub fn install<C: Sync + 'static>(
    builder: &mut ExtensionRegistryBuilder<C>,
    settings: fn(&C) -> Option<Settings>,
) {
    let extension = Arc::new(RecorderExtension {
        settings,
        _config: PhantomData,
    });
    builder.thread_lifecycle_contributor(extension.clone());
    builder.tool_lifecycle_contributor(extension);
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
