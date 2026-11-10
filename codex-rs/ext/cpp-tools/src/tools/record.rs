use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;

use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;

const TOOL_NAME: &str = "debug_record";
/// Cap on the captured replay output handed back to the model.
const OUTPUT_CAP: usize = 8000;
/// gdb commands run during replay when the caller does not supply their own.
/// A small, useful reverse-debugging starting set.
const DEFAULT_COMMANDS: [&str; 2] = ["continue", "bt"];
/// Recording attempts in `mode="chaos"` when the caller does not say. A 1-in-20
/// failure is caught with ~90% probability in 45 runs, but each run records a
/// full trace, so this is the pragmatic default and the model can raise it.
const CHAOS_DEFAULT_REPEAT: u32 = 12;
/// Hard ceiling on recording attempts: one tool call must not run unbounded.
const MAX_REPEAT: u32 = 50;

pub struct RecordTool {
    pub cwd: PathBuf,
    /// `rr record` runs the program natively. See [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    program: String,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    commands: Option<Vec<String>>,
    /// "replay" (default) = record once, then replay. "chaos" = record under
    /// rr's chaos scheduler, repeatedly, until the run FAILS — the way a rare
    /// race is caught on purpose instead of by luck.
    #[serde(default)]
    mode: Option<String>,
    /// How many recording attempts to make while hunting an intermittent
    /// failure. Defaults to 1 in "replay" mode and [`CHAOS_DEFAULT_REPEAT`] in
    /// "chaos" mode; capped at [`MAX_REPEAT`] so one call cannot run forever.
    #[serde(default, deserialize_with = "whole_number")]
    repeat: Option<u32>,
}

