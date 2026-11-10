//! Cancelling an automatic check: any check can be stopped, within 2 s, and a
//! stopped check is NOT RUN (cancelled), never PASS.
//!
//! Three ways a running check is cancelled:
//! - `byteask checks cancel` (from any terminal) writes a request next to the project's results
//!   store; a running gate polls for it every [`POLL`] and stops;
//! - the agent stops the hook process (Claude Code and Codex send SIGTERM when the user
//!   interrupts): the toolbox catches it, records the NOT RUN and exits;
//! - the ByteAsk CLI drops the gate when the user interrupts the turn: [`Guard`] records the
//!   NOT RUN on drop.
//!
//! Stopping is dropping the gate's future: every program it started runs in its own process
//! group, which `run_capped` kills when its future is dropped, so nothing keeps building.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use check_result::CheckResult;
use check_result::NotRunReason;

/// How often a running check looks for a cancel request.
pub const POLL: Duration = Duration::from_millis(100);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Where the cancel request for `root` lives.
pub fn request_path(home: &Path, root: &Path) -> PathBuf {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    home.join("results")
        .join(crate::builds::project_key(&root))
        .join("cancel")
}

/// Ask every check running for the repository at `cwd` to stop. Returns the repository root.
pub async fn request(home: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let root = crate::gitx::toplevel(cwd)
        .await
        .ok_or_else(|| format!("{} is not inside a git repository", cwd.display()))?;
    let path = request_path(home, &root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    }
    std::fs::write(&path, now_ms().to_string())
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(root)
}

fn requested_at(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Resolves when a cancel request made at or after `since_ms` appears for `root`.
pub(crate) async fn requested(home: PathBuf, root: PathBuf, since_ms: u64) {
    let path = request_path(&home, &root);
    loop {
        if requested_at(&path).is_some_and(|at| at >= since_ms) {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The result of a check that was stopped before it finished.
pub fn cancelled(check: &str, how: &str) -> CheckResult {
    CheckResult::not_run(
        check,
        NotRunReason::Cancelled,
        &format!("The check was cancelled before it finished ({how})."),
        "finish again to run it.",
    )
}

/// Run `work` unless a cancel request for `root` arrives first (`None`). Requests older than
/// the start of this call are ignored.
pub(crate) async fn unless_requested<F: Future>(home: &Path, root: &Path, work: F) -> Option<F::Output> {
    let since = now_ms();
    tokio::select! {
        out = work => Some(out),
        () = requested(home.to_path_buf(), root.to_path_buf(), since) => None,
    }
}

/// Records NOT RUN (cancelled) for a check if it is dropped before [`Guard::finish`] (the
/// ByteAsk CLI drops the gate when the user interrupts the turn).
pub(crate) struct Guard {
    record: Option<Box<dyn FnOnce() + Send>>,
}

impl Guard {
    pub(crate) fn new(record: impl FnOnce() + Send + 'static) -> Self {
        Self {
            record: Some(Box::new(record)),
        }
    }

    /// The check finished: nothing to record on drop.
    pub(crate) fn finish(mut self) {
        self.record = None;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            record();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_stops_the_work_and_an_old_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let ok = std::process::Command::new("git").arg("-C").arg(&root).args(["init", "-q"]).status();
        assert!(ok.is_ok_and(|s| s.success()));
        let home = dir.path().join("home");
        // A request from before the check started is ignored.
        request(&home, &root).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let done = unless_requested(&home, &root, async { 7 }).await;
        assert_eq!(done, Some(7));
        let started = std::time::Instant::now();
        let home2 = home.clone();
        let root2 = root.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            request(&home2, &root2).await.unwrap();
        });
        let stopped = unless_requested(&home, &root, tokio::time::sleep(Duration::from_secs(30))).await;
        assert_eq!(stopped, None);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn the_guard_records_only_when_dropped_unfinished() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        Guard::new(move || {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .finish();
        let h = hits.clone();
        drop(Guard::new(move || {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let result = cancelled("cpp_finish_gate", "a person cancelled it");
        assert!(!result.is_pass());
        assert_eq!(result.reason, Some(NotRunReason::Cancelled));
        result.validate().unwrap();
    }
}
