use super::*;
use crate::unified_exec::output::CapturedStream;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::ImageReference;
use codex_protocol::models::SearchToolCallParams;
use codex_utils_head_tail_buffer::format_output_omission_marker;
use core_test_support::assert_regex_match;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn custom_tool_calls_should_roundtrip_as_custom_outputs() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_text("patched".to_string(), Some(true))
        .to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("patched"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[test]
fn function_payloads_remain_function_outputs() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = FunctionToolOutput::from_text("ok".to_string(), Some(true))
        .to_response_item("fn-1", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "fn-1");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("ok"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_code_mode_result_omits_private_metadata() {
    let output = CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "ignored",
        })],
        structured_content: Some(serde_json::json!({
            "threadId": "thread_123",
            "content": "done",
        })),
        is_error: Some(false),
        meta: Some(serde_json::json!({
            "source": "mcp",
        })),
    };

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "threadId": "thread_123",
                "content": "done",
            },
            "isError": false,
        })
    );
    assert_eq!(output.meta, Some(serde_json::json!({ "source": "mcp" })));
}

#[test]
fn mcp_tool_output_response_item_includes_wall_time() {
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "done",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        result_metadata_capture_allowed: false,
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(1024),
    };

    let response = output.to_response_item(
        "mcp-call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    assert_eq!(
        response,
        ResponseInputItem::FunctionCallOutput {
            call_id: "mcp-call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "Wall time: 1.2500 seconds\nOutput:".to_string(),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: "done".to_string(),
                    },
                ]),
                success: Some(true),
            },
        }
    );
}

#[test]
fn mcp_tool_output_response_item_truncates_large_structured_content() {
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "ignored when structured content is present",
            })],
            structured_content: Some(serde_json::json!({
                "items": "large structured value ".repeat(1_000),
            })),
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        result_metadata_capture_allowed: false,
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(128),
    };

    assert_eq!(
        output.log_output(),
        format!(
            "Wall time: 1.2500 seconds\nOutput:\n{}",
            json!({"items": "large structured value ".repeat(1_000)})
        )
    );
    let response = output.to_response_item(
        "mcp-call-large",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "mcp-call-large");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("MCP output should serialize as text");
            assert!(text.starts_with("Wall time: 1.2500 seconds\nOutput:\n"));
            assert!(text.contains("chars truncated"));
            assert!(!text.contains("ignored when structured content is present"));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_response_item_preserves_content_items() {
    let image_url = "data:image/png;base64,AAA";
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "AAA",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        result_metadata_capture_allowed: false,
        wall_time: std::time::Duration::from_millis(500),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(1024),
    };

    let response = output.to_response_item(
        "mcp-call-2",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(
                output.content_items(),
                Some(
                    vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "Wall time: 0.5000 seconds\nOutput:".to_string(),
                        },
                        FunctionCallOutputContentItem::InputImage {
                            image: ImageReference::Inline {
                                image_url: image_url.to_string()
                            },
                            detail: Some(DEFAULT_IMAGE_DETAIL),
                        },
                    ]
                    .as_slice()
                )
            );
            assert_eq!(
                output.body.to_text().as_deref(),
                Some("Wall time: 0.5000 seconds\nOutput:")
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test_case::test_case(TruncationPolicy::Bytes(64); "byte budget")]
#[test_case::test_case(TruncationPolicy::Tokens(1); "token budget")]
fn mcp_tool_output_code_mode_result_preserves_content_without_private_metadata(
    truncation_policy: TruncationPolicy,
) {
    let large_content = "large structured value ".repeat(1_000);
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "ignored",
            })],
            structured_content: Some(serde_json::json!({
                "content": large_content,
            })),
            is_error: Some(false),
            meta: Some(serde_json::json!({
                "hive_dispatch_id": "private-dispatch-id",
            })),
        },
        tool_input: json!({}),
        result_metadata_capture_allowed: false,
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy,
    };

    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let result = output.code_mode_result(&payload);

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "content": "large structured value ".repeat(1_000),
            },
            "isError": false,
        })
    );
    assert_eq!(
        output.result.meta,
        Some(serde_json::json!({ "hive_dispatch_id": "private-dispatch-id" }))
    );
}

