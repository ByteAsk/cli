//! Writing one event into the change it belongs to, from any process.
//!
//! Several processes write the same session's change: the engine (or the toolbox serving
//! Claude Code) records tool results, and the agent's hooks, each a short process of its
//! own, record its edits and the finish gate's verdicts. So nothing about a change is kept
//! in memory between events: each event takes the change's lock, reads the change's state
//! back from its file (cached by file length, so a busy change is not re-read), decides
//! whether the change continues, and appends one record.
//!
//! Files: `changes/<key>-<n>.jsonl`, where `<key>` is a hash of (repository, branch,
//! session) and `<n>` counts that key's changes (a push or 30 idle days starts the next one).
//! Locks: 256 striped lock files under `changes/locks/`, so none is ever deleted while in use.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use check_result::CheckResult;
use sha2::Digest as _;
use sha2::Sha256;

use crate::store;
use crate::store::CloseHow;
use crate::store::Record;
use crate::workspace::Probe;
use crate::workspace::hex;

/// What happened.
pub enum Event<'a> {
    /// The AI changed files.
    Edit,
    /// A check finished; `answer` is the tool's full text, kept as a disposable payload.
    Result {
        result: &'a CheckResult,
        answer: Option<&'a str>,
    },
}

/// The key of (repository, branch, session).
pub fn change_key(repo: &Path, branch: &str, session: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(branch.as_bytes());
    hasher.update(b"\0");
    hasher.update(session.as_bytes());
    hex(&hasher.finalize())[..16].to_string()
}

/// What a change's file says about it, as far as writing the next event needs.
#[derive(Clone, Debug, Default)]
struct State {
    start_head: Option<String>,
    has_edit: bool,
    has_base: bool,
    last_at: u64,
    results: u64,
    capped: bool,
    closed: bool,
}

fn state_of(log: &store::ChangeLog) -> State {
    let mut state = State {
        last_at: log.last_activity().unwrap_or(0),
        closed: log.closed().is_some(),
        ..Default::default()
    };
    for record in &log.records {
        match record {
            Record::Open { head, .. } => state.start_head = head.clone(),
            Record::Edit { .. } => state.has_edit = true,
            Record::Base { .. } => state.has_base = true,
            Record::Result { .. } => state.results += 1,
            Record::Capped { .. } => state.capped = true,
            Record::Close { .. } | Record::PayloadEvicted { .. } => {}
        }
    }
    state
}

/// Change states by file, with the file length they were read at.
static STATES: Mutex<Option<HashMap<PathBuf, (u64, State)>>> = Mutex::new(None);

fn read_state(path: &Path) -> Option<State> {
    let len = fs::metadata(path).ok()?.len();
    {
        let cache = STATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((seen, state)) = cache.as_ref().and_then(|cache| cache.get(path))
            && *seen == len
        {
            return Some(state.clone());
        }
    }
    let state = state_of(&store::read_change(path).ok()?);
    remember(path, state.clone());
    Some(state)
}

fn remember(path: &Path, state: State) {
    let Ok(len) = fs::metadata(path).map(|meta| meta.len()) else {
        return;
    };
    let mut cache = STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() > 4_096 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (len, state));
}

fn stem(key: &str, n: u64) -> String {
    format!("{key}-{n}")
}

/// The newest change number of `key`, if it has any.
fn latest(changes: &Path, key: &str) -> Option<u64> {
    let prefix = format!("{key}-");
    fs::read_dir(changes)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix(&prefix)?
                .strip_suffix(".jsonl")?
                .parse::<u64>()
                .ok()
        })
        .max()
}

