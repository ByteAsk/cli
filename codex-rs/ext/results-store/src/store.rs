//! The files of the results store, their records, and the clean-up that keeps
//! the store inside its limits. See the crate docs for the layout.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use byteask_policy::ResultsStoreLimits;
use check_result::CheckResult;
use serde::Deserialize;
use serde::Serialize;

/// Directory under `$BYTEASK_HOME`.
pub const DIR_NAME: &str = "results";
const CHANGES: &str = "changes";
const PAYLOADS: &str = "payloads";
const GC_LOCK: &str = "gc.lock";
const GC_STAMP: &str = "gc.stamp";
/// How many times (10 ms apart) a clean-up retries a busy lock.
const GC_LOCK_TRIES: u32 = 20;

/// A change with no activity for this long closes.
pub const IDLE_CLOSE_SECS: u64 = 30 * 86_400;
/// At most this many closed changes are kept; the oldest go first (the count cap).
pub const MAX_CLOSED_CHANGES: usize = 10_000;
/// A change stops recording results after this many (a runaway loop must not
/// fill the disk); it is marked `capped` and the report says it is incomplete.
pub const MAX_RESULTS_PER_CHANGE: u64 = 50_000;
/// One stored tool answer is cut at this size.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// `$BYTEASK_HOME/results`.
pub fn root(home: &Path) -> PathBuf {
    home.join(DIR_NAME)
}

/// Why a change closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseHow {
    /// Its commits reached the branch's upstream (it left the session).
    Pushed,
    /// The session that owned it ended.
    SessionEnded,
    /// No activity for [`IDLE_CLOSE_SECS`].
    Idle,
}

/// One line of a change's evidence log. Fields are a contract: add, never rename.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Open {
        at: u64,
        repo: String,
        branch: String,
        session: String,
        /// The commit checked out when the change opened (`None`: no commit yet, or not a repository).
        #[serde(default)]
        head: Option<String>,
    },
    /// The code as it was before the AI changed anything in this change (an agent session
    /// starting, or the code just pushed): results on this version are never the change's.
    Base {
        at: u64,
        #[serde(default)]
        version: Option<String>,
    },
    /// The AI changed files (its edit tool finished, or the agent's Stop hook first saw its
    /// changes, timed at the start of that turn).
    Edit {
        at: u64,
        #[serde(default)]
        version: Option<String>,
    },
    /// One check result and the version of the code it checked.
    Result {
        at: u64,
        #[serde(default)]
        version: Option<String>,
        result: CheckResult,
        /// File name of the stored tool answer under `payloads/<stem>/`.
        #[serde(default)]
        payload: Option<String>,
    },
    Close {
        at: u64,
        how: CloseHow,
    },
    /// Some of this change's payloads were deleted to stay under the cap.
    PayloadEvicted {
        at: u64,
    },
    /// [`MAX_RESULTS_PER_CHANGE`] was reached; later results were not recorded.
    Capped {
        at: u64,
    },
}

impl Record {
    pub fn at(&self) -> u64 {
        match self {
            Record::Open { at, .. }
            | Record::Base { at, .. }
            | Record::Edit { at, .. }
            | Record::Result { at, .. }
            | Record::Close { at, .. }
            | Record::PayloadEvicted { at }
            | Record::Capped { at } => *at,
        }
    }
}

/// Create one directory level (never its parents: a missing `$BYTEASK_HOME`
/// is not re-created by recording), mode 0700.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn open_private(path: &Path, append: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// The evidence log of change `stem`.
pub fn change_path(root: &Path, stem: &str) -> PathBuf {
    root.join(CHANGES).join(format!("{stem}.jsonl"))
}

fn payload_dir(root: &Path, stem: &str) -> PathBuf {
    root.join(PAYLOADS).join(stem)
}

/// Append one record to a change's log: the record and its newline in ONE
/// write, so concurrent appends cannot interleave.
pub fn append(root: &Path, stem: &str, record: &Record) -> io::Result<()> {
    let dir = root.join(CHANGES);
    create_private_dir(root)?;
    create_private_dir(&dir)?;
    let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
    line.push('\n');
    open_private(&change_path(root, stem), true)?.write_all(line.as_bytes())
}

