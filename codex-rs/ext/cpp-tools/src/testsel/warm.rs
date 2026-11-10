//! The background warm build: when a task arrives, build the test programs it will most
//! likely be checked with, in the tree the checks use, before the agent asks for a check.
//!
//! [`start_warm_build`] returns at once. In a spawned task it takes the tools' own tree for the
//! repository ([`super::tree::own_tree`], the tree `cpp_finish_check` and `cpp_test_select`
//! build in) under its per-tree lock, configures it if needed (once per repository and
//! commit), maps the given program names to CMake targets and builds those incrementally. The
//! lock is taken before the call returns whenever the tree is free, so a check that starts
//! right after waits for the warm build and then finds the programs built: its own build is
//! incremental. The build runs like every engine-started build: the session-wide build slot,
//! the memory-aware job count, low priority on the spare CPUs, killed on cancel.
//!
//! Failures (not a CMake project, configure or build errors) are logged and ignored; nothing
//! is reported to the model. CMake only: Meson and Make projects are left alone.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;

use super::Model;
use super::check;
use super::check::CheckPlan;
use super::cmake;
use super::exec;
use super::exec::Ctx;
use super::graph;
use super::graph::canonical;
use super::tree;
use crate::builds::ProjectGuard;

/// The warm builds started in this process with `BYTEASK_FINISH_CHECK_WARM_YIELD` on, by tree.
static STARTED: std::sync::Mutex<Vec<(PathBuf, WarmHandle)>> = std::sync::Mutex::new(Vec::new());

/// Stop the warm build of `tree` if one is still running; true when one was.
pub(crate) fn stop_for_check(tree: &Path) -> bool {
    let Ok(started) = STARTED.lock() else {
        return false;
    };
    let wanted = canonical(tree);
    let mut stopped = false;
    for (built, handle) in started.iter() {
        if canonical(built) == wanted && !handle.is_finished() {
            handle.cancel();
            stopped = true;
        }
    }
    stopped
}

/// Longest the warm build waits for its tree when a check holds it.
const LOCK_WAIT: Duration = exec::BUILD_TIMEOUT;

/// How a warm build ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WarmOutcome {
    /// The tree it configured and built in (`None` when it did not get that far).
    pub tree: Option<PathBuf>,
    /// The CMake targets it built (or tried to build).
    pub targets: Vec<String>,
    /// Names it could not map to a target of this project (ignored).
    pub ignored: Vec<String>,
    /// Configured and every target built (or none was asked for).
    pub ok: bool,
    /// One line for logs.
    pub summary: String,
}

/// A running (or finished) warm build. Dropping the handle does NOT stop the build; call
/// [`WarmHandle::cancel`] for that.
#[derive(Clone)]
pub struct WarmHandle {
    done: watch::Receiver<Option<WarmOutcome>>,
    /// `Some(plan)` once the check for the task's place is known (`Some(None)`: none).
    plan: watch::Receiver<Option<Option<CheckPlan>>>,
    cancel: Arc<watch::Sender<bool>>,
}

impl WarmHandle {
    fn finished(outcome: WarmOutcome) -> Self {
        tracing::info!(target: "byteask_cpp", "warm build: {}", outcome.summary);
        let (_tx, done) = watch::channel(Some(outcome));
        let (_plan_tx, plan) = watch::channel(Some(None));
        let (cancel, _) = watch::channel(false);
        Self { done, plan, cancel: Arc::new(cancel) }
    }

    /// The build has ended (finished, failed, was cancelled or never started).
    pub fn is_finished(&self) -> bool {
        self.done.borrow().is_some()
    }

    /// The outcome, once finished.
    pub fn outcome(&self) -> Option<WarmOutcome> {
        self.done.borrow().clone()
    }

    /// Wait until it ends.
    pub async fn wait(&self) -> WarmOutcome {
        let mut done = self.done.clone();
        match done.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or_default(),
            Err(_) => WarmOutcome {
                summary: "the warm build task ended without an outcome".to_string(),
                ..WarmOutcome::default()
            },
        }
    }

    /// The check for the task's place (see [`start_warm_build_for`]), as soon as the tree
    /// is configured and before the build starts; `None` when there is none (no place given,
    /// test programs named instead, not CMake, or the warm build ended first).
    pub async fn check_plan(&self) -> Option<CheckPlan> {
        let mut plan = self.plan.clone();
        match plan.wait_for(Option::is_some).await {
            Ok(value) => value.clone().flatten(),
            Err(_) => None,
        }
    }

    /// Stop it: its build processes are killed and the tree is released.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

