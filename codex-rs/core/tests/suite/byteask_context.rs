//! ByteAsk Context in the harness (CLI 0.2.0) against a local mock of the hosted MCP
//! server (its tools, instructions and answers copied from the server's source) and a mock
//! Responses API. Never the production server.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_protocol::protocol::ContextPackEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::apps_test_server::configure_search_capable_model;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const SERVER: &str = "byteask-context";
const ORIGIN: &str = "git@github.com:acme/feed.git";
const PROMPT: &str = "The book drops the cancel when an add arrives first:\n\
                      #0  0x0000555555 in Book::cancel (id=7) at src/book.cpp:42";
const PACK: &str = "github.com/acme/feed @ 0123456789ab (no edits synced)\n\
                    ## Where the change most likely belongs\n\
                    1. Book::cancel - src/book.cpp:40-58\n\
                    ## Test and check\n\
                    - Test programs that exercise 1-2: book_test (of 3 in this build).";

/// The hosted server's tools, instructions and not-indexed answer.
fn hosted() -> Value {
    serde_json::from_str(include_str!(
        "../../src/byteask_context_testdata/hosted_server.json"
    ))
    .unwrap_or_else(|err| panic!("fixture: {err}"))
}

#[derive(Clone)]
enum PackBehavior {
    Answer(String),
    ToolError(String),
    Delay(Duration),
}

/// The mock Context server: JSON-RPC over plain HTTP responses, like the hosted one.
#[derive(Clone)]
struct ContextMock {
    pack: PackBehavior,
    /// Answer of every other tool (`search_code`, `get_symbol`, ...): (is_error, text).
    other: (bool, String),
    index: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl ContextMock {
    fn new(pack: PackBehavior) -> Self {
        Self {
            pack,
            other: (false, "github.com/acme/feed @ 0123456789ab\n- Book::cancel".to_string()),
            index: "github.com/acme/feed is being indexed now (indexing, started 3 s ago). Work as usual; call list_repositories to check.".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn calls_to(&self, tool: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args)
            .collect()
    }

    async fn mount(self) -> (MockServer, Self) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(self.clone())
            .mount(&server)
            .await;
        (server, self)
    }
}

fn tool_result(id: Value, is_error: bool, text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "jsonrpc": "2.0", "id": id,
        "result": {"content": [{"type": "text", "text": text}], "isError": is_error},
    }))
}

impl Respond for ContextMock {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return ResponseTemplate::new(400);
        };
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        match body.get("method").and_then(Value::as_str).unwrap_or_default() {
            "initialize" => ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": body.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "byteask", "title": "ByteAsk", "version": "test"},
                    "instructions": hosted()["instructions"],
                },
            })),
            "tools/list" => ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": id, "result": {"tools": hosted()["tools"]},
            })),
            "tools/call" => {
                let name = body
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let args = body
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push((name.clone(), args));
                }
                match (name.as_str(), &self.pack) {
                    ("find_code_for_task", PackBehavior::Answer(text)) => tool_result(id, false, text),
                    ("find_code_for_task", PackBehavior::ToolError(text)) => tool_result(id, true, text),
                    ("find_code_for_task", PackBehavior::Delay(delay)) => {
                        tool_result(id, false, PACK).set_delay(*delay)
                    }
                    ("index_repository", _) => tool_result(id, false, &self.index),
                    _ => tool_result(id, self.other.0, &self.other.1),
                }
            }
            _ if body.get("id").is_none() => ResponseTemplate::new(202),
            _ => ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A C++ git repository with an origin remote, in `cwd`.
fn cpp_repo(cwd: &Path, origin: &str) {
    std::fs::create_dir_all(cwd.join("src")).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(
        cwd.join("src/book.cpp"),
        "int cancel(int id) { return id; }\n",
    )
    .unwrap_or_else(|err| panic!("{err}"));
    git(cwd, &["init", "-q"]);
    git(cwd, &["remote", "add", "origin", origin]);
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-q", "-m", "init"]);
}

fn add_context_server(config: &mut Config, url: &str, harness: &str) {
    add_server(config, &format!("url = {url:?}"), harness);
}

