//! Builds that several agent sessions can share safely.
//!
//! * [`project_lock`]: one build per project at a time across EVERY ByteAsk
//!   process of this user (the CLI, the Claude Code toolbox, the Codex
//!   toolbox). The lock used to be a static inside one process, so two
//!   sessions on one worktree could still run an in-tree `make` at the same
//!   moment and link each other's objects.
//! * [`stable_build_dir`]: an out-of-tree build directory that stays put per
//!   project and build setting, so a second sanitizer build of a project is
//!   incremental instead of a full rebuild in a fresh temp dir. Sessions
//!   on one worktree share it and take turns through the lock.
//!
//! Both live in this user's private state directory ([`state_dir`]), never
//! in the user's repository.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

/// A held project lock. Dropping it releases both halves.
pub(crate) struct ProjectGuard {
    // Fields drop in order: the cross-process lock is released first, then
    // the next task in this process gets in.
    _file: Option<std::fs::File>,
    _local: tokio::sync::OwnedMutexGuard<()>,
}

/// Another session kept the project busy for the whole wait.
#[derive(Debug)]
pub(crate) struct ProjectBusy {
    pub(crate) waited: Duration,
}

impl ProjectBusy {
    /// The sentence a tool puts in its NOT RUN result.
    pub(crate) fn summary(&self) -> String {
        format!(
            "Another ByteAsk session was building this project for the whole {} s this call \
             waited, so nothing was built or checked.",
            self.waited.as_secs()
        )
    }

    pub(crate) const FIX: &'static str = "wait for the other session's build to finish, then call again.";
}

/// How often a waiting call retries the cross-process lock.
const LOCK_POLL: Duration = Duration::from_millis(100);

/// A project's build trees are deleted once no session has used them for this
/// long, so a cache of multi-gigabyte sanitizer builds cannot grow forever.
pub(crate) const BUILD_KEEP: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// File touched each time a project's build trees are used; its age decides
/// [`prune_stale_builds`].
const LAST_USED: &str = ".last-used";

/// Hold this for the whole of a build that writes into a project's own tree
/// (an in-tree `make`, a CMake configure of `build/`) or into the project's
/// stable build directory: one such build per project directory at a time,
/// across every ByteAsk process of this user.
///
/// The model may issue these tools in parallel, and two agent sessions may
/// work on one worktree. Measured: two project-mode sanitizer
/// builds of one Makefile tree linked each other's objects (asan+tsan failed
/// 6/6 with the OTHER sanitizer's symbols) or hit `Text file busy`; three
/// concurrent `cpp_compile_db generate` calls on one CMake project failed
/// "configure_file: No such file or directory" in 3 of 5 rounds. A build in a
/// throwaway tree of our own (a fresh temp dir) needs no lock.
///
/// Waits at most `wait`; `Err` means another session held the project the
/// whole time. A missing state directory or a filesystem without locks falls
/// back to the in-process half alone:
/// a lock we cannot take must never stop a build.
pub(crate) async fn project_lock(dir: &Path, wait: Duration) -> Result<ProjectGuard, ProjectBusy> {
    let started = tokio::time::Instant::now();
    let deadline = started + wait;
    let root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

    let local = tokio::time::timeout_at(deadline, local_lock(&root).lock_owned())
        .await
        .map_err(|_| ProjectBusy {
            waited: started.elapsed(),
        })?;

    let file = state_dir().and_then(|state| open_lock_file_in(&state, &project_key(&root)));
    let Some(file) = file else {
        return Ok(ProjectGuard {
            _file: None,
            _local: local,
        });
    };
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(ProjectGuard {
                    _file: Some(file),
                    _local: local,
                });
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(_)) => {
                return Ok(ProjectGuard {
                    _file: None,
                    _local: local,
                });
            }
        }
        if tokio::time::Instant::now() + LOCK_POLL > deadline {
            return Err(ProjectBusy {
                waited: started.elapsed(),
            });
        }
        tokio::time::sleep(LOCK_POLL).await;
    }
}

/// [`project_lock`] without waiting: `None` when another task or session holds `dir` now.
/// For a caller that must take the lock before it returns (a background build started by the
/// harness, which a later check must find held).
pub(crate) fn try_project_lock(dir: &Path) -> Option<ProjectGuard> {
    let root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let local = local_lock(&root).try_lock_owned().ok()?;
    let file = state_dir().and_then(|state| open_lock_file_in(&state, &project_key(&root)));
    let Some(file) = file else {
        return Some(ProjectGuard {
            _file: None,
            _local: local,
        });
    };
    match file.try_lock() {
        Ok(()) => Some(ProjectGuard {
            _file: Some(file),
            _local: local,
        }),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(_)) => Some(ProjectGuard {
            _file: None,
            _local: local,
        }),
    }
}

/// The in-process half: one async mutex per canonical project root.
fn local_lock(root: &Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    type Locks = std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>;
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<Locks>> = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(root.to_path_buf())
        .or_default()
        .clone()
}

fn open_lock_file_in(state: &Path, key: &str) -> Option<std::fs::File> {
    let dir = state.join("locks");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{key}.lock")))
        .ok()
}

