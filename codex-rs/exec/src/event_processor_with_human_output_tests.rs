use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::ConfigBuilder;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use codex_utils_path_uri::PathUri;
use codex_utils_sandbox_summary::summarize_permission_profile;
use owo_colors::Style;
use pretty_assertions::assert_eq;

use super::EventProcessorWithHumanOutput;
use super::TOOL_OUTPUT_TEXT_CAP;
use super::config_summary_entries;
use super::final_message_from_turn_items;
use super::looks_like_sandbox_write_denial;
use super::reasoning_text;
use super::should_print_final_message_to_stdout;
use super::should_print_final_message_to_tty;
use crate::event_processor::EventProcessor;

/// An unstyled processor, so assertions read against plain text.
fn plain_processor() -> EventProcessorWithHumanOutput {
    EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: None,
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    }
}

fn dynamic_tool_call(
    status: codex_app_server_protocol::DynamicToolCallStatus,
    content_items: Option<Vec<codex_app_server_protocol::DynamicToolCallOutputContentItem>>,
    success: Option<bool>,
) -> ThreadItemPayload {
    ThreadItemPayload::DynamicToolCall {
        id: "tool-1".to_string(),
        namespace: None,
        tool: "cpp_sanitize".to_string(),
        arguments: serde_json::json!({"source": "main.cpp"}),
        status,
        content_items,
        success,
        duration_ms: Some(1234),
    }
}

fn text_item(text: &str) -> codex_app_server_protocol::DynamicToolCallOutputContentItem {
    codex_app_server_protocol::DynamicToolCallOutputContentItem::InputText {
        text: text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// `byteask exec` must render extension tools (`cpp_*`, `ref_*`, skills,
// memories, image generation). Without these arms a `cpp_sanitize` call that
// compiles and runs a program under ASan/UBSan and finds the bug prints not
// one line to the terminal.
// ---------------------------------------------------------------------------

#[test]
fn dynamic_tool_call_started_names_the_tool_and_its_target() {
    let processor = plain_processor();

    let lines = processor.item_started_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::InProgress,
        None,
        None,
    ));

    assert_eq!(
        lines,
        vec![
            "tool: cpp_sanitize started".to_string(),
            "{\"source\":\"main.cpp\"}".to_string(),
        ]
    );
}

#[test]
fn dynamic_tool_call_started_uses_the_namespace_when_present() {
    let processor = plain_processor();
    let mut item = dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::InProgress,
        None,
        None,
    );
    if let ThreadItemPayload::DynamicToolCall { namespace, .. } = &mut item {
        *namespace = Some("cpp".to_string());
    }

    let lines = processor.item_started_lines(&item);

    assert!(lines[0].contains("cpp/cpp_sanitize"), "{lines:?}");
}

#[test]
fn dynamic_tool_call_completed_prints_the_tool_output() {
    let processor = plain_processor();

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Completed,
        Some(vec![text_item(
            "1 issue(s) (exit 1)\nubsan error include/pool.hpp:11:26 store to address 0x7ffc with insufficient space",
        )]),
        Some(true),
    ));

    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "tool: cpp_sanitize (completed in 1234ms)");
    assert!(
        lines[1].contains("ubsan error include/pool.hpp:11:26"),
        "{lines:?}"
    );
}

#[test]
fn dynamic_tool_call_completed_reports_failure() {
    let processor = plain_processor();

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Failed,
        Some(vec![text_item("compiler not found")]),
        None,
    ));

    assert_eq!(lines[0], "tool: cpp_sanitize (failed in 1234ms)");
    assert_eq!(lines[1], "compiler not found");
}

/// `success: false` on an otherwise `Completed` call still means the tool
/// failed; the TUI treats it that way and so must exec.
#[test]
fn dynamic_tool_call_success_false_renders_as_failed() {
    let processor = plain_processor();

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Completed,
        None,
        Some(false),
    ));

    assert_eq!(lines[0], "tool: cpp_sanitize (failed in 1234ms)");
}

