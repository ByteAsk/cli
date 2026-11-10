//! File handling for a root process that serves an unprivileged caller.
//!
//! The runner runs as root (it must place jobs on isolated CPUs) but its
//! paths and environment come from the caller. Rules, so a caller can never
//! make root read or write a file the caller could not:
//! - files the caller names (reports, the perf-ut file) are written and read
//!   with the caller's effective uid ([`as_caller`]);
//! - root creates its own scratch only inside root-owned, non-writable
//!   directories ([`root_dir`]) and never follows a symlink there;
//! - files the job wrote (samples, perf output) are read only if they are
//!   regular files owned by the job's uid, opened without following links
//!   ([`read_job_file`]);
//! - ownership is changed with `lchown`/`fchown`, never `chown`.

use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::spawn::Creds;

fn cpath(p: &Path) -> Result<CString, String> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("path has a NUL byte: {}", p.display()))
}

/// Run `f` with the caller's effective uid/gid (no-op without creds or when
/// not root). Single-threaded use only: seteuid is process-wide.
pub fn as_caller<T>(creds: Option<&Creds>, f: impl FnOnce() -> T) -> T {
    // SAFETY: plain credential syscalls; restored before returning.
    let root = unsafe { libc::geteuid() } == 0;
    let Some(c) = creds.filter(|_| root) else { return f() };
    unsafe {
        libc::setgroups(c.groups.len(), c.groups.as_ptr());
        libc::setegid(c.gid);
        libc::seteuid(c.uid);
    }
    let out = f();
    unsafe {
        libc::seteuid(0);
        libc::setegid(0);
        libc::setgroups(0, std::ptr::null());
    }
    out
}

/// `path` as a directory owned by root and writable only by root: created
/// (0755) if missing, refused if it is a symlink, someone else's, or
/// group/other-writable. Without root (tests) it must merely be ours.
pub fn root_dir(path: &Path) -> Result<(), String> {
    let me = unsafe { libc::geteuid() };
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c = cpath(path)?;
            // SAFETY: mkdir on a NUL-terminated path; never follows a final symlink.
            if unsafe { libc::mkdir(c.as_ptr(), 0o755) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(format!("mkdir {}: {e}", path.display()));
                }
            }
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let m = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.file_type().is_dir() {
        return Err(format!("{} is not a real directory (a symlink or a file); refusing to use it", path.display()));
    }
    if m.uid() != me {
        return Err(format!("{} belongs to uid {}, not {me}; refusing to use it", path.display(), m.uid()));
    }
    if m.mode() & 0o022 != 0 {
        return Err(format!("{} is writable by others; refusing to use it", path.display()));
    }
    Ok(())
}

