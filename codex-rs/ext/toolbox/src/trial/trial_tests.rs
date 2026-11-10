//! The trial's decisions, each pinned: which target, which program the
//! sanitizer runs, which files are checked, what local-only writes, what the
//! report may contain.

use std::path::Path;
use std::path::PathBuf;

use check_result::CheckResult;
use check_result::NotRunReason;
use check_result::Verdict;
use pretty_assertions::assert_eq;

use super::build::Target;
use super::build::choose_run_target;
use super::build::choose_target;
use super::checks;
use super::local_only::quiet_config;
use super::state;
use super::state::CheckTally;
use super::state::IndexFacts;
use super::state::TrialState;
use super::summary;

fn target(name: &str, kind: &str, deps: &[&str]) -> Target {
    Target {
        name: name.to_string(),
        kind: kind.to_string(),
        artifacts: vec![PathBuf::from(format!("/b/{name}"))],
        deps: deps.iter().map(ToString::to_string).collect(),
    }
}

fn project() -> Vec<Target> {
    vec![
        target("feed", "STATIC_LIBRARY", &[]),
        target("feed_tests", "EXECUTABLE", &["feed"]),
        target("bench", "EXECUTABLE", &["feed"]),
        target("docs", "UTILITY", &[]),
    ]
}

#[test]
fn the_one_test_target_is_the_default() {
    let tests = vec!["feed_tests".to_string()];
    assert_eq!(
        choose_target(&project(), &tests, None),
        Ok("feed_tests".to_string())
    );
}

#[test]
fn several_candidates_and_no_choice_lists_them_and_never_guesses() {
    let (problem, fix) = choose_target(&project(), &[], None).expect_err("ambiguous");
    assert!(problem.contains("3 targets"), "{problem}");
    assert_eq!(fix, "pass --target with one of: feed, feed_tests, bench");
    // A utility target never compiles code, so it is not a choice.
    assert!(choose_target(&project(), &[], Some("docs")).is_err());
    assert!(choose_target(&project(), &[], Some("nope")).is_err());
    assert_eq!(
        choose_target(&project(), &[], Some("feed")),
        Ok("feed".to_string())
    );
}

#[test]
fn the_sanitizer_runs_the_target_or_a_test_that_links_it() {
    let tests = vec!["feed_tests".to_string()];
    assert_eq!(
        choose_run_target(&project(), &tests, "bench", None),
        Ok(Some("bench".to_string()))
    );
    assert_eq!(
        choose_run_target(&project(), &tests, "feed", None),
        Ok(Some("feed_tests".to_string()))
    );
    assert_eq!(choose_run_target(&project(), &[], "feed", None), Ok(None));
    assert!(choose_run_target(&project(), &tests, "feed", Some("feed")).is_err());
    assert_eq!(
        choose_run_target(&project(), &tests, "feed", Some("bench")),
        Ok(Some("bench".to_string()))
    );
}

#[test]
fn the_build_tree_is_in_the_cache_never_in_the_project() {
    let root = Path::new("/home/me/src/feed");
    let env = |key: &str| match key {
        "HOME" => Some(std::ffi::OsString::from("/home/me")),
        _ => None,
    };
    let dir = super::build::build_dir(root, "RelWithDebInfo", env).expect("dir");
    assert!(!dir.starts_with(root), "{}", dir.display());
    if !cfg!(target_os = "macos") {
        assert!(
            dir.starts_with("/home/me/.cache/byteask/trial/"),
            "{}",
            dir.display()
        );
        let env = |key: &str| match key {
            "HOME" => Some(std::ffi::OsString::from("/home/me")),
            "XDG_CACHE_HOME" => Some(std::ffi::OsString::from("/fast/cache")),
            _ => None,
        };
        let dir = super::build::build_dir(root, "Debug", env).expect("dir");
        assert!(dir.starts_with("/fast/cache/byteask/trial/"));
        assert!(dir.ends_with("debug"));
    }
    assert_eq!(super::build::build_dir(root, "Debug", |_| None), None);
}

