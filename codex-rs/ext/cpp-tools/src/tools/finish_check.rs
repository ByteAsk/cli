//! `cpp_finish_check`: the model-called, advisory finish check (0.2.0, replaces the automatic
//! finish gate as the default).
//!
//! It runs the finish gate's build-and-test part ([`crate::gate::run_gate`] with
//! `advisory: true`) on what changed since the session started. In a CMake project
//! ([`crate::testsel::verify`]) it builds ONLY the test programs the change reaches, from the
//! local build model, incrementally in the C/C++ tools' own tree outside the project (the tree
//! `cpp_test_select` and the background warm build use; it waits for a warm build in progress),
//! runs exactly those tests, and names the targets, the test filter and `git diff --check` in
//! its answer; when the model cannot map a test it builds a stated superset. Other build
//! systems: the project's existing build, built whole, and its suite. Failures are compared with
//! the unmodified tree. Within [`BUDGET`], with no background run.
//!
//! What it does NOT do, unlike the automatic gate: no hygiene rules (edited or new test files,
//! files left from the session), no checklist, no patch-shape critic, and it never sends the
//! model back. It never deletes a file: untracked files an in-place build created are named in
//! the answer and left where they are. Its answer is a verdict (PASS / FAIL / NOT RUN with the
//! reason) and the details. It needs the same project trust as the gate's builds.

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

use crate::gate;
use crate::workflow::Workflow;

pub(crate) const TOOL_NAME: &str = gate::FINISH_CHECK;
/// The whole check's time limit (the gate's former fast tier).
pub(crate) const BUDGET: Duration = Duration::from_secs(90);
/// Another time limit for the check, in seconds (at least 10).
pub(crate) const ENV_BUDGET: &str = "BYTEASK_FINISH_CHECK_SECS";

/// The check's time limit: [`ENV_BUDGET`] when set, else [`BUDGET`].
fn budget(root: &std::path::Path) -> Duration {
    #[cfg(test)]
    if let Some(budget) = test_budget(root) {
        return budget;
    }
    #[cfg(not(test))]
    let _ = root;
    std::env::var(ENV_BUDGET)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(10)))
        .unwrap_or(BUDGET)
}

/// Tests: a time limit for one project root (the environment is shared by parallel tests).
#[cfg(test)]
pub(crate) static TEST_BUDGETS: std::sync::Mutex<Vec<(PathBuf, Duration)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn test_budget(root: &std::path::Path) -> Option<Duration> {
    TEST_BUDGETS
        .lock()
        .ok()?
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, budget)| *budget)
}

pub struct FinishCheckTool {
    pub cwd: PathBuf,
    /// The build runs the project's own build scripts.
    pub native_exec: bool,
    /// This thread's workflow: its session-start snapshot (what "the change" is measured
    /// against) and the check's state kept between calls. `None` outside a C/C++ root thread:
    /// then the change is the work tree against HEAD.
    pub workflow: Option<Arc<Workflow>>,
}

