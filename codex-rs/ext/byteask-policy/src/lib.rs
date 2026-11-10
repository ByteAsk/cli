//! The protected local policy file, `$BYTEASK_HOME/policy.toml`.
//!
//! One home for the settings that must never be written by the launcher, by a
//! repository, or by anything the AI can write:
//!
//! * the per-project sandbox relax setting,
//! * the per-project opt-in to send source to public sites,
//! * the debugger host policy,
//! * the toolbox's 7-day plan cache.
//!
//! The rules this module enforces:
//!
//! * **One writer.** Only the functions in this crate write the file. The
//!   `byteask policy` command and the toolbox's plan refresh call the same
//!   writer. The launcher rewrites `config.toml` and keeps only `[features]`
//!   when it does, which is why these settings do not live there.
//! * **Every problem means every safe default.** A missing, unreadable,
//!   malformed or tampered file gives: nothing relaxed, nothing opted in, no
//!   debugger host allowed, no cached plan. The reason is returned in
//!   [`Loaded::problems`] so the caller can say it out loud.
//! * **Nobody else can loosen it.** A file owned by another user, or writable by
//!   other users, is ignored entirely. A file that sits inside a workspace the
//!   AI can write cannot relax anything (its plan cache is still read: that is
//!   an honest-user convenience, not a security control).
//! * **Writes are atomic and keep what they do not understand.** Temp file +
//!   rename, mode 0600, serialized across processes by a lock file; sections a
//!   newer engine wrote survive an older engine's update.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use toml_edit::DocumentMut;

/// File name inside `$BYTEASK_HOME`.
pub const FILE_NAME: &str = "policy.toml";
const LOCK_NAME: &str = "policy.toml.lock";
/// Where an unreadable file is moved before a write replaces it, so nothing the
/// user typed is silently destroyed.
pub const CORRUPT_BACKUP_NAME: &str = "policy.toml.corrupt";
/// Schema version this engine writes. Readers accept any version and read the
/// keys they know.
pub const VERSION: i64 = 1;
/// A policy file is a few hundred bytes; anything this large is not ours.
const MAX_FILE_BYTES: u64 = 1 << 20;

const HEADER: &str = "# ByteAsk policy. Written only by `byteask policy`; do not edit while ByteAsk runs.\n\
# Settings here can relax the sandbox, so they are never taken from a repository.\n";

/// The effective policy. `Default` is every safe default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// The last plan the gateway reported. `None` = never checked.
    pub plan: Option<PlanCache>,
    /// Projects whose sandbox the user relaxed. Absolute paths.
    pub relaxed_projects: Vec<PathBuf>,
    /// Projects allowed to send source to public sites. Absolute paths.
    pub upload_projects: Vec<PathBuf>,
    /// Hosts a debugger may attach to. Empty = deny every remote host.
    pub debugger_hosts: Vec<String>,
}

impl Policy {
    /// Is `project` (or a directory above it) relaxed?
    pub fn is_relaxed(&self, project: &Path) -> bool {
        self.relaxed_projects
            .iter()
            .any(|relaxed| project.starts_with(relaxed))
    }
}

/// The toolbox's cached view of the signed-in account's plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCache {
    /// Plan name exactly as the gateway reported it (`free`, `pro`, `max`, ...).
    pub plan: String,
    /// Unix seconds of the last successful check.
    pub checked_at: u64,
    /// Fingerprint of the account the plan belongs to, never the address itself.
    pub account: String,
    /// The last served value of the toolbox plan-gate switch.
    pub gate: bool,
}

/// The result of reading the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub policy: Policy,
    /// Plain-language reasons settings were ignored. Empty when the file was
    /// absent or read cleanly.
    pub problems: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFile {
    #[serde(default)]
    plan: Option<PlanCache>,
    #[serde(default)]
    sandbox: RawSandbox,
    #[serde(default)]
    uploads: RawUploads,
    #[serde(default)]
    debugger: RawDebugger,
}

