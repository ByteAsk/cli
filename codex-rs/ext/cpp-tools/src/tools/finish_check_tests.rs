use std::path::Path;

use check_result::Verdict;
use pretty_assertions::assert_eq;

use super::*;
use crate::gate::GateSettings;
use crate::impact::tests::git;

/// `BYTEASK_CHECK_RELATED_FIRST`: off, the related section is appended and every note listed;
/// on, it comes before the "Checks:" line (after the verdict and problems) and the notes stop at NOTES_CAP.
#[test]
fn related_first_and_notes_cap() {
    let notes: Vec<String> = (0..2822).map(|i| format!("T{i} runs no program of its own; built t{i}")).collect();
    assert_eq!(capped_notes(&notes, false), notes.join("; "));
    let capped = capped_notes(&notes, true);
    assert!(capped.starts_with("T0 runs no program of its own; built t0; T1 "), "{capped}");
    assert!(capped.ends_with("T11 runs no program of its own; built t11; and 2810 more not listed"), "{capped}");
    assert_eq!(capped_notes(&notes[..12], true), notes[..12].join("; "));
    let body = "Finish check (advisory): no problem found.\nChecks: a; b\nNot checked: c\n";
    let section = "\nRelated places (ByteAsk Context's find_related_changes on this change; ...):\n1. x.cc:3\n";
    let mut off = body.to_string();
    place_related(&mut off, section, false);
    assert_eq!(off, format!("{body}{section}"));
    let mut on = body.to_string();
    place_related(&mut on, section, true);
    assert_eq!(on, "Finish check (advisory): no problem found.\nRelated places (ByteAsk Context's \
        find_related_changes on this change; ...):\n1. x.cc:3\nChecks: a; b\nNot checked: c\n");
    let mut only = "Finish check (advisory):\n1. a problem\nNot checked: c\n".to_string();
    place_related(&mut only, section, true);
    assert!(only.starts_with("Finish check (advisory):\n1. a problem\nRelated places"), "{only}");
    let mut bare = "Finish check (advisory): no problem found.".to_string();
    place_related(&mut bare, section, true);
    assert_eq!(bare, format!("Finish check (advisory): no problem found.{section}"));
}

fn have(tool: &str) -> bool {
    crate::tools::binary_on_path(tool)
}

/// A committed repository with these files.
fn repo(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
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
    (git(root, &["init", "-q"])
        && git(root, &["add", "-A"])
        && git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]))
    .then_some(dir)
}

/// The tool as a C/C++ root thread has it: the workflow with today's defaults (automatic gate
/// and checklist off), its session-start snapshot taken before any edit.
async fn tool_for(root: &Path) -> FinishCheckTool {
    let workflow = Workflow::start(GateSettings::default(), true, false, root.to_path_buf());
    assert!(!workflow.settings.gate, "the automatic gate is off by default");
    assert!(workflow.snapshot().await.is_some(), "the snapshot exists with the gate off");
    FinishCheckTool {
        cwd: root.to_path_buf(),
        native_exec: true,
        workflow: Some(workflow),
    }
}

fn cleanup(tool: &FinishCheckTool) {
    let _ = std::fs::remove_dir_all(crate::gate::scratch_for(&tool.cwd));
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(&tool.cwd));
}

