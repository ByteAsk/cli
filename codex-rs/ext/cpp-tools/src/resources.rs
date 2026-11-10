//! Memory-aware parallelism and the session-wide build lock for every build or test run the
//! engine starts itself (finish gate, `cpp_test_select`, `cpp_sanitize` project mode,
//! `cpp_fix_verify`'s test command, and the sub-agents' engine-run builds).
//!
//! - [`jobs`]: `clamp(1, cpus, floor(available_GB / 1.5))`, read when the build starts. Available
//!   memory is the tightest of the process's cgroup limits (v2 `memory.max` minus
//!   `memory.current`, page cache that can be dropped (`inactive_file`) not counted as used, at
//!   every level up to the cgroup root; v1 `memory.limit_in_bytes` likewise) and the host's
//!   `MemAvailable` from `/proc/meminfo`. Template-heavy C++ translation units need about
//!   1.5 GB each; a parallel build that ignores what is free can take the host down.
//!   `BYTEASK_BUILD_JOBS=<n>` overrides it (still at least 1).
//! - [`build_slot`]: one engine-started build or test run at a time per process (a session), so
//!   the finish gate, a background `cpp_test_select` run and a sub-agent check never compile at
//!   the same time. Callers wait for the slot no longer than their own time budget.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

/// Memory one parallel compile job is assumed to need.
pub(crate) const BYTES_PER_JOB: u64 = 1536 * 1024 * 1024;
/// Explicit override of the job count.
pub(crate) const ENV_JOBS: &str = "BYTEASK_BUILD_JOBS";

/// Parallel jobs for a build or test run started now.
pub fn jobs() -> usize {
    if let Some(n) = std::env::var(ENV_JOBS)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return n.max(1);
    }
    // Inside an automatic check, never more jobs than its spare CPUs.
    crate::priority::clamp_jobs(jobs_for(cpus(), available_memory().map(|m| m.bytes)))
}

/// `clamp(1, cpus, floor(available / 1.5 GiB))`; without a memory reading, the CPU count.
pub(crate) fn jobs_for(cpus: usize, available: Option<u64>) -> usize {
    let cpus = cpus.max(1);
    match available {
        Some(bytes) => usize::try_from(bytes / BYTES_PER_JOB)
            .unwrap_or(usize::MAX)
            .clamp(1, cpus),
        None => cpus,
    }
}

/// For `BYTEASK_PROBE_MEMORY`: the most memory this session may use: the tightest cgroup limit
/// (v2 `memory.max` at every level from the process's cgroup up to the root, v1 `memory.limit_in_bytes`), else
/// the host's `MemTotal`. Unlike [`available_memory`] it does not move between sessions.
pub(crate) fn memory_limit() -> Option<u64> {
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    memory_limit_in(Path::new("/sys/fs/cgroup"), &self_cgroup, &meminfo)
}

/// [`memory_limit`] over an explicit cgroup mount and file contents.
pub(crate) fn memory_limit_in(cgroup_root: &Path, self_cgroup: &str, meminfo: &str) -> Option<u64> {
    let read =
        |path: PathBuf| -> Option<u64> { std::fs::read_to_string(path).ok()?.trim().parse().ok() };
    let mut limits = Vec::new();
    if let Some(rel) = self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
    {
        let mut dir = cgroup_root.join(rel.trim_start_matches('/'));
        loop {
            limits.extend(read(dir.join("memory.max"))); // "max" = no limit at this level
            if dir == cgroup_root || !dir.starts_with(cgroup_root) {
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => break,
            }
        }
    }
    limits.extend(
        read(cgroup_root.join("memory/memory.limit_in_bytes")).filter(|limit| *limit < 1 << 50),
    );
    let total = meminfo
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok());
    limits.extend(total.map(|kb| kb * 1024));
    limits.into_iter().min()
}

/// What a session keeps for itself (the engine, its helpers, the shell) when the job count comes from the limit.
pub(crate) const RESERVED_BYTES: u64 = 1024 * 1024 * 1024;

/// `clamp(1, cpus, floor((limit - 1 GiB) / 1.5 GiB))`: the parallel jobs a session of this size can afford.
pub(crate) fn jobs_for_limit(cpus: usize, limit: u64) -> usize {
    usize::try_from(limit.saturating_sub(RESERVED_BYTES) / BYTES_PER_JOB)
        .unwrap_or(usize::MAX)
        .clamp(1, cpus.max(1))
}

/// CPUs this process may use: the scheduler's answer (affinity and, on current Rust, the
/// cgroup quota) bounded by the cgroup v2 `cpu.max` quota.
pub(crate) fn cpus() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(2);
    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
        .ok()
        .and_then(|text| parse_cpu_max(&text));
    cpus.min(quota.unwrap_or(usize::MAX)).max(1)
}

