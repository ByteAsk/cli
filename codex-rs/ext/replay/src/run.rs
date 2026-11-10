//! Feed one recorded input to two programs and compare what they print.
//!
//! Each program (a "subject") runs under a fixed, minimal environment: the environment is
//! cleared and only `PATH` is kept, with `LC_ALL=C`, `LANG=C` and `TZ=UTC` set, so a locale or a
//! time zone on the machine cannot make two runs differ. A subject adds what it needs through
//! `env`; `inherit_env: true` keeps the caller's environment instead (for tools that need it,
//! named in the report).
//!
//! The recorded input reaches the program on stdin, or as a path wherever an argument contains
//! `{input}`. Standard output goes to a file in the work directory (never held in memory) and is
//! read back as JSON Lines or plain lines. A run that times out, prints more than the output
//! limit, or cannot start is never compared: the report says NOT RUN and why. On Unix every
//! subject runs in its own process group, and the whole group is stopped when the run ends, so a
//! timed-out program cannot leave children behind.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use crate::compare::Comparison;
use crate::compare::compare_streams;
use crate::record::JsonLines;
use crate::record::RecordResult;
use crate::record::TextLines;
use crate::record::sha256_file;
use crate::report;
use crate::rules::Rules;
use crate::rules::RulesSpec;

/// One program to run.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    /// Short name used in reports and for its output files (letters, digits, `.`, `_`, `-`).
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inherit_env: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// How a subject's standard output is split into records.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Jsonl,
    Lines,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_output_bytes: u64,
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_MAX_OUTPUT: u64 = 1 << 30;

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    /// The program exited on its own with this code.
    Exited { code: i32 },
    /// The program was killed by a signal it did not get from us (a crash, for example 11).
    Signaled { signal: i32 },
    /// We stopped it at the time limit.
    TimedOut { after_ms: u64 },
    /// We stopped it because it printed more than the output limit.
    OutputLimit { bytes: u64 },
    /// It never started.
    SpawnFailed { error: String },
}

impl RunStatus {
    /// True when the program ran to its own end (an exit or a crash), so its output is complete
    /// and can be compared. False when we stopped it or it never started.
    pub fn completed(&self) -> bool {
        matches!(self, RunStatus::Exited { .. } | RunStatus::Signaled { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            RunStatus::Exited { code } => format!("exited with code {code}"),
            RunStatus::Signaled { signal } => format!("was killed by signal {signal}"),
            RunStatus::TimedOut { after_ms } => {
                format!("was stopped after {after_ms} ms (time limit)")
            }
            RunStatus::OutputLimit { bytes } => {
                format!("was stopped after printing {bytes} bytes (output limit)")
            }
            RunStatus::SpawnFailed { error } => format!("could not start: {error}"),
        }
    }
}

/// What one run produced.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RunOutput {
    pub name: String,
    pub status: RunStatus,
    pub elapsed_ms: u64,
    pub stdout_path: PathBuf,
    pub stdout_bytes: u64,
    /// The last few KiB of standard error, with control characters replaced.
    pub stderr_tail: String,
}

const STDERR_TAIL_BYTES: u64 = 4096;

/// Is this a safe subject name (it becomes part of file names)?
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Build the command for a subject, without spawning it.
fn command_for(subject: &Subject, input: &Path) -> Result<(Command, bool), String> {
    let Some((program, args)) = subject.argv.split_first() else {
        return Err("argv is empty".to_string());
    };
    let input_text = input.to_string_lossy();
    let mut uses_path = false;
    let mut subst = |s: &String| {
        if s.contains("{input}") {
            uses_path = true;
            s.replace("{input}", &input_text)
        } else {
            s.clone()
        }
    };
    let program = subst(program);
    let args: Vec<String> = args.iter().map(&mut subst).collect();
    let mut cmd = Command::new(program);
    cmd.args(args);
    if !subject.inherit_env {
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("LC_ALL", "C").env("LANG", "C").env("TZ", "UTC");
    }
    cmd.envs(&subject.env);
    if let Some(cwd) = &subject.cwd {
        cmd.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    Ok((cmd, uses_path))
}

#[cfg(unix)]
fn stop_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: killpg only sends a signal; the group id is the child's pid because the child
        // was spawned with process_group(0), so this cannot reach any process we did not start.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn stop_group(_pid: u32) {}

fn status_of(status: std::process::ExitStatus) -> RunStatus {
    if let Some(code) = status.code() {
        return RunStatus::Exited { code };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return RunStatus::Signaled { signal };
        }
    }
    RunStatus::Exited { code: -1 }
}