/// The image-generation extension registers into the same tool registry
/// and returns `image_url: "data:image/png;base64,…"`, so a renderer that
/// echoed content items verbatim would dump megabytes of base64 to the
/// terminal.
#[test]
fn dynamic_tool_call_image_output_is_a_placeholder_not_base64() {
    let processor = plain_processor();
    let payload = "A".repeat(4000);
    let item = ThreadItemPayload::DynamicToolCall {
        id: "tool-1".to_string(),
        namespace: None,
        tool: "image_generation".to_string(),
        arguments: serde_json::Value::Null,
        status: codex_app_server_protocol::DynamicToolCallStatus::Completed,
        content_items: Some(vec![
            codex_app_server_protocol::DynamicToolCallOutputContentItem::InputImage {
                image_url: format!("data:image/png;base64,{payload}"),
            },
        ]),
        success: Some(true),
        duration_ms: None,
    };

    let lines = processor.item_completed_lines(&item);

    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], "<image: image/png, ~3000 bytes>");
    assert!(
        !lines[1].contains(&payload),
        "base64 payload must not be printed"
    );
}

/// A tool result is whatever the program under a debugger or sanitizer, or a
/// third-party MCP server, printed: its escape sequences must not reach the
/// user's terminal (OSC 52 writes the clipboard, OSC 0 retitles the window,
/// CSI 2J clears the screen). Lines and tabs are kept.
#[test]
fn tool_output_escape_sequences_never_reach_the_terminal() {
    let processor = plain_processor();
    let payload =
        "ok \x1b]52;c;aGVsbG8=\x07 \x1b[2J\x1b]0;pwned\x07\n\tsecond line \x1b[31mred\x1b[0m";

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Completed,
        Some(vec![text_item(payload)]),
        Some(true),
    ));

    let printed = lines.join("\n");
    for sequence in ["\x1b", "\x07"] {
        assert!(
            !printed.contains(sequence),
            "{sequence:?} reached the terminal: {printed:?}"
        );
    }
    assert!(
        printed.contains("ok ") && printed.contains("\n\tsecond line"),
        "{printed:?}"
    );
}

/// A byte cap with an explicit elision marker, so a reader can tell
/// truncated output from complete output.
#[test]
fn dynamic_tool_call_text_is_capped_with_an_explicit_elision_marker() {
    let processor = plain_processor();
    let huge = "x".repeat(TOOL_OUTPUT_TEXT_CAP + 500);

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Completed,
        Some(vec![text_item(&huge)]),
        Some(true),
    ));

    assert!(lines[1].len() < huge.len(), "output must be capped");
    assert!(
        lines[1].ends_with("… [truncated, 500 more bytes]"),
        "expected an explicit elision marker, got: {}",
        &lines[1][lines[1].len().saturating_sub(60)..]
    );
}

/// The renderer cap must stay well above any tool-owned cap, or it becomes the
/// reason a sanitizer trace goes unseen. cpp-tools' `STDOUT_CAP`
/// is 2 KiB and its sanitizer trace budget sits in the same order of magnitude.
#[test]
fn renderer_cap_is_far_above_tool_owned_caps() {
    assert!(
        TOOL_OUTPUT_TEXT_CAP >= 16 * 1024,
        "renderer cap must not become the binding constraint on tool output"
    );
}

#[test]
fn dynamic_tool_call_with_no_output_still_shows_the_call() {
    let processor = plain_processor();

    let lines = processor.item_completed_lines(&dynamic_tool_call(
        codex_app_server_protocol::DynamicToolCallStatus::Completed,
        Some(Vec::new()),
        Some(true),
    ));

    assert_eq!(lines, vec!["tool: cpp_sanitize (completed in 1234ms)"]);
}

// ---------------------------------------------------------------------------
// The MCP arm prints the output body as well as status and error (it used to
// print only the latter two: the same defect as for extension tools).
// ---------------------------------------------------------------------------

#[test]
fn mcp_tool_call_completed_prints_the_output_body() {
    let processor = plain_processor();
    let item = ThreadItemPayload::McpToolCall {
        id: "mcp-1".to_string(),
        server: "docs".to_string(),
        tool: "search".to_string(),
        status: codex_app_server_protocol::McpToolCallStatus::Completed,
        arguments: serde_json::json!({"q": "ubsan"}),
        app_context: None,
        mcp_app_resource_uri: None,
        plugin_id: None,
        read_only_hint: None,
        result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
            content: vec![serde_json::json!({"type": "text", "text": "the answer"})],
            structured_content: None,
            meta: None,
        })),
        error: None,
        duration_ms: Some(42),
    };

    let lines = processor.item_completed_lines(&item);

    assert_eq!(lines[0], "mcp: docs/search (completed in 42ms)");
    assert_eq!(lines[1], "the answer");
}

