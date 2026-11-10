//! Fixture projects (CMake with and without a File API reply, CMake < 3.14
//! style trees, Make, Meson) exercised through the tool's real entry point.
//! Tests that need a toolchain return early when it is missing.

use std::path::Path;
use std::process::Command;

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::session_head::test_repo;
use crate::test_support::call;

/// A GoogleTest-compatible subset (TEST, EXPECT_EQ, --gtest_filter, the
/// console format), so the fixtures need no installed framework.
const MINI_TEST_H: &str = r#"#pragma once
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>
namespace mini {
struct Case { const char* suite; const char* name; void (*fn)(); };
inline std::vector<Case>& cases() { static std::vector<Case> c; return c; }
inline bool& failed() { static bool f = false; return f; }
struct Reg { Reg(const char* s, const char* n, void (*f)()) { cases().push_back(Case{s, n, f}); } };
inline bool match(const char* p, const char* s) {
  if (*p == 0) return *s == 0;
  if (*p == '*') return match(p + 1, s) || (*s && match(p, s + 1));
  return *s && (*p == '?' || *p == *s) && match(p + 1, s + 1);
}
inline bool selected(const std::string& filter, const std::string& full) {
  if (filter.empty()) return true;
  size_t start = 0;
  while (start <= filter.size()) {
    size_t end = filter.find(':', start);
    if (end == std::string::npos) end = filter.size();
    if (match(filter.substr(start, end - start).c_str(), full.c_str())) return true;
    start = end + 1;
  }
  return false;
}
}  // namespace mini
#define TEST(s, n) static void s##_##n(); static mini::Reg s##_##n##_reg(#s, #n, s##_##n); static void s##_##n()
#define EXPECT_EQ(a, b) do { if (!((a) == (b))) { std::printf("%s:%d: Failure\nExpected equality of these values:\n  %s\n  %s\n", __FILE__, __LINE__, #a, #b); mini::failed() = true; } } while (0)
int main(int argc, char** argv) {
  std::string filter;
  for (int i = 1; i < argc; ++i) {
    if (std::strncmp(argv[i], "--gtest_filter=", 15) == 0) filter = argv[i] + 15;
    if (std::strcmp(argv[i], "--help") == 0) { std::printf("--gtest_filter=PATTERN\n"); return 0; }
  }
  int ran = 0, bad = 0;
  std::printf("[==========] Running tests.\n");
  for (const auto& c : mini::cases()) {
    std::string full = std::string(c.suite) + "." + c.name;
    if (!mini::selected(filter, full)) continue;
    ++ran;
    std::printf("[ RUN      ] %s\n", full.c_str());
    mini::failed() = false;
    c.fn();
    if (mini::failed()) { ++bad; std::printf("[  FAILED  ] %s (0 ms)\n", full.c_str()); }
    else std::printf("[       OK ] %s (0 ms)\n", full.c_str());
  }
  std::printf("[==========] %d tests ran.\n", ran);
  return bad ? 1 : 0;
}
"#;

const CORE_H: &str = "#pragma once\nint twice(int x);\nint add(int a, int b);\n";
const CORE_CPP: &str = "#include \"fx/core.h\"\nint twice(int x) { return 2 * x; }\nint add(int a, int b) { return a + b; }\n";
const UTIL_H: &str = "#pragma once\n#include \"fx/core.h\"\ninline int clampi(int x, int lo, int hi) { return x < lo ? lo : (x > hi ? hi : x); }\n";
const CORE_TEST: &str = "#include \"mini_test.h\"\n#include \"fx/core.h\"\n\nTEST(Core, Twice) {\n  EXPECT_EQ(twice(2), 4);\n}\n\nTEST(Core, Add) {\n  EXPECT_EQ(add(1, 2), 3);\n}\n";
const API_TEST: &str = "#include \"mini_test.h\"\n#include \"fx/util.h\"\n\nTEST(Api, Clamp) {\n  EXPECT_EQ(clampi(add(5, 5), 0, 8), 8);\n}\n";
const OTHER_TEST: &str = "#include \"mini_test.h\"\n\nTEST(Other, Truth) {\n  EXPECT_EQ(1, 1);\n}\n";

