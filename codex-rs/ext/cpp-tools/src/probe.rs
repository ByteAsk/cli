//! Shared "is this external tool usable here?" probe.
//!
//! Every performance backend (perf, valgrind, pahole, taskset, …) runs a cheap
//! probe before use and walks a fallback ladder on a miss. The bottom of every
//! ladder is an actionable, per-OS message — never a hard error, never a silent
//! wrong answer. This module centralizes the three things that would otherwise
//! drift across ~9 call sites:
//!
//! - **spawn-error classification** — `NotFound` (not installed) vs a tool that
//!   ran-but-refused (blocked),
//! - **per-OS install hints** — [`install_hint`],
//! - **the perf-restriction matcher** — [`is_perf_restricted`] (generalized from
//!   `record.rs`, which now delegates here).
//!
//! ```text
//!   probe(tool, args, cwd) ──► Probe::Available
//!                          ├──► Probe::NotInstalled { install_hint }   (NotFound)
//!                          └──► Probe::Blocked { remediation }          (perf_event_paranoid, no PMU, …)
//! ```

use std::path::Path;
use std::process::Stdio;

/// Outcome of probing one external tool.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// The binary is present and a trivial invocation ran.
    Available,
    /// The binary was not found on `PATH`.
    NotInstalled { tool: String, install_hint: String },
    /// The binary ran but refused to work here (e.g. `perf_event_paranoid` too
    /// high, no PMU, ptrace restricted). `remediation` explains how to unblock.
    Blocked { tool: String, remediation: String },
}

impl Probe {
    pub fn is_available(&self) -> bool {
        matches!(self, Probe::Available)
    }

    /// A short "tool: reason (hint)" phrase for composing ladder-exhausted
    /// messages, or `None` when the tool is available.
    pub fn describe(&self) -> Option<String> {
        match self {
            Probe::Available => None,
            Probe::NotInstalled { tool, install_hint } => {
                Some(format!("{tool} not installed ({install_hint})"))
            }
            Probe::Blocked { tool, remediation } => {
                Some(format!("{tool} blocked ({remediation})"))
            }
        }
    }
}

/// Probe whether `tool` is usable by running it with `args` (a cheap,
/// side-effect-free form such as `--version` or `stat -- true`).
///
/// Classification:
/// - spawn `NotFound` → [`Probe::NotInstalled`] with a per-OS install hint,
/// - ran and exited 0 → [`Probe::Available`],
/// - ran, exited non-zero, and stderr matches the perf-restriction patterns →
///   [`Probe::Blocked`] (installed but the kernel/sandbox forbids it),
/// - ran, exited non-zero otherwise → [`Probe::Available`] (the binary exists;
///   this particular probe form failed benignly). Callers that need the probe
///   command itself to succeed (e.g. "is this actually a Google Benchmark
///   binary?") validate that separately.
///
/// A probe that never answers (a hung `pahole --version`, a wrapper
/// script waiting on a lock) is stopped after [`PROBE_TIMEOUT`] and counts as
/// [`Probe::Blocked`]. It used to be a bare `.output().await` with no cap, so
/// every tool that probes first — cpp_layout, cpp_perf, cpp_bench — waited
/// forever and the user's turn with it.
pub async fn probe(tool: &str, args: &[&str], cwd: &Path) -> Probe {
    let argv: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let wrapped = match crate::sandbox::wrap(tool, &argv, cwd) {
        Ok(wrapped) => wrapped,
        Err(_) => {
            return Probe::Blocked {
                tool: tool.to_string(),
                remediation: "ByteAsk's tool sandbox could not start here".to_string(),
            };
        }
    };
    let mut command = tokio::process::Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = match command.spawn() {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Probe::NotInstalled {
                tool: tool.to_string(),
                install_hint: install_hint(tool),
            };
        }
        Err(err) => {
            return Probe::Blocked {
                tool: tool.to_string(),
                remediation: format!("could not run {tool}: {err}"),
            };
        }
        Ok(child) => child,
    };
    let pid = child.id();
    let out = match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Err(_elapsed) => {
            // The direct child is killed on drop; its group goes too.
            crate::run::kill_process_group(pid);
            return Probe::Blocked {
                tool: tool.to_string(),
                remediation: format!(
                    "`{tool} {}` did not answer within {}s and was stopped, so {tool} is \
                     treated as unavailable here",
                    args.join(" "),
                    PROBE_TIMEOUT.as_secs()
                ),
            };
        }
        Ok(Err(err)) => {
            return Probe::Blocked {
                tool: tool.to_string(),
                remediation: format!("could not run {tool}: {err}"),
            };
        }
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return Probe::Available;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_perf_restricted(&stderr) {
        Probe::Blocked {
            tool: tool.to_string(),
            remediation: perf_restricted_remediation(),
        }
    } else {
        Probe::Available
    }
}

