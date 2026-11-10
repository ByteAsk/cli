//! The lldb backend of `debug_session`: gdb does not run on Apple Silicon
//! Macs, so there the session is lldb, driven through its Python API by
//! `byteask_lldb_session.py` (why the Python API and not the lldb command
//! line, and how commands are checked, is written at the top of that file).
//!
//! This side finds a Python that can import lldb, runs the helper, and owns
//! the one thing the helper cannot do for itself: stopping a program that
//! never stops. lldb's Python calls hold the GIL while `continue` blocks, so
//! the engine sends the program SIGSTOP after [`EXEC_TIMEOUT`]; lldb reports
//! that as an ordinary stop and the blocked command returns.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;

use super::session::GdbSession;
use super::session::Job;

const HELPER_PY: &str = include_str!("../../byteask_lldb_session.py");
/// Loading a large core (the one that motivated this was 2.2 GB) takes time.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a command may leave the program running before it is stopped.
const EXEC_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(30)
};
/// How long to wait for the reply after stopping it.
const AFTER_STOP: Duration = Duration::from_secs(10);
const REQUEST_QUEUE: usize = 16;

/// Start an lldb session. `modules` are sysroot files to load into a core at
/// the core's recorded addresses (see [`crate::corefile`]). Returns the
/// session and a note on what start did (empty when nothing to say).
pub async fn start_lldb(
    id: u64,
    program: Option<&Path>,
    core: Option<&Path>,
    prog_args: &[String],
    cwd: &Path,
    modules: &[(PathBuf, u64)],
) -> Result<(Arc<GdbSession>, String), String> {
    let python = locate_python().await?;
    let config = serde_json::json!({
        "program": program.map(|p| p.to_string_lossy().into_owned()),
        "core": core.map(|p| p.to_string_lossy().into_owned()),
        "args": prog_args,
        "cwd": cwd.to_string_lossy(),
        "modules": modules
            .iter()
            .map(|(path, slide)| serde_json::json!([path.to_string_lossy(), slide]))
            .collect::<Vec<_>>(),
    });
    // Debuggers are not inside the tool sandbox yet; this records the
    // note the output carries.
    crate::sandbox::wrap("lldb", &[], cwd).map_err(crate::sandbox::error_text)?;
    let mut command = Command::new(&python.python);
    command
        .arg("-u")
        .arg("-c")
        .arg(HELPER_PY)
        .env("BYTEASK_LLDB_SESSION", config.to_string())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = &python.pythonpath {
        let joined = match std::env::var_os("PYTHONPATH") {
            Some(existing) if !existing.is_empty() => {
                let mut all = std::ffi::OsString::from(path);
                all.push(":");
                all.push(existing);
                all
            }
            _ => std::ffi::OsString::from(path),
        };
        command.env("PYTHONPATH", joined);
    }
    if let Some(server) = &python.debugserver {
        command.env("LLDB_DEBUGSERVER_PATH", server);
    }
    // The helper, lldb-server and the program go down together (see the
    // same note on the gdb session).
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|err| {
        format!(
            "could not start {} for lldb: {err}",
            python.python.display()
        )
    })?;
    let mut stdin = child.stdin.take().ok_or("lldb helper stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("lldb helper stdout unavailable")?;
    let mut stderr = child.stderr.take();
    let mut lines = BufReader::new(stdout).lines();

    let first = match timeout(START_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        other => {
            let mut said = String::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = timeout(Duration::from_millis(500), stderr.read_to_string(&mut said)).await;
            }
            crate::run::kill_process_group(child.id());
            let why = match other {
                Err(_) => format!("it did not answer within {}s", START_TIMEOUT.as_secs()),
                _ => "it exited".to_string(),
            };
            return Err(format!(
                "lldb did not start a debugging session ({why}); nothing was started.{}",
                if said.trim().is_empty() {
                    String::new()
                } else {
                    format!(" It said: {}", tail_chars(said.trim(), 800))
                }
            ));
        }
    };
    let hello: serde_json::Value = serde_json::from_str(&first)
        .map_err(|_| format!("the lldb helper said something unexpected: {first}"))?;
    if let Some(fatal) = hello.get("fatal").and_then(|v| v.as_str()) {
        crate::run::kill_process_group(child.id());
        return Err(format!(
            "{fatal}. The interactive lldb session needs lldb's Python module (on macOS it \
             comes with the Xcode Command Line Tools: xcode-select --install); the one-shot \
             `debug` tool with backend=\"lldb\" works without it."
        ));
    }
    // Let a crash message reach nobody rather than block the pipe.
    if let Some(mut stderr) = stderr {
        tokio::spawn(async move {
            let mut sink = Vec::new();
            let _ = stderr.read_to_end(&mut sink).await;
        });
    }
    let note = hello
        .get("note")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let state = Arc::new(Mutex::new(
        hello
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("ready")
            .to_string(),
    ));

    let (request_tx, mut request_rx) = mpsc::channel::<Job>(REQUEST_QUEUE);
    let actor_state = state.clone();
    tokio::spawn(async move {
        // Kills the program and the helper's whole group when this task ends
        // for ANY reason, including being dropped (the engine exiting) before
        // the orderly shutdown below finishes: a spinning program outlived its
        // session that way.
        let mut reaper = Reaper {
            program: None,
            group: child.id(),
        };
        let mut pid: Option<i32> = None;
        while let Some(job) = request_rx.recv().await {
            let line = serde_json::json!({ "cmd": job.command }).to_string() + "\n";
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                let _ = job.reply.send("lldb session has ended.".to_string());
                break;
            }
            let (reply, alive) = await_reply(&mut lines, &mut pid).await;
            reaper.program = pid;
            if let Some(value) = reply.1 {
                set(&actor_state, &value);
            }
            let _ = job.reply.send(reply.0);
            if !alive {
                // The helper is gone or hung: nothing orderly is left to do.
                set(&actor_state, "ended");
                return;
            }
        }
        // End of input tells the helper to kill the program and exit. If it
        // does, the program's pid may already belong to someone else: forget
        // it. Otherwise the reaper takes everything down as this task ends.
        drop(stdin);
        if matches!(timeout(Duration::from_secs(5), child.wait()).await, Ok(Ok(status)) if status.success())
        {
            reaper.program = None;
            reaper.group = None;
        }
    });

    let session = GdbSession::from_parts(
        id,
        program
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no program)".to_string()),
        "lldb",
        state,
        request_tx,
    );
    Ok((session, note))
}

