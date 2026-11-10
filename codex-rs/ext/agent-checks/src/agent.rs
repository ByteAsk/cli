//! `mode: agent`: the configure/build/test/lint commands the agent ran through its shell tools,
//! as `exec` observes them. Only the last run of each distinct command (text + cwd) is kept,
//! ordered by that last run, at most 50 (the newest).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use crate::Checks;
use crate::CommandEntry;
use crate::MAX_CMD_CHARS;
use crate::MAX_COMMANDS;
use crate::MAX_JUNIT_PER_COMMAND;
use crate::Mode;
use crate::Purpose;
use crate::RepoInfo;
use crate::TestStatus;
use crate::classify;
use crate::dedupe_tests;
use crate::duration_s;
use crate::git;
use crate::junit;
use crate::log_tail;
use crate::reports;

/// The shell tools report a timeout as exit code 124 (`EXEC_TIMEOUT_EXIT_CODE` in core).
const TIMEOUT_EXIT_CODE: i32 = 124;
/// Filesystem timestamps can be coarse; a report this much older than the command's start
/// still counts as written by it.
const MTIME_SLACK: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    cmd: String,
    cwd: String,
}

#[derive(Debug)]
struct Pending {
    key: Key,
    purpose: Purpose,
    script: String,
    abs_cwd: PathBuf,
    started: Instant,
    started_wall: SystemTime,
}

#[derive(Debug)]
struct Finished {
    key: Key,
    purpose: Purpose,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    log_tail: String,
    junit: Vec<String>,
    tests: Vec<(String, TestStatus)>,
}

#[derive(Debug)]
pub struct AgentRecorder {
    root: PathBuf,
    base_sha: String,
    pending: HashMap<String, Pending>,
    finished: Vec<Finished>,
}

impl AgentRecorder {
    /// Start recording for the repository that contains `dir`; `base_sha` is HEAD now.
    pub fn new(dir: &Path) -> Result<Self, String> {
        let root = git::repo_root(dir)?;
        let base_sha = git::head_sha(&root)?;
        Ok(Self {
            root: canonical(&root),
            base_sha,
            pending: HashMap::new(),
            finished: Vec::new(),
        })
    }

