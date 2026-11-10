//! One orchestration per root session: the independent reproduction-test writer.
//!
//! Timeline
//! 1. Root's first turn starts: snapshot HEAD + working tree (synchronously, before the root can
//!    edit anything); start the writer in the background, as a fresh sub-agent in a detached
//!    worktree outside the repository.
//! 2. When the writer is done, the engine re-checks its tests on the starting code (they must
//!    fail, or fail to compile), freezes them, and injects them into the root's running turn as
//!    a message. Nobody waits for the writer.
//! 3. Root's turn ends: stop the writer if it is still running, make sure the frozen tests are
//!    not in the real tree, remove every worktree and scratch file, and write a JSON report under
//!    `CODEX_HOME/byteask-subagents/`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ccache;
use crate::evaluate;
use crate::evaluate::EvalContext;
use crate::evaluate::StepLog;
use crate::gitops;
use crate::gitops::Snapshot;
use crate::host::ChildHandle;
use crate::host::ChildHost;
use crate::host::ChildSpec;
use crate::host::ChildState;
use crate::host::Role;
use crate::prompts;
use crate::report::SubagentReport;
use crate::report::parse_report;
use crate::runner;
use crate::settings::Settings;

pub(crate) const POLL: Duration = Duration::from_millis(500);

/// Model-request counters per child thread, fed by the token-usage hook.
#[derive(Default)]
pub struct StepCounters {
    map: StdMutex<HashMap<String, Arc<AtomicUsize>>>,
}

impl StepCounters {
    pub fn register(&self, thread_id: &str) -> Arc<AtomicUsize> {
        let counter = Arc::new(AtomicUsize::new(0));
        if let Ok(mut map) = self.map.lock() {
            map.insert(thread_id.to_string(), counter.clone());
        }
        counter
    }

    pub fn register_or_get(&self, thread_id: &str) -> Arc<AtomicUsize> {
        if let Ok(map) = self.map.lock()
            && let Some(c) = map.get(thread_id)
        {
            return c.clone();
        }
        self.register(thread_id)
    }

    pub fn bump(&self, thread_id: &str) {
        if let Ok(map) = self.map.lock()
            && let Some(counter) = map.get(thread_id)
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn unregister(&self, thread_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(thread_id);
        }
    }
}