/// How long a probe (`--version` and the like) may take. A real one answers in
/// milliseconds. (2 s under test so the hang test is quick.)
const PROBE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(if cfg!(test) { 2 } else { 10 });

/// Per-OS install / availability hint for a known external tool.
pub fn install_hint(tool: &str) -> String {
    install_hint_for(tool, std::env::consts::OS)
}

/// [`install_hint`] for a named OS, so every OS's advice is testable on any
/// machine (a `cfg!`-only branch is never run by a Linux CI).
fn install_hint_for(tool: &str, os: &str) -> String {
    let hint: &str = match (tool, os) {
        ("perf", "linux") => "install perf: apt install linux-perf (or linux-tools-$(uname -r))",
        ("perf", _) => "perf is Linux-only and unavailable on this OS",
        ("valgrind", "linux") => "install valgrind: apt install valgrind",
        ("valgrind", "macos") => {
            "valgrind is effectively unavailable on modern macOS (especially Apple Silicon)"
        }
        ("valgrind", _) => "valgrind is unavailable on this OS",
        ("pahole", "linux") => "install pahole: apt install dwarves",
        ("pahole", _) => "pahole (dwarves package) is Linux-only",
        ("taskset", "linux") => "install taskset: apt install util-linux",
        ("taskset", _) => "taskset is Linux-only; runs will be unpinned",
        ("rr", "linux") => "install rr from https://rr-project.org",
        // rr is Linux-only (it depends on Linux perf counters, ptrace and
        // seccomp). Telling a macOS/Windows user to "install rr" sends them
        // after something that does not exist for their OS.
        ("rr", _) => {
            "rr is Linux-only (x86-64/aarch64) — record/replay debugging cannot run on this OS. \
             Use a Linux box, VM or container with hardware perf counters; for a data race use \
             cpp_sanitize with sanitizer=\"tsan\" instead"
        }
        // gdb does not run on Apple Silicon at all (it has no arm64-darwin
        // port), and on Intel Macs it needs code-signing before it can debug
        // anything. "Install gdb" sends a Mac user after something that
        // cannot work; lldb is the debugger there.
        ("gdb", "macos") => {
            "gdb does not run on Apple Silicon Macs (and needs code-signing on Intel ones): use \
             lldb, which comes with the Xcode Command Line Tools (xcode-select --install)"
        }
        ("gdb", "linux") => "install gdb: apt install gdb (or dnf install gdb)",
        ("lldb", "macos") => "install the Xcode Command Line Tools: xcode-select --install",
        ("lldb", "linux") => "install lldb: apt install lldb (or dnf install lldb)",
        ("clang" | "clang++", _) => "install clang (LLVM / your OS's C++ toolchain)",
        _ => return format!("install {tool} and ensure it is on PATH"),
    };
    hint.to_string()
}

/// Detect, from a failed invocation's stderr, that a perf-counter tool cannot
/// run here because hardware performance counters / ptrace are restricted (the
/// common sandbox case: `perf_event_paranoid` too high, no PMU, no
/// `CAP_SYS_ADMIN`). Generalized from `record.rs`; `record.rs` delegates here.
pub fn is_perf_restricted(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("perf_event_paranoid")
        || s.contains("perf event")
        || s.contains("performance counter")
        || s.contains("/proc/sys/kernel/perf")
        || s.contains("pmu")
        || s.contains("ptrace")
        || s.contains("cap_sys_admin")
}

