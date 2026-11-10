use pretty_assertions::assert_eq;

use super::*;

const DAY: u64 = 86_400;

fn limits(max_days: u64, max_bytes: u64) -> ResultsStoreLimits {
    ResultsStoreLimits {
        max_days,
        max_bytes,
    }
}

fn open(at: u64) -> Record {
    Record::Open {
        at,
        repo: "/work/repo".to_string(),
        branch: "main".to_string(),
        session: "s".to_string(),
        head: None,
    }
}

fn result(at: u64, payload: Option<&str>) -> Record {
    Record::Result {
        at,
        version: Some("v1".to_string()),
        result: CheckResult::pass("cpp_check", "No findings."),
        payload: payload.map(str::to_string),
    }
}

/// A change opened at `opened`, active until `last`, closed if `closed`, with
/// `payload_bytes` of payload.
fn change(root: &Path, stem: &str, opened: u64, last: u64, closed: bool, payload_bytes: usize) {
    append(root, stem, &open(opened)).unwrap();
    append(
        root,
        stem,
        &result(last, (payload_bytes > 0).then_some("1.txt")),
    )
    .unwrap();
    if payload_bytes > 0 {
        write_payload(root, stem, "1.txt", &"x".repeat(payload_bytes)).unwrap();
    }
    if closed {
        append(
            root,
            stem,
            &Record::Close {
                at: last + 1,
                how: CloseHow::SessionEnded,
            },
        )
        .unwrap();
    }
}

fn store() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let root = root(home.path());
    (home, root)
}

fn stems(root: &Path) -> Vec<String> {
    load(root)
        .changes
        .into_iter()
        .map(|change| change.stem)
        .collect()
}

fn has_payloads(root: &Path, stem: &str) -> bool {
    payload_dir(root, stem).exists()
}

fn evicted(root: &Path, stem: &str) -> bool {
    read_change(&change_path(root, stem))
        .unwrap()
        .records
        .iter()
        .any(|record| matches!(record, Record::PayloadEvicted { .. }))
}

#[test]
fn records_round_trip_and_the_store_is_private() {
    let (_home, root) = store();
    change(&root, "a", 10, 20, true, 5);
    let loaded = load(&root);
    assert_eq!(loaded.changes.len(), 1);
    let log = &loaded.changes[0];
    assert_eq!(log.opened().map(|(at, ..)| at), Some(10));
    assert_eq!(log.last_activity(), Some(20));
    assert_eq!(log.closed(), Some((21, CloseHow::SessionEnded)));
    assert!(log.unreadable.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join(CHANGES)), 0o700);
        assert_eq!(mode(&change_path(&root, "a")), 0o600);
        assert_eq!(mode(&payload_dir(&root, "a").join("1.txt")), 0o600);
    }
}

#[test]
fn an_unreadable_line_is_kept_as_a_problem_never_dropped() {
    let (_home, root) = store();
    change(&root, "a", 10, 20, false, 0);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(change_path(&root, "a"))
        .unwrap();
    file.write_all(b"{\"t\":\"result\",\"at\":30,\"res\n")
        .unwrap();
    append(&root, "a", &result(40, None)).unwrap();
    let loaded = load(&root);
    let log = &loaded.changes[0];
    assert_eq!(log.records.len(), 3, "the readable records survive");
    assert_eq!(log.unreadable, vec![Some(20)]);
    assert_eq!(loaded.incomplete_since(), Some(Some(20)));
}

#[test]
fn a_missing_home_is_not_recreated_by_recording() {
    let home = tempfile::tempdir().unwrap();
    let gone = home.path().join("deleted-home");
    let root = root(&gone);
    assert!(append(&root, "a", &open(1)).is_err());
    assert!(write_payload(&root, "a", "1.txt", "x").is_err());
    assert!(!gone.exists());
}

#[test]
fn a_payload_is_cut_at_the_limit() {
    let (_home, root) = store();
    append(&root, "a", &open(1)).unwrap();
    let big = "é".repeat(MAX_PAYLOAD_BYTES); // two bytes each: the cut lands mid-character
    let kept = write_payload(&root, "a", "1.txt", &big).unwrap();
    assert!(kept <= MAX_PAYLOAD_BYTES as u64);
    let text = fs::read_to_string(payload_dir(&root, "a").join("1.txt")).unwrap();
    assert!(text.ends_with("limited to 1 MiB]\n"));
}

