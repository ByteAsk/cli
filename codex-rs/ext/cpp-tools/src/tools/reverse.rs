//! `debug_reverse`: answer questions about ONE recorded execution (an rr trace)
//! by running time BACKWARDS from where it stopped. Nothing here re-runs the
//! live program: the recording is the evidence, and replaying it is
//! deterministic, so the same question always gets the same answer.
//!
//! The model never writes debugger commands for this tool. It picks a typed
//! question; a fixed driver ([`DRIVER`], Python inside gdb inside `rr replay`)
//! asks it and writes one JSON fact per line, which is rendered here.

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
use crate::run::run_capped_env;
use crate::run::timeout_from;
use crate::tools::runtime_util::ScratchDir;
use crate::tools::runtime_util::TraceProcess;
use crate::tools::runtime_util::cap;
use crate::tools::runtime_util::describe_exit;
use crate::tools::runtime_util::display_path;
use crate::tools::runtime_util::not_run;
use crate::tools::runtime_util::signal_name;
use crate::tools::runtime_util::source_line;
use crate::tools::runtime_util::tail_lines;

const TOOL_NAME: &str = "debug_reverse";
const OUTPUT_CAP: usize = 9000;
/// The fixed gdb-side driver. Shipped inside the engine, written to a private
/// scratch directory per call.
const DRIVER: &str = include_str!("rr_driver.py");
const MAX_COUNT: u32 = 50;

pub struct ReverseTool {
    pub cwd: PathBuf,
    /// Replaying re-executes the recorded program's code natively
    /// (with its system calls fed from the recording). See [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReverseArgs {
    query: String,
    #[serde(default)]
    trace: Option<String>,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    count: Option<u32>,
    #[serde(default)]
    stop_at: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    stop_hit: Option<u32>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    stop_event: Option<u64>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    WhoWrote,
    LastReached,
    LastLines,
    StopPoint,
}

impl Query {
    fn parse(text: &str) -> Result<Self, String> {
        match text.trim() {
            "who_wrote" => Ok(Self::WhoWrote),
            "last_reached" => Ok(Self::LastReached),
            "last_lines" => Ok(Self::LastLines),
            "stop_point" => Ok(Self::StopPoint),
            other => Err(format!(
                "unknown query {other:?}: expected \"who_wrote\", \"last_reached\", \"last_lines\" \
                 or \"stop_point\""
            )),
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Self::WhoWrote => "who_wrote",
            Self::LastReached => "last_reached",
            Self::LastLines => "last_lines",
            Self::StopPoint => "stop_point",
        }
    }

    fn default_count(self) -> u32 {
        match self {
            Self::WhoWrote | Self::LastReached => 3,
            Self::LastLines => 20,
            Self::StopPoint => 1,
        }
    }
}

/// Where the replay starts the debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// From the beginning; the driver runs forward to the stop point.
    Beginning,
    /// `rr replay -g N`: at an event number from an earlier answer.
    Event(u64),
    /// `rr replay -e`: at the end of the recording (a normal exit).
    Exit,
}

/// One checked request: everything the driver needs, nothing it must guess.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    query: Query,
    expr: Option<String>,
    location: Option<String>,
    count: u32,
    stop_at: Option<String>,
    stop_hit: u32,
    stop_event: Option<u64>,
}

fn check_request(args: &ReverseArgs) -> Result<Request, String> {
    let query = Query::parse(&args.query)?;
    let expr = args
        .expr
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    let location = args
        .location
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let stop_at = args
        .stop_at
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    match query {
        Query::WhoWrote if expr.is_none() => {
            return Err(
                "query \"who_wrote\" needs `expr`: the variable, field or address whose \
                        writes you want (e.g. \"book.ask\", \"o->next\", \"*(long*)0x7ffd1234\")"
                    .to_string(),
            );
        }
        Query::LastReached if location.is_none() => {
            return Err(
                "query \"last_reached\" needs `location`: a function or file:line \
                        (e.g. \"OrderBook::apply\", \"book.cpp:42\")"
                    .to_string(),
            );
        }
        _ => {}
    }
    if expr.is_some() && query != Query::WhoWrote {
        return Err("`expr` is only used by query \"who_wrote\"".to_string());
    }
    if location.is_some() && query != Query::LastReached {
        return Err("`location` is only used by query \"last_reached\"".to_string());
    }
    if stop_at.is_some() && args.stop_event.is_some() {
        return Err("give `stop_at` or `stop_event`, not both: both say where to stop".to_string());
    }
    if args.stop_hit.is_some() && stop_at.is_none() {
        return Err("`stop_hit` counts hits of `stop_at`; give `stop_at` too".to_string());
    }
    if let Some(expr) = &expr {
        check_expr(expr)?;
    }
    for (field, spec) in [("location", &location), ("stop_at", &stop_at)] {
        if let Some(spec) = spec {
            check_location(field, spec)?;
        }
    }
    let count = args
        .count
        .unwrap_or(query.default_count())
        .clamp(1, MAX_COUNT);
    Ok(Request {
        query,
        expr,
        location,
        count,
        stop_at,
        stop_hit: args.stop_hit.unwrap_or(1).max(1),
        stop_event: args.stop_event,
    })
}