/// Store a tool answer for change `stem`; returns its file name. The text is
/// cut at [`MAX_PAYLOAD_BYTES`].
pub fn write_payload(root: &Path, stem: &str, name: &str, text: &str) -> io::Result<u64> {
    let dir = payload_dir(root, stem);
    create_private_dir(root)?;
    create_private_dir(&root.join(PAYLOADS))?;
    create_private_dir(&dir)?;
    let mut end = text.len().min(MAX_PAYLOAD_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = open_private(&dir.join(name), false)?;
    out.write_all(&text.as_bytes()[..end])?;
    if end < text.len() {
        out.write_all(b"\n[cut: the stored copy is limited to 1 MiB]\n")?;
    }
    Ok(end as u64)
}

/// One change as read back.
#[derive(Clone, Debug, Default)]
pub struct ChangeLog {
    pub stem: String,
    /// Records in file order (which is time order: one writer appends them).
    pub records: Vec<Record>,
    /// Lines that could not be read; the earliest time near them, if any.
    pub unreadable: Vec<Option<u64>>,
}

impl ChangeLog {
    pub fn opened(&self) -> Option<(u64, &str, &str, &str)> {
        self.records.iter().find_map(|record| match record {
            Record::Open {
                at,
                repo,
                branch,
                session,
                ..
            } => Some((*at, repo.as_str(), branch.as_str(), session.as_str())),
            _ => None,
        })
    }

    /// Latest time of anything that happened in the change (closing and
    /// clean-up records do not count as activity).
    pub fn last_activity(&self) -> Option<u64> {
        self.records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::Open { .. }
                        | Record::Base { .. }
                        | Record::Edit { .. }
                        | Record::Result { .. }
                )
            })
            .map(Record::at)
            .max()
    }

    /// The version the change started from, if one was recorded.
    pub fn base_version(&self) -> Option<&str> {
        self.records.iter().find_map(|record| match record {
            Record::Base { version, .. } => Some(version.as_deref()),
            _ => None,
        })?
    }

    /// The change holds something to count (an edit or a result).
    pub fn has_activity(&self) -> bool {
        self.records
            .iter()
            .any(|record| matches!(record, Record::Edit { .. } | Record::Result { .. }))
    }

    pub fn closed(&self) -> Option<(u64, CloseHow)> {
        self.records.iter().find_map(|record| match record {
            Record::Close { at, how } => Some((*at, *how)),
            _ => None,
        })
    }
}

/// Everything in the store.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub changes: Vec<ChangeLog>,
    /// The store (or a part of it) could not be read at all.
    pub unreadable_files: Vec<String>,
}

impl Loaded {
    /// Unix time of the first known problem: an unreadable record, file or
    /// change. `None` when everything was read.
    pub fn incomplete_since(&self) -> Option<Option<u64>> {
        let mut problems = self
            .changes
            .iter()
            .flat_map(|change| change.unreadable.iter().copied())
            .chain(self.unreadable_files.iter().map(|_| None))
            .peekable();
        problems.peek()?;
        Some(problems.flatten().min())
    }
}

/// Read one change file. A line that does not parse is kept as a problem,
/// never silently dropped (a partial store must read as INCOMPLETE).
pub fn read_change(path: &Path) -> io::Result<ChangeLog> {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = fs::read(path)?;
    let text = String::from_utf8_lossy(&text);
    let mut log = ChangeLog {
        stem,
        ..Default::default()
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(record) => log.records.push(record),
            Err(_) => log.unreadable.push(log.records.last().map(Record::at)),
        }
    }
    // A change whose open record is unreadable cannot be attributed.
    if log.opened().is_none() && log.unreadable.is_empty() && !log.records.is_empty() {
        log.unreadable.push(log.records.first().map(Record::at));
    }
    Ok(log)
}

