use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn failed_turn_does_not_overwrite_output_last_message_file() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("last-message.txt");
    std::fs::write(&output_path, "keep existing contents").expect("seed output file");

    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification::new(
            "thread-1".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::AgentMessage {
                id: "msg-1".to_string(),
                text: "partial answer".to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 0,
        ),
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(processor.final_message(), Some("partial answer"));

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    misalignment: None,
                    message: "turn failed".to_string(),
                    additional_details: None,
                    codex_error_info: None,
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(status, CodexStatus::InitiateShutdown);
    assert_eq!(processor.final_message(), None);

    EventProcessor::print_final_output(&mut processor);

    assert_eq!(
        std::fs::read_to_string(&output_path).expect("read output file"),
        "keep existing contents"
    );
}

#[test]
fn runtime_warning_emits_a_non_fatal_error_item() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::Warning(
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-1".to_string()),
            message: "invalid global instructions".to_string(),
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ExecThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "invalid global instructions".to_string(),
                    }),
                },
            })],
            status: CodexStatus::Running,
        }
    );
}

fn dynamic_tool_call(
    content_items: Option<Vec<DynamicToolCallOutputContentItem>>,
    status: DynamicToolCallStatus,
    success: Option<bool>,
) -> ThreadItem {
    ThreadItem::DynamicToolCall {
        id: "tool-1".to_string(),
        namespace: Some("cpp".to_string()),
        tool: "cpp_sanitize".to_string(),
        arguments: json!({"source": "main.cpp", "sanitizer": "address"}),
        status,
        content_items,
        success,
        duration_ms: Some(1234),
    }
}

fn dynamic_tool_details(item: ThreadItem) -> crate::exec_events::DynamicToolCallItem {
    let mapped = EventProcessorWithJsonOutput::map_item_for_test(item)
        .expect("dynamic tool calls must map to a JSONL item");
    match mapped.details {
        ThreadItemDetails::DynamicToolCall(item) => item,
        other => panic!("expected a dynamic tool call item, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// `--json` must carry extension tool calls. A `_ => None` arm dropped every
// one of them, so a CI consumer never saw that `cpp_sanitize` ran at all.
// ---------------------------------------------------------------------------

#[test]
fn dynamic_tool_call_is_emitted_as_a_jsonl_item() {
    let item = dynamic_tool_details(dynamic_tool_call(
        Some(vec![DynamicToolCallOutputContentItem::InputText {
            text: "1 issue(s) (exit 1)".to_string(),
        }]),
        DynamicToolCallStatus::Completed,
        Some(true),
    ));

    assert_eq!(item.tool, "cpp_sanitize");
    assert_eq!(item.namespace.as_deref(), Some("cpp"));
    assert_eq!(item.arguments["source"], json!("main.cpp"));
    assert_eq!(item.status, ExecDynamicToolCallStatus::Completed);
    assert_eq!(item.success, Some(true));
    assert_eq!(item.duration_ms, Some(1234));
    assert_eq!(
        item.content,
        vec![DynamicToolCallContent::Text {
            text: "1 issue(s) (exit 1)".to_string()
        }]
    );
}

/// A consumer that silently receives a truncated sanitizer trace would
/// conclude the run was clean, so `--json` text is uncapped — matching the
/// existing uncapped `aggregated_output` precedent.
#[test]
fn dynamic_tool_call_text_is_not_truncated_in_json() {
    let huge = "x".repeat(200_000);
    let item = dynamic_tool_details(dynamic_tool_call(
        Some(vec![DynamicToolCallOutputContentItem::InputText {
            text: huge.clone(),
        }]),
        DynamicToolCallStatus::Completed,
        Some(true),
    ));

    assert_eq!(
        item.content,
        vec![DynamicToolCallContent::Text { text: huge }]
    );
}

/// Images become a `{type, bytes}` descriptor: the image-generation
/// extension returns multi-megabyte `data:` URLs that would balloon the stream.
#[test]
fn dynamic_tool_call_image_becomes_a_descriptor_in_json() {
    let item = dynamic_tool_details(dynamic_tool_call(
        Some(vec![DynamicToolCallOutputContentItem::InputImage {
            image_url: format!("data:image/png;base64,{}", "A".repeat(4000)),
        }]),
        DynamicToolCallStatus::Completed,
        Some(true),
    ));

    assert_eq!(
        item.content,
        vec![DynamicToolCallContent::Image {
            mime_type: "image/png".to_string(),
            bytes: 3000,
        }]
    );
    let serialized = serde_json::to_string(&item).expect("serialize item");
    assert!(
        !serialized.contains("AAAA"),
        "base64 payload must not be emitted"
    );
}

#[test]
fn dynamic_tool_call_failure_is_visible_in_json() {
    let item = dynamic_tool_details(dynamic_tool_call(
        None,
        DynamicToolCallStatus::Failed,
        Some(false),
    ));

    assert_eq!(item.status, ExecDynamicToolCallStatus::Failed);
    assert_eq!(item.success, Some(false));
    assert!(item.content.is_empty());
}

#[test]
fn dynamic_tool_call_serializes_under_a_stable_type_tag() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: dynamic_tool_call(
                Some(vec![DynamicToolCallOutputContentItem::InputText {
                    text: "ubsan error pool.hpp:11".to_string(),
                }]),
                DynamicToolCallStatus::Completed,
                Some(true),
            ),
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.events.len(), 1);
    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(serialized["type"], json!("item.completed"));
    assert_eq!(serialized["item"]["type"], json!("dynamic_tool_call"));
    assert_eq!(serialized["item"]["tool"], json!("cpp_sanitize"));
    assert_eq!(
        serialized["item"]["content"][0],
        json!({"type": "text", "text": "ubsan error pool.hpp:11"})
    );
}

/// The `_ => None` arm is gone, but the variants that genuinely
/// have no JSONL representation must still map to nothing rather than to a
/// wrong item type.
#[test]
fn unrepresented_variants_still_map_to_nothing() {
    let item = ThreadItem::ContextCompaction {
        id: "ctx-1".to_string(),
    };

    assert!(EventProcessorWithJsonOutput::map_item_for_test(item).is_none());
}

#[test]
fn mcp_tool_call_result_preserves_meta_in_jsonl_event() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification::new(
            "thread-1".to_string(),
            "turn-1".to_string(),
            ThreadItemPayload::McpToolCall {
                id: "mcp-1".to_string(),
                server: "search service".to_string(),
                tool: "web_run".to_string(),
                status: McpToolCallStatus::Completed,
                arguments: json!({"search_query": [{"q": "OpenAI Codex CLI documentation"}]}),
                app_context: None,
                mcp_app_resource_uri: None,
                mcp_app_ui: None,
                plugin_id: None,
                read_only_hint: None,
                result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
                    content: vec![json!({"type": "text", "text": "search result"})],
                    structured_content: None,
                    meta: Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]})),
                })),
                error: None,
                duration_ms: Some(42),
            },
            /*started_at_ms*/ None,
            /*completed_at_ms*/ 0,
        ),
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(collected.events.len(), 1);

    let ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) = &collected.events[0] else {
        panic!("expected item.completed event");
    };
    let ThreadItemDetails::McpToolCall(item) = &item.details else {
        panic!("expected MCP tool call item");
    };
    let result = item.result.as_ref().expect("expected MCP tool result");
    assert_eq!(
        result.meta,
        Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]}))
    );

    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(
        serialized["item"]["result"]["_meta"],
        json!({"raw_messages": [{"ref_id": "turn0search0"}]})
    );
    assert!(serialized["item"]["result"].get("meta").is_none());
}

