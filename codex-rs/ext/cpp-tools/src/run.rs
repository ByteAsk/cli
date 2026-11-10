//! Run an external command with a hard wall-clock cap.
//!
//! On timeout the ENTIRE process group is killed, so a profiler/child tree
//! (`bash -lc`, `perf record`, `valgrind --trace-children`, a `taskset` wrapper)
//! cannot outlive the tool call and wedge the conversation or leak multi-GB
//! trace files. Dropping a `tokio` timeout future only detaches the child and
//! orphans its grandchildren, which is exactly the failure this module exists to
//! prevent. The group is killed after a NORMAL finish
//! too, and the run ends when the direct child exits — not when the last
//! holder of its output pipes lets go. A process that detached itself
//! with `setsid()` has left the group and is beyond either kill.
//!
//! ```text
//!   run_capped(program, args, cwd, timeout)
//!       spawn (unix: new process group; pgid == child pid)
//!       ├─ finished in time            → pipes get PIPE_GRACE to hit EOF, else kill(-pgid) + one more grace
//!       │                                → kill(-pgid) → RunOutcome::Finished { success, code, stdout, stderr, truncated }
//!       └─ exceeded timeout            → kill(-pgid) (unix) / child.kill() → RunOutcome::TimedOut
//!       spawn NotFound                 → Err(RespondToModel …)   (bash → "use cpp_bench local")
//!       spawn other error              → Err(RespondToModel …)   never Fatal: that ends the turn
//! ```
//!
//! # Truncation ownership
//!
//! Three independent layers can shorten a tool's output. They are deliberately
//! ordered largest → smallest, and each owns exactly one concern:
//!
//! | layer | who | cap | owns |
//! |---|---|---|---|
//! | 0 | this module, [`CAPTURE_CAP_BYTES`] | 4 MiB **per stream** | engine **memory**. A hard ceiling on what is ever held in RAM. Never a display decision. Trips → `Finished { truncated: true }`, so a cut stream can never be mistaken for a complete clean run. |
//! | 1 | each tool (`STDOUT_CAP`, `TRACE_CAP`, `OUTPUT_CAP`, …) | KB | what the **model** reads. Semantic trims (e.g. `cpp_sanitize` drops the ASan shadow-byte map) happen here, *before* the byte cap. |
//! | 2 | the exec / TUI renderer | its own | what the **human** sees. |
//!
//! Invariant: **layer 0 ≫ layer 1**, and **layer 2 ≥ layer 1**. If the renderer
//! cap is set below a tool's layer-1 budget, the evidence is invisible again —
//! which is the exact failure this whole batch exists to fix. `cpp_sanitize`'s
//! layer-1 budget is documented at its `TRACE_CAP`.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use codex_extension_api::FunctionCallError;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;

/// Default per-run wall-clock cap. Overridable via each tool's `timeout_sec`.
/// The cap is applied per measured run (per `perf`/`valgrind`/binary
/// invocation), which is where a runaway happens — callgrind runs a command
/// 20-100× slower, so a 60s benchmark becomes an hour-long tool call.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Default wall-clock cap for a **compile** step.
///
/// [`DEFAULT_TIMEOUT_SECS`] was sized for *running* (callgrind's 20-100×
/// slowdown). Reusing it for *building* kills legitimate work: heavy template
/// instantiation under ASan+UBSan is slow, and a multi-TU
/// `extra_sources` build multiplies it. A compile timeout must also report
/// distinctly from a run timeout, or a slow build reads as a crash.
pub const DEFAULT_COMPILE_TIMEOUT_SECS: u64 = 600;

/// Layer-0 capture ceiling, **per stream**.
///
/// `Command::output()` buffers stdout and stderr with an unbounded
/// `read_to_end`, so an instrumented binary in a print loop can exhaust engine
/// memory long before any wall-clock timeout fires. Past this many bytes we keep
/// *draining* the pipe (so the child never blocks on a full pipe and the timeout
/// stays meaningful) but stop *retaining*, and flag the run as truncated.
pub const CAPTURE_CAP_BYTES: usize = 4 * 1024 * 1024;