/// What one thread's finish checks share between calls: the unmodified tree's build and test
/// results, and the last change set's outcome (an unchanged change set is not rebuilt).
#[derive(Default)]
pub(crate) struct FinishState {
    base: gate::BaseCache,
    memo: Option<gate::BuildMemo>,
    advisories: gate::Advisories,
    /// `finish_review`: the content key of the last change a check ran on to its end.
    checked: Option<String>,
    /// `finish_review`: automatic checks sent, per turn.
    reviews: std::collections::HashMap<String, u32>,
    /// `BYTEASK_FINISH_REVIEW_OWN_TESTS`: the content key the agent's own last successful build saw.
    pub(crate) built: Option<String>,
    /// The content key the agent's own tests last passed on (after a build of it), with the command.
    pub(crate) tested: Option<(String, String)>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Args {}

impl<'call> ToolExecutor<ToolCall<'call>> for FinishCheckTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Optional check before you say a C/C++ change works: builds only the \
                test programs the change since the session started reaches, with the project's \
                own build system (CMake: incrementally, in a tree of its own outside the \
                project; otherwise the existing build), runs exactly those tests and compares \
                failures with the unmodified tree, within 90 seconds. The answer names the \
                targets built, the test filter and `git diff --check`. \
                Advisory: it answers PASS, FAIL or NOT RUN (with the reason) and the details, and \
                never blocks finishing; it does not edit or delete any file. Report what it \
                checked and what is still unverified. It runs the project's build, so it needs \
                a trusted project."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                Default::default(),
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
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl FinishCheckTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let _: Args = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
        let (body, result) = self.check().await;
        Ok(crate::report::report_plain(TOOL_NAME, body, result))
    }

    /// The check itself: the answer's text and its verdict.
    pub(crate) async fn check(&self) -> (String, check_result::CheckResult) {
        self.check_within(None).await
    }

    /// [`Self::check`] with another time limit (the automatic review's own, [`ENV_REVIEW_BUDGET`]).
    pub(crate) async fn check_within(
        &self,
        limit: Option<Duration>,
    ) -> (String, check_result::CheckResult) {
        let snapshot = match &self.workflow {
            Some(workflow) => workflow.snapshot().await,
            None => head_snapshot(&self.cwd).await.map(Arc::new),
        };
        let Some(snapshot) = snapshot else {
            return (
                "Not a git work tree with a commit, so there is no unmodified version to build and compare with.".to_string(),
                check_result::CheckResult::not_run(
                    TOOL_NAME,
                    check_result::NotRunReason::NoBuildProfile,
                    "The workspace is not a git work tree with a commit.",
                    "build and run the tests with the project's own build system, and say what you checked.",
                ),
            );
        };
        let changes = gate::agent_changes(&snapshot).await;
        let budget = limit.unwrap_or_else(|| budget(&self.cwd));
        // Context's related places (switch `finish_related`), beside the build and tests.
        let related =
            codex_core::byteask_context::related_changes_for(&snapshot.root).map(|related| {
                let root = snapshot.root.clone();
                let head = snapshot.head.clone();
                tokio::spawn(async move {
                    let input = crate::testsel::context::change(&root, &head).await?;
                    related(input).await
                })
            });
        let related_until =
            tokio::time::Instant::now() + codex_core::byteask_context::related_timeout();
        let settings = gate::GateSettings {
            gate: false,
            impact: false,
            tests: true,
            critic: false,
            budget,
            fast_tier: Duration::ZERO,
            max_loops: 0,
            session_cap: Duration::from_secs(86_400),
            targeted: true,
        };
        let mut fresh = FinishState::default();
        let mut shared = match &self.workflow {
            Some(workflow) => Some(workflow.finish_state().await),
            None => None,
        };
        let state: &mut FinishState = match shared.as_deref_mut() {
            Some(state) => state,
            None => &mut fresh,
        };
        let outcome = offload_check(gate::run_gate(gate::GateInput {
            snapshot: &snapshot,
            changes: &changes,
            settings: &settings,
            native_exec: self.native_exec,
            non_interactive: false,
            impact_state: None,
            base: &mut state.base,
            assistant_text: "",
            memo: &mut state.memo,
            advisories: &mut state.advisories,
            crash: None,
            session_left: settings.session_cap,
            advisory: true,
        }))
        .await;
        // The verdict belongs to the code it checked: an edit made meanwhile makes it nobody's.
        let after = gate::agent_changes(&snapshot).await;
        if after.content_key == changes.content_key {
            state.checked = Some(changes.content_key.clone());
        }
        let result = if after.content_key == changes.content_key {
            outcome.result()
        } else {
            check_result::CheckResult::not_run(
                TOOL_NAME,
                check_result::NotRunReason::Cancelled,
                "Files changed while the check was building and testing them, so its result belongs to neither version.",
                "call cpp_finish_check again on the final version.",
            )
        };
        let mut text = render(&outcome, budget);
        let first = related_first();
        if let Some(handle) = related {
            // `BYTEASK_RELATED_GRACE_SECS`: wait at least the grace from now for a ready related answer
            let related_until = match codex_core::byteask_context::related_grace() {
                Some(grace) => related_until.max(tokio::time::Instant::now() + grace),
                None => related_until,
            };
            // No answer is said, not left silent (silence reads as "nothing related").
            match tokio::time::timeout_at(related_until, handle).await {
                Ok(Ok(Ok(answer))) => place_related(&mut text, &related_section(&answer), first),
                _ if changes.is_empty() => {}
                Ok(Ok(Err(why))) => place_related(&mut text, &related_missing(&why), first),
                Ok(Err(_)) => place_related(&mut text, &related_missing("the call failed"), first),
                // the handle is dropped; the call ends with its own timeout
                Err(_) => place_related(
                    &mut text,
                    &related_missing(&format!(
                        "no answer within {} s",
                        codex_core::byteask_context::related_timeout().as_secs()
                    )),
                    first,
                ),
            }
        }
        (text, result)
    }
}

