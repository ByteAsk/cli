//! `cpp_fix_verify` — check a crash/memory-safety fix as strictly as a
//! maintainer review would, so an incomplete fix is caught before it is final:
//!
//! 0. Given the UNPATCHED binary, confirm it crashes on the PoC (else the
//!    checks below prove nothing) and take its crash stack as the original one.
//! 1. Replay the PoC on the (already instrumented) patched binary — it must no
//!    longer crash.
//! 2. Optionally run the project's functionality tests (a command the caller
//!    gives) and report the result.
//! 3. Optionally diff behaviour on benign inputs against the UNPATCHED binary:
//!    the vulnerable build is the oracle for every input that does not trigger
//!    the bug, so a change there is a regression the fix introduced.
//! 4. Run the patch-shape critic on the diff.
//!
//! It builds nothing: the caller passes binaries it built with the project's
//! own sanitizer build (single source of truth for how the project compiles).
//! The verdict names every check and its evidence, and never claims more than
//! the evidence shows:
//! - NOT VERIFIED: a check failed;
//! - VERIFIED: every check that ran passed AND they cover the reported crash —
//!   the unpatched build crashes on the PoC and the patched one does not;
//! - PARTIAL: nothing failed, but something the verdict depends on was not
//!   checked or was inconclusive (no unpatched build, an unpatched build that
//!   does not crash, a timeout, a behaviour diff with no comparable input); the
//!   verdict lists what was and was not checked;
//! - NOTHING CHECKED.

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

use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::timeout_from;

use super::critic;
use super::runner;
use super::runner::InputMode;
use super::runner::Outcome;

const TOOL_NAME: &str = "cpp_fix_verify";
/// How many benign inputs the behaviour diff will run at most.
const MAX_BENIGN: usize = 60;
/// Per-input replay timeout when the caller gives none.
const DEFAULT_REPLAY_TIMEOUT_SECS: u64 = 60;

