//! Launching one measured process: fork, move into the isolated cgroup, pin
//! to one CPU, drop to the calling user, wait for the parent to arm the
//! counters (enable-on-exec), then exec. Everything the child does between
//! fork and exec is async-signal-safe; all allocation happens before fork.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// Who the job runs as (the user behind `sudo`).
#[derive(Debug, Clone)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub name: String,
    pub home: String,
}

impl Creds {
    /// The invoking user when we run under sudo; `None` when not root or
    /// when root was not reached through sudo.
    pub fn from_sudo() -> Option<Creds> {
        let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
        let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
        Creds::for_uid(uid, gid)
    }

    pub fn for_uid(uid: u32, gid: u32) -> Option<Creds> {
        // SAFETY: getpwuid returns a pointer into static storage or null; we
        // copy the fields out immediately and are single-threaded here.
        let pw = unsafe { libc::getpwuid(uid) };
        if pw.is_null() {
            return None;
        }
        let (name, home) = unsafe {
            (
                std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned(),
                std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned(),
            )
        };
        let cname = CString::new(name.clone()).ok()?;
        let mut n: libc::c_int = 64;
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        loop {
            // SAFETY: buffer of n entries; getgrouplist updates n.
            let r = unsafe { libc::getgrouplist(cname.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
            if r >= 0 {
                groups.truncate(n as usize);
                break;
            }
            groups.resize(n as usize + 16, 0);
            n = groups.len() as libc::c_int;
        }
        Some(Creds { uid, gid, groups, name, home })
    }
}

pub struct Job<'a> {
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a [(String, String)],
    /// Pin to these CPUs (usually one).
    pub cpus: &'a [usize],
    /// cgroup.procs of the isolated partition; `None` = stay where we are.
    pub cgroup_procs: Option<&'a Path>,
    pub creds: Option<&'a Creds>,
    pub stdout: &'a Path,
    pub stderr: &'a Path,
    /// Arm cycles / instructions / task-clock on the child.
    pub counters: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub wall_s: f64,
    pub user_s: f64,
    pub sys_s: f64,
    pub max_rss_kb: i64,
    pub voluntary_switches: i64,
    pub involuntary_switches: i64,
    pub cycles: Option<u64>,
    pub instructions: Option<u64>,
    pub task_clock_ns: Option<u64>,
    /// A counter ran less than its whole enabled time (multiplexed).
    pub counters_multiplexed: bool,
    pub counter_error: Option<String>,
}

impl RunResult {
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }

    /// cycles / task-clock: the effective core frequency while the job ran.
    pub fn freq_ghz(&self) -> Option<f64> {
        match (self.cycles, self.task_clock_ns) {
            (Some(c), Some(t)) if t > 1_000_000 => Some(c as f64 / t as f64),
            _ => None,
        }
    }

    pub fn describe_failure(&self) -> String {
        if self.timed_out {
            "timed out (killed)".to_string()
        } else if let Some(sig) = self.signal {
            format!("killed by signal {sig}")
        } else {
            format!("exit status {}", self.exit_code.unwrap_or(-1))
        }
    }
}

/// Resolve argv[0] like execvp would, against the job's PATH and cwd.
pub fn resolve_program(prog: &str, cwd: &Path, env: &[(String, String)]) -> Result<PathBuf, String> {
    if prog.contains('/') {
        let p = if Path::new(prog).is_absolute() { PathBuf::from(prog) } else { cwd.join(prog) };
        if is_executable(&p) {
            return Ok(p);
        }
        return Err(format!("{} is not an executable file", p.display()));
    }
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("/usr/local/bin:/usr/bin:/bin");
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(prog);
        if is_executable(&p) {
            return Ok(p);
        }
    }
    Err(format!("{prog}: command not found on PATH"))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