/// The work tree measured against HEAD, for a thread without a session-start snapshot: every
/// uncommitted change and every untracked file counts as the change.
async fn head_snapshot(cwd: &std::path::Path) -> Option<gate::Snapshot> {
    let mut snapshot = gate::take_snapshot(cwd).await?;
    snapshot.dirty.clear();
    snapshot.untracked.clear();
    snapshot.start_copy = None;
    Some(snapshot)
}

/// Most of Context's related-places answer shown.
const RELATED_LINES: usize = 30;
const RELATED_CHARS: usize = 4000;

/// Context's answer under a neutral header (on its own line; the header does not promise places over an
/// answer that names none), or one line when the answer names nothing (blank, or JSON whose lists are all empty).
fn related_section(answer: &str) -> String {
    let answer = answer.trim();
    if answer.is_empty() {
        return RELATED_NONE.to_string();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(answer) {
        let empty = match &value {
            serde_json::Value::Array(items) => items.is_empty(),
            serde_json::Value::Object(map) => map
                .values()
                .filter_map(serde_json::Value::as_array)
                .all(Vec::is_empty),
            _ => false,
        };
        if empty {
            return RELATED_NONE.to_string();
        }
    }
    let mut shown: String = answer
        .lines()
        .take(RELATED_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if shown.len() > RELATED_CHARS {
        let cut = (0..=RELATED_CHARS)
            .rev()
            .find(|i| shown.is_char_boundary(*i))
            .unwrap_or(0);
        shown.truncate(cut);
    }
    if shown.len() < answer.len() {
        shown.push_str("\n…");
    }
    format!(
        "\nRelated places (ByteAsk Context's find_related_changes on this change; check whether each place it names \
         needs the same change):\n{shown}\n"
    )
}

/// An empty answer, said as such.
const RELATED_NONE: &str =
    "\nRelated places: ByteAsk Context's find_related_changes named none for this change.\n";

/// Context gave no answer (an error or no answer in time), said in one line.
fn related_missing(why: &str) -> String {
    let why: String = why.trim().chars().take(240).collect();
    format!(
        "\nRelated places: ByteAsk Context's find_related_changes gave no answer for this change ({why}); look for \
         them yourself.\n"
    )
}

/// Optional switch (off by default): `[byteask_cpp] finish_review = true` or `BYTEASK_CPP_FINISH_REVIEW=1`.
/// When the model ends its turn and no `cpp_finish_check` ran on its final change, the engine runs that same
/// check (same tree, same memo, Context's related places when `finish_related` is on) and sends the answer back as
/// one message, so the turn goes on once more. A change the model already checked runs nothing and sends nothing.
/// At most [`REVIEW_ROUNDS`] automatic checks per turn.
pub(crate) const FINISH_REVIEW_KEY: &str = "finish_review";
pub(crate) const ENV_FINISH_REVIEW: &str = "BYTEASK_CPP_FINISH_REVIEW";
const REVIEW_ROUNDS: u32 = 2;
static FINISH_REVIEW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Process-wide (one session per process), set at thread start.
pub(crate) fn set_finish_review(on: bool) {
    FINISH_REVIEW.store(on, std::sync::atomic::Ordering::Release);
}

pub(crate) fn finish_review_on() -> bool {
    FINISH_REVIEW.load(std::sync::atomic::Ordering::Acquire)
}

const REVIEW_HEAD: &str = "Automatic finish check: you ended your turn without running cpp_finish_check on your final \
    change, so it was run for you. Its answer:";
/// Optional switch (off unless set): the automatic review's own time limit in seconds (at least 10).
pub(crate) const ENV_REVIEW_BUDGET: &str = "BYTEASK_FINISH_REVIEW_SECS";

fn review_budget() -> Option<Duration> {
    std::env::var(ENV_REVIEW_BUDGET)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(10)))
}