fn add_server(config: &mut Config, server_toml: &str, harness: &str) {
    let mut servers = config.mcp_servers.get().clone();
    let mut server: McpServerConfig =
        toml::from_str(server_toml).unwrap_or_else(|err| panic!("{err}"));
    server.startup_timeout_sec = Some(Duration::from_secs(10));
    assert!(matches!(
        server.transport,
        McpServerTransportConfig::StreamableHttp { .. }
    ));
    servers.insert(SERVER.to_string(), server);
    config
        .mcp_servers
        .set(servers)
        .unwrap_or_else(|err| panic!("mcp servers: {err}"));
    let table: toml::Value = toml::from_str(harness).unwrap_or_else(|err| panic!("{err}"));
    config.config_layer_stack = codex_config::ConfigLayerStack::new(
        vec![codex_config::ConfigLayerEntry::new(
            codex_config::ConfigLayerSource::SessionFlags,
            table,
        )],
        codex_config::ConfigRequirements::default(),
        codex_config::ConfigRequirementsToml::default(),
    )
    .unwrap_or_else(|err| panic!("config layer stack: {err}"));
}

/// `[byteask_harness]` naming the mock as the Context endpoint (the gate), plus `extra`.
fn gate_on(mock: &MockServer, extra: &str) -> String {
    format!(
        "[byteask_harness]\ncontext_url = \"{}/mcp\"\n{extra}",
        mock.uri()
    )
}

async fn build(
    responses_server: &MockServer,
    url: String,
    harness: String,
    origin: &'static str,
    search: bool,
) -> Result<TestCodex> {
    let mut builder = test_codex()
        .with_workspace_setup(move |cwd, _fs| async move {
            cpp_repo(cwd.as_path(), origin);
            Ok(())
        })
        .with_config(move |config| {
            if search {
                configure_search_capable_model(config);
            }
            add_context_server(config, &url, &harness);
        });
    builder.build(responses_server).await
}

/// Runs one user turn and returns the turn's events.
async fn run_turn(test: &TestCodex, prompt: &str) -> Result<Vec<EventMsg>> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut events = Vec::new();
    loop {
        let event =
            tokio::time::timeout(Duration::from_secs(60), test.codex.next_event()).await??;
        let done = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if done {
            return Ok(events);
        }
    }
}

fn pack_events(events: &[EventMsg]) -> Vec<ContextPackEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            EventMsg::ContextPack(event) => Some(event.clone()),
            _ => None,
        })
        .collect()
}

fn reply() -> String {
    sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ])
}

/// Every text of every message in the request, by role, in order.
fn texts(request: &ResponsesRequest, role: &str) -> Vec<String> {
    request.message_input_texts(role)
}

/// The Context server's namespace tool in the request's tool list.
fn context_namespace(body: &Value) -> Option<Value> {
    body["tools"].as_array()?.iter().find_map(|tool| {
        (tool["type"] == "namespace"
            && tool["name"]
                .as_str()
                .is_some_and(|name| name.contains("byteask")))
        .then(|| tool.clone())
    })
}

// ---- The turn-0 pack --------------------------------------------------------------------------

/// The background builds the harness asked for, as (workspace, test programs).
static WARM_BUILDS: Mutex<Vec<(std::path::PathBuf, Vec<String>)>> = Mutex::new(Vec::new());

/// The line the recorded host offers for a pack that names no test programs.
const CHECK_LINE: &str = "To check your change: cmake --build /b --target book_test -j2 && ctest --test-dir /b -R '^(book)$' --output-on-failure";

