//! A persistent `gdb --interpreter=mi3` subprocess the agent drives across tool
//! calls, plus a per-thread registry of such sessions.
//!
//! The hard parts of co-driving GDB interactively (a human at the prompt, posted
//! events, SIGINT) disappear in MI mode: execution commands return `^running`
//! immediately and the stop arrives later as an async `*stopped` record.
//!
//! Concurrency is an actor: a single task owns the child's stdin and the stream
//! of parsed MI records *by value* and processes one command at a time, so no
//! lock is ever held across an `.await` (which this workspace forbids). Callers
//! send a command over a channel and await a one-shot reply.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::parser::AsyncKind;
use super::parser::MiRecord;
use super::parser::StreamKind;
use super::parser::parse_mi_line;
use super::render::render_stop;

/// How long to wait for an ordinary command's `^done`/`^error`.
const CMD_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for a `*stopped` after an execution command went `^running`.
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// Depth of the request queue between callers and the session actor.
const REQUEST_QUEUE: usize = 16;
/// One-time hardening run at session start (mirrors the batch `debug`),
/// plus niceties that improve what the model reads (`pretty`, no pagination).
const HARDEN: [&str; 7] = [
    "set may-call-functions off",
    "set auto-load off",
    "set startup-with-shell off",
    "set confirm off",
    "set pagination off",
    "set print pretty on",
    // MI execution commands (`-exec-continue`) run in the background, so the
    // session keeps reading commands while the program runs.
    "set mi-async on",
];

/// One queued command and the channel to deliver its rendered output.
pub(super) struct Job {
    pub(super) command: String,
    pub(super) reply: oneshot::Sender<String>,
}

/// A thread-scoped collection of live GDB sessions, keyed by a small integer id.
pub struct GdbRegistry {
    sessions: Mutex<HashMap<u64, Arc<GdbSession>>>,
    next_id: AtomicU64,
}

impl GdbRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn insert(&self, session: Arc<GdbSession>) {
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(session.id, session);
        }
    }

    pub fn get(&self, id: u64) -> Option<Arc<GdbSession>> {
        self.sessions.lock().ok()?.get(&id).cloned()
    }

    pub fn remove(&self, id: u64) -> Option<Arc<GdbSession>> {
        self.sessions.lock().ok()?.remove(&id)
    }

    /// `(id, program, state)` for every open session, ordered by id.
    pub fn summaries(&self) -> Vec<(u64, String, String)> {
        let Ok(map) = self.sessions.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<(u64, String, String)> = map
            .values()
            .map(|s| (s.id, s.program.clone(), s.state_line()))
            .collect();
        rows.sort_by_key(|row| row.0);
        rows
    }
}

/// Handle to a single owned GDB/MI session. Dropping it (e.g. via
/// [`GdbRegistry::remove`]) closes the request channel, which tells the actor to
/// exit GDB cleanly and reap the process.
pub struct GdbSession {
    pub id: u64,
    pub program: String,
    /// `gdb`, or `lldb` (see `lldb.rs`): which debugger, and so which command
    /// syntax, this session speaks.
    pub backend: &'static str,
    /// Coarse state for `list`, updated by the actor/reader: starting/running/
    /// stopped/exited/ended.
    state: Arc<Mutex<String>>,
    requests: mpsc::Sender<Job>,
}

impl GdbSession {
    /// Spawn `gdb -i=mi3`, harden it, and (optionally) load a program/core and
    /// program arguments. Returns a ready session or a model-facing error string.
    pub async fn start(
        id: u64,
        program: Option<&Path>,
        core: Option<&Path>,
        prog_args: &[String],
        cwd: &Path,
    ) -> Result<Arc<GdbSession>, String> {
        Self::start_with("gdb", id, program, core, prog_args, cwd, None).await
    }

    /// [`Self::start`] for a core from another machine: shared libraries are
    /// looked up under `sysroot` (`set sysroot`) when the core is loaded.
    pub async fn start_with_sysroot(
        id: u64,
        program: Option<&Path>,
        core: Option<&Path>,
        prog_args: &[String],
        cwd: &Path,
        sysroot: Option<&Path>,
    ) -> Result<Arc<GdbSession>, String> {
        Self::start_with("gdb", id, program, core, prog_args, cwd, sysroot).await
    }

    /// A session whose actor is another backend's (`lldb.rs`).
    pub(super) fn from_parts(
        id: u64,
        program: String,
        backend: &'static str,
        state: Arc<Mutex<String>>,
        requests: mpsc::Sender<Job>,
    ) -> Arc<GdbSession> {
        Arc::new(GdbSession {
            id,
            program,
            backend,
            state,
            requests,
        })
    }