/// A watch expression is evaluated by gdb in the replayed program. It may
/// READ anything, but it must not change the replay: no assignment, no
/// increment, no call into the program (calls are also switched off in gdb).
fn check_expr(expr: &str) -> Result<(), String> {
    if expr.len() > 200 {
        return Err("`expr` is longer than 200 characters".to_string());
    }
    if expr.chars().any(char::is_control) {
        return Err("`expr` contains a line break or control character".to_string());
    }
    let bytes = expr.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| bytes[j]);
        let next = bytes.get(i + 1).copied();
        if b == b'=' && !matches!(prev, Some(b'=' | b'!' | b'<' | b'>')) && next != Some(b'=') {
            return Err(format!(
                "`expr` {expr:?} assigns (`=`): a watch expression may only read. Use `==` to compare"
            ));
        }
    }
    if expr.contains("++") || expr.contains("--") {
        return Err(format!(
            "`expr` {expr:?} modifies a value (`++`/`--`): it may only read"
        ));
    }
    // A call: an identifier, `)` or `]` directly followed by `(`, except the
    // operators that look like calls but only read.
    let mut word = String::new();
    let mut last_significant: Option<char> = None;
    for c in expr.chars() {
        if c == '(' {
            let calls = match last_significant {
                Some(')') | Some(']') => true,
                Some(p) if p.is_alphanumeric() || p == '_' => !matches!(
                    word.as_str(),
                    "sizeof" | "alignof" | "_Alignof" | "decltype" | "typeof"
                ),
                _ => false,
            };
            if calls {
                return Err(format!(
                    "`expr` {expr:?} calls a function: a watch expression may only read memory. \
                     Name the variable or field the function would return"
                ));
            }
        }
        if c.is_alphanumeric() || c == '_' {
            if !last_significant.is_some_and(|p| p.is_alphanumeric() || p == '_') {
                word.clear();
            }
            word.push(c);
        }
        if !c.is_whitespace() {
            last_significant = Some(c);
        }
    }
    Ok(())
}

/// A gdb linespec: `function`, `Class::method`, `file.cpp:42`, `*0x401136`.
/// Breakpoint conditions are refused (they are expressions run on every hit).
fn check_location(field: &str, spec: &str) -> Result<(), String> {
    if spec.len() > 300 {
        return Err(format!("`{field}` is longer than 300 characters"));
    }
    if spec.chars().any(char::is_control) {
        return Err(format!(
            "`{field}` contains a line break or control character"
        ));
    }
    let lower = format!(" {} ", spec.to_ascii_lowercase());
    for word in [
        " if ",
        " thread ",
        " task ",
        " -force-condition ",
        " inferior ",
    ] {
        if lower.contains(word) {
            return Err(format!(
                "`{field}` {spec:?} carries a breakpoint condition or qualifier ({}); give a plain \
                 location: a function, file:line or *ADDRESS",
                word.trim()
            ));
        }
    }
    if spec.starts_with('-') {
        return Err(format!(
            "`{field}` {spec:?} starts with `-`; give a plain location: a function, file:line or \
             *ADDRESS"
        ));
    }
    Ok(())
}

