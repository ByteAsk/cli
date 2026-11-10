//! Process-group helpers shared by pipe/pty and shell command execution.
//!
//! This module centralizes the OS-specific pieces that ensure a spawned
//! command can be cleaned up reliably:
//! - `set_process_group` is called in `pre_exec` so the child starts its own
//!   process group.
//! - `detach_from_tty` starts a new session so non-interactive children do not
//!   inherit the controlling TTY.
//! - `kill_process_group_by_pid` targets the whole group (children/grandchildren)
//! - `kill_process_group` targets a known process group ID directly
//!   instead of a single PID.
//! - `set_parent_death_signal` (Linux only) arranges for the child to receive a
//!   `SIGTERM` when the parent exits, and re-checks the parent PID to avoid
//!   races during fork/exec.
//!
//! On macOS, `terminate_process_group` and `kill_process_group` retry denied
//! group signals against individual members.
//! On non-Unix platforms these helpers are no-ops.

use std::io;

use tokio::process::Child;

#[cfg(target_os = "linux")]
/// Ensure the child receives SIGTERM when the original parent dies.
///
/// This should run in `pre_exec` and uses `parent_pid` captured before spawn to
/// avoid a race where the parent exits between fork and exec.
pub fn set_parent_death_signal(parent_pid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::getppid() } != parent_pid {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
/// No-op on non-Linux platforms.
pub fn set_parent_death_signal(_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

/// Switch (the CLI sets 500 at start unless it is already set): `BYTEASK_CHILD_OOM_SCORE_ADJ=<-1000..1000>` makes every command the
/// engine starts (and every program that command starts, which inherits it) begin with that Linux
/// `oom_score_adj`, so that when memory runs out the kernel kills the command before the engine. Unset, or not a
/// whole number in range: nothing is written.
pub const ENV_CHILD_OOM_SCORE_ADJ: &str = "BYTEASK_CHILD_OOM_SCORE_ADJ";

/// An `oom_score_adj` value as the bytes `/proc/<pid>/oom_score_adj` takes, made before `fork` so that the
/// child writes it without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OomScoreAdj {
    text: [u8; 8],
    len: u8,
}

impl OomScoreAdj {
    /// A whole number in `-1000..=1000`, else `None`.
    pub fn parse(value: &str) -> Option<Self> {
        let number: i32 = value.trim().parse().ok()?;
        if !(-1000..=1000).contains(&number) {
            return None;
        }
        let digits = number.to_string();
        let mut text = [0u8; 8];
        text.get_mut(..digits.len())?
            .copy_from_slice(digits.as_bytes());
        Some(Self {
            text,
            len: u8::try_from(digits.len()).ok()?,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.text[..usize::from(self.len)]
    }
}

/// The value [`ENV_CHILD_OOM_SCORE_ADJ`] asks for, read in the parent before a spawn.
pub fn child_oom_score_adj() -> Option<OomScoreAdj> {
    std::env::var(ENV_CHILD_OOM_SCORE_ADJ)
        .ok()
        .as_deref()
        .and_then(OomScoreAdj::parse)
}

#[cfg(target_os = "linux")]
/// Set the calling process's `oom_score_adj`, for `pre_exec`: only open(2), write(2) and close(2) on a fixed
/// path, nothing allocated. Best effort: an unprivileged process may raise its value, not lower it.
pub fn set_own_oom_score_adj(value: OomScoreAdj) {
    let bytes = value.as_bytes();
    unsafe {
        let fd = libc::open(
            b"/proc/self/oom_score_adj\0"
                .as_ptr()
                .cast::<libc::c_char>(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if fd >= 0 {
            let _ = libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
            libc::close(fd);
        }
    }
}

#[cfg(target_os = "linux")]
/// The same for a child that is already running (a spawn path with no `pre_exec` hook), written by the parent.
pub fn set_oom_score_adj_of(pid: u32, value: OomScoreAdj) {
    let _ = std::fs::write(format!("/proc/{pid}/oom_score_adj"), value.as_bytes());
}

#[cfg(unix)]
/// Detach from the controlling TTY by starting a new session.
pub fn detach_from_tty() -> io::Result<()> {
    let result = unsafe { libc::setsid() };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            return set_process_group();
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Put the calling process into its own process group.
///
/// Intended for use in `pre_exec` so the child becomes the group leader.
pub fn set_process_group() -> io::Result<()> {
    let result = unsafe { libc::setpgid(0, 0) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn set_process_group() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Kill the process group for the given PID (best-effort).
///
/// This resolves the PGID for `pid` and sends SIGKILL to the whole group.
pub fn kill_process_group_by_pid(pid: u32) -> io::Result<()> {
    use std::io::ErrorKind;

    let pid = pid as libc::pid_t;
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
        return Ok(());
    }

    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_process_group_by_pid(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn signal_process_group_id(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    use std::io::ErrorKind;

    let result = unsafe { libc::killpg(pgid, signal) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() == ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(err);
    }

    Ok(true)
}

#[cfg(target_os = "macos")]
fn signal_process_id(pid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    if unsafe { libc::kill(pid, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error);
    }

    Ok(true)
}

#[cfg(target_os = "macos")]
fn signal_process_group_with_member_fallback(
    process_group_id: u32,
    signal: libc::c_int,
    signal_group: impl FnOnce(libc::pid_t, libc::c_int) -> io::Result<bool>,
    mut signal_member: impl FnMut(libc::pid_t, libc::c_int) -> io::Result<bool>,
) -> io::Result<bool> {
    let process_group_id = libc::pid_t::try_from(process_group_id)
        .ok()
        .filter(|process_group_id| *process_group_id > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group ID"))?;

    match signal_group(process_group_id, signal) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => return result,
    }

    let mut process_ids: Vec<libc::pid_t> = vec![0; 16];
    loop {
        let buffer_size = libc::c_int::try_from(std::mem::size_of_val(process_ids.as_slice()))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
            })?;
        let count = unsafe {
            libc::proc_listpgrppids(
                process_group_id,
                process_ids.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count < process_ids.len() {
            process_ids.truncate(count);
            break;
        }
        let capacity = process_ids.len().checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
        })?;
        process_ids.resize(capacity, 0);
    }
    process_ids.sort_unstable_by_key(|process_id| *process_id == process_group_id);

    let mut signalled = false;
    let mut first_error = None;
    for process_id in process_ids {
        if process_id <= 0 {
            continue;
        }
        let current_group_id = unsafe { libc::getpgid(process_id) };
        if current_group_id == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) && first_error.is_none() {
                first_error = Some(error);
            }
            continue;
        }
        if current_group_id != process_group_id {
            continue;
        }
        match signal_member(process_id, signal) {
            Ok(delivered) => signalled |= delivered,
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if signalled {
        Ok(true)
    } else {
        first_error.map_or(Ok(false), Err)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Send SIGTERM to a specific process group ID (best-effort).
///
/// Returns `Ok(true)` when SIGTERM was delivered to an existing group and
/// `Ok(false)` when the group no longer exists.
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGTERM)
}

#[cfg(target_os = "macos")]
/// Send SIGTERM to a specific process group, retrying denied signals against its members.
///
/// Returns `Ok(true)` when the group or at least one member was signalled.
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_with_member_fallback(
        process_group_id,
        libc::SIGTERM,
        signal_process_group_id,
        signal_process_id,
    )
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
/// Send SIGINT to a specific process group ID (best-effort).
pub fn interrupt_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGINT).map(|_| ())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Kill a specific process group ID (best-effort).
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGKILL).map(|_| ())
}

#[cfg(target_os = "macos")]
/// Kill a specific process group, retrying denied signals against its members (best-effort).
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_with_member_fallback(
        process_group_id,
        libc::SIGKILL,
        signal_process_group_id,
        signal_process_id,
    )
    .map(|_| ())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

/// Wait up to `grace` for a signaled group's members to release their resources.
/// After that, allow the caller to proceed. An unreaped zombie or a recycled group ID
/// can remain visible after the original processes have released their resources.
#[cfg(unix)]
pub fn wait_for_process_group_exit(process_group_id: u32, grace: std::time::Duration) {
    let deadline = std::time::Instant::now() + grace;
    loop {
        #[cfg(target_os = "macos")]
        let exists = signal_process_group_with_member_fallback(
            process_group_id,
            /*signal*/ 0,
            signal_process_group_id,
            signal_process_id,
        );
        #[cfg(not(target_os = "macos"))]
        let exists = signal_process_group_id(process_group_id as libc::pid_t, /*signal*/ 0);
        if matches!(exists, Ok(false)) {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
    }
}

#[cfg(unix)]
/// Kill the process group for a tokio child (best-effort).
pub fn kill_child_process_group(child: &mut Child) -> io::Result<()> {
    if let Some(pid) = child.id() {
        return kill_process_group_by_pid(pid);
    }

    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_child_process_group(_child: &mut Child) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
#[path = "process_group_tests.rs"]
mod tests;
