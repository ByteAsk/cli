use check_result::NotRunReason;
use check_result::Verdict;
use pretty_assertions::assert_eq;

use super::*;
use crate::git::testing;
use crate::run::RunOptions;
use crate::run::run_checks;
use crate::sign::RunnerKey;
use crate::testkit::FakeServer;
use crate::testkit::Fixture;

struct World {
    fixture: Fixture,
    server: FakeServer,
    ci: RunnerKey,
    bench: RunnerKey,
    evidence: std::path::PathBuf,
    now: std::cell::Cell<u64>,
}

impl World {
    fn new(fixture: Fixture) -> Self {
        let server = FakeServer::default();
        let ci = RunnerKey::from_seed([1u8; 32]);
        let bench = RunnerKey::from_seed([2u8; 32]);
        server.enrol(&ci, RunnerKind::Ci);
        server.enrol(&bench, RunnerKind::Bench);
        let evidence = fixture.dir.path().join("evidence");
        std::fs::create_dir_all(&evidence).expect("mkdir");
        Self {
            fixture,
            server,
            ci,
            bench,
            evidence,
            now: std::cell::Cell::new(1_000_000),
        }
    }

    fn tick(&self) -> u64 {
        self.now.set(self.now.get() + 1_000);
        self.now.get()
    }

    async fn run(&self, head: &str) -> RunReport {
        run_checks(&RunOptions {
            repo: self.fixture.repo.clone(),
            base: "main".to_string(),
            head: head.to_string(),
            work_dir: Some(self.fixture.dir.path().join("work-tmp")),
        })
        .await
        .expect("run")
    }

    /// The CI signing step: sign with `key`, record it with the server, file it.
    fn sign(&self, key: &RunnerKey, kind: RunnerKind, payload: String, name: &str) -> Envelope {
        let envelope = key.sign(kind, payload, self.tick());
        self.server.record(&envelope);
        self.file(name, &envelope);
        envelope
    }

    fn file(&self, name: &str, envelope: &Envelope) {
        std::fs::write(
            self.evidence.join(name),
            serde_json::to_string(envelope).expect("json"),
        )
        .expect("write");
    }

    fn candidate(&self, head: &str) -> Candidate {
        Candidate::compute(&self.fixture.repo, "main", head).expect("candidate")
    }

    async fn gate(&self, head: &str, approvals: &[Approval]) -> Decision {
        let candidate = self.candidate(head);
        let key = candidate.binding.candidate_key();
        let envelopes = collect_envelopes(std::slice::from_ref(&self.evidence), &key);
        let evidence = sort_evidence(envelopes, &self.server, &candidate.binding).await;
        decide(&candidate, &evidence, approvals)
    }
}

fn approval(reviewer: &str, head: &str, at_ms: u64) -> Approval {
    Approval {
        reviewer: reviewer.to_string(),
        is_bot: false,
        is_author: false,
        head_commit: head.to_string(),
        at_ms,
    }
}

/// All-PASS signed by an enrolled CI runner for the same content key allows the merge.
#[tokio::test]
async fn all_pass_for_the_same_content_key_allows_the_merge() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(decision.allowed, "{decision:?}");
    assert_eq!(decision.verdict, Verdict::Pass);
    assert!(decision.summary.starts_with("PASS"), "{}", decision.summary);
    assert!(decision.ignored.is_empty(), "{:?}", decision.ignored);
}

#[tokio::test]
async fn a_fail_blocks_the_merge() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("red", |repo| testing::write(repo, "build-fails", ""));
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let decision = world
        .gate(&head, &[approval("alice", &head, u64::MAX)])
        .await;
    assert!(!decision.allowed);
    assert_eq!(decision.verdict, Verdict::Fail);
    assert!(
        decision.summary.starts_with("FAIL: build"),
        "{}",
        decision.summary
    );
}

