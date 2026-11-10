//! `cpp_test_select`: build and run only the tests a change can affect.
//!
//! See [`crate::testsel`] for how tests are selected. This module is the tool:
//! arguments, the plan/run pipeline, background runs and the model-facing
//! text (plan ≤ ~600 tokens, a run ≤ ~1,000 tokens; the full log stays on
//! disk). Trust-gated: it configures and builds the project, which runs the
//! project's own build scripts.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
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

use crate::base_run_cache::TestStatus;
use crate::session_head::Head;
use crate::session_head::SessionHead;
use crate::testsel::DirKind;
use crate::testsel::Model;
use crate::testsel::System;
use crate::testsel::base;
use crate::testsel::cmake;
use crate::testsel::exec;
use crate::testsel::exec::Ctx;
use crate::testsel::exec::RunItem;
use crate::testsel::jobs::Jobs;
use crate::testsel::jobs::Waited;
use crate::testsel::make;
use crate::testsel::meson;
use crate::testsel::select;
use crate::testsel::select::Selection;
use crate::testsel::symbols;
use crate::testsel::symbols::Changes;
use crate::testsel::tree;

const TOOL_NAME: &str = "cpp_test_select";
const DEFAULT_WAIT_S: u64 = 600;
const MAX_WAIT_S: u64 = 3600;

/// A whole number of seconds from the environment (1..=3600), else `default`.
fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| (1..=3600).contains(secs))
        .unwrap_or(default)
}

/// `BYTEASK_TEST_SELECT_WAIT_S`: how long a call blocks for its run by default.
fn default_wait_s() -> u64 {
    env_secs("BYTEASK_TEST_SELECT_WAIT_S", DEFAULT_WAIT_S)
}

/// `BYTEASK_TEST_SELECT_MAX_WAIT_S`: the most a call blocks, whatever it asks.
fn max_wait_s() -> u64 {
    env_secs("BYTEASK_TEST_SELECT_MAX_WAIT_S", MAX_WAIT_S)
}

/// `BYTEASK_TEST_SELECT_WARM_YIELD`: a run stops a warm build still going in its tree.
fn warm_yield() -> bool {
    std::env::var("BYTEASK_TEST_SELECT_WARM_YIELD")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
}
const DEFAULT_TEST_TIMEOUT_S: u64 = 120;
/// Test names listed per tier line.
const NAMES_SHOWN: usize = 8;
/// Failures shown in a run.
const FAILURES_SHOWN: usize = 10;
/// Hard cap on the model-facing text (≈ 1,000 tokens).
const OUTPUT_CAP: usize = 4200;
/// Targets named on the BUILD line.
const TARGETS_SHOWN: usize = 30;
/// The run line when the build failed without naming the failing target: no test ran.
const NOT_RUN_BUILD_FAILED: &str = "RUN: not run — the build FAILED, so the test programs could be \
    from an older build; fix the build first";
/// The run line when the build hit its time cap: no test ran.
const NOT_RUN_TIME_CAP: &str = "RUN: not run — the build hit its time cap";

