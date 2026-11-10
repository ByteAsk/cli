use std::path::Path;

use pretty_assertions::assert_eq;

use super::*;
use crate::impact::tests::git;

fn have(tool: &str) -> bool {
    crate::tools::binary_on_path(tool)
}

fn settings() -> GateSettings {
    GateSettings {
        budget: Duration::from_secs(300),
        ..GateSettings::default()
    }
}

/// A CMake project with a library function and a ctest test of it.
fn cmake_fixture() -> Option<tempfile::TempDir> {
    if !have("cmake") || !(have("c++") || have("g++") || have("clang++")) || !have("git") {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).ok()?;
    std::fs::create_dir_all(root.join("test")).ok()?;
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.5)\nproject(calc CXX)\nenable_testing()\n\
         ADD_LIBRARY(calc src/calc.cpp)\ntarget_include_directories(calc PUBLIC src)\n\
         add_executable(calc_test test/calc_test.cpp)\ntarget_link_libraries(calc_test calc)\n\
         ADD_TEST(NAME adds COMMAND calc_test add)\nADD_TEST(NAME subs COMMAND calc_test sub)\n",
    )
    .ok()?;
    std::fs::write(
        root.join("src/calc.h"),
        "int add(int a, int b);\nint sub(int a, int b);\n",
    )
    .ok()?;
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .ok()?;
    std::fs::write(
        root.join("test/calc_test.cpp"),
        "#include \"calc.h\"\n#include <cstring>\nint main(int argc, char** argv) {\n  if (argc > 1 && std::strcmp(argv[1], \"add\") == 0) return add(2, 3) == 5 ? 0 : 1;\n  return sub(5, 3) == 2 ? 0 : 1;\n}\n",
    )
    .ok()?;
    std::fs::write(root.join(".gitignore"), "").ok()?;
    if !git(root, &["init", "-q"])
        || !git(root, &["add", "-A"])
        || !git(root, &["commit", "-q", "-m", "base"])
    {
        return None;
    }
    Some(dir)
}

async fn gate_on(
    root: &Path,
    snapshot: &Snapshot,
    base: &mut BaseCache,
    non_interactive: bool,
) -> GateOutcome {
    let _ = root;
    let mut memo = None;
    let mut advisories = Advisories::default();
    gate_with(
        snapshot,
        base,
        &mut memo,
        &mut advisories,
        None,
        &settings(),
        non_interactive,
    )
    .await
}

/// One gate run with explicit session state (memo, advisories, crash context).
async fn gate_with(
    snapshot: &Snapshot,
    base: &mut BaseCache,
    memo: &mut Option<BuildMemo>,
    advisories: &mut Advisories,
    crash: Option<&CrashContext>,
    settings: &GateSettings,
    non_interactive: bool,
) -> GateOutcome {
    let changes = agent_changes(snapshot).await;
    run_gate(GateInput {
        snapshot,
        changes: &changes,
        settings,
        native_exec: true,
        non_interactive,
        impact_state: None,
        base,
        assistant_text: "",
        memo,
        advisories,
        crash,
        session_left: Duration::from_secs(3_000),
        advisory: false,
    })
    .await
}

/// A committed git repository with the given files (no build system unless one is written).
fn plain_repo(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
    if !have("git") {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    for (path, text) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(path, text).ok()?;
    }
    let root = dir.path();
    (git(root, &["init", "-q"]) && git(root, &["add", "-A"]) && git(root, &["commit", "-q", "-m", "b"]))
        .then_some(dir)
}

fn kinds(outcome: &GateOutcome) -> Vec<ProblemKind> {
    outcome.problems.iter().map(|p| p.kind.clone()).collect()
}

#[tokio::test]
async fn cmake_regression_is_caught_and_compared_with_base() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let mut base = BaseCache::default();

    // A correct change: the gate passes and reports what it checked.
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\n// sum\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert!(outcome.problems.is_empty(), "{outcome:#?}");
    assert!(
        outcome.notes.iter().any(|n| n.starts_with("build: ok")),
        "{outcome:#?}"
    );
    assert!(
        outcome.notes.iter().any(|n| n.contains("2 passed")),
        "{outcome:#?}"
    );

    // A regression: `adds` passes on the unmodified tree and fails now.
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b + 1; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::Tests], "{outcome:#?}");
    assert!(outcome.problems[0].text.contains("adds"), "{outcome:#?}");
    assert!(!outcome.problems[0].text.contains("subs"), "{outcome:#?}");
    assert!(
        base.tests.get("adds") == Some(&TestStatus::Passed),
        "{base:#?}"
    );

    // A compile error is a build problem (new vs base); the test no longer builds.
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + ; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::Build], "{outcome:#?}");
    assert!(
        outcome.problems[0].text.contains("src/calc.cpp"),
        "{outcome:#?}"
    );

    // Hygiene: an edited existing test and a leftover file.
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let test = std::fs::read_to_string(root.join("test/calc_test.cpp")).unwrap();
    std::fs::write(
        root.join("test/calc_test.cpp"),
        format!("{test}// touched\n"),
    )
    .unwrap();
    std::fs::write(root.join("repro.cpp"), "int main() {}\n").unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    let found = kinds(&outcome);
    assert!(found.contains(&ProblemKind::TestEdits), "{outcome:#?}");
    assert!(found.contains(&ProblemKind::Leftovers), "{outcome:#?}");
    assert!(
        outcome
            .problems
            .iter()
            .any(|p| p.text.contains("repro.cpp")),
        "{outcome:#?}"
    );
    // The repository itself was not touched by the gate: only the files written here differ.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout).to_string();
    assert_eq!(status.lines().count(), 2, "{status}");
    let text = render_feedback(&outcome, 1, 2);
    assert!(text.starts_with("ByteAsk finish gate"), "{text}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// `gate_targeted`: on a project whose tests sit behind an option that is OFF by default, the
/// whole-suite gate has no test to run; the targeted gate turns the option on in the tools' own
/// tree, runs the test the change reaches and catches the regression.
#[tokio::test]
async fn the_targeted_gate_runs_tests_behind_an_off_option() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.14)\nproject(calc CXX)\noption(CALC_TESTS \"tests\" OFF)\n\
         ADD_LIBRARY(calc src/calc.cpp)\ntarget_include_directories(calc PUBLIC src)\n\
         if(CALC_TESTS)\n enable_testing()\n add_executable(calc_test test/calc_test.cpp)\n\
         target_link_libraries(calc_test calc)\n ADD_TEST(NAME adds COMMAND calc_test add)\n\
         ADD_TEST(NAME subs COMMAND calc_test sub)\nendif()\n",
    )
    .unwrap();
    assert!(git(root, &["commit", "-q", "-a", "--amend", "-m", "base"]));
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(root));
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b + 1; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let run = |targeted: bool| {
        let snapshot = snapshot.clone();
        async move {
            let mut base = BaseCache::default();
            let mut memo = None;
            let mut advisories = Advisories::default();
            let settings = GateSettings {
                targeted,
                ..settings()
            };
            gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings, true).await
        }
    };
    let whole = run(false).await;
    assert!(!kinds(&whole).contains(&ProblemKind::Tests), "{whole:#?}");
    let targeted = run(true).await;
    assert!(kinds(&targeted).contains(&ProblemKind::Tests), "{targeted:#?}");
    assert!(
        targeted.problems.iter().any(|p| p.text.contains("adds")),
        "{targeted:#?}"
    );
    assert!(
        targeted
            .notes
            .iter()
            .any(|n| n.contains("with the project's test options on (CALC_TESTS)")),
        "{targeted:#?}"
    );
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(root));
}