/// Hold `key`'s striped lock.
fn lock(root: &Path, key: &str) -> io::Result<fs::File> {
    let dir = root.join("changes").join("locks");
    store::create_private_dir(root)?;
    store::create_private_dir(&root.join("changes"))?;
    store::create_private_dir(&dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{}.lock", &key[..2.min(key.len())])))?;
    file.lock()?;
    Ok(file)
}

/// How the current change of a key is found or started for one event.
struct Current {
    stem: String,
    state: State,
}

fn current(root: &Path, key: &str, session: &str, probe: &Probe, at: u64) -> io::Result<Current> {
    let changes = root.join("changes");
    let found = latest(&changes, key).and_then(|n| {
        let path = store::change_path(root, &stem(key, n));
        read_state(&path).map(|state| (n, state))
    });
    let mut base = None;
    let next = match &found {
        None => 1,
        Some((n, state)) if state.closed => n + 1,
        Some((n, state)) if at.saturating_sub(state.last_at) > store::IDLE_CLOSE_SECS => {
            // The clean-up may already have closed it; a second close is harmless.
            close(root, &stem(key, *n), at, CloseHow::Idle)?;
            n + 1
        }
        Some((n, state))
            if state.has_edit
                && probe.head.is_some()
                && probe.head != state.start_head
                && probe.tracked_clean
                && probe.pushed =>
        {
            // The change left the session when its commits reached the upstream. The
            // code as pushed is the next change's starting point.
            close(root, &stem(key, *n), at, CloseHow::Pushed)?;
            base = Some(probe.version.clone());
            n + 1
        }
        Some((n, state)) => {
            return Ok(Current {
                stem: stem(key, *n),
                state: state.clone(),
            });
        }
    };
    let stem = stem(key, next);
    store::append(
        root,
        &stem,
        &Record::Open {
            at,
            repo: probe.repo.to_string_lossy().into_owned(),
            branch: probe.branch.clone(),
            session: session.to_string(),
            head: probe.head.clone(),
        },
    )?;
    let mut state = State {
        start_head: probe.head.clone(),
        last_at: at,
        ..Default::default()
    };
    if let Some(version) = base {
        store::append(root, &stem, &Record::Base { at, version })?;
        state.has_base = true;
    }
    Ok(Current { stem, state })
}

fn close(root: &Path, stem: &str, at: u64, how: CloseHow) -> io::Result<()> {
    store::append(root, stem, &Record::Close { at, how })?;
    let path = store::change_path(root, stem);
    if let Some(mut state) = read_state(&path) {
        state.closed = true;
        remember(&path, state);
    }
    Ok(())
}

/// Record `event` for `session` in the workspace `probe` saw, at `at`. Returns the change's
/// stem.
pub fn append(
    home: &Path,
    session: &str,
    probe: &Probe,
    at: u64,
    event: Event<'_>,
) -> io::Result<String> {
    append_capped(
        home,
        session,
        probe,
        at,
        event,
        store::MAX_RESULTS_PER_CHANGE,
    )
}

pub(crate) fn append_capped(
    home: &Path,
    session: &str,
    probe: &Probe,
    at: u64,
    event: Event<'_>,
    max_results: u64,
) -> io::Result<String> {
    let root = store::root(home);
    let key = change_key(&probe.repo, &probe.branch, session);
    let _lock = lock(&root, &key)?;
    let Current { stem, mut state } = current(&root, &key, session, probe, at)?;
    match event {
        Event::Edit => {
            store::append(
                &root,
                &stem,
                &Record::Edit {
                    at,
                    version: probe.version.clone(),
                },
            )?;
            state.has_edit = true;
        }
        Event::Result { result, answer } => {
            if state.results >= max_results {
                if !state.capped {
                    store::append(&root, &stem, &Record::Capped { at })?;
                    state.capped = true;
                }
            } else {
                let payload = match answer {
                    Some(text) => {
                        let name = format!("{at}-{}-{}.txt", std::process::id(), state.results + 1);
                        store::write_payload(&root, &stem, &name, text)
                            .ok()
                            .map(|_| name)
                    }
                    None => None,
                };
                store::append(
                    &root,
                    &stem,
                    &Record::Result {
                        at,
                        version: probe.version.clone(),
                        result: result.clone(),
                        payload,
                    },
                )?;
                state.results += 1;
            }
        }
    }
    state.last_at = state.last_at.max(at);
    remember(&store::change_path(&root, &stem), state);
    Ok(stem)
}

/// An agent session started: the code as it is now is the starting point (base version) of
/// its change, so results on that code never count as the change's. A resumed session
/// keeps the base it started with.
pub fn begin_session(home: &Path, session: &str, probe: &Probe, at: u64) -> io::Result<()> {
    let root = store::root(home);
    let key = change_key(&probe.repo, &probe.branch, session);
    let _lock = lock(&root, &key)?;
    let Current { stem, mut state } = current(&root, &key, session, probe, at)?;
    if !state.has_base {
        store::append(
            &root,
            &stem,
            &Record::Base {
                at,
                version: probe.version.clone(),
            },
        )?;
        state.has_base = true;
        remember(&store::change_path(&root, &stem), state);
    }
    Ok(())
}

/// The session ended: close its change for each of `keys` that is still open.
pub fn end_session(home: &Path, keys: &[String], at: u64) {
    let root = store::root(home);
    for key in keys {
        let Ok(_lock) = lock(&root, key) else {
            continue;
        };
        let Some(n) = latest(&root.join("changes"), key) else {
            continue;
        };
        let stem = stem(key, n);
        if read_state(&store::change_path(&root, &stem)).is_some_and(|state| !state.closed) {
            let _ = close(&root, &stem, at, CloseHow::SessionEnded);
        }
    }
}

#[cfg(test)]
#[path = "changes_tests.rs"]
mod tests;
