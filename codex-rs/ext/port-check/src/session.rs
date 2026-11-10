//! Run one version of a strategy as a child process under the harness, and write its transcript.
//!
//! The transcript is JSON Lines, one row per step:
//! `{"step":N,"ts":T,"ev":"md","in":{...event...},"out":{"actions":[...],"decisions":[...],"diag":{...}}}`
//! and a final `{"step":N,"ev":"summary","summary":{...},"exit":"exited with code 0"}`. When the
//! strategy fails at a step (it exits, crashes, breaks the protocol or does not answer in time)
//! that step's row carries `"error"` instead of `"out"` and the transcript ends there, so a
//! comparison reports the failure at exactly the step it happened.
//!
//! The child gets a cleared environment (only `PATH`, plus `LC_ALL=C`, `LANG=C`, `TZ=UTC`,
//! `PYTHONDONTWRITEBYTECODE=1`), then the request's `env`, then the harness's own per-run
//! variables (`PYTHONHASHSEED`, `MALLOC_PERTURB_`), which the request cannot override. It runs in
//! its own process group, and the whole group is stopped when the run ends.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use crate::model::MdRecord;
use crate::protocol::parse_response;
use crate::sim::HarnessConfig;
use crate::sim::RecordStream;
use crate::sim::Sim;
use crate::sim::SimError;
use crate::sim::StepLimits;

/// How to start one version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The command actually started, when the host wraps `argv` (the engine's tool sandbox).
    /// Set only by the host, never by a request (not deserialized); `argv` stays what is
    /// fingerprinted and reported.
    #[serde(skip)]
    pub launch: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLimits {
    /// Time for the answer to `init` (imports, warm-up).
    pub first_response: Duration,
    /// Time for every later answer.
    pub response: Duration,
    /// Time for the whole run.
    pub run: Duration,
    pub steps: StepLimits,
    /// Longest line a strategy may print.
    pub max_line_bytes: usize,
    /// Most lines in one answer.
    pub max_answer_lines: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            first_response: Duration::from_secs(60),
            response: Duration::from_secs(10),
            run: Duration::from_secs(3600),
            steps: StepLimits::default(),
            max_line_bytes: 1 << 20,
            max_answer_lines: 100_000,
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEnd {
    /// Every event was answered; `exit` says how the process ended after the last one.
    Completed { exit: String },
    /// The strategy exited, crashed or broke the protocol at `step`.
    StrategyFailed { step: u64, detail: String },
    /// The strategy did not answer `step` in time.
    StrategyTimedOut { step: u64, detail: String },
    /// A harness limit stopped the run at `step` (for example a feedback loop).
    HarnessLimit { step: u64, detail: String },
    /// The harness itself could not continue (recording changed, overflow, I/O).
    HarnessError { detail: String },
    /// The process never started.
    SpawnFailed { detail: String },
}

impl RunEnd {
    pub fn completed(&self) -> bool {
        matches!(self, RunEnd::Completed { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            RunEnd::Completed { exit } => format!("answered every event, then {exit}"),
            RunEnd::StrategyFailed { step, detail } => format!("failed at step {step}: {detail}"),
            RunEnd::StrategyTimedOut { step, detail } => {
                format!("timed out at step {step}: {detail}")
            }
            RunEnd::HarnessLimit { step, detail } => {
                format!("was stopped by a harness limit at step {step}: {detail}")
            }
            RunEnd::HarnessError { detail } => format!("could not be run: {detail}"),
            RunEnd::SpawnFailed { detail } => format!("could not start: {detail}"),
        }
    }
}

/// One finished run.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct VersionRun {
    pub label: String,
    /// The per-run variables the harness set (e.g. PYTHONHASHSEED=0).
    pub varied: BTreeMap<String, String>,
    pub transcript: PathBuf,
    pub steps: u64,
    pub end: RunEnd,
    pub elapsed_ms: u64,
    pub stderr_tail: String,
    /// The simulator's summary when the run completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
}

/// Opens the recording afresh for each run.
pub type RecordingOpener = dyn Fn() -> Result<Box<RecordStream<'static>>, String> + Sync;

/// Everything shared by every run of one check.
pub struct Setup<'a> {
    pub config: &'a HarnessConfig,
    pub symbols: &'a [String],
    pub initial_book: &'a [MdRecord],
    pub open_recording: &'a RecordingOpener,
    pub limits: &'a RunLimits,
    pub workdir: &'a Path,
}

