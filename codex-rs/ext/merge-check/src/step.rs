//! One profile command: `bash -c` in the candidate's checkout, its own process group, a time
//! limit that kills the whole group, and the tail of its output kept as evidence.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncReadExt as _;

/// Lines of output kept per step.
const TAIL_LINES: usize = 40;
/// Bytes of output read per step (the rest is drained and dropped).
const MAX_CAPTURE: usize = 4 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepEnd {
    /// Exited with this status (`None`: killed by a signal).
    Exited(Option<i32>),
    /// The time limit ran out and the group was killed.
    TimedOut,
    /// Could not start.
    NotStarted(String),
}

#[derive(Clone, Debug)]
pub struct StepOutcome {
    pub command: String,
    pub end: StepEnd,
    pub seconds: f64,
    /// The last lines of stdout and stderr together.
    pub tail: Vec<String>,
}

impl StepOutcome {
    pub fn succeeded(&self) -> bool {
        self.end == StepEnd::Exited(Some(0))
    }
}

/// Run one command. `env` is added to the inherited environment.
pub async fn run(
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
    limit: Duration,
) -> StepOutcome {
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return StepOutcome {
                command: command.to_string(),
                end: StepEnd::NotStarted(err.to_string()),
                seconds: 0.0,
                tail: Vec::new(),
            };
        }
    };
    let pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read_all = async move {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = async {
            if let Some(pipe) = stdout.as_mut() {
                read_capped(pipe, &mut out).await;
            }
        };
        let read_err = async {
            if let Some(pipe) = stderr.as_mut() {
                read_capped(pipe, &mut err).await;
            }
        };
        tokio::join!(read_out, read_err);
        (out, err)
    };
    let waited = tokio::time::timeout(limit, async {
        let (output, status) = tokio::join!(read_all, child.wait());
        (output, status)
    })
    .await;
    let (end, output) = match waited {
        Ok(((out, err), status)) => {
            let code = status.ok().and_then(|status| status.code());
            (StepEnd::Exited(code), (out, err))
        }
        Err(_) => {
            kill_group(pid);
            (StepEnd::TimedOut, (Vec::new(), Vec::new()))
        }
    };
    let mut tail: Vec<String> = String::from_utf8_lossy(&output.0)
        .lines()
        .chain(String::from_utf8_lossy(&output.1).lines())
        .map(clean_line)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if tail.len() > TAIL_LINES {
        tail.drain(..tail.len() - TAIL_LINES);
    }
    StepOutcome {
        command: command.to_string(),
        end,
        seconds: started.elapsed().as_secs_f64(),
        tail,
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(pipe: &mut R, into: &mut Vec<u8>) {
    let mut buf = [0u8; 16 << 10];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if into.len() < MAX_CAPTURE {
                    into.extend_from_slice(&buf[..n]);
                }
            }
        }
    }
}

/// Printable text only: no escape sequences reach a status page.
fn clean_line(line: &str) -> String {
    line.chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .take(400)
        .collect()
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok())
        && pid > 0
    {
        // SAFETY: kill(2) on the process group this step created with process_group(0).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exit_status_and_tail_are_kept() {
        let dir = tempfile::tempdir().expect("tmp");
        let ok = run("echo one; echo two >&2", dir.path(), &[], Duration::from_secs(10)).await;
        assert!(ok.succeeded(), "{ok:?}");
        assert_eq!(ok.tail, vec!["one".to_string(), "two".to_string()]);
        let failed = run("exit 3", dir.path(), &[], Duration::from_secs(10)).await;
        assert_eq!(failed.end, StepEnd::Exited(Some(3)));
        let env = run(
            "test \"$X\" = y",
            dir.path(),
            &[("X".to_string(), "y".to_string())],
            Duration::from_secs(10),
        )
        .await;
        assert!(env.succeeded());
    }

    /// A step that outlives its limit is stopped with everything it started.
    #[tokio::test]
    async fn the_time_limit_kills_the_whole_group() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("child-alive");
        let command = format!("(sleep 3; touch {}) & sleep 30", marker.display());
        let started = Instant::now();
        let out = run(&command, dir.path(), &[], Duration::from_millis(300)).await;
        assert_eq!(out.end, StepEnd::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the background child kept running");
    }
}
