//! Git helpers shared by `cpp_impact`, the finish gate (read-only helpers below) and the
//! history tools `cpp_test_select` / `cpp_repo_history` (audited calls, [`git_logged`]).
//!
//! Every call goes through [`crate::run::run_capped_env`] (process group, hard timeout, capped
//! capture) with the same hardening: settings that keep repository-selected programs from
//! running (hooks, fsmonitor, pagers), no optional locks, no replace objects, paths printed
//! verbatim. Callers add `--no-ext-diff --no-textconv` where a diff is produced. The audited
//! calls are also logged as `git_call` records and counted into an [`Audit`] footer, so a
//! trajectory reader sees which revisions a history tool asked git for. Nothing here writes
//! to the repository.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::run::RunOutcome;
use crate::run::run_capped_env;

pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// C/C++ source and header extensions (lower case).
pub(crate) const CPP_EXTS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "cp", "h", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tcc",
    "tpp", "ixx", "cppm",
];

pub(crate) fn is_cpp_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| CPP_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Environment of the read-only helpers: the audited calls' environment plus a C locale.
fn git_env() -> Vec<(String, String)> {
    let mut out = env();
    out.push(("LC_ALL".to_string(), "C".to_string()));
    out
}

fn full_args(args: &[&str]) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    argv(&args)
}

/// Run git; `Some(stdout)` only for a finished exit-0 run.
pub(crate) async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    git_timeout(cwd, args, GIT_TIMEOUT).await
}

pub(crate) async fn git_timeout(cwd: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    match run_capped_env("git", &full_args(args), cwd, timeout, &git_env()).await {
        Ok(RunOutcome::Finished {
            success: true,
            stdout,
            truncated: false,
            ..
        }) => Some(stdout),
        _ => None,
    }
}

/// Run git and return (exit ok, stdout) even when it exits non-zero (e.g. `git grep` with no
/// match exits 1).
pub(crate) async fn git_status_out(cwd: &Path, args: &[&str]) -> Option<(bool, String)> {
    match run_capped_env("git", &full_args(args), cwd, GIT_TIMEOUT, &git_env()).await {
        Ok(RunOutcome::Finished {
            success, stdout, ..
        }) => Some((success, stdout)),
        _ => None,
    }
}

/// The repository's top-level directory for `cwd`, if it is inside a work tree.
pub(crate) async fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let line = out.lines().next()?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// Repository roots for a workspace at `cwd`: the work tree containing `cwd`; otherwise the
/// work trees below it (a workspace that is a parent of the repository, e.g. `/app` holding
/// `/app/<project>`), C/C++ ones first. Empty when there is none.
pub(crate) async fn repo_roots(cwd: &Path) -> Vec<PathBuf> {
    match toplevel(cwd).await {
        Some(top) => vec![top],
        None => repos_below(cwd),
    }
}

/// The repository the C/C++ features apply to for a workspace at `cwd`: the one containing
/// it, else the only one below it, else the first below it with uncommitted changes, else the
/// first (C/C++ ones first).
pub async fn primary_root(cwd: &Path) -> Option<PathBuf> {
    let roots = repo_roots(cwd).await;
    if roots.len() <= 1 {
        return roots.into_iter().next();
    }
    for root in &roots {
        if git(root, &["status", "--porcelain", "--untracked-files=no"])
            .await
            .is_some_and(|out| !out.trim().is_empty())
        {
            return Some(root.clone());
        }
    }
    roots.into_iter().next()
}

/// The same without running git (filesystem only): the nearest ancestor of `cwd` (or `cwd`)
/// holding `.git`, else the work trees below `cwd`, C/C++ ones first.
pub(crate) fn repo_roots_sync(cwd: &Path) -> Vec<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if is_git_marker(&d.join(".git")) {
            return vec![d.to_path_buf()];
        }
        dir = d.parent();
    }
    repos_below(cwd)
}