#[tokio::test]
async fn an_empty_change_is_flagged_only_non_interactively() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let snapshot = take_snapshot(dir.path()).await.expect("snapshot");
    let mut base = BaseCache::default();
    let outcome = gate_on(dir.path(), &snapshot, &mut base, true).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::EmptyChange]);
    let outcome = gate_on(dir.path(), &snapshot, &mut base, false).await;
    assert!(outcome.problems.is_empty());
}

#[tokio::test]
async fn make_project_is_built_in_place_and_nothing_is_deleted() {
    if !have("make") || !have("git") || !(have("cc") || have("gcc")) {
        eprintln!("skipped: make/cc/git not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Makefile"),
        "all: tool\ntool: tool.c\n\tcc -o tool tool.c\ncheck: tool\n\t@./tool one && echo 'PASS: one' || echo 'FAIL: one'\n\t@./tool two && echo 'PASS: two' || echo 'FAIL: two'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tool.c"),
        "#include <string.h>\nint main(int c, char** v) { return strcmp(v[1], \"one\") == 0 ? 0 : 0; }\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    assert!(
        git(root, &["init", "-q"])
            && git(root, &["add", "-A"])
            && git(root, &["commit", "-q", "-m", "b"])
    );
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let mut base = BaseCache::default();
    std::fs::write(
        root.join("tool.c"),
        "#include <string.h>\nint main(int c, char** v) { return strcmp(v[1], \"one\") == 0 ? 1 : 0; }\n",
    )
    .unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::Tests], "{outcome:#?}");
    assert!(outcome.problems[0].text.contains("one"), "{outcome:#?}");
    assert!(!outcome.problems[0].text.contains("two"), "{outcome:#?}");
    // 0.2.0: the gate deletes nothing. The `tool` binary its in-place build created stays,
    // is named in a note, and is not reported as a file the agent left behind.
    assert!(root.join("tool").exists());
    assert!(root.join("tool.c").exists());
    assert!(
        outcome.notes.iter().any(|n| n.contains("left in place: tool")),
        "{outcome:#?}"
    );
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

#[test]
fn stray_paths_exclude_files_the_change_uses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.cc"), "#include \"new_helper.h\"\n").unwrap();
    let changes = AgentChanges {
        tracked: vec![('M', "a.cc".to_string())],
        new_untracked: vec![
            "new_helper.h".to_string(),
            "scratch.cpp".to_string(),
            "out/".to_string(),
        ],
        fingerprint: 0,
        content_key: String::new(),
    };
    assert_eq!(
        stray_paths(dir.path(), &changes, &BTreeSet::new()),
        vec!["scratch.cpp".to_string(), "out/".to_string()]
    );
}

/// Build outputs and what the gate's own in-place build created are not stray files (object
/// directories its own build re-created must not be flagged); scratch sources are.
#[test]
fn build_outputs_and_gate_created_paths_are_not_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("programs/obj/conf_1")).unwrap();
    std::fs::write(root.join("programs/obj/conf_1/main.o"), b"\x7fELF....").unwrap();
    std::fs::write(root.join("programs/obj/conf_1/main.d"), "main.o: main.c\n").unwrap();
    std::fs::create_dir_all(root.join("asan-build")).unwrap();
    std::fs::write(root.join("asan-build/CMakeCache.txt"), "").unwrap();
    std::fs::write(root.join("probe"), b"\x7fELF\x02\x01\x01").unwrap();
    std::fs::create_dir_all(root.join("scratch")).unwrap();
    std::fs::write(root.join("scratch/repro.cpp"), "int main() {}\n").unwrap();
    std::fs::write(root.join("scratch/repro.o"), b"\x7fELF").unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    let changes = AgentChanges {
        tracked: Vec::new(),
        new_untracked: vec![
            "programs/obj/".to_string(),
            "asan-build/".to_string(),
            "probe".to_string(),
            "scratch/".to_string(),
            "lib/obj/".to_string(),
            "notes.txt".to_string(),
        ],
        fingerprint: 0,
        content_key: String::new(),
    };
    let created: BTreeSet<String> = ["lib/obj/x.o".to_string()].into_iter().collect();
    assert_eq!(
        stray_paths(root, &changes, &created),
        vec!["scratch/".to_string(), "notes.txt".to_string()]
    );
}

