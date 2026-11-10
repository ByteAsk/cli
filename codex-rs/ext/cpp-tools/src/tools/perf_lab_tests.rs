use pretty_assertions::assert_eq;

use super::*;

const FASTER: &str = include_str!("testdata/perf_lab_faster.json");
const REFUSED: &str = include_str!("testdata/perf_lab_refused.json");
/// A real A/A run recorded on a bench host (12 rounds).
const AA_RECORDED: &str = include_str!("testdata/perf_lab_aa_recorded.json");

fn args(json: &str) -> LabArgs {
    serde_json::from_str(json).expect("args")
}

#[test]
fn lab_target_forms() {
    assert_eq!(
        LabTarget::parse("local"),
        Some(LabTarget::Local {
            runner: DEFAULT_RUNNER.into()
        })
    );
    assert_eq!(
        LabTarget::parse("local:/x/perf-runner"),
        Some(LabTarget::Local {
            runner: "/x/perf-runner".into()
        })
    );
    assert_eq!(
        LabTarget::parse("ssh:bench-host"),
        Some(LabTarget::Ssh {
            host: "bench-host".into(),
            runner: DEFAULT_RUNNER.into()
        })
    );
    assert_eq!(
        LabTarget::parse("ssh:lab@10.0.0.4:/opt/x"),
        Some(LabTarget::Ssh {
            host: "lab@10.0.0.4".into(),
            runner: "/opt/x".into()
        })
    );
    // Never an ssh option or a shell word as the host.
    assert_eq!(LabTarget::parse("ssh:-oProxyCommand=evil"), None);
    assert_eq!(LabTarget::parse("ssh:a;b"), None);
    assert_eq!(LabTarget::parse("ssh:"), None);
    assert_eq!(LabTarget::parse(""), None);
    assert_eq!(LabTarget::parse("bench-host"), None);
}

#[test]
fn quoting_survives_any_argument() {
    assert_eq!(sq("it's"), r"'it'\''s'");
    assert_eq!(sq("a b;rm -rf /"), "'a b;rm -rf /'");
}

#[test]
fn mirror_names_are_stable_and_distinct() {
    let a = mirror_name(Path::new("/home/u/src/feed-handler"));
    assert_eq!(a, mirror_name(Path::new("/home/u/src/feed-handler")));
    assert!(
        a.starts_with("perflab-repos/feed-handler-") && a.ends_with(".git"),
        "{a}"
    );
    assert_ne!(a, mirror_name(Path::new("/home/v/src/feed-handler")));
    assert!(mirror_name(Path::new("/x/we ird;name")).starts_with("perflab-repos/we_ird_name-"));
}