/// Read every change in the store. A missing store is an empty one.
pub fn load(root: &Path) -> Loaded {
    let mut loaded = Loaded::default();
    let dir = root.join(CHANGES);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return loaded,
        Err(err) => {
            loaded
                .unreadable_files
                .push(format!("{}: {err}", dir.display()));
            return loaded;
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.sort();
    for path in paths {
        match read_change(&path) {
            Ok(log) => loaded.changes.push(log),
            Err(err) => loaded
                .unreadable_files
                .push(format!("{}: {err}", path.display())),
        }
    }
    loaded
}

/// What one clean-up did (for tests and the status line).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcOutcome {
    pub closed_idle: Vec<String>,
    pub deleted: Vec<String>,
    /// Closed changes whose payloads were evicted for the byte cap.
    pub evicted_closed: Vec<String>,
    /// Open (pinned) changes whose payloads were evicted for the byte cap.
    pub evicted_pinned: Vec<String>,
    pub payload_bytes_after: u64,
    /// Finish-gate index entries older than `max_days`.
    pub index_entries_deleted: usize,
    /// Agent-hook session files idle 30 days.
    pub session_files_deleted: usize,
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(fs::Metadata::is_file)
        .map(|meta| meta.len())
        .sum()
}

/// Bring the store inside its limits. Holds `gc.lock`; another
/// process already cleaning up makes this a no-op.
///
/// Order: close idle changes; delete closed changes older than `max_days`;
/// delete the oldest closed changes beyond [`MAX_CLOSED_CHANGES`]; then, while
/// payloads exceed `max_bytes`, evict closed changes' payloads oldest first,
/// then open changes' payloads oldest first, marking each evicted change.
/// Evidence of an open change is never deleted.
pub fn gc(root: &Path, now: u64, limits: &ResultsStoreLimits) -> io::Result<GcOutcome> {
    gc_with(root, now, limits, MAX_CLOSED_CHANGES)
}

