use check_result::NotRunReason;
use check_result::Verdict;
use pretty_assertions::assert_eq;

use super::*;
use crate::git::testing;
use crate::testkit::Fixture;

async fn run(fixture: &Fixture, head: &str) -> RunReport {
    run_checks(&RunOptions {
        repo: fixture.repo.clone(),
        base: "main".to_string(),
        head: head.to_string(),
        work_dir: Some(fixture.dir.path().join("work-tmp")),
    })
    .await
    .expect("run")
}

fn verdicts(report: &RunReport) -> Vec<(String, Verdict, Option<NotRunReason>)> {
    report
        .results
        .iter()
        .map(|r| (r.check.clone(), r.verdict, r.reason))
        .collect()
}

fn v(
    name: &str,
    verdict: Verdict,
    reason: Option<NotRunReason>,
) -> (String, Verdict, Option<NotRunReason>) {
    (name.to_string(), verdict, reason)
}

#[tokio::test]
async fn a_good_change_passes_every_required_check() {
    let fixture = Fixture::new();
    let head = fixture.branch("good", |repo| {
        testing::write(
            repo,
            "src/book.cpp",
            "int add(int a, int b) { return b + a; }\n",
        );
    });
    let report = run(&fixture, &head).await;
    assert_eq!(
        verdicts(&report),
        vec![
            v("build", Verdict::Pass, None),
            v("tests", Verdict::Pass, None),
            v("manifest", Verdict::Pass, None),
        ]
    );
    assert_eq!(fixture.builds(), 1);
    // Every result is bound to the candidate's content key.
    assert!(
        report
            .results
            .iter()
            .all(|r| r.content_key.as_deref() == Some(report.content_key.as_str()))
    );
    assert_eq!(report.binding.head_commit, head);
    assert!(!report.marked);
    for result in &report.results {
        result.validate().expect("contract");
    }
}

/// Without a profile on the base branch nothing defines the checks: NOT RUN (no build
/// profile), with the setup command as the fix; nothing is built.
#[tokio::test]
async fn a_repo_with_no_profile_shows_not_run_no_build_profile() {
    let fixture = Fixture::with_profile(None);
    let head = fixture.branch("feature", |repo| {
        testing::write(
            repo,
            "src/book.cpp",
            "int add(int a, int b) { return 0; }\n",
        );
    });
    let report = run(&fixture, &head).await;
    assert_eq!(
        verdicts(&report),
        vec![
            v("build", Verdict::NotRun, Some(NotRunReason::NoBuildProfile)),
            v("tests", Verdict::NotRun, Some(NotRunReason::NoBuildProfile)),
        ]
    );
    assert!(
        report.results[0]
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("byteask checks profile propose")
    );
    assert_eq!(fixture.builds(), 0);
    assert_eq!(report.binding.profile_rev, "none");
}

#[tokio::test]
async fn a_profile_that_does_not_parse_is_no_build_profile_too() {
    let fixture = Fixture::with_profile(Some("schema = 1\n[build]\nrun = []\n"));
    let head = fixture.branch("feature", |repo| {
        testing::write(repo, "src/book.cpp", "x\n")
    });
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[0].reason, Some(NotRunReason::NoBuildProfile));
    assert!(
        report.results[0].summary.contains("does not parse"),
        "{:?}",
        report.results[0]
    );
}

#[tokio::test]
async fn a_broken_build_fails_and_nothing_after_it_claims_anything() {
    let fixture = Fixture::new();
    let head = fixture.branch("broken", |repo| testing::write(repo, "build-fails", ""));
    let report = run(&fixture, &head).await;
    assert_eq!(
        verdicts(&report),
        vec![
            v("build", Verdict::Fail, None),
            v("tests", Verdict::NotRun, Some(NotRunReason::BuildFailed)),
            v("manifest", Verdict::NotRun, Some(NotRunReason::BuildFailed)),
        ]
    );
    assert!(
        report.results[0]
            .evidence
            .iter()
            .any(|line| line.contains("build broke"))
    );
}

