//! `cpp_perf` — hardware-aware performance verification.
//!
//! Four modes, all built on the shared [`crate::stats`] (noise-gated verdicts),
//! [`crate::probe`] (per-OS backend ladders), and [`crate::run`] (capped,
//! process-group-killed runs) modules:
//!
//! ```text
//!   mode=time      A/B wall-clock timing, noise-gated — NEVER claims a win
//!                  inside measurement noise, and a failed command invalidates
//!                  the comparison outright.   backend: interleaved `bash -c`
//!                                                      runs, our own clock
//!   mode=profile   ranked hot-function summary.     backend: perf record → callgrind
//!   mode=topdown   bottleneck classification.       backend: perf --topdown → perf
//!                                                             counters → cachegrind
//!   mode=callgrind profile with the callgrind backend forced (kept name).
//! ```
//!
//! Every backend is probed first; a miss walks down the ladder to an actionable
//! per-OS message — never a hard error, never a silent wrong answer. Simulated
//! backends (callgrind / cachegrind) tag their output `[simulated]`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;
use std::time::Instant;

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

use check_result::CheckResult;
use check_result::NotRunReason;

use crate::probe::Probe;
use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;
use crate::stats::Measurement;

const TOOL_NAME: &str = "cpp_perf";
const DEFAULT_MODE: &str = "time";
const DEFAULT_REPEAT: u32 = 5;
/// Upper bound on `repeat`. `repeat: 4294967295` once reached a
/// `Vec::with_capacity` that aborted the whole engine (34 GB allocation), or,
/// where the allocation succeeded, looped for what would have been years. Real
/// calls use 3-15; past ~100 the 3% floor below, not the sample size, bounds
/// what a comparison can resolve.
const MAX_REPEAT: u32 = 100;
/// Relative floor of the time-mode verdict: never call a difference under 3%
/// of the slower command's mean. Whole-process timings move by a few percent
/// between builds and runs for reasons that are not the code under test (code
/// and stack alignment, link order, environment size, CPU frequency), so a
/// smaller difference is not evidence about the change even when it is
/// statistically clean. (In-process `cpp_bench` repetitions keep 1%.)
const TIME_FLOOR: f64 = 0.03;
/// Shell runs (`bash -c :`) timed to measure the startup inside every sample.
const SHELL_BASELINE_RUNS: usize = 3;
/// Report the shell's startup once it is this share of a command's mean: above
/// it, means and ratios no longer describe the programs' own work.
const SHELL_SHARE_NOTE: f64 = 0.05;
/// How many hot-function rows to surface in profile/callgrind output.
const PROFILE_TOP: usize = 15;
/// Above this share of unresolved self% mass, tell the model how to fix symbols.
const SYMBOL_QUALITY_POOR: f64 = 0.20;
/// The tool-specific "how to get a cleaner answer" clause for the noise gate.
const TIME_HINT: &str = "increase repeat, enlarge the workload, or close background load";
/// The same clause when [`TIME_FLOOR`] set the gate, where more runs cannot help.
const TIME_FLOOR_HINT: &str = "whole-process timing cannot attribute a difference under 3% to \
    the code (alignment, OS and CPU-frequency effects are that large); benchmark the code \
    in-process with cpp_bench instead";

pub struct PerfTool {
    pub cwd: PathBuf,
    /// Every mode runs the model-supplied command natively (`bash -c`,
    /// or under perf/valgrind), outside the filesystem sandbox. See
    /// [`crate::trust`].
    pub native_exec: bool,
}