/// The schema says "number": `3.0` is a whole number, and was refused as
/// "invalid type: floating point". A fraction or a negative still is.
fn whole_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    use serde::de::Error;
    match Option::<f64>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) if value.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&value) => {
            Ok(Some(value as u32))
        }
        Some(value) => Err(D::Error::custom(format!(
            "expected a whole number, got {value}"
        ))),
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for RecordTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "program".to_string(),
            JsonSchema::string(Some(
                "Path to the executable to record, relative to the workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "args".to_string(),
            JsonSchema::string(Some(
                "Optional program arguments, written like a shell command line: \
                 space-separated, quote with '…' or \"…\" to keep spaces, \"\" is an empty \
                 argument. No shell runs: pipes, redirection and $VAR are rejected."
                    .to_string(),
            )),
        );
        properties.insert(
            "commands".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(format!(
                    "Optional gdb commands to run during deterministic replay \
                     (default: {}). Reverse-execution commands such as \
                     `reverse-continue`, `reverse-step`, and `watch` work here.",
                    DEFAULT_COMMANDS.join(", ")
                )),
            ),
        );

        properties.insert(
            "mode".to_string(),
            JsonSchema::string(Some(
                "\"replay\" (default) records ONE run then replays it. \"chaos\" records under \
                 rr's chaos scheduler and repeats until the run FAILS, then replays that failing \
                 trace — use it for a flaky/intermittent/racy failure you cannot reproduce on \
                 demand."
                    .to_string(),
            )),
        );
        properties.insert(
            "repeat".to_string(),
            JsonSchema::number(Some(format!(
                "How many recording attempts to make while hunting an intermittent failure. \
                 Default 1 for \"replay\", {CHAOS_DEFAULT_REPEAT} for \"chaos\"; capped at \
                 {MAX_REPEAT}. Recording stops at the first FAILING run, which is the one \
                 replayed."
            ))),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Catch a NONDETERMINISTIC bug: record one real execution with rr and \
                replay that exact execution as often as you like, stepping BACKWARDS through it. \
                This is the only tool here that can run time in reverse, and the only one that \
                makes an intermittent failure reproducible on demand. REACH FOR IT whenever a bug \
                will not reproduce or changes under observation — 'fails about 1 in 20', 'only \
                fails in CI', 'passes when I run it under gdb/valgrind', 'goes away when I add a \
                print', 'works in debug but not release', a flaky or racy test, an occasional \
                crash or wrong answer — and whenever you need to know WHAT WROTE A VALUE: set a \
                hardware watchpoint and `reverse-continue` to land on the exact instruction that \
                wrote it, which no forward debugger can do. mode=\"chaos\" records under rr's \
                chaos scheduler and keeps re-running until it CATCHES the failure, then replays \
                that failing trace — this is how you reproduce a rare race deliberately instead \
                of waiting for luck; `repeat` sets how many attempts. Replay is deterministic, so \
                reverse-execution commands (`reverse-continue`, `reverse-step`, `reverse-finish`, \
                `watch -l <expr>`) all work in `commands`. Recording needs hardware performance \
                counters: where they are missing this tool says exactly why and what to use \
                instead, so it is always worth trying. C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["program".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // Recording spins up a single trace dir per run; keep these serialized.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl RecordTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: RecordArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;

        // The model's input first: a bad argument list or mode is its to fix,
        // and must not be hidden behind (or cost) an rr preflight.
        let prog_args = crate::program_args::parse("args", args.args.as_deref().unwrap_or(""))
            .map_err(FunctionCallError::RespondToModel)?;
        let chaos = is_chaos(args.mode.as_deref()).map_err(FunctionCallError::RespondToModel)?;
        let commands = args
            .commands
            .filter(|c| !c.is_empty())
            .unwrap_or_else(default_commands);
        // The replay runs these in gdb on the host, outside the sandbox, like
        // the `debug` tool's commands: the same check, before anything runs.
        vet_commands(&commands).map_err(FunctionCallError::RespondToModel)?;

        // Preflight: rr needs hardware perf counters + ptrace. Many sandboxes
        // block them. Probe cheaply and degrade with a clear message rather than
        // failing deep inside a real record.
        // All three rr invocations go through run_capped — rr *records the
        // program*, so an unbounded `.output()` here would be a hang with a
        // multi-GB trace attached.
        // The probe records into a scratch directory: in rr's own trace root it
        // left an `echo-N` recording behind on every call and moved
        // `latest-trace` to it.
        let scratch = crate::tools::runtime_util::ScratchDir::new("rr-probe").map_err(|e| {
            FunctionCallError::RespondToModel(format!("cannot create a scratch directory: {e}"))
        })?;
        let probe_argv = [
            "record".to_string(),
            "-o".to_string(),
            scratch.path().join("probe").to_string_lossy().into_owned(),
            "echo".to_string(),
            "ok".to_string(),
        ];
        let probe = match run_capped("rr", &probe_argv, &self.cwd, timeout_from(None)).await? {
            RunOutcome::TimedOut { secs } => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "rr preflight timed out after {secs}s; rr is unusable here."
                )));
            }
            outcome => outcome,
        };
        if let RunOutcome::Finished {
            success: false,
            stderr,
            ..
        } = &probe
        {
            if is_perf_unavailable(stderr) {
                return Err(FunctionCallError::RespondToModel(unavailable_message()));
            }
            return Err(FunctionCallError::RespondToModel(format!(
                "rr preflight failed: {}",
                stderr.trim()
            )));
        }

        let program = resolve_path(&self.cwd, &args.program);
        let Some(trace_root) = crate::tools::runtime_util::rr_trace_root() else {
            return Err(FunctionCallError::RespondToModel(
                "cannot tell where rr keeps recordings: neither _RR_TRACE_DIR nor HOME is set"
                    .to_string(),
            ));
        };
        let _ = std::fs::create_dir_all(&trace_root);

        // 1) Record → a deterministic trace. In chaos mode keep recording until a
        //    run FAILS: that failing trace is the one worth replaying, and it is
        //    how a 1-in-N race is caught deliberately rather than by luck.
        //    Each attempt records into a directory of its own (`rr record -o`)
        //    and the replay names it: rr's `latest-trace` is whatever ANY rr on
        //    the box recorded last, so replaying it could show another run.
        let attempts = resolve_repeat(args.repeat, chaos);
        let mut record_err = String::new();
        let mut caught_on: Option<u32> = None;
        let mut trace_dir = PathBuf::new();
        for attempt in 1..=attempts {
            if attempt > 1 {
                // A passing attempt is superseded by the next one; its trace is
                // ours, so it does not stay behind filling the disk.
                let _ = std::fs::remove_dir_all(&trace_dir);
            }
            trace_dir = fresh_trace_dir(&trace_root, &program);
            let record_args = build_record_args(&trace_dir, &program, &prog_args, chaos);
            match run_capped("rr", &record_args, &self.cwd, timeout_from(None)).await? {
                RunOutcome::TimedOut { secs } => {
                    let _ = std::fs::remove_dir_all(&trace_dir);
                    return Err(FunctionCallError::RespondToModel(format!(
                        "rr record timed out after {secs}s and its process group was killed (the \
                         partial trace is discarded). The program may be looping or waiting on \
                         input; record a smaller reproducer."
                    )));
                }
                RunOutcome::Finished {
                    success, stderr, ..
                } => {
                    record_err = stderr;
                    if !success {
                        caught_on = Some(attempt);
                        break;
                    }
                }
            }
        }
        if !crate::tools::runtime_util::is_rr_trace(&trace_dir) {
            return Err(FunctionCallError::RespondToModel(format!(
                "rr record did not produce a recording; nothing was replayed.\n{}",
                record_err.trim()
            )));
        }
        // `rr record -o` does not move rr's `latest-trace` link the way a plain
        // record does; move it, so `rr replay` with no argument and
        // debug_reverse without `trace` both mean this recording.
        let _ = crate::tools::runtime_util::point_latest_trace_at(&trace_root, &trace_dir);
        let record_summary = record_attempt_summary(chaos, attempts, caught_on);

        // 2) Replay non-interactively. `rr replay` forwards everything after `--`
        //    to the debugger, so `-- -batch -ex <cmd>` drives gdb in batch mode.
        let replay_args = build_replay_args(&trace_dir, &commands);
        let replay = run_capped("rr", &replay_args, &self.cwd, timeout_from(None)).await?;
        let (replay_out, replay_err, truncated) = match replay {
            RunOutcome::TimedOut { secs } => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "rr replay timed out after {secs}s. The recording succeeded; the replay \
                     commands did not terminate — pass `commands` that end (e.g. `continue`, \
                     `bt`) rather than ones that wait."
                )));
            }
            RunOutcome::Finished {
                stdout,
                stderr,
                truncated,
                ..
            } => (stdout, stderr, truncated),
        };

        let mut combined = String::new();
        // The hunt result comes FIRST: whether a failing run was caught is the
        // single fact that decides what the replay below is worth.
        combined.push_str(&record_summary);
        // Before the replay text, so the output cap can never cut it.
        combined.push_str(&format!(
            "trace: {}\nAsk this exact execution questions backwards (who wrote a value, the last \
             time a function ran, the lines before the stop) with debug_reverse trace=\"{}\".\n",
            trace_dir.display(),
            trace_dir.display()
        ));
        if truncated {
            combined.push_str("note: replay output was cut at the capture ceiling.\n");
        }
        if !record_err.trim().is_empty() {
            combined.push_str("== rr record ==\n");
            combined.push_str(record_err.trim());
            combined.push('\n');
        }
        combined.push_str("== rr replay (reverse-exec) ==\n");
        combined.push_str(&replay_out);
        if !replay_err.trim().is_empty() {
            if !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&replay_err);
        }

        let result = if truncated || combined.len() > OUTPUT_CAP {
            check_result::CheckResult::approximate(
                TOOL_NAME,
                "The replay output was cut, so what is shown is incomplete.",
            )
        } else {
            match (chaos, caught_on) {
                (true, None) => check_result::CheckResult::no_claim(
                    TOOL_NAME,
                    &format!(
                        "No failing run was caught in {attempts} chaos recording(s); that is not proof the bug is gone."
                    ),
                ),
                (true, Some(attempt)) => check_result::CheckResult::pass(
                    TOOL_NAME,
                    &format!("Caught a failing run on recording {attempt} and replayed it."),
                ),
                (false, _) => {
                    check_result::CheckResult::pass(TOOL_NAME, "Recorded the run and replayed it.")
                }
            }
        };
        Ok(crate::report::report(
            TOOL_NAME,
            &args.program,
            truncate(combined, OUTPUT_CAP),
            result,
        ))
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

