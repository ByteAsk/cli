//! Shared pieces of the runtime tools: `debug_reverse`, `debug_core`,
//! and the trace hand-off from `debug_record`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The shared verdict for a run that did not happen: the
/// reason, and the one line that makes it run. Never shown as a result.
pub(crate) fn not_run(reason: &str, fix: &str) -> String {
    format!("RESULT: NOT RUN — {reason}\nFIX: {fix}\n")
}

/// The shared result for a runtime tool's answer, read
/// from the markers the tools already write. A body holding [`not_run`]'s `RESULT: NOT RUN`
/// is NOT RUN with that reason and fix; a partial answer (`INCOMPLETE`, `TIMED OUT`, or cut at
/// the output cap) is APPROXIMATE; anything else answered the question: PASS with `summary`.
pub(crate) fn verdict(
    tool: &str,
    body: &str,
    cut: bool,
    summary: &str,
) -> check_result::CheckResult {
    use check_result::CheckResult;
    use check_result::NotRunReason;
    if let Some(at) = body.find("RESULT: NOT RUN — ") {
        let rest = &body[at + "RESULT: NOT RUN — ".len()..];
        let reason_text = rest
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches('.');
        let fix = rest
            .lines()
            .find_map(|line| line.strip_prefix("FIX: "))
            .unwrap_or_default()
            .trim();
        let lower = reason_text.to_ascii_lowercase();
        let reason = if lower.contains("did not") && lower.contains("within") {
            NotRunReason::TimeLimit
        } else if lower.contains("not installed") || lower.contains("without python") {
            NotRunReason::ToolMissing
        } else if lower.contains("cannot replay here") || lower.contains("not supported") {
            NotRunReason::Unsupported
        } else {
            NotRunReason::ToolError
        };
        return CheckResult::not_run(tool, reason, &format!("{reason_text}."), fix);
    }
    if cut || body.contains("INCOMPLETE") || body.contains("TIMED OUT after") {
        return CheckResult::approximate(
            tool,
            "The answer above is partial (incomplete, timed out or cut), so it is not the whole history.",
        );
    }
    CheckResult::pass(tool, summary)
}

/// A private directory for one tool call, removed when dropped.
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(tag: &str) -> std::io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "byteask-{tag}-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Linux x86-64 / aarch64 signal names (the numbers rr and the kernel report).
pub(crate) fn signal_name(signo: i32) -> Option<&'static str> {
    Some(match signo {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        16 => "SIGSTKFLT",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        31 => "SIGSYS",
        _ => return None,
    })
}

/// Where rr keeps recordings, by rr's own rule: `$_RR_TRACE_DIR`, else a
/// legacy `~/.rr` that exists, else `$XDG_DATA_HOME/rr`, else
/// `~/.local/share/rr`.
pub(crate) fn rr_trace_root() -> Option<PathBuf> {
    let env = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    trace_root_from(env("_RR_TRACE_DIR"), env("HOME"), env("XDG_DATA_HOME"))
}

fn trace_root_from(
    explicit: Option<PathBuf>,
    home: Option<PathBuf>,
    xdg_data: Option<PathBuf>,
) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    if let Some(home) = &home {
        let legacy = home.join(".rr");
        if legacy.is_dir() {
            return Some(legacy);
        }
    }
    if let Some(xdg) = xdg_data {
        return Some(xdg.join("rr"));
    }
    home.map(|h| h.join(".local/share/rr"))
}

/// The recording rr made last (`<root>/latest-trace`), resolved to its real
/// directory so a later recording cannot change what a caller is pointed at.
pub(crate) fn latest_trace() -> Option<PathBuf> {
    let link = rr_trace_root()?.join("latest-trace");
    std::fs::canonicalize(link).ok().filter(|p| is_rr_trace(p))
}

/// Point rr's `<root>/latest-trace` link at `trace` (a directory under
/// `root`), replacing it atomically: a new link under a temporary name,
/// renamed over the old one, so a reader never sees it missing.
pub(crate) fn point_latest_trace_at(root: &Path, trace: &Path) -> std::io::Result<()> {
    let target = trace.strip_prefix(root).unwrap_or(trace);
    let tmp = root.join(format!(".latest-trace.byteask-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &tmp)?;
        std::fs::rename(&tmp, root.join("latest-trace"))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, tmp);
        Err(std::io::Error::other("rr runs on Linux only"))
    }
}