/// A stable, out-of-tree build directory for `root` built with `setting`
/// (everything that changes the objects: sanitizer, flags, compilers).
/// `None` when there is no private state directory; the caller then uses a
/// throwaway temp dir as before.
///
/// Must be used under [`project_lock`] for `root`: sessions share it.
pub(crate) fn stable_build_dir(root: &Path, setting: &str) -> Option<PathBuf> {
    stable_build_dir_in(&state_dir()?, root, setting)
}

fn stable_build_dir_in(state: &Path, root: &Path, setting: &str) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let project = state.join("builds").join(project_key(&root));
    std::fs::create_dir_all(&project).ok()?;
    // Best effort: a stamp we cannot write only makes the tree look older.
    let _ = std::fs::File::create(project.join(LAST_USED));
    Some(project.join(setting_key(setting)))
}

/// True when `dir` is one of [`stable_build_dir`]'s trees, which must be kept
/// after a call (the next build reuses it) instead of deleted like a temp dir.
pub(crate) fn is_stable_build_dir(dir: &Path) -> bool {
    state_dir().is_some_and(|state| dir.starts_with(state.join("builds")))
}

/// Delete the build trees of every project (except `keep`'s) that no session
/// has used for [`BUILD_KEEP`]. A project whose lock is held right now is
/// skipped, so a running build is never deleted from under its session.
/// Returns how many projects were removed.
pub(crate) fn prune_stale_builds(keep: &Path, now: SystemTime) -> usize {
    state_dir().map_or(0, |state| prune_stale_builds_in(&state, keep, now))
}

fn prune_stale_builds_in(state: &Path, keep: &Path, now: SystemTime) -> usize {
    let keep_root = std::fs::canonicalize(keep).unwrap_or_else(|_| keep.to_path_buf());
    let keep_key = project_key(&keep_root);
    let Ok(entries) = std::fs::read_dir(state.join("builds")) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let key = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if key == keep_key || !path.is_dir() {
            continue;
        }
        let last_used = std::fs::metadata(path.join(LAST_USED))
            .or_else(|_| std::fs::metadata(&path))
            .and_then(|meta| meta.modified());
        let Ok(last_used) = last_used else {
            continue;
        };
        if now.duration_since(last_used).unwrap_or_default() < BUILD_KEEP {
            continue;
        }
        // Only with the project's lock in hand, never blocking for it.
        let Some(lock) = open_lock_file_in(state, &key) else {
            continue;
        };
        if lock.try_lock().is_ok() && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// `<name>-<hash>`: readable in a directory listing, unique per canonical
/// path. FNV-1a, not std's `DefaultHasher`, because the name must not change
/// between Rust releases or every build would turn cold after an update.
pub(crate) fn project_key(root: &Path) -> String {
    // One definition for the build trees, the results store's index and the agent hooks'
    // session files.
    ::results_store::project_key(root)
}

fn setting_key(setting: &str) -> String {
    let word: String = setting
        .split_whitespace()
        .nth(1)
        .unwrap_or("build")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(16)
        .collect();
    let word = if word.is_empty() { "build".to_string() } else { word };
    format!("{word}-{:016x}", fnv1a64(setting.as_bytes()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// This user's private directory for locks and build trees, created on first
/// use. The platform cache directory (`$XDG_CACHE_HOME` or `~/.cache` on
/// Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows), NOT
/// `$BYTEASK_HOME` and NOT `/tmp`: build trees run to gigabytes, `/tmp` is RAM
/// on several distributions, and backup tools already skip cache directories.
/// Every session of one user resolves the same directory whatever its
/// `BYTEASK_HOME`, so they all see each other's locks.
///
/// `None` when the directory cannot be made private to this user.
pub(crate) fn state_dir() -> Option<PathBuf> {
    let dir = state_dir_path();
    ensure_private_dir(&dir).then_some(dir)
}

#[cfg(not(test))]
fn state_dir_path() -> PathBuf {
    cache_dir_from(|key| std::env::var_os(key))
        .map(|cache| cache.join("byteask"))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("byteask-{}", user_tag())))
}

/// Tests never touch the developer's real cache directory.
#[cfg(test)]
fn state_dir_path() -> PathBuf {
    std::env::temp_dir().join(format!("byteask-test-state-{}", user_tag()))
}

/// The platform cache directory, from the environment `env` describes.
fn cache_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |value: std::ffi::OsString| {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    };
    if cfg!(windows) {
        return env("LOCALAPPDATA").and_then(absolute);
    }
    if cfg!(target_os = "macos") {
        return env("HOME")
            .and_then(absolute)
            .map(|home| home.join("Library").join("Caches"));
    }
    env("XDG_CACHE_HOME")
        .and_then(absolute)
        .or_else(|| env("HOME").and_then(absolute).map(|home| home.join(".cache")))
}

#[cfg(unix)]
fn user_tag() -> String {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(unix))]
fn user_tag() -> String {
    std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(32)
        .collect()
}

/// Create `dir` (and parents) and make sure only this user can use it: it must
/// be a real directory (not a symlink) owned by us, mode 0700. Like ssh, an
/// unsafe directory is refused rather than trusted: anyone who can write it
/// could swap a lock file or plant objects in a build tree.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me {
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_ok();
    }
    true
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok() && dir.is_dir()
}

#[cfg(test)]
#[path = "builds_tests.rs"]
mod tests;