impl<'call> ToolExecutor<ToolCall<'call>> for ReverseTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "query".to_string(),
            JsonSchema::string_enum(
                vec![
                    serde_json::json!("who_wrote"),
                    serde_json::json!("last_reached"),
                    serde_json::json!("last_lines"),
                    serde_json::json!("stop_point"),
                ],
                Some(
                    "\"who_wrote\": the most recent writes to `expr` before the stop, newest \
                     first, each with thread, function, file:line and the value changed from -> to. \
                     \"last_reached\": the most recent times `location` ran before the stop, with \
                     its arguments. \"last_lines\": the source lines the stopped thread ran just \
                     before the stop, oldest first. \"stop_point\": where the recording stops and \
                     what every thread was doing there."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "trace".to_string(),
            JsonSchema::string(Some(
                "rr trace directory (debug_record prints it as `trace: …`). Default: the \
                 recording rr made most recently on this machine."
                    .to_string(),
            )),
        );
        properties.insert(
            "expr".to_string(),
            JsonSchema::string(Some(
                "who_wrote: what to watch, evaluated at the stop in your innermost frame: a \
                 variable, field or address (\"book.ask\", \"o->next\", \"*(long*)0x7ffd1234\"). \
                 It may only read: no assignment and no calls. At most 32 bytes."
                    .to_string(),
            )),
        );
        properties.insert(
            "location".to_string(),
            JsonSchema::string(Some(
                "last_reached: a function, file:line or *ADDRESS (\"OrderBook::apply\", \
                 \"book.cpp:42\")."
                    .to_string(),
            )),
        );
        properties.insert(
            "count".to_string(),
            JsonSchema::number(Some(format!(
                "How many answers to walk back: writes or hits (default 3), lines (default 20); \
                 capped at {MAX_COUNT}."
            ))),
        );
        properties.insert(
            "stop_at".to_string(),
            JsonSchema::string(Some(
                "Ask from here instead of the end of the recording: a function, file:line or \
                 *ADDRESS, reached for the `stop_hit`-th time (default 1)."
                    .to_string(),
            )),
        );
        properties.insert(
            "stop_hit".to_string(),
            JsonSchema::number(Some(
                "Which arrival at `stop_at` to stop on (1 = the first).".to_string(),
            )),
        );
        properties.insert(
            "stop_event".to_string(),
            JsonSchema::number(Some(
                "Ask from this rr event number instead, to chain questions further back. An answer \
                 labelled `event N` happened just after event N, so stop_event N-1 asks from just \
                 before it."
                    .to_string(),
            )),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some(
                "Wall-clock limit for the replay (default 300).".to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Ask a RECORDED execution (an rr trace) what happened, by running time \
                BACKWARDS from where it stopped — it never re-runs the program, so a crash, race \
                or wrong value captured once can be examined as often as needed and always gives \
                the same answer. Use it after debug_record (or on any rr trace) for: WHO WROTE a \
                bad value (query=\"who_wrote\": a hardware watchpoint plus reverse-continue lands \
                on each write, newest first, naming the thread, function, file:line and the value \
                before -> after — including a write from ANOTHER thread, the question a forward \
                debugger cannot answer); WHEN a function last ran and with which arguments \
                (\"last_reached\"); WHICH lines led to the stop (\"last_lines\"); WHERE it \
                stopped and what every thread was doing (\"stop_point\"). Questions are asked from \
                the end of the recording (the crash signal, or the exit) unless stop_at / \
                stop_event move the stop. Chain them: who wrote the bad pointer, then who wrote \
                the field it was loaded from. Linux with rr only; needs hardware performance \
                counters, like recording. C/C++."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["query".to_string()]),
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
        // Each replay re-executes the whole recording on one core; parallel
        // ones thrash a laptop and change nothing about the answers.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl ReverseTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: ReverseArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let request = check_request(&args).map_err(FunctionCallError::RespondToModel)?;
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
        let timeout = timeout_from(args.timeout_sec);

        let trace = match resolve_trace(&self.cwd, args.trace.as_deref()) {
            Ok(trace) => trace,
            Err(TraceError::Model(msg)) => return Err(FunctionCallError::RespondToModel(msg)),
            Err(TraceError::NotRun { reason, fix }) => {
                let body = not_run(&reason, &fix);
                let result = crate::tools::runtime_util::verdict(TOOL_NAME, &body, false, "");
                return Ok(crate::report::report(
                    TOOL_NAME,
                    request.query.wire(),
                    body,
                    result,
                ));
            }
        };
        let label = format!("{} · {}", request.query.wire(), trace.display());

        let processes = match run_capped(
            "rr",
            &["ps".to_string(), trace.to_string_lossy().into_owned()],
            &self.cwd,
            timeout_from(Some(60)),
        )
        .await?
        {
            RunOutcome::Finished {
                success: true,
                stdout,
                ..
            } => crate::tools::runtime_util::parse_rr_ps(&stdout),
            RunOutcome::Finished { stderr, .. } => {
                let body = not_run(
                    &format!(
                        "rr could not read the recording {}: {}",
                        trace.display(),
                        tail_lines(&stderr, 3)
                    ),
                    "point `trace` at a directory rr recorded (rr ls lists them), with the rr that recorded it",
                );
                let result = crate::tools::runtime_util::verdict(TOOL_NAME, &body, false, "");
                return Ok(crate::report::report(TOOL_NAME, &label, body, result));
            }
            RunOutcome::TimedOut { secs } => {
                let body = not_run(
                    &format!("`rr ps` did not answer within {secs}s"),
                    "check the trace directory is on a local disk",
                );
                let result = crate::tools::runtime_util::verdict(TOOL_NAME, &body, false, "");
                return Ok(crate::report::report(TOOL_NAME, &label, body, result));
            }
        };
        let first = processes.first().cloned();
        let fatal_signal = first
            .as_ref()
            .and_then(|p| p.exit)
            .filter(|code| *code < 0)
            .and_then(|code| signal_name(-code));
        let start = start_for(&request, first.as_ref().and_then(|p| p.exit));

        let scratch = ScratchDir::new("rr-reverse").map_err(|e| {
            FunctionCallError::RespondToModel(format!("cannot create a scratch directory: {e}"))
        })?;
        let driver = scratch.path().join("byteask_rr_driver.py");
        let hook = scratch.path().join("hook.gdb");
        let out = scratch.path().join("answer.jsonl");
        let io = |e: std::io::Error| {
            FunctionCallError::RespondToModel(format!("cannot write the replay driver: {e}"))
        };
        std::fs::write(&driver, DRIVER).map_err(io)?;
        std::fs::write(&hook, hook_script(&driver)).map_err(io)?;
        let roots = vec![std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone())];
        let query_json = query_json(&request, fatal_signal, &roots);

        let mut unbound = false;
        let (records, timed_out, stderr) = loop {
            let _ = std::fs::remove_file(&out);
            let argv = build_replay_argv(&trace, start, unbound, &hook);
            let envs = vec![
                ("BYTEASK_RR_QUERY".to_string(), query_json.clone()),
                (
                    "BYTEASK_RR_OUT".to_string(),
                    out.to_string_lossy().into_owned(),
                ),
            ];
            let outcome = run_capped_env("rr", &argv, &self.cwd, timeout, &envs).await?;
            let records = read_records(&out);
            let (timed_out, stderr) = match outcome {
                RunOutcome::TimedOut { secs } => (Some(secs), String::new()),
                RunOutcome::Finished { stdout, stderr, .. } => {
                    (None, format!("{stdout}\n{stderr}"))
                }
            };
            if !unbound && records.is_empty() && cpu_bind_failed(&stderr) {
                // The trace was recorded on a CPU this process may not use (an
                // isolated trading core, taskset, a cgroup cpuset). rr checks
                // every replay against the recording and stops on divergence,
                // so an unbound replay is either right or loudly refused.
                unbound = true;
                continue;
            }
            break (records, timed_out, stderr);
        };

        let header = header_lines(&trace, first.as_ref(), processes.len(), unbound);
        let body = render(&request, &header, &records, timed_out, &stderr, &self.cwd)?;
        let cut = body.len() > OUTPUT_CAP;
        let result = crate::tools::runtime_util::verdict(
            TOOL_NAME,
            &body,
            cut,
            "Answered from the recording by running it backwards.",
        );
        Ok(crate::report::report(
            TOOL_NAME,
            &label,
            cap(body, OUTPUT_CAP),
            result,
        ))
    }
}