#[test]
fn custom_tool_calls_can_derive_text_from_content_items() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "line 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AAA".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: "line 2".to_string(),
            },
        ],
        Some(true),
    )
    .to_response_item("call-99", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            let expected = vec![
                FunctionCallOutputContentItem::InputText {
                    text: "line 1".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,AAA".to_string(),
                    },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "line 2".to_string(),
                },
            ];
            assert_eq!(call_id, "call-99");
            assert_eq!(output.content_items(), Some(expected.as_slice()));
            assert_eq!(output.body.to_text().as_deref(), Some("line 1\nline 2"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[test]
fn tool_search_payloads_roundtrip_as_tool_search_outputs() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let response = ToolSearchOutput {
        tools: vec![LoadableToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "create_event".to_string(),
            description: String::new(),
            strict: false,
            defer_loading: Some(true),
            parameters: codex_tools::JsonSchema::object(
                /*properties*/ Default::default(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
            output_schema: None,
        })],
    }
    .to_response_item("search-1", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            tools,
        } => {
            assert_eq!(call_id, "search-1");
            assert_eq!(status, "completed");
            assert_eq!(execution, "client");
            assert_eq!(
                tools,
                vec![json!({
                    "type": "function",
                    "name": "create_event",
                    "description": "",
                    "strict": false,
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                })]
            );
        }
        other => panic!("expected ToolSearchOutput, got {other:?}"),
    }
}

#[test]
fn log_preview_uses_content_items_when_plain_text_is_missing() {
    let output = FunctionToolOutput::from_content(
        vec![FunctionCallOutputContentItem::InputText {
            text: "preview".to_string(),
        }],
        Some(true),
    );

    assert_eq!(output.log_output(), "preview");
    assert_eq!(
        function_call_output_content_items_to_text(&output.body),
        Some("preview".to_string())
    );
}

#[test]
fn exec_command_tool_output_formats_truncated_response() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-42".to_string(),
        chunk_id: "abc123".to_string(),
        wall_time: std::time::Duration::from_millis(1250),
        raw_output: "token one token two token three token four token five".into(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(4),
        process_id: None,
        exit_code: Some(0),
        original_token_count: Some(10),
        hook_command: None,
        tool_cwd: None,
        wait_note: None,
    };
    assert_eq!(
        output.log_output(),
        "Chunk ID: abc123\nWall time: 1.2500 seconds\nProcess exited with code 0\nOriginal token count: 10\nOutput:\ntoken one token two token three token four token five"
    );
    let response = output.to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("exec output should serialize as text");
            assert_regex_match(
                r#"(?sx)
                    ^Chunk\ ID:\ abc123
                    \nWall\ time:\ \d+\.\d{4}\ seconds
                    \nProcess\ exited\ with\ code\ 0
                    \nOriginal\ token\ count:\ 10
                    \nOutput:
                    \n.*tokens\ truncated.*
                    $"#,
                &text,
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn exec_command_tool_output_reserves_metadata_budget_and_preserves_policy_units() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let raw_output = (1..=150)
        .map(|line| format!("{line}\n"))
        .collect::<String>();

    for (policy, marker) in [
        (TruncationPolicy::Bytes(200), "chars truncated"),
        (TruncationPolicy::Tokens(50), "tokens truncated"),
    ] {
        let response = ExecCommandToolOutput {
            event_call_id: "call-42".to_string(),
            chunk_id: "abc123".to_string(),
            wall_time: std::time::Duration::from_millis(/*millis*/ 1250),
            raw_output: raw_output.as_str().into(),
            truncation_policy: policy,
            max_output_tokens: None,
            process_id: None,
            exit_code: Some(0),
            original_token_count: Some(123),
            hook_command: None,
            tool_cwd: None,
            wait_note: None,
        }
        .to_response_item("call-42", &payload);

        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            panic!("expected FunctionCallOutput");
        };
        let text = output
            .body
            .to_text()
            .expect("exec output should serialize as text");

        assert!(text.len() <= (policy * 1.2).byte_budget());
        assert_eq!(text.matches(marker).count(), 1);
        assert!(text.contains("Original token count: 123"));
        assert!(text.contains("Total output lines: 150"));
        assert!(text.contains("\n1\n2\n3\n"));
        assert!(text.ends_with("149\n150\n"));
    }
}

#[test]
fn exec_command_tool_output_preserves_omission_metadata_when_truncated() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let marker = format_output_omission_marker(/*omitted_bytes*/ 123_456);
    let raw_output = format!(
        "HEAD-{}\n{marker}\nTAIL-{}",
        "a".repeat(/*n*/ 100),
        "z".repeat(/*n*/ 100)
    );
    let mut output = ExecCommandToolOutput {
        event_call_id: "call-omitted".to_string(),
        chunk_id: "abc123".to_string(),
        wall_time: std::time::Duration::from_millis(/*millis*/ 1250),
        raw_output: CapturedStream {
            text: raw_output.clone(),
            omitted_bytes: 123_456,
        },
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(4),
        process_id: None,
        exit_code: Some(0),
        original_token_count: Some(42_000),
        hook_command: None,
        tool_cwd: None,
        wait_note: None,
    };
    let expected_header = "Chunk ID: abc123\nWall time: 1.2500 seconds\nProcess exited with code 0\nOriginal token count: 42000\nOutput:\n";
    assert_eq!(
        output.log_output(),
        format!("{expected_header}{raw_output}")
    );
    let response = output.to_response_item("call-omitted", &payload);

    // Collection may report omitted bytes without including the marker in its text.
    output.raw_output.text = "remaining output".to_string();
    assert_eq!(
        output.log_output(),
        format!("{expected_header}{marker}\nremaining output")
    );

    let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
        panic!("expected FunctionCallOutput");
    };
    let text = output
        .body
        .to_text()
        .expect("exec output should serialize as text");
    assert!(text.contains("Original token count: 42000"));
    assert!(text.contains("Warning: truncated output (original token count: 42000)"));
    assert_eq!(text.matches(&marker).count(), 1);
}

