//! Registration with the host: thread start, the tools, the guidance.

use std::sync::Arc;

use codex_core::config::Config;
use codex_extension_api::ContentItemKind;
use codex_extension_api::ContextContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::TurnGateContributor;
use codex_extension_api::TurnGateFragments;
use codex_extension_api::TurnGateInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_features::Feature;
use codex_features::Features;
use codex_protocol::user_input::UserInput;

use crate::backend::Backend;
use crate::backend::EngineBackend;
use crate::engine;
use crate::guidance::CONTEXT_GUIDANCE;
use crate::harness::ContextHarness;
use crate::harness::EngineCli;
use crate::tools::ContextTool;

/// Upstream's ephemeral structured threads (auto-recap, guardian review,
/// memories) get no tools: the same rule, word for word, as the C/C++ and
/// reference tools (see `codex_ref_tools::tools_gated` for why it is
/// `ShellTool` alone). Pinned identical by a test.
pub fn tools_gated(ephemeral: bool, features: &Features) -> bool {
    let shell_enabled = features.enabled(Feature::ShellTool);
    ephemeral && !shell_enabled
}

/// What a thread knows about the context engine, from its start.
#[derive(Clone)]
struct ContextState {
    /// `None` = no index covers this workspace (or the thread is gated).
    backend: Option<Arc<dyn Backend>>,
    /// The turn-0 pack and the finish check (root threads with an index, when switched on in
    /// `[byteask_harness]`).
    harness: Option<Arc<ContextHarness>>,
}

/// A model-visible message from the Context harness side (a separate developer message).
struct ContextFragment {
    kind: &'static str,
    text: String,
}

impl ContextualUserFragment for ContextFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(format!("context_tools.{}", self.kind))
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

struct ContextToolsExtension;

impl ThreadLifecycleContributor<Config> for ContextToolsExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let config = input.config;
            let workspace = if tools_gated(config.ephemeral, config.features.get()) {
                None
            } else {
                engine::workspace_for(config.codex_home.as_path(), config.cwd.as_path())
            };
            let backend = workspace
                .as_ref()
                .map(|workspace| Arc::new(EngineBackend::new(workspace)) as Arc<dyn Backend>);
            // Root threads only: a sub-agent's work is checked when its parent finishes. The
            // engine is the installed one that covers the workspace, else the session's MCP
            // server entry for it.
            let pack = codex_core::byteask_harness::flag(config, "context_pack");
            let finish_check = codex_core::byteask_harness::flag(config, "context_finish_check");
            let harness = if (pack || finish_check)
                && !input.session_source.is_non_root_agent()
                && !tools_gated(config.ephemeral, config.features.get())
            {
                let cwd = config.cwd.as_path();
                workspace
                    .as_ref()
                    .map(|workspace| EngineCli::from_workspace(workspace, cwd))
                    .or_else(|| {
                        codex_core::byteask_harness::context_engine_server(config).and_then(
                            |server| {
                                EngineCli::from_mcp_server(
                                    &server.command,
                                    &server.args,
                                    &server.env,
                                    cwd,
                                )
                            },
                        )
                    })
                    .and_then(|cli| ContextHarness::new(cli, pack, finish_check))
                    .map(Arc::new)
            } else {
                None
            };
            input.thread_store.insert(ContextState { backend, harness });
        })
    }
}

impl ToolContributor for ContextToolsExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        // Fail closed: no state = an unknown host.
        let Some(backend) = thread_store
            .get::<ContextState>()
            .and_then(|state| state.backend.clone())
        else {
            return Vec::new();
        };
        ContextTool::all(backend)
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>)
            // Each answer's result goes into the local results store, so the
            // value report can say how often the index was asked and answered.
            .map(|tool| results_store::Recorded::wrap(tool, thread_store))
            .collect()
    }
}

impl ContextContributor for ContextToolsExtension {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            let indexed = thread_store
                .get::<ContextState>()
                .is_some_and(|state| state.backend.is_some());
            if !indexed {
                return Vec::new();
            }
            // The host joins a developer message's parts with nothing in between; the blank
            // line keeps the next part from running into this guidance's last sentence.
            vec![PromptFragment::developer_capability(
                format!("{CONTEXT_GUIDANCE}\n\n"),
                ContentItemKind("context_tools.guidance".to_string()),
            )]
        })
    }
}