fn record_warm_build(
    _config: &Config,
    workspace: &Path,
    programs: &[String],
    _pack: &str,
) -> Option<codex_core::byteask_context::WarmBuildStarted> {
    if let Ok(mut builds) = WARM_BUILDS.lock() {
        builds.push((workspace.to_path_buf(), programs.to_vec()));
    }
    let check_line = programs.is_empty().then(|| {
        Box::pin(async {
            Some(codex_core::byteask_context::CheckLine {
                line: CHECK_LINE.to_string(),
                targets: vec!["book_test".to_string()],
            })
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    });
    Some(codex_core::byteask_context::WarmBuildStarted {
        cancel: Box::new(|| {}),
        check_line,
    })
}

/// A pack without test programs gets the host's check line as its last line, and the event
/// names the line's targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pack_without_test_programs_gets_the_check_line() -> Result<()> {
    skip_if_no_network!(Ok(()));
    codex_core::byteask_context::set_warm_build(record_warm_build);
    let pack = PACK
        .rsplit_once("\n## Test and check")
        .map_or(PACK, |(head, _)| head);
    let responses_server = responses::start_mock_server().await;
    let (mcp, _mock) = ContextMock::new(PackBehavior::Answer(pack.to_string()))
        .mount()
        .await;
    let first = mount_sse_once(&responses_server, reply()).await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, ""),
        ORIGIN,
        false,
    )
    .await?;
    let events = run_turn(&test, PROMPT).await?;
    let request = first.single_request();
    let user = texts(&request, "user");
    let injected = user
        .iter()
        .find(|text| text.starts_with("<byteask_context>"))
        .unwrap_or_else(|| panic!("pack in {user:#?}"));
    assert!(
        injected.contains(&format!("{pack}\n\n{CHECK_LINE}")),
        "{injected}"
    );
    let packs = pack_events(&events);
    assert_eq!(packs.len(), 1, "{packs:?}");
    assert!(packs[0].warm_build_started, "{:?}", packs[0]);
    assert_eq!(packs[0].test_programs, vec!["book_test".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pack_is_in_the_first_user_turn_before_the_first_model_request() -> Result<()> {
    skip_if_no_network!(Ok(()));
    codex_core::byteask_context::set_warm_build(record_warm_build);
    let responses_server = responses::start_mock_server().await;
    let (mcp, mock) = ContextMock::new(PackBehavior::Answer(PACK.to_string()))
        .mount()
        .await;
    let first = mount_sse_once(&responses_server, reply()).await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, ""),
        ORIGIN,
        false,
    )
    .await?;
    let events = run_turn(&test, PROMPT).await?;

    // The harness called find_code_for_task once, with the message verbatim and the identity.
    let calls = mock.calls_to("find_code_for_task");
    assert_eq!(calls.len(), 1, "{:?}", mock.calls());
    let cwd = test.cwd_path();
    assert_eq!(
        calls[0],
        json!({
            "task": PROMPT,
            "repo_url": ORIGIN,
            "commit_sha": git(cwd, &["rev-parse", "HEAD"]),
            "worktree": git(cwd, &["rev-parse", "--show-toplevel"]),
        })
    );

    // The first (and only) model request carries the pack in a user message right after the
    // prompt, and nowhere in the developer prefix.
    let request = first.single_request();
    let user = texts(&request, "user");
    let prompt_at = user
        .iter()
        .position(|text| text == PROMPT)
        .unwrap_or_else(|| panic!("prompt in {user:#?}"));
    let pack = &user[prompt_at + 1];
    assert!(pack.starts_with("<byteask_context>\n"), "{pack}");
    assert!(pack.contains(PACK), "{pack}");
    assert!(pack.ends_with("</byteask_context>"), "{pack}");
    for developer in texts(&request, "developer") {
        assert!(
            !developer.contains("Book::cancel - src/book.cpp"),
            "pack in the prefix"
        );
    }
    assert!(!request.instructions_text().contains("<byteask_context>"));

    // The instruction, once, in the developer prefix.
    let developer = texts(&request, "developer").join("\n");
    assert_eq!(
        developer.matches("call cpp_finish_check").count(),
        1,
        "{developer}"
    );
    assert!(developer.contains("Do not write scratch test programs or build the whole project"));

    // One event, observable in exec --json.
    let packs = pack_events(&events);
    assert_eq!(packs.len(), 1, "{packs:?}");
    let event = &packs[0];
    assert!(event.attempted && event.injected, "{event:?}");
    assert_eq!(event.reason, None);
    assert_eq!(event.answer_bytes, Some(PACK.len() as u64));
    assert_eq!(event.approx_tokens, Some((PACK.len() as u64).div_ceil(4)));
    assert_eq!(event.test_programs, vec!["book_test".to_string()]);
    assert!(event.latency_ms.is_some());
    assert_eq!(event.message, None);
    // The pack's test programs are being built in the background, in the worktree.
    assert!(event.warm_build_started);
    let top = std::path::PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]));
    let builds = WARM_BUILDS.lock().map(|b| b.clone()).unwrap_or_default();
    assert!(
        builds.contains(&(top, vec!["book_test".to_string()])),
        "{builds:?}"
    );
    Ok(())
}

