//! `port_check`: the whole check, from a JSON request to a result in the shared format.
//!
//! Order of work, and why:
//! 1. Read and verify the recording end to end before anything runs (every line parses, time
//!    never goes back, every cancel/exec/delete/replace names a resting order). A recording
//!    error found halfway through would stop both versions at the same place and make a partial
//!    check look complete.
//! 2. Run the Python version at least twice, with different `PYTHONHASHSEED`s. Its transcripts
//!    must be identical, field for field, with no tolerance. If they differ, nothing about the
//!    port can be claimed: the result is NOT RUN (not reproducible) with the first difference.
//! 3. Run the C++ version (twice by default; the second run with `MALLOC_PERTURB_` so a read of
//!    uninitialised heap memory changes behaviour). Compare it with Python under the port rules:
//!    actions (orders, cancels, timers), decisions, every harness event, the final positions and
//!    cash are exact; only `diag` numbers may use the requested tolerance. Then the C++ runs
//!    against each other, exactly.
//! 4. PASS only when all of that holds and the strategy actually traded; if neither version sent
//!    an order the result is NO CLAIM, because the check then says nothing about order logic.

use byteask_replay::Comparison;
use byteask_replay::Rules;
use byteask_replay::RulesSpec;
use byteask_replay::compare_streams;
use byteask_replay::record::JsonLines;
use byteask_replay::record::sha256_file;
use byteask_replay::report;
use byteask_replay::rules::NumberRuleSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::book::Book;
use crate::model::MdKind;
use crate::model::MdRecord;
use crate::model::parse_md;
use crate::protocol::PROTOCOL_VERSION;
use crate::session::ProcessSpec;
use crate::session::RunEnd;
use crate::session::RunLimits;
use crate::session::Setup;
use crate::session::VersionRun;
use crate::session::run_version;
use crate::sim::HarnessConfig;
use crate::sim::RecordStream;
use crate::sim::StepLimits;
use crate::sim::apply_record;

pub const HARNESS_VERSION: &str = "port-check harness 1 (2026-10-18)";

/// The shared result vocabulary. Anything but PASS is never success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "FAIL")]
    Fail,
    #[serde(rename = "NOT RUN")]
    NotRun,
    #[serde(rename = "NO CLAIM")]
    NoClaim,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    /// Normalized market events, one JSON object per line (see `model.rs`).
    #[default]
    EventsJsonl,
    /// A Nasdaq TotalView-ITCH 5.0 capture (2-byte big-endian length before every message),
    /// decoded by the HFT pack.
    Itch50,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiagTolerance {
    #[serde(default)]
    pub abs: f64,
    #[serde(default)]
    pub rel: f64,
    #[serde(default)]
    pub ulps: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LimitsSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_response_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps_per_ts: Option<u64>,
}

impl LimitsSpec {
    fn compile(&self) -> RunLimits {
        let d = RunLimits::default();
        RunLimits {
            first_response: self
                .first_response_timeout_ms
                .map_or(d.first_response, Duration::from_millis),
            response: self
                .response_timeout_ms
                .map_or(d.response, Duration::from_millis),
            run: self.run_timeout_ms.map_or(d.run, Duration::from_millis),
            steps: StepLimits {
                max_steps: self.max_steps.unwrap_or(d.steps.max_steps),
                max_steps_per_ts: self.max_steps_per_ts.unwrap_or(d.steps.max_steps_per_ts),
            },
            ..d
        }
    }
}

fn default_python_runs() -> u32 {
    2
}

fn default_cpp_runs() -> u32 {
    2
}

/// The JSON request (the tool's input).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PortCheckRequest {
    pub recording: PathBuf,
    #[serde(default)]
    pub recording_format: RecordingFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_book: Option<PathBuf>,
    pub python: ProcessSpec,
    pub cpp: ProcessSpec,
    #[serde(default)]
    pub harness: HarnessConfig,
    #[serde(default)]
    pub diag_tolerance: DiagTolerance,
    #[serde(default)]
    pub limits: LimitsSpec,
    #[serde(default = "default_python_runs")]
    pub python_runs: u32,
    #[serde(default = "default_cpp_runs")]
    pub cpp_runs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
}