/// The fixture's CMakeLists.txt. `NAME`-style test registration and a static
/// library; one registered test runs a shell command (no program to map).
fn cmakelists() -> String {
    "cmake_minimum_required(VERSION 3.5)\nproject(fx CXX)\nenable_testing()\n\
add_library(core STATIC src/core.cpp)\ntarget_include_directories(core PUBLIC include)\n\
add_executable(core_test tests/core_test.cpp)\ntarget_include_directories(core_test PRIVATE tests)\ntarget_link_libraries(core_test core)\n\
add_executable(api_test tests/api_test.cpp)\ntarget_include_directories(api_test PRIVATE tests)\ntarget_link_libraries(api_test core)\n\
add_executable(other_test tests/other_test.cpp)\ntarget_include_directories(other_test PRIVATE tests include)\n\
add_test(NAME core_test COMMAND core_test)\nadd_test(NAME api_test COMMAND api_test)\nadd_test(NAME other_test COMMAND other_test)\n\
add_test(NAME script_check COMMAND sh -c \"exit 0\")\n"
        .to_string()
}

const MAKEFILE: &str = "CXX ?= c++\n\
all: build/core_test\n\
build/libcore.a: src/core.cpp include/fx/core.h\n\
\tmkdir -p build\n\
\t$(CXX) -Iinclude -c src/core.cpp -o build/core.o\n\
\tar rcs build/libcore.a build/core.o\n\
build/core_test: tests/core_test.cpp build/libcore.a\n\
\tmkdir -p build\n\
\t$(CXX) -Iinclude -Itests -c tests/core_test.cpp -o build/core_test.o\n\
\t$(CXX) -o build/core_test build/core_test.o build/libcore.a\n\
check: build/core_test\n\
\t./build/core_test\n\
.PHONY: all check\n";

fn have(tool: &str) -> bool {
    crate::tools::binary_on_path(tool)
}

fn cmake_at_least(min: (u32, u32)) -> bool {
    let Ok(out) = Command::new("cmake").arg("--version").output() else {
        return false;
    };
    crate::toolver::parse_version(&String::from_utf8_lossy(&out.stdout)).is_some_and(|v| v >= min)
}

fn write_fixture(root: &Path, build_file: (&str, &str)) {
    for (path, text) in [
        ("include/fx/core.h", CORE_H),
        ("include/fx/util.h", UTIL_H),
        ("src/core.cpp", CORE_CPP),
        ("tests/mini_test.h", MINI_TEST_H),
        ("tests/core_test.cpp", CORE_TEST),
        ("tests/api_test.cpp", API_TEST),
        ("tests/other_test.cpp", OTHER_TEST),
        (".gitignore", "build/\nbuild-*/\n"),
        build_file,
    ] {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), text).unwrap();
    }
}

fn setup() {
    crate::base_run_cache::use_test_cache_dir();
    // SAFETY: tests share one throwaway log path; no test reads it.
    unsafe {
        std::env::set_var(
            "BYTEASK_TOOL_LOG",
            std::env::temp_dir().join(format!("byteask-testsel-tool-log-{}.jsonl", std::process::id())),
        )
    };
}

fn tool(root: &Path) -> TestSelectTool {
    TestSelectTool {
        cwd: root.to_path_buf(),
        native_exec: true,
        head: SessionHead::spawn(root.to_path_buf()),
        jobs: Jobs::new(),
        finish_gate_suite: false,
    }
}

fn git_status(root: &Path) -> String {
    test_repo::git(root, &["status", "--porcelain", "--untracked-files=all"])
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn refuses_without_trust_and_answers_status() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let t = TestSelectTool {
        cwd: dir.path().to_path_buf(),
        native_exec: false,
        head: SessionHead::none(dir.path().to_path_buf()),
        jobs: Jobs::new(),
        finish_gate_suite: false,
    };
    let err = call(&t, json!({"action": "plan"})).await.unwrap_err().to_string();
    assert!(err.contains("BYTEASK_ALLOW_NATIVE_TOOLS"), "{err}");
    let text = call(&t, json!({"action": "status"})).await.unwrap();
    assert!(text.contains("No runs yet."), "{text}");
    let text = call(&t, json!({"action": "cancel"})).await.unwrap();
    assert!(text.contains("No run in progress."), "{text}");
    let err = call(&t, json!({"action": "bogus"})).await.unwrap_err().to_string();
    assert!(err.contains("unknown action"), "{err}");
}