#[tokio::test]
async fn a_failing_test_fails_the_tests_check() {
    let fixture = Fixture::new();
    let head = fixture.branch("red", |repo| {
        testing::write(
            repo,
            "tests.txt",
            "book_add pass\nbook_cross fail\nextra pass\n",
        );
    });
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[1].verdict, Verdict::Fail);
    assert!(
        report.results[1].summary.contains("book_cross"),
        "{:?}",
        report.results[1]
    );
    assert_eq!(
        report.results[2].verdict,
        Verdict::Fail,
        "manifest: a required test failed"
    );
}

/// A branch that drops or skips a required test fails, even when everything it still
/// runs passes.
#[tokio::test]
async fn a_branch_that_drops_or_skips_a_required_test_fails() {
    let fixture = Fixture::new();
    let dropped = fixture.branch("drop", |repo| {
        testing::write(repo, "tests.txt", "book_add pass\nextra pass\n")
    });
    let report = run(&fixture, &dropped).await;
    assert_eq!(
        report.results[1].verdict,
        Verdict::Pass,
        "the tests that ran passed"
    );
    assert_eq!(report.results[2].verdict, Verdict::Fail);
    assert!(
        report.results[2]
            .summary
            .contains("did not run: book_cross"),
        "{:?}",
        report.results[2]
    );

    let skipped = fixture.branch("skip", |repo| {
        testing::write(repo, "tests.txt", "book_add pass\nbook_cross skip\n");
    });
    let report = run(&fixture, &skipped).await;
    assert_eq!(report.results[2].verdict, Verdict::Fail);
    assert!(
        report.results[2].summary.contains("skipped: book_cross"),
        "{:?}",
        report.results[2]
    );
}

/// The branch's own edits to the profile and the manifest do not apply to its own
/// checks: they come from the protected base branch.
#[tokio::test]
async fn a_branch_edit_to_the_profile_or_manifest_does_not_change_its_checks() {
    let fixture = Fixture::new();
    let head = fixture.branch("sneaky", |repo| {
        testing::write(
            repo,
            ".byteask/checks.toml",
            "schema = 1\n[build]\nrun = [\"true\"]\n[test]\nrun = [\"true\"]\n",
        );
        testing::write(repo, ".byteask/required-tests.txt", "book_add\n");
        testing::write(repo, "tests.txt", "book_add pass\n");
        testing::write(repo, "build-fails", "");
    });
    let report = run(&fixture, &head).await;
    assert_eq!(
        verdicts(&report),
        vec![
            v("build", Verdict::Fail, None),
            v("tests", Verdict::NotRun, Some(NotRunReason::BuildFailed)),
            v("manifest", Verdict::NotRun, Some(NotRunReason::BuildFailed)),
        ],
        "the base branch's build ran, not the branch's `true`"
    );
    let main_profile = testing::run(&fixture.repo, &["rev-parse", "main:.byteask/checks.toml"]);
    assert_eq!(report.binding.profile_rev, main_profile);

    // Without the build break, the base manifest still wants book_cross.
    let head = fixture.branch("sneaky2", |repo| {
        testing::write(repo, ".byteask/required-tests.txt", "book_add\n");
        testing::write(repo, "tests.txt", "book_add pass\n");
    });
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[2].verdict, Verdict::Fail);
}

/// A docs-only change starts no build and is NOT RUN (no code or build change), never
/// PASS.
#[tokio::test]
async fn a_docs_only_change_starts_no_build_and_is_never_pass() {
    let fixture = Fixture::new();
    let head = fixture.branch("docs", |repo| {
        testing::write(repo, "README.md", "# book\n\nMore words.\n");
        testing::write(repo, "docs/design.rst", "Design\n");
    });
    let report = run(&fixture, &head).await;
    assert!(
        report
            .results
            .iter()
            .all(|r| r.verdict == Verdict::NotRun && r.reason == Some(NotRunReason::NoCodeChange))
    );
    assert_eq!(fixture.builds(), 0, "no build started");

    // A profile change is never "docs only", whatever its extension.
    let head = fixture.branch("profile-doc", |repo| {
        testing::write(repo, ".byteask/notes.md", "x\n")
    });
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[0].verdict, Verdict::Pass);
}