fn default_commands() -> Vec<String> {
    DEFAULT_COMMANDS.iter().map(|c| c.to_string()).collect()
}

/// `rr record <program> <args...>`
/// `mode` is "replay" (the default) or "chaos". Anything else used to be taken
/// as replay without a word, so a model asking for a race hunt by another name
/// got one ordinary recording — which it would read as "the race did not occur".
fn is_chaos(mode: Option<&str>) -> Result<bool, String> {
    match mode.map(str::trim) {
        None => Ok(false),
        Some(m) if m.eq_ignore_ascii_case("replay") => Ok(false),
        Some(m) if m.eq_ignore_ascii_case("chaos") => Ok(true),
        Some(other) => Err(format!(
            "unknown mode {other:?}: expected \"replay\" (record once, then replay) or \"chaos\" \
             (record under rr's chaos scheduler until a run fails)"
        )),
    }
}

/// Attempts to make, bounded so one tool call cannot run forever.
fn resolve_repeat(requested: Option<u32>, chaos: bool) -> u32 {
    let default = if chaos { CHAOS_DEFAULT_REPEAT } else { 1 };
    requested.unwrap_or(default).clamp(1, MAX_REPEAT)
}

/// Lead line for the report. Whether a FAILING run was caught decides what the
/// replay is worth, so it is stated first and a clean sweep is never dressed up
/// as proof the bug is gone.
fn record_attempt_summary(chaos: bool, attempts: u32, caught_on: Option<u32>) -> String {
    match (chaos, caught_on) {
        (_, Some(n)) if attempts > 1 => format!(
            "CAUGHT a failing run on attempt {n} of {attempts}; that failing trace is what is \
             replayed below. Replay is deterministic, so you can now step BACKWARDS through this \
             exact failure as often as you like.\n"
        ),
        (_, Some(_)) => {
            "the recorded run exited non-zero; that trace is replayed below.\n".to_string()
        }
        (true, None) => format!(
            "recorded {attempts} run(s) under rr's chaos scheduler and every one PASSED. That is \
             NOT proof the bug is gone — a rare race can hide for far more runs than this. Raise \
             `repeat`, increase the workload/thread count, or record the exact command that fails \
             in CI. The last (passing) trace is replayed below.\n"
        ),
        (false, None) => String::new(),
    }
}

/// A recording directory that does not exist yet, under rr's own trace root
/// so `rr ls` still lists it. `rr record -o` refuses an existing directory.
fn fresh_trace_dir(root: &Path, program: &Path) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "program".to_string());
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    loop {
        let dir = root.join(format!(
            "{name}-byteask-{secs}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if !dir.exists() {
            return dir;
        }
    }
}

/// gdb's own check for the `commands` (see [`crate::tools::gdb_session::blocked_reason`]).
fn vet_commands(commands: &[String]) -> Result<(), String> {
    for (index, command) in commands.iter().enumerate() {
        if let Some(reason) = crate::tools::gdb_session::blocked_reason(command.trim()) {
            return Err(format!(
                "Refused commands[{index}] `{command}`: {reason}. Nothing was recorded: the replay \
                 debugger blocks host-side escapes and calls into the program."
            ));
        }
    }
    Ok(())
}

fn build_record_args(
    trace_dir: &Path,
    program: &Path,
    prog_args: &[String],
    chaos: bool,
) -> Vec<String> {
    let mut argv = vec![
        "record".to_string(),
        "-o".to_string(),
        trace_dir.to_string_lossy().into_owned(),
    ];
    if chaos {
        // rr's chaos scheduler deliberately perturbs scheduling to surface races
        // that a normal run almost never hits.
        argv.push("--chaos".to_string());
    }
    argv.push(program.to_string_lossy().into_owned());
    argv.extend(prog_args.iter().cloned());
    argv
}

/// `rr replay <trace> -- -batch <hardening> -ex <cmd1> -ex <cmd2> ...`
///
/// Everything after `--` is forwarded to the debugger (gdb), so `-batch` makes
/// gdb run the `-ex` commands against the replayed process and exit instead of
/// dropping into an interactive prompt. The hardening is the `debug` tool's:
/// no ~/.gdbinit, no auto-loaded scripts, no calls into the program.
fn build_replay_args(trace_dir: &Path, commands: &[String]) -> Vec<String> {
    let mut argv = vec![
        "replay".to_string(),
        trace_dir.to_string_lossy().into_owned(),
        "--".to_string(),
        "-nx".to_string(),
        "-batch".to_string(),
        "-iex".to_string(),
        "set auto-load off".to_string(),
        "-ex".to_string(),
        "set may-call-functions off".to_string(),
    ];
    for cmd in commands {
        argv.push("-ex".to_string());
        argv.push(cmd.clone());
    }
    argv
}

/// Detect, from a failed `rr` invocation's stderr, that rr cannot run here
/// because hardware perf counters / ptrace are restricted (the common sandbox
/// case: `perf_event_paranoid` too high, no PMU, or no `CAP_SYS_ADMIN`).
pub(crate) fn is_perf_unavailable(stderr: &str) -> bool {
    // Delegates to the shared matcher so the perf-restriction patterns live in
    // one place (probe.rs) rather than drifting across tools.
    crate::probe::is_perf_restricted(stderr)
}

