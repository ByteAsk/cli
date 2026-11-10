//! The C/C++ finish gate and the forced change-impact checklist triggers.
//!
//! When the model ends its turn in a C/C++ workspace, the engine runs, within a time budget:
//! - hygiene: a non-empty change (non-interactive runs), no edits to existing test files,
//!   generated copies in sync, no leftover files created during the session;
//! - build every target including tests (a reused build tree, incrementally, or a scratch one);
//! - run the tests and compare failures with the unmodified tree (a cached base run on a copy
//!   of the session-start state in a scratch directory, built lazily, only when something
//!   fails); failures are re-run once to drop flaky tests; a test that does not build fails;
//! - the change-impact checklist again: items still without an answer, and new items;
//! - in a crash-fix session (a sanitizer report in the conversation), the advisory patch-shape
//!   critic ([`crate::critique_patch`]'s rules), each finding shown once.
//!
//! Failures go back to the model as one message and the turn continues (at most
//! `max_loops` times). Nothing here edits the repository or deletes a file in it (0.2.0):
//! untracked files its own in-place build created are named in a note and left in place.
//!
//! Since 0.2.0 the automatic run is opt-in (`byteask_cpp.finish_gate`, `BYTEASK_CPP_GATE`). The
//! default is `cpp_finish_check`, which the model calls: the build and tests only
//! ([`GateInput::advisory`]), within 90 s, never sending the model back.
//!
//! One full build + suite run per change set: the result is kept with the change set's
//! fingerprint ([`BuildMemo`]), so stopping again after answering feedback without changing
//! the code reuses it (and does not repeat build/test findings already shown). The guidance
//! therefore tells the model not to run the full suite itself (see [`crate::guidance`]).
//!
//! Every run ends in one shared result ([`GateOutcome::result`]): PASS only when
//! the checks that apply really ran and passed; FAIL when a problem is open; NOT RUN (with its
//! reason) when the build or the tests could not run, so unchecked work is never "passed".
//! Results are bound to [`AgentChanges::content_key`]: a hash of the exact contents the
//! gate looked at, so any later edit, including a same-size edit to a new file, makes an
//! earlier result stale.
//!
//! What the gate cannot check never blocks finishing: a failing test is reported only when it
//! passes on the unmodified tree (after one serial re-run), never "could not compare"; a
//! suite that could not be built or run, failures that could not be compared and tests that
//! hit the per-test time limit are collected into one "could not check" note
//! ([`ProblemKind::SuiteNotChecked`]) that rides along, once per session, with feedback that is
//! sent anyway — it never causes a feedback round by itself, and it names a bounded way to
//! check instead of the whole suite.

pub(crate) mod build;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use build::BuildKind;
use build::BuildPlan;
use build::TestStatus;

use crate::gitx;
use crate::impact;

/// Gate settings, resolved once per thread from config and environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GateSettings {
    /// Run the finish gate (`byteask_cpp.finish_gate`, `BYTEASK_CPP_GATE`).
    pub gate: bool,
    /// Inject the change-impact checklist (`byteask_cpp.impact`, `BYTEASK_CPP_IMPACT`).
    pub impact: bool,
    /// Build and run tests inside the gate (`byteask_cpp.gate_tests`, `BYTEASK_CPP_GATE_TESTS`).
    pub tests: bool,
    /// Advisory patch-shape critic in crash-fix sessions (`byteask_cpp.gate_critic`,
    /// `BYTEASK_CPP_GATE_CRITIC`).
    pub critic: bool,
    /// Time budget of one gate run (`byteask_cpp.gate_budget_secs`, `BYTEASK_CPP_GATE_BUDGET_SECS`).
    pub budget: Duration,
    /// Interactive sessions only: what the gate spends before the turn ends; a check that
    /// does not fit continues in the background with the whole `budget`, and its result is
    /// shown later (`byteask_cpp.gate_fast_secs`, `BYTEASK_CPP_GATE_FAST_SECS`; 0 = the whole
    /// budget in the turn). Non-interactive runs (`exec`) always spend the whole budget in the
    /// turn: there is no next turn to show a background result in.
    pub fast_tier: Duration,
    /// Feedback rounds per turn before the model may finish regardless
    /// (`byteask_cpp.gate_max_loops`, `BYTEASK_CPP_GATE_MAX_LOOPS`).
    pub max_loops: u32,
    /// No builds or test runs once the session is older than this; the gate's budget is also
    /// cut to what remains (`byteask_cpp.gate_session_cap_secs`, `BYTEASK_CPP_GATE_SESSION_CAP_SECS`).
    /// Protects callers with a wall-clock limit on the whole run.
    pub session_cap: Duration,
    /// On a CMake project, build and test like `cpp_finish_check` (the test programs the change
    /// reaches, nearest first, in the tools' own tree with the project's test options on) instead
    /// of every target and the whole suite (`byteask_cpp.gate_targeted`,
    /// `BYTEASK_CPP_GATE_TARGETED`; off by default).
    pub targeted: bool,
}

impl Default for GateSettings {
    /// 0.2.0: the automatic finish gate and the automatic change-impact checklist are off unless
    /// opted in (`byteask_cpp.finish_gate` / `BYTEASK_CPP_GATE`, `byteask_cpp.impact` /
    /// `BYTEASK_CPP_IMPACT`); the model calls `cpp_finish_check` when it wants the check.
    fn default() -> Self {
        Self {
            gate: false,
            impact: false,
            tests: true,
            critic: true,
            budget: Duration::from_secs(900),
            fast_tier: Duration::from_secs(90),
            max_loops: 2,
            session_cap: Duration::from_secs(3_600),
            targeted: false,
        }
    }
}

/// Below this much session time the gate does not start a build and test run (it reports
/// NOT RUN (time limit) instead).
pub(crate) const MIN_SUITE_TIME: Duration = Duration::from_secs(120);

/// The gate's budget in non-interactive sessions (`exec`) when `gate_budget_secs` is not set.
/// There it runs after the agent's final message and the caller waits for it. In
/// measured sessions every PASS took at most 207 s; the default 900 s budget only
/// made builds that could not finish anyway end in NOT RUN (time limit) after about 540 s.
pub(crate) const EXEC_BUDGET: Duration = Duration::from_secs(300);

/// With `[byteask_harness] time_budget_secs` set (the caller's wall-clock limit for the whole
/// run) the gate's session cap ends this long before that limit.
pub(crate) const TIME_BUDGET_MARGIN: Duration = Duration::from_secs(60);

pub(crate) const ENV_GATE: &str = "BYTEASK_CPP_GATE";
pub(crate) const ENV_IMPACT: &str = "BYTEASK_CPP_IMPACT";
pub(crate) const ENV_TESTS: &str = "BYTEASK_CPP_GATE_TESTS";
pub(crate) const ENV_CRITIC: &str = "BYTEASK_CPP_GATE_CRITIC";
pub(crate) const ENV_BUDGET: &str = "BYTEASK_CPP_GATE_BUDGET_SECS";
pub(crate) const ENV_LOOPS: &str = "BYTEASK_CPP_GATE_MAX_LOOPS";
pub(crate) const ENV_SESSION_CAP: &str = "BYTEASK_CPP_GATE_SESSION_CAP_SECS";
pub(crate) const ENV_TARGETED: &str = "BYTEASK_CPP_GATE_TARGETED";
pub(crate) const ENV_FAST: &str = "BYTEASK_CPP_GATE_FAST_SECS";
/// Config table: `[byteask_cpp]` in config.toml, or `-c byteask_cpp.finish_gate=false`.
pub(crate) const CONFIG_TABLE: &str = "byteask_cpp";

impl GateSettings {
    /// Resolve from a config lookup (`key` → bool/int) and the environment; env wins.
    pub fn resolve(
        config_bool: impl Fn(&str) -> Option<bool>,
        config_int: impl Fn(&str) -> Option<i64>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let mut settings = Self::default();
        let env_bool = |name: &str| {
            env(name).and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            })
        };
        let env_int = |name: &str| env(name).and_then(|value| value.trim().parse::<i64>().ok());
        if let Some(value) = env_bool(ENV_GATE).or_else(|| config_bool("finish_gate")) {
            settings.gate = value;
        }
        if let Some(value) = env_bool(ENV_IMPACT).or_else(|| config_bool("impact")) {
            settings.impact = value;
        }
        if let Some(value) = env_bool(ENV_TESTS).or_else(|| config_bool("gate_tests")) {
            settings.tests = value;
        }
        if let Some(value) = env_bool(ENV_CRITIC).or_else(|| config_bool("gate_critic")) {
            settings.critic = value;
        }
        if let Some(value) = env_int(ENV_BUDGET).or_else(|| config_int("gate_budget_secs")) {
            settings.budget = Duration::from_secs(value.clamp(30, 7_200) as u64);
        }
        if let Some(value) = env_int(ENV_FAST).or_else(|| config_int("gate_fast_secs")) {
            settings.fast_tier = Duration::from_secs(value.clamp(0, 7_200) as u64);
        }
        if let Some(value) = env_int(ENV_LOOPS).or_else(|| config_int("gate_max_loops")) {
            settings.max_loops = value.clamp(0, 5) as u32;
        }
        if let Some(value) =
            env_int(ENV_SESSION_CAP).or_else(|| config_int("gate_session_cap_secs"))
        {
            settings.session_cap = Duration::from_secs(value.clamp(60, 86_400) as u64);
        }
        if let Some(value) = env_bool(ENV_TARGETED).or_else(|| config_bool("gate_targeted")) {
            settings.targeted = value;
        }
        settings
    }
}

