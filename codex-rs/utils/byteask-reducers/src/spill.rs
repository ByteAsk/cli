//! The full original of any output we reduce, kept on disk.
//!
//! Principle: say exactly what was removed and keep the original reachable WITHOUT
//! re-running anything (a migration, a flaky test, a remote call). The spill is written
//! and closed BEFORE the footer that names it is published, so a crash can leave an orphan
//! file but never a dangling reference. If the spill cannot be written, the caller must
//! fall back to the engine's ordinary behaviour for that output: no lower budget without
//! a spill, ever.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Per-process cap on spilled bytes. When reached, new spills are REFUSED (never evict:
/// an evicted file would be a dangling reference in older context).
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Spill files older than this are removed at startup.
pub const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static USED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Choose the spill directory once, create it, and drop files past their age.
pub fn init(dir: &Path) {
    if DIR.get().is_some() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    restrict_dir(dir);
    sweep_old(dir);
    let _ = DIR.set(dir.to_path_buf());
}

pub fn dir() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

fn file_name(call_id: &str) -> String {
    let safe: String = call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(96)
        .collect();
    format!("{}.txt", if safe.is_empty() { "call".to_string() } else { safe })
}

/// Write `bytes` as the original for `call_id` and return its path. Idempotent: if the
/// file already exists with the same length it is reused (the engine may serialise one
/// output more than once). `None` means "could not spill": do not reduce lossily.
pub fn write(call_id: &str, bytes: &[u8]) -> Option<PathBuf> {
    let dir = DIR.get()?;
    // A tee already holds the COMPLETE original (more than `bytes`, which lost its middle
    // to the 1 MiB collection cap). Never overwrite it with the shorter copy.
    let full = dir.join(full_file_name(call_id));
    if full.is_file() {
        return Some(full);
    }
    let path = dir.join(file_name(call_id));
    let len = u64::try_from(bytes.len()).ok()?;
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() == len {
            return Some(path);
        }
    }
    let used = USED.load(std::sync::atomic::Ordering::Relaxed);
    if used.saturating_add(len) > MAX_TOTAL_BYTES {
        return None;
    }
    let tmp = dir.join(format!(".{}.tmp", file_name(call_id)));
    let ok = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        restrict_file(&tmp);
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })()
    .is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    USED.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
    Some(path)
}

fn full_file_name(call_id: &str) -> String {
    let name = file_name(call_id);
    format!("{}.full.txt", name.trim_end_matches(".txt"))
}

