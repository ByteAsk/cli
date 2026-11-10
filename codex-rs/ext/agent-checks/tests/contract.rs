//! Every checks.json `verify` writes validates against the
//! frozen `checks.schema.json` with the contracts' own `validate.py`, and `verify --checks`
//! keeps the input's command ids and order.
//!
//! `tests/contracts/` holds a copy of the contracts' `validate.py` and `schemas/{checks,
//! common}` (v1); set `BYTEASK_CONTRACTS_DIR` to check
//! against another contracts directory instead.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use codex_agent_checks::Checks;
use codex_agent_checks::Mode;
use codex_agent_checks::TestStatus;
use codex_agent_checks::VerifyCli;
use codex_agent_checks::run_verify;
use pretty_assertions::assert_eq;

fn contracts_dir() -> PathBuf {
    std::env::var_os("BYTEASK_CONTRACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/contracts"))
}

/// `python3 validate.py checks FILE` must print nothing and exit 0.
fn assert_valid(file: &Path) {
    let validator = contracts_dir().join("validate.py");
    let output = match Command::new("python3")
        .arg(&validator)
        .arg("checks")
        .arg(file)
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!(
                "SKIPPED schema validation of {}: python3 not runnable ({err})",
                file.display()
            );
            return;
        }
    };
    assert!(
        output.status.success(),
        "{} does not validate:\n{}{}\n{}",
        file.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(file).unwrap_or_default()
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A repo whose "build" is a shell script and whose "tests" write a JUnit report with one
/// pass, one failure and one skip, then exit 1.
fn script_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    write_exec(
        &root.join("build.sh"),
        "#!/bin/sh\nmkdir -p out\necho built > out/app\necho 'build done'\n",
    );
    write_exec(
        &root.join("run_tests.sh"),
        r#"#!/bin/sh
mkdir -p out
cat > out/junit.xml <<'EOF'
<testsuites><testsuite name="Book">
<testcase classname="BookTest" name="add"/>
<testcase classname="BookTest" name="cross"><failure message="boom"/></testcase>
<testcase classname="FeedTest" name="slow"><skipped/></testcase>
</testsuite></testsuites>
EOF
echo '1 of 3 tests failed'
exit 1
"#,
    );
    std::fs::create_dir_all(root.join(".byteask")).unwrap();
    std::fs::write(
        root.join(".byteask/environment.toml"),
        r#"version = 1
[image]
image = "ghcr.io/acme/ci-toolchain:2026-10"
[setup]
run = []
[build]
run = "./build.sh"
[test]
run = "./run_tests.sh"
junit = ["out/*.xml"]
[agent]
network = "none"
cpus = 2
memory_gb = 8
"#,
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    dir
}

fn cli(repo: &Path, out: &Path, checks: Option<&Path>) -> VerifyCli {
    VerifyCli {
        repo: repo.to_path_buf(),
        out: out.to_path_buf(),
        checks: checks.map(Path::to_path_buf),
        env_spec: PathBuf::from(".byteask/environment.toml"),
        timeout_s: Some(120),
    }
}

fn read(path: &Path) -> Checks {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn tests_of(checks: &Checks) -> Vec<(&str, TestStatus, &str)> {
    checks
        .tests
        .iter()
        .map(|t| (t.id.as_str(), t.status, t.command_id.as_str()))
        .collect()
}

#[test]
fn baseline_then_verify_validate_and_keep_ids() {
    let repo = script_repo();
    let root = repo.path();
    let out_dir = tempfile::tempdir().unwrap();
    let baseline_path = out_dir.path().join("baseline.json");

    assert_eq!(run_verify(cli(root, &baseline_path, None)), 0);
    assert_valid(&baseline_path);
    let baseline = read(&baseline_path);
    let head = git(root, &["rev-parse", "HEAD"]);
    assert_eq!(baseline.mode, Mode::Baseline);
    assert_eq!(baseline.repo.base_sha, head);
    // Baseline: index == HEAD, so tree_sha is base_sha's tree.
    assert_eq!(
        baseline.repo.tree_sha.as_deref(),
        Some(git(root, &["rev-parse", "HEAD^{tree}"]).as_str())
    );
    let ids: Vec<(&str, Option<i32>, Option<Vec<String>>)> = baseline
        .commands
        .iter()
        .map(|c| (c.id.as_str(), c.exit_code, c.junit.clone()))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("build", Some(0), None),
            ("test", Some(1), Some(vec!["out/junit.xml".to_string()])),
        ]
    );
    assert_eq!(
        tests_of(&baseline),
        vec![
            ("BookTest::add", TestStatus::Pass, "test"),
            ("BookTest::cross", TestStatus::Fail, "test"),
            ("FeedTest::slow", TestStatus::Skip, "test"),
        ]
    );
    assert!(baseline.summary.build_ok);
    assert!(
        baseline.commands[1]
            .log_tail
            .contains("1 of 3 tests failed")
    );

    // The "agent" fixes the failing test; the runner stages the patch (`git apply --index`)
    // and verify re-runs the baseline's commands.
    write_exec(
        &root.join("run_tests.sh"),
        "#!/bin/sh\nmkdir -p out\nprintf '<testsuite><testcase classname=\"BookTest\" name=\"add\"/><testcase classname=\"BookTest\" name=\"cross\"/><testcase classname=\"FeedTest\" name=\"slow\"><skipped/></testcase></testsuite>' > out/junit.xml\n",
    );
    git(root, &["add", "run_tests.sh"]);
    // Untracked, unignored setup output must not change the tree.
    std::fs::write(root.join("setup.log"), "deps fetched\n").unwrap();
    let staged_tree = git(root, &["write-tree"]);
    let index_before = git(root, &["diff", "--cached", "--name-only"]);
    let verify_path = out_dir.path().join("verify.json");
    assert_eq!(run_verify(cli(root, &verify_path, Some(&baseline_path))), 0);
    assert_valid(&verify_path);
    let verified = read(&verify_path);
    assert_eq!(verified.mode, Mode::Verify);
    assert_eq!(
        verified
            .commands
            .iter()
            .map(|c| (&c.id, &c.purpose, &c.cmd, &c.cwd))
            .collect::<Vec<_>>(),
        baseline
            .commands
            .iter()
            .map(|c| (&c.id, &c.purpose, &c.cmd, &c.cwd))
            .collect::<Vec<_>>()
    );
    assert_eq!(verified.summary.tests_failed, 0);
    // tree_sha is the staged index's tree, and verify left the index alone.
    assert_eq!(
        verified.repo.tree_sha.as_deref(),
        Some(staged_tree.as_str())
    );
    assert_ne!(verified.repo.tree_sha, baseline.repo.tree_sha);
    assert_eq!(
        git(root, &["diff", "--cached", "--name-only"]),
        index_before
    );
    assert_eq!(git(root, &["write-tree"]), staged_tree);
}

