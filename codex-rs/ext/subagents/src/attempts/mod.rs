//! Tier B: adaptive multi-attempt with execution-based selection.
//!
//! Attempt 1 is the normal session (the root thread, unchanged). When its turn ends:
//! 1. The independent test writer gets a short grace to finish, then stops; its frozen tests
//!    (if any) become held-out evidence for selection (attempts never see them).
//! 2. Runtime evidence about attempt 1 is collected ([`evidence`]): empty change, the finish
//!    gate's last verdict (open problems, or what it could not check), uncertainty markers in the
//!    final message, and — only when the writer delivered qualified tests (they fail on the
//!    starting code) — whether one passes on the change. Without such tests (the writer is off by
//!    default) a clean gate and a confident final message mean one attempt.
//! 3. Attempt 2 starts only on weak evidence (adaptive mode; a fixed `N` always runs). Attempts
//!    run ONE AFTER ANOTHER, never in parallel, each a fresh sub-agent (no shared transcript) in
//!    its own detached worktree outside the repository, with a short strategy card and
//!    alternating effort (even attempts high). Further attempts, up to the cap, only while the
//!    candidates disagree (different test outcomes, or different changes with no test to tell).
//! 4. Every candidate is evaluated on one evaluation worktree ([`bench`]) and the selector
//!    ([`selector`]) picks: hard filters, cross-attempt test matrix, tie-breaks, then a
//!    fresh-context pairwise judge in both orders (inconsistent: attempt 1 stays).
//! 5. The winner's change replaces the root's in the real tree in one `git apply`; every
//!    worktree is removed. A per-session summary goes into the sub-agents report
//!    (`CODEX_HOME/byteask-subagents/<root>.json`, key `attempts`) and the log.
//!
//! Budget: the session's task timeout (`BYTEASK_TASK_TIMEOUT_SECS`) minus a margin of at least
//! 10% ([`budget`]); hard caps on attempts and on the tokens of all attempt and judge
//! sub-agents ([`settings`]). Every child's rollout lands in the session's sessions directory with
//! `parent_thread_id` = the root, so the harness can sum them.

pub mod bench;
pub mod budget;
pub mod evidence;
pub mod meter;
pub mod patches;
pub mod prompts;
pub mod selector;
pub mod settings;
pub mod testparse;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

pub use settings::AttemptsMode;
pub use settings::AttemptsSettings;

use crate::attempts::bench::Bench;
use crate::attempts::bench::CandidateInput;
use crate::attempts::bench::TestPatch;
use crate::attempts::budget::Budget;
use crate::attempts::budget::BudgetReport;
use crate::attempts::evidence::Evidence;
use crate::attempts::evidence::TestEvidence;
use crate::attempts::evidence::Trigger;
use crate::attempts::selector::Author;
use crate::attempts::selector::CandidateEval;
use crate::attempts::selector::Cell;
use crate::attempts::selector::Judge;
use crate::attempts::selector::JudgeFuture;
use crate::attempts::selector::Pick;
use crate::attempts::selector::PooledTest;
use crate::attempts::selector::Selection;
use crate::evaluate;
use crate::evaluate::EvalContext;
use crate::evaluate::Recipe;
use crate::evaluate::StepLog;
use crate::gitops;
use crate::host::ChildHandle;
use crate::host::ChildSpec;
use crate::host::ChildState;
use crate::host::Role;
use crate::orchestrator::Orchestration;
use crate::orchestrator::POLL;
use crate::orchestrator::ReproOutcome;
use crate::orchestrator::final_message;
use crate::orchestrator::jobs_per_build;
use crate::orchestrator::short_state;
use crate::report::SubagentReport;
use crate::report::parse_report;

const JUDGE_BUDGET: Duration = Duration::from_secs(300);
const JUDGE_MAX_STEPS: usize = 6;
const MAX_JUDGE_DIFF_CHARS: usize = 40_000;
/// No new attempt worktree with less free disk than this.
const MIN_FREE_BYTES: u64 = 2 << 30;
/// No new attempt with less memory headroom than this (after waiting up to a minute).
const MIN_HEADROOM: u64 = 1 << 30;

/// Tier B state of one orchestration (a field of [`Orchestration`]).
#[derive(Default)]
pub struct AttemptsSlot {
    config: OnceLock<(AttemptsSettings, AttemptsMode)>,
    observed: StdMutex<Observed>,
    cancel: CancellationToken,
    live: StdMutex<Vec<Arc<dyn ChildHandle>>>,
    children: StdMutex<Vec<String>>,
    report: StdMutex<Option<AttemptsReport>>,
}

#[derive(Clone, Debug, Default)]
struct Observed {
    turn_id: Option<String>,
    last_message: Option<String>,
}

impl AttemptsSlot {
    /// Switch Tier B on for this orchestration (once).
    pub fn configure(&self, settings: AttemptsSettings, mode: AttemptsMode) {
        let _ = self.config.set((settings, mode));
    }