/// The check never deletes a file. An in-place `make` build creates an untracked
/// binary in the source tree. It stays, and the answer names
/// it. A new untracked test file and a scratch file of the session stay too, and none of the
/// gate's hygiene rules (new tests, leftovers, edited tests) applies.
#[tokio::test]
async fn it_never_deletes_files_and_applies_no_hygiene_rules() {
    if !have("make") || !(have("cc") || have("gcc")) {
        eprintln!("skipped: make/cc not available");
        return;
    }
    let Some(dir) = repo(&[
        (
            "Makefile",
            "all: tool\ntool: tool.c\n\tcc -o tool tool.c\ncheck: tool\n\t@./tool one && echo 'PASS: one' || echo 'FAIL: one'\n",
        ),
        ("tool.c", "int main(void) { return 0; }\n"),
        ("tests/old_test.c", "int old;\n"),
        (".gitignore", ""),
    ]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let tool = tool_for(root).await;
    std::fs::write(root.join("tool.c"), "int main(void) { return 0; } /* edited */\n").unwrap();
    std::fs::write(root.join("tests/old_test.c"), "int old; /* edited */\n").unwrap();
    std::fs::write(root.join("tests/new_test.c"), "int fresh;\n").unwrap();
    std::fs::write(root.join("notes.txt"), "scratch\n").unwrap();

    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert_eq!(result.check, "cpp_finish_check");
    assert!(root.join("tool").is_file(), "the binary the build created is left in place: {text}");
    assert!(text.contains("left in place: tool"), "{text}");
    for kept in ["tests/new_test.c", "notes.txt", "tool.c", "tests/old_test.c"] {
        assert!(root.join(kept).is_file(), "{kept} must not be deleted");
    }
    for rule in ["NEW TEST FILES", "FILES LEFT FROM THIS SESSION", "EXISTING TEST FILES EDITED"] {
        assert!(!text.contains(rule), "no hygiene rule in the advisory check: {text}");
    }
    cleanup(&tool);
}

/// A build that fails without any recognised error line (here: a script that
/// prints something odd and exits 2) is FAIL with the end of the build log, and the tests are
/// not run on whatever binaries an earlier build left.
#[tokio::test]
async fn a_build_failing_with_an_unrecognised_error_is_fail_not_pass() {
    if !have("make") {
        eprintln!("skipped: make not available");
        return;
    }
    let Some(dir) = repo(&[
        (
            "Makefile",
            "all:\n\t@echo 'generator step went sideways'; exit 2\ncheck:\n\t@echo 'PASS: stale'\n",
        ),
        ("lib.c", "int f(void) { return 1; }\n"),
        (".gitignore", ""),
    ]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let tool = tool_for(root).await;
    std::fs::write(root.join("lib.c"), "int f(void) { return 2; }\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Fail, "{text}");
    assert!(text.contains("BUILD FAILS"), "{text}");
    assert!(text.contains("generator step went sideways"), "the end of the log is shown: {text}");
    assert!(text.contains("tests: not run"), "{text}");
    assert!(!text.contains("PASS: stale"), "{text}");
    cleanup(&tool);
}

/// A suite with no tests is "no tests ran", never PASS.
#[tokio::test]
async fn zero_tests_is_not_pass() {
    if !have("cmake") || !have("ctest") || !(have("c++") || have("g++") || have("clang++")) {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    let Some(dir) = repo(&[
        (
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.5)\nproject(z CXX)\nenable_testing()\nadd_library(z z.cpp)\n",
        ),
        ("z.cpp", "int z() { return 1; }\n"),
        (".gitignore", ""),
    ]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = dir.path();
    let tool = tool_for(root).await;
    std::fs::write(root.join("z.cpp"), "int z() { return 2; }\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::NotRun, "{text}");
    assert!(text.contains("no tests ran"), "{text}");
    cleanup(&tool);
}

/// Nothing changed since the session started: NOT RUN (no code change), never PASS.
#[tokio::test]
async fn nothing_changed_is_not_run() {
    let Some(dir) = repo(&[("a.c", "int a;\n")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let tool = tool_for(dir.path()).await;
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::NotRun, "{text}");
    assert_eq!(result.reason, Some(check_result::NotRunReason::NoCodeChange));
    cleanup(&tool);
}

/// `finish_related`: with Context's `find_related_changes` registered for the repository, the check
/// sends it the change as a diff and lists what it names under "Related places"; an
/// empty answer and an error are each said in one line instead of nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn related_places_from_context_are_listed() {
    use codex_core::byteask_context::SelectInput;
    use codex_core::byteask_context::register_related_changes_fn;
    use std::sync::Arc;
    use std::sync::Mutex;
    let Some(dir) = repo(&[("a.cpp", "int a() { return 1; }\n"), (".gitignore", "")]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    std::fs::write(root.join("a.cpp"), "int a() { return 2; }\n").unwrap();
    let seen: Arc<Mutex<Option<SelectInput>>> = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    register_related_changes_fn(
        &root,
        Arc::new(move |input| {
            let record = Arc::clone(&record);
            Box::pin(async move {
                *record.lock().unwrap() = Some(input);
                Ok("1. b() \u{2014} b.cpp:3 (same-name sibling of a())".to_string())
            })
        }),
    );
    let (text, _) = tool.check().await;
    assert!(
        text.contains(
            "Related places (ByteAsk Context's find_related_changes on this change; check whether each place it names needs the same change):\n1. b() \u{2014} b.cpp:3"
        ),
        "{text}"
    );
    match seen.lock().unwrap().clone() {
        Some(SelectInput::Diff(diff)) => assert!(diff.contains("+int a() { return 2; }"), "{diff}"),
        other => panic!("expected the change as a diff, got {other:?}"),
    }
    register_related_changes_fn(&root, Arc::new(|_| Box::pin(async { Ok("{\"related\": []}".to_string()) })));
    let (text, _) = tool.check().await;
    assert!(
        text.contains("Related places: ByteAsk Context's find_related_changes named none for this change."),
        "{text}"
    );
    register_related_changes_fn(&root, Arc::new(|_| Box::pin(async { Err("not indexed".to_string()) })));
    let (text, _) = tool.check().await;
    assert!(
        text.contains("Related places: ByteAsk Context's find_related_changes gave no answer for this change (not indexed)"),
        "{text}"
    );
    cleanup(&tool);
}

// ---------------------------------------------------------------------------------------------
// The targeted check and the background warm build on a real CMake + CTest project
// ---------------------------------------------------------------------------------------------

fn have_cmake_toolchain() -> bool {
    have("cmake") && have("ctest") && (have("c++") || have("g++") || have("clang++"))
}

/// Two libraries, two test programs: `a_test` links `liba`, `b_test` links `libb`.
fn two_target_project(extra_cmake: &str) -> Option<tempfile::TempDir> {
    let cmakelists = format!(
        "cmake_minimum_required(VERSION 3.14)\nproject(w CXX)\nenable_testing()\n\
add_library(liba STATIC src/a.cpp)\ntarget_include_directories(liba PUBLIC include)\n\
add_library(libb STATIC src/b.cpp)\ntarget_include_directories(libb PUBLIC include)\n\
add_executable(a_test tests/a_test.cpp)\ntarget_link_libraries(a_test liba)\n\
add_executable(b_test tests/b_test.cpp)\ntarget_link_libraries(b_test libb)\n\
add_test(NAME a_test COMMAND a_test)\nadd_test(NAME b_test COMMAND b_test)\n{extra_cmake}"
    );
    repo(&[
        ("CMakeLists.txt", cmakelists.as_str()),
        ("include/a.h", "#pragma once\nint a();\n"),
        ("include/b.h", "#pragma once\nint b();\n"),
        ("src/a.cpp", "#include \"a.h\"\nint a() { return 1; }\n"),
        ("src/b.cpp", "#include \"b.h\"\nint b() { return 2; }\n"),
        ("tests/a_test.cpp", "#include \"a.h\"\nint main() { return a() == 1 ? 0 : 1; }\n"),
        ("tests/b_test.cpp", "#include \"b.h\"\nint main() { return b() == 2 ? 0 : 1; }\n"),
        (".gitignore", ""),
    ])
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A change to `liba` builds only `a_test` (and what it links) in the tools' own tree,
/// runs exactly `ctest -R '^(a_test)$'`, and says so; `b_test` is never built. The failure
/// passes on the unmodified tree, so it is a regression (FAIL), and `git diff --check` is shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targeted_check_builds_only_the_affected_test_program() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\" \nint a() { return 3; }\n").unwrap();

    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Fail, "{text}");
    assert!(text.contains("builds and runs only what the change reaches"), "{text}");
    assert!(text.contains("built only targets a_test in its own tree"), "{text}");
    assert!(text.contains("ctest -R '^(a_test)$'"), "{text}");
    assert!(text.contains("selection: 1 of 2 registered test(s) reach the change"), "{text}");
    assert!(text.contains("TESTS THAT PASS ON THE UNMODIFIED TREE NOW FAIL"), "{text}");
    assert!(text.contains("src/a.cpp:1: trailing whitespace."), "{text}");
    let own = crate::testsel::tree::own_tree(&root);
    assert!(own.join("a_test").is_file(), "a_test is built in the tools' own tree: {text}");
    assert!(!own.join("b_test").exists(), "b_test is not affected and must not be built: {text}");
    // Nothing was built in (or written to) the project.
    assert!(!root.join("build").exists() && !root.join("CMakeCache.txt").exists());

    // Fixed: PASS, and the unaffected program is still not built.
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; }\n// fixed\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(result.summary.contains("The targets the change reaches built"), "{result:?}");
    assert!(text.contains("tests: 1 passed of 1 (ctest -R '^(a_test)$'"), "{text}");
    assert!(text.contains(": clean"), "{text}");
    assert!(!own.join("b_test").exists(), "{text}");
    cleanup(&tool);
}

/// A project whose tests are behind its own option, OFF by default (ada's ADA_TESTING, sml's
/// SML_BUILD_TESTS): the check turns the option on in its own tree and runs the test. When
/// `fail_with_tests` is set, configuring with the option on fails, and the tree is configured
/// without it (the unconditional test still runs).
fn opt_in_tests_project(fail_with_tests: bool) -> Option<tempfile::TempDir> {
    let gated = if fail_with_tests {
        "if(W_TESTS)\n  message(FATAL_ERROR \"needs a test framework\")\nendif()\n\
enable_testing()\nadd_executable(a_test tests/a_test.cpp)\ntarget_link_libraries(a_test liba)\n\
add_test(NAME a_test COMMAND a_test)\n"
    } else {
        "if(W_TESTS)\n  enable_testing()\n  add_executable(a_test tests/a_test.cpp)\n\
  target_link_libraries(a_test liba)\n  add_test(NAME a_test COMMAND a_test)\nendif()\n"
    };
    let cmakelists = format!(
        "cmake_minimum_required(VERSION 3.14)\nproject(w CXX)\noption(W_TESTS \"Build tests\" OFF)\n\
add_library(liba STATIC src/a.cpp)\ntarget_include_directories(liba PUBLIC include)\n{gated}"
    );
    repo(&[
        ("CMakeLists.txt", cmakelists.as_str()),
        ("include/a.h", "#pragma once\nint a();\n"),
        ("src/a.cpp", "#include \"a.h\"\nint a() { return 1; }\n"),
        ("tests/a_test.cpp", "#include \"a.h\"\nint main() { return a() == 1 ? 0 : 1; }\n"),
        (".gitignore", ""),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_check_turns_on_the_projects_test_option() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    for fail_with_tests in [false, true] {
        let Some(dir) = opt_in_tests_project(fail_with_tests) else {
            eprintln!("skipped: git not available");
            return;
        };
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let tool = tool_for(&root).await;
        std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // same\n").unwrap();
        let (text, result) = tool.check().await;
        assert_eq!(result.verdict, Verdict::Pass, "{text}");
        assert!(text.contains("tests: 1 passed of 1"), "{text}");
        if fail_with_tests {
            assert!(
                text.contains("without the project's test options: configuring with BUILD_TESTING, W_TESTS on failed"),
                "{text}"
            );
        } else {
            assert!(text.contains("with the project's test options on (W_TESTS)"), "{text}");
        }
        cleanup(&tool);
    }
}

/// Many test programs and not enough time: the nearest tests' programs are built and run first,
/// and the tests whose programs did not finish building are named as not checked (never a
/// silent PASS, never nothing verified). `s1_test`/`s2_test` take 40 s to build (a generated
/// source behind `cmake -E sleep`), the check has 35 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_nearest_tests_run_first_when_the_rest_does_not_fit() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let mut cmake = String::from(
        "cmake_minimum_required(VERSION 3.14)\nproject(w CXX)\nenable_testing()\n\
add_library(liba STATIC src/a.cpp)\ntarget_include_directories(liba PUBLIC include)\n",
    );
    for fast in ["a_test", "b_test", "c_test"] {
        cmake.push_str(&format!(
            "add_executable({fast} tests/ok.cpp)\ntarget_link_libraries({fast} liba)\nadd_test(NAME {fast} COMMAND {fast})\n"
        ));
    }
    for slow in ["s1_test", "s2_test"] {
        cmake.push_str(&format!(
            "add_custom_command(OUTPUT {slow}.cpp COMMAND ${{CMAKE_COMMAND}} -E sleep 40 \
COMMAND ${{CMAKE_COMMAND}} -E copy ${{CMAKE_SOURCE_DIR}}/tests/ok.cpp {slow}.cpp DEPENDS tests/ok.cpp)\n\
add_executable({slow} ${{CMAKE_BINARY_DIR}}/{slow}.cpp tests/extra.cpp)\ntarget_link_libraries({slow} liba)\n\
add_test(NAME {slow} COMMAND {slow})\n"
        ));
    }
    let Some(dir) = repo(&[
        ("CMakeLists.txt", cmake.as_str()),
        ("include/a.h", "#pragma once\nint a();\n"),
        ("src/a.cpp", "#include \"a.h\"\nint a() { return 1; }\n"),
        ("tests/ok.cpp", "#include \"a.h\"\nint main() { return a() == 1 ? 0 : 1; }\n"),
        ("tests/extra.cpp", "int extra() { return 0; }\n"),
        (".gitignore", ""),
    ]) else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    super::TEST_BUDGETS
        .lock()
        .unwrap()
        .push((root.clone(), Duration::from_secs(35)));
    let tool = tool_for(&root).await;
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // same\n").unwrap();
    let (text, result) = tool.check().await;
    assert_ne!(result.verdict, Verdict::Fail, "{text}");
    assert!(text.contains("35s limit"), "{text}");
    assert!(text.contains("built the nearest tests' targets a_test, b_test, c_test (of 5 to build)"), "{text}");
    assert!(text.contains("tests: 3 passed of 3"), "{text}");
    assert!(
        text.contains("2 more test(s) the change reaches, whose programs did not finish building within the time limit (35s"),
        "{text}"
    );
    assert!(text.contains("s1_test, s2_test"), "{text}");
    cleanup(&tool);
}

/// A test the build model cannot map (a shell command) falls back to the smallest safe
/// superset, every executable target, and the answer says so; it is never a silent "build
/// everything".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmapped_test_falls_back_to_a_stated_superset() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = two_target_project("add_test(NAME script_check COMMAND sh -c \"exit 0\")\n") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // same\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(
        text.contains("fallback: the build model does not say which program script_check runs, so every executable target (2) is built"),
        "{text}"
    );
    assert!(text.contains("built only targets a_test, b_test"), "{text}");
    assert!(text.contains("ctest -R '^(a_test|script_check)$'"), "{text}");
    assert!(!text.contains("everything"), "{text}");
    cleanup(&tool);
}

/// Warm build, then check: the warm build fills the tools' tree before the change; the check afterwards
/// only recompiles the edited test source (the library it links is not rebuilt).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_build_makes_the_check_incremental() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    let warm = crate::start_warm_build(&root, &["a_test".to_string(), "no_such_program".to_string()]);
    let outcome = warm.wait().await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.targets, vec!["a_test".to_string()]);
    assert_eq!(outcome.ignored, vec!["no_such_program".to_string()]);
    let own = crate::testsel::tree::own_tree(&root);
    assert_eq!(outcome.tree.as_deref(), Some(own.as_path()));
    let lib = own.join("libliba.a");
    let lib_built = mtime(&lib).expect("the warm build built liba");
    let program_built = mtime(&own.join("a_test")).expect("the warm build built a_test");
    assert!(!own.join("b_test").exists());

    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(root.join("tests/a_test.cpp"), "#include \"a.h\"\nint main() { return a() == 1 ? 0 : 1; } // edited\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(text.contains("built only targets a_test"), "{text}");
    assert_eq!(mtime(&lib), Some(lib_built), "the library the warm build made is reused: {text}");
    assert!(mtime(&own.join("a_test")) > Some(program_built), "the edited program is rebuilt: {text}");
    cleanup(&tool);
}

/// ByteAsk Context's select_tests (when registered for the repository) and the local build graph
/// choose together: the check runs the tests both name; when they share none, both sets; select_tests
/// receives the change as a diff, new untracked files included. An answer it cannot trust (here:
/// unknown) falls back to the local build graph's selection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_tests_from_context_chooses_and_unknown_falls_back() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    // The change reaches liba (a_test). Context names a_test and b_test: only a_test (both name it) runs.
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // touched\n").unwrap();
    std::fs::write(root.join("tests/new_test.cpp"), "int main() { return 0; }\n").unwrap();
    let diffs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let answer = std::sync::Arc::new(std::sync::Mutex::new(serde_json::json!({
        "confidence": "selected", "stale": false, "truncated": false, "build_targets": ["a_test"],
        "tests": [{"target": "a_test", "ctest": ["a_test"], "tier": "direct", "why": "stand-in"}],
        "also": [{"target": "b_test", "ctest": ["b_test"], "tier": "links", "why": "stand-in"}],
    })));
    let (seen, reply) = (diffs.clone(), answer.clone());
    codex_core::byteask_context::register_select_tests_fn(
        &root,
        std::sync::Arc::new(move |input: codex_core::byteask_context::SelectInput| {
            let codex_core::byteask_context::SelectInput::Diff(diff) = input else {
                panic!("a small change is sent as a diff");
            };
            seen.lock().unwrap().push(diff);
            let reply = reply.lock().unwrap().clone();
            Box::pin(async move { Ok(reply) })
        }),
    );

    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(text.contains("1 of 2 registered test(s), the tests both name: ByteAsk Context select_tests named 2"), "{text}");
    assert!(text.contains("the local build graph reaches 1"), "{text}");
    assert!(text.contains("ctest -R '^(a_test)$'"), "{text}");
    let sent = diffs.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("src/a.cpp") && sent[0].contains("tests/new_test.cpp"), "{}", sent[0]);

    // Context names only b_test, the graph only a_test: no common test, so both run.
    *answer.lock().unwrap() = serde_json::json!({"confidence": "selected",
        "tests": [{"target": "b_test", "ctest": ["b_test"]}], "also": []});
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // touched twice\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(text.contains("2 of 2 registered test(s), both sets (they share no test)"), "{text}");
    assert!(text.contains("ctest -R '^(a_test|b_test)$'"), "{text}");

    *answer.lock().unwrap() = serde_json::json!({"confidence": "unknown", "reason": "no build model"});
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // touched again\n").unwrap();
    let (text, result) = tool.check().await;
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(text.contains("ByteAsk Context select_tests not used: confidence \"unknown\" (no build model)"), "{text}");
    assert!(text.contains("ctest -R '^(a_test)$'"), "{text}");
    cleanup(&tool);
}