struct Reaper {
    program: Option<i32>,
    group: Option<u32>,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(pid) = self.program {
            signal(pid, Signal::Kill);
        }
        crate::run::kill_process_group(self.group);
    }
}

/// Read helper lines until the command's reply. Returns ((text, new state),
/// whether the session is still usable).
async fn await_reply(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    pid: &mut Option<i32>,
) -> ((String, Option<String>), bool) {
    let mut deadline = Instant::now() + EXEC_TIMEOUT;
    let mut stopped = false;
    loop {
        let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(_) => {
                return (
                    ("lldb session ended unexpectedly.".to_string(), None),
                    false,
                );
            }
            Err(_) if !stopped && pid.is_some() => {
                if let Some(pid) = *pid {
                    signal(pid, Signal::Stop);
                }
                stopped = true;
                deadline = Instant::now() + AFTER_STOP;
                continue;
            }
            Err(_) => {
                return (
                    (
                        "lldb stopped responding, so the session was ended. Start a new one."
                            .to_string(),
                        None,
                    ),
                    false,
                );
            }
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(value) = message.get("pid").and_then(serde_json::Value::as_i64) {
            *pid = i32::try_from(value).ok();
            continue;
        }
        let Some(out) = message.get("out").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut text = String::new();
        if stopped {
            text.push_str(&format!(
                "note: the program was still running after {}s, so it was stopped (SIGSTOP) \
                 where it was. `continue` lets it run on.\n",
                EXEC_TIMEOUT.as_secs()
            ));
        }
        text.push_str(out);
        let state = message
            .get("state")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return ((text, state), true);
    }
}

/// A Python that can import lldb, and what it needs to find it.
struct LldbPython {
    python: PathBuf,
    pythonpath: Option<PathBuf>,
    /// Linux: lldb's Python finds `lldb-server` relative to liblldb, which
    /// distribution packages move; point it at the one beside `lldb`.
    debugserver: Option<PathBuf>,
}

async fn locate_python() -> Result<LldbPython, String> {
    let Some(lldb) = find_on_path("lldb") else {
        return Err(format!(
            "lldb not installed ({})",
            crate::probe::install_hint("lldb")
        ));
    };
    let real = lldb.canonicalize().unwrap_or(lldb.clone());
    let python = match std::env::var_os("BYTEASK_LLDB_PYTHON") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => default_python().await,
    };
    let pythonpath = match std::env::var_os("BYTEASK_LLDB_PYTHONPATH") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => lldb_python_dir(&lldb, &real).await,
    };
    let debugserver =
        if cfg!(target_os = "linux") && std::env::var_os("LLDB_DEBUGSERVER_PATH").is_none() {
            real.parent()
                .map(|dir| dir.join("lldb-server"))
                .filter(|p| p.is_file())
        } else {
            None
        };
    Ok(LldbPython {
        python,
        pythonpath,
        debugserver,
    })
}

/// macOS: the Python lldb was built against is Xcode's, not Homebrew's.
async fn default_python() -> PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(path) = output_of("xcrun", &["-f", "python3"]).await {
            return PathBuf::from(path);
        }
        return PathBuf::from("/usr/bin/python3");
    }
    PathBuf::from("python3")
}

/// `lldb -P` names lldb's Python directory. Some distribution packages print
/// a directory that does not exist (Debian's lldb 14 does); then look beside
/// the real `lldb` binary.
async fn lldb_python_dir(lldb: &Path, real: &Path) -> Option<PathBuf> {
    let has_module = |dir: &Path| dir.join("lldb").join("__init__.py").is_file();
    if let Some(dir) = output_of(&lldb.to_string_lossy(), &["-P"])
        .await
        .map(PathBuf::from)
    {
        if has_module(&dir) {
            return Some(dir);
        }
    }
    let lib = real.parent()?.parent()?.join("lib");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&lib)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("python3"))
        })
        .flat_map(|p| [p.join("dist-packages"), p.join("site-packages")])
        .filter(|p| has_module(p))
        .collect();
    candidates.sort();
    candidates.pop()
}

async fn output_of(program: &str, args: &[&str]) -> Option<String> {
    let out = timeout(
        Duration::from_secs(10),
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())?
        .trim()
        .to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

enum Signal {
    Stop,
    Kill,
}

fn signal(pid: i32, which: Signal) {
    #[cfg(unix)]
    {
        let number = match which {
            Signal::Stop => libc::SIGSTOP,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) with a signal number; no memory is touched.
        unsafe {
            libc::kill(pid, number);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, which);
}

fn set(state: &Arc<Mutex<String>>, value: &str) {
    if let Ok(mut s) = state.lock() {
        *s = value.to_string();
    }
}

fn tail_chars(text: &str, cap: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(cap)).collect()
}