    /// [`Self::start`] with the gdb binary named (a test drives a fake one).
    async fn start_with(
        gdb: &str,
        id: u64,
        program: Option<&Path>,
        core: Option<&Path>,
        prog_args: &[String],
        cwd: &Path,
        sysroot: Option<&Path>,
    ) -> Result<Arc<GdbSession>, String> {
        // Refuse an argument list gdb cannot pass exactly BEFORE spawning.
        let inferior = crate::gdb_common::inferior_args(prog_args)?;
        // Debuggers are not inside the tool sandbox yet; this records the
        // note the output carries (and refuses a tool without a profile).
        crate::sandbox::wrap(gdb, &[], cwd).map_err(crate::sandbox::error_text)?;
        let mut command = Command::new(gdb);
        command.arg("--interpreter=mi3").arg("-q").arg("-nx");
        if inferior.needs_shell {
            let (key, value) = crate::gdb_common::INFERIOR_SHELL;
            command.env(key, value);
        }
        if let Some(program) = program {
            command.arg(program);
        }
        // The program runs where gdb does. Without this it ran in the engine's
        // own directory, not the workspace the batch `debug` tool uses, so a
        // relative data file (`data/trace_evict.txt` — common in real calls)
        // opened in one tool and was "cannot open" in the other.
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Kept: when gdb cannot start an MI session, why is only
            // ever said here ("Interpreter `mi3' unrecognized").
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // An interactive session cannot use `run_capped`, but it needs
        // the same process-group isolation. `kill_on_drop` reaps gdb; gdb's
        // *inferior* — the program under debug — is a grandchild and survived,
        // still holding whatever the debug session had it running.
        #[cfg(unix)]
        command.process_group(0);
        // With its own terminal (`-inferior-tty-set`), gdb makes the
        // program a session leader whose controlling tty is that pty. When
        // such a leader exits, the kernel sends SIGHUP to its process group,
        // so a forked worker was killed the moment `main` returned — a shell
        // run and the batch `debug` let it finish. gdb hands the dispositions
        // it started with to the program, so start gdb with SIGHUP ignored,
        // as `nohup` does. (A program that installs its own handler keeps it.)
        #[cfg(unix)]
        // SAFETY: runs in the forked child before exec and only makes the
        // signal(2) system call, which takes no locks and allocates nothing.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                "gdb not installed".to_string()
            } else {
                format!("failed to start gdb: {err}")
            }
        })?;
        let stdin = child.stdin.take().ok_or("gdb stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("gdb stdout unavailable")?;
        let stderr_text = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(collect_stderr(stderr, stderr_text.clone()));
        }

        let state = Arc::new(Mutex::new("starting".to_string()));
        let inferior_pid = Arc::new(AtomicI32::new(0));
        let (record_tx, record_rx) = mpsc::unbounded_channel();
        tokio::spawn(reader_loop(
            stdout,
            record_tx,
            state.clone(),
            inferior_pid.clone(),
        ));

        // The program gets its own terminal (see `inferior_tty`). If one cannot
        // be opened, it shares gdb's stdio as before rather than failing.
        #[cfg(target_os = "linux")]
        let tty = super::inferior_tty::InferiorTty::open().ok();
        #[cfg(not(target_os = "linux"))]
        let tty: Option<NoTty> = None;
        let tty_path = tty.as_ref().map(|tty| tty.path().to_string());

        let (request_tx, request_rx) = mpsc::channel(REQUEST_QUEUE);
        tokio::spawn(session_actor(
            request_rx,
            record_rx,
            stdin,
            child,
            state.clone(),
            tty,
            inferior_pid,
        ));

        let session = Arc::new(GdbSession {
            id,
            program: program
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no program)".to_string()),
            backend: "gdb",
            state,
            requests: request_tx,
        });

        // The first command proves gdb is alive and speaking MI. A gdb
        // that exited at once (gdb 8 rejects `--interpreter=mi3`) or never
        // answers used to come back "Ready" — every later command failed
        // with "not responding", and a hung one took 8 x 15 s to get there.
        let first = session.command(HARDEN[0]).await;
        if let Some(reason) = startup_failure(&first, &session.state_line()) {
            // Let gdb's last words reach the collector.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let said = stderr_text
                .lock()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let said = if said.is_empty() {
                String::new()
            } else {
                format!(" gdb said: {said}")
            };
            return Err(format!(
                "gdb did not start a debugging session ({reason}); nothing was started.{said}"
            ));
        }
        for cmd in &HARDEN[1..] {
            let _ = session.command(cmd).await;
        }
        if let Some(path) = tty_path {
            let _ = session.command(&format!("-inferior-tty-set {path}")).await;
        }
        for cmd in &inferior.commands {
            let _ = session.command(cmd).await;
        }
        if let Some(root) = sysroot {
            // Before the core: its shared libraries are resolved when it loads.
            let _ = session
                .command(&format!("set sysroot {}", root.display()))
                .await;
            let debug = root.join("usr/lib/debug");
            if debug.is_dir() {
                let _ = session
                    .command(&format!("set debug-file-directory {}", debug.display()))
                    .await;
            }
        }
        if let Some(core) = core {
            let _ = session
                .command(&format!("core-file {}", core.display()))
                .await;
        }
        // Never over an `ended`/`exited` the reader has already recorded.
        if let Ok(mut state) = session.state.lock()
            && *state == "starting"
        {
            *state = "ready".to_string();
        }
        Ok(session)
    }

    fn state_line(&self) -> String {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Run one GDB command and return its output as clean console text. For an
    /// execution command (`run`/`continue`/`step`/…) the returned text leads
    /// with a one-line stop summary.
    pub async fn command(&self, raw: &str) -> String {
        let command = raw.trim();
        if command.is_empty() {
            return "(empty command)".to_string();
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = Job {
            command: command.to_string(),
            reply: reply_tx,
        };
        if self.requests.send(job).await.is_err() {
            return format!("{} session has ended.", self.backend);
        }
        reply_rx
            .await
            .unwrap_or_else(|_| format!("{} session ended before responding.", self.backend))
    }
}

/// The session actor: owns stdin and the record stream and runs one job at a
/// time. When the request channel closes (the session was dropped), it asks GDB
/// to exit cleanly and reaps the child.
async fn session_actor(
    mut requests: mpsc::Receiver<Job>,
    mut records: mpsc::UnboundedReceiver<MiRecord>,
    mut stdin: ChildStdin,
    mut child: Child,
    state: Arc<Mutex<String>>,
    tty: Option<Tty>,
    inferior_pid: Arc<AtomicI32>,
) {
    let mut token: u64 = 1;
    while let Some(job) = requests.recv().await {
        if let Some(tty) = &tty {
            if starts_a_program(&job.command) {
                tty.reset();
            }
        }
        let (mut output, ran) =
            run_exchange(&mut stdin, &mut records, &state, token, &job.command).await;
        token += 1;
        if let Some(tty) = &tty {
            if ran {
                // The kernel hands a pty write to the reading side
                // asynchronously; give output written just before the stop a
                // moment to arrive.
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            if let Some(text) = tty.take() {
                append_line(&mut output, "program output (stdout+stderr):");
                output.push('\n');
                output.push_str(&tail(&text, PROGRAM_OUTPUT_CAP));
            }
        }
        let _ = job.reply.send(output);
    }
    let _ = stdin.write_all(b"-gdb-exit\n").await;
    let _ = stdin.flush().await;
    // With its own pty the program is a session leader in its OWN process
    // group, so killing gdb's group below never reached it — a program that
    // was `detach`ed, or that handles SIGHUP, kept running after `stop`,
    // reparented to init. Its group goes first, if it still holds our pty.
    #[cfg(target_os = "linux")]
    if let Some(tty) = &tty {
        kill_program_on_tty(inferior_pid.load(Ordering::SeqCst), tty.path());
    }
    #[cfg(not(target_os = "linux"))]
    let _ = &inferior_pid;
    // Take the whole group down, not just gdb — see the `process_group(0)` note
    // at spawn.
    crate::run::kill_process_group(child.id());
    let _ = child.start_kill();
}

/// SIGKILL the process group of `pid` — but only while that process still has
/// `tty_path` as its controlling terminal. The pid is remembered from the
/// program's start and may have exited since; the terminal check means a
/// recycled pid belonging to someone else is never signalled.
#[cfg(target_os = "linux")]
fn kill_program_on_tty(pid: i32, tty_path: &str) {
    use std::os::unix::fs::MetadataExt as _;
    if pid <= 0 {
        return;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return;
    };
    // Fields after the parenthesised command name: state ppid pgrp session tty_nr.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let (Some(pgrp), Some(tty_nr)) = (
        fields.get(2).and_then(|v| v.parse::<i32>().ok()),
        fields.get(4).and_then(|v| v.parse::<u64>().ok()),
    ) else {
        return;
    };
    let Ok(meta) = std::fs::metadata(tty_path) else {
        return;
    };
    let rdev = meta.rdev();
    let (major, minor) = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
    // The kernel's /proc encoding of a device number.
    let ours = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
    if tty_nr != ours || pgrp <= 0 {
        return;
    }
    // SAFETY: kill(2) with a process-group id and a signal number.
    unsafe {
        libc::kill(-pgrp, libc::SIGKILL);
    }
}

/// Why a session's first command shows gdb never became a usable MI session.
fn startup_failure(reply: &str, state: &str) -> Option<&'static str> {
    if state == "ended" || reply.starts_with("gdb session has ended") {
        Some("gdb exited at once")
    } else if reply.starts_with("gdb session is not responding") {
        Some("gdb closed its input")
    } else if reply.starts_with("(gdb did not respond") {
        Some("gdb did not answer")
    } else {
        None
    }
}