/// With no test programs but the task's place, the warm build picks the tests covering the
/// place from the project's own build, offers one line that builds and runs only those, and
/// builds their programs; `b_test` is never built.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_check_line_names_only_the_tests_covering_the_place() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let own = crate::testsel::tree::own_tree(&root);
    let places = crate::context_place_files(
        "## Where the change most likely belongs\n1. a \u{2014} include/a.h:2-2  `int a()`\n",
        &root,
    );
    assert_eq!(places, vec![root.join("include/a.h")]);
    let warm = crate::start_warm_build_for(&root, &[], &places);
    let plan = warm.check_plan().await.expect("a check line");
    let own_text = crate::testsel::tree::shell_quote(&own.to_string_lossy());
    // The job count is memory-aware (it changes with the machine's load).
    let (head, tail) = plan.line.split_once(" -j").expect("a job count");
    assert_eq!(head, format!("To check your change: cmake --build {own_text} --target a_test"));
    let tail = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    assert_eq!(
        tail,
        format!(" && ctest --test-dir {own_text} -R '^(a_test)$' --output-on-failure")
    );
    assert_eq!(plan.targets, vec!["a_test".to_string()]);
    let outcome = warm.wait().await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.targets, vec!["a_test".to_string()]);
    assert!(own.join("a_test").is_file() && !own.join("b_test").exists());

    // The line runs as given: the covering test passes.
    let command = plan.line.split_once(": ").map(|(_, command)| command).unwrap_or_default();
    let run = std::process::Command::new("sh").arg("-c").arg(command).output().unwrap();
    assert!(
        run.status.success(),
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(&root));
}