impl TurnInputContributor for ContextToolsExtension {
    /// The turn-0 pack, after the user's first message and before the first model request.
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        _extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(harness) = thread_store
                .get::<ContextState>()
                .and_then(|state| state.harness.clone())
            else {
                return Vec::new();
            };
            let prompt = input
                .user_input
                .iter()
                .filter_map(|item| match item {
                    UserInput::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            match harness.turn0_pack(thread_store.level_id(), &prompt).await {
                Some(text) => vec![Box::new(ContextFragment {
                    kind: "turn0_pack",
                    text,
                }) as Box<dyn ContextualUserFragment + Send>],
                None => Vec::new(),
            }
        })
    }
}

impl TurnGateContributor for ContextToolsExtension {
    /// The Context finish check, before the turn may end.
    fn before_stop<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let Some(harness) = input
                .thread_store
                .get::<ContextState>()
                .and_then(|state| state.harness.clone())
            else {
                return Vec::new();
            };
            match harness
                .finish_check(input.thread_store.level_id(), input.last_agent_message)
                .await
            {
                Some(text) => vec![Box::new(ContextFragment {
                    kind: "finish_check",
                    text,
                }) as Box<dyn ContextualUserFragment + Send>],
                None => Vec::new(),
            }
        })
    }
}

/// Register the context tools with the host registry.
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(ContextToolsExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.tool_contributor(extension.clone());
    builder.turn_input_contributor(extension.clone());
    builder.turn_gate_contributor(extension.clone());
    builder.prompt_contributor(extension);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Outcome;
    use codex_extension_api::ToolName;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    struct Canned;

    impl Backend for Canned {
        fn worktree(&self) -> Option<&str> {
            Some("/src/feed")
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _arguments: serde_json::Map<String, serde_json::Value>,
            _timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send + 'a>> {
            Box::pin(async { Outcome::BadArguments("canned".into()) })
        }
    }

    fn store_with(backend: Option<Arc<dyn Backend>>) -> ExtensionData {
        let store = ExtensionData::new("thread");
        store.insert(ContextState {
            backend,
            harness: None,
        });
        store
    }

    #[tokio::test]
    async fn an_indexed_workspace_gets_every_tool_and_the_guidance() {
        let extension = ContextToolsExtension;
        let session = ExtensionData::new("session");
        let thread = store_with(Some(Arc::new(Canned)));
        let names: Vec<ToolName> = extension
            .tools(&session, &thread)
            .iter()
            .map(|tool| tool.tool_name())
            .collect();
        assert_eq!(
            names,
            crate::tools::TOOL_NAMES.map(ToolName::plain).to_vec()
        );
        let guidance = extension.contribute_thread_context(&session, &thread).await;
        assert_eq!(guidance.len(), 1);
    }

    #[tokio::test]
    async fn without_an_index_there_are_no_tools_and_no_guidance() {
        let extension = ContextToolsExtension;
        let session = ExtensionData::new("session");
        let thread = store_with(None);
        assert!(extension.tools(&session, &thread).is_empty());
        assert!(
            extension
                .contribute_thread_context(&session, &thread)
                .await
                .is_empty()
        );
        // A host that never ran thread start gets nothing either.
        let unknown = ExtensionData::new("thread");
        assert!(extension.tools(&session, &unknown).is_empty());
        assert!(
            extension
                .contribute_thread_context(&session, &unknown)
                .await
                .is_empty()
        );
    }

    /// Same containment rule as the C/C++ and reference tools.
    #[test]
    fn the_gate_is_the_same_as_the_reference_tools() {
        for ephemeral in [false, true] {
            for shell in [false, true] {
                for unified in [false, true] {
                    let mut features = Features::with_defaults();
                    features.set_enabled(Feature::ShellTool, shell);
                    features.set_enabled(Feature::UnifiedExec, unified);
                    assert_eq!(
                        tools_gated(ephemeral, &features),
                        codex_ref_tools::tools_gated(ephemeral, &features),
                        "ephemeral={ephemeral} shell={shell} unified={unified}"
                    );
                }
            }
        }
    }
}
