//! The host-wide measurement lock: only one job measures at a time.
//!
//! `flock /var/lock/perf-lab-measure <cmd>` is the convention. The runner
//! takes the lock itself, only around measurement. If it is already held by
//! one of our ancestors (someone wrapped the runner in `flock`), we go on:
//! taking it again would deadlock against our own parent.

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use std::time::Instant;

pub const LOCK_PATH: &str = "/var/lock/perf-lab-measure";

pub struct MeasureLock {
    _file: Option<std::fs::File>,
    pub how: String,
}

/// `PERFLAB_LOCK` (tests) is ignored as root, like every other path override.
fn lock_path() -> String {
    if unsafe { libc::geteuid() } == 0 {
        return LOCK_PATH.to_string();
    }
    std::env::var("PERFLAB_LOCK").unwrap_or_else(|_| LOCK_PATH.to_string())
}

/// Wait while the file named by `PERFLAB_YIELD_TO` exists (another user of
/// the host has priority, e.g. a reserved time slot). Only existence is
/// checked, so the path needs no protection.
pub fn yield_to_priority() -> f64 {
    let Ok(flag) = std::env::var("PERFLAB_YIELD_TO") else { return 0.0 };
    let start = Instant::now();
    let mut said = false;
    while std::path::Path::new(&flag).exists() {
        if !said {
            eprintln!("perf-runner: {flag} exists (a priority job is running); waiting before taking the measurement lock");
            said = true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    start.elapsed().as_secs_f64()
}

pub fn acquire(wait: Duration) -> Result<MeasureLock, String> {
    yield_to_priority();
    let path = lock_path();
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o666)
        .open(&path)
        .or_else(|_| std::fs::File::open(&path))
        .map_err(|e| format!("cannot open the measurement lock {path}: {e}"))?;
    let fd = file.as_raw_fd();
    // SAFETY: flock on an owned fd.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(MeasureLock { _file: Some(file), how: format!("took {path}") });
    }
    let meta = file.metadata().map_err(|e| format!("stat {path}: {e}"))?;
    let holders = holders_of(meta.dev(), meta.ino());
    if let Some(pid) = holders.iter().copied().find(|&p| is_ancestor(p)) {
        return Ok(MeasureLock { _file: None, how: format!("{path} is held by our parent process {pid}") });
    }
    // `exec 9<lock; flock 9` in a shell: /proc/locks names the short-lived
    // flock(1) process, but the shell (our ancestor) keeps the locked open
    // file. An ancestor with the lock file open while it is held = ours.
    // Only when no live process outside our ancestry is the recorded holder:
    // someone else's lock must never be mistaken for ours.
    let foreign = holders.iter().any(|&p| std::path::Path::new(&format!("/proc/{p}")).exists() && !is_ancestor(p));
    if !foreign {
        if let Some(pid) = ancestor_with_open(meta.dev(), meta.ino(), fd) {
            return Ok(MeasureLock { _file: None, how: format!("{path} is held through a descriptor of our parent process {pid}") });
        }
    }
    let who = holders
        .iter()
        .map(|p| format!("pid {p} ({})", cmdline(*p)))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("perf-runner: waiting for the measurement lock {path} (held by {who})");
    let deadline = Instant::now() + wait;
    loop {
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(MeasureLock { _file: Some(file), how: format!("took {path} after waiting") });
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the measurement lock {path} stayed busy for {}s (held by {who}); another job is measuring",
                wait.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// PIDs holding a FLOCK on (dev, ino), from /proc/locks.
fn holders_of(dev: u64, ino: u64) -> Vec<i32> {
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let want = format!("{maj:02x}:{min:02x}:{ino}");
    std::fs::read_to_string("/proc/locks")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.contains("->"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // "1: FLOCK  ADVISORY  WRITE 1234 08:01:5678 0 EOF"
            if f.len() >= 6 && f[1] == "FLOCK" && f[5] == want { f[4].parse().ok() } else { None }
        })
        .collect()
}

fn ppid_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(1)?.parse().ok()
}

fn is_ancestor(pid: i32) -> bool {
    let mut cur = std::process::id() as i32;
    for _ in 0..64 {
        match ppid_of(cur) {
            Some(p) if p == pid => return true,
            Some(p) if p > 1 => cur = p,
            _ => return false,
        }
    }
    false
}

/// The nearest of ourselves (an inherited descriptor, not `own_fd`) and our
/// ancestors with an open descriptor on (dev, ino).
fn ancestor_with_open(dev: u64, ino: u64, own_fd: i32) -> Option<i32> {
    let me = std::process::id() as i32;
    let mut cur = me;
    for _ in 0..64 {
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{cur}/fd")) {
            for fd in fds.flatten() {
                if cur == me && fd.file_name().to_string_lossy() == own_fd.to_string() {
                    continue;
                }
                if let Ok(m) = std::fs::metadata(fd.path()) {
                    if m.dev() == dev && m.ino() == ino {
                        return Some(cur);
                    }
                }
            }
        }
        match ppid_of(cur) {
            Some(p) if p > 1 => cur = p,
            _ => return None,
        }
    }
    None
}

fn cmdline(pid: i32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
        .trim()
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_reentrant_through_a_parent() {
        let dir = std::env::temp_dir().join(format!("perflab-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");
        // SAFETY: tests in this module run in one thread for env changes.
        unsafe { std::env::set_var("PERFLAB_LOCK", &path) };
        let held = acquire(Duration::from_millis(10)).unwrap();
        assert!(held.how.starts_with("took"));
        // A second acquire from the same process (different open file) must
        // wait and then fail: the lock is really held.
        let second = acquire(Duration::from_millis(300));
        assert!(second.is_err(), "lock was not exclusive");
        drop(held);
        assert!(acquire(Duration::from_millis(10)).is_ok());
        // Held by an ancestor (flock(1) wrapping us): we proceed.
        let status = std::process::Command::new("flock")
            .arg(&path)
            .arg("sh")
            .arg("-c")
            .arg("exec \"$0\" --exact lock::tests::child_sees_parent_lock --ignored --nocapture")
            .arg(std::env::current_exe().unwrap())
            .env("PERFLAB_LOCK", &path)
            .status();
        if let Ok(s) = status {
            assert!(s.success(), "child under flock(1) could not proceed");
        }
        // Held through a shell's descriptor (`exec 9<lock; flock 9`): the
        // locking flock(1) process has exited, the shell keeps the lock.
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg("exec 9<\"$1\"; flock 9 || exit 9; \"$0\" --exact lock::tests::child_sees_parent_lock --ignored --nocapture")
            .arg(std::env::current_exe().unwrap())
            .arg(&path)
            .env("PERFLAB_LOCK", &path)
            .status();
        if let Ok(s) = status {
            assert!(s.success(), "child under a shell-held lock could not proceed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn child_sees_parent_lock() {
        let l = acquire(Duration::from_millis(200)).expect("should proceed under parent's lock");
        assert!(l.how.contains("parent"), "{}", l.how);
    }
}