pub struct TestSelectTool {
    pub cwd: PathBuf,
    /// Configuring and building run the project's own scripts.
    pub native_exec: bool,
    pub head: SessionHead,
    pub jobs: Arc<Jobs>,
    /// This thread's finish gate builds everything and runs the whole suite when the model
    /// finishes, so the tool does not suggest a full-suite run of its own.
    pub finish_gate_suite: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    action: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    tier: Option<u32>,
    #[serde(default)]
    full_suite: bool,
    #[serde(default)]
    changed: Option<Vec<String>>,
    #[serde(default)]
    symbols: Option<Vec<String>>,
    #[serde(default)]
    build_dir: Option<String>,
    #[serde(default)]
    compare_base: bool,
    #[serde(default)]
    background: bool,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    wait_s: Option<u64>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_per_test: Option<u64>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    jobs: Option<u32>,
    #[serde(default, deserialize_with = "crate::args::whole_u32")]
    run_id: Option<u32>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for TestSelectTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut p = BTreeMap::new();
        p.insert(
            "action".to_string(),
            JsonSchema::string(Some(
                "\"run\" (default): build the selected tests' programs and run them. \"plan\": only \
                 show what each tier would build and run. \"wait\": block until a background run \
                 finishes (use only when you have nothing else to do). \"status\": list runs. \
                 \"cancel\": stop the running one."
                    .to_string(),
            )),
        );
        p.insert(
            "tier".to_string(),
            JsonSchema::number(Some(
                "1 (default): only the test cases that mention the changed functions/types \
                 (fastest; picked by name, so not safe). 2: the whole test programs that mention \
                 them. 3: every test the build graph says the change reaches, plus tests it cannot \
                 map (script/data tests) — the safe superset."
                    .to_string(),
            )),
        );
        p.insert(
            "full_suite".to_string(),
            JsonSchema::boolean(Some(
                "Build everything and run the whole suite (once, at the end, when no finish gate runs it). Default false."
                    .to_string(),
            )),
        );
        p.insert(
            "changed".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Files to treat as changed, relative to the repository root. Default: the \
                     working tree's diff against the session-start commit plus new source files."
                        .to_string(),
                ),
            ),
        );
        p.insert(
            "symbols".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Function/type/macro names to select tests by. Default: the ones the diff \
                     changes."
                        .to_string(),
                ),
            ),
        );
        p.insert(
            "build_dir".to_string(),
            JsonSchema::string(Some(
                "A configured build directory to use instead (CMake, Meson or Make); it is built \
                 incrementally and never reconfigured or deleted. Default for CMake: the tool's \
                 own tree outside the project, configured once with the options of your build \
                 tree (yours is never built in)."
                    .to_string(),
            )),
        );
        p.insert(
            "compare_base".to_string(),
            JsonSchema::boolean(Some(
                "Also build and run the same selection on the session-start commit (in a tree of \
                 its own; cached for the session) and report the tests whose status flipped, plus \
                 untouched neighbouring cases that exercise the same names. Default false."
                    .to_string(),
            )),
        );
        p.insert(
            "background".to_string(),
            JsonSchema::boolean(Some(
                "Return at once and keep running; the result is shown to you when it finishes. \
                 Default false."
                    .to_string(),
            )),
        );
        p.insert(
            "wait_s".to_string(),
            JsonSchema::number(Some(format!(
                "How long this call waits for the run (default {}, max \
                 {}). A run still going after that continues in the background and its \
                 result is shown to you automatically when it ends; do not poll.",
                default_wait_s(),
                max_wait_s()
            ))),
        );
        p.insert(
            "timeout_per_test".to_string(),
            JsonSchema::number(Some(format!(
                "Seconds before one test is killed (default {DEFAULT_TEST_TIMEOUT_S})."
            ))),
        );
        p.insert(
            "jobs".to_string(),
            JsonSchema::number(Some(
                "Parallel build/test jobs. Default and maximum: the CPUs available, capped by available memory (about 1.5 GB per job)."
                    .to_string(),
            )),
        );
        p.insert(
            "run_id".to_string(),
            JsonSchema::number(Some(
                "For wait/cancel: which run (default: the latest).".to_string(),
            )),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Build and run only the tests your change can affect, using the \
                project's own build system (CMake File API and CTest, Meson introspection, Make \
                dry runs). Builds only their programs, incrementally, in a tree of its own \
                outside the project (CMake; configured once, already warm when a background \
                build ran), states the exact targets built, the test filter and `git diff \
                --check`, and returns failures with their first assertion plus a summary; the \
                full log stays on disk. Use it after \
                an edit instead of rebuilding and re-running the whole suite; the whole suite runs \
                once, at the end (in the finish gate when one runs it, else full_suite=true). Long \
                runs continue in the background and \
                report when done. C/C++ projects; it runs the project's build, so it needs a \
                trusted project."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(p, None, Some(AdditionalProperties::from(false))),
            output_schema: None,
            defer_loading: None,
        })
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

impl TestSelectTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: Args = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let action = args.action.clone().unwrap_or_else(|| "run".to_string());
        let wait = Duration::from_secs(args.wait_s.unwrap_or(default_wait_s()).min(max_wait_s()));
        let text = match action.as_str() {
            "status" => {
                let lines = self.jobs.summary();
                if lines.is_empty() { "No runs yet.".to_string() } else { lines.join("\n") }
            }
            "wait" => match self.jobs.wait(args.run_id, wait).await {
                Some(Waited::Done(text)) => text,
                Some(running) => still_running(&running),
                None => "No run to wait for.".to_string(),
            },
            "cancel" => match self.jobs.cancel(args.run_id) {
                Some(id) => format!("Run {id} is being cancelled; its build/test processes are killed."),
                None => "No run in progress.".to_string(),
            },
            "plan" | "run" => {
                crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
                if action == "run" {
                    if let Some((id, label, secs, progress)) = self.jobs.running() {
                        return Ok(crate::report::report_plain(
                            TOOL_NAME,
                            format!(
                                "Run {id} ({label}) is still going ({secs}s, {progress}). Its result \
                                 will be shown to you when it finishes; call action=\"wait\" to block \
                                 until then, or action=\"cancel\" to stop it."
                            ),
                            check_result::CheckResult::not_run(
                                TOOL_NAME,
                                check_result::NotRunReason::Skipped,
                                "Another run is still going, so no new run was started.",
                                "call action=\"wait\" for it, or action=\"cancel\" to stop it.",
                            ),
                        ));
                    }
                }
                let mut request = Request::new(&self.cwd, self.head.get().await, &args, action == "plan");
                request.finish_gate_suite = self.finish_gate_suite;
                if action == "plan" {
                    pipeline(request, None).await
                } else {
                    let label = request.label();
                    self.jobs.start(label, move |cancel, progress| async move {
                        let ctx = Ctx {
                            log: request.log_path(),
                            jobs: request.jobs,
                            test_timeout: request.test_timeout,
                            cancel,
                            progress,
                        };
                        pipeline(request, Some(ctx)).await
                    });
                    let wait = if args.background { Duration::ZERO } else { wait };
                    match self.jobs.wait(None, wait).await {
                        Some(Waited::Done(text)) => text,
                        Some(running) => still_running(&running),
                        None => "The run could not be started.".to_string(),
                    }
                }
            }
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unknown action {other:?}: use run, plan, wait, status or cancel"
                )));
            }
        };
        let result = test_select_result(&action, &text);
        Ok(crate::report::report(TOOL_NAME, &action, cap(text), result))
    }
}