/// `deny_unknown_fields`: a misnamed field used to vanish silently —
/// `"repetitions": 3` ran the default 5 repeats, and `"command": "./b"` timed
/// command_a alone and reported no comparison at all.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PerfArgs {
    command_a: String,
    #[serde(default)]
    command_b: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    repeat: Option<u32>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for PerfTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "command_a".to_string(),
            JsonSchema::string(Some(
                "Shell command to run and measure, e.g. \"./bench\". Must be repeatable / \
                 idempotent — it is run multiple times (a warmup plus each repeat)."
                    .to_string(),
            )),
        );
        properties.insert(
            "command_b".to_string(),
            JsonSchema::string(Some(
                "Optional second shell command to A/B compare against command_a (time mode)."
                    .to_string(),
            )),
        );
        properties.insert(
            "mode".to_string(),
            JsonSchema::string(Some(
                "\"time\" (default) = noise-gated A/B wall-clock timing (never claims a speedup \
                 inside measurement noise); \"profile\" = ranked hot-function summary; \
                 \"topdown\" = bottleneck classification (memory/core/frontend/bad-speculation); \
                 \"callgrind\" = hot-function profile with the callgrind backend."
                    .to_string(),
            )),
        );
        properties.insert(
            "repeat".to_string(),
            JsonSchema::integer(Some(format!(
                "How many times to run each command in time mode (default: {DEFAULT_REPEAT}, \
                 max: {MAX_REPEAT}). Use >=5 for a usable noise estimate."
            ))),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::integer(Some(
                "Per-run wall-clock cap in seconds (default: 300). A profiler can run a command \
                 20-100x slower, so cap long runs or raise this deliberately. In time mode it \
                 bounds the whole A/B measurement: runs that would not fit are skipped and the \
                 report says how many completed."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Measure C/C++ performance and answer it HONESTLY: noise-gated A/B \
                wall-clock timing ('is it really faster?' — never claims a win inside measurement \
                noise), a ranked hot-function profile ('where does time go?'), or a topdown \
                bottleneck classification ('why is it slow?'). Falls back gracefully per-OS when \
                perf/valgrind are unavailable. Use after perf-relevant changes."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["command_a".to_string()]),
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
        // Timing/profiling is sensitive to load; serialize perf calls.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl PerfTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: PerfArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;

        let mode = args.mode.as_deref().unwrap_or(DEFAULT_MODE);
        let repeat = clamp_repeat(args.repeat);
        let timeout = timeout_from(args.timeout_sec);

        let (rendered, result) = match mode {
            "time" => self.run_time_mode(&args, repeat, timeout).await?,
            "profile" | "callgrind" | "topdown" if args.command_b.is_some() => {
                return Err(FunctionCallError::RespondToModel(command_b_error(mode)));
            }
            "profile" => self.run_profile_mode(&args, timeout, false).await?,
            "callgrind" => self.run_profile_mode(&args, timeout, true).await?,
            "topdown" => self.run_topdown_mode(&args, timeout).await?,
            other => {
                return Err(FunctionCallError::RespondToModel(unknown_mode_error(other)));
            }
        };

        Ok(crate::report::report_plain(TOOL_NAME, rendered, result))
    }

    // ----- time mode --------------------------------------------------------

    /// A/B wall-clock timing with our own clock, the two commands INTERLEAVED:
    /// after one untimed warmup per side, round `i` runs A then B for even `i`
    /// and B then A for odd `i` (see [`round_order`]). The old design timed all
    /// of A's runs and then all of B's (`perf stat -r` can only do that), so
    /// any drift in machine speed between the two blocks read as a difference
    /// between the commands: identical commands were called "significantly"
    /// different in 10-40% of trials, more runs did not help
    /// (measured). `timeout_sec` bounds the whole measurement ([`Budget`]), not
    /// one run.
    async fn run_time_mode(
        &self,
        args: &PerfArgs,
        repeat: u32,
        timeout: Duration,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let budget = Budget::new(timeout);
        let shell_secs = self.shell_startup(&budget).await?;
        let mut sides = vec![Side::new(&args.command_a)];
        if let Some(cmd) = &args.command_b {
            sides.push(Side::new(cmd));
        }
        let mut notes = Vec::new();
        if let Some(asked) = args.repeat.filter(|asked| *asked > MAX_REPEAT) {
            notes.push(format!(
                "note: repeat={asked} is above the maximum of {MAX_REPEAT}; ran {MAX_REPEAT}."
            ));
        }

        // One untimed warmup per side. It also probes health: in an A/B a
        // failed warmup ends the call, since a failing command has nothing to
        // compare.
        if repeat >= 2 {
            for (i, side) in sides.iter_mut().enumerate() {
                match self.timed_run(side.cmd, budget.remaining()).await? {
                    Timed::TimedOut => {
                        return Ok((time_budget_message(label(i), budget.total), budget_result()));
                    }
                    Timed::Done { secs, ok, code } => {
                        side.last_secs = secs;
                        if !ok {
                            side.warmup_failure = Some(exit_status(code));
                        }
                    }
                }
            }
            if sides.len() == 2 {
                let failed = sides
                    .iter()
                    .enumerate()
                    .find_map(|(i, side)| side.warmup_failure.as_deref().map(|s| (i, s)));
                if let Some((i, status)) = failed {
                    return Ok((
                        warmup_failure_message(label(i), status, repeat),
                        failing_command_result(label(i)),
                    ));
                }
            }
        }

        let mut rounds = 0;
        'rounds: for round in 0..repeat {
            let order = round_order(round, sides.len());
            let need = Duration::from_secs_f64(order.iter().map(|&i| sides[i].last_secs).sum());
            if need > budget.remaining() {
                if rounds == 0 {
                    return Ok((no_round_fits_message(budget.total, need), budget_result()));
                }
                notes.push(format!(
                    "note: stopped after {rounds} of {repeat} runs per command: another round \
                     (~{need:.1}s) would not fit in what is left of timeout_sec ({total}s), which \
                     bounds the whole time-mode measurement. Raise timeout_sec for more runs.",
                    need = need.as_secs_f64(),
                    total = budget.total.as_secs(),
                ));
                break;
            }
            let mut round_runs: [Option<(f64, bool)>; 2] = [None, None];
            for &i in order {
                match self.timed_run(sides[i].cmd, budget.remaining()).await? {
                    Timed::TimedOut if rounds == 0 => {
                        return Ok((time_budget_message(label(i), budget.total), budget_result()));
                    }
                    Timed::TimedOut => {
                        notes.push(format!(
                            "note: a run of {side} was still going when the {total}s \
                             timeout_sec budget ran out; it was stopped and its round \
                             discarded, leaving {rounds} of {repeat} runs per command.",
                            side = label(i),
                            total = budget.total.as_secs(),
                        ));
                        break 'rounds;
                    }
                    Timed::Done { secs, ok, .. } => {
                        sides[i].last_secs = secs;
                        round_runs[i] = Some((secs, ok));
                    }
                }
            }
            // Keep whole rounds only, so both sides stay interleaved.
            for (side, run) in sides.iter_mut().zip(round_runs) {
                if let Some((secs, ok)) = run {
                    side.samples.push(secs);
                    if !ok {
                        side.failures += 1;
                    }
                }
            }
            rounds += 1;
        }

        let a = sides[0].measurement();
        let b = sides.get(1).map(Side::measurement);
        if let Some(note) = shell_share_note(shell_secs, &a, b.as_ref()) {
            notes.push(note);
        }
        Ok((
            render_time(repeat, &a, b.as_ref(), &notes),
            time_result(&a, b.as_ref()),
        ))
    }

    /// One `bash -c <cmd>` run timed by our clock, capped at `cap`.
    async fn timed_run(&self, cmd: &str, cap: Duration) -> Result<Timed, FunctionCallError> {
        let start = Instant::now();
        Ok(
            match run_capped("bash", &timed_argv(cmd), &self.cwd, cap).await? {
                RunOutcome::TimedOut { .. } => Timed::TimedOut,
                RunOutcome::Finished { success, code, .. } => Timed::Done {
                    secs: start.elapsed().as_secs_f64(),
                    ok: success,
                    code,
                },
            },
        )
    }

    /// The shell and process startup inside every timed sample: the fastest of
    /// a few `bash -c :` runs (`:` is a builtin, so nothing else runs). `None`
    /// when it could not be measured; it only ever feeds a note.
    async fn shell_startup(&self, budget: &Budget) -> Result<Option<f64>, FunctionCallError> {
        let mut fastest: Option<f64> = None;
        for _ in 0..SHELL_BASELINE_RUNS {
            match self.timed_run(":", budget.remaining()).await? {
                Timed::Done { secs, ok: true, .. } => {
                    fastest = Some(fastest.map_or(secs, |f| f.min(secs)));
                }
                _ => break,
            }
        }
        Ok(fastest)
    }

    // ----- profile mode (perf backend, callgrind fallback) ------------------

    async fn run_profile_mode(
        &self,
        args: &PerfArgs,
        timeout: Duration,
        force_callgrind: bool,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let perf_probe = if force_callgrind {
            Probe::NotInstalled {
                tool: "perf".to_string(),
                install_hint: "callgrind mode forces the valgrind backend".to_string(),
            }
        } else {
            crate::probe::probe("perf", &["stat", "--", "true"], &self.cwd).await
        };

        if perf_probe.is_available() {
            match self.perf_record_report(&args.command_a, timeout).await? {
                ProfileOutcome::TimedOut(secs) => {
                    return Ok((profile_timeout_message(secs), profile_timeout_result()));
                }
                ProfileOutcome::Report(text) => {
                    let result = profile_result(&text, "Ranked hot functions from perf samples.");
                    return Ok((text, result));
                }
                ProfileOutcome::Empty => { /* fall through to callgrind */ }
            }
        }

        let vg_probe = crate::probe::probe("valgrind", &["--version"], &self.cwd).await;
        if vg_probe.is_available() {
            match self.callgrind_report(&args.command_a, timeout).await? {
                ProfileOutcome::TimedOut(secs) => {
                    return Ok((profile_timeout_message(secs), profile_timeout_result()));
                }
                ProfileOutcome::Report(text) => {
                    let result = profile_result(
                        &text,
                        "Ranked hot functions from callgrind instruction counts.",
                    );
                    return Ok((text, result));
                }
                ProfileOutcome::Empty => {}
            }
        }

        Ok((
            profile_unavailable_message(&perf_probe, &vg_probe),
            unavailable_result(
                "No profiler works on this machine (perf and valgrind are both unavailable).",
            ),
        ))
    }

    async fn perf_record_report(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<ProfileOutcome, FunctionCallError> {
        let data = unique_tmp("byteask-perf.data");
        let data_str = data.to_string_lossy().to_string();
        let inner = format!(
            "perf record -F 997 -g -o {} -- bash -c {}",
            shell_quote(&data_str),
            shell_quote(cmd),
        );
        let rec = run_capped("bash", &shell_argv(inner), &self.cwd, timeout).await?;
        // perf record forwards the workload's exit status, so this is our signal
        // that the profiled command itself failed.
        let target_ok = match &rec {
            RunOutcome::Finished { success, .. } => *success,
            RunOutcome::TimedOut { .. } => true,
        };
        if let RunOutcome::TimedOut { secs } = rec {
            let _ = std::fs::remove_file(&data);
            return Ok(ProfileOutcome::TimedOut(secs));
        }
        if !data.exists() {
            return Ok(ProfileOutcome::Empty);
        }

        let report_inner = format!(
            "perf report --stdio -i {} 2>/dev/null",
            shell_quote(&data_str)
        );
        let rep = run_capped("bash", &shell_argv(report_inner), &self.cwd, timeout).await?;
        let _ = std::fs::remove_file(&data);
        match rep {
            RunOutcome::TimedOut { secs } => Ok(ProfileOutcome::TimedOut(secs)),
            RunOutcome::Finished { stdout, .. } => {
                let rows = parse_perf_report(&stdout);
                if rows.is_empty() {
                    Ok(ProfileOutcome::Empty)
                } else {
                    Ok(ProfileOutcome::Report(note_if_failed(
                        target_ok,
                        render_perf_profile(&rows),
                    )))
                }
            }
        }
    }

    async fn callgrind_report(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<ProfileOutcome, FunctionCallError> {
        let dir = unique_tmp("byteask-callgrind");
        if std::fs::create_dir_all(&dir).is_err() {
            return Ok(ProfileOutcome::Empty);
        }
        let pattern = dir.join("cg.out.%p");
        let pattern_str = pattern.to_string_lossy().to_string();
        let inner = format!(
            "valgrind --tool=callgrind --vgdb=no --trace-children=yes --callgrind-out-file={} bash -c {}",
            shell_quote(&pattern_str),
            shell_quote(cmd),
        );
        let run = run_capped("bash", &shell_argv(inner), &self.cwd, timeout).await?;
        // valgrind forwards the workload's exit status.
        let target_ok = match &run {
            RunOutcome::Finished { success, .. } => *success,
            RunOutcome::TimedOut { .. } => true,
        };
        if let RunOutcome::TimedOut { secs } = run {
            let _ = std::fs::remove_dir_all(&dir);
            return Ok(ProfileOutcome::TimedOut(secs));
        }

        // Annotate every part; the user's program is the part with the largest
        // instruction total (the traced bash shell is tiny).
        let parts = callgrind_parts(&dir);
        let n_parts = parts.len();
        let mut best: Option<(u64, String, PathBuf)> = None;
        for part in parts {
            // --auto=no: by default callgrind_annotate appends the annotated
            // SOURCE of every hot file, whose lines look like table rows.
            let annotate = format!(
                "callgrind_annotate --auto=no {}",
                shell_quote(&part.to_string_lossy())
            );
            if let RunOutcome::Finished { stdout, .. } =
                run_capped("bash", &shell_argv(annotate), &self.cwd, timeout).await?
            {
                let total = parse_callgrind_total(&stdout);
                if best.as_ref().map(|(t, _, _)| total > *t).unwrap_or(true) {
                    best = Some((total, stdout, part));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        match best {
            None => Ok(ProfileOutcome::Empty),
            Some((_, stdout, path)) => {
                let rows = parse_callgrind_rows(&stdout);
                if rows.is_empty() {
                    Ok(ProfileOutcome::Empty)
                } else {
                    Ok(ProfileOutcome::Report(note_if_failed(
                        target_ok,
                        render_callgrind_profile(&rows, n_parts, &part_pid(&path)),
                    )))
                }
            }
        }
    }

    // ----- topdown mode -----------------------------------------------------

    async fn run_topdown_mode(
        &self,
        args: &PerfArgs,
        timeout: Duration,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let cmd = &args.command_a;
        let perf_probe = crate::probe::probe("perf", &["stat", "--", "true"], &self.cwd).await;

        if perf_probe.is_available() {
            // Rung 1: real TMA (perf --topdown), plain classification.
            match self.perf_topdown(cmd, timeout).await? {
                TopdownOutcome::TimedOut(secs) => {
                    return Ok((profile_timeout_message(secs), profile_timeout_result()));
                }
                TopdownOutcome::Report(text) => {
                    let result =
                        profile_result(&text, "Bottleneck class measured with perf --topdown.");
                    return Ok((text, result));
                }
                TopdownOutcome::Empty => {}
            }
            // Rung 2: generic counters, hedged.
            match self.perf_counters_topdown(cmd, timeout).await? {
                TopdownOutcome::TimedOut(secs) => {
                    return Ok((profile_timeout_message(secs), profile_timeout_result()));
                }
                TopdownOutcome::Report(text) => {
                    let result = CheckResult::approximate(
                        TOOL_NAME,
                        "Bottleneck estimated from generic hardware counters, not a full top-down analysis.",
                    );
                    return Ok((text, result));
                }
                TopdownOutcome::Empty => {}
            }
        }

        // Rung 3: cachegrind, simulated.
        let vg_probe = crate::probe::probe("valgrind", &["--version"], &self.cwd).await;
        if vg_probe.is_available() {
            match self.cachegrind_topdown(cmd, timeout).await? {
                TopdownOutcome::TimedOut(secs) => {
                    return Ok((profile_timeout_message(secs), profile_timeout_result()));
                }
                TopdownOutcome::Report(text) => {
                    let result = CheckResult::approximate(
                        TOOL_NAME,
                        "Bottleneck simulated by cachegrind on a model cache, not measured on this CPU.",
                    );
                    return Ok((text, result));
                }
                TopdownOutcome::Empty => {}
            }
        }

        Ok((
            topdown_unavailable_message(&perf_probe, &vg_probe),
            unavailable_result(
                "No counter backend works on this machine (perf and valgrind are both unavailable).",
            ),
        ))
    }

    async fn perf_topdown(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<TopdownOutcome, FunctionCallError> {
        let inner = format!("perf stat --topdown -- bash -c {}", shell_quote(cmd));
        match run_capped("bash", &shell_argv(inner), &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => Ok(TopdownOutcome::TimedOut(secs)),
            RunOutcome::Finished {
                stderr, success, ..
            } => match parse_perf_topdown(&stderr) {
                Some((bucket, table)) => Ok(TopdownOutcome::Report(note_if_failed(
                    success,
                    render_topdown_tma(bucket, &table),
                ))),
                None => Ok(TopdownOutcome::Empty),
            },
        }
    }

    async fn perf_counters_topdown(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<TopdownOutcome, FunctionCallError> {
        // ONE perf stat invocation for the whole event set: pinning all
        // counters to a single run avoids cross-run normalization error.
        let events = "cycles,instructions,branches,branch-misses,cache-references,\
                      cache-misses,stalled-cycles-frontend,stalled-cycles-backend";
        let inner = format!("perf stat -e {events} -- bash -c {}", shell_quote(cmd));
        match run_capped("bash", &shell_argv(inner), &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => Ok(TopdownOutcome::TimedOut(secs)),
            RunOutcome::Finished {
                stderr, success, ..
            } => {
                let counters = parse_perf_counters(&stderr);
                // No usable counters means perf itself did not measure —
                // an Ubuntu `perf not found for kernel …` wrapper, or events
                // `<not supported>` with no PMU. That is not the user's
                // command failing, so fall through to cachegrind instead of
                // blaming the command ("exited non-zero") over an empty table.
                if classify_counters(&counters).is_none() {
                    return Ok(TopdownOutcome::Empty);
                }
                Ok(TopdownOutcome::Report(note_if_failed(
                    success,
                    render_counters_topdown(&counters),
                )))
            }
        }
    }

    async fn cachegrind_topdown(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<TopdownOutcome, FunctionCallError> {
        // --trace-children=yes is REQUIRED: without it valgrind detaches at the
        // bash exec and profiles only the shell, never the user's program (the
        // same class of bug the callgrind mode had). See the live test.
        // --cachegrind-out-file=/dev/null: we parse the summary on stderr, and
        // without it every call left a cachegrind.out.<pid> in the user's
        // working directory (one turned up at a repository root).
        let inner = format!(
            "valgrind --tool=cachegrind --vgdb=no --branch-sim=yes --cache-sim=yes --trace-children=yes \
             --cachegrind-out-file=/dev/null bash -c {}",
            shell_quote(cmd),
        );
        match run_capped("bash", &shell_argv(inner), &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => Ok(TopdownOutcome::TimedOut(secs)),
            RunOutcome::Finished {
                stderr, success, ..
            } => match parse_cachegrind(&stderr) {
                Some(rates) => Ok(TopdownOutcome::Report(note_if_failed(
                    success,
                    render_cachegrind_topdown(&rates),
                ))),
                None => Ok(TopdownOutcome::Empty),
            },
        }
    }
}

// ============================ helper types ================================

/// One command's runs in time mode.
struct Side<'a> {
    cmd: &'a str,
    /// Wall-clock seconds of each kept timed run (warmup excluded).
    samples: Vec<f64>,
    /// How many of `samples` exited non-zero.
    failures: u32,
    /// This side's latest run, warmup included: predicts whether the next
    /// round still fits the budget.
    last_secs: f64,
    /// How the untimed warmup failed, e.g. "exit status 3".
    warmup_failure: Option<String>,
}

impl<'a> Side<'a> {
    fn new(cmd: &'a str) -> Self {
        Side {
            cmd,
            samples: Vec::new(),
            failures: 0,
            last_secs: 0.0,
            warmup_failure: None,
        }
    }

    fn measurement(&self) -> Measurement {
        Measurement::from_samples(&self.samples, self.failures)
    }
}

/// One timed run's outcome.
enum Timed {
    Done {
        secs: f64,
        ok: bool,
        code: Option<i32>,
    },
    TimedOut,
}

/// Time mode's whole-call wall-clock budget (`timeout_sec`): every run is
/// capped at what is left of it, and no round starts that would not fit.
/// Without it `repeat: 150, timeout_sec: 1` ran for 16 s (the cap applied to
/// each run separately) and nothing at all bounded the call.
struct Budget {
    start: Instant,
    total: Duration,
}

impl Budget {
    fn new(total: Duration) -> Self {
        Budget {
            start: Instant::now(),
            total,
        }
    }

    fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.start.elapsed())
    }
}

enum ProfileOutcome {
    Report(String),
    TimedOut(u64),
    /// The backend produced no usable profile → caller walks the ladder.
    Empty,
}

enum TopdownOutcome {
    Report(String),
    TimedOut(u64),
    Empty,
}

/// The four (simplified) top-down bottleneck classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bottleneck {
    BadSpeculation,
    MemoryBound,
    FrontendBound,
    BackendBound,
    Retiring,
}

impl Bottleneck {
    fn label(self) -> &'static str {
        match self {
            Bottleneck::BadSpeculation => "bad-speculation",
            Bottleneck::MemoryBound => "memory-bound",
            Bottleneck::FrontendBound => "frontend-bound",
            Bottleneck::BackendBound => "core/backend-bound",
            Bottleneck::Retiring => "retiring (no obvious stall)",
        }
    }

    fn fix(self) -> &'static str {
        match self {
            Bottleneck::BadSpeculation => {
                "make branches predictable or branchless; reduce data-dependent branching"
            }
            Bottleneck::MemoryBound => {
                "improve locality/layout (see cpp_layout), shrink the working set, prefer \
                 sequential access"
            }
            Bottleneck::FrontendBound => {
                "reduce code size / I-cache pressure; curb excessive inlining"
            }
            Bottleneck::BackendBound => {
                "shorten dependency chains, raise ILP, consider SIMD (see cpp_intrinsic)"
            }
            Bottleneck::Retiring => {
                "already retiring efficiently — the win is doing less work (better algorithm) or SIMD"
            }
        }
    }
}

// ============================ pure helpers ================================

/// `bash -c "<cmd>"` argv for a TIMED command. Deliberately NOT a login shell:
/// `-l` sources the profile files on every run, which cost ~4.5 ms per sample
/// here (vs ~2 ms), and that constant does not cancel in a ratio: 2 ms vs 4 ms
/// of work was reported as 1.27x. The command still inherits the engine's
/// environment; what startup remains is measured and reported
/// ([`shell_share_note`]).
fn timed_argv(cmd: &str) -> Vec<String> {
    vec!["-c".to_string(), cmd.to_string()]
}

/// `bash -c <script>` argv for the profiling / topdown runs. NOT a login
/// shell: `-l` re-sources the profile files, which RESET PATH — measured, a
/// `mybench` found only on the engine's PATH "ran" as four non-zero exits
/// reported as a profile, a project-local shim earlier on PATH was bypassed for
/// /usr/bin's copy, and a fake `perf` was probed while the real one ran. The
/// probes run on the engine's environment, so the runs must too (venv, conda,
/// module, direnv and nix users all depend on it).
fn shell_argv(script: String) -> Vec<String> {
    vec!["-c".to_string(), script]
}

/// Single-quote a string for safe embedding inside a `bash -c` argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Resolve the requested repeat count: the default, then `1..=MAX_REPEAT`.
fn clamp_repeat(repeat: Option<u32>) -> u32 {
    repeat.unwrap_or(DEFAULT_REPEAT).clamp(1, MAX_REPEAT)
}

/// Time mode's run order for round `round` over `sides` commands: A,B then
/// B,A, alternating (an ABBA design). A steady drift in machine speed, or a
/// warm-cache carry-over from the previous run, then lands on both commands
/// equally instead of on whichever one runs second.
fn round_order(round: u32, sides: usize) -> &'static [usize] {
    match (sides, round % 2) {
        (1, _) => &[0],
        (_, 0) => &[0, 1],
        _ => &[1, 0],
    }
}

/// `'A'` / `'B'` for a side index.
fn label(side: usize) -> char {
    if side == 0 { 'A' } else { 'B' }
}

/// "exit status 3", or how a signal death reads (no exit code).
fn exit_status(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit status {code}"),
        None => "killed by a signal".to_string(),
    }
}