#[derive(Debug, Default, Deserialize)]
struct RawSandbox {
    #[serde(default)]
    relaxed_projects: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUploads {
    #[serde(default)]
    allowed_projects: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDebugger {
    #[serde(default)]
    allowed_hosts: Vec<String>,
}

/// `$BYTEASK_HOME/policy.toml`.
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

/// Read the policy for a session whose AI can write under `workspace_roots`.
///
/// Never fails: every problem degrades to the safe default and is reported in
/// [`Loaded::problems`].
pub fn load(home: &Path, workspace_roots: &[PathBuf]) -> Loaded {
    let file = path(home);
    let mut problems = Vec::new();
    let text = match read_trusted(&file) {
        Ok(Some(text)) => text,
        Ok(None) => return Loaded::default(),
        Err(problem) => {
            problems.push(problem);
            return Loaded {
                policy: Policy::default(),
                problems,
            };
        }
    };
    let raw: RawFile = match toml::from_str(&text) {
        Ok(raw) => raw,
        Err(err) => {
            problems.push(format!(
                "{} could not be read ({}), so every setting in it is at its safe default. \
                 Fix it with `byteask policy reset`.",
                file.display(),
                first_line(&err.to_string())
            ));
            return Loaded {
                policy: Policy::default(),
                problems,
            };
        }
    };

    let mut policy = Policy {
        plan: raw.plan,
        relaxed_projects: absolute_paths(
            raw.sandbox.relaxed_projects,
            "sandbox.relaxed_projects",
            &mut problems,
        ),
        upload_projects: absolute_paths(
            raw.uploads.allowed_projects,
            "uploads.allowed_projects",
            &mut problems,
        ),
        debugger_hosts: raw
            .debugger
            .allowed_hosts
            .into_iter()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty())
            .collect(),
    };

    if let Some(root) = writable_root_containing(&file, workspace_roots) {
        let relaxes_something = !policy.relaxed_projects.is_empty()
            || !policy.upload_projects.is_empty()
            || !policy.debugger_hosts.is_empty();
        policy.relaxed_projects.clear();
        policy.upload_projects.clear();
        policy.debugger_hosts.clear();
        if relaxes_something {
            problems.push(format!(
                "{} is inside {}, which the AI can write, so its sandbox, upload and debugger \
                 settings are ignored. Keep BYTEASK_HOME outside your projects.",
                file.display(),
                root.display()
            ));
        }
    }
    Loaded { policy, problems }
}

/// Read the file only if it is safe to trust. `Ok(None)` = no file.
fn read_trusted(file: &Path) -> Result<Option<String>, String> {
    let meta = match fs::metadata(file) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "{} could not be opened ({err}), so every setting is at its safe default.",
                file.display()
            ));
        }
    };
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file, so every setting is at its safe default.",
            file.display()
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is {} bytes, far larger than a policy file, so it is ignored.",
            file.display(),
            meta.len()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err(format!(
                "{} belongs to another user, so it is ignored and every setting is at its safe \
                 default. Remove it, then set what you need with `byteask policy`.",
                file.display()
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} can be changed by other users, so it is ignored and every setting is at its \
                 safe default. Fix it with `chmod 600 {}`.",
                file.display(),
                file.display()
            ));
        }
    }
    fs::read_to_string(file).map(Some).map_err(|err| {
        format!(
            "{} could not be read ({err}), so every setting is at its safe default.",
            file.display()
        )
    })
}

fn absolute_paths(entries: Vec<String>, key: &str, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in entries {
        let path = PathBuf::from(entry.trim());
        if path.is_absolute() {
            out.push(path);
        } else {
            problems.push(format!(
                "{key} entry {entry:?} is not an absolute path, so it is ignored."
            ));
        }
    }
    out
}