enum TraceError {
    Model(String),
    NotRun { reason: String, fix: String },
}

fn resolve_trace(cwd: &Path, given: Option<&str>) -> Result<PathBuf, TraceError> {
    if let Some(given) = given.map(str::trim).filter(|g| !g.is_empty()) {
        let path = if Path::new(given).is_absolute() {
            PathBuf::from(given)
        } else {
            cwd.join(given)
        };
        if !path.is_dir() {
            return Err(TraceError::Model(format!(
                "trace {} is not a directory",
                path.display()
            )));
        }
        if !crate::tools::runtime_util::is_rr_trace(&path) {
            return Err(TraceError::Model(format!(
                "{} is not an rr recording (no `version`/`events` files); give the directory \
                 debug_record printed as `trace: …`",
                path.display()
            )));
        }
        return Ok(std::fs::canonicalize(&path).unwrap_or(path));
    }
    crate::tools::runtime_util::latest_trace().ok_or_else(|| TraceError::NotRun {
        reason: "there is no rr recording on this machine to ask about".to_string(),
        fix: "record the failing run first with debug_record (it prints `trace: …`), then pass \
              that path as `trace`"
            .to_string(),
    })
}

/// Where the debugger should start for this request.
fn start_for(request: &Request, exit: Option<i32>) -> Start {
    if let Some(event) = request.stop_event {
        return Start::Event(event);
    }
    if request.stop_at.is_some() {
        return Start::Beginning;
    }
    match exit {
        // Killed by a signal: run forward to its delivery (the crash itself).
        Some(code) if code < 0 => Start::Beginning,
        _ => Start::Exit,
    }
}

