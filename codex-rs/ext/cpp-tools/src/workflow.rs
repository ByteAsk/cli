//! Engine-forced C/C++ workflow: the automatic change-impact checklist and the finish gate.
//!
//! - After the first tool step in a turn that leaves a C/C++ file changed (relative to the
//!   session start), `cpp_impact` runs and its checklist is recorded into the conversation
//!   once per turn.
//! - When the model ends its turn, the finish gate runs (see [`crate::gate`]); failures are
//!   recorded as one message and the turn continues, at most `max_loops` times per turn.
//! - The C/C++ guidance names only what this thread's workflow enforces ([`Workflow::enforcement`]
//!   selects the variant in [`crate::guidance::cpp_guidance`]).
//!
//! Both are off by default since 0.2.0 and switched on with the `[byteask_cpp]` config table or
//! `BYTEASK_CPP_*` environment variables (root threads of C/C++ workspaces only). The workflow
//! itself exists in every C/C++ root thread: its session-start snapshot is what
//! `cpp_finish_check` compares the change with.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_extension_api::ContentItemKind;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ConversationHistorySnapshot;
use codex_extension_api::TurnGateFragments;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

use crate::gate;
use crate::gate::GateSettings;
use crate::guidance::Enforcement;
use crate::impact;

/// Title line of the automatic checklist (also how the gate finds it in the history).
pub(crate) const IMPACT_TITLE: &str = "cpp_impact (automatic, after your first C/C++ edit)";
const GATE_IMPACT_TITLE: &str = "cpp_impact at finish";

/// A model-visible message recorded by the workflow.
pub(crate) struct WorkflowFragment {
    kind: &'static str,
    text: String,
}

impl ContextualUserFragment for WorkflowFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(format!("cpp_tools.{}", self.kind))
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

pub(crate) fn fragment(kind: &'static str, text: String) -> TurnGateFragments {
    vec![Box::new(WorkflowFragment { kind, text })]
}

/// Checklist bookkeeping across the automatic checklist and the finish gate.
#[derive(Debug, Default)]
pub(crate) struct ImpactState {
    pub numbering: impact::Numbering,
    /// Keys already shown to the model.
    pub listed: BTreeSet<String>,
    /// Keys re-asked at a finish gate already (asked at most once there).
    pub reasked: HashSet<String>,
}

impl ImpactState {
    /// Record that these items were shown.
    pub fn mark_listed(&mut self, report: &impact::ImpactReport) {
        for item in &report.items {
            self.numbering.number(&item.key);
            self.listed.insert(item.key.clone());
        }
    }

    /// The finish-gate part of the checklist: new items, and listed items that have neither
    /// an answer (`[n]` in an assistant message) nor an edit at their location. `None` when
    /// nothing needs an answer.
    pub fn finish_delta(
        &mut self,
        report: &impact::ImpactReport,
        assistant_text: &str,
    ) -> Option<String> {
        let answered = answered_numbers(assistant_text);
        let mut wanted: HashSet<String> = HashSet::new();
        for item in &report.items {
            if self.reasked.contains(&item.key) {
                continue;
            }
            if self.listed.contains(&item.key) {
                let number = self.numbering.number(&item.key);
                if answered.contains(&number) || edited_at(report, item) {
                    continue;
                }
            } else if edited_at(report, item) {
                continue;
            }
            wanted.insert(item.key.clone());
        }
        if wanted.is_empty() {
            return None;
        }
        let text = impact::render(
            report,
            &mut self.numbering,
            GATE_IMPACT_TITLE,
            Some(&wanted),
        );
        for key in &wanted {
            self.listed.insert(key.clone());
            self.reasked.insert(key.clone());
        }
        Some(format!(
            "CHANGE-IMPACT ITEMS WITHOUT AN ANSWER ({} item(s)); answer each in your final message:\n{text}",
            wanted.len()
        ))
    }
}

/// An item whose location lies inside an entity the diff changed counts as edited.
fn edited_at(report: &impact::ImpactReport, item: &impact::Item) -> bool {
    report.changed.iter().any(|c| {
        c.path == item.path && c.entity.line <= item.line && item.line <= c.entity.end_line
    })
}

/// `[n]` markers in the text.
pub(crate) fn answered_numbers(text: &str) -> HashSet<usize> {
    let mut out = HashSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b']' {
                if let Ok(n) = text[i + 1..j].parse::<usize>() {
                    out.insert(n);
                }
                i = j;
            }
        }
        i += 1;
    }
    out
}

/// Words that make a request a crash / memory-error report (generic C/C++ vocabulary).
const CRASH_CUES: &[&str] = &[
    "crash",
    "segfault",
    "segmentation fault",
    "sanitizer",
    "use-after-free",
    "use after free",
    "buffer overflow",
    "buffer-overflow",
    "out-of-bounds",
    "out of bounds",
    "double free",
    "double-free",
    "null pointer dereference",
    "memory corruption",
    "memory leak",
];