/// Does this directory look like an rr recording? Every rr trace holds a
/// `version` file next to its `events` stream.
pub(crate) fn is_rr_trace(dir: &Path) -> bool {
    dir.join("version").is_file() && dir.join("events").exists()
}

/// One process of a recording, from `rr ps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceProcess {
    pub pid: i64,
    /// Exit status; negative = killed by that signal; `None` = unknown (the
    /// recording was cut short).
    pub exit: Option<i32>,
    pub command: String,
}

/// Parse `rr ps <trace>`: a header, then `PID PPID EXIT CMD` tab-separated.
pub(crate) fn parse_rr_ps(text: &str) -> Vec<TraceProcess> {
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("PID"))
        .filter_map(|line| {
            let mut cols = line.splitn(4, '\t');
            let pid = cols.next()?.trim().parse().ok()?;
            let _ppid = cols.next()?;
            let exit = cols.next()?.trim().parse().ok();
            let command = cols.next().unwrap_or("").trim().to_string();
            Some(TraceProcess { pid, exit, command })
        })
        .collect()
}

/// Describe how a recorded process ended, for the first line of an answer.
pub(crate) fn describe_exit(exit: Option<i32>) -> String {
    match exit {
        Some(code) if code < 0 => match signal_name(-code) {
            Some(name) => format!("ended by {name}"),
            None => format!("ended by signal {}", -code),
        },
        Some(code) => format!("exited with status {code}"),
        None => "did not finish inside the recording".to_string(),
    }
}

/// Show a path relative to the workspace when it is inside it.
pub(crate) fn display_path(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    match p.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.to_string(),
    }
}

/// Line `line` (1-based) of a source file, trimmed and capped, when the file
/// is readable here.
pub(crate) fn source_line(path: &str, line: u64) -> Option<String> {
    if line == 0 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let raw = text.lines().nth((line - 1) as usize)?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut shown: String = raw.chars().take(160).collect();
    if raw.chars().count() > 160 {
        shown.push('…');
    }
    Some(shown)
}

/// Cap a rendered answer on a char boundary.
pub(crate) fn cap(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... [output truncated]");
    text
}