fn query_json(request: &Request, fatal_signal: Option<&str>, roots: &[PathBuf]) -> String {
    serde_json::json!({
        "query": request.query.wire(),
        "expr": request.expr,
        "location": request.location,
        "count": request.count,
        "stop_at": request.stop_at,
        "stop_hit": request.stop_hit,
        "started_at_event": request.stop_event,
        // A stop_event or stop_at decides the stop; the fatal signal only
        // when neither is given.
        "fatal_signal": if request.stop_event.is_none() && request.stop_at.is_none() { fatal_signal } else { None },
        "user_roots": roots.iter().map(|r| r.to_string_lossy().into_owned()).collect::<Vec<_>>(),
    })
    .to_string()
}

/// rr puts the caller's gdb options BEFORE its own `target extended-remote`,
/// so a plain `-x driver.py` runs before the replay is connected (measured:
/// "The program is not being run."). The driver therefore runs from a hook on
/// that very command.
fn hook_script(driver: &Path) -> String {
    format!(
        "define target hookpost-extended-remote\nsource {}\nend\n",
        driver.display()
    )
}

fn build_replay_argv(trace: &Path, start: Start, unbound: bool, hook: &Path) -> Vec<String> {
    let mut argv = vec!["replay".to_string()];
    if unbound {
        argv.push("-u".to_string());
    }
    match start {
        Start::Beginning => {}
        Start::Event(n) => {
            argv.push("-g".to_string());
            argv.push(n.to_string());
        }
        Start::Exit => argv.push("-e".to_string()),
    }
    // Do not replay the program's own stdout/stderr into our capture.
    argv.push("-q".to_string());
    argv.push(trace.to_string_lossy().into_owned());
    argv.extend(
        [
            "--",
            "-q",
            "-nx",
            "-batch",
            "-iex",
            "set auto-load off",
            "-iex",
            "set may-call-functions off",
            "-x",
        ]
        .map(str::to_string),
    );
    argv.push(hook.to_string_lossy().into_owned());
    argv
}

fn cpu_bind_failed(stderr: &str) -> bool {
    stderr.contains("Can't bind to requested CPU")
}

