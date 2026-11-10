//! Which repository, branch and version of the code a result belongs to, and
//! whether a stored result is still current.
//!
//! One `git status --porcelain=v2 --branch -z` gives the commit, the branch,
//! whether its upstream already holds the commit, and every changed or new
//! file. From it the probe computes two different identities, each erring in
//! its own safe direction:
//!
//! * the **freshness fingerprint** (`check_status`, the prompt hook):
//!   the commit, the status records and the size and modification time of
//!   every listed path. Anything touched reads as changed, so a result is shown
//!   as current only when nothing could have changed (it errs towards STALE);
//! * the **version** (the value report): a SHA-256 over the commit and the
//!   content of each changed and new file, so two results share a version
//!   exactly when they saw the same code; touching a file or rebuilding is not
//!   a new version (it errs towards "same code").
//!
//! Neither is the content key (the finish gate computes that one and stores
//! it in the result itself).
//!
//! Git runs with `GIT_OPTIONAL_LOCKS=0` so a probe never takes the index lock
//! from under the user's own git commands, and the whole probe has a time
//! budget: a repository too large to read in time gives no version (the
//! result is still counted, just not tied to a version).

use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use sha2::Digest as _;
use sha2::Sha256;

/// How long one probe may take.
pub const PROBE_BUDGET: Duration = Duration::from_secs(2);
/// At most this many changed files are read for the version; the rest count
/// by path, size and modification time.
const MAX_HASHED_FILES: usize = 2_000;
/// Larger files count by size and modification time.
const MAX_HASHED_BYTES: u64 = 8 << 20;

/// What a probe found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The repository's top directory, or the directory itself when it is not a repository.
    pub repo: PathBuf,
    /// The branch, `(detached)`, or empty when not a repository.
    pub branch: String,
    /// The checked-out commit, if there is one.
    pub head: Option<String>,
    /// A git repository was found.
    pub git: bool,
    /// No tracked file differs from the commit.
    pub tracked_clean: bool,
    /// The branch has an upstream and the commit is already on it.
    pub pushed: bool,
    /// The version of the code, `None` when it could not be read in time, was
    /// not asked for, or there is no repository.
    pub version: Option<String>,
    /// The freshness fingerprint, `None` when git did not answer in time
    /// or there is no repository.
    pub fresh: Option<String>,
}

/// Run git with a deadline. `None` on failure, non-zero exit or timeout.
fn git(dir: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // An answer first seen after the deadline is late and is not used: the budget
                // is a promise to the caller, and without this a zero budget still returned a
                // version whenever git happened to finish before the first check (a busy
                // machine made that test fail about 1 run in 30).
                let in_time = Instant::now() <= deadline;
                let out = reader.join().ok()?;
                return (status.success() && in_time).then_some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
}