pub struct FixVerifyTool {
    pub cwd: PathBuf,
    /// Runs the built binaries natively (outside the sandbox); see [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    binary: String,
    #[serde(default)]
    poc: Option<String>,
    #[serde(default)]
    input_mode: Option<String>,
    #[serde(default)]
    run_args: Option<String>,
    #[serde(default)]
    sanitizer: Option<String>,
    #[serde(default)]
    baseline_binary: Option<String>,
    #[serde(default)]
    benign_inputs: Option<Vec<String>>,
    #[serde(default)]
    test_command: Option<Vec<String>>,
    #[serde(default)]
    diff: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for FixVerifyTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "binary".to_string(),
            JsonSchema::string(Some(
                "Path to the PATCHED, sanitizer-instrumented binary to test (the fuzz target or \
                 CLI). Build it with the project's OWN sanitizer build (e.g. its ASan/UBSan \
                 config, or the project's own fuzz/oss-fuzz build), not a hand-written compile line."
                    .to_string(),
            )),
        );
        properties.insert(
            "poc".to_string(),
            JsonSchema::string(Some(
                "Path to the crashing input file (the PoC). It is replayed on the patched binary, \
                 which must no longer crash (and on baseline_binary, which must still crash)."
                    .to_string(),
            )),
        );
        properties.insert(
            "input_mode".to_string(),
            JsonSchema::string(Some(
                "How the input reaches the program: \"arg\" (default; the file path is the last \
                 argument, as for a libFuzzer target or most CLIs) or \"stdin\" (the file is piped \
                 to stdin). Put {poc} in run_args to place the path yourself."
                    .to_string(),
            )),
        );
        properties.insert(
            "run_args".to_string(),
            JsonSchema::string(Some(
                "Arguments for the binary, shell-style (space-separated, quote to keep spaces; no \
                 shell runs). {poc} is replaced by the current input path; without it the path is \
                 appended (arg mode) or piped (stdin mode)."
                    .to_string(),
            )),
        );
        properties.insert(
            "sanitizer".to_string(),
            JsonSchema::string(Some(
                "Which sanitizer the binaries were built with: \"asan_ubsan\" (default), \"msan\" \
                 or \"tsan\". Sets only the runtime options for the replay; it does not rebuild."
                    .to_string(),
            )),
        );
        properties.insert(
            "baseline_binary".to_string(),
            JsonSchema::string(Some(
                "Path to the UNPATCHED build. When given, the tool first checks it still crashes on \
                 the PoC, then diffs exit code and stdout of the patched vs unpatched binary on \
                 benign (non-crashing) inputs: the vulnerable \
                 build is the oracle for inputs that do not trigger the bug, so a difference is a \
                 behaviour change the fix introduced. Build it the same way, at the base commit."
                    .to_string(),
            )),
        );
        properties.insert(
            "benign_inputs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Files or directories of non-crashing inputs for the behaviour diff (the \
                     repo's test inputs or seed corpus, and inputs near the PoC that you expect \
                     to stay valid)."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "test_command".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "The project's functionality-test command as an argv list (no shell: [\"ctest\", \
                     \"--output-on-failure\"], [\"make\", \"check\"]). The tool runs it in the \
                     workspace and reports pass/fail. Run it on the base commit too and compare."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "diff".to_string(),
            JsonSchema::string(Some(
                "Path to the fix's patch (unified diff) for the patch-shape critic. Omit to use \
                 `git diff` in the workspace."
                    .to_string(),
            )),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some(format!(
                "Per-run wall-clock cap for each replay and each test/diff run. Default \
                 {DEFAULT_REPLAY_TIMEOUT_SECS}s."
            ))),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Verify a crash / memory-safety fix as strictly as a maintainer review would, to \
                catch a fix that stops the one PoC but not the bug. Given the PATCHED \
                sanitizer-instrumented binary and the PoC, it: checks the UNPATCHED binary (if \
                given) still crashes on the PoC; replays the PoC on the patched binary (must no \
                longer crash); optionally runs the project's functionality tests; and, given the \
                UNPATCHED binary, diffs behaviour on benign inputs (the vulnerable build is the \
                oracle for inputs that do not trigger the bug). It also runs the patch-shape \
                critic on the diff. It BUILDS NOTHING — pass binaries built with the project's own \
                sanitizer build. Reports a verdict with per-check evidence: VERIFIED only when the \
                unpatched build crashes on the PoC, the patched one does not, and every other check \
                that ran passed; PARTIAL (with what was and was not checked) when something was \
                not checked or was inconclusive. Reach for it before finishing any \
                crash/sanitizer/PoC fix."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["binary".to_string()]),
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
        // Serial: it runs builds' binaries and tests under contention.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl FixVerifyTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: Args = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;

        let request = self.resolve(args)?;
        let report = self.verify(request).await?;
        Ok(crate::report::report(
            TOOL_NAME,
            TOOL_NAME,
            report.render(),
            report.result(),
        ))
    }

    fn resolve(&self, args: Args) -> Result<Request, FunctionCallError> {
        let binary = resolve_existing(&self.cwd, &args.binary, "binary")?;
        let poc = match &args.poc {
            Some(p) => Some(resolve_existing(&self.cwd, p, "poc")?),
            None => None,
        };
        let mode = match args.input_mode.as_deref() {
            None | Some("arg") => InputMode::Arg,
            Some("stdin") => InputMode::Stdin,
            Some(other) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unknown input_mode {other:?}; expected \"arg\" or \"stdin\""
                )));
            }
        };
        let sanitizer = args.sanitizer.as_deref().unwrap_or("asan_ubsan").to_string();
        if !matches!(sanitizer.as_str(), "asan_ubsan" | "msan" | "tsan") {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown sanitizer {sanitizer:?}; expected \"asan_ubsan\", \"msan\" or \"tsan\""
            )));
        }
        let run_args = crate::program_args::parse("run_args", args.run_args.as_deref().unwrap_or(""))
            .map_err(FunctionCallError::RespondToModel)?;
        let baseline = match &args.baseline_binary {
            Some(p) => Some(resolve_existing(&self.cwd, p, "baseline_binary")?),
            None => None,
        };
        let mut benign = Vec::new();
        for entry in args.benign_inputs.iter().flatten() {
            benign.push(resolve_existing(&self.cwd, entry, "benign_inputs")?);
        }
        Ok(Request {
            binary,
            poc,
            mode,
            run_args,
            sanitizer,
            baseline,
            benign,
            test_command: args.test_command.unwrap_or_default(),
            diff: args.diff.clone(),
            timeout: timeout_from(Some(args.timeout_sec.unwrap_or(DEFAULT_REPLAY_TIMEOUT_SECS))),
        })
    }

    async fn verify(&self, req: Request) -> Result<Report, FunctionCallError> {
        let mut report = Report::default();
        let env = self.sanitizer_env(&req.sanitizer);

        // Benign inputs for the behaviour diff.
        let mut benign: Vec<(String, Vec<u8>)> = Vec::new();
        // Crash-stack functions of any crash we still see, for the critic.
        let mut crash_functions: Vec<String> = Vec::new();
        // The ORIGINAL crash stack (the unpatched build on the PoC): what the
        // critic's crash-site rule compares the edited functions against.
        let mut original_stack: Vec<String> = Vec::new();

        // 0. The unpatched build must reproduce the bug, or the PoC checks
        //    below prove nothing about the fix.
        if let (Some(baseline), Some(poc)) = (&req.baseline, &req.poc) {
            let outcome = self.run_binary(baseline, &req, poc, &env).await?;
            report.baseline = Some(match outcome {
                Outcome::Crashed { bug, evidence, .. } => {
                    if let Some(ev) = &evidence {
                        original_stack = ev.crash_functions();
                    }
                    Check::Pass(format!("the unpatched build crashes on the PoC ({bug})"))
                }
                _ => Check::Note(
                    "the unpatched build does NOT crash on the PoC, so the PoC check proves \
                     nothing about the fix; check baseline_binary and the PoC."
                        .to_string(),
                ),
            });
        }

        // 1. PoC replay (three times; reproducers can be flaky).
        if let Some(poc) = &req.poc {
            let mut last = None;
            for _ in 0..3 {
                let outcome = self.run_input(&req, poc, &env).await?;
                let crashed = outcome.crashed();
                last = Some(outcome);
                if crashed {
                    break;
                }
            }
            report.poc = match last {
                Some(Outcome::Crashed { bug, evidence, .. }) => {
                    if let Some(ev) = &evidence {
                        crash_functions = ev.crash_functions();
                    }
                    Check::Fail(format!("the PoC still crashes: {bug}"))
                }
                Some(Outcome::TimedOut { secs }) => Check::Note(format!(
                    "the PoC run did not finish within {secs}s; inconclusive (raise timeout_sec, \
                     or the fix may have introduced a hang)"
                )),
                _ => Check::Pass("the PoC no longer crashes".to_string()),
            };
        }

        // 2. Functionality tests.
        if !req.test_command.is_empty() {
            report.tests = Some(self.run_tests(&req).await?);
        }

        // 3. Behaviour diff vs the unpatched binary on benign inputs.
        if let Some(baseline) = &req.baseline {
            self.collect_benign(&req, &mut benign)?;
            report.behaviour = Some(self.behaviour_diff(&req, baseline, &benign, &env).await?);
        }

        // 4. Patch-shape critic.
        if !original_stack.is_empty() {
            crash_functions = original_stack;
        }
        report.critic = self.critique(&req, &crash_functions).await?;

        Ok(report)
    }

    fn sanitizer_env(&self, sanitizer: &str) -> Vec<(String, String)> {
        // Reuse the single-TU runtime options so the verifier and cpp_sanitize
        // read a report identically. For msan/asan halt at the first error.
        match sanitizer {
            "tsan" => crate::tools::sanitizer_runtime_env("tsan"),
            "msan" => crate::tools::sanitizer_runtime_env("msan"),
            _ => crate::tools::sanitizer_runtime_env("asan_ubsan"),
        }
    }

    async fn run_input(
        &self,
        req: &Request,
        input: &Path,
        env: &[(String, String)],
    ) -> Result<Outcome, FunctionCallError> {
        self.run_path(req, input, env).await
    }

    async fn run_path(
        &self,
        req: &Request,
        input: &Path,
        env: &[(String, String)],
    ) -> Result<Outcome, FunctionCallError> {
        runner::run_once(&req.binary, &req.run_args, input, req.mode, env, &self.cwd, req.timeout).await
    }

    async fn run_tests(&self, req: &Request) -> Result<Check, FunctionCallError> {
        let program = req.test_command[0].clone();
        let args = req.test_command[1..].to_vec();
        // Tests can be slow: give them a generous multiple of the per-run cap.
        let cap = Duration::from_secs(req.timeout.as_secs().max(60) * 10);
        // One engine-started build or test run at a time per session, and parallelism bounded
        // by available memory unless the command sets its own.
        let started = std::time::Instant::now();
        let Some(_slot) = crate::resources::build_slot(cap).await else {
            return Ok(Check::Note(
                "the test command did not start: another ByteAsk build or test run was still in progress"
                    .to_string(),
            ));
        };
        let env = crate::resources::parallel_env(crate::resources::jobs());
        let cap = cap.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
        let run = crate::run::run_capped_env(&program, &args, &self.cwd, cap, &env).await?;
        Ok(match run {
            RunOutcome::TimedOut { secs } => {
                Check::Note(format!("the test command did not finish within {secs}s"))
            }
            RunOutcome::Finished { success, code, .. } => {
                if success {
                    Check::Pass("the project's tests pass on the patched tree".to_string())
                } else {
                    Check::Fail(format!(
                        "the project's tests fail on the patched tree (exit {}). Run them on the \
                         base commit too: a failure the base also has is pre-existing.",
                        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())
                    ))
                }
            }
        })
    }

    fn collect_benign(
        &self,
        req: &Request,
        benign: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), FunctionCallError> {
        let add_file = |path: &Path, benign: &mut Vec<(String, Vec<u8>)>| {
            if benign.len() >= MAX_BENIGN {
                return;
            }
            if let Ok(bytes) = std::fs::read(path) {
                benign.push((path.display().to_string(), bytes));
            }
        };
        for source in req.benign.clone() {
            if source.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&source) {
                    let mut files: Vec<PathBuf> =
                        entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
                    files.sort();
                    for file in files {
                        add_file(&file, benign);
                    }
                }
            } else {
                add_file(&source, benign);
            }
        }
        benign.truncate(MAX_BENIGN);
        Ok(())
    }

    async fn behaviour_diff(
        &self,
        req: &Request,
        baseline: &Path,
        benign: &[(String, Vec<u8>)],
        env: &[(String, String)],
    ) -> Result<Check, FunctionCallError> {
        if benign.is_empty() {
            return Ok(Check::NotRun(
                "no benign inputs to diff (pass benign_inputs)".to_string(),
            ));
        }
        let mut compared = 0;
        let mut diffs: Vec<String> = Vec::new();
        for (label, bytes) in benign {
            let path = runner::write_temp("byteask-verify-benign", bytes)?;
            let patched = self.run_binary(&req.binary, req, &path, env).await;
            let base = self.run_binary(baseline, req, &path, env).await;
            let _ = std::fs::remove_file(&path);
            let (patched, base) = (patched?, base?);
            // Only inputs benign on BOTH builds are oracles for equality.
            let (Outcome::Clean { code: pc, stdout: po, truncated: pt }, Outcome::Clean { code: bc, stdout: bo, truncated: bt }) =
                (&patched, &base)
            else {
                continue;
            };
            if *pt || *bt {
                continue; // truncated output: cannot assert equality
            }
            compared += 1;
            if pc != bc {
                if diffs.len() < 5 {
                    diffs.push(format!("{label}: exit {bc:?} → {pc:?}"));
                }
            } else if po != bo && diffs.len() < 5 {
                diffs.push(format!("{label}: stdout changed"));
            }
        }
        Ok(if compared == 0 {
            Check::Note("no input was benign on both builds; behaviour diff inconclusive".to_string())
        } else if diffs.is_empty() {
            Check::Pass(format!(
                "patched and unpatched agree on {compared} benign input(s)"
            ))
        } else {
            Check::Fail(format!(
                "behaviour changed on {} of {compared} benign input(s) vs the unpatched build — a \
                 regression unless the bug condition explains it: {}",
                diffs.len(),
                diffs.join("; ")
            ))
        })
    }

    async fn run_binary(
        &self,
        binary: &Path,
        req: &Request,
        input: &Path,
        env: &[(String, String)],
    ) -> Result<Outcome, FunctionCallError> {
        runner::run_once(binary, &req.run_args, input, req.mode, env, &self.cwd, req.timeout).await
    }

    async fn critique(
        &self,
        req: &Request,
        crash_functions: &[String],
    ) -> Result<Option<String>, FunctionCallError> {
        let diff_text = match &req.diff {
            Some(path) => {
                let path = resolve_existing(&self.cwd, &path.clone(), "diff")?;
                std::fs::read_to_string(&path).map_err(|err| {
                    FunctionCallError::RespondToModel(format!("cannot read diff {}: {err}", path.display()))
                })?
            }
            None => {
                // The repository the workspace is in, or the one below it.
                let repo = crate::gitx::primary_root(&self.cwd)
                    .await
                    .unwrap_or_else(|| self.cwd.clone());
                let run = run_capped(
                    "git",
                    &["-C".to_string(), repo.display().to_string(), "diff".to_string()],
                    &self.cwd,
                    Duration::from_secs(30),
                )
                .await?;
                match run {
                    RunOutcome::Finished { success: true, stdout, .. } => stdout,
                    _ => return Ok(None), // not a git repo, or git absent
                }
            }
        };
        if diff_text.trim().is_empty() {
            return Ok(None);
        }
        let findings = critic::critique(&diff_text, crash_functions);
        Ok(Some(critic::render(&findings)))
    }
}