fn parse_cpu_max(text: &str) -> Option<usize> {
    let mut parts = text.split_whitespace();
    let quota: f64 = parts.next()?.parse().ok()?;
    let period: f64 = parts.next()?.parse().ok()?;
    let cpus = (quota / period).ceil();
    (cpus >= 1.0).then_some(cpus as usize)
}

/// Available memory and where the binding number came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Available {
    pub bytes: u64,
    /// `cgroup <dir>` or `meminfo`.
    pub source: String,
}

/// The tightest of the cgroup limits and the host's `MemAvailable`.
pub(crate) fn available_memory() -> Option<Available> {
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    available_memory_in(Path::new("/sys/fs/cgroup"), &self_cgroup, &meminfo)
}

/// [`available_memory`] over an explicit cgroup mount and file contents (for tests).
pub(crate) fn available_memory_in(
    cgroup_root: &Path,
    self_cgroup: &str,
    meminfo: &str,
) -> Option<Available> {
    let mut best: Option<Available> = None;
    let mut consider = |bytes: u64, source: String| {
        if best.as_ref().is_none_or(|b| bytes < b.bytes) {
            best = Some(Available { bytes, source });
        }
    };
    for (dir, avail) in cgroup_v2_levels(cgroup_root, self_cgroup)
        .into_iter()
        .chain(cgroup_v1(cgroup_root))
    {
        consider(avail, format!("cgroup {}", dir.display()));
    }
    if let Some(bytes) = mem_available(meminfo) {
        consider(bytes, "meminfo".to_string());
    }
    best
}