#[tokio::test]
async fn a_marked_path_runs_the_slow_checks() {
    let fixture = Fixture::new();
    let cold = fixture.branch("cold", |repo| testing::write(repo, "src/book.cpp", "y\n"));
    let report = run(&fixture, &cold).await;
    assert!(!report.results.iter().any(|r| r.check == "asan"));
    let hot = fixture.branch("hot", |repo| {
        testing::write(repo, "src/hot/feed.cpp", "z\n")
    });
    let report = run(&fixture, &hot).await;
    assert!(report.marked);
    assert_eq!(
        report.results.last().map(|r| (r.check.as_str(), r.verdict)),
        Some(("asan", Verdict::Pass))
    );
    let hot_fail = fixture.branch("hot-fail", |repo| {
        testing::write(repo, "src/hot/feed.cpp", "w\n");
        testing::write(repo, "asan-fail", "");
    });
    let report = run(&fixture, &hot_fail).await;
    assert_eq!(
        report.results.last().map(|r| r.verdict),
        Some(Verdict::Fail)
    );
}

#[tokio::test]
async fn a_conflicting_head_is_not_run_and_never_built() {
    let fixture = Fixture::new();
    let head = fixture.branch("conflict", |repo| {
        testing::write(repo, "src/book.cpp", "branch\n")
    });
    fixture.advance_main(|repo| testing::write(repo, "src/book.cpp", "main\n"));
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[0].reason, Some(NotRunReason::BuildFailed));
    assert!(report.results[0].summary.contains("does not merge cleanly"));
    assert_eq!(fixture.builds(), 0);
}

/// The content key is bound to the merge candidate, so a base-only update is a new key.
#[tokio::test]
async fn a_base_only_update_changes_the_content_key() {
    let fixture = Fixture::new();
    let head = fixture.branch("feature", |repo| {
        testing::write(repo, "src/book.cpp", "v2\n")
    });
    let first = run(&fixture, &head).await;
    fixture.advance_main(|repo| testing::write(repo, "NOTES.unrelated", "base moves\n"));
    let second = run(&fixture, &head).await;
    assert_ne!(first.content_key, second.content_key);
    assert_eq!(first.binding.head_commit, second.binding.head_commit);
    assert_eq!(
        fixture.builds(),
        2,
        "the checks ran again for the new candidate"
    );
}

/// The steps run in the merge candidate, not in the work tree the CI job checked out.
#[tokio::test]
async fn the_checks_build_the_merged_tree_not_the_working_tree() {
    let fixture = Fixture::new();
    let head = fixture.branch("feature", |repo| {
        testing::write(repo, "src/book.cpp", "v2\n")
    });
    // A dirty work tree file that would break the build if it were used.
    fixture.write("build-fails", "");
    let report = run(&fixture, &head).await;
    assert_eq!(report.results[0].verdict, Verdict::Pass);
    let worktrees = testing::run(&fixture.repo, &["worktree", "list"]);
    assert_eq!(
        worktrees.lines().count(),
        1,
        "scratch worktree removed: {worktrees}"
    );
}

/// The checks build head merged into the current base, not head alone. A semantic merge
/// conflict (head starts using an API the base removed meanwhile, no textual conflict) passes
/// on head alone and must fail here.
#[tokio::test]
async fn a_semantic_conflict_with_the_new_base_fails_although_head_alone_passes() {
    let fixture = Fixture::new();
    let head = fixture.branch("uses-old-api", |repo| {
        testing::write(repo, "uses_old_api", "call\n")
    });
    let alone = run(&fixture, &head).await;
    assert_eq!(alone.results[0].verdict, Verdict::Pass, "head alone builds");
    fixture.advance_main(|repo| {
        std::fs::remove_file(repo.join("old_api")).expect("rm");
    });
    let merged = run(&fixture, &head).await;
    assert_eq!(
        merged.results[0].verdict,
        Verdict::Fail,
        "{:?}",
        merged.results[0]
    );
    assert!(
        merged.results[0]
            .evidence
            .iter()
            .any(|l| l.contains("old_api was removed"))
    );
    let head_tree = testing::run(&fixture.repo, &["rev-parse", &format!("{head}^{{tree}}")]);
    assert_ne!(merged.binding.merge_tree, head_tree);
}
