//! `byteask-engine verify` through the real binary (the Cloud Agent run contract): exit 0 with a
//! checks.json that validates when it ran, exit 2 when it could not run. The verify logic
//! itself is tested in `codex-agent-checks`.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use tempfile::TempDir;

fn verify_command(home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex-byteask")?);
    cmd.env("CODEX_HOME", home).arg("verify");
    Ok(cmd)
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "git {args:?} failed");
    Ok(())
}

#[test]
fn verify_baseline_writes_valid_checks() -> Result<()> {
    let home = TempDir::new()?;
    let repo = TempDir::new()?;
    let root = repo.path();
    git(root, &["init", "-q"])?;
    std::fs::create_dir_all(root.join(".byteask"))?;
    std::fs::write(
        root.join(".byteask/environment.toml"),
        "version = 1\n[image]\nimage = \"debian:12\"\n[setup]\nrun = []\n\
         [build]\nrun = \"echo building\"\n\
         [test]\nrun = \"mkdir -p out && printf '<testsuite><testcase classname=\\\"A\\\" name=\\\"b\\\"/></testsuite>' > out/j.xml\"\n\
         junit = [\"out/j.xml\"]\n\
         [agent]\nnetwork = \"none\"\ncpus = 1\nmemory_gb = 2\n",
    )?;
    std::fs::write(root.join(".gitignore"), "out/\n")?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", "init"])?;

    let out_dir = TempDir::new()?;
    let out = out_dir.path().join("checks.json");
    verify_command(home.path())?
        .arg("-C")
        .arg(root)
        .arg("--out")
        .arg(&out)
        .args(["--timeout-s", "60"])
        .assert()
        .code(0);

    let checks: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out)?)?;
    assert_eq!(checks["mode"], "baseline");
    assert_eq!(checks["tests"][0]["id"], "A::b");
    let validator = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ext/agent-checks/tests/contracts/validate.py");
    match Command::new("python3")
        .arg(&validator)
        .arg("checks")
        .arg(&out)
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "checks.json does not validate: {}",
            String::from_utf8_lossy(&output.stdout)
        ),
        Err(err) => eprintln!("SKIPPED schema validation: python3 not runnable ({err})"),
    }
    Ok(())
}

#[test]
fn verify_exits_2_when_it_cannot_run() -> Result<()> {
    let home = TempDir::new()?;
    let not_a_repo = TempDir::new()?;
    let out_dir = TempDir::new()?;
    let out = out_dir.path().join("checks.json");
    verify_command(home.path())?
        .arg("-C")
        .arg(not_a_repo.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .code(2);

    let repo = TempDir::new()?;
    git(repo.path(), &["init", "-q"])?;
    git(
        repo.path(),
        &["commit", "-q", "--allow-empty", "-m", "init"],
    )?;
    // No recipe.
    verify_command(home.path())?
        .arg("-C")
        .arg(repo.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .code(2);
    // Unreadable --checks.
    verify_command(home.path())?
        .arg("-C")
        .arg(repo.path())
        .arg("--out")
        .arg(&out)
        .arg("--checks")
        .arg(out_dir.path().join("missing.json"))
        .assert()
        .code(2);
    assert!(!out.exists());
    Ok(())
}