/// The warm build takes the tree's lock before it returns, so a check started right after
/// it waits for it (one build at a time in one tree) and then builds on top of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_waits_for_the_warm_build_of_its_tree() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let tool = tool_for(&root).await;
    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // touched\n").unwrap();
    let warm = crate::start_warm_build(&root, &["a_test".to_string()]);
    assert!(!warm.is_finished());
    let (text, result) = tool.check().await;
    assert!(warm.is_finished(), "the check ran only after the warm build released the tree: {text}");
    assert!(warm.outcome().is_some_and(|o| o.ok));
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    // It built on the warm build's tree instead of configuring its own. ("waited Ns" shows only
    // for waits of a second or more, which a fast machine does not need.)
    assert!(!text.contains("configured its own tree"), "{text}");
    cleanup(&tool);
}

/// The warm build never blocks its caller, waits for whoever holds the tree (no second build in the same
/// tree at once), and with no names only configures the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_warm_build_never_blocks_and_takes_turns_in_its_tree() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    let Some(dir) = two_target_project("") else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let own = crate::testsel::tree::own_tree(&root);
    let held = crate::testsel::tree::lock_tree(&own, std::time::Duration::from_secs(5))
        .await
        .expect("free tree");
    let started = std::time::Instant::now();
    let warm = crate::start_warm_build(&root, &["a_test".to_string()]);
    assert!(started.elapsed() < std::time::Duration::from_millis(500), "start_warm_build returns at once");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(!warm.is_finished(), "it waits while another build holds the tree");
    assert!(!own.join("CMakeCache.txt").exists() && !own.join("a_test").exists());
    drop(held);
    let outcome = warm.wait().await;
    assert!(outcome.ok && own.join("a_test").is_file(), "{outcome:?}");

    // Nothing named: configure only (a fresh tree: configured, nothing built).
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(&root));
    let outcome = crate::start_warm_build(&root, &[]).wait().await;
    assert!(outcome.ok && outcome.targets.is_empty(), "{outcome:?}");
    assert!(own.join("CMakeCache.txt").is_file() && !own.join("a_test").exists());

    // Cancelled: stops and reports it.
    let held = crate::testsel::tree::lock_tree(&own, std::time::Duration::from_secs(5)).await.unwrap();
    let warm = crate::start_warm_build(&root, &["b_test".to_string()]);
    warm.cancel();
    let outcome = warm.wait().await;
    assert!(!outcome.ok && outcome.summary.contains("cancelled"), "{outcome:?}");
    drop(held);

    // Not a CMake project: a finished handle, nothing started.
    let plain = tempfile::tempdir().unwrap();
    let outcome = crate::start_warm_build(plain.path(), &["x".to_string()]).wait().await;
    assert!(!outcome.ok && outcome.summary.starts_with("not started"), "{outcome:?}");
    let _ = std::fs::remove_dir_all(crate::testsel::tree::scratch_dir(&root));
}