// ---- perf_event_open ---------------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct PerfAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
}

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_SOFTWARE: u32 = 1;
const HW_CPU_CYCLES: u64 = 0;
const HW_INSTRUCTIONS: u64 = 1;
const SW_TASK_CLOCK: u64 = 1;
const FLAG_DISABLED: u64 = 1 << 0;
const FLAG_INHERIT: u64 = 1 << 1;
const FLAG_EXCLUDE_HV: u64 = 1 << 6;
const FLAG_ENABLE_ON_EXEC: u64 = 1 << 12;
const FORMAT_TIMES: u64 = 1 | 2; // TOTAL_TIME_ENABLED | TOTAL_TIME_RUNNING
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 8;

fn perf_open(type_: u32, config: u64, pid: libc::pid_t) -> Result<libc::c_int, std::io::Error> {
    let attr = PerfAttr {
        type_,
        size: std::mem::size_of::<PerfAttr>() as u32,
        config,
        read_format: FORMAT_TIMES,
        flags: FLAG_DISABLED | FLAG_INHERIT | FLAG_EXCLUDE_HV | FLAG_ENABLE_ON_EXEC,
        ..Default::default()
    };
    // SAFETY: attr is a valid perf_event_attr prefix of the declared size.
    let fd = unsafe {
        libc::syscall(libc::SYS_perf_event_open, &attr as *const PerfAttr, pid, -1 as libc::c_int, -1 as libc::c_int, PERF_FLAG_FD_CLOEXEC)
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd as libc::c_int)
    }
}

/// (value, multiplexed) from a counter fd opened with FORMAT_TIMES.
fn perf_read(fd: libc::c_int) -> Option<(u64, bool)> {
    let mut buf = [0u64; 3];
    // SAFETY: reading 24 bytes into a 24-byte buffer.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), 24) };
    if n != 24 {
        return None;
    }
    let (value, enabled, running) = (buf[0], buf[1], buf[2]);
    Some((value, running < enabled))
}

// ---- the launcher --------------------------------------------------------------

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("argument contains a NUL byte: {s:?}"))
}

/// Child-side failure report: stage byte + errno.
const STAGE_CGROUP: u8 = 1;
const STAGE_AFFINITY: u8 = 2;
const STAGE_CHDIR: u8 = 3;
const STAGE_CREDS: u8 = 4;
const STAGE_EXEC: u8 = 5;
const STAGE_DUP: u8 = 6;

/// Report a failed child stage to the parent and exit. Async-signal-safe.
unsafe fn child_fail(rep_w: libc::c_int, stage: u8) -> ! {
    unsafe {
        let e = *libc::__errno_location();
        let mut msg = [0u8; 5];
        msg[0] = stage;
        msg[1..].copy_from_slice(&e.to_ne_bytes());
        libc::write(rep_w, msg.as_ptr().cast(), 5);
        libc::_exit(127)
    }
}