/// Read a small integer sysctl such as `/proc/sys/kernel/perf_event_paranoid`.
fn read_sysctl_i32(path: &str) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(crate) fn unavailable_message() -> String {
    unavailable_message_for(
        has_core_pmu(std::path::Path::new("/sys/bus/event_source/devices")),
        read_sysctl_i32("/proc/sys/kernel/perf_event_paranoid"),
        read_sysctl_i32("/proc/sys/kernel/yama/ptrace_scope"),
    )
}

/// Does the kernel expose a hardware (core) PMU? A core PMU registers under
/// `/sys/bus/event_source/devices` — `cpu` on x86, `cpu_core`/`cpu_atom` on
/// hybrid Intel, `armv8_*` on ARM — whatever `perf_event_paranoid` says, and
/// the listing needs no privilege. `None` when it cannot be read. Measured on
/// a cloud VM without a virtual PMU: only breakpoint, kprobe, msr, software,
/// tracepoint and uprobe — no core PMU, so rr can never record there.
fn has_core_pmu(devices: &std::path::Path) -> Option<bool> {
    let entries = std::fs::read_dir(devices).ok()?;
    Some(entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        matches!(name.as_ref(), "cpu" | "cpu_core" | "cpu_atom") || name.starts_with("armv")
    }))
}

/// Diagnose WHICH blocker was hit before advising, because the two are not
/// equally bad: a too-high `perf_event_paranoid` is a ten-second sysctl the user
/// can fix and then rr works, while a machine with no PMU can never record at
/// all. The old message said "requires hardware performance counters" for both,
/// which sent people away from a setting they could simply have changed.
fn unavailable_message_for(
    pmu: Option<bool>,
    paranoid: Option<i32>,
    ptrace_scope: Option<i32>,
) -> String {
    // No core PMU at all: lowering paranoid cannot help, and saying it would
    // sends the user to run a root sysctl for nothing.
    if let Some(p) = paranoid
        && p > 1
        && pmu != Some(false)
    {
        return format!(
            "rr cannot record here yet — but this is FIXABLE in ten seconds. The kernel is \
             blocking performance counters for unprivileged users: \
             /proc/sys/kernel/perf_event_paranoid is {p} and rr needs it <= 1.\n\
             Ask the user to run (it needs root):\n  \
             sudo sysctl kernel.perf_event_paranoid=1\n\
             To persist it:\n  \
             echo 'kernel.perf_event_paranoid = 1' | sudo tee /etc/sysctl.d/10-rr.conf\n\
             Then call this tool again. Until then use debug / debug_session (forward-only \
             gdb), or cpp_sanitize with sanitizer=\"tsan\" for a suspected data race."
        );
    }
    if let Some(s) = ptrace_scope
        && s >= 2
    {
        return format!(
            "rr cannot record here: ptrace is locked down \
             (/proc/sys/kernel/yama/ptrace_scope is {s}; rr needs to trace its own child). Ask \
             the user to run `sudo sysctl kernel.yama.ptrace_scope=0`, then call this tool \
             again. Until then use debug / debug_session, or cpp_sanitize with \
             sanitizer=\"tsan\" for a suspected data race."
        );
    }
    let paranoid_note = match paranoid {
        Some(p) if p > 1 => format!(
            "perf_event_paranoid is {p}, but lowering it would not help: there is no hardware \
             PMU, so "
        ),
        Some(p) => format!("perf_event_paranoid is already permissive ({p}) yet "),
        None => String::new(),
    };
    format!(
        "rr cannot record on this machine and no setting will change that: {paranoid_note}the \
         hardware performance counters rr depends on are not available. That is what a cloud VM, \
         a container, or a VM without a virtualised PMU looks like (`perf stat -e cycles` reports \
         <not supported> there). Do NOT keep retrying rr here.\n\
         Use instead: for a suspected DATA RACE, cpp_sanitize with sanitizer=\"tsan\" — it proves \
         races with no PMU at all; for a reproducible crash or a core dump, debug / \
         debug_session; for 'what wrote this value', a hardware watchpoint in debug_session \
         (forward-only). To use rr itself, run it on bare metal or a VM that exposes a vPMU."
    )
}