/// NOT RUN blocks; a repository with no profile shows NOT RUN (no build profile).
#[tokio::test]
async fn not_run_blocks_and_no_profile_is_not_run_no_build_profile() {
    let world = World::new(Fixture::with_profile(None));
    let head = world
        .fixture
        .branch("f", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let decision = world
        .gate(&head, &[approval("alice", &head, u64::MAX)])
        .await;
    assert!(
        !decision.allowed,
        "no profile: a reviewer cannot approve it by default"
    );
    assert_eq!(decision.verdict, Verdict::NotRun);
    assert!(
        decision.summary.starts_with("NOT RUN (no build profile)"),
        "{}",
        decision.summary
    );
}

/// A NOT RUN reason the profile lets a reviewer approve goes through only with a
/// person's approval of this head, given after the result existed; it is still NOT RUN.
#[tokio::test]
async fn an_approvable_not_run_needs_a_person_approving_this_exact_version() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("docs", |repo| testing::write(repo, "README.md", "# more\n"));
    let report = world.run(&head).await;
    let signed = world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let at = signed.signed_ms;

    let blocked = world.gate(&head, &[]).await;
    assert!(!blocked.allowed);
    assert!(
        blocked.summary.contains("A reviewer may approve"),
        "{}",
        blocked.summary
    );

    let not_counting = [
        Approval {
            is_bot: true,
            ..approval("ci-bot[bot]", &head, at + 1)
        },
        Approval {
            is_author: true,
            ..approval("author", &head, at + 1)
        },
        approval("alice", "0000000000000000000000000000000000000000", at + 1),
        approval("alice", &head, at - 1),
    ];
    for one in &not_counting {
        let decision = world.gate(&head, std::slice::from_ref(one)).await;
        assert!(!decision.allowed, "{one:?} must not count");
    }

    let decision = world.gate(&head, &[approval("alice", &head, at + 1)]).await;
    assert!(decision.allowed);
    assert_eq!(
        decision.verdict,
        Verdict::NotRun,
        "approved, but never PASS"
    );
    assert_eq!(decision.approved_by.as_deref(), Some("alice"));
    assert!(
        decision.summary.contains("approved by alice") && decision.summary.contains("Not a pass")
    );
}

/// Nothing local or unsigned counts. With only forged results the merge check finds
/// no trusted result: NOT RUN (not checked), which no reviewer can approve.
#[tokio::test]
async fn tampered_unsigned_and_local_results_are_ignored() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("bad", |repo| testing::write(repo, "build-fails", ""));
    let report = world.run(&head).await;
    assert_eq!(report.results[0].verdict, Verdict::Fail);
    // The AI or the developer edits the real FAIL into a PASS after signing.
    let mut forged = report.clone();
    for result in &mut forged.results {
        *result = CheckResult::pass(&result.check, "forged").with_content_key(&report.content_key);
    }
    let mut envelope = world.ci.sign(
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        world.tick(),
    );
    world.server.record(&envelope);
    envelope.payload = serde_json::to_string(&forged).unwrap();
    world.file("edited.json", &envelope);
    // An unsigned report, and a laptop's local results-store entry for the same key.
    std::fs::write(
        world.evidence.join("unsigned.json"),
        serde_json::to_string(&forged).unwrap(),
    )
    .unwrap();
    std::fs::write(
        world.evidence.join("index.json"),
        serde_json::json!({"entries": [{"result": forged.results[0], "root": "/home/dev/repo", "at": 1,
            "fresh": "x", "tier": "full", "source": "claude-code"}]})
        .to_string(),
    )
    .unwrap();
    // Signed by a key nobody enrolled.
    let stranger = RunnerKey::from_seed([66u8; 32]);
    let envelope = stranger.sign(
        RunnerKind::Ci,
        serde_json::to_string(&forged).unwrap(),
        world.tick(),
    );
    world.file("stranger.json", &envelope);
    // Signed by the bench key (enrolled, wrong kind).
    let envelope = world.bench.sign(
        RunnerKind::Ci,
        serde_json::to_string(&forged).unwrap(),
        world.tick(),
    );
    world.file("bench-as-ci.json", &envelope);

    let decision = world
        .gate(&head, &[approval("alice", &head, u64::MAX)])
        .await;
    assert!(!decision.allowed);
    assert_eq!(decision.verdict, Verdict::NotRun);
    assert!(
        decision
            .checks
            .iter()
            .all(|c| c.reason == Some(NotRunReason::NotChecked)),
        "{:?}",
        decision.checks
    );
    let ignored = decision.ignored.join("\n");
    for (file, why) in [
        ("edited.json", "signature does not verify"),
        ("unsigned.json", "not a signed result"),
        ("index.json", "not a signed result"),
        ("stranger.json", "not enrolled"),
        ("bench-as-ci.json", "bench runner key"),
    ] {
        assert!(
            ignored
                .lines()
                .any(|line| line.contains(file) && line.contains(why)),
            "{file} should be ignored because {why}:\n{ignored}"
        );
    }

    // The real, signed FAIL counts.
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "real.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert_eq!(decision.verdict, Verdict::Fail);
}

