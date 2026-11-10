//! The three CI commands end to end, the way a company's CI would run them: a local bare
//! repository whose `main` is protected (a pre-receive hook refuses direct pushes; only the
//! fake merge button moves it), a fake CI runner (separate clones for the run job and the
//! signing job), a fake enterprise server, and a fake GitHub. Nothing real is contacted.
//!
//! One more test runs the same flow against a real enterprise server; it is skipped unless
//! `BYTEASK_TEST_ENTERPRISE` names that server's `byteask/` directory.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use check_result::Verdict;
use clap::Parser as _;
use pretty_assertions::assert_eq;

use super::*;
use crate::gate::Decision;
use crate::git::testing;
use crate::platform::tests::FakeApi;
use crate::sign::RunnerKey;
use crate::testkit;
use crate::testkit::FakeServer;

#[derive(Debug, clap::Parser)]
struct TestCli {
    #[command(subcommand)]
    command: MergeCommand,
}

async fn cli(args: &[&str]) -> i32 {
    let parsed = TestCli::try_parse_from(std::iter::once("checks").chain(args.iter().copied()))
        .unwrap_or_else(|err| panic!("{args:?}: {err}"));
    run_command(parsed.command)
        .await
        .unwrap_or_else(|err| panic!("{args:?}: {err:#}"))
}

async fn cli_err(args: &[&str]) -> String {
    let parsed = TestCli::try_parse_from(std::iter::once("checks").chain(args.iter().copied()))
        .unwrap_or_else(|err| panic!("{args:?}: {err}"));
    format!(
        "{:#}",
        run_command(parsed.command).await.expect_err("should fail")
    )
}

fn secret_file(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// A fake enterprise server over HTTP, with the real server's verify and attest rules.
fn fake_enterprise(server: Arc<FakeServer>, token: &'static str) -> String {
    let http = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", http.server_addr().to_ip().expect("ip"));
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        for mut request in http.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let args: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let envelope = sign::Envelope {
                schema: sign::ENVELOPE_SCHEMA.into(),
                kind: RunnerKind::Ci,
                key_id: args["key_id"].as_str().unwrap_or_default().into(),
                signed_ms: args["signed_ms"].as_u64().unwrap_or_default(),
                signature: args["signature"].as_str().unwrap_or_default().into(),
                payload: String::new(),
            };
            // The fake checks the digest the client sent, like the real server.
            let digest = args["digest"].as_str().unwrap_or_default().to_string();
            let authorized = request.headers().iter().any(|h| {
                h.field.equiv("Authorization") && h.value.as_str() == format!("Bearer {token}")
            });
            let (status, answer) = match request.url() {
                "/enterprise/v1/runner-keys/verify" if !authorized => (401, serde_json::json!({})),
                "/enterprise/v1/runner-keys/verify" => {
                    let answer = runtime.block_on(server.verify_digest(&envelope, &digest));
                    (
                        200,
                        serde_json::json!({"valid": answer.valid, "reason": answer.reason, "kind": answer.kind}),
                    )
                }
                "/enterprise/v1/runner-keys/attest" => {
                    match server.attest_digest(&envelope, &digest) {
                        Ok(()) => (200, serde_json::json!({"ok": true, "attested_ms": 1})),
                        Err(reason) => (403, serde_json::json!({"ok": false, "reason": reason})),
                    }
                }
                _ => (404, serde_json::json!({})),
            };
            let _ = request.respond(
                tiny_http::Response::from_string(answer.to_string()).with_status_code(status),
            );
        }
    });
    base
}

struct Ci {
    dir: tempfile::TempDir,
    origin: PathBuf,
    dev: PathBuf,
    /// Holds the build counter file.
    fixture: testkit::Fixture,
}

