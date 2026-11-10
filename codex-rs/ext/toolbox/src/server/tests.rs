//! The toolbox-vs-TUI parity contract and the gate over MCP.
//!
//! Every test talks to the server over a real MCP session (rmcp client <->
//! rmcp server on an in-memory pipe), so what is asserted is what Claude Code
//! or Codex would receive, not an internal list.

use std::collections::BTreeMap;
use std::path::Path;

use byteask_policy::PlanCache;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::LoaderOverrides;
use pretty_assertions::assert_eq;
use rmcp::ServiceExt as _;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;

use super::*;
use crate::gate::Identity;

const CPP_SNAPSHOT: &str = include_str!("../../../cpp-tools/src/snapshots/model_facing_text.json");
const REF_SNAPSHOT: &str = include_str!("../../../ref-tools/src/snapshots/model_facing_text.json");
const NEW_TOOLS_SNAPSHOT: &str =
    include_str!("../../../cpp-tools/src/snapshots/model_facing_text_new_tools.json");
// Integration: the runtime tools, pinned in their own file. port_check and
// cpp_spike_trace are not registered in 0.2.0 (their text stays pinned in cpp-tools);
// cpp_finish_check is new in 0.2.0.
const RUNTIME_SNAPSHOT: &str = include_str!("../../../cpp-tools/src/snapshots/runtime_tools.json");
const FINISH_CHECK_SNAPSHOT: &str =
    include_str!("../../../cpp-tools/src/snapshots/finish_check.json");
const LIST_SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/snapshots/tool_list.json");

async fn config_for(workspace: &Path, home: &Path, trusted: bool) -> Config {
    // The packaged defaults hide C/C++ tools whose program is not installed (0.2.4); these tests pin
    // the full tool list whatever this machine has installed.
    let mut toml = "[byteask_cpp]\nhide_missing_tools = false\n".to_string();
    if trusted {
        toml.push_str(&format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            workspace.display()
        ));
    }
    std::fs::write(home.join("config.toml"), toml).expect("config.toml");
    ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(workspace.to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .expect("config")
}

fn cpp_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("CMakeLists.txt"), "project(x CXX)\n").unwrap();
    std::fs::write(dir.path().join("main.cpp"), "int main() { return 0; }\n").unwrap();
    dir
}

fn plain_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("README.md"), "notes\n").unwrap();
    dir
}

async fn server_for(workspace: &Path, trusted: bool, gate: GateState) -> ToolboxServer {
    let home = tempfile::tempdir().expect("home");
    let config = config_for(workspace, home.path(), trusted).await;
    let thread = ToolboxThread::start(&config).await;
    // The home is only needed while the thread starts (helper materialisation).
    drop(home);
    ToolboxServer::new(thread, gate)
}

async fn connect(server: ToolboxServer) -> RunningService<RoleClient, ()> {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_io).await.expect("client connects")
}

/// name -> (description, parameters) from a pinned description snapshot.
fn pinned(snapshot: &str) -> BTreeMap<String, (String, serde_json::Value)> {
    let doc: serde_json::Value = serde_json::from_str(snapshot).expect("snapshot json");
    doc["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("name").to_string(),
                (
                    tool["description"]
                        .as_str()
                        .expect("description")
                        .to_string(),
                    tool["parameters"].clone(),
                ),
            )
        })
        .collect()
}

fn pinned_guidance(snapshot: &str) -> String {
    let doc: serde_json::Value = serde_json::from_str(snapshot).expect("snapshot json");
    doc["guidance"].as_str().expect("guidance").to_string()
}