#[test]
fn mcp_tool_call_image_content_is_a_placeholder() {
    let processor = plain_processor();
    let item = ThreadItemPayload::McpToolCall {
        id: "mcp-1".to_string(),
        server: "docs".to_string(),
        tool: "render".to_string(),
        status: codex_app_server_protocol::McpToolCallStatus::Completed,
        arguments: serde_json::Value::Null,
        app_context: None,
        mcp_app_resource_uri: None,
        plugin_id: None,
        read_only_hint: None,
        result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
            content: vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "A".repeat(400),
            })],
            structured_content: None,
            meta: None,
        })),
        error: None,
        duration_ms: None,
    };

    let lines = processor.item_completed_lines(&item);

    assert_eq!(lines[1], "<image: image/png, ~300 bytes>");
}

// ---------------------------------------------------------------------------
// Read-only sandbox + `approval: never` is a dead end with no signal: the
// agent hits `Read-only file system`, retries with `TMPDIR=/tmp`, fails
// again, and concludes the product cannot compile. The hint is that signal.
// ---------------------------------------------------------------------------

#[test]
fn sandbox_denial_signatures_are_recognised() {
    assert!(looks_like_sandbox_write_denial(
        Some(1),
        "clang: error: unable to make temporary file: Read-only file system"
    ));
    // macOS Seatbelt denies writes with EPERM.
    assert!(looks_like_sandbox_write_denial(
        Some(1),
        "mkdir: build: Operation not permitted"
    ));
}

#[test]
fn ordinary_failures_are_not_treated_as_sandbox_denials() {
    assert!(!looks_like_sandbox_write_denial(
        Some(1),
        "error[E0308]: mismatched types"
    ));
    // `permission denied` is deliberately excluded: it fires routinely on a
    // non-executable file with no sandbox involved.
    assert!(!looks_like_sandbox_write_denial(
        Some(126),
        "./run.sh: permission denied"
    ));
    assert!(!looks_like_sandbox_write_denial(
        Some(0),
        "read-only file system"
    ));
}

#[test]
fn sandbox_denial_hint_is_offered_once_under_a_read_only_sandbox() {
    let mut processor = plain_processor();
    processor.sandbox_is_read_only = true;
    let item = failed_command("clang: error: unable to make temporary file: Read-only file system");

    let hint = processor
        .sandbox_denial_hint(&item)
        .expect("expected a sandbox hint");
    assert!(hint.contains("--sandbox workspace-write"), "{hint}");
    assert!(hint.contains("cpp_sanitize"), "{hint}");

    // Advice, not an error: shown once, then suppressed.
    processor.sandbox_hint_shown = true;
    processor.render_item_completed(item);
    assert!(processor.sandbox_hint_shown);
}

#[test]
fn sandbox_denial_hint_is_suppressed_when_writes_are_allowed() {
    let processor = plain_processor();
    let item = failed_command("clang: error: unable to make temporary file: Read-only file system");

    assert_eq!(processor.sandbox_denial_hint(&item), None);
}

#[test]
fn sandbox_denial_hint_is_suppressed_for_ordinary_failures() {
    let mut processor = plain_processor();
    processor.sandbox_is_read_only = true;
    let item = failed_command("error[E0308]: mismatched types");

    assert_eq!(processor.sandbox_denial_hint(&item), None);
}

/// Built through serde because `cwd`'s `LegacyAppPathString` is deliberately
/// not constructible from a raw string outside deserialization.
fn failed_command(output: &str) -> ThreadItemPayload {
    serde_json::from_value(serde_json::json!({
        "type": "commandExecution",
        "id": "cmd-1",
        "command": "clang++ main.cpp",
        "cwd": "/tmp/hft",
        "processId": null,
        "status": "failed",
        "commandActions": [],
        "aggregatedOutput": output,
        "exitCode": 1,
        "durationMs": 10,
    }))
    .expect("valid commandExecution item")
}