fn read_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > STDERR_TAIL_BYTES {
        let _ = file.seek(SeekFrom::Start(len - STDERR_TAIL_BYTES));
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Run one subject on one input. Never panics; every failure is a [`RunStatus`].
pub fn run_subject(subject: &Subject, input: &Path, workdir: &Path, limits: &Limits) -> RunOutput {
    let stdout_path = workdir.join(format!("{}.stdout", subject.name));
    let stderr_path = workdir.join(format!("{}.stderr", subject.name));
    let failed = |error: String, stdout_path: PathBuf| RunOutput {
        name: subject.name.clone(),
        status: RunStatus::SpawnFailed { error },
        elapsed_ms: 0,
        stdout_path,
        stdout_bytes: 0,
        stderr_tail: String::new(),
    };
    if !valid_name(&subject.name) {
        return failed(
            format!(
                "subject name {:?} must be 1-64 letters, digits, '.', '_' or '-'",
                subject.name
            ),
            stdout_path,
        );
    }
    let (mut cmd, uses_path) = match command_for(subject, input) {
        Ok(c) => c,
        Err(e) => return failed(e, stdout_path),
    };
    let files = (|| -> std::io::Result<(File, File, Stdio)> {
        let out = File::create(&stdout_path)?;
        let err = File::create(&stderr_path)?;
        let stdin = if uses_path {
            Stdio::null()
        } else {
            Stdio::from(File::open(input)?)
        };
        Ok((out, err, stdin))
    })();
    let (out, err, stdin) = match files {
        Ok(f) => f,
        Err(e) => {
            return failed(
                format!("cannot set up input/output files: {e}"),
                stdout_path,
            );
        }
    };
    cmd.stdin(stdin).stdout(out).stderr(err);
    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return failed(
                format!("{}: {e}", subject.argv.first().map_or("", String::as_str)),
                stdout_path,
            );
        }
    };
    let pid = child.id();
    let mut sleep = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break status_of(st),
            Ok(None) => {}
            Err(e) => {
                stop_group(pid);
                let _ = child.wait();
                break RunStatus::SpawnFailed {
                    error: format!("waiting for the program failed: {e}"),
                };
            }
        }
        let elapsed = start.elapsed();
        if elapsed >= limits.timeout {
            stop_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            break RunStatus::TimedOut {
                after_ms: elapsed.as_millis() as u64,
            };
        }
        let printed = std::fs::metadata(&stdout_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if printed > limits.max_output_bytes {
            stop_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            break RunStatus::OutputLimit { bytes: printed };
        }
        std::thread::sleep(sleep);
        sleep = (sleep * 2).min(Duration::from_millis(20));
    };
    // Anything the program left running in its group goes too.
    stop_group(pid);
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let stdout_bytes = std::fs::metadata(&stdout_path)
        .map(|m| m.len())
        .unwrap_or(0);
    // A program can print past the limit in its final moments, after our last size check.
    let status = if status.completed() && stdout_bytes > limits.max_output_bytes {
        RunStatus::OutputLimit {
            bytes: stdout_bytes,
        }
    } else {
        status
    };
    RunOutput {
        name: subject.name.clone(),
        status,
        elapsed_ms,
        stdout_path,
        stdout_bytes,
        stderr_tail: read_tail(&stderr_path),
    }
}