/// The shared result of one call, from the text this tool itself wrote: a run whose
/// summary line names a failure is FAIL; tests that were not run make it APPROXIMATE; a run
/// that did not happen or has not finished is NOT RUN; a plan, a status or a cancel judges
/// nothing.
fn test_select_result(action: &str, text: &str) -> check_result::CheckResult {
    use check_result::CheckResult;
    use check_result::NotRunReason;
    if text.contains("status: DID NOT RUN") {
        let next = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("next: "))
            .unwrap_or("");
        let reason = if text.contains("configure the project") {
            NotRunReason::NoBuildProfile
        } else {
            NotRunReason::ToolError
        };
        return CheckResult::not_run(TOOL_NAME, reason, "No tests were run.", next);
    }
    if matches!(action, "status" | "cancel" | "plan") {
        return CheckResult::no_claim(TOOL_NAME, "Nothing was run by this call.");
    }
    if text.contains("is still going after") {
        return CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::TimeLimit,
            "The run had not finished when this call stopped waiting.",
            "carry on; its result is shown when it finishes, or call action=\"wait\".",
        );
    }
    if text.lines().any(|line| line.starts_with(NOT_RUN_BUILD_FAILED)) {
        return CheckResult::fail(TOOL_NAME, "The build failed, so no test was run.");
    }
    if text.lines().any(|line| line.starts_with(NOT_RUN_TIME_CAP)) {
        return CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::TimeLimit,
            "The build did not finish within its time cap, so no test was run.",
            "build the project yourself (it may take long), then run the tests again.",
        );
    }
    let Some(run) = text.lines().find(|line| line.starts_with("RUN ")) else {
        return CheckResult::no_claim(TOOL_NAME, "No test run result is in this answer.");
    };
    if run.contains(": 0 result(s)") {
        return CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::NoBuildProfile,
            "No tests ran: the selected tests produced no result.",
            "try a higher tier or full_suite=true, or check that the tests are registered in this build tree.",
        );
    }
    if run.contains("FAILED") || run.contains("NOT BUILT") {
        CheckResult::fail(TOOL_NAME, run.trim())
    } else if run.contains("not run") {
        CheckResult::approximate(TOOL_NAME, run.trim())
    } else {
        CheckResult::pass(TOOL_NAME, run.trim())
    }
}

#[cfg(test)]
mod result_tests {
    use check_result::Verdict;

    use super::test_select_result;

    #[test]
    fn a_run_is_judged_by_its_own_summary_line() {
        let run = |line: &str| test_select_result("run", &format!("header\n{line}\nnext: x"));
        assert_eq!(run("RUN tier 1: 3 result(s) in 1.0s — 3 passed").verdict, Verdict::Pass);
        assert_eq!(run("RUN tier 1: 3 result(s) in 1.0s — 2 passed, 1 FAILED").verdict, Verdict::Fail);
        assert_eq!(run("RUN tier 1: 3 result(s) in 1.0s — 2 passed, 1 NOT BUILT").verdict, Verdict::Fail);
        assert_eq!(run("RUN tier 1: 3 result(s) in 1.0s — 2 passed, 1 not run").verdict, Verdict::Approximate);
        // Zero tests is never a pass.
        assert_eq!(run("RUN tier 1: 0 result(s) in 0.1s — 0 passed").verdict, Verdict::NotRun);
        // A failed build never falls through to a PASS from old test programs.
        let failed_build = test_select_result(
            "run",
            &format!("BUILD everything: FAILED in 3s\n{}\nlog: x", super::NOT_RUN_BUILD_FAILED),
        );
        assert_eq!(failed_build.verdict, Verdict::Fail);
        let timed_out = test_select_result("run", &format!("{}\nlog: x", super::NOT_RUN_TIME_CAP));
        assert_eq!(timed_out.verdict, Verdict::NotRun);
        assert_eq!(timed_out.reason, Some(check_result::NotRunReason::TimeLimit));
        let not_run = test_select_result(
            "run",
            &crate::toolver::did_not_run("x", "configure the project (cmake)", ""),
        );
        assert_eq!(not_run.verdict, Verdict::NotRun);
        assert!(not_run.validate().is_ok());
        assert_eq!(test_select_result("plan", "plan").verdict, Verdict::NoClaim);
        assert_eq!(
            test_select_result("wait", "Run 1 (x) is still going after 3s (y); ...").verdict,
            Verdict::NotRun
        );
    }
}

fn still_running(waited: &Waited) -> String {
    match waited {
        Waited::Running { id, label, secs, progress } => format!(
            "Run {id} ({label}) is still going after {secs}s ({progress}); it continues in the \
             background and its result will be shown to you automatically when it finishes. Do \
             not poll: carry on, or call action=\"wait\" if there is nothing else to do."
        ),
        Waited::Done(text) => text.clone(),
    }
}

fn cap(text: String) -> String {
    if text.len() <= OUTPUT_CAP {
        return text;
    }
    let mut end = OUTPUT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (output capped; the full log path is above)", &text[..end])
}

/// Everything one plan/run needs, owned (runs outlive the call).
struct Request {
    cwd: PathBuf,
    head: Option<Head>,
    plan_only: bool,
    tier: u32,
    full: bool,
    changed: Option<Vec<String>>,
    symbols: Option<Vec<String>>,
    build_dir: Option<String>,
    compare_base: bool,
    jobs: usize,
    test_timeout: Duration,
    finish_gate_suite: bool,
}