// ---- ByteAsk token efficiency ------------------------------

fn byteask_exec_output(raw: Vec<u8>) -> ExecCommandToolOutput {
    ExecCommandToolOutput {
        event_call_id: "call-eff".to_string(),
        chunk_id: "c1".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: String::from_utf8_lossy(&raw).as_ref().into(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: None,
        tool_cwd: None,
        wait_note: None,
    }
}

#[test]
fn byteask_plain_output_is_untouched_byte_for_byte() {
    let raw = "plain line\n".repeat(200).into_bytes();
    let out = byteask_exec_output(raw.clone());
    let upstream = TruncationPolicy::Tokens(10_000);
    let (text, policy, footer) =
        out.byteask_reduce_with(upstream, &byteask_output_reducers::Policy::default());
    assert_eq!(text.as_bytes(), raw.as_slice());
    assert_eq!(policy, upstream);
    assert_eq!(footer, "");
}

#[test]
fn byteask_colour_codes_are_stripped_and_the_output_is_tagged() {
    let raw = format!("\u{1b}[31merror\u{1b}[0m here\n{}", "filler\n".repeat(200));
    let out = byteask_exec_output(raw.into_bytes());
    let (text, _policy, footer) = out.byteask_reduce_with(
        TruncationPolicy::Tokens(10_000),
        &byteask_output_reducers::Policy::default(),
    );
    assert!(text.starts_with("error here\n"));
    assert!(footer.contains("[byteask: ansi"), "footer was: {footer}");
    // The serialized text ends with the sentinel, which is what later passes key on.
    assert!(byteask_output_reducers::is_reduced(&out.response_text()));
}

#[test]
fn byteask_a_lower_budget_is_never_applied_without_a_spilled_original() {
    // No spill directory is initialised in this test process, so the write fails and
    // the upstream budget MUST be kept: no lossy cut without a recoverable original.
    let raw = "0123456789 abcdefghij\n".repeat(4_000).into_bytes(); // ~88 KB
    let out = byteask_exec_output(raw);
    let upstream = TruncationPolicy::Tokens(10_000);
    let eff = byteask_output_reducers::Policy {
        exec_budget_tokens: Some(4_000),
        ..byteask_output_reducers::Policy::default()
    };
    // The spill directory is process-wide and another test may have set it, so BOTH
    // outcomes are asserted: this test must never pass by checking nothing.
    let spill_available =
        byteask_output_reducers::spill::dir().is_some_and(std::path::Path::is_dir);
    let (_text, policy, footer) = out.byteask_reduce_with(upstream, &eff);
    if spill_available {
        assert_eq!(policy, TruncationPolicy::Tokens(4_000));
        assert!(
            footer.contains("trimmed") && footer.contains("saved at"),
            "footer: {footer}"
        );
    } else {
        assert_eq!(policy, upstream);
        assert_eq!(footer, "");
    }
}

#[test]
fn byteask_the_brake_switches_everything_off() {
    let raw = format!("\u{1b}[31mx\u{1b}[0m\n{}", "filler\n".repeat(200));
    let out = byteask_exec_output(raw.clone().into_bytes());
    let eff = byteask_output_reducers::Policy::from_catalog_json(
        r#"{"byteask_efficiency":{"brake":true,"budgets":{"exec":4000}}}"#,
    );
    let (text, _policy, footer) = out.byteask_reduce_with(TruncationPolicy::Tokens(10_000), &eff);
    assert_eq!(text, raw);
    assert_eq!(footer, "");
}

fn byteask_all_reducers_on() -> byteask_output_reducers::Policy {
    let mut p = byteask_output_reducers::Policy::default();
    for name in byteask_output_reducers::policy::DARK_REDUCERS {
        p.enabled.insert((*name).to_string());
    }
    p
}

fn byteask_cargo_test_log() -> String {
    let mut s = String::from("running 80 tests\n");
    for i in 0..80 {
        s.push_str(&format!("test module::case_number_{i} ... ok\n"));
    }
    s.push_str("\ntest result: ok. 80 passed; 0 failed\n");
    s
}

#[test]
fn byteask_a_lossy_reducer_is_undone_when_the_original_cannot_be_spilled() {
    let raw = byteask_cargo_test_log();
    let mut out = byteask_exec_output(raw.clone().into_bytes());
    out.hook_command = Some("cargo test".to_string());
    let spill_available =
        byteask_output_reducers::spill::dir().is_some_and(std::path::Path::is_dir);
    let (text, policy, footer) =
        out.byteask_reduce_with(TruncationPolicy::Tokens(10_000), &byteask_all_reducers_on());
    assert_eq!(policy, TruncationPolicy::Tokens(10_000));
    if spill_available {
        assert!(
            text.len() < raw.len() / 3,
            "the passing-test lines became a count"
        );
        assert!(text.contains("test result: ok. 80 passed"));
        assert!(
            footer.contains("tests") && footer.contains("saved at"),
            "footer: {footer}"
        );
    } else {
        assert_eq!(
            text, raw,
            "content was removed with no recoverable original"
        );
        assert_eq!(footer, "");
    }
}

#[test]
fn byteask_family_reducers_need_a_positive_command_match() {
    let raw = byteask_cargo_test_log();
    for command in [
        None,
        Some("bash -lc 'cat test.log'"),
        Some("cargo test && cat x"),
    ] {
        let mut out = byteask_exec_output(raw.clone().into_bytes());
        out.hook_command = command.map(str::to_string);
        let (text, _policy, footer) =
            out.byteask_reduce_with(TruncationPolicy::Tokens(10_000), &byteask_all_reducers_on());
        assert_eq!(text, raw, "{command:?}");
        assert_eq!(footer, "", "{command:?}");
    }
}

#[test]
fn byteask_search_output_is_regrouped_losslessly_by_default() {
    let mut raw = String::new();
    for i in 0..60 {
        raw.push_str(&format!(
            "crates/long/path/to/a/module/file_a.rs:{i}:    let value_{i} = compute();\n"
        ));
    }
    let mut out = byteask_exec_output(raw.clone().into_bytes());
    out.hook_command = Some("rg -n compute".to_string());
    let (text, _policy, footer) = out.byteask_reduce_with(
        TruncationPolicy::Tokens(10_000),
        &byteask_output_reducers::Policy::default(),
    );
    assert_eq!(text.matches("file_a.rs").count(), 1);
    assert!(text.contains("\n59:    let value_59 = compute();\n"));
    assert!(footer.contains("search-group"), "footer was: {footer}");
    // The same bytes through a command that could be printing a file: untouched.
    out.hook_command = Some("cat matches.txt".to_string());
    let (text, _policy, _footer) = out.byteask_reduce_with(
        TruncationPolicy::Tokens(10_000),
        &byteask_output_reducers::Policy::default(),
    );
    assert_eq!(text, raw);
}

#[test]
fn byteask_the_compact_header_keeps_what_the_model_acts_on() {
    let mut out = byteask_exec_output(b"hi\n".to_vec());
    out.wall_time = std::time::Duration::from_millis(1234);
    assert_eq!(out.byteask_compact_header(), "[exit 0 | 1.2s]");
    out.exit_code = None;
    out.process_id = Some(7);
    assert_eq!(
        out.byteask_compact_header(),
        "[running | 1.2s]\nProcess running with session ID 7"
    );
    // The token count appears only when something was cut.
    out.original_token_count = Some(50);
    assert!(
        !out.byteask_compact_header()
            .contains("Original token count")
    );
    out.original_token_count = Some(500_000);
    assert!(
        out.byteask_compact_header()
            .contains("Original token count: 500000")
    );
}

#[test]
fn byteask_a_wait_floor_note_follows_the_session_line() {
    // When a wait floor, not the model, set the wait on a running command, both
    // header forms say how long it waited and that waits end when the command exits.
    let mut out = byteask_exec_output(b"building\n".to_vec());
    out.wall_time = std::time::Duration::from_secs(60);
    out.exit_code = None;
    out.process_id = Some(7);
    out.wait_note = Some(crate::unified_exec::wait_floor_note(out.wall_time));
    assert_eq!(
        out.byteask_compact_header(),
        "[running | 60.0s]\nProcess running with session ID 7\nWaited 60s: a running command \
         without a TTY is waited on at least that long; every wait ends as soon as the command exits."
    );
    // Whichever header form the active policy selects, the note follows the session line.
    let header = out.response_header();
    assert!(
        header.contains("Process running with session ID 7\nWaited 60s:"),
        "{header}"
    );
    out.wait_note = None;
    assert!(!out.byteask_compact_header().contains("Waited"));
}
