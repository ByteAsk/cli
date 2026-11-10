use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Weak;

use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_tools::ConversationHistory;
use codex_tools::ExtensionTurnItem;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolEnvironment;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::TurnItemEmissionFuture;
use codex_tools::TurnItemEmitter;
use codex_utils_string::to_ascii_json_string;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::lifecycle::extension_tool_call_source;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_metadata::McpTurnMetadataContext;

pub(crate) struct ExtensionToolAdapter(
    Arc<dyn for<'call> codex_tools::ToolExecutor<ExtensionToolCall<'call>>>,
);

impl ExtensionToolAdapter {
    pub(crate) fn new(
        executor: Arc<dyn for<'call> codex_tools::ToolExecutor<ExtensionToolCall<'call>>>,
    ) -> Self {
        Self(executor)
    }
}

impl ToolExecutor<ToolInvocation> for ExtensionToolAdapter {
    fn tool_name(&self) -> ToolName {
        self.0.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }

    fn exposure(&self) -> crate::tools::registry::ToolExposure {
        self.0.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.0.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.0.search_info()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            // Executors that emit their own turn items (web_search, image_gen)
            // opt out via manages_turn_items(); everything else gets a generic
            // visible tool-call item around the call. Emitted as the core
            // `TurnItem::DynamicToolCall` (already wired through conversion,
            // history, and client rendering) — no new wire kind.
            if self.0.manages_turn_items() {
                let call = to_extension_call(&invocation).await;
                return self.0.handle(call).await;
            }

            let session = Arc::clone(&invocation.session);
            let turn = Arc::clone(&invocation.turn);
            let call_id = invocation.call_id.clone();
            let tool = invocation.tool_name.to_string();
            let arguments = tool_call_arguments(&invocation.payload);
            let payload = invocation.payload.clone();

            session
                .emit_turn_item_started(
                    turn.as_ref(),
                    &TurnItem::DynamicToolCall(DynamicToolCallItem {
                        id: call_id.clone(),
                        namespace: None,
                        tool: tool.clone(),
                        arguments: arguments.clone(),
                        status: DynamicToolCallStatus::InProgress,
                        content_items: None,
                        success: None,
                        error: None,
                        duration: None,
                        host_executed: true,
                    }),
                )
                .await;

            let started_at = std::time::Instant::now();
            // Turn aborts hard-cancel this future (parallel.rs aborts the
            // dispatch task), which would otherwise strand the InProgress item
            // in thread history forever. The guard emits a terminal Failed
            // item from Drop unless disarmed by normal completion below.
            let mut abort_guard = ToolCallAbortGuard {
                session: Arc::downgrade(&session),
                turn: Arc::downgrade(&turn),
                item: Some(DynamicToolCallItem {
                    id: call_id.clone(),
                    namespace: None,
                    tool: tool.clone(),
                    arguments: arguments.clone(),
                    status: DynamicToolCallStatus::Failed,
                    content_items: Some(Vec::new()),
                    success: Some(false),
                    error: Some("tool call interrupted".to_string()),
                    duration: None,
                    host_executed: true,
                }),
            };

            let call = to_extension_call(&invocation).await;
            let result = self.0.handle(call).await;
            abort_guard.item = None; // disarm — we emit the real terminal item
            let duration = Some(started_at.elapsed());

            let item = match &result {
                Ok(output) => {
                    let (text, success) =
                        tool_output_text_and_success(output.as_ref(), &call_id, &payload);
                    DynamicToolCallItem {
                        id: call_id,
                        namespace: None,
                        tool,
                        arguments,
                        status: DynamicToolCallStatus::Completed,
                        content_items: Some(
                            text.map(|text| vec![DynamicToolCallOutputContentItem::InputText { text }])
                                .unwrap_or_default(),
                        ),
                        success: Some(success),
                        error: None,
                        duration,
                        host_executed: true,
                    }
                }
                // RespondToModel is these tools' routine soft-error channel
                // (retryable guidance the model recovers from) — render it as
                // a completed call carrying the message, NOT a red failure.
                Err(codex_tools::FunctionCallError::RespondToModel(message)) => {
                    DynamicToolCallItem {
                        id: call_id,
                        namespace: None,
                        tool,
                        arguments,
                        status: DynamicToolCallStatus::Completed,
                        content_items: Some(vec![DynamicToolCallOutputContentItem::InputText {
                            text: truncate_tool_output(message.clone()),
                        }]),
                        success: None,
                        error: None,
                        duration,
                        host_executed: true,
                    }
                }
                Err(err) => DynamicToolCallItem {
                    id: call_id,
                    namespace: None,
                    tool,
                    arguments,
                    status: DynamicToolCallStatus::Failed,
                    content_items: Some(Vec::new()),
                    success: Some(false),
                    error: Some(truncate_tool_output(err.to_string())),
                    duration,
                    host_executed: true,
                },
            };
            session
                .emit_turn_item_completed(turn.as_ref(), TurnItem::DynamicToolCall(item))
                .await;

            result
        })
    }
}