impl Request {
    fn new(cwd: &Path, head: Option<Head>, args: &Args, plan_only: bool) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            head,
            plan_only,
            tier: args.tier.unwrap_or(1).clamp(1, 3),
            full: args.full_suite,
            changed: args.changed.clone(),
            symbols: args.symbols.clone(),
            build_dir: args.build_dir.clone(),
            compare_base: args.compare_base,
            // Memory-aware; an explicit `jobs` is honoured only up to what memory allows.
            jobs: args
                .jobs
                .map(|j| (j.max(1) as usize).min(exec::default_jobs()))
                .unwrap_or_else(exec::default_jobs),
            test_timeout: Duration::from_secs(args.timeout_per_test.unwrap_or(DEFAULT_TEST_TIMEOUT_S).max(1)),
            finish_gate_suite: false,
        }
    }

    fn root(&self) -> PathBuf {
        self.head.as_ref().map(|h| h.root.clone()).unwrap_or_else(|| self.cwd.clone())
    }

    fn label(&self) -> String {
        let what = if self.full { "full suite".to_string() } else { format!("tier {}", self.tier) };
        if self.compare_base { format!("{what} + base comparison") } else { what }
    }

    fn scratch(&self) -> PathBuf {
        scratch_dir(&self.root())
    }

    fn log_path(&self) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        self.scratch().join("logs").join(format!("run-{stamp}.log"))
    }
}

pub(crate) use crate::testsel::tree::scratch_dir;

/// Plan, and with a context also build, run and compare.
async fn pipeline(req: Request, ctx: Option<Ctx>) -> String {
    let root = req.root();
    let scratch = req.scratch();
    let (base_rev, base_label) = match &req.head {
        Some(head) => (head.sha.clone(), format!("session-start commit {}", head.short())),
        None => ("HEAD".to_string(), "HEAD".to_string()),
    };
    if let Some(ctx) = &ctx {
        ctx.progress("reading the change");
    }
    let changes = if req.head.is_none() && req.changed.is_some() {
        Changes {
            files: req
                .changed
                .iter()
                .flatten()
                .map(|f| crate::testsel::normalize(&root, Path::new(f)))
                .collect(),
            symbols: req.symbols.clone().unwrap_or_default(),
            base_label: "files given".to_string(),
            ..Changes::default()
        }
    } else {
        match symbols::changes(&root, &base_rev, base_label, req.changed.as_deref(), req.symbols.as_deref()).await {
            Ok(changes) => changes,
            Err(err) => {
                return crate::toolver::did_not_run(
                    &format!("could not read the change ({err})"),
                    "run it inside the git work tree, or pass `changed` (files) and `symbols`",
                    "",
                );
            }
        }
    };
    // The tools' own tree: one build at a time (a background warm build may be filling it).
    let _tree_guard = if req.build_dir.is_none() {
        match hold_own_tree(&req, &root, ctx.as_ref()).await {
            Ok(guard) => guard,
            Err(text) => return text,
        }
    } else {
        None
    };
    if let Some(ctx) = &ctx {
        ctx.progress("reading the build system");
    }
    let model = match discover(&req, &root, &scratch, &changes).await {
        Ok(model) => model,
        Err(err) => {
            return crate::toolver::did_not_run(
                &err,
                "configure the project (e.g. cmake -S . -B build, or ./configure) and pass build_dir, \
                 or build and run its tests in the shell",
                "",
            );
        }
    };
    let sel = select::select(&model, &changes).await;
    dump_selection(&model, &changes, &sel);
    let mut out = String::new();
    out.push_str(&header(&model));
    out.push_str(&change_line(&model, &changes));
    out.push_str(&plan_lines(&model, &sel));
    let Some(ctx) = ctx else {
        out.push_str(&next_line(&model, &sel, None, false, req.finish_gate_suite));
        return out;
    };
    debug_assert!(!req.plan_only);

    // What to run.
    let chosen: Vec<usize> = if req.full {
        sel.enabled.clone()
    } else {
        match req.tier {
            3 => sel.tier3.clone(),
            2 => sel.tier2.clone(),
            _ => sel.tier1.iter().map(|p| p.test).collect(),
        }
    };
    if chosen.is_empty() && !(req.full && model.system == System::Make) {
        out.push_str("RUN: nothing selected — ");
        out.push_str(if sel.tier3.is_empty() {
            "the change reaches no registered test (docs/scripts only, or tests are not registered in this build tree)."
        } else {
            "try a higher tier."
        });
        out.push('\n');
        out.push_str(&next_line(&model, &sel, Some(req.tier), req.full, req.finish_gate_suite));
        return out;
    }
    let (plan, items) = plan_run(&model, &sel, &chosen, req.tier, req.full);
    let targets = plan.targets.clone();
    let build_all = plan.build_all;
    for note in &plan.notes {
        out.push_str(&format!("  {note}\n"));
    }

    // Build.
    let build_dir = model.build_dir.clone();
    let mut not_built: BTreeSet<String> = BTreeSet::new();
    let skip_build = items.iter().all(|i| matches!(i, RunItem::MakeCheck { .. }));
    if !skip_build {
        let to_build: Vec<String> = if build_all { Vec::new() } else { targets.clone() };
        let built = exec::build(&ctx, &model, &build_dir, &to_build).await;
        if built.cancelled {
            out.push_str("Cancelled during the build.\n");
            return out;
        }
        out.push_str(&format!(
            "BUILD {}: {} in {:.0}s{}\n",
            if build_all || targets.is_empty() { "everything".to_string() } else { short_list(&targets, TARGETS_SHOWN) },
            if built.ok { "OK" } else { "FAILED" },
            built.secs,
            if built.failed.is_empty() { String::new() } else { format!(" — not built: {}", short_list(&built.failed, 6)) }
        ));
        for error in built.errors.iter().take(3) {
            out.push_str(&indent(error, "  "));
        }
        if built.timed_out {
            out.push_str("  (the build hit its time cap)\n");
        }
        not_built.extend(built.failed.iter().cloned());
        // A build that did not finish, or failed without naming which target failed (the
        // whole build, or an error no target owns), leaves programs from an older build:
        // running them would test old code, so nothing runs.
        if built.timed_out {
            out.push_str(&format!("{NOT_RUN_TIME_CAP}\n"));
            out.push_str(&next_line(&model, &sel, Some(req.tier), req.full, req.finish_gate_suite));
            out.push_str(&format!("log: {}\n", ctx.log.display()));
            return out;
        }
        if !built.ok && (build_all || built.failed.is_empty()) {
            out.push_str(&format!("{NOT_RUN_BUILD_FAILED}\n"));
            out.push_str(&format!("log: {}\n", ctx.log.display()));
            return out;
        }
    }
    // Tests whose program did not build are reported, not run.
    let mut now: BTreeMap<String, TestStatus> = BTreeMap::new();
    let runnable: Vec<RunItem> = items
        .into_iter()
        .filter_map(|item| drop_not_built(item, &model, &not_built, &mut now))
        .collect();
    for line in tree::run_lines(&model, &runnable) {
        out.push_str(&format!("TESTS: {line}\n"));
    }
    let ran = exec::run(&ctx, &runnable).await;
    if ran.cancelled {
        out.push_str("Cancelled while running tests.\n");
        return out;
    }
    now.extend(ran.results.iter().map(|(k, v)| (k.clone(), *v)));
    out.push_str(&run_summary(&req, &sel, &now, ran.secs));
    out.push_str(&failure_lines(&now, &ran.details));
    for note in ran.notes.iter().take(3) {
        out.push_str(&format!("  note: {note}\n"));
    }

    // Base comparison.
    if req.compare_base {
        match &req.head {
            None => out.push_str("FLIPS: no session-start commit (not a git repository), so no base to compare with.\n"),
            Some(head) => {
                let compared = base::compare(
                    &ctx,
                    &base::Compared {
                        model: &model,
                        head,
                        scratch: &scratch,
                        targets: &targets,
                        items: &runnable,
                        now: &now,
                        sel: &sel,
                        changes: &changes,
                    },
                )
                .await;
                out.push_str(&flip_lines(head, &compared));
            }
        }
    }
    out.push_str(&format!("DIFF: {}\n", tree::diff_check(&root, &base_rev).await));
    out.push_str(&next_line(&model, &sel, Some(req.tier), req.full, req.finish_gate_suite));
    out.push_str(&format!("log: {}\n", ctx.log.display()));
    out
}

