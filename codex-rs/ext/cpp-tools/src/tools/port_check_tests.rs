use std::collections::BTreeMap;

use byteask_port_check::check::RecordingFormat;
use byteask_port_check::session::ProcessSpec;
use byteask_port_check::sim::HarnessConfig;
use check_result::Verdict;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn port_check_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../port-check")
}

fn spec(argv: &[&str]) -> ProcessSpec {
    ProcessSpec {
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        env: BTreeMap::new(),
        cwd: None,
        launch: None,
    }
}

fn harness_result(status: Status, sentence: &str, reason: Option<&str>) -> PortCheckResult {
    PortCheckResult {
        tool: "port_check",
        status,
        reason: reason.map(str::to_string),
        sentence: sentence.to_string(),
        evidence: json!({}),
        first_difference: None,
        perf: json!({}),
    }
}

#[test]
fn every_harness_status_maps_onto_the_shared_result_and_only_pass_is_success() {
    let cases = [
        (Status::Pass, "PASS: on all 600 records", None, Verdict::Pass),
        (Status::Fail, "FAIL: step 41 differs", None, Verdict::Fail),
        (Status::NoClaim, "NO CLAIM: nothing traded", None, Verdict::NoClaim),
        (Status::NotRun, "NOT RUN: cpp timed out", Some("cpp timed out after 30 s"), Verdict::NotRun),
    ];
    for (status, sentence, reason, want) in cases {
        let result = verdict(&harness_result(status, sentence, reason));
        assert_eq!(result.verdict, want, "{sentence}");
        assert_eq!(result.is_pass(), want == Verdict::Pass);
        result.validate().expect("the result keeps the contract");
    }
    // The verdict word is said once, by the result line.
    let line = verdict(&harness_result(Status::Pass, "PASS: on all 400 records", None)).render_line();
    assert_eq!(line, "Result: PASS: on all 400 records");
    let timed = verdict(&harness_result(Status::NotRun, "NOT RUN: x", Some("cpp timed out")));
    assert_eq!(timed.reason, Some(NotRunReason::TimeLimit));
    let bad = verdict(&harness_result(Status::NotRun, "NOT RUN: x", Some("the request is not valid: y")));
    assert_eq!(bad.reason, Some(NotRunReason::BadArguments));
}

#[test]
fn relative_paths_mean_the_thread_workspace() {
    let mut req: PortCheckRequest = serde_json::from_value(json!({
        "recording": "data/market.jsonl",
        "initial_book": "/abs/book.jsonl",
        "python": {"argv": ["python3", "s.py"]},
        "cpp": {"argv": ["./build/s"], "cwd": "port"},
        "workdir": "out"
    }))
    .expect("request");
    resolve_paths(&mut req, Path::new("/ws"));
    assert_eq!(req.recording, PathBuf::from("/ws/data/market.jsonl"));
    assert_eq!(req.initial_book, Some(PathBuf::from("/abs/book.jsonl")));
    assert_eq!(req.workdir, Some(PathBuf::from("/ws/out")));
    assert_eq!(req.python.cwd, Some(PathBuf::from("/ws")));
    assert_eq!(req.cpp.cwd, Some(PathBuf::from("/ws/port")));
}

#[test]
fn port_check_runs_its_strategies_with_no_network_and_a_read_only_workspace() {
    assert_eq!(crate::sandbox::profile_for(TOOL_NAME), Some(crate::sandbox::Profile::Analyze));
}

#[tokio::test]
async fn an_untrusted_project_is_refused_before_anything_runs() {
    let tool = PortCheckTool {
        cwd: PathBuf::from("/workspace"),
        native_exec: false,
    };
    let err = crate::test_support::call(
        &tool,
        json!({"recording": "r", "python": {"argv": ["python3"]}, "cpp": {"argv": ["x"]}}),
    )
    .await
    .expect_err("refused");
    let text = crate::sandbox::error_text(err);
    assert!(text.contains(crate::trust::ALLOW_ENV), "{text}");
}