/// The result (the tool's output).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PortCheckResult {
    pub tool: &'static str,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One plain sentence, safe to show as is.
    pub sentence: String,
    pub evidence: Value,
    /// Which comparison found it, the step, the field, both values and both rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_difference: Option<Value>,
    /// Speed and allocation results (from the perf lab once it exists).
    pub perf: Value,
}

const PYTHON_HASH_SEEDS: [&str; 5] = ["0", "1234567", "42", "987654321", "7"];
const MALLOC_PERTURB: [&str; 5] = ["", "165", "90", "33", "201"];

/// Facts about the recording, established before any run.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingInfo {
    pub sha256: String,
    pub records: u64,
    pub symbols: Vec<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

/// The recording as market records, whatever its format.
pub fn open_events(
    path: &Path,
    format: RecordingFormat,
) -> Result<Box<RecordStream<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let values: Box<dyn Iterator<Item = Result<Value, String>>> = match format {
        RecordingFormat::EventsJsonl => {
            Box::new(JsonLines::new(BufReader::new(file)).map(|r| r.map_err(|e| e.to_string())))
        }
        RecordingFormat::Itch50 => Box::new(byteask_hft_pack::read_events(BufReader::new(file))),
    };
    let mut index = 0u64;
    Ok(Box::new(values.map(move |r| {
        let i = index;
        index += 1;
        r.and_then(|v| parse_md(&v).map_err(|e| format!("record {i}: {e}")))
    })))
}

fn read_initial_book(path: &Path) -> Result<Vec<MdRecord>, String> {
    let mut out = Vec::new();
    for (i, r) in open_events(path, RecordingFormat::EventsJsonl)?.enumerate() {
        let r = r.map_err(|e| format!("initial_book {e}"))?;
        if !matches!(r.kind, MdKind::Add { .. }) {
            return Err(format!("initial_book record {i} must be an \"add\""));
        }
        out.push(r);
    }
    Ok(out)
}

/// Read the whole recording once, applying it to a book, so every problem is found before any
/// strategy runs.
pub fn verify_recording(
    path: &Path,
    format: RecordingFormat,
    initial_book: &[MdRecord],
    strict: bool,
) -> Result<RecordingInfo, String> {
    let sha256 = sha256_file(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut books: BTreeMap<String, Book> = BTreeMap::new();
    let mut order_sym = BTreeMap::new();
    let mut symbols = BTreeSet::new();
    for r in initial_book {
        if let MdKind::Add { sym, .. } = &r.kind {
            symbols.insert(sym.clone());
        }
        apply_record(&mut books, &mut order_sym, true, None, r)
            .map_err(|e| format!("initial_book: {e}"))?;
    }
    let (mut records, mut first_ts, mut last_ts) = (0u64, None, None);
    for r in open_events(path, format)? {
        let r = r?;
        if let Some(last) = last_ts
            && r.ts < last
        {
            return Err(format!(
                "record {records} goes back in time ({} after {last})",
                r.ts
            ));
        }
        if let MdKind::Add { sym, .. } | MdKind::Trade { sym, .. } = &r.kind {
            symbols.insert(sym.clone());
        }
        apply_record(&mut books, &mut order_sym, strict, Some(records), &r).map_err(|e| {
            format!(
                "record {records}: {e}{}",
                if strict {
                    " (if the recording starts mid-session, give an initial_book or set harness.strict_book=false)"
                } else {
                    ""
                }
            )
        })?;
        first_ts.get_or_insert(r.ts);
        last_ts = Some(r.ts);
        records += 1;
    }
    Ok(RecordingInfo {
        sha256,
        records,
        symbols: symbols.into_iter().collect(),
        first_ts,
        last_ts,
    })
}

/// The comparison rules between Python and C++: exact everywhere except `diag` numbers.
pub fn port_rules(tol: &DiagTolerance) -> Result<Rules, String> {
    let mut spec = RulesSpec {
        key: Some("/step".to_string()),
        ..RulesSpec::default()
    };
    if tol.abs != 0.0 || tol.rel != 0.0 || tol.ulps != 0 {
        spec.numbers.push(NumberRuleSpec {
            path: "/out/diag/*".to_string(),
            abs: tol.abs,
            rel: tol.rel,
            ulps: tol.ulps,
        });
    }
    Rules::from_spec(&spec).map_err(|e| format!("diag_tolerance: {e}"))
}

/// Rules for "does one version reproduce itself": exact on everything.
pub fn self_rules() -> Rules {
    Rules::from_spec(&RulesSpec {
        key: Some("/step".to_string()),
        ..RulesSpec::default()
    })
    .unwrap_or_default()
}

fn compare_transcripts(a: &Path, b: &Path, rules: &Rules) -> Result<Comparison, String> {
    let open = |p: &Path| {
        File::open(p)
            .map(|f| JsonLines::new(BufReader::new(f)))
            .map_err(|e| format!("cannot open the transcript {}: {e}", p.display()))
    };
    compare_streams(open(a)?, open(b)?, rules).map_err(|e| e.to_string())
}

fn resolve_program(argv0: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(argv0);
    if argv0.contains('/') {
        let full = match cwd {
            Some(c) if p.is_relative() => c.join(p),
            _ => p.to_path_buf(),
        };
        return full.is_file().then_some(full);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(argv0))
            .find(|c| c.is_file())
    })
}

