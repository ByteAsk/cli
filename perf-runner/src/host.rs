//! Reading the host: topology, /proc/stat, /proc/interrupts, sysfs knobs, and
//! the lab configuration written by `perf-runner setup`.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

pub const DEFAULT_CONFIG: &str = "/etc/perf-lab/perflab.json";
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const CHANGES_LOG: &str = "/etc/perf-lab-changes.log";

/// The lab config. `PERFLAB_CONFIG` (tests) is ignored when running as root:
/// through `sudo -E` it would let the caller point root at any file.
pub fn config_path() -> PathBuf {
    let root = unsafe { libc::geteuid() } == 0;
    match std::env::var_os("PERFLAB_CONFIG") {
        Some(p) if !root => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_CONFIG),
    }
}

/// What `setup` decided and measured; every preflight checks the host still
/// matches it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabConfig {
    pub version: u32,
    /// The isolated cpuset partition, e.g. /sys/fs/cgroup/perflab.
    pub cgroup: String,
    pub housekeeping: Vec<usize>,
    pub isolated: Vec<usize>,
    pub measure_cpu: usize,
    pub sibling_cpu: Option<usize>,
    /// An isolated CPU on another core for perf's own process during TMA runs.
    pub control_cpu: usize,
    /// Effective frequency measured on the measure CPU at setup time.
    pub freq_ghz: Option<f64>,
    pub created_utc: String,
}

impl LabConfig {
    pub fn load(path: &Path) -> Result<LabConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} is not a valid lab config: {e}", path.display()))
    }

    pub fn procs_file(&self) -> PathBuf {
        Path::new(&self.cgroup).join("cgroup.procs")
    }
}

pub fn read(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// "0-3,5,7-8" -> [0,1,2,3,5,7,8]. Empty or unparseable parts are skipped.
pub fn parse_cpulist(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(a) = part.parse::<usize>() {
            out.push(a);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub fn format_cpulist(cpus: &[usize]) -> String {
    let mut v = cpus.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    parts.join(",")
}

/// Hex CPU mask ("11" for cpus 0 and 4).
pub fn cpumask_hex(cpus: &[usize]) -> String {
    let mut words: Vec<u32> = vec![0; cpus.iter().max().map_or(1, |m| m / 32 + 1)];
    for &c in cpus {
        words[c / 32] |= 1 << (c % 32);
    }
    words.iter().rev().map(|w| format!("{w:08x}")).collect::<Vec<_>>().join(",").trim_start_matches('0').to_string()
}

pub fn online_cpus() -> Vec<usize> {
    read("/sys/devices/system/cpu/online").map(|s| parse_cpulist(&s)).unwrap_or_default()
}

pub fn siblings_of(cpu: usize) -> Vec<usize> {
    read(format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"))
        .map(|s| parse_cpulist(&s))
        .unwrap_or_else(|| vec![cpu])
}

/// Per-CPU tick counters from /proc/stat.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTicks {
    pub busy: u64,
    pub idle: u64,
    pub steal: u64,
}

pub fn parse_proc_stat(text: &str) -> BTreeMap<usize, CpuTicks> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(name) = it.next() else { continue };
        let Some(id) = name.strip_prefix("cpu").and_then(|n| n.parse::<usize>().ok()) else { continue };
        let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
        if v.len() < 8 {
            continue;
        }
        // user nice system idle iowait irq softirq steal [guest guest_nice]
        out.insert(
            id,
            CpuTicks {
                busy: v[0] + v[1] + v[2] + v[5] + v[6],
                idle: v[3] + v[4],
                steal: v[7],
            },
        );
    }
    out
}

/// Device interrupts (numbered IRQ lines only, not LOC/RES/...) per CPU column.
pub fn parse_device_interrupts(text: &str) -> BTreeMap<usize, u64> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else { return BTreeMap::new() };
    let cpus: Vec<usize> = header
        .split_whitespace()
        .filter_map(|c| c.strip_prefix("CPU").and_then(|n| n.parse().ok()))
        .collect();
    let mut out: BTreeMap<usize, u64> = cpus.iter().map(|&c| (c, 0)).collect();
    for line in lines {
        let mut it = line.split_whitespace();
        let Some(label) = it.next() else { continue };
        let Some(num) = label.strip_suffix(':') else { continue };
        if num.parse::<u32>().is_err() {
            continue;
        }
        for (i, tok) in it.take(cpus.len()).enumerate() {
            if let Ok(n) = tok.parse::<u64>() {
                *out.entry(cpus[i]).or_default() += n;
            }
        }
    }
    out
}

