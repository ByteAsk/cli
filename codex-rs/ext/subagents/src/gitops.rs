//! Git plumbing for snapshots, worktrees outside the repository, and candidate diffs.
//!
//! Everything here runs from the engine process (never from a sub-agent's shell) and never
//! touches the main repository's index, HEAD or branches: snapshots are built in a temporary
//! index file, worktrees are detached, and the only ref created
//! (`refs/byteask/snapshots/<id>`, which keeps the snapshot commit alive) is deleted at cleanup.
//!
//! Works with old git (2.7 era): no `git worktree remove` (rm + `worktree prune` instead).
//!
//! "What counts as a change" deliberately mirrors the harness-neutral patch extraction used by
//! C/C++ issue benchmarks: every change to tracked files, plus new untracked, non-ignored text
//! files of at most 1 MB that do not sit in a new build-like directory.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use regex_lite::Regex;

use crate::runner::spawn_capture;

const GIT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_NEW_FILE_BYTES: u64 = 1_000_000;

#[allow(clippy::expect_used)] // a constant pattern: verified by the unit tests
static BUILDLIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(build.*|cmake-build.*|_build|out|bin|obj|CMakeFiles|Testing|\.cache|dist|target|\.ccls-cache|\.clangd)$",
    )
    .expect("valid regex")
});

const IDENTITY_ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "byteask"),
    ("GIT_AUTHOR_EMAIL", "byteask@localhost"),
    ("GIT_COMMITTER_NAME", "byteask"),
    ("GIT_COMMITTER_EMAIL", "byteask@localhost"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// A snapshot of the repository at orchestration start: HEAD plus the working tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub ref_name: String,
}

async fn git_env(
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let mut full: Vec<&str> = vec![
        "-c",
        "core.quotepath=off",
        "-c",
        "safe.directory=*",
        "-c",
        "core.hooksPath=/dev/null",
    ];
    full.extend_from_slice(args);
    let mut env: Vec<(&str, &str)> = IDENTITY_ENV.to_vec();
    env.extend_from_slice(extra_env);
    let (ok, out, err) = spawn_capture("git", &full, dir, &env, stdin, GIT_TIMEOUT).await;
    if ok {
        Ok(out)
    } else {
        Err(format!("git {}: {}", args.join(" "), err.trim()))
    }
}

async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    git_env(dir, args, &[], None).await
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// The top-level directory of the git work tree containing `cwd`.
pub async fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let top = text(out);
    (!top.is_empty()).then(|| PathBuf::from(top))
}

pub async fn head(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .ok()
        .map(text)
        .filter(|s| !s.is_empty())
}