/// Wall time of `cpp_finish_check` with ByteAsk Context's select_tests vs the local build graph vs the
/// full suite, on real commits (model-free). Not a regular test: run by hand with
///   SELECT_REPLAY_PLAN=<plan.json> SELECT_REPLAY_OUT=<out.jsonl> cargo test -p codex-cpp-tools -- --ignored select_tests_time_replay
/// plan.json: {"repo": name, "root": <checkout with a configured build/ tree at index_head>, "index_head": sha,
///             "commits": [{"commit": sha, "answer": <recorded select_tests JSON>}]}
/// Per commit: the checkout is reset to index_head and the commit's change is reverse-applied (same files and
/// regions as the fix; applies cleanly for "fresh" commits), then these variants run, in an order rotated per
/// commit, each after touching the changed files so every variant recompiles the same code:
///   context  a stand-in select_tests returning the recorded answer (falls back like the real one)
///   local    select_tests unavailable: the local build graph's selection
///   full     a stand-in answer selecting every registered test
///   both     a stand-in answer with only the tests in BOTH the context and the local selection
/// One JSON line per variant: wall seconds, verdict, the answer's selection/build/test lines, the test names it
/// selected and whether they include every test the commit touched (`touched` from the plan's `recorded`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn select_tests_time_replay() {
    use std::io::Write as _;
    let plan_path = std::env::var("SELECT_REPLAY_PLAN").expect("SELECT_REPLAY_PLAN");
    let out_path = std::env::var("SELECT_REPLAY_OUT").expect("SELECT_REPLAY_OUT");
    let plan: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&plan_path).unwrap()).unwrap();
    let root = std::fs::canonicalize(plan["root"].as_str().unwrap()).unwrap();
    let head = plan["index_head"].as_str().unwrap().to_string();
    let repo = plan["repo"].as_str().unwrap_or("?").to_string();
    let cold = std::env::var("SELECT_REPLAY_COLD").is_ok_and(|v| v == "1");
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&out_path).unwrap();
    let sh = |cmd: &str| {
        std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(&root).output().unwrap()
    };
    let answer_slot = std::sync::Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
    let slot = answer_slot.clone();
    codex_core::byteask_context::register_select_tests_fn(
        &root,
        std::sync::Arc::new(move |_input: codex_core::byteask_context::SelectInput| {
            let answer = slot.lock().unwrap().clone();
            Box::pin(async move { answer.ok_or_else(|| "measurement: local arm".to_string()) })
        }),
    );
    for (n, entry) in plan["commits"].as_array().unwrap().iter().enumerate() {
        let commit = entry["commit"].as_str().unwrap();
        let reset = sh(&format!("git checkout -qf {head} && git clean -fdq -e build"));
        assert!(reset.status.success(), "{reset:?}");
        // 3-way: the fix's lines may have moved since; a conflict skips the commit.
        let apply = sh(&format!("git diff {commit}^ {commit} | git apply -R -3"));
        let conflicted = !String::from_utf8_lossy(&sh("git diff --name-only --diff-filter=U").stdout).trim().is_empty();
        let changed = String::from_utf8_lossy(&sh("git diff --name-only HEAD").stdout).to_string();
        if !apply.status.success() || conflicted || changed.trim().is_empty() {
            writeln!(out, "{}", serde_json::json!({"repo": repo, "commit": commit, "skipped": "reverse apply failed"})).unwrap();
            continue;
        }
        // The local graph's selection for this change (the same calls as cpp_finish_check's steps 2-4).
        let scratch = crate::testsel::tree::scratch_dir(&root);
        let version = crate::toolver::tool_version("cmake", &root).await;
        let request = crate::testsel::cmake::Request {
            root: &root,
            build_dir: None,
            scratch: &scratch,
            version,
            cmake_files: &[],
            commit: Some(&head),
        };
        let model = crate::testsel::cmake::model(&request).await.expect("build model");
        let changes = crate::testsel::symbols::changes(&root, &head, "index head".to_string(), None, None)
            .await
            .expect("changes");
        let sel = crate::testsel::select::select(&model, &changes).await;
        let registered: std::collections::BTreeSet<String> = model.tests.iter().map(|t| t.name.clone()).collect();
        let local_names: std::collections::BTreeSet<String> =
            sel.tier3.iter().map(|&i| model.tests[i].name.clone()).collect();
        let names_of = |answer: &serde_json::Value| -> std::collections::BTreeSet<String> {
            ["tests", "also"]
                .iter()
                .filter_map(|k| answer.get(*k).and_then(|v| v.as_array()))
                .flatten()
                .filter_map(|e| e.get("ctest").and_then(|v| v.as_array()))
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|name| registered.contains(name))
                .collect()
        };
        let context_names = names_of(&entry["answer"]);
        let both_names: Vec<String> = context_names.intersection(&local_names).cloned().collect();
        let touched: Vec<String> = entry["recorded"]["touched_tests"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if std::env::var("SELECT_REPLAY_NAMES_ONLY").is_ok_and(|v| v == "1") {
            // Hit rates only (no builds): which tests each selection names, and whether the touched ones are in.
            let hit = |set: &dyn Fn(&String) -> bool| touched.iter().all(|t| set(t));
            writeln!(out, "{}", serde_json::json!({
                "repo": repo, "commit": commit, "names_only": true, "touched": touched,
                "context": context_names.len(), "local": local_names.len(), "both": both_names.len(),
                "registered": registered.len(),
                "local_unmapped": sel.affected.unmapped.len(), "local_scan_truncated": sel.affected.scan_truncated,
                "local_unmapped_headers": sel.affected.unmapped.iter().filter(|f| crate::testsel::is_header(f)).count(),
                "hit_context": hit(&|t| context_names.contains(t)), "hit_local": hit(&|t| local_names.contains(t)),
                "hit_both": hit(&|t| both_names.contains(t)),
            })).unwrap();
            continue;
        }
        let mut arms = vec!["context", "local", "full", "both"];
        arms.rotate_left(n % 4);
        for arm in arms {
            let answer = match arm {
                "context" => Some(entry["answer"].clone()),
                "local" => None,
                "both" => Some(serde_json::json!({"confidence": "selected", "stale": false, "truncated": false,
                    "tests": [{"target": "", "ctest": both_names}], "also": []})),
                _ => {
                    // Every test the tools' own tree registers (configured by now, or by this check).
                    let listed = String::from_utf8_lossy(&sh(&format!(
                        "ctest --test-dir {} -N 2>/dev/null | sed -n 's/^ *Test *#[0-9]*: //p'",
                        crate::testsel::tree::own_tree(&root).display()
                    )).stdout).to_string();
                    let all: Vec<String> = listed.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                    if all.is_empty() {
                        // Not configured yet: run the local variant's check once to configure it.
                        *answer_slot.lock().unwrap() = None;
                        let _ = FinishCheckTool { cwd: root.clone(), native_exec: true, workflow: None }.check().await;
                    }
                    let listed = String::from_utf8_lossy(&sh(&format!(
                        "ctest --test-dir {} -N 2>/dev/null | sed -n 's/^ *Test *#[0-9]*: //p'",
                        crate::testsel::tree::own_tree(&root).display()
                    )).stdout).to_string();
                    let all: Vec<String> = listed.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                    Some(serde_json::json!({"confidence": "selected", "stale": false, "truncated": false,
                        "tests": [{"target": "", "ctest": all}], "also": []}))
                }
            };
            *answer_slot.lock().unwrap() = answer;
            sh(&format!("touch {}", changed.split_whitespace().collect::<Vec<_>>().join(" ")));
            if cold {
                // Cold: the tools' own tree configured but nothing built, as for the first check of a task
                // whose background build has not run.
                sh(&format!("cmake --build {} --target clean", crate::testsel::tree::own_tree(&root).display()));
            }
            // No workflow: the change is the work tree against HEAD, and nothing is remembered between
            // calls (a thread's memo would skip an unchanged change set).
            let tool = FinishCheckTool { cwd: root.clone(), native_exec: true, workflow: None };
            let started = std::time::Instant::now();
            let (text, result) = tool.check().await;
            let secs = started.elapsed().as_secs_f64();
            let line = |prefix: &str| {
                text.split(['\n', ';'])
                    .map(str::trim)
                    .find(|seg| seg.starts_with(prefix) || seg.contains(prefix))
                    .unwrap_or("")
                    .to_string()
            };
            writeln!(out, "{}", serde_json::json!({
                "repo": repo, "commit": commit, "arm": arm, "cold": cold, "secs": (secs * 10.0).round() / 10.0,
                "verdict": format!("{:?}", result.verdict),
                "selection": line("selection:"), "context_note": line("select_tests not used"),
                "build": line("build:"), "tests": line("tests:"),
                "head": text.chars().take(400).collect::<String>(),
                "selected": match arm {
                    "context" => context_names.iter().cloned().collect::<Vec<_>>(),
                    "local" => local_names.iter().cloned().collect::<Vec<_>>(),
                    "both" => both_names.clone(),
                    _ => registered.iter().cloned().collect::<Vec<_>>(),
                },
                "touched": touched.clone(),
                "hit": touched.iter().all(|t| match arm {
                    "context" => context_names.contains(t),
                    "local" => local_names.contains(t),
                    "both" => both_names.contains(t),
                    _ => registered.contains(t),
                }),
            })).unwrap();
        }
    }
}