#[test]
fn cases_inside_test_output_are_read() {
    let gtest = "make[1]: Entering directory '/r'\n[==========] Running 3 tests\n[ RUN      ] Lexer.Plain\n[       OK ] Lexer.Plain (0 ms)\n[ RUN      ] Lexer.Triple\nlexer.cc:9: Failure\n[  FAILED  ] Lexer.Triple (1 ms)\n[ RUN      ] Parser.Ok\n[       OK ] Parser.Ok (0 ms)\n[  FAILED  ] 1 test, listed below:\n[  FAILED  ] Lexer.Triple\n";
    let cases = build::parse_cases("make test", gtest);
    assert_eq!(cases.get("make test::Lexer.Plain"), Some(&TestStatus::Passed));
    assert_eq!(cases.get("make test::Lexer.Triple"), Some(&TestStatus::Failed));
    assert_eq!(cases.get("make test::Parser.Ok"), Some(&TestStatus::Passed));
    assert_eq!(cases.len(), 3, "{cases:?}");

    let tap = "1..3\nok 1 - parses empty\nnot ok 2 - parses nested # TODO later\nnot ok 3 - rejects junk\nok 4 # SKIP no network\n";
    let cases = build::parse_cases("t", tap);
    assert_eq!(cases.get("t::parses empty"), Some(&TestStatus::Passed));
    assert_eq!(cases.get("t::parses nested"), Some(&TestStatus::Passed));
    assert_eq!(cases.get("t::rejects junk"), Some(&TestStatus::Failed));
    assert_eq!(cases.get("t::4"), Some(&TestStatus::Skipped));
    assert!(build::parse_cases("t", "ok so far\n").is_empty(), "no plan line, no TAP");

    let pytest = "tests/test_a.py::test_one PASSED\n=========== short test summary info ===========\nFAILED tests/test_b.py::test_two - AssertionError: x\n===== 1 failed, 41 passed in 3.20s =====\n";
    let cases = build::parse_cases("cmake check", pytest);
    assert_eq!(cases.get("cmake check::tests/test_a.py::test_one"), Some(&TestStatus::Passed));
    assert_eq!(cases.get("cmake check::tests/test_b.py::test_two"), Some(&TestStatus::Failed));
    assert_eq!(cases.get("cmake check::*"), Some(&TestStatus::Passed), "completed: unlisted cases passed");

    let rule = "-".repeat(79);
    let catch = format!("{rule}\nparses a number\n  with a sign\n{rule}\nt.cpp:3\n{dots}\n\nt.cpp:5: FAILED:\n  REQUIRE( f() == 2 )\nwith expansion:\n  1 == 2\n\n{eq}\ntest cases: 2 | 1 passed | 1 failed\n", dots = ".".repeat(79), eq = "=".repeat(79));
    let cases = build::parse_cases("ctest_entry", &catch);
    assert_eq!(cases.get("ctest_entry::parses a number"), Some(&TestStatus::Failed), "{cases:?}");
    assert_eq!(cases.get("ctest_entry::*"), Some(&TestStatus::Passed));

    let ctest = " 1/2 Test  #1: unit .............................***Failed    0.02 sec\n[ RUN      ] A.b\n[  FAILED  ] A.b (0 ms)\n[ RUN      ] A.c\n[       OK ] A.c (0 ms)\n      Start  2: other\n 2/2 Test  #2: other ............................   Passed    0.01 sec\n";
    let cases = build::ctest_cases(ctest);
    assert_eq!(cases.get("unit::A.b"), Some(&TestStatus::Failed));
    assert_eq!(cases.get("unit::A.c"), Some(&TestStatus::Passed));
    assert_eq!(cases.len(), 2, "{cases:?}");
}