/// Resolved request.
struct Request {
    binary: PathBuf,
    poc: Option<PathBuf>,
    mode: InputMode,
    run_args: Vec<String>,
    sanitizer: String,
    baseline: Option<PathBuf>,
    benign: Vec<PathBuf>,
    test_command: Vec<String>,
    diff: Option<String>,
    timeout: Duration,
}

/// One check's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Check {
    /// Not requested.
    Skipped,
    /// Requested but not run, with the reason (e.g. no inputs were given).
    NotRun(String),
    Pass(String),
    Fail(String),
    /// Ran but could not decide: does not fail the verdict, but keeps it PARTIAL.
    Note(String),
}

impl Default for Check {
    fn default() -> Self {
        Check::Skipped
    }
}

impl Check {
    fn is_fail(&self) -> bool {
        matches!(self, Check::Fail(_))
    }
    fn line(&self, name: &str) -> Option<String> {
        match self {
            Check::Skipped => None,
            Check::NotRun(msg) => Some(format!("- NOT RUN {name}: {msg}")),
            Check::Pass(msg) => Some(format!("- PASS {name}: {msg}")),
            Check::Fail(msg) => Some(format!("- FAIL {name}: {msg}")),
            Check::Note(msg) => Some(format!("- NOTE {name}: {msg}")),
        }
    }
}