/// A follow-up that is not a task gets no pack (and says why); a new task with a stack frame does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_tasks_get_a_pack() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let responses_server = responses::start_mock_server().await;
    let (mcp, mock) = ContextMock::new(PackBehavior::Answer(PACK.to_string()))
        .mount()
        .await;
    let replies = mount_sse_sequence(&responses_server, vec![reply(), reply(), reply()]).await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, ""),
        ORIGIN,
        false,
    )
    .await?;
    run_turn(&test, PROMPT).await?;
    let follow_up = run_turn(&test, "thanks, now run the tests").await?;
    let new_task = run_turn(
        &test,
        "another crash: #3 0x00007f in Book::add at src/book.cpp:12",
    )
    .await?;
    assert_eq!(mock.calls_to("find_code_for_task").len(), 2);
    let skipped = pack_events(&follow_up);
    assert_eq!(skipped.len(), 1);
    assert!(!skipped[0].attempted);
    assert_eq!(skipped[0].reason.as_deref(), Some("not_a_task"));
    assert!(pack_events(&new_task)[0].injected);
    let second = &replies.requests()[1];
    let packs = texts(second, "user")
        .iter()
        .filter(|text| text.starts_with("<byteask_context>"))
        .count();
    assert_eq!(packs, 1, "the follow-up added no second pack");
    Ok(())
}

// ---- failure paths ----------------------------------------------------------------------------

async fn failure_case(
    behavior: PackBehavior,
    extra: &str,
) -> Result<(Vec<EventMsg>, ResponsesRequest, ContextMock)> {
    let responses_server = responses::start_mock_server().await;
    let (mcp, mock) = ContextMock::new(behavior).mount().await;
    let first = mount_sse_once(&responses_server, reply()).await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, extra),
        ORIGIN,
        false,
    )
    .await?;
    let events = run_turn(&test, PROMPT).await?;
    Ok((events, first.single_request(), mock))
}