impl Ci {
    /// `origin` (bare, `main` protected) seeded from a fixture repository.
    fn new(profile: Option<&str>) -> Self {
        let fixture = testkit::Fixture::with_profile(profile);
        let dir = tempfile::tempdir().expect("tmp");
        let origin = dir.path().join("origin.git");
        testing::run(
            dir.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &fixture.repo.to_string_lossy(),
                &origin.to_string_lossy(),
            ],
        );
        // Protected base branch: only the merge button may move main.
        let hook = origin.join("hooks/pre-receive");
        std::fs::write(
            &hook,
            "#!/bin/bash\nwhile read -r old new ref; do\n  if [ \"$ref\" = refs/heads/main ] && [ -z \"$BYTEASK_FAKE_MERGE_BUTTON\" ]; then\n    echo \"main is protected: changes land through the merge check\" >&2; exit 1\n  fi\ndone\n",
        )
        .expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let dev = dir.path().join("dev");
        testing::run(
            dir.path(),
            &[
                "clone",
                "-q",
                &origin.to_string_lossy(),
                &dev.to_string_lossy(),
            ],
        );
        Self {
            dir,
            origin,
            dev,
            fixture,
        }
    }

    /// The developer pushes a branch.
    fn push_branch(&self, name: &str, edit: impl FnOnce(&Path)) {
        testing::run(&self.dev, &["fetch", "-q", "origin"]);
        testing::run(&self.dev, &["checkout", "-q", "-B", name, "origin/main"]);
        edit(&self.dev);
        testing::commit_all(&self.dev, name);
        testing::run(&self.dev, &["push", "-q", "-f", "origin", name]);
    }

    /// A fresh CI checkout (each job gets its own).
    fn ci_clone(&self, job: &str) -> PathBuf {
        let path = self.dir.path().join(job);
        if !path.exists() {
            testing::run(
                self.dir.path(),
                &[
                    "clone",
                    "-q",
                    &self.origin.to_string_lossy(),
                    &path.to_string_lossy(),
                ],
            );
        }
        testing::run(&path, &["fetch", "-q", "origin"]);
        path
    }

    fn builds(&self) -> usize {
        self.fixture.builds()
    }
}

/// The whole CI flow: run (no key), sign (key, no branch code), gate (signed results only),
/// posted to a fake GitHub; then the merge button.
#[tokio::test]
async fn the_ci_flow_allows_pass_and_blocks_fail_and_no_profile() {
    let ci = Ci::new(Some(testkit::PROFILE));
    // The protected branch refuses a direct push.
    testing::write(&ci.dev, "src/book.cpp", "direct\n");
    testing::commit_all(&ci.dev, "direct");
    let refused = std::process::Command::new("git")
        .arg("-C")
        .arg(&ci.dev)
        .args(["push", "-q", "origin", "HEAD:main"])
        .output()
        .expect("git");
    assert!(!refused.status.success(), "main must be protected");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("main is protected"));
    testing::run(&ci.dev, &["reset", "-q", "--hard", "origin/main"]);

    let server = Arc::new(FakeServer::default());
    let key = RunnerKey::generate().expect("key");
    server.enrol(&key, RunnerKind::Ci);
    let enterprise = fake_enterprise(server.clone(), "svc-token");
    let secrets = ci.dir.path().join("secrets");
    std::fs::create_dir(&secrets).expect("mkdir");
    let key_file = secrets.join("runner.pem");
    secret_file(&key_file, &key.to_pem().expect("pem"));
    secret_file(&secrets.join("enterprise-token"), "svc-token");
    secret_file(&secrets.join("github-token"), "gh-token");
    let cache = ci.dir.path().join("cache");
    let github = FakeApi::start(vec![]);

    for (branch, edit, want_allowed, want_conclusion) in [
        ("good", "v2\n", true, "success"),
        ("bad", "BREAK", false, "failure"),
    ] {
        ci.push_branch(branch, |repo| {
            if edit == "BREAK" {
                testing::write(repo, "build-fails", "");
            } else {
                testing::write(repo, "src/book.cpp", edit);
            }
        });
        let head = format!("origin/{branch}");
        // Job 1: runs the branch's code; has no key.
        let run_repo = ci.ci_clone("run-job");
        let report = ci.dir.path().join(format!("{branch}-run.json"));
        let run_repo_s = run_repo.to_string_lossy().into_owned();
        assert_eq!(
            cli(&[
                "run",
                "--repo",
                &run_repo_s,
                "--base",
                "origin/main",
                "--head",
                &head,
                "--out",
                &report.to_string_lossy()
            ])
            .await,
            0
        );
        // Job 2: signs; never runs branch code.
        let sign_repo = ci.ci_clone("sign-job");
        assert_eq!(
            cli(&[
                "sign",
                "--repo",
                &sign_repo.to_string_lossy(),
                "--base",
                "origin/main",
                "--head",
                &head,
                "--key",
                &key_file.to_string_lossy(),
                "--report",
                &report.to_string_lossy(),
                "--enterprise",
                &enterprise,
                "--cache",
                &cache.to_string_lossy(),
            ])
            .await,
            0
        );
        // Job 3: the merge status check.
        let gate_repo = ci.ci_clone("gate-job");
        let decision_file = ci.dir.path().join(format!("{branch}-decision.json"));
        let status = cli(&[
            "gate",
            "--repo",
            &gate_repo.to_string_lossy(),
            "--base",
            "origin/main",
            "--head",
            &head,
            "--cache",
            &cache.to_string_lossy(),
            "--enterprise",
            &enterprise,
            "--token-file",
            &secrets.join("enterprise-token").to_string_lossy(),
            "--github-repo",
            "acme/book",
            "--github-api",
            &github.base,
            "--github-token-file",
            &secrets.join("github-token").to_string_lossy(),
            "--json",
            &decision_file.to_string_lossy(),
            "--jenkins-out",
            &ci.dir
                .path()
                .join(format!("{branch}-jenkins"))
                .to_string_lossy(),
        ])
        .await;
        assert_eq!(status, if want_allowed { 0 } else { 1 }, "{branch}");
        let posted = github.posts();
        let last = posted.last().expect("a check run");
        assert_eq!(last.1["conclusion"], want_conclusion, "{branch}");
        let head_commit = testing::run(&gate_repo, &["rev-parse", &head]);
        assert_eq!(last.1["head_sha"], head_commit);
        // The fake merge button: only an allowed decision lands.
        if status == 0 {
            let decision: Decision =
                serde_json::from_str(&std::fs::read_to_string(&decision_file).unwrap()).unwrap();
            assert_eq!(decision.verdict, Verdict::Pass);
            let merged = testing::run(&ci.dev, &["rev-parse", &head]);
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&ci.origin)
                .args(["update-ref", "refs/heads/main", &merged])
                .env("BYTEASK_FAKE_MERGE_BUTTON", "1")
                .output()
                .expect("git");
            assert!(output.status.success());
        }
    }
    assert_eq!(
        ci.builds(),
        2,
        "one build per candidate, in the run job only"
    );

    // Signing refuses a report for another candidate (e.g. an old base).
    let stale = ci.dir.path().join("bad-run.json");
    let sign_repo = ci.ci_clone("sign-job");
    let err = cli_err(&[
        "sign",
        "--repo",
        &sign_repo.to_string_lossy(),
        "--base",
        "origin/main",
        "--head",
        "origin/good",
        "--key",
        &key_file.to_string_lossy(),
        "--report",
        &stale.to_string_lossy(),
    ])
    .await;
    assert!(err.contains("another merge candidate"), "{err}");
}