#[tokio::test]
async fn no_build_system_is_a_did_not_run() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.c"), "int a;\n").unwrap();
    test_repo::init_and_commit(dir.path());
    let t = tool(dir.path());
    let text = call(&t, json!({"action": "plan"})).await.unwrap();
    assert!(text.contains("DID NOT RUN"), "{text}");
    assert!(text.contains("no CMake, Meson or Make build"), "{text}");
}

/// No existing build tree: the tool configures its own outside the project
/// (with a File API query), selects by build graph, symbol and case, builds
/// only what it runs, and compares with the session-start commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cmake_fixture_selects_builds_runs_and_compares() {
    if !have("cmake") || !have("c++") || !cmake_at_least((3, 14)) {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write_fixture(&root, ("CMakeLists.txt", &cmakelists()));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    let t = tool(&root);

    // Nothing changed: nothing selected.
    let text = call(&t, json!({"action": "plan"})).await.unwrap();
    assert!(text.contains("CMake"), "{text}");
    assert!(text.contains("the tool's own tree outside the project"), "{text}");
    assert!(text.contains("File API"), "{text}");
    assert!(text.contains("tier 3 = 0 of 4"), "{text}");

    // A bug in a library function.
    std::fs::write(root.join("src/core.cpp"), CORE_CPP.replace("return 2 * x;", "return x + x + 1;")).unwrap();
    let text = call(&t, json!({"action": "plan"})).await.unwrap();
    assert!(text.contains("symbols: twice"), "{text}");
    assert!(text.contains("tier 1 = 1 case(s) in 1 program(s)"), "{text}");
    assert!(text.contains("tier 2 = 1 test(s)"), "{text}");
    // core_test and api_test link core; script_check is unmapped.
    assert!(text.contains("tier 3 = 3 of 4 test(s), 1 of them unmapped"), "{text}");
    assert!(text.contains("core_test::Core.Twice"), "{text}");

    let text = call(&t, json!({"action": "run"})).await.unwrap();
    assert!(text.contains("BUILD core_test: OK"), "{text}");
    assert!(text.contains("/core_test --gtest_filter=Core.Twice"), "{text}");
    assert!(text.contains("DIFF: git diff --check"), "{text}");
    assert!(text.contains("RUN tier 1: 1 result(s)"), "{text}");
    assert!(text.contains("FAIL core_test::Core.Twice"), "{text}");
    assert!(text.contains("tests/core_test.cpp:5: Failure"), "{text}");
    assert!(text.contains("NEXT: tier=2"), "{text}");
    assert!(text.contains("full_suite=true once before finishing"), "{text}");
    // Where the finish gate runs the suite, the tool does not ask for a second full run.
    let gated = TestSelectTool {
        finish_gate_suite: true,
        ..tool(&root)
    };
    let plan = call(&gated, json!({"action": "plan"})).await.unwrap();
    assert!(plan.contains("runs in the finish gate when you finish"), "{plan}");
    assert!(!plan.contains("full_suite=true"), "{plan}");
    // Only core_test (and its library) were built.
    let build = scratch_dir(&root).join("build");
    assert!(!build.join("api_test").exists() && !build.join("other_test").exists());

    // Tier 3 builds what it runs; the unmapped script test falls back to every executable
    // target (stated), never a silent "everything".
    let text = call(&t, json!({"action": "run", "tier": 3})).await.unwrap();
    assert!(text.contains("BUILD core_test, api_test, other_test: OK"), "{text}");
    assert!(text.contains("fallback: the build model does not say which program script_check runs"), "{text}");
    assert!(text.contains("TESTS: ctest -R '^(core_test|api_test|script_check)$'"), "{text}");
    assert!(!text.contains("BUILD everything"), "{text}");
    assert!(text.contains("FAIL core_test"), "{text}");
    assert!(text.contains("2 passed"), "{text}");

    // The base comparison flips exactly the broken case.
    let text = call(&t, json!({"action": "run", "compare_base": true})).await.unwrap();
    assert!(text.contains("FLIPS vs base"), "{text}");
    assert!(text.contains("pass->fail 1 (core_test::Core.Twice)"), "{text}");
    // Cached: the second comparison runs nothing on base.
    let text = call(&t, json!({"action": "run", "compare_base": true})).await.unwrap();
    assert!(text.contains("0 run on base now, 1 from cache"), "{text}");

    // A header-only change reaches only what includes it.
    std::fs::write(root.join("src/core.cpp"), CORE_CPP).unwrap();
    std::fs::write(root.join("include/fx/util.h"), UTIL_H.replace("x < lo ? lo", "x <= lo ? lo")).unwrap();
    let text = call(&t, json!({"action": "plan"})).await.unwrap();
    assert!(text.contains("symbols: clampi"), "{text}");
    assert!(text.contains("tier 2: api_test"), "{text}");
    assert!(!text.contains("other_test"), "{text}");

    // The repository only shows the edit: every build went outside it.
    assert_eq!(git_status(&root), "M include/fx/util.h");
}