#[test]
fn checked_files_are_the_targets_own_with_their_project_include_dirs() {
    let root = tempfile::tempdir().expect("root");
    let root = root.path();
    let db = root.join("cc.json");
    let mut entries = vec![
        serde_json::json!({"directory": "/b", "file": root.join("src/a.cpp"),
            "arguments": ["c++", "-I", root.join("include"), format!("-I{}", root.join("src").display()),
                          "-isystem", "/usr/include/x", "-iquote/b/gen", "-c", root.join("src/a.cpp")]}),
        serde_json::json!({"directory": "/b", "file": "/elsewhere/dep.cpp", "command": "c++ -c /elsewhere/dep.cpp"}),
    ];
    for i in 0..12 {
        entries.push(
            serde_json::json!({"directory": root, "file": format!("src/z{i:02}.cpp"),
            "command": format!("c++ -Iinclude -c src/z{i:02}.cpp")}),
        );
    }
    std::fs::write(&db, serde_json::to_string(&entries).expect("json")).expect("write");
    let (units, over) = checks::units(&db, root);
    assert_eq!(units.len(), checks::MAX_FILES);
    assert_eq!(
        over,
        13 - checks::MAX_FILES,
        "the cap is counted, never silent"
    );
    assert_eq!(units[0].path, "src/a.cpp");
    // Project and generated include dirs; system dirs are left to the tools.
    assert_eq!(units[0].include_dirs, vec!["include", "src", "/b/gen"]);
    assert_eq!(units[1].include_dirs, vec!["include"]);
    assert!(
        units.iter().all(|unit| !unit.path.contains("dep.cpp")),
        "files outside the project are not checked"
    );
}

#[test]
fn a_checks_step_is_never_pass_unless_every_run_passed() {
    let mut tally = CheckTally {
        check: "cpp_tidy".to_string(),
        ..CheckTally::default()
    };
    checks::count(&mut tally, &CheckResult::pass("cpp_tidy", "clean"));
    assert_eq!(checks::step_result(&tally).verdict, Verdict::Pass);
    checks::count(
        &mut tally,
        &CheckResult::not_run(
            "cpp_tidy",
            NotRunReason::SandboxUnavailable,
            "no sandbox",
            "install bubblewrap",
        ),
    );
    let step = checks::step_result(&tally);
    assert_eq!(step.verdict, Verdict::NotRun);
    assert_eq!(step.reason, Some(NotRunReason::SandboxUnavailable));
    assert_eq!(step.fix.as_deref(), Some("install bubblewrap"));
    checks::count(&mut tally, &CheckResult::fail("cpp_tidy", "a bug"));
    assert_eq!(checks::step_result(&tally).verdict, Verdict::Fail);
    let empty = CheckTally {
        check: "cpp_check".to_string(),
        ..CheckTally::default()
    };
    assert_eq!(
        checks::step_result(&empty).verdict,
        Verdict::NotRun,
        "nothing ran is not a pass"
    );
}

#[test]
fn local_only_writes_the_quiet_keys_and_keeps_everything_else() {
    let before = "# my settings\nmodel = \"gpt-6-sol\"\nmodel_provider = \"corp\"\n\n[model_providers.corp]\nbase_url = \"https://llm.corp.example/v1\"\n\n[features]\nplugins = true\n";
    let (after, changed) = quiet_config(before).expect("quiet");
    assert!(
        after.starts_with("# my settings\nmodel = \"gpt-6-sol\"\nmodel_provider = \"corp\"\n"),
        "{after}"
    );
    assert!(
        after.contains("base_url = \"https://llm.corp.example/v1\""),
        "the provider is untouched"
    );
    let doc: toml::Table = after.parse().expect("toml");
    assert_eq!(doc["check_for_update_on_startup"].as_bool(), Some(false));
    assert_eq!(doc["web_search"].as_str(), Some("disabled"));
    assert_eq!(doc["otel"]["metrics_exporter"].as_str(), Some("none"));
    assert_eq!(doc["analytics"]["enabled"].as_bool(), Some(false));
    assert_eq!(doc["features"]["plugins"].as_bool(), Some(false));
    assert_eq!(doc["features"]["apps"].as_bool(), Some(false));
    assert_eq!(changed.len(), 8);
    // A second run changes nothing.
    let (again, changed) = quiet_config(&after).expect("quiet");
    assert_eq!(again, after);
    assert!(changed.is_empty());
    assert!(
        quiet_config("otel = 3\n").is_err(),
        "a key that is not a table is reported"
    );
    assert!(quiet_config("[[broken").is_err());
}