/// Keep the tail of gdb's stderr for a startup failure; drain the rest so a
/// chatty gdb never blocks on a full pipe.
async fn collect_stderr(mut stderr: tokio::process::ChildStderr, into: Arc<Mutex<String>>) {
    const KEEP: usize = 2000;
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        if let Ok(mut text) = into.lock() {
            text.push_str(&String::from_utf8_lossy(&chunk[..n]));
            if text.len() > KEEP {
                let mut cut = text.len() - KEEP;
                while !text.is_char_boundary(cut) {
                    cut += 1;
                }
                text.drain(..cut);
            }
        }
    }
}

/// Send one command and collect its result as rendered text, plus whether the
/// program ran (an execution command, or a stop was seen).
async fn run_exchange(
    stdin: &mut ChildStdin,
    records: &mut mpsc::UnboundedReceiver<MiRecord>,
    state: &Arc<Mutex<String>>,
    token: u64,
    command: &str,
) -> (String, bool) {
    // Surface a stop left unconsumed by a prior execution that timed out while
    // still running, so it isn't lost.
    let mut stop_summary = None;
    while let Ok(rec) = records.try_recv() {
        if let MiRecord::Async {
            kind: AsyncKind::Exec,
            class,
            payload,
        } = &rec
        {
            if class == "stopped" {
                stop_summary = Some(render_stop(payload));
                set(state, super::render::state_after_stop(payload));
            }
        }
    }

    let command = &in_background(command);
    // An MI command must follow its token with no space (`7-exec-interrupt`);
    // with one, gdb reads `-exec-interrupt` as an unknown CLI command — which
    // is how the "Send `-exec-interrupt`" advice below never worked. A CLI
    // command takes the space.
    let line = if command.starts_with('-') {
        format!("{token}{command}\n")
    } else {
        format!("{token} {command}\n")
    };
    if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
        return (
            "gdb session is not responding (write failed).".to_string(),
            false,
        );
    }

    let mut body = String::new();
    // gdb's log stream (`&`). It echoes each CLI command there, but it also
    // carries messages for commands that still end in `^done` — `break
    // uni.cpp:4` reports "No line 4 in file" only here, and the model was told
    // "(no output)" and believed the breakpoint was set.
    let mut log = String::new();
    let mut error = None;
    let mut exec_running = false;
    let mut wait = CMD_TIMEOUT;
    loop {
        match timeout(wait, records.recv()).await {
            Err(_) => {
                let note = if exec_running {
                    format!(
                        "Program is still running (no stop within {}s). Send `interrupt` to stop it \
                         where it is, or any other command later to see whether it has stopped. \
                         (Repeating an execution command would start it over.)",
                        EXEC_TIMEOUT.as_secs()
                    )
                } else {
                    format!("(gdb did not respond within {}s)", CMD_TIMEOUT.as_secs())
                };
                append_line(&mut body, &note);
                break;
            }
            Ok(None) => return ("gdb session has ended.".to_string(), exec_running),
            Ok(Some(rec)) => match rec {
                MiRecord::Stream {
                    kind: StreamKind::Console | StreamKind::Target,
                    text,
                } => body.push_str(&text),
                MiRecord::Stream {
                    kind: StreamKind::Log,
                    text,
                } => {
                    if text.trim() != command.trim() {
                        log.push_str(&text);
                    }
                }
                // Raw program output, when the program shares gdb's stdout
                // (no terminal of its own).
                MiRecord::Other(line) => append_line(&mut body, &line),
                MiRecord::Result {
                    token: Some(t),
                    class,
                    payload,
                } if t == token => match class.as_str() {
                    "running" => {
                        exec_running = true;
                        wait = EXEC_TIMEOUT;
                        set(state, "running");
                    }
                    "error" => {
                        let msg = payload
                            .get("msg")
                            .and_then(|v| v.as_str())
                            .unwrap_or("command failed");
                        error = Some(msg.to_string());
                        break;
                    }
                    "exit" => {
                        append_line(&mut body, "(gdb exited)");
                        break;
                    }
                    // An interrupt is done once it is sent; the stop it causes
                    // arrives after, and is what the model asked for.
                    "done" if stops_the_program(command) && stop_summary.is_none() => {
                        exec_running = true;
                    }
                    // "done", "connected", … — the command is complete.
                    _ => break,
                },
                MiRecord::Async {
                    kind: AsyncKind::Exec,
                    class,
                    payload,
                } if class == "stopped" => {
                    stop_summary = Some(render_stop(&payload));
                    // Not "stopped" for an exited program: `list` showed a dead
                    // program as [stopped].
                    set(state, super::render::state_after_stop(&payload));
                    if exec_running {
                        break;
                    }
                }
                _ => {}
            },
        }
    }

    // The log repeats an error's own message; show it once, as the error.
    for line in log.lines() {
        if error
            .as_deref()
            .is_some_and(|msg| msg.trim() == line.trim())
        {
            continue;
        }
        append_line(&mut body, line);
    }
    if let Some(msg) = &error {
        append_line(&mut body, &format!("error: {msg}"));
    }
    let ran = exec_running || stop_summary.is_some();
    (
        crate::gdb_common::truncate(compose(stop_summary, body), crate::gdb_common::OUTPUT_CAP),
        ran,
    )
}

