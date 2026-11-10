//! The one list of ByteAsk tools, and a toolbox "thread" built from it.
//!
//! [`install_byteask_tools`] is called by the app-server (which the TUI and
//! `byteask exec` run) AND by the toolbox, so the two hosts cannot offer
//! different tools: there is no second list to drift.
//! [`ToolboxThread::start`] seeds the same per-thread state the TUI seeds at
//! thread start, from the same `Config`, by running the very same
//! thread-lifecycle contributors, then asks the same tool contributors for
//! their tools. Descriptions therefore come from the same `spec()` calls the
//! description snapshots pin.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_core::config::Config;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SessionSource;
use codex_utils_output_truncation::TruncationPolicy;

/// A tool bound to one thread's state.
pub type SharedTool = Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>;

/// Install every ByteAsk tool (the C/C++ tools and the reference tools).
///
/// The single source of the ByteAsk tool set. The app-server calls this for the
/// TUI and `byteask exec`; the toolbox calls it for Claude Code and Codex. Add a
/// ByteAsk tool by adding it to its crate's contributor, never by registering
/// it in one host only.
pub fn install_byteask_tools(builder: &mut ExtensionRegistryBuilder<Config>) {
    codex_cpp_tools::install(builder);
    codex_ref_tools::install(builder);
    // ByteAsk Context: the turn-0 pack's test programs are built in the background, in the
    // tree cpp_finish_check uses, when the session may run the project's build. When the
    // pack names none, the tests covering its place 1 are picked from the project's build.
    codex_core::byteask_context::set_warm_build(start_context_warm_build);
    // The agent's finished build and test commands reach the finish review.
    codex_core::byteask_context::set_agent_command_hook(codex_cpp_tools::note_agent_command);
    // ByteAsk Context ships as its own hosted MCP service (0.2.0): the local context engine's
    // tools are not installed in the CLI. `byteask context connect` adds the hosted service.
    // Every C/C++ tool result goes into the local results store.
    results_store::install(builder, results_store_settings);
}