/// Free memory under each limited cgroup v2 level from the process's cgroup up to the root.
fn cgroup_v2_levels(root: &Path, self_cgroup: &str) -> Vec<(PathBuf, u64)> {
    let Some(rel) = self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
    else {
        return Vec::new();
    };
    let mut dir = root.join(rel.trim_start_matches('/'));
    let mut out = Vec::new();
    loop {
        if let Some(avail) = v2_level(&dir) {
            out.push((dir.clone(), avail));
        }
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    out
}

fn v2_level(dir: &Path) -> Option<u64> {
    let max = std::fs::read_to_string(dir.join("memory.max")).ok()?;
    let max: u64 = max.trim().parse().ok()?; // "max" = unlimited at this level
    let current: u64 = std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let reclaimable = stat_value(dir, "memory.stat", "inactive_file").unwrap_or(0);
    let used = current.saturating_sub(reclaimable);
    Some(max.saturating_sub(used))
}

fn cgroup_v1(root: &Path) -> Option<(PathBuf, u64)> {
    let dir = root.join("memory");
    let limit: u64 = std::fs::read_to_string(dir.join("memory.limit_in_bytes"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if limit >= 1 << 50 {
        return None; // unlimited
    }
    let usage: u64 = std::fs::read_to_string(dir.join("memory.usage_in_bytes"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let reclaimable = stat_value(&dir, "memory.stat", "total_inactive_file").unwrap_or(0);
    Some((dir, limit.saturating_sub(usage.saturating_sub(reclaimable))))
}

fn stat_value(dir: &Path, file: &str, key: &str) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == key).then(|| parts.next()?.parse().ok())?
    })
}

fn mem_available(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Environment that bounds the parallelism of a build or test command the engine runs but does
/// not spell itself (a user-given test command): only variables the caller's environment does
/// not set already.
pub(crate) fn parallel_env(jobs: usize) -> Vec<(String, String)> {
    [
        ("MAKEFLAGS", format!("-j{jobs}")),
        ("CMAKE_BUILD_PARALLEL_LEVEL", jobs.to_string()),
        ("CTEST_PARALLEL_LEVEL", jobs.to_string()),
    ]
    .into_iter()
    .filter(|(name, _)| std::env::var_os(name).is_none())
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Session-wide build lock
// ---------------------------------------------------------------------------------------------

/// A one-at-a-time lock for builds and test runs.
#[derive(Clone, Default)]
pub(crate) struct BuildLock(Arc<tokio::sync::Mutex<()>>);

/// Held while one engine-started build or test run is in progress.
pub type BuildSlot = tokio::sync::OwnedMutexGuard<()>;

impl BuildLock {
    /// Wait up to `wait` for the slot; `None` when it is still taken after that.
    pub(crate) async fn acquire(&self, wait: Duration) -> Option<BuildSlot> {
        tokio::time::timeout(wait, self.0.clone().lock_owned())
            .await
            .ok()
    }

    #[cfg(test)]
    pub(crate) fn busy(&self) -> bool {
        self.0.try_lock().is_err()
    }
}

static BUILD_LOCK: OnceLock<BuildLock> = OnceLock::new();

/// The process-wide (session-wide) lock.
pub(crate) fn session_lock() -> &'static BuildLock {
    BUILD_LOCK.get_or_init(BuildLock::default)
}

/// Wait up to `wait` for the session's build slot. `None` when another engine-started build or
/// test run still holds it after that.
pub async fn build_slot(wait: Duration) -> Option<BuildSlot> {
    session_lock().acquire(wait).await
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn jobs_follow_memory_and_cpus() {
        assert_eq!(jobs_for(8, Some(16 * GIB)), 8);
        assert_eq!(jobs_for(8, Some(6 * GIB)), 4);
        assert_eq!(jobs_for(8, Some(3 * GIB)), 2);
        assert_eq!(jobs_for(8, Some(GIB)), 1, "never below one");
        assert_eq!(jobs_for(2, Some(64 * GIB)), 2, "never above the CPUs");
        assert_eq!(jobs_for(4, None), 4);
        assert_eq!(jobs_for(0, Some(0)), 1);
    }

    #[test]
    fn cpu_quota_is_read() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2));
        assert_eq!(parse_cpu_max("150000 100000"), Some(2));
        assert_eq!(parse_cpu_max("max 100000"), None);
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// A container limited to 4 GiB with 3 GiB in use (0.5 GiB of it droppable page cache) on a
    /// host with 10 GiB free: the container's 1.5 GiB is what binds.
    #[test]
    fn the_tightest_cgroup_level_binds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(&root.join("memory.max"), "max\n");
        write(&root.join("slice/memory.max"), &format!("{}\n", 4 * GIB));
        write(
            &root.join("slice/memory.current"),
            &format!("{}\n", 3 * GIB),
        );
        write(
            &root.join("slice/memory.stat"),
            &format!("anon 1\ninactive_file {}\nactive_file 5\n", GIB / 2),
        );
        write(&root.join("slice/job/memory.max"), "max\n");
        let meminfo = format!(
            "MemTotal: 16000000 kB\nMemAvailable: {} kB\n",
            10 * GIB / 1024
        );
        let avail = available_memory_in(root, "0::/slice/job\n", &meminfo).expect("reading");
        assert_eq!(avail.bytes, 4 * GIB - (3 * GIB - GIB / 2));
        assert!(avail.source.contains("slice"), "{avail:?}");
        assert_eq!(jobs_for(8, Some(avail.bytes)), 1);
        // A host with less free memory than the container's headroom binds instead.
        let tight = format!("MemAvailable: {} kB\n", GIB / 1024);
        let avail = available_memory_in(root, "0::/slice/job\n", &tight).expect("reading");
        assert_eq!((avail.bytes, avail.source.as_str()), (GIB, "meminfo"));
    }

    #[test]
    fn falls_back_to_meminfo_and_cgroup_v1() {
        let dir = tempfile::tempdir().expect("tempdir");
        let meminfo = format!("MemAvailable: {} kB\n", 6 * GIB / 1024);
        let avail = available_memory_in(dir.path(), "", &meminfo).expect("reading");
        assert_eq!((avail.bytes, avail.source.as_str()), (6 * GIB, "meminfo"));
        write(
            &dir.path().join("memory/memory.limit_in_bytes"),
            &format!("{}\n", 2 * GIB),
        );
        write(
            &dir.path().join("memory/memory.usage_in_bytes"),
            &format!("{GIB}\n"),
        );
        write(
            &dir.path().join("memory/memory.stat"),
            "total_inactive_file 0\n",
        );
        let avail = available_memory_in(dir.path(), "", &meminfo).expect("reading");
        assert_eq!(avail.bytes, GIB);
        assert!(available_memory_in(dir.path(), "", "").is_some());
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(available_memory_in(empty.path(), "", ""), None);
    }

    #[test]
    fn this_host_gives_a_reading() {
        assert!(jobs() >= 1);
        assert!(cpus() >= 1);
    }

    #[tokio::test]
    async fn one_build_at_a_time() {
        // A private lock: other tests in this process use the session lock concurrently.
        let lock = BuildLock::default();
        let first = lock
            .acquire(Duration::from_secs(1))
            .await
            .expect("free slot");
        assert!(lock.busy());
        assert!(
            lock.acquire(Duration::from_millis(50)).await.is_none(),
            "second build waits"
        );
        drop(first);
        assert!(!lock.busy());
        assert!(lock.acquire(Duration::from_millis(50)).await.is_some());
    }

    #[test]
    fn parallel_env_bounds_unset_variables() {
        let env = parallel_env(3);
        for (name, value) in &env {
            assert!(value == "3" || value == "-j3", "{name}={value}");
        }
    }
}