    pub fn enabled(&self) -> bool {
        self.config
            .get()
            .is_some_and(|(_, mode)| *mode != AttemptsMode::Off)
    }

    /// Record the root turn's id and final message each time the model ends its turn (the
    /// last call before the turn really ends wins).
    pub fn observe_turn_end(&self, turn_id: &str, last_message: Option<&str>) {
        if let Ok(mut o) = self.observed.lock() {
            o.turn_id = Some(turn_id.to_string());
            o.last_message = last_message.map(str::to_string);
        }
    }

    fn observed(&self) -> Observed {
        self.observed.lock().map(|o| o.clone()).unwrap_or_default()
    }

    fn children(&self) -> Vec<String> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn add_live(&self, handle: Arc<dyn ChildHandle>) {
        if let Ok(mut live) = self.live.lock() {
            live.push(handle);
        }
    }

    fn remove_live(&self, id: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.retain(|h| h.id() != id);
        }
    }

    pub fn take_report(&self) -> Option<AttemptsReport> {
        self.report.lock().ok().and_then(|mut r| r.take())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptRun {
    pub attempt: usize,
    pub high_effort: bool,
    pub strategy: String,
    pub thread_id: Option<String>,
    pub spawn_error: Option<String>,
    pub state: Option<ChildState>,
    /// Why the attempt was stopped early (deadline, step cap, token cap, cancelled).
    pub stopped: Option<String>,
    pub elapsed_secs: u64,
    pub tokens: u64,
    pub diff_bytes: usize,
    pub tests_bytes: usize,
    pub report: Option<SubagentReport>,
    pub disqualified: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Escalation {
    pub after_attempt: usize,
    pub disagree: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BenchReport {
    pub reused_writer_worktree: bool,
    pub recipe: Option<Recipe>,
    pub base: bench::BaseRecord,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AttemptsReport {
    pub mode: String,
    pub cap: usize,
    pub budget: Option<BudgetReport>,
    pub evidence: Option<Evidence>,
    pub triggers: Vec<Trigger>,
    pub attempts: Vec<AttemptRun>,
    pub escalations: Vec<Escalation>,
    /// Why no further attempt ran.
    pub stopped: String,
    pub bench: Option<BenchReport>,
    pub tests: Vec<TestPatch>,
    pub pooled: Vec<PooledTest>,
    pub candidates: Vec<CandidateEval>,
    pub selection: Option<Selection>,
    /// The attempt whose change is in the real tree at the end.
    pub applied: usize,
    pub apply_note: String,
    pub tokens_total: u64,
    pub elapsed_secs: u64,
    pub steps: Vec<StepLog>,
    pub summary: String,
}

/// One candidate change.
#[derive(Clone)]
struct Cand {
    attempt: usize,
    diff: Vec<u8>,
    finished: bool,
    disqualified: Option<String>,
}

impl Orchestration {
    fn attempts_ctx(&self, budget: &Budget) -> EvalContext {
        let mut ctx = self.eval_ctx();
        ctx.deadline = budget.attempts_end();
        ctx.cancel = self.attempts.cancel.clone();
        ctx
    }

    /// Stop the attempts phase: cancel its work and interrupt running attempt/judge children.
    pub(crate) async fn stop_attempt_children(&self) {
        self.attempts.cancel.cancel();
        let live: Vec<Arc<dyn ChildHandle>> =
            self.attempts.live.lock().map(|l| l.clone()).unwrap_or_default();
        for h in live {
            h.interrupt().await;
            h.shutdown().await;
        }
    }

    /// Let a still-running independent test writer finish within a short grace, then stop it.
    async fn settle_writer(&self, budget: &Budget, settings: &AttemptsSettings) -> Option<ReproOutcome> {
        let launcher = self.inner.lock().await.launcher.take();
        if let Some(l) = launcher {
            let _ = l.await;
        }
        let task = self.inner.lock().await.repro_task.take();
        if let Some(mut task) = task {
            let grace = Duration::from_secs(settings.writer_grace_secs).min(budget.left(Instant::now()) / 4);
            let outcome = match tokio::time::timeout(grace, &mut task).await {
                Ok(result) => result.unwrap_or_default(),
                Err(_) => {
                    self.event(format!(
                        "attempts: the test writer is still running after {}s; stopping it",
                        grace.as_secs()
                    ))
                    .await;
                    self.cancel.cancel();
                    task.await.unwrap_or_default()
                }
            };
            self.inner.lock().await.repro = Some(outcome);
        }
        // Nothing of the writer may run during the attempts phase.
        self.cancel.cancel();
        self.inner.lock().await.repro.clone()
    }

    /// Tier B at the end of the root's turn: evidence, attempts, selection, apply. The caller
    /// cleans up and writes the report afterwards.
    pub(crate) async fn finalize_with_attempts(self: &Arc<Self>) {
        let started = Instant::now();
        let Some((settings, mode)) = self.attempts.config.get().cloned() else {
            return;
        };
        let session_start = meter::process_start().map_or(self.started, |p| p.min(self.started));
        let budget = Budget::new(
            session_start,
            self.deadline,
            settings.task_timeout_secs.map(Duration::from_secs),
            Duration::from_secs(settings.apply_margin_secs),
        );
        let cap = settings.cap(mode);
        let mut rep = AttemptsReport {
            mode: mode.as_str(),
            cap,
            budget: Some(budget.report(Instant::now())),
            applied: 1,
            ..AttemptsReport::default()
        };

        let repro = self.settle_writer(&budget, &settings).await;
        self.remove_leaked_tests().await;

        // Attempt 1: the root's change in the real tree.
        let tmp = self.scratch.join("tmp");
        let root_tree = match gitops::capture_tree(&self.p.repo, &self.snapshot.commit, &tmp).await {
            Ok(tree) => tree,
            Err(e) => {
                rep.stopped = format!("cannot read the root's change ({e}); it is kept");
                self.store_report(rep, started).await;
                return;
            }
        };
        // Files that were untracked at session start: changes to them are build noise.
        let noise = patches::added_by_snapshot(&self.p.repo, &self.snapshot.commit).await;
        let root_diff = if root_tree == self.snapshot.tree {
            Vec::new()
        } else {
            let d = gitops::diff_trees(&self.p.repo, &self.snapshot.tree, &root_tree)
                .await
                .unwrap_or_default();
            patches::drop_paths(&d, &noise)
        };
        let mut cands: Vec<Cand> = vec![Cand {
            attempt: 1,
            diff: root_diff.clone(),
            finished: true,
            disqualified: None,
        }];
        let mut tests: Vec<TestPatch> = Vec::new();
        if let Some(f) = repro.as_ref().and_then(|r| r.frozen.as_ref()) {
            tests.push(TestPatch {
                id: "independent".to_string(),
                author: Author::Independent,
                patch: f.patch.clone(),
                build: f.build.clone(),
                run: f.run.clone(),
                prequalified: true,
            });
        }
        let reuse = repro
            .as_ref()
            .filter(|r| r.frozen.is_some())
            .and_then(|r| r.dir.clone());
        let jobs = jobs_per_build(&self.p.settings);
        let mut bench: Option<Bench> = None;

        // Evidence about attempt 1.
        let observed = self.attempts.observed();
        let gate_log = codex_cpp_tools::finish_gate_runs_log(&self.p.repo);
        // Only qualified tests the writer delivered in this session are test evidence; without
        // them (the writer is off by default) the other runtime evidence decides.
        let tests_evidence = if tests.is_empty() {
            TestEvidence::NoTests
        } else if root_diff.is_empty() {
            TestEvidence::NotRun("the change is empty".to_string())
        } else {
            self.open_bench(&mut bench, reuse.clone(), &budget, jobs).await;
            match bench.as_mut() {
                None => TestEvidence::NotRun("no evaluation worktree".to_string()),
                Some(b) => {
                    for t in &tests {
                        b.qualify(t).await;
                    }
                    let cells = b.cells_on(1, &root_diff, &tests).await;
                    let named = |want: &[Cell]| -> Vec<String> {
                        cells
                            .iter()
                            .filter(|(_, (c, _))| want.contains(c))
                            .map(|(id, _)| id.clone())
                            .collect()
                    };
                    let passed = named(&[Cell::Pass]);
                    let failed = named(&[Cell::Fail, Cell::Conflict]);
                    if !passed.is_empty() {
                        TestEvidence::Passed(passed)
                    } else if !failed.is_empty() {
                        TestEvidence::Failed(failed)
                    } else {
                        TestEvidence::NotRun("the qualified tests could not be built or run".to_string())
                    }
                }
            }
        };
        let evidence = Evidence {
            diff_empty: root_diff.is_empty(),
            gate: evidence::gate_verdict(&gate_log, observed.turn_id.as_deref()),
            uncertainty: observed
                .last_message
                .as_deref()
                .map(evidence::uncertainty_markers)
                .unwrap_or_default(),
            tests: tests_evidence,
        };
        rep.triggers = evidence::triggers(&evidence);
        self.event(format!(
            "attempts: evidence after attempt 1: triggers {:?} (gate {:?})",
            rep.triggers, evidence.gate
        ))
        .await;
        rep.evidence = Some(evidence);

        let go = match mode {
            AttemptsMode::Off => false,
            AttemptsMode::Adaptive => !rep.triggers.is_empty(),
            AttemptsMode::Fixed(_) => true,
        };
        let mut evals: BTreeMap<usize, CandidateEval> = BTreeMap::new();
        if !go {
            rep.stopped = "strong evidence after attempt 1 (no trigger fired)".to_string();
        } else if cap < 2 {
            rep.stopped = "attempt cap is 1".to_string();
        } else {
            let mut k = 2;
            loop {
                if let Err(why) = self.may_start(&settings).await {
                    rep.stopped = why;
                    break;
                }
                let reserve = budget.eval_reserve(
                    Instant::now(),
                    bench.as_ref().and_then(|b| b.eval_time),
                    settings.judge,
                );
                let child_deadline = match budget.child_deadline(
                    Instant::now(),
                    reserve,
                    Duration::from_secs(settings.min_attempt_secs),
                ) {
                    Ok(d) => d,
                    Err(why) => {
                        rep.stopped = why;
                        break;
                    }
                };
                let (run, cand, test) = self.run_attempt(k, child_deadline, &settings, &root_tree, &noise).await;
                rep.attempts.push(run);
                cands.push(cand);
                if let Some(t) = test {
                    tests.push(t);
                }
                let reports: BTreeMap<usize, SubagentReport> = rep
                    .attempts
                    .iter()
                    .filter_map(|a| a.report.clone().map(|r| (a.attempt, r)))
                    .collect();
                self.evaluate_pool(&mut bench, reuse.clone(), &budget, jobs, &cands, &tests, &mut evals, &repro, &reports)
                    .await;
                if self.attempts.cancel.is_cancelled() {
                    rep.stopped = "cancelled".to_string();
                    break;
                }
                if k >= cap {
                    rep.stopped = format!("attempt cap ({cap}) reached");
                    break;
                }
                if mode == AttemptsMode::Adaptive {
                    let pooled = bench.as_ref().map(|b| b.pooled(&tests, &list(&evals))).unwrap_or_default();
                    let (disagree, reason) = selector::disagreement(&list(&evals), &pooled);
                    self.event(format!("attempts: after attempt {k}: disagree = {disagree} ({reason})"))
                        .await;
                    rep.escalations.push(Escalation {
                        after_attempt: k,
                        disagree,
                        reason: reason.clone(),
                    });
                    if !disagree {
                        rep.stopped = format!("attempts agree ({reason})");
                        break;
                    }
                }
                k += 1;
            }
        }

        // Select and apply (only when there is more than one candidate).
        if cands.len() > 1 && !self.attempts.cancel.is_cancelled() {
            let evals_list = list(&evals);
            let pooled = bench.as_ref().map(|b| b.pooled(&tests, &evals_list)).unwrap_or_default();
            let diffs: BTreeMap<usize, Vec<u8>> = cands.iter().map(|c| (c.attempt, c.diff.clone())).collect();
            let use_judge = settings.judge && budget.left(Instant::now()) > Duration::from_secs(60);
            let judge = RealJudge {
                orch: self.clone(),
                diffs: diffs.clone(),
                tests: pooled.clone(),
                budget: budget.clone(),
                max_tokens: settings.max_tokens,
            };
            let judge_ref: Option<&dyn Judge> = if use_judge { Some(&judge) } else { None };
            let selection = selector::select(&evals_list, &pooled, judge_ref).await;
            self.event(format!(
                "attempts: selection: winner {:?} ({})",
                selection.winner, selection.reason
            ))
            .await;
            let winner = selection.winner.unwrap_or(1);
            if self.attempts.cancel.is_cancelled() {
                rep.apply_note = "cancelled before applying; the root's change is kept".to_string();
            } else {
                let (applied, note) = self.apply_winner(winner, &diffs, &noise).await;
                rep.applied = applied;
                rep.apply_note = note;
            }
            self.event(format!("attempts: applied attempt {}: {}", rep.applied, rep.apply_note))
                .await;
            rep.selection = Some(selection);
            rep.pooled = pooled;
        } else if rep.apply_note.is_empty() {
            rep.apply_note = "single candidate; the root's change is kept".to_string();
        }
        rep.candidates = list(&evals);
        rep.tests = tests;
        if let Some(b) = bench {
            rep.bench = Some(BenchReport {
                reused_writer_worktree: b.reused,
                recipe: b.recipe.clone(),
                base: b.base.clone(),
            });
            rep.steps = b.log;
        }
        self.store_report(rep, started).await;
    }

    async fn store_report(&self, mut rep: AttemptsReport, started: Instant) {
        let children = self.attempts.children();
        rep.tokens_total = meter::sum(&children);
        meter::forget(&children);
        rep.elapsed_secs = started.elapsed().as_secs();
        let runs: Vec<String> = rep
            .attempts
            .iter()
            .map(|a| {
                let state = a.state.as_ref().map_or_else(
                    || a.spawn_error.clone().unwrap_or_else(|| "not started".to_string()),
                    short_state,
                );
                format!("{}: {state}, {}s, {} tokens", a.attempt, a.elapsed_secs, a.tokens)
            })
            .collect();
        rep.summary = format!(
            "tier-b mode {}: triggers {:?}; attempts run {} [{}]; stopped: {}; winner {} ({}); applied {}: {}; sub-agent tokens {}; {}s",
            rep.mode,
            rep.triggers,
            1 + rep.attempts.len(),
            runs.join("; "),
            rep.stopped,
            rep.selection
                .as_ref()
                .and_then(|s| s.winner)
                .map_or_else(|| "-".to_string(), |w| w.to_string()),
            rep.selection.as_ref().map_or("no selection", |s| s.reason.as_str()),
            rep.applied,
            rep.apply_note,
            rep.tokens_total,
            rep.elapsed_secs
        );
        tracing::info!(target: "byteask_subagents", "{}", rep.summary);
        self.event(rep.summary.clone()).await;
        if let Ok(mut slot) = self.attempts.report.lock() {
            *slot = Some(rep);
        }
    }

    async fn open_bench(&self, bench: &mut Option<Bench>, reuse: Option<std::path::PathBuf>, budget: &Budget, jobs: usize) {
        if bench.is_some() {
            return;
        }
        match Bench::open(&self.p.repo, &self.snapshot, &self.scratch, reuse, self.attempts_ctx(budget), jobs).await {
            Ok(b) => {
                self.event(format!(
                    "attempts: evaluation worktree {} (writer's reused: {})",
                    b.dir.display(),
                    b.reused
                ))
                .await;
                *bench = Some(b);
            }
            Err(e) => {
                self.event(format!("attempts: no evaluation worktree ({e})")).await;
            }
        }
    }

    /// Can another attempt start? Token cap, disk, memory headroom.
    async fn may_start(&self, settings: &AttemptsSettings) -> Result<(), String> {
        if self.attempts.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let used = meter::sum(&self.attempts.children());
        if used >= settings.max_tokens {
            return Err(format!("token cap reached ({used} of {})", settings.max_tokens));
        }
        if let Some(free) = meter::free_bytes(&self.scratch)
            && free < MIN_FREE_BYTES
        {
            return Err(format!("only {} MB of free disk for a new worktree", free >> 20));
        }
        for _ in 0..12 {
            match meter::mem_headroom() {
                Some(h) if h < MIN_HEADROOM => tokio::time::sleep(Duration::from_secs(5)).await,
                _ => return Ok(()),
            }
        }
        Err("memory headroom below 1 GB for a minute".to_string())
    }

    /// Spawn attempt `k`, wait for it, take its change and tests, and remove its worktree.
    async fn run_attempt(
        &self,
        k: usize,
        deadline: Instant,
        settings: &AttemptsSettings,
        root_tree: &str,
        noise: &BTreeSet<String>,
    ) -> (AttemptRun, Cand, Option<TestPatch>) {
        let started = Instant::now();
        let dir = self.scratch.join(format!("wt-{k}"));
        let child_dir = self.scratch.join(format!("attempt-{k}"));
        let _ = std::fs::create_dir_all(&child_dir);
        let tests_patch = child_dir.join("tests.patch");
        let high_effort = k.is_multiple_of(2);
        let mut run = AttemptRun {
            attempt: k,
            high_effort,
            strategy: prompts::strategy_card(k).to_string(),
            thread_id: None,
            spawn_error: None,
            state: None,
            stopped: None,
            elapsed_secs: 0,
            tokens: 0,
            diff_bytes: 0,
            tests_bytes: 0,
            report: None,
            disqualified: None,
        };
        let mut cand = Cand {
            attempt: k,
            diff: Vec::new(),
            finished: false,
            disqualified: None,
        };
        if let Err(e) = gitops::worktree_add(&self.p.repo, &dir, &self.snapshot.commit).await {
            run.spawn_error = Some(format!("worktree: {e}"));
            cand.disqualified = Some("could not start".to_string());
            return (run, cand, None);
        }
        let prompt = prompts::attempt_brief(&self.p.task, k, &self.p.repo, &dir, &tests_patch);
        let spawned = self
            .host
            .spawn(ChildSpec {
                role: Role::Attempt(k),
                cwd: dir.clone(),
                prompt,
                high_effort,
                env: self.env.clone(),
                extra_writable: vec![child_dir.clone()],
            })
            .await;
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                self.event(format!("attempts: attempt {k} failed to start: {e}")).await;
                run.spawn_error = Some(e);
                cand.disqualified = Some("could not start".to_string());
                gitops::worktree_remove(&self.p.repo, &dir).await;
                return (run, cand, None);
            }
        };
        let id = handle.id();
        run.thread_id = Some(id.clone());
        meter::track(&id);
        if let Ok(mut c) = self.attempts.children.lock() {
            c.push(id.clone());
        }
        self.attempts.add_live(handle.clone());
        let counter = self.counters.register(&id);
        self.event(format!("attempts: attempt {k} started ({id}, high effort: {high_effort})"))
            .await;
        let (state, stopped) = self
            .wait_child_capped(&handle, deadline, &counter, settings.child_max_steps, settings.max_tokens)
            .await;
        self.counters.unregister(&id);
        handle.shutdown().await;
        self.attempts.remove_live(&id);
        run.tokens = meter::used(&id);
        run.report = final_message(&state).and_then(parse_report);
        cand.finished = matches!(state, ChildState::Done(_));
        self.event(format!(
            "attempts: attempt {k} finished: {}{}",
            short_state(&state),
            stopped.as_deref().map(|s| format!(" ({s})")).unwrap_or_default()
        ))
        .await;
        run.state = Some(state);
        run.stopped = stopped;

        // Its change: the fix (new tests split off) and its tests.
        let tmp = self.scratch.join("tmp");
        let full = gitops::candidate_diff(&dir, &self.snapshot, &tmp).await.unwrap_or_default();
        let (fix, split_off) = patches::split_tests(&patches::drop_paths(&full, noise));
        let saved = std::fs::read(&tests_patch).unwrap_or_default();
        let test_patch = if saved.is_empty() { split_off } else { saved };
        cand.diff = fix;
        run.diff_bytes = cand.diff.len();
        run.tests_bytes = test_patch.len();
        if let Some(why) = self.guard_real_tree(root_tree, &cand.diff, noise).await {
            run.disqualified = Some(why.clone());
            cand.disqualified = Some(why);
        }
        gitops::worktree_remove(&self.p.repo, &dir).await;
        let test = run.report.as_ref().and_then(|r| {
            (!test_patch.is_empty() && !r.run.trim().is_empty() && !r.build.trim().is_empty()).then(|| TestPatch {
                id: format!("attempt-{k}"),
                author: Author::Attempt(k),
                patch: test_patch.clone(),
                build: r.build.clone(),
                run: r.run.clone(),
                prequalified: false,
            })
        });
        run.elapsed_secs = started.elapsed().as_secs();
        (run, cand, test)
    }

    /// Wait for a child; interrupt it at the deadline, the step cap, the token cap of all
    /// attempt/judge children together, or on cancellation.
    async fn wait_child_capped(
        &self,
        handle: &Arc<dyn ChildHandle>,
        deadline: Instant,
        counter: &AtomicUsize,
        max_steps: usize,
        max_tokens: u64,
    ) -> (ChildState, Option<String>) {
        loop {
            let state = handle.state().await;
            if state.is_terminal() {
                return (state, None);
            }
            let reason = if self.attempts.cancel.is_cancelled() {
                Some("cancelled")
            } else if Instant::now() >= deadline {
                Some("deadline")
            } else if max_steps > 0 && counter.load(Ordering::Relaxed) >= max_steps {
                Some("step cap")
            } else if meter::sum(&self.attempts.children()) >= max_tokens {
                Some("token cap")
            } else {
                None
            };
            if let Some(reason) = reason {
                handle.interrupt().await;
                let grace = Instant::now() + Duration::from_secs(30);
                loop {
                    let state = handle.state().await;
                    if state.is_terminal() {
                        return (state, Some(reason.to_string()));
                    }
                    if Instant::now() >= grace {
                        return (ChildState::Interrupted, Some(reason.to_string()));
                    }
                    tokio::time::sleep(POLL).await;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                _ = self.attempts.cancel.cancelled() => {}
            }
        }
    }

    /// Attempts must not touch the real repository. If the real tree changed on paths the
    /// attempt's change touches (or the attempt left its worktree unchanged), the real tree is
    /// restored and the attempt disqualified. Returns why.
    async fn guard_real_tree(&self, root_tree: &str, attempt_diff: &[u8], noise: &BTreeSet<String>) -> Option<String> {
        let tmp = self.scratch.join("tmp");
        let now = gitops::capture_tree(&self.p.repo, &self.snapshot.commit, &tmp).await.ok()?;
        if now == root_tree {
            return None;
        }
        let delta = gitops::diff_trees(&self.p.repo, root_tree, &now).await.ok()?;
        let changed: BTreeSet<String> = gitops::changed_paths(&delta)
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !noise.contains(p))
            .collect();
        if changed.is_empty() {
            return None;
        }
        let own: BTreeSet<String> = gitops::changed_paths(attempt_diff).into_iter().map(|(p, _)| p).collect();
        if !attempt_diff.is_empty() && changed.is_disjoint(&own) {
            self.event(format!(
                "attempts: the real tree changed outside the attempt's files ({}); left as is",
                changed.iter().cloned().collect::<Vec<_>>().join(", ")
            ))
            .await;
            return None;
        }
        let back = gitops::diff_trees(&self.p.repo, &now, root_tree).await.ok()?;
        let restored = gitops::apply(&self.p.repo, &back, false, false).await.is_ok();
        let why = format!(
            "changed the real repository ({}); restored: {restored}",
            changed.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        self.event(format!("attempts: {why}")).await;
        Some(why)
    }

    /// Build recipes in order of trust: explicit settings, the independent writer's report,
    /// the attempts' reports, detection from the layout.
    fn recipes(&self, repro: &Option<ReproOutcome>, cands: &[Cand], reports: &BTreeMap<usize, SubagentReport>, root: &Path) -> Vec<Recipe> {
        let s = &self.p.settings;
        let mut out: Vec<Recipe> = Vec::new();
        let from_report = |r: &SubagentReport, source: String| Recipe {
            build: r.build.clone(),
            test: Some(r.test.clone()).filter(|t| !t.trim().is_empty()),
            source,
        };
        if let Some(build) = &s.build_cmd {
            out.push(Recipe {
                build: build.clone(),
                test: None,
                source: "settings".to_string(),
            });
        }
        if let Some(r) = repro.as_ref().and_then(|r| r.report.as_ref()).filter(|r| r.has_recipe()) {
            out.push(from_report(r, "independent test writer".to_string()));
        }
        for c in cands {
            if let Some(r) = reports.get(&c.attempt).filter(|r| r.has_recipe()) {
                out.push(from_report(r, format!("attempt {}", c.attempt)));
            }
        }
        if let Some(r) = evaluate::detect_recipe(root, jobs_per_build(s)) {
            out.push(r);
        }
        out
    }

    /// Bring the evaluation up to date: recipe and base record, qualification of new tests,
    /// new tests on candidates evaluated earlier, and a full evaluation of new candidates.
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_pool(
        &self,
        bench: &mut Option<Bench>,
        reuse: Option<std::path::PathBuf>,
        budget: &Budget,
        jobs: usize,
        cands: &[Cand],
        tests: &[TestPatch],
        evals: &mut BTreeMap<usize, CandidateEval>,
        repro: &Option<ReproOutcome>,
        reports: &BTreeMap<usize, SubagentReport>,
    ) {
        self.open_bench(bench, reuse, budget, jobs).await;
        let Some(b) = bench.as_mut() else {
            // No bench: candidates are compared on what needs no build.
            for c in cands {
                evals.entry(c.attempt).or_insert_with(|| static_eval(c));
            }
            return;
        };
        let recipes = self.recipes(repro, cands, reports, &b.dir);
        if b.ensure_recipe(&recipes).await {
            b.base_suite().await;
        }
        let new_tests: Vec<TestPatch> = tests
            .iter()
            .filter(|t| !b.base.qualified.contains_key(&t.id))
            .cloned()
            .collect();
        let mut qualified_new = Vec::new();
        for t in &new_tests {
            if b.qualify(t).await {
                qualified_new.push(t.clone());
            }
        }
        // New qualified tests on candidates evaluated earlier.
        if !qualified_new.is_empty() {
            for c in cands {
                let Some(e) = evals.get_mut(&c.attempt) else {
                    continue;
                };
                if c.diff.is_empty() || c.disqualified.is_some() || e.build_ok == Some(false) {
                    continue;
                }
                for (id, (cell, san)) in b.cells_on(c.attempt, &c.diff, &qualified_new).await {
                    e.cells.insert(id, cell);
                    e.new_sanitizer_reports += san;
                }
            }
        }
        // New candidates.
        for c in cands {
            if evals.contains_key(&c.attempt) {
                continue;
            }
            let e = b
                .evaluate(
                    CandidateInput {
                        attempt: c.attempt,
                        diff: &c.diff,
                        finished: c.finished,
                        disqualified: c.disqualified.clone(),
                    },
                    tests,
                )
                .await;
            self.event(format!(
                "attempts: candidate {}: build {:?}, regressions {:?}, cells {:?}",
                e.attempt, e.build_ok, e.regressions, e.cells
            ))
            .await;
            evals.insert(c.attempt, e);
        }
    }

    /// Put the winner's change into the real tree in place of the root's: one `git apply` of
    /// the difference between the current tree and "snapshot + winner", built without a work
    /// tree. On any failure the root's change stays.
    async fn apply_winner(&self, winner: usize, diffs: &BTreeMap<usize, Vec<u8>>, noise: &BTreeSet<String>) -> (usize, String) {
        if winner == 1 {
            return (1, "the root's own change is kept".to_string());
        }
        let Some(diff) = diffs.get(&winner) else {
            return (1, "the winner has no change; the root's is kept".to_string());
        };
        let tmp = self.scratch.join("tmp");
        let target = match patches::tree_with_patch(&self.p.repo, &self.snapshot.tree, diff, &tmp).await {
            Ok(t) => t,
            Err(e) => return (1, format!("the winner's change does not apply to the snapshot ({e}); the root's is kept")),
        };
        let current = match gitops::capture_tree(&self.p.repo, &self.snapshot.commit, &tmp).await {
            Ok(t) => t,
            Err(e) => return (1, format!("cannot read the real tree ({e}); the root's change is kept")),
        };
        // Files untracked at session start keep whatever state the session left them in.
        let delta = match gitops::diff_trees(&self.p.repo, &current, &target).await {
            Ok(d) => patches::drop_paths(&d, noise),
            Err(e) => return (1, format!("cannot diff ({e}); the root's change is kept")),
        };
        if let Err(e) = gitops::apply(&self.p.repo, &delta, false, true).await {
            return (1, format!("the switch does not apply cleanly ({e}); the root's change is kept"));
        }
        if let Err(e) = gitops::apply(&self.p.repo, &delta, false, false).await {
            return (1, format!("applying the switch failed ({e}); the root's change is kept"));
        }
        let after = gitops::capture_tree(&self.p.repo, &self.snapshot.commit, &tmp).await.unwrap_or_default();
        let rest = gitops::diff_trees(&self.p.repo, &after, &target).await.unwrap_or_default();
        let matches = patches::drop_paths(&rest, noise).is_empty();
        (
            winner,
            format!("attempt {winner} applied to the real tree (tree matches: {matches})"),
        )
    }
}

fn list(evals: &BTreeMap<usize, CandidateEval>) -> Vec<CandidateEval> {
    evals.values().cloned().collect()
}

/// A candidate evaluated without any execution (no evaluation worktree).
fn static_eval(c: &Cand) -> CandidateEval {
    CandidateEval {
        attempt: c.attempt,
        diff_bytes: c.diff.len(),
        changed_files: patches::changed_files(&c.diff),
        changed_lines: patches::changed_lines(&c.diff),
        normalized: gitops::normalized_change(&c.diff),
        test_file_edits: bench::test_file_edits(&c.diff),
        finished: c.finished,
        disqualified: c.disqualified.clone(),
        ..CandidateEval::default()
    }
}

/// The fresh-context pairwise judge: a short sub-agent session that sees only the task, the
/// two diffs and the check results (never an attempt's transcript).
struct RealJudge {
    orch: Arc<Orchestration>,
    diffs: BTreeMap<usize, Vec<u8>>,
    tests: Vec<PooledTest>,
    budget: Budget,
    max_tokens: u64,
}

impl RealJudge {
    fn checks(&self, c: &CandidateEval) -> String {
        let passed: Vec<&str> = self
            .tests
            .iter()
            .filter(|t| t.qualified && t.valid && c.cells.get(&t.id) == Some(&Cell::Pass))
            .map(|t| t.id.as_str())
            .collect();
        let qualified = self.tests.iter().filter(|t| t.qualified && t.valid).count();
        format!(
            "builds: {}\nnew failures among existing tests: {}\nqualified tests passed: {}/{} {:?}\nnew compiler warnings in changed files: {}\nnew sanitizer reports: {}\nfiles changed: {}, lines changed: {}",
            c.build_ok.map_or("unknown".to_string(), |b| b.to_string()),
            c.regressions
                .as_ref()
                .map_or("unknown".to_string(), |r| r.len().to_string()),
            passed.len(),
            qualified,
            passed,
            c.new_warnings,
            c.new_sanitizer_reports,
            c.changed_files,
            c.changed_lines,
        )
    }

    fn diff_text(&self, attempt: usize) -> String {
        let raw = self
            .diffs
            .get(&attempt)
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .unwrap_or_default();
        if raw.len() <= MAX_JUDGE_DIFF_CHARS {
            return raw;
        }
        let mut cut = MAX_JUDGE_DIFF_CHARS;
        while !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n[... diff truncated ...]", &raw[..cut])
    }
}

impl Judge for RealJudge {
    fn compare<'a>(&'a self, first: &'a CandidateEval, second: &'a CandidateEval) -> JudgeFuture<'a> {
        Box::pin(async move {
            let orch = &self.orch;
            if orch.attempts.cancel.is_cancelled() || meter::sum(&orch.attempts.children()) >= self.max_tokens {
                return None;
            }
            let deadline = (Instant::now() + JUDGE_BUDGET).min(self.budget.attempts_end());
            if deadline <= Instant::now() {
                return None;
            }
            let dir = orch.scratch.join("judge");
            let _ = std::fs::create_dir_all(&dir);
            let prompt = prompts::judge_prompt(
                &orch.p.task,
                (&self.diff_text(first.attempt), &self.checks(first)),
                (&self.diff_text(second.attempt), &self.checks(second)),
            );
            let handle = orch
                .host
                .spawn(ChildSpec {
                    role: Role::Judge,
                    cwd: dir,
                    prompt,
                    high_effort: false,
                    env: Vec::new(),
                    extra_writable: Vec::new(),
                })
                .await
                .ok()?;
            let id = handle.id();
            meter::track(&id);
            if let Ok(mut c) = orch.attempts.children.lock() {
                c.push(id.clone());
            }
            orch.attempts.add_live(handle.clone());
            let counter = orch.counters.register(&id);
            let (state, _) = orch
                .wait_child_capped(&handle, deadline, &counter, JUDGE_MAX_STEPS, self.max_tokens)
                .await;
            orch.counters.unregister(&id);
            handle.shutdown().await;
            orch.attempts.remove_live(&id);
            let verdict = final_message(&state)
                .and_then(prompts::parse_judge)
                .map(|a| if a { Pick::First } else { Pick::Second });
            orch.event(format!(
                "attempts: judge {} vs {} ({id}): {verdict:?}",
                first.attempt, second.attempt
            ))
            .await;
            verdict
        })
    }
}
