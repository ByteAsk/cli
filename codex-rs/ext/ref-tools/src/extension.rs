use std::sync::Arc;

use codex_core::config::Config;
use codex_extension_api::ContentItemKind;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_features::Feature;
use codex_features::Features;

use crate::guidance::REF_GUIDANCE;
use crate::tools;

/// Should this thread see none of our tools and none of our guidance?
///
/// Upstream starts *ephemeral structured threads* for internal work (auto-recap
/// via `tui/src/temporary_structured_request.rs`, guardian review, the memories
/// phase-2 agent). Those prompts carry untrusted transcript text, so upstream
/// disables every built-in and extension tool on them — each upstream extension
/// by its own `features.*` flag. Ours have no flag, so all 9 `*_ref` tools
/// reached those threads, and their outbound calls to `mcp.byteask.ai` are an
/// exfiltration channel under prompt injection. This is the containment
/// boundary, not tidiness.
///
/// `ephemeral` alone is not enough: `byteask exec --ephemeral` is a real user
/// flag (`exec/src/cli.rs`) and must keep its tools. Shell alone is not enough
/// either: a user who merely turned the shell off would lose all 9 read-only
/// documentation tools. Both together have no false positive we could construct.
///
/// ⚠️ `shell_enabled` is `Feature::ShellTool` **alone**, deliberately NOT
/// `ShellTool || UnifiedExec`. `UnifiedExec` cannot be
/// switched off by ordinary config — `ManagedFeatures::normalize_candidate`
/// force-enables it unless an enterprise `requirements.toml` pins it (upstream's
/// own `legacy_unified_exec_disable_flags_do_not_disable_command_execution`
/// asserts exactly that), so `|| UnifiedExec` would be `|| true` and the gate
/// would never fire — including on the recap thread this exists for.
///
/// This function is duplicated verbatim in `codex-cpp-tools` (a shared
/// crate for one boolean is heavier than the duplication). The two copies are
/// pinned to the same answer by `gate_decision_is_identical_in_both_crates` in
/// `ext/cpp-tools/src/extension.rs`, which dev-depends on this crate.
pub fn tools_gated(ephemeral: bool, features: &Features) -> bool {
    let shell_enabled = features.enabled(Feature::ShellTool);
    ephemeral && !shell_enabled
}

/// [`tools_gated`] applied to the thread's resolved [`Config`].
fn tools_gated_for(config: &Config) -> bool {
    tools_gated(config.ephemeral, config.features.get())
}

/// Extension handle registered as every contributor flavor.
struct RefToolsExtension;

/// Thread-scoped state seeded at thread start.
///
/// The reference tools need no other per-thread state (they call the ByteAsk MCP
/// directly); this exists solely so `tools()` and the guidance can see the gate
/// decision, which is only computable from `Config` at thread start.
#[derive(Clone)]
struct RefToolsState {
    gated: bool,
    /// `policy.toml` says local only, so every lookup answers NOT RUN
    /// (offline) instead of reaching the reference service.
    local_only: bool,
    /// `[byteask_harness] disable_tools` removes every reference tool: the guidance, which
    /// names them, goes too.
    all_disabled: bool,
}

/// Every reference tool matches the session's `disable_tools` patterns.
fn all_ref_tools_disabled(config: &Config) -> bool {
    let patterns = codex_core::byteask_harness::disabled_tool_patterns(config);
    !patterns.is_empty()
        && tools::REF_TOOL_NAMES
            .iter()
            .all(|name| codex_core::byteask_harness::matches_any(&patterns, name))
}

impl ThreadLifecycleContributor<Config> for RefToolsExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.thread_store.insert(RefToolsState {
                gated: tools_gated_for(input.config),
                local_only: byteask_policy::local_only(input.config.codex_home.as_path()).0,
                all_disabled: all_ref_tools_disabled(input.config),
            });
        })
    }
}