/// Max output text carried on the visible tool-call item — display surfaces
/// truncate further; this only keeps rollout persistence light.
const TOOL_CALL_ITEM_OUTPUT_CAP: usize = 4096;

/// Emits a terminal Failed item if the tool-call future is dropped before
/// completing (turn abort). Disarmed by setting `item` to `None`.
struct ToolCallAbortGuard {
    session: Weak<Session>,
    turn: Weak<TurnContext>,
    item: Option<DynamicToolCallItem>,
}

impl Drop for ToolCallAbortGuard {
    fn drop(&mut self) {
        let Some(item) = self.item.take() else {
            return;
        };
        let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
            return;
        };
        // Drop runs synchronously inside the runtime worker that dropped the
        // aborted task; the emission itself is async, so hand it off.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                session
                    .emit_turn_item_completed(turn.as_ref(), TurnItem::DynamicToolCall(item))
                    .await;
            });
        }
    }
}

/// The tool arguments as a JSON value for display; never fails (a
/// non-JSON payload is carried verbatim as a string).
fn tool_call_arguments(payload: &ToolPayload) -> serde_json::Value {
    let raw = payload.log_payload();
    serde_json::from_str(raw.as_ref())
        .unwrap_or_else(|_| serde_json::Value::String(raw.into_owned()))
}

/// Best-effort (text, success) of a tool output via its model-facing
/// response item. `to_response_item` is pure formatting, so calling it here
/// in addition to the router is safe.
fn tool_output_text_and_success(
    output: &dyn codex_tools::ToolOutput,
    call_id: &str,
    payload: &ToolPayload,
) -> (Option<String>, bool) {
    use codex_protocol::models::ResponseInputItem;
    match output.to_response_item(call_id, payload) {
        ResponseInputItem::FunctionCallOutput { output, .. } => (
            output.body.to_text().map(truncate_tool_output),
            output.success.unwrap_or(true),
        ),
        ResponseInputItem::CustomToolCallOutput { output, .. } => (
            output.body.to_text().map(truncate_tool_output),
            output.success.unwrap_or(true),
        ),
        _ => (None, output.success_for_logging()),
    }
}

fn truncate_tool_output(mut text: String) -> String {
    if text.len() > TOOL_CALL_ITEM_OUTPUT_CAP {
        let mut cut = TOOL_CALL_ITEM_OUTPUT_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("…");
    }
    text
}

impl CoreToolRuntime for ExtensionToolAdapter {
    fn is_builtin_control_tool(&self) -> bool {
        let tool_name = self.0.tool_name();
        if tool_name.is_default_namespace() {
            return matches!(
                tool_name.name.as_str(),
                "get_goal" | "create_goal" | "update_goal"
            );
        }
        matches!(
            (tool_name.namespace.as_deref(), tool_name.name.as_str()),
            (
                Some("notes"),
                "list_files_by_prefix"
                    | "read_file"
                    | "search_contents"
                    | "append_to_file"
                    | "write_file"
            ) | (
                Some("history"),
                "list_windows" | "list_items" | "read_item" | "search_contents"
            )
        )
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        match payload {
            ToolPayload::Function { .. } => true,
            ToolPayload::Custom { .. } => match self.0.spec() {
                ToolSpec::Freeform(_) => true,
                ToolSpec::Namespace(namespace) => namespace.tools.iter().any(|tool| {
                    matches!(
                        tool,
                        ResponsesApiNamespaceTool::Custom(tool)
                            if tool.name == self.0.tool_name().name
                    )
                }),
                ToolSpec::Function(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. } => false,
            },
            ToolPayload::ToolSearch { .. } => false,
        }
    }
}