/// The toolbox serves exactly the shared registry's tools, and
/// every description and parameter schema is byte-identical to the pinned
/// snapshots the CLI's text is held to.
#[tokio::test]
async fn mcp_tools_match_the_pinned_tui_snapshots_byte_for_byte() {
    let workspace = cpp_workspace();
    let client = connect(server_for(workspace.path(), false, GateState::off()).await).await;
    let tools = client.list_all_tools().await.expect("tools/list");

    let mut expected = pinned(CPP_SNAPSHOT);
    expected.extend(pinned(REF_SNAPSHOT));
    expected.extend(pinned(NEW_TOOLS_SNAPSHOT));
    expected.extend(pinned(RUNTIME_SNAPSHOT));
    expected.extend(pinned(FINISH_CHECK_SNAPSHOT));
    assert_eq!(
        expected.len(),
        37,
        "0.2.0 registry on Linux: 28 C/C++ (24 + check_status + debug_reverse, \
         debug_core + cpp_finish_check) + 9 reference tools"
    );

    let served: BTreeMap<String, (String, serde_json::Value)> = tools
        .iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                (
                    tool.description.as_deref().unwrap_or_default().to_string(),
                    serde_json::Value::Object((*tool.input_schema).clone()),
                ),
            )
        })
        .collect();
    assert_eq!(served.len(), tools.len(), "no tool name is served twice");
    assert_eq!(served, expected);
    client.cancel().await.ok();
}

/// The guidance the TUI adds to the prompt reaches the agent as MCP
/// instructions, unchanged, and only where the TUI would add it.
#[tokio::test]
async fn instructions_carry_the_same_guidance_as_the_tui() {
    let cpp = cpp_workspace();
    let client = connect(server_for(cpp.path(), false, GateState::off()).await).await;
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");
    // Today's guidance byte for byte, then the session-start environment note the
    // TUI also adds after it in a C/C++ workspace, then the reference guidance.
    let cpp_guidance = format!("{}\n\n", pinned_guidance(CPP_SNAPSHOT));
    let ref_guidance = format!("\n\n{}", pinned_guidance(REF_SNAPSHOT));
    assert!(instructions.starts_with(&cpp_guidance), "{instructions}");
    assert!(instructions.ends_with(&ref_guidance), "{instructions}");
    let middle = &instructions[cpp_guidance.len()..instructions.len() - ref_guidance.len()];
    assert!(
        middle.starts_with("C/C++ environment (probed at session start):"),
        "{middle}"
    );
    client.cancel().await.ok();

    let plain = plain_workspace();
    let client = connect(server_for(plain.path(), false, GateState::off()).await).await;
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .expect("instructions");
    assert_eq!(
        instructions,
        pinned_guidance(REF_SNAPSHOT),
        "C/C++ guidance is workspace-gated"
    );
    client.cancel().await.ok();
}

/// The tool list per configuration, pinned. Regenerate only for a reviewed
/// membership change: `BYTEASK_UPDATE_TOOL_SNAPSHOT=1 cargo test -p codex-toolbox tool_list`.
#[tokio::test]
async fn tool_list_snapshot_per_configuration() {
    let mut doc = serde_json::Map::new();
    for (label, workspace, trusted) in [
        ("cpp_workspace_untrusted", cpp_workspace(), false),
        ("cpp_workspace_trusted", cpp_workspace(), true),
        ("plain_workspace_untrusted", plain_workspace(), false),
    ] {
        let server = server_for(workspace.path(), trusted, GateState::off()).await;
        let names: Vec<serde_json::Value> = server
            .listed_tools()
            .iter()
            .map(|tool| serde_json::Value::String(tool.name.to_string()))
            .collect();
        doc.insert(
            label.to_string(),
            serde_json::json!({
                "tools": names,
                "guidance_fragments": server.thread.guidance().len(),
            }),
        );
    }
    let now = format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::Value::Object(doc)).expect("json")
    );
    if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
        std::fs::create_dir_all(Path::new(LIST_SNAPSHOT).parent().expect("parent")).expect("mkdir");
        std::fs::write(LIST_SNAPSHOT, &now).expect("write snapshot");
        return;
    }
    let pinned = std::fs::read_to_string(LIST_SNAPSHOT)
        .expect("snapshot missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1");
    assert_eq!(
        pinned, now,
        "the toolbox tool list changed for some configuration"
    );
}

/// Only a person may skip a check, so no tool an agent can call can skip one; the skip
/// is the `byteask checks skip` command, which refuses agent sessions.
#[tokio::test]
async fn no_tool_can_skip_or_waive_a_check() {
    for trusted in [false, true] {
        let workspace = cpp_workspace();
        let server = server_for(workspace.path(), trusted, GateState::off()).await;
        for tool in server.listed_tools() {
            let name = tool.name.to_ascii_lowercase();
            assert!(
                !["skip", "waive", "approve", "override"]
                    .iter()
                    .any(|word| name.contains(word)),
                "{name} looks like a way for the agent to skip a check"
            );
            let schema = serde_json::to_string(&tool.input_schema).expect("json");
            assert!(
                !schema.contains("\"skip_check\"") && !schema.contains("\"skip_gate\""),
                "{name} takes a skip argument"
            );
        }
    }
}

