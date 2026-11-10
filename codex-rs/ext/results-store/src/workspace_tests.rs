use std::path::Path;

use pretty_assertions::assert_eq;

use super::*;

pub(crate) fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A repository on branch `main` with one commit and a bare upstream it is
/// pushed to.
pub(crate) fn repo_with_upstream() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream.git");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    run_git(
        dir.path(),
        &["init", "-q", "--bare", "-b", "main", "upstream.git"],
    );
    run_git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("book.cpp"), "int depth() { return 1; }\n").unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "start"]);
    run_git(
        &work,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);
    (dir, work)
}

pub(crate) fn probe_now(dir: &Path) -> Probe {
    let top = toplevel(dir, PROBE_BUDGET);
    probe(dir, top.as_deref(), PROBE_BUDGET)
}

#[test]
fn a_plain_directory_has_no_repository_and_no_version() {
    let dir = tempfile::tempdir().unwrap();
    let found = probe_now(dir.path());
    assert_eq!(
        found,
        Probe {
            repo: dir.path().to_path_buf(),
            ..Default::default()
        }
    );
}

#[test]
fn the_version_follows_the_code_and_nothing_else() {
    let (_dir, work) = repo_with_upstream();
    let base = probe_now(&work);
    assert!(base.git && base.tracked_clean && base.pushed);
    assert_eq!(base.branch, "main");
    assert!(base.head.as_deref().is_some_and(|head| head.len() == 40));
    let v0 = base.version.expect("a version");
    assert_eq!(
        probe_now(&work).version.as_deref(),
        Some(v0.as_str()),
        "stable"
    );

    // An edit to a tracked file is a new version; the edit undone is the old one.
    std::fs::write(work.join("book.cpp"), "int depth() { return 2; }\n").unwrap();
    let edited = probe_now(&work);
    let v1 = edited.version.clone().unwrap();
    assert_ne!(v1, v0);
    assert!(!edited.tracked_clean);
    // Staging the same content is not a new version.
    run_git(&work, &["add", "book.cpp"]);
    assert_eq!(probe_now(&work).version.as_deref(), Some(v1.as_str()));
    // A same-size edit is (content, not size, decides).
    std::fs::write(work.join("book.cpp"), "int depth() { return 3; }\n").unwrap();
    let v2 = probe_now(&work).version.unwrap();
    assert_ne!(v2, v1);

    // Build products do not make a version: a binary and compile_commands.json.
    std::fs::write(work.join("a.out"), b"\x7fELF\0\0\0binary").unwrap();
    std::fs::write(work.join("compile_commands.json"), "[]").unwrap();
    assert_eq!(probe_now(&work).version.as_deref(), Some(v2.as_str()));
    // A new source file does.
    std::fs::write(work.join("fill.cpp"), "int fill();\n").unwrap();
    let v3 = probe_now(&work).version.unwrap();
    assert_ne!(v3, v2);
    std::fs::remove_file(work.join("book.cpp")).unwrap();
    assert_ne!(
        probe_now(&work).version.unwrap(),
        v3,
        "a deleted file is a change"
    );
}

#[test]
fn pushed_means_the_upstream_holds_the_commit() {
    let (_dir, work) = repo_with_upstream();
    std::fs::write(work.join("book.cpp"), "int depth() { return 2; }\n").unwrap();
    run_git(&work, &["commit", "-q", "-am", "fix"]);
    let committed = probe_now(&work);
    assert!(committed.tracked_clean);
    assert!(!committed.pushed, "one commit ahead of the upstream");
    run_git(&work, &["push", "-q"]);
    let pushed = probe_now(&work);
    assert!(pushed.pushed);
    assert_eq!(
        pushed.head, committed.head,
        "pushing does not move the commit"
    );
    // A branch without an upstream is never "pushed".
    run_git(&work, &["checkout", "-q", "-b", "local-only"]);
    let local = probe_now(&work);
    assert_eq!(local.branch, "local-only");
    assert!(!local.pushed);
}

#[test]
fn a_subdirectory_reports_the_repository_top() {
    let (_dir, work) = repo_with_upstream();
    std::fs::create_dir(work.join("src")).unwrap();
    let found = probe_now(&work.join("src"));
    assert_eq!(
        found.repo.canonicalize().unwrap(),
        work.canonicalize().unwrap()
    );
    assert!(found.version.is_some());
}

#[test]
fn no_time_left_means_no_version_never_a_wrong_one() {
    let (_dir, work) = repo_with_upstream();
    let top = toplevel(&work, PROBE_BUDGET);
    let found = probe(&work, top.as_deref(), Duration::ZERO);
    assert!(found.git);
    assert_eq!(found.version, None);
}