/// The session's original crash, for the finish gate's patch-shape critic. The session is a
/// crash fix when the request (a user message) carries a sanitizer report or names a crash or
/// memory error; the crash stack is the first sanitizer report in the request or, failing
/// that, in a tool output (the reproduction). `None` otherwise: a report that only shows up
/// in an ordinary task's test run (say, a pre-existing leak) does not make it a crash fix.
pub(crate) fn crash_context(history: &dyn ConversationHistorySnapshot) -> Option<gate::CrashContext> {
    let mut crash_task = false;
    let mut functions: Option<Vec<String>> = None;
    let mut shown_elsewhere = String::new();
    for item in history.items() {
        let (text, from_user) = match item {
            ResponseItem::Message { role, content, .. } if role == "user" => (
                content
                    .iter()
                    .filter_map(|c| match c {
                        ContentItem::InputText { text } if !is_context_block(text) => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                true,
            ),
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                (output.body.to_text().unwrap_or_default(), false)
            }
            _ => continue,
        };
        let report = if functions.is_none() {
            crate::memsafety::evidence::parse(&text)
        } else {
            None
        };
        if from_user {
            let lower = text.to_lowercase();
            crash_task |= report.is_some() || CRASH_CUES.iter().any(|cue| lower.contains(cue));
        }
        if let Some(report) = report {
            functions = Some(report.crash_functions());
        }
        if !from_user && text.contains("patch-shape review") {
            shown_elsewhere.push_str(&text);
            shown_elsewhere.push('\n');
        }
    }
    crash_task.then(|| gate::CrashContext {
        functions: functions.unwrap_or_default(),
        shown_elsewhere,
    })
}

/// Context the engine adds as user-role text (environment, AGENTS.md and other instruction
/// blocks, internal context), which is not the request itself.
fn is_context_block(text: &str) -> bool {
    let text = text.trim_start();
    let first_line = text.lines().next().unwrap_or("").trim_end();
    text.starts_with("# AGENTS.md instructions")
        || (first_line.starts_with('<') && first_line.ends_with('>'))
}

/// Assistant text after the first automatic checklist (the answers to it).
pub(crate) fn assistant_text_after_checklist(history: &dyn ConversationHistorySnapshot) -> String {
    let mut seen_checklist = false;
    let mut out = String::new();
    for item in history.items() {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if role == "developer"
            && (text.starts_with(IMPACT_TITLE) || text.contains(GATE_IMPACT_TITLE))
        {
            seen_checklist = true;
            continue;
        }
        if seen_checklist && role == "assistant" {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// Per-thread workflow state.
pub(crate) struct Workflow {
    pub settings: GateSettings,
    pub native_exec: bool,
    pub non_interactive: bool,
    pub cwd: PathBuf,
    started: std::time::Instant,
    /// Session-start snapshots of the repositories the workflow applies to: the work tree
    /// containing `cwd`, else the work trees below it (a workspace that is a parent of the
    /// repository), at most [`MAX_REPOS`].
    snapshot: Arc<tokio::sync::OnceCell<Vec<Arc<gate::Snapshot>>>>,
    /// A tool ran since the last check (it may have edited files).
    maybe_edited: AtomicBool,
    inner: tokio::sync::Mutex<Inner>,
    /// The sandbox the gate's builds and tests run in (set by the extension at thread
    /// start; unset in unit tests, where programs run as before).
    sandbox: std::sync::OnceLock<Arc<crate::sandbox::Settings>>,
    /// The content key a background full check is running for, if one is.
    background_running: Arc<std::sync::Mutex<Option<String>>>,
    /// A finished background full check, with the content key it checked.
    background_done: Arc<std::sync::Mutex<Option<(String, gate::GateOutcome)>>>,
    /// The session (thread id) the gate's verdicts are recorded under in the results
    /// store's per-change log (set by the extension at thread start).
    session: Arc<std::sync::OnceLock<String>>,
    /// `cpp_finish_check`'s state between calls in this thread.
    finish: tokio::sync::Mutex<crate::tools::FinishState>,
}

#[derive(Default)]
struct Inner {
    impact: ImpactState,
    /// Turns in which the automatic checklist was shown.
    t1_turns: HashSet<String>,
    /// Change set of the last checklist attempt that found nothing to list (retry only
    /// after further edits).
    t1_last_empty: Option<u64>,
    /// Gate feedback rounds per turn.
    gate_rounds: HashMap<String, u32>,
    /// Content key of the last change set that PASSED the gate, with that result.
    passed: Option<(String, check_result::CheckResult)>,
    /// The last verdict the gate reached in this session, with the content key it is for.
    last: Option<(String, check_result::CheckResult)>,
    /// Per turn: the verdict line to show the user when the turn ends (`stop_notice`).
    notice: HashMap<String, String>,
    base: gate::BaseCache,
    /// Build/test outcome of the last change set the gate checked.
    memo: Option<gate::BuildMemo>,
    advisories: gate::Advisories,
    /// The "why is the build slow" card was shown in this session.
    slow_card_shown: bool,
}

impl Workflow {
    /// Create the state and start capturing the session-start snapshot in the background.
    pub fn start(
        settings: GateSettings,
        native_exec: bool,
        non_interactive: bool,
        cwd: PathBuf,
    ) -> Arc<Self> {
        let workflow = Arc::new(Self {
            settings,
            native_exec,
            non_interactive,
            cwd,
            started: std::time::Instant::now(),
            snapshot: Arc::new(tokio::sync::OnceCell::new()),
            maybe_edited: AtomicBool::new(false),
            inner: tokio::sync::Mutex::new(Inner::default()),
            sandbox: std::sync::OnceLock::new(),
            background_running: Arc::default(),
            background_done: Arc::default(),
            session: Arc::default(),
            finish: tokio::sync::Mutex::default(),
        });
        // Always: `cpp_finish_check` needs the session-start state even when the automatic gate
        // and checklist are off.
        {
            let cell = Arc::clone(&workflow.snapshot);
            let cwd = workflow.cwd.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    cell.get_or_init(|| async move { take_snapshots(&cwd).await })
                        .await;
                });
            }
        }
        workflow
    }

    async fn snapshots(&self) -> Vec<Arc<gate::Snapshot>> {
        let cwd = self.cwd.clone();
        self.snapshot
            .get_or_init(|| async move { take_snapshots(&cwd).await })
            .await
            .clone()
    }

    /// The snapshot of the repository being worked on: the only one, else the first whose
    /// tree the agent changed, else the first.
    pub async fn snapshot(&self) -> Option<Arc<gate::Snapshot>> {
        let all = self.snapshots().await;
        if all.len() > 1 {
            for snapshot in &all {
                if !gate::agent_changes(snapshot).await.is_empty() {
                    return Some(Arc::clone(snapshot));
                }
            }
        }
        all.into_iter().next()
    }

    /// What this thread's workflow enforces, for the guidance variant. Both parts need the
    /// session-start snapshot (a git work tree with a commit); the full suite also needs the
    /// gate's builds and tests to be on and allowed. The snapshot is started at thread start;
    /// if it is still being taken after a few seconds, the repository is assumed usable.
    pub async fn enforcement(&self) -> Enforcement {
        let has_snapshot =
            match tokio::time::timeout(std::time::Duration::from_secs(5), self.snapshot()).await {
                Ok(snapshot) => snapshot.is_some(),
                Err(_) => true,
            };
        Enforcement {
            checklist: has_snapshot && self.settings.impact,
            finish_suite: has_snapshot && self.gate_runs_suite(),
        }
    }

    /// The finish gate will build everything and run the whole suite (settings and trust; the
    /// git snapshot is checked by [`Self::enforcement`]).
    pub fn gate_runs_suite(&self) -> bool {
        self.settings.gate && self.settings.tests && self.settings.max_loops > 0 && self.native_exec
    }

    /// Render a `cpp_impact` tool result with the checklist's numbering (and record its items
    /// as listed), so `[n]` answers mean the same item in the automatic checklist, a manual
    /// call and the finish gate.
    pub async fn render_shared(&self, report: &impact::ImpactReport, title: &str) -> String {
        let mut inner = self.inner.lock().await;
        let text = impact::render(report, &mut inner.impact.numbering, title, None);
        inner.impact.mark_listed(report);
        text
    }

    /// The full check of `changes` continues in the background with the whole budget;
    /// its verdict goes to the results store (tier `full`) and becomes this code's verdict at
    /// the next finish. One at a time.
    fn start_background(
        &self,
        snapshot: &Arc<gate::Snapshot>,
        changes: &gate::AgentChanges,
        settings: GateSettings,
        left: std::time::Duration,
    ) {
        {
            let mut running = lock(&self.background_running);
            if running.is_some() {
                return;
            }
            *running = Some(changes.content_key.clone());
        }
        let snapshot = Arc::clone(snapshot);
        let changes = changes.clone();
        let native_exec = self.native_exec;
        let sandbox = self.sandbox.get().cloned();
        let running = Arc::clone(&self.background_running);
        let done = Arc::clone(&self.background_done);
        let session = Arc::clone(&self.session);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            *lock(&running) = None;
            return;
        };
        handle.spawn(async move {
            let key = changes.content_key.clone();
            let fresh = crate::results_store::fingerprint(&snapshot.root, FRESHNESS_BUDGET)
                .await
                .unwrap_or_default();
            // The model keeps working while this runs, so the check builds a copy of
            // the code taken now, and its result belongs to exactly that code.
            let mirror = match gate::mirror_snapshot(&snapshot).await {
                Some(mirror) => mirror,
                None => {
                    *lock(&running) = None;
                    return;
                }
            };
            let mirror_changes = gate::agent_changes(&mirror).await;
            if mirror_changes.content_key != key {
                // Edited while the copy was taken: the next finish checks the new code.
                *lock(&running) = None;
                return;
            }
            let mut base = gate::BaseCache::default();
            let mut memo = None;
            let mut advisories = gate::Advisories::default();
            let run = gate::run_gate(gate::GateInput {
                snapshot: &mirror,
                changes: &mirror_changes,
                settings: &settings,
                native_exec,
                non_interactive: false,
                impact_state: None,
                base: &mut base,
                assistant_text: "",
                memo: &mut memo,
                advisories: &mut advisories,
                crash: None,
                session_left: left,
                advisory: false,
            });
            let sandbox = sandbox.map(|sandbox| {
                Arc::new(crate::sandbox::Settings {
                    workspace: mirror.root.clone(),
                    ..(*sandbox).clone()
                })
            });
            let mut outcome = match sandbox {
                Some(sandbox) => crate::sandbox::scope(gate::CHECK, sandbox, run).await,
                None => run.await,
            };
            outcome.map_paths(&mirror.root, &snapshot.root);
            // Bound to the copy's content key by construction: edits made meanwhile make
            // it stale (the fingerprint no longer matches), never wrong.
            let result = outcome.result();
            store_result_tier(
                &snapshot,
                &key,
                &result,
                fresh,
                "full",
                session.get().map(String::as_str),
            );
            *lock(&done) = Some((key, outcome));
            *lock(&running) = None;
        });
    }

    /// Run the gate's builds and tests in this sandbox (the Build profile).
    pub fn set_sandbox(&self, settings: Arc<crate::sandbox::Settings>) {
        let _ = self.sandbox.set(settings);
    }

    /// Record the gate's verdicts under this session (the thread id) in the results
    /// store's per-change log, where the value report reads them.
    pub fn set_session(&self, session: &str) {
        let _ = self.session.set(session.to_string());
    }

    fn session(&self) -> Option<&str> {
        self.session.get().map(String::as_str)
    }

    /// `cpp_finish_check`'s state for this thread (one check at a time).
    pub(crate) async fn finish_state(&self) -> tokio::sync::MutexGuard<'_, crate::tools::FinishState> {
        self.finish.lock().await
    }

    pub fn note_tool_call(&self) {
        self.maybe_edited.store(true, Ordering::Release);
    }

    /// The automatic checklist after the first C/C++ edit of the turn.
    pub async fn after_step(&self, turn_id: &str) -> TurnGateFragments {
        if !self.settings.impact || !self.maybe_edited.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        if self.inner.lock().await.t1_turns.contains(turn_id) {
            return Vec::new();
        }
        let Some(snapshot) = self.snapshot().await else {
            return Vec::new();
        };
        let changes = gate::agent_changes(&snapshot).await;
        if !changes.touches_tracked_cpp()
            || self.inner.lock().await.t1_last_empty == Some(changes.fingerprint)
        {
            return Vec::new();
        }
        let req = impact::ImpactRequest {
            root: snapshot.root.clone(),
            base_rev: snapshot.head.clone(),
            source: impact::Source::Diff,
            budget: std::time::Duration::from_secs(60),
            use_clangd: true,
            max_items: impact::DEFAULT_MAX_ITEMS,
            preexisting_untracked: snapshot.untracked.iter().cloned().collect(),
        };
        let report = impact::analyze(&req).await;
        let mut inner = self.inner.lock().await;
        if report.changed.is_empty() || report.items.is_empty() {
            // Nothing to answer yet: stay silent and look again after the next edit.
            inner.t1_last_empty = Some(changes.fingerprint);
            return Vec::new();
        }
        inner.t1_turns.insert(turn_id.to_string());
        let text = impact::render(&report, &mut inner.impact.numbering, IMPACT_TITLE, None);
        inner.impact.mark_listed(&report);
        crate::report::log_tool_outcome("cpp_impact", "auto");
        fragment("impact", text)
    }

    /// The finish gate. Empty when the turn may end.
    pub async fn before_stop(
        &self,
        turn_id: &str,
        history: Arc<dyn ConversationHistorySnapshot>,
    ) -> TurnGateFragments {
        if !self.settings.gate {
            return Vec::new();
        }
        let rounds = self
            .inner
            .lock()
            .await
            .gate_rounds
            .get(turn_id)
            .copied()
            .unwrap_or(0);
        let Some(snapshot) = self.snapshot().await else {
            return Vec::new();
        };
        let changes = gate::agent_changes(&snapshot).await;
        let mut inner = self.inner.lock().await;
        if rounds >= self.settings.max_loops {
            // The retry limit ends the turn, but the change keeps its verdict. Still
            // failing = FAIL; edited after the last check = NOT RUN; never PASS unchecked.
            let result = retry_limit_result(
                inner.last.as_ref(),
                &changes.content_key,
                self.settings.max_loops,
            );
            crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&result));
            write_verdict_record(&snapshot, turn_id, rounds, &changes.content_key, &result);
            let fresh = crate::results_store::fingerprint(&snapshot.root, FRESHNESS_BUDGET)
                .await
                .unwrap_or_default();
            store_result(&snapshot, &changes.content_key, &result, fresh, self.session());
            inner
                .notice
                .insert(turn_id.to_string(), result.render_line());
            return Vec::new();
        }
        if let Some((key, result)) = &inner.passed
            && *key == changes.content_key
            && !changes.is_empty()
        {
            let line = result.render_line();
            inner.notice.insert(turn_id.to_string(), line);
            return Vec::new();
        }
        // An empty change is only a problem once, in non-interactive runs.
        if changes.is_empty() && (!self.non_interactive || rounds > 0) {
            return Vec::new();
        }
        // A person skipped the check for exactly this version: NOT RUN (skipped),
        // never PASS, never a loop.
        if let Some(home) = crate::results_store::default_home()
            && let Some(skipped) = crate::skip::active(&home, &snapshot.root, gate::CHECK).await
        {
            crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&skipped));
            write_verdict_record(&snapshot, turn_id, rounds, &changes.content_key, &skipped);
            let fresh = crate::results_store::fingerprint(&snapshot.root, FRESHNESS_BUDGET)
                .await
                .unwrap_or_default();
            store_result(&snapshot, &changes.content_key, &skipped, fresh, self.session());
            inner.last = Some((changes.content_key.clone(), skipped.clone()));
            inner.notice.insert(turn_id.to_string(), skipped.render_line());
            return Vec::new();
        }
        let assistant_text = assistant_text_after_checklist(history.as_ref());
        let crash = crash_context(history.as_ref());
        // Stay inside the session's wall-clock cap: shrink the budget. With too little left the
        // gate does not start builds and tests and says NOT RUN (time limit).
        let mut settings = self.settings.clone();
        let left = settings.session_cap.saturating_sub(self.started.elapsed());
        settings.budget = settings.budget.min(left);
        // In an interactive session the turn waits for the fast tier only; whatever does
        // not fit continues in the background with the whole budget (below).
        let full_settings = settings.clone();
        let fast = !self.non_interactive
            && settings.fast_tier > std::time::Duration::ZERO
            && settings.fast_tier < settings.budget;
        if fast {
            settings.budget = settings.fast_tier;
        }
        let key = changes.content_key.clone();
        if lock(&self.background_running).as_deref() == Some(key.as_str()) {
            // The full check of exactly this code is still running: nothing new to start.
            let result = check_result::CheckResult::not_run(
                gate::CHECK,
                check_result::NotRunReason::TimeLimit,
                "The full check of this version is still running in the background.",
                "carry on; its result is shown when it finishes (check_status shows it too).",
            );
            crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&result));
            inner.notice.insert(turn_id.to_string(), result.render_line());
            return Vec::new();
        }
        // The freshness fingerprint of the tree as the check starts.
        let fresh = crate::results_store::fingerprint(&snapshot.root, FRESHNESS_BUDGET)
            .await
            .unwrap_or_default();
        if let Some(sandbox) = self.sandbox.get()
            && let Err(result) = crate::sandbox::check_available(gate::CHECK, sandbox)
        {
            // No sandbox here, so the gate builds nothing and says how to fix it.
            crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&result));
            store_result(&snapshot, &key, &result, fresh, self.session());
            inner.last = Some((key, result.clone()));
            inner.notice.insert(turn_id.to_string(), result.render_line());
            return Vec::new();
        }
        let finished = {
            let mut done = lock(&self.background_done);
            match done.take() {
                Some((done_key, outcome)) if done_key == key => Some(outcome),
                other => {
                    *done = other;
                    None
                }
            }
        };
        let inner = &mut *inner;
        let outcome = match finished {
            // The background full check of this exact code finished: that is the verdict.
            Some(outcome) => outcome,
            None => {
                let run = gate::run_gate(gate::GateInput {
                    snapshot: &snapshot,
                    changes: &changes,
                    settings: &settings,
                    native_exec: self.native_exec,
                    non_interactive: self.non_interactive,
                    impact_state: Some(&mut inner.impact),
                    base: &mut inner.base,
                    assistant_text: &assistant_text,
                    memo: &mut inner.memo,
                    advisories: &mut inner.advisories,
                    crash: crash.as_ref(),
                    session_left: left,
                    advisory: false,
                });
                // The gate builds and runs the project's tests like cpp_sanitize does, so it
                // runs in the same Build profile.
                let sandboxed = async {
                    match self.sandbox.get() {
                        Some(sandbox) => crate::sandbox::scope(gate::CHECK, sandbox.clone(), run).await,
                        None => run.await,
                    }
                };
                // An interrupted turn drops this future; the guard records NOT RUN
                // (cancelled) for the version being checked. `byteask checks cancel` stops it
                // the same way, within 2 s.
                let guard = {
                    let root = snapshot.root.clone();
                    let key = changes.content_key.clone();
                    crate::cancel::Guard::new(move || {
                        store_cancelled(&root, &key, "the turn was interrupted");
                    })
                };
                let home = crate::results_store::default_home();
                let ran = match &home {
                    Some(home) => {
                        crate::cancel::unless_requested(home, &snapshot.root, sandboxed).await
                    }
                    None => Some(sandboxed.await),
                };
                guard.finish();
                match ran {
                    Some(outcome) => outcome,
                    None => {
                        let result =
                            crate::cancel::cancelled(gate::CHECK, "`byteask checks cancel` was run");
                        crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&result));
                        store_result(&snapshot, &key, &result, String::new(), self.session());
                        inner.last = Some((key, result.clone()));
                        inner.notice.insert(turn_id.to_string(), result.render_line());
                        return Vec::new();
                    }
                }
            }
        };
        // The verdict belongs to the code as it was when the check started.
        // If anything changed while the gate was building or testing, the result is bound to
        // neither version and says so; it is never reused.
        let after = gate::agent_changes(&snapshot).await;
        let result = bound_to_checked_code(outcome.result(), &changes.content_key, &after.content_key);
        // Feedback about a version that no longer exists would mislead: the turn ends with the
        // NOT RUN shown, and the next finish checks the final version.
        let moved = after.content_key != changes.content_key;
        let feedback = gate::needs_feedback(&outcome) && !moved;
        if fast && !moved && ran_out_of_time(&result) {
            self.start_background(&snapshot, &changes, full_settings, left);
        }
        // The log, the record and what the user is shown carry the real verdict; only a
        // PASS is remembered as passed.
        crate::report::log_tool_outcome(gate::CHECK, &verdict_label(&result));
        write_gate_record(&snapshot, turn_id, rounds, &changes.content_key, &outcome, &result);
        // The verdict goes to the results store, where check_status, the prompt hook and
        // the merge check read it (as current only while the tree still matches `fresh`).
        store_result(
            &snapshot,
            &changes.content_key,
            &result,
            if moved { String::new() } else { fresh },
            self.session(),
        );
        inner.last = Some((changes.content_key.clone(), result.clone()));
        // What the gate could not check never holds the model back on its own (NOT RUN
        // lets the turn end and never loops), but it is not PASS either.
        if !feedback {
            if result.is_pass() {
                inner.passed = Some((changes.content_key.clone(), result.clone()));
            }
            let mut notice = result.render_line();
            // A check that ran out of time says why the build is slow, once.
            if ran_out_of_time(&result)
                && !inner.slow_card_shown
                && let Some(card) = gate::slow_build_card(&snapshot)
            {
                inner.slow_card_shown = true;
                notice = format!("{notice}\n{card}");
            }
            inner.notice.insert(turn_id.to_string(), notice);
            return Vec::new();
        }
        if outcome
            .problems
            .iter()
            .any(|p| p.kind == gate::ProblemKind::SuiteNotChecked)
        {
            inner.advisories.not_checked_shown = true;
        }
        if let Some(memo) = inner.memo.as_mut() {
            memo.delivered = true;
        }
        let round = rounds + 1;
        inner.gate_rounds.insert(turn_id.to_string(), round);
        fragment(
            "finish_gate",
            gate::render_feedback(&outcome, round, self.settings.max_loops),
        )
    }
}