/// An agent-mode file (ids `configure`, `test-2`, ...; out-of-order purposes) re-runs with the
/// same ids in the same order; a test command whose report is not rewritten reports its
/// earlier tests as errors instead of dropping them.
#[test]
fn verify_keeps_agent_ids_and_order() {
    let repo = script_repo();
    let root = repo.path();
    let head = git(root, &["rev-parse", "HEAD"]);
    let input = serde_json::json!({
        "version": 1,
        "mode": "agent",
        "generated_by": {"tool": "byteask-engine", "version": "0.2.0"},
        "repo": {"base_sha": head, "tree_sha": null},
        "commands": [
            {"id": "test-2", "purpose": "test", "cmd": "./run_tests.sh", "cwd": ".", "exit_code": 1,
             "timed_out": false, "duration_s": 0.1, "log_tail": "", "junit": ["out/junit.xml"]},
            {"id": "build", "purpose": "build", "cmd": "./build.sh", "cwd": ".", "exit_code": 0,
             "timed_out": false, "duration_s": 0.1, "log_tail": ""},
            {"id": "test", "purpose": "test", "cmd": "echo 'no report this time'", "cwd": "out", "exit_code": 0,
             "timed_out": false, "duration_s": 0.1, "log_tail": "", "junit": ["out/other.xml"]}
        ],
        "tests": [
            {"id": "Old::case", "status": "pass", "command_id": "test"}
        ],
        "summary": {"build_ok": true, "tests_total": 1, "tests_failed": 0, "tests_skipped": 0}
    });
    let dir = tempfile::tempdir().unwrap();
    let input_path = dir.path().join("agent.json");
    std::fs::write(&input_path, serde_json::to_vec(&input).unwrap()).unwrap();
    assert_valid(&input_path);
    std::fs::create_dir_all(root.join("out")).unwrap();

    let out = dir.path().join("verify.json");
    assert_eq!(run_verify(cli(root, &out, Some(&input_path))), 0);
    assert_valid(&out);
    let verified = read(&out);
    assert_eq!(
        verified
            .commands
            .iter()
            .map(|c| (c.id.as_str(), c.cwd.as_str(), c.exit_code))
            .collect::<Vec<_>>(),
        vec![
            ("test-2", ".", Some(1)),
            ("build", ".", Some(0)),
            ("test", "out", Some(0))
        ]
    );
    assert_eq!(
        tests_of(&verified),
        vec![
            ("BookTest::add", TestStatus::Pass, "test-2"),
            ("BookTest::cross", TestStatus::Fail, "test-2"),
            ("FeedTest::slow", TestStatus::Skip, "test-2"),
            ("Old::case", TestStatus::Error, "test"),
        ]
    );
    assert_eq!(verified.summary.tests_failed, 2);
}

