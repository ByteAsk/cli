//! `cpp_perf_lab` — measure on the perf lab: an isolated, checked
//! core on a dedicated bench host, driven by `perf-runner`.
//!
//! The tool exists only when a lab is configured (`BYTEASK_PERF_LAB`), so a
//! session without one sees no change at all:
//!
//! ```text
//!   BYTEASK_PERF_LAB=ssh:bench-host               # runner at /opt/perflab/bin/perf-runner there
//!   BYTEASK_PERF_LAB=ssh:bench-host:/path/perf-runner
//!   BYTEASK_PERF_LAB=local                        # this machine is the bench host
//!   BYTEASK_PERF_LAB=local:/path/perf-runner
//! ```
//!
//! Flow: resolve base and candidate to commits (`WORKTREE` = the working tree
//! as it is now, through `git stash create`, which touches neither the index
//! nor the files) -> over ssh, push just those commits into a mirror on the
//! bench host -> `perf-runner compare --json -` there (it builds both, refuses
//! a mis-tuned host, measures interleaved rounds on the isolated core) ->
//! render the verdict. The whole JSON report is saved locally.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

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
use serde_json::Value;

use check_result::CheckResult;
use check_result::NotRunReason;

use crate::run::RunOutcome;
use crate::run::run_capped;

pub(crate) const TOOL_NAME: &str = "cpp_perf_lab";
const LAB_ENV: &str = "BYTEASK_PERF_LAB";
const DEFAULT_RUNNER: &str = "/opt/perflab/bin/perf-runner";
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// Where the lab is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabTarget {
    Local { runner: String },
    Ssh { host: String, runner: String },
}

impl LabTarget {
    /// Parse `BYTEASK_PERF_LAB`. `None` for unset/empty/unknown forms.
    pub fn parse(v: &str) -> Option<LabTarget> {
        let v = v.trim();
        if v == "local" {
            return Some(LabTarget::Local {
                runner: DEFAULT_RUNNER.to_string(),
            });
        }
        if let Some(r) = v.strip_prefix("local:") {
            return (!r.is_empty()).then(|| LabTarget::Local {
                runner: r.to_string(),
            });
        }
        let rest = v.strip_prefix("ssh:")?;
        let (host, runner) = match rest.split_once(':') {
            Some((h, r)) if !r.is_empty() => (h, r.to_string()),
            _ => (rest, DEFAULT_RUNNER.to_string()),
        };
        // A host is an ssh destination, never an option or a shell word.
        if host.is_empty()
            || host.starts_with('-')
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "@._-".contains(c))
        {
            return None;
        }
        Some(LabTarget::Ssh {
            host: host.to_string(),
            runner,
        })
    }

    pub fn from_env() -> Option<LabTarget> {
        std::env::var(LAB_ENV)
            .ok()
            .and_then(|v| LabTarget::parse(&v))
    }

    fn describe(&self) -> String {
        match self {
            LabTarget::Local { .. } => "this machine".to_string(),
            LabTarget::Ssh { host, .. } => host.clone(),
        }
    }
}

pub struct PerfLabTool {
    pub cwd: PathBuf,
    pub native_exec: bool,
    pub lab: LabTarget,
}

