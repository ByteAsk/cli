//! Automatic checks run at low priority on spare cores.
//!
//! The finish gate's builds and tests run while the developer keeps working, so every program
//! the gate starts is niced (+10 by default), gets the lowest best-effort I/O priority, and on
//! Linux is pinned to the "spare" CPUs: the allowed CPUs minus the lowest quarter (at least one
//! CPU), which stay free for the developer's editor, shell and agent. Build parallelism follows
//! the same CPU count. Tools the user or the AI calls directly (cpp_perf, cpp_bench, ...) are not
//! in this scope and run at normal priority.
//!
//! Overrides: `BYTEASK_CPP_GATE_NICE` (0-19) and `BYTEASK_CPP_GATE_CPUS` (a CPU list such as
//! `2-7` or `1,3,5`; must be inside the allowed set).

use std::future::Future;

pub(crate) const ENV_NICE: &str = "BYTEASK_CPP_GATE_NICE";
pub(crate) const ENV_CPUS: &str = "BYTEASK_CPP_GATE_CPUS";
pub(crate) const DEFAULT_NICE: i32 = 10;

/// How the programs of an automatic check run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Background {
    pub nice: i32,
    /// CPUs the check may use (`None`: no affinity change, e.g. off Linux).
    pub cpus: Option<Vec<usize>>,
}

tokio::task_local! {
    static BACKGROUND: Background;
}

/// Run `work` with every program it starts at background priority.
pub(crate) async fn scope<F: Future>(background: Background, work: F) -> F::Output {
    BACKGROUND.scope(background, work).await
}

/// The background settings of the current task, if it is an automatic check.
pub(crate) fn current() -> Option<Background> {
    BACKGROUND.try_with(Clone::clone).ok()
}

/// The CPUs this process may run on (Linux), in order.
#[cfg(target_os = "linux")]
pub(crate) fn allowed_cpus() -> Vec<usize> {
    // SAFETY: sched_getaffinity fills the zeroed set we own.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|cpu| libc::CPU_ISSET(*cpu, &set))
            .collect()
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn allowed_cpus() -> Vec<usize> {
    Vec::new()
}

/// The spare CPUs: all but the lowest quarter (rounded up, at least one kept free) when there
/// are two or more; the only one otherwise.
pub(crate) fn spare(allowed: &[usize]) -> Vec<usize> {
    if allowed.len() < 2 {
        return allowed.to_vec();
    }
    let reserved = allowed.len().div_ceil(4).max(1);
    allowed[reserved..].to_vec()
}

/// `2-4,7` → [2, 3, 4, 7].
pub(crate) fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi): (usize, usize) = (lo.trim().parse().ok()?, hi.trim().parse().ok()?);
                if hi < lo || hi - lo > 4096 {
                    return None;
                }
                cpus.extend(lo..=hi);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    (!cpus.is_empty()).then_some(cpus)
}

/// The settings for automatic checks on this machine.
pub(crate) fn for_automatic_checks(env: impl Fn(&str) -> Option<String>, allowed: &[usize]) -> Background {
    let nice = env(ENV_NICE)
        .and_then(|value| value.trim().parse::<i32>().ok())
        .map_or(DEFAULT_NICE, |nice| nice.clamp(0, 19));
    let cpus = if allowed.is_empty() {
        None
    } else {
        let chosen = env(ENV_CPUS)
            .and_then(|value| parse_cpu_list(&value))
            .map(|list| list.into_iter().filter(|cpu| allowed.contains(cpu)).collect::<Vec<_>>())
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| spare(allowed));
        Some(chosen)
    };
    Background { nice, cpus }
}

/// The default settings, from the environment and this process's CPU set.
pub(crate) fn automatic() -> Background {
    for_automatic_checks(|key| std::env::var(key).ok(), &allowed_cpus())
}

/// Test switch (off unless set): `BYTEASK_BACKGROUND_OOM_SCORE_ADJ=<0..1000>` starts the programs
/// of the engine's background builds (the warm build, the automatic gate) at that `oom_score_adj`, so that when
/// memory runs out the kernel kills them before the agent's commands and the engine. Made before `fork` as bytes.
#[cfg(target_os = "linux")]
fn background_oom_score_adj() -> Option<([u8; 8], usize)> {
    let value: i32 = std::env::var("BYTEASK_BACKGROUND_OOM_SCORE_ADJ").ok()?.trim().parse().ok()?;
    if !(0..=1000).contains(&value) {
        return None;
    }
    let digits = value.to_string();
    let mut text = [0u8; 8];
    text.get_mut(..digits.len())?.copy_from_slice(digits.as_bytes());
    Some((text, digits.len()))
}

