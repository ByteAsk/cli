//! The per-project index of finish-gate verdicts, first written in the C/C++ tools
//! (`cpp-tools/src/results_store.rs`) and moved here so the results store is one
//! component. The on-disk format is unchanged.
//!
//! Every verdict is kept per project in `$BYTEASK_HOME/results/<project-key>/index.json`,
//! under its content key, with a freshness fingerprint of the work tree it was taken for.
//! Readers (the `check_status` tool, the Claude Code / Codex prompt hook, the merge check)
//! show a result as current only while that fingerprint still matches; anything else is
//! stale, never current. The index is small on purpose: the prompt hook reads only it,
//! within a 2 s budget.
//!
//! [`record_for_session`] also writes the verdict into the per-change evidence log (the value
//! report's source), with the version of the code taken when the check started.

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use check_result::CheckResult;
use serde::Deserialize;
use serde::Serialize;

use crate::workspace;

/// Results kept per project (newest first).
pub const KEEP: usize = 20;
const INDEX: &str = "index.json";
const INDEX_LOCK: &str = "index.lock";

/// One stored verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The verdict, with `content_key` set.
    pub result: CheckResult,
    /// The work tree it was taken for.
    pub root: PathBuf,
    /// Unix seconds.
    pub at: u64,
    /// Freshness fingerprint of the work tree when the check started.
    pub fresh: String,
    /// Which run produced it: `fast` (the bounded check at the end of a turn) or `full`
    /// (the background follow-up).
    pub tier: String,
    /// Who ran it: `engine`, `claude-code`, `codex`, ...
    pub source: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Index {
    pub(crate) entries: Vec<Entry>,
}

/// How a stored result relates to the work tree now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Nothing changed since the check started.
    Current,
    /// Something changed since: the result belongs to an older version.
    Stale,
    /// The fingerprint could not be taken within the budget ("freshness unknown").
    Unknown,
}