/// Read MI lines until EOF, updating coarse state and forwarding every record.
async fn reader_loop(
    stdout: ChildStdout,
    tx: mpsc::UnboundedSender<MiRecord>,
    state: Arc<Mutex<String>>,
    inferior_pid: Arc<AtomicI32>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let rec = parse_mi_line(&line);
        // Remember the running program's pid (`=thread-group-started`),
        // so `stop` can take down its own process group.
        if let MiRecord::Async {
            kind: AsyncKind::Notify,
            class,
            payload,
        } = &rec
        {
            // Kept after the program exits or is detached: `stop` checks the
            // process still holds THIS session's pty before signalling it.
            if class == "thread-group-started" {
                let pid = payload
                    .get("pid")
                    .and_then(|v| v.as_str())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                inferior_pid.store(pid, Ordering::SeqCst);
            }
        }
        if let MiRecord::Async {
            kind: AsyncKind::Exec,
            class,
            payload,
        } = &rec
        {
            if class == "running" {
                set(&state, "running");
            } else if class == "stopped" {
                let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                set(
                    &state,
                    if reason.starts_with("exited") {
                        "exited"
                    } else {
                        "stopped"
                    },
                );
            }
        }
        if tx.send(rec).is_err() {
            return;
        }
    }
    // EOF: the process is gone.
    if let Ok(mut s) = state.lock() {
        if *s != "exited" {
            *s = "ended".to_string();
        }
    }
}