#[test]
fn a_change_idle_thirty_days_closes_and_one_idle_less_does_not() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(&root, "idle", now - 40 * DAY, now - 31 * DAY, false, 0);
    change(&root, "recent", now - 40 * DAY, now - 29 * DAY, false, 0);
    let outcome = gc(&root, now, &limits(90, 1 << 30)).unwrap();
    assert_eq!(outcome.closed_idle, vec!["idle".to_string()]);
    let closed = |stem: &str| read_change(&change_path(&root, stem)).unwrap().closed();
    assert_eq!(closed("idle"), Some((now, CloseHow::Idle)));
    assert_eq!(closed("recent"), None);
}

/// Closed changes go after 90 days, but never a result an open change still refers to.
#[test]
fn closed_changes_go_after_max_days_and_open_ones_keep_old_results() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(
        &root,
        "old-closed",
        now - 200 * DAY,
        now - 91 * DAY,
        true,
        10,
    );
    change(
        &root,
        "young-closed",
        now - 200 * DAY,
        now - 89 * DAY,
        true,
        10,
    );
    // Opened 120 days ago and still in use: its 120-day-old results stay.
    append(&root, "long-open", &open(now - 120 * DAY)).unwrap();
    append(&root, "long-open", &result(now - 120 * DAY, None)).unwrap();
    append(&root, "long-open", &result(now - DAY, None)).unwrap();
    let outcome = gc(&root, now, &limits(90, 1 << 30)).unwrap();
    assert_eq!(outcome.deleted, vec!["old-closed".to_string()]);
    assert!(!has_payloads(&root, "old-closed"));
    assert_eq!(stems(&root), vec!["long-open", "young-closed"]);
    let long = read_change(&change_path(&root, "long-open")).unwrap();
    assert_eq!(long.records.len(), 3, "an open change keeps every result");
    // The limit is the policy's, not a constant.
    gc(&root, now, &limits(30, 1 << 30)).unwrap();
    assert_eq!(stems(&root), vec!["long-open"]);
}

/// Below 30 days the idle close does not protect an open change: the age
/// rule itself must spare it.
#[test]
fn with_max_days_below_the_idle_close_an_open_change_still_keeps_its_results() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(&root, "closed", now - 20 * DAY, now - 10 * DAY, true, 0);
    change(&root, "open", now - 20 * DAY, now - 10 * DAY, false, 0);
    let outcome = gc(&root, now, &limits(5, 1 << 30)).unwrap();
    assert_eq!(outcome.deleted, vec!["closed".to_string()]);
    assert_eq!(stems(&root), vec!["open"]);
}

#[test]
fn the_count_cap_removes_the_oldest_closed_changes_only() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(&root, "c1", now - 9 * DAY, now - 8 * DAY, true, 0);
    change(&root, "c2", now - 9 * DAY, now - 6 * DAY, true, 0);
    change(&root, "c3", now - 9 * DAY, now - 4 * DAY, true, 0);
    change(
        &root,
        "open-oldest",
        now - 20 * DAY,
        now - 10 * DAY,
        false,
        0,
    );
    let outcome = gc_with(&root, now, &limits(90, 1 << 30), 2).unwrap();
    assert_eq!(outcome.deleted, vec!["c1".to_string()]);
    assert_eq!(stems(&root), vec!["c2", "c3", "open-oldest"]);
}

/// Over the cap, closed changes lose their payloads first,
/// oldest first, and only as many as needed; open changes keep theirs.
#[test]
fn over_the_cap_closed_payloads_go_first_oldest_first() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(
        &root,
        "closed-old",
        now - 9 * DAY,
        now - 8 * DAY,
        true,
        4_000,
    );
    change(
        &root,
        "closed-new",
        now - 9 * DAY,
        now - 2 * DAY,
        true,
        4_000,
    );
    change(&root, "open-a", now - 7 * DAY, now - DAY, false, 4_000);
    change(&root, "open-b", now - 3 * DAY, now - DAY, false, 4_000);
    let outcome = gc(&root, now, &limits(90, 10_000)).unwrap();
    assert_eq!(
        (
            outcome.evicted_closed.clone(),
            outcome.evicted_pinned.clone()
        ),
        (
            vec!["closed-old".to_string(), "closed-new".to_string()],
            vec![]
        )
    );
    assert_eq!(outcome.payload_bytes_after, 8_000);
    assert!(has_payloads(&root, "open-a") && has_payloads(&root, "open-b"));
    assert!(evicted(&root, "closed-old") && evicted(&root, "closed-new"));
    // Evidence is untouched: every change and its result are still there.
    assert_eq!(stems(&root).len(), 4);
}