fn unknown_mode_error(mode: &str) -> String {
    format!("unknown mode {mode:?}; expected \"time\", \"profile\", \"topdown\", or \"callgrind\"")
}

/// Profile and topdown modes examine ONE command. They used to drop
/// `command_b` without a word, so a model that asked for two profiles got one
/// and could not tell.
fn command_b_error(mode: &str) -> String {
    format!(
        "command_b is only used by mode=\"time\" (A/B timing). mode={mode:?} examines one \
         command: call cpp_perf once per command, each as command_a, and compare the reports."
    )
}

/// A process-unique temp path under the system temp dir.
fn unique_tmp(prefix: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, AtomicOrdering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// Prepend a warning when the profiled command exited non-zero: the
/// profile / counters below may reflect a partial or failed run.
fn note_if_failed(success: bool, body: String) -> String {
    if success {
        body
    } else {
        format!(
            "note: the profiled command exited non-zero — the results below may reflect a partial \
             or failed run; fix the command and re-profile.\n{body}"
        )
    }
}

// ----- Shared results -----------------------------------------------------

fn budget_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::TimeLimit,
        "Not one timed round fitted in timeout_sec, so nothing was measured.",
        "call cpp_perf again with a larger `timeout_sec`, or a shorter command.",
    )
}

fn failing_command_result(side: char) -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::ToolError,
        &format!("Command {side} failed, and a failing command has nothing to compare."),
        &format!("make command {side} exit 0 on its own, then call cpp_perf again."),
    )
}