fn set(state: &Arc<Mutex<String>>, value: &str) {
    if let Ok(mut s) = state.lock() {
        *s = value.to_string();
    }
}

/// Join an optional stop summary with the captured body into final output.
fn compose(stop_summary: Option<String>, body: String) -> String {
    let body = body.trim();
    match stop_summary {
        Some(summary) if body.is_empty() => summary,
        Some(summary) => format!("{summary}\n{body}"),
        None if body.is_empty() => "(no output)".to_string(),
        None => body.to_string(),
    }
}

/// First words of the CLI commands that resume the program.
const EXECUTION_COMMANDS: [&str; 22] = [
    "run", "r", "start", "starti", "continue", "c", "fg", "next", "n", "step", "s", "stepi", "si",
    "nexti", "ni", "finish", "fin", "until", "u", "advance", "jump", "signal",
];

/// Run an execution command in the background (`continue` -> `continue&`).
///
/// Measured on gdb 13: a foreground `run`/`continue` blocks gdb's command input
/// until the program stops by itself, in every MI mode. A program that loops
/// then wedged the session: `interrupt` and `-exec-interrupt` sat unread in
/// the pipe, and the old advice to "repeat the command to keep waiting" queued a
/// second `run`, which restarted the program. In the background the command
/// returns `^running` at once, the stop still arrives as `*stopped` (which
/// [`run_exchange`] waits for), and an interrupt is read and acted on.
fn in_background(command: &str) -> String {
    let command = command.trim_end();
    let first = command.split_whitespace().next().unwrap_or("");
    if EXECUTION_COMMANDS.contains(&first) && !command.ends_with('&') {
        format!("{command}&")
    } else {
        command.to_string()
    }
}

/// Commands that start the program afresh (the terminal is reset first).
fn starts_a_program(command: &str) -> bool {
    matches!(
        command.split_whitespace().next(),
        Some("run" | "r" | "start" | "starti" | "-exec-run")
    )
}

/// Commands whose result is a stop that arrives after their `^done`.
fn stops_the_program(command: &str) -> bool {
    matches!(
        command.split_whitespace().next(),
        Some("interrupt" | "-exec-interrupt")
    )
}