/// Start building `test_programs` in the background, in the C/C++ tools' own build tree for
/// the repository containing `workspace`; returns at once and never fails.
///
/// `test_programs` are names as a Context pack lists them: CMake target or executable names
/// (`foo_test`), a test program's path or source file, or a CTest test name. Each is mapped to
/// a CMake target of the project; names that map to nothing are ignored (and logged). With no
/// names (or none that map) it only configures the tree, which is most of the first check's
/// fixed cost. Later `cpp_finish_check` / `cpp_test_select` calls wait for it through the
/// per-tree lock and build incrementally on top of it.
///
/// Runs the project's own build scripts: call it only when the session allows native execution
/// for the project ([`crate::native_exec_allowed`]). Needs a Tokio runtime; without one it
/// returns a finished handle and does nothing.
pub fn start_warm_build(workspace: &Path, test_programs: &[String]) -> WarmHandle {
    start_warm_build_for(workspace, test_programs, &[])
}

/// [`start_warm_build`], and more: with no `test_programs` but the task's likely place
/// (`place_files`, absolute paths; the first one counts), it picks the tests that cover the
/// place once the tree is configured ([`check::plan`]), offers the command that builds and runs
/// them ([`WarmHandle::check_plan`]) and builds their programs.
pub fn start_warm_build_for(
    workspace: &Path,
    test_programs: &[String],
    place_files: &[PathBuf],
) -> WarmHandle {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return WarmHandle::finished(WarmOutcome {
            summary: "not started: no async runtime".to_string(),
            ..WarmOutcome::default()
        });
    };
    let Some(root) = crate::gitx::repo_roots_sync(workspace).into_iter().next() else {
        return WarmHandle::finished(WarmOutcome {
            summary: format!("not started: {} is not in a git repository", workspace.display()),
            ..WarmOutcome::default()
        });
    };
    let root = canonical(&root);
    if !root.join("CMakeLists.txt").is_file() || !crate::tools::binary_on_path("cmake") {
        return WarmHandle::finished(WarmOutcome {
            summary: format!("not started: {} is not a CMake project (or cmake is missing)", root.display()),
            ..WarmOutcome::default()
        });
    }
    let tree = tree::own_tree(&root);
    let registry_tree = tree.clone();
    // Taken now, before returning, so a check that starts right after this call waits for it.
    let guard = tree::try_lock_tree(&tree);
    let (done_tx, done) = watch::channel(None);
    let (plan_tx, plan) = watch::channel(None);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let cancel_tx = Arc::new(cancel_tx);
    let stop_tx = cancel_tx.clone();
    let programs = test_programs.to_vec();
    let place = if programs.is_empty() {
        place_files.first().cloned()
    } else {
        None
    };
    runtime.spawn(async move {
        let outcome = crate::priority::scope(
            crate::priority::automatic(),
            warm(
                root, tree, programs, place, plan_tx, guard, cancel_rx, stop_tx,
            ),
        )
        .await;
        if outcome.ok {
            tracing::info!(target: "byteask_cpp", "warm build: {}", outcome.summary);
        } else {
            tracing::warn!(target: "byteask_cpp", "warm build: {}", outcome.summary);
        }
        let _ = done_tx.send(Some(outcome));
    });
    let handle = WarmHandle {
        done,
        plan,
        cancel: cancel_tx,
    };
    // A check may stop it (`BYTEASK_FINISH_CHECK_WARM_YIELD`).
    if super::verify::warm_yield()
        && let Ok(mut started) = STARTED.lock()
    {
        started.push((registry_tree, handle.clone()));
    }
    handle
}