/// A build tree that existed before (Makefile generator, no File API reply):
/// not used by default (the tool configures its own with its options); named
/// with `build_dir`, it is mapped from DependInfo.cmake, built incrementally,
/// never reconfigured, and no query file is written into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_makefile_tree_is_read_not_reconfigured() {
    if !have("cmake") || !have("c++") || !have("make") {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write_fixture(&root, ("CMakeLists.txt", &cmakelists()));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    let build = root.join("build");
    let ok = Command::new("cmake")
        .args(["-G", "Unix Makefiles", ".."])
        .current_dir({
            std::fs::create_dir_all(&build).unwrap();
            &build
        })
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok);
    let cache_before = std::fs::metadata(build.join("CMakeCache.txt")).unwrap().modified().unwrap();
    std::fs::write(root.join("src/core.cpp"), CORE_CPP.replace("return a + b;", "return a - b;")).unwrap();
    let t = tool(&root);
    // By default the tool builds in its own tree, configured with this tree's options.
    let text = call(&t, json!({"action": "plan"})).await.unwrap();
    assert!(text.contains("the tool's own tree outside the project"), "{text}");
    assert!(text.contains("with the options of build"), "{text}");
    // Named, the existing tree is used as it is.
    let text = call(&t, json!({"action": "run", "tier": 2, "build_dir": "build"})).await.unwrap();
    assert!(text.contains("build tree build (existing: built incrementally, never reconfigured)"), "{text}");
    assert!(text.contains("DependInfo.cmake"), "{text}");
    assert!(text.contains("symbols: add"), "{text}");
    // core_test (Core.Add) and api_test (Api.Clamp uses add) both mention it.
    assert!(text.contains("tier 2 = 2 test(s)"), "{text}");
    assert!(text.contains("FAIL core_test"), "{text}");
    assert!(text.contains("FAIL api_test"), "{text}");
    assert!(!build.join(".cmake/api/v1/query/client-byteask").exists());
    let cache_after = std::fs::metadata(build.join("CMakeCache.txt")).unwrap().modified().unwrap();
    assert_eq!(cache_before, cache_after);
    // The same tree read as CMake < 3.14 would: CTestTestfile.cmake + DependInfo.
    let tests = cmake::ctest_testfile_tests(&build);
    let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["core_test", "api_test", "other_test", "script_check"]);
    let targets = cmake::dependinfo_targets(&build);
    let core_test = targets.iter().find(|t| t.name == "core_test").unwrap();
    assert!(core_test.is_exe);
    assert_eq!(core_test.deps, vec!["core".to_string()]);
    assert_eq!(core_test.sources, vec![root.join("tests/core_test.cpp")]);
}

