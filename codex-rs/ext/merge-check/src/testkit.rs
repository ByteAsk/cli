//! Test fixtures: a git repository with a protected base branch, a local "origin" that refuses
//! direct pushes to it, and an in-process stand-in for the enterprise server's runner-key
//! verifier (the real server is exercised by `enterprise_e2e` in `cli_tests.rs`).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine as _;
use ed25519_dalek::Verifier as _;

use crate::git::testing;
use crate::sign::Envelope;
use crate::sign::RunnerKey;
use crate::sign::RunnerKind;
use crate::verify::Answer;
use crate::verify::Verifier;
use crate::verify::VerifyFuture;

/// A profile whose build and tests are plain shell, so tests need no compiler: `build.sh`
/// exits with the status in `status.txt`; `test.sh` writes a JUnit report.
pub(crate) const PROFILE: &str = r#"schema = 1

[build]
run = ["bash build.sh"]
timeout_secs = 60

[test]
run = ["bash test.sh"]
junit = "junit.xml"
manifest = ".byteask/required-tests.txt"
timeout_secs = 60

[[slow]]
name = "asan"
run = ["echo slow check ran >> \"$BYTEASK_RESULTS_DIR/slow.log\"; test ! -e asan-fail"]

[files]
marked = ["src/hot/**"]

[merge]
reviewer_may_approve = ["no_code_change"]
perf = "required"
"#;

/// Appends a line to `calls` (outside the checkout) every time a build starts.
pub(crate) fn build_sh(calls: &Path) -> String {
    format!(
        "#!/bin/bash\necho building >> '{}'\n\
         test -e build-fails && {{ echo 'error: build broke' >&2; exit 2; }}\n\
         if [ -e uses_old_api ] && [ ! -e old_api ]; then echo 'error: old_api was removed' >&2; exit 3; fi\n\
         exit 0\n",
        calls.display()
    )
}

/// Writes one test case per line of `tests.txt` (`name status`), status pass|fail|skip.
pub(crate) const TEST_SH: &str = "#!/bin/bash\n\
    out=\"$BYTEASK_RESULTS_DIR/junit.xml\"\n\
    echo '<testsuite name=\"t\">' > \"$out\"\n\
    rc=0\n\
    while read -r name status; do\n\
      [ -z \"$name\" ] && continue\n\
      case \"$status\" in\n\
        pass) echo \"<testcase classname=\\\"t\\\" name=\\\"$name\\\"/>\" >> \"$out\";;\n\
        fail) echo \"<testcase classname=\\\"t\\\" name=\\\"$name\\\"><failure message=\\\"x\\\"/></testcase>\" >> \"$out\"; rc=1;;\n\
        skip) echo \"<testcase classname=\\\"t\\\" name=\\\"$name\\\"><skipped/></testcase>\" >> \"$out\";;\n\
      esac\n\
    done < tests.txt\n\
    echo '</testsuite>' >> \"$out\"\n\
    exit $rc\n";

/// A work repository whose `main` holds the profile, scripts and code.
pub(crate) struct Fixture {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) repo: PathBuf,
    /// One line per build the checks started.
    pub(crate) build_calls: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_profile(Some(PROFILE))
    }

    pub(crate) fn with_profile(profile: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let repo = dir.path().join("work");
        std::fs::create_dir(&repo).expect("mkdir");
        testing::init(&repo);
        if let Some(profile) = profile {
            testing::write(&repo, ".byteask/checks.toml", profile);
            testing::write(
                &repo,
                ".byteask/required-tests.txt",
                "# required tests\nbook_add\nbook_cross\n",
            );
        }
        let build_calls = dir.path().join("build-calls.log");
        testing::write(&repo, "build.sh", &build_sh(&build_calls));
        testing::write(&repo, "test.sh", TEST_SH);
        testing::write(
            &repo,
            "tests.txt",
            "book_add pass\nbook_cross pass\nextra pass\n",
        );
        testing::write(
            &repo,
            "src/book.cpp",
            "int add(int a, int b) { return a + b; }\n",
        );
        testing::write(&repo, "src/hot/feed.cpp", "int feed() { return 1; }\n");
        testing::write(&repo, "README.md", "# book\n");
        testing::write(&repo, "old_api", "an API a later base commit removes\n");
        testing::commit_all(&repo, "base");
        Self {
            dir,
            repo,
            build_calls,
        }
    }

    /// A branch off main with `edit` applied and committed; returns the head commit.
    pub(crate) fn branch(&self, name: &str, edit: impl FnOnce(&Path)) -> String {
        testing::run(&self.repo, &["checkout", "-q", "-B", name, "main"]);
        edit(&self.repo);
        let head = testing::commit_all(&self.repo, name);
        testing::run(&self.repo, &["checkout", "-q", "main"]);
        head
    }

    /// A commit on main (a base-branch change).
    pub(crate) fn advance_main(&self, edit: impl FnOnce(&Path)) -> String {
        testing::run(&self.repo, &["checkout", "-q", "main"]);
        edit(&self.repo);
        testing::commit_all(&self.repo, "main moves")
    }

    pub(crate) fn write(&self, path: &str, text: &str) {
        testing::write(&self.repo, path, text);
    }

    /// How many builds the checks started so far.
    pub(crate) fn builds(&self) -> usize {
        std::fs::read_to_string(&self.build_calls)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
}