/// When pinned payloads alone exceed the cap, the oldest
/// pinned change loses its payloads, its evidence stays, marked
/// 'payload evicted', and it stays open.
#[test]
fn over_the_cap_with_only_pinned_payloads_the_oldest_pinned_change_loses_them() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(&root, "pinned-old", now - 7 * DAY, now - DAY, false, 6_000);
    change(&root, "pinned-new", now - 3 * DAY, now - DAY, false, 6_000);
    let outcome = gc(&root, now, &limits(90, 10_000)).unwrap();
    assert_eq!(outcome.evicted_pinned, vec!["pinned-old".to_string()]);
    assert!(outcome.evicted_closed.is_empty());
    assert_eq!(outcome.payload_bytes_after, 6_000);
    assert!(!has_payloads(&root, "pinned-old"));
    assert!(has_payloads(&root, "pinned-new"));
    let old = read_change(&change_path(&root, "pinned-old")).unwrap();
    assert!(evicted(&root, "pinned-old"));
    assert_eq!(
        old.closed(),
        None,
        "evicting payloads does not close the change"
    );
    assert!(
        old.records
            .iter()
            .any(|record| matches!(record, Record::Result { .. })),
        "the evidence record stays"
    );
    assert!(!evicted(&root, "pinned-new"));
}

#[test]
fn payload_directories_without_a_change_are_removed() {
    let (_home, root) = store();
    change(&root, "a", 10, 20, false, 5);
    write_payload(&root, "orphan", "1.txt", "x").unwrap();
    gc(&root, 30, &limits(90, 1 << 30)).unwrap();
    assert!(!payload_dir(&root, "orphan").exists());
    assert!(has_payloads(&root, "a"));
}

#[test]
fn a_cleanup_already_running_elsewhere_makes_this_one_a_no_op() {
    let (_home, root) = store();
    let now = 1_000 * DAY;
    change(&root, "idle", now - 40 * DAY, now - 31 * DAY, false, 0);
    let held = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(GC_LOCK))
        .unwrap();
    held.lock().unwrap();
    // A second open file description of the same lock file conflicts with it.
    assert_eq!(
        gc(&root, now, &limits(90, 1)).unwrap(),
        GcOutcome::default()
    );
    assert_eq!(
        read_change(&change_path(&root, "idle")).unwrap().closed(),
        None
    );
    held.unlock().unwrap();
    assert_eq!(
        gc(&root, now, &limits(90, 1)).unwrap().closed_idle,
        vec!["idle".to_string()]
    );
}

/// The clean-up over the whole store: the finish gate's index entries older than `max_days` go, the
/// agent hooks' session files idle 30 days go, and a closed change holding nothing goes.
#[test]
fn the_cleanup_covers_the_gate_index_session_files_and_empty_changes() {
    let (home, root) = store();
    let now = 1_000 * DAY;
    let project = tempfile::tempdir().unwrap();
    let verdict = |at: u64, key: &str| crate::index::Entry {
        result: CheckResult::pass("cpp_finish_gate", "ok").with_content_key(key),
        root: project.path().to_path_buf(),
        at,
        fresh: "f".to_string(),
        tier: "fast".to_string(),
        source: "engine".to_string(),
    };
    crate::index::record(home.path(), verdict(now - 91 * DAY, "old"));
    crate::index::record(home.path(), verdict(now - DAY, "new"));
    let sessions = crate::index::project_dir(home.path(), project.path()).join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let idle = sessions.join("idle.json");
    let active = sessions.join("active.json");
    fs::write(&idle, "{}").unwrap();
    fs::write(&active, "{}").unwrap();
    let month_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * DAY);
    fs::File::options()
        .write(true)
        .open(&idle)
        .unwrap()
        .set_modified(month_ago)
        .unwrap();
    // An agent session that started and changed nothing, then ended.
    append(&root, "empty", &open(now - 2 * DAY)).unwrap();
    append(
        &root,
        "empty",
        &Record::Close {
            at: now - 2 * DAY,
            how: CloseHow::SessionEnded,
        },
    )
    .unwrap();
    change(&root, "kept", now - 2 * DAY, now - DAY, true, 0);

    let outcome = gc(&root, now, &limits(90, 1 << 30)).unwrap();
    assert_eq!(outcome.index_entries_deleted, 1);
    assert_eq!(outcome.session_files_deleted, 1);
    assert_eq!(outcome.deleted, vec!["empty".to_string()]);
    let keys: Vec<String> = crate::index::all(home.path(), project.path())
        .into_iter()
        .filter_map(|entry| entry.result.content_key)
        .collect();
    assert_eq!(keys, vec!["new".to_string()]);
    assert!(!idle.exists() && active.exists());
    assert_eq!(stems(&root), vec!["kept"]);
}