/// A repository with no profile shows NOT RUN (no build profile) at merge, and blocks.
#[tokio::test]
async fn no_profile_is_not_run_no_build_profile_at_merge() {
    let ci = Ci::new(None);
    ci.push_branch("f", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let server = Arc::new(FakeServer::default());
    let key = RunnerKey::generate().expect("key");
    server.enrol(&key, RunnerKind::Ci);
    let enterprise = fake_enterprise(server, "t");
    let secrets = ci.dir.path().join("s");
    std::fs::create_dir(&secrets).unwrap();
    secret_file(&secrets.join("key.pem"), &key.to_pem().unwrap());
    secret_file(&secrets.join("tok"), "t");
    let repo = ci.ci_clone("job");
    let repo_s = repo.to_string_lossy().into_owned();
    let report = ci.dir.path().join("run.json");
    let cache = ci.dir.path().join("cache");
    assert_eq!(
        cli(&[
            "run",
            "--repo",
            &repo_s,
            "--base",
            "origin/main",
            "--head",
            "origin/f",
            "--out",
            &report.to_string_lossy()
        ])
        .await,
        0
    );
    assert_eq!(
        cli(&[
            "sign",
            "--repo",
            &repo_s,
            "--base",
            "origin/main",
            "--head",
            "origin/f",
            "--key",
            &secrets.join("key.pem").to_string_lossy(),
            "--report",
            &report.to_string_lossy(),
            "--enterprise",
            &enterprise,
            "--cache",
            &cache.to_string_lossy()
        ])
        .await,
        0
    );
    let jenkins = ci.dir.path().join("jenkins");
    let status = cli(&[
        "gate",
        "--repo",
        &repo_s,
        "--base",
        "origin/main",
        "--head",
        "origin/f",
        "--cache",
        &cache.to_string_lossy(),
        "--enterprise",
        &enterprise,
        "--token-file",
        &secrets.join("tok").to_string_lossy(),
        "--jenkins-out",
        &jenkins.to_string_lossy(),
    ])
    .await;
    assert_eq!(status, 1);
    let summary = std::fs::read_to_string(jenkins.join("byteask-merge-check.md")).unwrap();
    assert!(summary.contains("NOT RUN (no build profile)"), "{summary}");
    assert_eq!(ci.builds(), 0);
}

#[tokio::test]
async fn secrets_must_not_be_readable_by_others_and_never_come_from_argv() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tok");
    std::fs::write(&path, "x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_secret(Some(&path), "UNSET_FOR_TEST")
                .unwrap_err()
                .to_string()
                .contains("chmod 600")
        );
    }
    // There is no flag that takes a token value.
    assert!(
        TestCli::try_parse_from([
            "checks",
            "gate",
            "--base",
            "m",
            "--enterprise",
            "x",
            "--token",
            "t"
        ])
        .is_err()
    );
}

