//! Snapshot / worktree / diff / apply / cleanup on real temp repositories.

mod common;

use codex_subagents::gitops;
use common::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn snapshot_captures_the_working_tree_without_touching_the_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    // Working-tree state at "session start": a tracked edit, a new text file, and things the
    // change rules exclude (binary, >1 MB, new build-like dir, ignored file).
    fix(&repo, FIX_GOOD);
    write(&repo, "notes/new.txt", "hello\n");
    std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 3]).expect("write");
    std::fs::write(repo.join("big.txt"), "x".repeat(1_100_000)).expect("write");
    write(&repo, "build/gen.txt", "generated\n");
    write(&repo, "run.log", "ignored\n");
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let status_before = git(&repo, &["status", "--porcelain"]);
    let cached_before = git(&repo, &["diff", "--cached", "--name-only"]);

    let scratch = tmp.path().join("scratch");
    let snap = gitops::snapshot(&repo, "s1", &scratch.join("tmp"))
        .await
        .expect("snapshot");

    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&repo, &["status", "--porcelain"]), status_before);
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]),
        cached_before
    );
    assert_eq!(
        byteask_refs(&repo).trim().split_whitespace().last(),
        Some(snap.ref_name.as_str())
    );

    let wt = scratch.join("wt-a");
    gitops::worktree_add(&repo, &wt, &snap.commit)
        .await
        .expect("worktree");
    assert_eq!(read(&wt, "calc.sh"), CALC.replace(BUG_LINE, FIX_GOOD));
    assert_eq!(read(&wt, "notes/new.txt"), "hello\n");
    assert!(!wt.join("blob.bin").exists(), "binary excluded");
    assert!(!wt.join("big.txt").exists(), "large file excluded");
    assert!(!wt.join("build").exists(), "new build-like dir excluded");
    assert!(!wt.join("run.log").exists(), "ignored file excluded");
    // A fresh worktree has no change relative to the snapshot.
    let d = gitops::candidate_diff(&wt, &snap, &scratch.join("tmp"))
        .await
        .expect("diff");
    assert!(d.is_empty());

    gitops::worktree_remove(&repo, &wt).await;
    gitops::delete_ref(&repo, &snap.ref_name).await;
    assert_eq!(extra_worktrees(&repo), Vec::<String>::new());
    assert_eq!(byteask_refs(&repo), "");
    assert!(!wt.exists());
}

#[tokio::test]
async fn candidate_diff_round_trips_between_worktrees_and_the_real_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let scratch = tmp.path().join("scratch");
    let t = scratch.join("tmp");
    let snap = gitops::snapshot(&repo, "s2", &t).await.expect("snapshot");
    let a = scratch.join("wt-a");
    let b = scratch.join("wt-b");
    gitops::worktree_add(&repo, &a, &snap.commit)
        .await
        .expect("a");
    gitops::worktree_add(&repo, &b, &snap.commit)
        .await
        .expect("b");

    // Worktree A: edit, add a file, delete a file, and build (build output must not count).
    fix(&a, FIX_GOOD);
    write(&a, "src/extra.c", "int extra(void) { return 1; }\n");
    std::fs::remove_file(a.join("build.sh")).expect("rm");
    write(&a, "out/calc.sh", "built\n");
    let d = gitops::candidate_diff(&a, &snap, &t).await.expect("diff");
    let paths: Vec<String> = gitops::changed_paths(&d)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(paths, vec!["build.sh", "calc.sh", "src/extra.c"]);

    // Apply A's change onto B: identical content; reverse: back to the snapshot.
    gitops::apply(&b, &d, false, false).await.expect("apply");
    let d_b = gitops::candidate_diff(&b, &snap, &t).await.expect("diff b");
    assert_eq!(
        gitops::normalized_change(&d_b),
        gitops::normalized_change(&d)
    );
    gitops::apply(&b, &d, true, false).await.expect("revert");
    assert!(
        gitops::candidate_diff(&b, &snap, &t)
            .await
            .expect("diff")
            .is_empty()
    );

    // Switching the real tree from one change to another leaves only the second.
    fix(&repo, FIX_WRONG);
    let root = gitops::candidate_diff(&repo, &snap, &t)
        .await
        .expect("root diff");
    gitops::apply(&repo, &root, true, false)
        .await
        .expect("revert root");
    gitops::apply(&repo, &d, false, false)
        .await
        .expect("apply winner");
    let now = gitops::candidate_diff(&repo, &snap, &t).await.expect("now");
    assert_eq!(
        gitops::normalized_change(&now),
        gitops::normalized_change(&d)
    );
    let status = git(&repo, &["status", "--porcelain"]);
    assert_eq!(status, " D build.sh\n M calc.sh\n?? src/\n");

    for wt in [&a, &b] {
        gitops::worktree_remove(&repo, wt).await;
    }
    gitops::delete_ref(&repo, &snap.ref_name).await;
    assert_eq!(extra_worktrees(&repo), Vec::<String>::new());
}

#[tokio::test]
async fn same_size_edit_right_after_checkout_is_seen() {
    // Racy-git: the edit lands in the same second as the worktree checkout and keeps the
    // file size, so only git's racily-clean content check can see it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let t = tmp.path().join("scratch/tmp");
    let snap = gitops::snapshot(&repo, "s5", &t).await.expect("snapshot");
    for i in 0..3 {
        let wt = tmp.path().join(format!("scratch/wt-{i}"));
        gitops::worktree_add(&repo, &wt, &snap.commit)
            .await
            .expect("worktree");
        fix(&wt, FIX_GOOD); // same length as the buggy line
        let d = gitops::candidate_diff(&wt, &snap, &t).await.expect("diff");
        assert_eq!(
            gitops::changed_paths(&d),
            vec![("calc.sh".to_string(), gitops::ChangeKind::Modified)],
            "round {i}"
        );
        gitops::worktree_remove(&repo, &wt).await;
    }
    gitops::delete_ref(&repo, &snap.ref_name).await;
}

#[tokio::test]
async fn untracked_files_present_at_snapshot_are_not_part_of_later_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(&repo, "local/config.txt", "pre-existing\n");
    let t = tmp.path().join("scratch/tmp");
    let snap = gitops::snapshot(&repo, "s3", &t).await.expect("snapshot");
    // Nothing changed since the snapshot -> empty diff, although the file is untracked.
    assert!(
        gitops::candidate_diff(&repo, &snap, &t)
            .await
            .expect("diff")
            .is_empty()
    );
    write(&repo, "local/config.txt", "changed\n");
    let d = gitops::candidate_diff(&repo, &snap, &t)
        .await
        .expect("diff");
    assert_eq!(
        gitops::changed_paths(&d),
        vec![("local/config.txt".to_string(), gitops::ChangeKind::Modified)]
    );
    gitops::delete_ref(&repo, &snap.ref_name).await;
}

#[tokio::test]
async fn repo_without_commits_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("empty");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q"]);
    let err = gitops::snapshot(&repo, "s4", &tmp.path().join("t"))
        .await
        .unwrap_err();
    assert!(err.contains("no HEAD"), "{err}");
}