/// The tool when a lab is configured, else none: with no lab the tool could
/// only ever answer NOT RUN, and an unconfigured session's tool list (and the
/// pinned description snapshot of every other tool) stays exactly as it was.
pub fn perf_lab_tool(cwd: &Path, native_exec: bool, lab: Option<LabTarget>) -> Option<PerfLabTool> {
    lab.map(|lab| PerfLabTool {
        cwd: cwd.to_path_buf(),
        native_exec,
        lab,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabArgs {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    candidate: Option<String>,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    bench: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    rounds: Option<u32>,
    #[serde(default)]
    floor_pct: Option<f64>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    primary: Option<String>,
    #[serde(default)]
    guards: Option<Vec<String>>,
    #[serde(default)]
    check: Option<String>,
    #[serde(default)]
    tma: Option<bool>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for PerfLabTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        spec()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

/// The model-facing spec. A new tool, so a new snapshot:
/// `src/snapshots/cpp_perf_lab.json`.
pub(crate) fn spec() -> ToolSpec {
    let mut p = BTreeMap::new();
    p.insert(
        "action".to_string(),
        JsonSchema::string(Some(
            "\"compare\" (default): base vs candidate. \"aa\": the base against itself, to show the lab's noise \
             (must answer NO CLAIM). \"preflight\": only check the bench host is fit to measure."
                .to_string(),
        )),
    );
    p.insert(
        "base".to_string(),
        JsonSchema::string(Some(
            "Git ref to compare against (default \"HEAD\").".to_string(),
        )),
    );
    p.insert(
        "candidate".to_string(),
        JsonSchema::string(Some(
            "Git ref of the change, or \"WORKTREE\" (default) for the working tree as it is now, \
             uncommitted edits included (untracked files are not)."
                .to_string(),
        )),
    );
    p.insert(
        "build".to_string(),
        JsonSchema::string(Some(
            "Shell command that builds the benchmark, run in each ref's own checkout on the bench host, \
             e.g. \"cmake --preset prod && cmake --build --preset prod --target bench\". Required for compare and aa."
                .to_string(),
        )),
    );
    p.insert(
        "bench".to_string(),
        JsonSchema::string(Some(
            "Benchmark command relative to the checkout root, e.g. \"build/prod/bench --feed /data/feed.cap\" \
             (run directly, not through a shell). For tail latency it should print one line \
             `PERFLAB {\"p50_ns\": …, \"p99_ns\": …, \"p999_ns\": …, \"throughput\": …}`; for TMA of its hot loop \
             it should accept `--perf-ctl <ctl-fifo> <ack-fifo>` and write enable/disable around the loop. \
             Required for compare and aa."
                .to_string(),
        )),
    );
    p.insert(
        "rounds".to_string(),
        JsonSchema::integer(Some(
            "Interleaved A/B rounds (default 12; at least 6, or 8 at 0.99 confidence).".to_string(),
        )),
    );
    p.insert(
        "floor_pct".to_string(),
        JsonSchema::number(Some(
            "Minimum effect in percent: a smaller change is never FASTER/SLOWER (default 2)."
                .to_string(),
        )),
    );
    p.insert(
        "confidence".to_string(),
        JsonSchema::number(Some(
            "Confidence level (default 0.95; use 0.99 for a merge decision).".to_string(),
        )),
    );
    p.insert(
        "primary".to_string(),
        JsonSchema::string(Some(
            "Metric that sets the verdict (default p50_ns when the benchmark reports it)."
                .to_string(),
        )),
    );
    p.insert(
        "guards".to_string(),
        JsonSchema::array(
            JsonSchema::string(None),
            Some("Metrics that must not increase, e.g. [\"allocs_per_msg\"]; an increase is REJECTED.".to_string()),
        ),
    );
    p.insert(
        "check".to_string(),
        JsonSchema::string(Some(
            "Shell command run in both checkouts whose output must be identical (e.g. a replay test); a \
             difference is REJECTED with the first differing line."
                .to_string(),
        )),
    );
    p.insert(
        "tma".to_string(),
        JsonSchema::boolean(Some(
            "Top-down (TMA) level 1+2 of the hot loop before and after (default true).".to_string(),
        )),
    );
    p.insert(
        "timeout_sec".to_string(),
        JsonSchema::integer(Some(format!(
            "Cap for the whole call, builds included (default {DEFAULT_TIMEOUT_SECS})."
        ))),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Measure a C/C++ change on the perf lab: builds base and candidate on a dedicated bench \
            host, refuses to measure if the host is mis-tuned (frequency, isolation, interrupts, SMT \
            sibling, other load), runs interleaved A/B rounds pinned to an isolated core, and answers \
            FASTER / SLOWER / NO CLAIM / MIXED / REJECTED with a confidence interval, p50/p99/p99.9 and \
            throughput, plus TMA of the hot loop. Use it before claiming a speedup or approving a \
            performance-sensitive change; NO CLAIM means the difference is inside the noise, not a win."
            .to_string(),
        strict: false,
        parameters: JsonSchema::object(p, None, Some(AdditionalProperties::from(false))),
        output_schema: None,
        defer_loading: None,
    })
}

/// Single-quote for a POSIX shell (ssh hands the remote command to one).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Mirror path on the bench host for a local repository: stable per repo.
fn mirror_name(top: &Path) -> String {
    let name: String = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // FNV-1a of the absolute path: two checkouts named the same never share a mirror.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in top.to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("perflab-repos/{name}-{:08x}.git", h as u32)
}

/// The runner's argument list for an action.
fn runner_args(
    action: &str,
    repo: &str,
    base: &str,
    cand: &str,
    a: &LabArgs,
) -> Result<Vec<String>, String> {
    if action == "preflight" {
        return Ok(vec!["preflight".to_string(), "--json".to_string()]);
    }
    let build = a
        .build
        .clone()
        .ok_or("`build` is required: the shell command that builds the benchmark in a checkout")?;
    let bench = a
        .bench
        .clone()
        .ok_or("`bench` is required: the benchmark command, relative to the checkout root")?;
    let mut v: Vec<String> = vec![action.to_string(), "--repo".into(), repo.to_string()];
    if action == "aa" {
        v.extend(["--ref".into(), base.to_string()]);
    } else {
        v.extend([
            "--base".into(),
            base.to_string(),
            "--cand".into(),
            cand.to_string(),
        ]);
    }
    v.extend([
        "--build".into(),
        build,
        "--bench".into(),
        bench,
        "--json".into(),
        "-".into(),
    ]);
    if let Some(r) = a.rounds {
        v.extend(["--rounds".into(), r.to_string()]);
    }
    if let Some(f) = a.floor_pct {
        v.extend(["--floor".into(), f.to_string()]);
    }
    if let Some(c) = a.confidence {
        v.extend(["--confidence".into(), c.to_string()]);
    }
    if let Some(p) = &a.primary {
        v.extend(["--primary".into(), p.clone()]);
    }
    for g in a.guards.iter().flatten() {
        v.extend(["--guard".into(), g.clone()]);
    }
    if let Some(c) = &a.check {
        v.extend(["--check".into(), c.clone()]);
    }
    if a.tma == Some(false) {
        v.push("--no-tma".into());
    }
    Ok(v)
}

impl PerfLabTool {
    async fn git(&self, top: &Path, args: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = ["-C".to_string(), top.to_string_lossy().into_owned()]
            .into_iter()
            .chain(args.iter().map(|s| s.to_string()))
            .collect();
        match run_capped("git", &argv, &self.cwd, Duration::from_secs(120)).await {
            Ok(RunOutcome::Finished {
                success: true,
                stdout,
                ..
            }) => Ok(stdout.trim().to_string()),
            Ok(RunOutcome::Finished { stderr, .. }) => {
                Err(format!("git {}: {}", args.join(" "), stderr.trim()))
            }
            Ok(RunOutcome::TimedOut { .. }) => Err(format!("git {} timed out", args.join(" "))),
            Err(e) => Err(format!("git: {e:?}")),
        }
    }

    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: LabArgs = serde_json::from_str(arguments)
            .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
        let (body, result) = match self.run(&args).await {
            Ok(done) => done,
            Err(e) => (
                format!("NOT RUN: {e}"),
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::ServiceUnavailable,
                    "The perf lab could not be reached or gave no report, so nothing was measured.",
                    "check the lab with action \"preflight\", then call again.",
                ),
            ),
        };
        Ok(crate::report::report_plain(TOOL_NAME, body, result))
    }

    async fn run(&self, args: &LabArgs) -> Result<(String, CheckResult), String> {
        let action = args.action.as_deref().unwrap_or("compare");
        if !matches!(action, "compare" | "aa" | "preflight") {
            return Err(format!(
                "unknown action {action:?}; expected \"compare\", \"aa\" or \"preflight\""
            ));
        }
        let timeout = Duration::from_secs(args.timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        let mut notes = Vec::new();
        let (mut repo_arg, mut base_sha, mut cand_sha) =
            (String::new(), String::new(), String::new());
        if action != "preflight" {
            let top = PathBuf::from(
                self.git(&self.cwd, &["rev-parse", "--show-toplevel"])
                    .await
                    .map_err(|e| format!("not in a git repository ({e})"))?,
            );
            let base = args.base.as_deref().unwrap_or("HEAD");
            base_sha = self
                .git(
                    &top,
                    &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
                )
                .await?;
            let cand = args.candidate.as_deref().unwrap_or("WORKTREE");
            cand_sha = if cand == "WORKTREE" {
                let stash = self.git(&top, &["stash", "create"]).await?;
                if stash.is_empty() {
                    notes.push(
                        "the working tree has no uncommitted changes: candidate = HEAD".to_string(),
                    );
                    self.git(&top, &["rev-parse", "HEAD"]).await?
                } else {
                    notes.push(format!(
                        "candidate = the working tree as it is now (snapshot commit {})",
                        &stash[..stash.len().min(12)]
                    ));
                    stash
                }
            } else {
                self.git(
                    &top,
                    &["rev-parse", "--verify", &format!("{cand}^{{commit}}")],
                )
                .await?
            };
            let untracked = self
                .git(&top, &["ls-files", "--others", "--exclude-standard"])
                .await
                .unwrap_or_default();
            let n = untracked.lines().filter(|l| !l.is_empty()).count();
            if n > 0 && cand == "WORKTREE" {
                notes.push(format!("{n} untracked file(s) are NOT in the candidate; `git add` them if the build needs them"));
            }
            repo_arg = match &self.lab {
                LabTarget::Local { .. } => top.to_string_lossy().into_owned(),
                LabTarget::Ssh { host, .. } => {
                    let mirror = mirror_name(&top);
                    self.ssh(host, &format!("mkdir -p perflab-repos && git init -q --bare {} >/dev/null 2>&1 || true", sq(&mirror)), Duration::from_secs(60)).await?;
                    let url = format!("{host}:{mirror}");
                    let refs: Vec<String> = [&base_sha, &cand_sha]
                        .iter()
                        .map(|s| format!("{s}:refs/perflab/{s}"))
                        .collect();
                    let mut push = vec!["push", "-q", "--force", url.as_str()];
                    push.extend(refs.iter().map(String::as_str));
                    self.git(&top, &push).await.map_err(|e| {
                        format!("could not send the commits to the bench host: {e}")
                    })?;
                    mirror
                }
            };
        }
        let rargs = runner_args(action, &repo_arg, &base_sha, &cand_sha, args)?;
        let (runner, out) = match &self.lab {
            LabTarget::Local { runner } => (
                runner.clone(),
                run_capped(runner, &rargs, &self.cwd, timeout)
                    .await
                    .map_err(|e| format!("{runner}: {e:?}"))?,
            ),
            LabTarget::Ssh { host, runner } => {
                let cmd = std::iter::once(sq(runner))
                    .chain(rargs.iter().map(|a| sq(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                (runner.clone(), self.ssh_raw(host, &cmd, timeout).await?)
            }
        };
        let (stdout, stderr) = match out {
            RunOutcome::Finished { stdout, stderr, .. } => (stdout, stderr),
            RunOutcome::TimedOut { secs } => {
                return Err(format!(
                    "the perf lab did not answer within {secs}s (builds and rounds included); raise timeout_sec or shorten the benchmark"
                ));
            }
        };
        let json: Value = serde_json::from_str(stdout.trim()).map_err(|_| {
            let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
            format!(
                "{runner} on {} gave no report. Last lines: {}",
                self.lab.describe(),
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            )
        })?;
        let saved = save_report(&json);
        Ok((
            render(action, &self.lab.describe(), &json, &notes, saved.as_deref()),
            lab_result(action, &json),
        ))
    }

    async fn ssh_raw(
        &self,
        host: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<RunOutcome, String> {
        let argv = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=20".to_string(),
            host.to_string(),
            cmd.to_string(),
        ];
        run_capped("ssh", &argv, &self.cwd, timeout)
            .await
            .map_err(|e| format!("ssh {host}: {e:?}"))
    }

    async fn ssh(&self, host: &str, cmd: &str, timeout: Duration) -> Result<String, String> {
        match self.ssh_raw(host, cmd, timeout).await? {
            RunOutcome::Finished {
                success: true,
                stdout,
                ..
            } => Ok(stdout),
            RunOutcome::Finished { stderr, .. } => Err(format!("ssh {host}: {}", stderr.trim())),
            RunOutcome::TimedOut { secs } => Err(format!("ssh {host} timed out after {secs}s")),
        }
    }
}

fn save_report(json: &Value) -> Option<String> {
    let dir = std::env::temp_dir().join("byteask-perf-lab");
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let p = dir.join(format!("report-{stamp}.json"));
    std::fs::write(&p, serde_json::to_string_pretty(json).ok()?).ok()?;
    Some(p.to_string_lossy().into_owned())
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn fmt(x: f64) -> String {
    let a = x.abs();
    if a >= 1e6 {
        format!("{:.3}M", x / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", x / 1e3)
    } else if a >= 100.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.3}")
    }
}

/// The shared result for the runner's JSON, the way `cpp_perf` maps its own verdicts: a
/// significant measured difference (FASTER, SLOWER, MIXED) is a measurement that worked (PASS,
/// the summary says which way); inside the noise is NO CLAIM; changed output or a guard metric
/// that went up is FAIL; a run that crashed or a host or build that refused is NOT RUN.
pub(crate) fn lab_result(action: &str, j: &Value) -> CheckResult {
    if action == "preflight" {
        return if j["ok"].as_bool().unwrap_or(false) {
            CheckResult::pass(TOOL_NAME, "The perf lab is fit to measure.")
        } else {
            CheckResult::not_run(
                TOOL_NAME,
                NotRunReason::ServiceUnavailable,
                "The perf lab refused: the host is not fit to measure.",
                "apply the fixes the failing host checks name, then run preflight again.",
            )
        };
    }
    let reason = j["verdict_reason"].as_str().unwrap_or("").trim();
    let summary = |prefix: &str| {
        let line = format!("{prefix}: {reason}");
        line.lines().next().unwrap_or_default().to_string()
    };
    match j["verdict"].as_str().unwrap_or("NOT_RUN") {
        "FASTER" => CheckResult::pass(TOOL_NAME, &summary("Measured, the candidate is faster")),
        "SLOWER" => CheckResult::pass(TOOL_NAME, &summary("Measured, the candidate is slower")),
        "MIXED" => CheckResult::pass(TOOL_NAME, &summary("Measured, better on one metric and worse on another")),
        "NO_CLAIM" => CheckResult::no_claim(TOOL_NAME, &summary("Inside the noise")),
        "REJECTED" => CheckResult::fail(TOOL_NAME, &summary("Rejected")),
        "INVALID" => CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::ToolError,
            &summary("A benchmark run failed, so nothing was measured"),
            "fix the failing run (exit status or crash in the report), then measure again.",
        ),
        _ => {
            let build_failed = ["a", "b"]
                .iter()
                .any(|side| j[side]["build"]["ok"].as_bool() == Some(false));
            if build_failed {
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::BuildFailed,
                    &summary("A side did not build, so nothing was measured"),
                    "fix the build (its log tail is in the report), then measure again.",
                )
            } else {
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::ServiceUnavailable,
                    &summary("Not measured"),
                    "apply the fix the report names (host checks or refs), then measure again.",
                )
            }
        }
    }
}

/// Render the runner's JSON for the model. Pure; fixture-tested.
pub(crate) fn render(
    action: &str,
    lab: &str,
    j: &Value,
    notes: &[String],
    saved: Option<&str>,
) -> String {
    let mut o = String::new();
    if action == "preflight" {
        let ok = j["ok"].as_bool().unwrap_or(false);
        o.push_str(&format!(
            "Perf lab on {lab}: {}\n",
            if ok {
                "fit to measure"
            } else {
                "REFUSED: not fit to measure"
            }
        ));
        for c in j["checks"].as_array().into_iter().flatten() {
            let st = c["status"].as_str().unwrap_or("?");
            if st == "FAIL" || st == "WARN" {
                o.push_str(&format!(
                    "  {st} {}: {} (fix: {})\n",
                    c["name"].as_str().unwrap_or("?"),
                    c["detail"].as_str().unwrap_or(""),
                    c["fix"].as_str().unwrap_or("-")
                ));
            }
        }
        let n = j["checks"].as_array().map_or(0, |a| a.len());
        let pass = j["checks"]
            .as_array()
            .map_or(0, |a| a.iter().filter(|c| c["status"] == "PASS").count());
        o.push_str(&format!("  {pass}/{n} checks PASS\n"));
        return o;
    }
    let verdict = j["verdict"].as_str().unwrap_or("NOT_RUN").replace('_', " ");
    let host = &j["host"];
    o.push_str(&format!(
        "Perf lab on {lab} (cpu {} isolated, {}): {verdict}\n",
        host["measured_cpu"]
            .as_u64()
            .map_or("?".to_string(), |c| c.to_string()),
        host["freq_ghz"]
            .as_f64()
            .map_or("frequency unknown".to_string(), |f| format!(
                "{f:.2} GHz pinned"
            )),
    ));
    o.push_str(&format!("{}\n", j["verdict_reason"].as_str().unwrap_or("")));
    let metrics = j["metrics"].as_array().cloned().unwrap_or_default();
    if !metrics.is_empty() {
        o.push_str(
            "  metric        base         candidate    change    CI               verdict\n",
        );
        for m in &metrics {
            let name = m["name"].as_str().unwrap_or("?");
            let unit = m["unit"].as_str().unwrap_or("");
            let ci = match (num(&m["ci"][0]), num(&m["ci"][1])) {
                (Some(a), Some(b)) => {
                    format!("{:+.1}%..{:+.1}%", (a - 1.0) * 100.0, (b - 1.0) * 100.0)
                }
                _ => "-".to_string(),
            };
            o.push_str(&format!(
                "  {:<13} {:<12} {:<12} {:<9} {:<16} {}\n",
                name,
                format!(
                    "{} {unit}",
                    num(&m["a_median"]).map(fmt).unwrap_or("-".into())
                )
                .trim(),
                format!(
                    "{} {unit}",
                    num(&m["b_median"]).map(fmt).unwrap_or("-".into())
                )
                .trim(),
                num(&m["change_pct"]).map_or("-".to_string(), |c| format!("{c:+.1}%")),
                ci,
                m["verdict"].as_str().unwrap_or("").replace('_', " ")
            ));
        }
    }
    for line in j["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .skip(2)
        .filter_map(|l| l.as_str())
    {
        o.push_str(&format!("{line}\n"));
    }
    if j["verdict"] == "NOT_RUN" {
        for c in j["preflight"]["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["status"] == "FAIL")
        {
            o.push_str(&format!(
                "  host check FAIL {}: {} (fix: {})\n",
                c["name"].as_str().unwrap_or("?"),
                c["detail"].as_str().unwrap_or(""),
                c["fix"].as_str().unwrap_or("-")
            ));
        }
    }
    for n in notes.iter().map(String::as_str).chain(
        j["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str()),
    ) {
        o.push_str(&format!("note: {n}\n"));
    }
    if matches!(j["verdict"].as_str(), Some("NO_CLAIM")) {
        o.push_str("NO CLAIM: do not report a speedup or a slowdown from this measurement.\n");
    }
    if let Some(p) = saved {
        o.push_str(&format!("full report: {p}\n"));
    }
    o
}

#[cfg(test)]
#[path = "perf_lab_tests.rs"]
mod tests;
