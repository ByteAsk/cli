//! Run one check command: `bash -lc CMD` in its cwd, stdout and stderr merged, the last bytes
//! kept for the log tail, and on timeout the whole process group killed.

use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

/// Bytes of output kept (the log tail is cut to 8192 characters later, after redaction).
const KEEP_BYTES: usize = 64 * 1024;
/// After the command exits, how long its leftover children may hold the output pipe open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(crate) struct Outcome {
    /// `None` when it did not finish (timed out, or killed by a signal).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub output: String,
}

#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    done: bool,
}

fn append(tail: &Mutex<Tail>, chunk: &[u8]) {
    if let Ok(mut tail) = tail.lock() {
        tail.bytes.extend_from_slice(chunk);
        if tail.bytes.len() > KEEP_BYTES * 2 {
            let cut = tail.bytes.len() - KEEP_BYTES;
            tail.bytes.drain(..cut);
        }
    }
}

pub(crate) fn run_shell(cmd: &str, cwd: &Path, timeout: Duration) -> Outcome {
    let started = Instant::now();
    let spawned = spawn(cmd, cwd);
    let (mut child, reader) = match spawned {
        Ok(spawned) => spawned,
        Err(err) => {
            return Outcome {
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                output: format!("byteask-engine verify: could not start the command: {err}"),
            };
        }
    };
    let pid = child.id();

    let tail = Arc::new(Mutex::new(Tail::default()));
    let reader_tail = Arc::clone(&tail);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => append(&reader_tail, &buf[..n]),
            }
        }
        if let Ok(mut tail) = reader_tail.lock() {
            tail.done = true;
        }
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            kill_group(pid);
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(POLL);
    };
    let duration_ms = started.elapsed().as_millis();

    // Background children (a server a test started, say) may keep the pipe open: give them a
    // moment, then kill what is left of the group so nothing outlives the check.
    let drain_started = Instant::now();
    loop {
        let done = tail.lock().map(|t| t.done).unwrap_or(true);
        if done {
            break;
        }
        if drain_started.elapsed() >= DRAIN_GRACE {
            kill_group(pid);
            if drain_started.elapsed() >= DRAIN_GRACE * 2 {
                break;
            }
        }
        std::thread::sleep(POLL);
    }

    let mut output = tail
        .lock()
        .map(|t| String::from_utf8_lossy(&t.bytes).into_owned())
        .unwrap_or_default();
    if timed_out {
        output.push_str(&format!(
            "\nbyteask-engine verify: timed out after {}s; process group killed\n",
            timeout.as_secs()
        ));
    }
    Outcome {
        exit_code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        timed_out,
        duration_ms,
        output,
    }
}

fn spawn(cmd: &str, cwd: &Path) -> std::io::Result<(std::process::Child, std::io::PipeReader)> {
    let (reader, writer) = std::io::pipe()?;
    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    // `command` holds the write ends; dropping it lets the reader see EOF once the child (and
    // anything it started) closes them.
    drop(command);
    Ok((child, reader))
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: killpg only sends a signal; the group id is the child's pid because it was
        // spawned with `process_group(0)`.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_shell(
            "echo out; echo err >&2; exit 3",
            dir.path(),
            Duration::from_secs(30),
        );
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.timed_out);
        assert!(
            outcome.output.contains("out") && outcome.output.contains("err"),
            "{outcome:?}"
        );
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived");
        let cmd = format!("(sleep 3; touch '{}') & sleep 30", marker.display());
        let started = Instant::now();
        let outcome = run_shell(&cmd, dir.path(), Duration::from_secs(1));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!((outcome.exit_code, outcome.timed_out), (None, true));
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a background child outlived the timeout");
    }
}
