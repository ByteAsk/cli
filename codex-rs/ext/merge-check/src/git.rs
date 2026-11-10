//! Git plumbing for the merge check. Every call reads objects only; nothing here runs code
//! from the repository (hooks, filters and the branch's own config are switched off), so the
//! signing step can use it on a repository it does not trust.

use std::path::Path;
use std::process::Command;

/// One git call: stdout on success, a one-line error otherwise.
pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        // Never run the repository's own hooks or filters, and never page.
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        let first = err.lines().find(|line| !line.trim().is_empty()).unwrap_or("failed");
        Err(format!("git {}: {}", args.first().copied().unwrap_or(""), first.trim()))
    }
}

/// The full object id of a revision (`commit`, `tree` or `blob` as the suffix asks).
pub(crate) fn rev_parse(repo: &Path, rev: &str) -> Result<String, String> {
    let out = git(repo, &["rev-parse", "--verify", "--quiet", "--end-of-options", rev])?;
    let id = out.trim().to_string();
    if id.is_empty() {
        Err(format!("no such revision: {rev}"))
    } else {
        Ok(id)
    }
}

/// The blob id of `path` at `commit`, if it exists there.
pub(crate) fn blob_at(repo: &Path, commit: &str, path: &str) -> Option<String> {
    rev_parse(repo, &format!("{commit}:{path}"))
        .ok()
        .filter(|id| !id.is_empty())
}

/// The content of `path` at `commit`.
pub(crate) fn show(repo: &Path, commit: &str, path: &str) -> Option<String> {
    git(repo, &["cat-file", "blob", &format!("{commit}:{path}")]).ok()
}

/// The tree `head` merged into `base` would produce, or why it cannot merge.
pub(crate) fn merge_tree(repo: &Path, base: &str, head: &str) -> Result<String, MergeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["merge-tree", "--write-tree", "--no-messages", "--name-only", base, head])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| MergeError::Git(format!("could not run git: {err}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next().unwrap_or_default().trim().to_string();
    match output.status.code() {
        Some(0) if is_object_id(&first) => Ok(first),
        Some(1) => Err(MergeError::Conflict(
            stdout.lines().skip(1).map(str::to_string).filter(|l| !l.is_empty()).collect(),
        )),
        _ => Err(MergeError::Git(format!(
            "git merge-tree: {}",
            String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or("failed")
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeError {
    /// The head does not merge cleanly; the conflicting paths.
    Conflict(Vec<String>),
    Git(String),
}

pub(crate) fn is_object_id(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Paths that differ between two tree-ish objects (renames as delete + add, so both names
/// count).
pub(crate) fn changed_paths(repo: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let out = git(repo, &["diff", "--name-only", "--no-renames", "-z", from, to, "--"])?;
    Ok(out
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

/// A commit object for a tree (not on any branch), so a worktree can check it out.
pub(crate) fn commit_tree(repo: &Path, tree: &str, parents: &[&str]) -> Result<String, String> {
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.push("-p");
        args.push(parent);
    }
    args.push("-m");
    args.push("byteask merge candidate");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .env("GIT_AUTHOR_NAME", "byteask merge check")
        .env("GIT_AUTHOR_EMAIL", "merge-check@byteask.invalid")
        .env("GIT_COMMITTER_NAME", "byteask merge check")
        .env("GIT_COMMITTER_EMAIL", "merge-check@byteask.invalid")
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && is_object_id(&id) {
        Ok(id)
    } else {
        Err(format!(
            "git commit-tree: {}",
            String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or("failed")
        ))
    }
}

/// The tree of a commit.
pub(crate) fn tree_of(repo: &Path, commit: &str) -> Result<String, String> {
    rev_parse(repo, &format!("{commit}^{{tree}}"))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::process::Command;

    /// Run git for a test fixture; panics on failure.
    pub(crate) fn run(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub(crate) fn write(repo: &Path, path: &str, text: &str) {
        let full = repo.join(path);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        std::fs::write(full, text).expect("write");
    }

    /// Commit everything in the work tree; returns the commit id.
    pub(crate) fn commit_all(repo: &Path, message: &str) -> String {
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "--allow-empty", "-m", message]);
        run(repo, &["rev-parse", "HEAD"])
    }

    pub(crate) fn init(repo: &Path) {
        run(repo, &["init", "-q", "-b", "main"]);
        run(repo, &["config", "commit.gpgsign", "false"]);
    }
}