/// Result of a capped run.
#[derive(Debug)]
pub enum RunOutcome {
    Finished {
        success: bool,
        /// Exit status, `None` when the process died on a signal (which is how
        /// an ASan abort usually terminates).
        code: Option<i32>,
        stdout: String,
        stderr: String,
        /// At least one stream hit [`CAPTURE_CAP_BYTES`] and was cut. Callers
        /// that render a verdict MUST surface this: a truncated stream is not
        /// evidence of a clean run.
        truncated: bool,
    },
    TimedOut {
        secs: u64,
    },
}

impl RunOutcome {
    /// Convenience: `true` only for a finished, exit-0 run.
    pub fn ok(&self) -> bool {
        matches!(self, RunOutcome::Finished { success: true, .. })
    }
}

/// Resolve `timeout_sec` (or the default) into a `Duration`, flooring at 1s so a
/// `timeout_sec = 0` can never mean "kill immediately".
pub fn timeout_from(timeout_sec: Option<u64>) -> Duration {
    Duration::from_secs(timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1))
}

/// Like [`timeout_from`] but for a compile step: an explicit `timeout_sec`
/// still wins, otherwise [`DEFAULT_COMPILE_TIMEOUT_SECS`].
pub fn compile_timeout_from(timeout_sec: Option<u64>) -> Duration {
    Duration::from_secs(timeout_sec.unwrap_or(DEFAULT_COMPILE_TIMEOUT_SECS).max(1))
}