/// The last `n` non-empty lines of `text`, for quoting a tool's own error.
pub(crate) fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// Integration: the shared result for the runtime tools comes from their own markers;
    /// nothing that did not answer the question can read as PASS.
    #[test]
    fn runtime_answers_get_the_shared_result_and_only_a_full_answer_passes() {
        use check_result::NotRunReason;
        use check_result::Verdict;
        let timed = verdict(
            "debug_reverse",
            &format!(
                "header\n{}",
                not_run(
                    "the replay did not reach the stop point within 30s",
                    "raise `timeout_sec`"
                )
            ),
            false,
            "x",
        );
        assert_eq!(timed.verdict, Verdict::NotRun);
        assert_eq!(timed.reason, Some(NotRunReason::TimeLimit));
        assert_eq!(timed.fix.as_deref(), Some("raise `timeout_sec`"));
        timed.validate().expect("contract");
        let missing = verdict(
            "debug_core",
            &not_run("gdb is not installed here", "install gdb"),
            false,
            "x",
        );
        assert_eq!(missing.reason, Some(NotRunReason::ToolMissing));
        let no_pmu = verdict(
            "debug_reverse",
            "RESULT: NOT RUN — rr cannot replay here.\nno PMU\n",
            false,
            "x",
        );
        assert_eq!(no_pmu.reason, Some(NotRunReason::Unsupported));
        no_pmu.validate().expect("a NOT RUN always has a fix");
        for partial in [
            "writes:\nTIMED OUT after 9s: the answer above is INCOMPLETE",
            "INCOMPLETE: the replay ended",
        ] {
            assert_eq!(
                verdict("debug_reverse", partial, false, "x").verdict,
                Verdict::Approximate
            );
        }
        assert_eq!(
            verdict("debug_core", "a full answer", true, "x").verdict,
            Verdict::Approximate
        );
        let full = verdict(
            "debug_core",
            "LIKELY CAUSE: null deref",
            false,
            "Triaged the core file.",
        );
        assert!(full.is_pass());
        assert_eq!(full.summary, "Triaged the core file.");
    }

    #[test]
    fn not_run_always_carries_a_fix_line() {
        let text = not_run("no Intel PT here", "run on bare metal");
        assert!(
            text.starts_with("RESULT: NOT RUN — no Intel PT here\n"),
            "{text}"
        );
        assert!(text.contains("\nFIX: run on bare metal"), "{text}");
    }

    #[test]
    fn trace_root_follows_rrs_own_order() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_path_buf();
        assert_eq!(
            trace_root_from(Some("/x".into()), Some(h.clone()), Some("/xdg".into())),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            trace_root_from(None, Some(h.clone()), Some("/xdg".into())),
            Some(PathBuf::from("/xdg/rr"))
        );
        assert_eq!(
            trace_root_from(None, Some(h.clone()), None),
            Some(h.join(".local/share/rr"))
        );
        std::fs::create_dir(h.join(".rr")).unwrap();
        assert_eq!(
            trace_root_from(None, Some(h.clone()), Some("/xdg".into())),
            Some(h.join(".rr"))
        );
        assert_eq!(trace_root_from(None, None, None), None);
    }

    #[test]
    fn rr_ps_rows_are_read_with_signal_exits_and_spaces_in_commands() {
        // Measured output of rr 5.10 `rr ps` on a machine with a PMU.
        let text = "PID\tPPID\tEXIT\tCMD\n4219\t--\t-6\t./book\n4300\t4219\t0\t/bin/sh -c echo hi\n4301\t--\t--\t./cut\n";
        let rows = parse_rr_ps(text);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            TraceProcess {
                pid: 4219,
                exit: Some(-6),
                command: "./book".into()
            }
        );
        assert_eq!(rows[1].command, "/bin/sh -c echo hi");
        assert_eq!(rows[2].exit, None);
        assert_eq!(describe_exit(Some(-6)), "ended by SIGABRT");
        assert_eq!(describe_exit(Some(3)), "exited with status 3");
        assert_eq!(describe_exit(None), "did not finish inside the recording");
    }

    #[test]
    fn scratch_dir_is_private_and_removed_on_drop() {
        let path;
        {
            let dir = ScratchDir::new("t").unwrap();
            path = dir.path().to_path_buf();
            assert!(path.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700);
            }
        }
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn latest_trace_is_repointed_atomically_and_relatively() {
        let root = tempfile::tempdir().unwrap();
        for name in ["old-0", "feed-byteask-1"] {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("version"), "85\n").unwrap();
            std::fs::write(dir.join("events"), "").unwrap();
        }
        point_latest_trace_at(root.path(), &root.path().join("old-0")).unwrap();
        point_latest_trace_at(root.path(), &root.path().join("feed-byteask-1")).unwrap();
        let link = root.path().join("latest-trace");
        // Relative, like rr's own link, so the trace root can be moved.
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            PathBuf::from("feed-byteask-1")
        );
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(root.path().join("feed-byteask-1")).unwrap()
        );
        // No temporary link left behind.
        let leftovers = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".latest-trace"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_trace_dir_needs_version_and_events() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_rr_trace(dir.path()));
        std::fs::write(dir.path().join("version"), "85\n").unwrap();
        assert!(!is_rr_trace(dir.path()));
        std::fs::write(dir.path().join("events"), "").unwrap();
        assert!(is_rr_trace(dir.path()));
    }

    #[test]
    fn source_lines_and_paths_are_shown_compactly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.cpp");
        std::fs::write(&file, "int main() {\n   book.ask = 99;   \n}\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        assert_eq!(source_line(&path, 2).as_deref(), Some("book.ask = 99;"));
        assert_eq!(source_line(&path, 9), None);
        assert_eq!(source_line(&path, 0), None);
        assert_eq!(display_path(&path, dir.path()), "a.cpp");
        assert_eq!(
            display_path("/usr/include/x.h", dir.path()),
            "/usr/include/x.h"
        );
    }
}