/// Remediation shown when a perf-counter tool is installed but blocked.
pub fn perf_restricted_remediation() -> String {
    "needs hardware performance counters / relaxed perf_event_paranoid; try \
     sudo sysctl kernel.perf_event_paranoid=1 (or =-1). In containers/sandboxes this often \
     cannot be changed"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_probe_that_never_answers_is_stopped() {
        // `.output().await` with no cap — a hung `--version` blocked the
        // tool (and the turn) forever.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("hangs");
        std::fs::write(&tool, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        let out = probe(&tool.display().to_string(), &["--version"], dir.path()).await;
        assert!(
            start.elapsed() < PROBE_TIMEOUT + std::time::Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        match out {
            Probe::Blocked { remediation, .. } => {
                assert!(remediation.contains("did not answer"), "{remediation}")
            }
            other => panic!("a hung probe must not read as available: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_is_not_installed() {
        let p = probe("byteask-definitely-not-a-real-binary", &["--version"], &PathBuf::from(".")).await;
        match p {
            Probe::NotInstalled { tool, .. } => assert_eq!(tool, "byteask-definitely-not-a-real-binary"),
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn present_binary_is_available() {
        // `true` exists on every unix; exits 0.
        let p = probe("true", &[], &PathBuf::from(".")).await;
        assert_eq!(p, Probe::Available);
    }

    #[tokio::test]
    async fn nonzero_benign_exit_is_still_available() {
        // `false` exists and exits 1 with empty stderr → the binary is present.
        let p = probe("false", &[], &PathBuf::from(".")).await;
        assert_eq!(p, Probe::Available);
    }

    #[test]
    fn install_hint_is_os_aware_for_perf() {
        let hint = install_hint("perf");
        if std::env::consts::OS == "linux" {
            assert!(hint.contains("linux-perf") || hint.contains("linux-tools"));
        } else {
            assert!(hint.contains("Linux-only"));
        }
    }

    #[test]
    fn a_mac_is_never_told_to_install_gdb() {
        // A Mac user was told "gdb not installed" and nothing else; the
        // generic fallback would have said "install gdb", which cannot work
        // on Apple Silicon. Point them at lldb instead.
        let hint = install_hint_for("gdb", "macos");
        assert!(hint.contains("lldb"), "{hint}");
        assert!(hint.contains("xcode-select --install"), "{hint}");
        assert!(install_hint_for("gdb", "linux").contains("apt install gdb"));
        assert!(install_hint_for("lldb", "macos").contains("xcode-select --install"));
    }

    #[test]
    fn install_hint_is_os_aware_for_rr() {
        // Old code had no non-Linux arm for rr, so a macOS user was told to
        // "install rr and ensure it is on PATH" — advice they can never follow,
        // because rr does not exist for macOS at all.
        let hint = install_hint("rr");
        if std::env::consts::OS == "linux" {
            assert!(hint.contains("rr-project.org"), "{hint}");
        } else {
            assert!(hint.contains("Linux-only"), "{hint}");
            // and it must route to something that DOES work on their machine
            assert!(hint.contains("tsan"), "{hint}");
        }
    }

    #[test]
    fn install_hint_unknown_tool_falls_back() {
        assert!(install_hint("frobnicator").contains("frobnicator"));
    }

    #[test]
    fn detects_perf_paranoid_and_ptrace_and_pmu() {
        assert!(is_perf_restricted(
            "rr needs /proc/sys/kernel/perf_event_paranoid <= 1, but it is 3."
        ));
        assert!(is_perf_restricted("ptrace(PTRACE_TRACEME) failed: Operation not permitted"));
        assert!(is_perf_restricted("Unable to open performance counter (PMU not available)"));
        assert!(is_perf_restricted("requires CAP_SYS_ADMIN"));
    }

    #[test]
    fn does_not_flag_unrelated_failures() {
        assert!(!is_perf_restricted(
            "perf: error: Unable to execute '/work/a.out': No such file"
        ));
    }

    #[test]
    fn describe_available_is_none() {
        assert!(Probe::Available.describe().is_none());
    }

    #[test]
    fn describe_not_installed_phrases_tool_and_hint() {
        let p = Probe::NotInstalled {
            tool: "perf".to_string(),
            install_hint: "install perf".to_string(),
        };
        assert_eq!(p.describe().unwrap(), "perf not installed (install perf)");
    }

    #[test]
    fn describe_blocked_phrases_remediation() {
        let p = Probe::Blocked {
            tool: "perf".to_string(),
            remediation: "raise paranoid".to_string(),
        };
        assert_eq!(p.describe().unwrap(), "perf blocked (raise paranoid)");
    }
}