#[test]
fn local_only_turns_the_policy_on_and_reads_it_back() {
    let home = tempfile::tempdir().expect("home");
    let result = super::local_only::apply(home.path());
    assert!(result.is_pass(), "{result:?}");
    assert!(byteask_policy::local_only(home.path()).0);
    let config = std::fs::read_to_string(home.path().join("config.toml")).expect("config");
    assert!(
        config.contains("check_for_update_on_startup = false"),
        "{config}"
    );
}

fn driver(name: &str) -> serde_json::Value {
    let text = match name {
        "ok" => include_str!("testdata/driver_ok.json"),
        "no_target" => include_str!("testdata/driver_no_target.json"),
        other => panic!("{other}"),
    };
    serde_json::from_str(text).expect("json")
}

/// The driver's real answer (rigtorp/SPSCQueue, SPSCQueueTest, GCC 12).
#[test]
fn the_index_answer_becomes_counts() {
    let (result, facts) = state::index_result(&driver("ok"));
    assert!(result.is_pass(), "{result:?}");
    let facts = facts.expect("facts");
    assert_eq!(facts.translation_units, 1);
    assert_eq!(facts.lines, Some(466));
    assert_eq!(facts.files, Some(2));
    assert_eq!(facts.compiler, "gcc 12.2.0");
    assert_eq!(facts.committed, "indexed");
    assert_eq!(facts.overlay, "clean (no uncommitted changes)");
    assert_eq!(facts.template_versions, Some(49));
    let (result, facts) = state::index_result(&driver("no_target"));
    assert_eq!(result.reason, Some(NotRunReason::BadArguments));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("SPSCQueueTest")
    );
    assert!(facts.is_none());
}

#[test]
fn some_approximate_lines_make_the_index_step_approximate() {
    let mut answer = driver("ok");
    answer["status"]["settings"][0]["approximate_line_share"] = serde_json::json!(0.024);
    let (result, _) = state::index_result(&answer);
    assert_eq!(result.verdict, Verdict::Approximate);
    assert!(result.summary.contains("2.4%"), "{}", result.summary);
}

/// The report is shareable: a marker planted in every name and every piece of
/// tool text the state holds appears in neither the text nor the JSON.
#[test]
fn the_report_carries_counts_only() {
    const MARK: &str = "SECRETNAME";
    let step = |name: &str, result: CheckResult| state::Step {
        name: name.to_string(),
        result,
        seconds: 1.0,
    };
    let trial = TrialState {
        schema: state::SCHEMA,
        project_root: PathBuf::from(format!("/home/me/src/{MARK}")),
        target: format!("{MARK}_tests"),
        build_type: "RelWithDebInfo".to_string(),
        steps: vec![
            step(
                "build",
                CheckResult::pass("build", &format!("built {MARK}_tests")),
            ),
            step(
                "cpp_tidy",
                CheckResult::not_run(
                    "cpp_tidy",
                    NotRunReason::SandboxUnavailable,
                    &format!("no sandbox for src/{MARK}.cpp"),
                    &format!("see /home/me/.cache/{MARK}/log"),
                ),
            ),
        ],
        index: Some(IndexFacts {
            translation_units: 3,
            targets: 2,
            lines: Some(1200),
            files: Some(9),
            compiler: "gcc 12.2.0".to_string(),
            committed: "indexed".to_string(),
            overlay: "clean (no uncommitted changes)".to_string(),
            ..IndexFacts::default()
        }),
        checks: vec![CheckTally {
            check: "cpp_tidy".to_string(),
            runs: 2,
            pass: 1,
            not_run: 1,
            not_run_reasons: vec![("sandbox unavailable".to_string(), format!("fix {MARK}"))],
            ..CheckTally::default()
        }],
        updated_at: 1_760_000_000,
    };
    let loaded = results_store::store::load(Path::new("/nonexistent-store"));
    let sessions = results_store::ValueReport::build(&loaded, 1_760_000_100, None);
    let (text, json) = summary::render(
        Some(&trial),
        &sessions,
        "Nothing recorded on this machine yet.",
        1_760_000_100,
    );
    assert!(!text.contains(MARK), "{text}");
    assert!(!json.to_string().contains(MARK), "{json}");
    // The numbers are there, and every section says it is local.
    assert!(text.contains("translation units indexed    3"), "{text}");
    assert!(
        text.contains("first-party lines indexed    1200 in 9 files"),
        "{text}"
    );
    assert!(
        text.contains("cpp_tidy      2 files: 1 passed, 1 not run (not run: sandbox unavailable)"),
        "{text}"
    );
    assert!(text.contains("never counted as\nproblems caught"), "{text}");
    for heading in [
        "Trial run of",
        "Index of one target",
        "Checks on the code as it was",
        "AI sessions on this machine",
    ] {
        let line = text
            .lines()
            .find(|line| line.starts_with(heading))
            .expect(heading);
        assert!(
            line.contains("local") || line.contains("this machine"),
            "{line}"
        );
    }
    assert_eq!(json["source"], serde_json::json!("local"));
    assert_eq!(json["index"]["lines"], serde_json::json!(1200));
}