fn profile_timeout_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::TimeLimit,
        "The profiled command was stopped at the time limit, so no profile was read.",
        "call cpp_perf again with a larger `timeout_sec`, or profile a shorter run.",
    )
}

fn unavailable_result(summary: &str) -> CheckResult {
    let reason = if cfg!(target_os = "linux") {
        NotRunReason::ToolMissing
    } else {
        NotRunReason::Unsupported
    };
    CheckResult::not_run(
        TOOL_NAME,
        reason,
        summary,
        "install perf or valgrind (Linux), or use cpp_perf mode=time, which needs neither.",
    )
}

/// A profile of a command that failed is only a partial picture.
fn profile_result(text: &str, summary: &str) -> CheckResult {
    if text.starts_with("note: the profiled command exited non-zero") {
        CheckResult::approximate(
            TOOL_NAME,
            "Profile of a command that exited non-zero, so it may cover only part of the run.",
        )
    } else {
        CheckResult::pass(TOOL_NAME, summary)
    }
}

/// Beside [`render_time`]: the stats verdict decides, never a raw delta.
fn time_result(a: &Measurement, b: Option<&Measurement>) -> CheckResult {
    let Some(b) = b else {
        if a.failures > 0 {
            return CheckResult::not_run(
                TOOL_NAME,
                NotRunReason::ToolError,
                &format!(
                    "The command failed in {} of {} runs, so the timing is meaningless.",
                    a.failures, a.runs
                ),
                "make the command exit 0 on every run, then call cpp_perf again.",
            );
        }
        if a.noise.is_none() {
            return CheckResult::approximate(
                TOOL_NAME,
                "One run only, so the time has no noise estimate.",
            );
        }
        return CheckResult::pass(
            TOOL_NAME,
            &format!("Measured over {} runs with a noise estimate.", a.runs),
        );
    };
    match crate::stats::compare(a, b, TIME_FLOOR) {
        crate::stats::Verdict::Faster { winner, ratio, .. } => CheckResult::pass(
            TOOL_NAME,
            &format!("{winner} is {ratio:.2}x faster, beyond the noise gate."),
        ),
        crate::stats::Verdict::WithinNoise { delta_pct, .. } => CheckResult::no_claim(
            TOOL_NAME,
            &format!(
                "The {delta_pct:.1}% difference is inside the measurement noise; neither is faster."
            ),
        ),
        crate::stats::Verdict::NoNoiseEstimate => CheckResult::no_claim(
            TOOL_NAME,
            "No noise estimate, so no speed claim can be made.",
        ),
        crate::stats::Verdict::InvalidComparison {
            side,
            failed,
            total,
        } => CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::ToolError,
            &format!(
                "Command {side} failed in {failed} of {total} runs, so the comparison is invalid."
            ),
            &format!("make command {side} exit 0 on every run, then call cpp_perf again."),
        ),
    }
}

