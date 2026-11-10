use std::fs;
use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;

fn cache(plan: &str, checked_at: u64) -> PlanCache {
    PlanCache {
        plan: plan.to_string(),
        checked_at,
        account: "acct-1".to_string(),
        gate: true,
    }
}

fn write_raw(home: &Path, text: &str) {
    fs::write(path(home), text).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path(home), fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

const RELAXING: &str = r#"
version = 1
[sandbox]
relaxed_projects = ["/work/trusted"]
[uploads]
allowed_projects = ["/work/public"]
[debugger]
allowed_hosts = ["lab-box-1"]
"#;

#[test]
fn no_file_means_every_safe_default_and_no_complaint() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(load(home.path(), &[]), Loaded::default());
}

#[test]
fn plan_cache_round_trips() {
    let home = tempfile::tempdir().unwrap();
    write_plan_cache(home.path(), &cache("pro", 1_760_000_000)).unwrap();
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.problems, Vec::<String>::new());
    assert_eq!(loaded.policy.plan, Some(cache("pro", 1_760_000_000)));
}

#[test]
fn every_section_is_read() {
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), RELAXING);
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.problems, Vec::<String>::new());
    assert_eq!(
        loaded.policy,
        Policy {
            plan: None,
            relaxed_projects: vec![PathBuf::from("/work/trusted")],
            upload_projects: vec![PathBuf::from("/work/public")],
            debugger_hosts: vec!["lab-box-1".to_string()],
        }
    );
    assert!(loaded.policy.is_relaxed(Path::new("/work/trusted/sub")));
    assert!(!loaded.policy.is_relaxed(Path::new("/work/other")));
}

#[test]
fn a_corrupt_file_means_every_safe_default_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), &format!("{RELAXING}\n[plan\nbroken = "));
    let loaded = load(home.path(), &[]);
    assert_eq!(
        loaded.policy,
        Policy::default(),
        "nothing may survive a corrupt file"
    );
    assert_eq!(loaded.problems.len(), 1);
    assert!(
        loaded.problems[0].contains("byteask policy reset"),
        "{:?}",
        loaded.problems
    );
}

#[test]
fn a_wrongly_typed_value_is_corruption_not_a_partial_read() {
    let home = tempfile::tempdir().unwrap();
    write_raw(
        home.path(),
        "[sandbox]\nrelaxed_projects = \"/not/a/list\"\n[debugger]\nallowed_hosts = [\"h\"]\n",
    );
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.policy, Policy::default());
    assert_eq!(loaded.problems.len(), 1);
}

#[test]
fn writing_over_a_corrupt_file_keeps_a_backup_of_it() {
    let home = tempfile::tempdir().unwrap();
    let corrupt = "[sandbox\nrelaxed_projects = [\"/x\"]";
    write_raw(home.path(), corrupt);
    write_plan_cache(home.path(), &cache("free", 5)).unwrap();
    assert_eq!(
        fs::read_to_string(home.path().join(CORRUPT_BACKUP_NAME)).unwrap(),
        corrupt
    );
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.problems, Vec::<String>::new());
    assert_eq!(loaded.policy.plan, Some(cache("free", 5)));
    assert!(
        loaded.policy.relaxed_projects.is_empty(),
        "the corrupt relax setting must not come back"
    );
}

#[test]
fn an_update_keeps_sections_this_version_does_not_know() {
    let home = tempfile::tempdir().unwrap();
    write_raw(
        home.path(),
        &format!("{RELAXING}\n[results_store]\nmax_days = 30 # a newer engine's setting\n"),
    );
    write_plan_cache(home.path(), &cache("max", 9)).unwrap();
    let text = fs::read_to_string(path(home.path())).unwrap();
    assert!(
        text.contains("max_days = 30 # a newer engine's setting"),
        "{text}"
    );
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.policy.plan, Some(cache("max", 9)));
    assert_eq!(
        loaded.policy.relaxed_projects,
        vec![PathBuf::from("/work/trusted")]
    );
}

#[test]
fn a_file_inside_the_workspace_cannot_relax_anything_but_keeps_its_plan() {
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), RELAXING);
    write_plan_cache(home.path(), &cache("pro", 7)).unwrap();
    let loaded = load(home.path(), &[home.path().to_path_buf()]);
    assert!(loaded.policy.relaxed_projects.is_empty());
    assert!(loaded.policy.upload_projects.is_empty());
    assert!(loaded.policy.debugger_hosts.is_empty());
    assert_eq!(loaded.policy.plan, Some(cache("pro", 7)));
    assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
    assert!(loaded.problems[0].contains("which the AI can write"));
}

