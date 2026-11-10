//! The git facts checks.json records: the repo root, `base_sha` (HEAD) and `tree_sha`
//! (`git write-tree` of the index as it was before any check ran).

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_sha1(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The top of the work tree that contains `dir`.
pub(crate) fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|_| format!("{} is not inside a git repository", dir.display()))?;
    Ok(PathBuf::from(root))
}

/// The commit HEAD points at.
pub(crate) fn head_sha(root: &Path) -> Result<String, String> {
    let sha = git(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|_| format!("{} has no HEAD commit", root.display()))?;
    if is_sha1(&sha) {
        Ok(sha)
    } else {
        Err(format!("HEAD is not a SHA-1 object id: {sha}"))
    }
}

/// `git write-tree` of the current index (the checks.json `repo.tree_sha` rule): the runner stages
/// the patch with `git apply --index` and records the same write-tree, so untracked setup or
/// build output must never enter it. Nothing is added to the index here.
pub(crate) fn tree_sha(root: &Path) -> Result<String, String> {
    let sha = git(root, &["write-tree"])?;
    if is_sha1(&sha) {
        Ok(sha)
    } else {
        Err(format!("git write-tree gave an unexpected id: {sha}"))
    }
}