/// The gate's verdict if the code did not change while it was being checked, else NOT RUN
/// (cancelled): a result must belong to exactly the code it checked.
pub(crate) fn bound_to_checked_code(
    result: check_result::CheckResult,
    checked_key: &str,
    key_after: &str,
) -> check_result::CheckResult {
    if checked_key == key_after {
        return result;
    }
    check_result::CheckResult::not_run(
        gate::CHECK,
        check_result::NotRunReason::Cancelled,
        "Files changed while the gate was building and testing them, so its result belongs to neither version.",
        "finish again so the gate checks the final version.",
    )
}

/// How long the freshness fingerprint may take.
const FRESHNESS_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// The verdict says the check ran out of its time (the fast tier's, or the gate budget's).
pub(crate) fn ran_out_of_time(result: &check_result::CheckResult) -> bool {
    result.verdict == check_result::Verdict::NotRun
        && result.reason == Some(check_result::NotRunReason::TimeLimit)
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keep a verdict in the results store (with a session, also in its per-change log for
/// the value report). Best effort.
fn store_result(
    snapshot: &gate::Snapshot,
    content_key: &str,
    result: &check_result::CheckResult,
    fresh: String,
    session: Option<&str>,
) {
    store_result_tier(snapshot, content_key, result, fresh, "fast", session);
}

fn store_result_tier(
    snapshot: &gate::Snapshot,
    content_key: &str,
    result: &check_result::CheckResult,
    fresh: String,
    tier: &str,
    session: Option<&str>,
) {
    let Some(home) = crate::results_store::default_home() else {
        return;
    };
    let entry = crate::results_store::Entry {
        result: result.clone().with_content_key(content_key),
        root: snapshot.root.clone(),
        at: crate::results_store::now_unix(),
        fresh,
        tier: tier.to_string(),
        source: "engine".to_string(),
    };
    match session {
        Some(session) => crate::results_store::record_for_session(&home, entry, session),
        None => ::results_store::record(&home, entry),
    }
}

/// The gate was stopped before it finished: NOT RUN (cancelled) for that version.
fn store_cancelled(root: &std::path::Path, content_key: &str, how: &str) {
    let Some(home) = crate::results_store::default_home() else {
        return;
    };
    crate::results_store::record(
        &home,
        crate::results_store::Entry {
            result: crate::cancel::cancelled(gate::CHECK, how).with_content_key(content_key),
            root: root.to_path_buf(),
            at: crate::results_store::now_unix(),
            fresh: String::new(),
            tier: "fast".to_string(),
            source: "engine".to_string(),
        },
    );
}

/// Lower-case verdict label for the tool-call log (`pass`, `fail`, `not_run`, ...).
fn verdict_label(result: &check_result::CheckResult) -> String {
    result.verdict.word().to_ascii_lowercase().replace(' ', "_")
}

/// The verdict a turn ends with when the retry limit is reached.
pub(crate) fn retry_limit_result(
    last: Option<&(String, check_result::CheckResult)>,
    content_key: &str,
    max_loops: u32,
) -> check_result::CheckResult {
    use check_result::CheckResult;
    use check_result::NotRunReason;
    use check_result::Verdict;
    match last {
        Some((key, result)) if key == content_key && result.verdict == Verdict::Fail => {
            CheckResult::fail(
                gate::CHECK,
                &format!(
                    "Still failing after {max_loops} round(s) of fixes: {}",
                    result.summary
                ),
            )
        }
        Some((key, result)) if key == content_key => result.clone(),
        _ => CheckResult::not_run(
            gate::CHECK,
            NotRunReason::Skipped,
            &format!(
                "The retry limit ({max_loops} round(s)) was reached before this version of the change was checked."
            ),
            "check the change again before relying on it (finish again in a new turn, or run its tests).",
        ),
    }
}

impl Workflow {
    /// The verdict line to show the user as this turn ends, once.
    pub async fn stop_notice(&self, turn_id: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .notice
            .remove(turn_id)
            .map(|line| format!("ByteAsk finish gate: {line}"))
    }
}

/// Repositories one workflow tracks at most (workspaces that hold several).
const MAX_REPOS: usize = 4;

/// Session-start snapshots of the repositories for a workspace at `cwd` (see
/// [`crate::gitx::repo_roots`]).
async fn take_snapshots(cwd: &std::path::Path) -> Vec<Arc<gate::Snapshot>> {
    let mut out = Vec::new();
    for root in crate::gitx::repo_roots(cwd).await.into_iter().take(MAX_REPOS) {
        if let Some(snapshot) = gate::take_snapshot(&root).await {
            out.push(Arc::new(snapshot));
        }
    }
    out
}

/// Keep a JSON line per gate run next to the gate's logs (for analysis; never read back).
fn write_gate_record(
    snapshot: &gate::Snapshot,
    turn_id: &str,
    round: u32,
    content_key: &str,
    outcome: &gate::GateOutcome,
    result: &check_result::CheckResult,
) {
    let record = serde_json::json!({
        "turn": turn_id,
        "round": round,
        "content_key": content_key,
        "result": result.clone().with_content_key(content_key),
        "problems": outcome.problems.iter().map(|p| format!("{:?}: {}", p.kind, p.text)).collect::<Vec<_>>(),
        "notes": outcome.notes,
        "still_open": outcome.still_open.iter().map(|p| format!("{:?}: {}", p.kind, p.text)).collect::<Vec<_>>(),
        "not_checked": outcome.not_checked,
    });
    let path = snapshot.scratch.join("gate-runs.jsonl");
    let _ = std::fs::create_dir_all(&snapshot.scratch);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{record}");
    }
}

/// The record of a verdict reached without a gate run (the retry limit).
fn write_verdict_record(
    snapshot: &gate::Snapshot,
    turn_id: &str,
    round: u32,
    content_key: &str,
    result: &check_result::CheckResult,
) {
    let record = serde_json::json!({
        "turn": turn_id,
        "round": round,
        "content_key": content_key,
        "result": result.clone().with_content_key(content_key),
        "retry_limit": true,
    });
    let path = snapshot.scratch.join("gate-runs.jsonl");
    let _ = std::fs::create_dir_all(&snapshot.scratch);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{record}");
    }
}