// ---------------------------------------------------------------------------------------------
// Session-start snapshot and the agent's change set
// ---------------------------------------------------------------------------------------------

/// The repository state when the session started.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    pub root: PathBuf,
    pub head: String,
    /// Tracked paths already modified at session start → their content hash then ("" when the
    /// file was deleted).
    pub dirty: BTreeMap<String, String>,
    /// Untracked, not ignored paths at session start (directories collapsed, `dir/`).
    pub untracked: BTreeSet<String>,
    /// Copies of the files that were dirty at start (for the base snapshot).
    pub start_copy: Option<PathBuf>,
    /// Scratch directory of this repository's gate (outside the repo).
    pub scratch: PathBuf,
    /// Build trees of the project that existed at session start (the project's own trees are
    /// preferred over trees the agent made for a one-off sanitizer or experiment).
    pub build_trees: Vec<PathBuf>,
}

#[cfg(test)]
impl GateSettings {
    /// The automatic gate and checklist switched on (they are off by default since 0.2.0).
    pub(crate) fn enabled() -> Self {
        Self {
            gate: true,
            impact: true,
            ..Self::default()
        }
    }
}

/// Where the gate keeps logs and scratch builds for `root`: `gate-<hash>` in this user's private
/// scratch directory ([`scratch_root`]), never a predictable name in the shared `/tmp`.
pub(crate) fn scratch_for(root: &Path) -> PathBuf {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    scratch_root().join(format!(
        "gate-{:016x}",
        gitx::fnv1a(key.to_string_lossy().as_bytes())
    ))
}

/// This user's directory for the gate's and `cpp_test_select`'s scratch trees: `scratch/` in
/// ByteAsk's private per-user state directory (mode 0700, owner checked; see
/// [`crate::builds::state_dir`]), which the Build sandbox may write. If that directory cannot be
/// made private, a fresh private directory with an unpredictable name in the temp directory,
/// for this process only.
pub(crate) fn scratch_root() -> PathBuf {
    static FALLBACK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(dir) = crate::builds::state_dir().map(|dir| dir.join("scratch"))
        && std::fs::create_dir_all(&dir).is_ok()
    {
        return dir;
    }
    FALLBACK
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "byteask-scratch-{:016x}",
                gitx::fnv1a(format!("{}-{nanos}", std::process::id()).as_bytes())
            ));
            create_private_dir(&dir);
            dir
        })
        .clone()
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    // Fails if the name is taken: then nothing is written there (every write is best effort).
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

/// Scratch trees unused for longer than this are removed when a session starts.
const SCRATCH_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3_600);
/// At most this many scratch trees are kept (the most recently used)...
const SCRATCH_MAX_ENTRIES: usize = 12;
/// ...but a tree used this recently is never removed: another live session may be using it.
const SCRATCH_IN_USE: Duration = Duration::from_secs(24 * 3_600);
/// Touched whenever a scratch tree is used; its age decides eviction.
const SCRATCH_STAMP: &str = ".last-used";

/// Mark `scratch` as used now.
pub(crate) fn touch_scratch(scratch: &Path) {
    let _ = std::fs::create_dir_all(scratch);
    let _ = std::fs::write(scratch.join(SCRATCH_STAMP), b"");
}

/// Bound the scratch directory: remove the gate's and `cpp_test_select`'s trees that were not
/// used for [`SCRATCH_MAX_AGE`], and the least recently used ones beyond
/// [`SCRATCH_MAX_ENTRIES`] that were not used within [`SCRATCH_IN_USE`]. `keep` is never
/// removed. Only names this crate creates are touched.
pub(crate) fn evict_scratch(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut trees: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name.starts_with("gate-") || name.starts_with("testsel-"))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| {
            let path = entry.path();
            let used = std::fs::metadata(path.join(SCRATCH_STAMP))
                .or_else(|_| std::fs::metadata(&path))
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (used, path)
        })
        .filter(|(_, path)| path != keep)
        .collect();
    trees.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    for (index, (used, path)) in trees.into_iter().enumerate() {
        let age = now.duration_since(used).unwrap_or_default();
        if age > SCRATCH_MAX_AGE || (index + 1 >= SCRATCH_MAX_ENTRIES && age > SCRATCH_IN_USE) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

pub(crate) async fn take_snapshot(cwd: &Path) -> Option<Snapshot> {
    let root = gitx::toplevel(cwd).await?;
    let head = gitx::head(&root).await?;
    let scratch = scratch_for(&root);
    evict_scratch(&scratch_root(), &scratch);
    touch_scratch(&scratch);
    let at_start = gitx::changed_vs(&root, "HEAD").await;
    let paths: Vec<String> = at_start.iter().map(|(_, p)| p.clone()).collect();
    let hashes = gitx::hash_objects(&root, &paths).await;
    let dirty: BTreeMap<String, String> = paths.into_iter().zip(hashes).collect();
    let untracked: BTreeSet<String> = gitx::untracked(&root).await.into_iter().collect();
    let mut start_copy = None;
    if !dirty.is_empty() && dirty.len() <= 500 {
        let dir = scratch.join("start");
        let _ = std::fs::remove_dir_all(&dir);
        let mut ok = true;
        for (path, hash) in &dirty {
            if hash.is_empty() {
                continue;
            }
            let target = dir.join(path);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            ok &= std::fs::copy(root.join(path), &target).is_ok();
        }
        if ok {
            start_copy = Some(dir);
        }
    }
    let build_trees = build::existing_trees(&root, &scratch);
    Some(Snapshot {
        root,
        head,
        dirty,
        untracked,
        start_copy,
        scratch,
        build_trees,
    })
}

/// The work tree as it is now, copied to a stable git repository beside the gate's
/// scratch directory, so a check that runs while the agent keeps working (the background full
/// check) builds and tests exactly the code it started with. The copy is a `git clone --shared`
/// of the repository (objects shared, nothing downloaded) at the session-start commit, with the
/// current content of every tracked and untracked (not ignored) file; files that did not change
/// keep their modification time, so the gate's build tree for the copy stays incremental.
/// Returns the snapshot of the copy: same commit and session-start state, its own root and
/// scratch. `None` when the copy could not be made (the caller then does not run).
pub(crate) async fn mirror_snapshot(snapshot: &Snapshot) -> Option<Snapshot> {
    let root = &snapshot.root;
    let mirror = snapshot.scratch.join("snapshot-src");
    if !mirror.join(".git").is_dir() {
        let _ = std::fs::remove_dir_all(&mirror);
        std::fs::create_dir_all(snapshot.scratch.as_path()).ok()?;
        let source = root.display().to_string();
        let target = mirror.display().to_string();
        gitx::git_timeout(
            &snapshot.scratch,
            &[
                "clone",
                "--quiet",
                "--shared",
                "--no-checkout",
                &source,
                &target,
            ],
            Duration::from_secs(120),
        )
        .await?;
    }
    gitx::git_timeout(
        &mirror,
        &["checkout", "--quiet", "--force", "--detach", &snapshot.head],
        Duration::from_secs(300),
    )
    .await?;
    let listed = |dir: PathBuf| async move {
        gitx::git_timeout(
            &dir,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
            Duration::from_secs(120),
        )
        .await
        .map(|out| gitx::split_z(&out))
    };
    let wanted: BTreeSet<String> = listed(root.clone()).await?.into_iter().collect();
    let present: BTreeSet<String> = listed(mirror.clone()).await?.into_iter().collect();
    for path in present.difference(&wanted) {
        let _ = std::fs::remove_file(mirror.join(path));
    }
    for path in &wanted {
        let from = root.join(path);
        let to = mirror.join(path);
        match std::fs::symlink_metadata(&from) {
            Ok(meta) if meta.is_file() => {
                if !same_file_content(&from, &to, &meta) {
                    if let Some(dir) = to.parent() {
                        std::fs::create_dir_all(dir).ok()?;
                    }
                    let _ = std::fs::remove_file(&to);
                    std::fs::copy(&from, &to).ok()?;
                    if let Ok(modified) = meta.modified()
                        && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&to)
                    {
                        let _ = file.set_modified(modified);
                    }
                }
            }
            // Deleted in the work tree (still in the index): deleted in the copy too.
            Err(_) => {
                let _ = std::fs::remove_file(&to);
            }
            // Symlinks and other entries: as git checked them out.
            Ok(_) => {}
        }
    }
    Some(Snapshot {
        scratch: scratch_for(&mirror),
        root: mirror,
        build_trees: Vec::new(),
        ..snapshot.clone()
    })
}

impl GateOutcome {
    /// Rewrite paths of the snapshot copy (`from`) to the work tree's (`to`) in every message,
    /// so what the agent reads names its own files.
    pub(crate) fn map_paths(&mut self, from: &Path, to: &Path) {
        let (from, to) = (from.display().to_string(), to.display().to_string());
        let fix = |text: &mut String| *text = text.replace(&from, &to);
        self.problems.iter_mut().for_each(|p| fix(&mut p.text));
        self.still_open.iter_mut().for_each(|p| fix(&mut p.text));
        self.notes.iter_mut().for_each(fix);
        self.not_checked.iter_mut().for_each(fix);
    }
}

/// Same size, same modification time and same bytes.
fn same_file_content(from: &Path, to: &Path, from_meta: &std::fs::Metadata) -> bool {
    let Ok(to_meta) = std::fs::symlink_metadata(to) else {
        return false;
    };
    if !to_meta.is_file() || to_meta.len() != from_meta.len() {
        return false;
    }
    if to_meta.modified().ok() == from_meta.modified().ok() {
        return true;
    }
    matches!((std::fs::read(from), std::fs::read(to)), (Ok(a), Ok(b)) if a == b)
}