/// Take the tools' own tree for this call. A run waits for a build already going in it (the
/// background warm build, or another session) and can be cancelled meanwhile; a plan only
/// reads, so it does not wait when the tree is already configured for this commit.
async fn hold_own_tree(
    req: &Request,
    root: &Path,
    ctx: Option<&Ctx>,
) -> Result<Option<crate::builds::ProjectGuard>, String> {
    let own = tree::own_tree(root);
    // `BYTEASK_TEST_SELECT_WARM_YIELD`: a run (not a plan) stops a warm build still going in its tree; what
    // it built is kept.
    if let Some(ctx) = ctx {
        if warm_yield() && crate::testsel::warm::stop_for_check(&own) {
            ctx.progress("stopped the background warm build of its tree (what it built is kept)");
        }
    }
    if let Some(guard) = tree::try_lock_tree(&own) {
        return Ok(Some(guard));
    }
    let commit = req.head.as_ref().map(|h| h.sha.as_str());
    if ctx.is_none() && own.join("CMakeCache.txt").is_file() && cmake::configured_at(&own).as_deref() == commit {
        return Ok(None);
    }
    let busy = |waited: u64| {
        crate::toolver::did_not_run(
            &format!("another build of this tool's tree {} was still going after {waited}s", own.display()),
            "call again when it has finished (a background build started with the task fills it)",
            "",
        )
    };
    let Some(ctx) = ctx else {
        return match tree::lock_tree(&own, Duration::from_secs(DEFAULT_WAIT_S)).await {
            Ok(guard) => Ok(Some(guard)),
            Err(err) => Err(busy(err.waited.as_secs())),
        };
    };
    ctx.progress("waiting for the background build of its tree");
    let mut cancel = ctx.cancel.clone();
    tokio::select! {
        held = tree::lock_tree(&own, exec::BUILD_TIMEOUT) => match held {
            Ok(guard) => Ok(Some(guard)),
            Err(err) => Err(busy(err.waited.as_secs())),
        },
        _ = cancel.wait_for(|c| *c) => Err("Cancelled while waiting for another build of its tree.\n".to_string()),
    }
}

