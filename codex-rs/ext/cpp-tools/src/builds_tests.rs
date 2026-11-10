use super::*;
use std::io::Write as _;

// What these tests guard against: `project_lock` was once a static mutex inside ONE
// process, so a second agent session on the same worktree (a separate
// process) was never made to wait, and every CMake sanitizer build started
// cold in a fresh temp directory that was deleted afterwards.

/// Environment for [`lock_child_session`], which this test binary runs as a
/// separate process to play "the other agent session".
const CHILD_DIR: &str = "BYTEASK_BUILDS_TEST_CHILD_DIR";
const CHILD_LOG: &str = "BYTEASK_BUILDS_TEST_CHILD_LOG";
const CHILD_HOLD_MS: &str = "BYTEASK_BUILDS_TEST_CHILD_HOLD_MS";
const CHILD_READY: &str = "BYTEASK_BUILDS_TEST_CHILD_READY";

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

/// Not a test of its own: a no-op unless a parent test started this binary
/// with [`CHILD_DIR`] set. Then it is another session: it takes the project
/// lock, records when it held it, and holds it for a while.
#[test]
fn lock_child_session() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    let log = std::env::var_os(CHILD_LOG).expect("log path");
    let hold: u64 = std::env::var(CHILD_HOLD_MS)
        .expect("hold")
        .parse()
        .expect("hold ms");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let guard = project_lock(Path::new(&dir), Duration::from_secs(60))
            .await
            .expect("the lock within a minute");
        let start = nanos();
        if let Some(ready) = std::env::var_os(CHILD_READY) {
            std::fs::write(ready, b"held").expect("ready file");
        }
        tokio::time::sleep(Duration::from_millis(hold)).await;
        let end = nanos();
        drop(guard);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("log");
        writeln!(file, "{start} {end}").expect("write log");
    });
}

fn spawn_session(
    dir: &Path,
    log: &Path,
    hold_ms: u64,
    ready: Option<&Path>,
) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "--exact",
            "builds::tests::lock_child_session",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, dir)
        .env(CHILD_LOG, log)
        .env(CHILD_HOLD_MS, hold_ms.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(ready) = ready {
        command.env(CHILD_READY, ready);
    }
    command.spawn().expect("spawn the second session")
}

fn wait_for(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the other session never took the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_sessions_on_one_project_never_build_at_the_same_time() {
    // Separate processes, one project, all at once. Each session
    // records when it held the lock; no two of those intervals may overlap.
    let project = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("held.log");
    let mut sessions: Vec<_> = (0..3)
        .map(|_| spawn_session(project.path(), &log, 300, None))
        .collect();
    for session in &mut sessions {
        assert!(session.wait().expect("wait").success(), "a session failed");
    }
    let mut held: Vec<(u128, u128)> = std::fs::read_to_string(&log)
        .expect("log")
        .lines()
        .map(|line| {
            let (start, end) = line.split_once(' ').expect("two numbers");
            (start.parse().expect("start"), end.parse().expect("end"))
        })
        .collect();
    assert_eq!(held.len(), 3, "every session got the lock: {held:?}");
    held.sort();
    for pair in held.windows(2) {
        assert!(
            pair[1].0 >= pair[0].1,
            "two sessions held the project at once: {held:?}"
        );
    }
}

#[tokio::test]
async fn a_session_waits_for_another_sessions_build_then_proceeds() {
    let project = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let ready = scratch.path().join("ready");
    let mut other = spawn_session(
        project.path(),
        &scratch.path().join("log"),
        1500,
        Some(&ready),
    );
    wait_for(&ready);

    let asked = std::time::Instant::now();
    let guard = project_lock(project.path(), Duration::from_secs(30))
        .await
        .expect("the lock once the other session is done");
    let waited = asked.elapsed();
    drop(guard);
    assert!(other.wait().expect("wait").success());
    assert!(
        waited >= Duration::from_millis(900),
        "got the lock after {waited:?} while another session was still building"
    );
}

#[tokio::test]
async fn a_project_busy_for_the_whole_wait_is_reported_not_built() {
    let project = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let ready = scratch.path().join("ready");
    let mut other = spawn_session(
        project.path(),
        &scratch.path().join("log"),
        3000,
        Some(&ready),
    );
    wait_for(&ready);

    let busy = project_lock(project.path(), Duration::from_millis(300))
        .await
        .err()
        .expect("busy: the other session holds the project");
    assert!(busy.waited >= Duration::from_millis(200), "{busy:?}");
    assert!(
        busy.summary().contains("Another ByteAsk session"),
        "{}",
        busy.summary()
    );
    other.kill().ok();
    other.wait().ok();
}

#[test]
fn project_keys_are_stable_readable_and_unique() {
    // Pinned: a key that changed between releases would turn every user's
    // build tree cold after an update.
    assert_eq!(
        project_key(Path::new("/home/dev/src/bookd")),
        "bookd-af3ee5f85eb57204"
    );
    assert_ne!(
        project_key(Path::new("/a/bookd")),
        project_key(Path::new("/b/bookd")),
        "same name, different checkout"
    );
    assert!(project_key(Path::new("/x/we ird$name")).starts_with("weirdname-"));
    assert!(project_key(Path::new("/x/.hidden")).starts_with("project-"));
    assert!(project_key(Path::new("/")).starts_with("project-"));
}

#[test]
fn a_build_dir_is_per_project_and_setting_and_never_in_the_project() {
    let state = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let asan = "sanitize asan_ubsan -fsanitize=address,undefined";
    let tsan = "sanitize tsan -fsanitize=thread";

    let a_asan = stable_build_dir_in(state.path(), a.path(), asan).expect("dir");
    assert_eq!(
        stable_build_dir_in(state.path(), a.path(), asan),
        Some(a_asan.clone()),
        "the same project and setting must reuse one tree; that is what makes it incremental"
    );
    assert_ne!(
        stable_build_dir_in(state.path(), a.path(), tsan),
        Some(a_asan.clone())
    );
    assert_ne!(
        stable_build_dir_in(state.path(), b.path(), asan),
        Some(a_asan.clone())
    );
    assert!(
        a_asan.starts_with(state.path().join("builds")),
        "{}",
        a_asan.display()
    );
    assert!(
        !a_asan.starts_with(a.path()),
        "never inside the user's repository"
    );
    let name = a_asan.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("asan_ubsan-"), "{name}");
}