/// What the agent changed since the session started.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentChanges {
    /// Tracked paths (status vs the start HEAD) whose content differs from session start.
    pub tracked: Vec<(char, String)>,
    /// Untracked paths that did not exist at session start (directories collapsed).
    pub new_untracked: Vec<String>,
    /// Hash of the change set's content (to skip re-checking an unchanged tree); derived from
    /// [`Self::content_key`].
    pub fingerprint: u64,
    /// SHA-256 of everything the gate's verdict depends on: the start commit, the content
    /// of every changed tracked file and of every new file, and the build environment. A result
    /// is reused only for the same key.
    pub content_key: String,
}

impl AgentChanges {
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty() && self.new_untracked.is_empty()
    }

    /// A tracked C/C++ file of the project changed (a new scratch program does not count).
    pub fn touches_tracked_cpp(&self) -> bool {
        self.tracked.iter().any(|(_, path)| gitx::is_cpp_path(path))
    }
}

pub(crate) async fn agent_changes(snapshot: &Snapshot) -> AgentChanges {
    let vs_head = gitx::changed_vs(&snapshot.root, &snapshot.head).await;
    // Files dirty at start that are now back at HEAD are changes too.
    let mut candidates: Vec<(char, String)> = vs_head.clone();
    for path in snapshot.dirty.keys() {
        if !vs_head.iter().any(|(_, p)| p == path) {
            candidates.push(('M', path.clone()));
        }
    }
    let paths: Vec<String> = candidates.iter().map(|(_, p)| p.clone()).collect();
    let hashes = gitx::hash_objects(&snapshot.root, &paths).await;
    let mut tracked = Vec::new();
    let mut fingerprint_input = String::new();
    for ((status, path), now) in candidates.into_iter().zip(hashes) {
        if snapshot.dirty.get(&path).is_some_and(|start| *start == now) {
            continue;
        }
        fingerprint_input.push_str(&format!("{status}{path}:{now};"));
        tracked.push((status, path));
    }
    let new_untracked: Vec<String> = gitx::untracked(&snapshot.root)
        .await
        .into_iter()
        .filter(|path| !snapshot.untracked.contains(path))
        .collect();
    // New files by CONTENT. Path + size let a same-size edit to a new file reuse an
    // earlier result for different code. Build outputs (objects, executables, configured build
    // trees) are not the change: an in-place build that creates one leaves the key as it was,
    // now that nothing deletes them again (0.2.0).
    for path in &new_untracked {
        if is_build_output(&snapshot.root.join(path.trim_end_matches('/'))) {
            continue;
        }
        content_of_new_path(&snapshot.root, path, &mut fingerprint_input);
    }
    let content_key = content_key(&snapshot.head, &fingerprint_input, &build_env());
    AgentChanges {
        tracked,
        new_untracked,
        fingerprint: gitx::fnv1a(content_key.as_bytes()),
        content_key,
    }
}

/// Environment that changes what a build produces; part of [`AgentChanges::content_key`].
const BUILD_ENV: &[&str] = &[
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CMAKE_GENERATOR",
    "CMAKE_BUILD_TYPE",
];

fn build_env() -> String {
    BUILD_ENV
        .iter()
        .map(|key| format!("{key}={};", std::env::var(key).unwrap_or_default()))
        .collect()
}