/// ByteAsk Context's turn-0 pack attempt in `exec --json`: one `context_pack` item per attempt.
/// The JSON shape is pinned here; the benchmark harness parses it.
#[test]
fn context_pack_attempt_is_one_completed_item() {
    let mut processor = EventProcessorWithJsonOutput::new(None);
    let notification = |injected: bool| {
        ServerNotification::ContextPack(codex_app_server_protocol::ContextPackNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            attempted: true,
            injected,
            reason: (!injected).then(|| "timeout".to_string()),
            latency_ms: Some(1234),
            answer_bytes: injected.then_some(4000),
            approx_tokens: injected.then_some(1000),
            test_programs: if injected {
                vec!["book_test".to_string()]
            } else {
                Vec::new()
            },
            warm_build_started: false,
            auto_index: None,
            message: (!injected).then(|| {
                "ByteAsk Context did not answer within 45 s; continuing without its code pack."
                    .to_string()
            }),
        })
    };
    let collected = processor.collect_thread_events(notification(true));
    assert_eq!(collected.status, CodexStatus::Running);
    let lines: Vec<serde_json::Value> = collected
        .events
        .iter()
        .map(|event| serde_json::to_value(event).expect("serialize"))
        .collect();
    assert_eq!(
        lines,
        vec![json!({
            "type": "item.completed",
            "item": {
                "id": "item_0",
                "type": "context_pack",
                "attempted": true,
                "injected": true,
                "reason": null,
                "latency_ms": 1234,
                "answer_bytes": 4000,
                "approx_tokens": 1000,
                "test_programs": ["book_test"],
                "warm_build_started": false,
                "auto_index": null,
                "message": null,
            },
        })]
    );
    let failed = processor.collect_thread_events(notification(false));
    let line = serde_json::to_value(&failed.events[0]).expect("serialize");
    assert_eq!(line["item"]["reason"], "timeout");
    assert_eq!(line["item"]["injected"], false);
    assert_eq!(
        line["item"]["message"],
        "ByteAsk Context did not answer within 45 s; continuing without its code pack."
    );
}