/// The first workspace root that contains the policy file, comparing resolved
/// paths so a symlink cannot hide the file's real location.
fn writable_root_containing(file: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let real = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    roots.iter().find_map(|root| {
        let root_real = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        (real.starts_with(&root_real) || file.starts_with(root)).then_some(root_real)
    })
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Store the toolbox's plan cache.
pub fn write_plan_cache(home: &Path, cache: &PlanCache) -> io::Result<()> {
    update(home, |doc| {
        let mut table = toml_edit::Table::new();
        table["plan"] = toml_edit::value(cache.plan.as_str());
        table["checked_at"] = toml_edit::value(i64::try_from(cache.checked_at).unwrap_or(i64::MAX));
        table["account"] = toml_edit::value(cache.account.as_str());
        table["gate"] = toml_edit::value(cache.gate);
        doc["plan"] = toml_edit::Item::Table(table);
    })
}

/// Turn the tool sandbox off (`relaxed`) or back on for `project`.
/// The path is stored resolved, so a symlink cannot widen it later; turning
/// it back on removes the entry and any entry inside it.
///
/// Only `byteask policy` calls this, and that command refuses to relax from
/// inside an agent session.
pub fn set_relaxed(home: &Path, project: &Path, relaxed: bool) -> io::Result<PathBuf> {
    let project = fs::canonicalize(project)?;
    if !project.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", project.display()),
        ));
    }
    let wanted = project.to_string_lossy().into_owned();
    update(home, |doc| {
        // A `sandbox` that is not a table was already read as corrupt (all defaults).
        if !doc.get("sandbox").is_some_and(toml_edit::Item::is_table_like) {
            doc["sandbox"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let list = doc
            .get("sandbox")
            .and_then(|sandbox| sandbox.get("relaxed_projects"))
            .and_then(toml_edit::Item::as_array)
            .cloned()
            .unwrap_or_default();
        let mut kept = toml_edit::Array::new();
        for entry in list.iter() {
            let Some(text) = entry.as_str() else {
                continue;
            };
            let same_or_inside = Path::new(text).starts_with(&project);
            if relaxed && text == wanted {
                continue;
            }
            if !relaxed && same_or_inside {
                continue;
            }
            kept.push(text);
        }
        if relaxed {
            kept.push(wanted.as_str());
        }
        doc["sandbox"]["relaxed_projects"] = toml_edit::value(kept);
    })?;
    Ok(project)
}

/// Forget the plan cache, e.g. on sign-out.
pub fn clear_plan_cache(home: &Path) -> io::Result<()> {
    update(home, |doc| {
        doc.remove("plan");
    })
}

/// Move the file aside (to [`CORRUPT_BACKUP_NAME`]) so every setting returns to
/// its safe default. Returns whether there was a file.
pub fn reset(home: &Path) -> io::Result<bool> {
    let _lock = lock(home)?;
    let file = path(home);
    match fs::rename(&file, home.join(CORRUPT_BACKUP_NAME)) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Apply `edit` to the file under the cross-process lock and write it back
/// atomically. Keys and sections `edit` does not touch are kept as they were.
pub fn update(home: &Path, edit: impl FnOnce(&mut DocumentMut)) -> io::Result<()> {
    fs::create_dir_all(home)?;
    let _lock = lock(home)?;
    let file = path(home);
    let mut doc = match fs::read_to_string(&file) {
        Ok(text) => match text.parse::<DocumentMut>() {
            Ok(doc) => doc,
            Err(_) => {
                // Keep what the user had: an unreadable file was already being
                // treated as all-defaults, so starting fresh loosens nothing.
                fs::rename(&file, home.join(CORRUPT_BACKUP_NAME))?;
                fresh_document()
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => fresh_document(),
        Err(err) => return Err(err),
    };
    edit(&mut doc);
    if !doc.contains_key("version") {
        doc["version"] = toml_edit::value(VERSION);
    }
    write_atomic(home, &file, doc.to_string().as_bytes())
}

fn fresh_document() -> DocumentMut {
    let mut doc = DocumentMut::new();
    doc["version"] = toml_edit::value(VERSION);
    doc.decor_mut().set_prefix(HEADER);
    doc
}

/// Held for the duration of one read-modify-write.
struct LockGuard(fs::File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock(home: &Path) -> io::Result<LockGuard> {
    fs::create_dir_all(home)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(home.join(LOCK_NAME))?;
    file.lock()?;
    Ok(LockGuard(file))
}

fn write_atomic(home: &Path, file: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = home.join(format!("{FILE_NAME}.tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut out = options.open(&tmp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        drop(out);
        #[cfg(unix)]
        {
            // `mode` above applies only when the file is created; an old temp
            // file left by a crash keeps its mode, so set it explicitly.
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, file)?;
        #[cfg(unix)]
        if let Ok(dir) = fs::File::open(home) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests;

mod results_store;
pub use results_store::ResultsStoreLimits;
pub use results_store::results_store_limits;

mod network;
mod sandbox_required;
pub use sandbox_required::sandbox_required;
pub use network::LOCAL_ONLY_FIX;
pub use network::local_only;
pub use network::local_only_summary;
pub use network::set_local_only;