/// Resolve a `spill:<id>` handle (or a bare id) to its file. Only ever inside the spill
/// directory: the id is sanitised the same way it was when the file was written.
pub fn resolve(handle: &str) -> Option<PathBuf> {
    let dir = DIR.get()?;
    let id = handle.strip_prefix("spill:").unwrap_or(handle);
    [full_file_name(id), file_name(id)]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

/// Per-output cap on a tee. Past it the tee stops and says so.
pub const TEE_MAX_BYTES: u64 = 64 * 1024 * 1024;

static TEE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A complete copy of one command's output, streamed to disk from the moment the
/// in-memory collector starts dropping its middle. Deleted on drop unless adopted.
#[derive(Debug)]
pub struct Tee {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
    /// Bytes that could not be kept (cap reached, I/O error, or a gap in the source).
    lost: u64,
}

impl Tee {
    /// Start a tee holding `initial`. `None` when there is no spill directory, the
    /// process-wide cap is reached, or the file cannot be created.
    pub fn start(initial: &[u8]) -> Option<Self> {
        let dir = DIR.get()?;
        if USED.load(std::sync::atomic::Ordering::Relaxed) >= MAX_TOTAL_BYTES {
            return None;
        }
        let seq = TEE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(".tee-{}-{seq}.part", std::process::id()));
        let file = std::fs::File::create(&path).ok()?;
        restrict_file(&path);
        let mut tee = Self { path, file: Some(file), written: 0, lost: 0 };
        tee.append(initial);
        Some(tee)
    }

    pub fn append(&mut self, bytes: &[u8]) {
        let len = bytes.len() as u64;
        let room = TEE_MAX_BYTES.saturating_sub(self.written);
        let take = len.min(room);
        let ok = match self.file.as_mut() {
            Some(f) if take > 0 => f.write_all(&bytes[..take as usize]).is_ok(),
            Some(_) => true,
            None => false,
        };
        if ok {
            self.written += take;
            self.lost += len - take;
        } else {
            self.file = None;
            self.lost += len;
        }
    }

    /// Record bytes the source itself no longer had.
    pub fn note_gap(&mut self, bytes: u64) {
        self.lost = self.lost.saturating_add(bytes);
    }

    /// Append another tee's content, consuming it.
    pub fn append_tee(&mut self, mut other: Tee) {
        other.file = None;
        match std::fs::read(&other.path) {
            Ok(bytes) => self.append(&bytes),
            Err(_) => self.lost = self.lost.saturating_add(other.written),
        }
        self.lost = self.lost.saturating_add(other.lost);
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Publish as the original for `call_id`. Commit before publish: the file is synced
    /// and renamed before the caller may name it anywhere.
    pub fn adopt(mut self, call_id: &str) -> Option<PathBuf> {
        let dir = DIR.get()?;
        let file = self.file.take()?;
        file.sync_all().ok()?;
        drop(file);
        let dest = dir.join(full_file_name(call_id));
        std::fs::rename(&self.path, &dest).ok()?;
        USED.fetch_add(self.written, std::sync::atomic::Ordering::Relaxed);
        Some(dest)
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        self.file = None;
        // After a successful adopt() the path no longer exists; this is then a no-op.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sweep_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() > MAX_AGE_SECS);
        let name = e.file_name();
        let stale_part = name.to_string_lossy().ends_with(".part")
            && meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age.as_secs() > 3600);
        if (old || stale_part) && meta.is_file() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}
#[cfg(unix)]
fn restrict_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}
#[cfg(not(unix))]
fn restrict_file(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn file_names_cannot_escape_the_directory() {
        assert_eq!(file_name("../../etc/passwd"), "______etc_passwd.txt");
        assert_eq!(file_name(""), "call.txt");
        assert_eq!(file_name("call_Ab-9"), "call_Ab-9.txt");
    }

    // `init` is process-global, so everything that needs a directory lives in ONE test.
    #[test]
    fn spill_roundtrip_idempotence_and_permissions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(write("before_init", b"x"), None, "no directory yet -> cannot spill");
        init(tmp.path());
        let p = write("call_1", b"the whole original\n").expect("spill");
        assert_eq!(std::fs::read(&p).expect("read"), b"the whole original\n");
        assert_eq!(write("call_1", b"the whole original\n"), Some(p.clone()), "idempotent");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(p.starts_with(tmp.path()));

        // A tee is adopted as the FULL original, and write() never overwrites it.
        let mut tee = Tee::start(b"head-").expect("tee");
        tee.append(b"middle-");
        tee.append(b"tail");
        let part = tee.path.clone();
        assert!(part.is_file());
        let full = tee.adopt("call_tee_1").expect("adopt");
        assert!(!part.exists());
        assert_eq!(std::fs::read(&full).expect("read"), b"head-middle-tail");
        assert_eq!(write("call_tee_1", b"head-tail"), Some(full.clone()));
        assert_eq!(std::fs::read(&full).expect("read"), b"head-middle-tail");
        assert_eq!(resolve("spill:call_tee_1"), Some(full));
        assert_eq!(resolve("spill:call_1"), Some(p));
        assert_eq!(resolve("spill:../../etc/passwd"), None);

        // Two tees concatenate; a dropped tee leaves nothing behind.
        let mut a = Tee::start(b"one-").expect("tee");
        let b = Tee::start(b"two").expect("tee");
        let b_part = b.path.clone();
        a.append_tee(b);
        assert!(!b_part.exists());
        assert_eq!(a.written(), 7);
        let a_part = a.path.clone();
        drop(a);
        assert!(!a_part.exists());
    }
}