// ------------------------------- driver output -------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
struct Frame {
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    pc: Option<String>,
    #[serde(default)]
    user: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Thread {
    tid: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ThreadFrames {
    tid: i64,
    #[serde(default)]
    frames: Vec<Frame>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Stop {
        how: String,
        #[serde(default)]
        event: Option<u64>,
        #[serde(default)]
        frames: Vec<Frame>,
        #[serde(default)]
        signal: Option<String>,
        #[serde(default)]
        thread: Option<Thread>,
        #[serde(default)]
        note: Option<String>,
    },
    Target {
        expr: String,
        address: String,
        size: u64,
        #[serde(rename = "type")]
        ty: String,
        value: String,
    },
    Write {
        #[serde(default)]
        event: Option<u64>,
        thread: Thread,
        #[serde(default)]
        frames: Vec<Frame>,
        before: String,
        after: String,
    },
    Hit {
        #[serde(default)]
        event: Option<u64>,
        thread: Thread,
        #[serde(default)]
        frames: Vec<Frame>,
        #[serde(default)]
        args: Vec<(String, String)>,
    },
    Line {
        #[serde(default)]
        event: Option<u64>,
        #[serde(flatten)]
        frame: Frame,
    },
    Threads {
        threads: Vec<ThreadFrames>,
    },
    Start {
        #[serde(default)]
        value: Option<String>,
    },
    Error {
        #[serde(rename = "where")]
        place: String,
        message: String,
    },
    Done,
}

fn read_records(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

// --------------------------------- rendering ---------------------------------

fn header_lines(
    trace: &Path,
    first: Option<&TraceProcess>,
    processes: usize,
    unbound: bool,
) -> String {
    let mut text = match first {
        Some(p) => format!(
            "recording: {} — `{}` {}\n",
            trace.display(),
            p.command,
            describe_exit(p.exit)
        ),
        None => format!("recording: {}\n", trace.display()),
    };
    if processes > 1 {
        text.push_str(&format!(
            "note: the recording holds {processes} processes; these answers are about the first one.\n"
        ));
    }
    if unbound {
        text.push_str(
            "note: the trace was recorded on a CPU this process may not use (CPU isolation or \
             affinity), so it was replayed unbound (`rr replay -u`); rr checks the replay against \
             the recording and stops on any divergence.\n",
        );
    }
    text
}

/// The frame a person would point at: the innermost one in their own code.
fn user_frame(frames: &[Frame]) -> Option<&Frame> {
    frames.iter().find(|f| f.user)
}

fn frame_text(frame: &Frame, cwd: &Path) -> String {
    let name = frame
        .function
        .clone()
        .unwrap_or_else(|| frame.pc.clone().unwrap_or_else(|| "??".to_string()));
    match (&frame.file, frame.line) {
        (Some(file), Some(line)) => format!("{name}() at {}:{line}", display_path(file, cwd)),
        _ => match &frame.library {
            Some(lib) => format!("{name} in {}", short_lib(lib)),
            None => name,
        },
    }
}

fn short_lib(lib: &str) -> &str {
    lib.rsplit('/').next().unwrap_or(lib)
}

/// "in risk_reset() at book.cpp:14", or, when the write happened inside a
/// library, "in __strcpy_evex (libc.so.6), called from set_name() at …".
fn site_text(frames: &[Frame], cwd: &Path) -> String {
    let Some(top) = frames.first() else {
        return "at an unknown place".to_string();
    };
    if top.user {
        return frame_text(top, cwd);
    }
    let lib = top
        .library
        .as_deref()
        .map(short_lib)
        .unwrap_or("system code");
    let inner = top.function.clone().unwrap_or_else(|| "??".to_string());
    match user_frame(frames) {
        Some(user) => format!("{inner} ({lib}), called from {}", frame_text(user, cwd)),
        None => format!("{inner} ({lib})"),
    }
}

fn source_of(frames: &[Frame]) -> Option<String> {
    let user = user_frame(frames)?;
    source_line(user.file.as_deref()?, user.line?)
}

/// "thread 4221 (risk_reset)": the outermost frame of the user's own code on
/// that thread is how a person knows which thread it is.
fn thread_text(tid: i64, frames: &[Frame]) -> String {
    match frames
        .iter()
        .rev()
        .find(|f| f.user)
        .and_then(|f| f.function.as_deref())
    {
        Some(entry) => format!("thread {tid} ({entry})"),
        None => format!("thread {tid}"),
    }
}

fn event_text(event: Option<u64>) -> String {
    event
        .map(|e| format!("event {e}"))
        .unwrap_or_else(|| "event ?".to_string())
}

fn render(
    request: &Request,
    header: &str,
    records: &[Record],
    timed_out: Option<u64>,
    stderr: &str,
    cwd: &Path,
) -> Result<String, FunctionCallError> {
    let mut text = header.to_string();
    let stop = records.iter().find_map(|r| match r {
        Record::Stop {
            how,
            event,
            frames,
            signal,
            thread,
            note,
        } => Some((how, event, frames, signal, thread, note)),
        _ => None,
    });
    let done = records.iter().any(|r| matches!(r, Record::Done));

    // Errors the model can fix (a bad expression or location) go back as an
    // error; everything else is a run that did not happen.
    if let Some((place, message)) = records.iter().find_map(|r| match r {
        Record::Error { place, message } => Some((place.as_str(), message.as_str())),
        _ => None,
    }) {
        let at = stop.map(|(_, event, frames, signal, ..)| {
            stop_text(signal.as_deref(), *event, frames, cwd)
        });
        match place {
            "expr" | "location" | "stop_at" => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{text}Nothing answered: `{place}` was refused by gdb: {message}{}",
                    at.map(|a| format!("\n(the stop point was: {a})"))
                        .unwrap_or_default()
                )));
            }
            _ => {
                text.push_str(&not_run(
                    &format!("the backwards run failed in gdb: {message}"),
                    "ask again from an earlier stop (stop_at / stop_event); if it repeats, record the run again with debug_record",
                ));
                return Ok(text);
            }
        }
    }

    let Some((how, event, frames, signal, thread, note)) = stop else {
        // Nothing reached the stop point at all.
        text.push_str(&not_run_reason(timed_out, stderr));
        return Ok(text);
    };

    let stop_line = match how.as_str() {
        "end" => format!(
            "the end of the recording (after the program exited, {})",
            event_text(*event)
        ),
        "stop_at" => format!(
            "arrival {} at `{}`: {} in {} ({})",
            request.stop_hit,
            request.stop_at.as_deref().unwrap_or(""),
            user_frame(frames)
                .map(|f| frame_text(f, cwd))
                .unwrap_or_else(|| site_text(frames, cwd)),
            thread
                .as_ref()
                .map(|t| thread_text(t.tid, frames))
                .unwrap_or_else(|| "thread ?".into()),
            event_text(*event)
        ),
        "event" => match user_frame(frames) {
            Some(f) => format!(
                "{}: {} in {}",
                event_text(*event),
                frame_text(f, cwd),
                thread
                    .as_ref()
                    .map(|t| thread_text(t.tid, frames))
                    .unwrap_or_else(|| "thread ?".into())
            ),
            None => format!(
                "{} (thread {} was outside your code there)",
                event_text(*event),
                thread
                    .as_ref()
                    .map(|t| t.tid.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
        },
        _ => format!(
            "{} in {}",
            stop_text(signal.as_deref(), *event, frames, cwd),
            thread
                .as_ref()
                .map(|t| thread_text(t.tid, frames))
                .unwrap_or_else(|| "thread ?".into())
        ),
    };
    text.push_str(&format!("asked from: {stop_line}\n"));
    if let Some(note) = note {
        text.push_str(&format!("note: {note}\n"));
    }
    let stop_tid = thread.as_ref().map(|t| t.tid);

    let mut body = String::new();
    let mut found = 0u32;
    let mut reached_start = None;
    match request.query {
        Query::StopPoint => render_stop_point(&mut body, frames, records, stop_tid, cwd),
        Query::WhoWrote => {
            if let Some(Record::Target {
                expr,
                address,
                size,
                ty,
                value,
            }) = records.iter().find(|r| matches!(r, Record::Target { .. }))
            {
                body.push_str(&format!(
                    "watching `{expr}` — {ty}, {size} bytes at {address}; its value at the stop: {value}\n\n"
                ));
            }
            body.push_str(
                "writes, most recent first (found by running backwards from the stop):\n",
            );
            for r in records {
                match r {
                    Record::Write {
                        event,
                        thread,
                        frames,
                        before,
                        after,
                    } => {
                        found += 1;
                        let change = if before.starts_with("<unreadable") {
                            format!(
                                "this memory did not exist before here (it was just allocated or mapped); now {after}"
                            )
                        } else {
                            format!("{before} -> {after}")
                        };
                        body.push_str(&format!(
                            "{found:>2}. {} · {} — {change}\n    {}\n",
                            event_text(*event),
                            thread_text(thread.tid, frames),
                            site_text(frames, cwd)
                        ));
                        if let Some(src) = source_of(frames) {
                            body.push_str(&format!("      | {src}\n"));
                        }
                        if found == 1 && stop_tid.is_some_and(|t| t != thread.tid) {
                            body.push_str(&format!(
                                "    ^ written by a DIFFERENT thread ({}) than the one that stopped ({}).\n",
                                thread.tid,
                                stop_tid.unwrap_or_default()
                            ));
                        }
                    }
                    Record::Start { value } => reached_start = Some(value.clone()),
                    _ => {}
                }
            }
            if found == 0 && reached_start.is_some() {
                body.push_str("  (none)\n");
            }
            match &reached_start {
                Some(value) => body.push_str(&format!(
                    "no earlier write in the recording: {} was already there when the recording began \
                     (an initializer, or written before rr started).\n",
                    value.as_deref().unwrap_or("the value")
                )),
                None if done && found == request.count => body.push_str(&format!(
                    "{found} write(s) shown (count={}); earlier writes exist: raise `count` to walk further back.\n",
                    request.count
                )),
                None => {}
            }
        }
        Query::LastReached => {
            body.push_str(&format!(
                "most recent times `{}` ran before the stop, newest first:\n",
                request.location.as_deref().unwrap_or("")
            ));
            for r in records {
                match r {
                    Record::Hit {
                        event,
                        thread,
                        frames,
                        args,
                    } => {
                        found += 1;
                        let shown_args = args
                            .iter()
                            .map(|(n, v)| format!("{n}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        body.push_str(&format!(
                            "{found:>2}. {} · {} — {}\n",
                            event_text(*event),
                            thread_text(thread.tid, frames),
                            site_text(frames, cwd)
                        ));
                        if !shown_args.is_empty() {
                            body.push_str(&format!("    args: {shown_args}\n"));
                        }
                        if let Some(caller) = frames.iter().skip(1).find(|f| f.user) {
                            body.push_str(&format!(
                                "    called from {}\n",
                                frame_text(caller, cwd)
                            ));
                        }
                    }
                    Record::Start { .. } => reached_start = Some(None),
                    _ => {}
                }
            }
            if found == 0 && reached_start.is_some() {
                body.push_str("  (never: it did not run before the stop)\n");
            } else if reached_start.is_some() {
                body.push_str(&format!("that is every time before the stop ({found}).\n"));
            } else if done && found == request.count {
                body.push_str(&format!(
                    "{found} shown (count={}); it ran earlier too: raise `count` to see more.\n",
                    request.count
                ));
            }
        }
        Query::LastLines => {
            let lines: Vec<(&Option<u64>, &Frame)> = records
                .iter()
                .filter_map(|r| match r {
                    Record::Line { event, frame } => Some((event, frame)),
                    _ => None,
                })
                .collect();
            reached_start = records
                .iter()
                .any(|r| matches!(r, Record::Start { .. }))
                .then_some(None);
            let user: Vec<_> = lines.iter().filter(|(_, f)| f.user).collect();
            found = user.len() as u32;
            body.push_str(&format!(
                "the last {} line(s) {} ran before the stop, oldest first:\n",
                user.len(),
                stop_tid
                    .map(|t| format!("thread {t}"))
                    .unwrap_or_else(|| "the stopped thread".into())
            ));
            for (i, (event, frame)) in user.iter().rev().enumerate() {
                let marker = if i + 1 == user.len() {
                    "   <- the stop"
                } else {
                    ""
                };
                let src = frame
                    .file
                    .as_deref()
                    .zip(frame.line)
                    .and_then(|(f, l)| source_line(f, l))
                    .unwrap_or_default();
                let row = format!(
                    "  {:<10} {:<28} {src}{marker}",
                    event_text(**event),
                    frame
                        .file
                        .as_deref()
                        .map(|f| format!("{}:{}", display_path(f, cwd), frame.line.unwrap_or(0)))
                        .unwrap_or_default()
                );
                body.push_str(row.trim_end());
                body.push('\n');
            }
            if lines.len() > user.len() {
                body.push_str("(earlier, the thread was in system code: that is where its history in your code begins)\n");
            } else if reached_start.is_some() {
                body.push_str("(that is where the recording begins)\n");
            }
        }
    }
    text.push('\n');
    text.push_str(&body);

    if let Some(secs) = timed_out {
        text.push_str(&format!(
            "\nTIMED OUT after {secs}s: the answer above is INCOMPLETE ({found} of the {} requested were \
             found before the limit; the list is not the full history). Raise `timeout_sec`, lower \
             `count`, or ask from a later stop (stop_at / stop_event).\n",
            request.count
        ));
    } else if !done {
        text.push_str(&format!(
            "\nINCOMPLETE: the replay ended before the question was finished; the list above is \
             partial.\n{}\n",
            tail_lines(stderr, 6)
        ));
    }
    Ok(text)
}

fn stop_text(signal: Option<&str>, event: Option<u64>, frames: &[Frame], cwd: &Path) -> String {
    let what = signal.unwrap_or("the stop");
    format!(
        "{what} at {} ({})",
        site_text(frames, cwd),
        event_text(event)
    )
}

fn render_stop_point(
    body: &mut String,
    frames: &[Frame],
    records: &[Record],
    stop_tid: Option<i64>,
    cwd: &Path,
) {
    body.push_str("backtrace of the stopped thread (your code marked *):\n");
    let mut hidden = 0;
    for (level, frame) in frames.iter().enumerate() {
        if frame.user || level == 0 {
            body.push_str(&format!(
                "  {}#{level} {}\n",
                if frame.user { "*" } else { " " },
                frame_text(frame, cwd)
            ));
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        body.push_str(&format!("  ({hidden} system frame(s) not shown)\n"));
    }
    if let Some(src) = source_of(frames) {
        body.push_str(&format!("      | {src}\n"));
    }
    if let Some(Record::Threads { threads }) =
        records.iter().find(|r| matches!(r, Record::Threads { .. }))
    {
        body.push_str(&format!("\nthreads at the stop ({}):\n", threads.len()));
        for t in threads {
            let mark = if Some(t.tid) == stop_tid { "*" } else { " " };
            let place = user_frame(&t.frames)
                .map(|f| frame_text(f, cwd))
                .unwrap_or_else(|| site_text(&t.frames, cwd));
            body.push_str(&format!(
                "  {mark} {} — {place}\n",
                thread_text(t.tid, &t.frames)
            ));
        }
    }
}

/// Why nothing at all was answered, from rr's / gdb's own words.
fn not_run_reason(timed_out: Option<u64>, stderr: &str) -> String {
    if let Some(secs) = timed_out {
        return not_run(
            &format!("the replay did not reach the stop point within {secs}s"),
            "raise `timeout_sec`, or move the stop earlier with stop_at / stop_event",
        );
    }
    if crate::tools::record::is_perf_unavailable(stderr) {
        return format!(
            "RESULT: NOT RUN — rr cannot replay here.\n{}\n",
            crate::tools::record::unavailable_message()
        );
    }
    if stderr.contains("Python scripting is not supported") {
        return not_run(
            "this machine's gdb was built without Python, which debug_reverse uses to ask its questions",
            "install the distribution's gdb package (it includes Python), e.g. `sudo apt install gdb`",
        );
    }
    if cpu_bind_failed(stderr) {
        return not_run(
            &format!(
                "rr could not replay on the CPU the trace was recorded on, even unbound: {}",
                tail_lines(stderr, 2)
            ),
            "replay where that CPU is available (e.g. inside the same cpuset), or record again on an allowed CPU",
        );
    }
    not_run(
        &format!(
            "rr replay stopped before answering: {}",
            tail_lines(stderr, 4)
        ),
        "check the trace was recorded by this rr version on this kind of CPU (rr traces are not portable across CPU models)",
    )
}

#[cfg(test)]
#[path = "reverse_tests.rs"]
mod tests;