/// A real `.git`: a directory holding `HEAD`, or a `gitdir:` file (worktrees, submodules).
/// Walking UP from the workspace, an empty `.git` directory in a shared parent (`/tmp`, a home
/// directory; sandbox placeholders left by a killed run look exactly like that) must not turn
/// that whole directory into "the repository".
fn is_git_marker(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => path.join("HEAD").is_file(),
        Ok(meta) if meta.is_file() => {
            std::fs::read_to_string(path).is_ok_and(|text| text.trim_start().starts_with("gitdir:"))
        }
        _ => false,
    }
}

/// Directory levels below the workspace searched for repositories.
const REPO_SEARCH_DEPTH: usize = 3;
/// Directories read at most while searching.
const REPO_SEARCH_DIRS: usize = 2_000;
/// Repositories returned at most.
const REPO_SEARCH_MAX: usize = 8;

/// Git work trees below `cwd` (not `cwd` itself; nested ones inside a found work tree are
/// part of it), C/C++ workspaces first, then by path.
pub(crate) fn repos_below(cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut budget = REPO_SEARCH_DIRS;
    let mut level: Vec<PathBuf> = vec![cwd.to_path_buf()];
    for _ in 0..REPO_SEARCH_DEPTH {
        let mut next = Vec::new();
        for dir in level {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut subdirs: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    !name.starts_with('.')
                        && !matches!(
                            name.as_str(),
                            "node_modules"
                                | "target"
                                | "__pycache__"
                                | "venv"
                                | "proc"
                                | "sys"
                                | "dev"
                        )
                })
                .map(|e| e.path())
                .collect();
            subdirs.sort();
            for sub in subdirs {
                if sub.join(".git").exists() {
                    found.push(sub);
                } else {
                    next.push(sub);
                }
            }
        }
        level = next;
        if level.is_empty() || found.len() >= REPO_SEARCH_MAX * 4 {
            break;
        }
    }
    found.sort_by_key(|root| (!crate::detect::is_cpp_workspace(root), root.clone()));
    found.truncate(REPO_SEARCH_MAX);
    found
}

/// `git rev-parse HEAD`, or `None` without a commit.
pub(crate) async fn head(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "--verify", "-q", "HEAD"]).await?;
    let sha = out.trim();
    (sha.len() >= 7).then(|| sha.to_string())
}

/// Tracked files in the index (current).
pub(crate) async fn ls_files(root: &Path) -> Vec<String> {
    git(root, &["ls-files", "-z"])
        .await
        .map(|out| split_z(&out))
        .unwrap_or_default()
}

/// Untracked, not ignored paths; untracked directories collapse to `dir/`.
pub(crate) async fn untracked(root: &Path) -> Vec<String> {
    git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )
    .await
    .map(|out| split_z(&out))
    .unwrap_or_default()
}

/// Untracked, not ignored FILES (no directory collapsing), capped.
pub(crate) async fn untracked_files(root: &Path, cap: usize) -> Vec<String> {
    let mut files = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])
        .await
        .map(|out| split_z(&out))
        .unwrap_or_default();
    files.truncate(cap);
    files
}

/// Paths whose working-tree content differs from `rev` (tracked files only), with status letter.
pub(crate) async fn changed_vs(root: &Path, rev: &str) -> Vec<(char, String)> {
    let Some(out) = git(
        root,
        &["diff", "--name-status", "--no-renames", "-z", rev, "--"],
    )
    .await
    else {
        return Vec::new();
    };
    let parts = split_z(&out);
    let mut result = Vec::new();
    let mut iter = parts.into_iter();
    while let Some(status) = iter.next() {
        let Some(path) = iter.next() else {
            break;
        };
        let letter = status.chars().next().unwrap_or('M');
        result.push((letter, path));
    }
    result
}

/// `git show <rev>:<path>`; `None` when the file does not exist at `rev`.
pub(crate) async fn show(root: &Path, rev: &str, path: &str) -> Option<String> {
    let spec = format!("{rev}:{path}");
    git(root, &["show", &spec]).await
}