#[tokio::test]
async fn keygen_and_profile_commands() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("k.pem");
    assert_eq!(cli(&["keygen", "--out", &key.to_string_lossy()]).await, 0);
    assert!(RunnerKey::load(&key).is_ok());
    assert!(
        cli_err(&["keygen", "--out", &key.to_string_lossy()])
            .await
            .contains("could not create")
    );
    let fixture = testkit::Fixture::new();
    let repo = fixture.repo.to_string_lossy().into_owned();
    assert_eq!(
        cli(&["profile", "check", "--repo", &repo, "--rev", "main"]).await,
        0
    );
    fixture.write(".byteask/checks.toml", "schema = 1\n[build]\nrun = []\n");
    assert_eq!(cli(&["profile", "check", "--repo", &repo]).await, 1);
    assert_eq!(cli(&["profile", "propose", "--repo", &repo]).await, 0);
}

/// The same flow against a real enterprise server (Python), started from its `byteask/`
/// directory: enrolment, attest, verify, retirement keeps recorded results, revocation drops
/// all, an unenrolled key never counts.
#[tokio::test]
async fn enterprise_e2e_against_real_server() {
    let Some(byteask_dir) = std::env::var_os("BYTEASK_TEST_ENTERPRISE").map(PathBuf::from) else {
        eprintln!(
            "skipped: set BYTEASK_TEST_ENTERPRISE to the byteask/ directory of a checkout of the enterprise server"
        );
        return;
    };
    let server = real_server::Server::start(&byteask_dir);
    let ci_key = RunnerKey::generate().unwrap();
    let bench_key = RunnerKey::generate().unwrap();
    let stranger = RunnerKey::generate().unwrap();
    server.enrol(&ci_key, "ci");
    server.enrol(&bench_key, "bench");
    let verifier = verify::EnterpriseVerifier::new(&server.base, &server.service).unwrap();

    let fixture = testkit::Fixture::new();
    let head = fixture.branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let report = run::run_checks(&run::RunOptions {
        repo: fixture.repo.clone(),
        base: "main".into(),
        head: head.clone(),
        work_dir: None,
    })
    .await
    .unwrap();
    let payload = serde_json::to_string(&report).unwrap();
    let candidate = crate::binding::Candidate::compute(&fixture.repo, "main", &head).unwrap();
    // Signed by the enrolled CI key and recorded: counts.
    let good = ci_key.sign(RunnerKind::Ci, payload.clone(), run::now_ms());
    verify::attest(&server.base, &good).await.expect("attest");
    let decision = decide_with(&verifier, &candidate, vec![good.clone()]).await;
    assert!(decision.allowed, "{decision:?}");

    // Unenrolled key: never counts.
    let foreign = stranger.sign(RunnerKind::Ci, payload.clone(), run::now_ms());
    let decision = decide_with(&verifier, &candidate, vec![foreign]).await;
    assert!(!decision.allowed);
    assert!(
        decision.ignored[0].contains("not enrolled"),
        "{:?}",
        decision.ignored
    );

    // A bench key cannot vouch for CI results.
    let wrong_kind = bench_key.sign(RunnerKind::Ci, payload.clone(), run::now_ms());
    let decision = decide_with(&verifier, &candidate, vec![wrong_kind]).await;
    assert!(!decision.allowed, "{:?}", decision.ignored);

    // Retirement (re-imaged runner): what it recorded before stays valid ...
    server.end(&ci_key, "retire");
    let decision = decide_with(&verifier, &candidate, vec![good.clone()]).await;
    assert!(
        decision.allowed,
        "recorded before retirement: {:?}",
        decision.ignored
    );
    // ... and it cannot sign anything new.
    let late = ci_key.sign(RunnerKind::Ci, payload.clone(), run::now_ms());
    assert!(verify::attest(&server.base, &late).await.is_err());
    assert!(!decide_with(&verifier, &candidate, vec![late]).await.allowed);

    // Compromise revocation: every signature by that key is rejected, even recorded ones.
    let second = RunnerKey::generate().unwrap();
    server.enrol(&second, "ci");
    let recorded = second.sign(RunnerKind::Ci, payload.clone(), run::now_ms());
    verify::attest(&server.base, &recorded)
        .await
        .expect("attest");
    assert!(
        decide_with(&verifier, &candidate, vec![recorded.clone()])
            .await
            .allowed
    );
    server.end(&second, "revoke");
    let decision = decide_with(&verifier, &candidate, vec![recorded]).await;
    assert!(!decision.allowed);
    assert!(
        decision.ignored[0].contains("revoked"),
        "{:?}",
        decision.ignored
    );
}

