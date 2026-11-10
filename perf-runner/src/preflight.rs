//! Preflight: refuse to measure on a host that is not fit to measure.
//!
//! Every check is decided by a pure function over [`Facts`] so each refusal
//! is unit-tested; [`gather`] reads the real host. A FAIL anywhere means no
//! numbers: the caller reports NOT_RUN with the failing checks and their fixes.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::host;
use crate::host::LabConfig;
use crate::host::Window;
use crate::spawn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "FAIL")]
    Fail,
    #[serde(rename = "WARN")]
    Warn,
    #[serde(rename = "NOT_APPLICABLE")]
    NotApplicable,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Warn => "WARN",
            Status::NotApplicable => "N/A",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

fn pass(name: &'static str, detail: impl Into<String>) -> Check {
    Check { name, status: Status::Pass, detail: detail.into(), fix: None }
}
fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { name, status: Status::Fail, detail: detail.into(), fix: Some(fix.into()) }
}
fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { name, status: Status::Warn, detail: detail.into(), fix: Some(fix.into()) }
}
fn na(name: &'static str, detail: impl Into<String>) -> Check {
    Check { name, status: Status::NotApplicable, detail: detail.into(), fix: None }
}

#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub ok: bool,
    pub checks: Vec<Check>,
    /// Median effective frequency on the measure CPU from this preflight.
    pub freq_ghz: Option<f64>,
}