/// A project whose `slow_test` takes 40 s to build (a generated source), beside `a_test`.
#[cfg(target_os = "linux")]
fn slow_target_project() -> Option<tempfile::TempDir> {
    two_target_project(
        "add_custom_command(OUTPUT slow_test.cpp COMMAND ${CMAKE_COMMAND} -E sleep 40 \
COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_SOURCE_DIR}/tests/b_test.cpp slow_test.cpp DEPENDS tests/b_test.cpp)\n\
add_executable(slow_test ${CMAKE_BINARY_DIR}/slow_test.cpp)\ntarget_link_libraries(slow_test libb)\n\
add_test(NAME slow_test COMMAND slow_test)\n",
    )
}

/// `tree_owner`: the warm build stops as soon as the agent builds in its tree itself, and a
/// check then waits for the agent's build instead of building beside it (two ninjas in one tree
/// corrupt its logs).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_owner_the_warm_build_yields_and_the_check_waits_for_the_agents_build() {
    use std::os::unix::process::CommandExt;
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = slow_target_project() else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let own = crate::testsel::tree::own_tree(&root);
    crate::testsel::tree::TEST_OWNER_TREES.lock().unwrap().push(own.clone());
    let tool = tool_for(&root).await;
    let warm = crate::start_warm_build(&root, &["slow_test".to_string()]);
    let configured = std::time::Instant::now();
    while !own.join("build.ninja").is_file() && !own.join("Makefile").is_file() {
        assert!(configured.elapsed() < Duration::from_secs(60), "the warm build configures its tree");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!warm.is_finished(), "slow_test takes 40 s to build");
    // The agent's own build in the tree (a process outside every build of ours).
    let mut agent = std::process::Command::new("sleep")
        .arg("15")
        .current_dir(&own)
        .process_group(0)
        .spawn()
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(20), warm.wait())
        .await
        .expect("the warm build stops within seconds");
    assert!(!outcome.ok, "{outcome:?}");
    assert!(outcome.summary.contains("another build started there"), "{outcome:?}");
    assert!(outcome.summary.contains("sleep 15"), "{outcome:?}");

    std::fs::write(root.join("src/a.cpp"), "#include \"a.h\"\nint a() { return 1; } // touched\n").unwrap();
    let (text, result) = tool.check().await;
    let _ = agent.kill();
    let _ = agent.wait();
    assert_eq!(result.verdict, Verdict::Pass, "{text}");
    assert!(text.contains("for a build already running in its tree (pid"), "{text}");
    assert!(text.contains("sleep 15"), "{text}");
    cleanup(&tool);
}