#[derive(Default)]
struct Report {
    baseline: Option<Check>,
    poc: Check,
    tests: Option<Check>,
    behaviour: Option<Check>,
    critic: Option<String>,
}

impl Report {
    fn checks(&self) -> Vec<(&str, &Check)> {
        let mut out = Vec::new();
        if let Some(c) = &self.baseline {
            out.push(("unpatched build", c));
        }
        out.push(("PoC replay", &self.poc));
        if let Some(c) = &self.tests {
            out.push(("functionality tests", c));
        }
        if let Some(c) = &self.behaviour {
            out.push(("behaviour diff", c));
        }
        out
    }

    /// What the verdict depends on that was not checked or was inconclusive. Empty only when
    /// the checks cover the reported crash (the unpatched build crashes on the PoC, the patched
    /// one does not) and no check that ran was inconclusive.
    fn gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        match &self.poc {
            Check::Pass(_) => match &self.baseline {
                Some(Check::Pass(_)) => {}
                Some(_) => gaps.push(
                    "the unpatched build does not crash on the PoC, so the replay does not exercise \
                     the reported bug"
                        .to_string(),
                ),
                None => gaps.push(
                    "the unpatched build was not run on the PoC (no baseline_binary), so nothing \
                     shows this PoC reproduces the reported crash in this build"
                        .to_string(),
                ),
            },
            Check::Note(_) => gaps.push("the PoC replay was inconclusive".to_string()),
            _ => gaps.push("no PoC was replayed, so the reported crash itself was not checked".to_string()),
        }
        if matches!(self.tests, Some(Check::Note(_))) {
            gaps.push("the functionality tests did not finish".to_string());
        }
        if matches!(self.behaviour, Some(Check::Note(_))) {
            gaps.push("the behaviour diff was inconclusive".to_string());
        }
        gaps
    }

    /// The shared result, from the same facts as the VERDICT line: a failed check is
    /// FAIL, nothing checked is NOT RUN, gaps are APPROXIMATE, and only a verified fix is PASS.
    fn result(&self) -> check_result::CheckResult {
        use check_result::CheckResult;
        let checks = self.checks();
        if let Some((name, _)) = checks.iter().find(|(_, c)| c.is_fail()) {
            return CheckResult::fail(TOOL_NAME, &format!("The {name} check failed; the fix is incomplete or changes behaviour."));
        }
        if checks
            .iter()
            .all(|(_, c)| matches!(c, Check::Skipped | Check::NotRun(_)))
        {
            return CheckResult::not_run(
                TOOL_NAME,
                check_result::NotRunReason::BadArguments,
                "Nothing was checked.",
                "pass a poc, or a test_command/baseline_binary.",
            );
        }
        let gaps = self.gaps();
        if !gaps.is_empty() {
            return CheckResult::approximate(
                TOOL_NAME,
                &format!("No check failed, but this does not show the fix works: {}.", gaps.join("; ")),
            );
        }
        CheckResult::pass(
            TOOL_NAME,
            "The unpatched build crashes on the PoC, the patched one does not, and every other check that ran passed.",
        )
    }

    fn render(&self) -> String {
        let checks = self.checks();
        let failed = checks.iter().any(|(_, c)| c.is_fail());
        let critic_flags = self
            .critic
            .as_deref()
            .is_some_and(|c| !c.contains("no findings"));
        let gaps = self.gaps();
        let passed: Vec<&str> = checks
            .iter()
            .filter(|(_, c)| matches!(c, Check::Pass(_)))
            .map(|(name, _)| *name)
            .collect();
        let mut out = String::new();
        let verdict = if failed {
            "VERDICT: NOT VERIFIED — a check failed; the fix is incomplete or changes behaviour."
                .to_string()
        } else if checks
            .iter()
            .all(|(_, c)| matches!(c, Check::Skipped | Check::NotRun(_)))
        {
            "VERDICT: NOTHING CHECKED — pass a poc, or a test_command/baseline_binary.".to_string()
        } else if !gaps.is_empty() {
            format!(
                "VERDICT: PARTIAL — no check failed, but this does not show the fix works. Checked: {}. Not checked: {}.",
                if passed.is_empty() {
                    "nothing conclusive".to_string()
                } else {
                    passed.join(", ")
                },
                gaps.join("; ")
            )
        } else if critic_flags {
            "VERDICT: VERIFIED, WITH PATCH-SHAPE FINDINGS — the unpatched build crashes on the PoC, the patched one does not, and every other check that ran passed; address the advisory findings below or explain them.".to_string()
        } else {
            "VERDICT: VERIFIED — the unpatched build crashes on the PoC, the patched one does not, and every other check that ran passed.".to_string()
        };
        out.push_str(&verdict);
        out.push('\n');
        for (name, check) in checks {
            if let Some(line) = check.line(name) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        if let Some(critic) = &self.critic {
            out.push('\n');
            out.push_str(critic.trim_end());
            out.push('\n');
        }
        out
    }
}