#[cfg(unix)]
#[test]
fn a_symlink_into_the_workspace_cannot_relax_anything() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let planted = repo.path().join("policy.toml");
    fs::write(&planted, RELAXING).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&planted, fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::os::unix::fs::symlink(&planted, path(home.path())).unwrap();
    let loaded = load(home.path(), &[repo.path().to_path_buf()]);
    assert!(loaded.policy.relaxed_projects.is_empty());
    assert!(loaded.policy.debugger_hosts.is_empty());
}

#[cfg(unix)]
#[test]
fn a_file_other_users_can_write_is_ignored_entirely() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), RELAXING);
    write_plan_cache(home.path(), &cache("ultra", 1)).unwrap();
    fs::set_permissions(path(home.path()), fs::Permissions::from_mode(0o666)).unwrap();
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.policy, Policy::default());
    assert!(
        loaded.problems[0].contains("chmod 600"),
        "{:?}",
        loaded.problems
    );
}

#[cfg(unix)]
#[test]
fn written_file_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    write_plan_cache(home.path(), &cache("pro", 1)).unwrap();
    let mode = fs::metadata(path(home.path()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn relative_paths_are_dropped_with_a_reason() {
    let home = tempfile::tempdir().unwrap();
    write_raw(
        home.path(),
        "[sandbox]\nrelaxed_projects = [\"relative/dir\", \"/abs\"]\n",
    );
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.policy.relaxed_projects, vec![PathBuf::from("/abs")]);
    assert_eq!(loaded.problems.len(), 1);
}

#[test]
fn reset_moves_the_file_aside() {
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), RELAXING);
    assert!(reset(home.path()).unwrap());
    assert_eq!(load(home.path(), &[]), Loaded::default());
    assert!(home.path().join(CORRUPT_BACKUP_NAME).exists());
    assert!(!reset(home.path()).unwrap(), "nothing left to reset");
}

#[test]
fn clear_plan_cache_removes_only_the_plan() {
    let home = tempfile::tempdir().unwrap();
    write_raw(home.path(), RELAXING);
    write_plan_cache(home.path(), &cache("pro", 3)).unwrap();
    clear_plan_cache(home.path()).unwrap();
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.policy.plan, None);
    assert_eq!(
        loaded.policy.relaxed_projects,
        vec![PathBuf::from("/work/trusted")]
    );
}

#[test]
fn concurrent_writers_never_leave_a_torn_file() {
    let home = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for t in 0..8u64 {
            let home = home.path();
            scope.spawn(move || {
                for i in 0..40u64 {
                    write_plan_cache(home, &cache("pro", t * 1000 + i)).unwrap();
                }
            });
        }
    });
    let loaded = load(home.path(), &[]);
    assert_eq!(loaded.problems, Vec::<String>::new());
    assert!(loaded.policy.plan.is_some());
}

#[test]
fn relax_and_unrelax_a_project() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let inner = project.path().join("sub");
    fs::create_dir(&inner).unwrap();

    let stored = set_relaxed(home.path(), project.path(), true).expect("relax");
    assert_eq!(stored, fs::canonicalize(project.path()).unwrap());
    // Twice is still one entry.
    set_relaxed(home.path(), project.path(), true).expect("relax again");
    set_relaxed(home.path(), &inner, true).expect("relax a subdirectory");
    let loaded = load(home.path(), &[]);
    assert_eq!(
        loaded.policy.relaxed_projects.len(),
        2,
        "{:?}",
        loaded.policy
    );
    assert!(loaded.policy.is_relaxed(&stored));
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);

    // Turning the project back on also removes what was relaxed inside it.
    set_relaxed(home.path(), project.path(), false).expect("unrelax");
    let loaded = load(home.path(), &[]);
    assert!(
        loaded.policy.relaxed_projects.is_empty(),
        "{:?}",
        loaded.policy
    );
}

#[test]
fn relax_needs_an_existing_directory() {
    let home = tempfile::tempdir().unwrap();
    assert!(set_relaxed(home.path(), Path::new("/no/such/project/t4"), true).is_err());
    assert!(
        !path(home.path()).exists(),
        "nothing is written for a refused path"
    );
}