/// Spawn `program args…` in `cwd`, capture stdout/stderr, and enforce `timeout`.
/// See the module docs for the outcome/error contract.
pub async fn run_capped(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOutcome, FunctionCallError> {
    run_capped_env(program, args, cwd, timeout, &[]).await
}

/// [`run_capped`] with extra environment variables. Split out rather than
/// widening `run_capped`'s signature so the existing call sites (perf, bench,
/// layout) stay untouched; the sanitizer path needs `ASAN_OPTIONS` /
/// `UBSAN_OPTIONS` and is the only caller that does.
pub async fn run_capped_env(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    envs: &[(String, String)],
) -> Result<RunOutcome, FunctionCallError> {
    run(program, args, cwd, timeout, envs, Cores::Allowed, None).await
}

/// [`run_capped_env`] with the child's stdin read from `stdin_file` (a
/// program under test that takes its input on stdin). `None` is `/dev/null`.
pub(crate) async fn run_capped_env_stdin(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    envs: &[(String, String)],
    stdin_file: Option<&Path>,
) -> Result<RunOutcome, FunctionCallError> {
    run(
        program,
        args,
        cwd,
        timeout,
        envs,
        Cores::Allowed,
        stdin_file,
    )
    .await
}

/// [`run_capped`] for a child that must never write a core file.
/// Measured: under `ulimit -c unlimited` valgrind wrote a 6.5 MB
/// `vgcore.<pid>` into the user's workspace for a program that aborted — it
/// honours only RLIMIT_CORE and has no option of its own. The child gets a
/// core limit of 0, soft AND hard, so it cannot raise it back.
///
/// Opt-in, not the default, on purpose: the limit is set in a `pre_exec` hook,
/// which makes std spawn through fork + `execvp`, and glibc's `execvp` retries
/// an ENOEXEC file with `/bin/sh`. A text file without a `#!` line would then
/// RUN as a shell script, and a binary for another architecture would be fed
/// to sh instead of being reported as one (see [`spawn_error`]). Use it only
/// where `program` is a real binary, like `valgrind` itself.
pub async fn run_capped_no_core(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOutcome, FunctionCallError> {
    run(program, args, cwd, timeout, &[], Cores::Suppressed, None).await
}

/// Whether a run's child may write a core file; see [`run_capped_no_core`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cores {
    Allowed,
    Suppressed,
}

async fn run(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    envs: &[(String, String)],
    cores: Cores,
    stdin_file: Option<&Path>,
) -> Result<RunOutcome, FunctionCallError> {
    let stdin = match stdin_file {
        Some(path) => Stdio::from(std::fs::File::open(path).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "cannot open {} as the program's stdin: {err}",
                path.display()
            ))
        })?),
        None => Stdio::null(),
    };
    // Inside the calling tool's sandbox profile (unchanged outside a
    // tool call, or for a profile/platform the sandbox does not cover yet).
    let wrapped = crate::sandbox::wrap(program, args, cwd)?;
    let mut command = Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    // Put the child (and everything it spawns) in its own process group so a
    // timeout can kill the whole tree with kill(-pgid).
    #[cfg(unix)]
    command.process_group(0);
    // Inside an automatic check, low priority on the spare CPUs.
    #[cfg(unix)]
    crate::priority::apply(&mut command);
    #[cfg(unix)]
    if cores == Cores::Suppressed {
        // SAFETY: the closure runs in the forked child before exec and only
        // makes the setrlimit(2) system call, which takes no locks and
        // allocates nothing.
        unsafe {
            command.pre_exec(|| {
                let no_core = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // Best effort: a failure here must not stop the run itself.
                libc::setrlimit(libc::RLIMIT_CORE, &no_core);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = cores;

    let mut child = command
        .spawn()
        .map_err(|err| spawn_error(program, cwd, err))?;
    let pid = child.id();
    // If this future is dropped mid-run (the turn was interrupted), take the process group
    // down with it instead of leaving a build running in the background.
    let mut group_guard = GroupGuard(pid);

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_cut = false;
    let mut err_cut = false;
    let status = {
        let drains = async {
            tokio::join!(
                async {
                    if let Some(pipe) = stdout.as_mut() {
                        out_cut = drain_capped(pipe, &mut out).await;
                    }
                },
                async {
                    if let Some(pipe) = stderr.as_mut() {
                        err_cut = drain_capped(pipe, &mut err).await;
                    }
                },
            )
        };
        tokio::pin!(drains);
        let waited = wait_draining(&mut child, drains.as_mut(), timeout).await;
        group_guard.0 = None;
        match waited {
            Some((status, drained)) => {
                if !drained {
                    settle_pipes(pid, drains.as_mut()).await;
                }
                Some(status)
            }
            None => None,
        }
    };
    let Some(status) = status else {
        if wrapped.sandboxed {
            // Let the sandbox helper stop its sandbox and remove its placeholders first;
            // the process-group kill below still follows.
            stop_helper(pid, &mut child).await;
        }
        kill_tree(pid, &mut child).await;
        crate::sandbox::remove_placeholders(&wrapped.placeholders);
        return Ok(RunOutcome::TimedOut {
            secs: timeout.as_secs(),
        });
    };
    // The run is over, and so is everything it left in its process group.
    // A `cmd &` that redirected its output away closes no pipe, so nothing
    // above waited for it — without this kill it outlives the tool call (measured:
    // cpp_perf `repeat: 2` left 3 orphan `sleep 20`s).
    kill_process_group(pid);
    crate::sandbox::remove_placeholders(&wrapped.placeholders);

    // Not `Fatal`: that would end the user's turn over one command.
    let status = status.map_err(|e| {
        FunctionCallError::RespondToModel(format!(
            "lost track of the command while waiting for it: {e}"
        ))
    })?;
    Ok(RunOutcome::Finished {
        success: status.success(),
        code: crate::sandbox::signal_exit(wrapped.sandboxed, status.code()),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated: out_cut || err_cut,
    })
}

/// A tool that was KILLED BY A SIGNAL (it crashed) is not a tool that
/// answered. Its output — often nothing at all — used to be handed back as a
/// normal result: a SIGSEGV'd r2 became an empty "decompilation", a crashed
/// gdb an empty backtrace. `Some(note)` for a crash, to put in front of the
/// output (or in place of it when there is none).
pub(crate) fn crash_note(program: &str, success: bool, code: Option<i32>) -> Option<String> {
    (!success && code.is_none()).then(|| {
        format!(
            "{program} was killed by a signal (it crashed, or was killed) — this is NOT a \
             complete answer; whatever follows is only what it printed before it died."
        )
    })
}

/// How long the output pipes may stay open after the direct child has exited
/// before we conclude a descendant is holding them. Output already written is
/// read at once; this only bounds the wait for a pipe some OTHER process
/// still holds, see [`settle_pipes`].
const PIPE_GRACE: Duration = Duration::from_millis(500);

/// Wait for the direct child to exit, draining its pipes all the while so a
/// chatty child never blocks on a full pipe. `None` when `timeout` elapses
/// first; otherwise the exit status and whether both pipes already hit EOF.
async fn wait_draining<F>(
    child: &mut Child,
    mut drains: Pin<&mut F>,
    timeout: Duration,
) -> Option<(std::io::Result<ExitStatus>, bool)>
where
    F: Future<Output = ((), ())>,
{
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut drained = false;
    loop {
        tokio::select! {
            // A child that exits exactly at the deadline finished; say so.
            biased;
            status = child.wait() => return Some((status, drained)),
            _ = drains.as_mut(), if !drained => drained = true,
            () = &mut deadline => return None,
        }
    }
}

/// The direct child has exited but a pipe is still open, so something it
/// started holds it — a background job, a forked daemon. Waiting for EOF
/// stalled every such run until the wall-clock cap (measured: a program that
/// exits at once and leaves a `sleep 25` holding stdout came back as a 300 s
/// "RUN TIMED OUT" with its exit status and output discarded).
///
/// Give in-flight output a short grace, then kill the process group — closing
/// every pipe end held inside it — and give the drains a second grace.
/// Anything still holding a pipe after that detached itself from the group
/// (setsid) and is not waited for: the run's own output and status are
/// complete, and a process that left the group is out of our reach.
async fn settle_pipes<F>(pid: Option<u32>, mut drains: Pin<&mut F>)
where
    F: Future<Output = ((), ())>,
{
    if tokio::time::timeout(PIPE_GRACE, drains.as_mut())
        .await
        .is_ok()
    {
        return;
    }
    kill_process_group(pid);
    let _ = tokio::time::timeout(PIPE_GRACE, drains.as_mut()).await;
}

/// Read `pipe` to EOF, retaining at most [`CAPTURE_CAP_BYTES`] in `buf`.
///
/// Past the cap we keep reading and discarding: stopping the read would leave a
/// full pipe buffer and block the child forever, turning a byte overrun into a
/// wall-clock hang. Returns `true` if anything was discarded.
async fn drain_capped<R>(pipe: &mut R, buf: &mut Vec<u8>) -> bool
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return truncated,
            Ok(n) => {
                let room = CAPTURE_CAP_BYTES.saturating_sub(buf.len());
                if room == 0 {
                    truncated = true;
                } else if n > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
}

/// Kills the process group on drop unless disarmed (`.0 = None`) once the child was waited for.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

/// How long a process group gets between SIGTERM and SIGKILL.
pub(crate) const TERM_GRACE: Duration = Duration::from_millis(250);

/// Stop an entire process group by the pgid we created with `process_group(0)` (so
/// `pgid == the child's pid`): SIGTERM now, SIGKILL [`TERM_GRACE`] later. No-op off unix.
///
/// SIGTERM first because build drivers move their jobs out of our group: Ninja runs every
/// command in a process group of its own, so a SIGKILL to our group alone leaves the
/// compilers and custom commands it started running to completion (measured: a cancelled
/// gate's `sleep` step kept running as an orphan). On SIGTERM Ninja (and make) pass the
/// signal on to their jobs before exiting; the SIGKILL then removes anything that ignored it.
///
/// Exposed for the long-lived session tools (`cpp_nav`'s clangd, `debug_session`'s
/// gdb) which cannot use [`run_capped`] — they are interactive, not
/// spawn-and-capture — but still need the tree torn down, not just the direct
/// child. Killing gdb alone leaves its *inferior* running.
pub fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let group = -(pid as i32);
        // Negative pid → the whole process group.
        // SAFETY: kill(2) with a signal number; no memory is touched.
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        std::thread::spawn(move || {
            std::thread::sleep(TERM_GRACE);
            // SAFETY: as above.
            unsafe {
                libc::kill(group, libc::SIGKILL);
            }
        });
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// How long the sandbox helper gets after SIGTERM to stop its sandbox and clean up.
const HELPER_STOP_GRACE: Duration = Duration::from_millis(1_000);

/// SIGTERM the sandbox helper (it forwards the signal into the sandbox, waits for it to end
/// and removes the placeholders it created), and wait for it a little.
async fn stop_helper(pid: Option<u32>, child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: kill(2) with a signal number; no memory is touched.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        let _ = tokio::time::timeout(HELPER_STOP_GRACE, child.wait()).await;
    }
    #[cfg(not(unix))]
    let _ = (pid, child);
}

/// Kill the child's whole process group (unix) or the child itself, then reap.
async fn kill_tree(pid: Option<u32>, child: &mut Child) {
    kill_process_group(pid);
    // Belt-and-suspenders (and the only path on non-unix): kill the direct child.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// The package a missing binary usually ships in, when it differs from the
/// binary's own name. Told to the model so "not installed" is actionable —
/// `r2` comes from `radare2`, `objdump` from `binutils`.
fn package_hint(program: &str) -> Option<&'static str> {
    match program {
        "r2" | "radare2" => Some("radare2"),
        "objdump" | "nm" | "readelf" | "addr2line" | "strings" => Some("binutils"),
        "pahole" => Some("dwarves"),
        "clangd" | "clang-tidy" | "clang-format" | "llvm-symbolizer" => Some("clang / llvm"),
        _ => None,
    }
}

/// Classify a spawn failure for the model.
///
/// Every case is `RespondToModel`, never `Fatal`: core turns a tool's
/// `Fatal` into `CodexErr::Fatal`, which ends the user's WHOLE TURN — and a
/// program that cannot be started is a fact about THIS command that the model
/// can act on (a header passed as a source, a directory, a file without the
/// exec bit). Measured: `cpp_sanitize source:"hdr.hpp"` (g++ writes a
/// precompiled header, which then cannot be executed) ended the turn.
///
/// `NotFound` is ambiguous, so it is disambiguated here rather than guessed:
/// std reports a failed `chdir` into `cwd` with the same errno as a missing
/// program (a deleted worktree would read as "'g++' is not installed"), and a
/// script whose `#!` interpreter is missing also fails with ENOENT although the
/// script itself exists. A missing shell keeps the pointer at `cpp_bench`
/// local mode.
pub(crate) fn spawn_error(program: &str, cwd: &Path, err: std::io::Error) -> FunctionCallError {
    if !cwd.is_dir() {
        return FunctionCallError::RespondToModel(format!(
            "could not run '{program}': the working directory {} does not exist (was it deleted \
             or moved?). Nothing was run.",
            cwd.display()
        ));
    }
    // A program named by path is resolved against the child's cwd (the chdir
    // happens before the exec); a bare name is looked up on PATH.
    let path = names_a_path(program).then(|| cwd.join(program));
    let msg = match (err.kind(), path) {
        (std::io::ErrorKind::NotFound, None) if program == "bash" || program == "sh" => format!(
            "no POSIX shell found ('{program}' is not on PATH), so shell-based timing / \
             profiling cannot run on this OS. Use cpp_bench local mode (it runs a Google \
             Benchmark binary directly, no shell) instead, or run under WSL."
        ),
        (std::io::ErrorKind::NotFound, None) => {
            let mut msg = format!("'{program}' is not installed / not on PATH.");
            if let Some(package) = package_hint(program) {
                msg.push_str(&format!(" It ships in the '{package}' package."));
            }
            msg
        }
        (std::io::ErrorKind::NotFound, Some(path)) if path.exists() => format!(
            "'{program}' exists but could not be started ({err}): the interpreter named on its \
             #! line, or the dynamic loader it was linked against, is missing. Nothing was run."
        ),
        (std::io::ErrorKind::NotFound, Some(_)) => {
            format!("'{program}' does not exist. Nothing was run.")
        }
        (std::io::ErrorKind::PermissionDenied, Some(path)) if path.is_dir() => {
            format!("'{program}' is a directory, not a program. Nothing was run.")
        }
        (std::io::ErrorKind::PermissionDenied, _) => format!(
            "'{program}' is not executable ({err}): it is not a program with the execute bit \
             set — e.g. a source file, a header's precompiled output, or a file that needs \
             `chmod +x`. Nothing was run."
        ),
        _ if err.raw_os_error() == Some(ENOEXEC) => format!(
            "'{program}' is not a program this machine can run ({err}): a text file without a \
             #! line, or a binary built for another OS or architecture. Nothing was run."
        ),
        _ => format!("could not start '{program}': {err}. Nothing was run."),
    };
    FunctionCallError::RespondToModel(msg)
}

/// Does `program` name a file (`./x`, `/abs/x`, `dir/x`) rather than a command
/// to look up on PATH? The same test execvp applies.
fn names_a_path(program: &str) -> bool {
    program.contains('/') || (cfg!(windows) && program.contains('\\'))
}

/// `ENOEXEC` ("Exec format error"). `std::io::ErrorKind` has no stable variant
/// for it, so it is matched by number. A Windows spawn error carries a Win32
/// code instead, where the same condition is `ERROR_BAD_EXE_FORMAT`.
#[cfg(unix)]
const ENOEXEC: i32 = libc::ENOEXEC;
#[cfg(not(unix))]
const ENOEXEC: i32 = 193;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cwd() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn timeout_from_defaults_and_floors() {
        assert_eq!(
            timeout_from(None),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        assert_eq!(timeout_from(Some(0)), Duration::from_secs(1));
        assert_eq!(timeout_from(Some(42)), Duration::from_secs(42));
    }

    #[test]
    fn compile_timeout_defaults_higher_than_run_and_honours_override() {
        // A build gets more headroom than a run, but an explicit
        // timeout_sec still wins for both.
        assert_eq!(
            compile_timeout_from(None),
            Duration::from_secs(DEFAULT_COMPILE_TIMEOUT_SECS)
        );
        assert!(compile_timeout_from(None) > timeout_from(None));
        assert_eq!(compile_timeout_from(Some(42)), Duration::from_secs(42));
        assert_eq!(compile_timeout_from(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn ok_only_for_finished_success() {
        assert!(
            RunOutcome::Finished {
                success: true,
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            }
            .ok()
        );
        assert!(
            !RunOutcome::Finished {
                success: false,
                code: Some(3),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            }
            .ok()
        );
        assert!(!RunOutcome::TimedOut { secs: 1 }.ok());
    }

    #[test]
    fn missing_shell_error_points_at_bench_local() {
        let err = spawn_error(
            "bash",
            &cwd(),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("cpp_bench local mode"));
                assert!(msg.contains("no POSIX shell"));
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
    }

    #[test]
    fn missing_other_tool_names_it() {
        let err = spawn_error(
            "perf",
            &cwd(),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("'perf' is not installed"))
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
    }

    #[test]
    fn missing_tool_names_its_package_when_it_differs() {
        // `r2` ships as `radare2`, `objdump` as `binutils`: naming only the
        // binary leaves the model with an unactionable error.
        for (program, package) in [
            ("r2", "radare2"),
            ("objdump", "binutils"),
            ("pahole", "dwarves"),
        ] {
            let err = spawn_error(
                program,
                &cwd(),
                std::io::Error::from(std::io::ErrorKind::NotFound),
            );
            let FunctionCallError::RespondToModel(msg) = err else {
                panic!("expected RespondToModel for {program}");
            };
            assert!(msg.contains(package), "{program}: {msg}");
        }
        // No hint invented for a binary whose package matches its name.
        let err = spawn_error(
            "gdb",
            &cwd(),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        let FunctionCallError::RespondToModel(msg) = err else {
            panic!("expected RespondToModel");
        };
        assert!(!msg.contains("package"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn no_spawn_error_ends_the_users_turn() {
        // Core maps a tool's `Fatal` to `CodexErr::Fatal`, which ends the
        // user's whole turn. A program that cannot be started is a fact about
        // THIS command the model can act on, whatever the errno.
        for err in [
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            std::io::Error::from_raw_os_error(libc::ENOEXEC),
            std::io::Error::from_raw_os_error(libc::E2BIG),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ] {
            let shown = err.to_string();
            let err = spawn_error("perf", &cwd(), err);
            assert!(
                matches!(err, FunctionCallError::RespondToModel(_)),
                "{shown} must go back to the model, not end the turn: {err:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_that_is_not_a_program_is_explained_to_the_model() {
        // The shapes seen in use: `cpp_bench binary:"bench.cpp"` (no exec
        // bit), a directory, and a text file WITH the exec bit (ENOEXEC).
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bench.cpp");
        std::fs::write(&source, "int main(){}\n").unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not a binary").unwrap();
        std::fs::set_permissions(&text, std::fs::Permissions::from_mode(0o755)).unwrap();
        let subdir = dir.path().join("adir");
        std::fs::create_dir(&subdir).unwrap();

        for (program, expect) in [
            (&source, "not executable"),
            (&subdir, "is a directory"),
            (&text, "Exec format error"),
        ] {
            let program = program.display().to_string();
            match run_capped(&program, &[], dir.path(), Duration::from_secs(10)).await {
                Err(FunctionCallError::RespondToModel(msg)) => {
                    assert!(msg.contains(expect), "{program}: {msg}");
                    assert!(!msg.contains("not installed"), "{program}: {msg}");
                }
                other => panic!("{program}: expected a model-facing error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_deleted_working_directory_is_named_not_blamed_on_the_tool() {
        // chdir into a vanished directory fails with ENOENT, which used to
        // read as "'g++' is not installed" — sending the model off to install
        // a compiler that is right there.
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("deleted-worktree");
        std::fs::create_dir(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();
        match run_capped(
            "sh",
            &["-c".to_string(), "true".to_string()],
            &gone,
            Duration::from_secs(10),
        )
        .await
        {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("working directory"), "{msg}");
                assert!(msg.contains(&gone.display().to_string()), "{msg}");
                assert!(!msg.contains("not installed"), "{msg}");
                assert!(!msg.contains("no POSIX shell"), "{msg}");
            }
            other => panic!("expected a model-facing error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn captures_stdout_of_a_quick_command() {
        let out = run_capped(
            "bash",
            &["-lc".to_string(), "printf hello".to_string()],
            &cwd(),
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        match out {
            RunOutcome::Finished {
                success,
                stdout,
                truncated,
                ..
            } => {
                assert!(success);
                assert_eq!(stdout, "hello");
                assert!(!truncated, "a tiny output must not be flagged truncated");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    // ---- Byte ceiling on captured output ----------------------------------

    #[tokio::test]
    async fn drain_capped_retains_at_most_the_cap_and_flags_the_cut() {
        // Feed more than the cap through the same code path run_capped uses.
        let big = vec![b'x'; CAPTURE_CAP_BYTES + 4096];
        let mut src: &[u8] = &big;
        let mut buf = Vec::new();
        let truncated = drain_capped(&mut src, &mut buf).await;
        assert!(truncated, "overrun must be reported");
        assert_eq!(buf.len(), CAPTURE_CAP_BYTES, "must retain exactly the cap");
    }

    #[tokio::test]
    async fn drain_capped_keeps_short_streams_whole() {
        let mut src: &[u8] = b"hello";
        let mut buf = Vec::new();
        assert!(!drain_capped(&mut src, &mut buf).await);
        assert_eq!(buf, b"hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn print_loop_binary_cannot_exhaust_engine_memory() {
        // A program that writes far more than the cap must still terminate the
        // call promptly (we keep draining, so the child never blocks) and must
        // come back flagged as truncated — never as a complete run.
        let bytes = CAPTURE_CAP_BYTES + (1 << 20);
        let out = run_capped(
            "bash",
            &[
                "-lc".to_string(),
                format!("head -c {bytes} /dev/zero | tr '\\0' 'a'"),
            ],
            &cwd(),
            Duration::from_secs(60),
        )
        .await
        .expect("run");
        match out {
            RunOutcome::Finished {
                stdout, truncated, ..
            } => {
                assert!(truncated, "must be flagged truncated");
                assert_eq!(stdout.len(), CAPTURE_CAP_BYTES);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_is_finished_not_success() {
        let out = run_capped(
            "bash",
            &["-lc".to_string(), "exit 3".to_string()],
            &cwd(),
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        assert!(matches!(out, RunOutcome::Finished { success: false, .. }));
    }

    /// Is `pid` gone (or a zombie awaiting its reaper)? A killed orphan is
    /// reparented and reaped asynchronously, so callers poll this.
    #[cfg(target_os = "linux")]
    fn process_is_dead(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // The state is the field after the parenthesised command name.
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            Err(_) => true,
        }
    }

    #[cfg(target_os = "linux")]
    async fn wait_until_dead(pid: i32) -> bool {
        for _ in 0..50 {
            if process_is_dead(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn kill_pid(pid: i32) {
        // SAFETY: kill(2) with a signal number; no memory is touched.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_child_holding_the_pipes_does_not_stall_a_finished_run() {
        // The program exits at once, but a child it started keeps stdout
        // open. Waiting for EOF turned that into a full-timeout "RUN TIMED
        // OUT" (300 s by default) with the real exit status and output thrown
        // away.
        let start = std::time::Instant::now();
        let out = run_capped(
            "bash",
            &[
                "-c".to_string(),
                "sleep 30 & echo parent done; exit 7".to_string(),
            ],
            &cwd(),
            Duration::from_secs(20),
        )
        .await
        .expect("run");
        match out {
            RunOutcome::Finished { code, stdout, .. } => {
                assert_eq!(code, Some(7), "the real exit status must survive");
                assert!(stdout.contains("parent done"), "{stdout:?}");
            }
            other => panic!("a finished program must not read as a hang: {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "took {:?}",
            start.elapsed()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn background_processes_do_not_outlive_a_finished_run() {
        // `sleep 20 >/dev/null 2>&1 &` closes the pipes, so the run ends
        // normally — and the process group was only ever killed on timeout,
        // leaving the sleep running after the tool call.
        let out = run_capped(
            "bash",
            &[
                "-c".to_string(),
                "sleep 30 >/dev/null 2>&1 & echo $!".to_string(),
            ],
            &cwd(),
            Duration::from_secs(20),
        )
        .await
        .expect("run");
        let RunOutcome::Finished { stdout, .. } = out else {
            panic!("expected Finished, got {out:?}");
        };
        let pid: i32 = stdout.trim().parse().expect("the background pid");
        let dead = wait_until_dead(pid).await;
        kill_pid(pid);
        assert!(dead, "background process {pid} outlived the finished run");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_detached_process_holding_the_pipes_cannot_stall_the_run_either() {
        // The caveat: a child that detaches with setsid() has left the
        // process group, so killing the group does not close its pipe ends.
        // The run must still come back promptly with the real result.
        let start = std::time::Instant::now();
        let out = run_capped(
            "bash",
            &[
                "-c".to_string(),
                "setsid sleep 30 & echo $!; exit 3".to_string(),
            ],
            &cwd(),
            Duration::from_secs(20),
        )
        .await
        .expect("run");
        let elapsed = start.elapsed();
        let RunOutcome::Finished { code, stdout, .. } = out else {
            panic!("expected Finished, got {out:?}");
        };
        if let Ok(pid) = stdout.trim().parse::<i32>() {
            kill_pid(pid);
        }
        assert_eq!(code, Some(3));
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_can_never_dump_core_into_the_workspace() {
        // Under `ulimit -c unlimited` valgrind wrote a 6.5 MB
        // vgcore.<pid> into the cwd. The HARD limit must be 0 as well, or the
        // program (or valgrind) could simply raise its soft limit back.
        let out = run_capped_no_core(
            "sh",
            &[
                "-c".to_string(),
                "ulimit -S -c unlimited 2>/dev/null; ulimit -c".to_string(),
            ],
            &cwd(),
            Duration::from_secs(10),
        )
        .await
        .expect("run");
        let RunOutcome::Finished { stdout, .. } = out else {
            panic!("expected Finished, got {out:?}");
        };
        assert_eq!(stdout.trim(), "0");
    }

    #[test]
    fn only_a_signal_death_is_a_crash() {
        assert!(crash_note("r2", false, None).is_some());
        assert!(
            crash_note("r2", false, Some(1)).is_none(),
            "a failing exit is not a crash"
        );
        assert!(crash_note("r2", true, Some(0)).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_process_group() {
        // A sleep well past the 1s cap must be killed, and the whole call must
        // return promptly (proving we did not wait for the sleep to finish).
        let start = std::time::Instant::now();
        let out = run_capped(
            "bash",
            &["-lc".to_string(), "sleep 30".to_string()],
            &cwd(),
            Duration::from_secs(1),
        )
        .await
        .expect("run");
        assert!(matches!(out, RunOutcome::TimedOut { secs: 1 }));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kill should be prompt"
        );
    }
}
