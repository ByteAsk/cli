//! The commit that was HEAD when the session started ("H0").
//!
//! History and base-commit comparisons read H0 and its ancestors only, never
//! another ref: in a checkout that still holds later refs (a remote branch, a
//! tag, a stash), those can contain the very fix being worked on. H0 is
//! recorded once, in the background from thread start, so commits the agent
//! makes during the session do not move it.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::watch;

use crate::gitx;

/// How long a tool waits for the thread-start probe still in flight.
const HEAD_WAIT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// H0 and the repository it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// Full commit id.
    pub sha: String,
    /// Top level of the work tree.
    pub root: PathBuf,
}

impl Head {
    pub(crate) fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(10)]
    }
}

/// H0 for one thread: resolved in the background from thread start.
#[derive(Clone)]
pub(crate) struct SessionHead {
    rx: watch::Receiver<Option<Option<Head>>>,
    cwd: PathBuf,
}

impl SessionHead {
    /// Start resolving H0 for the workspace at `cwd`.
    pub(crate) fn spawn(cwd: PathBuf) -> Self {
        let (tx, rx) = watch::channel(None);
        let probe_cwd = cwd.clone();
        tokio::spawn(async move {
            let head = resolve(&probe_cwd).await;
            let _ = tx.send(Some(head));
        });
        Self { rx, cwd }
    }

    /// Nothing recorded (gated threads). [`Self::get`] then resolves on demand.
    pub(crate) fn none(cwd: PathBuf) -> Self {
        let (_tx, rx) = watch::channel(None);
        Self { rx, cwd }
    }

    /// A head that is already known (tests).
    #[cfg(test)]
    pub(crate) fn ready(head: Option<Head>, cwd: PathBuf) -> Self {
        let (_tx, rx) = watch::channel(Some(head));
        Self { rx, cwd }
    }

    /// H0, or `None` outside a git repository / before the first commit.
    ///
    /// When no thread-start probe exists (or it never answered), the current
    /// HEAD is resolved now and said to be such by the caller's wording.
    pub(crate) async fn get(&self) -> Option<Head> {
        let mut rx = self.rx.clone();
        if let Ok(Ok(value)) = tokio::time::timeout(HEAD_WAIT, rx.wait_for(Option::is_some)).await
        {
            return value.clone().flatten();
        }
        resolve(&self.cwd).await
    }
}

/// `git rev-parse` for the work-tree root and the HEAD commit. A workspace that is a parent
/// of the repository uses the repository below it ([`gitx::primary_root`]).
pub(crate) async fn resolve(cwd: &Path) -> Option<Head> {
    let top = gitx::git_str("session", None, cwd, &["rev-parse", "--show-toplevel"], PROBE_TIMEOUT)
        .await;
    let root = if top.ok {
        PathBuf::from(top.stdout.trim())
    } else {
        gitx::primary_root(cwd).await?
    };
    let sha = gitx::git_str(
        "session",
        None,
        &root,
        &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
        PROBE_TIMEOUT,
    )
    .await;
    let sha = sha.stdout.trim().to_string();
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 40 {
        return None;
    }
    Some(Head { sha, root })
}

#[cfg(test)]
pub(crate) mod test_repo {
    //! A throwaway git repository for tests.

    use std::path::Path;
    use std::process::Command;

    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `git init` + one commit of everything in `dir`; returns the commit id.
    pub(crate) fn init_and_commit(dir: &Path) -> String {
        git(dir, &["init", "-q"]);
        commit_all(dir, "initial")
    }

    pub(crate) fn commit_all(dir: &Path, message: &str) -> String {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_head_at_start_and_ignores_later_commits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "int a;\n").unwrap();
        let first = test_repo::init_and_commit(dir.path());
        let head = SessionHead::spawn(dir.path().to_path_buf());
        let got = head.get().await.expect("head");
        assert_eq!(got.sha, first);
        std::fs::write(dir.path().join("a.c"), "int b;\n").unwrap();
        test_repo::commit_all(dir.path(), "second");
        assert_eq!(head.get().await.expect("head").sha, first);
    }

    #[tokio::test]
    async fn no_head_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(dir.path()).await, None);
    }
}
