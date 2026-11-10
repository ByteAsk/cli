//! The finish gate for agents that are not the ByteAsk CLI: Claude Code and Codex run these
//! from their own hooks, through `byteask toolbox hook
//! <event>`, which `byteask toolbox install` writes into their settings.
//!
//! - `SessionStart`: remember the work tree as the session starts (the gate compares
//!   against it, exactly like the CLI's session-start snapshot).
//! - `Stop`: run the gate on what the agent changed, only when the automatic gate is switched
//!   on (`BYTEASK_CPP_GATE=1`; off by default since 0.2.0). FAIL sends the failure back to the agent
//!   (the turn continues), up to the retry limit; then the turn ends with FAIL shown. NOT RUN
//!   reports its reason and lets the turn end (it never loops). Every verdict goes to the
//!   results store. The check is the fast tier: [`FAST_TIER`] at most; the Stop hook's
//!   own timeout is set above it at install time.
//! - `UserPromptSubmit`: tell the agent the latest result once, marked stale if the code
//!   changed since, within a 2 s budget and never blocking the prompt.
//!
//! State per agent session lives next to the results store:
//! `$BYTEASK_HOME/results/<project-key>/sessions/<session>.json`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use check_result::CheckResult;
use serde::Deserialize;
use serde::Serialize;

use crate::gate;
use crate::results_store;

/// The check a turn waits for is bounded; whatever does not fit is NOT RUN (time limit)
/// for now and is left to the background follow-up.
pub const FAST_TIER: Duration = Duration::from_secs(90);
/// The prompt hook's whole budget.
pub const PROMPT_BUDGET: Duration = Duration::from_secs(2);

/// What a hook knows about its call.
pub struct HookContext {
    /// `$BYTEASK_HOME`.
    pub home: PathBuf,
    /// The agent's working directory.
    pub cwd: PathBuf,
    /// The agent's session id (any string; sanitised for the file name).
    pub session_id: String,
    /// `claude-code` or `codex`, recorded with each result.
    pub source: String,
    /// The project may run native builds (the same trust rule as the CLI).
    pub native_exec: bool,
    /// The engine's sandbox helper, for the Build profile.
    pub sandbox_helper: Option<PathBuf>,
    /// The user relaxed the sandbox for this project.
    pub relaxed: bool,
    /// The fast tier, if not the configured one (`BYTEASK_CPP_GATE_FAST_SECS`, default
    /// [`FAST_TIER`]).
    pub fast_tier: Option<Duration>,
}