/// Measurement hook: with `BYTEASK_TESTSEL_DUMP=<file>` set, every plan also
/// writes the complete tier sets as JSON (the model-facing text lists at most
/// a few names per tier). Off by default; nothing else reads it.
fn dump_selection(model: &Model, changes: &Changes, sel: &Selection) {
    let Some(path) = std::env::var_os("BYTEASK_TESTSEL_DUMP").filter(|p| !p.is_empty()) else {
        return;
    };
    let names = |ids: &[usize]| -> Vec<String> { ids.iter().map(|&i| model.tests[i].name.clone()).collect() };
    let tier1: Vec<serde_json::Value> = sel
        .tier1
        .iter()
        .map(|p| serde_json::json!({"test": model.tests[p.test].name, "cases": p.cases}))
        .collect();
    let doc = serde_json::json!({
        "system": model.system.label(),
        "build_dir": model.build_dir.display().to_string(),
        "dir_kind": format!("{:?}", model.dir_kind),
        "mapping": model.mapping_note,
        "files": changes.files.iter().map(|f| model.rel(f).to_string()).collect::<Vec<_>>(),
        "symbols": changes.symbols,
        "all": names(&sel.enabled),
        "tier1": tier1,
        "tier2": names(&sel.tier2),
        "tier3": names(&sel.tier3),
        "header_only": sel.header_only,
        "warnings": sel.warnings,
        "sources": sel
            .enabled
            .iter()
            .filter_map(|&i| {
                let test = &model.tests[i];
                let target = model.target(test.target.as_deref()?)?;
                let files: Vec<String> = target.sources.iter().map(|f| model.rel(f).to_string()).collect();
                Some((test.name.clone(), serde_json::json!(files)))
            })
            .collect::<serde_json::Map<String, serde_json::Value>>(),
    });
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&doc).unwrap_or_default());
}

/// Find the build system and read its model.
async fn discover(req: &Request, root: &Path, scratch: &Path, changes: &Changes) -> Result<Model, String> {
    let cmake_files: Vec<PathBuf> = changes
        .files
        .iter()
        .filter(|f| {
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            name == "CMakeLists.txt" || name.ends_with(".cmake")
        })
        .cloned()
        .collect();
    let commit = req.head.as_ref().map(|h| h.sha.as_str());
    let has_cmake = crate::tools::binary_on_path("cmake");
    let version = if has_cmake { crate::toolver::tool_version("cmake", root).await } else { None };
    if let Some(dir) = &req.build_dir {
        let dir = crate::testsel::normalize(root, Path::new(dir));
        if dir.join("CMakeCache.txt").is_file() {
            return cmake::model(&cmake::Request { root, build_dir: Some(&dir), scratch, version, cmake_files: &cmake_files, commit }).await;
        }
        if dir.join("meson-info/intro-targets.json").is_file() {
            return meson::model(root, &dir).await;
        }
        if ["GNUmakefile", "makefile", "Makefile"].iter().any(|m| dir.join(m).is_file()) {
            return make::model(root, &dir, &changes.files).await;
        }
        return Err(format!("{} is not a configured CMake, Meson or Make build directory", dir.display()));
    }
    if has_cmake && !cmake::existing_build_dirs(root).is_empty() {
        return cmake::model(&cmake::Request { root, build_dir: None, scratch, version, cmake_files: &cmake_files, commit }).await;
    }
    if let Some(dir) = meson::existing_build_dirs(root).into_iter().next() {
        return meson::model(root, &dir).await;
    }
    if root.join("CMakeLists.txt").is_file() {
        if !has_cmake {
            return Err("CMake project, but cmake is not installed".to_string());
        }
        return cmake::model(&cmake::Request { root, build_dir: None, scratch, version, cmake_files: &cmake_files, commit }).await;
    }
    if let Some(dir) = make::make_dir(root) {
        return make::model(root, &dir, &changes.files).await;
    }
    if root.join("meson.build").is_file() {
        return Err("Meson project without a configured build directory (run `meson setup <dir>`, then pass build_dir)".to_string());
    }
    if root.join("configure.ac").is_file() || root.join("configure").is_file() {
        return Err("Autotools project that is not configured yet (no Makefile)".to_string());
    }
    Err("no CMake, Meson or Make build found at the repository root".to_string())
}

/// What to build (exact targets; everything only for the full suite or without a target map,
/// and said so) and the items to run.
fn plan_run(model: &Model, sel: &Selection, chosen: &[usize], tier: u32, full: bool) -> (tree::TargetPlan, Vec<RunItem>) {
    let plan = if full && model.system != System::Make {
        tree::TargetPlan {
            build_all: true,
            ..tree::TargetPlan::default()
        }
    } else if model.system == System::Make {
        // Make builds what its rules say; unmapped programs come with the check target.
        let mut targets: Vec<String> = Vec::new();
        for &i in chosen {
            if let Some(t) = &model.tests[i].target
                && !targets.contains(t)
            {
                targets.push(t.clone());
            }
        }
        tree::TargetPlan {
            targets,
            ..tree::TargetPlan::default()
        }
    } else {
        tree::plan_targets(model, chosen)
    };
    let mut items = Vec::new();
    match model.system {
        System::Make if full || tier == 3 => {
            if let Some(check) = &model.check_target {
                items.push(RunItem::MakeCheck { dir: model.build_dir.clone(), target: check.clone() });
                return (plan, items);
            }
            for &i in chosen {
                items.push(direct(model, i, None, &[]));
            }
        }
        System::CMake => {
            let mut ctest_names: Vec<String> = Vec::new();
            if tier == 1 && !full {
                for pick in &sel.tier1 {
                    if !pick.cases.is_empty() && select::runs_its_own_program(model, &model.tests[pick.test]) {
                        items.push(direct(model, pick.test, pick.framework, &pick.cases));
                    } else {
                        ctest_names.push(model.tests[pick.test].name.clone());
                    }
                }
            } else if !full {
                ctest_names = chosen.iter().map(|&i| model.tests[i].name.clone()).collect();
            }
            if full {
                items.push(RunItem::Ctest { dir: model.build_dir.clone(), names: Vec::new() });
            } else if !ctest_names.is_empty() {
                items.push(RunItem::Ctest { dir: model.build_dir.clone(), names: ctest_names });
            }
        }
        System::Meson | System::Make => {
            if tier == 1 && !full {
                for pick in &sel.tier1 {
                    items.push(direct(model, pick.test, pick.framework, &pick.cases));
                }
            } else {
                for &i in chosen {
                    items.push(direct(model, i, None, &[]));
                }
            }
        }
    }
    (plan, items)
}