pub fn run(job: &Job) -> Result<RunResult, String> {
    let program = resolve_program(&job.argv[0], job.cwd, job.env)?;
    let path_c = CString::new(program.as_os_str().as_bytes()).map_err(|_| "program path has a NUL byte".to_string())?;
    let argv_c: Vec<CString> = job.argv.iter().map(|a| cstr(a)).collect::<Result<_, _>>()?;
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let env_c: Vec<CString> = job.env.iter().map(|(k, v)| cstr(&format!("{k}={v}"))).collect::<Result<_, _>>()?;
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());
    let cwd_c = CString::new(job.cwd.as_os_str().as_bytes()).map_err(|_| "cwd has a NUL byte".to_string())?;

    // SAFETY: plain cpu_set_t manipulation.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &c in job.cpus {
        unsafe { libc::CPU_SET(c, &mut set) };
    }

    // Created exclusively without following links, then given to the job.
    let out = crate::fsutil::create_root_file(job.stdout, job.creds)?;
    let err = if job.stderr == job.stdout { out.try_clone().map_err(|e| format!("dup: {e}"))? } else { crate::fsutil::create_root_file(job.stderr, job.creds)? };
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let cg = match job.cgroup_procs {
        Some(p) => Some(std::fs::OpenOptions::new().write(true).open(p).map_err(|e| format!("open {}: {e}", p.display()))?),
        None => None,
    };
    let groups: Vec<libc::gid_t> = job.creds.map(|c| c.groups.clone()).unwrap_or_default();

    let mut go = [0 as libc::c_int; 2];
    let mut report = [0 as libc::c_int; 2];
    // SAFETY: pipe2 into 2-int arrays.
    unsafe {
        if libc::pipe2(go.as_mut_ptr(), libc::O_CLOEXEC) != 0 || libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
        }
    }
    let (go_r, go_w, rep_r, rep_w) = (go[0], go[1], report[0], report[1]);
    let out_fd = out.as_raw_fd();
    let err_fd = err.as_raw_fd();
    let null_fd = null.as_raw_fd();
    let cg_fd = cg.as_ref().map(|f| f.as_raw_fd());
    let creds = job.creds.map(|c| (c.uid, c.gid));

    // SAFETY: fork in a single-threaded process; the child only makes
    // async-signal-safe calls before execve or _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            libc::setpgid(0, 0);
            if let Some(fd) = cg_fd
                && libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                    child_fail(rep_w, STAGE_CGROUP);
                }
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                child_fail(rep_w, STAGE_AFFINITY);
            }
            if libc::dup2(null_fd, 0) < 0 || libc::dup2(out_fd, 1) < 0 || libc::dup2(err_fd, 2) < 0 {
                child_fail(rep_w, STAGE_DUP);
            }
            if libc::chdir(cwd_c.as_ptr()) != 0 {
                child_fail(rep_w, STAGE_CHDIR);
            }
            if let Some((uid, gid)) = creds
                && (libc::setgroups(groups.len(), groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0) {
                    child_fail(rep_w, STAGE_CREDS);
                }
            let mut b = 0u8;
            if libc::read(go_r, (&mut b as *mut u8).cast(), 1) != 1 {
                libc::_exit(126);
            }
            libc::execve(path_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            child_fail(rep_w, STAGE_EXEC);
        }
    }

    // Parent.
    unsafe {
        libc::close(go_r);
        libc::close(rep_w);
    }
    let mut fds: Vec<libc::c_int> = Vec::new();
    let mut counter_error = None;
    if job.counters {
        for (t, c) in [(PERF_TYPE_HARDWARE, HW_CPU_CYCLES), (PERF_TYPE_HARDWARE, HW_INSTRUCTIONS), (PERF_TYPE_SOFTWARE, SW_TASK_CLOCK)] {
            match perf_open(t, c, pid) {
                Ok(fd) => fds.push(fd),
                Err(e) => {
                    counter_error = Some(format!("perf_event_open: {e}"));
                    break;
                }
            }
        }
        if counter_error.is_some() {
            for fd in fds.drain(..) {
                unsafe { libc::close(fd) };
            }
        }
    }

    let start = Instant::now();
    unsafe {
        libc::write(go_w, b"g".as_ptr().cast(), 1);
        libc::close(go_w);
    }
    // EOF = exec succeeded (CLOEXEC); 5 bytes = a stage failed.
    let mut msg = [0u8; 5];
    let n = unsafe { libc::read(rep_r, msg.as_mut_ptr().cast(), 5) };
    unsafe { libc::close(rep_r) };

    // Watchdog: kill the whole process group at the deadline.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let timeout = job.timeout;
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired2 = fired.clone();
    let watchdog = std::thread::spawn(move || {
        if rx.recv_timeout(timeout).is_err() {
            fired2.store(true, std::sync::atomic::Ordering::SeqCst);
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    });

    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if r == pid {
            break;
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let _ = tx.send(());
    let _ = watchdog.join();
    // Reap anything left in the group (a daemonised child).
    unsafe { libc::kill(-pid, libc::SIGKILL) };

    if n == 5 {
        let stage = msg[0];
        let errno = i32::from_ne_bytes([msg[1], msg[2], msg[3], msg[4]]);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        let what = match stage {
            STAGE_CGROUP => "moving into the isolated cgroup",
            STAGE_AFFINITY => "pinning to the measured CPU",
            STAGE_CHDIR => "changing directory",
            STAGE_CREDS => "dropping to the calling user",
            STAGE_DUP => "redirecting output",
            _ => "exec",
        };
        return Err(format!("{} failed while {what}: {}", program.display(), std::io::Error::from_raw_os_error(errno)));
    }

    let mut r = RunResult {
        wall_s: wall,
        timed_out: fired.load(std::sync::atomic::Ordering::SeqCst),
        user_s: ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 * 1e-6,
        sys_s: ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 * 1e-6,
        max_rss_kb: ru.ru_maxrss,
        voluntary_switches: ru.ru_nvcsw,
        involuntary_switches: ru.ru_nivcsw,
        counter_error,
        ..Default::default()
    };
    if libc::WIFEXITED(status) {
        r.exit_code = Some(libc::WEXITSTATUS(status));
    } else if libc::WIFSIGNALED(status) {
        r.signal = Some(libc::WTERMSIG(status));
    }
    if fds.len() == 3 {
        let vals: Vec<Option<(u64, bool)>> = fds.iter().map(|&fd| perf_read(fd)).collect();
        r.cycles = vals[0].map(|v| v.0);
        r.instructions = vals[1].map(|v| v.0);
        r.task_clock_ns = vals[2].map(|v| v.0);
        r.counters_multiplexed = vals.iter().any(|v| v.is_some_and(|v| v.1));
    }
    for fd in fds {
        unsafe { libc::close(fd) };
    }
    drop((out, err, null, cg));
    Ok(r)
}

/// Split a command line like a shell would (quotes and backslashes; no
/// expansion, pipes or redirections).
pub fn split_command(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated ' in: {s}")),
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err(format!("unterminated \" in: {s}")),
                        },
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated \" in: {s}")),
                    }
                }
            }
            '\\' => {
                have = true;
                if let Some(x) = chars.next() {
                    cur.push(x);
                }
            }
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '|' | '<' | '>' | ';' | '&' => {
                return Err(format!("the benchmark command is run directly, not through a shell; {c:?} is not supported: {s}"));
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    if out.is_empty() {
        return Err("empty command".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split() {
        assert_eq!(split_command("./b --feed 'a b' \"c\\\"d\" e\\ f").unwrap(), vec!["./b", "--feed", "a b", "c\"d", "e f"]);
        assert!(split_command("a | b").is_err());
        assert!(split_command("  ").is_err());
        assert!(split_command("'x").is_err());
        assert_eq!(split_command("x ''").unwrap(), vec!["x", ""]);
    }

    #[test]
    fn runs_a_process_and_reports_exit_status() {
        let dir = std::env::temp_dir().join(format!("perflab-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env: Vec<(String, String)> = std::env::vars().collect();
        let argv = vec!["sh".to_string(), "-c".to_string(), "echo hi; exit 3".to_string()];
        let cpu = [0usize];
        let job = Job {
            argv: &argv,
            cwd: &dir,
            env: &env,
            cpus: &cpu,
            cgroup_procs: None,
            creds: None,
            stdout: &dir.join("o"),
            stderr: &dir.join("e"),
            counters: false,
            timeout: Duration::from_secs(10),
        };
        let r = run(&job).unwrap();
        assert_eq!(r.exit_code, Some(3));
        assert_eq!(std::fs::read_to_string(dir.join("o")).unwrap(), "hi\n");
        // Timeout kills.
        let argv = vec!["sleep".to_string(), "5".to_string()];
        let job = Job { argv: &argv, timeout: Duration::from_millis(200), ..job };
        let r = run(&job).unwrap();
        assert!(r.timed_out);
        assert!(!r.ok());
        // Missing program is an error, not a run.
        let argv = vec!["/nonexistent/x".to_string()];
        let job = Job { argv: &argv, ..job };
        assert!(run(&job).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