pub struct Params {
    pub settings: Settings,
    pub repo: PathBuf,
    pub task: String,
    pub root_id: String,
    pub codex_home: PathBuf,
    pub run_repro: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FrozenTests {
    #[serde(skip)]
    pub patch: Vec<u8>,
    pub patch_path: PathBuf,
    pub script: PathBuf,
    pub build: String,
    pub run: String,
    pub test_suite: Option<String>,
    pub names: Vec<String>,
    pub fails_because: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReproOutcome {
    pub thread_id: Option<String>,
    pub state: Option<ChildState>,
    pub report: Option<SubagentReport>,
    pub frozen: Option<FrozenTests>,
    pub delivered_to_root: bool,
    pub note: String,
    pub log: Vec<StepLog>,
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

#[derive(Default, Serialize)]
pub struct FinalReport {
    pub root_thread: String,
    pub repro_tests: bool,
    pub snapshot: String,
    pub started_unix: u64,
    pub elapsed_ms: u64,
    pub repro: Option<ReproOutcome>,
    pub leaked_tests_removed: bool,
    pub leftover_worktrees: Vec<PathBuf>,
    pub events: Vec<String>,
    /// Tier B: attempts, triggers, selector decision (absent when the mode is off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<crate::attempts::AttemptsReport>,
}

pub(crate) struct Inner {
    pub(crate) launcher: Option<JoinHandle<()>>,
    pub(crate) repro_task: Option<JoinHandle<ReproOutcome>>,
    pub(crate) repro: Option<ReproOutcome>,
    pub(crate) events: Vec<String>,
    pub(crate) report: FinalReport,
}

pub struct Orchestration {
    pub(crate) p: Params,
    pub(crate) host: Arc<dyn ChildHost>,
    pub(crate) counters: Arc<StepCounters>,
    pub(crate) scratch: PathBuf,
    pub(crate) snapshot: Snapshot,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) started: Instant,
    pub(crate) started_unix: u64,
    pub(crate) deadline: Instant,
    pub(crate) cancel: CancellationToken,
    pub(crate) finalized: AtomicBool,
    pub(crate) frozen: StdMutex<Option<FrozenTests>>,
    pub(crate) inner: Mutex<Inner>,
    /// Tier B (adaptive multi-attempt); inert until configured.
    pub attempts: crate::attempts::AttemptsSlot,
}

fn scratch_dir(settings: &Settings, repo: &Path, id: &str) -> Result<PathBuf, String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(root) = &settings.scratch_root {
        roots.push(root.clone());
    }
    roots.push(std::env::temp_dir());
    roots.push(PathBuf::from("/var/tmp"));
    roots.push(PathBuf::from("/tmp"));
    let repo_real = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    for root in roots {
        let dir = root.join("byteask-subagents").join(id);
        let real_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
        if real_root.starts_with(&repo_real) {
            continue; // never inside the repository
        }
        if std::fs::create_dir_all(&dir).is_ok() {
            return Ok(dir);
        }
    }
    Err("no writable scratch directory outside the repository".to_string())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parallel jobs per build: the memory-aware job count of the C/C++ tools (CPUs, bounded by
/// available memory at about 1.5 GB per job), shared among the allowed parallel runs.
pub(crate) fn jobs_per_build(settings: &Settings) -> usize {
    (codex_cpp_tools::memory_aware_jobs() / settings.max_parallel_runs.max(1)).max(1)
}

/// Heuristic: does the task read like an issue to resolve (as opposed to a question)?
pub fn looks_like_issue(task: &str) -> bool {
    const CUES: &[&str] = &[
        "bug",
        "issue",
        "error",
        "fail",
        "crash",
        "incorrect",
        "wrong",
        "expected",
        "should",
        "doesn't",
        "does not",
        "fix",
        "support",
        "implement",
        "feature",
        "regression",
        "segfault",
        "exception",
        "undefined",
        "leak",
        "overflow",
        "unexpected",
        "broken",
        "warning",
        "add ",
    ];
    let words = task.split_whitespace().count();
    let lower = task.to_ascii_lowercase();
    words >= 25 && CUES.iter().any(|cue| lower.contains(cue))
}

/// Poll a child until it is terminal; interrupt it at the deadline, at the step cap, or on
/// cancellation.
pub(crate) async fn wait_child(
    handle: &Arc<dyn ChildHandle>,
    deadline: Instant,
    counter: Option<(&AtomicUsize, usize)>,
    cancel: &CancellationToken,
) -> ChildState {
    loop {
        let state = handle.state().await;
        if state.is_terminal() {
            return state;
        }
        let over_steps =
            counter.is_some_and(|(c, max)| max > 0 && c.load(Ordering::Relaxed) >= max);
        if Instant::now() >= deadline || over_steps || cancel.is_cancelled() {
            handle.interrupt().await;
            let grace = Instant::now() + Duration::from_secs(30);
            loop {
                let state = handle.state().await;
                if state.is_terminal() {
                    return state;
                }
                if Instant::now() >= grace {
                    return ChildState::Interrupted;
                }
                tokio::time::sleep(POLL).await;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = cancel.cancelled() => {}
        }
    }
}

pub(crate) fn final_message(state: &ChildState) -> Option<&str> {
    match state {
        ChildState::Done(Some(msg)) => Some(msg.as_str()),
        _ => None,
    }
}

pub(crate) fn short_state(state: &ChildState) -> String {
    match state {
        ChildState::Running => "running".to_string(),
        ChildState::Done(Some(m)) => format!("done ({} chars)", m.len()),
        ChildState::Done(None) => "done (no message)".to_string(),
        ChildState::Interrupted => "interrupted".to_string(),
        ChildState::Failed(e) => format!("failed: {e}"),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

impl Orchestration {
    /// Snapshot now; start sub-agents in the background.
    pub async fn start(
        p: Params,
        host: Arc<dyn ChildHost>,
        counters: Arc<StepCounters>,
    ) -> Result<Arc<Self>, String> {
        let scratch = scratch_dir(&p.settings, &p.repo, &p.root_id)?;
        let snapshot = match tokio::time::timeout(
            Duration::from_secs(120),
            gitops::snapshot(&p.repo, &p.root_id, &scratch.join("tmp")),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(e);
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err("snapshot timed out".to_string());
            }
        };
        let env = ccache::build_env(&scratch, jobs_per_build(&p.settings));
        let slots = runner::run_slots(p.settings.max_parallel_runs);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(p.settings.deadline_secs);
        let orch = Arc::new(Self {
            host,
            counters,
            scratch,
            snapshot,
            env,
            slots,
            started,
            started_unix: epoch_secs(),
            deadline,
            cancel: CancellationToken::new(),
            finalized: AtomicBool::new(false),
            frozen: StdMutex::new(None),
            inner: Mutex::new(Inner {
                launcher: None,
                repro_task: None,
                repro: None,
                events: Vec::new(),
                report: FinalReport::default(),
            }),
            attempts: crate::attempts::AttemptsSlot::default(),
            p,
        });
        orch.event(format!(
            "start: repo {}, snapshot {}, scratch {}, repro {}",
            orch.p.repo.display(),
            orch.snapshot.commit,
            orch.scratch.display(),
            orch.p.run_repro
        ))
        .await;
        let launcher = {
            let orch = orch.clone();
            tokio::spawn(async move {
                if orch.p.run_repro {
                    let task = tokio::spawn(orch.clone().run_repro());
                    orch.inner.lock().await.repro_task = Some(task);
                }
            })
        };
        orch.inner.lock().await.launcher = Some(launcher);
        Ok(orch)
    }

    pub(crate) async fn event(&self, text: String) {
        let t = self.started.elapsed().as_secs();
        tracing::info!(target: "byteask_subagents", "{text}");
        self.inner.lock().await.events.push(format!("+{t}s {text}"));
    }

    pub(crate) fn eval_ctx(&self) -> EvalContext {
        EvalContext {
            snapshot: self.snapshot.clone(),
            scratch: self.scratch.clone(),
            env: self.env.clone(),
            slots: self.slots.clone(),
            deadline: self.deadline,
            build_timeout: Duration::from_secs(self.p.settings.build_timeout_secs),
            test_timeout: Duration::from_secs(self.p.settings.test_timeout_secs),
            cancel: self.cancel.clone(),
        }
    }

    /// The independent reproduction-test writer: spawn, wait, qualify, freeze, deliver.
    async fn run_repro(self: Arc<Self>) -> ReproOutcome {
        let mut out = ReproOutcome::default();
        let dir = self.scratch.join("wt-repro");
        if let Err(e) = gitops::worktree_add(&self.p.repo, &dir, &self.snapshot.commit).await {
            out.note = format!("worktree: {e}");
            return out;
        }
        out.dir = Some(dir.clone());
        let patch_dir = self.scratch.join("repro");
        let _ = std::fs::create_dir_all(&patch_dir);
        let patch_path = patch_dir.join("tests.patch");
        let s = &self.p.settings;
        let prompt = prompts::repro_brief(
            &self.p.task,
            &dir,
            &patch_path,
            s.repro_max_steps,
            s.repro_deadline_secs / 60,
        );
        let handle = match self
            .host
            .spawn(ChildSpec {
                role: Role::Repro,
                cwd: dir.clone(),
                prompt,
                high_effort: false,
                env: self.env.clone(),
                extra_writable: vec![patch_dir.clone()],
            })
            .await
        {
            Ok(h) => h,
            Err(e) => {
                out.note = format!("spawn: {e}");
                return out;
            }
        };
        let id = handle.id();
        out.thread_id = Some(id.clone());
        self.event(format!("repro writer started ({id})")).await;
        let counter = self.counters.register(&id);
        let deadline =
            (Instant::now() + Duration::from_secs(s.repro_deadline_secs)).min(self.deadline);
        let state = wait_child(
            &handle,
            deadline,
            Some((counter.as_ref(), s.repro_max_steps)),
            &self.cancel,
        )
        .await;
        self.counters.unregister(&id);
        handle.shutdown().await;
        self.event(format!("repro writer finished: {}", short_state(&state)))
            .await;
        out.report = final_message(&state).and_then(parse_report);
        out.state = Some(state);
        if self.cancel.is_cancelled() {
            out.note = "cancelled before qualification".to_string();
            return out;
        }
        // The tests: what the writer left in its worktree (filtered by the same rules as any
        // change, so build output never gets in), else the patch file it saved.
        let tmp = self.scratch.join("tmp");
        let current = gitops::candidate_diff(&dir, &self.snapshot, &tmp)
            .await
            .unwrap_or_default();
        let patch = if current.is_empty() {
            std::fs::read(&patch_path).unwrap_or_default()
        } else {
            current.clone()
        };
        if patch.is_empty() {
            out.note = "no tests produced".to_string();
            return out;
        }
        if current.is_empty() && gitops::apply(&dir, &patch, false, false).await.is_err() {
            out.note = "tests patch does not apply on the starting code".to_string();
            return out;
        }
        let report = out.report.clone().unwrap_or_default();
        let build = match report.build.trim() {
            "" => s
                .build_cmd
                .clone()
                .or_else(|| evaluate::detect_recipe(&dir, jobs_per_build(s)).map(|r| r.build))
                .unwrap_or_default(),
            b => b.to_string(),
        };
        let run = report.run.trim().to_string();
        if build.is_empty() || run.is_empty() {
            out.note = "no build/run command reported; tests cannot be verified".to_string();
            return out;
        }
        // Qualification: the project builds without the tests; with them it fails to build or
        // the tests fail.
        let ctx = self.eval_ctx();
        let _ = gitops::apply(&dir, &patch, true, false).await;
        let base_build = ctx
            .logged(
                "repro: base build",
                &build,
                &dir,
                ctx.build_timeout,
                &mut out.log,
            )
            .await;
        if !base_build.is_some_and(|o| o.ok()) {
            out.note = "the project does not build at the starting code with the reported command"
                .to_string();
            return out;
        }
        let _ = gitops::apply(&dir, &patch, false, false).await;
        let why = match ctx
            .logged(
                "repro: build with tests",
                &build,
                &dir,
                ctx.build_timeout,
                &mut out.log,
            )
            .await
        {
            None => {
                out.note = "out of time during qualification".to_string();
                return out;
            }
            Some(o) if !o.ok() => "they do not compile against it".to_string(),
            Some(_) => match ctx
                .logged("repro: run", &run, &dir, ctx.test_timeout, &mut out.log)
                .await
            {
                None => {
                    out.note = "out of time during qualification".to_string();
                    return out;
                }
                Some(o) if o.ok() => {
                    out.note = "the tests pass on the starting code (not discriminating); dropped"
                        .to_string();
                    return out;
                }
                Some(_) => "they fail when run".to_string(),
            },
        };
        // Freeze.
        let frozen_dir = self.scratch.join("frozen");
        let _ = std::fs::create_dir_all(&frozen_dir);
        let frozen_patch = frozen_dir.join("tests.patch");
        if std::fs::write(&frozen_patch, &patch).is_err() {
            out.note = "could not write the frozen patch".to_string();
            return out;
        }
        set_mode(&frozen_patch, 0o444);
        let script = frozen_dir.join("run-repro.sh");
        let script_text = prompts::run_repro_script(
            &self.p.repo,
            &self.snapshot.commit,
            &frozen_patch,
            &build,
            &run,
            &self.scratch.join("runs"),
            &self.env,
        );
        if std::fs::write(&script, script_text).is_ok() {
            set_mode(&script, 0o444);
        }
        let names: Vec<String> = report
            .tests
            .iter()
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty())
            .collect();
        let frozen = FrozenTests {
            patch,
            patch_path: frozen_patch,
            script,
            build,
            run,
            test_suite: Some(report.test.trim().to_string()).filter(|t| !t.is_empty()),
            names,
            fails_because: why,
        };
        if let Ok(mut slot) = self.frozen.lock() {
            *slot = Some(frozen.clone());
        }
        let msg = prompts::repro_delivery(
            &frozen.names,
            &frozen.fails_because,
            &frozen.patch_path,
            &frozen.script,
        );
        out.delivered_to_root = self.host.notify_root(msg).await;
        self.event(format!(
            "frozen tests delivered to root: {}",
            out.delivered_to_root
        ))
        .await;
        out.frozen = Some(frozen);
        out.note = "qualified and frozen".to_string();
        out
    }

    /// Stop everything, keep the root's work, clean up (turn aborted / thread stopped).
    pub async fn abort(self: &Arc<Self>) {
        // Tier B: also stops an attempts phase that is running right now.
        self.stop_attempt_children().await;
        if self.finalized.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancel.cancel();
        self.stop_all().await;
        self.event("aborted; the root's changes are kept".to_string())
            .await;
        self.cleanup_and_report().await;
    }

    pub(crate) async fn stop_all(&self) {
        let launcher = self.inner.lock().await.launcher.take();
        if let Some(l) = launcher {
            let _ = l.await;
        }
        self.join_repro().await;
    }

    /// Wait for the writer's task (it returns quickly once cancelled).
    pub(crate) async fn join_repro(&self) -> Option<ReproOutcome> {
        let task = self.inner.lock().await.repro_task.take();
        if let Some(task) = task {
            let outcome = task.await.unwrap_or_default();
            self.inner.lock().await.repro = Some(outcome);
        }
        self.inner.lock().await.repro.clone()
    }

    /// Called when the root's turn ends. Never makes the main agent wait for the writer.
    pub async fn finalize(self: &Arc<Self>) {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.attempts.enabled() {
            // Tier B: evidence, further attempts when it is weak, selection, apply.
            self.finalize_with_attempts().await;
            self.cleanup_and_report().await;
            return;
        }
        self.cancel.cancel();
        self.stop_all().await;
        self.remove_leaked_tests().await;
        self.event("root finished; repro writer stopped".to_string())
            .await;
        self.cleanup_and_report().await;
    }

    /// If the frozen tests ended up in the real tree, take them out again.
    pub(crate) async fn remove_leaked_tests(&self) -> bool {
        let frozen = self.frozen.lock().ok().and_then(|f| f.clone());
        let Some(frozen) = frozen else {
            return false;
        };
        if gitops::apply(&self.p.repo, &frozen.patch, true, true)
            .await
            .is_ok()
            && gitops::apply(&self.p.repo, &frozen.patch, true, false)
                .await
                .is_ok()
        {
            self.event("the frozen tests were found in the real tree and removed".to_string())
                .await;
            self.inner.lock().await.report.leaked_tests_removed = true;
            return true;
        }
        false
    }

    /// Remove every worktree and scratch file of this session and write the report.
    pub(crate) async fn cleanup_and_report(&self) {
        let repro = self.inner.lock().await.repro.clone();
        // Every worktree we created lives under the scratch dir (including run-repro.sh's).
        for wt in gitops::worktree_list(&self.p.repo).await {
            if wt.starts_with(&self.scratch) {
                gitops::worktree_remove(&self.p.repo, &wt).await;
            }
        }
        gitops::delete_ref(&self.p.repo, &self.snapshot.ref_name).await;
        let frozen_dir = self.scratch.join("frozen");
        if let Ok(entries) = std::fs::read_dir(&frozen_dir) {
            for e in entries.flatten() {
                set_mode(&e.path(), 0o644);
            }
        }
        let _ = tokio::fs::remove_dir_all(&self.scratch).await;
        let leftover: Vec<PathBuf> = gitops::worktree_list(&self.p.repo)
            .await
            .into_iter()
            .filter(|w| w.starts_with(&self.scratch))
            .collect();

        let mut inner = self.inner.lock().await;
        let events = std::mem::take(&mut inner.events);
        let r = &mut inner.report;
        r.root_thread = self.p.root_id.clone();
        r.repro_tests = self.p.run_repro;
        r.snapshot = self.snapshot.commit.clone();
        r.started_unix = self.started_unix;
        r.elapsed_ms = self.started.elapsed().as_millis() as u64;
        r.repro = repro;
        r.leftover_worktrees = leftover;
        r.events = events;
        r.attempts = self.attempts.take_report();
        let dir = self.p.codex_home.join("byteask-subagents");
        if std::fs::create_dir_all(&dir).is_ok()
            && let Ok(json) = serde_json::to_string_pretty(&*r)
        {
            let _ = std::fs::write(dir.join(format!("{}.json", self.p.root_id)), json);
        }
    }

    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_heuristic() {
        let issue = "Calling the parser on an empty input crashes with a segfault instead of \
                     returning an error. Expected behaviour: it should return an empty document \
                     and report no error, like the other entry points already do for this case.";
        assert!(looks_like_issue(issue));
        assert!(!looks_like_issue("fix the bug"));
        assert!(!looks_like_issue(
            "What does this function return when the list is empty? Please explain the \
             algorithm to me in detail and list every helper it calls and what each of them \
             computes along the way, step by step."
        ));
    }
}
