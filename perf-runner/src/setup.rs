//! `setup` / `teardown`: put the host into (and out of) measurement shape at
//! runtime, without a reboot. Idempotent; every change goes to the VM's
//! change log.

use std::path::Path;

use crate::host;
use crate::host::LabConfig;

pub struct SetupOptions {
    pub measure_cpu: Option<usize>,
    pub cgroup_name: String,
}

fn write(path: &str, value: &str) -> Result<(), String> {
    std::fs::write(path, value).map_err(|e| format!("write {value:?} to {path}: {e}"))
}

/// Cores as sorted lists of sibling CPUs.
fn cores() -> Vec<Vec<usize>> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for cpu in host::online_cpus() {
        if seen.contains(&cpu) {
            continue;
        }
        let sib = host::siblings_of(cpu);
        seen.extend(sib.iter().copied());
        out.push(sib);
    }
    out
}

pub fn setup(opts: &SetupOptions, runner_exe: &Path) -> Result<LabConfig, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("setup changes system settings: run it with sudo".to_string());
    }
    let cores = cores();
    if cores.len() < 2 {
        return Err(format!("only {} core(s): need one for the OS and one to measure on", cores.len()));
    }
    // Core 0 (the core of cpu0) keeps the OS, sshd, builds and the runner.
    let housekeeping = cores[0].clone();
    let isolated: Vec<usize> = cores[1..].iter().flatten().copied().collect();
    let cg = format!("{}/{}", host::CGROUP_ROOT, opts.cgroup_name);

    // 1. cpuset partition.
    let sub = host::read(format!("{}/cgroup.subtree_control", host::CGROUP_ROOT)).unwrap_or_default();
    if !sub.split_whitespace().any(|c| c == "cpuset") {
        write(&format!("{}/cgroup.subtree_control", host::CGROUP_ROOT), "+cpuset")?;
        host::log_change("enabled the cpuset controller in the root cgroup");
    }
    if !Path::new(&cg).exists() {
        std::fs::create_dir(&cg).map_err(|e| format!("mkdir {cg}: {e}"))?;
    }
    let want = host::format_cpulist(&isolated);
    if host::read(format!("{cg}/cpuset.cpus.partition")).as_deref() != Some("isolated")
        || host::read(format!("{cg}/cpuset.cpus")).map(|s| host::parse_cpulist(&s)) != Some(isolated.clone())
    {
        let _ = write(&format!("{cg}/cpuset.cpus.partition"), "member");
        write(&format!("{cg}/cpuset.cpus"), &want)?;
        let mems = host::read("/sys/devices/system/node/online").unwrap_or_else(|| "0".to_string());
        write(&format!("{cg}/cpuset.mems"), &mems)?;
        write(&format!("{cg}/cpuset.cpus.partition"), "isolated")?;
        host::log_change(&format!("cpuset isolated partition {cg} = cpus {want}; everything else runs on {}", host::format_cpulist(&housekeeping)));
    }
    let part = host::read(format!("{cg}/cpuset.cpus.partition")).unwrap_or_default();
    if part != "isolated" {
        return Err(format!("the kernel refused the isolated partition: {cg}/cpuset.cpus.partition = {part:?}"));
    }

    // 2. Interrupts to the housekeeping core. Kernel-managed IRQs (nvme
    //    queues) refuse the write; they are handled by choosing the measured
    //    core below.
    let hk_list = host::format_cpulist(&housekeeping);
    let _ = write("/proc/irq/default_smp_affinity", &host::cpumask_hex(&housekeeping));
    let mut moved = 0;
    let mut stuck = Vec::new();
    for irq in host::irqs() {
        let p = format!("/proc/irq/{}/smp_affinity_list", irq.irq);
        let cur = host::read(&p).map(|s| host::parse_cpulist(&s)).unwrap_or_default();
        if cur == housekeeping {
            continue;
        }
        if std::fs::write(&p, &hk_list).is_ok() {
            moved += 1;
        } else {
            stuck.push(irq.irq);
        }
    }
    if moved > 0 {
        host::log_change(&format!("moved {moved} IRQ(s) to cpus {hk_list}; kernel-managed (not movable): {stuck:?}"));
    }

    // 3. irqbalance would undo step 2.
    if std::process::Command::new("systemctl").args(["is-active", "--quiet", "irqbalance"]).status().is_ok_and(|s| s.success()) {
        let _ = std::process::Command::new("systemctl").args(["stop", "irqbalance"]).status();
        host::log_change("stopped irqbalance");
    }

    // 4. Pick the measured core: the first isolated core none of whose
    //    threads receives a device interrupt (managed IRQs stay put).
    let irqs = host::irqs();
    let noisy = |cpu: usize| irqs.iter().any(|i| i.lands_on(&[cpu]));
    let measure_core = match opts.measure_cpu {
        Some(c) => cores.iter().find(|core| core.contains(&c)).cloned().ok_or_else(|| format!("cpu {c} is not online"))?,
        None => cores[1..]
            .iter()
            .find(|core| !core.iter().any(|&c| noisy(c)))
            .or(cores.get(1))
            .cloned()
            .ok_or("no core to measure on")?,
    };
    let measure_cpu = opts.measure_cpu.unwrap_or(measure_core[0]);
    if !isolated.contains(&measure_cpu) {
        return Err(format!("cpu {measure_cpu} is on the housekeeping core"));
    }
    let sibling_cpu = measure_core.iter().copied().find(|&c| c != measure_cpu);
    let control_cpu = isolated
        .iter()
        .copied()
        .find(|c| !measure_core.contains(c) && !noisy(*c))
        .or_else(|| isolated.iter().copied().find(|c| !measure_core.contains(c)))
        .unwrap_or(measure_cpu);

    let mut cfg = LabConfig {
        version: 1,
        cgroup: cg,
        housekeeping,
        isolated,
        measure_cpu,
        sibling_cpu,
        control_cpu,
        freq_ghz: None,
        created_utc: host::now_utc(),
    };

    // 5. Frequency baseline on the measured CPU.
    let ghz = crate::preflight::calibrate(runner_exe, &cfg)?;
    cfg.freq_ghz = Some(crate::stats::median(&ghz));

    let path = host::config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
    host::log_change(&format!(
        "lab config {}: measure cpu {measure_cpu}, sibling {:?}, control cpu {control_cpu}, {:.3} GHz",
        path.display(),
        sibling_cpu,
        cfg.freq_ghz.unwrap_or(0.0)
    ));
    Ok(cfg)
}