#[test]
fn turn_diff_notifications_render_only_changes() -> anyhow::Result<()> {
    const CHILD: &str = "CODEX_EXEC_TURN_DIFF_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Capture stderr in a child so other tests cannot interfere with rendering.
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "event_processor_with_human_output::tests::turn_diff_notifications_render_only_changes",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        insta::assert_snapshot!(String::from_utf8(output.stderr)?, @r"
        turn-1
        @@ -1 +1 @@
        -value=1
        +value=2
        @@ -1 +1 @@
        -value=1
        +value=3
        @@ -1 +1 @@
        -value=1
        +value=2
        @@ -1 +1 @@
        -value=1
        +value=2
        @@ -1 +1 @@
        -value=1
        +value=2
        turn-2
        @@ -1 +1 @@
        -value=1
        +value=2
        ");
        return Ok(());
    }

    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: None,
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
        last_turn_diff: String::new(),
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
    };
    let first = "@@ -1 +1 @@\n-value=1\n+value=2";
    let changed = "@@ -1 +1 @@\n-value=1\n+value=3";
    for (turn_id, diffs) in [
        // Repeated updates are quiet, but changed and restored diffs still print.
        (
            "turn-1",
            vec![
                "", " \n", first, first, changed, changed, first, "", first, " \n", first,
            ],
        ),
        ("turn-2", vec![first, first]),
    ] {
        processor.process_server_notification(ServerNotification::TurnStarted(
            codex_app_server_protocol::TurnStartedNotification {
                thread_id: "thread-1".to_string(),
                turn: Turn {
                    id: turn_id.to_string(),
                    root_turn_id: None,
                    items_view: codex_app_server_protocol::TurnItemsView::Full,
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            },
        ));
        eprintln!("{turn_id}");
        for diff in diffs {
            assert_eq!(
                processor.process_server_notification(ServerNotification::TurnDiffUpdated(
                    codex_app_server_protocol::TurnDiffUpdatedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: turn_id.to_string(),
                        diff: diff.to_string(),
                    },
                )),
                crate::event_processor::CodexStatus::Running
            );
        }
    }
    Ok(())
}

#[test]
fn suppresses_final_stdout_message_when_both_streams_are_terminals() {
    assert!(!should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn prints_final_stdout_message_when_stdout_is_not_terminal() {
    assert!(should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ false,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn prints_final_stdout_message_when_stderr_is_not_terminal() {
    assert!(should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ false
    ));
}

#[test]
fn suppresses_final_stdout_message_when_missing() {
    assert!(!should_print_final_message_to_stdout(
        /*final_message*/ None, /*stdout_is_terminal*/ false,
        /*stderr_is_terminal*/ false
    ));
}

#[test]
fn prints_final_tty_message_when_not_yet_rendered() {
    assert!(should_print_final_message_to_tty(
        Some("hello"),
        /*final_message_rendered*/ false,
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn suppresses_final_tty_message_when_already_rendered() {
    assert!(!should_print_final_message_to_tty(
        Some("hello"),
        /*final_message_rendered*/ true,
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn reasoning_text_prefers_summary_when_raw_reasoning_is_hidden() {
    let text = reasoning_text(
        &["summary".to_string()],
        &["raw".to_string()],
        /*show_raw_agent_reasoning*/ false,
    );

    assert_eq!(text.as_deref(), Some("summary"));
}

#[test]
fn reasoning_text_uses_raw_content_when_enabled() {
    let text = reasoning_text(
        &["summary".to_string()],
        &["raw".to_string()],
        /*show_raw_agent_reasoning*/ true,
    );

    assert_eq!(text.as_deref(), Some("raw"));
}

#[test]
fn summarizes_disabled_permission_profile_as_danger_full_access() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp").abs());

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::Disabled,
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "danger-full-access"
    );
}

#[test]
fn summarizes_external_permission_profile() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp").abs());

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Enabled,
            },
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "external-sandbox (network access enabled)"
    );
}

#[test]
fn summarizes_managed_workspace_write_permission_profile() {
    let cwd = test_path_buf("/tmp/project").abs();
    let cache_root = test_path_buf("/tmp/cache").abs();
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: cwd.clone().into(),
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cache_root.clone().into(),
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
        ]),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(
            &profile,
            &PathUri::from_abs_path(&cwd),
            &[cwd.clone().into(), cache_root.clone().into()],
        ),
        format!("workspace-write [workdir, {}]", cache_root.display())
    );
}

#[test]
fn summarizes_managed_read_only_permission_profile() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp/project").abs());
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(Vec::new()),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(&profile, &cwd, std::slice::from_ref(&cwd)),
        "read-only"
    );
}