/// The repository's top directory, if `dir` is inside one.
pub fn toplevel(dir: &Path, budget: Duration) -> Option<PathBuf> {
    let out = git(
        dir,
        &["rev-parse", "--show-toplevel"],
        Instant::now() + budget,
    )?;
    let text = String::from_utf8(out).ok()?;
    let top = text.trim_end_matches(['\n', '\r']);
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// Probe `dir`, whose repository top is `top` (from [`toplevel`]; `None` =
/// not a repository): freshness and version.
pub fn probe(dir: &Path, top: Option<&Path>, budget: Duration) -> Probe {
    probe_with(dir, top, budget, true)
}

/// [`probe`] without the version (no file is read): for readers that only ask
/// "is this result still current", within a tight budget.
pub fn probe_fresh(dir: &Path, top: Option<&Path>, budget: Duration) -> Probe {
    probe_with(dir, top, budget, false)
}

fn probe_with(dir: &Path, top: Option<&Path>, budget: Duration, with_version: bool) -> Probe {
    let Some(top) = top else {
        return Probe {
            repo: dir.to_path_buf(),
            ..Default::default()
        };
    };
    let deadline = Instant::now() + budget;
    let mut found = Probe {
        repo: top.to_path_buf(),
        git: true,
        ..Default::default()
    };
    let Some(out) = git(
        top,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        deadline,
    ) else {
        return found;
    };

    let mut ahead: Option<u64> = None;
    let mut upstream = false;
    // Every path whose working-tree content differs from the commit, and
    // whether it is new (untracked). Staging state is left out of the version
    // on purpose: `git add` does not change the code.
    let mut changed: Vec<(String, bool)> = Vec::new();
    // The status records themselves (without the branch headers, so a fetch
    // that moves the upstream does not make results stale).
    let mut records: Vec<String> = Vec::new();
    let mut tracked_changes = 0usize;
    let mut fields = out.split(|byte| *byte == 0);
    while let Some(raw) = fields.next() {
        let line = String::from_utf8_lossy(raw).into_owned();
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            found.head = (oid != "(initial)").then(|| oid.to_string());
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            found.branch = head.to_string();
        } else if line.starts_with("# branch.upstream ") {
            upstream = true;
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            ahead = ab
                .split(' ')
                .next()
                .and_then(|plus| plus.strip_prefix('+'))
                .and_then(|count| count.parse().ok());
        } else if line.starts_with("1 ") {
            tracked_changes += 1;
            changed.push((nth_field_rest(&line, 8), false));
            records.push(line);
        } else if line.starts_with("u ") {
            tracked_changes += 1;
            changed.push((nth_field_rest(&line, 10), false));
            records.push(line);
        } else if line.starts_with("2 ") {
            tracked_changes += 1;
            changed.push((nth_field_rest(&line, 9), false));
            // The original path follows as its own NUL-separated field.
            let original = fields
                .next()
                .map(|original| String::from_utf8_lossy(original).into_owned())
                .unwrap_or_default();
            records.push(format!("{line}\t{original}"));
            if !original.is_empty() {
                changed.push((original, false));
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            changed.push((path.to_string(), true));
            records.push(line);
        }
    }
    found.tracked_clean = tracked_changes == 0;
    found.pushed = upstream && ahead == Some(0);
    changed.sort();
    changed.dedup();

    // The freshness fingerprint (stat only, no file is read).
    let mut fresh = Sha256::new();
    fresh.update(b"byteask-fresh-1\0");
    fresh.update(found.head.as_deref().unwrap_or("(none)").as_bytes());
    fresh.update(b"\0");
    for record in &records {
        fresh.update(record.as_bytes());
        fresh.update(b"\0");
    }
    for (path, _) in &changed {
        let stat = std::fs::symlink_metadata(top.join(path))
            .map(|meta| size_and_time(&meta))
            .unwrap_or_else(|_| "missing".to_string());
        fresh.update(format!("{path}:{stat};").as_bytes());
    }
    found.fresh = Some(hex(&fresh.finalize()));
    if !with_version {
        return found;
    }

    let mut hasher = Sha256::new();
    hasher.update(b"byteask-version-1\0");
    hasher.update(found.head.as_deref().unwrap_or("(none)").as_bytes());
    hasher.update(b"\0");
    for (index, (path, untracked)) in changed.iter().enumerate() {
        if Instant::now() >= deadline {
            return found;
        }
        let file = top.join(path);
        let content = if *untracked {
            match untracked_content(&file, index) {
                Some(content) => content,
                None => continue, // a build product, not code
            }
        } else {
            file_content(&file, index)
        };
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(content.as_bytes());
        hasher.update(b"\n");
    }
    found.version = Some(hex(&hasher.finalize()));
    found
}

/// The rest of a porcelain line after `n` space-separated fields (the path,
/// which may itself contain spaces).
fn nth_field_rest(line: &str, n: usize) -> String {
    line.splitn(n + 1, ' ')
        .nth(n)
        .unwrap_or_default()
        .to_string()
}

/// A short description of a file's content for the version.
fn file_content(file: &Path, index: usize) -> String {
    let Ok(meta) = std::fs::symlink_metadata(file) else {
        return "deleted".to_string();
    };
    if meta.file_type().is_symlink() {
        return match std::fs::read_link(file) {
            Ok(target) => format!("link:{}", target.display()),
            Err(_) => "link".to_string(),
        };
    }
    if !meta.is_file() {
        return "not a file".to_string();
    }
    if index >= MAX_HASHED_FILES || meta.len() > MAX_HASHED_BYTES {
        return size_and_time(&meta);
    }
    match std::fs::read(file) {
        Ok(bytes) => hex(&Sha256::digest(&bytes)),
        Err(_) => size_and_time(&meta),
    }
}

/// Like [`file_content`] for a new (untracked) file, but `None` for files
/// that are build products rather than code: binary files (a NUL byte in the
/// first 8 KiB) and `compile_commands.json`, which tools regenerate. A build
/// must not look like a new version of the code.
fn untracked_content(file: &Path, index: usize) -> Option<String> {
    if file
        .file_name()
        .is_some_and(|name| name == "compile_commands.json")
    {
        return None;
    }
    if let Ok(mut handle) = std::fs::File::open(file) {
        let mut head = [0u8; 8192];
        let read = handle.read(&mut head).unwrap_or(0);
        if head[..read].contains(&0) {
            return None;
        }
    }
    Some(file_content(file, index))
}

fn size_and_time(meta: &std::fs::Metadata) -> String {
    let nanos = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("size:{}:mtime:{nanos}", meta.len())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
pub(crate) mod tests;