/// The Stop hook's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum StopDecision {
    /// Let the turn end; show `message` to the user if there is one. `follow_up`: the check
    /// did not fit the fast tier, so the caller starts `toolbox hook follow-up` in the
    /// background, which finishes it with the whole budget.
    Allow {
        message: Option<String>,
        follow_up: bool,
    },
    /// Keep the agent working: `reason` goes back to it.
    Block { reason: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionState {
    snapshot: Option<gate::Snapshot>,
    /// Gate feedback rounds since the user's last prompt.
    rounds: u32,
    /// The last verdict and the content key it is for.
    last: Option<(String, CheckResult)>,
    /// The feedback sent with that verdict (re-sent, not rebuilt, while the code is unchanged).
    last_feedback: Option<String>,
    /// The content key that passed.
    passed: Option<(String, CheckResult)>,
    /// The last result the prompt hook reported (`<content key>@<at>`).
    shown: Option<String>,
    /// The content key a background full check was started for.
    pending_full: Option<String>,
    /// When the current turn started (the prompt hook; the session start before the
    /// first prompt). The AI's first edit in a turn is recorded at this time: the hooks see
    /// the change only when the turn ends, and the edit cannot have come before the turn.
    turn_started: Option<u64>,
    /// The agent's changes were already recorded as an edit for this content key.
    edit_recorded: Option<String>,
    /// The "why is the build slow" card was shown in this session.
    #[serde(default)]
    slow_card_shown: bool,
}

fn session_file(home: &Path, root: &Path, session_id: &str) -> PathBuf {
    let name: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(80)
        .collect();
    let name = if name.is_empty() {
        "session".to_string()
    } else {
        name
    };
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    home.join("results")
        .join(crate::builds::project_key(&root))
        .join("sessions")
        .join(format!("{name}.json"))
}

fn load(path: &Path) -> SessionState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(path: &Path, state: &SessionState) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string(state) {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

async fn repo_root(cwd: &Path) -> Option<PathBuf> {
    crate::gitx::toplevel(cwd).await
}

/// SessionStart: remember the work tree. A resumed or compacted session keeps the snapshot it
/// started with.
pub async fn session_start(ctx: &HookContext, fresh_start: bool) {
    let Some(root) = repo_root(&ctx.cwd).await else {
        return;
    };
    let path = session_file(&ctx.home, &root, &ctx.session_id);
    let mut state = load(&path);
    if state.snapshot.is_some() && !fresh_start {
        return;
    }
    let Some(snapshot) = gate::take_snapshot(&root).await else {
        return;
    };
    state = SessionState {
        snapshot: Some(snapshot),
        turn_started: Some(results_store::now_unix()),
        ..SessionState::default()
    };
    save(&path, &state);
    // The code as it is now is where this session's change starts.
    ::results_store::begin_agent_session(&ctx.home, &ctx.session_id, &root);
}

/// Stop: run the gate (fast tier) and decide whether the turn may end.
pub async fn stop(ctx: &HookContext) -> StopDecision {
    let allow = |message: Option<String>| StopDecision::Allow {
        message,
        follow_up: false,
    };
    let Some(root) = repo_root(&ctx.cwd).await else {
        return allow(None);
    };
    let path = session_file(&ctx.home, &root, &ctx.session_id);
    let mut state = load(&path);
    let Some(snapshot) = state.snapshot.clone() else {
        // No SessionStart snapshot: what the agent changed is unknown, so nothing is claimed.
        return allow(None);
    };
    let changes = gate::agent_changes(&snapshot).await;
    if changes.is_empty() {
        return allow(None);
    }
    // The agent changed code in this turn (once per version of its changes).
    if state.edit_recorded.as_deref() != Some(changes.content_key.as_str()) {
        let at = state.turn_started.unwrap_or_else(results_store::now_unix);
        ::results_store::record_agent_edit(&ctx.home, &ctx.session_id, &root, at);
        state.edit_recorded = Some(changes.content_key.clone());
        save(&path, &state);
    }
    let mut settings = crate::workflow::settings_from_env();
    // The automatic gate runs only when switched on (`BYTEASK_CPP_GATE=1`).
    if !settings.gate {
        return allow(None);
    }
    if let Some(fast) = ctx.fast_tier {
        settings.fast_tier = fast;
    }
    if settings.fast_tier > Duration::ZERO {
        settings.budget = settings.budget.min(settings.fast_tier);
    }
    let key = changes.content_key.clone();
    let notice =
        |result: &CheckResult| Some(format!("ByteAsk finish gate: {}", result.render_line()));

    if let Some((passed, result)) = &state.passed
        && *passed == key
    {
        return allow(notice(result));
    }
    // A person skipped the check for exactly this version: NOT RUN (skipped), never
    // PASS, never a loop.
    if let Some(skipped) = crate::skip::active(&ctx.home, &root, gate::CHECK).await {
        let fresh = results_store::fingerprint(&root, PROMPT_BUDGET)
            .await
            .unwrap_or_default();
        record(ctx, &root, &key, &skipped, fresh);
        state.last = Some((key, skipped.clone()));
        state.last_feedback = None;
        save(&path, &state);
        return allow(notice(&skipped));
    }
    if state.pending_full.as_deref() == Some(key.as_str()) && follow_up_running(&path) {
        let result = CheckResult::not_run(
            gate::CHECK,
            check_result::NotRunReason::TimeLimit,
            "The full check of this version is still running in the background.",
            "carry on; its result is shown at your next prompt (check_status shows it too).",
        );
        return allow(notice(&result));
    }
    if state.rounds >= settings.max_loops {
        let result =
            crate::workflow::retry_limit_result(state.last.as_ref(), &key, settings.max_loops);
        let fresh = results_store::fingerprint(&root, PROMPT_BUDGET)
            .await
            .unwrap_or_default();
        record(ctx, &root, &key, &result, fresh);
        state.rounds = 0;
        save(&path, &state);
        return allow(notice(&result));
    }
    if let (Some((last_key, last)), Some(feedback)) = (&state.last, &state.last_feedback)
        && *last_key == key
        && last.verdict == check_result::Verdict::Fail
    {
        // Unchanged since the failure was sent: the same failure, without rebuilding.
        let reason = feedback.clone();
        state.rounds += 1;
        save(&path, &state);
        return StopDecision::Block { reason };
    }

    let fresh = results_store::fingerprint(&root, PROMPT_BUDGET)
        .await
        .unwrap_or_default();
    let sandbox = Arc::new(crate::sandbox::Settings {
        helper: ctx.sandbox_helper.clone(),
        workspace: root.clone(),
        relaxed: ctx.relaxed,
        required: byteask_policy::sandbox_required(&ctx.home).0,
    });
    if let Err(result) = crate::sandbox::check_available(gate::CHECK, &sandbox) {
        // No sandbox here, so nothing is built; the turn ends with the fix.
        record(ctx, &root, &key, &result, fresh);
        state.last = Some((key, result.clone()));
        save(&path, &state);
        return allow(notice(&result));
    }
    let mut base = gate::BaseCache::default();
    let mut memo = None;
    let mut advisories = gate::Advisories::default();
    let run = gate::run_gate(gate::GateInput {
        snapshot: &snapshot,
        changes: &changes,
        settings: &settings,
        native_exec: ctx.native_exec,
        non_interactive: false,
        impact_state: None,
        base: &mut base,
        assistant_text: "",
        memo: &mut memo,
        advisories: &mut advisories,
        crash: None,
        session_left: settings.session_cap,
        advisory: false,
    });
    // `byteask checks cancel` stops it within 2 s; NOT RUN (cancelled), never PASS.
    let Some(outcome) = crate::cancel::unless_requested(
        &ctx.home,
        &root,
        crate::sandbox::scope(gate::CHECK, sandbox, run),
    )
    .await
    else {
        let result = crate::cancel::cancelled(gate::CHECK, "`byteask checks cancel` was run");
        record(ctx, &root, &key, &result, String::new());
        state.last = Some((key, result.clone()));
        state.last_feedback = None;
        save(&path, &state);
        return allow(notice(&result));
    };
    let after = gate::agent_changes(&snapshot).await;
    let moved = after.content_key != key;
    let result = crate::workflow::bound_to_checked_code(outcome.result(), &key, &after.content_key);
    record(
        ctx,
        &root,
        &key,
        &result,
        if moved { String::new() } else { fresh },
    );
    state.last = Some((key.clone(), result.clone()));
    if gate::needs_feedback(&outcome) && !moved {
        state.rounds += 1;
        let reason = gate::render_feedback(&outcome, state.rounds, settings.max_loops);
        state.last_feedback = Some(reason.clone());
        save(&path, &state);
        return StopDecision::Block { reason };
    }
    if result.is_pass() {
        state.passed = Some((key.clone(), result.clone()));
    }
    state.last_feedback = None;
    let follow_up =
        !moved && crate::workflow::ran_out_of_time(&result) && settings.fast_tier > Duration::ZERO;
    if follow_up {
        state.pending_full = Some(key);
    }
    let mut message = notice(&result);
    // A check that ran out of time says why the build is slow, once per session.
    if crate::workflow::ran_out_of_time(&result)
        && !state.slow_card_shown
        && let Some(card) = gate::slow_build_card(&snapshot)
    {
        state.slow_card_shown = true;
        message = message.map(|line| format!("{line}\n{card}"));
    }
    save(&path, &state);
    StopDecision::Allow { message, follow_up }
}

fn follow_up_lock(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("follow-up.lock"))
        .ok()
}

/// A background full check holds this session's follow-up lock.
fn follow_up_running(path: &Path) -> bool {
    follow_up_lock(path).is_some_and(|lock| lock.try_lock().is_err())
}

/// The background follow-up after a Stop hook ran out of the fast tier. Finishes the
/// check of the same code with the whole budget, stores it (tier `full`), and makes it this
/// code's verdict for the next Stop (FAIL then goes back to the agent; PASS ends the turn).
pub async fn follow_up(ctx: &HookContext) {
    let Some(root) = repo_root(&ctx.cwd).await else {
        return;
    };
    let path = session_file(&ctx.home, &root, &ctx.session_id);
    let Some(lock) = follow_up_lock(&path) else {
        return;
    };
    if lock.try_lock().is_err() {
        return;
    }
    let state = load(&path);
    let (Some(snapshot), Some(pending)) = (state.snapshot.clone(), state.pending_full.clone())
    else {
        return;
    };
    let settings = crate::workflow::settings_from_env();
    let fresh = results_store::fingerprint(&root, PROMPT_BUDGET)
        .await
        .unwrap_or_default();
    // The agent keeps working while this runs, so it builds a copy of the code taken now.
    let Some(mirror) = gate::mirror_snapshot(&snapshot).await else {
        return;
    };
    let changes = gate::agent_changes(&mirror).await;
    if changes.content_key != pending {
        // Edited since the Stop: the next Stop checks the new version.
        return;
    }
    let sandbox = Arc::new(crate::sandbox::Settings {
        helper: ctx.sandbox_helper.clone(),
        workspace: root.clone(),
        relaxed: ctx.relaxed,
        required: byteask_policy::sandbox_required(&ctx.home).0,
    });
    if crate::sandbox::check_available(gate::CHECK, &sandbox).is_err() {
        return;
    }
    let sandbox = Arc::new(crate::sandbox::Settings {
        workspace: mirror.root.clone(),
        ..(*sandbox).clone()
    });
    let mut base = gate::BaseCache::default();
    let mut memo = None;
    let mut advisories = gate::Advisories::default();
    let run = gate::run_gate(gate::GateInput {
        snapshot: &mirror,
        changes: &changes,
        settings: &settings,
        native_exec: ctx.native_exec,
        non_interactive: false,
        impact_state: None,
        base: &mut base,
        assistant_text: "",
        memo: &mut memo,
        advisories: &mut advisories,
        crash: None,
        session_left: settings.session_cap,
        advisory: false,
    });
    let Some(mut outcome) = crate::cancel::unless_requested(
        &ctx.home,
        &root,
        crate::sandbox::scope(gate::CHECK, sandbox, run),
    )
    .await
    else {
        let result = crate::cancel::cancelled(gate::CHECK, "`byteask checks cancel` was run");
        results_store::record(
            &ctx.home,
            results_store::Entry {
                result: result.clone().with_content_key(&pending),
                root: root.clone(),
                at: results_store::now_unix(),
                fresh: String::new(),
                tier: "full".to_string(),
                source: ctx.source.clone(),
            },
        );
        let mut state = load(&path);
        state.pending_full = None;
        state.last = Some((pending, result));
        state.last_feedback = None;
        save(&path, &state);
        let _ = lock.unlock();
        return;
    };
    outcome.map_paths(&mirror.root, &root);
    // Bound to the copy's content key by construction: edits made meanwhile make it
    // stale (the fingerprint no longer matches), never wrong.
    let result = outcome.result();
    results_store::record_for_session(
        &ctx.home,
        results_store::Entry {
            result: result.clone().with_content_key(&pending),
            root: root.clone(),
            at: results_store::now_unix(),
            fresh,
            tier: "full".to_string(),
            source: ctx.source.clone(),
        },
        &ctx.session_id,
    );
    // Re-read: the session moved on while this ran (a prompt reset its retry count).
    let mut state = load(&path);
    state.pending_full = None;
    state.last = Some((pending.clone(), result.clone()));
    state.last_feedback = gate::needs_feedback(&outcome)
        .then(|| gate::render_feedback(&outcome, state.rounds + 1, settings.max_loops));
    if result.is_pass() {
        state.passed = Some((pending, result));
    }
    save(&path, &state);
    let _ = lock.unlock();
}

/// The agent stopped the hook (SIGTERM on an interrupt): record NOT RUN (cancelled) for the
/// version the Stop was checking, so nothing reads as checked. Fast: git only.
pub async fn record_cancelled(ctx: &HookContext, how: &str) {
    let Some(root) = repo_root(&ctx.cwd).await else {
        return;
    };
    let path = session_file(&ctx.home, &root, &ctx.session_id);
    let mut state = load(&path);
    let Some(snapshot) = state.snapshot.clone() else {
        return;
    };
    let key = gate::agent_changes(&snapshot).await.content_key;
    let result = crate::cancel::cancelled(gate::CHECK, how);
    record(ctx, &root, &key, &result, String::new());
    state.last = Some((key, result));
    state.last_feedback = None;
    save(&path, &state);
}

fn record(ctx: &HookContext, root: &Path, key: &str, result: &CheckResult, fresh: String) {
    results_store::record_for_session(
        &ctx.home,
        results_store::Entry {
            result: result.clone().with_content_key(key),
            root: root.to_path_buf(),
            at: results_store::now_unix(),
            fresh,
            tier: "fast".to_string(),
            source: ctx.source.clone(),
        },
        &ctx.session_id,
    );
}

/// UserPromptSubmit: the latest result, once per result, with its freshness. A new prompt also
/// starts a new turn, so the retry count starts again.
pub async fn prompt(ctx: &HookContext) -> Option<String> {
    let root = tokio::time::timeout(PROMPT_BUDGET, repo_root(&ctx.cwd))
        .await
        .ok()
        .flatten()?;
    let path = session_file(&ctx.home, &root, &ctx.session_id);
    let mut state = load(&path);
    state.rounds = 0;
    state.turn_started = Some(results_store::now_unix());
    let entry = results_store::latest(&ctx.home, &root);
    let shown = entry.as_ref().map(|entry| {
        format!(
            "{}@{}",
            entry.result.content_key.clone().unwrap_or_default(),
            entry.at
        )
    });
    let text = match (&entry, &shown) {
        (Some(entry), Some(key)) if state.shown.as_deref() != Some(key.as_str()) => {
            let freshness = results_store::freshness(entry, PROMPT_BUDGET).await;
            state.shown = Some(key.clone());
            Some(format!(
                "ByteAsk check status: {}",
                results_store::describe(entry, freshness, results_store::now_unix())
            ))
        }
        _ => None,
    };
    save(&path, &state);
    text
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;