/// Content fingerprints of what ran (a result belongs to the exact code it checked).
fn fingerprint(spec: &ProcessSpec, python: bool) -> Value {
    let cwd = spec.cwd.as_deref();
    let mut v = json!({"argv": spec.argv});
    if python {
        if let Some(script) = spec.argv.iter().skip(1).find(|a| a.ends_with(".py")) {
            let p = match cwd {
                Some(c) if Path::new(script).is_relative() => c.join(script),
                _ => PathBuf::from(script),
            };
            v["script"] = json!(p);
            v["script_sha256"] = json!(sha256_file(&p).ok());
            v["note"] = json!("only the entry script is fingerprinted; modules it imports are not");
        }
    } else if let Some(exe) = spec.argv.first().and_then(|a| resolve_program(a, cwd)) {
        v["executable"] = json!(exe);
        v["executable_sha256"] = json!(sha256_file(&exe).ok());
    }
    v
}

struct Outcome {
    status: Status,
    reason: Option<String>,
    sentence: String,
    first_difference: Option<Value>,
}

fn difference_value(which: &str, c: &Comparison) -> Option<Value> {
    c.first_difference
        .as_ref()
        .map(|d| json!({"comparison": which, "difference": d}))
}

fn step_text(c: &Comparison) -> String {
    let Some(d) = &c.first_difference else {
        return String::new();
    };
    let row = d.record_a.as_ref().or(d.record_b.as_ref());
    let step = d
        .key
        .as_ref()
        .map_or_else(|| "?".to_string(), Value::to_string);
    let ev = row
        .and_then(|r| r.get("ev"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let rec = row
        .and_then(|r| r.pointer("/in/rec"))
        .and_then(Value::as_u64)
        .map(|r| format!(", recording record {r}"))
        .unwrap_or_default();
    let ts = row
        .and_then(|r| r.get("ts"))
        .map(|t| format!(" at ts {t}"))
        .unwrap_or_default();
    format!("step {step} ({ev} event{ts}{rec})")
}

fn tolerance_text(t: &DiagTolerance) -> String {
    let mut parts = Vec::new();
    if t.abs > 0.0 {
        parts.push(format!("abs {}", t.abs));
    }
    if t.rel > 0.0 {
        parts.push(format!("rel {}", t.rel));
    }
    if t.ulps > 0 {
        parts.push(format!("{} ulps", t.ulps));
    }
    if parts.is_empty() {
        "exactly".to_string()
    } else {
        format!("within {}", parts.join(" or "))
    }
}

fn not_run(reason: String) -> Outcome {
    Outcome {
        status: Status::NotRun,
        sentence: format!("NOT RUN: {reason}"),
        reason: Some(reason),
        first_difference: None,
    }
}

/// Run the check. Never panics; every problem becomes NOT RUN with its reason.
pub fn port_check(req: &PortCheckRequest) -> PortCheckResult {
    let mut evidence = json!({
        "harness_version": HARNESS_VERSION,
        "protocol_version": PROTOCOL_VERSION,
        "harness": req.harness,
        "diag_tolerance": req.diag_tolerance,
    });
    let perf = json!({
        "status": "NOT RUN",
        "reason": "speed and allocation results come from the perf lab, which is not built yet; the wall times in evidence are the check runs themselves, not a benchmark",
    });
    let outcome = check_inner(req, &mut evidence);
    PortCheckResult {
        tool: "port_check",
        status: outcome.status,
        reason: outcome.reason,
        sentence: outcome.sentence,
        evidence,
        first_difference: outcome.first_difference,
        perf,
    }
}

fn run_set(
    label: &str,
    count: u32,
    spec: &ProcessSpec,
    python: bool,
    setup: &Setup<'_>,
) -> Vec<VersionRun> {
    std::thread::scope(|s| {
        // Collected on purpose: every run must start before the first join, or they would run
        // one after another.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..count as usize)
            .map(|i| {
                let mut varied = BTreeMap::new();
                if python {
                    varied.insert(
                        "PYTHONHASHSEED".to_string(),
                        PYTHON_HASH_SEEDS[i].to_string(),
                    );
                } else if !MALLOC_PERTURB[i].is_empty() {
                    varied.insert("MALLOC_PERTURB_".to_string(), MALLOC_PERTURB[i].to_string());
                }
                let name = format!("{label}-{}", i + 1);
                s.spawn(move || run_version(&name, spec, varied, setup))
            })
            .collect();
        handles
            .into_iter()
            .enumerate()
            .map(|(i, h)| {
                h.join().unwrap_or_else(|_| VersionRun {
                    label: format!("{label}-{}", i + 1),
                    varied: BTreeMap::new(),
                    transcript: PathBuf::new(),
                    steps: 0,
                    end: RunEnd::HarnessError {
                        detail: "the run thread panicked".to_string(),
                    },
                    elapsed_ms: 0,
                    stderr_tail: String::new(),
                    summary: None,
                })
            })
            .collect()
    })
}

fn tail_note(run: &VersionRun) -> String {
    let t = run.stderr_tail.trim();
    if t.is_empty() {
        String::new()
    } else {
        let lines: Vec<&str> = t.lines().collect();
        let from = lines.len().saturating_sub(3);
        format!(" Its stderr ends: {}", lines[from..].join(" | "))
    }
}

fn check_inner(req: &PortCheckRequest, evidence: &mut Value) -> Outcome {
    if let Err(e) = req.harness.validate() {
        return not_run(format!("harness: {e}"));
    }
    if req.python_runs < 2 || req.python_runs > 5 {
        return not_run(format!(
            "python_runs must be 2-5 (got {}): the Python version must reproduce itself before anything can be claimed",
            req.python_runs
        ));
    }
    if req.cpp_runs < 1 || req.cpp_runs > 5 {
        return not_run(format!("cpp_runs must be 1-5 (got {})", req.cpp_runs));
    }
    let rules = match port_rules(&req.diag_tolerance) {
        Ok(r) => r,
        Err(e) => return not_run(e),
    };
    let initial_book = match req
        .initial_book
        .as_deref()
        .map(read_initial_book)
        .transpose()
    {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return not_run(e),
    };
    let info = match verify_recording(
        &req.recording,
        req.recording_format,
        &initial_book,
        req.harness.strict_book,
    ) {
        Ok(i) => i,
        Err(e) => {
            return not_run(format!(
                "the recording {} is not usable: {e}",
                req.recording.display()
            ));
        }
    };
    evidence["recording"] = json!({
        "path": req.recording, "format": req.recording_format, "sha256": info.sha256, "records": info.records,
        "symbols": info.symbols, "first_ts": info.first_ts, "last_ts": info.last_ts,
    });
    evidence["python"] = fingerprint(&req.python, true);
    evidence["cpp"] = fingerprint(&req.cpp, false);
    let workdir = req.workdir.clone().unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("byteask-port-check-{}-{nanos}", std::process::id()))
    });
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return not_run(format!(
            "cannot create the work directory {}: {e}",
            workdir.display()
        ));
    }
    evidence["workdir"] = json!(workdir);
    let limits = req.limits.compile();
    let recording = req.recording.clone();
    let format = req.recording_format;
    let open = move || open_events(&recording, format);
    let setup = Setup {
        config: &req.harness,
        symbols: &info.symbols,
        initial_book: &initial_book,
        open_recording: &open,
        limits: &limits,
        workdir: &workdir,
    };

    // 1. Python against itself.
    let py = run_set("python", req.python_runs, &req.python, true, &setup);
    evidence["python"]["runs"] = json!(py);
    if let Some(bad) = py.iter().find(|r| !r.end.completed()) {
        return not_run(format!(
            "the Python version did not run cleanly, so there is nothing to compare against: {} {}.{}",
            bad.label,
            bad.end.describe(),
            tail_note(bad)
        ));
    }
    let strict = self_rules();
    for other in &py[1..] {
        match compare_transcripts(&py[0].transcript, &other.transcript, &strict) {
            Err(e) => return not_run(format!("comparing the Python runs failed: {e}")),
            Ok(c) if !c.same => {
                let which = format!("{} vs {}", py[0].label, other.label);
                let mut o = not_run(format!(
                    "the Python version is not reproducible under the harness: {which} first differ at {}. {} Two runs that see identical events must answer identically; look for wall-clock time, unseeded randomness, iteration over a set or a dict keyed by strings (the runs used different PYTHONHASHSEED values), threads, or reading files that change.",
                    step_text(&c),
                    report::sentence(&c, &py[0].label, &other.label)
                ));
                o.first_difference = difference_value(&which, &c);
                evidence["self_check"] = json!({"python": "differs"});
                return o;
            }
            Ok(_) => {}
        }
    }
    evidence["self_check"] = json!({"python": format!("reproduced exactly in {} runs", py.len())});

    // 2. C++ against Python, then against itself.
    let cc = run_set("cpp", req.cpp_runs, &req.cpp, false, &setup);
    evidence["cpp"]["runs"] = json!(cc);
    if let Some(bad) = cc.iter().find(|r| {
        matches!(
            r.end,
            RunEnd::SpawnFailed { .. } | RunEnd::HarnessError { .. }
        )
    }) {
        return not_run(format!(
            "the C++ version {} {}.",
            bad.label,
            bad.end.describe()
        ));
    }
    let which = format!("{} vs {}", py[0].label, cc[0].label);
    let port = match compare_transcripts(&py[0].transcript, &cc[0].transcript, &rules) {
        Ok(c) => c,
        Err(e) => return not_run(format!("comparing Python with C++ failed: {e}")),
    };
    evidence["comparison"] = json!({"rules": port.rules, "steps_compared": port.records_a});
    if !port.same {
        if let RunEnd::StrategyTimedOut { step, detail } = &cc[0].end {
            let diff_step = port
                .first_difference
                .as_ref()
                .and_then(|d| d.key.as_ref())
                .and_then(Value::as_u64);
            if diff_step == Some(*step) {
                let mut o = not_run(format!(
                    "the C++ version matched Python for {step} steps and then timed out at step {step} ({detail}); a hang or a very slow step cannot be called a mismatch.{}",
                    tail_note(&cc[0])
                ));
                o.first_difference = difference_value(&which, &port);
                return o;
            }
        }
        let crash = match &cc[0].end {
            RunEnd::StrategyFailed { step, detail } => {
                format!(" The C++ run {detail} at step {step}.{}", tail_note(&cc[0]))
            }
            RunEnd::HarnessLimit { step, detail } => {
                format!(" The C++ run hit a harness limit at step {step}: {detail}.")
            }
            _ => String::new(),
        };
        return Outcome {
            status: Status::Fail,
            reason: None,
            sentence: format!(
                "FAIL: the C++ version first differs from the Python version at {}. {}{crash}",
                step_text(&port),
                report::sentence(&port, "Python", "C++")
            ),
            first_difference: difference_value(&which, &port),
        };
    }
    for other in &cc[1..] {
        let c = match compare_transcripts(&cc[0].transcript, &other.transcript, &strict) {
            Ok(c) => c,
            Err(e) => return not_run(format!("comparing the C++ runs failed: {e}")),
        };
        if !c.same {
            let which = format!("{} vs {}", cc[0].label, other.label);
            evidence["self_check"]["cpp"] = json!("differs");
            return Outcome {
                status: Status::Fail,
                reason: None,
                sentence: format!(
                    "FAIL: the C++ version matched Python once but is not reproducible: {which} first differ at {}. {} Python is deterministic here, so the port is not; look for uninitialised memory (the second run filled new heap memory with a pattern), ordering by pointer value, unseeded randomness or threads.",
                    step_text(&c),
                    report::sentence(&c, &cc[0].label, &other.label)
                ),
                first_difference: difference_value(&which, &c),
            };
        }
    }
    evidence["self_check"]["cpp"] = json!(format!("reproduced exactly in {} runs", cc.len()));

    // 3. What was actually exercised.
    let counts = py[0]
        .summary
        .as_ref()
        .map(|s| s["counts"].clone())
        .unwrap_or(Value::Null);
    evidence["coverage"] = counts.clone();
    let n = |k: &str| counts.get(k).and_then(Value::as_u64).unwrap_or(0);
    let steps = py[0].steps;
    if n("orders") == 0 {
        return Outcome {
            status: Status::NoClaim,
            reason: Some("neither version sent an order on this recording".to_string()),
            sentence: format!(
                "NO CLAIM: both versions answered all {steps} steps identically, but neither sent a single order, so this says nothing about the order logic. Use a recording (or harness settings) where the strategy trades."
            ),
            first_difference: None,
        };
    }
    let mut untested = Vec::new();
    if n("fills") == 0 {
        untested.push("fills (no order traded)");
    }
    if n("rejects") == 0 {
        untested.push("rejects (set harness.reject_every or reject_ppm to inject some)");
    }
    if n("cancels") == 0 {
        untested.push("cancels (the strategy sent none)");
    }
    let untested = if untested.is_empty() {
        String::new()
    } else {
        format!(" Not exercised by this recording: {}.", untested.join("; "))
    };
    let file = req.recording.file_name().map_or_else(
        || req.recording.display().to_string(),
        |f| f.to_string_lossy().to_string(),
    );
    Outcome {
        status: Status::Pass,
        reason: None,
        sentence: format!(
            "PASS: on all {} records of {file} (sha256 {}), the C++ version sent the same {} orders and {} cancels, got the same {} fills and {} rejects, and made the same {} decisions as the Python version, step for step ({steps} steps), with diagnostics compared {}, under one harness (seed {}, latency {} ns). Python reproduced itself exactly in {} runs with different hash seeds; C++ in {} runs.{untested}",
            info.records,
            &info.sha256[..12.min(info.sha256.len())],
            n("orders"),
            n("cancels"),
            n("fills"),
            n("rejects"),
            n("decisions"),
            tolerance_text(&req.diag_tolerance),
            req.harness.seed,
            req.harness.latency_ns,
            py.len(),
            cc.len(),
        ),
        first_difference: None,
    }
}