/// Open a subject's output as a record stream.
pub fn read_output(
    path: &Path,
    format: OutputFormat,
) -> std::io::Result<Box<dyn Iterator<Item = RecordResult>>> {
    let reader = BufReader::new(File::open(path)?);
    Ok(match format {
        OutputFormat::Jsonl => Box::new(JsonLines::new(reader)),
        OutputFormat::Lines => Box::new(TextLines::new(reader)),
    })
}

/// A complete replay: one input, two subjects, the rules.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReplaySpec {
    pub input: PathBuf,
    pub a: Subject,
    pub b: Subject,
    #[serde(default)]
    pub rules: RulesSpec,
    #[serde(default)]
    pub output: OutputFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
    /// Also require the same exit status (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare_exit_status: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Same,
    Different,
    NotRun,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ReplayReport {
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub sentence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<RunOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<RunOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
}

impl ReplayReport {
    fn not_run(reason: String) -> Self {
        Self {
            outcome: Outcome::NotRun,
            sentence: format!("NOT RUN: {reason}"),
            reason: Some(reason),
            input_sha256: None,
            a: None,
            b: None,
            comparison: None,
        }
    }
}

/// Run both subjects on the input (at the same time) and compare their outputs.
pub fn replay_and_compare(spec: &ReplaySpec, workdir: &Path) -> ReplayReport {
    let rules = match Rules::from_spec(&spec.rules) {
        Ok(r) => r,
        Err(e) => return ReplayReport::not_run(e.0),
    };
    if spec.a.name == spec.b.name {
        return ReplayReport::not_run(format!(
            "both subjects are named {:?}; give them different names",
            spec.a.name
        ));
    }
    let input_sha256 = match sha256_file(&spec.input) {
        Ok(h) => h,
        Err(e) => {
            return ReplayReport::not_run(format!(
                "the input {} cannot be read: {e}",
                spec.input.display()
            ));
        }
    };
    let limits = Limits {
        timeout: spec
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis),
        max_output_bytes: spec.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT),
    };
    let (ra, rb) = std::thread::scope(|s| {
        let ha = s.spawn(|| run_subject(&spec.a, &spec.input, workdir, &limits));
        let rb = run_subject(&spec.b, &spec.input, workdir, &limits);
        let ra = ha.join().unwrap_or_else(|_| RunOutput {
            name: spec.a.name.clone(),
            status: RunStatus::SpawnFailed {
                error: "the runner thread panicked".to_string(),
            },
            elapsed_ms: 0,
            stdout_path: PathBuf::new(),
            stdout_bytes: 0,
            stderr_tail: String::new(),
        });
        (ra, rb)
    });
    finish(spec, &rules, input_sha256, ra, rb)
}