pub(crate) fn content_key(head: &str, changes: &str, env: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for part in ["head:", head, "\nchanges:", changes, "\nenv:", env] {
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Files above this are keyed by size and modification time instead of content.
const CONTENT_HASH_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Files of one new directory hashed at most (a new directory of thousands of files is an
/// output tree, not a source change).
const CONTENT_HASH_MAX_FILES: usize = 2_000;

/// Append `path` (a new file, or a new directory `dir/`) to the content-key input: each file's
/// SHA-256. A configured build tree is outputs, not inputs: only its name counts.
fn content_of_new_path(root: &Path, path: &str, out: &mut String) {
    let full = root.join(path.trim_end_matches('/'));
    if full.is_dir() {
        if full.join("CMakeCache.txt").is_file()
            || full.join("build.ninja").is_file()
            || full.join("meson-private").is_dir()
            || full.join("config.status").is_file()
        {
            out.push_str(&format!("{path}:build-tree;"));
            return;
        }
        let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&full)
            .follow_links(false)
            .into_iter()
            .flatten()
            .filter(|entry| entry.file_type().is_file())
            .take(CONTENT_HASH_MAX_FILES)
            .map(walkdir::DirEntry::into_path)
            .collect();
        files.sort();
        for file in files {
            let name = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            out.push_str(&format!("{name}:{};", file_digest(&file)));
        }
        return;
    }
    out.push_str(&format!("{path}:{};", file_digest(&full)));
}

fn file_digest(path: &Path) -> String {
    use sha2::Digest as _;
    let Ok(meta) = std::fs::metadata(path) else {
        return "missing".to_string();
    };
    if meta.len() > CONTENT_HASH_MAX_BYTES {
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos());
        return format!("size={},mtime={modified}", meta.len());
    }
    match std::fs::read(path) {
        Ok(bytes) => sha2::Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        Err(_) => "unreadable".to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Base results (computed lazily, once, on a copy of the session-start state)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct BaseCache {
    pub tree_ready: bool,
    pub tree_failed: bool,
    pub built: Option<build::BuildResult>,
    pub configured: bool,
    /// Accumulated per-test results on the base tree.
    pub tests: BTreeMap<String, TestStatus>,
    /// Tests already asked for (so a test absent from base output is "did not exist").
    pub asked: BTreeSet<String>,
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProblemKind {
    EmptyChange,
    Build,
    Tests,
    TestEdits,
    Copies,
    Leftovers,
    Impact,
    /// Advisory, once per session, never a reason for a feedback round by itself: what the gate
    /// could not check (the suite not built/run, failures not compared, time-limit hits).
    SuiteNotChecked,
    /// Advisory, each finding once: patch-shape critic on a crash fix.
    ShapeAdvisory,
}

impl ProblemKind {
    /// Whether this problem alone justifies sending feedback (and continuing the turn).
    pub(crate) fn blocking(&self) -> bool {
        !matches!(self, ProblemKind::SuiteNotChecked)
    }

    /// Whether this problem makes the change FAIL: every blocking problem except the
    /// patch-shape critic, which is advice the model is asked to consider once.
    pub(crate) fn fails_check(&self) -> bool {
        self.blocking() && !matches!(self, ProblemKind::ShapeAdvisory)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Problem {
    pub kind: ProblemKind,
    pub text: String,
}

/// Whether the gate's build and tests applied to this change and ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum SuiteState {
    /// Nothing changed: there was nothing to build or test.
    #[default]
    NotNeeded,
    /// Built and tested (anything that could not be checked is in `not_checked`).
    Ran,
    /// They apply but did not run, and why.
    NotRun(check_result::NotRunReason, String),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GateOutcome {
    /// Did the build and tests run (a change they did not check is never PASS).
    pub suite: SuiteState,
    pub problems: Vec<Problem>,
    /// One line per check that ran and passed, or did not run and why.
    pub notes: Vec<String>,
    /// Build/test problems of an unchanged change set that were already shown and are not
    /// repeated (still open; recorded in the gate's run log for the sub-agents' evidence).
    pub still_open: Vec<Problem>,
    /// What the gate could not check for this change set, whether or not the note is shown now.
    pub not_checked: Vec<String>,
    /// The run was `cpp_finish_check`'s (see [`GateInput::advisory`]).
    pub advisory: bool,
    /// Its build and tests were the targeted ones ([`crate::testsel::verify`]): only the
    /// targets and tests the change reaches.
    pub targeted: bool,
}

/// Build and test outcome of one change set, reused while the change set is unchanged.
#[derive(Clone, Debug)]
pub(crate) struct BuildMemo {
    pub fingerprint: u64,
    pub problems: Vec<Problem>,
    pub notes: Vec<String>,
    /// What could not be checked for this change set (one line each).
    pub not_checked: Vec<String>,
    /// Its problems were already sent to the model (not repeated for the same change set).
    pub delivered: bool,
}

/// Whether the gate's build + full-suite run happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SuiteRun {
    /// Built and tested, or the build fails (reported as a problem).
    Ran,
    /// The gate meant to build and test but could not (reason).
    NotChecked(String),
}

/// Advisory feedback, each piece shown at most once per session, and other session state of
/// the gate.
#[derive(Debug, Default)]
pub(crate) struct Advisories {
    /// The "could not check" note was delivered.
    pub not_checked_shown: bool,
    /// `[rule] file` keys of patch-shape findings already shown.
    pub shape_shown: HashSet<String>,
    /// Untracked paths the gate's own in-place builds created (never reported as stray files).
    pub gate_created: BTreeSet<String>,
}

/// The session's original crash: the first sanitizer report in the conversation.
#[derive(Clone, Debug, Default)]
pub(crate) struct CrashContext {
    /// Its crash-stack functions, innermost first (may be empty).
    pub functions: Vec<String>,
    /// Tool output that already showed patch-shape findings (`cpp_fix_verify`).
    pub shown_elsewhere: String,
}

/// Inputs of one gate run.
pub(crate) struct GateInput<'a> {
    pub snapshot: &'a Snapshot,
    pub changes: &'a AgentChanges,
    pub settings: &'a GateSettings,
    pub native_exec: bool,
    pub non_interactive: bool,
    /// Checklist keys listed so far and the numbers of those the model answered.
    pub impact_state: Option<&'a mut crate::workflow::ImpactState>,
    pub base: &'a mut BaseCache,
    /// Assistant text since the checklist was first shown.
    pub assistant_text: &'a str,
    /// Build/test result of the last change set checked.
    pub memo: &'a mut Option<BuildMemo>,
    pub advisories: &'a mut Advisories,
    /// Set in crash-fix sessions: enables the patch-shape critic.
    pub crash: Option<&'a CrashContext>,
    /// Wall-clock time the session has left (bounds any check the gate suggests).
    pub session_left: Duration,
    /// `cpp_finish_check`, the model-called advisory check: build and tests only (no hygiene
    /// rules, no checklist, no critic), and its verdict carries that tool's name.
    pub advisory: bool,
}

pub(crate) async fn run_gate(input: GateInput<'_>) -> GateOutcome {
    // Every program the gate starts runs at low priority on the spare CPUs.
    crate::priority::scope(crate::priority::automatic(), run_gate_inner(input)).await
}

async fn run_gate_inner(input: GateInput<'_>) -> GateOutcome {
    let started = Instant::now();
    let deadline = started + input.settings.budget;
    let snapshot = input.snapshot;
    let root = &snapshot.root;
    let changes = input.changes;
    let mut outcome = GateOutcome {
        advisory: input.advisory,
        ..GateOutcome::default()
    };
    touch_scratch(&snapshot.scratch);
    let _ = std::fs::create_dir_all(snapshot.scratch.join("logs"));

    if changes.is_empty() {
        if input.non_interactive && !input.advisory {
            outcome.problems.push(Problem {
                kind: ProblemKind::EmptyChange,
                text: "The repository has no changes: nothing was edited in this session. If the request asks for a code change, make it now; if it only asked a question, say so in your final message and finish.".to_string(),
            });
        }
        return outcome;
    }

    // A change that touches only the profile's non-code files (docs, images,
    // licence files) starts no build and no check: NOT RUN (no code or build change), never
    // PASS. Every other file, unknown types included, is code.
    let classes = session_file_classes(snapshot).await;
    let paths = changed_paths(root, changes);
    if classes.only_non_code(paths.iter().map(String::as_str)) {
        outcome.suite = SuiteState::NotRun(
            check_result::NotRunReason::NoCodeChange,
            format!(
                "the change touches only non-code files ({} path(s): docs, images or licence files)",
                paths.len()
            ),
        );
        outcome
            .notes
            .push("checks: skipped (no code or build change)".to_string());
        return outcome;
    }

    if !input.advisory {
        hygiene(snapshot, changes, input.advisories, &mut outcome);
    }

    // Patch-shape critic (advisory), in crash-fix sessions only.
    if let Some(crash) = input
        .crash
        .filter(|_| input.settings.critic && !input.advisory)
    {
        shape_review(snapshot, changes, crash, input.advisories, &mut outcome).await;
    }

    // Change-impact checklist. Always computed by the automatic gate: its copies facet is the
    // stale-copy check. The advisory check spends its time on the build and the tests.
    let report = if input.advisory {
        None
    } else {
        let req = impact::ImpactRequest {
            root: root.clone(),
            base_rev: snapshot.head.clone(),
            source: impact::Source::Diff,
            budget: Duration::from_secs(60).min(input.settings.budget / 3),
            use_clangd: false,
            max_items: impact::DEFAULT_MAX_ITEMS,
            preexisting_untracked: snapshot.untracked.iter().cloned().collect(),
        };
        Some(impact::analyze(&req).await)
    };
    if let Some(report) = &report {
        let stale: Vec<&impact::Item> = report
            .items
            .iter()
            .filter(|item| item.facet == impact::Facet::Copies)
            .collect();
        if !stale.is_empty() {
            outcome.problems.push(Problem {
                kind: ProblemKind::Copies,
                text: format!(
                    "GENERATED COPIES OUT OF SYNC: {}",
                    stale
                        .iter()
                        .take(3)
                        .map(|item| format!("{}:{} {}", item.path, item.line, item.text))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            });
        } else if !report.generated_copies.is_empty() {
            outcome.notes.push(format!(
                "generated copies in sync: {}",
                report
                    .generated_copies
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // Build and tests. They apply to every change that is not docs-only (returned above),
    // whatever the file type: an unknown file can change what the build produces.
    if !input.settings.tests {
        outcome.suite = SuiteState::NotRun(
            check_result::NotRunReason::Skipped,
            "they are switched off in this configuration (byteask_cpp.gate_tests)".to_string(),
        );
        outcome
            .notes
            .push("build/tests: off (configuration)".to_string());
    } else if input.session_left < MIN_SUITE_TIME {
        outcome.suite = SuiteState::NotRun(
            check_result::NotRunReason::TimeLimit,
            "too little of the session's time cap was left".to_string(),
        );
        outcome
            .notes
            .push("build/tests: off (too little of the session's time cap left)".to_string());
    } else if !input.native_exec {
        outcome.suite = SuiteState::NotRun(
            check_result::NotRunReason::NotTrusted,
            "the project is not trusted for native execution".to_string(),
        );
        outcome.notes.push(format!(
            "build/tests: did not run — the project is not trusted for native execution (set {}=1 or trust the project)",
            crate::trust::ALLOW_ENV
        ));
    } else {
        outcome.suite = SuiteState::Ran;
        let targeted =
            (input.advisory || input.settings.targeted) && crate::testsel::verify::applies(root);
        outcome.targeted = targeted;
        // `tree_owner`: a check cut short (something not checked) runs again on the kept build.
        let rerun_cut = input.advisory
            && crate::testsel::tree::owner_mode(&crate::testsel::tree::own_tree(root));
        let rerun = |memo: &BuildMemo| rerun_cut && !memo.not_checked.is_empty();
        let memo = match input.memo.take() {
            Some(memo) if memo.fingerprint == changes.fingerprint && !rerun(&memo) => memo,
            _ => {
                let mut part = GateOutcome::default();
                let mut not_checked = Vec::new();
                // `cpp_finish_check` on a CMake project builds only the test programs the
                // change reaches, in the tools' own tree, and runs exactly those tests.
                let suite = if targeted {
                    crate::testsel::verify::finish_check(
                        snapshot,
                        deadline,
                        input.settings.budget,
                        &mut part,
                        &mut not_checked,
                    )
                    .await
                } else {
                    build_and_test(
                        snapshot,
                        changes,
                        input.base,
                        &mut input.advisories.gate_created,
                        deadline,
                        &mut part,
                        &mut not_checked,
                    )
                    .await
                };
                if let SuiteRun::NotChecked(reason) = &suite {
                    not_checked.insert(0, format!("the build and test suite ({reason})"));
                }
                BuildMemo {
                    fingerprint: changes.fingerprint,
                    problems: part.problems,
                    notes: part.notes,
                    not_checked,
                    delivered: false,
                }
            }
        };
        // Findings of an unchanged change set were already sent once: not repeated (but they
        // are still open, which the run log records).
        if memo.delivered {
            outcome.still_open.extend(memo.problems.iter().cloned());
        } else {
            outcome.problems.extend(memo.problems.iter().cloned());
        }
        outcome.notes.extend(memo.notes.iter().cloned());
        outcome.not_checked.extend(memo.not_checked.iter().cloned());
        if !memo.not_checked.is_empty() && !input.advisories.not_checked_shown && !input.advisory {
            outcome.problems.push(Problem {
                kind: ProblemKind::SuiteNotChecked,
                text: not_checked_text(&memo.not_checked, input.session_left),
            });
        }
        *input.memo = Some(memo);
    }

    // Checklist items still without an answer.
    if input.settings.impact
        && let (Some(report), Some(state)) = (&report, input.impact_state)
    {
        if let Some(text) = state.finish_delta(report, input.assistant_text) {
            outcome.problems.push(Problem {
                kind: ProblemKind::Impact,
                text,
            });
        }
    }
    outcome.problems.sort_by(|a, b| a.kind.cmp(&b.kind));
    outcome
}

/// The automatic gate's hygiene rules (not the advisory check's): existing test files edited,
/// files left from this session, new test files added.
fn hygiene(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    advisories: &Advisories,
    outcome: &mut GateOutcome,
) {
    let root = &snapshot.root;
    // Existing test files edited.
    let edited_tests: Vec<&String> = changes
        .tracked
        .iter()
        .filter(|(status, path)| {
            *status != 'A' && gitx::is_test_path(path) && !snapshot.dirty.contains_key(path)
        })
        .map(|(_, path)| path)
        .collect();
    if !edited_tests.is_empty() {
        outcome.problems.push(Problem {
            kind: ProblemKind::TestEdits,
            text: format!(
                "EXISTING TEST FILES EDITED: {}. Revert them (`git checkout {} -- <file>`) unless the request itself changes what they assert; keep any tests you need for yourself outside these files and remove them before finishing.",
                edited_tests.iter().take(8).map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                &snapshot.head[..snapshot.head.len().min(12)]
            ),
        });
    }

    // Leftover files: new untracked paths nothing in the change refers to (build outputs and
    // what the gate's own builds created do not count).
    let stray_paths = stray_paths(root, changes, &advisories.gate_created);
    if !stray_paths.is_empty() {
        outcome.problems.push(Problem {
            kind: ProblemKind::Leftovers,
            text: format!(
                "FILES LEFT FROM THIS SESSION that the change does not use: {}. Delete them (scratch programs, logs, build directories, temporary tests) unless they belong to the change.",
                stray_paths.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    let new_tests: Vec<&String> = changes
        .tracked
        .iter()
        .filter(|(status, path)| *status == 'A' && gitx::is_test_path(path))
        .map(|(_, path)| path)
        .chain(
            changes
                .new_untracked
                .iter()
                .filter(|p| gitx::is_test_path(p) && !stray_paths.contains(*p)),
        )
        .collect();
    if !new_tests.is_empty() {
        outcome.problems.push(Problem {
            kind: ProblemKind::TestEdits,
            text: format!(
                "NEW TEST FILES ADDED: {}. Keep temporary tests out of the final change: remove these files unless the request asks for tests, and describe the tests you would add in your final message instead.",
                new_tests.iter().take(6).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        });
    }
}

/// The one "could not check" note: what was not checked, and a bounded way to check it.
fn not_checked_text(items: &[String], session_left: Duration) -> String {
    let minutes = (session_left
        .saturating_sub(Duration::from_secs(300))
        .as_secs()
        / 60)
        .min(10);
    let how = if minutes >= 2 {
        format!(
            " If you want to check it yourself, run only the tests that cover your change (cpp_test_select) under `timeout {}m`, not the whole suite; otherwise finish.",
            minutes
        )
    } else {
        " Too little session time is left to check it; finish.".to_string()
    };
    format!(
        "COULD NOT CHECK (this does not block finishing): {}.{how}",
        items.join("; ")
    )
}

/// Whether the outcome needs a feedback round: some problem that is not only advisory.
pub(crate) fn needs_feedback(outcome: &GateOutcome) -> bool {
    outcome.problems.iter().any(|p| p.kind.blocking())
}

/// The name the gate's results carry.
pub(crate) const CHECK: &str = "cpp_finish_gate";
/// The name the model-called advisory check's results carry.
pub(crate) const FINISH_CHECK: &str = "cpp_finish_check";

impl GateOutcome {
    /// The run's verdict, in the shared result format. PASS only when every check that
    /// applies ran and passed. An open problem is FAIL. A build or test run that applies but did
    /// not run, or anything the gate could not check, is NOT RUN with its reason; it still never
    /// holds the turn back by itself, but it is never "passed". Nothing C/C++ changed is
    /// NO CLAIM: the gate checks C/C++ changes only.
    pub(crate) fn result(&self) -> check_result::CheckResult {
        use check_result::CheckResult;
        let check = if self.advisory { FINISH_CHECK } else { CHECK };
        let open: Vec<&Problem> = self
            .problems
            .iter()
            .filter(|p| p.kind.fails_check())
            .collect();
        let still_open: Vec<&Problem> = self
            .still_open
            .iter()
            .filter(|p| p.kind.fails_check())
            .collect();
        if let Some(first) = open.first().or(still_open.first()) {
            let count = open.len().max(still_open.len());
            return CheckResult::fail(
                check,
                &format!(
                    "{count} problem(s) open; first: {}",
                    first_line_of(&first.text)
                ),
            );
        }
        if let SuiteState::NotRun(reason, why) = &self.suite {
            return CheckResult::not_run(
                check,
                *reason,
                &format!("The build and tests did not run: {why}."),
                "",
            );
        }
        if let Some(first) = self.not_checked.first() {
            return CheckResult::not_run(
                check,
                not_checked_reason(first),
                &format!(
                    "Not everything was checked: {}.",
                    self.not_checked.join("; ")
                ),
                if self.advisory {
                    "run only the tests that cover the change (cpp_test_select) under a time limit, and say what is still unverified."
                } else {
                    "run only the tests that cover the change (cpp_test_select) under a time limit, or give the gate more time (byteask_cpp.gate_budget_secs)."
                },
            );
        }
        match self.suite {
            SuiteState::NotNeeded if self.advisory => CheckResult::not_run(
                check,
                check_result::NotRunReason::NoCodeChange,
                "Nothing changed since the session started, so there was nothing to build or test.",
                "make the change first, then call cpp_finish_check again.",
            ),
            SuiteState::NotNeeded => CheckResult::no_claim(
                CHECK,
                "Nothing changed, so there was nothing to build or test.",
            ),
            // `BYTEASK_FINISH_CHECK_NEAR_ONLY`: only the nearest tests' programs were built and run.
            _ if self.advisory
                && self.targeted
                && self
                    .notes
                    .iter()
                    .any(|note| note.contains(crate::testsel::verify::NEAR_ONLY_NOTE)) =>
            {
                CheckResult::pass(
                    check,
                    "The nearest tests' programs the change reaches built and their tests pass as they did before \
                     the change (the other tests it reaches were not run; they are named above).",
                )
            }
            _ if self.advisory && self.targeted => CheckResult::pass(
                check,
                "The targets the change reaches built and their tests pass as they did before the change.",
            ),
            _ if self.advisory => CheckResult::pass(
                check,
                "Every target built and the tests pass as they did before the change.",
            ),
            _ => CheckResult::pass(
                CHECK,
                "Every target built, the tests pass as they did before the change, and the hygiene checks passed.",
            ),
        }
    }
}

fn first_line_of(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// The NOT RUN reason for one "could not check" item.
fn not_checked_reason(item: &str) -> check_result::NotRunReason {
    use check_result::NotRunReason;
    let item = item.to_ascii_lowercase();
    if item.contains("time budget") || item.contains("time limit") {
        NotRunReason::TimeLimit
    } else if item.contains("no configured build")
        || item.contains("no test target")
        || item.contains("no tests ran")
    {
        NotRunReason::NoBuildProfile
    } else if item.contains("configuring a scratch build failed") {
        NotRunReason::BuildFailed
    } else if item.contains("the change is not covered") {
        // `BYTEASK_FINISH_CHECK_UNCOVERED`: no target compiles the change.
        NotRunReason::NotChecked
    } else {
        NotRunReason::ToolError
    }
}

/// New untracked paths nothing in the change refers to (by file name). Build outputs (object
/// files, archives, executables, directories holding only those or a configured build tree)
/// and paths the gate's own builds created do not count.
fn stray_paths(
    root: &Path,
    changes: &AgentChanges,
    gate_created: &BTreeSet<String>,
) -> Vec<String> {
    let mut referenced_text = String::new();
    for (_, path) in &changes.tracked {
        if let Ok(text) = std::fs::read_to_string(root.join(path)) {
            referenced_text.push_str(&text);
        }
    }
    let mut out = Vec::new();
    for path in &changes.new_untracked {
        let trimmed = path.trim_end_matches('/');
        let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
        let is_dir = path.ends_with('/');
        let created_by_gate = gate_created
            .iter()
            .any(|created| created == trimmed || created.starts_with(&format!("{trimmed}/")));
        if created_by_gate || is_build_output(&root.join(trimmed)) {
            continue;
        }
        if !is_dir && referenced_text.contains(name) {
            continue;
        }
        if is_dir {
            // A new directory is fine when a changed file refers to it (a new module).
            if referenced_text.contains(&format!("{name}/"))
                || referenced_text.contains(&format!("\"{name}\""))
            {
                continue;
            }
        }
        out.push(path.clone());
    }
    out
}

/// The profile's file classes, read from the commit the session started on (so
/// an edit to the profile during the session never applies to its own checks), else the
/// defaults. A profile that does not parse gives the defaults too (the merge check reports it).
pub(crate) async fn session_file_classes(snapshot: &Snapshot) -> checks_profile::FileClasses {
    match gitx::show(&snapshot.root, &snapshot.head, checks_profile::PATH).await {
        Some(text) => checks_profile::Profile::parse(&text)
            .map(|profile| profile.file_classes())
            .unwrap_or_else(|_| checks_profile::FileClasses::defaults()),
        None => checks_profile::FileClasses::defaults(),
    }
}

/// Every path the change touches, for the code/non-code classification: changed tracked files and the
/// files inside new untracked paths (a new directory file by file, up to 2,000; a bigger one
/// stands for itself, which counts as code). Configured build trees and build artifacts are
/// outputs, not the change.
pub(crate) fn changed_paths(root: &Path, changes: &AgentChanges) -> Vec<String> {
    let mut paths: Vec<String> = changes
        .tracked
        .iter()
        .map(|(_, path)| path.clone())
        .collect();
    for path in &changes.new_untracked {
        let full = root.join(path.trim_end_matches('/'));
        if is_build_output(&full) {
            continue;
        }
        if full.is_dir() {
            let files: Vec<String> = walkdir::WalkDir::new(&full)
                .follow_links(false)
                .into_iter()
                .flatten()
                .filter(|entry| entry.file_type().is_file())
                .take(2_001)
                .filter_map(|entry| {
                    entry
                        .path()
                        .strip_prefix(root)
                        .ok()
                        .map(|rel| rel.to_string_lossy().into_owned())
                })
                .collect();
            if files.is_empty() || files.len() > 2_000 {
                paths.push(path.clone());
            } else {
                paths.extend(files);
            }
        } else {
            paths.push(path.clone());
        }
    }
    paths
}

/// The "why is the build slow" card for this checkout, from the context engine's
/// -ftime-trace data or trace files in its build trees; `None` without data.
pub(crate) fn slow_build_card(snapshot: &Snapshot) -> Option<String> {
    let mut trees = snapshot.build_trees.clone();
    trees.push(snapshot.scratch.join("cur-build"));
    let costs = crate::slowcard::collect(&trees, |key| std::env::var(key).ok());
    crate::slowcard::card(&costs)
}

/// Extensions of build artifacts (objects, archives, libraries, dependency and coverage files).
const ARTIFACT_EXTS: &[&str] = &[
    "o",
    "obj",
    "a",
    "lib",
    "so",
    "dylib",
    "dll",
    "lo",
    "la",
    "d",
    "dep",
    "po",
    "plo",
    "tpo",
    "gcno",
    "gcda",
    "pyc",
    "pyo",
    "class",
    "exp",
    "pdb",
    "ilk",
    "stamp",
    "ninja_deps",
    "ninja_log",
];

/// A build artifact or a directory of nothing but build artifacts / a configured build tree.
fn is_build_output(path: &Path) -> bool {
    if path.is_dir() {
        if path.join("CMakeCache.txt").is_file()
            || path.join("build.ninja").is_file()
            || path.join("meson-private").is_dir()
            || path.join("config.status").is_file()
        {
            return true;
        }
        let mut files = 0usize;
        for entry in walkdir::WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .flatten()
            .take(2_000)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if !is_artifact_file(entry.path()) {
                return false;
            }
            files += 1;
        }
        return files > 0;
    }
    path.is_file() && is_artifact_file(path)
}

fn is_artifact_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if ARTIFACT_EXTS.contains(&ext) || name.contains(".so.") {
        return true;
    }
    // Executables and other binaries the compiler produced (ELF / Mach-O).
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .is_ok()
        && (magic == *b"\x7fELF"
            || magic == [0xcf, 0xfa, 0xed, 0xfe]
            || magic == [0xfe, 0xed, 0xfa, 0xcf])
}

async fn build_and_test(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    base: &mut BaseCache,
    gate_created: &mut BTreeSet<String>,
    deadline: Instant,
    outcome: &mut GateOutcome,
    not_checked: &mut Vec<String>,
) -> SuiteRun {
    let root = &snapshot.root;
    let scratch = &snapshot.scratch;
    let logs = scratch.join("logs");
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let Some(plan) = build::plan(root, scratch, &snapshot.build_trees) else {
        outcome.notes.push(
            "build/tests: did not run — no configured build (CMake/Meson/Make) was found and the gate does not configure a project in place".to_string(),
        );
        return SuiteRun::NotChecked(
            "no configured build (CMake, Meson or Make) was found, and the gate does not configure a project in place".to_string(),
        );
    };
    // Scratch CMake tree for the working tree: configure it first.
    if plan.scratch
        && let BuildKind::CMake { dir, .. } = &plan.kind
    {
        let ok = build::cmake_configure(
            root,
            dir,
            None,
            remaining(deadline).min(Duration::from_secs(300)),
            &logs.join("configure.log"),
        )
        .await;
        if !ok && remaining(deadline).is_zero() {
            // A configure stopped by the gate's own budget is a time limit, not a broken
            // project (the fast tier makes this the common case).
            outcome.notes.push(
                "build/tests: did not run — configuring a scratch build did not finish within the gate's time budget"
                    .to_string(),
            );
            return SuiteRun::NotChecked(
                "configuring a scratch build did not finish within the gate's time budget"
                    .to_string(),
            );
        }
        if !ok {
            outcome.notes.push(format!(
                "build/tests: did not run — configuring a scratch build failed (log: {})",
                logs.join("configure.log").display()
            ));
            return SuiteRun::NotChecked(format!(
                "configuring a scratch build failed (log: {})",
                logs.join("configure.log").display()
            ));
        }
    }
    // Snapshot untracked files so files our in-place build creates can be named (never removed).
    let in_place = matches!(plan.kind, BuildKind::Make { .. });
    let before: BTreeSet<String> = if in_place {
        gitx::untracked_files(root, 50_000)
            .await
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    let suite = build_and_test_with(
        snapshot,
        changes,
        base,
        &plan,
        deadline,
        outcome,
        not_checked,
    )
    .await;
    note_in_place_outputs(root, in_place, &before, gate_created, outcome).await;
    suite
}

/// [`build_and_test`] once the plan is known (naming what an in-place build created is the
/// caller's).
async fn build_and_test_with(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    base: &mut BaseCache,
    plan: &BuildPlan,
    deadline: Instant,
    outcome: &mut GateOutcome,
    not_checked: &mut Vec<String>,
) -> SuiteRun {
    let root = &snapshot.root;
    let logs = snapshot.scratch.join("logs");
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());

    // 1. Build.
    let build_budget = remaining(deadline)
        .mul_f64(0.6)
        .max(Duration::from_secs(30));
    let current = build::build(plan, root, build_budget, &logs.join("build.log")).await;
    if !current.started {
        outcome.notes.push(format!(
            "build: could not start (log: {})",
            current.log.display()
        ));
        return SuiteRun::NotChecked(format!(
            "its build could not start: {}",
            first_line_of(&current.tail)
        ));
    }
    if current.timed_out {
        outcome.notes.push(format!(
            "build: did not finish within the gate's time budget ({}s; log: {})",
            current.secs,
            current.log.display()
        ));
        return SuiteRun::NotChecked(format!(
            "its build did not finish within the gate's time budget ({}s)",
            current.secs
        ));
    } else if !current.ok {
        // A build that exits non-zero FAILS, whether or not an error line was recognised (CMake
        // errors, MSVC, ninja, a linker or a script): the tests would run binaries left from an
        // earlier build, so they are not run.
        let shown = if current.errors.is_empty() {
            format!(
                "no recognised compiler error; the end of the build log:\n{}",
                current.tail
            )
        } else {
            current
                .errors
                .iter()
                .take(6)
                .map(|(_, text)| text.clone())
                .collect::<Vec<_>>()
                .join(" | ")
        };
        outcome.problems.push(Problem {
            kind: ProblemKind::Build,
            text: format!(
                "BUILD FAILS ({}): {} — full log: {}",
                plan.description,
                shown,
                current.log.display()
            ),
        });
        outcome
            .notes
            .push("tests: not run — the build fails".to_string());
        return SuiteRun::Ran;
    } else {
        outcome.notes.push(format!(
            "build: ok ({}, {}s)",
            plan.description, current.secs
        ));
    }

    // 2. Tests.
    let Some(command) = build::test_command(plan, root).await else {
        outcome
            .notes
            .push("tests: no test target found".to_string());
        return SuiteRun::NotChecked(format!(
            "no test target was found in {} (ctest, meson test, a check/tests target)",
            plan.description
        ));
    };
    if remaining(deadline) < Duration::from_secs(20) {
        outcome
            .notes
            .push("tests: not run — time budget used by the build".to_string());
        return SuiteRun::NotChecked("the build used the gate's time budget".to_string());
    }
    let run = build::run_tests(
        &command,
        None,
        remaining(deadline).mul_f64(0.7),
        &logs.join("tests.log"),
        false,
    )
    .await;
    if run.no_tests {
        outcome
            .notes
            .push(format!("tests: no tests ran ({})", run.command));
        not_checked.push(format!(
            "no tests ran (`{}` found no tests to run)",
            run.command
        ));
        return SuiteRun::Ran;
    }
    let total = run.results.len();
    let failing_entries: BTreeSet<String> = failing_names(&run.results);
    let failing_cases: BTreeSet<String> = failing_names(&run.cases);
    if run.timed_out {
        not_checked.push(format!(
            "the test run did not finish within the gate's time budget ({} results so far)",
            total
        ));
    }
    if failing_entries.is_empty() && failing_cases.is_empty() {
        outcome.notes.push(if run.timed_out {
            format!(
                "tests: did not finish within the time budget ({total} results so far, none failing; log: {})",
                run.log.display()
            )
        } else {
            format!("tests: {total} passed or skipped ({}, {}s)", run.command, run.secs)
        });
        return SuiteRun::Ran;
    }

    // 3. Re-run the failures once, serially (flaky and load-dependent tests). Tests that hit the
    //    per-test time limit are not re-run (each would cost the whole limit again), and a suite
    //    that cannot be filtered is re-run only when there is time for it.
    let timed_out: BTreeSet<String> = failing_entries
        .iter()
        .filter(|n| run.results.get(*n) == Some(&TestStatus::TimedOut))
        .cloned()
        .collect();
    let to_rerun: BTreeSet<String> = failing_entries.difference(&timed_out).cloned().collect();
    let filterable = command.0 == "ctest";
    let rerun = if !to_rerun.is_empty()
        && remaining(deadline) > Duration::from_secs(20)
        && (filterable || remaining(deadline) > Duration::from_secs(run.secs.saturating_mul(2)))
    {
        Some(
            build::run_tests(
                &command,
                filterable.then_some(&to_rerun),
                remaining(deadline).mul_f64(0.4),
                &logs.join("tests-rerun.log"),
                true,
            )
            .await,
        )
    } else {
        None
    };
    let still_failing: BTreeSet<String> = match &rerun {
        Some(rerun) => to_rerun
            .iter()
            .filter(|name| rerun.results.get(*name).is_none_or(|s| s.failing()))
            .cloned()
            .collect(),
        None => to_rerun.clone(),
    };
    let still_failing_cases: BTreeSet<String> = failing_cases
        .iter()
        .filter(|case| {
            let entry = case
                .split_once(build::CASE_SEP)
                .map(|(e, _)| e)
                .unwrap_or(case.as_str());
            if timed_out.contains(entry) {
                return false;
            }
            match &rerun {
                Some(rerun) if rerun.results.contains_key(entry) => {
                    rerun.cases.get(*case).is_some_and(|s| s.failing())
                        || (!rerun.cases.contains_key(*case)
                            && rerun.results.get(entry).is_some_and(|s| s.failing())
                            && !rerun
                                .cases
                                .keys()
                                .any(|k| k.starts_with(&format!("{entry}{}", build::CASE_SEP))))
                }
                _ => true,
            }
        })
        .cloned()
        .collect();
    let flaky = to_rerun.len() - still_failing.len();
    if !timed_out.is_empty() {
        not_checked.push(format!(
            "{} test(s) hit the per-test time limit and were not re-run or compared with the unmodified tree: {}",
            timed_out.len(),
            timed_out.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if still_failing.is_empty() && still_failing_cases.is_empty() {
        outcome.notes.push(format!(
            "tests: {total} ran; {flaky} failure(s) passed on the serial re-run (flaky){}",
            if timed_out.is_empty() {
                String::new()
            } else {
                format!("; {} hit the time limit", timed_out.len())
            }
        ));
        return SuiteRun::Ran;
    }

    // 4. Base results for what still fails (entries; the cases come with them).
    let base_results = base_test_results(
        snapshot,
        changes,
        plan,
        &command,
        &still_failing,
        base,
        deadline,
    )
    .await;
    let pseudo = build::suite_label(&command.0, &command.1);
    let mut regressions: Vec<String> = Vec::new();
    let mut case_regressions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut new_tests = Vec::new();
    let mut not_compared: Vec<String> = Vec::new();
    let mut preexisting = 0usize;
    for name in &still_failing {
        let status = run.results.get(name).copied().unwrap_or(TestStatus::Failed);
        let label = if status == TestStatus::NotRun {
            format!("{name} (not built / not run)")
        } else {
            name.clone()
        };
        match base_results.as_ref().map(|b| b.get(name)) {
            Some(Some(TestStatus::Passed)) => regressions.push(name.clone()),
            Some(Some(_)) => preexisting += 1,
            Some(None) if *name != pseudo => new_tests.push(label),
            // The base run of the suite gave no overall result (it did not finish).
            Some(None) | None => not_compared.push(label),
        }
    }
    // Cases: a case that passes on the unmodified tree and fails now, inside an entry that
    // fails in both runs (the regression the entry-level comparison cannot see).
    for case in &still_failing_cases {
        let Some((entry, _)) = case.split_once(build::CASE_SEP) else {
            continue;
        };
        if regressions.iter().any(|r| r == entry) {
            continue; // listed with its entry
        }
        let Some(base_map) = &base_results else {
            not_compared.push(case.clone());
            continue;
        };
        let base_status = base_map.get(case).copied().or_else(|| {
            base_map
                .get(&format!("{entry}{}{}", build::CASE_SEP, build::OTHER_CASES))
                .copied()
        });
        match base_status {
            Some(TestStatus::Passed) => case_regressions
                .entry(entry.to_string())
                .or_default()
                .push(case[entry.len() + build::CASE_SEP.len()..].to_string()),
            Some(_) => {}
            None => {}
        }
    }
    // The suite's own pseudo-test adds nothing when more specific results explain it.
    if regressions.len() > 1 || !case_regressions.is_empty() {
        regressions.retain(|r| *r != pseudo);
    }
    if !regressions.is_empty() {
        let listed: Vec<String> = regressions
            .iter()
            .take(12)
            .map(|name| {
                let cases = cases_of(&run.cases, name);
                if cases.is_empty() {
                    name.clone()
                } else {
                    format!(
                        "{name} (failing cases: {})",
                        cases.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
                    )
                }
            })
            .collect();
        outcome.problems.push(Problem {
            kind: ProblemKind::Tests,
            text: format!(
                "TESTS THAT PASS ON THE UNMODIFIED TREE NOW FAIL (re-run once, serially): {} — log: {}. A test that no longer builds counts as failing. Fix the code (do not edit the tests) unless the request changes what they assert.",
                listed.join(", "),
                run.log.display()
            ),
        });
    }
    if !case_regressions.is_empty() {
        let listed: Vec<String> = case_regressions
            .iter()
            .take(6)
            .map(|(entry, cases)| {
                format!(
                    "{entry}: {}",
                    cases
                        .iter()
                        .take(10)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        outcome.problems.push(Problem {
            kind: ProblemKind::Tests,
            text: format!(
                "TEST CASES THAT PASS ON THE UNMODIFIED TREE NOW FAIL (inside tests that already failed there for other cases): {} — log: {}. Fix the code (do not edit the tests) unless the request changes what they assert.",
                listed.join("; "),
                run.log.display()
            ),
        });
    }
    if !new_tests.is_empty() {
        outcome.problems.push(Problem {
            kind: ProblemKind::Tests,
            text: format!(
                "TESTS THAT DO NOT EXIST ON THE UNMODIFIED TREE FAIL: {} — log: {}",
                new_tests
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                run.log.display()
            ),
        });
    }
    if !not_compared.is_empty() {
        not_checked.push(format!(
            "{} failing test(s) could not be compared with the unmodified tree (its build or run did not fit in the gate's time budget), so they may have failed before your change: {} (log: {})",
            not_compared.len(),
            not_compared.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
            run.log.display()
        ));
        outcome.notes.push(format!(
            "tests: {} failure(s) not compared with the unmodified tree",
            not_compared.len()
        ));
    }
    if preexisting > 0 {
        outcome.notes.push(format!("tests: {preexisting} failure(s) also fail on the unmodified tree (not caused by the change)"));
    }
    SuiteRun::Ran
}

fn failing_names(results: &BTreeMap<String, TestStatus>) -> BTreeSet<String> {
    results
        .iter()
        .filter(|(name, status)| {
            status.failing()
                && !name.ends_with(&format!("{}{}", build::CASE_SEP, build::OTHER_CASES))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The failing cases of `entry` in a run.
fn cases_of(cases: &BTreeMap<String, TestStatus>, entry: &str) -> Vec<String> {
    let prefix = format!("{entry}{}", build::CASE_SEP);
    cases
        .iter()
        .filter(|(name, status)| status.failing() && name.starts_with(&prefix))
        .map(|(name, _)| name[prefix.len()..].to_string())
        .collect()
}

/// The advisory patch-shape critic on the agent's diff, each finding once per session.
/// Findings a `cpp_fix_verify` run already showed, and test edits the gate reports itself,
/// are not repeated.
async fn shape_review(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    crash: &CrashContext,
    advisories: &mut Advisories,
    outcome: &mut GateOutcome,
) {
    let Some(diff) = agent_diff(snapshot, changes).await else {
        outcome
            .notes
            .push("patch-shape review: the diff could not be read".to_string());
        return;
    };
    let test_edits_reported = outcome
        .problems
        .iter()
        .any(|p| p.kind == ProblemKind::TestEdits);
    let key = |finding: &crate::memsafety::critic::ShapeFinding| {
        if finding.file.is_empty() {
            format!("[{}]", finding.rule)
        } else {
            format!("[{}] {}:", finding.rule, finding.file)
        }
    };
    let findings: Vec<crate::memsafety::critic::ShapeFinding> =
        crate::memsafety::critic::critique(&diff, &crash.functions)
            .into_iter()
            .filter(|f| !(test_edits_reported && f.rule == "test-or-harness-edit"))
            .filter(|f| !crash.shown_elsewhere.contains(&key(f)))
            .filter(|f| !advisories.shape_shown.contains(&key(f)))
            .collect();
    if findings.is_empty() {
        outcome
            .notes
            .push("patch-shape review (crash fix): no new findings".to_string());
        return;
    }
    let mut text = String::from(
        "PATCH SHAPE OF THE CRASH FIX (advisory, static; shown once): for each item, change the patch or say in one line in your final message why it is right.",
    );
    for finding in &findings {
        advisories.shape_shown.insert(key(finding));
        let at = if finding.file.is_empty() {
            String::new()
        } else {
            format!(" {}:{}", finding.file, finding.line)
        };
        text.push_str(&format!(
            "\n   - [{}]{at} {}",
            finding.rule, finding.message
        ));
    }
    outcome.problems.push(Problem {
        kind: ProblemKind::ShapeAdvisory,
        text,
    });
}

/// The agent's change as a unified diff: tracked files vs the session-start HEAD, plus new
/// untracked C/C++ files. `None` when git fails.
async fn agent_diff(snapshot: &Snapshot, changes: &AgentChanges) -> Option<String> {
    let root = &snapshot.root;
    let mut out = String::new();
    if !changes.tracked.is_empty() {
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            snapshot.head.as_str(),
            "--",
        ];
        args.extend(changes.tracked.iter().map(|(_, path)| path.as_str()));
        out.push_str(&gitx::git(root, &args).await?);
    }
    for path in &changes.new_untracked {
        if path.ends_with('/') || !gitx::is_cpp_path(path) || impact::looks_like_build_output(path)
        {
            continue;
        }
        // `--no-index` exits 1 when the files differ, which is the point here.
        if let Some((_, text)) = gitx::git_status_out(
            root,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
                path.as_str(),
            ],
        )
        .await
        {
            out.push_str(&text);
        }
    }
    Some(out)
}

/// Name the untracked files our own in-place build created in the source tree (they are left in
/// place: nothing here deletes a file), and remember them (and their directories) so they are
/// never reported as files the agent left behind.
async fn note_in_place_outputs(
    root: &Path,
    in_place: bool,
    before: &BTreeSet<String>,
    gate_created: &mut BTreeSet<String>,
    outcome: &mut GateOutcome,
) {
    if !in_place {
        return;
    }
    let mut created = Vec::new();
    for path in gitx::untracked_files(root, 50_000).await {
        if before.contains(&path) {
            continue;
        }
        let mut dir = Path::new(&path).parent();
        while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            let rel = d.display().to_string();
            if before.iter().any(|b| b.starts_with(&format!("{rel}/"))) {
                break;
            }
            gate_created.insert(rel);
            dir = d.parent();
        }
        gate_created.insert(path.clone());
        created.push(path);
    }
    if !created.is_empty() {
        outcome.notes.push(format!(
            "build: the in-place build created {} untracked file(s) in the source tree, left in place: {}",
            created.len(),
            created.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
}

/// Make sure the base tree exists (copy of the session-start state) and its build is set up.
async fn ensure_base(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    plan: &BuildPlan,
    base: &mut BaseCache,
    deadline: Instant,
) -> Option<BuildPlan> {
    if base.tree_failed {
        return None;
    }
    let scratch = &snapshot.scratch;
    let logs = scratch.join("logs");
    let tree = scratch.join("base");
    if !base.tree_ready {
        let mut exclude: Vec<String> = changes.new_untracked.clone();
        // Build trees inside the repository are not copied (absolute paths; rebuilt instead).
        if let BuildKind::CMake { .. } | BuildKind::Meson { .. } = plan.kind {
            for dir in build::cmake_build_dirs(&snapshot.root, scratch) {
                if let Ok(rel) = dir.strip_prefix(&snapshot.root) {
                    exclude.push(rel.display().to_string());
                }
            }
        }
        let ok = build::prepare_base_tree(
            &snapshot.root,
            &tree,
            &snapshot.head,
            &base_marker(snapshot),
            &changes.tracked,
            snapshot.start_copy.as_deref(),
            &exclude,
            &logs.join("base-copy.log"),
        )
        .await;
        if !ok {
            base.tree_failed = true;
            return None;
        }
        base.tree_ready = true;
    }
    let Some(base_plan) = build::base_plan(plan, &snapshot.root, &tree, scratch) else {
        base.tree_failed = true;
        return None;
    };
    if !base.configured {
        if let BuildKind::Make { .. } = &base_plan.kind {
            // Configured makefiles name the working tree by absolute path: point them at the
            // copy, so the base build and tests never read or write the working tree.
            crate::testsel::make::rewrite_absolute_paths(&tree, &snapshot.root);
        }
        if let BuildKind::CMake { dir, .. } = &base_plan.kind {
            let reference = match &plan.kind {
                BuildKind::CMake { dir, .. } if !plan.scratch => Some(dir.clone()),
                _ => None,
            };
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(300));
            if !build::cmake_configure(
                &tree,
                dir,
                reference.as_deref(),
                remaining,
                &logs.join("base-configure.log"),
            )
            .await
            {
                base.tree_failed = true;
                return None;
            }
        }
        base.configured = true;
    }
    Some(base_plan)
}

/// What a base tree on disk must have been made from to be reused: the session-start commit and
/// the session-start state of the work tree (dirty files and untracked paths). A tree left by
/// another session of the same repository with a different start is made again.
fn base_marker(snapshot: &Snapshot) -> String {
    let mut state = String::new();
    for (path, hash) in &snapshot.dirty {
        state.push_str(&format!("M{path}:{hash};"));
    }
    for path in &snapshot.untracked {
        state.push_str(&format!("U{path};"));
    }
    format!("{}\n{:016x}", snapshot.head, gitx::fnv1a(state.as_bytes()))
}

async fn base_build(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    plan: &BuildPlan,
    base: &mut BaseCache,
    deadline: Instant,
) -> Option<(BuildPlan, PathBuf)> {
    let base_plan = ensure_base(snapshot, changes, plan, base, deadline).await?;
    let tree = snapshot.scratch.join("base");
    if base.built.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(10) {
            return None;
        }
        let result = build::build(
            &base_plan,
            &tree,
            remaining,
            &snapshot.scratch.join("logs/base-build.log"),
        )
        .await;
        if result.timed_out {
            return None;
        }
        base.built = Some(result);
    }
    Some((base_plan, tree))
}

async fn base_test_results(
    snapshot: &Snapshot,
    changes: &AgentChanges,
    plan: &BuildPlan,
    command: &(String, Vec<String>, PathBuf),
    names: &BTreeSet<String>,
    base: &mut BaseCache,
    deadline: Instant,
) -> Option<BTreeMap<String, TestStatus>> {
    // The shared base-run cache (`base_run_cache`): results `cpp_test_select compare_base`
    // or an earlier gate run recorded for the same tree, commit and configuration. One base
    // run per session, whoever ran it.
    let shared = shared_base_key(snapshot, plan);
    if let Some(run) = shared.as_ref().and_then(crate::base_run_cache::load) {
        let known: Vec<(String, TestStatus)> = names
            .iter()
            .filter(|n| !base.asked.contains(*n))
            .filter_map(|n| {
                run.results
                    .get(n)
                    .and_then(|s| from_shared(*s))
                    .map(|s| (n.clone(), s))
            })
            .collect();
        for (name, status) in known {
            base.tests.insert(name.clone(), status);
            base.asked.insert(name);
        }
    }
    let missing: BTreeSet<String> = names
        .iter()
        .filter(|n| !base.asked.contains(*n))
        .cloned()
        .collect();
    if !missing.is_empty() {
        // Cheapest first: build just the failing tests' same-named targets on the base tree;
        // a full base build only when that is not possible.
        let mut targeted = None;
        if base.built.is_none()
            && command.0 == "ctest"
            && let Some(base_plan) = ensure_base(snapshot, changes, plan, base, deadline).await
        {
            let tree = snapshot.scratch.join("base");
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Some(result) = build::build_targets(
                &base_plan,
                &tree,
                &missing,
                remaining,
                &snapshot.scratch.join("logs/base-build-targets.log"),
            )
            .await
                && !result.timed_out
            {
                targeted = Some((base_plan, tree));
            }
        }
        let (base_plan, tree) = match targeted {
            Some(found) => found,
            None => base_build(snapshot, changes, plan, base, deadline).await?,
        };
        let base_command = match &base_plan.kind {
            BuildKind::CMake { dir, .. } | BuildKind::Meson { dir } => {
                let mut cmd = command.clone();
                if command.0 == "meson" {
                    cmd.1 = vec![
                        "test".to_string(),
                        "-C".to_string(),
                        dir.display().to_string(),
                        "--print-errorlogs".to_string(),
                    ];
                    cmd.2 = tree.clone();
                } else {
                    cmd.2 = dir.clone();
                }
                cmd
            }
            BuildKind::Make { dir, .. } => {
                let mut cmd = command.clone();
                cmd.2 = dir.clone();
                cmd
            }
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(10) {
            return None;
        }
        let run = build::run_tests(
            &base_command,
            Some(&missing),
            remaining,
            &snapshot.scratch.join("logs/base-tests.log"),
            false,
        )
        .await;
        if run.timed_out && run.results.is_empty() {
            return None;
        }
        if let Some(key) = &shared {
            let recorded = run
                .results
                .iter()
                .filter(|(name, _)| missing.contains(*name))
                .map(|(name, status)| (name.clone(), to_shared(*status)));
            // Best effort: the session's in-memory results below still hold.
            let _ = crate::base_run_cache::record(key, recorded);
        }
        // A suite without per-test output reports one pseudo-test under the same name; the
        // cases inside the output come along for the case-level comparison.
        base.tests.extend(run.results);
        base.tests.extend(run.cases);
        base.asked.extend(missing);
    }
    Some(base.tests.clone())
}

/// The shared base-run cache key for this gate's base comparison: only for a reused CMake
/// tree (ctest names, the same key `cpp_test_select` computes for that tree) and only when the
/// working tree was clean at session start (then the base is exactly the session-start
/// commit, which is what the cache records).
fn shared_base_key(
    snapshot: &Snapshot,
    plan: &BuildPlan,
) -> Option<crate::base_run_cache::BaseRunKey> {
    if !snapshot.dirty.is_empty() || plan.scratch {
        return None;
    }
    let BuildKind::CMake { dir, .. } = &plan.kind else {
        return None;
    };
    let config = crate::testsel::cmake::tree_config_key(dir)?;
    Some(crate::base_run_cache::BaseRunKey::new(
        &snapshot.root,
        &snapshot.head,
        &config,
    ))
}

fn to_shared(status: TestStatus) -> crate::base_run_cache::TestStatus {
    use crate::base_run_cache::TestStatus as Shared;
    match status {
        TestStatus::Passed => Shared::Pass,
        TestStatus::Failed => Shared::Fail,
        TestStatus::TimedOut => Shared::Timeout,
        // The gate's NotRun is "not built / not found", which fails.
        TestStatus::NotRun => Shared::NotBuilt,
        TestStatus::Skipped => Shared::Skip,
    }
}

fn from_shared(status: crate::base_run_cache::TestStatus) -> Option<TestStatus> {
    use crate::base_run_cache::TestStatus as Shared;
    match status {
        Shared::Pass | Shared::Flaky => Some(TestStatus::Passed),
        Shared::Fail => Some(TestStatus::Failed),
        Shared::Timeout => Some(TestStatus::TimedOut),
        Shared::NotBuilt => Some(TestStatus::NotRun),
        Shared::Skip => Some(TestStatus::Skipped),
        // Selected but never executed: no information.
        Shared::NotRun => None,
    }
}

/// Render the feedback message for the model.
pub(crate) fn render_feedback(outcome: &GateOutcome, round: u32, max_rounds: u32) -> String {
    let mut out = format!(
        "ByteAsk finish gate (automatic check before you finish, round {round} of {max_rounds}): {} problem(s). Fix what applies, then finish again; if an item is intended, say why in your final message.\n",
        outcome.problems.len()
    );
    for (n, problem) in outcome.problems.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", n + 1, problem.text));
    }
    if !outcome.notes.is_empty() {
        out.push_str("Other checks: ");
        out.push_str(&outcome.notes.join("; "));
        out.push('\n');
    }
    // The verdict as the last line, in the shared result format.
    let result = outcome.result();
    let body = impact::truncate_chars(out.trim_end(), 6_500);
    result.append_to(&body)
}

#[cfg(test)]
mod tests;
