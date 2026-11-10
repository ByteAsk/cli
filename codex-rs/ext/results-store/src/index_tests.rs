//! The index tests (moved here with the code from `cpp-tools`, unchanged in
//! what they assert), plus the per-change side of `record_for_session`.

use pretty_assertions::assert_eq;

use super::*;
use crate::store;
use crate::store::Record;
use crate::workspace::tests::repo_with_upstream;
use crate::workspace::tests::run_git;

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.cpp"), "int a() { return 1; }\n").unwrap();
    run_git(dir.path(), &["init", "-q"]);
    run_git(dir.path(), &["add", "-A"]);
    run_git(dir.path(), &["commit", "-q", "-m", "b"]);
    dir
}

fn entry(root: &Path, key: &str, fresh: &str, tier: &str) -> Entry {
    Entry {
        result: CheckResult::pass("cpp_finish_gate", "ok").with_content_key(key),
        root: root.to_path_buf(),
        at: 1_000,
        fresh: fresh.to_string(),
        tier: tier.to_string(),
        source: "engine".to_string(),
    }
}

#[test]
fn the_newest_result_comes_first_and_one_key_is_kept_once_per_tier() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    assert!(latest(home.path(), project.path()).is_none());
    record(home.path(), entry(project.path(), "k1", "f1", "fast"));
    record(home.path(), entry(project.path(), "k2", "f2", "fast"));
    record(home.path(), entry(project.path(), "k1", "f1", "full"));
    record(home.path(), entry(project.path(), "k1", "f1", "fast"));
    let all = all(home.path(), project.path());
    let keys: Vec<(String, String)> = all
        .iter()
        .map(|e| (e.result.content_key.clone().unwrap(), e.tier.clone()))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("k1".to_string(), "fast".to_string()),
            ("k1".to_string(), "full".to_string()),
            ("k2".to_string(), "fast".to_string()),
        ]
    );
    for n in 0..30 {
        record(
            home.path(),
            entry(project.path(), &format!("x{n}"), "f", "fast"),
        );
    }
    assert_eq!(super::all(home.path(), project.path()).len(), KEEP);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_is_current_until_the_work_tree_changes() {
    let dir = repo();
    let root = dir.path();
    let budget = Duration::from_secs(10);
    std::fs::write(root.join("a.cpp"), "int a() { return 2; }\n").unwrap();
    let fresh = fingerprint(root, budget).await.expect("fingerprint");
    let stored = entry(root, "k", &fresh, "fast");
    assert_eq!(freshness(&stored, budget).await, Freshness::Current);

    // An already-modified file edited again: `git status` alone would not notice.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(root.join("a.cpp"), "int a() { return 3; }\n").unwrap();
    assert_eq!(freshness(&stored, budget).await, Freshness::Stale);
    let text = describe(&stored, Freshness::Stale, 1_090);
    assert!(text.contains("STALE"), "{text}");
    assert!(text.starts_with("Result: PASS"), "{text}");

    // A new file is a change too.
    let now = fingerprint(root, budget).await.unwrap();
    let again = entry(root, "k", &now, "fast");
    std::fs::write(root.join("b.cpp"), "int b();\n").unwrap();
    assert_eq!(freshness(&again, budget).await, Freshness::Stale);
}

/// A path with a space: the old parser took the last space-separated field as the path, so
/// a second edit to such a file was not seen and a STALE result read as current.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_edit_to_a_file_with_a_space_in_its_name_is_stale() {
    let dir = repo();
    let root = dir.path();
    let budget = Duration::from_secs(10);
    std::fs::write(root.join("order book.cpp"), "int a;\n").unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", "c"]);
    std::fs::write(root.join("order book.cpp"), "int b;\n").unwrap();
    let stored = entry(root, "k", &fingerprint(root, budget).await.unwrap(), "fast");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(root.join("order book.cpp"), "int c;\n").unwrap();
    assert_eq!(freshness(&stored, budget).await, Freshness::Stale);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_fingerprint_is_never_current() {
    let dir = repo();
    let stored = entry(dir.path(), "k", "", "fast");
    assert_eq!(
        freshness(&stored, Duration::from_secs(10)).await,
        Freshness::Stale
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_git_answer_means_freshness_unknown_never_current() {
    let plain = tempfile::tempdir().unwrap();
    let stored = entry(plain.path(), "k", "anything", "fast");
    assert_eq!(
        freshness(&stored, Duration::from_secs(5)).await,
        Freshness::Unknown
    );
    assert!(describe(&stored, Freshness::Unknown, 1_000).contains("freshness unknown"));
}

/// The index lives where the `cpp-tools` code put it (the format and path did not move).
#[test]
fn the_index_keeps_its_place_and_format() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    record(home.path(), entry(project.path(), "k1", "f1", "fast"));
    let canonical = std::fs::canonicalize(project.path()).unwrap();
    let path = home
        .path()
        .join("results")
        .join(project_key(&canonical))
        .join("index.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let first = &doc["entries"][0];
    let mut keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["at", "fresh", "result", "root", "source", "tier"]
    );
    assert_eq!(first["result"]["content_key"], "k1");
    let name = project_key(&canonical);
    assert!(name.contains('-') && name.len() > 17, "{name}");
}

/// `record_for_session` also writes the verdict into the session's change, tied to the
/// version observed when the check started (the fingerprint call), not the code at the time
/// it is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verdict_recorded_for_a_session_lands_in_its_change_with_the_checked_version() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.join("book.cpp"), "int depth() { return 9; }\n").unwrap();
    let fresh = fingerprint(&work, Duration::from_secs(10)).await.unwrap();
    let checked = workspace::probe(&work, Some(&work), Duration::from_secs(10)).version;
    // Edited again after the check started and before it is recorded.
    std::fs::write(work.join("book.cpp"), "int depth() { return 10; }\n").unwrap();
    let verdict = Entry {
        result: CheckResult::fail("cpp_finish_gate", "1 test failed").with_content_key("ck"),
        root: work.clone(),
        at: 2_000,
        fresh,
        tier: "fast".to_string(),
        source: "claude-code".to_string(),
    };
    record_for_session(home.path(), verdict.clone(), "claude-session-1");
    assert_eq!(latest(home.path(), &work), Some(verdict));
    let changes = store::load(&store::root(home.path())).changes;
    assert_eq!(changes.len(), 1);
    let (_, _, branch, session) = changes[0].opened().unwrap();
    assert_eq!((branch, session), ("main", "claude-session-1"));
    let results: Vec<(Option<String>, Option<String>)> = changes[0]
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Result {
                version, result, ..
            } => Some((version.clone(), result.content_key.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec![(checked, Some("ck".to_string()))]);
}