fn assert_no_pack(request: &ResponsesRequest) {
    for role in ["user", "developer"] {
        for text in texts(request, role) {
            assert!(!text.contains("<byteask_context>"), "{role}: {text}");
            assert!(!text.contains("STOP:"), "{role}: {text}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unindexed_repository_injects_nothing_and_says_one_line() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let stop = hosted()["not_indexed"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (events, request, mock) = failure_case(PackBehavior::ToolError(stop), "").await?;
    assert_no_pack(&request);
    let packs = pack_events(&events);
    assert_eq!(packs.len(), 1);
    assert!(packs[0].attempted && !packs[0].injected);
    assert_eq!(packs[0].reason.as_deref(), Some("not_indexed"));
    assert_eq!(packs[0].auto_index, None);
    assert!(
        packs[0]
            .message
            .as_deref()
            .is_some_and(|m| !m.contains('\n'))
    );
    assert!(
        mock.calls_to("index_repository").is_empty(),
        "not pre-approved"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_approved_indexing_calls_index_repository() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let stop = hosted()["not_indexed"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (events, request, mock) =
        failure_case(PackBehavior::ToolError(stop), "context_auto_index = true\n").await?;
    assert_no_pack(&request);
    let index = mock.calls_to("index_repository");
    assert_eq!(index.len(), 1);
    assert_eq!(index[0]["repo_url"], ORIGIN);
    let packs = pack_events(&events);
    assert_eq!(packs[0].reason.as_deref(), Some("not_indexed"));
    // The mock answers "already being indexed": nothing is started locally.
    assert_eq!(packs[0].auto_index.as_deref(), Some("already"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_error_injects_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (events, request, _mock) = failure_case(
        PackBehavior::ToolError("index unavailable: boom".to_string()),
        "",
    )
    .await?;
    assert_no_pack(&request);
    let packs = pack_events(&events);
    assert_eq!(packs[0].reason.as_deref(), Some("error"));
    assert!(
        packs[0]
            .message
            .as_deref()
            .is_some_and(|m| m.contains("index unavailable: boom"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_server_times_out_and_injects_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (events, request, _mock) = failure_case(
        PackBehavior::Delay(Duration::from_secs(8)),
        "context_pack_timeout_secs = 2\n",
    )
    .await?;
    assert_no_pack(&request);
    let packs = pack_events(&events);
    assert_eq!(packs[0].reason.as_deref(), Some("timeout"));
    assert!(packs[0].latency_ms.is_some_and(|ms| ms < 6000), "{packs:?}");
    Ok(())
}

// ---- Tool list, identity arguments, indexing --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_sees_the_pack_tool_up_front_and_no_identity_arguments() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let responses_server = responses::start_mock_server().await;
    let (mcp, _mock) = ContextMock::new(PackBehavior::Answer(PACK.to_string()))
        .mount()
        .await;
    let first = mount_sse_once(&responses_server, reply()).await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, ""),
        ORIGIN,
        true,
    )
    .await?;
    run_turn(&test, PROMPT).await?;
    let body = first.single_request().body_json();
    let namespace = context_namespace(&body)
        .unwrap_or_else(|| panic!("no Context namespace in {:#}", body["tools"]));
    let direct: Vec<&str> = namespace["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        direct,
        vec!["find_code_for_task"],
        "only the pack tool up front"
    );
    let schema = &namespace["tools"][0]["parameters"];
    for param in ["repo_url", "commit_sha", "worktree", "workdir"] {
        assert!(
            schema["properties"].get(param).is_none(),
            "{param} listed: {schema:#}"
        );
    }
    assert_eq!(schema["required"], json!(["task"]));
    // The other Context tools stay findable through tool_search.
    assert!(
        body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|tool| tool["type"] == "tool_search"),
        "{:#}",
        body["tools"]
    );
    // In exec the server instructions no longer tell the model to stop and ask.
    let description = namespace["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("call find_code_for_task first"),
        "{description}"
    );
    assert!(!description.contains("stop and ask"), "{description}");
    assert!(!description.contains("ask the user"), "{description}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_calls_get_the_identity_filled_and_never_a_stop_answer() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let responses_server = responses::start_mock_server().await;
    let mut context = ContextMock::new(PackBehavior::Answer(PACK.to_string()));
    context.other = (
        true,
        hosted()["not_indexed"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    );
    let (mcp, mock) = context.mount().await;
    let test = build(
        &responses_server,
        format!("{}/mcp", mcp.uri()),
        gate_on(&mcp, ""),
        ORIGIN,
        false,
    )
    .await?;
    // The model's namespace for the server, as the first request lists it.
    let probe = mount_sse_once(&responses_server, reply()).await;
    run_turn(&test, PROMPT).await?;
    let namespace = context_namespace(&probe.single_request().body_json())
        .and_then(|tool| tool["name"].as_str().map(str::to_string))
        .unwrap_or_else(|| "mcp__byteask-context".to_string());

    let replies = mount_sse_sequence(
        &responses_server,
        vec![
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    "call-1",
                    &namespace,
                    "get_symbol",
                    r#"{"symbols":["Book::cancel"],"repo_url":"https://evil.example/x"}"#,
                ),
                ev_completed("resp-2"),
            ]),
            reply(),
        ],
    )
    .await;
    run_turn(
        &test,
        "and look at Book::cancel; it should not compile: `cancel(int id) {`",
    )
    .await?;
    let calls = mock.calls_to("get_symbol");
    assert_eq!(calls.len(), 1, "{:?}", mock.calls());
    let cwd = test.cwd_path();
    assert_eq!(
        calls[0],
        json!({
            "symbols": ["Book::cancel"],
            "repo_url": ORIGIN,
            "commit_sha": git(cwd, &["rev-parse", "HEAD"]),
            "worktree": git(cwd, &["rev-parse", "--show-toplevel"]),
        })
    );
    // The answer the model gets back is not the server's STOP text (exec).
    let output = replies.requests()[1]
        .function_call_output("call-1")
        .to_string();
    assert!(!output.contains("STOP"), "{output}");
    assert!(output.contains("do the task without ByteAsk"), "{output}");
    Ok(())
}

// ---- The same prefix in every workspace; the gate --------------------------------------

/// The parts of a request that must not differ between sessions: instructions, tools and every
/// developer message.
fn prefix(request: &ResponsesRequest) -> Value {
    let body = request.body_json();
    json!({
        "instructions": body["instructions"],
        "tools": body["tools"],
        "developer": texts(request, "developer"),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prefix_is_identical_across_workspaces() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let mut prefixes = Vec::new();
    for origin in [
        "git@github.com:acme/feed.git",
        "https://github.com/other/engine.git",
    ] {
        let responses_server = responses::start_mock_server().await;
        let (mcp, _mock) = ContextMock::new(PackBehavior::Answer(PACK.to_string()))
            .mount()
            .await;
        let first = mount_sse_once(&responses_server, reply()).await;
        let test = build(
            &responses_server,
            format!("{}/mcp", mcp.uri()),
            // The same endpoint string in both sessions (the mock port differs).
            format!("[byteask_harness]\ncontext_url = \"{}/mcp\"\n", mcp.uri()),
            origin,
            true,
        )
        .await?;
        run_turn(&test, PROMPT).await?;
        let mut prefix = prefix(&first.single_request());
        // Only the mock's port may differ; normalize it.
        let text = prefix.to_string().replace(&mcp.uri(), "MOCK");
        prefix = serde_json::from_str(&text)?;
        prefixes.push((test.cwd_path().to_path_buf(), prefix));
    }
    assert_ne!(prefixes[0].0, prefixes[1].0, "two workspaces");
    assert_eq!(prefixes[0].1, prefixes[1].1);
    let developer = prefixes[0].1["developer"].to_string();
    assert!(
        !developer.contains("<permissions instructions>"),
        "exec drops the permissions block"
    );
    Ok(())
}

/// The same server at another URL is not ByteAsk Context: no pack, no event, no instruction,
/// the tool schema untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_server_changes_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let responses_server = responses::start_mock_server().await;
    let (mcp, mock) = ContextMock::new(PackBehavior::Answer(PACK.to_string()))
        .mount()
        .await;
    let first = mount_sse_once(&responses_server, reply()).await;
    // Every tool of the server up front, so its schema can be read from the request.
    let server_toml = format!(
        "url = \"{}/mcp\"\nomit_tools_from = [\"deferred\"]\n",
        mcp.uri()
    );
    let test = test_codex()
        .with_workspace_setup(|cwd, _fs| async move {
            cpp_repo(cwd.as_path(), ORIGIN);
            Ok(())
        })
        .with_config(move |config| add_server(config, &server_toml, "[byteask_harness]\n"))
        .build(&responses_server)
        .await?;
    // Nothing here waits for the server before the first request (the pack call does when
    // gated), so wait for it to list its tools.
    core_test_support::wait_for_mcp_server(&test.codex, SERVER).await?;
    let events = run_turn(&test, PROMPT).await?;
    assert!(mock.calls_to("find_code_for_task").is_empty());
    assert!(pack_events(&events).is_empty());
    let request = first.single_request();
    assert_no_pack(&request);
    assert!(
        !texts(&request, "developer")
            .join("\n")
            .contains("cpp_finish_check")
    );
    let body = request.body_json();
    let namespace = context_namespace(&body).unwrap_or_else(|| {
        let names: Vec<String> = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tool| format!("{}:{}", tool["type"], tool["name"]))
            .collect();
        panic!("no Context namespace in {names:?}")
    });
    let schema = namespace["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["name"] == "find_code_for_task")
        .map(|tool| tool["parameters"].clone())
        .unwrap_or_default();
    assert!(schema["properties"].get("repo_url").is_some(), "{schema:#}");
    Ok(())
}