async fn decide_with(
    verifier: &verify::EnterpriseVerifier,
    candidate: &crate::binding::Candidate,
    envelopes: Vec<sign::Envelope>,
) -> Decision {
    let named = envelopes
        .into_iter()
        .enumerate()
        .map(|(i, e)| (format!("e{i}"), Ok(e)))
        .collect();
    let evidence = gate::sort_evidence(named, verifier, &candidate.binding).await;
    gate::decide(candidate, &evidence, &[])
}

mod real_server {
    //! Starts the enterprise server (`python3 -m enterprise serve`) on a free loopback
    //! port with a throwaway data directory, and talks to its admin API.

    use std::path::Path;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;

    use crate::sign::RunnerKey;

    pub(super) struct Server {
        pub(super) base: String,
        pub(super) admin: String,
        pub(super) service: String,
        child: Child,
        _dir: tempfile::TempDir,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn post(url: &str, token: &str, body: serde_json::Value) -> serde_json::Value {
        let out = Command::new("curl")
            .args([
                "-sS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-H",
            ])
            .arg(format!("Authorization: Bearer {token}"))
            .args(["--data", &body.to_string(), url])
            .output()
            .expect("curl");
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{url}: {}", String::from_utf8_lossy(&out.stdout)))
    }

    impl Server {
        pub(super) fn start(byteask_dir: &Path) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let port = free_port();
            let config = dir.path().join("enterprise.toml");
            std::fs::write(
                &config,
                format!(
                    "[server]\nlisten = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\ndata_dir = \"{}\"\n",
                    dir.path().join("data").display()
                ),
            )
            .unwrap();
            let env = |cmd: &mut Command| {
                cmd.current_dir(byteask_dir)
                    .env("BYTEASK_LICENCE_FILE", dir.path().join("licence.json"))
                    .env("BYTEASK_LICENCE_STATE", dir.path().join("licence-state"))
                    .env_remove("BYTEASK_USAGE_DB")
                    .env_remove("BYTEASK_CONTENT_DB");
            };
            let mut boot = Command::new("python3");
            boot.args(["-m", "enterprise", "admin-bootstrap", "--config"])
                .arg(&config);
            env(&mut boot);
            let out = boot.output().expect("python3");
            assert!(
                out.status.success(),
                "bootstrap: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let admin = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            let mut serve = Command::new("python3");
            serve
                .args(["-m", "enterprise", "serve", "--config"])
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            env(&mut serve);
            let child = serve.spawn().expect("serve");
            let base = format!("http://127.0.0.1:{port}");
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let token = post(
                &format!("{base}/enterprise/v1/admin/tokens"),
                &admin,
                serde_json::json!({"kind": "service", "name": "merge-check"}),
            );
            let service = token["token"]
                .as_str()
                .unwrap_or_else(|| panic!("token: {token}"))
                .to_string();
            Self {
                base,
                admin,
                service,
                child,
                _dir: dir,
            }
        }

        pub(super) fn enrol(&self, key: &RunnerKey, kind: &str) {
            let out = post(
                &format!("{}/enterprise/v1/admin/runner-keys", self.base),
                &self.admin,
                serde_json::json!({"public_key": key.public_b64(), "kind": kind, "name": format!("{kind} runner")}),
            );
            assert_eq!(out["key_id"].as_str(), Some(key.key_id().as_str()), "{out}");
        }

        pub(super) fn end(&self, key: &RunnerKey, how: &str) {
            let out = post(
                &format!(
                    "{}/enterprise/v1/admin/runner-keys/{}/{how}",
                    self.base,
                    key.key_id()
                ),
                &self.admin,
                serde_json::json!({"reason": how}),
            );
            assert!(out["state"].as_str().is_some(), "{out}");
        }
    }
}
