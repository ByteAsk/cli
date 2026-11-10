//! The Cloud Agent run contract for `exec`: `BYTEASK_CHECKS_OUT` writes a checks.json (`mode:
//! agent`) that validates against the frozen schema, and SIGTERM stops the run with exit 124
//! after flushing `-o` and checks.json within 20 s.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use codex_login::CODEX_API_KEY_ENV_VAR;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;

const TEST_CMD: &str = "./run_tests.sh --junitxml=out/junit.xml";

fn validator() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ext/agent-checks/tests/contracts/validate.py")
}

fn assert_valid(file: &Path) {
    match Command::new("python3")
        .arg(validator())
        .arg("checks")
        .arg(file)
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "{} does not validate:\n{}\n{}",
            file.display(),
            String::from_utf8_lossy(&output.stdout),
            std::fs::read_to_string(file).unwrap_or_default()
        ),
        Err(err) => eprintln!("SKIPPED schema validation: python3 not runnable ({err})"),
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A git repo with a test script that writes a JUnit report (one pass, one failure).
fn init_repo(root: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    git(root, &["init", "-q"]);
    let script = root.join("run_tests.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nmkdir -p out\nprintf '<testsuite><testcase classname=\"Book\" name=\"add\"/>\
<testcase classname=\"Book\" name=\"cross\"><failure/></testcase></testsuite>' > out/junit.xml\n\
echo '1 of 2 failed'\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    git(root, &["rev-parse", "HEAD"])
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("file written")).expect("json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_out_records_the_agents_test_command() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let test = test_codex_exec();
    let head = init_repo(test.cwd_path());
    let out = tempfile::tempdir()?;
    let checks_path = out.path().join("checks.json");

    let server = start_mock_server().await;
    mount_response_sequence(
        &server,
        vec![
            sse_response(sse(vec![
                ev_response_created("r1"),
                ev_exec_command_call("c1", "ls -la"),
                ev_exec_command_call("c2", TEST_CMD),
                ev_completed("r1"),
            ])),
            sse_response(sse(vec![
                ev_response_created("r2"),
                ev_assistant_message("m1", "Tests ran: Book::cross fails."),
                ev_completed("r2"),
            ])),
        ],
    )
    .await;

    test.cmd_with_server(&server)
        .env("BYTEASK_CHECKS_OUT", &checks_path)
        .args(["--json", "-s", "danger-full-access", "run the tests"])
        .assert()
        .code(0);

    assert_valid(&checks_path);
    let checks = read_json(&checks_path);
    assert_eq!(checks["mode"], "agent");
    assert_eq!(checks["repo"]["base_sha"], head.as_str());
    assert_eq!(checks["repo"]["tree_sha"], Value::Null);
    let commands = checks["commands"].as_array().unwrap();
    assert_eq!(
        commands.len(),
        1,
        "only the test command is a check: {checks:#}"
    );
    assert_eq!(
        (
            &commands[0]["id"],
            &commands[0]["purpose"],
            &commands[0]["cmd"],
            &commands[0]["cwd"],
            &commands[0]["exit_code"],
            &commands[0]["junit"]
        ),
        (
            &Value::from("test"),
            &Value::from("test"),
            &Value::from(TEST_CMD),
            &Value::from("."),
            &Value::from(1),
            &serde_json::json!(["out/junit.xml"])
        )
    );
    assert_eq!(
        checks["tests"],
        serde_json::json!([
            {"id": "Book::add", "status": "pass", "command_id": "test"},
            {"id": "Book::cross", "status": "fail", "command_id": "test"}
        ])
    );
    assert_eq!(checks["summary"]["tests_failed"], 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_flushes_final_and_checks_and_exits_124() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let test = test_codex_exec();
    init_repo(test.cwd_path());
    let out = tempfile::tempdir()?;
    let checks_path = out.path().join("checks.json");
    let final_path = out.path().join("final.txt");

    let server = start_mock_server().await;
    mount_response_sequence(
        &server,
        vec![
            sse_response(sse(vec![
                ev_response_created("r1"),
                ev_assistant_message("m1", "Looking at the failing test."),
                ev_exec_command_call("c1", TEST_CMD),
                ev_completed("r1"),
            ])),
            // The model "thinks" for longer than the test will wait.
            sse_response(sse(vec![ev_completed("r2")])).set_delay(Duration::from_secs(300)),
        ],
    )
    .await;

    let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
        .current_dir(test.cwd_path())
        .env("CODEX_HOME", test.home_path())
        .env("CODEX_SQLITE_HOME", test.home_path())
        .env(CODEX_API_KEY_ENV_VAR, "dummy")
        .env("BYTEASK_CHECKS_OUT", &checks_path)
        .arg("-c")
        .arg(format!("openai_base_url=\"{}/v1\"", server.uri()))
        .args([
            "--json",
            "--skip-git-repo-check",
            "-s",
            "danger-full-access",
            "-o",
        ])
        .arg(&final_path)
        .arg("fix the failing test")
        .stdout(std::process::Stdio::null())
        .spawn()?;

    // Wait until the tool ran and the second model request is in flight.
    let waited = Instant::now();
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        if requests
            .iter()
            .filter(|r| r.url.path().ends_with("/responses"))
            .count()
            >= 2
        {
            break;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(60),
            "exec never sent the second request"
        );
        assert!(child.try_wait()?.is_none(), "exec exited before SIGTERM");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let pid = libc::pid_t::try_from(child.id())?;
    // SAFETY: sending a signal to our own child process.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let signalled = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if signalled.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("exec did not exit within 20 s of SIGTERM");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    eprintln!(
        "exec exited {:?} after {:?}",
        status.code(),
        signalled.elapsed()
    );
    assert_eq!(status.code(), Some(124));

    assert_eq!(
        std::fs::read_to_string(&final_path)?,
        "Looking at the failing test."
    );
    assert_valid(&checks_path);
    let checks = read_json(&checks_path);
    assert_eq!(checks["commands"][0]["cmd"], TEST_CMD);
    assert_eq!(checks["summary"]["tests_total"], 2);
    Ok(())
}