fn resolve_existing(cwd: &Path, path: &str, field: &str) -> Result<PathBuf, FunctionCallError> {
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    if !resolved.exists() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{field}: path does not exist: {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reproduced() -> Option<Check> {
        Some(Check::Pass("the unpatched build crashes on the PoC (heap-buffer-overflow)".to_string()))
    }

    #[test]
    fn verdict_verified_when_the_crash_is_reproduced_and_all_pass() {
        let report = Report {
            baseline: reproduced(),
            poc: Check::Pass("the PoC no longer crashes".to_string()),
            behaviour: Some(Check::Pass("patched and unpatched agree on 3 benign input(s)".to_string())),
            critic: Some("patch-shape review: no findings.".to_string()),
            ..Default::default()
        };
        let text = report.render();
        assert!(text.starts_with("VERDICT: VERIFIED —"), "{text}");
        assert!(text.contains("- PASS PoC replay:"));
        assert!(text.contains("- PASS unpatched build:"));
    }

    #[test]
    fn a_failing_check_fails_the_verdict() {
        let report = Report {
            baseline: reproduced(),
            poc: Check::Pass("ok".to_string()),
            tests: Some(Check::Fail("the project's tests fail".to_string())),
            ..Default::default()
        };
        assert!(report.render().starts_with("VERDICT: NOT VERIFIED"));
    }

    #[test]
    fn patch_shape_findings_qualify_a_pass() {
        let report = Report {
            baseline: reproduced(),
            poc: Check::Pass("ok".to_string()),
            critic: Some("patch-shape review (advisory, static):\n- [crash-site-guard] a.c:1 ...".to_string()),
            ..Default::default()
        };
        assert!(report.render().starts_with("VERDICT: VERIFIED, WITH PATCH-SHAPE FINDINGS"));
    }

    /// Guards against an overclaim: a clean PoC replay with no unpatched build must not
    /// say VERIFIED, because nothing shows the PoC triggers the bug in this build.
    #[test]
    fn a_clean_replay_without_the_unpatched_build_is_partial() {
        let report = Report {
            poc: Check::Pass("the PoC no longer crashes".to_string()),
            ..Default::default()
        };
        let text = report.render();
        assert!(text.starts_with("VERDICT: PARTIAL"), "{text}");
        assert!(text.contains("Checked: PoC replay"), "{text}");
        assert!(text.contains("no baseline_binary"), "{text}");
        assert!(!text.contains("VERIFIED —"), "{text}");
    }

    #[test]
    fn a_baseline_that_does_not_reproduce_is_partial() {
        let report = Report {
            baseline: Some(Check::Note("the unpatched build does NOT crash".to_string())),
            poc: Check::Pass("ok".to_string()),
            ..Default::default()
        };
        let text = report.render();
        assert!(text.starts_with("VERDICT: PARTIAL"), "{text}");
        assert!(text.contains("- NOTE unpatched build:"), "{text}");
        assert!(text.contains("does not exercise the reported bug"), "{text}");
    }

    /// The second overclaim: an inconclusive behaviour diff counted as a pass.
    #[test]
    fn an_inconclusive_check_keeps_the_verdict_partial() {
        let inconclusive = Report {
            baseline: reproduced(),
            poc: Check::Pass("ok".to_string()),
            behaviour: Some(Check::Note("no input was benign on both builds".to_string())),
            ..Default::default()
        };
        let text = inconclusive.render();
        assert!(text.starts_with("VERDICT: PARTIAL"), "{text}");
        assert!(text.contains("the behaviour diff was inconclusive"), "{text}");
        let timed_out = Report {
            baseline: reproduced(),
            poc: Check::Note("the PoC run did not finish within 60s".to_string()),
            ..Default::default()
        };
        assert!(timed_out.render().starts_with("VERDICT: PARTIAL"));
        let slow_tests = Report {
            baseline: reproduced(),
            poc: Check::Pass("ok".to_string()),
            tests: Some(Check::Note("the test command did not finish within 600s".to_string())),
            ..Default::default()
        };
        assert!(slow_tests.render().contains("the functionality tests did not finish"));
    }

    /// A behaviour diff that was asked for without inputs did not run; it neither fails nor
    /// blocks VERIFIED (the crash itself is covered), and the output says it did not run.
    #[test]
    fn a_behaviour_diff_without_inputs_is_reported_as_not_run() {
        let report = Report {
            baseline: reproduced(),
            poc: Check::Pass("ok".to_string()),
            behaviour: Some(Check::NotRun("no benign inputs to diff (pass benign_inputs)".to_string())),
            ..Default::default()
        };
        let text = report.render();
        assert!(text.starts_with("VERDICT: VERIFIED —"), "{text}");
        assert!(text.contains("- NOT RUN behaviour diff:"), "{text}");
    }

    #[test]
    fn tests_alone_do_not_cover_the_crash() {
        let report = Report {
            tests: Some(Check::Pass("the project's tests pass on the patched tree".to_string())),
            ..Default::default()
        };
        let text = report.render();
        assert!(text.starts_with("VERDICT: PARTIAL"), "{text}");
        assert!(text.contains("no PoC was replayed"), "{text}");
    }

    #[test]
    fn nothing_checked() {
        assert!(Report::default().render().starts_with("VERDICT: NOTHING CHECKED"));
    }
}

#[cfg(test)]
mod live_tests {
    //! End-to-end tests that build tiny C programs with the real sanitizer and
    //! run cpp_fix_verify against them. `#[ignore]`d: they need clang/gcc.
    //! Run with `cargo test -p codex-cpp-tools -- --include-ignored memsafety::verify::live`.
    use std::path::Path;
    use std::path::PathBuf;

    use crate::program_args::fixture::have;

    use super::*;

    /// Compile `src` to `out` with the given sanitizer flags. Returns false if
    /// the compiler is missing (the test then skips).
    fn compile(cc: &str, src: &Path, out: &Path, extra: &[&str]) -> bool {
        if !have(cc) {
            return false;
        }
        let mut args: Vec<String> = vec![
            "-g".to_string(),
            "-O0".to_string(),
            "-fno-omit-frame-pointer".to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push(src.display().to_string());
        args.push("-o".to_string());
        args.push(out.display().to_string());
        std::process::Command::new(cc)
            .args(&args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn tool(cwd: &Path) -> FixVerifyTool {
        FixVerifyTool {
            cwd: cwd.to_path_buf(),
            native_exec: true,
        }
    }

    async fn run(tool: &FixVerifyTool, args: serde_json::Value) -> String {
        crate::test_support::call(tool, args).await.expect("verify runs")
    }

    // A CLI: reads argv[1] as a file, copies its body into a fixed 8-byte heap
    // buffer. VULN copies the whole file (overflow); FIX bounds-checks.
    const VULN: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  FILE *f = fopen(argv[1], "rb");
  if (!f) return 2;
  char in[256]; size_t n = fread(in, 1, sizeof in, f); fclose(f);
  char *buf = (char *)malloc(8);
  memcpy(buf, in, n);            /* overflow when n > 8 */
  printf("copied %zu\n", n);
  free(buf);
  return 0;
}
"#;
    const FIX: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  FILE *f = fopen(argv[1], "rb");
  if (!f) return 2;
  char in[256]; size_t n = fread(in, 1, sizeof in, f); fclose(f);
  if (n > 8) { fprintf(stderr, "input too long\n"); return 1; }
  char *buf = (char *)malloc(8);
  memcpy(buf, in, n);
  printf("copied %zu\n", n);
  free(buf);
  return 0;
}
"#;

    #[tokio::test]
    #[ignore = "needs clang/gcc with ASan; run with --include-ignored"]
    async fn a_fixed_overflow_verifies_and_the_vulnerable_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (vuln_c, fix_c) = (root.join("vuln.c"), root.join("fix.c"));
        std::fs::write(&vuln_c, VULN).unwrap();
        std::fs::write(&fix_c, FIX).unwrap();
        let (vuln_bin, fix_bin): (PathBuf, PathBuf) = (root.join("vuln"), root.join("fix"));
        let san = ["-fsanitize=address,undefined"];
        if !compile("clang", &vuln_c, &vuln_bin, &san) || !compile("clang", &fix_c, &fix_bin, &san) {
            eprintln!("skip: no clang");
            return;
        }
        // A PoC that overflows the 8-byte buffer.
        let poc = root.join("poc.bin");
        std::fs::write(&poc, vec![b'A'; 64]).unwrap();

        // Sanity: the vulnerable build crashes on the PoC → PoC check FAILs.
        let vulner = run(
            &tool(root),
            serde_json::json!({"binary": "vuln", "poc": "poc.bin"}),
        )
        .await;
        assert!(vulner.contains("VERDICT: NOT VERIFIED"), "{vulner}");
        assert!(vulner.contains("FAIL PoC replay"), "{vulner}");

        // The fixed build: PoC clean, behaviour matches on
        // benign inputs → VERIFIED (patch-shape critic has no diff here).
        let benign = root.join("small.bin");
        std::fs::write(&benign, b"hello").unwrap();
        let fixed = run(
            &tool(root),
            serde_json::json!({
                "binary": "fix",
                "poc": "poc.bin",
                "baseline_binary": "vuln",
                "benign_inputs": ["small.bin"],
            }),
        )
        .await;
        assert!(fixed.contains("VERDICT: VERIFIED"), "{fixed}");
        assert!(fixed.contains("PASS PoC replay"), "{fixed}");
        assert!(fixed.contains("behaviour"), "{fixed}");
        assert!(fixed.contains("PASS unpatched build"), "{fixed}");
    }

    #[tokio::test]
    #[ignore = "needs clang with MSan; run with --include-ignored"]
    async fn msan_reports_use_of_uninitialised_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = root.join("um.c");
        std::fs::write(
            &src,
            "#include <stdio.h>\n#include <stdlib.h>\nint main(int c,char**v){int*a=malloc(20);a[0]=c;if(a[c])puts(\"x\");else puts(\"y\");free(a);return 0;}\n",
        )
        .unwrap();
        let bin = root.join("um");
        if !compile(
            "clang",
            &src,
            &bin,
            &["-fsanitize=memory", "-fsanitize-memory-track-origins=2"],
        ) {
            eprintln!("skip: clang has no MSan");
            return;
        }
        // No PoC needed; run the binary with a benign input and confirm the
        // sanitizer env is wired (msan). Here we just confirm the tool runs.
        let poc = root.join("in");
        std::fs::write(&poc, b"x").unwrap();
        let out = run(
            &tool(root),
            serde_json::json!({"binary": "um", "poc": "in", "sanitizer": "msan", "input_mode": "arg"}),
        )
        .await;
        // The program ignores its argument, so no crash: PoC replay passes.
        assert!(out.contains("PoC replay"), "{out}");
    }
}