#[test]
fn the_real_state_dir_is_private_and_build_dirs_are_recognised() {
    let state = state_dir().expect("a state dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&state).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o700, "{}", state.display());
        // SAFETY: geteuid has no preconditions.
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    }
    let project = tempfile::tempdir().unwrap();
    let dir = stable_build_dir(project.path(), "sanitize tsan x").expect("dir");
    assert!(is_stable_build_dir(&dir));
    assert!(!is_stable_build_dir(
        &std::env::temp_dir().join("byteask-cpp-sanitize-build-1")
    ));
}

#[cfg(unix)]
#[test]
fn an_unsafe_state_dir_is_tightened_or_refused() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();

    let open = scratch.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ensure_private_dir(&open));
    let mode = std::fs::metadata(&open).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "a directory others could write must be closed first"
    );

    // A symlink (someone else's planted path in /tmp) is never followed.
    let target = scratch.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = scratch.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(!ensure_private_dir(&link));
}

#[cfg(target_os = "linux")]
#[test]
fn the_cache_dir_follows_the_xdg_rules() {
    use std::ffi::OsString;
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    };
    assert_eq!(
        cache_dir_from(env(&[
            ("XDG_CACHE_HOME", "/big/cache"),
            ("HOME", "/home/u")
        ])),
        Some(PathBuf::from("/big/cache"))
    );
    // A relative XDG_CACHE_HOME is invalid by the spec and ignored.
    assert_eq!(
        cache_dir_from(env(&[("XDG_CACHE_HOME", "rel"), ("HOME", "/home/u")])),
        Some(PathBuf::from("/home/u/.cache"))
    );
    assert_eq!(cache_dir_from(env(&[])), None);
}

#[test]
fn stale_build_trees_are_pruned_unless_in_use_or_ours() {
    let state = tempfile::tempdir().unwrap();
    let old = tempfile::tempdir().unwrap();
    let busy = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    let mut trees = Vec::new();
    for project in [&old, &busy, &current] {
        let dir =
            stable_build_dir_in(state.path(), project.path(), "sanitize asan_ubsan x").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("obj.o"), b"x").unwrap();
        trees.push(dir);
    }
    // `busy` is being built by another session right now.
    let lock = open_lock_file_in(
        state.path(),
        &project_key(&std::fs::canonicalize(busy.path()).unwrap()),
    )
    .unwrap();
    lock.lock().unwrap();

    // Nothing is old yet.
    assert_eq!(
        prune_stale_builds_in(state.path(), current.path(), SystemTime::now()),
        0
    );
    for tree in &trees {
        assert!(tree.exists());
    }

    let later = SystemTime::now() + BUILD_KEEP + Duration::from_secs(3600);
    assert_eq!(
        prune_stale_builds_in(state.path(), current.path(), later),
        1
    );
    assert!(!trees[0].exists(), "an unused tree past the keep time goes");
    assert!(
        trees[1].exists(),
        "a tree whose project is locked is never deleted"
    );
    assert!(trees[2].exists(), "the calling project's own tree stays");
}