/// Retirement and revocation with the in-process verifier (`cli_tests` covers a real server).
#[tokio::test]
async fn retirement_keeps_recorded_results_and_revocation_drops_all() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let report = serde_json::to_string(&world.run(&head).await).unwrap();
    world.sign(&world.ci, RunnerKind::Ci, report.clone(), "run.json");
    world.server.retire(&world.ci);
    assert!(
        world.gate(&head, &[]).await.allowed,
        "recorded before retirement: still valid"
    );
    // A signature the retired key makes afterwards does not count.
    std::fs::remove_file(world.evidence.join("run.json")).unwrap();
    let late = world.ci.sign(RunnerKind::Ci, report.clone(), world.tick());
    world.file("late.json", &late);
    assert!(!world.gate(&head, &[]).await.allowed);

    let other = RunnerKey::from_seed([3u8; 32]);
    world.server.enrol(&other, RunnerKind::Ci);
    std::fs::remove_file(world.evidence.join("late.json")).unwrap();
    world.sign(&other, RunnerKind::Ci, report, "other.json");
    assert!(world.gate(&head, &[]).await.allowed);
    world.server.revoke(&other);
    let decision = world.gate(&head, &[]).await;
    assert!(!decision.allowed);
    assert!(
        decision.ignored.join("\n").contains("revoked"),
        "{:?}",
        decision.ignored
    );
}

/// When the enterprise server cannot be asked, nothing counts (never accept on an error).
#[tokio::test]
async fn an_unreachable_verifier_accepts_nothing() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    *world.server.down.lock().unwrap() = true;
    let decision = world.gate(&head, &[]).await;
    assert!(!decision.allowed);
    assert!(
        decision.ignored[0].contains("could not be checked"),
        "{:?}",
        decision.ignored
    );
}

/// After a PASS, a base-only update makes a new merge candidate: the old PASS does not
/// count, an approval given before does not count, and the checks run again.
#[tokio::test]
async fn a_base_only_update_after_pass_forces_a_rerun() {
    let world = World::new(Fixture::new());
    let head = world.fixture.branch("feature", |repo| {
        testing::write(repo, "src/book.cpp", "v2\n")
    });
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run1.json",
    );
    assert!(world.gate(&head, &[]).await.allowed);
    assert_eq!(world.fixture.builds(), 1);

    world
        .fixture
        .advance_main(|repo| testing::write(repo, "src/other.cpp", "base change\n"));
    let stale = world
        .gate(&head, &[approval("alice", &head, u64::MAX)])
        .await;
    assert!(!stale.allowed, "the earlier PASS was for the old base");
    assert!(
        stale
            .checks
            .iter()
            .all(|c| c.reason == Some(NotRunReason::NotChecked))
    );
    assert!(
        stale
            .ignored
            .iter()
            .any(|why| why.contains("another version")),
        "{:?}",
        stale.ignored
    );

    let rerun = world.run(&head).await;
    assert_eq!(world.fixture.builds(), 2, "the checks ran again");
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&rerun).unwrap(),
        "run2.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(decision.allowed && decision.verdict == Verdict::Pass);

    // A base-branch change to the profile alone (no code) is a new candidate too.
    world
        .fixture
        .advance_main(|repo| testing::write(repo, ".byteask/required-tests.txt", "book_add\n"));
    assert!(!world.gate(&head, &[]).await.allowed);
}

fn ut_report(base: &str, head: &str, gate: &str, verdict: &str) -> serde_json::Value {
    let mut compare: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/aa-recorded.json")).expect("recorded");
    compare["verdict"] = serde_json::Value::from(verdict);
    serde_json::json!({
        "schema": "byteask.perflab.ut/1",
        "gate": gate,
        "base": {"label": "base main", "reference": "main", "commit": base},
        "head": {"label": "head", "reference": "cand", "commit": head},
        "tests": [{"name": "feed handler", "gate": gate, "reasons": [], "report": compare}],
        "started_utc": "2026-10-18T17:40:00Z", "finished_utc": "2026-10-18T17:44:00Z",
        "runner_version": "0.1.0"
    })
}