/// A tool registered in one host only is exactly the drift this crate exists
/// to prevent: the app-server must install the ByteAsk tools through the one
/// shared function, never crate by crate.
#[test]
fn the_app_server_installs_the_tools_through_the_shared_function() {
    let source = include_str!("../../../../app-server/src/extensions.rs");
    assert!(
        source.contains("codex_toolbox::install_byteask_tools(&mut builder);"),
        "app-server must call codex_toolbox::install_byteask_tools"
    );
    for direct in ["codex_cpp_tools::install(", "codex_ref_tools::install("] {
        assert!(
            !source.contains(direct),
            "app-server registers {direct} directly; use install_byteask_tools"
        );
    }
}

#[tokio::test]
async fn every_paid_tool_is_a_registry_tool() {
    let workspace = plain_workspace();
    let server = server_for(workspace.path(), false, GateState::off()).await;
    for tool in gate::PAID_TOOLS {
        assert!(
            server.thread.has_tool(tool),
            "{tool} is gated but not in the registry: it would never be refused"
        );
    }
}

fn call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    let serde_json::Value::Object(arguments) = arguments else {
        panic!("arguments must be an object");
    };
    CallToolRequestParams::new(name.to_string()).with_arguments(arguments)
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// With the served switch OFF (the default), a paid tool runs exactly as in
/// the CLI: here its own argument check answers, which proves the call reached
/// the tool rather than the gate.
#[tokio::test]
async fn switch_off_reaches_the_tool_exactly_as_the_cli_does() {
    let workspace = plain_workspace();
    let client = connect(server_for(workspace.path(), false, GateState::off()).await).await;
    let result = client
        .call_tool(call(
            "cpp_bench",
            serde_json::json!({ "not_an_argument": 1 }),
        ))
        .await
        .expect("tools/call");
    let text = text_of(&result);
    assert!(!text.contains("ByteAsk Pro"), "{text}");
    assert!(!text.contains("byteask login"), "{text}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn never_signed_in_refuses_a_paid_tool_with_the_sign_in_step() {
    let workspace = plain_workspace();
    let gate = GateState {
        switch_on: true,
        identity: Identity::SignedOut,
        cache: None,
    };
    let client = connect(server_for(workspace.path(), true, gate).await).await;
    let result = client
        .call_tool(call(
            "debug_session",
            serde_json::json!({ "action": "list" }),
        ))
        .await
        .expect("tools/call");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains("byteask login"), "{text}");
    assert!(text.contains("Nothing was run"), "{text}");
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["verdict"], "NOT_RUN");
    assert_eq!(structured["reason"], "needs_plan");
    client.cancel().await.ok();
}

