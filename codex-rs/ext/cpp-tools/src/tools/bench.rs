use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_login::default_client::create_client;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;
use crate::stats::Measurement;

const TOOL_NAME: &str = "cpp_bench";
/// Default Google Benchmark repetitions for the local mode.
const DEFAULT_REPETITIONS: u32 = 9;
const DEFAULT_COMPILER: &str = "clang-17.0";
const DEFAULT_CPP_VERSION: &str = "c++20";
const DEFAULT_OPTIM: &str = "3";
const DEFAULT_LIB: &str = "gnu";
/// Public quick-bench.com origin. Requests go directly from the user's machine
/// (their own IP), like the godbolt tool, rather than through the ByteAsk
/// gateway. Override with `BYTEASK_QUICKBENCH_URL` (used by tests).
const QUICKBENCH_ORIGIN: &str = "https://quick-bench.com";
/// Cap the error body we echo back to the model.
const ERROR_BODY_CAP: usize = 2000;
/// Closing caveat appended to every successful report. quick-bench normalizes
/// every case to a `Noop` loop (= 1.0), so the numbers are relative, not times.
const CAVEAT: &str = "note: quick-bench reports cpu_time normalized to a noop loop (Noop = 1.0), \
    not absolute wall time. Treat A/B ratios as a guide and re-run to confirm.";

/// Microbenchmark C++ via the public quick-bench.com service. The snippet is
/// sent verbatim — quick-bench supplies `<benchmark/benchmark.h>`, `main`, and
/// `BENCHMARK_MAIN()` itself.
pub struct BenchTool {
    #[allow(dead_code)]
    pub cwd: PathBuf,
    /// LOCAL mode runs an existing benchmark binary natively, outside
    /// the filesystem sandbox. Remote mode only posts source to quick-bench.com
    /// and is not gated. See [`crate::trust`].
    pub native_exec: bool,
}

/// `deny_unknown_fields`: a misnamed field used to vanish silently —
/// `"benchmark_filter"` for `filter` and `"reps"` for `repetitions` ran EVERY
/// benchmark at the default 9 repetitions (41 s) and reported success.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BenchArgs {
    // Remote (quick-bench.com) inputs. `source` is now optional: a call provides
    // EITHER `source` (remote) OR `binary` (local), validated at runtime.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    std: Option<String>,
    #[serde(default)]
    optim: Option<String>,
    #[serde(default)]
    compiler: Option<String>,
    #[serde(default)]
    lib: Option<String>,
    #[serde(default)]
    flags: Option<String>,
    // Local mode inputs.
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    repetitions: Option<u32>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for BenchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "source".to_string(),
            JsonSchema::string(Some(
                "REMOTE mode. Inline C++ containing ONLY your benchmark function(s), e.g. \
                 `static void BM_X(benchmark::State& state){ for (auto _ : state){ /* work */ } } \
                 BENCHMARK(BM_X);`. Do NOT add `#include <benchmark/benchmark.h>`, a `main`, or \
                 `BENCHMARK_MAIN()` — quick-bench.com wraps the snippet for you. Define two \
                 BENCHMARK() cases to compare A vs B. Provide EITHER source OR binary, not both."
                    .to_string(),
            )),
        );
        properties.insert(
            "std".to_string(),
            JsonSchema::string(Some(format!(
                "C++ standard (quick-bench `cppVersion`), default \"{DEFAULT_CPP_VERSION}\". \
                 Examples: \"c++17\", \"c++23\"."
            ))),
        );
        properties.insert(
            "optim".to_string(),
            JsonSchema::string(Some(format!(
                "Optimization level (the -O digit), default \"{DEFAULT_OPTIM}\". Use release \
                 optimization (\"2\" or \"3\") for meaningful numbers."
            ))),
        );
        properties.insert(
            "compiler".to_string(),
            JsonSchema::string(Some(format!(
                "quick-bench compiler id, default \"{DEFAULT_COMPILER}\". Examples: \"gcc-13.1\", \
                 \"clang-16.0\"."
            ))),
        );
        properties.insert(
            "lib".to_string(),
            JsonSchema::string(Some(format!(
                "Standard library: \"gnu\" (libstdc++) or \"llvm\" (libc++), default \
                 \"{DEFAULT_LIB}\"."
            ))),
        );
        properties.insert(
            "flags".to_string(),
            JsonSchema::string(Some(
                "REMOTE mode. Extra compiler flags, whitespace-separated (default none), e.g. \
                 \"-march=native\"."
                    .to_string(),
            )),
        );
        properties.insert(
            "binary".to_string(),
            JsonSchema::string(Some(
                "LOCAL mode. Path to an already-built Google Benchmark executable to run on THIS \
                 machine (pinned, repeated, noise-gated, ABSOLUTE ns). Build the bench target \
                 first. Provide EITHER binary OR source, not both."
                    .to_string(),
            )),
        );
        properties.insert(
            "filter".to_string(),
            JsonSchema::string(Some(
                "LOCAL mode. Optional --benchmark_filter regex to select which benchmarks in the \
                 binary to run (default: all)."
                    .to_string(),
            )),
        );
        properties.insert(
            "repetitions".to_string(),
            JsonSchema::integer(Some(format!(
                "LOCAL mode. Google Benchmark repetitions (default: {DEFAULT_REPETITIONS}). \
                 Use >=5 for a usable noise estimate."
            ))),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::integer(Some(
                "LOCAL mode. Per-run wall-clock cap in seconds (default: 300).".to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Microbenchmark C++ with Google Benchmark. Two modes: REMOTE (`source` = \
                inline BENCHMARK() cases run on quick-bench.com; RELATIVE cpu_time, foreign \
                hardware), or LOCAL (`binary` = an already-built Google Benchmark executable run \
                on THIS machine, pinned + repeated + noise-gated, ABSOLUTE ns — never claims a \
                difference inside measurement noise). Use to answer 'which is faster?'. Provide \
                exactly one of source|binary. C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                // Neither is individually required; exactly-one-of is validated
                // at runtime (JSON Schema cannot express xor cleanly here).
                None,
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
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl BenchTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: BenchArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        // Exactly one of source (remote) | binary (local).
        match select_mode(args.binary.as_deref(), args.source.as_deref()) {
            BenchMode::Local(binary) => {
                crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
                self.run_local(&args, binary).await
            }
            BenchMode::Remote(source) => self.run_remote(&args, source).await,
            BenchMode::Both | BenchMode::Neither => {
                Err(FunctionCallError::RespondToModel(select_mode_error()))
            }
        }
    }

    /// Remote mode: send the snippet to quick-bench.com (unchanged behavior).
    async fn run_remote(
        &self,
        args: &BenchArgs,
        source: &str,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let std = args.std.as_deref().unwrap_or(DEFAULT_CPP_VERSION);
        let optim = args.optim.as_deref().unwrap_or(DEFAULT_OPTIM);
        let compiler = args.compiler.as_deref().unwrap_or(DEFAULT_COMPILER);
        let lib = args.lib.as_deref().unwrap_or(DEFAULT_LIB);
        let flags = split_flags(args.flags.as_deref().unwrap_or(""));

        // The snippet is sent verbatim: quick-bench wraps it with the include
        // and BENCHMARK_MAIN(), so we must NOT prepend anything ourselves.
        let body = build_request_body(source, compiler, std, optim, lib, &flags);
        let url = quick_url();

        let client = create_client();
        let response = client
            .post(url.as_str())
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "Quick Bench (quick-bench.com) is unavailable: request failed: {err}"
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            let text = cap_text(response.text().await.unwrap_or_default(), ERROR_BODY_CAP);
            return Err(FunctionCallError::RespondToModel(format!(
                "Quick Bench (quick-bench.com) is unavailable: HTTP {}: {text}",
                status.as_u16()
            )));
        }

        let value: Value = response.json().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "Quick Bench (quick-bench.com) is unavailable: invalid JSON: {err}"
            ))
        })?;

        match parse_result(&value) {
            Ok((ctx, rows)) => Ok(crate::report::report(
                TOOL_NAME,
                compiler,
                render_report(&ctx, &rows),
                check_result::CheckResult::approximate(
                    TOOL_NAME,
                    "Relative times from Quick Bench's shared remote machine, without a noise estimate.",
                ),
            )),
            Err(parse_err) => {
                // No benchmarks: either a compile error/timeout (carried in
                // `message`) or an unexpected empty payload.
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(clean_message)
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or(parse_err);
                let message = cap_text(message, ERROR_BODY_CAP);
                Err(FunctionCallError::RespondToModel(format!(
                    "Quick Bench (quick-bench.com) is unavailable: {}",
                    message.trim_end()
                )))
            }
        }
    }

    /// Local mode: run an already-built Google Benchmark binary on THIS machine,
    /// pinned + repeated + noise-gated, and report ABSOLUTE times.
    ///
    /// ONE run, with the JSON written to a file of ours (`--benchmark_out`).
    /// The JSON used to be read from stdout, so a benchmark that printed
    /// anything there ("loading fixture data...") failed to parse; and a
    /// `--benchmark_list_tests` probe run first only checked the exit status,
    /// so a program that is not a benchmark at all passed it and was started
    /// a second time, side effects and all.
    async fn run_local(
        &self,
        args: &BenchArgs,
        binary: &str,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let reps = args.repetitions.unwrap_or(DEFAULT_REPETITIONS).max(1);
        let timeout = timeout_from(args.timeout_sec);
        let bin = resolve_binary(&self.cwd, binary);
        let bin_str = bin.to_string_lossy().to_string();
        let json_out = JsonOut::create().map_err(json_out_error)?;

        let pinning = self.pinning().await;
        let (program, argv) = build_local_argv(
            &bin_str,
            args.filter.as_deref(),
            reps,
            &json_out.0,
            &pinning,
        );

        match run_capped(&program, &argv, &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => Ok(crate::report::report_plain(
                TOOL_NAME,
                local_timeout_message(secs),
                crate::results::timed_out(
                    TOOL_NAME,
                    "The benchmark",
                    secs,
                    "call cpp_bench again with a larger `timeout_sec`, a `filter`, or fewer `repetitions`.",
                ),
            )),
            RunOutcome::Finished {
                success,
                code,
                stderr,
                ..
            } => {
                let json = std::fs::read_to_string(&json_out.0).unwrap_or_default();
                if json.trim().is_empty() {
                    return Err(FunctionCallError::RespondToModel(no_json_message(
                        binary,
                        args.filter.as_deref(),
                        success,
                        code,
                        &stderr,
                    )));
                }
                let value: Value = serde_json::from_str(&json).map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "Google Benchmark's JSON output did not parse ({err}); the run ended \
                         with {}. stderr: {}",
                        exit_status(success, code),
                        stderr.trim()
                    ))
                })?;
                let ctx = parse_context(value.get("context"));
                let rows = parse_gbench_rows(&value, reps);
                if rows.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "Google Benchmark ran but reported no benchmarks (check your filter)."
                            .to_string(),
                    ));
                }
                Ok(crate::report::report(
                    TOOL_NAME,
                    binary,
                    render_local_report(&ctx, &rows, &pinning),
                    if success {
                        check_result::CheckResult::pass(
                            TOOL_NAME,
                            &format!("Measured {} benchmark(s) on this machine.", rows.len()),
                        )
                    } else {
                        check_result::CheckResult::approximate(
                            TOOL_NAME,
                            &format!(
                                "The benchmark ended with {}, so these rows may be partial.",
                                exit_status(success, code)
                            ),
                        )
                    },
                ))
            }
        }
    }

    /// Resolve pinning: the last CPU via `taskset` on Linux when installed, else
    /// none (and the report says so).
    async fn pinning(&self) -> Pinning {
        if crate::probe::probe("taskset", &["--version"], &self.cwd)
            .await
            .is_available()
        {
            let last = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1))
                .unwrap_or(0);
            Pinning::Cpu(last)
        } else {
            Pinning::None
        }
    }
}