    /// The command (as the tools report it, shell wrapper included) and its working
    /// directory, as checks.json and `verify` want them; `None` for commands that are not
    /// configure/build/test/lint.
    fn normalize(&self, command: &str, cwd: &Path) -> Option<(Key, Purpose, String, PathBuf)> {
        let script = classify::unwrap_shell(command);
        let purpose = classify::classify(&script)?;
        let abs_cwd = canonical(cwd);
        let (cmd, rel_cwd) = match abs_cwd.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => (script.clone(), ".".to_string()),
            Ok(rel) => (script.clone(), rel.to_string_lossy().into_owned()),
            // Outside the repo (an out-of-tree build dir): keep it re-runnable from the root.
            Err(_) => (
                format!(
                    "cd {} && {script}",
                    shlex::try_quote(&abs_cwd.to_string_lossy()).ok()?
                ),
                ".".to_string(),
            ),
        };
        if cmd.chars().count() > MAX_CMD_CHARS {
            return None;
        }
        Some((Key { cmd, cwd: rel_cwd }, purpose, script, abs_cwd))
    }

    pub fn command_started(&mut self, item_id: &str, command: &str, cwd: &Path) {
        if let Some((key, purpose, script, abs_cwd)) = self.normalize(command, cwd) {
            self.pending.insert(
                item_id.to_string(),
                Pending {
                    key,
                    purpose,
                    script,
                    abs_cwd,
                    started: Instant::now(),
                    started_wall: SystemTime::now(),
                },
            );
        }
    }

    /// A command the tools declined or never ran: forget it.
    pub fn command_dropped(&mut self, item_id: &str) {
        self.pending.remove(item_id);
    }

    pub fn command_finished(
        &mut self,
        item_id: &str,
        command: &str,
        cwd: &Path,
        exit_code: Option<i32>,
        duration_ms: Option<i64>,
        output: Option<&str>,
    ) {
        let pending = match self.pending.remove(item_id) {
            Some(pending) => pending,
            None => {
                let Some((key, purpose, script, abs_cwd)) = self.normalize(command, cwd) else {
                    return;
                };
                let ran_for = Duration::from_millis(
                    duration_ms.and_then(|d| u64::try_from(d).ok()).unwrap_or(0),
                );
                Pending {
                    key,
                    purpose,
                    script,
                    abs_cwd,
                    started: Instant::now(),
                    started_wall: SystemTime::now()
                        .checked_sub(ran_for)
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                }
            }
        };
        let duration_ms = duration_ms
            .and_then(|d| u128::try_from(d).ok())
            .unwrap_or_else(|| pending.started.elapsed().as_millis());
        let timed_out = exit_code == Some(TIMEOUT_EXIT_CODE);
        let (junit, mut tests) = self.read_reports(&pending);
        // No JUnit from a test command: automake .trs, Meson testlog.json, automake console
        // lines, else its exit status as one test.
        if tests.is_empty() && pending.purpose == Purpose::Test {
            let since = pending
                .started_wall
                .checked_sub(MTIME_SLACK)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            tests = reports::read(
                &self.root,
                &pending.abs_cwd,
                &pending.script,
                output.unwrap_or_default(),
                since,
            );
            if tests.is_empty() {
                tests = vec![reports::exit_status(&pending.script, exit_code, timed_out)];
            }
        }
        self.push(Finished {
            key: pending.key,
            purpose: pending.purpose,
            exit_code: if timed_out { None } else { exit_code },
            timed_out,
            duration_ms,
            log_tail: log_tail(output.unwrap_or_default()),
            junit,
            tests,
        });
    }

    /// JUnit reports the command was asked to write and did write (modified after it
    /// started): repo-relative paths for checks.json, and the parsed test cases.
    fn read_reports(&self, pending: &Pending) -> (Vec<String>, Vec<(String, TestStatus)>) {
        let since = pending
            .started_wall
            .checked_sub(MTIME_SLACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut paths = Vec::new();
        let mut tests = Vec::new();
        for path in classify::junit_outputs(&pending.script, &pending.abs_cwd) {
            let Some(text) = read_fresh(&path, since) else {
                continue;
            };
            let Ok(cases) = junit::parse(&text) else {
                continue;
            };
            tests.extend(cases);
            if let Ok(rel) = canonical(&path).strip_prefix(&self.root)
                && paths.len() < MAX_JUNIT_PER_COMMAND
            {
                paths.push(rel.to_string_lossy().into_owned());
            }
        }
        (paths, tests)
    }

    fn push(&mut self, finished: Finished) {
        self.finished.retain(|f| f.key != finished.key);
        self.finished.push(finished);
        if self.finished.len() > MAX_COMMANDS {
            let excess = self.finished.len() - MAX_COMMANDS;
            self.finished.drain(..excess);
        }
    }

    /// The checks.json for everything seen so far. Commands still running are listed with
    /// `exit_code: null` (they did not finish).
    pub fn checks(&self) -> Checks {
        let mut running: Vec<&Pending> = self.pending.values().collect();
        running.sort_by_key(|p| p.started);
        let mut rows: Vec<Row<'_>> = self
            .finished
            .iter()
            .filter(|f| !running.iter().any(|p| p.key == f.key))
            .map(|f| Row {
                key: &f.key,
                purpose: f.purpose,
                exit_code: f.exit_code,
                timed_out: f.timed_out,
                duration_ms: f.duration_ms,
                log_tail: f.log_tail.clone(),
                junit: &f.junit,
                tests: &f.tests,
            })
            .collect();
        for p in running {
            if rows.iter().any(|r| r.key == &p.key) {
                continue;
            }
            rows.push(Row {
                key: &p.key,
                purpose: p.purpose,
                exit_code: None,
                timed_out: false,
                duration_ms: p.started.elapsed().as_millis(),
                log_tail: "(still running when the run ended)".to_string(),
                junit: &[],
                tests: &[],
            });
        }
        if rows.len() > MAX_COMMANDS {
            rows.drain(..rows.len() - MAX_COMMANDS);
        }

        let mut per_purpose: HashMap<Purpose, usize> = HashMap::new();
        let mut commands = Vec::new();
        let mut tests = Vec::new();
        for row in rows {
            let n = per_purpose.entry(row.purpose).or_insert(0);
            *n += 1;
            let id = if *n == 1 {
                row.purpose.as_str().to_string()
            } else {
                format!("{}-{n}", row.purpose.as_str())
            };
            tests.extend(dedupe_tests(&id, row.tests.to_vec()));
            commands.push(CommandEntry {
                id,
                purpose: row.purpose,
                cmd: row.key.cmd.clone(),
                cwd: row.key.cwd.clone(),
                exit_code: row.exit_code,
                timed_out: row.timed_out,
                duration_s: duration_s(row.duration_ms),
                log_tail: row.log_tail,
                junit: (!row.junit.is_empty()).then(|| row.junit.to_vec()),
            });
        }
        Checks::new(
            Mode::Agent,
            RepoInfo {
                base_sha: self.base_sha.clone(),
                tree_sha: None,
            },
            commands,
            tests,
        )
    }
}