#[test]
fn no_count_yet_is_never_zero() {
    let trial = TrialState {
        schema: state::SCHEMA,
        project_root: PathBuf::from("/p"),
        target: "t".to_string(),
        build_type: "Debug".to_string(),
        steps: Vec::new(),
        index: Some(IndexFacts::default()),
        checks: Vec::new(),
        updated_at: 1,
    };
    let loaded = results_store::store::load(Path::new("/nonexistent-store"));
    let sessions = results_store::ValueReport::build(&loaded, 2, None);
    let (text, _) = summary::render(Some(&trial), &sessions, "", 2);
    assert!(text.contains("no count yet (no completed index)"), "{text}");
}

/// The File API answers of a real CMake configure (skipped where CMake is
/// not installed).
#[test]
fn real_cmake_targets_are_read_from_the_file_api() {
    let Some(cmake) = super::which("cmake", None) else {
        eprintln!("cmake not installed; skipped");
        return;
    };
    let src = tempfile::tempdir().expect("src");
    std::fs::write(
        src.path().join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(feed CXX)\nadd_library(feed STATIC feed.cpp)\n\
         add_executable(feed_tests t.cpp)\ntarget_link_libraries(feed_tests feed)\nenable_testing()\n\
         add_test(NAME feed_tests COMMAND feed_tests)\nadd_custom_target(docs)\n",
    )
    .expect("cmake");
    std::fs::write(src.path().join("feed.cpp"), "int feed() { return 1; }\n").expect("src");
    std::fs::write(
        src.path().join("t.cpp"),
        "int feed(); int main() { return feed() - 1; }\n",
    )
    .expect("src");
    let build = tempfile::tempdir().expect("build");
    let query = build.path().join(".cmake/api/v1/query");
    std::fs::create_dir_all(&query).expect("query");
    std::fs::write(query.join("codemodel-v2"), "").expect("query");
    let status = std::process::Command::new(cmake)
        .arg("-S")
        .arg(src.path())
        .arg("-B")
        .arg(build.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("cmake runs");
    assert!(status.success());
    let targets = super::build::read_targets(build.path()).expect("targets");
    let names: Vec<(&str, &str)> = targets
        .iter()
        .map(|t| (t.name.as_str(), t.kind.as_str()))
        .collect();
    assert!(names.contains(&("feed", "STATIC_LIBRARY")), "{names:?}");
    assert!(names.contains(&("feed_tests", "EXECUTABLE")), "{names:?}");
    let tests_target = targets
        .iter()
        .find(|t| t.name == "feed_tests")
        .expect("feed_tests");
    assert_eq!(tests_target.deps, vec!["feed".to_string()]);
    assert_eq!(
        choose_run_target(&targets, &["feed_tests".to_string()], "feed", None),
        Ok(Some("feed_tests".to_string()))
    );
}

/// CTest gives no command for a test whose program is not built yet; the test's
/// name still names its target (found on tinyxml2: xmltest links the library).
#[test]
fn tests_are_found_by_command_or_by_name() {
    let targets = vec![
        target("tinyxml2", "STATIC_LIBRARY", &[]),
        target("xmltest", "EXECUTABLE", &["tinyxml2"]),
        target("tool", "EXECUTABLE", &[]),
    ];
    let unbuilt = serde_json::json!({"tests": [{"name": "xmltest", "command": null}]});
    assert_eq!(super::build::tests_in(&unbuilt, &targets), vec!["xmltest".to_string()]);
    let built = serde_json::json!({"tests": [{"name": "runs-the-tool", "command": ["/b/tool", "--self-test"]}]});
    assert_eq!(super::build::tests_in(&built, &targets), vec!["tool".to_string()]);
    let other = serde_json::json!({"tests": [{"name": "lint", "command": ["/usr/bin/python3", "lint.py"]}]});
    assert!(super::build::tests_in(&other, &targets).is_empty());
}