fn direct(model: &Model, test: usize, framework: Option<symbols::Framework>, cases: &[String]) -> RunItem {
    let t = &model.tests[test];
    RunItem::Direct {
        test: t.name.clone(),
        argv: t.command.clone(),
        workdir: t.workdir.clone().unwrap_or_else(|| model.build_dir.clone()),
        env: t.env.clone(),
        framework,
        cases: cases.to_vec(),
    }
}

/// Drop a run item whose program did not build, recording NOT BUILT for it.
fn drop_not_built(
    item: RunItem,
    model: &Model,
    not_built: &BTreeSet<String>,
    now: &mut BTreeMap<String, TestStatus>,
) -> Option<RunItem> {
    let target_of = |name: &str| model.tests.iter().find(|t| t.name == name).and_then(|t| t.target.clone());
    match item {
        RunItem::Ctest { dir, names } => {
            // An empty list means "every test" and is kept as it is.
            let every_test = names.is_empty();
            let (bad, good): (Vec<String>, Vec<String>) = names
                .into_iter()
                .partition(|n| target_of(n).is_some_and(|t| not_built.contains(&t)));
            for name in bad {
                now.insert(name, TestStatus::NotBuilt);
            }
            (every_test || !good.is_empty()).then_some(RunItem::Ctest { dir, names: good })
        }
        RunItem::Direct { ref test, .. } if target_of(test).is_some_and(|t| not_built.contains(&t)) => {
            now.insert(test.clone(), TestStatus::NotBuilt);
            None
        }
        other => Some(other),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn header(model: &Model) -> String {
    let version = model.version.map(|(a, b)| format!(" {a}.{b}")).unwrap_or_default();
    let dir = model.rel(&model.build_dir).to_string();
    let kind = match model.dir_kind {
        DirKind::Existing => "existing: built incrementally, never reconfigured",
        DirKind::Scratch => "the tool's own tree outside the project",
        DirKind::InTree => "in the source tree, as `make` builds it",
    };
    let mut out = format!(
        "{}{version}, build tree {dir} ({kind}); {}\n",
        model.system.label(),
        model.mapping_note
    );
    for note in model.notes.iter().take(3) {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

fn change_line(model: &Model, changes: &Changes) -> String {
    let files: Vec<String> = changes.files.iter().map(|f| model.rel(f).to_string()).collect();
    let symbols = if changes.symbols.is_empty() {
        "none found".to_string()
    } else {
        let mut s = changes.symbols.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
        let more = changes.symbols.len().saturating_sub(8) + changes.symbols_dropped;
        if more > 0 {
            s.push_str(&format!(" (+{more})"));
        }
        s
    };
    format!(
        "CHANGE vs {}: {} file(s) ({}); symbols: {symbols}\n",
        changes.base_label,
        files.len(),
        short_list(&files, 4)
    )
}

fn plan_lines(model: &Model, sel: &Selection) -> String {
    let programs: BTreeSet<usize> = sel.tier1.iter().map(|p| p.test).collect();
    let cases = sel.tier1_case_count();
    let whole = sel.tier1.iter().filter(|p| p.cases.is_empty()).count();
    let mut out = format!(
        "PLAN: tier 1 = {} · tier 2 = {} test(s) · tier 3 = {} of {} test(s){}\n",
        if cases == 0 {
            format!("{} test(s)", programs.len())
        } else if whole == 0 {
            format!("{cases} case(s) in {} program(s)", programs.len())
        } else {
            format!("{cases} case(s) + {whole} whole test(s)")
        },
        sel.tier2.len(),
        sel.tier3.len(),
        sel.enabled.len(),
        if sel.unmapped_in_tier3 > 0 { format!(", {} of them unmapped script/data tests", sel.unmapped_in_tier3) } else { String::new() }
    );
    let names = |ids: &[usize]| -> Vec<String> { ids.iter().map(|&i| model.tests[i].name.clone()).collect() };
    if !sel.tier1.is_empty() && cases > 0 {
        let shown: Vec<String> = sel
            .tier1
            .iter()
            .flat_map(|p| {
                let test = model.tests[p.test].name.clone();
                if p.cases.is_empty() { vec![test] } else { p.cases.iter().map(|c| format!("{test}::{c}")).collect() }
            })
            .collect();
        out.push_str(&format!("  tier 1: {}\n", short_list(&shown, NAMES_SHOWN)));
    }
    if !sel.tier2.is_empty() {
        out.push_str(&format!("  tier 2: {}\n", short_list(&names(&sel.tier2), NAMES_SHOWN)));
    }
    if !sel.tier3.is_empty() && sel.tier3.len() != sel.tier2.len() {
        out.push_str(&format!("  tier 3: {}\n", short_list(&names(&sel.tier3), NAMES_SHOWN)));
    }
    if !sel.affected.unmapped.is_empty() {
        let files: Vec<String> = sel.affected.unmapped.iter().map(|f| model.rel(f).to_string()).collect();
        out.push_str(&format!("  not compiled by any known target: {}\n", short_list(&files, 4)));
    }
    for warning in &sel.warnings {
        out.push_str(&format!("  ! {warning}\n"));
    }
    out
}

fn run_summary(req: &Request, sel: &Selection, now: &BTreeMap<String, TestStatus>, secs: f64) -> String {
    let count = |f: fn(TestStatus) -> bool| now.values().filter(|s| f(**s)).count();
    let passed = count(|s| s == TestStatus::Pass);
    let failed = count(|s| s == TestStatus::Fail || s == TestStatus::Timeout);
    let flaky = count(|s| s == TestStatus::Flaky);
    let not_built = count(|s| s == TestStatus::NotBuilt);
    let skipped = count(|s| s == TestStatus::Skip);
    let not_run = count(|s| s == TestStatus::NotRun);
    let what = if req.full {
        "full suite".to_string()
    } else if req.tier == 1 && sel.tier1_case_count() > 0 {
        "tier 1".to_string()
    } else {
        format!("tier {}", req.tier)
    };
    let mut parts = vec![format!("{passed} passed")];
    if failed > 0 {
        parts.push(format!("{failed} FAILED"));
    }
    if not_built > 0 {
        parts.push(format!("{not_built} NOT BUILT"));
    }
    if flaky > 0 {
        parts.push(format!("{flaky} flaky (failed, then passed on a re-run)"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if not_run > 0 {
        parts.push(format!("{not_run} not run"));
    }
    format!("RUN {what}: {} result(s) in {secs:.1}s — {}\n", now.len(), parts.join(", "))
}

fn failure_lines(now: &BTreeMap<String, TestStatus>, details: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = String::new();
    let failing: Vec<(&String, &TestStatus)> = now.iter().filter(|(_, s)| s.failing()).collect();
    for (name, status) in failing.iter().take(FAILURES_SHOWN) {
        out.push_str(&format!("  {} {name}\n", status.label()));
        for line in details.get(*name).into_iter().flatten().take(3) {
            out.push_str(&format!("      {line}\n"));
        }
    }
    if failing.len() > FAILURES_SHOWN {
        out.push_str(&format!("  … {} more failure(s) in the log\n", failing.len() - FAILURES_SHOWN));
    }
    out
}

fn flip_lines(head: &Head, compared: &base::BaseOutcome) -> String {
    let mut out = String::new();
    let regress: Vec<String> = compared.flips.iter().filter(|f| f.is_regression()).map(|f| f.name.clone()).collect();
    let fixes: Vec<String> = compared.flips.iter().filter(|f| f.is_fix()).map(|f| format!("{} ({})", f.name, f.label())).collect();
    out.push_str(&format!(
        "FLIPS vs base {} ({} run on base now, {} from cache): pass->fail {}{}; fail->pass {}{}\n",
        head.short(),
        compared.ran,
        compared.cached,
        regress.len(),
        if regress.is_empty() { String::new() } else { format!(" ({})", short_list(&regress, 6)) },
        fixes.len(),
        if fixes.is_empty() { String::new() } else { format!(" ({})", short_list(&fixes, 6)) },
    ));
    for (name, near) in compared.neighbours.iter().take(3) {
        out.push_str(&format!(
            "  neighbours of {name} (same group, same names/messages, did not flip): {}\n",
            short_list(near, 5)
        ));
    }
    for note in &compared.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

fn next_line(model: &Model, sel: &Selection, ran: Option<u32>, full: bool, gate_suite: bool) -> String {
    if full {
        return String::new();
    }
    let mut options = Vec::new();
    let tier_ran = ran.unwrap_or(0);
    if tier_ran < 2 && !sel.tier2.is_empty() {
        options.push(format!("tier=2 ({} test(s))", sel.tier2.len()));
    }
    if tier_ran < 3 && !sel.tier3.is_empty() && sel.tier3.len() != sel.tier2.len() {
        options.push(format!("tier=3 ({} test(s))", sel.tier3.len()));
    }
    // One full-suite run per change: the finish gate's when it runs one, else the model's.
    let suite = match (gate_suite, model.system, &model.check_target) {
        (true, _, _) => format!("the full suite ({} tests) runs in the finish gate when you finish", sel.enabled.len()),
        (false, System::Make, Some(check)) => format!("full_suite=true once before finishing (`make {check}`)"),
        _ => format!("full_suite=true once before finishing ({} tests)", sel.enabled.len()),
    };
    options.push(suite);
    if ran.is_some() {
        options.push("compare_base=true to see which failures are new".to_string());
    }
    format!("NEXT: {}\n", options.join(" · "))
}

fn short_list(items: &[String], n: usize) -> String {
    let mut s = items.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > n {
        s.push_str(&format!(" (+{})", items.len() - n));
    }
    s
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}\n")).collect()
}

#[cfg(test)]
#[path = "test_select_tests.rs"]
mod tests;