struct Row<'a> {
    key: &'a Key,
    purpose: Purpose,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    log_tail: String,
    junit: &'a [String],
    tests: &'a [(String, TestStatus)],
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The file's text if it was modified at or after `since`.
pub(crate) fn read_fresh(path: &Path, since: SystemTime) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    if modified < since {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        dir
    }

    #[test]
    fn keeps_the_last_run_of_each_command_in_order() {
        let repo = git_repo();
        let root = repo.path();
        let build = root.join("build");
        std::fs::create_dir_all(&build).expect("mkdir");
        let mut recorder = AgentRecorder::new(root).expect("recorder");
        let ok = Some("ok");
        recorder.command_started("1", "/bin/bash -lc 'cmake -S . -B build'", root);
        recorder.command_finished(
            "1",
            "/bin/bash -lc 'cmake -S . -B build'",
            root,
            Some(0),
            Some(1500),
            ok,
        );
        recorder.command_finished("2", "/bin/bash -lc 'ls -la'", root, Some(0), Some(5), ok);
        recorder.command_finished(
            "3",
            "/bin/bash -lc 'ctest'",
            &build,
            Some(8),
            Some(900),
            Some("1 failed"),
        );
        recorder.command_finished(
            "4",
            "/bin/bash -lc 'cmake --build build'",
            root,
            Some(0),
            Some(2000),
            ok,
        );
        recorder.command_finished(
            "5",
            "/bin/bash -lc 'ctest'",
            &build,
            Some(0),
            Some(800),
            Some("100% passed"),
        );
        recorder.command_finished(
            "6",
            "/bin/bash -lc 'ctest -R Book'",
            &build,
            Some(124),
            Some(60_000),
            None,
        );
        recorder.command_started("7", "/bin/bash -lc 'make -C build test'", root);
        let checks = recorder.checks();
        let summary: Vec<(&str, &str, &str, Option<i32>, bool)> = checks
            .commands
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.cmd.as_str(),
                    c.cwd.as_str(),
                    c.exit_code,
                    c.timed_out,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("configure", "cmake -S . -B build", ".", Some(0), false),
                ("build", "cmake --build build", ".", Some(0), false),
                ("test", "ctest", "build", Some(0), false),
                ("test-2", "ctest -R Book", "build", None, true),
                ("test-3", "make -C build test", ".", None, false),
            ]
        );
        assert_eq!(checks.mode, Mode::Agent);
        assert_eq!(checks.repo.tree_sha, None);
        assert!(checks.summary.build_ok);
    }

    #[test]
    fn a_command_outside_the_repo_runs_from_the_root() {
        let repo = git_repo();
        let outside = tempfile::tempdir().expect("tempdir");
        let mut recorder = AgentRecorder::new(repo.path()).expect("recorder");
        recorder.command_finished(
            "1",
            "bash -lc 'ninja'",
            outside.path(),
            Some(0),
            Some(10),
            None,
        );
        let checks = recorder.checks();
        let expected = format!("cd {} && ninja", canonical(outside.path()).display());
        assert_eq!(
            (
                checks.commands[0].cmd.as_str(),
                checks.commands[0].cwd.as_str()
            ),
            (expected.as_str(), ".")
        );
    }
}