#[tokio::test]
async fn free_plan_refuses_paid_tools_and_free_tools_still_run() {
    let workspace = plain_workspace();
    let gate = GateState {
        switch_on: true,
        identity: Identity::SignedIn {
            account: "acct".to_string(),
        },
        cache: Some(PlanCache {
            plan: "free".to_string(),
            checked_at: now_unix(),
            account: "acct".to_string(),
            gate: true,
        }),
    };
    let client = connect(server_for(workspace.path(), true, gate).await).await;
    let refused = client
        .call_tool(call(
            "cpp_sanitize",
            serde_json::json!({ "file": "main.cpp" }),
        ))
        .await
        .expect("tools/call");
    assert!(
        text_of(&refused).contains("free plan"),
        "{}",
        text_of(&refused)
    );

    let free = client
        .call_tool(call(
            "cpp_check",
            serde_json::json!({ "path": "missing.cpp" }),
        ))
        .await
        .expect("tools/call");
    assert!(
        !text_of(&free).contains("ByteAsk Pro"),
        "{}",
        text_of(&free)
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_pro_plan_offline_within_seven_days_runs_paid_tools() {
    let workspace = plain_workspace();
    let gate = GateState {
        switch_on: true,
        identity: Identity::SignedIn {
            account: "acct".to_string(),
        },
        cache: Some(PlanCache {
            plan: "pro".to_string(),
            checked_at: now_unix() - 6 * 24 * 3600,
            account: "acct".to_string(),
            gate: true,
        }),
    };
    let client = connect(server_for(workspace.path(), true, gate).await).await;
    let result = client
        .call_tool(call(
            "cpp_bench",
            serde_json::json!({ "not_an_argument": 1 }),
        ))
        .await
        .expect("tools/call");
    assert!(
        !text_of(&result).contains("ByteAsk Pro"),
        "{}",
        text_of(&result)
    );
    client.cancel().await.ok();
}

/// Over MCP, the tool's own shared result reaches the agent as
/// structured content, and a NOT RUN is flagged as an error.
#[tokio::test]
async fn the_shared_result_travels_as_structured_content() {
    let workspace = plain_workspace();
    let client = connect(server_for(workspace.path(), false, GateState::off()).await).await;
    let result = client
        .call_tool(call(
            "cpp_check",
            serde_json::json!({ "path": "missing.cpp" }),
        ))
        .await
        .expect("tools/call");
    let structured = result
        .structured_content
        .clone()
        .expect("structured result");
    assert_eq!(structured["verdict"], "NOT_RUN", "{structured}");
    assert!(
        structured["fix"]
            .as_str()
            .is_some_and(|fix| !fix.is_empty())
    );
    assert_eq!(result.is_error, Some(true));
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let workspace = plain_workspace();
    let client = connect(server_for(workspace.path(), false, GateState::off()).await).await;
    let err = client
        .call_tool(call("shell", serde_json::json!({})))
        .await
        .expect_err("no such tool");
    assert!(err.to_string().contains("unknown tool"), "{err}");
    client.cancel().await.ok();
}

/// The MCP channel must survive whatever a tool or a child process does with
/// fds 0 and 1. Runs in a child copy of this test binary, because it rewires
/// the process's own descriptors.
#[cfg(unix)]
#[test]
fn stdio_is_private_to_the_mcp_channel() {
    use std::io::Read as _;
    use std::io::Write as _;

    if std::env::var_os("BYTEASK_TOOLBOX_STDIO_CHILD").is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            use tokio::io::AsyncBufReadExt as _;
            use tokio::io::AsyncWriteExt as _;
            let (input, mut output) = take_stdio().expect("take stdio");
            // A stray write to fd 1 must not reach the agent.
            #[allow(clippy::print_stdout)]
            {
                println!("STRAY-STDOUT");
            }
            // A child that inherits stdin must see EOF, not the agent's bytes.
            let child = std::process::Command::new("cat")
                .stdin(std::process::Stdio::inherit())
                .output()
                .expect("cat runs");
            assert!(child.stdout.is_empty(), "the child read the MCP stream");
            let mut lines = tokio::io::BufReader::new(input).lines();
            let line = lines.next_line().await.expect("read").expect("a line");
            output
                .write_all(format!("ECHO:{line}\n").as_bytes())
                .await
                .expect("write");
            output.flush().await.expect("flush");
        });
        return;
    }

    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "server::tests::stdio_is_private_to_the_mcp_channel",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("BYTEASK_TOOLBOX_STDIO_CHILD", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn child");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\"}\n")
        .expect("write request");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let output = child.wait_with_output().expect("child exits");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed: {stderr}");
    // libtest writes its header (and the unterminated "test <name> ... ") to fd 1
    // before the test body takes stdio; after that the channel carries exactly
    // the echo, and every later write to fd 1 (the stray print, libtest's own
    // result line) is on stderr.
    assert!(
        stdout.ends_with("... ECHO:{\"jsonrpc\":\"2.0\"}\n"),
        "stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        !stdout.contains("test result"),
        "libtest's result line reached the channel: {stdout}"
    );
    assert!(
        !stdout.contains("STRAY-STDOUT"),
        "a stray write reached the channel: {stdout}"
    );
    assert!(
        stderr.contains("STRAY-STDOUT"),
        "the stray write went to stderr: {stderr}"
    );
}