/// One instantaneous view of the counters that disturbance checks compare.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub ticks: BTreeMap<usize, CpuTicks>,
    pub device_irqs: BTreeMap<usize, u64>,
    pub at: Option<std::time::Instant>,
}

impl Snapshot {
    pub fn take() -> Snapshot {
        Snapshot {
            ticks: std::fs::read_to_string("/proc/stat").map(|t| parse_proc_stat(&t)).unwrap_or_default(),
            device_irqs: std::fs::read_to_string("/proc/interrupts")
                .map(|t| parse_device_interrupts(&t))
                .unwrap_or_default(),
            at: Some(std::time::Instant::now()),
        }
    }
}

/// What happened on each CPU between two snapshots.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Window {
    pub seconds: f64,
    pub ticks_per_s: f64,
    pub busy: BTreeMap<usize, u64>,
    pub steal: BTreeMap<usize, u64>,
    pub device_irqs: BTreeMap<usize, u64>,
}

impl Window {
    pub fn between(a: &Snapshot, b: &Snapshot) -> Window {
        let seconds = match (a.at, b.at) {
            (Some(x), Some(y)) => y.duration_since(x).as_secs_f64(),
            _ => 0.0,
        };
        let mut w = Window {
            seconds,
            ticks_per_s: clk_tck(),
            ..Default::default()
        };
        for (cpu, tb) in &b.ticks {
            let ta = a.ticks.get(cpu).copied().unwrap_or_default();
            w.busy.insert(*cpu, tb.busy.saturating_sub(ta.busy));
            w.steal.insert(*cpu, tb.steal.saturating_sub(ta.steal));
        }
        for (cpu, nb) in &b.device_irqs {
            let na = a.device_irqs.get(cpu).copied().unwrap_or_default();
            w.device_irqs.insert(*cpu, nb.saturating_sub(na));
        }
        w
    }

    /// Busy share of `cpu` over the window (0..1).
    pub fn busy_share(&self, cpu: usize) -> f64 {
        let ticks = self.busy.get(&cpu).copied().unwrap_or(0) as f64;
        let cap = self.seconds * self.ticks_per_s;
        if cap <= 0.0 { 0.0 } else { ticks / cap }
    }

    pub fn busy_ticks(&self, cpu: usize) -> u64 {
        self.busy.get(&cpu).copied().unwrap_or(0)
    }
}

pub fn clk_tck() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

#[derive(Debug, Clone, Serialize)]
pub struct IrqInfo {
    pub irq: u32,
    pub name: String,
    /// Configured affinity (smp_affinity_list).
    pub affinity: Vec<usize>,
    /// Where the kernel last delivered it. Stale for an IRQ that has not fired
    /// since its affinity changed: it moves on its next interrupt.
    pub effective: Vec<usize>,
}

impl IrqInfo {
    /// Delivered to one of `cpus` now AND allowed to stay there. A movable
    /// IRQ whose affinity excludes `cpus` is not counted even if its
    /// effective CPU is stale.
    pub fn lands_on(&self, cpus: &[usize]) -> bool {
        self.effective.iter().any(|c| cpus.contains(c)) && self.affinity.iter().any(|c| cpus.contains(c))
    }
}