#[test]
fn exit_2_when_it_cannot_run() {
    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("checks.json");

    // Not a git repository.
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(run_verify(cli(plain.path(), &out, None)), 2);

    // No recipe.
    let repo = script_repo();
    let root = repo.path();
    let mut no_recipe = cli(root, &out, None);
    no_recipe.env_spec = PathBuf::from("missing/environment.toml");
    assert_eq!(run_verify(no_recipe), 2);

    // A recipe without a test command.
    std::fs::write(
        root.join("partial.toml"),
        "version = 1\n[build]\nrun = \"true\"\n",
    )
    .unwrap();
    let mut partial = cli(root, &out, None);
    partial.env_spec = root.join("partial.toml");
    assert_eq!(run_verify(partial), 2);

    // Unreadable and malformed --checks.
    assert_eq!(
        run_verify(cli(root, &out, Some(&out_dir.path().join("nope.json")))),
        2
    );
    let bad = out_dir.path().join("bad.json");
    std::fs::write(&bad, "{\"version\": 1}").unwrap();
    assert_eq!(run_verify(cli(root, &out, Some(&bad))), 2);

    assert!(!out.exists(), "no checks.json when verify could not run");
}

/// The same flow with a real CMake + CTest project: the report path comes from
/// `ctest --output-junit` (no `test.junit` in the recipe).
#[test]
fn cmake_ctest_baseline_validates() {
    let tools = ["cmake", "ctest"];
    if tools
        .iter()
        .any(|t| Command::new(t).arg("--version").output().is_err())
    {
        eprintln!("SKIPPED cmake_ctest_baseline_validates: cmake/ctest not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(tiny NONE)\nenable_testing()\n\
         add_test(NAME passes COMMAND true)\nadd_test(NAME fails COMMAND false)\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".byteask")).unwrap();
    std::fs::write(
        root.join(".byteask/environment.toml"),
        "version = 1\n[image]\nimage = \"debian:12\"\n[setup]\nrun = []\n\
         [build]\nrun = \"cmake -S . -B build && cmake --build build\"\n\
         [test]\nrun = \"ctest --test-dir build --output-junit junit.xml\"\n\
         [agent]\nnetwork = \"none\"\ncpus = 1\nmemory_gb = 2\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("checks.json");
    assert_eq!(run_verify(cli(root, &out, None)), 0);
    assert_valid(&out);
    let checks = read(&out);
    assert_eq!(
        checks.commands[1].junit,
        Some(vec!["build/junit.xml".to_string()])
    );
    let mut tests = tests_of(&checks);
    tests.sort_by_key(|t| t.0);
    assert_eq!(
        tests,
        vec![
            ("fails::fails", TestStatus::Fail, "test"),
            ("passes::passes", TestStatus::Pass, "test")
        ]
    );
}