/// `git hash-object` of several working-tree files in one call (same order; "" when a file
/// cannot be hashed, e.g. it was deleted).
pub(crate) async fn hash_objects(root: &Path, paths: &[String]) -> Vec<String> {
    let existing: Vec<&String> = paths
        .iter()
        .filter(|p| root.join(p.as_str()).is_file())
        .collect();
    let mut hashes = std::collections::HashMap::new();
    for chunk in existing.chunks(200) {
        let mut args = vec!["hash-object", "--"];
        args.extend(chunk.iter().map(|p| p.as_str()));
        if let Some(out) = git(root, &args).await {
            for (path, hash) in chunk.iter().zip(out.lines()) {
                hashes.insert(path.as_str(), hash.trim().to_string());
            }
        }
    }
    paths
        .iter()
        .map(|p| hashes.get(p.as_str()).cloned().unwrap_or_default())
        .collect()
}

pub(crate) fn split_z(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// FNV-1a 64-bit, stable across runs (used for scratch-dir names and cache keys).
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Is `path` (relative, `/`-separated) a test file or inside a test directory?
pub(crate) fn is_test_path(path: &str) -> bool {
    const TEST_DIRS: &[&str] = &[
        "test",
        "tests",
        "testing",
        "unittest",
        "unittests",
        "unit_tests",
        "unit-tests",
        "testsuite",
        "test-suite",
        "testcases",
        "regress",
        "regression",
        "regressions",
        "spec",
        "specs",
    ];
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').collect();
    let (dirs, file) = parts.split_at(parts.len().saturating_sub(1));
    if dirs.iter().any(|dir| TEST_DIRS.contains(dir)) {
        return true;
    }
    let Some(file) = file.first() else {
        return false;
    };
    let stem = file.split('.').next().unwrap_or(file);
    // CamelCase `WidgetTest.cpp` / `WidgetTests.cpp`, from the original spelling.
    let original_file = path.rsplit('/').next().unwrap_or(path);
    let original_stem = original_file.split('.').next().unwrap_or(original_file);
    stem.starts_with("test_")
        || stem.starts_with("test-")
        || stem.ends_with("_test")
        || stem.ends_with("-test")
        || stem.ends_with("_tests")
        || stem.ends_with("-tests")
        || stem.ends_with("_unittest")
        || (original_stem.len() > 4
            && (original_stem.ends_with("Test") || original_stem.ends_with("Tests")))
        || file.ends_with(".test")
}

/// Is `path` documentation (markdown, rst, plain text, texinfo, man pages)?
pub(crate) fn is_doc_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let ext = Path::new(&lower)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let text_doc = matches!(
        ext,
        "md" | "markdown" | "rst" | "adoc" | "txt" | "texi" | "man"
    ) && !lower.ends_with("cmakelists.txt");
    let man_page = ext.len() == 1 && ext.chars().all(|c| c.is_ascii_digit());
    text_doc || man_page
}

/// Third-party code vendored into the repository (not the project's own).
pub(crate) fn is_vendored_path(path: &str) -> bool {
    path.to_ascii_lowercase().split('/').any(|part| {
        matches!(
            part,
            "third_party"
                | "third-party"
                | "vendor"
                | "vendored"
                | "external"
                | "externals"
                | "googletest"
                | "gtest"
                | "gmock"
        )
    })
}

// --- audited calls of the history tools -----------------------------------------------------

/// `-c` settings put in front of every git command.
const CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "safe.bareRepository=explicit",
    "core.fsmonitor=false",
    "core.pager=cat",
    "color.ui=never",
    "log.showSignature=false",
    "core.quotepath=off",
];

/// Environment for every git command.
fn env() -> Vec<(String, String)> {
    [
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect()
}

/// The full argv (without the program) for `git <args>`.
pub(crate) fn argv(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(CONFIG.len() * 2 + args.len() + 1);
    for setting in CONFIG {
        out.push("-c".to_string());
        out.push((*setting).to_string());
    }
    out.push("--no-pager".to_string());
    out.extend(args.iter().cloned());
    out
}

/// What one git call produced.
#[derive(Debug, Default)]
pub(crate) struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// The 4 MiB capture cap cut the output.
    pub truncated: bool,
}

/// The git subcommands one tool call ran, for the audit footer.
#[derive(Default)]
pub(crate) struct Audit {
    calls: Mutex<Vec<String>>,
}