impl ToolContributor for RefToolsExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        // Exposed in every thread (task-driven, like cpp-tools): the model calls
        // e.g. autosar_ref only for a standards question, so there is nothing to
        // gate on the cwd. The one exception is upstream's ephemeral structured
        // threads — see [`tools_gated`]. Fail closed when the thread never ran
        // `on_thread_start`: both contributors are installed together
        // (`app-server/src/extensions.rs`), so a missing state means an unknown
        // host, not a normal session.
        let Some(state) = thread_store.get::<RefToolsState>() else {
            return Vec::new();
        };
        if state.gated {
            return Vec::new();
        }
        // Every error carries a NOT RUN result line; specs and every
        // other property are delegated unchanged.
        let local_only = state.local_only;
        tools::ref_tools()
            .into_iter()
            .map(|tool| {
                if local_only {
                    crate::offline::Offline::wrap(tool, "ByteAsk's reference service")
                } else {
                    tool
                }
            })
            .map(crate::results::WithResult::wrap)
            .collect()
    }
}

impl ToolLifecycleContributor for RefToolsExtension {
    /// Backstop logging: [`crate::report::report`] logs successful outputs; this
    /// records error/blocked/aborted reference calls so the dashboard captures
    /// every invocation.
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let tool = input.tool_name.to_string();
        let outcome = match input.outcome {
            ToolCallOutcome::Completed { .. } => None,
            ToolCallOutcome::Blocked => Some("blocked"),
            ToolCallOutcome::Failed { .. } => Some("failed"),
            ToolCallOutcome::Aborted => Some("aborted"),
        };
        Box::pin(async move {
            if let Some(outcome) = outcome
                && tools::is_ref_tool(&tool)
            {
                crate::report::log_tool_outcome(&tool, outcome);
            }
        })
    }
}

impl ContextContributor for RefToolsExtension {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            // Gated threads get no guidance either: otherwise the recap
            // ships — and pays for — instructions describing 9 absent tools.
            let gated = thread_store
                .get::<RefToolsState>()
                .map(|state| state.gated || state.all_disabled)
                .unwrap_or(true);
            if gated {
                return Vec::new();
            }
            // The host joins a developer message's parts with nothing in between; the blank
            // line keeps the next part from running into this guidance's last sentence.
            vec![PromptFragment::developer_capability(
                format!("{REF_GUIDANCE}\n\n"),
                ContentItemKind("ref_tools.guidance".to_string()),
            )]
        })
    }
}