/// Program output shown with one command, beyond gdb's own [`OUTPUT_CAP`]:
/// the tail, where a crash's last words are.
///
/// [`OUTPUT_CAP`]: crate::gdb_common::OUTPUT_CAP
const PROGRAM_OUTPUT_CAP: usize = 3000;

#[cfg(target_os = "linux")]
type Tty = super::inferior_tty::InferiorTty;
#[cfg(not(target_os = "linux"))]
type Tty = NoTty;

/// Stand-in where the program cannot get a terminal of its own.
#[cfg(not(target_os = "linux"))]
struct NoTty;

#[cfg(not(target_os = "linux"))]
impl NoTty {
    fn path(&self) -> &str {
        ""
    }

    fn take(&self) -> Option<String> {
        None
    }

    fn reset(&self) {}
}

/// The last `cap` bytes of `text` (on a char boundary), marked when cut.
fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {start} earlier bytes not shown]\n{}", &text[start..])
}

/// Append a line to a buffer, inserting a separating newline when needed.
fn append_line(buf: &mut String, line: &str) {
    if !buf.is_empty() && !buf.ends_with('\n') {
        buf.push('\n');
    }
    buf.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn compose_summary_and_body() {
        assert_eq!(
            compose(Some("Stopped: x".to_string()), "  line\n".to_string()),
            "Stopped: x\nline"
        );
    }

    #[test]
    fn compose_summary_only() {
        assert_eq!(
            compose(Some("Stopped: x".to_string()), "  \n".to_string()),
            "Stopped: x"
        );
    }

    #[test]
    fn compose_body_only_and_empty() {
        assert_eq!(compose(None, "hello\n".to_string()), "hello");
        assert_eq!(compose(None, "   ".to_string()), "(no output)");
    }

    #[test]
    fn append_line_inserts_separator() {
        let mut buf = String::from("a");
        append_line(&mut buf, "b");
        assert_eq!(buf, "a\nb");
        let mut fresh = String::new();
        append_line(&mut fresh, "first");
        assert_eq!(fresh, "first");
    }

    #[test]
    fn execution_commands_run_in_the_background() {
        for (sent, want) in [
            ("run", "run&"),
            ("r", "r&"),
            ("continue 3", "continue 3&"),
            ("next", "next&"),
            ("finish", "finish&"),
            ("until 12", "until 12&"),
            ("start", "start&"),
            ("run&", "run&"),
            ("continue &", "continue &"),
        ] {
            assert_eq!(in_background(sent), want, "{sent}");
        }
        for sent in [
            "bt",
            "print n",
            "break main",
            "info locals",
            "-exec-continue",
            "interrupt",
        ] {
            assert_eq!(in_background(sent), sent, "{sent}");
        }
        assert!(stops_the_program("interrupt") && stops_the_program("-exec-interrupt"));
        assert!(!stops_the_program("info threads"));
    }

    /// Compile `source` into `dir` and start a session on it; `None` (skip)
    /// when gdb or a C compiler is missing.
    async fn session_on(dir: &Path, source: &str) -> Option<Arc<GdbSession>> {
        use crate::program_args::fixture::have;
        if !have("gdb") || !have("cc") {
            eprintln!("skip: needs gdb and cc");
            return None;
        }
        let src = dir.join("prog.c");
        let bin = dir.join("prog");
        std::fs::write(&src, source).unwrap();
        let built = std::process::Command::new("cc")
            .args(["-g", "-O0", "-o"])
            .arg(&bin)
            .arg(&src)
            .status()
            .unwrap();
        assert!(built.success());
        Some(
            GdbSession::start(1, Some(&bin), None, &[], dir)
                .await
                .expect("start"),
        )
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_program_has_its_own_stdin_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let Some(session) = session_on(
            dir.path(),
            "#include <stdio.h>\nint main(void){ char b[64];\n\
             printf(\"Enter value: \"); fflush(stdout);\n\
             if (fgets(b, sizeof b, stdin)) printf(\"got %s\", b); else printf(\"stdin EOF\\n\");\n\
             fprintf(stderr, \"to stderr\\n\"); return 0; }\n",
        )
        .await
        else {
            return;
        };
        let run = session.command("run").await;
        // Before: the output was discarded, stderr went to /dev/null, and
        // fgets blocked the session for 30 s reading the MI command pipe.
        assert!(run.contains("Program exited normally"), "{run}");
        assert!(run.contains("Enter value: stdin EOF"), "{run}");
        assert!(run.contains("to stderr"), "{run}");
        // The next command reached gdb, not the program's stdin.
        let print = session.command("print 1234").await;
        assert!(print.contains("$1 = 1234"), "{print}");
    }

    #[tokio::test]
    async fn mi_commands_reach_gdb_as_mi_commands() {
        let dir = tempfile::tempdir().unwrap();
        let Some(session) = session_on(
            dir.path(),
            "int twice(int x){ return x * 2; }\nint main(void){ return twice(1) - 2; }\n",
        )
        .await
        else {
            return;
        };
        session.command("break main").await;
        session.command("break twice").await;
        let run = session.command("run").await;
        assert!(run.contains("main"), "{run}");
        // `N -exec-continue` (with a space) was an unknown CLI command.
        let cont = session.command("-exec-continue").await;
        assert!(cont.contains("twice"), "{cont}");
        assert!(!cont.contains("Undefined command"), "{cont}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_program_that_changes_the_terminal_does_not_block_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        // Run 1 switches the terminal to line mode (like a password prompt)
        // and exits; run 2 only reads stdin, and must see EOF, not block.
        let Some(session) = session_on(
            dir.path(),
            "#include <stdio.h>\n#include <stdlib.h>\n#include <termios.h>\n\
             int main(int argc, char **argv){ if (argc > 1) { struct termios t;\n\
             tcgetattr(0, &t); t.c_lflag |= ICANON; t.c_cc[VMIN] = 1; tcsetattr(0, TCSANOW, &t);\n\
             return 0; }\n\
             char b[8]; puts(fgets(b, sizeof b, stdin) ? \"input\" : \"stdin EOF\"); return 0; }\n",
        )
        .await
        else {
            return;
        };
        let first = session.command("run switch").await;
        assert!(first.contains("exited normally"), "{first}");
        // gdb would reuse `switch` for a bare `run`.
        session.command("set args").await;
        let second = session.command("run").await;
        assert!(second.contains("stdin EOF"), "{second}");
    }

    #[tokio::test]
    async fn the_program_runs_in_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "from the workspace\n").unwrap();
        let Some(session) = session_on(
            dir.path(),
            "#include <stdio.h>\nint main(void){ FILE *f = fopen(\"data.txt\", \"r\");\n\
             return f ? 0 : 7; }\n",
        )
        .await
        else {
            return;
        };
        // The test process runs in the crate directory, not `dir`.
        let run = session.command("run").await;
        assert!(run.contains("exited normally"), "{run}");
    }

    #[tokio::test]
    async fn gdbs_log_stream_messages_reach_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let Some(session) = session_on(dir.path(), "int main(void){ return 0; }\n").await else {
            return;
        };
        // gdb answers `^done` here: the only word of failure is on `&`.
        let brk = session.command("break prog.c:999").await;
        assert!(brk.contains("No line 999"), "{brk}");
        // An error's message is shown once, not also echoed from the log.
        let print = session.command("print nosuchvar").await;
        assert_eq!(print.matches("No symbol").count(), 1, "{print}");
    }

    #[test]
    fn registry_assigns_increasing_ids() {
        let reg = GdbRegistry::new();
        assert_eq!(reg.next_id(), 1);
        assert_eq!(reg.next_id(), 2);
        assert!(reg.get(99).is_none());
        assert!(reg.remove(99).is_none());
        assert!(reg.summaries().is_empty());
    }

    /// Real-gdb end-to-end drive of the actor: compile a tiny program, start a
    /// session on it, set a breakpoint, run, and inspect state through the
    /// actual MI actor path. Skips cleanly when gdb/gcc are unavailable.
    #[tokio::test]
    async fn drives_real_gdb_end_to_end() {
        let Some(cc) = ["cc", "gcc"].into_iter().find(|c| {
            std::process::Command::new(c)
                .arg("--version")
                .output()
                .is_ok()
        }) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("demo.c");
        let bin = dir.path().join("demo");
        std::fs::write(
            &src,
            "int add(int a,int b){int s=a+b;return s;}\n\
             int main(void){int x=41;int y=add(x,1);return y-y;}\n",
        )
        .unwrap();
        let compiled = std::process::Command::new(cc)
            .args(["-g", "-O0", "-o"])
            .arg(&bin)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(compiled, "compile demo.c");

        let session = match GdbSession::start(1, Some(&bin), None, &[], dir.path()).await {
            Ok(session) => session,
            Err(err) => {
                eprintln!("skip: gdb unavailable ({err})");
                return;
            }
        };

        let brk = session.command("break add").await;
        assert!(
            brk.to_lowercase().contains("breakpoint"),
            "break add: {brk}"
        );
        // Execution command: leads with a one-line stop summary from render_stop.
        let run = session.command("run").await;
        assert!(
            run.to_lowercase().contains("add") || run.to_lowercase().contains("breakpoint"),
            "run: {run}"
        );
        let val = session.command("print a").await;
        assert!(val.contains("41"), "print a: {val}");
        drop(session); // actor sends -gdb-exit and reaps the child
    }

    #[tokio::test]
    async fn real_session_passes_every_argument_byte_for_byte() {
        use crate::program_args::fixture;
        if !fixture::have("gdb") {
            eprintln!("skip: gdb not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let cases = fixture::hard_cases();
        for (id, args) in cases.iter().enumerate() {
            let session = GdbSession::start(id as u64 + 1, Some(&program), None, args, dir.path())
                .await
                .expect("start");
            let _ = session.command("run").await;
            drop(session);
        }
        let runs = fixture::runs(dir.path());
        assert_eq!(runs.len(), cases.len(), "{runs:?}");
        for (run, want) in runs.iter().zip(&cases) {
            assert_eq!(&run.args, want);
            assert_eq!(run.shell, std::env::var("SHELL").ok(), "{want:?}");
        }
    }

    #[tokio::test]
    async fn an_argument_gdb_cannot_pass_is_refused_before_gdb_starts() {
        let err = GdbSession::start(1, None, None, &["a\nb".to_string()], Path::new("/"))
            .await
            .err()
            .expect("refused");
        assert!(err.contains("line break"), "{err}");
    }
    #[test]
    fn a_first_reply_that_shows_no_mi_session_is_a_startup_failure() {
        assert!(startup_failure("gdb session has ended.", "ended").is_some());
        assert!(
            startup_failure("gdb session is not responding (write failed).", "starting").is_some()
        );
        assert!(startup_failure("(gdb did not respond within 15s)", "starting").is_some());
        assert!(startup_failure("(no output)", "starting").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_gdb_that_cannot_speak_mi3_is_an_error_not_ready() {
        // gdb 8 rejects `--interpreter=mi3` and exits; `start` said
        // "Ready" and every later command failed "not responding".
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("oldgdb");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"Interpreter \\`mi3' unrecognized\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = match GdbSession::start_with(
            &fake.display().to_string(),
            1,
            None,
            None,
            &[],
            dir.path(),
            None,
        )
        .await
        {
            Ok(_) => panic!("a gdb that exited must not be a ready session"),
            Err(err) => err,
        };
        assert!(err.contains("did not start"), "{err}");
        assert!(
            err.contains("unrecognized"),
            "gdb's own reason must reach the model: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_forked_worker_outlives_its_parent_under_the_debugger() {
        // With the program a session leader on its pty, SIGHUP killed a
        // forked worker the moment main returned; a shell run lets it finish.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("worker-finished");
        let Some(session) = session_on(
            dir.path(),
            &format!(
                "#include <stdio.h>\n#include <unistd.h>\nint main(void){{\n\
                 if (fork() == 0) {{ usleep(300000); FILE *f = fopen(\"{}\", \"w\"); \
                 if (f) {{ fputs(\"done\", f); fclose(f); }} return 0; }}\n\
                 return 0; }}\n",
                marker.display()
            ),
        )
        .await
        else {
            return;
        };
        let run = session.command("run").await;
        assert!(run.contains("exited"), "{run}");
        for _ in 0..30 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(marker.exists(), "the worker was killed before it finished");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stopping_the_session_ends_a_detached_program_too() {
        // A detached program that handles SIGHUP kept running after
        // `stop`, reparented to init — it is in its OWN process group.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let Some(session) = session_on(
            dir.path(),
            &format!(
                "#include <stdio.h>\n#include <signal.h>\n#include <unistd.h>\n\
                 static void on_hup(int s){{ (void)s; }}\nvoid ready(void){{}}\nint main(void){{\n\
                 signal(SIGHUP, on_hup);\n\
                 FILE *f = fopen(\"{}\", \"w\"); fprintf(f, \"%d\", (int)getpid()); fclose(f);\n\
                 ready();\n for (;;) pause(); }}\n",
                pid_file.display()
            ),
        )
        .await
        else {
            return;
        };
        let _ = session.command("break ready").await;
        let run = session.command("run").await;
        assert!(
            run.contains("Breakpoint") || run.contains("stopped"),
            "{run}"
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("the program wrote its pid")
            .trim()
            .parse()
            .unwrap();
        let _ = session.command("detach").await;
        drop(session);
        let mut gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|stat| {
                    !stat
                        .rsplit_once(')')
                        .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            if !alive {
                gone = true;
                break;
            }
        }
        if !gone {
            // SAFETY: kill(2); cleanup so a failure leaves no daemon behind.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(gone, "the detached program outlived `stop`");
    }
}