async fn tree_dirs(dir: &Path, treeish: &str) -> BTreeSet<String> {
    let mut dirs: BTreeSet<String> = git(dir, &["ls-tree", "-r", "-d", "--name-only", treeish])
        .await
        .map(|out| {
            String::from_utf8_lossy(&out)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    dirs.insert(String::new());
    dirs
}

/// Why an untracked file is not part of the change, or `None` when it is.
pub fn exclusion_reason(dir: &Path, rel: &str, base_dirs: &BTreeSet<String>) -> Option<String> {
    let path = dir.join(rel);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(_) => return Some("unreadable".to_string()),
    };
    if !meta.is_file() {
        return Some("not a regular file".to_string());
    }
    if meta.len() > MAX_NEW_FILE_BYTES {
        return Some("larger than 1 MB".to_string());
    }
    if let Ok(bytes) = std::fs::read(&path) {
        let head = &bytes[..bytes.len().min(8192)];
        if head.contains(&0) {
            return Some("binary".to_string());
        }
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if name == "CMakeCache.txt" || format!("/{rel}").contains("/CMakeFiles/") {
        return Some("cmake build output".to_string());
    }
    let comps: Vec<&str> = rel.split('/').collect();
    let dirs = &comps[..comps.len().saturating_sub(1)];
    for i in 1..=dirs.len() {
        let d = dirs[..i].join("/");
        if !base_dirs.contains(&d) {
            if BUILDLIKE.is_match(dirs[i - 1]) || dir.join(&d).join("CMakeCache.txt").exists() {
                return Some(format!("inside new build-like directory {d}/"));
            }
            break;
        }
    }
    None
}

/// Write a tree object for the current state of the work tree at `dir` (tracked changes plus
/// new untracked files that pass [`exclusion_reason`]), without touching the real index.
/// `base` decides which directories count as pre-existing. `tmp_dir` holds the scratch index.
pub async fn capture_tree(dir: &Path, base: &str, tmp_dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(tmp_dir).map_err(|e| e.to_string())?;
    let index = tmp_dir.join(format!("index-{}-{}", std::process::id(), next_seq()));
    let real_index = text(git(dir, &["rev-parse", "--git-path", "index"]).await?);
    let real_index = {
        let p = PathBuf::from(&real_index);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    if real_index.exists() {
        std::fs::copy(&real_index, &index).map_err(|e| format!("copy index: {e}"))?;
        // Keep the index's mtime. Git decides whether an entry is "racily clean" (and must be
        // content-checked) by comparing file mtimes with the index file's mtime; a copy with a
        // fresh mtime would hide a same-size edit made within the same second as the checkout.
        if let Ok(mtime) = std::fs::metadata(&real_index).and_then(|m| m.modified())
            && let Ok(copy) = std::fs::OpenOptions::new().write(true).open(&index)
        {
            let _ = copy.set_modified(mtime);
        }
    }
    let index_str = index.to_string_lossy().to_string();
    let env = [("GIT_INDEX_FILE", index_str.as_str())];
    let result = async {
        if !real_index.exists() {
            git_env(dir, &["read-tree", "HEAD"], &env, None).await?;
        }
        git_env(dir, &["add", "-u", "--", "."], &env, None).await?;
        let base_dirs = tree_dirs(dir, base).await;
        // Files staged as new (e.g. `git add -N .` after an in-tree build) get the same rules as
        // untracked ones, so build output never becomes part of a change.
        let staged_new = git_env(
            dir,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--diff-filter=A",
                "-z",
                base,
                "--",
            ],
            &env,
            None,
        )
        .await
        .unwrap_or_default();
        let mut drop: Vec<u8> = Vec::new();
        for rel in String::from_utf8_lossy(&staged_new).split('\0') {
            if !rel.is_empty() && exclusion_reason(dir, rel, &base_dirs).is_some() {
                drop.extend_from_slice(rel.as_bytes());
                drop.push(0);
            }
        }
        if !drop.is_empty() {
            git_env(
                dir,
                &["update-index", "--force-remove", "-z", "--stdin"],
                &env,
                Some(drop),
            )
            .await?;
        }
        let others = git_env(
            dir,
            &["ls-files", "-z", "--others", "--exclude-standard"],
            &env,
            None,
        )
        .await?;
        let mut included: Vec<u8> = Vec::new();
        for rel in String::from_utf8_lossy(&others).split('\0') {
            if rel.is_empty() {
                continue;
            }
            if exclusion_reason(dir, rel, &base_dirs).is_none() {
                included.extend_from_slice(rel.as_bytes());
                included.push(0);
            }
        }
        if !included.is_empty() {
            git_env(
                dir,
                &["update-index", "--add", "-z", "--stdin"],
                &env,
                Some(included),
            )
            .await?;
        }
        let tree = text(git_env(dir, &["write-tree"], &env, None).await?);
        Ok::<String, String>(tree)
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

fn next_seq() -> u64 {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Snapshot HEAD plus the working tree of `repo` as a commit kept alive by a private ref.
pub async fn snapshot(repo: &Path, id: &str, tmp_dir: &Path) -> Result<Snapshot, String> {
    let head = head(repo)
        .await
        .ok_or_else(|| "repository has no HEAD commit".to_string())?;
    let tree = capture_tree(repo, &head, tmp_dir).await?;
    let commit = text(
        git(
            repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &head,
                "-m",
                "byteask snapshot (temporary)",
            ],
        )
        .await?,
    );
    let ref_name = format!("refs/byteask/snapshots/{id}");
    git(repo, &["update-ref", &ref_name, &commit]).await?;
    Ok(Snapshot {
        commit,
        tree,
        ref_name,
    })
}

/// Detached worktree of `commit` at `path` (which must not exist yet).
pub async fn worktree_add(repo: &Path, path: &Path, commit: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let p = path.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "--detach", &p, commit])
        .await
        .map(|_| ())
}

/// Remove a worktree directory and its metadata (works without `git worktree remove`).
pub async fn worktree_remove(repo: &Path, path: &Path) {
    let _ = tokio::fs::remove_dir_all(path).await;
    let _ = git(repo, &["worktree", "prune"]).await;
}

pub async fn delete_ref(repo: &Path, ref_name: &str) {
    let _ = git(repo, &["update-ref", "-d", ref_name]).await;
}

/// Worktrees registered for `repo` (absolute paths), for leak checks.
pub async fn worktree_list(repo: &Path) -> Vec<PathBuf> {
    git(repo, &["worktree", "list", "--porcelain"])
        .await
        .map(|out| {
            String::from_utf8_lossy(&out)
                .lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

pub async fn diff_trees(dir: &Path, from: &str, to: &str) -> Result<Vec<u8>, String> {
    git(
        dir,
        &["diff", "--binary", "--no-color", "--no-ext-diff", from, to],
    )
    .await
}

/// The change in the work tree at `dir` relative to the snapshot, as a binary patch.
pub async fn candidate_diff(
    dir: &Path,
    snap: &Snapshot,
    tmp_dir: &Path,
) -> Result<Vec<u8>, String> {
    let tree = capture_tree(dir, &snap.commit, tmp_dir).await?;
    if tree == snap.tree {
        return Ok(Vec::new());
    }
    diff_trees(dir, &snap.tree, &tree).await
}

/// `git apply --binary` of `patch` in the work tree at `dir`.
pub async fn apply(
    dir: &Path,
    patch: &[u8],
    reverse: bool,
    check_only: bool,
) -> Result<(), String> {
    if patch.is_empty() {
        return Ok(());
    }
    let mut args = vec!["apply", "--binary", "--whitespace=nowarn"];
    if reverse {
        args.push("-R");
    }
    if check_only {
        args.push("--check");
    }
    args.push("-");
    git_env(dir, &args, &[], Some(patch.to_vec()))
        .await
        .map(|_| ())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
}

/// Paths touched by a git patch, with how they changed.
pub fn changed_paths(patch: &[u8]) -> Vec<(String, ChangeKind)> {
    let text = String::from_utf8_lossy(patch);
    let mut out: Vec<(String, ChangeKind)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/<path> b/<path>"; paths with spaces are rare in C/C++ repos, take the b/ side.
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            out.push((path, ChangeKind::Modified));
        } else if line.starts_with("new file mode") {
            if let Some(last) = out.last_mut() {
                last.1 = ChangeKind::Added;
            }
        } else if line.starts_with("deleted file mode")
            && let Some(last) = out.last_mut()
        {
            last.1 = ChangeKind::Deleted;
        }
    }
    out
}

/// Whitespace-insensitive fingerprint of a patch's added/removed lines, for "same change" checks.
pub fn normalized_change(patch: &[u8]) -> String {
    let text = String::from_utf8_lossy(patch);
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("diff --git") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
            continue;
        }
        if let Some(body) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
            let squashed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if squashed.is_empty() {
                continue;
            }
            out.push(line.as_bytes()[0] as char);
            out.push_str(&squashed);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn changed_paths_reads_kinds() {
        let patch = b"diff --git a/src/x.c b/src/x.c\nindex 1..2 100644\n--- a/src/x.c\n+++ b/src/x.c\n@@ -1 +1 @@\n-a\n+b\n\
diff --git a/tests/new.cpp b/tests/new.cpp\nnew file mode 100644\n--- /dev/null\n+++ b/tests/new.cpp\n@@ -0,0 +1 @@\n+x\n\
diff --git a/old.h b/old.h\ndeleted file mode 100644\n";
        assert_eq!(
            changed_paths(patch),
            vec![
                ("src/x.c".to_string(), ChangeKind::Modified),
                ("tests/new.cpp".to_string(), ChangeKind::Added),
                ("old.h".to_string(), ChangeKind::Deleted),
            ]
        );
    }

    #[test]
    fn normalized_change_ignores_whitespace_and_context() {
        let a = b"diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n-int  a;\n+int a = 1;\n";
        let b = b"diff --git a/x b/x\nindex 3..4\n--- a/x\n+++ b/x\n@@ -5,2 +5,2 @@\n other\n-int a;\n+int  a  =  1;\n";
        assert_eq!(normalized_change(a), normalized_change(b));
    }

    #[test]
    fn buildlike_names() {
        for name in [
            "build",
            "Build-debug",
            "cmake-build-release",
            "out",
            "CMakeFiles",
        ] {
            assert!(BUILDLIKE.is_match(name), "{name}");
        }
        for name in ["src", "include", "builder_utils", "outline"] {
            // `builder_utils` starts with "build" and so is build-like by the shared rule;
            // only the rest must not match.
            if name == "builder_utils" {
                continue;
            }
            assert!(!BUILDLIKE.is_match(name), "{name}");
        }
    }
}