/// One enrolled key in [`FakeServer`].
struct Enrolled {
    public: ed25519_dalek::VerifyingKey,
    kind: RunnerKind,
    retired_at: Option<u64>,
    revoked: bool,
}

/// The enterprise server's verify rules, in process: unknown key, bad signature, revoked (every signature
/// rejected), retired (valid only if recorded before retirement), kind returned.
#[derive(Default)]
pub(crate) struct FakeServer {
    keys: Mutex<HashMap<String, Enrolled>>,
    recorded: Mutex<HashMap<(String, String, u64), u64>>,
    clock: Mutex<u64>,
    pub(crate) down: Mutex<bool>,
}

impl FakeServer {
    pub(crate) fn enrol(&self, key: &RunnerKey, kind: RunnerKind) {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(key.public_b64())
            .expect("b64");
        let public =
            ed25519_dalek::VerifyingKey::from_bytes(&raw.try_into().expect("32")).expect("key");
        self.keys.lock().expect("lock").insert(
            key.key_id(),
            Enrolled {
                public,
                kind,
                retired_at: None,
                revoked: false,
            },
        );
    }

    pub(crate) fn tick(&self) -> u64 {
        let mut clock = self.clock.lock().expect("lock");
        *clock += 1;
        *clock
    }

    pub(crate) fn record(&self, envelope: &Envelope) {
        let now = self.tick();
        self.recorded.lock().expect("lock").insert(
            (
                envelope.key_id.clone(),
                envelope.digest(),
                envelope.signed_ms,
            ),
            now,
        );
    }

    pub(crate) fn retire(&self, key: &RunnerKey) {
        let now = self.tick();
        if let Some(entry) = self.keys.lock().expect("lock").get_mut(&key.key_id()) {
            entry.retired_at = Some(now);
        }
    }

    pub(crate) fn revoke(&self, key: &RunnerKey) {
        if let Some(entry) = self.keys.lock().expect("lock").get_mut(&key.key_id()) {
            entry.revoked = true;
        }
    }
}

impl FakeServer {
    fn signature_ok(entry: &Enrolled, digest: &str, signed_ms: u64, signature: &str) -> bool {
        base64::engine::general_purpose::STANDARD
            .decode(signature)
            .ok()
            .and_then(|bytes| ed25519_dalek::Signature::from_slice(&bytes).ok())
            .is_some_and(|signature| {
                entry
                    .public
                    .verify(&crate::sign::message(digest, signed_ms), &signature)
                    .is_ok()
            })
    }

    /// The enterprise server's `attest` for a digest the client computed.
    pub(crate) fn attest_digest(&self, envelope: &Envelope, digest: &str) -> Result<(), String> {
        {
            let keys = self.keys.lock().expect("lock");
            let entry = keys.get(&envelope.key_id).ok_or("unknown runner key")?;
            if entry.revoked || entry.retired_at.is_some() {
                return Err("this runner key is no longer active".to_string());
            }
            if !Self::signature_ok(entry, digest, envelope.signed_ms, &envelope.signature) {
                return Err("signature does not verify".to_string());
            }
        }
        let now = self.tick();
        self.recorded.lock().expect("lock").insert(
            (
                envelope.key_id.clone(),
                digest.to_string(),
                envelope.signed_ms,
            ),
            now,
        );
        Ok(())
    }

    /// The enterprise server's `verify` for a digest the client computed.
    pub(crate) async fn verify_digest(&self, envelope: &Envelope, digest: &str) -> Answer {
        let keys = self.keys.lock().expect("lock");
        let Some(entry) = keys.get(&envelope.key_id) else {
            return Answer {
                valid: false,
                reason: "signed by a key that is not enrolled".to_string(),
                kind: None,
            };
        };
        let kind = Some(entry.kind);
        let answer = |valid: bool, reason: &str| Answer {
            valid,
            reason: reason.to_string(),
            kind,
        };
        if !Self::signature_ok(entry, digest, envelope.signed_ms, &envelope.signature) {
            return answer(false, "signature does not verify");
        }
        if entry.revoked {
            return answer(false, "the runner key was revoked as compromised");
        }
        let id = (
            envelope.key_id.clone(),
            digest.to_string(),
            envelope.signed_ms,
        );
        let recorded = self.recorded.lock().expect("lock").get(&id).copied();
        if let Some(retired) = entry.retired_at {
            return match recorded {
                Some(at) if at < retired => answer(true, "recorded before the runner was retired"),
                _ => answer(
                    false,
                    "the runner was retired and this result was not recorded before that",
                ),
            };
        }
        drop(keys);
        if recorded.is_none() {
            let now = self.tick();
            self.recorded.lock().expect("lock").insert(id, now);
        }
        answer(true, "signed by an active enrolled runner")
    }
}

impl Verifier for FakeServer {
    fn verify<'a>(&'a self, envelope: &'a Envelope) -> VerifyFuture<'a> {
        Box::pin(async move {
            if *self.down.lock().expect("lock") {
                return Err("connection refused".to_string());
            }
            Ok(self.verify_digest(envelope, &envelope.digest()).await)
        })
    }
}