/// Time mode ran out of budget before a single timed round completed.
fn time_budget_message(side: char, total: Duration) -> String {
    format!(
        "cpp_perf timed out: the {secs}s timeout_sec budget (it bounds the whole time-mode \
         measurement) ran out while command {side} was running, before any timed round \
         completed. Re-run with a shorter representative workload, or raise timeout_sec.",
        secs = total.as_secs(),
    )
}

/// The warmups fit the budget but not one timed round would.
fn no_round_fits_message(total: Duration, round: Duration) -> String {
    format!(
        "cpp_perf: timeout_sec ({secs}s) bounds the whole time-mode measurement, and after the \
         untimed warmups there is no room left for a timed round (~{round:.1}s each), so nothing \
         was measured. Re-run with a shorter representative workload, or raise timeout_sec.",
        secs = total.as_secs(),
        round = round.as_secs_f64(),
    )
}

/// An A/B whose warmup failed: nothing is timed. Its own message
/// rather than an `InvalidComparison` count, which used to add the warmup to
/// the failures but not to the runs ("exited non-zero on 6/5 run(s)").
fn warmup_failure_message(side: char, status: &str, repeat: u32) -> String {
    format!(
        "Method: wall-clock timing, A and B interleaved (repeat={repeat}, warmup=1 untimed each)\n\
         INVALID COMPARISON: {side} failed its untimed warmup run ({status}) — timings of a \
         failing command are meaningless; fix the command first. No timed runs were made."
    )
}

fn profile_timeout_message(secs: u64) -> String {
    format!(
        "cpp_perf profiling timed out after {secs}s. Under a profiler (perf record / callgrind) a \
         command can run 20-100x slower, so a short native run becomes a long profile. Re-run with \
         a shorter representative workload, or raise timeout_sec."
    )
}

fn profile_unavailable_message(perf: &Probe, vg: &Probe) -> String {
    let os = std::env::consts::OS;
    let mut why = Vec::new();
    if let Some(d) = perf.describe() {
        why.push(d);
    }
    if let Some(d) = vg.describe() {
        why.push(d);
    }
    let why = if why.is_empty() {
        "no profiling backend produced output".to_string()
    } else {
        why.join("; ")
    };
    format!(
        "profiling is unavailable on {os}: {why}. What I can do instead: cpp_perf mode=time \
         (A/B wall-clock timing), or cpp_bench local mode on a Google Benchmark binary. On macOS \
         profile with Instruments / `sample` manually; on Windows use WPR / VTune manually."
    )
}

fn topdown_unavailable_message(perf: &Probe, vg: &Probe) -> String {
    let os = std::env::consts::OS;
    let mut why = Vec::new();
    if let Some(d) = perf.describe() {
        why.push(d);
    }
    if let Some(d) = vg.describe() {
        why.push(d);
    }
    let why = if why.is_empty() {
        "no counter backend produced output".to_string()
    } else {
        why.join("; ")
    };
    format!(
        "topdown bottleneck classification is unavailable on {os}: {why}. What I can do instead: \
         cpp_perf mode=time or mode=profile. Real Top-down Microarchitecture Analysis needs perf \
         (Linux) or a vendor tool (Intel VTune / AMD uProf / Apple Instruments) run manually."
    )
}

// ----- time mode rendering -----------------------------------------------

/// The time-mode report: method, one summary line per command, the verdict
/// for an A/B, then any notes (repeat cap, budget stop, shell startup).
fn render_time(repeat: u32, a: &Measurement, b: Option<&Measurement>, notes: &[String]) -> String {
    let (method, warm) = match (b.is_some(), repeat >= 2) {
        (true, true) => (
            "wall-clock timing, A and B interleaved",
            ", warmup=1 untimed each",
        ),
        (true, false) => ("wall-clock timing, A and B interleaved", ""),
        (false, true) => ("wall-clock timing", ", warmup=1 untimed"),
        (false, false) => ("wall-clock timing", ""),
    };
    let mut out = format!("Method: {method} (repeat={repeat}{warm})\n");
    out.push_str(&a.summary("A", "s", 4));
    match b {
        Some(b) => {
            out.push('\n');
            out.push_str(&b.summary("B", "s", 4));
            out.push('\n');
            let verdict = crate::stats::compare(a, b, TIME_FLOOR);
            out.push_str(&crate::stats::render_verdict(
                &verdict,
                "s",
                4,
                TIME_HINT,
                TIME_FLOOR_HINT,
            ));
        }
        None if a.noise.is_none() => out.push_str(
            "\nnote: single run — no noise estimate; differences are unverified. \
             Re-run with repeat>=5.",
        ),
        None => {}
    }
    for note in notes {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// Say how much of each sample is shell and process startup once it is large
/// enough to distort the numbers: it inflates every mean, and it shrinks an
/// A/B ratio toward 1 (2 ms vs 4 ms of work read as 1.27x under `bash -lc`).
fn shell_share_note(
    shell: Option<f64>,
    a: &Measurement,
    b: Option<&Measurement>,
) -> Option<String> {
    let shell = shell?;
    let fastest = b.map_or(a.mean, |b| a.mean.min(b.mean));
    if fastest <= 0.0 {
        return None;
    }
    let share = shell / fastest;
    (share >= SHELL_SHARE_NOTE).then(|| {
        format!(
            "note: ~{ms:.1} ms of every run is shell and process startup (`bash -c`, measured \
             separately), {pct:.0}% of the fastest mean: the programs' own times are that much \
             lower, and an A/B ratio understates their difference. Enlarge the workload for \
             cleaner numbers.",
            ms = shell * 1000.0,
            pct = share * 100.0,
        )
    })
}

// ----- perf report (profile mode) parsing --------------------------------

struct PerfRow {
    children: f64,
    self_pct: f64,
    dso: String,
    symbol: String,
}

impl PerfRow {
    /// A row whose samples could not be attributed to a real function name.
    fn is_unresolved(&self) -> bool {
        self.dso.contains("[unknown]")
            || self.symbol == "[unknown]"
            || self.symbol.starts_with("0x")
    }
}

/// Parse `perf report --stdio` rows. Columns are Children, Self, Command,
/// Shared Object, Symbol; the symbol carries a `[.]`/`[k]` map marker we strip.
fn parse_perf_report(stdout: &str) -> Vec<PerfRow> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let children = match fields.next().and_then(parse_pct) {
            Some(v) => v,
            None => continue,
        };
        let self_pct = match fields.next().and_then(parse_pct) {
            Some(v) => v,
            None => continue,
        };
        // command, then shared object (dso), then the symbol (rest of line).
        let _command = fields.next().unwrap_or("");
        let dso = fields.next().unwrap_or("").to_string();
        let mut symbol: String = fields.collect::<Vec<_>>().join(" ");
        // Strip a leading map-type marker like "[.] " / "[k] ".
        if symbol.starts_with('[') {
            if let Some(idx) = symbol.find("] ") {
                symbol = symbol[idx + 2..].to_string();
            }
        }
        if symbol.is_empty() {
            symbol = "[unknown]".to_string();
        }
        rows.push(PerfRow {
            children,
            self_pct,
            dso,
            symbol,
        });
    }
    rows
}