#[tokio::test]
async fn config_summary_entries_include_runtime_workspace_roots() {
    let codex_home = tempfile::tempdir().expect("create codex home");
    let cwd = tempfile::tempdir().expect("create cwd");
    let extra_root = tempfile::tempdir().expect("create extra root");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    let cwd = cwd.path().to_path_buf().abs();
    let extra_root = extra_root.path().to_path_buf().abs();
    let expected_extra_root_name = extra_root
        .file_name()
        .expect("extra root should have file name")
        .to_string_lossy()
        .to_string();
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd.clone(), extra_root];
    config
        .permissions
        .set_workspace_roots(config.workspace_roots.clone());
    config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ))
        .expect("set permission profile");

    let session_configured_event = SessionConfiguredEvent {
        session_id: SessionId::new(),
        thread_id: ThreadId::new(),
        forked_from_id: None,
        parent_thread_id: None,
        thread_source: None,
        thread_name: None,
        model: "gpt-5.4".to_string(),
        model_provider_id: config.model_provider_id.clone(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: config.approvals_reviewer,
        permission_profile: config.permissions.effective_permission_profile(),
        active_permission_profile: None,
        cwd,
        reasoning_effort: None,
        network_proxy: None,
        rollout_path: None,
    };

    let summary_entries = config_summary_entries(&config, &session_configured_event);
    let sandbox_summary = summary_entries
        .iter()
        .find_map(|(key, value)| (*key == "sandbox").then_some(value))
        .expect("sandbox summary entry");
    assert!(
        sandbox_summary.starts_with("workspace-write [workdir, ")
            && sandbox_summary.contains(&expected_extra_root_name),
        "expected runtime workspace root in sandbox summary: {summary_entries:?}"
    );

    // An unset `model_reasoning_effort` means "not configured, the
    // provider decides". The header must not print "none", which reads
    // as "reasoning was disabled".
    assert_eq!(config.model_reasoning_effort, None);
    let reasoning_effort = summary_entries
        .iter()
        .find_map(|(key, value)| (*key == "reasoning effort").then_some(value.as_str()));
    assert_eq!(reasoning_effort, Some("default (provider-side)"));
}

#[test]
fn final_message_from_turn_items_uses_latest_agent_message() {
    let message = final_message_from_turn_items(&[
        ThreadItemPayload::AgentMessage {
            id: "msg-1".to_string(),
            text: "first".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        ThreadItemPayload::Plan {
            id: "plan-1".to_string(),
            text: "plan".to_string(),
        },
        ThreadItemPayload::AgentMessage {
            id: "msg-2".to_string(),
            text: "second".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    ]);

    assert_eq!(message.as_deref(), Some("second"));
}

#[test]
fn final_message_from_turn_items_falls_back_to_latest_plan() {
    let message = final_message_from_turn_items(&[
        ThreadItemPayload::Reasoning {
            id: "reasoning-1".to_string(),
            summary: vec!["inspect".to_string()],
            content: Vec::new(),
        },
        ThreadItemPayload::Plan {
            id: "plan-1".to_string(),
            text: "first plan".to_string(),
        },
        ThreadItemPayload::Plan {
            id: "plan-2".to_string(),
            text: "final plan".to_string(),
        },
    ]);

    assert_eq!(message.as_deref(), Some("final plan"));
}

#[test]
fn turn_completed_recovers_final_message_from_turn_items() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: None,
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![ThreadItem::untimed(ThreadItemPayload::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "final answer".to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                })],
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("final answer"));
}

#[test]
fn turn_completed_overwrites_stale_final_message_from_turn_items() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("stale answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![ThreadItem::untimed(ThreadItemPayload::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "final answer".to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                })],
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("final answer"));
    assert!(!processor.final_message_rendered);
}

#[test]
fn turn_completed_preserves_streamed_final_message_when_turn_items_are_empty() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("streamed answer".to_string()),
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("streamed answer"));
    assert!(processor.emit_final_message_on_shutdown);
}

#[test]
fn turn_failed_clears_stale_final_message() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("partial answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: true,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message, None);
    assert!(!processor.final_message_rendered);
    assert!(!processor.emit_final_message_on_shutdown);
}

#[test]
fn turn_interrupted_clears_stale_final_message() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("partial answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: true,
        last_total_token_usage: None,
        sandbox_is_read_only: false,
        sandbox_hint_shown: false,
        last_turn_diff: String::new(),
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Interrupted,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message, None);
    assert!(!processor.final_message_rendered);
    assert!(!processor.emit_final_message_on_shutdown);
}