/// Apply the current task's background settings (if any) to a command before it starts.
#[cfg(unix)]
pub(crate) fn apply(command: &mut tokio::process::Command) {
    let Some(background) = current() else {
        return;
    };
    let nice = background.nice;
    #[cfg(target_os = "linux")]
    let set = background.cpus.as_ref().map(|cpus| {
        // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET writes inside it.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            for cpu in cpus {
                if *cpu < libc::CPU_SETSIZE as usize {
                    libc::CPU_SET(*cpu, &mut set);
                }
            }
            set
        }
    });
    #[cfg(target_os = "linux")]
    let oom = background_oom_score_adj();
    // `BYTEASK_BACKGROUND_IDLE=1`: the SCHED_IDLE class, read before fork. The raw system call:
    // musl's sched_setscheduler() is a stub that fails with ENOSYS; the kernel reads only sched_priority (0).
    #[cfg(target_os = "linux")]
    let idle = std::env::var("BYTEASK_BACKGROUND_IDLE")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"));
    #[cfg(target_os = "linux")]
    // SAFETY: an all-zero sched_param is valid (priority 0, the only one SCHED_IDLE takes).
    let param: libc::sched_param = unsafe { std::mem::zeroed() };
    // SAFETY: the closure runs in the forked child before exec and only makes the
    // setpriority(2), sched_setaffinity(2) and ioprio_set(2) system calls on itself, which
    // take no locks and allocate nothing. Failures are ignored: priority is best effort.
    // And open(2)/write(2)/close(2) of /proc/self/oom_score_adj with bytes made before fork.
    unsafe {
        command.pre_exec(move || {
            libc::setpriority(libc::PRIO_PROCESS, 0, nice);
            #[cfg(target_os = "linux")]
            {
                if let Some(set) = &set {
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set);
                }
                // IOPRIO_WHO_PROCESS = 1; best-effort class (2) at its lowest level (7).
                libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7);
                if idle {
                    libc::syscall(
                        libc::SYS_sched_setscheduler,
                        0,
                        libc::SCHED_IDLE,
                        &param as *const libc::sched_param,
                    );
                }
                if let Some((text, len)) = &oom {
                    let fd = libc::open(
                        b"/proc/self/oom_score_adj\0".as_ptr().cast::<libc::c_char>(),
                        libc::O_WRONLY | libc::O_CLOEXEC,
                    );
                    if fd >= 0 {
                        let _ = libc::write(fd, text.as_ptr().cast::<libc::c_void>(), *len);
                        libc::close(fd);
                    }
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn apply(_command: &mut tokio::process::Command) {}

/// Parallel jobs inside a background scope never exceed its CPUs.
pub(crate) fn clamp_jobs(jobs: usize) -> usize {
    match current().and_then(|background| background.cpus) {
        Some(cpus) => jobs.min(cpus.len()).min(background_jobs_cap()).max(1),
        None => jobs,
    }
}

/// Test switch (off unless set): `BYTEASK_BACKGROUND_JOBS=<n>` caps a background build's jobs.
fn background_jobs_cap() -> usize {
    std::env::var("BYTEASK_BACKGROUND_JOBS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(usize::MAX, |jobs| jobs.max(1))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn spare_cores_keep_the_lowest_quarter_free() {
        assert_eq!(spare(&[0]), vec![0]);
        assert_eq!(spare(&[0, 1]), vec![1]);
        assert_eq!(spare(&(0..8).collect::<Vec<_>>()), (2..8).collect::<Vec<_>>());
        assert_eq!(spare(&[4, 5, 6]), vec![5, 6]);
        assert_eq!(parse_cpu_list("2-4, 7,7"), Some(vec![2, 3, 4, 7]));
        assert_eq!(parse_cpu_list("4-2"), None);
        assert_eq!(parse_cpu_list("x"), None);
    }

    #[test]
    fn settings_come_from_the_environment_within_the_allowed_set() {
        let allowed: Vec<usize> = (0..8).collect();
        let none = |_: &str| None;
        assert_eq!(
            for_automatic_checks(none, &allowed),
            Background {
                nice: 10,
                cpus: Some((2..8).collect())
            }
        );
        let env = |key: &str| match key {
            ENV_NICE => Some("50".to_string()),
            ENV_CPUS => Some("6-9".to_string()),
            _ => None,
        };
        assert_eq!(
            for_automatic_checks(env, &allowed),
            Background {
                nice: 19,
                cpus: Some(vec![6, 7])
            }
        );
        assert_eq!(for_automatic_checks(none, &[]).cpus, None);
    }

    /// Checked from /proc: a program an automatic check starts runs niced, on the spare CPUs,
    /// at the lowest best-effort I/O priority; outside the scope nothing changes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_program_started_by_an_automatic_check_is_niced_and_pinned() {
        let allowed = allowed_cpus();
        assert!(!allowed.is_empty());
        let background = for_automatic_checks(|_| None, &allowed);
        let probe = "awk '{print $19}' /proc/self/stat; grep Cpus_allowed_list /proc/self/status";
        let run = || async {
            let out = crate::run::run_capped(
                "sh",
                &["-c".to_string(), probe.to_string()],
                &std::env::temp_dir(),
                std::time::Duration::from_secs(10),
            )
            .await
            .expect("ran");
            match out {
                crate::run::RunOutcome::Finished { stdout, .. } => stdout,
                other => panic!("{other:?}"),
            }
        };
        let inside = scope(background.clone(), run()).await;
        let mut lines = inside.lines();
        let nice: i32 = lines.next().unwrap().trim().parse().unwrap();
        let list = lines.next().unwrap().split(':').nth(1).unwrap().trim().to_string();
        let own_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        // setpriority sets an absolute value; an unprivileged process can only raise it.
        assert_eq!(nice, own_nice.max(DEFAULT_NICE), "{inside}");
        assert_eq!(parse_cpu_list(&list), background.cpus, "{inside}");
        if allowed.len() >= 2 {
            assert!(!parse_cpu_list(&list).unwrap().contains(&allowed[0]), "the first CPU stays free");
        }
        let outside = run().await;
        let nice_outside: i32 = outside.lines().next().unwrap().trim().parse().unwrap();
        assert_eq!(nice_outside, own_nice, "a tool call outside an automatic check runs as before");
        assert_eq!(scope(background.clone(), async { clamp_jobs(64) }).await, background.cpus.unwrap().len());
        assert_eq!(clamp_jobs(64), 64);
    }
}