/// A Ninja tree without a File API reply: the map comes from a configure-only
/// tree outside the project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_ninja_tree_is_mapped_from_a_scratch_configure() {
    if !have("cmake") || !have("c++") || !have("ninja") || !cmake_at_least((3, 14)) {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write_fixture(&root, ("CMakeLists.txt", &cmakelists()));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    let build = root.join("build-ninja");
    std::fs::create_dir_all(&build).unwrap();
    assert!(Command::new("cmake").args(["-G", "Ninja", ".."]).current_dir(&build).output().unwrap().status.success());
    assert!(Command::new("ninja").current_dir(&build).output().unwrap().status.success());
    std::fs::write(root.join("include/fx/core.h"), format!("{CORE_H}// touched\n")).unwrap();
    let t = tool(&root);
    let text = call(&t, json!({"action": "plan", "build_dir": "build-ninja"})).await.unwrap();
    assert!(text.contains("configure-only mapping tree"), "{text}");
    // core.h reaches everything that includes it: core, both tests using core.
    assert!(text.contains("tier 3 = 3 of 4"), "{text}");
    assert!(!build.join(".cmake/api/v1/query/client-byteask").exists());
}

/// A plain Makefile project: programs and sources from dry runs, cases run
/// directly with a filter, the suite through `make check`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_fixture_selects_and_runs() {
    if !have("make") || !have("c++") || !have("ar") {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write_fixture(&root, ("Makefile", MAKEFILE));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    std::fs::write(root.join("src/core.cpp"), CORE_CPP.replace("return 2 * x;", "return 3 * x;")).unwrap();
    let t = tool(&root);
    let text = call(&t, json!({"action": "run"})).await.unwrap();
    assert!(text.contains("Make, build tree"), "{text}");
    assert!(text.contains("tier 1 = 1 case(s) in 1 program(s)"), "{text}");
    assert!(text.contains("FAIL core_test::Core.Twice"), "{text}");
    let text = call(&t, json!({"action": "run", "tier": 3})).await.unwrap();
    assert!(text.contains("FAIL Core.Twice"), "{text}");
    assert!(text.contains("`make check`"), "{text}");
    assert_eq!(git_status(&root), "M src/core.cpp");
}

/// A Meson build directory (only when meson is installed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meson_fixture_selects_and_runs() {
    if !have("meson") || !have("ninja") || !have("c++") {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let meson_build = "project('fx', 'cpp')\ninc = include_directories('include', 'tests')\n\
core = static_library('core', 'src/core.cpp', include_directories: inc)\n\
core_test = executable('core_test', 'tests/core_test.cpp', include_directories: inc, link_with: core)\n\
api_test = executable('api_test', 'tests/api_test.cpp', include_directories: inc, link_with: core)\n\
other_test = executable('other_test', 'tests/other_test.cpp', include_directories: inc)\n\
test('core_test', core_test)\ntest('api_test', api_test)\ntest('other_test', other_test)\n";
    write_fixture(&root, ("meson.build", meson_build));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    let setup_ok = Command::new("meson").args(["setup", "build"]).current_dir(&root).output().unwrap().status.success();
    assert!(setup_ok);
    std::fs::write(root.join("src/core.cpp"), CORE_CPP.replace("return 2 * x;", "return 3 * x;")).unwrap();
    let t = tool(&root);
    let text = call(&t, json!({"action": "run"})).await.unwrap();
    assert!(text.contains("Meson"), "{text}");
    assert!(text.contains("tier 3 = 2 of 3"), "{text}");
    assert!(text.contains("FAIL core_test::Core.Twice"), "{text}");
}

/// A run longer than wait_s continues and is announced, not polled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_run_is_waited_for_and_status_lists_it() {
    if !have("make") || !have("c++") || !have("ar") {
        return;
    }
    setup();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write_fixture(&root, ("Makefile", MAKEFILE));
    test_repo::init_and_commit(&root);
    let _scratch = Scratch(scratch_dir(&root));
    std::fs::write(root.join("src/core.cpp"), CORE_CPP.replace("return 2 * x;", "return 3 * x;")).unwrap();
    let t = tool(&root);
    let text = call(&t, json!({"action": "run", "background": true})).await.unwrap();
    assert!(text.contains("continues in the") || text.contains("RUN tier 1"), "{text}");
    let text = call(&t, json!({"action": "status"})).await.unwrap();
    assert!(text.contains("run 1: tier 1"), "{text}");
    let text = call(&t, json!({"action": "wait", "wait_s": 300})).await.unwrap();
    assert!(text.contains("FAIL core_test::Core.Twice"), "{text}");
}