fn gc_with(
    root: &Path,
    now: u64,
    limits: &ResultsStoreLimits,
    max_closed: usize,
) -> io::Result<GcOutcome> {
    let mut outcome = GcOutcome::default();
    if !root.exists() {
        return Ok(outcome);
    }
    let lock = open_private(&root.join(GC_LOCK), true)?;
    // A lock can look held for a moment after its holder let it go: a child
    // process forked by another thread keeps a copy of the descriptor until it
    // execs. So wait briefly before deciding another clean-up is running.
    let mut tries = 0;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if tries < GC_LOCK_TRIES => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(outcome),
            Err(fs::TryLockError::Error(err)) => return Err(err),
        }
    }
    let max_age = limits.max_days.saturating_mul(86_400);

    struct Entry {
        stem: String,
        opened: u64,
        last: u64,
        closed: bool,
        active: bool,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for change in load(root).changes {
        let opened = change.opened().map(|(at, ..)| at).unwrap_or(0);
        let last = change.last_activity().unwrap_or(opened);
        let mut closed = change.closed().is_some();
        if !closed && now.saturating_sub(last) > IDLE_CLOSE_SECS {
            append(
                root,
                &change.stem,
                &Record::Close {
                    at: now,
                    how: CloseHow::Idle,
                },
            )?;
            outcome.closed_idle.push(change.stem.clone());
            closed = true;
        }
        let active = change.has_activity();
        entries.push(Entry {
            stem: change.stem,
            opened,
            last,
            closed,
            active,
        });
    }

    let delete = |stem: &str| -> io::Result<()> {
        match fs::remove_file(change_path(root, stem)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        match fs::remove_dir_all(payload_dir(root, stem)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    };

    // Age: closed changes only. A closed change that holds no edit and no result (an agent
    // session that changed nothing) carries nothing to keep.
    let mut kept = Vec::new();
    for entry in entries {
        if entry.closed && (now.saturating_sub(entry.last) > max_age || !entry.active) {
            delete(&entry.stem)?;
            outcome.deleted.push(entry.stem);
        } else {
            kept.push(entry);
        }
    }
    // Count cap: closed changes only, oldest first.
    let closed_count = kept.iter().filter(|entry| entry.closed).count();
    if closed_count > max_closed {
        let mut closed: Vec<(u64, String)> = kept
            .iter()
            .filter(|entry| entry.closed)
            .map(|entry| (entry.last, entry.stem.clone()))
            .collect();
        closed.sort();
        let excess: Vec<String> = closed
            .into_iter()
            .take(closed_count - max_closed)
            .map(|(_, stem)| stem)
            .collect();
        for stem in &excess {
            delete(stem)?;
        }
        kept.retain(|entry| !excess.contains(&entry.stem));
        outcome.deleted.extend(excess);
    }

    // Payload bytes. Directories with no evidence file are orphans: delete.
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    if let Ok(dirs) = fs::read_dir(root.join(PAYLOADS)) {
        for dir in dirs.filter_map(Result::ok) {
            let stem = dir.file_name().to_string_lossy().into_owned();
            if kept.iter().any(|entry| entry.stem == stem) {
                sizes.insert(stem, dir_bytes(&dir.path()));
            } else {
                let _ = fs::remove_dir_all(dir.path());
            }
        }
    }
    let mut total: u64 = sizes.values().sum();
    if total > limits.max_bytes {
        let mut order: Vec<(bool, u64, String)> = kept
            .iter()
            .filter(|entry| sizes.get(&entry.stem).copied().unwrap_or(0) > 0)
            // Closed first (false < true for "open"), then oldest first:
            // closed by last activity, open (pinned) by when they opened.
            .map(|entry| {
                let when = if entry.closed {
                    entry.last
                } else {
                    entry.opened
                };
                (!entry.closed, when, entry.stem.clone())
            })
            .collect();
        order.sort();
        for (open, _, stem) in order {
            if total <= limits.max_bytes {
                break;
            }
            match fs::remove_dir_all(payload_dir(root, &stem)) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            total = total.saturating_sub(sizes.get(&stem).copied().unwrap_or(0));
            append(root, &stem, &Record::PayloadEvicted { at: now })?;
            if open {
                outcome.evicted_pinned.push(stem);
            } else {
                outcome.evicted_closed.push(stem);
            }
        }
    }
    outcome.payload_bytes_after = total;
    prune_projects(root, now, max_age, &mut outcome);
    let _ = open_private(&root.join(GC_STAMP), false);
    Ok(outcome)
}

/// The per-project directories (the finish gate's index and the agent hooks' session
/// files): index entries older than `max_age` go; a session file untouched for
/// [`IDLE_CLOSE_SECS`] goes (its agent session is over).
fn prune_projects(root: &Path, now: u64, max_age: u64, outcome: &mut GcOutcome) {
    let Ok(dirs) = fs::read_dir(root) else {
        return;
    };
    for dir in dirs.filter_map(Result::ok) {
        let name = dir.file_name().to_string_lossy().into_owned();
        if name == CHANGES || name == PAYLOADS || !dir.path().is_dir() {
            continue;
        }
        let index = crate::index::read_index(&dir.path().join("index.json"));
        if index
            .entries
            .iter()
            .any(|entry| now.saturating_sub(entry.at) > max_age)
        {
            crate::index::update_index(&dir.path(), |index| {
                let before = index.entries.len();
                index
                    .entries
                    .retain(|entry| now.saturating_sub(entry.at) <= max_age);
                outcome.index_entries_deleted += before - index.entries.len();
            });
        }
        let Ok(sessions) = fs::read_dir(dir.path().join("sessions")) else {
            continue;
        };
        for file in sessions.filter_map(Result::ok) {
            let idle = file
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() > IDLE_CLOSE_SECS);
            if idle && fs::remove_file(file.path()).is_ok() {
                outcome.session_files_deleted += 1;
            }
        }
    }
}

/// Seconds since the last clean-up, `None` if there never was one.
pub fn since_last_gc(root: &Path) -> Option<u64> {
    let modified = fs::metadata(root.join(GC_STAMP)).ok()?.modified().ok()?;
    modified.elapsed().ok().map(|elapsed| elapsed.as_secs())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