/// `make test` already fails on the unmodified tree (one known-bad case). Comparing only the
/// suite's exit status would call the change's new failing cases "not caused by the change",
/// so the gate compares the cases inside the suite's output.
#[tokio::test]
async fn a_new_failing_case_inside_an_already_failing_suite_is_caught() {
    if !have("make") || !have("git") || !(have("cc") || have("gcc")) {
        eprintln!("skipped: make/cc/git not available");
        return;
    }
    let suite = "#include <stdio.h>\nint one(void);\nstatic int report(const char* n, int ok) { printf(\"[ RUN      ] Lexer.%s\\n\", n); printf(ok ? \"[       OK ] Lexer.%s (0 ms)\\n\" : \"[  FAILED  ] Lexer.%s (0 ms)\\n\", n); return ok; }\nint main(void) { int a = report(\"one\", one() == 1); int b = report(\"two\", 0); return a && b ? 0 : 1; }\n";
    let Some(dir) = plain_repo(&[
        ("Makefile", "all: suite\nsuite: suite.c lib.c\n\tcc -o suite suite.c lib.c\ntest: suite\n\t./suite\n"),
        ("suite.c", suite),
        ("lib.c", "int one(void) { return 1; }\n"),
        (".gitignore", ""),
    ]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let mut base = BaseCache::default();
    std::fs::write(root.join("lib.c"), "int one(void) { return 2; }\n").unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::Tests], "{outcome:#?}");
    let text = &outcome.problems[0].text;
    assert!(text.contains("TEST CASES THAT PASS ON THE UNMODIFIED TREE NOW FAIL"), "{text}");
    assert!(text.contains("Lexer.one"), "{text}");
    assert!(!text.contains("Lexer.two"), "pre-existing failure reported: {text}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// One full build + suite run per change set: stopping again with the same change reuses the
/// result (no rebuild, no re-run); a further edit is built and tested again.
#[tokio::test]
async fn an_unchanged_change_set_is_not_built_or_tested_twice() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let (mut base, mut memo, mut advisories) = (BaseCache::default(), None, Advisories::default());
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b + 1; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let first = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert_eq!(kinds(&first), vec![ProblemKind::Tests], "{first:#?}");
    let tree = snapshot.scratch.join("cur-build");
    assert!(tree.is_dir());
    std::fs::remove_dir_all(&tree).unwrap();
    // Same change set (the model only answered): the same result, from the memo.
    let second = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert_eq!(kinds(&second), kinds(&first), "{second:#?}");
    assert_eq!(second.problems[0].text, first.problems[0].text);
    assert!(!tree.exists(), "the gate rebuilt an unchanged change set");
    // Once delivered, the same findings are not repeated, but they are still open (the run log
    // records them for the sub-agents' evidence).
    if let Some(m) = memo.as_mut() {
        m.delivered = true;
    }
    let repeat = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert!(repeat.problems.is_empty(), "{repeat:#?}");
    assert_eq!(repeat.still_open.len(), 1, "{repeat:#?}");
    assert_eq!(repeat.still_open[0].kind, ProblemKind::Tests);
    // A new change set is built and tested again.
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\n// sum\nint add(int a, int b) { return a + b; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let third = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert!(third.problems.is_empty(), "{third:#?}");
    assert!(tree.is_dir());
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// When the gate cannot build or test, it never blocks finishing: the "could not check" note is
/// advisory (no feedback round by itself), names a bounded check instead of the whole suite,
/// and once delivered it is not repeated.
#[tokio::test]
async fn what_the_gate_cannot_check_never_blocks_finishing() {
    let Some(dir) = plain_repo(&[("src/tool.c", "int answer(void) { return 1; }\n")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let (mut base, mut memo, mut advisories) = (BaseCache::default(), None, Advisories::default());
    std::fs::write(root.join("src/tool.c"), "int answer(void) { return 2; }\n").unwrap();
    let first = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert_eq!(kinds(&first), vec![ProblemKind::SuiteNotChecked], "{first:#?}");
    assert!(!needs_feedback(&first), "advisory only: {first:#?}");
    let text = &first.problems[0].text;
    assert!(text.starts_with("COULD NOT CHECK (this does not block finishing)"), "{text}");
    assert!(text.contains("no configured build"), "{text}");
    assert!(text.contains("cpp_test_select") && text.contains("timeout 10m"), "{text}");
    assert!(!text.contains("yourself once now"), "{text}");
    // Delivered once (with other feedback): not repeated.
    advisories.not_checked_shown = true;
    let second = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert!(second.problems.is_empty(), "{second:#?}");
    // Little session time left: no suggestion to run anything.
    let mut fresh = Advisories::default();
    let changes = agent_changes(&snapshot).await;
    let late = run_gate(GateInput {
        snapshot: &snapshot,
        changes: &changes,
        settings: &settings(),
        native_exec: true,
        non_interactive: true,
        impact_state: None,
        base: &mut base,
        assistant_text: "",
        memo: &mut memo,
        advisories: &mut fresh,
        crash: None,
        session_left: Duration::from_secs(200),
        advisory: false,
    })
    .await;
    assert!(late.problems[0].text.contains("Too little session time"), "{late:#?}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// A failing test that cannot be compared with the unmodified tree (here: it hits its time
/// limit, so it is neither re-run nor compared) is not a problem: reporting
/// it as a failing test ("the unmodified tree could not be tested…") would be a false alarm.
#[tokio::test]
async fn a_failure_that_cannot_be_compared_is_only_noted() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let lists = std::fs::read_to_string(root.join("CMakeLists.txt")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        format!("{lists}ADD_TEST(NAME slow COMMAND sleep 5)\nset_tests_properties(slow PROPERTIES TIMEOUT 1)\n"),
    )
    .unwrap();
    assert!(git(root, &["commit", "-qam", "slow test"]));
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let mut base = BaseCache::default();
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\n// sum\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert!(!needs_feedback(&outcome), "{outcome:#?}");
    assert_eq!(kinds(&outcome), vec![ProblemKind::SuiteNotChecked], "{outcome:#?}");
    assert!(outcome.problems[0].text.contains("time limit"), "{outcome:#?}");
    assert!(outcome.problems[0].text.contains("slow"), "{outcome:#?}");
    assert!(!snapshot.scratch.join("base").exists(), "a timed-out test was compared with a base build");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

#[test]
fn the_projects_own_tree_is_preferred() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let own = root.join("build");
    let asan = root.join("asan");
    let fresh = root.join("fresh");
    for (tree, flags, tests) in [(&own, "-O2", true), (&asan, "-fsanitize=address", false), (&fresh, "", true)] {
        std::fs::create_dir_all(tree).unwrap();
        std::fs::write(tree.join("CMakeCache.txt"), format!("CMAKE_CXX_FLAGS:STRING={flags}\n")).unwrap();
        if tests {
            std::fs::write(tree.join("CTestTestfile.cmake"), "").unwrap();
        }
    }
    // Most recently used first, as cmake_build_dirs returns them.
    let ranked = build::rank_cmake_trees(vec![asan.clone(), fresh.clone(), own.clone()], &[own.clone()]);
    assert_eq!(ranked, vec![own.clone(), fresh.clone(), asan.clone()]);
    let none_at_start = build::rank_cmake_trees(vec![asan.clone(), own.clone()], &[]);
    assert_eq!(none_at_start, vec![own.clone(), asan.clone()], "tests registered beat recency");
}

#[test]
fn meson_out_of_tree_make_and_custom_targets_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("meson.build"), "project('p')\n").unwrap();
    std::fs::write(root.join("CMakeLists.txt"), "project(p)\n").unwrap();
    std::fs::create_dir_all(root.join("build/meson-private")).unwrap();
    // An existing Meson tree wins over configuring a scratch CMake tree.
    let plan = build::plan(root, &root.join("scratch-gate"), &[]).expect("plan");
    assert_eq!(plan.kind, BuildKind::Meson { dir: root.join("build") });

    let auto = tempfile::tempdir().unwrap();
    let a = auto.path();
    std::fs::write(a.join("configure.ac"), "AC_INIT\n").unwrap();
    std::fs::write(a.join("Makefile.am"), "").unwrap();
    std::fs::create_dir_all(a.join("_obj")).unwrap();
    std::fs::write(a.join("_obj/config.status"), "").unwrap();
    std::fs::write(a.join("_obj/Makefile"), "all:\ncheck: all\n\t./run\n").unwrap();
    let plan = build::plan(a, &a.join("scratch-gate"), &[]).expect("out-of-tree build found");
    assert_eq!(plan.kind, BuildKind::Make { autotools: true, dir: a.join("_obj") });

    let ninja = tempfile::tempdir().unwrap();
    std::fs::write(
        ninja.path().join("build.ninja"),
        "build all: phony lib\nbuild check: phony CMakeFiles/check\nbuild lib.a: CXX_STATIC_LIBRARY_LINKER a.o\n",
    )
    .unwrap();
    assert_eq!(build::cmake_custom_check_target(ninja.path()).as_deref(), Some("check"));
    let make = tempfile::tempdir().unwrap();
    std::fs::write(make.path().join("Makefile"), "all: cmake_check_build_system\ntests: all\n\t$(MAKE) -f x\n").unwrap();
    assert_eq!(build::cmake_custom_check_target(make.path()).as_deref(), Some("tests"));
    let bare = tempfile::tempdir().unwrap();
    std::fs::write(bare.path().join("build.ninja"), "build all: phony lib\n").unwrap();
    assert_eq!(build::cmake_custom_check_target(bare.path()), None);
}

/// In a crash-fix session the advisory patch-shape critic runs on the agent's diff; each
/// finding is shown once, not repeated after cpp_fix_verify showed it, and never outside a
/// crash-fix session.
#[tokio::test]
async fn a_crash_fix_gets_an_advisory_shape_review_once() {
    let source = "#include <stdlib.h>\nvoid drop(char *name, char *it)\n{\n  free (name);\n  free (it);\n}\n";
    let Some(dir) = plain_repo(&[("src/item.c", source)]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(
        root.join("src/item.c"),
        "#include <stdlib.h>\nvoid drop(char *name, char *it)\n{\n  free (it);\n}\n",
    )
    .unwrap();
    let quiet = GateSettings {
        tests: false,
        ..settings()
    };
    let crash = CrashContext {
        functions: vec!["drop".to_string(), "main".to_string()],
        shown_elsewhere: String::new(),
    };
    let (mut base, mut memo) = (BaseCache::default(), None);

    // Not a crash-fix session: no critic.
    let mut advisories = Advisories::default();
    let outcome = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &quiet, true).await;
    assert!(outcome.problems.is_empty(), "{outcome:#?}");

    // A crash-fix session: advisory findings, then not again.
    let first = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, Some(&crash), &quiet, true).await;
    assert_eq!(kinds(&first), vec![ProblemKind::ShapeAdvisory], "{first:#?}");
    assert!(first.problems[0].text.contains("[deleted-free] src/item.c:"), "{first:#?}");
    let second = gate_with(&snapshot, &mut base, &mut memo, &mut advisories, Some(&crash), &quiet, true).await;
    assert!(second.problems.is_empty(), "{second:#?}");

    // Already shown by a cpp_fix_verify run: not repeated.
    let mut fresh = Advisories::default();
    let verified = CrashContext {
        shown_elsewhere: "patch-shape review (advisory, static): ...\n- [deleted-free] src/item.c:4 removes".to_string(),
        ..crash.clone()
    };
    let outcome = gate_with(&snapshot, &mut base, &mut memo, &mut fresh, Some(&verified), &quiet, true).await;
    assert!(
        !outcome.problems.iter().any(|p| p.text.contains("deleted-free")),
        "{outcome:#?}"
    );
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// The base comparison shares the persistent base-run cache with cpp_test_select: for a reused
/// build tree, the base result the gate computes is recorded under the same key
/// cpp_test_select uses, and a later session finds it without building the base again.
#[tokio::test]
async fn the_base_comparison_shares_the_base_run_cache() {
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    assert!(git(root, &["commit", "-qam", "ignore the build tree"]));
    let configured = std::process::Command::new("cmake")
        .args(["-S", ".", "-B", "build"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(configured);
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let config = crate::testsel::cmake::tree_config_key(&root.join("build")).expect("cache");
    let key = crate::base_run_cache::BaseRunKey::new(root, &snapshot.head, &config);
    assert_eq!(crate::base_run_cache::load(&key), None);
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return a + b + 1; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();

    // First session: the base is built and tested, and the result is recorded.
    let mut base = BaseCache::default();
    let first = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&first), vec![ProblemKind::Tests], "{first:#?}");
    assert!(first.problems[0].text.contains("adds"), "{first:#?}");
    let recorded = crate::base_run_cache::load(&key).expect("recorded");
    assert_eq!(
        recorded.results.get("adds"),
        Some(&crate::base_run_cache::TestStatus::Pass)
    );

    // A later session (fresh in-memory state, no base copy): the cached result is used.
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let mut base = BaseCache::default();
    let second = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(kinds(&second), vec![ProblemKind::Tests], "{second:#?}");
    assert!(second.problems[0].text.contains("adds"), "{second:#?}");
    assert!(
        !snapshot.scratch.join("base").exists(),
        "the base was built although the shared cache had the result"
    );
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

// ---------------------------------------------------------------------------------------------
// The strict rules. Before them, the run log said "passed" whenever the gate
// had nothing to send back, including when the build and tests never ran; the change set was
// keyed by path + size for new files, so a same-size edit reused the earlier result.
// ---------------------------------------------------------------------------------------------

fn verdict(outcome: &GateOutcome) -> (check_result::Verdict, Option<check_result::NotRunReason>) {
    let result = outcome.result();
    assert!(result.validate().is_ok(), "{result:?}");
    (result.verdict, result.reason)
}

#[tokio::test]
async fn a_same_size_edit_to_a_new_file_makes_the_old_result_stale() {
    let Some(dir) = plain_repo(&[("src/tool.c", "int answer(void) { return 1; }\n")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(root.join("src/extra.c"), "int extra(void) { return 1; }\n").unwrap();
    let first = agent_changes(&snapshot).await;
    std::fs::write(root.join("src/extra.c"), "int extra(void) { return 2; }\n").unwrap();
    let second = agent_changes(&snapshot).await;
    assert_ne!(first.content_key, second.content_key, "same size, different code");
    assert_ne!(first.fingerprint, second.fingerprint);
    // Nothing changed: the same key (results stay reusable).
    assert_eq!(agent_changes(&snapshot).await.content_key, second.content_key);
    // A new directory is keyed by every file's content too.
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("lib/a.c"), "int a(void) { return 1; }\n").unwrap();
    let with_dir = agent_changes(&snapshot).await;
    std::fs::write(root.join("lib/a.c"), "int a(void) { return 3; }\n").unwrap();
    assert_ne!(agent_changes(&snapshot).await.content_key, with_dir.content_key);
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

#[test]
fn the_build_environment_is_part_of_the_key() {
    let a = content_key("abc", "M src/a.c:1;", "CC=gcc;");
    assert_eq!(a, content_key("abc", "M src/a.c:1;", "CC=gcc;"));
    assert_ne!(a, content_key("abc", "M src/a.c:1;", "CC=clang;"));
    assert_ne!(a, content_key("abd", "M src/a.c:1;", "CC=gcc;"));
    assert_eq!(a.len(), 64);
}

#[tokio::test]
async fn work_the_gate_could_not_check_is_never_pass() {
    use check_result::NotRunReason;
    use check_result::Verdict;
    let Some(dir) = plain_repo(&[("src/tool.c", "int answer(void) { return 1; }\n")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(root.join("src/tool.c"), "int answer(void) { return 2; }\n").unwrap();

    // No build system: the suite could not be checked.
    let (mut base, mut memo, mut advisories) = (BaseCache::default(), None, Advisories::default());
    let outcome =
        gate_with(&snapshot, &mut base, &mut memo, &mut advisories, None, &settings(), true).await;
    assert!(!needs_feedback(&outcome), "NOT RUN never loops: {outcome:#?}");
    assert_eq!(verdict(&outcome), (Verdict::NotRun, Some(NotRunReason::NoBuildProfile)));

    // Switched off, not trusted, out of time: each NOT RUN with its own reason.
    let changes = agent_changes(&snapshot).await;
    let off = GateSettings {
        tests: false,
        ..settings()
    };
    for (settings, native_exec, left, reason) in [
        (off, true, Duration::from_secs(3_000), NotRunReason::Skipped),
        (settings(), false, Duration::from_secs(3_000), NotRunReason::NotTrusted),
        (settings(), true, Duration::from_secs(60), NotRunReason::TimeLimit),
    ] {
        let (mut base, mut memo, mut advisories) =
            (BaseCache::default(), None, Advisories::default());
        let outcome = run_gate(GateInput {
            snapshot: &snapshot,
            changes: &changes,
            settings: &settings,
            native_exec,
            non_interactive: false,
            impact_state: None,
            base: &mut base,
            assistant_text: "",
            memo: &mut memo,
            advisories: &mut advisories,
            crash: None,
            session_left: left,
            advisory: false,
        })
        .await;
        assert_eq!(verdict(&outcome), (Verdict::NotRun, Some(reason)), "{outcome:#?}");
    }
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

#[test]
fn open_problems_are_fail_even_when_already_shown() {
    use check_result::Verdict;
    let problem = Problem {
        kind: ProblemKind::Tests,
        text: "FAILING TESTS: adds\nmore".to_string(),
    };
    let shown_now = GateOutcome {
        suite: SuiteState::Ran,
        problems: vec![problem.clone()],
        ..GateOutcome::default()
    };
    assert_eq!(verdict(&shown_now).0, Verdict::Fail);
    assert!(shown_now.result().summary.contains("FAILING TESTS: adds"));
    // Delivered in an earlier round and not repeated: still open, still FAIL. The old gate
    // let the turn end here and logged "passed".
    let still_open = GateOutcome {
        suite: SuiteState::Ran,
        still_open: vec![problem],
        ..GateOutcome::default()
    };
    assert!(!needs_feedback(&still_open));
    assert_eq!(verdict(&still_open).0, Verdict::Fail);
    // An advisory alone is not a failure.
    let advisory = GateOutcome {
        suite: SuiteState::Ran,
        problems: vec![Problem {
            kind: ProblemKind::ShapeAdvisory,
            text: "patch-shape review".to_string(),
        }],
        ..GateOutcome::default()
    };
    assert_eq!(verdict(&advisory).0, Verdict::Pass);
    // Nothing C/C++ changed: no claim about builds.
    assert_eq!(verdict(&GateOutcome::default()).0, Verdict::NoClaim);
}

#[tokio::test]
async fn a_change_that_builds_and_passes_its_tests_is_pass() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\n// sum\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, true).await;
    assert_eq!(verdict(&outcome), (check_result::Verdict::Pass, None), "{outcome:#?}");
    let feedback = render_feedback(&outcome, 1, 2);
    assert!(feedback.ends_with(&outcome.result().render_line()), "{feedback}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}


/// A background check builds a copy of the code taken when it starts; editing the work
/// tree afterwards changes nothing the check sees, and the copy has the same content key.
#[tokio::test]
async fn the_snapshot_copy_is_the_code_as_it_was_when_the_check_started() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let edited = "#include \"calc.h\"\n#include \"extra.h\"\n// sum\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n";
    std::fs::write(root.join("src/calc.cpp"), edited).unwrap();
    std::fs::write(root.join("src/extra.h"), "int extra();\n").unwrap();
    let changes = agent_changes(&snapshot).await;

    let mirror = mirror_snapshot(&snapshot).await.expect("copy");
    assert_ne!(mirror.root, snapshot.root);
    let mirror_changes = agent_changes(&mirror).await;
    assert_eq!(mirror_changes.content_key, changes.content_key, "the copy is the same code");
    assert_eq!(std::fs::read_to_string(mirror.root.join("src/calc.cpp")).unwrap(), edited);
    assert!(mirror.root.join("src/extra.h").is_file(), "new files are part of the copy");

    // The agent breaks the work tree while the check runs: the copy's check still passes.
    std::fs::write(root.join("src/calc.cpp"), "this does not compile\n").unwrap();
    let (mut base, mut memo, mut advisories) = (BaseCache::default(), None, Advisories::default());
    let mut outcome = run_gate(GateInput {
        snapshot: &mirror,
        changes: &mirror_changes,
        settings: &settings(),
        native_exec: true,
        non_interactive: false,
        impact_state: None,
        base: &mut base,
        assistant_text: "",
        memo: &mut memo,
        advisories: &mut advisories,
        crash: None,
        session_left: Duration::from_secs(3_000),
        advisory: false,
    })
    .await;
    outcome.map_paths(&mirror.root, &snapshot.root);
    assert_eq!(outcome.result().verdict, check_result::Verdict::Pass, "{outcome:#?}");

    // A second copy is refreshed in place: the deletion and the new content arrive.
    std::fs::remove_file(root.join("src/extra.h")).unwrap();
    let again = mirror_snapshot(&snapshot).await.expect("copy");
    assert_eq!(again.root, mirror.root, "one stable copy per repository");
    assert!(!again.root.join("src/extra.h").exists());
    assert_eq!(
        std::fs::read_to_string(again.root.join("src/calc.cpp")).unwrap(),
        "this does not compile\n"
    );
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    let _ = std::fs::remove_dir_all(&mirror.scratch);
}

/// A docs-only change starts no build and no check, and is NOT RUN (no code or
/// build change), never PASS and never a feedback round.
#[tokio::test]
async fn a_docs_only_change_starts_no_build() {
    use check_result::NotRunReason;
    use check_result::Verdict;
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
    std::fs::write(root.join("README.md"), "# calc\n").unwrap();
    std::fs::create_dir_all(root.join("docs/img")).unwrap();
    std::fs::write(root.join("docs/guide.rst"), "Guide\n").unwrap();
    std::fs::write(root.join("docs/img/arch.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(verdict(&outcome), (Verdict::NotRun, Some(NotRunReason::NoCodeChange)), "{outcome:#?}");
    assert!(!needs_feedback(&outcome));
    assert!(!snapshot.scratch.join("cur-build").exists(), "no build tree was configured");
    assert!(!snapshot.scratch.join("logs/build.log").exists(), "no build ran");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// Unknown file types count as code. A change to a script in a C/C++ project builds and
/// tests (it used to be NO CLAIM: nothing C/C++ changed).
#[tokio::test]
async fn an_unknown_file_type_counts_as_code() {
    use check_result::Verdict;
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::write(root.join("tools/gen.py"), "print(1)\n").unwrap();
    assert!(git(root, &["add", "-A"]) && git(root, &["commit", "-q", "-m", "tool"]));
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(root.join("tools/gen.py"), "print(2)\n").unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(outcome.suite, SuiteState::Ran, "{outcome:#?}");
    assert_eq!(verdict(&outcome).0, Verdict::Pass, "{outcome:#?}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// The local gate reads the profile from the commit the session started on, so a
/// profile change that is not committed (or made during the session) never decides what counts
/// as docs for its own checks.
#[tokio::test]
async fn the_non_code_list_comes_from_the_session_start_commit() {
    use check_result::NotRunReason;
    use check_result::Verdict;
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    std::fs::create_dir_all(root.join(".byteask")).unwrap();
    std::fs::write(
        root.join(".byteask/checks.toml"),
        "schema = 1\n[files]\nnon_code = [\"**/*.txt\"]\n",
    )
    .unwrap();
    assert!(git(root, &["add", "-A"]) && git(root, &["commit", "-q", "-m", "profile"]));
    // Uncommitted at session start: claims C++ sources are docs. It must not apply.
    std::fs::write(
        root.join(".byteask/checks.toml"),
        "schema = 1\n[files]\nnon_code = [\"**/*.txt\", \"**/*.cpp\"]\n",
    )
    .unwrap();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(root.join("notes.txt"), "the committed profile says .txt is not code\n").unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(verdict(&outcome), (Verdict::NotRun, Some(NotRunReason::NoCodeChange)), "{outcome:#?}");
    std::fs::write(root.join("src/calc.cpp"), "#include \"calc.h\"\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n").unwrap();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(outcome.suite, SuiteState::Ran, "the .cpp edit is code: {outcome:#?}");
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// Checked from /proc: the tests the gate runs (and so its builds) run niced, on the
/// spare CPUs only, while the engine itself keeps its priority.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_gates_builds_and_tests_run_niced_on_spare_cores() {
    let Some(dir) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let root = dir.path();
    let probe = root.join("probe.txt");
    // A test that records its own nice value and CPU list from /proc.
    std::fs::write(
        root.join("CMakeLists.txt"),
        format!(
            "cmake_minimum_required(VERSION 3.5)\nproject(calc CXX)\nenable_testing()\n\
             ADD_LIBRARY(calc src/calc.cpp)\ntarget_include_directories(calc PUBLIC src)\n\
             add_executable(calc_test test/calc_test.cpp)\ntarget_link_libraries(calc_test calc)\n\
             ADD_TEST(NAME adds COMMAND calc_test add)\n\
             ADD_TEST(NAME probe COMMAND sh -c \"awk '{{print $19}}' /proc/self/stat > {p}; grep Cpus_allowed_list /proc/self/status >> {p}\")\n",
            p = probe.display()
        ),
    )
    .unwrap();
    assert!(git(root, &["add", "-A"]) && git(root, &["commit", "-q", "-m", "probe"]));
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(
        root.join("src/calc.cpp"),
        "#include \"calc.h\"\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n",
    )
    .unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(outcome.suite, SuiteState::Ran, "{outcome:#?}");
    let text = std::fs::read_to_string(&probe).expect("the probe test ran");
    let mut lines = text.lines();
    let nice: i32 = lines.next().unwrap().trim().parse().unwrap();
    let cpus = crate::priority::parse_cpu_list(lines.next().unwrap().split(':').nth(1).unwrap()).unwrap();
    let own = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    assert!(nice >= crate::priority::DEFAULT_NICE && nice >= own, "nice {nice}");
    let allowed = crate::priority::allowed_cpus();
    assert_eq!(cpus, crate::priority::spare(&allowed), "{text}");
    if allowed.len() >= 2 {
        assert!(!cpus.contains(&allowed[0]));
    }
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}

/// The gate's scratch tree compiles through the shared compiler cache, so a second
/// checkout of the same commit (a new session in a new worktree) gets its objects from the
/// cache instead of compiling them.
#[tokio::test]
async fn a_second_checkout_compiles_from_the_shared_compiler_cache() {
    let Some(sccache) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join("sccache")).find(|p| p.is_file())
    }) else {
        eprintln!("skipped: sccache not available");
        return;
    };
    let Some(first) = cmake_fixture() else {
        eprintln!("skipped: cmake/c++/git not available");
        return;
    };
    let second = tempfile::tempdir().unwrap();
    let clone = second.path().join("calc");
    assert!(git(second.path(), &["clone", "-q", &first.path().to_string_lossy(), &clone.to_string_lossy()]));
    let mut builds = Vec::new();
    for root in [first.path().to_path_buf(), clone.clone()] {
        let snapshot = take_snapshot(&root).await.expect("snapshot");
        let _ = std::fs::remove_dir_all(&snapshot.scratch);
        std::fs::write(
            root.join("src/calc.cpp"),
            "#include \"calc.h\"\nint add(int a, int b) { return b + a; }\nint sub(int a, int b) { return a - b; }\n",
        )
        .unwrap();
        let mut base = BaseCache::default();
        let outcome = gate_on(&root, &snapshot, &mut base, false).await;
        assert_eq!(outcome.suite, SuiteState::Ran, "{outcome:#?}");
        let cache = std::fs::read_to_string(snapshot.scratch.join("cur-build/CMakeCache.txt")).unwrap();
        assert!(cache.contains("CMAKE_CXX_COMPILER_LAUNCHER") && cache.contains("sccache"), "launcher set");
        let socket = std::fs::canonicalize(snapshot.scratch.join("cur-build"))
            .unwrap()
            .join(".byteask-sccache.sock");
        let stats = std::process::Command::new(&sccache)
            .arg("--show-stats")
            .env("SCCACHE_SERVER_UDS", &socket)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&stats.stdout).to_string();
        let hits: u64 = text
            .lines()
            .find(|l| l.starts_with("Cache hits") && !l.contains('('))
            .and_then(|l| l.split_whitespace().last()?.parse().ok())
            .unwrap_or(0);
        builds.push(hits);
        let _ = std::process::Command::new(&sccache)
            .arg("--stop-server")
            .env("SCCACHE_SERVER_UDS", &socket)
            .output();
        let _ = std::fs::remove_dir_all(&snapshot.scratch);
    }
    assert!(builds[1] > 0, "the second checkout compiled nothing from the cache: {builds:?}");
}

#[test]
fn the_compiler_cache_can_be_switched_off_and_never_breaks_the_cmake_list() {
    let dir = tempfile::tempdir().unwrap();
    let off = |key: &str| (key == build::ENV_COMPILER_CACHE).then(|| "off".to_string());
    assert_eq!(build::compiler_cache_launcher(dir.path(), dir.path(), off), None);
    let fake = dir.path().join("sccache");
    std::fs::write(&fake, "#!/bin/sh\n").unwrap();
    let named = |key: &str| (key == build::ENV_COMPILER_CACHE).then(|| fake.to_string_lossy().into_owned());
    let launcher = build::compiler_cache_launcher(&dir.path().join("src"), dir.path(), named).expect("launcher");
    assert!(launcher.starts_with("env;SCCACHE_BASEDIRS="), "{launcher}");
    assert!(launcher.ends_with(&*fake.to_string_lossy()), "{launcher}");
    let colon = dir.path().join("a:b");
    assert_eq!(build::compiler_cache_launcher(&colon, dir.path(), named), None);
}

/// Security (0.2.0): a CMake tree in a shared temp directory that names this repository as its
/// source is never picked up (someone else could have configured it to run their commands).
#[test]
fn build_trees_in_shared_temp_directories_are_never_used() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CMakeLists.txt"), "project(x)\n").unwrap();
    let planted = tempfile::tempdir().unwrap();
    let canon = std::fs::canonicalize(repo.path()).unwrap();
    std::fs::write(
        planted.path().join("CMakeCache.txt"),
        format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", canon.display()),
    )
    .unwrap();
    let found = build::cmake_build_dirs(repo.path(), Path::new("/nonexistent"));
    assert!(found.is_empty(), "{found:?}");
    // The same tree inside the repository is the project's own and is found.
    let own = repo.path().join("build");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::copy(planted.path().join("CMakeCache.txt"), own.join("CMakeCache.txt")).unwrap();
    assert_eq!(build::cmake_build_dirs(repo.path(), Path::new("/nonexistent")).len(), 1);
}

/// /tmp growth (0.2.0): the gate's scratch lives in this user's private directory, not under a
/// predictable shared `/tmp` name, and old trees are evicted.
#[test]
fn scratch_is_private_and_bounded() {
    let root = Path::new("/some/repo");
    let scratch = scratch_for(root);
    assert!(scratch.starts_with(scratch_root()), "{}", scratch.display());
    assert!(!scratch.starts_with("/tmp/byteask-gate-"), "{}", scratch.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let parent = scratch_root().parent().map(Path::to_path_buf).unwrap();
        let meta = std::fs::metadata(&parent).unwrap();
        assert_eq!(meta.mode() & 0o077, 0, "the per-user directory is private");
    }
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().join("gate-keep");
    for name in ["gate-keep", "gate-old", "testsel-old", "unrelated", "gate-fresh"] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
    }
    touch_scratch(&dir.path().join("gate-fresh"));
    let old = std::time::SystemTime::now() - Duration::from_secs(10 * 24 * 3_600);
    for name in ["gate-keep", "gate-old", "testsel-old", "unrelated"] {
        let stamp = dir.path().join(name).join(".last-used");
        std::fs::write(&stamp, b"").unwrap();
        std::fs::File::options().write(true).open(&stamp).unwrap().set_modified(old).unwrap();
    }
    evict_scratch(dir.path(), &keep);
    assert!(keep.is_dir(), "the tree in use is kept");
    assert!(!dir.path().join("gate-old").exists());
    assert!(!dir.path().join("testsel-old").exists());
    assert!(dir.path().join("unrelated").is_dir(), "only our own names are touched");
    assert!(dir.path().join("gate-fresh").is_dir(), "a tree in use is never evicted");
}

/// A base tree on disk is reused only when it was made from the same session-start state.
#[tokio::test]
async fn a_base_tree_from_another_start_is_made_again() {
    let Some(dir) = plain_repo(&[("a.c", "int a;\n")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let scratch = tempfile::tempdir().unwrap();
    let base = scratch.path().join("base");
    let log = scratch.path().join("copy.log");
    assert!(build::prepare_base_tree(root, &base, "h1", "h1\nx", &[], None, &[], &log).await);
    std::fs::write(base.join("stale-marker-file"), b"").unwrap();
    // Same start: reused as it is.
    assert!(build::prepare_base_tree(root, &base, "h1", "h1\nx", &[], None, &[], &log).await);
    assert!(base.join("stale-marker-file").exists());
    // Another start (another commit or start state): made again from scratch.
    assert!(build::prepare_base_tree(root, &base, "h2", "h2\nx", &[], None, &[], &log).await);
    assert!(!base.join("stale-marker-file").exists());
}

/// The automatic gate and checklist are off unless switched on.
#[test]
fn the_automatic_gate_and_checklist_are_off_by_default() {
    let defaults = GateSettings::default();
    assert!(!defaults.gate && !defaults.impact, "{defaults:?}");
    let on = GateSettings::resolve(|_| None, |_| None, |name| {
        matches!(name, ENV_GATE | ENV_IMPACT).then(|| "1".to_string())
    });
    assert!(on.gate && on.impact);
}

/// A build that exits non-zero with no recognised error line is FAIL with the end
/// of the log, and stale test binaries are not run (the automatic gate too).
#[tokio::test]
async fn an_unrecognised_build_failure_fails_the_automatic_gate() {
    if !have("make") || !have("git") {
        eprintln!("skipped: make/git not available");
        return;
    }
    let Some(dir) = plain_repo(&[
        ("Makefile", "all:\n\t@echo 'CMake Error at odd.cmake:3'; exit 1\ncheck:\n\t@echo 'PASS: stale'\n"),
        ("lib.c", "int f;\n"),
        (".gitignore", ""),
    ]) else {
        return;
    };
    let root = dir.path();
    let snapshot = take_snapshot(root).await.expect("snapshot");
    std::fs::write(root.join("lib.c"), "int f = 2;\n").unwrap();
    let mut base = BaseCache::default();
    let outcome = gate_on(root, &snapshot, &mut base, false).await;
    assert_eq!(kinds(&outcome), vec![ProblemKind::Build], "{outcome:#?}");
    assert!(outcome.problems[0].text.contains("CMake Error at odd.cmake"), "{outcome:#?}");
    assert_eq!(outcome.result().verdict, check_result::Verdict::Fail);
    let _ = std::fs::remove_dir_all(&snapshot.scratch);
}