fn finish(
    spec: &ReplaySpec,
    rules: &Rules,
    input_sha256: String,
    ra: RunOutput,
    rb: RunOutput,
) -> ReplayReport {
    let mut report = ReplayReport {
        outcome: Outcome::NotRun,
        reason: None,
        sentence: String::new(),
        input_sha256: Some(input_sha256),
        a: None,
        b: None,
        comparison: None,
    };
    let stopped = [&ra, &rb]
        .into_iter()
        .find(|run| !run.status.completed())
        .map(|run| format!("{} {}", run.name, run.status.describe()));
    if let Some(reason) = stopped {
        report.sentence = format!("NOT RUN: {reason}.");
        report.reason = Some(reason);
        report.a = Some(ra);
        report.b = Some(rb);
        return report;
    }
    let opened = read_output(&ra.stdout_path, spec.output)
        .and_then(|a| read_output(&rb.stdout_path, spec.output).map(|b| (a, b)));
    let comparison = match opened {
        Err(e) => Err(format!("an output file cannot be read: {e}")),
        Ok((a, b)) => compare_streams(a, b, rules).map_err(|e| {
            e.to_string()
                .replacen("stream A", &format!("{}'s output", ra.name), 1)
                .replacen("stream B", &format!("{}'s output", rb.name), 1)
        }),
    };
    match comparison {
        Err(reason) => {
            report.sentence = format!("NOT RUN: {reason}.");
            report.reason = Some(reason);
        }
        Ok(c) => {
            let exit_differs = spec.compare_exit_status.unwrap_or(true) && ra.status != rb.status;
            report.outcome = if c.same && !exit_differs {
                Outcome::Same
            } else {
                Outcome::Different
            };
            report.sentence = if c.same && exit_differs {
                format!(
                    "Same output, but {} {} and {} {}.",
                    ra.name,
                    ra.status.describe(),
                    rb.name,
                    rb.status.describe()
                )
            } else {
                let mut s = report::sentence(&c, &ra.name, &rb.name);
                if ra.status != (RunStatus::Exited { code: 0 }) || ra.status != rb.status {
                    s.push_str(&format!(
                        " ({} {}; {} {}.)",
                        ra.name,
                        ra.status.describe(),
                        rb.name,
                        rb.status.describe()
                    ));
                }
                s
            };
            report.comparison = Some(c);
        }
    }
    report.a = Some(ra);
    report.b = Some(rb);
    report
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn sh(name: &str, script: &str) -> Subject {
        Subject {
            name: name.to_string(),
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            env: BTreeMap::new(),
            inherit_env: false,
            cwd: None,
        }
    }

    fn spec(dir: &Path, a: Subject, b: Subject) -> ReplaySpec {
        let input = dir.join("input.jsonl");
        std::fs::write(&input, "{\"seq\":0,\"px\":100}\n{\"seq\":1,\"px\":101}\n").expect("write");
        ReplaySpec {
            input,
            a,
            b,
            rules: RulesSpec::default(),
            output: OutputFormat::Jsonl,
            timeout_ms: Some(10_000),
            max_output_bytes: None,
            compare_exit_status: None,
        }
    }

    #[test]
    fn two_programs_fed_the_same_input_compare_same() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = spec(
            dir.path(),
            sh("old", "cat"),
            sh("new", "while read l; do echo \"$l\"; done"),
        );
        let r = replay_and_compare(&s, dir.path());
        assert_eq!(r.outcome, Outcome::Same, "{}", r.sentence);
        assert_eq!(r.comparison.as_ref().map(|c| c.records_a), Some(2));
        assert_eq!(r.input_sha256.as_ref().map(String::len), Some(64));
        assert!(
            r.sentence.starts_with("Same output: 2 records"),
            "{}",
            r.sentence
        );
    }

    #[test]
    fn the_first_difference_names_the_subjects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = spec(dir.path(), sh("old", "cat"), sh("new", "sed 's/101/102/'"));
        let r = replay_and_compare(&s, dir.path());
        assert_eq!(r.outcome, Outcome::Different);
        assert_eq!(
            r.sentence,
            "First difference at record 1: field /px is 101 in old and 102 in new (rule: exact)."
        );
    }

    #[test]
    fn the_environment_is_fixed_unless_a_subject_asks_for_more() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut a = sh(
            "a",
            "printf '{\"tz\":\"%s\",\"lc\":\"%s\",\"home\":\"%s\"}\\n' \"$TZ\" \"$LC_ALL\" \"$HOME\"",
        );
        a.env.insert("EXTRA".to_string(), "1".to_string());
        let b = sh(
            "b",
            "printf '{\"tz\":\"UTC\",\"lc\":\"C\",\"home\":\"\"}\\n'",
        );
        let r = replay_and_compare(&spec(dir.path(), a, b), dir.path());
        assert_eq!(r.outcome, Outcome::Same, "{}", r.sentence);
    }

    #[test]
    fn input_can_be_passed_as_a_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut a = sh("a", "cat \"$0\"");
        a.argv.push("{input}".to_string());
        let r = replay_and_compare(&spec(dir.path(), a, sh("b", "cat")), dir.path());
        assert_eq!(r.outcome, Outcome::Same, "{}", r.sentence);
    }

    #[test]
    fn a_timeout_is_not_run_and_the_whole_process_group_is_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("child-still-alive");
        // The subject starts a grandchild that would write a marker after 1.5 s, then hangs.
        let script = format!("(sleep 1.5; touch '{}') & sleep 30", marker.display());
        let mut s = spec(dir.path(), sh("slow", &script), sh("fast", "cat"));
        s.timeout_ms = Some(300);
        let r = replay_and_compare(&s, dir.path());
        assert_eq!(r.outcome, Outcome::NotRun);
        assert!(
            r.sentence.starts_with("NOT RUN: slow was stopped after"),
            "{}",
            r.sentence
        );
        assert!(r.comparison.is_none(), "a run we stopped is never compared");
        std::thread::sleep(Duration::from_millis(2000));
        assert!(!marker.exists(), "the grandchild survived the timeout");
    }

    #[test]
    fn output_limit_spawn_failure_bad_output_and_exit_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = spec(
            dir.path(),
            sh("big", "yes '{}' | head -c 100000"),
            sh("small", "cat"),
        );
        s.max_output_bytes = Some(1000);
        let r = replay_and_compare(&s, dir.path());
        assert_eq!(r.outcome, Outcome::NotRun);
        assert!(r.sentence.contains("output limit"), "{}", r.sentence);

        let mut missing = sh("ghost", "");
        missing.argv = vec!["/nonexistent/program".to_string()];
        let r = replay_and_compare(&spec(dir.path(), missing, sh("b", "cat")), dir.path());
        assert_eq!(r.outcome, Outcome::NotRun);
        assert!(
            r.sentence.contains("ghost could not start"),
            "{}",
            r.sentence
        );

        let r = replay_and_compare(
            &spec(
                dir.path(),
                sh("a", "cat"),
                sh("b", "echo '{\"seq\":0,\"px\":100}'; echo 'oops'"),
            ),
            dir.path(),
        );
        assert_eq!(r.outcome, Outcome::NotRun);
        assert!(
            r.sentence
                .contains("b's output could not be read: record 1 (line 2)"),
            "{}",
            r.sentence
        );

        let r = replay_and_compare(
            &spec(dir.path(), sh("a", "cat"), sh("b", "cat; exit 3")),
            dir.path(),
        );
        assert_eq!(r.outcome, Outcome::Different);
        assert_eq!(
            r.sentence,
            "Same output, but a exited with code 0 and b exited with code 3."
        );
        let mut s = spec(dir.path(), sh("a", "cat"), sh("b", "cat; exit 3"));
        s.compare_exit_status = Some(false);
        assert_eq!(replay_and_compare(&s, dir.path()).outcome, Outcome::Same);

        let r = replay_and_compare(
            &spec(dir.path(), sh("x", "cat"), sh("x", "cat")),
            dir.path(),
        );
        assert!(r.sentence.contains("different names"), "{}", r.sentence);
        let r = replay_and_compare(
            &spec(dir.path(), sh("../x", "cat"), sh("y", "cat")),
            dir.path(),
        );
        assert_eq!(r.outcome, Outcome::NotRun);
    }

    #[test]
    fn a_crash_midway_is_a_difference_with_the_signal_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = replay_and_compare(
            &spec(
                dir.path(),
                sh("a", "cat"),
                sh("b", "head -n 1; kill -SEGV $$"),
            ),
            dir.path(),
        );
        assert_eq!(r.outcome, Outcome::Different, "{}", r.sentence);
        assert!(
            r.sentence.contains("b ended after 1 records"),
            "{}",
            r.sentence
        );
        assert!(
            r.sentence.contains("b was killed by signal 11"),
            "{}",
            r.sentence
        );
    }

    #[test]
    fn stderr_tail_is_kept_and_control_characters_are_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = replay_and_compare(
            &spec(
                dir.path(),
                sh("a", "cat; printf 'warn \\033[31mred\\n' >&2"),
                sh("b", "cat"),
            ),
            dir.path(),
        );
        let tail = r.a.expect("a ran").stderr_tail;
        assert_eq!(tail, "warn ?[31mred\n");
    }
}