/// A fresh directory (must not exist yet) for the job, owned by the job's
/// user, inside a root-owned parent.
pub fn job_dir(path: &Path, creds: Option<&Creds>) -> Result<(), String> {
    let c = cpath(path)?;
    if unsafe { libc::mkdir(c.as_ptr(), 0o755) } != 0 {
        return Err(format!("mkdir {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    if let Some(cr) = creds {
        std::os::unix::fs::lchown(path, Some(cr.uid), Some(cr.gid)).map_err(|e| format!("lchown {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Create (exclusively, no symlink following) a file for root to write and
/// the job to inherit as its stdout/stderr.
pub fn create_root_file(path: &Path, creds: Option<&Creds>) -> Result<std::fs::File, String> {
    let _ = std::fs::remove_file(path);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    if let Some(c) = creds {
        use std::os::fd::AsRawFd;
        // SAFETY: fchown on an owned fd.
        unsafe { libc::fchown(f.as_raw_fd(), c.uid, c.gid) };
    }
    Ok(f)
}

/// Read a file the job wrote: only a regular file owned by the job's user,
/// opened without following a symlink. `Ok(None)` when it does not exist.
pub fn read_job_file(path: &Path, creds: Option<&Creds>) -> Result<Option<Vec<u8>>, String> {
    let f = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e} (not a regular file the benchmark wrote?)", path.display())),
    };
    let m = f.metadata().map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let want = creds.map_or_else(|| unsafe { libc::geteuid() }, |c| c.uid);
    if m.uid() != want {
        return Err(format!("{} is owned by uid {}, not by the benchmark's user; ignoring it", path.display(), m.uid()));
    }
    let mut buf = Vec::with_capacity(m.len() as usize);
    let mut f = f;
    f.read_to_end(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(buf))
}

/// Where one run keeps its files. `run_dir` and `scratch` are root-owned (the
/// caller cannot plant anything there); `trees` (git worktrees, builds) and
/// `job` (what the benchmark writes: samples, perf output) belong to the
/// job's user.
pub struct Workspace {
    pub run_dir: std::path::PathBuf,
    pub trees: std::path::PathBuf,
    pub scratch: std::path::PathBuf,
    pub job: std::path::PathBuf,
}

pub const WORK_ROOT: &str = "/var/tmp/perflab";
pub const SCRATCH_ROOT: &str = "/dev/shm/perflab";

pub fn workspace(stamp: &str, creds: Option<&Creds>) -> Result<Workspace, String> {
    workspace_in(Path::new(WORK_ROOT), Path::new(SCRATCH_ROOT), stamp, creds)
}

pub fn workspace_in(work: &Path, shm: &Path, stamp: &str, creds: Option<&Creds>) -> Result<Workspace, String> {
    root_dir(work)?;
    root_dir(shm)?;
    let run_dir = work.join(stamp);
    let scratch = shm.join(stamp);
    for d in [&run_dir, &scratch] {
        let c = cpath(d)?;
        // A fresh directory, never one that already exists (it could be planted).
        if unsafe { libc::mkdir(c.as_ptr(), 0o755) } != 0 {
            return Err(format!("mkdir {}: {}", d.display(), std::io::Error::last_os_error()));
        }
    }
    let trees = run_dir.join("trees");
    let job = scratch.join("job");
    job_dir(&trees, creds)?;
    job_dir(&job, creds)?;
    Ok(Workspace { run_dir, trees, scratch, job })
}

/// Write a file the caller named, as the caller.
pub fn write_as_caller(path: &str, data: &[u8], creds: Option<&Creds>) -> Result<(), String> {
    as_caller(creds, || std::fs::write(path, data)).map_err(|e| format!("cannot write {path}: {e}"))
}

/// Read a file the caller named, as the caller.
pub fn read_as_caller(path: &Path, creds: Option<&Creds>) -> Result<String, String> {
    as_caller(creds, || std::fs::read_to_string(path)).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("perflab-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn root_dir_refuses_symlinks_and_open_dirs() {
        let d = tmp("rootdir");
        root_dir(&d).unwrap();
        root_dir(&d).unwrap(); // idempotent
        let link = tmp("rootdir-link");
        std::os::unix::fs::symlink(&d, &link).unwrap();
        assert!(root_dir(&link).unwrap_err().contains("not a real directory"));
        let open = tmp("rootdir-open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        assert!(root_dir(&open).unwrap_err().contains("writable by others"));
        for p in [&link] {
            let _ = std::fs::remove_file(p);
        }
        for p in [&d, &open] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn root_files_never_follow_a_planted_symlink() {
        let d = tmp("plant");
        std::fs::create_dir(&d).unwrap();
        let victim = d.join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let out = d.join("run.out");
        std::os::unix::fs::symlink(&victim, &out).unwrap();
        // create_root_file removes the planted name and creates a fresh file.
        let _f = create_root_file(&out, None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(!std::fs::symlink_metadata(&out).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn job_files_are_read_only_when_they_are_real_and_ours() {
        let d = tmp("jobfile");
        std::fs::create_dir(&d).unwrap();
        let p = d.join("samples.bin");
        assert_eq!(read_job_file(&p, None).unwrap(), None);
        std::fs::write(&p, [1u8, 2]).unwrap();
        assert_eq!(read_job_file(&p, None).unwrap(), Some(vec![1, 2]));
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &p).unwrap();
        assert!(read_job_file(&p, None).is_err(), "a symlink must not be followed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn workspace_layout() {
        let (w, s) = (tmp("ws-work"), tmp("ws-shm"));
        let ws = workspace_in(&w, &s, "run1", None).unwrap();
        assert!(ws.trees.starts_with(&ws.run_dir) && ws.job.starts_with(&ws.scratch));
        // The same stamp twice is refused (no reuse of a possibly planted dir).
        assert!(workspace_in(&w, &s, "run1", None).is_err());
        let _ = std::fs::remove_dir_all(&w);
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn job_dir_must_be_new() {
        let d = tmp("jobdir");
        job_dir(&d, None).unwrap();
        assert!(job_dir(&d, None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