/// `BYTEASK_FINISH_REVIEW_OWN_TESTS`: the message when the agent's own tests passed on its final change.
const REVIEW_OWN_HEAD: &str = "Automatic finish check: you ended your turn without running cpp_finish_check on \
    your final change. Your own test run on that change (";
const REVIEW_OWN_MID: &str =
    ") passed after a build of it, so the build and tests were not run again.";
const REVIEW_OWN_TAIL: &str = "Before you finish: for each related place it lists, either make the same change \
    there or say in your final message why it needs none. If nothing in it needs action, finish.";

/// Context's related places for the change alone (no build, no test), within the check's wait for them;
/// `None` when Context names none, fails or does not answer in time.
async fn related_only(snapshot: &gate::Snapshot) -> Option<String> {
    let related = codex_core::byteask_context::related_changes_for(&snapshot.root)?;
    let root = snapshot.root.clone();
    let head = snapshot.head.clone();
    let call = async move {
        let input = crate::testsel::context::change(&root, &head).await?;
        related(input).await
    };
    match tokio::time::timeout(codex_core::byteask_context::related_timeout(), call).await {
        Ok(Ok(answer)) => {
            let section = related_section(&answer);
            (section != RELATED_NONE).then_some(section)
        }
        _ => None,
    }
}

const REVIEW_TAIL: &str = "Before you finish: fix any problem it reports, and for each related place it lists, \
    either make the same change there or say in your final message why it needs none. If nothing in it needs \
    action, finish.";

/// The automatic check of [`FINISH_REVIEW_KEY`]; empty when the turn may end.
pub(crate) async fn finish_review(
    cwd: &std::path::Path,
    native_exec: bool,
    sandbox: Arc<crate::sandbox::Settings>,
    workflow: &Arc<Workflow>,
    turn_id: &str,
) -> codex_extension_api::TurnGateFragments {
    let Some(snapshot) = workflow.snapshot().await else {
        return Vec::new();
    };
    let changes = gate::agent_changes(&snapshot).await;
    if changes.is_empty() || !native_exec {
        return Vec::new();
    }
    let own_tests = {
        let state = workflow.finish_state().await;
        if state.checked.as_deref() == Some(changes.content_key.as_str())
            || state.reviews.get(turn_id).copied().unwrap_or(0) >= REVIEW_ROUNDS
        {
            return Vec::new();
        }
        // The agent's own tests passed on exactly this change, after a build of it.
        state
            .tested
            .as_ref()
            .filter(|(key, _)| super::own_tests::on() && *key == changes.content_key)
            .map(|(_, command)| command.clone())
    };
    let started = std::time::Instant::now();
    if let Some(command) = own_tests {
        let related = related_only(&snapshot).await;
        {
            let mut state = workflow.finish_state().await;
            *state.reviews.entry(turn_id.to_string()).or_insert(0) += 1;
            state.checked = Some(changes.content_key.clone());
        }
        crate::report::log_review(
            "automatic-own-tests",
            related.is_some(),
            started.elapsed().as_millis(),
            &command,
        );
        let message = related.as_ref().map(|section| {
            format!("{REVIEW_OWN_HEAD}`{command}`{REVIEW_OWN_MID}\n{section}\n{REVIEW_OWN_TAIL}")
        });
        // `BYTEASK_FINISH_REVIEW_LOG`: the full message and its related places, per review.
        crate::report::log_review_message(
            "own-tests",
            message.is_some(),
            started.elapsed().as_millis(),
            message.as_deref().unwrap_or_default(),
            related.as_deref().unwrap_or_default(),
        );
        return match message {
            Some(message) => crate::workflow::fragment("finish_review", message),
            None => Vec::new(),
        };
    }
    let tool = FinishCheckTool {
        cwd: cwd.to_path_buf(),
        native_exec,
        workflow: Some(Arc::clone(workflow)),
    };
    let (body, result) =
        crate::sandbox::scope(TOOL_NAME, sandbox, tool.check_within(review_budget())).await;
    *workflow
        .finish_state()
        .await
        .reviews
        .entry(turn_id.to_string())
        .or_insert(0) += 1;
    let answer = crate::report::report_text(TOOL_NAME, "automatic", body, result);
    if super::own_tests::on() || review_budget().is_some() {
        crate::report::log_review("automatic", true, started.elapsed().as_millis(), "");
    }
    let message = format!("{REVIEW_HEAD}\n\n{answer}\n\n{REVIEW_TAIL}");
    // `BYTEASK_FINISH_REVIEW_LOG`: the full message and its related-places section, per review.
    let related = answer
        .find("\nRelated places")
        .map(|at| {
            answer[at + 1..]
                .split("\nsandbox:")
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_string()
        })
        .unwrap_or_default();
    crate::report::log_review_message(
        "check",
        true,
        started.elapsed().as_millis(),
        &message,
        &related,
    );
    crate::workflow::fragment("finish_review", message)
}

