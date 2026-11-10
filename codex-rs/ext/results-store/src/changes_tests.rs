use pretty_assertions::assert_eq;

use super::*;
use crate::workspace;
use crate::workspace::tests::probe_now;
use crate::workspace::tests::repo_with_upstream;

fn kinds(home: &Path) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = store::load(&store::root(home))
        .changes
        .iter()
        .map(|change| {
            change
                .records
                .iter()
                .map(|record| match record {
                    Record::Open { session, .. } => format!("open:{session}"),
                    Record::Base { .. } => "base".to_string(),
                    Record::Edit { .. } => "edit".to_string(),
                    Record::Result { result, .. } => format!("{}:{}", result.check, result.verdict),
                    Record::Close { how, .. } => format!("close:{how:?}"),
                    Record::PayloadEvicted { .. } => "payload_evicted".to_string(),
                    Record::Capped { .. } => "capped".to_string(),
                })
                .collect()
        })
        .collect();
    out.sort();
    out
}

/// Three writers of one agent session, as in Claude Code: its SessionStart hook, the
/// toolbox (MCP) recording tool results, and its Stop hook recording the edit and the gate's
/// verdict. Each call stands for a separate process: nothing is shared but the files.
#[test]
fn one_agent_session_s_writers_meet_in_one_change() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    let session = "claude-session-7";
    let check = |verdict: &str| match verdict {
        "pass" => CheckResult::pass("cpp_finish_gate", "ok"),
        _ => CheckResult::fail("cpp_finish_gate", "1 test failed"),
    };
    begin_session(home.path(), session, &probe_now(&work), 100).unwrap();
    let tool_result = CheckResult::pass("cpp_check", "No findings.");
    append(
        home.path(),
        session,
        &probe_now(&work),
        105,
        Event::Result {
            result: &tool_result,
            answer: Some("cppcheck said nothing"),
        },
    )
    .unwrap();
    std::fs::write(work.join("book.cpp"), "int depth() { return 2; }\n").unwrap();
    append(home.path(), session, &probe_now(&work), 101, Event::Edit).unwrap();
    append(
        home.path(),
        session,
        &probe_now(&work),
        130,
        Event::Result {
            result: &check("fail"),
            answer: None,
        },
    )
    .unwrap();
    // Another session in the same repository and branch is another change.
    append(
        home.path(),
        "other-session",
        &probe_now(&work),
        140,
        Event::Edit,
    )
    .unwrap();
    assert_eq!(
        kinds(home.path()),
        vec![
            vec![
                "open:claude-session-7".to_string(),
                "base".to_string(),
                "cpp_check:PASS".to_string(),
                "edit".to_string(),
                "cpp_finish_gate:FAIL".to_string(),
            ],
            vec!["open:other-session".to_string(), "edit".to_string()],
        ]
    );
    // The base is the code as the session started.
    let changes = store::load(&store::root(home.path())).changes;
    let ours = changes
        .iter()
        .find(|change| change.opened().is_some_and(|(_, _, _, s)| s == session))
        .unwrap();
    let base = workspace::probe(&work, Some(&work), workspace::PROBE_BUDGET);
    assert_ne!(ours.base_version(), base.version.as_deref(), "edited since");
}

#[test]
fn a_resumed_session_keeps_its_first_base() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    let first = probe_now(&work);
    begin_session(home.path(), "s", &first, 10).unwrap();
    std::fs::write(work.join("book.cpp"), "int depth() { return 2; }\n").unwrap();
    begin_session(home.path(), "s", &probe_now(&work), 20).unwrap();
    let changes = store::load(&store::root(home.path())).changes;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].base_version(), first.version.as_deref());
}

/// A base taken after the change already opened (the toolbox recorded first) is still
/// recorded.
#[test]
fn a_late_session_start_still_records_the_base() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    let probe = probe_now(&work);
    let result = CheckResult::pass("cpp_check", "ok");
    append(
        home.path(),
        "s",
        &probe,
        10,
        Event::Result {
            result: &result,
            answer: None,
        },
    )
    .unwrap();
    begin_session(home.path(), "s", &probe, 11).unwrap();
    let changes = store::load(&store::root(home.path())).changes;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].base_version(), probe.version.as_deref());
}

/// Writers in parallel (the engine and a hook at once) never lose a record, never open two
/// changes, and keep the per-change result cap exactly: the lock is what makes "read the
/// change's state, then append" one step.
#[test]
fn parallel_writers_of_one_session_lose_nothing_and_keep_the_cap() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    let probe = probe_now(&work);
    let start = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for writer in 0..8u64 {
            let (home, probe, start) = (home.path(), &probe, &start);
            scope.spawn(move || {
                start.wait();
                for n in 0..25u64 {
                    let result = CheckResult::pass("cpp_check", &format!("{writer}/{n}"));
                    append_capped(
                        home,
                        "s",
                        probe,
                        1_000 + n,
                        Event::Result {
                            result: &result,
                            answer: None,
                        },
                        100,
                    )
                    .unwrap();
                }
            });
        }
    });
    let changes = store::load(&store::root(home.path())).changes;
    assert_eq!(changes.len(), 1, "one change, opened once");
    let log = &changes[0];
    assert!(log.unreadable.is_empty());
    let count = |is: fn(&Record) -> bool| log.records.iter().filter(|record| is(record)).count();
    assert_eq!(
        (
            count(|r| matches!(r, Record::Open { .. })),
            count(|r| matches!(r, Record::Result { .. })),
            count(|r| matches!(r, Record::Capped { .. })),
        ),
        (1, 100, 1)
    );
}

#[test]
fn ending_a_session_closes_its_changes_and_leaves_others_open() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    let probe = probe_now(&work);
    append(home.path(), "ending", &probe, 10, Event::Edit).unwrap();
    append(home.path(), "staying", &probe, 10, Event::Edit).unwrap();
    end_session(
        home.path(),
        &[change_key(&probe.repo, &probe.branch, "ending")],
        20,
    );
    let changes = store::load(&store::root(home.path())).changes;
    let closed: Vec<(String, bool)> = {
        let mut closed: Vec<(String, bool)> = changes
            .iter()
            .map(|change| {
                (
                    change.opened().unwrap().3.to_string(),
                    change.closed().is_some(),
                )
            })
            .collect();
        closed.sort();
        closed
    };
    assert_eq!(
        closed,
        vec![("ending".to_string(), true), ("staying".to_string(), false)]
    );
}

/// After a push the next change starts from the pushed code (its base).
#[test]
fn the_change_after_a_push_starts_from_the_pushed_code() {
    let (_dir, work) = repo_with_upstream();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work.join("book.cpp"), "int depth() { return 2; }\n").unwrap();
    append(home.path(), "s", &probe_now(&work), 10, Event::Edit).unwrap();
    crate::workspace::tests::run_git(&work, &["commit", "-q", "-am", "x"]);
    crate::workspace::tests::run_git(&work, &["push", "-q"]);
    let pushed = probe_now(&work);
    let result = CheckResult::pass("cpp_check", "ok");
    append(
        home.path(),
        "s",
        &pushed,
        20,
        Event::Result {
            result: &result,
            answer: None,
        },
    )
    .unwrap();
    let changes = store::load(&store::root(home.path())).changes;
    assert_eq!(changes.len(), 2);
    let next = changes
        .iter()
        .find(|change| change.closed().is_none())
        .unwrap();
    assert_eq!(next.base_version(), pushed.version.as_deref());
}