#[test]
fn runner_arguments() {
    let a = args(
        r#"{"build": "make", "bench": "./b --feed f", "rounds": 16, "confidence": 0.99, "guards": ["allocs_per_msg"], "tma": false, "check": "./replay"}"#,
    );
    let v = runner_args("compare", "perflab-repos/r.git", "aaa", "bbb", &a).unwrap();
    assert_eq!(
        v,
        [
            "compare",
            "--repo",
            "perflab-repos/r.git",
            "--base",
            "aaa",
            "--cand",
            "bbb",
            "--build",
            "make",
            "--bench",
            "./b --feed f",
            "--json",
            "-",
            "--rounds",
            "16",
            "--confidence",
            "0.99",
            "--guard",
            "allocs_per_msg",
            "--check",
            "./replay",
            "--no-tma"
        ]
        .map(String::from)
    );
    let v = runner_args("aa", "r", "aaa", "bbb", &a).unwrap();
    assert_eq!(
        &v[..5],
        ["aa", "--repo", "r", "--ref", "aaa"].map(String::from)
    );
    assert_eq!(
        runner_args("preflight", "", "", "", &args("{}")).unwrap(),
        ["preflight", "--json"].map(String::from)
    );
    let e = runner_args("compare", "r", "a", "b", &args(r#"{"bench": "./b"}"#)).unwrap_err();
    assert!(e.contains("`build` is required"), "{e}");
}

#[test]
fn unknown_arguments_are_refused() {
    assert!(serde_json::from_str::<LabArgs>(r#"{"benchmark": "./b"}"#).is_err());
    assert!(serde_json::from_str::<LabArgs>(r#"{"rounds": 1.5}"#).is_err());
}

#[test]
fn renders_a_faster_report() {
    let j: Value = serde_json::from_str(FASTER).unwrap();
    let out = render(
        "compare",
        "bench-host",
        &j,
        &["candidate = the working tree".into()],
        Some("/tmp/r.json"),
    );
    assert!(
        out.starts_with("Perf lab on bench-host (cpu 2 isolated, 3.09 GHz pinned): FASTER\n"),
        "{out}"
    );
    assert!(out.contains("p50: B is 36.7% better"), "{out}");
    assert!(
        out.contains("p99_ns") && out.contains("throughput"),
        "{out}"
    );
    assert!(out.contains("TMA (hot loop)"), "{out}");
    assert!(out.contains("allocs_per_msg"), "{out}");
    assert!(out.contains("note: candidate = the working tree"), "{out}");
    assert!(out.ends_with("full report: /tmp/r.json\n"), "{out}");
    assert!(!out.contains("do not report a speedup"));
}

#[test]
fn renders_a_refusal_with_its_fix() {
    let j: Value = serde_json::from_str(REFUSED).unwrap();
    let out = render("compare", "bench-host", &j, &[], None);
    assert!(out.contains(": NOT RUN\n"), "{out}");
    assert!(
        out.contains("host check FAIL other_load") && out.contains("(fix: "),
        "{out}"
    );
}

#[test]
fn no_claim_says_not_to_claim() {
    let mut j: Value = serde_json::from_str(FASTER).unwrap();
    j["verdict"] = Value::from("NO_CLAIM");
    let out = render("compare", "bench-host", &j, &[], None);
    assert!(
        out.contains("NO CLAIM: do not report a speedup or a slowdown"),
        "{out}"
    );
}

/// Every answer carries the shared result, and only a measurement
/// that really happened is PASS; inside the noise is never PASS.
#[test]
fn every_runner_verdict_maps_to_the_shared_result() {
    use check_result::Verdict;
    let faster: Value = serde_json::from_str(FASTER).unwrap();
    let r = lab_result("compare", &faster);
    assert_eq!(r.verdict, Verdict::Pass, "{r:?}");
    assert!(r.summary.contains("faster"), "{r:?}");
    let aa: Value = serde_json::from_str(AA_RECORDED).unwrap();
    assert_eq!(aa["verdict"], "NO_CLAIM");
    assert_eq!(lab_result("aa", &aa).verdict, Verdict::NoClaim);
    let refused: Value = serde_json::from_str(REFUSED).unwrap();
    let r = lab_result("compare", &refused);
    assert_eq!(
        (r.verdict, r.reason),
        (Verdict::NotRun, Some(NotRunReason::ServiceUnavailable))
    );
    let mut rejected = faster.clone();
    rejected["verdict"] = Value::from("REJECTED");
    assert_eq!(lab_result("compare", &rejected).verdict, Verdict::Fail);
    let mut invalid = faster.clone();
    invalid["verdict"] = Value::from("INVALID");
    assert_eq!(lab_result("compare", &invalid).verdict, Verdict::NotRun);
    let mut no_build = faster;
    no_build["verdict"] = Value::from("NOT_RUN");
    no_build["b"]["build"]["ok"] = Value::from(false);
    assert_eq!(
        lab_result("compare", &no_build).reason,
        Some(NotRunReason::BuildFailed)
    );
    assert_eq!(
        lab_result("preflight", &serde_json::json!({"ok": false})).verdict,
        Verdict::NotRun
    );
    for r in [lab_result("compare", &refused), lab_result("aa", &aa)] {
        r.validate().expect("contract");
    }
}

#[test]
fn renders_preflight() {
    let j = serde_json::json!({"ok": false, "checks": [
        {"name": "isolated_cpu", "status": "FAIL", "detail": "partition missing", "fix": "sudo perf-runner setup"},
        {"name": "frequency", "status": "PASS", "detail": "3.08 GHz", "fix": null}]});
    let out = render("preflight", "bench-host", &j, &[], None);
    assert!(
        out.contains("REFUSED")
            && out.contains("FAIL isolated_cpu: partition missing (fix: sudo perf-runner setup)")
            && out.contains("1/2 checks PASS"),
        "{out}"
    );
}

/// A new tool gets its own pinned description. Regenerate only with a
/// maintainer-approved diff: `BYTEASK_UPDATE_TOOL_SNAPSHOT=1`.
#[test]
fn description_snapshot_is_byte_identical() {
    const SNAPSHOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/snapshots/cpp_perf_lab.json"
    );
    let now = format!(
        "{}\n",
        serde_json::to_string_pretty(&crate::test_support::sorted_keys(
            serde_json::to_value(spec()).unwrap()
        ))
        .unwrap()
    );
    if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
        std::fs::write(SNAPSHOT, &now).expect("write snapshot");
        return;
    }
    let pinned = std::fs::read_to_string(SNAPSHOT)
        .expect("snapshot missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1");
    assert_eq!(pinned, now, "cpp_perf_lab's model-facing text changed");
}

#[test]
fn registered_only_when_a_lab_is_configured() {
    let cwd = PathBuf::from("/workspace");
    assert!(perf_lab_tool(&cwd, true, None).is_none());
    let t = perf_lab_tool(&cwd, true, LabTarget::parse("ssh:bench-host")).expect("tool");
    assert_eq!(t.tool_name(), ToolName::plain(TOOL_NAME));
}

/// Drive the tool the way the engine does: JSON arguments in, model text out.
async fn call(tool: &PerfLabTool, arguments: serde_json::Value) -> Result<String, String> {
    let payload = codex_extension_api::ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let call = ToolCall {
        turn_id: "turn".to_string(),
        call_id: "call".to_string(),
        tool_name: ToolName::plain(TOOL_NAME),
        model: "test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
        source: codex_extension_api::ToolCallSource::Direct,
        conversation_history: codex_extension_api::ConversationHistory::default(),
        turn_item_emitter: std::sync::Arc::new(codex_extension_api::NoopTurnItemEmitter),
        environments: Vec::new(),
        payload: payload.clone(),
    };
    match tool.handle(call).await {
        Ok(output) => Ok(
            serde_json::to_string(&output.to_response_item("call", &payload)).expect("serialize"),
        ),
        Err(err) => Err(err.to_string()),
    }
}

#[tokio::test]
async fn untrusted_project_is_refused_before_anything_runs() {
    let tool = PerfLabTool {
        cwd: PathBuf::from("/nonexistent"),
        native_exec: false,
        lab: LabTarget::parse("ssh:bench-host").unwrap(),
    };
    let err = call(&tool, serde_json::json!({"build": "make", "bench": "./b"}))
        .await
        .unwrap_err();
    assert!(err.contains("trusted"), "{err}");
}

#[tokio::test]
async fn outside_a_git_repository_is_not_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = PerfLabTool {
        cwd: dir.path().to_path_buf(),
        native_exec: true,
        lab: LabTarget::parse("ssh:bench-host").unwrap(),
    };
    let out = call(&tool, serde_json::json!({"build": "make", "bench": "./b"}))
        .await
        .unwrap();
    assert!(out.contains("NOT RUN: not in a git repository"), "{out}");
}

/// Live: a real perf lab. Run with
/// `BYTEASK_PERF_LAB=ssh:bench-host cargo test -p codex-cpp-tools -- --include-ignored perf_lab::tests::live`.
/// Commits a small benchmark, makes the working tree do 25% less work, and
/// expects FASTER for the uncommitted candidate.
#[tokio::test]
#[ignore]
async fn live_worktree_candidate_reads_faster() {
    let Some(lab) = LabTarget::from_env() else {
        eprintln!("BYTEASK_PERF_LAB not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    // A whole-process benchmark (no PERFLAB line): the verdict is on wall time.
    let src = "#include <cstdio>\n#define WORK 400\nint main() { volatile unsigned long x = 1;\n  \
               for (long i = 0; i < WORK * 1000000L; ++i) x = x * 6364136223846793005ul + 1;\n  \
               std::printf(\"%lu\\n\", (unsigned long)x); }\n";
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.path().join("bench.cpp"), src).unwrap();
    git(&["add", "bench.cpp"]);
    git(&["commit", "-qm", "base"]);
    std::fs::write(
        dir.path().join("bench.cpp"),
        src.replace("WORK 400", "WORK 300"),
    )
    .unwrap();
    let tool = PerfLabTool {
        cwd: dir.path().to_path_buf(),
        native_exec: true,
        lab,
    };
    let out = call(&tool, serde_json::json!({"build": "g++ -O2 -std=c++20 -o bench bench.cpp", "bench": "./bench", "tma": false})).await.unwrap();
    assert!(out.contains("): FASTER"), "{out}");
    assert!(out.contains("candidate = the working tree"), "{out}");
    // The working tree was not touched by the snapshot.
    assert!(
        std::fs::read_to_string(dir.path().join("bench.cpp"))
            .unwrap()
            .contains("WORK 300")
    );
}