async fn warm(
    root: PathBuf,
    tree: PathBuf,
    mut programs: Vec<String>,
    place: Option<PathBuf>,
    plan_tx: watch::Sender<Option<Option<CheckPlan>>>,
    guard: Option<ProjectGuard>,
    cancel: watch::Receiver<bool>,
    stop_tx: Arc<watch::Sender<bool>>,
) -> WarmOutcome {
    let mut outcome = WarmOutcome::default();
    let _guard = match guard {
        Some(guard) => guard,
        None => {
            let mut stop = cancel.clone();
            tokio::select! {
                held = tree::lock_tree(&tree, LOCK_WAIT) => match held {
                    Ok(guard) => guard,
                    Err(busy) => {
                        outcome.summary = format!("not run: {}", busy.summary());
                        return outcome;
                    }
                },
                _ = stop.wait_for(|c| *c) => {
                    outcome.summary = "cancelled while waiting for the tree".to_string();
                    return outcome;
                }
            }
        }
    };
    let scratch = tree::scratch_dir(&root);
    let version = crate::toolver::tool_version("cmake", &root).await;
    let commit = crate::gitx::head(&root).await;
    let request = cmake::Request {
        root: &root,
        build_dir: None,
        scratch: &scratch,
        version,
        cmake_files: &[],
        commit: commit.as_deref(),
    };
    let mut stop = cancel.clone();
    let model = tokio::select! {
        model = cmake::model(&request) => model,
        _ = stop.wait_for(|c| *c) => {
            outcome.summary = "cancelled while configuring".to_string();
            return outcome;
        }
    };
    let model = match model {
        Ok(model) => model,
        Err(err) => {
            outcome.summary = format!("configure failed: {}", err.lines().next().unwrap_or(""));
            return outcome;
        }
    };
    outcome.tree = Some(model.build_dir.clone());
    let plan = match place {
        Some(place) => {
            let affected = graph::affected(&model, std::slice::from_ref(&place)).await;
            check::plan(&model, &affected, &place, exec::default_jobs())
        }
        None => None,
    };
    if let Some(plan) = &plan {
        tracing::info!(target: "byteask_cpp", "check line: {}", plan.line);
        programs = plan.targets.clone();
    }
    let _ = plan_tx.send(Some(plan));
    let (targets, ignored) = map_programs(&model, &programs);
    outcome.targets = targets;
    outcome.ignored = ignored;
    if outcome.targets.is_empty() {
        outcome.ok = true;
        outcome.summary = format!(
            "configured {}; nothing to build{}",
            model.build_dir.display(),
            ignored_note(&outcome.ignored)
        );
        return outcome;
    }
    let ctx = Ctx {
        log: scratch.join("logs").join(format!("warm-{}.log", millis())),
        jobs: exec::default_jobs(),
        test_timeout: Duration::from_secs(60),
        cancel,
        progress: Arc::new(Mutex::new(String::new())),
    };
    // `tree_owner`: the tree is the agent's as soon as it builds there itself (e.g. with the
    // check line): this build stops instead of running beside it.
    let mut yielded: Option<(u32, String)> = None;
    let built = if tree::owner_mode(&model.build_dir) {
        let build = exec::build(&ctx, &model, &model.build_dir, &outcome.targets);
        tokio::pin!(build);
        loop {
            tokio::select! {
                built = &mut build => break built,
                found = tree::foreign_build(&model.build_dir), if yielded.is_none() => {
                    yielded = Some(found);
                    let _ = stop_tx.send(true);
                }
            }
        }
    } else {
        exec::build(&ctx, &model, &model.build_dir, &outcome.targets).await
    };
    outcome.ok = built.ok;
    if let Some((pid, cmd)) = yielded {
        outcome.summary = format!(
            "stopped building {} in {} after {:.0}s: another build started there (pid {pid}: {})",
            outcome.targets.join(" "),
            model.build_dir.display(),
            built.secs,
            tree::short_command(&cmd)
        );
        return outcome;
    }
    outcome.summary = format!(
        "{} {} in {} ({:.0}s, log {}){}",
        if built.cancelled {
            "cancelled building"
        } else if built.ok {
            "built"
        } else {
            "FAILED to build"
        },
        outcome.targets.join(" "),
        model.build_dir.display(),
        built.secs,
        ctx.log.display(),
        ignored_note(&outcome.ignored)
    );
    outcome
}

fn ignored_note(ignored: &[String]) -> String {
    if ignored.is_empty() {
        String::new()
    } else {
        format!("; ignored (no such target): {}", tree::list(ignored, 6))
    }
}

fn millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `programs` as target names of `model`: a target name, a program's file name or path, a
/// source file one target compiles, or a CTest test name. Returns (targets, ignored names).
pub(crate) fn map_programs(model: &Model, programs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut targets: Vec<String> = Vec::new();
    let mut ignored = Vec::new();
    for raw in programs {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let file = Path::new(name)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let stem = file.strip_suffix(".exe").unwrap_or(&file).to_string();
        let source = canonical(&super::normalize(&model.root, Path::new(name)));
        let hit = model
            .target(name)
            .or_else(|| model.targets.iter().find(|t| t.is_exe && t.name == stem))
            .or_else(|| {
                model.targets.iter().find(|t| {
                    t.is_exe
                        && t.artifacts.iter().any(|a| {
                            a.file_name().is_some_and(|f| f.to_string_lossy() == file)
                        })
                })
            })
            .or_else(|| {
                super::is_source(&source)
                    .then(|| model.targets.iter().find(|t| t.sources.contains(&source)))
                    .flatten()
            })
            .map(|t| t.name.clone())
            .or_else(|| {
                model
                    .tests
                    .iter()
                    .find(|t| t.name == name)
                    .and_then(|t| t.target.clone())
            });
        match hit {
            Some(target) if !targets.contains(&target) => targets.push(target),
            Some(_) => {}
            None => ignored.push(name.to_string()),
        }
    }
    (targets, ignored)
}
