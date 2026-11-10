//! Shell commands run by the engine itself (builds, test runs, git), with a timeout, a capped
//! output buffer, and a process-wide semaphore that bounds how many builds/test runs happen at
//! once inside the container.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncReadExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

/// Output kept per command: the tail is what matters for build errors and test summaries.
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct RunOutput {
    pub code: Option<i32>,
    pub timed_out: bool,
    /// Combined stdout+stderr, truncated from the front to [`OUTPUT_CAP`] bytes.
    pub output: String,
    pub elapsed_ms: u64,
    /// The command could not be started at all.
    pub spawn_error: Option<String>,
}

impl RunOutput {
    pub fn ok(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.code == Some(0)
    }

    /// Last `max` bytes of the output, on a char boundary.
    pub fn tail(&self, max: usize) -> &str {
        tail(&self.output, max)
    }
}

pub fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

static RUN_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// The process-wide build/test semaphore. The first caller fixes the permit count.
pub fn run_slots(permits: usize) -> Arc<Semaphore> {
    RUN_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(permits.max(1))))
        .clone()
}

/// Wait for a build/test slot, or give up at `deadline`.
pub async fn acquire_slot(
    slots: &Arc<Semaphore>,
    deadline: Instant,
) -> Option<OwnedSemaphorePermit> {
    let wait = deadline.saturating_duration_since(Instant::now());
    tokio::time::timeout(wait, slots.clone().acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
}

/// Run `cmd` with `sh -c` in `cwd`. The command gets its own process group, which is killed
/// as a whole on timeout.
pub async fn run_shell(
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> RunOutput {
    let started = Instant::now();
    let script = format!("exec 2>&1\n{cmd}\n");
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, v) in env {
        command.env(k, v);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return RunOutput {
                spawn_error: Some(err.to_string()),
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..RunOutput::default()
            };
        }
    };
    let pid = child.id();
    // Kills the whole process group if this future is dropped (cancellation) before the
    // command finished; disarmed on normal completion.
    let mut guard = GroupGuard { pid, armed: true };
    let mut stdout = child.stdout.take();
    let reader = async move {
        let mut buf: Vec<u8> = Vec::new();
        if let Some(out) = stdout.as_mut() {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match out.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > OUTPUT_CAP * 2 {
                            let cut = buf.len() - OUTPUT_CAP;
                            buf.drain(..cut);
                        }
                    }
                }
            }
        }
        buf
    };
    let waited = tokio::time::timeout(timeout, async {
        let (buf, status) = tokio::join!(reader, child.wait());
        (buf, status)
    })
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match waited {
        Ok((mut buf, status)) => {
            guard.armed = false;
            if buf.len() > OUTPUT_CAP {
                let cut = buf.len() - OUTPUT_CAP;
                buf.drain(..cut);
            }
            RunOutput {
                code: status.ok().and_then(|s| s.code()),
                timed_out: false,
                output: String::from_utf8_lossy(&buf).into_owned(),
                elapsed_ms,
                spawn_error: None,
            }
        }
        Err(_) => {
            guard.armed = false;
            kill_group(pid);
            let _ = child.kill().await;
            RunOutput {
                code: None,
                timed_out: true,
                output: format!("[timed out after {}s]", timeout.as_secs()),
                elapsed_ms,
                spawn_error: None,
            }
        }
    }
}

struct GroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            kill_group(self.pid);
        }
    }
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid
        && let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        // SAFETY: plain syscall on a process group we created; failure is harmless.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Run a program (no shell) and return (success, stdout, stderr). Used for git plumbing.
pub async fn spawn_capture(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> (bool, Vec<u8>, String) {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return (false, Vec::new(), err.to_string()),
    };
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        use tokio::io::AsyncWriteExt;
        tokio::spawn(async move {
            let _ = pipe.write_all(&input).await;
            let _ = pipe.shutdown().await;
        });
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => (
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Ok(Err(err)) => (false, Vec::new(), err.to_string()),
        Err(_) => (false, Vec::new(), format!("{program} timed out")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = run_shell(
            "echo hello; echo oops >&2; exit 3",
            dir.path(),
            &[],
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(out.code, Some(3));
        assert!(!out.ok());
        assert!(out.output.contains("hello"));
        assert!(out.output.contains("oops"));
    }

    #[tokio::test]
    async fn timeout_kills_the_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Instant::now();
        let out = run_shell(
            "sleep 30 & sleep 30",
            dir.path(),
            &[],
            Duration::from_millis(300),
        )
        .await;
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn env_is_passed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = run_shell(
            "printf '%s' \"$E3_PROBE\"",
            dir.path(),
            &[("E3_PROBE".to_string(), "value".to_string())],
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(out.output, "value");
    }
}
