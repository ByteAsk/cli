//! The Cloud Agent `checks.json` file the harness writes.
//!
//! Three producers share one file format (`tests/contracts/schemas/checks.schema.json`,
//! frozen v1):
//!
//! - `mode: agent`: `byteask-engine exec` with `BYTEASK_CHECKS_OUT` set records the configure,
//!   build, test and lint commands the agent ran through its shell tools ([`AgentRecorder`]);
//! - `mode: baseline`: `byteask-engine verify` without `--checks` runs the environment recipe's build and
//!   test commands;
//! - `mode: verify`: `byteask-engine verify --checks FILE` re-runs exactly those commands, with
//!   the same ids, so test results compare by id.
//!
//! No model and no network is involved in any of them.

mod agent;
mod classify;
mod git;
mod junit;
mod redact;
mod reports;
mod run;
mod verify;

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

pub use agent::AgentRecorder;
pub use verify::VerifyCli;
pub use verify::run_verify;

/// Whether a command the agent ran builds and whether it runs tests, from any of
/// its simple commands (`cmake --build b && ctest` does both). Heuristics on the text, as for checks.json.
pub fn command_builds_and_tests(command: &str) -> (bool, bool) {
    let purposes = classify::purposes(&classify::unwrap_shell(command));
    (
        purposes.contains(&Purpose::Build),
        purposes.contains(&Purpose::Test),
    )
}

/// The environment variable that turns on agent-mode `checks.json` in `exec`.
pub const CHECKS_OUT_ENV_VAR: &str = "BYTEASK_CHECKS_OUT";

/// Schema limits (`checks.schema.json`, `common.schema.json`).
const MAX_COMMANDS: usize = 50;
const MAX_TESTS: usize = 20_000;
const MAX_JUNIT_PER_COMMAND: usize = 10;
const MAX_CMD_CHARS: usize = 4000;
const MAX_LOG_TAIL_CHARS: usize = 8192;
const MAX_DURATION_S: f64 = 86_400.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Baseline,
    Agent,
    Verify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Configure,
    Build,
    Test,
    Lint,
    Other,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Configure => "configure",
            Purpose::Build => "build",
            Purpose::Test => "test",
            Purpose::Lint => "lint",
            Purpose::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Pass,
    Fail,
    Skip,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeneratedBy {
    pub tool: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub base_sha: String,
    pub tree_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEntry {
    pub id: String,
    pub purpose: Purpose,
    pub cmd: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_s: f64,
    pub log_tail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junit: Option<Vec<String>>,
}

impl CommandEntry {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestEntry {
    pub id: String,
    pub status: TestStatus,
    pub command_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub build_ok: bool,
    pub tests_total: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checks {
    pub version: u32,
    pub mode: Mode,
    pub generated_by: GeneratedBy,
    pub repo: RepoInfo,
    pub commands: Vec<CommandEntry>,
    pub tests: Vec<TestEntry>,
    pub summary: Summary,
}

impl Checks {
    /// Assemble a file whose `summary` agrees with its entries (the cross-field rules in
    /// `validate.py`): `build_ok` = every configure and build command exited 0.
    fn new(
        mode: Mode,
        repo: RepoInfo,
        commands: Vec<CommandEntry>,
        mut tests: Vec<TestEntry>,
    ) -> Self {
        tests.truncate(MAX_TESTS);
        let build_ok = commands
            .iter()
            .filter(|c| matches!(c.purpose, Purpose::Configure | Purpose::Build))
            .all(CommandEntry::succeeded);
        let summary = Summary {
            build_ok,
            tests_total: tests.len(),
            tests_failed: tests
                .iter()
                .filter(|t| matches!(t.status, TestStatus::Fail | TestStatus::Error))
                .count(),
            tests_skipped: tests
                .iter()
                .filter(|t| t.status == TestStatus::Skip)
                .count(),
        };
        Self {
            version: 1,
            mode,
            generated_by: GeneratedBy {
                tool: "byteask-engine".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            repo,
            commands,
            tests,
            summary,
        }
    }

    /// Write the file atomically (temp file in the same directory, then rename), so a reader
    /// never sees half a file even if the process is killed mid-write.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        json.push(b'\n');
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "checks.json".to_string());
        let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// Keep at most `max` characters from the end of `text`.
fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// The log tail as the schema wants it: secrets redacted, at most 8192 characters.
fn log_tail(output: &str) -> String {
    // Redact a generous window first so a secret cut in half at the window edge is still seen
    // whole, then cut to the limit.
    let window = tail_chars(output, MAX_LOG_TAIL_CHARS * 2);
    tail_chars(&redact::redact(&window), MAX_LOG_TAIL_CHARS)
}

fn duration_s(millis: u128) -> f64 {
    let secs = (millis as f64) / 1000.0;
    secs.clamp(0.0, MAX_DURATION_S)
}

/// Tests from one command, one entry per id: if a report lists an id twice, the worst status
/// wins, so a flaky duplicate can never hide a failure.
fn dedupe_tests(command_id: &str, cases: Vec<(String, TestStatus)>) -> Vec<TestEntry> {
    fn rank(status: TestStatus) -> u8 {
        match status {
            TestStatus::Skip => 0,
            TestStatus::Pass => 1,
            TestStatus::Fail => 2,
            TestStatus::Error => 3,
        }
    }
    let mut out: Vec<TestEntry> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for (id, status) in cases {
        match index.get(&id) {
            Some(&i) => {
                let existing: &mut TestEntry = &mut out[i];
                if rank(status) > rank(existing.status) {
                    existing.status = status;
                }
            }
            None => {
                index.insert(id.clone(), out.len());
                out.push(TestEntry {
                    id,
                    status,
                    command_id: command_id.to_string(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn summary_agrees_with_entries() {
        let command = |id: &str, purpose, exit_code| CommandEntry {
            id: id.to_string(),
            purpose,
            cmd: "x".to_string(),
            cwd: ".".to_string(),
            exit_code,
            timed_out: false,
            duration_s: 1.0,
            log_tail: String::new(),
            junit: None,
        };
        let tests = dedupe_tests(
            "test",
            vec![
                ("a".to_string(), TestStatus::Pass),
                ("b".to_string(), TestStatus::Fail),
                ("b".to_string(), TestStatus::Pass),
                ("c".to_string(), TestStatus::Error),
                ("d".to_string(), TestStatus::Skip),
            ],
        );
        let checks = Checks::new(
            Mode::Baseline,
            RepoInfo {
                base_sha: "0".repeat(40),
                tree_sha: None,
            },
            vec![
                command("build", Purpose::Build, Some(0)),
                command("test", Purpose::Test, Some(8)),
            ],
            tests,
        );
        assert_eq!(
            checks.summary,
            Summary {
                build_ok: true,
                tests_total: 4,
                tests_failed: 2,
                tests_skipped: 1,
            }
        );
        let broken = Checks::new(
            Mode::Agent,
            checks.repo.clone(),
            vec![command("configure", Purpose::Configure, None)],
            Vec::new(),
        );
        assert!(!broken.summary.build_ok);
    }

    #[test]
    fn log_tail_is_bounded_and_redacted() {
        let long = format!(
            "{}\nAuthorization: Bearer abcdefghijklmnopqrstuvwxyz0123",
            "x".repeat(20_000)
        );
        let tail = log_tail(&long);
        assert!(tail.chars().count() <= MAX_LOG_TAIL_CHARS);
        assert!(!tail.contains("abcdefghijklmnopqrstuvwxyz0123"), "{tail}");
        assert!(tail.ends_with("[REDACTED_SECRET]"), "{tail}");
    }
}