enum Msg {
    Line(String),
    Eof,
    Bad(String),
}

const STDERR_TAIL: u64 = 4096;

fn read_tail(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > STDERR_TAIL {
        let _ = f.seek(SeekFrom::Start(len - STDERR_TAIL));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c })
        .collect()
}

#[cfg(unix)]
fn stop_group(child: &mut Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: killpg only sends a signal; the group id is the child's pid because the child
        // was spawned with process_group(0), so it reaches only processes this run started.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn stop_group(child: &mut Child) {
    let _ = child.kill();
}

fn exit_text(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("was killed by signal {sig}");
        }
    }
    "ended".to_string()
}

/// Wait up to `grace` for the child to exit on its own; stop it otherwise.
fn finish_child(child: &mut Child, grace: Duration) -> String {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                stop_group(child);
                return exit_text(st);
            }
            Ok(None) if start.elapsed() < grace => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                stop_group(child);
                let _ = child.wait();
                return format!("did not exit within {} ms after its input closed", grace.as_millis());
            }
        }
    }
}

fn reader_thread(stdout: std::process::ChildStdout, max_line: usize, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let limit = (max_line as u64).saturating_add(1);
            let msg = match r.by_ref().take(limit).read_until(b'\n', &mut buf) {
                Ok(0) => Msg::Eof,
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                    } else if buf.len() > max_line {
                        let _ = tx.send(Msg::Bad(format!("printed a line longer than {max_line} bytes")));
                        return;
                    }
                    match String::from_utf8(buf.clone()) {
                        Ok(s) => Msg::Line(s),
                        Err(_) => Msg::Bad("printed a line that is not UTF-8 text".to_string()),
                    }
                }
                Err(e) => Msg::Bad(format!("reading its output failed: {e}")),
            };
            let last = !matches!(msg, Msg::Line(_));
            if tx.send(msg).is_err() || last {
                return;
            }
        }
    });
}

fn writer_thread(mut stdin: std::process::ChildStdin, rx: mpsc::Receiver<String>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in rx {
            if stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush())
                .is_err()
            {
                return;
            }
        }
    })
}

fn spawn(spec: &ProcessSpec, varied: &BTreeMap<String, String>, stderr: File) -> Result<Child, String> {
    let argv = spec.launch.as_ref().unwrap_or(&spec.argv);
    let Some((program, args)) = argv.split_first() else {
        return Err("argv is empty".to_string());
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .env("PYTHONDONTWRITEBYTECODE", "1");
    cmd.envs(&spec.env);
    cmd.envs(varied);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| format!("{program}: {e}"))
}

/// Run one version over the recording and write its transcript.
pub fn run_version(
    label: &str,
    spec: &ProcessSpec,
    varied: BTreeMap<String, String>,
    setup: &Setup<'_>,
) -> VersionRun {
    let transcript = setup.workdir.join(format!("{label}.transcript.jsonl"));
    let stderr_path = setup.workdir.join(format!("{label}.stderr"));
    let start = Instant::now();
    let mut result = VersionRun {
        label: label.to_string(),
        varied: varied.clone(),
        transcript: transcript.clone(),
        steps: 0,
        end: RunEnd::HarnessError {
            detail: "not started".to_string(),
        },
        elapsed_ms: 0,
        stderr_tail: String::new(),
        summary: None,
    };
    let end = drive(spec, &varied, setup, &transcript, &stderr_path, &mut result);
    result.end = end;
    result.elapsed_ms = start.elapsed().as_millis() as u64;
    result.stderr_tail = read_tail(&stderr_path);
    result
}