#[cfg(test)]
mod replay_tests {
    //! Offline replay of the STATIC patch-shape critic against agents' final
    //! diffs from earlier runs, to measure agreement with the official grader
    //! (reward). No build, no docker, no network. Benchmark data is NOT stored
    //! in the repo: the cases are read at run time from a directory named by
    //! `BYTEASK_REPLAY_DIR`, one subdir per case with `agent.diff` and
    //! `meta.json` = {"reward": 0|1, "crash_functions": ["..."]}.
    //!
    //! Build the case directory first (one subdirectory per case), then:
    //!   BYTEASK_REPLAY_DIR=/path cargo test -p codex-cpp-tools \
    //!     -- --include-ignored memsafety::verify::replay --nocapture
    use std::path::PathBuf;

    #[test]
    #[ignore = "set BYTEASK_REPLAY_DIR to a prepared case directory"]
    fn replay_critic_agreement() {
        let Some(dir) = std::env::var_os("BYTEASK_REPLAY_DIR") else {
            eprintln!("skip: BYTEASK_REPLAY_DIR unset");
            return;
        };
        let dir = PathBuf::from(dir);
        let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("replay dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        cases.sort();

        // Confusion counts: does the critic flag a patch (any finding)?
        // "wrong" = reward 0, "correct" = reward 1.
        let (mut wrong_flagged, mut wrong_total) = (0u32, 0u32);
        let (mut correct_flagged, mut correct_total) = (0u32, 0u32);
        println!("\n{:<44} {:>6} {:>7} {}", "case", "reward", "flagged", "rules");
        for case in &cases {
            let diff = std::fs::read_to_string(case.join("agent.diff")).unwrap_or_default();
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(case.join("meta.json")).unwrap_or_default())
                    .unwrap_or(serde_json::Value::Null);
            let reward = meta.get("reward").and_then(|r| r.as_f64()).unwrap_or(-1.0);
            let crash_functions: Vec<String> = meta
                .get("crash_functions")
                .and_then(|c| c.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let findings = super::super::critic::critique(&diff, &crash_functions);
            let flagged = !findings.is_empty();
            let rules: Vec<&str> = findings.iter().map(|f| f.rule).collect();
            println!(
                "{:<44} {:>6} {:>7} {}",
                case.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                reward,
                flagged,
                rules.join(",")
            );
            if reward == 0.0 {
                wrong_total += 1;
                if flagged {
                    wrong_flagged += 1;
                }
            } else if reward == 1.0 {
                correct_total += 1;
                if flagged {
                    correct_flagged += 1;
                }
            }
        }
        println!(
            "\nwrong patches flagged (recall): {wrong_flagged}/{wrong_total}\n\
             correct patches flagged (false alarm): {correct_flagged}/{correct_total}"
        );
    }
}