/// Undo `setup`: dissolve the partition and spread IRQs over every CPU again.
/// The lab config is kept, so preflight then says exactly what is missing.
pub fn teardown(cgroup_name: &str) -> Result<String, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("teardown changes system settings: run it with sudo".to_string());
    }
    let cg = format!("{}/{}", host::CGROUP_ROOT, cgroup_name);
    let all = host::format_cpulist(&host::online_cpus());
    let mut done = Vec::new();
    if Path::new(&cg).exists() {
        // Any task still inside goes back to the root cgroup.
        if let Some(procs) = host::read(format!("{cg}/cgroup.procs")) {
            for pid in procs.lines() {
                let _ = std::fs::write(format!("{}/cgroup.procs", host::CGROUP_ROOT), pid);
            }
        }
        let _ = write(&format!("{cg}/cpuset.cpus.partition"), "member");
        std::fs::remove_dir(&cg).map_err(|e| format!("rmdir {cg}: {e}"))?;
        done.push(format!("removed the isolated partition {cg}"));
    }
    let _ = write("/proc/irq/default_smp_affinity", &host::cpumask_hex(&host::online_cpus()));
    for irq in host::irqs() {
        let _ = std::fs::write(format!("/proc/irq/{}/smp_affinity_list", irq.irq), &all);
    }
    done.push(format!("IRQ affinity back to cpus {all}"));
    let msg = done.join("; ");
    host::log_change(&format!("teardown: {msg}"));
    Ok(msg)
}
