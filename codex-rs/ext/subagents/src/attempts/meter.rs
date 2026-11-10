//! Resource meters for Tier B: tokens of the attempt and judge sub-agents (fed by the
//! token-usage hook), memory headroom and free disk.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

fn tokens() -> &'static Mutex<HashMap<String, u64>> {
    static TOKENS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start counting tokens of a sub-agent thread.
pub fn track(thread_id: &str) {
    if let Ok(mut map) = tokens().lock() {
        map.entry(thread_id.to_string()).or_insert(0);
    }
}

/// Latest cumulative usage of a tracked thread (input + output tokens). Untracked threads are
/// ignored.
pub fn record(thread_id: &str, total: u64) {
    if let Ok(mut map) = tokens().lock()
        && let Some(v) = map.get_mut(thread_id)
    {
        *v = total;
    }
}

pub fn used(thread_id: &str) -> u64 {
    tokens()
        .lock()
        .ok()
        .and_then(|m| m.get(thread_id).copied())
        .unwrap_or(0)
}

pub fn sum(ids: &[String]) -> u64 {
    ids.iter().map(|id| used(id)).sum()
}

pub fn forget(ids: &[String]) {
    if let Ok(mut map) = tokens().lock() {
        for id in ids {
            map.remove(id);
        }
    }
}

/// When the process (the extension host) started, as near as the extension can tell.
pub fn process_start() -> Option<Instant> {
    PROCESS_START.get().copied()
}

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub fn note_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

/// Bytes of memory still available to this process: the cgroup v2 limit minus its usage (not
/// counting reclaimable inactive page cache) when a limit is set, else `MemAvailable`.
pub fn mem_headroom() -> Option<u64> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    if let (Some(max), Some(cur)) = (read("/sys/fs/cgroup/memory.max"), read("/sys/fs/cgroup/memory.current"))
        && let (Ok(max), Ok(cur)) = (max.trim().parse::<u64>(), cur.trim().parse::<u64>())
    {
        let inactive_file = read("/sys/fs/cgroup/memory.stat")
            .and_then(|stat| {
                stat.lines()
                    .find_map(|l| l.strip_prefix("inactive_file "))
                    .and_then(|v| v.trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        return Some(max.saturating_sub(cur.saturating_sub(inactive_file)));
    }
    let info = read("/proc/meminfo")?;
    info.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// Free bytes on the filesystem holding `path` (`None` where it cannot be measured, e.g. Windows).
#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Free bytes on the filesystem holding `path`.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct we own; `c` is a valid C string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (rc == 0).then(|| (st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tracked_threads_count() {
        record("tierb-untracked", 99);
        assert_eq!(used("tierb-untracked"), 0);
        track("tierb-a");
        track("tierb-b");
        record("tierb-a", 1_000);
        record("tierb-b", 500);
        record("tierb-a", 1_500);
        let ids = vec!["tierb-a".to_string(), "tierb-b".to_string()];
        assert_eq!(sum(&ids), 2_000);
        forget(&ids);
        assert_eq!(sum(&ids), 0);
    }

    #[test]
    fn resources_are_readable_here() {
        assert!(mem_headroom().is_some_and(|b| b > 0));
        assert!(free_bytes(Path::new("/")).is_some());
    }
}