/// Registers the reference tooling extension's contributors with the host registry.
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(RefToolsExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.tool_contributor(extension.clone());
    builder.tool_lifecycle_contributor(extension.clone());
    builder.prompt_contributor(extension);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_extension_api::ToolName;
    use pretty_assertions::assert_eq;

    fn features(shell: bool, unified_exec: bool) -> Features {
        let mut features = Features::with_defaults();
        features.set_enabled(Feature::ShellTool, shell);
        features.set_enabled(Feature::UnifiedExec, unified_exec);
        features
    }

    /// Seed a thread store the way `on_thread_start` does for a config carrying
    /// these knobs. The gate decision itself runs for real.
    fn thread_store_for(ephemeral: bool, shell: bool) -> ExtensionData {
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(RefToolsState {
            gated: tools_gated(ephemeral, &features(shell, /*unified_exec*/ true)),
            local_only: false,
            all_disabled: false,
        });
        thread_store
    }

    async fn guidance_fragments(thread_store: &ExtensionData) -> Vec<PromptFragment> {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        extension
            .contribute_thread_context(&session_store, thread_store)
            .await
    }

    /// The containment case: upstream's auto-recap / guardian / memories threads.
    /// The `*_ref` tools reach the network, so their absence is the boundary.
    #[tokio::test]
    async fn ephemeral_thread_without_shell_gets_no_tools_and_no_guidance() {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ true, /*shell*/ false);
        assert!(extension.tools(&session_store, &thread_store).is_empty());
        assert!(guidance_fragments(&thread_store).await.is_empty());
    }

    /// `byteask exec --ephemeral` is a real user flag and must keep every tool.
    #[tokio::test]
    async fn ephemeral_thread_with_shell_keeps_tools_and_guidance() {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ true, /*shell*/ true);
        assert_eq!(extension.tools(&session_store, &thread_store).len(), 9);
        assert_eq!(guidance_fragments(&thread_store).await.len(), 1);
    }

    /// `disable_tools` covering every reference tool drops the guidance that names them; a list
    /// that leaves one of them keeps it.
    #[tokio::test]
    async fn disabling_every_reference_tool_drops_the_guidance() {
        let cloud_agent = [
            "*_ref",
            "ref_*",
            "cpp_intrinsic",
            "cpp_godbolt",
            "cpp_insights",
        ];
        let patterns: Vec<String> = cloud_agent.iter().map(|s| (*s).to_string()).collect();
        assert!(
            tools::REF_TOOL_NAMES
                .iter()
                .all(|name| codex_core::byteask_harness::matches_any(&patterns, name))
        );
        let partial: Vec<String> = vec!["*_ref".to_string()];
        assert!(
            !tools::REF_TOOL_NAMES
                .iter()
                .all(|name| codex_core::byteask_harness::matches_any(&partial, name))
        );
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(RefToolsState {
            gated: false,
            local_only: false,
            all_disabled: true,
        });
        assert!(guidance_fragments(&thread_store).await.is_empty());
    }

    /// A user who merely turned the shell off keeps the documentation tools.
    #[tokio::test]
    async fn non_ephemeral_thread_without_shell_keeps_tools_and_guidance() {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ false, /*shell*/ false);
        assert_eq!(extension.tools(&session_store, &thread_store).len(), 9);
        assert_eq!(guidance_fragments(&thread_store).await.len(), 1);
    }

    /// A local-only home keeps all nine tools (same specs), and each
    /// answers NOT RUN (offline) without reaching the reference service.
    #[tokio::test]
    async fn local_only_answers_offline_without_a_lookup() {
        use codex_extension_api::ConversationHistory;
        use codex_extension_api::NoopTurnItemEmitter;
        use codex_extension_api::ToolCallSource;
        use codex_extension_api::ToolPayload;
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(RefToolsState {
            gated: false,
            local_only: true,
            all_disabled: false,
        });
        let tools = extension.tools(&session_store, &thread_store);
        assert_eq!(tools.len(), 9);
        for tool in &tools {
            let name = tool.tool_name().name;
            let payload = ToolPayload::Function {
                arguments: r#"{"query":"ITCH add order","ref_id":"x","request":"y"}"#.to_string(),
            };
            let outcome = tool
                .handle(codex_extension_api::ToolCall {
                    turn_id: "t".to_string(),
                    call_id: "c".to_string(),
                    tool_name: tool.tool_name(),
                    model: "m".to_string(),
                    codex_turn_metadata: None,
                    truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
                    source: ToolCallSource::Direct,
                    conversation_history: ConversationHistory::default(),
                    turn_item_emitter: std::sync::Arc::new(NoopTurnItemEmitter),
                    environments: Vec::new(),
                    payload,
                })
                .await;
            let Err(codex_extension_api::FunctionCallError::RespondToModel(text)) = outcome else {
                panic!("{name}: expected an offline answer");
            };
            let result = check_result::CheckResult::from_tool_text(&name, &text).expect("result line");
            assert_eq!(result.reason, Some(check_result::NotRunReason::Offline), "{name}: {text}");
        }
    }

    /// Fail closed: a host that never ran `on_thread_start` gets nothing.
    #[tokio::test]
    async fn no_tools_or_guidance_without_thread_state() {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        assert!(extension.tools(&session_store, &thread_store).is_empty());
        assert!(guidance_fragments(&thread_store).await.is_empty());
    }

    /// Pins that `UnifiedExec` being on does not lift the gate — see [`tools_gated`].
    #[test]
    fn unified_exec_being_on_does_not_ungate_a_shell_less_ephemeral_thread() {
        assert!(tools_gated(
            /*ephemeral*/ true,
            &features(/*shell*/ false, /*unified_exec*/ true)
        ));
    }

    #[test]
    fn contributes_nine_reference_tools() {
        let extension = RefToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ false, /*shell*/ true);
        let names: Vec<ToolName> = extension
            .tools(&session_store, &thread_store)
            .iter()
            .map(|tool| tool.tool_name())
            .collect();
        assert_eq!(
            names,
            vec![
                ToolName::plain("autosar_ref"),
                ToolName::plain("mcu_ref"),
                ToolName::plain("fpga_ref"),
                ToolName::plain("hft_ref"),
                ToolName::plain("grid_ref"),
                ToolName::plain("scpi_ref"),
                ToolName::plain("x86_ref"),
                ToolName::plain("ref_context"),
                ToolName::plain("ref_request"),
            ]
        );
    }
}