#[test]
fn the_spec_carries_the_pinned_description_and_schema_properties() {
    let tool = PortCheckTool {
        cwd: PathBuf::from("/workspace"),
        native_exec: false,
    };
    let ToolSpec::Function(spec) = <PortCheckTool as ToolExecutor<ToolCall<'_>>>::spec(&tool) else {
        panic!("a function tool");
    };
    assert_eq!(spec.description, TOOL_DESCRIPTION);
    let props = spec.parameters.properties.expect("properties");
    for key in ["recording", "python", "cpp", "harness", "diag_tolerance", "limits", "workdir"] {
        assert!(props.contains_key(key), "{key} missing from the schema");
    }
    assert_eq!(
        spec.parameters.required,
        Some(vec!["recording".to_string(), "python".to_string(), "cpp".to_string()])
    );
}

// Live: the engine's real sandbox helper (BYTEASK_TEST_SANDBOX_HELPER, as in sandbox_tests).

fn live_helper() -> Option<PathBuf> {
    let helper = PathBuf::from(std::env::var_os("BYTEASK_TEST_SANDBOX_HELPER")?);
    (cfg!(target_os = "linux") && helper.is_file()).then_some(helper)
}

fn build_port(dir: &Path) -> PathBuf {
    let out = dir.join("imbalance");
    let o = std::process::Command::new("g++")
        .args(["-std=c++17", "-O2"])
        .arg("-I")
        .arg(port_check_dir().join("adapters"))
        .arg(port_check_dir().join("tests/fixtures/imbalance.cpp"))
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run g++");
    assert!(o.status.success(), "g++ failed: {}", String::from_utf8_lossy(&o.stderr));
    out
}

/// The whole check through the engine's tool layers, inside the real sandbox: the honest
/// port passes, the output says the strategies ran sandboxed, and a strategy that tries to
/// write into the workspace cannot (Analyze profile), while the check itself still works.
#[tokio::test]
async fn live_an_honest_port_passes_inside_the_sandbox_and_cannot_write_the_workspace() {
    let Some(helper) = live_helper() else {
        eprintln!("skip: BYTEASK_TEST_SANDBOX_HELPER not set");
        return;
    };
    // Under $HOME, never a temp dir: temp dirs are writable by design.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let ws = tempfile::tempdir_in(home).expect("workspace");
    let cpp = build_port(ws.path());
    std::fs::copy(port_check_dir().join("tests/fixtures/imbalance.py"), ws.path().join("imbalance.py"))
        .expect("copy");
    let escaped = ws.path().join("escaped.txt");
    std::fs::write(
        ws.path().join("escape_then_run.py"),
        format!(
            "import runpy\ntry:\n    open({:?}, 'w').write('x')\nexcept OSError:\n    pass\nrunpy.run_path('imbalance.py', run_name='__main__')\n",
            escaped.display().to_string()
        ),
    )
    .expect("write launcher");
    let mut harness = HarnessConfig::standard();
    harness.seed = 20261018;
    harness.reject_every = 7;
    harness.max_position = Some(40);
    harness.tick = 100;
    harness.params.insert("max_pos".to_string(), "900".to_string());
    let mut python = spec(&["python3", "escape_then_run.py"]);
    python
        .env
        .insert("PYTHONPATH".to_string(), port_check_dir().join("adapters").display().to_string());
    let req = PortCheckRequest {
        recording: port_check_dir().join("../hft-pack/fixtures/itch50/synthetic-20261018.itch"),
        recording_format: RecordingFormat::Itch50,
        initial_book: None,
        python,
        cpp: spec(&[cpp.to_str().expect("utf-8")]),
        harness,
        diag_tolerance: Default::default(),
        limits: Default::default(),
        python_runs: 2,
        cpp_runs: 2,
        workdir: Some(std::env::temp_dir().join(format!("byteask-portcheck-live-{}", std::process::id()))),
    };
    let tool = PortCheckTool {
        cwd: ws.path().to_path_buf(),
        native_exec: true,
    };
    let settings = crate::sandbox::Settings {
        helper: Some(helper),
        workspace: ws.path().to_path_buf(),
        relaxed: false,
        required: false,
    };
    // The same task-local context the Sandboxed wrapper sets for a real call.
    let text = crate::sandbox::scope_for_test(
        TOOL_NAME,
        settings,
        crate::test_support::call(&tool, serde_json::to_value(&req).expect("encode")),
    )
    .await
    .expect("answers");
    assert!(text.contains("sandbox: on ("), "{text}");
    let result = CheckResult::from_tool_text(TOOL_NAME, &text).expect("a result line");
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(!escaped.exists(), "a sandboxed strategy wrote into the workspace");
}