impl Audit {
    fn record(&self, args: &[String]) {
        let mut sub = "?".to_string();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if arg == "-c" || arg == "-C" {
                iter.next();
            } else if !arg.starts_with('-') {
                sub = arg.clone();
                break;
            }
        }
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sub);
    }

    /// Number of git calls so far.
    pub(crate) fn count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// One line: how many git calls, which subcommands, and the revision bound.
    pub(crate) fn footer(&self, bound: &str) -> String {
        let calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut kinds: Vec<(String, usize)> = Vec::new();
        for call in &calls {
            match kinds.iter_mut().find(|(k, _)| k == call) {
                Some((_, n)) => *n += 1,
                None => kinds.push((call.clone(), 1)),
            }
        }
        let list: Vec<String> = kinds
            .iter()
            .map(|(k, n)| {
                if *n == 1 {
                    k.clone()
                } else {
                    format!("{k}×{n}")
                }
            })
            .collect();
        format!(
            "audit: {} git call(s) ({}); {bound}",
            calls.len(),
            if list.is_empty() {
                "none".to_string()
            } else {
                list.join(", ")
            }
        )
    }
}

/// Run `git <args>` in `cwd` with the hardening above; log it under `tool` (the history
/// tools' audited calls).
pub(crate) async fn git_logged(
    tool: &str,
    audit: Option<&Audit>,
    cwd: &Path,
    args: &[String],
    timeout: Duration,
) -> GitOut {
    let full = argv(args);
    if let Some(audit) = audit {
        audit.record(args);
    }
    let started = Instant::now();
    let out = match run_capped_env("git", &full, cwd, timeout, &env()).await {
        Ok(RunOutcome::Finished {
            success,
            stdout,
            stderr,
            truncated,
            ..
        }) => GitOut {
            ok: success,
            stdout,
            stderr,
            timed_out: false,
            truncated,
        },
        Ok(RunOutcome::TimedOut { .. }) => GitOut {
            timed_out: true,
            ..GitOut::default()
        },
        Err(err) => GitOut {
            stderr: err.to_string(),
            ..GitOut::default()
        },
    };
    crate::report::log_git_call(tool, &full, out.ok, started.elapsed().as_millis());
    out
}

