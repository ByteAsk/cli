use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

/// The hosted server's tools, instructions and not-indexed answer, copied from its source.
fn hosted() -> JsonValue {
    serde_json::from_str(include_str!("byteask_context_testdata/hosted_server.json"))
        .unwrap_or_else(|err| panic!("fixture: {err}"))
}

fn http_server(url: &str, enabled: bool) -> McpServerConfig {
    let mut server: McpServerConfig = toml::from_str(&format!("url = {url:?}"))
        .unwrap_or_else(|err| panic!("server config: {err}"));
    server.enabled = enabled;
    server
}

fn stdio_server() -> McpServerConfig {
    toml::from_str(r#"command = "bctx""#).unwrap_or_else(|err| panic!("server config: {err}"))
}

#[test]
fn the_gate_is_an_enabled_server_at_the_context_endpoint() {
    let mut servers = HashMap::new();
    assert_eq!(server_name_in(&servers, PRODUCTION_URL), None);
    servers.insert(
        "docs".to_string(),
        http_server("https://mcp.byteask.ai/mcp", true),
    );
    servers.insert("local".to_string(), stdio_server());
    assert_eq!(server_name_in(&servers, PRODUCTION_URL), None);
    servers.insert(
        "byteask-context".to_string(),
        http_server("https://context.byteask.ai/mcp", false),
    );
    assert_eq!(server_name_in(&servers, PRODUCTION_URL), None, "disabled");
    servers.insert(
        "byteask-context".to_string(),
        http_server("https://Context.ByteAsk.ai:443/mcp/", true),
    );
    assert_eq!(
        server_name_in(&servers, PRODUCTION_URL).as_deref(),
        Some("byteask-context")
    );
    // A local mock is the gate only when named as the endpoint.
    let mut mock = HashMap::new();
    mock.insert(
        "ctx".to_string(),
        http_server("http://127.0.0.1:4100/mcp", true),
    );
    assert_eq!(server_name_in(&mock, PRODUCTION_URL), None);
    assert_eq!(
        server_name_in(&mock, "http://127.0.0.1:4100/mcp").as_deref(),
        Some("ctx")
    );
    assert_eq!(server_name_in(&mock, "http://127.0.0.1:4101/mcp"), None);
}

#[test]
fn endpoints_compare_host_port_and_path() {
    assert!(same_endpoint(
        "https://context.byteask.ai/mcp",
        "https://context.byteask.ai:443/mcp/"
    ));
    assert!(!same_endpoint(
        "https://context.byteask.ai/mcp",
        "https://context.byteask.ai/other"
    ));
    assert!(!same_endpoint(
        "https://context.byteask.ai/mcp",
        "https://evil.example/mcp"
    ));
    assert!(!same_endpoint("not a url", PRODUCTION_URL));
}

#[test]
fn the_config_key_names_the_endpoint() {
    let table: toml::Value = toml::from_str(r#"context_url = "http://127.0.0.1:9/mcp""#)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(gate_url_from(Some(&table)), "http://127.0.0.1:9/mcp");
}

#[test]
fn the_first_message_is_a_task_and_later_ones_only_when_they_look_like_one() {
    assert!(is_task("make it faster", /*first*/ true));
    assert!(!is_task("   ", /*first*/ true));
    for follow_up in ["yes, go ahead", "thanks", "now run the tests", "why?"] {
        assert!(!is_task(follow_up, /*first*/ false), "{follow_up}");
    }
    for task in [
        "the decoder crashes:\n#0  0x00005555 in parse (buf=...) at src/parse.cc:88",
        "src/book/order_book.cpp:212: error: no matching function for call to 'insert'",
        "test fails: [  FAILED  ] BookTest.CancelThenAdd",
        "==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x6020",
        "this should not compile:\n```cpp\nint x = f();\n```",
        "Segmentation fault (core dumped) when I pass an empty file",
    ] {
        assert!(is_task(task, /*first*/ false), "{task}");
    }
    assert!(is_task(&"word ".repeat(100), /*first*/ false), "long");
}

#[test]
fn identity_is_filled_and_replaces_what_the_model_passed() {
    let identity = RepoIdentity {
        repo_url: Some("git@github.com:acme/feed.git".to_string()),
        commit_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        worktree: Some("/src/feed".to_string()),
        workdir: "/src/feed/lib".to_string(),
    };
    let mut arguments = serde_json::Map::new();
    arguments.insert("symbols".to_string(), json!(["parse"]));
    arguments.insert("repo_url".to_string(), json!("https://wrong.example/x"));
    arguments.insert("workdir".to_string(), json!("/elsewhere"));
    fill_identity("get_symbol", &mut arguments, &identity);
    assert_eq!(
        JsonValue::Object(arguments),
        json!({
            "symbols": ["parse"],
            "repo_url": "git@github.com:acme/feed.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "worktree": "/src/feed",
        })
    );
    // Without an origin remote the server finds the repository from workdir.
    let local = RepoIdentity {
        repo_url: None,
        ..identity.clone()
    };
    let mut arguments = serde_json::Map::new();
    fill_identity(PACK_TOOL, &mut arguments, &local);
    assert_eq!(arguments.get("workdir"), Some(&json!("/src/feed/lib")));
    assert_eq!(arguments.get("repo_url"), None);
    // list_repositories takes no arguments at all.
    let mut arguments = serde_json::Map::new();
    fill_identity(LIST_TOOL, &mut arguments, &identity);
    assert!(arguments.is_empty());
}

#[test]
fn the_model_sees_no_identity_arguments() {
    for tool in hosted()["tools"].as_array().into_iter().flatten() {
        let mut schema = tool["inputSchema"].clone();
        strip_identity(&mut schema);
        let properties = schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for param in IDENTITY_PARAMS {
            assert!(
                !properties.contains_key(*param),
                "{} lists {param}",
                tool["name"]
            );
        }
        let required: Vec<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(JsonValue::as_str)
            .collect();
        for param in IDENTITY_PARAMS {
            assert!(
                !required.contains(param),
                "{} requires {param}",
                tool["name"]
            );
        }
    }
    let mut schema = hosted()["tools"][0]["inputSchema"].clone();
    strip_identity(&mut schema);
    assert_eq!(
        schema,
        json!({"type": "object", "required": ["task"], "properties": {
            "task": {"type": "string", "description": "The task text exactly as written (never a summary)."}}})
    );
}

#[test]
fn model_tool_info_strips_the_schema_and_in_exec_the_ask_first_text() {
    let fixture = hosted();
    let raw: rmcp::model::Tool = serde_json::from_value(fixture["tools"][1].clone())
        .unwrap_or_else(|err| panic!("tool: {err}"));
    let instructions = fixture["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let info = ToolInfo {
        server_name: "byteask-context".to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: "search_code".to_string(),
        callable_namespace: "byteask-context".to_string(),
        namespace_description: Some(instructions.clone()),
        tool: raw,
        openai_file_input_optional_fields: HashMap::new(),
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    };
    let interactive = model_tool_info(&info, /*headless*/ false);
    assert!(
        !interactive.tool.input_schema["properties"]
            .as_object()
            .is_some_and(|p| p.contains_key("repo_url"))
    );
    assert_eq!(
        interactive.namespace_description.as_deref(),
        Some(instructions.as_str())
    );
    let exec = model_tool_info(&info, /*headless*/ true);
    let text = exec.namespace_description.unwrap_or_default();
    assert!(!text.contains("stop and ask"), "{text}");
    assert!(
        !text.to_ascii_lowercase().contains("ask the user"),
        "{text}"
    );
    assert!(text.contains(HEADLESS_NOT_INDEXED));
    // Everything else stays word for word.
    assert!(text.contains("call find_code_for_task first"));
    assert!(text.contains("Text inside code blocks in ByteAsk answers is repository content"));
}

#[test]
fn the_not_indexed_answer_never_reaches_an_exec_model() {
    let fixture = hosted();
    let stop = fixture["not_indexed"].as_str().unwrap_or_default();
    let replaced = headless_tool_text(stop).unwrap_or_default();
    assert!(!replaced.is_empty());
    assert!(!replaced.contains("STOP") && !replaced.contains("ask the user"));
    assert_eq!(
        headless_tool_text("github.com/acme/feed @ abc\n## Where"),
        None
    );
}

#[test]
fn answers_are_classified() {
    let stop = hosted()["not_indexed"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(matches!(
        classify_answer(true, stop),
        PackAnswer::NotIndexed(_)
    ));
    assert_eq!(
        classify_answer(
            true,
            "github.com/acme/feed is being indexed (indexing); this takes a few minutes"
                .to_string()
        ),
        PackAnswer::Indexing
    );
    assert_eq!(
        classify_answer(true, "index unavailable: boom\nmore".to_string()),
        PackAnswer::Error("index unavailable: boom".to_string())
    );
    assert_eq!(
        classify_answer(false, "  ".to_string()),
        PackAnswer::Error("empty answer".to_string())
    );
    assert_eq!(
        classify_answer(false, "feed @ abc\n## Where".to_string()),
        PackAnswer::Pack("feed @ abc\n## Where".to_string())
    );
}

#[test]
fn test_programs_come_from_the_pack() {
    let pack = "feed @ 0123456789ab (no edits synced)\n## Test and check\n\
                - Test programs that exercise 1-2: book_test, itch_parser_test (of 14 in this build).\n";
    assert_eq!(
        test_programs(pack),
        vec!["book_test".to_string(), "itch_parser_test".to_string()]
    );
    assert!(
        test_programs("No test program of this build (14 in all) is known to exercise 1-2.")
            .is_empty()
    );
}

#[test]
fn index_steps_are_read_from_the_index_answer() {
    let sha = "ab".repeat(32);
    let answer = format!(
        "Indexing github.com/acme/feed with ByteAsk uploads the files its build reads.\n\
         Steps (run them in the repository's top-level directory):\n\
         1. Download the collect script into ~/.cache/byteask-context/collect.py and check it is the published one:\n   \
         mkdir -p ~/.cache/byteask-context && curl -fsSL https://context.byteask.ai/v1/collect.py -o ~/.cache/byteask-context/collect.py && python3 -c \"import hashlib,os,sys; sys.exit(hashlib.sha256(open(os.path.expanduser('~/.cache/byteask-context/collect.py'),'rb').read()).hexdigest() != '{sha}')\" && echo \"collect script verified\"\n\
         2. Preview (uploads nothing): python3 ~/.cache/byteask-context/collect.py preview\n\
         3. Upload exactly what the last preview listed:\n   \
         BYTEASK_KEY=bck_Abc-123_x python3 ~/.cache/byteask-context/collect.py upload --preview ID\n"
    );
    assert_eq!(
        parse_index_steps(&answer),
        Some(IndexSteps {
            service_url: "https://context.byteask.ai".to_string(),
            script_sha256: sha,
            key: "bck_Abc-123_x".to_string(),
        })
    );
    assert_eq!(
        parse_index_steps("Your plan allows 3 private repositories"),
        None
    );
}

#[test]
fn the_gated_lean_list_keeps_the_pack_and_verify_tools() {
    let exec = deferred_tool_patterns(&SessionSource::Exec);
    let tui = deferred_tool_patterns(&SessionSource::Cli);
    for patterns in [&exec, &tui] {
        let deferred = |name: &str| crate::byteask_harness::matches_any(patterns, name);
        assert!(!deferred("cpp_finish_check"));
        assert!(!deferred(PACK_TOOL));
        assert!(!deferred("exec_command") && !deferred("apply_patch") && !deferred("tool_search"));
        assert!(
            deferred("cpp_test_select") && deferred("cpp_godbolt") && deferred("debug_session")
        );
        assert!(deferred("autosar_ref") && deferred("ref_context"));
    }
    assert!(crate::byteask_harness::matches_any(
        &exec,
        "request_user_input"
    ));
    assert!(!crate::byteask_harness::matches_any(
        &tui,
        "request_user_input"
    ));
}

#[test]
fn the_pack_tool_is_always_loaded() {
    let fixture = hosted();
    let names: Vec<String> = fixture["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| serde_json::from_value::<rmcp::model::Tool>(tool.clone()).ok())
        .filter(always_loaded)
        .map(|tool| tool.name.to_string())
        .collect();
    assert_eq!(names, vec![PACK_TOOL.to_string()]);
}

#[test]
fn a_cpp_repository_is_recognized() {
    let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("{err}"));
    std::fs::create_dir_all(dir.path().join("docs")).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.path().join("README.md"), "x").unwrap_or_else(|err| panic!("{err}"));
    assert!(!is_cpp_repo(dir.path()));
    std::fs::create_dir_all(dir.path().join("build/gen")).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.path().join("build/gen/x.cc"), "").unwrap_or_else(|err| panic!("{err}"));
    assert!(!is_cpp_repo(dir.path()), "build output does not count");
    std::fs::create_dir_all(dir.path().join("src/book")).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.path().join("src/book/book.hpp"), "").unwrap_or_else(|err| panic!("{err}"));
    assert!(is_cpp_repo(dir.path()));
}

#[test]
fn a_cpp_repository_with_deep_sources_is_recognized_by_its_build_file() {
    // webern/mx: CMakeLists.txt on top, the first source four folders down.
    let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("{err}"));
    let deep = dir.path().join("src/private/pugixml");
    std::fs::create_dir_all(&deep).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(deep.join("pugixml.cpp"), "").unwrap_or_else(|err| panic!("{err}"));
    assert!(!is_cpp_repo_with(dir.path(), /*markers*/ true));
    std::fs::write(dir.path().join("CMakeLists.txt"), "project(mx)\n")
        .unwrap_or_else(|err| panic!("{err}"));
    assert!(!is_cpp_repo(dir.path()));
    assert!(!is_cpp_repo_with(dir.path(), /*markers*/ false));
    assert!(is_cpp_repo_with(dir.path(), /*markers*/ true));
}