struct CoreTurnItemEmitter {
    session: Weak<Session>,
    turn: Weak<TurnContext>,
}

async fn emit_legacy_events(session: &Session, turn: &TurnContext, legacy_events: Vec<EventMsg>) {
    for msg in legacy_events {
        session
            .send_event_raw(Event {
                id: turn.sub_id.clone(),
                msg,
            })
            .await;
    }
}

impl TurnItemEmitter for CoreTurnItemEmitter {
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }

    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }
}

async fn to_extension_call(invocation: &ToolInvocation) -> ExtensionToolCall<'_> {
    let conversation_history =
        ConversationHistory::new(invocation.session.clone_history().await.into_raw_items());
    let codex_turn_metadata = invocation
        .turn
        .turn_metadata_state
        .current_meta_value_for_mcp_request(McpTurnMetadataContext {
            model: invocation.turn.model_info().slug.as_str(),
            reasoning_effort: invocation.turn.effective_reasoning_effort(),
            node_repl_disabled: invocation.turn.model_info().node_repl_disabled,
        })
        .and_then(|metadata| to_ascii_json_string(&metadata).ok());
    let mut environments = Vec::new();
    for environment in invocation.step_context.environments.turn_environments() {
        // TODO(anp): Migrate extension ToolEnvironment and granted-permission lookup to PathUri
        // so extensions can receive foreign environment cwd values.
        let Ok(native_cwd) = environment.cwd().to_abs_path() else {
            continue;
        };
        let additional_permissions = apply_granted_turn_permissions(
            invocation.session.as_ref(),
            environment,
            environment.cwd(),
            SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
        )
        .await
        .additional_permissions;
        let file_system_sandbox_context = environment.sandbox_context(additional_permissions);
        environments.push(ToolEnvironment {
            _lifetime: PhantomData,
            environment_id: environment.selection.environment_id.clone(),
            cwd: native_cwd,
            file_system: environment.environment.get_filesystem(),
            file_system_sandbox_context,
        });
    }
    ExtensionToolCall {
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.clone(),
        model: invocation.turn.model_info().slug.clone(),
        codex_turn_metadata,
        truncation_policy: invocation.turn.model_info().truncation_policy.into(),
        source: extension_tool_call_source(invocation.source.clone()),
        conversation_history,
        turn_item_emitter: Arc::new(CoreTurnItemEmitter {
            session: Arc::downgrade(&invocation.session),
            turn: Arc::downgrade(&invocation.turn),
        }),
        environments,
        payload: invocation.payload.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_extension_items::ExtensionItem;
    use codex_extension_items::image_generation::ImageGenerationItem;
    use codex_extension_items::web_search::WebSearchItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ImageGenerationBeginEvent;
    use codex_protocol::protocol::ImageGenerationEndEvent;
    use codex_tools::ExtensionTurnItem;
    use codex_tools::ToolCallSource as ExtensionToolCallSource;
    use codex_utils_absolute_path::test_support::PathExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use codex_utils_path_uri::PathUri;
    use core_test_support::responses::strip_response_item_id;
    use core_test_support::responses::strip_response_item_ids;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::CoreTurnItemEmitter;
    use super::ExtensionToolAdapter;
    use codex_protocol::items::DynamicToolCallItem;
    use codex_protocol::items::DynamicToolCallStatus;
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::hook_names::HookToolName;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::PostToolUsePayload;
    use crate::tools::registry::PreToolUsePayload;
    use crate::turn_diff_tracker::TurnDiffTracker;

    struct StubExtensionExecutor;

    impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>>
        for StubExtensionExecutor
    {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Echoes arguments.".to_string(),
                strict: true,
                parameters: codex_tools::parse_tool_input_schema(&json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                }))
                .expect("extension schema should parse"),
                output_schema: None,
                defer_loading: None,
            })
        }

        fn handle<'a>(
            &'a self,
            _call: codex_tools::ToolCall<'call>,
        ) -> codex_tools::ToolExecutorFuture<'a>
        where
            'call: 'a,
        {
            Box::pin(async {
                Ok(
                    Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                        as Box<dyn codex_tools::ToolOutput>,
                )
            })
        }
    }

    struct CapturingExtensionExecutor {
        captured_call: Arc<Mutex<Option<codex_tools::ToolCall<'static>>>>,
        captured_sandbox_cwds: Arc<Mutex<Vec<Option<PathUri>>>>,
    }

    impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>>
        for CapturingExtensionExecutor
    {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("extension_echo")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "extension_echo".to_string(),
                description: "Captures arguments.".to_string(),
                strict: false,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
                defer_loading: None,
            })
        }

        // Plays the web_search role: emits its own extension item, so the
        // adapter must not add a generic tool-call item around it.
        fn manages_turn_items(&self) -> bool {
            true
        }

        fn handle<'a>(
            &'a self,
            call: codex_tools::ToolCall<'call>,
        ) -> codex_tools::ToolExecutorFuture<'a>
        where
            'call: 'a,
        {
            Box::pin(self.handle_call(call))
        }
    }

    impl CapturingExtensionExecutor {
        async fn handle_call(
            &self,
            call: codex_tools::ToolCall<'_>,
        ) -> Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError> {
            call.turn_item_emitter
                .emit_started(ExtensionTurnItem {
                    item: ExtensionItem::WebSearch(WebSearchItem {
                        id: call.call_id.clone(),
                        query: String::new(),
                        action: None,
                        results: None,
                    }),
                    legacy_events: Vec::new(),
                })
                .await;
            // Record owned metadata only; the invocation lifetime belongs to this callback.
            *self.captured_sandbox_cwds.lock().await = call
                .environments
                .iter()
                .map(|environment| environment.file_system_sandbox_context.cwd.clone())
                .collect();
            let call = codex_tools::ToolCall {
                environments: Vec::new(),
                ..call
            };
            *self.captured_call.lock().await = Some(call);
            Ok(
                Box::new(codex_tools::JsonToolOutput::new(json!({ "ok": true })))
                    as Box<dyn codex_tools::ToolOutput>,
            )
        }
    }

    #[test]
    fn function_extensions_reject_custom_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));

        assert!(handler.matches_kind(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }));
        assert!(!handler.matches_kind(&ToolPayload::Custom {
            input: "raw input".to_string(),
        }));
    }

    #[tokio::test]
    async fn exposes_generic_hook_payloads() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let turn = Arc::new(turn);
        let invocation = ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };
        let output = codex_tools::JsonToolOutput::new(json!({ "ok": true }));

        assert_eq!(
            CoreToolRuntime::pre_tool_use_payload(&handler, &invocation),
            Some(PreToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_input: json!({ "message": "hello" }),
            })
        );
        assert_eq!(
            CoreToolRuntime::post_tool_use_payload(&handler, &invocation, &output),
            Some(PostToolUsePayload {
                tool_name: HookToolName::new("extension_echo"),
                tool_use_id: "call-extension".to_string(),
                tool_input: json!({ "message": "hello" }),
                tool_response: json!({ "ok": true }),
            })
        );
    }

    #[tokio::test]
    async fn passes_turn_fields_and_scoped_turn_item_emitter_to_extension_call() {
        let captured_call = Arc::new(Mutex::new(None));
        let captured_sandbox_cwds = Arc::new(Mutex::new(Vec::new()));
        let handler = ExtensionToolAdapter::new(Arc::new(CapturingExtensionExecutor {
            captured_call: Arc::clone(&captured_call),
            captured_sandbox_cwds: Arc::clone(&captured_sandbox_cwds),
        }));
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let weak_session = Arc::downgrade(&session);
        let weak_turn = Arc::downgrade(&turn);
        let turn_id = turn.sub_id.clone();
        let model = turn.model_info().slug.clone();
        let truncation_policy = turn.model_info().truncation_policy.into();
        let expected_sandbox_cwds = turn
            .environments
            .turn_environments()
            .map(|environment| Some(environment.cwd().clone()))
            .collect::<Vec<_>>();
        let history_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "extension history".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items(&turn, std::slice::from_ref(&history_item))
            .await;
        let expected_history_item = strip_response_item_id(
            session
                .clone_history()
                .await
                .raw_items()
                .next()
                .expect("history item")
                .clone(),
        );
        let raw_history_event = rx.recv().await.expect("history raw response item event");
        let EventMsg::RawResponseItem(raw_history_item) = raw_history_event.msg else {
            panic!("expected raw response item event");
        };
        assert_eq!(
            strip_response_item_id(raw_history_item.item),
            expected_history_item
        );
        let step_context = StepContext::for_test(Arc::clone(&turn));
        let invocation = ToolInvocation {
            session,
            step_context,
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "nested-call-1".to_string(),
            },
            payload: ToolPayload::Function {
                arguments: json!({ "message": "hello" }).to_string(),
            },
        };

        crate::tools::registry::ToolExecutor::handle(&handler, invocation)
            .await
            .expect("extension call should succeed");

        let captured_call = captured_call.lock().await.clone().expect("captured call");
        assert!(weak_session.upgrade().is_none());
        assert!(weak_turn.upgrade().is_none());
        assert_eq!(captured_call.turn_id, turn_id);
        assert_eq!(captured_call.call_id, "call-extension");
        assert_eq!(
            captured_call.tool_name,
            codex_tools::ToolName::plain("extension_echo")
        );
        assert_eq!(captured_call.model, model);
        assert_eq!(captured_call.truncation_policy, truncation_policy);
        assert_eq!(
            captured_call.source,
            ExtensionToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "nested-call-1".to_string(),
            }
        );
        assert_eq!(*captured_sandbox_cwds.lock().await, expected_sandbox_cwds);
        assert_eq!(
            strip_response_item_ids(captured_call.conversation_history.items()),
            vec![expected_history_item]
        );
        match captured_call.payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, json!({ "message": "hello" }).to_string());
            }
            payload => panic!("expected function payload, got {payload:?}"),
        }

        let started = rx.recv().await.expect("item started event");
        let EventMsg::ItemStarted(started) = started.msg else {
            panic!("expected item started event");
        };
        let TurnItem::Extension(ExtensionItem::WebSearch(started_item)) = started.item else {
            panic!("expected extension web search item");
        };
        assert_eq!(
            started_item,
            WebSearchItem {
                id: "call-extension".to_string(),
                query: String::new(),
                action: None,
                results: None,
            }
        );
    }

    #[tokio::test]
    async fn image_generation_publication_preserves_extension_saved_path() {
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let expected_path = test_path_buf("/tmp/extension-claimed.png").abs();
        let emitter = CoreTurnItemEmitter {
            session: Arc::downgrade(&session),
            turn: Arc::downgrade(&turn),
        };
        let expected_started_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
            id: "call-image".to_string(),
            status: "in_progress".to_string(),
            revised_prompt: None,
            result: String::new(),
            transparent_background: None,
            failure: None,
            saved_path: None,
            imagegen_request_id: None,
        });
        let expected_completed_item = ExtensionItem::ImageGeneration(ImageGenerationItem {
            id: "call-image".to_string(),
            status: "completed".to_string(),
            revised_prompt: Some("A tiny blue square".to_string()),
            result: "cG5n".to_string(),
            transparent_background: Some(true),
            failure: None,
            saved_path: Some(expected_path.clone()),
            imagegen_request_id: None,
        });
        codex_tools::TurnItemEmitter::emit_started(
            &emitter,
            ExtensionTurnItem {
                item: expected_started_item.clone(),
                legacy_events: vec![EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                    call_id: "call-image".to_string(),
                })],
            },
        )
        .await;
        codex_tools::TurnItemEmitter::emit_completed(
            &emitter,
            ExtensionTurnItem {
                item: expected_completed_item.clone(),
                legacy_events: vec![EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                    call_id: "call-image".to_string(),
                    status: "completed".to_string(),
                    revised_prompt: Some("A tiny blue square".to_string()),
                    result: "cG5n".to_string(),
                    transparent_background: Some(true),
                    failure: None,
                    saved_path: Some(expected_path.clone()),
                })],
            },
        )
        .await;

        let started = rx.recv().await.expect("item started event");
        let EventMsg::ItemStarted(started) = started.msg else {
            panic!("expected item started event");
        };
        let TurnItem::Extension(started_item) = started.item else {
            panic!("expected extension item");
        };
        let begin = rx.recv().await.expect("legacy image start event");
        assert!(matches!(begin.msg, EventMsg::ImageGenerationBegin(_)));
        let completed = rx.recv().await.expect("item completed event");
        let EventMsg::ItemCompleted(completed) = completed.msg else {
            panic!("expected item completed event");
        };
        let TurnItem::Extension(completed_item) = completed.item else {
            panic!("expected extension item");
        };
        let end = rx.recv().await.expect("legacy image end event");
        assert!(matches!(end.msg, EventMsg::ImageGenerationEnd(_)));

        assert_eq!(started_item, expected_started_item);
        assert_eq!(completed_item, expected_completed_item);
    }

    /// Drain every pending event and return the extension tool-call items seen
    /// as (is_started, item) pairs.
    fn drain_tool_call_items(
        rx: &async_channel::Receiver<codex_protocol::protocol::Event>,
    ) -> Vec<(bool, DynamicToolCallItem)> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event.msg {
                EventMsg::ItemStarted(ev) => {
                    if let TurnItem::DynamicToolCall(item) = ev.item {
                        out.push((true, item));
                    }
                }
                EventMsg::ItemCompleted(ev) => {
                    if let TurnItem::DynamicToolCall(item) = ev.item {
                        out.push((false, item));
                    }
                }
                _ => {}
            }
        }
        out
    }

    async fn invoke_adapter(
        handler: &ExtensionToolAdapter,
        arguments: String,
    ) -> (
        Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError>,
        async_channel::Receiver<codex_protocol::protocol::Event>,
    ) {
        let (session, turn, rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let step_context = StepContext::for_test(Arc::clone(&turn));
        let invocation = ToolInvocation {
            session,
            step_context,
            turn,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-extension".to_string(),
            tool_name: codex_tools::ToolName::plain("extension_echo"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function { arguments },
        };
        let result = crate::tools::registry::ToolExecutor::handle(handler, invocation).await;
        (result, rx)
    }

    #[tokio::test]
    async fn auto_emits_tool_call_items_around_extension_call() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));
        let (result, rx) =
            invoke_adapter(&handler, json!({ "message": "hello" }).to_string()).await;
        result.expect("extension call should succeed");

        let items = drain_tool_call_items(&rx);
        assert_eq!(items.len(), 2, "expected started + completed items");
        let (started_flag, started) = &items[0];
        let (completed_flag, completed) = &items[1];
        assert!(*started_flag && !*completed_flag);
        assert_eq!(started.id, "call-extension");
        assert_eq!(started.tool, "extension_echo");
        assert_eq!(started.arguments, json!({ "message": "hello" }));
        assert_eq!(started.status, DynamicToolCallStatus::InProgress);
        assert_eq!(started.content_items, None);
        // Same id started -> completed so clients correlate + history upserts.
        assert_eq!(completed.id, started.id);
        assert_eq!(completed.status, DynamicToolCallStatus::Completed);
        assert_eq!(completed.success, Some(true));
        assert!(completed.duration.is_some());
        let output = tool_call_item_text(completed);
        assert!(output.contains("ok"), "output should carry the tool text: {output}");
    }

    #[tokio::test]
    async fn failed_extension_call_emits_failed_tool_call_item() {
        struct FailingExecutor;
        impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>> for FailingExecutor {
            fn tool_name(&self) -> codex_tools::ToolName {
                codex_tools::ToolName::plain("extension_echo")
            }
            fn spec(&self) -> codex_tools::ToolSpec {
                StubExtensionExecutor.spec()
            }
            fn handle<'a>(
                &'a self,
                _call: codex_tools::ToolCall<'call>,
            ) -> codex_tools::ToolExecutorFuture<'a>
            where
                'call: 'a,
            {
                Box::pin(async {
                    Err(codex_tools::FunctionCallError::Fatal(
                        "backend unreachable".to_string(),
                    ))
                })
            }
        }
        let handler = ExtensionToolAdapter::new(Arc::new(FailingExecutor));
        let (result, rx) = invoke_adapter(&handler, "{}".to_string()).await;
        assert!(result.is_err(), "the error must propagate unchanged");

        let items = drain_tool_call_items(&rx);
        assert_eq!(items.len(), 2);
        let (_, completed) = &items[1];
        assert_eq!(completed.status, DynamicToolCallStatus::Failed);
        assert_eq!(completed.success, Some(false));
        assert!(
            completed.error.as_deref().unwrap_or_default().contains("backend unreachable"),
            "failed item should carry the error text"
        );
    }

    #[tokio::test]
    async fn respond_to_model_error_renders_completed_not_failed() {
        struct SoftErrorExecutor;
        impl<'call> codex_extension_api::ToolExecutor<codex_tools::ToolCall<'call>> for SoftErrorExecutor {
            fn tool_name(&self) -> codex_tools::ToolName {
                codex_tools::ToolName::plain("extension_echo")
            }
            fn spec(&self) -> codex_tools::ToolSpec {
                StubExtensionExecutor.spec()
            }
            fn handle<'a>(
                &'a self,
                _call: codex_tools::ToolCall<'call>,
            ) -> codex_tools::ToolExecutorFuture<'a>
            where
                'call: 'a,
            {
                Box::pin(async {
                    Err(codex_tools::FunctionCallError::RespondToModel(
                        "unknown compiler, candidates: arm64g1430".to_string(),
                    ))
                })
            }
        }
        let handler = ExtensionToolAdapter::new(Arc::new(SoftErrorExecutor));
        let (result, rx) = invoke_adapter(&handler, "{}".to_string()).await;
        assert!(result.is_err(), "the soft error must still propagate to the router");

        let items = drain_tool_call_items(&rx);
        assert_eq!(items.len(), 2);
        let (_, completed) = &items[1];
        // Routine retryable guidance must NOT render as a red failure.
        assert_eq!(completed.status, DynamicToolCallStatus::Completed);
        assert_eq!(completed.success, None);
        assert_eq!(completed.error, None);
        assert!(tool_call_item_text(completed).contains("candidates"));
    }

    fn tool_call_item_text(item: &DynamicToolCallItem) -> String {
        item.content_items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|ci| match ci {
                codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputText {
                    text,
                } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn self_emitting_executor_gets_no_auto_tool_call_items() {
        let handler = ExtensionToolAdapter::new(Arc::new(CapturingExtensionExecutor {
            captured_call: Arc::new(Mutex::new(None)),
            captured_sandbox_cwds: Arc::new(Mutex::new(Vec::new())),
        }));
        let (result, rx) = invoke_adapter(&handler, "{}".to_string()).await;
        result.expect("extension call should succeed");

        assert_eq!(
            drain_tool_call_items(&rx).len(),
            0,
            "manages_turn_items executors must not get auto tool-call items"
        );
    }

    #[tokio::test]
    async fn non_json_arguments_fall_back_to_string() {
        let handler = ExtensionToolAdapter::new(Arc::new(StubExtensionExecutor));
        let (result, rx) = invoke_adapter(&handler, "not json".to_string()).await;
        result.expect("extension call should succeed");

        let items = drain_tool_call_items(&rx);
        assert_eq!(items[0].1.arguments, json!("not json"));
    }

    #[test]
    fn truncate_tool_output_caps_on_char_boundary() {
        let long = "é".repeat(super::TOOL_CALL_ITEM_OUTPUT_CAP); // 2 bytes each
        let out = super::truncate_tool_output(long);
        assert!(out.len() <= super::TOOL_CALL_ITEM_OUTPUT_CAP + '…'.len_utf8());
        assert!(out.ends_with('…'));
    }
}