/// Parse a `NN.NN%` token into a fraction-of-100 number.
fn parse_pct(token: &str) -> Option<f64> {
    token.strip_suffix('%').and_then(|n| n.parse::<f64>().ok())
}

fn render_perf_profile(rows: &[PerfRow]) -> String {
    let mut sorted: Vec<&PerfRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        b.self_pct
            .partial_cmp(&a.self_pct)
            .unwrap_or(Ordering::Equal)
    });

    let mut out = String::from("Method: perf profile (perf record -F 997 -g)\n");
    out.push_str(" self%  incl%  function                                  dso\n");
    for row in sorted.iter().take(PROFILE_TOP) {
        out.push_str(&format!(
            "{self_pct:>5.1}  {incl:>5.1}  {sym:<40}  {dso}\n",
            self_pct = row.self_pct,
            incl = row.children,
            sym = trunc(&row.symbol, 40),
            dso = trunc(&row.dso, 24),
        ));
    }
    out.push_str(&symbol_quality_line(rows));
    out
}

/// The symbol-quality contract: share of top-table self% mass on
/// unresolved rows, labeled approximate.
fn symbol_quality_line(rows: &[PerfRow]) -> String {
    let total: f64 = rows.iter().map(|r| r.self_pct).sum();
    let unresolved: f64 = rows
        .iter()
        .filter(|r| r.is_unresolved())
        .map(|r| r.self_pct)
        .sum();
    symbol_quality(
        "self%",
        total,
        unresolved,
        "Rebuild with -g (and an unstripped binary); add -fno-omit-frame-pointer for call \
         graphs (or profile with dwarf call graphs); for system libs install debug symbols.",
    )
}

/// The same contract for the callgrind table, over its Ir shares. Callgrind
/// follows calls by instrumentation, so frame pointers do not matter here.
fn callgrind_symbol_quality_line(rows: &[CallgrindRow]) -> String {
    let total: f64 = rows.iter().map(|r| r.pct).sum();
    let unresolved: f64 = rows
        .iter()
        .filter(|r| r.is_unresolved())
        .map(|r| r.pct)
        .sum();
    symbol_quality(
        "Ir",
        total,
        unresolved,
        "Rebuild with -g and do not strip the binary; for system libs install debug symbols.",
    )
}

/// `unresolved / total` of the table's `mass` (`self%`, `Ir`) had no name.
fn symbol_quality(mass: &str, total: f64, unresolved: f64, advice: &str) -> String {
    let frac = if total > 0.0 { unresolved / total } else { 0.0 };
    let pct = frac * 100.0;
    if frac > SYMBOL_QUALITY_POOR {
        format!(
            "symbol quality: POOR (~{pct:.0}% of top-table {mass} mass unsymbolized; \
             approximate). {advice}"
        )
    } else {
        let resolved = 100.0 - pct;
        format!(
            "symbol quality: OK (~{resolved:.0}% of top-table {mass} mass symbolized; approximate)"
        )
    }
}

// ----- callgrind (profile mode) parsing ----------------------------------

struct CallgrindRow {
    ir: u64,
    pct: f64,
    function: String,
}

impl CallgrindRow {
    /// No function name: a stripped binary shows up as `???:0x0000000000001139`.
    fn is_unresolved(&self) -> bool {
        self.function.starts_with("0x") || self.function.is_empty() || self.function == "???"
    }
}

/// List the `cg.out.<pid>` parts callgrind produced in `dir`.
fn callgrind_parts(dir: &Path) -> Vec<PathBuf> {
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cg.out.") {
                parts.push(entry.path());
            }
        }
    }
    parts
}

fn part_pid(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("cg.out."))
        .unwrap_or("?")
        .to_string()
}