/// [`git_logged`] with `&str` arguments.
pub(crate) async fn git_str(
    tool: &str,
    audit: Option<&Audit>,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> GitOut {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    git_logged(tool, audit, cwd, &args, timeout).await
}

/// Is `sha` a commit that is `head` or one of its ancestors?
pub(crate) async fn is_ancestor(
    tool: &str,
    audit: Option<&Audit>,
    cwd: &Path,
    sha: &str,
    head: &str,
) -> bool {
    if !looks_like_sha(sha) {
        return false;
    }
    let exists = git_str(
        tool,
        audit,
        cwd,
        &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        GIT_TIMEOUT,
    )
    .await;
    if !exists.ok {
        return false;
    }
    git_str(
        tool,
        audit,
        cwd,
        &["merge-base", "--is-ancestor", sha, head],
        GIT_TIMEOUT,
    )
    .await
    .ok
}

/// 7–40 hex digits: the only revision spelling the tools accept from a caller.
pub(crate) fn looks_like_sha(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace that is a parent of the repositories (`/app` holding `/app/<project>`):
    /// the repositories below are found, C/C++ ones first; nested repositories inside a found
    /// one, hidden and dependency directories are not searched.
    #[tokio::test]
    async fn repositories_below_a_parent_workspace_are_found() {
        let Some(parent) = tempfile::tempdir().ok() else {
            return;
        };
        let cpp = parent.path().join("b-proj");
        let docs = parent.path().join("a-docs");
        let Some(()) = crate::impact::tests::fixture_base_in(&cpp) else {
            eprintln!("skipped: git not available");
            return;
        };
        std::fs::create_dir_all(docs.join(".git")).ok();
        std::fs::write(docs.join("README.md"), "docs").ok();
        std::fs::create_dir_all(cpp.join("third_party/dep/.git")).ok();
        std::fs::create_dir_all(parent.path().join("node_modules/x/.git")).ok();
        std::fs::create_dir_all(parent.path().join(".hidden/y/.git")).ok();
        let below = repos_below(parent.path());
        assert_eq!(below, vec![cpp.clone(), docs.clone()]);
        assert_eq!(repo_roots_sync(parent.path()), below);
        // Inside a repository, that repository and nothing else.
        assert_eq!(repo_roots_sync(&cpp.join("src")), vec![cpp.clone()]);
        let roots = repo_roots(parent.path()).await;
        assert_eq!(roots, below);
        assert_eq!(primary_root(parent.path()).await, Some(cpp.clone()));
        let inside = repo_roots(&cpp.join("src")).await;
        assert_eq!(inside.len(), 1);
        assert_eq!(
            std::fs::canonicalize(&inside[0]).ok(),
            std::fs::canonicalize(&cpp).ok()
        );
        // No repository anywhere: nothing.
        let Some(empty) = tempfile::tempdir().ok() else {
            return;
        };
        assert!(repo_roots(empty.path()).await.is_empty());
        assert_eq!(primary_root(empty.path()).await, None);
    }

    #[test]
    fn classifies_test_and_doc_paths() {
        assert!(is_test_path("test/chrono-test.cc"));
        assert!(is_test_path("src/foo_test.cpp"));
        assert!(is_test_path("tests/data/x.json"));
        assert!(is_test_path("lib/test_util.c"));
        assert!(is_test_path("tests/regress.test"));
        assert!(!is_test_path("src/latest.cpp"));
        assert!(!is_test_path("src/contest.cpp"));
        assert!(is_test_path("src/WidgetTest.cpp"));
        assert!(!is_test_path("include/lib/widget.h"));
        assert!(is_doc_path("docs/api.md"));
        assert!(is_doc_path("README.md"));
        assert!(is_doc_path("man/tool.1"));
        assert!(!is_doc_path("CMakeLists.txt"));
        assert!(!is_doc_path("doc/theme/site.min.js"));
        assert!(is_vendored_path("test/gtest/gmock-gtest-all.cc"));
        assert!(!is_vendored_path("src/extern_api.cc"));
        assert!(!is_doc_path("src/a.cc"));
        assert!(is_cpp_path("a/b.hpp"));
        assert!(is_cpp_path("a/b.C"));
        assert!(!is_cpp_path("a/b.py"));
    }

    #[test]
    fn every_command_is_hardened() {
        let args = argv(&["log".to_string(), "abc1234".to_string()]);
        let joined = args.join(" ");
        for needle in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "safe.bareRepository=explicit",
            "--no-pager",
        ] {
            assert!(joined.contains(needle), "{joined}");
        }
        assert_eq!(args.last().map(String::as_str), Some("abc1234"));
        let env = env();
        assert!(env.contains(&("GIT_NO_REPLACE_OBJECTS".to_string(), "1".to_string())));
        assert!(env.contains(&("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string())));
    }

    #[test]
    fn audit_footer_counts_subcommands() {
        let audit = Audit::default();
        audit.record(&["log".to_string(), "-n".to_string()]);
        audit.record(&["log".to_string()]);
        audit.record(&["blame".to_string()]);
        assert_eq!(audit.count(), 3);
        assert_eq!(
            audit.footer("revisions: ancestors of abc1234 only"),
            "audit: 3 git call(s) (log×2, blame); revisions: ancestors of abc1234 only"
        );
    }

    #[test]
    fn only_hex_revisions_are_accepted() {
        assert!(looks_like_sha("abc1234"));
        assert!(looks_like_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!looks_like_sha("HEAD"));
        assert!(!looks_like_sha("abc12"));
        assert!(!looks_like_sha("--all"));
        assert!(!looks_like_sha("origin/main"));
    }

    #[test]
    fn an_empty_git_directory_above_is_not_a_repository() {
        // An empty /tmp/.git (for example a sandbox placeholder a killed run left) must not make
        // every workspace under /tmp resolve to /tmp as its repository.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        assert!(
            repo_roots_sync(&work).is_empty(),
            "an empty .git is not a repository"
        );
        // A real one above still is.
        let real = tempfile::tempdir().unwrap();
        assert!(crate::impact::tests::git(real.path(), &["init", "-q"]));
        let sub = real.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(repo_roots_sync(&sub), vec![real.path().to_path_buf()]);
    }
}