fn drive(
    spec: &ProcessSpec,
    varied: &BTreeMap<String, String>,
    setup: &Setup<'_>,
    transcript: &Path,
    stderr_path: &Path,
    result: &mut VersionRun,
) -> RunEnd {
    let harness_error = |detail: String| RunEnd::HarnessError { detail };
    let mut sim = match Sim::new(
        setup.config.clone(),
        setup.limits.steps,
        setup.symbols.to_vec(),
        setup.initial_book,
    ) {
        Ok(s) => s,
        Err(e) => return harness_error(e),
    };
    let mut recording = match (setup.open_recording)() {
        Ok(r) => r,
        Err(e) => return harness_error(e),
    };
    let (out, stderr) = match (File::create(transcript), File::create(stderr_path)) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(e), _) | (_, Err(e)) => return harness_error(format!("cannot create run files: {e}")),
    };
    let mut out = BufWriter::new(out);
    let mut child = match spawn(spec, varied, stderr) {
        Ok(c) => c,
        Err(e) => return RunEnd::SpawnFailed { detail: e },
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        stop_group(&mut child);
        return harness_error("the child's pipes are missing".to_string());
    };
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let writer = writer_thread(stdin, line_rx);
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    reader_thread(stdout, setup.limits.max_line_bytes, msg_tx);
    let deadline = Instant::now() + setup.limits.run;
    let mut write_row = |row: &Value| -> Result<(), String> {
        serde_json::to_writer(&mut out, row)
            .map_err(|e| e.to_string())
            .and_then(|()| out.write_all(b"\n").map_err(|e| e.to_string()))
    };

    let end = loop {
        let ev = match sim.next_event(&mut *recording) {
            Ok(Some(ev)) => ev,
            Ok(None) => break None,
            Err(SimError::Limit(m)) => {
                break Some(RunEnd::HarnessLimit {
                    step: result.steps,
                    detail: m,
                });
            }
            Err(e) => break Some(harness_error(e.to_string())),
        };
        let step = ev.step;
        let row_head = json!({"step": step, "ts": ev.ts, "ev": ev.type_name(), "in": ev.json()});
        let fail_row = |detail: &str| {
            let mut r = row_head.clone();
            r["error"] = json!(detail);
            r
        };
        let _ = line_tx.send(ev.line());
        let wait = if step == 0 {
            setup.limits.first_response
        } else {
            setup.limits.response
        };
        let step_deadline = (Instant::now() + wait).min(deadline);
        let mut lines = Vec::new();
        let answer: Result<(), RunEnd> = loop {
            let left = step_deadline.saturating_duration_since(Instant::now());
            match msg_rx.recv_timeout(left) {
                Ok(Msg::Line(l)) => {
                    if l.trim() == "done" {
                        break Ok(());
                    }
                    if lines.len() >= setup.limits.max_answer_lines {
                        break Err(RunEnd::StrategyFailed {
                            step,
                            detail: format!(
                                "protocol error: more than {} lines in one answer",
                                setup.limits.max_answer_lines
                            ),
                        });
                    }
                    lines.push(l);
                }
                Ok(Msg::Bad(m)) => {
                    break Err(RunEnd::StrategyFailed {
                        step,
                        detail: format!("protocol error: {m}"),
                    });
                }
                Ok(Msg::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let how = finish_child(&mut child, Duration::from_secs(2));
                    break Err(RunEnd::StrategyFailed {
                        step,
                        detail: format!("{how} before answering"),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let what = if Instant::now() >= deadline {
                        format!("the whole run passed its {} ms limit", setup.limits.run.as_millis())
                    } else {
                        format!("no answer within {} ms", wait.as_millis())
                    };
                    break Err(RunEnd::StrategyTimedOut { step, detail: what });
                }
            }
        };
        if let Err(end) = answer {
            let detail = match &end {
                RunEnd::StrategyFailed { detail, .. } | RunEnd::StrategyTimedOut { detail, .. } => {
                    detail.clone()
                }
                other => other.describe(),
            };
            let _ = write_row(&fail_row(&detail));
            break Some(end);
        }
        let response = match parse_response(&lines) {
            Ok(r) => r,
            Err(m) => {
                let detail = format!("protocol error: {m}");
                let _ = write_row(&fail_row(&detail));
                break Some(RunEnd::StrategyFailed { step, detail });
            }
        };
        sim.respond(&response);
        let mut row = row_head;
        row["out"] = response.json();
        if let Err(e) = write_row(&row) {
            break Some(harness_error(format!("cannot write the transcript: {e}")));
        }
        result.steps = step + 1;
    };
    drop(line_tx);
    let end = match end {
        Some(end) => {
            stop_group(&mut child);
            let _ = child.wait();
            end
        }
        None => {
            let exit = finish_child(&mut child, Duration::from_secs(5));
            let summary = sim.summary();
            let row = json!({"step": result.steps, "ev": "summary", "summary": summary, "exit": exit});
            match write_row(&row) {
                Ok(()) => {
                    result.summary = Some(summary);
                    RunEnd::Completed { exit }
                }
                Err(e) => harness_error(format!("cannot write the transcript: {e}")),
            }
        }
    };
    let _ = writer.join();
    if let Err(e) = out.flush() {
        return harness_error(format!("cannot write the transcript: {e}"));
    }
    end
}