/// Parse the `NN,NNN (100.0%)  PROGRAM TOTALS` instruction count.
fn parse_callgrind_total(annotate: &str) -> u64 {
    annotate
        .lines()
        .find(|l| l.contains("PROGRAM TOTALS"))
        .and_then(|l| l.split_whitespace().next())
        .map(|t| t.replace(',', ""))
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Parse the `Ir  file:function` table rows:
/// `<Ir> (<pct>%)  <file>:<function> [<binary>]`. Two traps, both measured on
/// real C++ output: the percentage is padded below 10% (`( 8.49%)`), and a C++
/// function carries its own `::`, parentheses and spaces
/// (`hotfn.cpp:engine::Book::hash(std::__cxx11::basic_string<...> const&)`).
/// Splitting on the LAST colon and the first `)` turned that into
/// `allocator<char> > const&`, and every row under 10% read 0.00.
fn parse_callgrind_rows(annotate: &str) -> Vec<CallgrindRow> {
    let mut rows = Vec::new();
    let mut in_table = false;
    let mut past_rule = false;
    for line in annotate.lines() {
        let trimmed = line.trim();
        if !in_table {
            in_table = trimmed.ends_with("file:function");
            continue;
        }
        if trimmed.starts_with('-') && !past_rule {
            past_rule = true; // the rule under the header
            continue;
        }
        // The table ends at the first blank line or rule. What follows —
        // auto-annotated source lines, the "events annotated" footer — has
        // the same `<count> (<pct>%)  <text>` shape and was once reported as
        // hot "functions" (`for (unsigned long i = 0; ...` at 99.91%).
        if trimmed.is_empty() || trimmed.starts_with('-') {
            break;
        }
        let Some((ir, rest)) = trimmed.split_once(' ') else {
            continue;
        };
        let Ok(ir) = ir.replace(',', "").parse::<u64>() else {
            continue;
        };
        // The percentage is the first parenthesized group, before the name.
        let Some((pct, location)) = rest
            .trim_start()
            .strip_prefix('(')
            .and_then(|r| r.split_once(')'))
        else {
            continue;
        };
        let pct = parse_pct(pct.trim()).unwrap_or(0.0);
        // Drop the trailing " [binary]": the LAST bracket group on the line.
        let location = location.trim();
        let location = match location.rfind(" [") {
            Some(at) if location.ends_with(']') => &location[..at],
            _ => location,
        };
        // `file:function`: the file is a path (`???` when unknown) and holds no
        // colon; everything after the FIRST colon is the function.
        let function = location
            .split_once(':')
            .map_or(location, |(_, function)| function)
            .to_string();
        rows.push(CallgrindRow { ir, pct, function });
    }
    rows
}

fn render_callgrind_profile(rows: &[CallgrindRow], n_parts: usize, pid: &str) -> String {
    let mut out = format!(
        "Method: callgrind profile [simulated: instruction counts, not wall time]\n\
         profiled {n_parts} process(es); showing the hottest (PID {pid})\n"
    );
    out.push_str("    Ir%  Ir              function\n");
    for row in rows.iter().take(PROFILE_TOP) {
        out.push_str(&format!(
            "{pct:>6.2}  {ir:>14}  {func}\n",
            pct = row.pct,
            ir = row.ir,
            func = trunc(&row.function, 50),
        ));
    }
    // The guidance promises this check for mode=profile, on every backend.
    out.push_str(&callgrind_symbol_quality_line(rows));
    out
}

// ----- topdown parsing ---------------------------------------------------

/// Best-effort parse of a `perf stat --topdown` L1 table: a header row naming
/// the four buckets followed by a row of four percentages. Returns the winning
/// bucket label + the full `(bucket, pct)` table, or `None` if the format does
/// not clearly match (caller falls through to generic counters).
fn parse_perf_topdown(stderr: &str) -> Option<(String, Vec<(String, f64)>)> {
    let lines: Vec<&str> = stderr.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let low = line.to_ascii_lowercase();
        if low.contains("retiring")
            && low.contains("frontend")
            && low.contains("backend")
            && low.contains("speculation")
        {
            // The next non-empty line should carry four percentages.
            let row = lines.get(i + 1..)?.iter().find(|l| !l.trim().is_empty())?;
            let pcts: Vec<f64> = row.split_whitespace().filter_map(parse_pct).collect();
            if pcts.len() == 4 {
                let table = vec![
                    ("retiring".to_string(), pcts[0]),
                    ("bad-speculation".to_string(), pcts[1]),
                    ("frontend-bound".to_string(), pcts[2]),
                    ("backend-bound".to_string(), pcts[3]),
                ];
                let winner = table
                    .iter()
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))?
                    .0
                    .clone();
                return Some((winner, table));
            }
        }
    }
    None
}

fn render_topdown_tma(bucket: String, table: &[(String, f64)]) -> String {
    let mut out = String::from("Method: topdown (perf --topdown, real Top-down analysis)\n");
    out.push_str(&format!("bottleneck: {bucket}\n"));
    for (name, pct) in table {
        out.push_str(&format!("  {name:<16} {pct:>6.1}%\n"));
    }
    let class = classify_tma_label(&bucket);
    out.push_str(&format!("fix: {}", class.fix()));
    out
}

fn classify_tma_label(bucket: &str) -> Bottleneck {
    match bucket {
        "bad-speculation" => Bottleneck::BadSpeculation,
        "frontend-bound" => Bottleneck::FrontendBound,
        "backend-bound" => Bottleneck::BackendBound,
        "retiring" => Bottleneck::Retiring,
        _ => Bottleneck::BackendBound,
    }
}

/// Parsed generic perf counters. `None` fields mean the event was unsupported /
/// not counted on this machine.
#[derive(Debug, Default, PartialEq)]
struct Counters {
    cycles: Option<f64>,
    instructions: Option<f64>,
    branches: Option<f64>,
    branch_misses: Option<f64>,
    cache_refs: Option<f64>,
    cache_misses: Option<f64>,
    fe_stall: Option<f64>,
    be_stall: Option<f64>,
}

/// Parse `perf stat -e ...` stderr: each counted event is a `<value> <event>`
/// line; `<not supported>` / `<not counted>` lines are skipped.
fn parse_perf_counters(stderr: &str) -> Counters {
    let mut c = Counters::default();
    for line in stderr.lines() {
        let mut fields = line.split_whitespace();
        let value_tok = match fields.next() {
            Some(t) => t,
            None => continue,
        };
        if value_tok.starts_with('<') {
            continue; // <not supported> / <not counted>
        }
        let value = match value_tok.replace(',', "").parse::<f64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event = match fields.next() {
            Some(e) => e,
            None => continue,
        };
        match event {
            "cycles" => c.cycles = Some(value),
            "instructions" => c.instructions = Some(value),
            "branches" => c.branches = Some(value),
            "branch-misses" => c.branch_misses = Some(value),
            "cache-references" => c.cache_refs = Some(value),
            "cache-misses" => c.cache_misses = Some(value),
            "stalled-cycles-frontend" => c.fe_stall = Some(value),
            "stalled-cycles-backend" => c.be_stall = Some(value),
            _ => {}
        }
    }
    c
}

/// Classify the bottleneck from generic counters, or `None` if the surviving
/// event set is too thin to say anything ("insufficient counters").
fn classify_counters(c: &Counters) -> Option<Bottleneck> {
    let (cycles, instructions, branches, branch_misses) =
        match (c.cycles, c.instructions, c.branches, c.branch_misses) {
            (Some(cy), Some(i), Some(b), Some(bm)) if cy > 0.0 && b > 0.0 => (cy, i, b, bm),
            _ => return None,
        };
    let ipc = instructions / cycles;
    let branch_miss_rate = branch_misses / branches;
    let llc_miss_rate = match (c.cache_misses, c.cache_refs) {
        (Some(m), Some(r)) if r > 0.0 => Some(m / r),
        _ => None,
    };
    let fe = c.fe_stall.map(|s| s / cycles);
    let be = c.be_stall.map(|s| s / cycles);

    if branch_miss_rate > 0.05 {
        return Some(Bottleneck::BadSpeculation);
    }
    if let Some(llc) = llc_miss_rate {
        if llc > 0.10 && be.map(|b| b > 0.20).unwrap_or(true) {
            return Some(Bottleneck::MemoryBound);
        }
    }
    if let Some(fe) = fe {
        if fe > 0.30 {
            return Some(Bottleneck::FrontendBound);
        }
    }
    if let Some(be) = be {
        if be > 0.40 {
            return Some(Bottleneck::BackendBound);
        }
    }
    if ipc < 1.0 {
        return Some(Bottleneck::BackendBound);
    }
    Some(Bottleneck::Retiring)
}

fn render_counters_topdown(c: &Counters) -> String {
    let mut out =
        String::from("Method: topdown (heuristic from generic perf counters) [heuristic]\n");
    match classify_counters(c) {
        None => {
            out.push_str(
                "insufficient counters to classify (need at least cycles, instructions, branches, \
                 branch-misses on this machine).\n",
            );
            out.push_str(&counter_rates_line(c));
            out
        }
        Some(bucket) => {
            out.push_str(&format!("suggests: {}\n", bucket.label()));
            out.push_str(&counter_rates_line(c));
            out.push('\n');
            out.push_str(
                "blind spots: generic counters cannot see out-of-order overlap or hardware \
                 prefetch — a high miss rate may be fully hidden. Treat this as a hint; confirm \
                 with perf --topdown (real TMA) where available.\n",
            );
            out.push_str(&format!("fix: {}", bucket.fix()));
            out
        }
    }
}