/// Settings from the environment only (`BYTEASK_CPP_*`), for the agent hooks, which run
/// without the ByteAsk config's `[byteask_cpp]` table.
pub(crate) fn settings_from_env() -> GateSettings {
    let mut settings = GateSettings::resolve(|_| None, |_| None, |name| std::env::var(name).ok());
    crate::ablation::Ablation::from_env().apply(&mut settings);
    settings
}

/// Settings from the thread's config (`[byteask_cpp]`) and the environment.
///
/// Unless set explicitly, a non-interactive session's gate budget is [`gate::EXEC_BUDGET`], and
/// `[byteask_harness] time_budget_secs` bounds the session cap so the gate ends before the
/// caller's limit.
pub(crate) fn settings_from(config: &codex_core::config::Config, non_interactive: bool) -> GateSettings {
    let table = config
        .config_layer_stack
        .effective_config()
        .get(gate::CONFIG_TABLE)
        .cloned();
    let lookup_bool = |key: &str| {
        table
            .as_ref()
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_bool())
    };
    let lookup_int = |key: &str| {
        table
            .as_ref()
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_integer())
    };
    let mut settings =
        GateSettings::resolve(lookup_bool, lookup_int, |name| std::env::var(name).ok());
    let explicit = |env: &str, key: &str| std::env::var(env).is_ok() || lookup_int(key).is_some();
    if non_interactive && !explicit(gate::ENV_BUDGET, "gate_budget_secs") {
        settings.budget = settings.budget.min(gate::EXEC_BUDGET);
    }
    if !explicit(gate::ENV_SESSION_CAP, "gate_session_cap_secs")
        && let Some(secs) = codex_core::byteask_harness::positive_int(config, "time_budget_secs")
    {
        let cap = std::time::Duration::from_secs(secs).saturating_sub(gate::TIME_BUDGET_MARGIN);
        settings.session_cap = settings.session_cap.min(cap);
    }
    // `BYTEASK_CPP_ABLATE` turns single parts off; the fast profile turns its parts off.
    crate::ablation::Ablation::from_env()
        .with_fast(codex_core::byteask_harness::flag(config, "fast"))
        .apply(&mut settings);
    settings
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    struct History(Vec<ResponseItem>);

    impl ConversationHistorySnapshot for History {
        fn history_version(&self) -> u64 {
            0
        }

        fn user_message_revision(&self) -> u64 {
            0
        }

        fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
            Box::new(self.0.iter())
        }
    }

    fn message(role: &str, text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![if role == "assistant" {
                ContentItem::OutputText {
                    text: text.to_string(),
                }
            } else {
                ContentItem::InputText {
                    text: text.to_string(),
                }
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn text_of(fragments: &TurnGateFragments) -> String {
        fragments
            .iter()
            .map(|f| f.body())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole loop on a fixture: the checklist after the first C/C++ edit (once per turn),
    /// then finish-gate rounds (unanswered items, stale copy) up to `max_loops`, then silence.
    #[tokio::test]
    async fn checklist_then_gate_rounds_then_finish() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            eprintln!("skipped: git not available");
            return;
        };
        let settings = GateSettings {
            tests: false,
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(settings, false, true, dir.path().to_path_buf());
        assert!(workflow.snapshot().await.is_some());
        // No tool ran yet: nothing to say.
        assert!(workflow.after_step("turn").await.is_empty());
        crate::impact::tests::apply_edits(dir.path()).expect("edits");
        workflow.note_tool_call();
        let checklist = text_of(&workflow.after_step("turn").await);
        assert!(checklist.starts_with(IMPACT_TITLE), "{checklist}");
        assert!(checklist.contains("to_local"), "{checklist}");
        // Once per turn.
        workflow.note_tool_call();
        assert!(workflow.after_step("turn").await.is_empty());

        // The model answers item 1 only, then tries to finish.
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(vec![
            message("developer", &checklist),
            message("assistant", "[1] n/a: not part of the request"),
        ]));
        let round1 = text_of(&workflow.before_stop("turn", history.clone()).await);
        assert!(round1.contains("round 1 of 2"), "{round1}");
        assert!(round1.contains("GENERATED COPIES OUT OF SYNC"), "{round1}");
        assert!(round1.contains("CHANGE-IMPACT ITEMS WITHOUT AN ANSWER"), "{round1}");
        assert!(!round1.contains(" [1] "), "answered item re-asked: {round1}");
        assert!(round1.contains("build/tests: off"), "{round1}");
        // Second round: the copy is still stale; unanswered items are not asked twice.
        let round2 = text_of(&workflow.before_stop("turn", history.clone()).await);
        assert!(round2.contains("round 2 of 2"), "{round2}");
        assert!(!round2.contains("CHANGE-IMPACT ITEMS"), "{round2}");
        // After max_loops the model may finish.
        assert!(workflow.before_stop("turn", history).await.is_empty());
        // ...but the change is not "passed": the copy is still stale, so the turn
        // ends with FAIL shown to the user, once.
        let notice = workflow.stop_notice("turn").await.expect("a verdict at the retry limit");
        assert!(notice.contains("Result: FAIL"), "{notice}");
        assert!(notice.contains("Still failing after 2 round(s)"), "{notice}");
        assert!(workflow.stop_notice("turn").await.is_none(), "shown once");
    }

    #[test]
    fn a_result_belongs_only_to_the_code_it_checked() {
        use check_result::CheckResult;
        use check_result::NotRunReason;
        use check_result::Verdict;
        let pass = CheckResult::pass(gate::CHECK, "ok");
        assert_eq!(bound_to_checked_code(pass.clone(), "k1", "k1"), pass);
        let moved = bound_to_checked_code(pass, "k1", "k2");
        assert_eq!(moved.verdict, Verdict::NotRun);
        assert_eq!(moved.reason, Some(NotRunReason::Cancelled));
        assert!(moved.validate().is_ok());
        // A FAIL for code that has since changed is not that code's verdict either.
        let fail = CheckResult::fail(gate::CHECK, "1 problem(s) open");
        assert_eq!(bound_to_checked_code(fail, "k1", "k2").verdict, Verdict::NotRun);
    }

    #[test]
    fn the_retry_limit_never_turns_into_pass() {
        use check_result::CheckResult;
        use check_result::Verdict;
        let fail = CheckResult::fail(gate::CHECK, "1 problem(s) open; first: FAILING TESTS");
        let key = "k1".to_string();
        // Still the code that failed: FAIL.
        let still = retry_limit_result(Some(&(key.clone(), fail.clone())), "k1", 2);
        assert_eq!(still.verdict, Verdict::Fail);
        // Edited after the last check: that version was never checked.
        let edited = retry_limit_result(Some(&(key.clone(), fail)), "k2", 2);
        assert_eq!(edited.verdict, Verdict::NotRun);
        assert!(edited.validate().is_ok());
        // Never checked at all.
        assert_eq!(retry_limit_result(None, "k1", 2).verdict, Verdict::NotRun);
        // A verdict reached for this exact code is kept (a NOT RUN stays NOT RUN).
        let pass = CheckResult::pass(gate::CHECK, "ok");
        assert_eq!(retry_limit_result(Some(&(key, pass)), "k1", 2).verdict, Verdict::Pass);
    }

    /// The workspace is a parent of the repository (`/app` holding `/app/<project>`): the
    /// checklist and the gate apply to the repository below (they must not stay
    /// silent).
    #[tokio::test]
    async fn a_parent_workspace_gets_the_checklist_and_gate_of_the_repository_below() {
        let Some(parent) = tempfile::tempdir().ok() else {
            return;
        };
        let repo = parent.path().join("proj");
        if crate::impact::tests::fixture_base_in(&repo).is_none() {
            eprintln!("skipped: git not available");
            return;
        }
        let settings = GateSettings {
            tests: false,
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(settings, false, true, parent.path().to_path_buf());
        let snapshot = workflow.snapshot().await.expect("snapshot of the repository below");
        assert_eq!(
            std::fs::canonicalize(&snapshot.root).ok(),
            std::fs::canonicalize(&repo).ok()
        );
        assert!(workflow.enforcement().await.checklist);
        crate::impact::tests::apply_edits(&repo).expect("edits");
        workflow.note_tool_call();
        let checklist = text_of(&workflow.after_step("turn").await);
        assert!(checklist.starts_with(IMPACT_TITLE), "{checklist}");
        let history: Arc<dyn ConversationHistorySnapshot> =
            Arc::new(History(vec![message("developer", &checklist)]));
        let round1 = text_of(&workflow.before_stop("turn", history).await);
        assert!(round1.contains("GENERATED COPIES OUT OF SYNC"), "{round1}");
    }

    /// What the gate could not check (here: no build system at all) never causes a feedback
    /// round by itself; it is said once, together with feedback sent for a real problem.
    #[tokio::test]
    async fn could_not_check_rides_along_once_and_never_blocks() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            eprintln!("skipped: git not available");
            return;
        };
        let settings = GateSettings {
            impact: false,
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(settings, true, true, dir.path().to_path_buf());
        assert!(workflow.snapshot().await.is_some());
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(Vec::new()));
        // A change the gate cannot build or test, and nothing else wrong: the model may finish.
        std::fs::write(
            dir.path().join("src/use.cc"),
            "#include \"lib/widget.h\"\nlong run() {\n  return lib::to_utc(6);\n}\n",
        )
        .expect("write");
        let quiet = text_of(&workflow.before_stop("t1", history.clone()).await);
        assert!(quiet.is_empty(), "{quiet}");
        // A real problem (a stale generated copy): the feedback carries the note, once.
        crate::impact::tests::apply_edits(dir.path()).expect("edits");
        let round1 = text_of(&workflow.before_stop("t2", history.clone()).await);
        assert!(round1.contains("GENERATED COPIES OUT OF SYNC"), "{round1}");
        assert!(round1.contains("COULD NOT CHECK (this does not block finishing)"), "{round1}");
        let round2 = text_of(&workflow.before_stop("t2", history).await);
        assert!(round2.contains("GENERATED COPIES OUT OF SYNC"), "{round2}");
        assert!(!round2.contains("COULD NOT CHECK"), "{round2}");
        // The run log keeps what could not be checked in every record (sub-agents' evidence).
        let snapshot = workflow.snapshot().await.expect("snapshot");
        let log = std::fs::read_to_string(snapshot.scratch.join("gate-runs.jsonl")).expect("gate log");
        let last: serde_json::Value =
            serde_json::from_str(log.lines().last().expect("a record")).expect("json");
        assert!(
            last["not_checked"].as_array().is_some_and(|a| !a.is_empty()),
            "{last}"
        );
        assert!(last["still_open"].is_array(), "{last}");
        let _ = std::fs::remove_dir_all(&snapshot.scratch);
    }

    /// A manual cpp_impact call numbers items like the automatic checklist, so answers given
    /// to either list count at finish (items answered under the manual call's own
    /// numbering must not be asked again).
    #[tokio::test]
    async fn a_manual_checklist_shares_the_numbering() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            eprintln!("skipped: git not available");
            return;
        };
        let settings = GateSettings {
            tests: false,
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(settings, false, true, dir.path().to_path_buf());
        let snapshot = workflow.snapshot().await.expect("snapshot");
        crate::impact::tests::apply_edits(dir.path()).expect("edits");
        workflow.note_tool_call();
        let auto = text_of(&workflow.after_step("turn").await);
        let report = impact::analyze(&impact::ImpactRequest {
            root: snapshot.root.clone(),
            base_rev: snapshot.head.clone(),
            source: impact::Source::Diff,
            budget: std::time::Duration::from_secs(60),
            use_clangd: false,
            max_items: impact::DEFAULT_MAX_ITEMS,
            preexisting_untracked: Vec::new(),
        })
        .await;
        let manual = workflow.render_shared(&report, "cpp_impact").await;
        // item text -> number, for each list
        let numbered = |text: &str| -> std::collections::HashMap<String, String> {
            text.lines()
                .filter_map(|l| {
                    let l = l.trim_start().strip_prefix('[')?;
                    let (n, rest) = l.split_once("] ")?;
                    n.chars().all(|c| c.is_ascii_digit()).then(|| (rest.to_string(), n.to_string()))
                })
                .collect()
        };
        let (auto_items, manual_items) = (numbered(&auto), numbered(&manual));
        let common: Vec<&String> = auto_items.keys().filter(|k| manual_items.contains_key(*k)).collect();
        assert!(!common.is_empty(), "{auto}\n---\n{manual}");
        for item in common {
            assert_eq!(auto_items[item], manual_items[item], "{item:?} numbered differently");
        }
        // Answer every item by the manual list's numbers: nothing is re-asked at finish.
        let answers: String = answered_numbers(&manual)
            .into_iter()
            .map(|n| format!("[{n}] n/a: not part of the request\n"))
            .collect();
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(vec![
            message("developer", &auto),
            message("assistant", &answers),
        ]));
        let round1 = text_of(&workflow.before_stop("turn", history).await);
        assert!(!round1.contains("CHANGE-IMPACT ITEMS WITHOUT AN ANSWER"), "{round1}");
    }

    #[tokio::test]
    async fn an_exec_run_that_changed_nothing_is_sent_back_once() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            return;
        };
        let workflow = Workflow::start(GateSettings::enabled(), false, true, dir.path().to_path_buf());
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(Vec::new()));
        let first = text_of(&workflow.before_stop("t", history.clone()).await);
        assert!(first.contains("no changes"), "{first}");
        assert!(workflow.before_stop("t", history.clone()).await.is_empty());
        // Interactive sessions are never sent back for an empty change.
        let interactive =
            Workflow::start(GateSettings::enabled(), false, false, dir.path().to_path_buf());
        assert!(interactive.before_stop("t", history).await.is_empty());
    }

    #[tokio::test]
    async fn switched_off_by_settings() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            return;
        };
        let off = GateSettings {
            gate: false,
            impact: false,
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(off, false, true, dir.path().to_path_buf());
        crate::impact::tests::apply_edits(dir.path()).expect("edits");
        workflow.note_tool_call();
        assert!(workflow.after_step("t").await.is_empty());
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(Vec::new()));
        assert!(workflow.before_stop("t", history).await.is_empty());
    }

    fn tool_output(text: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(text.to_string()),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    /// A request that reports a crash (with a sanitizer report, or in words with the report
    /// from the reproduction) makes the session a crash fix for the gate's patch-shape critic;
    /// a report that only appears in an ordinary task's tool output does not.
    #[test]
    fn a_crash_report_makes_the_session_a_crash_fix() {
        let report = "==7==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000010 at pc 0x55 bp 0x7f sp 0x7e\n\
                      READ of size 8 at 0x602000000010 thread T0\n\
                      \x20   #0 0x5594b218850a in use /src/p/uaf.c:7\n\
                      \x20   #1 0x5594b218862a in main /src/p/uaf.c:8\n";
        let plain = History(vec![message("user", "Add an option to print the version.")]);
        assert!(crash_context(&plain).is_none());
        let reported = History(vec![
            message("user", &format!("The tool fails on this input:\n{report}")),
            message("assistant", "Looking at it."),
        ]);
        let context = crash_context(&reported).expect("crash context");
        assert_eq!(context.functions, vec!["use".to_string(), "main".to_string()]);
        assert!(context.shown_elsewhere.is_empty());

        // Named in words; the stack comes from the reproduction.
        let described = History(vec![
            message("user", "Parsing an empty file crashes the tool."),
            tool_output(report),
            tool_output("patch-shape review (advisory, static): ...\n- [deleted-free] a.c:3 removes"),
        ]);
        let context = crash_context(&described).expect("crash context");
        assert_eq!(context.functions.first().map(String::as_str), Some("use"));
        assert!(context.shown_elsewhere.contains("[deleted-free] a.c:"));

        // An ordinary task whose test run printed a report is not a crash fix, and neither are
        // instruction blocks that mention sanitizers.
        let ordinary = History(vec![
            message("user", "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nRun the tests under a sanitizer build.\n</INSTRUCTIONS>"),
            message("user", "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"),
            message("user", "Add an option to print the version."),
            tool_output(report),
        ]);
        assert!(crash_context(&ordinary).is_none());
    }

    #[test]
    fn answers_are_found() {
        let text = "[1] edited\n[12] n/a: not in this configuration\nsee [x] and [3a]";
        let numbers = answered_numbers(text);
        assert_eq!(numbers, [1usize, 12].into_iter().collect());
    }

    #[test]
    fn settings_resolve_env_over_config() {
        let config: std::collections::BTreeMap<&str, i64> = [("gate_budget_secs", 120), ("gate_max_loops", 1)]
            .into_iter()
            .collect();
        let settings = GateSettings::resolve(
            |key| (key == "finish_gate").then_some(false),
            |key| config.get(key).copied(),
            |name| (name == gate::ENV_GATE).then(|| "1".to_string()),
        );
        assert!(settings.gate, "env wins over config");
        assert_eq!(settings.budget, std::time::Duration::from_secs(120));
        assert_eq!(settings.max_loops, 1);
        assert!(!settings.impact, "the checklist is off unless switched on");
        let defaults = GateSettings::resolve(|_| None, |_| None, |_| None);
        assert_eq!(defaults, GateSettings::default());
        // 0.2.0: the automatic gate and the automatic checklist are opt-in.
        assert!(!defaults.gate && !defaults.impact, "{defaults:?}");
        let opted_in = GateSettings::resolve(
            |key| matches!(key, "finish_gate" | "impact").then_some(true),
            |_| None,
            |_| None,
        );
        assert!(opted_in.gate && opted_in.impact, "the config switches still turn them on");
        let off = GateSettings::resolve(
            |_| None,
            |_| None,
            |name| (name == gate::ENV_IMPACT).then(|| "off".to_string()),
        );
        assert!(!off.impact);
        assert!(defaults.critic);
        let no_critic = GateSettings::resolve(|key| (key == "gate_critic").then_some(false), |_| None, |_| None);
        assert!(!no_critic.critic && !no_critic.gate);
        let env_critic = GateSettings::resolve(
            |key| (key == "gate_critic").then_some(false),
            |_| None,
            |name| (name == gate::ENV_CRITIC).then(|| "1".to_string()),
        );
        assert!(env_critic.critic, "env wins over config");
    }

    /// An interactive turn waits for the fast tier only; a check that does not fit ends
    /// the turn as NOT RUN (time limit), finishes in the background with the whole budget,
    /// and its verdict is this code's verdict at the next finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_check_that_does_not_fit_the_fast_tier_finishes_in_the_background() {
        let have = |tool: &str| crate::tools::binary_on_path(tool);
        if !have("cmake") || !(have("c++") || have("g++")) || !have("git") {
            eprintln!("skipped: cmake/c++/git not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.5)\nproject(t CXX)\nenable_testing()\n\
             add_executable(t_test src/t.cpp)\nadd_test(NAME works COMMAND t_test)\n",
        )
        .unwrap();
        std::fs::write(root.join("src/t.cpp"), "int main() { return 0; }\n").unwrap();
        use crate::impact::tests::git;
        assert!(git(root, &["init", "-q"]) && git(root, &["add", "-A"]));
        assert!(git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]));

        let settings = GateSettings {
            impact: false,
            critic: false,
            budget: std::time::Duration::from_secs(300),
            fast_tier: std::time::Duration::from_millis(500),
            ..GateSettings::enabled()
        };
        let workflow = Workflow::start(settings, true, false, root.to_path_buf());
        assert!(workflow.snapshot().await.is_some());
        std::fs::write(root.join("src/t.cpp"), "int main() { return 0; } // edited\n").unwrap();
        let history: Arc<dyn ConversationHistorySnapshot> = Arc::new(History(Vec::new()));

        assert!(workflow.before_stop("t1", history.clone()).await.is_empty(), "NOT RUN never loops");
        let notice = workflow.stop_notice("t1").await.expect("a verdict");
        assert!(notice.contains("NOT RUN (time limit)"), "{notice}");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        while lock(&workflow.background_done).is_none() {
            assert!(std::time::Instant::now() < deadline, "the background check never finished");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(workflow.before_stop("t2", history).await.is_empty());
        let notice = workflow.stop_notice("t2").await.expect("a verdict");
        assert!(notice.contains("Result: PASS"), "the background result is this code's verdict: {notice}");
    }
}