/// `BYTEASK_CHECK_OFFLOAD`: the check's gate run (synchronous, CPU- and file-heavy steps on large trees) runs
/// under `block_in_place` + `Handle::block_on` on a multi-thread runtime, so the runtime hands the engine's other tasks
/// (the related call, its transport, timers) to other workers meanwhile. Off, or on another runtime: awaited in place.
async fn offload_check<F: std::future::Future>(fut: F) -> F::Output {
    let on = std::env::var("BYTEASK_CHECK_OFFLOAD")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"));
    if on {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                return tokio::task::block_in_place(move || handle.block_on(fut));
            }
        }
    }
    fut.await
}

/// The answer's text (the verdict line is appended by the caller's report).
/// `BYTEASK_CHECK_RELATED_FIRST`: the related places come before the "Checks:" line and that line lists at
/// most [`NOTES_CAP`] notes, so an answer cut at the tool-output cap keeps its related places. Off: as before.
fn related_first() -> bool {
    std::env::var("BYTEASK_CHECK_RELATED_FIRST")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
}

/// The most "Checks:" notes listed with `BYTEASK_CHECK_RELATED_FIRST`; the rest are counted.
const NOTES_CAP: usize = 12;

/// `section` (a related-places paragraph, starting with a newline) at the end of `text` (off), or before
/// its "Checks:" line, else before its "Not checked:" line, else at the end (on).
fn place_related(text: &mut String, section: &str, first: bool) {
    if !first {
        text.push_str(section);
        return;
    }
    match ["\nChecks: ", "\nNot checked: "]
        .iter()
        .find_map(|mark| text.find(mark))
    {
        Some(at) => text.insert_str(at + 1, section.trim_start_matches('\n')),
        None => text.push_str(section),
    }
}

/// The "Checks:" notes, at most [`NOTES_CAP`] when `cap` (the rest counted, not listed).
fn capped_notes(notes: &[String], cap: bool) -> String {
    if cap && notes.len() > NOTES_CAP {
        format!(
            "{}; and {} more not listed",
            notes[..NOTES_CAP].join("; "),
            notes.len() - NOTES_CAP
        )
    } else {
        notes.join("; ")
    }
}

fn render(outcome: &gate::GateOutcome, budget: Duration) -> String {
    let mut out = format!(
        "Finish check (advisory: {}, {}s limit, nothing deleted)",
        if outcome.targeted {
            "builds and runs only what the change reaches"
        } else {
            "build + tests"
        },
        budget.as_secs()
    );
    if outcome.problems.is_empty() && outcome.still_open.is_empty() {
        out.push_str(": no problem found.\n");
    } else {
        out.push_str(":\n");
        for (n, problem) in outcome
            .problems
            .iter()
            .chain(outcome.still_open.iter())
            .enumerate()
        {
            out.push_str(&format!("{}. {}\n", n + 1, problem.text));
        }
    }
    if !outcome.notes.is_empty() {
        out.push_str(&format!(
            "Checks: {}\n",
            capped_notes(&outcome.notes, related_first())
        ));
    }
    if !outcome.not_checked.is_empty() {
        out.push_str(&format!(
            "Not checked: {}\n",
            outcome.not_checked.join("; ")
        ));
    }
    crate::impact::truncate_chars(out.trim_end(), 6_500)
}

#[cfg(test)]
#[path = "finish_check_tests.rs"]
mod tests;