/// A compact one-line rate summary (IPC, branch-miss%, LLC-miss%, stall%),
/// including only the metrics present.
fn counter_rates_line(c: &Counters) -> String {
    let mut parts = Vec::new();
    if let (Some(i), Some(cy)) = (c.instructions, c.cycles) {
        if cy > 0.0 {
            parts.push(format!("IPC {:.2}", i / cy));
        }
    }
    if let (Some(bm), Some(b)) = (c.branch_misses, c.branches) {
        if b > 0.0 {
            parts.push(format!("branch-miss {:.2}%", bm / b * 100.0));
        }
    }
    if let (Some(m), Some(r)) = (c.cache_misses, c.cache_refs) {
        if r > 0.0 {
            parts.push(format!("LLC-miss {:.2}%", m / r * 100.0));
        }
    }
    if let (Some(fe), Some(cy)) = (c.fe_stall, c.cycles) {
        if cy > 0.0 {
            parts.push(format!("frontend-stall {:.0}%", fe / cy * 100.0));
        }
    }
    if let (Some(be), Some(cy)) = (c.be_stall, c.cycles) {
        if cy > 0.0 {
            parts.push(format!("backend-stall {:.0}%", be / cy * 100.0));
        }
    }
    if parts.is_empty() {
        "counters: (none usable)".to_string()
    } else {
        format!("counters: {}", parts.join(" | "))
    }
}

/// Miss/mispredict rates parsed from a cachegrind summary.
#[derive(Debug, Default, PartialEq)]
struct CachegrindRates {
    d1_miss: Option<f64>,
    ll_miss: Option<f64>,
    mispredict: Option<f64>,
}

fn parse_cachegrind(stderr: &str) -> Option<CachegrindRates> {
    // With --trace-children the output can carry more than one process's summary
    // (each ==PID==-prefixed). Group by PID and keep the process with the most
    // instruction refs — the user's program; a traced bash shell is tiny.
    let mut by_pid: BTreeMap<String, (f64, CachegrindRates)> = BTreeMap::new();
    for line in stderr.lines() {
        let pid = valgrind_pid(line).unwrap_or_default();
        let entry = by_pid.entry(pid).or_default();
        let low = line.to_ascii_lowercase();
        if low.contains("i   refs") {
            if let Some(v) = first_count(line) {
                entry.0 = v;
            }
        } else if low.contains("d1  miss rate") {
            entry.1.d1_miss = first_pct(line);
        } else if low.contains("lld miss rate") || low.contains("ll miss rate") {
            entry.1.ll_miss = first_pct(line);
        } else if low.contains("mispred rate") || low.contains("mispredict rate") {
            entry.1.mispredict = first_pct(line);
        }
    }
    by_pid
        .into_values()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal))
        .map(|(_, r)| r)
        .filter(|r| *r != CachegrindRates::default())
}

/// Extract the `NNN` from a valgrind `==NNN== ...` line prefix.
fn valgrind_pid(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("==")?;
    let pid = rest.split("==").next()?.trim();
    if pid.chars().all(|c| c.is_ascii_digit()) && !pid.is_empty() {
        Some(pid.to_string())
    } else {
        None
    }
}

/// Pull the first `NN.N%` token out of a cachegrind summary line.
fn first_pct(line: &str) -> Option<f64> {
    line.split_whitespace().find_map(parse_pct)
}

/// Pull the first comma-formatted integer token out of a line.
fn first_count(line: &str) -> Option<f64> {
    line.split_whitespace()
        .find_map(|t| t.replace(',', "").parse::<f64>().ok())
}

fn render_cachegrind_topdown(r: &CachegrindRates) -> String {
    // Classify from simulated cache/branch rates using the same thresholds.
    let bucket = cachegrind_bottleneck(r);
    let mut out = String::from(
        "Method: topdown (cachegrind) [simulated: a cache/branch model with no out-of-order \
         execution and no hardware prefetch — miss rates can overstate real stalls]\n",
    );
    match bucket {
        Some(bucket) => out.push_str(&format!("suggests: {}\n", bucket.label())),
        None => out.push_str(
            "suggests: INCONCLUSIVE — no memory or branch problem in the simulated model, and \
             cachegrind cannot see what is left (core/backend-bound vs frontend-bound vs \
             already retiring well).\n",
        ),
    }
    let mut rates = Vec::new();
    if let Some(v) = r.d1_miss {
        rates.push(format!("D1-miss {v:.1}%"));
    }
    if let Some(v) = r.ll_miss {
        rates.push(format!("LL-miss {v:.1}%"));
    }
    if let Some(v) = r.mispredict {
        rates.push(format!("branch-mispredict {v:.1}%"));
    }
    out.push_str(&format!("rates: {}\n", rates.join(" | ")));
    out.push_str(
        "blind spots: cachegrind simulates an idealized cache and does NOT model out-of-order \
         overlap or prefetch. Confirm with perf --topdown on real hardware where available.\n",
    );
    match bucket {
        Some(bucket) => out.push_str(&format!("fix: {}", bucket.fix())),
        None => out.push_str(
            "next: measure candidate changes directly with cpp_perf mode=time, or run \
             mode=topdown on hardware with perf counters to classify the core-side bottleneck.",
        ),
    }
    out
}

/// cachegrind simulates caches and branch prediction only. It can show
/// a memory or bad-speculation problem; it has NO evidence for anything else,
/// so "neither" is `None` (inconclusive) — it used to default to
/// "core/backend-bound" even for a pure ALU loop with 0% misses, and the
/// frontend-bound / retiring answers the tool promises were unreachable.
fn cachegrind_bottleneck(r: &CachegrindRates) -> Option<Bottleneck> {
    if r.mispredict.is_some_and(|m| m > 5.0) {
        Some(Bottleneck::BadSpeculation)
    } else if r.ll_miss.is_some_and(|m| m > 1.0) {
        Some(Bottleneck::MemoryBound)
    } else {
        None
    }
}

/// Truncate a display string to `max` chars, appending `…` when cut.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
#[path = "perf_tests.rs"]
mod tests;

/// The shared result beside the time-mode verdict.
#[cfg(test)]
mod result_tests {
    use check_result::Verdict;
    use pretty_assertions::assert_eq;

    use super::*;

    fn m(mean: f64, noise: Option<f64>, failures: u32) -> Measurement {
        Measurement {
            mean,
            noise,
            runs: 10,
            failures,
        }
    }

    #[test]
    fn inside_the_noise_is_no_claim_never_a_pass() {
        let result = time_result(&m(1.00, Some(0.05), 0), Some(&m(0.99, Some(0.05), 0)));
        assert_eq!(result.verdict, Verdict::NoClaim);
    }

    #[test]
    fn a_win_beyond_the_noise_gate_is_a_pass() {
        let result = time_result(&m(2.0, Some(0.001), 0), Some(&m(1.0, Some(0.001), 0)));
        assert!(result.is_pass(), "{result:?}");
        assert!(result.summary.contains("faster"), "{}", result.summary);
    }

    #[test]
    fn a_failing_side_makes_the_comparison_not_run() {
        let result = time_result(&m(1.0, Some(0.01), 2), Some(&m(1.0, Some(0.01), 0)));
        assert_eq!(result.verdict, Verdict::NotRun);
        let single = time_result(&m(1.0, Some(0.01), 1), None);
        assert_eq!(single.verdict, Verdict::NotRun);
    }

    #[test]
    fn one_run_without_noise_is_approximate() {
        assert_eq!(
            time_result(&m(1.0, None, 0), None).verdict,
            Verdict::Approximate
        );
        assert!(time_result(&m(1.0, Some(0.01), 0), None).is_pass());
    }

    #[test]
    fn a_profile_of_a_failing_command_is_approximate() {
        let failed = note_if_failed(false, "rows".to_string());
        assert_eq!(profile_result(&failed, "x").verdict, Verdict::Approximate);
        assert!(profile_result("rows", "x").is_pass());
    }
}