fn truncate(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    // Truncate on a char boundary at or below `cap`.
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... [output truncated]");
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn a_misnamed_field_is_an_error_not_silently_ignored() {
        // A field outside the schema used to be dropped without a word.
        let err = serde_json::from_str::<RecordArgs>(r#"{"program":"a.out","run_args":"x"}"#)
            .err()
            .expect("unknown field refused");
        assert!(
            err.to_string().contains("unknown field `run_args`"),
            "{err}"
        );
    }

    use super::*;

    fn trace() -> PathBuf {
        PathBuf::from("/rr/a.out-byteask-1")
    }

    #[test]
    fn builds_record_argv() {
        let program = PathBuf::from("/work/a.out");
        let prog_args = vec!["--flag".to_string(), "value".to_string()];
        let argv = build_record_args(&trace(), &program, &prog_args, false);
        assert_eq!(
            argv,
            vec![
                "record".to_string(),
                "-o".to_string(),
                "/rr/a.out-byteask-1".to_string(),
                "/work/a.out".to_string(),
                "--flag".to_string(),
                "value".to_string(),
            ]
        );
    }

    #[test]
    fn builds_record_argv_without_args() {
        let program = PathBuf::from("/work/a.out");
        let argv = build_record_args(&trace(), &program, &[], false);
        assert_eq!(
            argv,
            vec![
                "record".to_string(),
                "-o".to_string(),
                "/rr/a.out-byteask-1".to_string(),
                "/work/a.out".to_string()
            ]
        );
    }

    #[test]
    fn every_attempt_gets_a_trace_dir_of_its_own() {
        // The old code replayed rr's `latest-trace`, which any other rr run on
        // the machine replaces: measured on a machine with a PMU, the live test below
        // replayed a parallel test's `true-1` recording instead of its own.
        let root = tempfile::tempdir().unwrap();
        let a = fresh_trace_dir(root.path(), Path::new("/work/feed"));
        std::fs::create_dir(&a).unwrap();
        let b = fresh_trace_dir(root.path(), Path::new("/work/feed"));
        assert_ne!(a, b);
        assert!(!b.exists());
        assert!(a.starts_with(root.path()));
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("feed-byteask-")
        );
    }

    #[test]
    fn replay_commands_are_vetted_like_the_debug_tools() {
        // The replay's gdb runs on the host outside the sandbox; the old code
        // passed `commands` straight through, so `shell …` ran.
        assert!(vet_commands(&["continue".to_string(), "reverse-continue".to_string()]).is_ok());
        let err = vet_commands(&["bt".to_string(), "shell touch /tmp/x".to_string()]).unwrap_err();
        assert!(err.contains("commands[1]"), "{err}");
        assert!(vet_commands(&["python print(1)".to_string()]).is_err());
        assert!(vet_commands(&["set may-call-functions on".to_string()]).is_err());
    }

    #[test]
    fn chaos_mode_passes_the_chaos_scheduler_flag_before_the_program() {
        // Would fail on the old code: --chaos was never emitted at all, so a
        // rare race could only be caught by luck.
        let program = PathBuf::from("/work/a.out");
        let argv = build_record_args(&trace(), &program, &["--gtest_repeat=1".to_string()], true);
        assert_eq!(
            argv,
            vec![
                "record".to_string(),
                "-o".to_string(),
                "/rr/a.out-byteask-1".to_string(),
                "--chaos".to_string(),
                "/work/a.out".to_string(),
                "--gtest_repeat=1".to_string(),
            ]
        );
        // rr requires its own flags before the tracee.
        assert!(
            argv.iter().position(|a| a == "--chaos").unwrap()
                < argv.iter().position(|a| a == "/work/a.out").unwrap()
        );
    }

    #[test]
    fn repeat_accepts_a_whole_number_written_as_a_float() {
        let args: RecordArgs = serde_json::from_str(r#"{"program":"a","repeat":3.0}"#).unwrap();
        assert_eq!(args.repeat, Some(3));
        let args: RecordArgs = serde_json::from_str(r#"{"program":"a","repeat":12}"#).unwrap();
        assert_eq!(args.repeat, Some(12));
        assert!(serde_json::from_str::<RecordArgs>(r#"{"program":"a","repeat":2.5}"#).is_err());
        assert!(serde_json::from_str::<RecordArgs>(r#"{"program":"a","repeat":-1}"#).is_err());
        let args: RecordArgs = serde_json::from_str(r#"{"program":"a"}"#).unwrap();
        assert_eq!(args.repeat, None);
    }

    #[test]
    fn mode_parsing_is_forgiving_but_narrow() {
        assert_eq!(is_chaos(Some("chaos")), Ok(true));
        assert_eq!(is_chaos(Some("  CHAOS  ")), Ok(true));
        assert_eq!(is_chaos(Some("replay")), Ok(false));
        assert_eq!(is_chaos(None), Ok(false));
        // Not silently replay any more.
        assert!(is_chaos(Some("chaotic")).is_err());
        assert!(is_chaos(Some("stress")).is_err());
    }

    #[test]
    fn repeat_defaults_differ_by_mode_and_are_bounded() {
        // Old code had no repeat at all: one recording, then give up.
        assert_eq!(resolve_repeat(None, false), 1);
        assert_eq!(resolve_repeat(None, true), CHAOS_DEFAULT_REPEAT);
        assert_eq!(resolve_repeat(Some(7), true), 7);
        // one call must never run unbounded, and never zero times
        assert_eq!(resolve_repeat(Some(10_000), true), MAX_REPEAT);
        assert_eq!(resolve_repeat(Some(0), true), 1);
    }

    #[test]
    fn a_clean_chaos_sweep_is_never_reported_as_proof() {
        let msg = record_attempt_summary(true, 12, None);
        assert!(msg.contains("PASSED"), "{msg}");
        assert!(msg.contains("NOT proof"), "{msg}");
        // and it must tell the model how to try harder
        assert!(msg.contains("repeat"), "{msg}");
    }

    #[test]
    fn catching_a_failing_run_is_reported_with_the_attempt_number() {
        let msg = record_attempt_summary(true, 12, Some(4));
        assert!(msg.contains("CAUGHT"), "{msg}");
        assert!(msg.contains("attempt 4 of 12"), "{msg}");
        // a single non-chaos run that failed still says the trace is the failing one
        let single = record_attempt_summary(false, 1, Some(1));
        assert!(single.contains("non-zero"), "{single}");
        // a plain single passing run stays quiet
        assert_eq!(record_attempt_summary(false, 1, None), "");
    }

    #[test]
    fn builds_replay_argv_forwards_batch_commands_after_dashdash() {
        let commands = vec!["continue".to_string(), "bt".to_string()];
        let argv = build_replay_args(&trace(), &commands);
        assert_eq!(
            argv,
            vec![
                "replay".to_string(),
                "/rr/a.out-byteask-1".to_string(),
                "--".to_string(),
                "-nx".to_string(),
                "-batch".to_string(),
                "-iex".to_string(),
                "set auto-load off".to_string(),
                "-ex".to_string(),
                "set may-call-functions off".to_string(),
                "-ex".to_string(),
                "continue".to_string(),
                "-ex".to_string(),
                "bt".to_string(),
            ]
        );
        // The `--` separator must come before any debugger options so rr forwards
        // them to gdb rather than parsing them itself.
        let dd = argv.iter().position(|a| a == "--").unwrap();
        let batch = argv.iter().position(|a| a == "-batch").unwrap();
        assert!(dd < batch);
    }

    #[test]
    fn default_commands_are_a_reverse_debug_starter_set() {
        assert_eq!(
            default_commands(),
            vec!["continue".to_string(), "bt".to_string()]
        );
    }

    #[test]
    fn args_splitting_collapses_whitespace() {
        let split: Vec<String> = "  --a   b\tc "
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(
            split,
            vec!["--a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "a.out"), PathBuf::from("/work/a.out"));
        assert_eq!(resolve_path(cwd, "/abs/a.out"), PathBuf::from("/abs/a.out"));
    }

    #[test]
    fn detects_perf_paranoid_preflight_failure() {
        // The exact message rr emits on a locked-down box.
        let stderr = "rr needs /proc/sys/kernel/perf_event_paranoid <= 1, but it is 3.\n\
                      Change it to 1, or use 'rr record -n' (slow).";
        assert!(is_perf_unavailable(stderr));
    }

    #[test]
    fn detects_ptrace_and_pmu_failures() {
        assert!(is_perf_unavailable(
            "ptrace(PTRACE_TRACEME) failed: Operation not permitted"
        ));
        assert!(is_perf_unavailable(
            "Unable to open performance counter (PMU not available)"
        ));
        assert!(is_perf_unavailable("requires CAP_SYS_ADMIN"));
    }

    #[test]
    fn does_not_flag_unrelated_failures_as_perf() {
        // A genuine "program not found" style failure must NOT be reported as the
        // perf/sandbox-unavailable case.
        assert!(!is_perf_unavailable(
            "rr: error: Unable to execute '/work/a.out': No such file"
        ));
    }

    #[test]
    fn a_too_high_paranoid_is_reported_as_fixable_with_the_exact_command() {
        // Old code called this "requires hardware performance counters" and sent
        // the user away from a setting they could have changed in ten seconds.
        let msg = unavailable_message_for(Some(true), Some(3), Some(0));
        assert!(msg.contains("FIXABLE"), "{msg}");
        // Unknown PMU (sysfs unreadable): still the fixable advice.
        assert!(unavailable_message_for(None, Some(3), Some(0)).contains("FIXABLE"));
        assert!(msg.contains("perf_event_paranoid is 3"), "{msg}");
        assert!(
            msg.contains("sudo sysctl kernel.perf_event_paranoid=1"),
            "{msg}"
        );
        assert!(msg.contains("debug"), "{msg}");
    }

    #[test]
    fn no_pmu_is_reported_as_impossible_not_as_a_setting() {
        // paranoid already permissive => the counters genuinely are not there.
        // This is the measured state of a cloud VM with no vPMU.
        let msg = unavailable_message_for(Some(false), Some(1), Some(0));
        assert!(msg.contains("no setting will change that"), "{msg}");
        assert!(!msg.contains("FIXABLE"), "{msg}");
        // it must route to the tool that CAN find a race without a PMU
        assert!(msg.contains("tsan"), "{msg}");
        assert!(msg.contains("Do NOT keep retrying"), "{msg}");
    }

    #[test]
    fn with_no_pmu_a_high_paranoid_is_not_called_fixable() {
        // The measured state of a VM without a PMU: paranoid 3 AND no core PMU. The old
        // message told the user a root sysctl would fix it; it cannot.
        let msg = unavailable_message_for(Some(false), Some(3), Some(0));
        assert!(!msg.contains("FIXABLE"), "{msg}");
        assert!(msg.contains("no setting will change that"), "{msg}");
        assert!(!msg.contains("permissive"), "3 is not permissive: {msg}");
        assert!(msg.contains("lowering it would not help"), "{msg}");
    }

    #[test]
    fn the_core_pmu_is_found_by_its_sysfs_name() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "breakpoint",
            "kprobe",
            "msr",
            "software",
            "tracepoint",
            "uprobe",
        ] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        assert_eq!(has_core_pmu(dir.path()), Some(false));
        std::fs::create_dir(dir.path().join("cpu")).unwrap();
        assert_eq!(has_core_pmu(dir.path()), Some(true));
        assert_eq!(has_core_pmu(&dir.path().join("missing")), None);
    }

    #[test]
    fn a_locked_down_ptrace_is_named_separately() {
        let msg = unavailable_message_for(Some(true), Some(1), Some(2));
        assert!(msg.contains("ptrace_scope is 2"), "{msg}");
        assert!(msg.contains("kernel.yama.ptrace_scope=0"), "{msg}");
    }

    #[test]
    fn unknown_sysctls_still_give_an_actionable_message() {
        let msg = unavailable_message_for(None, None, None);
        assert!(msg.contains("tsan"), "{msg}");
        assert!(msg.contains("debug"), "{msg}");
    }

    #[test]
    fn truncate_caps_long_output() {
        let truncated = truncate("x".repeat(10_000), OUTPUT_CAP);
        assert!(truncated.starts_with(&"x".repeat(OUTPUT_CAP)));
        assert!(truncated.ends_with("[output truncated]"));
    }

    // ------------- real-rr argv tests (these RUN on a machine with no PMU) -------------
    //
    // The full record/replay loop needs hardware counters, which many VMs do
    // not have. But rr PARSES ITS ARGUMENTS BEFORE it checks for a PMU, and the
    // two failures are textually distinct:
    //
    //   rr record --chaos /bin/true   -> "rr needs /proc/sys/kernel/perf_event_paranoid <= 1"
    //   rr record --bogus  /bin/true  -> "Invalid option --bogus"
    //
    // So a counter-less machine can still prove the thing most likely to break:
    // that REAL rr accepts every argv we construct. A renamed or mistyped flag
    // is caught here, today, instead of waiting for someone with a PMU. What is
    // NOT covered is whether a recording is correct — that is the #[ignore]d
    // pair below.

    /// True when rr rejected the command line itself, as opposed to failing
    /// later for an environmental reason (no PMU, ptrace restricted, …).
    fn rr_rejected_the_argv(output: &str) -> bool {
        let lower = output.to_ascii_lowercase();
        lower.contains("invalid option") || lower.contains("unknown option")
    }

    /// Run real rr with `argv` and return combined output, or None to skip.
    async fn rr_output(argv: &[String]) -> Option<String> {
        let dir = std::env::temp_dir();
        match run_capped("rr", argv, &dir, timeout_from(None)).await {
            Ok(RunOutcome::Finished { stdout, stderr, .. }) => Some(format!("{stdout}\n{stderr}")),
            Ok(RunOutcome::TimedOut { .. }) => None,
            // rr not installed: skip, exactly like the live tests below.
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn real_rr_accepts_every_argv_we_build() {
        // Guard first: if our own detector cannot spot a rejection, every
        // assertion below would pass on nothing. Prove it against a flag rr
        // certainly does not have, using the real binary.
        let Some(control) = rr_output(&[
            "record".to_string(),
            "--byteask-definitely-not-a-flag".to_string(),
            "/bin/true".to_string(),
        ])
        .await
        else {
            eprintln!("skip: rr not installed or did not run");
            return;
        };
        assert!(
            rr_rejected_the_argv(&control),
            "the non-vacuity control failed: rr did not reject a bogus flag, so this \
             test cannot prove anything about the real argv:\n{control}"
        );

        // Now the argv the tool actually builds, in all three shapes. Each
        // records into a directory of its own, never rr's shared trace root.
        let traces = tempfile::tempdir().unwrap();
        let plain_dir = traces.path().join("plain");
        let plain = build_record_args(&plain_dir, Path::new("/bin/true"), &[], false);
        let chaos = build_record_args(
            &traces.path().join("chaos"),
            Path::new("/bin/true"),
            &[],
            true,
        );
        let with_args = build_record_args(
            &traces.path().join("args"),
            Path::new("/bin/true"),
            &["--some-program-arg".to_string()],
            true,
        );
        let replay = build_replay_args(
            &plain_dir,
            &[
                "continue".to_string(),
                "watch -l magic".to_string(),
                "reverse-continue".to_string(),
            ],
        );

        for (label, argv) in [
            ("record", plain),
            ("record --chaos", chaos),
            ("record --chaos + program args", with_args),
            ("replay", replay),
        ] {
            let Some(out) = rr_output(&argv).await else {
                eprintln!("skip: rr did not run for {label}");
                continue;
            };
            assert!(
                !rr_rejected_the_argv(&out),
                "real rr REJECTED the argv we build for {label} ({argv:?}):\n{out}"
            );
        }
    }

    #[tokio::test]
    async fn program_args_are_not_parsed_as_rr_flags() {
        // A program argument that looks like an rr option must reach the
        // program, not rr. `build_record_args` puts the program before its
        // args, after every rr flag; if that order were ever reversed, rr would
        // claim the argument for itself. Proven against the real binary.
        let traces = tempfile::tempdir().unwrap();
        let argv = build_record_args(
            &traces.path().join("t"),
            Path::new("/bin/true"),
            &["--chaos".to_string()],
            false,
        );
        let program_at = argv.iter().position(|a| a == "/bin/true");
        let arg_at = argv.iter().rposition(|a| a == "--chaos");
        assert!(
            program_at < arg_at,
            "a program arg that looks like an rr flag must come AFTER the program: {argv:?}"
        );
        let Some(out) = rr_output(&argv).await else {
            eprintln!("skip: rr not installed or did not run");
            return;
        };
        assert!(
            !rr_rejected_the_argv(&out),
            "real rr mis-parsed a program argument as its own flag:\n{out}"
        );
    }

    // ---------------- live tests (#[ignore]; run with --include-ignored) ----------------
    //
    // rr can only record on a machine with hardware performance counters. The
    // A VM without a virtual PMU has none, so these are #[ignore]d by default.
    // They run on a machine with a PMU:
    //   BYTEASK_RR_LIVE_REQUIRED=1 <test-binary> --include-ignored record::tests::live
    // With BYTEASK_RR_LIVE_REQUIRED set, a machine that cannot record FAILS the
    // test instead of skipping it: on the host that is meant to prove rr, a
    // skip reading as a pass is the false-clean this crate keeps fixing.

    /// Skip, or fail when this host is supposed to prove rr works.
    pub(crate) fn rr_live_skip(why: &str) {
        if crate::trust::truthy(std::env::var("BYTEASK_RR_LIVE_REQUIRED").ok().as_deref()) {
            panic!(
                "BYTEASK_RR_LIVE_REQUIRED is set but this host cannot run the live rr test: {why}"
            );
        }
        eprintln!("skip: {why}");
    }

    /// Compile a C fixture with `cc -g -O0` (plus `extra` flags) into `dir`.
    pub(crate) async fn build_c(
        dir: &Path,
        name: &str,
        source: &str,
        extra: &[&str],
    ) -> Option<PathBuf> {
        let src = dir.join(format!("{name}.c"));
        std::fs::write(&src, source).ok()?;
        let bin = dir.join(name);
        for cc in ["cc", "gcc", "clang"] {
            if let Ok(st) = tokio::process::Command::new(cc)
                .args(["-g", "-O0"])
                .args(extra)
                .arg("-o")
                .arg(&bin)
                .arg(&src)
                .status()
                .await
                && st.success()
            {
                return Some(bin);
            }
        }
        None
    }

    /// Record a program that clobbers a known global, then use the one thing
    /// only rr can do: a hardware watchpoint plus `reverse-continue` to land on
    /// the instruction that wrote the bad value.
    #[tokio::test]
    #[ignore]
    async fn live_records_and_reverse_continues_to_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = build_c(
            dir.path(),
            "clobber",
            "#include <stdio.h>\n\
             int magic = 0x1234;\n\
             __attribute__((noinline)) void clobber(void){ magic = 0xdead; }\n\
             int main(void){ clobber(); printf(\"%x\\n\", magic); return 0; }\n",
            &[],
        )
        .await
        else {
            return rr_live_skip("no C compiler");
        };
        let trace = dir.path().join("trace");
        let record = run_capped(
            "rr",
            &build_record_args(&trace, &bin, &[], false),
            dir.path(),
            timeout_from(None),
        )
        .await
        .expect("record runs");
        if let RunOutcome::Finished {
            success: false,
            stderr,
            ..
        } = &record
            && is_perf_unavailable(stderr)
        {
            return rr_live_skip(&format!("this machine cannot record with rr:\n{stderr}"));
        }
        assert!(record.ok(), "rr record failed: {record:?}");
        assert!(
            crate::tools::runtime_util::is_rr_trace(&trace),
            "no trace at {}",
            trace.display()
        );

        // Run to exit, then watch the global and walk BACKWARDS to its writer.
        let commands = vec![
            "continue".to_string(),
            "watch -l magic".to_string(),
            "reverse-continue".to_string(),
            "bt".to_string(),
        ];
        let replay = run_capped(
            "rr",
            &build_replay_args(&trace, &commands),
            dir.path(),
            timeout_from(None),
        )
        .await
        .expect("replay runs");
        let out = match replay {
            RunOutcome::Finished { stdout, stderr, .. } => format!("{stdout}\n{stderr}"),
            RunOutcome::TimedOut { secs } => panic!("replay timed out after {secs}s"),
        };
        assert!(
            out.contains("clobber ()") || out.contains("in clobber"),
            "reverse-continue should land in clobber(), the function that wrote magic:\n{out}"
        );
        // gdb reports a REVERSE stop's values in execution order: going
        // backwards over `magic = 0xdead`, the value before it is 0x1234.
        assert!(
            out.contains("New value = 4660"),
            "the pre-write value 0x1234 must be shown:\n{out}"
        );
    }

    /// Chaos mode must actually drive `rr record --chaos` against real rr.
    #[tokio::test]
    #[ignore]
    async fn live_chaos_recording_is_accepted_by_rr() {
        let dir = tempfile::tempdir().unwrap();
        let trace = dir.path().join("trace");
        let argv = build_record_args(&trace, Path::new("/bin/true"), &[], true);
        let outcome = run_capped("rr", &argv, dir.path(), timeout_from(None))
            .await
            .expect("rr runs");
        if let RunOutcome::Finished {
            success: false,
            stderr,
            ..
        } = &outcome
            && is_perf_unavailable(stderr)
        {
            return rr_live_skip("this machine cannot record with rr");
        }
        match outcome {
            RunOutcome::Finished {
                success, stderr, ..
            } => {
                assert!(success, "rr rejected --chaos: {stderr}");
            }
            RunOutcome::TimedOut { secs } => panic!("rr record --chaos timed out after {secs}s"),
        }
        assert!(crate::tools::runtime_util::is_rr_trace(&trace));
    }

    /// The whole tool, through its real entry point: it records into a trace
    /// directory of its own, replays THAT one, and names it for debug_reverse.
    #[tokio::test]
    #[ignore]
    async fn live_the_tool_replays_its_own_recording_and_names_the_trace() {
        let dir = tempfile::tempdir().unwrap();
        let Some(_bin) = build_c(
            dir.path(),
            "clobber",
            "int magic = 0x1234;\n\
             __attribute__((noinline)) void clobber(void){ magic = 0xdead; }\n\
             int main(void){ clobber(); return magic == 0xdead ? 7 : 0; }\n",
            &[],
        )
        .await
        else {
            return rr_live_skip("no C compiler");
        };
        let tool = RecordTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let text = match crate::test_support::call(
            &tool,
            serde_json::json!({
                "program": "clobber",
                "commands": ["continue", "watch -l magic", "reverse-continue", "bt"]
            }),
        )
        .await
        {
            Ok(text) => text,
            Err(err)
                if is_perf_unavailable(&format!("{err:?}"))
                    || format!("{err:?}").contains("cannot record") =>
            {
                return rr_live_skip(&format!("{err:?}"));
            }
            Err(err) => panic!("debug_record failed: {err:?}"),
        };
        assert!(
            text.contains("exited non-zero"),
            "exit 7 is the failing run:\n{text}"
        );
        assert!(
            text.contains("in clobber") || text.contains("clobber ()"),
            "{text}"
        );
        let trace_line = text
            .lines()
            .find(|l| l.starts_with("trace: "))
            .expect("trace line");
        let trace = PathBuf::from(trace_line.trim_start_matches("trace: "));
        assert!(crate::tools::runtime_util::is_rr_trace(&trace), "{text}");
        // debug_reverse's default ("the most recent recording") is this one.
        assert_eq!(
            crate::tools::runtime_util::latest_trace(),
            std::fs::canonicalize(&trace).ok(),
            "latest-trace must name the recording just made"
        );
        let _ = std::fs::remove_dir_all(&trace);
    }

    /// A refused command stops the call before rr records anything.
    #[tokio::test]
    async fn a_shell_escape_in_commands_is_refused_before_recording() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RecordTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let marker = dir.path().join("pwned");
        let err = crate::test_support::call(
            &tool,
            serde_json::json!({
                "program": "/bin/true",
                "commands": ["continue", format!("shell touch {}", marker.display())]
            }),
        )
        .await
        .expect_err("refused");
        let text = format!("{err:?}");
        assert!(text.contains("Refused commands[1]"), "{text}");
        assert!(text.contains("Nothing was recorded"), "{text}");
        assert!(!marker.exists(), "the shell escape ran: {text}");
    }
}