/// A marked change needs the bench's signed perf result when the profile requires it,
/// and the card it posts is the noise-gated change with its confidence.
#[tokio::test]
async fn a_marked_change_needs_a_signed_bench_result_and_gets_a_perf_card() {
    let world = World::new(Fixture::new());
    let head = world.fixture.branch("hot", |repo| {
        testing::write(repo, "src/hot/feed.cpp", "fast\n")
    });
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let without = world.gate(&head, &[]).await;
    assert!(!without.allowed);
    assert!(
        without
            .checks
            .iter()
            .any(|c| c.check == "perf" && c.reason == Some(NotRunReason::NotChecked))
    );

    let candidate = world.candidate(&head);
    let commit = crate::git::commit_tree(
        &world.fixture.repo,
        &candidate.binding.merge_tree,
        &[&candidate.binding.base_commit, &head],
    )
    .unwrap();
    let ut = ut_report(&candidate.binding.base_commit, &commit, "PASS", "NO_CLAIM");
    let repo = world.fixture.repo.clone();
    let evidence = PerfEvidence::bind(ut, &candidate.binding, |c| {
        crate::git::tree_of(&repo, c).ok()
    })
    .unwrap();
    // Signed by the CI key it does not count; by the bench key it does.
    world.sign(
        &world.ci,
        RunnerKind::Bench,
        serde_json::to_string(&evidence).unwrap(),
        "perf-by-ci.json",
    );
    assert!(!world.gate(&head, &[]).await.allowed);
    world.sign(
        &world.bench,
        RunnerKind::Bench,
        serde_json::to_string(&evidence).unwrap(),
        "perf.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(decision.allowed, "{decision:?}");
    assert_eq!(decision.verdict, Verdict::Pass);
    let card = decision.perf_card.expect("card");
    assert!(card.contains("no significant change"), "{card}");
    assert!(card.contains("bench-host-1"), "{card}");
}

/// The perf card: a bench result for the old base does not count after the base
/// moves (perf evidence has no inner results, so only its own content key protects it).
#[tokio::test]
async fn a_bench_result_for_an_older_base_does_not_count() {
    let world = World::new(Fixture::new());
    let head = world.fixture.branch("hot", |repo| {
        testing::write(repo, "src/hot/feed.cpp", "fast\n")
    });
    let candidate = world.candidate(&head);
    let commit = crate::git::commit_tree(
        &world.fixture.repo,
        &candidate.binding.merge_tree,
        &[&candidate.binding.base_commit, &head],
    )
    .unwrap();
    let ut = ut_report(&candidate.binding.base_commit, &commit, "PASS", "NO_CLAIM");
    let repo = world.fixture.repo.clone();
    let old = PerfEvidence::bind(ut, &candidate.binding, |c| {
        crate::git::tree_of(&repo, c).ok()
    })
    .unwrap();
    world.sign(
        &world.bench,
        RunnerKind::Bench,
        serde_json::to_string(&old).unwrap(),
        "perf-old.json",
    );
    world
        .fixture
        .advance_main(|repo| testing::write(repo, "src/other.cpp", "base moves\n"));
    let report = world.run(&head).await;
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(!decision.allowed, "{decision:?}");
    assert!(decision.perf_card.is_none());
    assert!(
        decision
            .ignored
            .iter()
            .any(|why| why.contains("perf-old.json") && why.contains("another version")),
        "{:?}",
        decision.ignored
    );
}

/// A signed report whose results belong to another version (a signer bug, or a key used
/// outside `checks sign`) does not count either.
#[tokio::test]
async fn a_signed_report_with_results_for_another_version_does_not_count() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let mut report = world.run(&head).await;
    for result in &mut report.results {
        result.content_key = Some("0".repeat(64));
    }
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "odd.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(!decision.allowed);
    assert!(
        decision.ignored.iter().any(|why| why.contains("odd.json") && why.contains("inside it is for another version")),
        "{:?}",
        decision.ignored
    );
}

/// The run job, the signing job and the gate can run on different runner images: the run
/// job's build environment travels in its report and does not make it "another version".
/// A report that claims this candidate but whose key does not match its own binding does not
/// count.
#[tokio::test]
async fn the_run_jobs_build_environment_travels_with_its_report() {
    let world = World::new(Fixture::new());
    let head = world
        .fixture
        .branch("good", |repo| testing::write(repo, "src/book.cpp", "v2\n"));
    let candidate = world.candidate(&head);
    let mut report = world.run(&head).await;
    // As if the run job had CC=clang-18 and the gate job nothing.
    report.binding.env = report.binding.env.replace("CC=;", "CC=clang-18;");
    report.content_key = report.binding.content_key();
    for result in &mut report.results {
        result.content_key = Some(report.content_key.clone());
    }
    assert_ne!(report.content_key, candidate.binding.content_key());
    assert_eq!(
        report.binding.candidate_key(),
        candidate.binding.candidate_key()
    );
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&report).unwrap(),
        "run.json",
    );
    assert!(world.gate(&head, &[]).await.allowed);

    let mut lying = report.clone();
    lying.content_key = "f".repeat(64);
    for result in &mut lying.results {
        result.content_key = Some(lying.content_key.clone());
    }
    std::fs::remove_file(world.evidence.join("run.json")).unwrap();
    world.sign(
        &world.ci,
        RunnerKind::Ci,
        serde_json::to_string(&lying).unwrap(),
        "lying.json",
    );
    let decision = world.gate(&head, &[]).await;
    assert!(!decision.allowed);
    assert!(
        decision
            .ignored
            .iter()
            .any(|why| why.contains("lying.json")),
        "{:?}",
        decision.ignored
    );
}
