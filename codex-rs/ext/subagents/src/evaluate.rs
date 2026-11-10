//! Engine-run builds and test runs in worktrees: every command takes a slot of the
//! process-wide build/test semaphore and the session-wide build lock shared with the C/C++
//! tools' finish gate, respects the session deadline, and is killed (with its whole process
//! group) when the work is cancelled.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::gitops::Snapshot;
use crate::runner::RunOutput;
use crate::runner::acquire_slot;
use crate::runner::run_shell;

/// How the project is built and its existing tests run (commands run from a worktree root).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Recipe {
    pub build: String,
    pub test: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepLog {
    pub what: String,
    pub cmd: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub tail: String,
}

impl StepLog {
    pub fn from_run(what: &str, cmd: &str, out: &RunOutput) -> Self {
        Self {
            what: what.to_string(),
            cmd: cmd.to_string(),
            code: out.code,
            timed_out: out.timed_out,
            elapsed_ms: out.elapsed_ms,
            tail: out.tail(2000).to_string(),
        }
    }
}

pub struct EvalContext {
    pub snapshot: Snapshot,
    pub scratch: PathBuf,
    pub env: Vec<(String, String)>,
    pub slots: Arc<Semaphore>,
    pub deadline: Instant,
    pub build_timeout: Duration,
    pub test_timeout: Duration,
    /// Cancelled when the work is stopped early; running commands are killed.
    pub cancel: CancellationToken,
}

impl EvalContext {
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Run one build/test command under a semaphore slot. `None` = out of time or cancelled.
    pub async fn step(&self, cmd: &str, dir: &Path, timeout: Duration) -> Option<RunOutput> {
        if self.remaining().is_zero() {
            return None;
        }
        let _permit = tokio::select! {
            permit = acquire_slot(&self.slots, self.deadline) => permit?,
            _ = self.cancel.cancelled() => return None,
        };
        // The session-wide build lock shared with the finish gate and the C/C++ tools: never
        // two engine-started builds at once.
        let _build = tokio::select! {
            slot = codex_cpp_tools::build_slot(timeout.min(self.remaining())) => slot?,
            _ = self.cancel.cancelled() => return None,
        };
        let limit = timeout.min(self.remaining());
        if limit.is_zero() || self.cancel.is_cancelled() {
            return None;
        }
        tokio::select! {
            out = run_shell(cmd, dir, &self.env, limit) => Some(out),
            _ = self.cancel.cancelled() => None,
        }
    }

    /// [`Self::step`] plus a log entry.
    pub async fn logged(
        &self,
        what: &str,
        cmd: &str,
        dir: &Path,
        timeout: Duration,
        log: &mut Vec<StepLog>,
    ) -> Option<RunOutput> {
        let out = self.step(cmd, dir, timeout).await?;
        log.push(StepLog::from_run(what, cmd, &out));
        Some(out)
    }
}

/// Build recipe from the repository layout, used when no sub-agent reported one.
pub fn detect_recipe(root: &Path, jobs: usize) -> Option<Recipe> {
    if root.join("CMakeLists.txt").is_file() {
        return Some(Recipe {
            build: format!(
                "cmake -S . -B _byteask_build -DCMAKE_BUILD_TYPE=Debug >/dev/null && cmake --build _byteask_build -- -j{jobs}"
            ),
            test: Some(format!(
                "cd _byteask_build && ctest --output-on-failure -j{jobs}"
            )),
            source: "detected: CMakeLists.txt".to_string(),
        });
    }
    if root.join("Makefile").is_file() || root.join("makefile").is_file() {
        return Some(Recipe {
            build: format!("make -j{jobs}"),
            test: Some(format!(
                "if make -n check >/dev/null 2>&1; then make -j{jobs} check; elif make -n test >/dev/null 2>&1; then make -j{jobs} test; else true; fi"
            )),
            source: "detected: Makefile".to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn detection() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        assert_eq!(detect_recipe(dir.path(), 2), None);
        std::fs::write(dir.path().join("Makefile"), "all:\n").unwrap_or_else(|e| panic!("{e}"));
        let r = detect_recipe(dir.path(), 2).unwrap_or_else(|| panic!("recipe"));
        assert_eq!(r.build, "make -j2");
        std::fs::write(dir.path().join("CMakeLists.txt"), "").unwrap_or_else(|e| panic!("{e}"));
        let r = detect_recipe(dir.path(), 2).unwrap_or_else(|| panic!("recipe"));
        assert!(r.build.starts_with("cmake -S ."));
    }

    #[tokio::test]
    async fn cancelled_context_runs_nothing() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let ctx = EvalContext {
            snapshot: Snapshot {
                commit: String::new(),
                tree: String::new(),
                ref_name: String::new(),
            },
            scratch: dir.path().to_path_buf(),
            env: Vec::new(),
            slots: Arc::new(Semaphore::new(1)),
            deadline: Instant::now() + Duration::from_secs(60),
            build_timeout: Duration::from_secs(10),
            test_timeout: Duration::from_secs(10),
            cancel: CancellationToken::new(),
        };
        let started = Instant::now();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let out = ctx
            .step("sleep 30", dir.path(), Duration::from_secs(60))
            .await;
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