pub fn irqs() -> Vec<IrqInfo> {
    let names: BTreeMap<u32, String> = std::fs::read_to_string("/proc/interrupts")
        .map(|t| {
            t.lines()
                .filter_map(|l| {
                    let (num, rest) = l.trim_start().split_once(':')?;
                    let irq = num.parse::<u32>().ok()?;
                    let name = rest.split_whitespace().last().unwrap_or("").to_string();
                    Some((irq, name))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc/irq") {
        for e in dir.flatten() {
            let Ok(irq) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            let eff = read(e.path().join("effective_affinity_list")).map(|s| parse_cpulist(&s)).unwrap_or_default();
            let aff = read(e.path().join("smp_affinity_list")).map(|s| parse_cpulist(&s)).unwrap_or_default();
            out.push(IrqInfo {
                irq,
                name: names.get(&irq).cloned().unwrap_or_default(),
                affinity: aff,
                effective: eff,
            });
        }
    }
    out.sort_by_key(|i| i.irq);
    out
}

/// The bracketed choice in a sysfs "a [b] c" file.
pub fn bracketed(s: &str) -> Option<String> {
    let start = s.find('[')?;
    let end = s[start..].find(']')? + start;
    Some(s[start + 1..end].to_string())
}

pub fn read_i64(path: &str) -> Option<i64> {
    read(path)?.parse().ok()
}

pub fn meminfo_kb(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':').map(str::trim))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

pub fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("model name").map(|r| r.trim_start_matches([' ', '\t', ':']).to_string())))
        .unwrap_or_default()
}

pub fn cpu_flags() -> Vec<String> {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("flags").map(|r| r.trim_start_matches([' ', '\t', ':']).to_string())))
        .map(|f| f.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn hostname() -> String {
    read("/proc/sys/kernel/hostname").unwrap_or_default()
}

pub fn kernel() -> String {
    read("/proc/sys/kernel/osrelease").unwrap_or_default()
}

pub fn now_utc() -> String {
    // Seconds since the epoch -> RFC 3339 (UTC), no external crates.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Append one line to the host's change log.
pub fn log_change(what: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(CHANGES_LOG) {
        let _ = writeln!(f, "{} perf-runner: {what}", now_utc());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpulists() {
        assert_eq!(parse_cpulist("0-3,5,7-8"), vec![0, 1, 2, 3, 5, 7, 8]);
        assert_eq!(parse_cpulist(""), Vec::<usize>::new());
        assert_eq!(format_cpulist(&[1, 2, 3, 5, 6, 7]), "1-3,5-7");
        assert_eq!(format_cpulist(&[0, 4]), "0,4");
        assert_eq!(cpumask_hex(&[0, 4]), "11");
        assert_eq!(cpumask_hex(&[0, 1, 2, 3, 4, 5, 6, 7]), "ff");
    }

    #[test]
    fn proc_stat() {
        let t = "cpu  2591 223 1199 482347 1894 0 12 0 0 0\ncpu0 357 0 136 60281 179 0 3 5 0 0\ncpu2 249 52 160 60503 63 1 2 0 0 0\nintr 1\n";
        let s = parse_proc_stat(t);
        assert_eq!(s.len(), 2);
        assert_eq!(s[&0].busy, 357 + 136 + 3);
        assert_eq!(s[&0].steal, 5);
        assert_eq!(s[&2].busy, 249 + 52 + 160 + 1 + 2);
    }

    #[test]
    fn interrupts() {
        let t = "           CPU0       CPU1       CPU2\n  0:        181          0          5  IO-APIC   0-edge      timer\n 57:          1          2          3 PCI-MSIX nvme0q1\nLOC:      17840      19331      18137   Local timer interrupts\n";
        let d = parse_device_interrupts(t);
        assert_eq!(d[&0], 182);
        assert_eq!(d[&1], 2);
        assert_eq!(d[&2], 8);
    }

    #[test]
    fn misc() {
        assert_eq!(bracketed("always [madvise] never").as_deref(), Some("madvise"));
        assert_eq!(now_utc().len(), 20);
    }
}