impl Preflight {
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| c.status == Status::Fail).collect()
    }

    pub fn failure_summary(&self) -> String {
        self.failures()
            .iter()
            .map(|c| format!("{}: {} (fix: {})", c.name, c.detail, c.fix.as_deref().unwrap_or("-")))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Thresholds. Ticks are USER_HZ (normally 100 per second).
pub const SIBLING_BUSY_MAX: f64 = 0.01;
pub const ISOLATED_BUSY_MAX: f64 = 0.01;
pub const HOUSEKEEPING_BUSY_MAX: f64 = 0.30;
pub const DEVICE_IRQ_PER_S_MAX: f64 = 2.0;
pub const FREQ_SPREAD_MAX: f64 = 0.005;
pub const FREQ_BASELINE_MAX: f64 = 0.01;
pub const CALIBRATION_RUNS: usize = 5;
/// Checks that fail because of other jobs' load, which passes: the campaign
/// waits and asks again instead of refusing at once.
pub const TRANSIENT: [&str; 3] = ["other_load", "smt_sibling_idle", "steal_time"];
pub const CALIBRATION_ITERS: u64 = 40_000_000;

/// Everything preflight decides on, read once.
#[derive(Debug, Clone)]
pub struct Facts {
    pub is_root: bool,
    pub config: Result<LabConfig, String>,
    pub partition: Option<String>,
    pub root_isolated: Vec<usize>,
    pub slices: Vec<(String, Vec<usize>)>,
    pub irqs: Vec<host::IrqInfo>,
    pub irqbalance_active: bool,
    /// (governor, min_khz, max_khz) of the measure CPU, when cpufreq exists.
    pub cpufreq: Option<(String, u64, u64)>,
    /// intel_pstate no_turbo (1 = turbo off) or cpufreq boost (1 = on).
    pub no_turbo: Option<i64>,
    pub boost: Option<i64>,
    pub clocksource: Option<String>,
    pub constant_tsc: bool,
    pub nonstop_tsc: bool,
    pub nmi_watchdog: Option<i64>,
    pub paranoid: Option<i64>,
    pub thp: Option<String>,
    pub swap_used_kb: u64,
    pub mem_available_kb: u64,
    /// Dynamic sample (None when it could not be taken).
    pub window: Option<Window>,
    /// Effective GHz of each calibration run, or why it could not run.
    pub calibration: Result<Vec<f64>, String>,
}

pub fn decide(f: &Facts) -> Vec<Check> {
    let mut v = Vec::new();
    v.push(if f.is_root {
        pass("root", "running as root (needed to place the job on the isolated CPU)")
    } else {
        fail("root", "not running as root", "run it with sudo (perf-runner re-runs itself through `sudo -n` when it can)")
    });
    let cfg = match &f.config {
        Ok(c) => {
            v.push(pass(
                "lab_config",
                format!(
                    "measure cpu {}, SMT sibling {}, housekeeping {}",
                    c.measure_cpu,
                    c.sibling_cpu.map_or("none".to_string(), |s| s.to_string()),
                    host::format_cpulist(&c.housekeeping)
                ),
            ));
            c
        }
        Err(e) => {
            v.push(fail("lab_config", e.clone(), "sudo perf-runner setup"));
            return v;
        }
    };
    v.push(check_isolation(f, cfg));
    v.push(check_irq_affinity(f, cfg));
    v.push(if f.irqbalance_active {
        fail("irqbalance", "irqbalance is running and will move interrupts back onto the measured core", "sudo systemctl stop irqbalance (setup does this)")
    } else {
        pass("irqbalance", "not running")
    });
    v.push(check_governor(f));
    v.push(check_turbo(f));
    v.push(check_frequency(f, cfg));
    v.push(check_clocksource(f));
    v.push(match f.nmi_watchdog {
        Some(0) => pass("nmi_watchdog", "off"),
        Some(n) => fail("nmi_watchdog", format!("kernel.nmi_watchdog={n}: it takes a PMU counter and fires NMIs"), "sudo sysctl kernel.nmi_watchdog=0"),
        None => na("nmi_watchdog", "not present"),
    });
    v.push(match f.paranoid {
        Some(p) if p <= 1 => pass("perf_counters", format!("perf_event_paranoid={p}")),
        Some(p) => fail("perf_counters", format!("perf_event_paranoid={p}: per-process counters (frequency check, TMA) are blocked"), "sudo sysctl kernel.perf_event_paranoid=1"),
        None => fail("perf_counters", "perf_event_paranoid unreadable", "is this Linux with perf events?"),
    });
    v.push(match f.thp.as_deref() {
        Some("always") => fail("transparent_hugepages", "THP=always: khugepaged compaction adds stalls at random times", "echo madvise | sudo tee /sys/kernel/mm/transparent_hugepage/enabled"),
        Some(m) => pass("transparent_hugepages", format!("THP={m}")),
        None => na("transparent_hugepages", "not present"),
    });
    v.push(if f.swap_used_kb > 0 {
        warn("swap", format!("{} MiB of swap in use: page-ins add latency spikes", f.swap_used_kb / 1024), "free memory or sudo swapoff -a")
    } else {
        pass("swap", "no swap in use")
    });
    v.push(if f.mem_available_kb < 2 * 1024 * 1024 {
        fail("memory", format!("only {} MiB available", f.mem_available_kb / 1024), "stop whatever is using the memory")
    } else {
        pass("memory", format!("{} GiB available", f.mem_available_kb / 1024 / 1024))
    });
    v.extend(check_load(f, cfg));
    v
}

fn check_isolation(f: &Facts, c: &LabConfig) -> Check {
    let want: Vec<usize> = std::iter::once(c.measure_cpu).chain(c.sibling_cpu).collect();
    match f.partition.as_deref() {
        None => return fail("isolated_cpu", format!("cpuset partition {} does not exist", c.cgroup), "sudo perf-runner setup"),
        Some(p) if p != "isolated" => {
            return fail("isolated_cpu", format!("{} is \"{p}\", not an isolated partition", c.cgroup), "sudo perf-runner setup");
        }
        _ => {}
    }
    let missing: Vec<usize> = want.iter().copied().filter(|x| !f.root_isolated.contains(x)).collect();
    if !missing.is_empty() {
        return fail("isolated_cpu", format!("cpu {} not in the kernel's isolated set", host::format_cpulist(&missing)), "sudo perf-runner setup");
    }
    for (name, eff) in &f.slices {
        let leak: Vec<usize> = eff.iter().copied().filter(|x| want.contains(x)).collect();
        if !leak.is_empty() {
            return fail("isolated_cpu", format!("{name} may still run on cpu {}", host::format_cpulist(&leak)), "sudo perf-runner setup");
        }
    }
    pass("isolated_cpu", format!("cpu {} (and its SMT sibling) isolated; other tasks confined to {}", c.measure_cpu, host::format_cpulist(&c.housekeeping)))
}

fn check_irq_affinity(f: &Facts, c: &LabConfig) -> Check {
    let core: Vec<usize> = std::iter::once(c.measure_cpu).chain(c.sibling_cpu).collect();
    let bad: Vec<String> = f
        .irqs
        .iter()
        .filter(|i| i.lands_on(&core))
        .map(|i| format!("{} ({})", i.irq, i.name))
        .collect();
    if !bad.is_empty() {
        return fail("irq_affinity", format!("IRQ {} delivered to the measured core", bad.join(", ")), "sudo perf-runner setup (moves every movable IRQ to the housekeeping CPUs)");
    }
    if let Some(w) = &f.window {
        let n: u64 = core.iter().map(|x| w.device_irqs.get(x).copied().unwrap_or(0)).sum();
        let rate = if w.seconds > 0.0 { n as f64 / w.seconds } else { 0.0 };
        if rate > DEVICE_IRQ_PER_S_MAX {
            return fail("irq_affinity", format!("{n} device interrupts hit the measured core in {:.1}s", w.seconds), "find the IRQ in /proc/interrupts and move it to the housekeeping CPUs");
        }
    }
    pass("irq_affinity", "no device interrupt is delivered to the measured core")
}

fn check_governor(f: &Facts) -> Check {
    match &f.cpufreq {
        None => na("cpu_governor", "cpufreq is not exposed (virtual machine); the frequency is verified by measurement instead"),
        Some((g, min, max)) if g == "performance" && min == max => pass("cpu_governor", format!("performance, fixed at {} MHz", max / 1000)),
        Some((g, min, max)) if g == "performance" => fail("cpu_governor", format!("performance governor but the range is {}-{} MHz", min / 1000, max / 1000), "pin scaling_min_freq = scaling_max_freq on the measured CPU"),
        Some((g, _, _)) => fail("cpu_governor", format!("governor is {g}"), "sudo cpupower frequency-set -g performance"),
    }
}

fn check_turbo(f: &Facts) -> Check {
    match (f.no_turbo, f.boost) {
        (Some(1), _) | (_, Some(0)) => pass("turbo", "turbo/boost off"),
        (Some(_), _) | (_, Some(_)) => fail("turbo", "turbo/boost is on: the frequency follows temperature and the other cores' load", "echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo"),
        (None, None) => na("turbo", "not controllable from the guest (on GCP C4 the all-core turbo is pinned by the host); verified by measured frequency"),
    }
}

fn check_frequency(f: &Facts, c: &LabConfig) -> Check {
    let samples = match &f.calibration {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return fail("frequency", "no calibration sample", "check perf_event_paranoid and the PMU"),
        Err(e) => return fail("frequency", format!("could not measure the core frequency: {e}"), "sudo sysctl kernel.perf_event_paranoid=1; check the VM exposes a PMU"),
    };
    let lo = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = samples.iter().copied().fold(0.0, f64::max);
    let med = crate::stats::median(samples);
    let spread = hi / lo - 1.0;
    if spread > FREQ_SPREAD_MAX {
        return fail(
            "frequency",
            format!("the measured core ran at {lo:.3}-{hi:.3} GHz across {} calibration runs ({:.2}% spread): the frequency is not pinned", samples.len(), spread * 100.0),
            "pin the frequency (performance governor, min=max, turbo off) and re-run",
        );
    }
    if let Some(base) = c.freq_ghz {
        let dev = (med / base - 1.0).abs();
        if dev > FREQ_BASELINE_MAX {
            return fail(
                "frequency",
                format!("the measured core runs at {med:.3} GHz but setup measured {base:.3} GHz ({:.2}% off)", dev * 100.0),
                "find what changed the frequency (power policy, host migration); re-run setup only if the new frequency is intended",
            );
        }
    }
    pass("frequency", format!("{med:.3} GHz, {:.2}% spread over {} runs", spread * 100.0, samples.len()))
}

fn check_clocksource(f: &Facts) -> Check {
    match f.clocksource.as_deref() {
        Some("tsc") if f.constant_tsc && f.nonstop_tsc => pass("timer_source", "tsc (constant, nonstop)"),
        Some("tsc") => fail("timer_source", "tsc is the clocksource but the CPU does not report constant_tsc and nonstop_tsc", "use a host with an invariant TSC"),
        Some(cs) => fail("timer_source", format!("clocksource is {cs}: reading the clock costs a VM exit or a slow device read"), "echo tsc | sudo tee /sys/devices/system/clocksource/clocksource0/current_clocksource"),
        None => fail("timer_source", "clocksource unreadable", "check /sys/devices/system/clocksource"),
    }
}

fn check_load(f: &Facts, c: &LabConfig) -> Vec<Check> {
    let Some(w) = &f.window else {
        return vec![fail("other_load", "could not sample /proc/stat", "is /proc mounted?")];
    };
    let mut v = Vec::new();
    match c.sibling_cpu {
        Some(s) => {
            let share = w.busy_share(s);
            v.push(if share > SIBLING_BUSY_MAX {
                fail("smt_sibling_idle", format!("SMT sibling cpu {s} was {:.1}% busy: it shares the measured core's pipeline and caches", share * 100.0), format!("find what runs on cpu {s}: ps -eo pid,psr,comm | awk '$2=={s}'"))
            } else {
                pass("smt_sibling_idle", format!("cpu {s} idle ({:.1}% busy)", share * 100.0))
            });
        }
        None => v.push(na("smt_sibling_idle", "no SMT sibling")),
    }
    let others: Vec<(usize, f64)> = c
        .isolated
        .iter()
        .copied()
        .filter(|x| *x != c.measure_cpu && Some(*x) != c.sibling_cpu)
        .map(|x| (x, w.busy_share(x)))
        .filter(|(_, s)| *s > ISOLATED_BUSY_MAX)
        .collect();
    let hk: f64 = c.housekeeping.iter().map(|&x| w.busy_share(x)).sum::<f64>() / c.housekeeping.len().max(1) as f64;
    v.push(if !others.is_empty() {
        fail(
            "other_load",
            format!("isolated cpu(s) busy: {}", others.iter().map(|(x, s)| format!("cpu{x} {:.0}%", s * 100.0)).collect::<Vec<_>>().join(", ")),
            "something was placed in the measurement partition; stop it",
        )
    } else if hk > HOUSEKEEPING_BUSY_MAX {
        fail(
            "other_load",
            format!("the housekeeping CPUs were {:.0}% busy (limit {:.0}%): another job shares the L3 and memory bandwidth", hk * 100.0, HOUSEKEEPING_BUSY_MAX * 100.0),
            "wait for builds or other jobs to finish (uptime, top)",
        )
    } else {
        pass("other_load", format!("housekeeping {:.0}% busy, isolated CPUs idle", hk * 100.0))
    });
    let steal = w.steal.get(&c.measure_cpu).copied().unwrap_or(0);
    v.push(if steal > 0 {
        fail("steal_time", format!("the hypervisor stole {steal} tick(s) from cpu {} in {:.1}s", c.measure_cpu, w.seconds), "the VM's host is oversubscribed; retry later or move the VM")
    } else {
        pass("steal_time", "no steal time on the measured CPU")
    });
    v
}

/// Read the real host and take the dynamic sample (about a second).
pub fn gather(runner_exe: &Path) -> Facts {
    let mut f = Facts {
        // SAFETY: geteuid has no preconditions.
        is_root: unsafe { libc::geteuid() } == 0,
        config: LabConfig::load(&host::config_path()),
        partition: None,
        root_isolated: Vec::new(),
        slices: Vec::new(),
        irqs: Vec::new(),
        irqbalance_active: false,
        cpufreq: None,
        no_turbo: None,
        boost: None,
        clocksource: None,
        constant_tsc: false,
        nonstop_tsc: false,
        nmi_watchdog: None,
        paranoid: None,
        thp: None,
        swap_used_kb: 0,
        mem_available_kb: 0,
        window: None,
        calibration: Err("not run".to_string()),
    };
    let flags = host::cpu_flags();
    f.constant_tsc = flags.iter().any(|x| x == "constant_tsc");
    f.nonstop_tsc = flags.iter().any(|x| x == "nonstop_tsc");
    f.clocksource = host::read("/sys/devices/system/clocksource/clocksource0/current_clocksource");
    f.nmi_watchdog = host::read_i64("/proc/sys/kernel/nmi_watchdog");
    f.paranoid = host::read_i64("/proc/sys/kernel/perf_event_paranoid");
    f.thp = host::read("/sys/kernel/mm/transparent_hugepage/enabled").and_then(|s| host::bracketed(&s));
    let swap_total = host::meminfo_kb("SwapTotal").unwrap_or(0);
    let swap_free = host::meminfo_kb("SwapFree").unwrap_or(0);
    f.swap_used_kb = swap_total.saturating_sub(swap_free);
    f.mem_available_kb = host::meminfo_kb("MemAvailable").unwrap_or(0);
    f.irqbalance_active = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "irqbalance"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    f.no_turbo = host::read_i64("/sys/devices/system/cpu/intel_pstate/no_turbo");
    f.boost = host::read_i64("/sys/devices/system/cpu/cpufreq/boost");
    f.root_isolated = host::read(format!("{}/cpuset.cpus.isolated", host::CGROUP_ROOT)).map(|s| host::parse_cpulist(&s)).unwrap_or_default();
    for slice in ["system.slice", "user.slice", "init.scope"] {
        if let Some(eff) = host::read(format!("{}/{slice}/cpuset.cpus.effective", host::CGROUP_ROOT)) {
            f.slices.push((slice.to_string(), host::parse_cpulist(&eff)));
        }
    }
    f.irqs = host::irqs();
    let Ok(cfg) = f.config.clone() else { return f };
    f.partition = host::read(format!("{}/cpuset.cpus.partition", cfg.cgroup));
    let base = format!("/sys/devices/system/cpu/cpu{}/cpufreq", cfg.measure_cpu);
    if let Some(g) = host::read(format!("{base}/scaling_governor")) {
        let min = host::read(format!("{base}/scaling_min_freq")).and_then(|s| s.parse().ok()).unwrap_or(0);
        let max = host::read(format!("{base}/scaling_max_freq")).and_then(|s| s.parse().ok()).unwrap_or(0);
        f.cpufreq = Some((g, min, max));
    }
    let before = host::Snapshot::take();
    f.calibration = if f.is_root && f.partition.as_deref() == Some("isolated") {
        calibrate(runner_exe, &cfg)
    } else {
        std::thread::sleep(Duration::from_millis(500));
        Err("needs root and the isolated partition".to_string())
    };
    let after = host::Snapshot::take();
    f.window = Some(Window::between(&before, &after));
    f
}

/// Effective GHz of [`CALIBRATION_RUNS`] runs of the built-in spin loop on
/// the measure CPU.
pub fn calibrate(runner_exe: &Path, cfg: &LabConfig) -> Result<Vec<f64>, String> {
    // Root-owned scratch, never a predictable name in world-writable /tmp.
    let dir = std::path::PathBuf::from(crate::fsutil::SCRATCH_ROOT);
    crate::fsutil::root_dir(&dir)?;
    let argv = vec![runner_exe.to_string_lossy().into_owned(), "__spin".to_string(), CALIBRATION_ITERS.to_string()];
    let env: Vec<(String, String)> = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    let out = dir.join(format!("cal-{}.out", std::process::id()));
    let procs = cfg.procs_file();
    let cpus = [cfg.measure_cpu];
    let mut ghz = Vec::new();
    for _ in 0..CALIBRATION_RUNS {
        let job = spawn::Job {
            argv: &argv,
            cwd: &dir,
            env: &env,
            cpus: &cpus,
            cgroup_procs: Some(&procs),
            creds: None,
            stdout: &out,
            stderr: &out,
            counters: true,
            timeout: Duration::from_secs(10),
        };
        let r = spawn::run(&job)?;
        if !r.ok() {
            return Err(format!("calibration run failed: {}", r.describe_failure()));
        }
        if let Some(e) = &r.counter_error {
            return Err(e.clone());
        }
        match r.freq_ghz() {
            Some(g) => ghz.push(g),
            None => return Err("cycles/task-clock not available".to_string()),
        }
    }
    let _ = std::fs::remove_file(&out);
    Ok(ghz)
}

/// The calibration workload: a dependent multiply-add chain (3-4 cycles per
/// step, no memory traffic), so cycles / task-clock is the core frequency.
pub fn spin(iters: u64) -> u64 {
    let mut x: u64 = 0x1234_5678;
    for _ in 0..iters {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
    }
    x
}

pub fn run(runner_exe: &Path) -> Preflight {
    let facts = gather(runner_exe);
    let freq = facts.calibration.as_ref().ok().map(|s| crate::stats::median(s));
    let checks = decide(&facts);
    let ok = checks.iter().all(|c| c.status != Status::Fail);
    Preflight { ok, checks, freq_ghz: freq }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn cfg() -> LabConfig {
        LabConfig {
            version: 1,
            cgroup: "/sys/fs/cgroup/perflab".to_string(),
            housekeeping: vec![0, 4],
            isolated: vec![1, 2, 3, 5, 6, 7],
            measure_cpu: 2,
            sibling_cpu: Some(6),
            control_cpu: 3,
            freq_ghz: Some(3.1),
            created_utc: String::new(),
        }
    }

    fn quiet_window() -> Window {
        let mut w = Window { seconds: 1.0, ticks_per_s: 100.0, ..Default::default() };
        for c in 0..8 {
            w.busy.insert(c, if c == 2 { 100 } else { 0 });
            w.steal.insert(c, 0);
            w.device_irqs.insert(c, if c == 0 { 50 } else { 0 });
        }
        w.busy.insert(0, 10);
        w
    }

    /// A host that passes everything.
    fn good() -> Facts {
        Facts {
            is_root: true,
            config: Ok(cfg()),
            partition: Some("isolated".to_string()),
            root_isolated: vec![1, 2, 3, 5, 6, 7],
            slices: vec![("user.slice".to_string(), vec![0, 4]), ("system.slice".to_string(), vec![0, 4])],
            irqs: vec![
                host::IrqInfo { irq: 57, name: "nvme0q1".to_string(), affinity: vec![0, 1, 4, 5], effective: vec![5] },
                // Moved away but never fired since: stale effective CPU.
                host::IrqInfo { irq: 31, name: "pciehp".to_string(), affinity: vec![0, 4], effective: vec![2] },
                // Managed, allowed on the measured core, but delivered elsewhere.
                host::IrqInfo { irq: 58, name: "nvme0q2".to_string(), affinity: vec![2, 3, 6, 7], effective: vec![7] },
            ],
            irqbalance_active: false,
            cpufreq: None,
            no_turbo: None,
            boost: None,
            clocksource: Some("tsc".to_string()),
            constant_tsc: true,
            nonstop_tsc: true,
            nmi_watchdog: Some(0),
            paranoid: Some(1),
            thp: Some("madvise".to_string()),
            swap_used_kb: 0,
            mem_available_kb: 28 * 1024 * 1024,
            window: Some(quiet_window()),
            calibration: Ok(vec![3.100, 3.101, 3.099, 3.100, 3.102]),
        }
    }

    fn failing(f: &Facts) -> Vec<&'static str> {
        decide(f).into_iter().filter(|c| c.status == Status::Fail).map(|c| c.name).collect()
    }

    #[test]
    fn a_good_host_passes() {
        let checks = decide(&good());
        let bad: Vec<_> = checks.iter().filter(|c| c.status == Status::Fail).collect();
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn each_mistuning_is_refused_with_a_fix() {
        let mut cases: Vec<(&str, Facts)> = Vec::new();
        let mut f = good();
        f.is_root = false;
        cases.push(("root", f));
        let mut f = good();
        f.config = Err("missing".to_string());
        cases.push(("lab_config", f));
        let mut f = good();
        f.partition = None;
        cases.push(("isolated_cpu", f));
        let mut f = good();
        f.partition = Some("member".to_string());
        cases.push(("isolated_cpu", f));
        let mut f = good();
        f.slices[0].1 = vec![0, 2, 4];
        cases.push(("isolated_cpu", f));
        let mut f = good();
        f.irqs.push(host::IrqInfo { irq: 61, name: "gve".to_string(), affinity: vec![2], effective: vec![2] });
        cases.push(("irq_affinity", f));
        let mut f = good();
        f.window.as_mut().unwrap().device_irqs.insert(6, 40);
        cases.push(("irq_affinity", f));
        let mut f = good();
        f.irqbalance_active = true;
        cases.push(("irqbalance", f));
        let mut f = good();
        f.cpufreq = Some(("powersave".to_string(), 800_000, 3_800_000));
        cases.push(("cpu_governor", f));
        let mut f = good();
        f.cpufreq = Some(("performance".to_string(), 800_000, 3_800_000));
        cases.push(("cpu_governor", f));
        let mut f = good();
        f.no_turbo = Some(0);
        cases.push(("turbo", f));
        let mut f = good();
        f.calibration = Ok(vec![3.1, 2.9, 3.1, 3.0, 3.1]);
        cases.push(("frequency", f));
        let mut f = good();
        f.calibration = Ok(vec![2.80, 2.80, 2.80, 2.80, 2.80]);
        cases.push(("frequency", f));
        let mut f = good();
        f.calibration = Err("perf_event_open: Permission denied".to_string());
        cases.push(("frequency", f));
        let mut f = good();
        f.clocksource = Some("kvm-clock".to_string());
        cases.push(("timer_source", f));
        let mut f = good();
        f.nmi_watchdog = Some(1);
        cases.push(("nmi_watchdog", f));
        let mut f = good();
        f.paranoid = Some(3);
        cases.push(("perf_counters", f));
        let mut f = good();
        f.thp = Some("always".to_string());
        cases.push(("transparent_hugepages", f));
        let mut f = good();
        f.window.as_mut().unwrap().busy.insert(6, 30);
        cases.push(("smt_sibling_idle", f));
        let mut f = good();
        f.window.as_mut().unwrap().busy.insert(3, 50);
        cases.push(("other_load", f));
        let mut f = good();
        f.window.as_mut().unwrap().busy.insert(0, 90);
        f.window.as_mut().unwrap().busy.insert(4, 90);
        cases.push(("other_load", f));
        let mut f = good();
        f.window.as_mut().unwrap().steal.insert(2, 3);
        cases.push(("steal_time", f));
        let mut f = good();
        f.mem_available_kb = 512 * 1024;
        cases.push(("memory", f));
        for (want, f) in &cases {
            let names = failing(f);
            assert!(names.contains(want), "expected {want} to fail, got {names:?}");
            for c in decide(f).iter().filter(|c| c.status == Status::Fail) {
                assert!(c.fix.as_deref().is_some_and(|x| !x.is_empty()), "{} has no fix", c.name);
            }
        }
    }

    #[test]
    fn managed_irq_on_another_core_is_fine() {
        let f = good();
        assert!(!failing(&f).contains(&"irq_affinity"));
        let _ = BTreeMap::<u8, u8>::new();
    }
}