/// The directory name of a project: its folder name (sanitised, at most 24 characters) and
/// a hash of its full path. The project's index, its agent-session files and its kept build
/// trees all use it.
pub fn project_key(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(24)
        .collect();
    let name = if name.is_empty() || name.starts_with('.') {
        "project".to_string()
    } else {
        name
    };
    format!("{name}-{:016x}", fnv1a64(path_bytes(root).as_ref()))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `$BYTEASK_HOME/results/<project-key>` for `root` (resolved).
pub fn project_dir(home: &Path, root: &Path) -> PathBuf {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    crate::store::root(home).join(project_key(&root))
}

fn index_path(home: &Path, root: &Path) -> PathBuf {
    project_dir(home, root).join(INDEX)
}

/// Add `entry` to the project's index (newest first; the oldest beyond [`KEEP`] are
/// dropped). Best effort: a store we cannot write never changes a verdict.
pub fn record(home: &Path, entry: Entry) {
    let path = index_path(home, &entry.root);
    let Some(dir) = path.parent() else {
        return;
    };
    update_index(dir, |index| {
        index.entries.retain(|old| {
            old.result.content_key != entry.result.content_key || old.tier != entry.tier
        });
        index.entries.insert(0, entry);
        index.entries.truncate(KEEP);
    });
}

/// [`record`], and the same verdict into the per-change evidence log of `session` (the
/// value report's source), tied to the version of the code observed when the check started
/// (the [`fingerprint`] call that produced `entry.fresh`).
pub fn record_for_session(home: &Path, entry: Entry, session: &str) {
    let version = version_for(&entry.fresh);
    let root = entry.root.clone();
    let result = entry.result.clone();
    let at = entry.at;
    record(home, entry);
    let top = workspace::toplevel(&root, workspace::PROBE_BUDGET).unwrap_or_else(|| root.clone());
    let mut probe = workspace::probe_fresh(&root, Some(&top), workspace::PROBE_BUDGET);
    probe.version = version;
    let _ = crate::changes::append(
        home,
        session,
        &probe,
        at,
        crate::changes::Event::Result {
            result: &result,
            answer: None,
        },
    );
}

/// Rewrite the index in `dir` under its lock. Shared with the clean-up.
pub(crate) fn update_index(dir: &Path, edit: impl FnOnce(&mut Index)) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(INDEX_LOCK))
    else {
        return;
    };
    if lock.lock().is_err() {
        return;
    }
    let path = dir.join(INDEX);
    let mut index = read_index(&path);
    edit(&mut index);
    let Ok(text) = serde_json::to_string_pretty(&index) else {
        return;
    };
    let tmp = dir.join(format!("{INDEX}.tmp-{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    let _ = lock.unlock();
}

pub(crate) fn read_index(path: &Path) -> Index {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The newest stored verdict for `root`.
pub fn latest(home: &Path, root: &Path) -> Option<Entry> {
    read_index(&index_path(home, root))
        .entries
        .into_iter()
        .next()
}

/// Every stored verdict for `root`, newest first.
pub fn all(home: &Path, root: &Path) -> Vec<Entry> {
    read_index(&index_path(home, root)).entries
}

/// Versions seen with recent fingerprints, so [`record_for_session`] can tie a verdict to
/// the code observed when its check started without a second (and later) look.
static VERSIONS: Mutex<VecDeque<(String, Option<String>)>> = Mutex::new(VecDeque::new());
const VERSIONS_KEPT: usize = 64;

fn remember_version(fresh: &str, version: Option<String>) {
    let mut seen = VERSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.retain(|(old, _)| old != fresh);
    seen.push_front((fresh.to_string(), version));
    seen.truncate(VERSIONS_KEPT);
}

fn version_for(fresh: &str) -> Option<String> {
    if fresh.is_empty() {
        return None;
    }
    VERSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(seen, _)| seen == fresh)
        .and_then(|(_, version)| version.clone())
}

/// A cheap fingerprint of the work tree: the commit, `git status --porcelain=v2`, and
/// the size and modification time of every path that status lists (status alone does not
/// change when an already-modified file is edited again). `None` when git does not answer in
/// time. Also takes the version of the code (see [`record_for_session`]).
pub async fn fingerprint(root: &Path, budget: Duration) -> Option<String> {
    let root = root.to_path_buf();
    let probe = tokio::task::spawn_blocking(move || {
        let top = workspace::toplevel(&root, budget)?;
        Some(workspace::probe(&root, Some(&top), budget))
    })
    .await
    .ok()
    .flatten()?;
    let fresh = probe.fresh?;
    remember_version(&fresh, probe.version);
    Some(fresh)
}

/// Is `entry` still about the work tree as it is now? Takes the fingerprint within `budget`
/// (no file content is read).
pub async fn freshness(entry: &Entry, budget: Duration) -> Freshness {
    let root = entry.root.clone();
    let now = tokio::time::timeout(
        budget,
        tokio::task::spawn_blocking(move || {
            let top = workspace::toplevel(&root, budget)?;
            workspace::probe_fresh(&root, Some(&top), budget).fresh
        }),
    )
    .await;
    match now {
        Ok(Ok(Some(now))) if !entry.fresh.is_empty() && now == entry.fresh => Freshness::Current,
        Ok(Ok(Some(_))) => Freshness::Stale,
        _ => Freshness::Unknown,
    }
}

/// One line for a person or a model: the verdict and whether it is still current.
pub fn describe(entry: &Entry, freshness: Freshness, now: u64) -> String {
    let age = now.saturating_sub(entry.at);
    let when = if age < 120 {
        format!("{age} s ago")
    } else if age < 7_200 {
        format!("{} min ago", age / 60)
    } else {
        format!("{} h ago", age / 3_600)
    };
    // "skip" is the tier a person's skip is kept under (cpp-tools `skip::TIER`).
    let tier = match entry.tier.as_str() {
        "full" => "full check",
        "skip" => "skipped by a person",
        _ => "fast check",
    };
    let state = match freshness {
        Freshness::Current => "current: the code has not changed since".to_string(),
        Freshness::Stale => "STALE: the code changed since, so this is the result for an older \
                             version, not for the code as it is now"
            .to_string(),
        Freshness::Unknown => "freshness unknown, re-checking".to_string(),
    };
    format!("{} ({tier} {when}, {state}).", entry.result.render_line())
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