/// `tree_owner`: a check cut by its time limit runs again on the next call (the kept build goes
/// on) instead of answering from its memo; without it, an unchanged change gets the memo.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_owner_a_cut_check_runs_again() {
    if !have_cmake_toolchain() {
        eprintln!("skipped: cmake/ctest/c++ not available");
        return;
    }
    crate::base_run_cache::use_test_cache_dir();
    let Some(dir) = slow_target_project() else {
        eprintln!("skipped: git not available");
        return;
    };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    super::TEST_BUDGETS
        .lock()
        .unwrap()
        .push((root.clone(), Duration::from_secs(12)));
    let tool = tool_for(&root).await;
    std::fs::write(root.join("src/b.cpp"), "#include \"b.h\"\nint b() { return 2; } // touched\n").unwrap();
    let (first, _) = tool.check().await;
    assert!(first.contains("did not finish within the time limit"), "{first}");
    let started = std::time::Instant::now();
    let (memo, _) = tool.check().await;
    assert!(started.elapsed() < Duration::from_secs(5), "without tree_owner the memo answers: {memo}");
    crate::testsel::tree::TEST_OWNER_TREES
        .lock()
        .unwrap()
        .push(crate::testsel::tree::own_tree(&root));
    let started = std::time::Instant::now();
    let (again, _) = tool.check().await;
    assert!(started.elapsed() >= Duration::from_secs(8), "it ran again: {again}");
    assert!(again.contains("did not finish within the time limit"), "{again}");
    cleanup(&tool);
}