fn start_context_warm_build(
    config: &Config,
    workspace: &std::path::Path,
    test_programs: &[String],
    pack: &str,
) -> Option<codex_core::byteask_context::WarmBuildStarted> {
    if !codex_cpp_tools::native_exec_allowed(config) {
        return None;
    }
    let places = if test_programs.is_empty() {
        codex_cpp_tools::context_place_files(pack, workspace)
    } else {
        Vec::new()
    };
    if test_programs.is_empty() && places.is_empty() {
        return None;
    }
    let handle = codex_cpp_tools::start_warm_build_for(workspace, test_programs, &places);
    // The line's commands build in the tools' own tree, outside the workspace: offered only
    // when the model's commands may write there unasked.
    let check_line = (!places.is_empty() && codex_cpp_tools::full_access(config)).then(|| {
        let handle = handle.clone();
        Box::pin(async move {
            handle
                .check_plan()
                .await
                .map(|plan| codex_core::byteask_context::CheckLine {
                    line: plan.line,
                    targets: plan.targets,
                })
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    });
    Some(codex_core::byteask_context::WarmBuildStarted {
        cancel: Box::new(move || handle.cancel()),
        check_line,
    })
}

/// Record results in every thread that gets ByteAsk's tools (the gate is
/// cpp-tools' own, so the two cannot disagree).
fn results_store_settings(config: &Config) -> Option<results_store::Settings> {
    if codex_cpp_tools::tools_gated(config.ephemeral, config.features.get()) {
        return None;
    }
    Some(results_store::Settings {
        home: config.codex_home.to_path_buf(),
        cwd: config.cwd.to_path_buf(),
    })
}

/// What a tool call produced, in host-neutral form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallOutcome {
    /// The text the model reads: the same text the CLI's model receives.
    pub text: String,
    /// The tool returned an error rather than a normal output.
    pub is_error: bool,
}

/// One toolbox session's tools and guidance, seeded like a TUI thread.
pub struct ToolboxThread {
    registry: ExtensionRegistry<Config>,
    session_store: ExtensionData,
    thread_store: ExtensionData,
    tools: Vec<SharedTool>,
    guidance: Vec<String>,
    next_call: AtomicU64,
}

impl ToolboxThread {
    /// Seed thread state from `config` exactly as a TUI thread start does.
    pub async fn start(config: &Config) -> Self {
        Self::start_with(config, install_byteask_tools).await
    }

    /// The same, with only the tools `install` registers (the trial's own
    /// check runner uses the C/C++ tools alone, so it records nothing in the
    /// results store).
    pub async fn start_with(
        config: &Config,
        install: fn(&mut ExtensionRegistryBuilder<Config>),
    ) -> Self {
        let mut builder = ExtensionRegistryBuilder::<Config>::new();
        install(&mut builder);
        let registry = builder.build();
        let session_store = ExtensionData::new("toolbox-session");
        let thread_store = ExtensionData::new("toolbox-thread");
        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config,
                    session_source: &SessionSource::Mcp,
                    persistent_thread_state_available: false,
                    environments: &[],
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store: &thread_store,
                })
                .await;
        }
        let tools = registry
            .tool_contributors()
            .iter()
            .flat_map(|contributor| contributor.tools(&session_store, &thread_store))
            .collect();
        let mut guidance = Vec::new();
        for contributor in registry.context_contributors() {
            for fragment in contributor
                .contribute_thread_context(&session_store, &thread_store)
                .await
            {
                // The TUI's fragments end with their own separator; the toolbox joins them
                // itself (`server.rs`), so the served instructions stay as they were.
                guidance.push(fragment.text().trim_end().to_string());
            }
        }
        Self {
            registry,
            session_store,
            thread_store,
            tools,
            guidance,
            next_call: AtomicU64::new(1),
        }
    }

    /// The guidance prompt fragments the TUI would add for this workspace.
    pub fn guidance(&self) -> &[String] {
        &self.guidance
    }

    /// Every tool's spec, in registry order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    fn find(&self, name: &str) -> Option<&SharedTool> {
        let wanted = ToolName::plain(name);
        self.tools.iter().find(|tool| tool.tool_name() == wanted)
    }

    /// Does this thread offer a tool with this name?
    pub fn has_tool(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Run one tool through its real `handle` entry point, as core does, and
    /// report the outcome to the lifecycle contributors (logging backstop).
    /// `None` = no such tool.
    pub async fn call(&self, name: &str, arguments: String) -> Option<CallOutcome> {
        let tool = self.find(name)?.clone();
        let call_number = self.next_call.fetch_add(1, Ordering::Relaxed);
        let call_id = format!("toolbox-call-{call_number}");
        let turn_id = format!("toolbox-turn-{call_number}");
        let tool_name = tool.tool_name();
        let payload = ToolPayload::Function { arguments };
        let result = tool
            .handle(ToolCall {
                turn_id: turn_id.clone(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                // The model belongs to the calling agent, not to ByteAsk.
                model: String::new(),
                codex_turn_metadata: None,
                // No ByteAsk tool reads this today; the harness applies its own
                // output limit (Claude Code: 25,000 tokens).
                truncation_policy: TruncationPolicy::Bytes(1 << 20),
                source: ToolCallSource::Direct,
                conversation_history: ConversationHistory::default(),
                turn_item_emitter: Arc::new(NoopTurnItemEmitter),
                environments: Vec::new(),
                payload: payload.clone(),
            })
            .await;
        let (outcome, lifecycle) = match result {
            Ok(output) => {
                let success = output.success_for_logging();
                (
                    CallOutcome {
                        text: response_text(output.to_response_item(&call_id, &payload)),
                        is_error: false,
                    },
                    ToolCallOutcome::Completed { success },
                )
            }
            Err(FunctionCallError::RespondToModel(message)) => (
                CallOutcome {
                    text: message,
                    is_error: true,
                },
                ToolCallOutcome::Failed {
                    handler_executed: true,
                },
            ),
            Err(FunctionCallError::Fatal(message)) => (
                CallOutcome {
                    text: format!("Fatal error: {message}"),
                    is_error: true,
                },
                ToolCallOutcome::Failed {
                    handler_executed: true,
                },
            ),
        };
        let turn_store = ExtensionData::new(turn_id.clone());
        for contributor in self.registry.tool_lifecycle_contributors() {
            contributor
                .on_tool_finish(ToolFinishInput {
                    session_store: &self.session_store,
                    thread_store: &self.thread_store,
                    turn_store: &turn_store,
                    turn_id: &turn_id,
                    call_id: &call_id,
                    tool_name: &tool_name,
                    source: ToolCallSource::Direct,
                    outcome: lifecycle,
                })
                .await;
        }
        Some(outcome)
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
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => text.clone(),
                    FunctionCallOutputContentItem::InputImage { .. } => "[image]".to_string(),
                    FunctionCallOutputContentItem::InputAudio { .. } => "[audio]".to_string(),
                    FunctionCallOutputContentItem::EncryptedContent { .. } => {
                        "[encrypted content]".to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        other => serde_json::to_string(&other).unwrap_or_default(),
    }
}