/// Which mode a `cpp_bench` call selected (exactly one of source|binary).
#[derive(Debug, PartialEq)]
enum BenchMode<'a> {
    Local(&'a str),
    Remote(&'a str),
    Both,
    Neither,
}

fn select_mode<'a>(binary: Option<&'a str>, source: Option<&'a str>) -> BenchMode<'a> {
    match (binary, source) {
        (Some(b), None) => BenchMode::Local(b),
        (None, Some(s)) => BenchMode::Remote(s),
        (Some(_), Some(_)) => BenchMode::Both,
        (None, None) => BenchMode::Neither,
    }
}

fn select_mode_error() -> String {
    "provide exactly one of `source` (inline benchmark for quick-bench.com) or `binary` (path to a \
     local Google Benchmark executable) — not both, and not neither."
        .to_string()
}

/// How the local benchmark process is CPU-pinned.
enum Pinning {
    Cpu(usize),
    None,
}

fn resolve_binary(cwd: &Path, binary: &str) -> PathBuf {
    let p = Path::new(binary);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Build the run argv, wrapping in `taskset -c <cpu>` when pinning. The JSON
/// goes to `out`; stdout stays the program's own.
fn build_local_argv(
    bin: &str,
    filter: Option<&str>,
    reps: u32,
    out: &Path,
    pinning: &Pinning,
) -> (String, Vec<String>) {
    let mut bench_args = Vec::new();
    if let Some(f) = filter {
        bench_args.push(format!("--benchmark_filter={f}"));
    }
    bench_args.push(format!("--benchmark_repetitions={reps}"));
    bench_args.push(format!("--benchmark_out={}", out.to_string_lossy()));
    bench_args.push("--benchmark_out_format=json".to_string());
    match pinning {
        Pinning::Cpu(cpu) => {
            let mut argv = vec!["-c".to_string(), cpu.to_string(), bin.to_string()];
            argv.extend(bench_args);
            ("taskset".to_string(), argv)
        }
        Pinning::None => (bin.to_string(), bench_args),
    }
}

/// A file of ours for `--benchmark_out`, removed on drop. Made with
/// `create_new`, so the path in the shared temp dir cannot be a planted
/// symlink that the benchmark would then overwrite.
struct JsonOut(PathBuf);

impl JsonOut {
    fn create() -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("byteask-gbench-{}-{n}.json", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(JsonOut(path)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for JsonOut {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The results file could not be created. Not `Fatal`: that ends the user's
/// whole turn, and a full or unwritable temp directory is something the
/// model can report.
fn json_out_error(err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "could not create a temporary file for the benchmark's JSON results in {} ({err}), so \
         the benchmark was NOT run. The temp directory (TMPDIR) must exist and be writable.",
        std::env::temp_dir().display()
    ))
}

/// "exit status 3" / "a signal" for a finished run.
fn exit_status(success: bool, code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit status {code}"),
        None if success => "exit status 0".to_string(),
        None => "a signal".to_string(),
    }
}

/// Why a local run produced no benchmark JSON, in the terms the model can act
/// on. The program did run (once), so say so.
fn no_json_message(
    binary: &str,
    filter: Option<&str>,
    success: bool,
    code: Option<i32>,
    stderr: &str,
) -> String {
    let stderr = stderr.trim();
    if stderr.contains("Failed to match any benchmarks") {
        return match filter {
            Some(filter) => format!(
                "no benchmark in '{binary}' matches filter {filter:?}: {stderr}. Drop or widen \
                 the filter."
            ),
            None => format!("'{binary}' has no benchmarks registered: {stderr}"),
        };
    }
    if success {
        return format!(
            "'{binary}' ran and exited 0 but wrote no Google Benchmark JSON, so it is not a \
             Google Benchmark binary. It WAS run once, with Google Benchmark's flags as its \
             arguments. Build the bench target first (e.g. `cmake --build build --target \
             <bench>`) and pass that binary."
        );
    }
    format!(
        "'{binary}' is not a runnable Google Benchmark binary: it ended with {} without writing \
         any benchmark JSON. Build the bench target first (e.g. `cmake --build build --target \
         <bench>`). Detail: {stderr}",
        exit_status(success, code)
    )
}

fn local_timeout_message(secs: u64) -> String {
    format!(
        "cpp_bench local run timed out after {secs}s. Re-run with a narrower --benchmark_filter, \
         fewer repetitions, or raise timeout_sec."
    )
}

/// Parse a Google Benchmark / quick-bench `context` object into [`BenchContext`].
fn parse_context(ctx: Option<&Value>) -> BenchContext {
    BenchContext {
        num_cpus: ctx
            .and_then(|c| c.get("num_cpus"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        mhz_per_cpu: ctx
            .and_then(|c| c.get("mhz_per_cpu"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cpu_scaling_enabled: ctx
            .and_then(|c| c.get("cpu_scaling_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// One local benchmark's summarized result.
struct LocalRow {
    name: String,
    mean: f64,
    /// Per-benchmark noise (stddev / sqrt(reps)) for the verdict; `None` = no
    /// estimate (single repetition or no stddev row).
    noise: Option<f64>,
    /// Raw stddev for display.
    stddev_disp: Option<f64>,
    median: Option<f64>,
    unit: String,
    /// Timed on the wall clock (see [`uses_real_time`]), not on cpu_time.
    real_time: bool,
    reps: u32,
    error: bool,
}

impl LocalRow {
    /// The row for the verdict, in NANOSECONDS whatever its `time_unit`:
    /// raw means once compared 0.6 (ms) against 330.5 (ns) and reported the
    /// ~1,900x SLOWER benchmark as "516.59x faster". Only called for rows
    /// whose unit [`ns_per`] knows.
    fn measurement(&self) -> Measurement {
        let failures = if self.error { self.reps.max(1) } else { 0 };
        let ns = ns_per(&self.unit).unwrap_or(1.0);
        Measurement::from_summary(
            self.mean * ns,
            self.noise.map(|n| n * ns),
            self.reps,
            failures,
        )
    }
}

/// Nanoseconds per Google Benchmark `time_unit`. `None` for a unit we do not
/// know, which is then left out of the comparison rather than compared raw.
fn ns_per(unit: &str) -> Option<f64> {
    match unit {
        "ns" => Some(1.0),
        "us" => Some(1e3),
        "ms" => Some(1e6),
        "s" => Some(1e9),
        _ => None,
    }
}

/// Whether Google Benchmark times this benchmark on the wall clock:
/// `UseRealTime()` / `UseManualTime()` mark the name `/real_time` /
/// `/manual_time`, and for those `cpu_time` is NOT what is measured — a
/// benchmark that sleeps or waits burns almost no CPU (BM_Sleep/real_time was
/// reported at its 8,489 ns of cpu_time; its real time was 276,941 ns).
fn uses_real_time(name: &str) -> bool {
    name.split('/')
        .any(|part| part.starts_with("real_time") || part.starts_with("manual_time"))
}

#[derive(Default)]
struct Accum {
    mean: Option<f64>,
    median: Option<f64>,
    stddev: Option<f64>,
    unit: String,
    iters: Vec<f64>,
    error: bool,
}

/// Parse Google Benchmark JSON into per-benchmark rows, hardened for the shapes
/// gbench actually emits: aggregate rows (mean/median/stddev), iteration-only
/// output (repetitions=1 or aggregates suppressed), missing stddev,
/// per-benchmark `time_unit`, `error_occurred`, and parameterized names.
fn parse_gbench_rows(value: &Value, requested_reps: u32) -> Vec<LocalRow> {
    let benchmarks = match value.get("benchmarks").and_then(Value::as_array) {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut order: Vec<String> = Vec::new();
    let mut accums: BTreeMap<String, Accum> = BTreeMap::new();
    for b in benchmarks {
        let name = b
            .get("run_name")
            .and_then(Value::as_str)
            .or_else(|| b.get("name").and_then(Value::as_str))
            .unwrap_or("?")
            .to_string();
        let entry = accums.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            Accum::default()
        });
        if b.get("error_occurred").and_then(Value::as_bool) == Some(true) {
            entry.error = true;
            continue;
        }
        if let Some(u) = b.get("time_unit").and_then(Value::as_str) {
            entry.unit = u.to_string();
        }
        let clock = if uses_real_time(&name) {
            "real_time"
        } else {
            "cpu_time"
        };
        let time = b.get(clock).and_then(Value::as_f64);
        let run_type = b.get("run_type").and_then(Value::as_str).unwrap_or("");
        if run_type == "aggregate" {
            match b
                .get("aggregate_name")
                .and_then(Value::as_str)
                .unwrap_or("")
            {
                "mean" => entry.mean = time,
                "median" => entry.median = time,
                "stddev" => entry.stddev = time,
                _ => {}
            }
        } else if let Some(t) = time {
            entry.iters.push(t);
        }
    }

    let mut rows = Vec::new();
    for name in order {
        let a = match accums.remove(&name) {
            Some(a) => a,
            None => continue,
        };
        let unit = if a.unit.is_empty() {
            "ns".to_string()
        } else {
            a.unit
        };
        let (mean, noise, stddev_disp, reps) = if let Some(m) = a.mean {
            let reps = if a.iters.is_empty() {
                requested_reps
            } else {
                a.iters.len() as u32
            };
            let noise = a.stddev.map(|s| s / (reps.max(1) as f64).sqrt());
            (m, noise, a.stddev, reps)
        } else if !a.iters.is_empty() {
            let meas = Measurement::from_samples(&a.iters, 0);
            let reps = a.iters.len() as u32;
            let stddev_disp = meas.noise.map(|n| n * (reps.max(1) as f64).sqrt());
            (meas.mean, meas.noise, stddev_disp, reps)
        } else if a.error {
            (0.0, None, None, 0)
        } else {
            continue;
        };
        rows.push(LocalRow {
            real_time: uses_real_time(&name),
            name,
            mean,
            noise,
            stddev_disp,
            median: a.median,
            unit,
            reps,
            error: a.error,
        });
    }
    rows
}

const BENCH_HINT: &str = "increase repetitions or reduce machine load";
/// Relative floor of the local-mode verdict: never call a difference under 1%
/// of the slower benchmark. Google Benchmark repetitions time the code
/// in-process over many iterations, so no process startup rides on them (the
/// whole-process `cpp_perf` floor is 3%); below 1%, alignment and frequency
/// effects still outweigh the code.
const BENCH_FLOOR: f64 = 0.01;
/// The advice when [`BENCH_FLOOR`] set the gate: more repetitions cannot help.
const BENCH_FLOOR_HINT: &str =
    "a difference under 1% is below what a benchmark can attribute to the code";

fn render_local_report(ctx: &BenchContext, rows: &[LocalRow], pinning: &Pinning) -> String {
    let mut out = String::from("Method: cpp_bench local (Google Benchmark, absolute times)\n");
    out.push_str(&render_context(ctx));
    out.push_str(&match pinning {
        Pinning::Cpu(cpu) => format!("pinned to CPU {cpu} via taskset\n"),
        Pinning::None => format!(
            "unpinned: taskset unavailable on {} — expect more variance\n",
            std::env::consts::OS
        ),
    });
    out.push_str(
        "benchmark                              mean         stddev      median     reps\n",
    );
    for r in rows {
        if r.error {
            out.push_str(&format!(
                "{:<36}  (error_occurred — excluded)\n",
                trunc_name(&r.name, 36)
            ));
            continue;
        }
        let stddev = r
            .stddev_disp
            .map(|s| format!("{s:.1} {}", r.unit))
            .unwrap_or_else(|| "-".to_string());
        let median = r
            .median
            .map(|m| format!("{m:.1} {}", r.unit))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "{name:<36}  {mean:>8.1} {unit}  {stddev:>12}  {median:>12}  {reps:>4}\n",
            name = trunc_name(&r.name, 36),
            mean = r.mean,
            unit = r.unit,
            reps = r.reps,
        ));
    }
    if rows.iter().any(|r| r.real_time && !r.error) {
        out.push_str(
            "note: /real_time and /manual_time benchmarks are timed on the wall clock, as they \
             request; the others on cpu_time.\n",
        );
    }
    for r in rows
        .iter()
        .filter(|r| !r.error && ns_per(&r.unit).is_none())
    {
        out.push_str(&format!(
            "note: {} reports an unknown time unit {:?}; it is left out of the comparison.\n",
            r.name, r.unit
        ));
    }

    // Compared in ns (`measurement`), so rows in different units line up.
    let comparable: Vec<&LocalRow> = rows
        .iter()
        .filter(|r| !r.error && r.reps > 0 && ns_per(&r.unit).is_some())
        .collect();
    if comparable.len() >= 2 {
        let fastest_idx = comparable
            .iter()
            .map(|r| r.measurement().mean)
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);
        if let Some(fi) = fastest_idx {
            out.push('\n');
            for (i, other) in comparable.iter().enumerate() {
                if i == fi {
                    continue;
                }
                out.push_str(&bench_verdict(comparable[fi], other));
                out.push('\n');
            }
            out.pop();
        }
    } else {
        out.pop();
        if comparable.len() == 1 && comparable[0].noise.is_none() {
            out.push_str(
                "\nnote: single repetition — no noise estimate; re-run with repetitions>=5 to verify.",
            );
        }
    }
    out
}

/// A noise-gated A/B line naming the two benchmarks.
fn bench_verdict(fastest: &LocalRow, other: &LocalRow) -> String {
    use crate::stats::Verdict;
    match crate::stats::compare(&fastest.measurement(), &other.measurement(), BENCH_FLOOR) {
        Verdict::Faster { ratio, .. } => {
            format!(
                "{} is {ratio:.2}x faster than {} (significant).",
                fastest.name, other.name
            )
        }
        Verdict::WithinNoise {
            delta_pct,
            gate_pct,
            floor_bound,
        } => format!(
            "{} vs {}: {delta_pct:.1}% apart, INSIDE noise (gate {gate_pct:.1}%) — do NOT claim a \
             difference; {}.",
            fastest.name,
            other.name,
            if floor_bound {
                BENCH_FLOOR_HINT
            } else {
                BENCH_HINT
            },
        ),
        Verdict::NoNoiseEstimate => format!(
            "{} vs {}: no noise estimate — difference unverified; {BENCH_HINT}.",
            fastest.name, other.name
        ),
        Verdict::InvalidComparison { .. } => format!(
            "{} vs {}: excluded (a benchmark reported error_occurred).",
            fastest.name, other.name
        ),
    }
}

/// Truncate a benchmark name for the table, appending `…` when cut.
fn trunc_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        name.to_string()
    } else {
        let head: String = name.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// quick-bench compile+run endpoint: `<origin>/quick/`. Origin defaults to
/// quick-bench.com and is overridable via `BYTEASK_QUICKBENCH_URL` for tests.
fn quick_url() -> String {
    let origin = std::env::var("BYTEASK_QUICKBENCH_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| QUICKBENCH_ORIGIN.to_string());
    let origin = origin.trim_end_matches('/');
    format!("{origin}/quick/")
}

/// Build the quick-bench POST body. `code` is the user's snippet sent verbatim
/// (quick-bench supplies the include + BENCHMARK_MAIN()); the compiler options
/// mirror the public API contract.
fn build_request_body(
    code: &str,
    compiler: &str,
    cpp_version: &str,
    optim: &str,
    lib: &str,
    flags: &[String],
) -> Value {
    json!({
        "code": code,
        "options": {
            "compiler": compiler,
            "cppVersion": cpp_version,
            "optim": optim,
            "lib": lib,
            "flags": flags,
        },
        "protocolVersion": 5,
        "force": false,
        "disassemblyOption": "att",
    })
}

/// Cut `text` to at most `cap` bytes, on a character boundary. A plain
/// `String::truncate` PANICS mid-character, and a quick-bench error with a
/// multi-byte character straddling the cap took the engine down with it
/// (for example `日本` at byte 1999 of an HTTP 503 body, `‘` in a compile message).
fn cap_text(mut text: String, cap: usize) -> String {
    text.truncate(text.floor_char_boundary(cap));
    text
}

/// Split the model's `flags` into compiler arguments. Shell-style quotes
/// group a flag that contains spaces: a bare whitespace split sent
/// `-DMSG="a b" -O2` to quick-bench as `-DMSG="a`, `b"`, `-O2`.
fn split_flags(flags: &str) -> Vec<String> {
    super::sanitizers::shell_split(flags)
}

/// Strip ANSI CSI escape sequences and tidy quick-bench's HTML-ish `<br>`/`\r`
/// so a compile-error `message` renders cleanly for the model.
fn clean_message(msg: &str) -> String {
    let mut out = String::new();
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC: drop a `[ ... <letter>` CSI sequence if that's what follows.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out.replace("<br>", "\n").replace('\r', "")
}

/// Subset of quick-bench's `result.context` object we surface.
struct BenchContext {
    num_cpus: u64,
    mhz_per_cpu: u64,
    cpu_scaling_enabled: bool,
}

/// One row of quick-bench's `result.benchmarks` array. quick-bench only returns
/// a name and a `cpu_time` that is normalized to the injected `Noop` loop (1.0).
struct BenchRow {
    name: String,
    cpu_time: f64,
}

/// Name of the synthetic baseline quick-bench injects (cpu_time == 1.0). Kept in
/// the table for context but excluded from the A/B "fastest" comparison.
const NOOP_NAME: &str = "Noop";

/// Parse a quick-bench success payload (`{ "result": { "context", "benchmarks" } }`)
/// into the context + rows we render. Errors when there are no benchmarks (the
/// shape quick-bench returns on a compile error/timeout).
fn parse_result(value: &Value) -> Result<(BenchContext, Vec<BenchRow>), String> {
    let result = value
        .get("result")
        .ok_or_else(|| "missing result object".to_string())?;

    let benchmarks = result
        .get("benchmarks")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing benchmarks array".to_string())?;
    if benchmarks.is_empty() {
        return Err("no benchmarks ran".to_string());
    }

    let ctx_value = result.get("context");
    let ctx = BenchContext {
        num_cpus: ctx_value
            .and_then(|c| c.get("num_cpus"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        mhz_per_cpu: ctx_value
            .and_then(|c| c.get("mhz_per_cpu"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cpu_scaling_enabled: ctx_value
            .and_then(|c| c.get("cpu_scaling_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };

    let mut rows = Vec::new();
    for b in benchmarks {
        rows.push(BenchRow {
            name: b
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            cpu_time: b.get("cpu_time").and_then(Value::as_f64).unwrap_or(0.0),
        });
    }

    Ok((ctx, rows))
}

/// Format a relative cpu_time. Tiny values (a case the optimizer eliminated)
/// use scientific notation so they don't collapse to 0.0000.
fn fmt_cpu(x: f64) -> String {
    if x != 0.0 && x.abs() < 0.001 {
        format!("{x:.3e}")
    } else {
        format!("{x:.4}")
    }
}

/// Compute the A/B relative-speed line over the real (non-Noop) cases: pick the
/// fastest by cpu_time and express every other case as a multiple slower.
/// Returns `None` for fewer than two real cases.
fn relative_note(rows: &[BenchRow]) -> Option<String> {
    let cases: Vec<&BenchRow> = rows.iter().filter(|r| r.name != NOOP_NAME).collect();
    if cases.len() < 2 {
        return None;
    }
    let (fastest_idx, fastest) = cases.iter().enumerate().min_by(|a, b| {
        a.1.cpu_time
            .partial_cmp(&b.1.cpu_time)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;

    let base = fastest.cpu_time;
    let mut parts = Vec::new();
    for (idx, row) in cases.iter().enumerate() {
        if idx == fastest_idx {
            continue;
        }
        let ratio = if base > 0.0 { row.cpu_time / base } else { 0.0 };
        parts.push(format!("{} is {ratio:.2}x slower", row.name));
    }

    Some(format!(
        "fastest: {} ({}); {}",
        fastest.name,
        fmt_cpu(fastest.cpu_time),
        parts.join("; ")
    ))
}

/// Render the model-facing report: a brief CPU context line, the relative
/// cpu_time table (Noop baseline kept for reference), the A/B note (if 2+ real
/// cases), and the indicative caveat.
/// The shared CPU-context line (reused by remote and local modes).
fn render_context(ctx: &BenchContext) -> String {
    format!(
        "cpu: {} core(s) @ ~{} MHz, scaling {}\n",
        ctx.num_cpus,
        ctx.mhz_per_cpu,
        if ctx.cpu_scaling_enabled {
            "ON (noisy!)"
        } else {
            "off"
        }
    )
}

fn render_report(ctx: &BenchContext, rows: &[BenchRow]) -> String {
    let mut out = String::new();
    out.push_str(&render_context(ctx));

    if rows.is_empty() {
        out.push_str("no benchmarks ran.\n");
        out.push_str(CAVEAT);
        return out;
    }

    out.push_str("benchmark | cpu_time (relative to Noop = 1.0)\n");
    for row in rows {
        out.push_str(&format!("{} | {}\n", row.name, fmt_cpu(row.cpu_time)));
    }

    if let Some(note) = relative_note(rows) {
        out.push_str(&note);
        out.push('\n');
    }

    out.push_str(CAVEAT);
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn error_text_is_capped_on_a_character_boundary() {
        // The regression: a 3-byte character straddling the cap panicked.
        let body = format!("{}日本", "c".repeat(ERROR_BODY_CAP - 1));
        assert_eq!(
            cap_text(body, ERROR_BODY_CAP),
            "c".repeat(ERROR_BODY_CAP - 1)
        );
        // Short text is untouched; a cut on a boundary is exact.
        assert_eq!(cap_text("héllo".to_string(), 100), "héllo");
        assert_eq!(cap_text("héllo".to_string(), 3), "hé");
    }

    #[test]
    fn split_flags_keeps_a_quoted_flag_whole() {
        // The regression: `-DMSG="a b" -O2` reached quick-bench
        // as ["-DMSG=\"a", "b\"", "-O2"].
        assert_eq!(split_flags(r#"-DMSG="a b" -O2"#), vec!["-DMSG=a b", "-O2"]);
        assert_eq!(
            split_flags("-DNAME='x y' -march=native"),
            vec!["-DNAME=x y", "-march=native"]
        );
    }

    #[test]
    fn split_flags_tokenizes_and_handles_empty() {
        assert_eq!(
            split_flags("-march=native -mavx2"),
            vec!["-march=native", "-mavx2"]
        );
        assert!(split_flags("").is_empty());
        assert!(split_flags("   ").is_empty());
    }

    #[test]
    fn quick_url_defaults_to_public_endpoint() {
        // SAFETY: single-threaded test; clear the override first.
        unsafe { std::env::remove_var("BYTEASK_QUICKBENCH_URL") };
        assert_eq!(quick_url(), "https://quick-bench.com/quick/");
    }

    #[test]
    fn quick_url_honors_env_override_and_trims_slash() {
        // SAFETY: single-threaded test; set + restore the override.
        unsafe { std::env::set_var("BYTEASK_QUICKBENCH_URL", "http://localhost:9000/") };
        assert_eq!(quick_url(), "http://localhost:9000/quick/");
        unsafe { std::env::remove_var("BYTEASK_QUICKBENCH_URL") };
    }

    #[test]
    fn build_request_body_passes_code_verbatim_without_include() {
        // A bare snippet must be sent AS-IS: no <benchmark/benchmark.h> prepend,
        // no BENCHMARK_MAIN() append — quick-bench wraps it server-side.
        let snippet =
            "static void BM_X(benchmark::State& s){ for (auto _ : s){} }\nBENCHMARK(BM_X);";
        let flags = split_flags("-march=native");
        let body = build_request_body(snippet, "clang-17.0", "c++20", "3", "gnu", &flags);

        assert_eq!(body["code"], snippet);
        let code = body["code"].as_str().unwrap();
        assert!(!code.contains("benchmark/benchmark.h"));
        assert!(!code.contains("BENCHMARK_MAIN()"));
    }

    #[test]
    fn build_request_body_shapes_quickbench_options() {
        let flags = split_flags("-O2 -Wall");
        let body = build_request_body("BENCHMARK(BM_X);", "gcc-13.1", "c++23", "2", "llvm", &flags);

        assert_eq!(body["options"]["compiler"], "gcc-13.1");
        assert_eq!(body["options"]["cppVersion"], "c++23");
        assert_eq!(body["options"]["optim"], "2");
        assert_eq!(body["options"]["lib"], "llvm");
        assert_eq!(body["options"]["flags"], json!(["-O2", "-Wall"]));
        assert_eq!(body["protocolVersion"], json!(5));
        assert_eq!(body["force"], json!(false));
        assert_eq!(body["disassemblyOption"], "att");
    }

    #[test]
    fn build_request_body_empty_flags_is_empty_array() {
        let body = build_request_body("BENCHMARK(BM_X);", "clang-17.0", "c++20", "3", "gnu", &[]);
        assert_eq!(body["options"]["flags"], json!([]));
    }

    /// Real quick-bench success payload (captured via curl). Note: benchmarks
    /// carry only `name` + `cpu_time` (normalized to the injected `Noop` = 1.0),
    /// and cpu_time can be in scientific notation for eliminated cases.
    const SAMPLE_RESPONSE: &str = r#"{
  "annotation": "----------- BM_StringCreation\n",
  "disassemblyOption": "att",
  "id": "5eiz7u4qst_4oIB_q-5JQ0CYy4k",
  "message": "",
  "result": {
    "context": {
      "date": "2026-06-30T08:19:43+00:00",
      "host_name": "53f5554cd63e",
      "executable": "./bench",
      "num_cpus": 1,
      "mhz_per_cpu": 2300,
      "cpu_scaling_enabled": false,
      "library_build_type": "release"
    },
    "benchmarks": [
      { "name": "BM_StringCreation", "cpu_time": 2.5391948965259116e-06 },
      { "name": "BM_StringCopy", "cpu_time": 12.58405367862923 },
      { "name": "Noop", "cpu_time": 1 }
    ]
  }
}"#;

    /// Real quick-bench compile-error payload: HTTP 200 but `result` is `{}` and
    /// the diagnostics live in an ANSI-coded `message`.
    const ERROR_RESPONSE: &str = "{\
        \"message\": \"\\u001b[0;1;31mError or timeout\\u001b[0m<br>bench-file.cpp:2:66: \
        \\u001b[0;1;31merror: \\u001b[0muse of undeclared identifier 'foo'\\r\\n<br>\", \
        \"result\": {}\
    }";

    #[test]
    fn parses_real_quickbench_response() {
        let value: Value = serde_json::from_str(SAMPLE_RESPONSE).expect("json");
        let (ctx, rows) = parse_result(&value).expect("parse");
        assert_eq!(ctx.num_cpus, 1);
        assert_eq!(ctx.mhz_per_cpu, 2300);
        assert!(!ctx.cpu_scaling_enabled);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "BM_StringCreation");
        assert_eq!(rows[0].cpu_time, 2.5391948965259116e-06);
        assert_eq!(rows[1].name, "BM_StringCopy");
        assert_eq!(rows[2].name, "Noop");
        assert_eq!(rows[2].cpu_time, 1.0);
    }

    #[test]
    fn renders_table_context_and_relative_note() {
        let value: Value = serde_json::from_str(SAMPLE_RESPONSE).expect("json");
        let (ctx, rows) = parse_result(&value).expect("parse");
        let out = render_report(&ctx, &rows);

        assert!(out.starts_with("cpu: 1 core(s) @ ~2300 MHz, scaling off\n"));
        assert!(out.contains("benchmark | cpu_time (relative to Noop = 1.0)\n"));
        assert!(out.contains("BM_StringCopy | 12.5841\n"));
        assert!(out.contains("Noop | 1.0000\n"));
        // Tiny eliminated case rendered in scientific notation, not 0.0000.
        assert!(out.contains("BM_StringCreation | 2.539e-6\n"));
        // Fastest real case is BM_StringCreation; Noop is excluded from A/B.
        assert!(out.contains("fastest: BM_StringCreation"));
        assert!(out.contains("BM_StringCopy is "));
        assert!(out.contains("x slower"));
        assert!(!out.contains("Noop is "));
        assert!(out.trim_end().ends_with("re-run to confirm."));
    }

    #[test]
    fn parse_result_errors_on_empty_result() {
        let value: Value = serde_json::from_str(ERROR_RESPONSE).expect("json");
        assert!(parse_result(&value).is_err());
    }

    #[test]
    fn parse_result_errors_on_empty_benchmarks_array() {
        let value = json!({ "result": { "benchmarks": [] } });
        assert!(parse_result(&value).is_err());
    }

    #[test]
    fn parse_result_errors_on_missing_result() {
        let value = json!({ "message": "boom" });
        assert!(parse_result(&value).is_err());
    }

    #[test]
    fn clean_message_strips_ansi_and_html_breaks() {
        let value: Value = serde_json::from_str(ERROR_RESPONSE).expect("json");
        let raw = value["message"].as_str().unwrap();
        let cleaned = clean_message(raw);
        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains("<br>"));
        assert!(!cleaned.contains('\r'));
        assert!(cleaned.contains("Error or timeout"));
        assert!(cleaned.contains("use of undeclared identifier 'foo'"));
    }

    #[test]
    fn relative_note_none_for_single_real_case() {
        // Only one real benchmark (plus the Noop baseline) -> no A/B note.
        let rows = vec![
            BenchRow {
                name: "BM_Only".to_string(),
                cpu_time: 5.0,
            },
            BenchRow {
                name: NOOP_NAME.to_string(),
                cpu_time: 1.0,
            },
        ];
        assert!(relative_note(&rows).is_none());
    }

    #[test]
    fn relative_note_picks_fastest_and_ignores_noop() {
        let rows = vec![
            BenchRow {
                name: "Slow".to_string(),
                cpu_time: 40.0,
            },
            BenchRow {
                name: "Fast".to_string(),
                cpu_time: 10.0,
            },
            BenchRow {
                name: NOOP_NAME.to_string(),
                cpu_time: 1.0,
            },
        ];
        let note = relative_note(&rows).expect("note");
        assert_eq!(note, "fastest: Fast (10.0000); Slow is 4.00x slower");
    }

    #[test]
    fn renders_scaling_warning_when_enabled() {
        let ctx = BenchContext {
            num_cpus: 4,
            mhz_per_cpu: 3000,
            cpu_scaling_enabled: true,
        };
        let out = render_report(&ctx, &[]);
        assert!(out.contains("scaling ON (noisy!)"));
        assert!(out.contains("no benchmarks ran."));
    }

    #[test]
    fn fmt_cpu_uses_scientific_for_tiny_values() {
        assert_eq!(fmt_cpu(12.5678), "12.5678");
        assert_eq!(fmt_cpu(1.0), "1.0000");
        assert_eq!(fmt_cpu(0.0), "0.0000");
        assert_eq!(fmt_cpu(2.5391948965259116e-06), "2.539e-6");
    }

    // ---------------- local mode ----------------

    const GBENCH_AGGREGATES: &str = r#"{
      "context": { "num_cpus": 8, "mhz_per_cpu": 3000, "cpu_scaling_enabled": false },
      "benchmarks": [
        {"name":"BM_Fast","run_name":"BM_Fast","run_type":"iteration","cpu_time":10.0,"time_unit":"ns"},
        {"name":"BM_Fast","run_name":"BM_Fast","run_type":"iteration","cpu_time":10.4,"time_unit":"ns"},
        {"name":"BM_Fast_mean","run_name":"BM_Fast","run_type":"aggregate","aggregate_name":"mean","cpu_time":10.2,"time_unit":"ns"},
        {"name":"BM_Fast_median","run_name":"BM_Fast","run_type":"aggregate","aggregate_name":"median","cpu_time":10.1,"time_unit":"ns"},
        {"name":"BM_Fast_stddev","run_name":"BM_Fast","run_type":"aggregate","aggregate_name":"stddev","cpu_time":0.3,"time_unit":"ns"},
        {"name":"BM_Slow_mean","run_name":"BM_Slow","run_type":"aggregate","aggregate_name":"mean","cpu_time":40.5,"time_unit":"ns"},
        {"name":"BM_Slow_stddev","run_name":"BM_Slow","run_type":"aggregate","aggregate_name":"stddev","cpu_time":1.1,"time_unit":"ns"}
      ]
    }"#;

    #[test]
    fn parses_gbench_aggregates() {
        let v: Value = serde_json::from_str(GBENCH_AGGREGATES).expect("json");
        let rows = parse_gbench_rows(&v, 9);
        assert_eq!(rows.len(), 2);
        let fast = &rows[0];
        assert_eq!(fast.name, "BM_Fast");
        assert!((fast.mean - 10.2).abs() < 1e-9);
        assert_eq!(fast.stddev_disp, Some(0.3));
        assert_eq!(fast.median, Some(10.1));
        assert_eq!(fast.reps, 2); // from the two iteration rows
        assert!(fast.noise.is_some());
        // BM_Slow had no iteration rows → reps falls back to the requested 9.
        assert_eq!(rows[1].reps, 9);
    }

    #[test]
    fn parses_gbench_context() {
        let v: Value = serde_json::from_str(GBENCH_AGGREGATES).expect("json");
        let ctx = parse_context(v.get("context"));
        assert_eq!(ctx.num_cpus, 8);
        assert_eq!(ctx.mhz_per_cpu, 3000);
        assert!(!ctx.cpu_scaling_enabled);
    }

    #[test]
    fn render_local_names_winner_and_notes_pinning() {
        let v: Value = serde_json::from_str(GBENCH_AGGREGATES).expect("json");
        let ctx = parse_context(v.get("context"));
        let rows = parse_gbench_rows(&v, 9);
        let out = render_local_report(&ctx, &rows, &Pinning::Cpu(7));
        assert!(
            out.contains("BM_Fast is 3.97x faster than BM_Slow (significant)."),
            "{out}"
        );
        assert!(out.contains("pinned to CPU 7 via taskset"));
        assert!(!out.contains("[simulated]")); // real hardware, no simulated tag
        assert!(out.contains("cpu: 8 core(s)"));
    }

    #[test]
    fn render_local_unpinned_note_when_no_taskset() {
        let v: Value = serde_json::from_str(GBENCH_AGGREGATES).expect("json");
        let ctx = parse_context(v.get("context"));
        let rows = parse_gbench_rows(&v, 9);
        let out = render_local_report(&ctx, &rows, &Pinning::None);
        assert!(out.contains("unpinned: taskset unavailable"));
    }

    #[test]
    fn iteration_only_output_has_no_noise_estimate() {
        // repetitions=1 → only an iteration row, no aggregates.
        let json = r#"{"context":{},"benchmarks":[
            {"name":"BM_X","run_name":"BM_X","run_type":"iteration","cpu_time":12.0,"time_unit":"ns"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reps, 1);
        assert!(rows[0].noise.is_none());
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(
            out.contains("single repetition — no noise estimate"),
            "{out}"
        );
    }

    #[test]
    fn missing_stddev_yields_unverified_verdict() {
        // Two means but no stddev rows → no noise estimate → verdict cannot claim.
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"A","run_type":"aggregate","aggregate_name":"mean","cpu_time":10.0,"time_unit":"ns"},
            {"run_name":"B","run_type":"aggregate","aggregate_name":"mean","cpu_time":40.0,"time_unit":"ns"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 5);
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(out.contains("no noise estimate"), "{out}");
        assert!(!out.contains("faster than"));
    }

    #[test]
    fn error_occurred_benchmark_excluded() {
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"OK","run_type":"aggregate","aggregate_name":"mean","cpu_time":10.0,"time_unit":"ns"},
            {"run_name":"OK","run_type":"aggregate","aggregate_name":"stddev","cpu_time":0.1,"time_unit":"ns"},
            {"run_name":"BadBench","run_type":"iteration","error_occurred":true,"error_message":"boom"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 5);
        let bad = rows
            .iter()
            .find(|r| r.name == "BadBench")
            .expect("bad row present");
        assert!(bad.error);
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(out.contains("error_occurred — excluded"), "{out}");
    }

    #[test]
    fn benchmarks_in_different_units_are_compared_in_ns() {
        // The regression (seen with a real binary): BM_Big_ms reports
        // in ms (0.6), BM_Small_ns in ns (330.5). Comparing the raw numbers
        // called the ~1,900x SLOWER benchmark "516.59x faster".
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"BM_Small_ns","run_type":"aggregate","aggregate_name":"mean","cpu_time":330.5,"real_time":331.0,"time_unit":"ns"},
            {"run_name":"BM_Small_ns","run_type":"aggregate","aggregate_name":"stddev","cpu_time":4.1,"real_time":4.0,"time_unit":"ns"},
            {"run_name":"BM_Big_ms","run_type":"aggregate","aggregate_name":"mean","cpu_time":0.628,"real_time":0.63,"time_unit":"ms"},
            {"run_name":"BM_Big_ms","run_type":"aggregate","aggregate_name":"stddev","cpu_time":0.004,"real_time":0.004,"time_unit":"ms"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 5);
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(
            out.contains("BM_Small_ns is 1900.15x faster than BM_Big_ms (significant)."),
            "{out}"
        );
        // Each row is still shown in its own unit.
        assert!(out.contains("0.6 ms"), "{out}");
    }

    #[test]
    fn an_unknown_time_unit_is_left_out_not_compared_raw() {
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"A","run_type":"aggregate","aggregate_name":"mean","cpu_time":10.0,"time_unit":"ns"},
            {"run_name":"A","run_type":"aggregate","aggregate_name":"stddev","cpu_time":0.1,"time_unit":"ns"},
            {"run_name":"B","run_type":"aggregate","aggregate_name":"mean","cpu_time":2.0,"time_unit":"fortnights"},
            {"run_name":"B","run_type":"aggregate","aggregate_name":"stddev","cpu_time":0.1,"time_unit":"fortnights"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 5);
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(
            out.contains("B reports an unknown time unit \"fortnights\""),
            "{out}"
        );
        assert!(!out.contains("faster than"), "{out}");
    }

    #[test]
    fn real_time_benchmarks_are_reported_on_the_wall_clock() {
        // The regression: BM_Sleep->UseRealTime() sleeps 200 us
        // per iteration; its cpu_time is ~8.5 us and was what the table showed.
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"BM_Sleep/real_time","run_type":"iteration","cpu_time":8400.0,"real_time":276000.0,"time_unit":"ns"},
            {"run_name":"BM_Sleep/real_time","run_type":"iteration","cpu_time":8600.0,"real_time":277882.0,"time_unit":"ns"},
            {"run_name":"BM_Sleep/real_time","run_type":"aggregate","aggregate_name":"mean","cpu_time":8489.3,"real_time":276941.0,"time_unit":"ns"},
            {"run_name":"BM_Sleep/real_time","run_type":"aggregate","aggregate_name":"stddev","cpu_time":273.2,"real_time":1330.8,"time_unit":"ns"},
            {"run_name":"BM_Manual/manual_time","run_type":"aggregate","aggregate_name":"mean","cpu_time":5.0,"real_time":900.0,"time_unit":"ns"},
            {"run_name":"BM_Cpu","run_type":"aggregate","aggregate_name":"mean","cpu_time":40.0,"real_time":41.0,"time_unit":"ns"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 5);
        assert_eq!(rows[0].mean, 276941.0);
        assert_eq!(rows[0].stddev_disp, Some(1330.8));
        assert_eq!(
            rows[1].mean, 900.0,
            "UseManualTime reports in real_time too"
        );
        assert_eq!(
            rows[2].mean, 40.0,
            "an ordinary benchmark stays on cpu_time"
        );
        let out = render_local_report(&parse_context(v.get("context")), &rows, &Pinning::None);
        assert!(out.contains("276941.0 ns"), "{out}");
        assert!(out.contains("timed on the wall clock"), "{out}");
    }

    #[test]
    fn parameterized_names_kept_verbatim() {
        let json = r#"{"context":{},"benchmarks":[
            {"run_name":"BM_X/64","run_type":"aggregate","aggregate_name":"mean","cpu_time":5.0,"time_unit":"ns"}
        ]}"#;
        let v: Value = serde_json::from_str(json).expect("json");
        let rows = parse_gbench_rows(&v, 1);
        assert_eq!(rows[0].name, "BM_X/64");
    }

    #[test]
    fn build_local_argv_pinned_and_unpinned() {
        let out = std::path::Path::new("/tmp/out.json");
        let (prog, argv) = build_local_argv("/bin/b", Some("BM_.*"), 9, out, &Pinning::Cpu(3));
        assert_eq!(prog, "taskset");
        assert_eq!(argv[0], "-c");
        assert_eq!(argv[1], "3");
        assert_eq!(argv[2], "/bin/b");
        assert!(argv.contains(&"--benchmark_filter=BM_.*".to_string()));
        assert!(argv.contains(&"--benchmark_repetitions=9".to_string()));
        // JSON to a file of ours, never stdout (the program owns stdout).
        assert!(argv.contains(&"--benchmark_out=/tmp/out.json".to_string()));
        assert!(argv.contains(&"--benchmark_out_format=json".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("--benchmark_format")));

        let (prog, argv) = build_local_argv("/bin/b", None, 5, out, &Pinning::None);
        assert_eq!(prog, "/bin/b");
        assert!(!argv.iter().any(|a| a.starts_with("--benchmark_filter")));
        assert!(argv.contains(&"--benchmark_repetitions=5".to_string()));
    }

    #[test]
    fn resolve_binary_relative_and_absolute() {
        let cwd = std::path::Path::new("/work");
        assert_eq!(
            resolve_binary(cwd, "bench"),
            std::path::PathBuf::from("/work/bench")
        );
        assert_eq!(
            resolve_binary(cwd, "/abs/bench"),
            std::path::PathBuf::from("/abs/bench")
        );
    }

    #[test]
    fn trunc_name_appends_ellipsis() {
        assert_eq!(trunc_name("short", 10), "short");
        assert_eq!(trunc_name("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn local_timeout_message_is_actionable() {
        assert!(local_timeout_message(300).contains("timeout_sec"));
    }

    #[test]
    fn select_mode_enforces_exactly_one_of() {
        assert_eq!(select_mode(Some("/b"), None), BenchMode::Local("/b"));
        assert_eq!(select_mode(None, Some("src")), BenchMode::Remote("src"));
        assert_eq!(select_mode(Some("/b"), Some("src")), BenchMode::Both);
        assert_eq!(select_mode(None, None), BenchMode::Neither);
        assert!(select_mode_error().contains("not both, and not neither"));
    }

    #[test]
    fn a_results_file_that_cannot_be_created_does_not_end_the_turn() {
        // `Fatal` ends the user's whole turn; an unwritable /tmp must not.
        let err = json_out_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let FunctionCallError::RespondToModel(msg) = err else {
            panic!("must go back to the model, not end the turn: {err:?}");
        };
        assert!(msg.contains("NOT run"), "{msg}");
    }

    #[tokio::test]
    async fn unknown_argument_names_are_refused() {
        // Refused at the call boundary, before anything runs.
        let tool = BenchTool {
            native_exec: true,
            cwd: std::env::temp_dir(),
        };
        let arguments = json!({"binary": "bench", "benchmark_filter": "BM_Fast$", "reps": 2});
        let payload = codex_extension_api::ToolPayload::Function {
            arguments: arguments.to_string(),
        };
        let call = ToolCall {
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            tool_name: ToolName::plain(TOOL_NAME),
            model: "test".to_string(),
            codex_turn_metadata: None,
            truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
            source: codex_extension_api::ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: std::sync::Arc::new(codex_extension_api::NoopTurnItemEmitter),
            environments: Vec::new(),
            payload,
        };
        let err = match tool.handle(call).await {
            Ok(_) => panic!("an unknown field must be refused"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("unknown field `benchmark_filter`"), "{err}");
        assert!(
            err.contains("`filter`"),
            "the error should list the real names: {err}"
        );
    }

    fn local_args(binary: &str) -> BenchArgs {
        BenchArgs {
            source: None,
            std: None,
            optim: None,
            compiler: None,
            lib: None,
            flags: None,
            binary: Some(binary.to_string()),
            filter: None,
            repetitions: Some(2),
            timeout_sec: Some(120),
        }
    }

    /// The model-facing text of a local run (Ok) or its error (Err).
    async fn run_local_text(tool: &BenchTool, binary: &str) -> Result<String, String> {
        match tool.run_local(&local_args(binary), binary).await {
            Ok(out) => {
                let payload = codex_extension_api::ToolPayload::Function {
                    arguments: "{}".to_string(),
                };
                Ok(serde_json::to_string(&out.to_response_item("call", &payload)).expect("json"))
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Compile a Google Benchmark program into `dir`; `None` without a C++
    /// compiler or libbenchmark (the caller skips).
    fn build_gbench(dir: &std::path::Path, name: &str, source: &str) -> Option<std::path::PathBuf> {
        let src = dir.join(format!("{name}.cpp"));
        std::fs::write(&src, source).ok()?;
        let bin = dir.join(name);
        let built = std::process::Command::new("c++")
            .args(["-O1", "-std=c++17"])
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .args(["-lbenchmark", "-lpthread"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        built.then_some(bin)
    }

    #[tokio::test]
    async fn a_program_that_is_not_a_benchmark_runs_once_and_is_named() {
        // The regression: a --benchmark_list_tests probe that
        // only checked the exit status let a plain program through, so it ran
        // twice (side effects and all) and then "did not return JSON".
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(bin) = crate::program_args::fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let tool = BenchTool {
            native_exec: true,
            cwd: dir.path().to_path_buf(),
        };
        let err = run_local_text(&tool, &bin.to_string_lossy())
            .await
            .expect_err("a plain program is not a benchmark");
        assert!(err.contains("wrote no Google Benchmark JSON"), "{err}");
        let runs = crate::program_args::fixture::runs(dir.path());
        assert_eq!(runs.len(), 1, "the program must run exactly once: {runs:?}");
    }

    #[tokio::test]
    async fn a_benchmark_that_prints_to_stdout_still_reports() {
        // The regression: the JSON was read from stdout, so a
        // benchmark that printed "loading fixture data..." failed to parse.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = "#include <benchmark/benchmark.h>\n#include <cstdio>\n\
            static int setup = [](){ std::printf(\"loading fixture data...\\n\"); return 1; }();\n\
            static void BM_A(benchmark::State& s){ for (auto _ : s) benchmark::DoNotOptimize(setup + 1); }\n\
            BENCHMARK(BM_A)->Iterations(1000);\n\
            BENCHMARK_MAIN();\n";
        let Some(bin) = build_gbench(dir.path(), "prints", source) else {
            eprintln!("skip: could not build a Google Benchmark binary");
            return;
        };
        let tool = BenchTool {
            native_exec: true,
            cwd: dir.path().to_path_buf(),
        };
        let out = run_local_text(&tool, &bin.to_string_lossy())
            .await
            .expect("reported");
        assert!(out.contains("BM_A"), "{out}");
        assert!(out.contains("Google Benchmark, absolute times"), "{out}");
    }

    #[tokio::test]
    async fn a_filter_that_matches_nothing_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = "#include <benchmark/benchmark.h>\n\
            static void BM_A(benchmark::State& s){ for (auto _ : s) benchmark::DoNotOptimize(1); }\n\
            BENCHMARK(BM_A)->Iterations(1000);\n\
            BENCHMARK_MAIN();\n";
        let Some(bin) = build_gbench(dir.path(), "one", source) else {
            eprintln!("skip: could not build a Google Benchmark binary");
            return;
        };
        let tool = BenchTool {
            native_exec: true,
            cwd: dir.path().to_path_buf(),
        };
        let bin = bin.to_string_lossy().to_string();
        let mut args = local_args(&bin);
        args.filter = Some("NoSuchBench".to_string());
        let err = match tool.run_local(&args, &bin).await {
            Ok(_) => panic!("nothing matched, nothing to report"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("matches filter \"NoSuchBench\""), "{err}");
    }

    #[tokio::test]
    #[ignore = "needs a C++ compiler + libbenchmark; run with --include-ignored"]
    async fn live_local_bench_runs_and_compares() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("b.cpp");
        std::fs::write(
            &src,
            "#include <benchmark/benchmark.h>\n\
             static void BM_Fast(benchmark::State& s){ for(auto _ : s){ \
             benchmark::DoNotOptimize(1+1);} }\n\
             BENCHMARK(BM_Fast);\n\
             static void BM_Slow(benchmark::State& s){ for(auto _ : s){ long x=0; \
             for(int i=0;i<2000;i++) benchmark::DoNotOptimize(x+=i);} }\n\
             BENCHMARK(BM_Slow);\n\
             BENCHMARK_MAIN();\n",
        )
        .expect("write");
        let bin = dir.path().join("b");
        let mut built = false;
        for cc in ["c++", "g++", "clang++"] {
            let ok = tokio::process::Command::new(cc)
                .args(["-O2", "-std=c++17"])
                .arg(&src)
                .arg("-o")
                .arg(&bin)
                .args(["-lbenchmark", "-lpthread"])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                built = true;
                break;
            }
        }
        if !built {
            eprintln!("skip: could not build a Google Benchmark binary");
            return;
        }
        let tool = BenchTool {
            native_exec: true,
            cwd: dir.path().to_path_buf(),
        };
        let args = BenchArgs {
            source: None,
            std: None,
            optim: None,
            compiler: None,
            lib: None,
            flags: None,
            binary: Some("./b".to_string()),
            filter: None,
            repetitions: Some(5),
            timeout_sec: Some(120),
        };
        let out = tool.run_local(&args, "./b").await.expect("local run");
        let item = out.to_response_item(
            "call-1",
            &codex_extension_api::ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        let text = serde_json::to_string(&item).expect("serialize");
        // Absolute-time table + a real, above-noise winner (BM_Fast ≫ BM_Slow).
        assert!(text.contains("Google Benchmark, absolute times"), "{text}");
        assert!(
            text.contains("BM_Fast is") && text.contains("faster than BM_Slow"),
            "{text}"
        );
    }
}
