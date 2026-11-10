//! The C/C++ tools' own build tree for a project, and what a targeted check states about it.
//!
//! One tree per repository, outside it: `<scratch>/testsel-<hash>/build` (see [`own_tree`]).
//! `cpp_test_select`, `cpp_finish_check` and the background warm build
//! ([`crate::testsel::warm`]) all build in it, so whatever one built the next reuses: it is
//! configured once per repository and commit, then only built incrementally. One build at a
//! time per tree, across every ByteAsk process of this user ([`lock_tree`], the per-tree half
//! of [`crate::builds::project_lock`]); each command inside still takes the session-wide build
//! slot ([`crate::resources::build_slot`]) and the memory-aware job count.
//!
//! A check builds only the targets the selected tests run ([`plan_targets`]); when the build
//! model cannot say which program a test runs, it falls back to the smallest superset it can
//! name and says so. The answer states the targets built, the exact test filter
//! ([`run_lines`]) and `git diff --check` ([`diff_check`]).

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use super::Model;
use super::TestEntry;
use super::exec::RunItem;
use super::exec::filtered_argv;
use super::exec::regex_escape;
use crate::builds::ProjectBusy;
use crate::builds::ProjectGuard;

/// The tools' directory for a project: `testsel-<hash>` in this user's private scratch
/// directory ([`crate::gate::scratch_root`], bounded there), outside the project, so nothing it
/// builds shows up in the repository; never a predictable name in the shared `/tmp`.
pub(crate) fn scratch_dir(root: &Path) -> PathBuf {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let dir = crate::gate::scratch_root().join(format!(
        "testsel-{:016x}",
        crate::base_run_cache::fnv(key.to_string_lossy().as_bytes())
    ));
    crate::gate::touch_scratch(&dir);
    dir
}

/// The tools' own CMake build tree for the project at `root`.
pub(crate) fn own_tree(root: &Path) -> PathBuf {
    scratch_dir(root).join("build")
}

/// Hold `tree` for a build: waits up to `wait` for another task or session building in it
/// (e.g. the background warm build), which the caller then finds already done.
pub(crate) async fn lock_tree(tree: &Path, wait: Duration) -> Result<ProjectGuard, ProjectBusy> {
    let _ = std::fs::create_dir_all(tree);
    crate::builds::project_lock(tree, wait).await
}

/// [`lock_tree`] without waiting.
pub(crate) fn try_lock_tree(tree: &Path) -> Option<ProjectGuard> {
    let _ = std::fs::create_dir_all(tree);
    crate::builds::try_project_lock(tree)
}

// ---- one build owner per tree (switch `[byteask_cpp] tree_owner`) --------------------------

/// The switch is on for this process (set at thread start from the config).
static OWNER_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The builds and test runs this process is running, by the pid it started ([`register_group`]).
static OUR_ROOTS: std::sync::Mutex<BTreeSet<u32>> = std::sync::Mutex::new(BTreeSet::new());

/// `[byteask_cpp] tree_owner` (off by default): a build of ours waits while another
/// process builds in its tree (e.g. the agent's own `cmake --build`), the background warm build
/// stops when one starts, and a check cut by its time limit is run again rather than repeated.
pub(crate) fn set_owner_mode(on: bool) {
    OWNER_MODE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The switch, for a build in `tree` (tests turn it on per tree, see [`TEST_OWNER_TREES`]).
pub(crate) fn owner_mode(tree: &Path) -> bool {
    #[cfg(test)]
    {
        TEST_OWNER_TREES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|own| tree.starts_with(own))
    }
    #[cfg(not(test))]
    {
        let _ = tree;
        OWNER_MODE.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Tests: the trees in owner mode (the process-wide switch would leak into parallel tests).
#[cfg(test)]
pub(crate) static TEST_OWNER_TREES: std::sync::Mutex<Vec<PathBuf>> =
    std::sync::Mutex::new(Vec::new());

/// Marks a process we started (and everything below it) as ours while the guard lives.
pub(crate) struct OurGroup(u32);

pub(crate) fn register_group(pid: u32) -> OurGroup {
    OUR_ROOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(pid);
    OurGroup(pid)
}

impl Drop for OurGroup {
    fn drop(&mut self) {
        OUR_ROOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// `(state, parent pid)` of a process, from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> Option<(String, u32)> {
    // `pid (comm) state ppid ...`; comm may hold spaces and parentheses.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.to_string();
    let parent = fields.next()?.parse::<u32>().ok()?;
    Some((state, parent))
}

/// A process working in `dir` (its working directory is `dir` or below: a build tool, a
/// compiler, a test) that none of this process's builds started (itself or an ancestor
/// registered; ninja puts each command in a process group of its own, so groups do not tell):
/// its pid and command line.
pub(crate) fn foreign_process(dir: &Path) -> Option<(u32, String)> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::fs::canonicalize(dir).ok()?;
        let ours = OUR_ROOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let me = std::process::id();
        let started_by_us = |pid: u32| {
            let mut pid = pid;
            for _ in 0..64 {
                if ours.contains(&pid) {
                    return true;
                }
                match proc_state(pid) {
                    Some((_, parent)) if parent > 1 && parent != me => pid = parent,
                    _ => return false,
                }
            }
            false
        };
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
                continue;
            };
            if !cwd.starts_with(&dir) {
                continue;
            }
            if proc_state(pid).is_none_or(|(state, _)| state == "Z") || started_by_us(pid) {
                continue;
            }
            let cmd = std::fs::read(entry.path().join("cmdline"))
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .replace('\0', " ")
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            return Some((pid, cmd));
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        None
    }
}

/// Resolves once a [`foreign_process`] works in `dir` on two polls in a row ([`FOREIGN_POLL`]
/// apart): a passing `ls` does not count, nor a build of ours in the instant between its start
/// and [`register_group`].
pub(crate) async fn foreign_build(dir: &Path) -> (u32, String) {
    let mut last: Option<u32> = None;
    loop {
        match foreign_process(dir) {
            Some((pid, cmd)) if last == Some(pid) => return (pid, cmd),
            found => last = found.map(|(pid, _)| pid),
        }
        tokio::time::sleep(FOREIGN_POLL).await;
    }
}

pub(crate) const FOREIGN_POLL: Duration = Duration::from_secs(1);

/// A process's command line for an answer: at most 120 characters.
pub(crate) fn short_command(cmd: &str) -> String {
    match cmd.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &cmd[..cut]),
        None => cmd.to_string(),
    }
}

/// What to build for a set of tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TargetPlan {
    /// Exact model target names to build, in a stable order.
    pub targets: Vec<String>,
    /// Build everything: only when the build system gave no target map at all.
    pub build_all: bool,
    /// How unmapped tests were handled (each a sentence for the answer).
    pub notes: Vec<String>,
}

/// Script files read for program names in a test's command.
const SCRIPT_BYTES_CAP: u64 = 256 * 1024;

/// The targets `tests` (indices into `model.tests`) need. A test mapped to its program builds
/// that program; an unmapped one (a script, a shell command) builds the executables its command
/// or script names; one that names none builds every executable target, the smallest superset
/// that is still safe. Only a model without any target map builds everything.
pub(crate) fn plan_targets(model: &Model, tests: &[usize]) -> TargetPlan {
    let mut plan = TargetPlan::default();
    if model.targets.is_empty() {
        plan.build_all = true;
        plan.notes.push(
            "fallback: the build system gave no target map, so everything is built".to_string(),
        );
        return plan;
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut push = |plan: &mut TargetPlan, name: &str| {
        if seen.insert(name.to_string()) {
            plan.targets.push(name.to_string());
        }
    };
    let mut unknown: Vec<String> = Vec::new();
    for &i in tests {
        let test = &model.tests[i];
        if let Some(target) = &test.target {
            push(&mut plan, target);
            continue;
        }
        let named = programs_named_in(model, test);
        if named.is_empty() {
            unknown.push(test.name.clone());
            continue;
        }
        plan.notes.push(format!(
            "{} runs no program of its own; built {} (named in its command)",
            test.name,
            named.join(", ")
        ));
        for name in &named {
            push(&mut plan, name);
        }
    }
    if !unknown.is_empty() {
        let exes: Vec<String> = model
            .targets
            .iter()
            .filter(|t| t.is_exe)
            .map(|t| t.name.clone())
            .collect();
        for name in &exes {
            push(&mut plan, name);
        }
        plan.notes.push(format!(
            "fallback: the build model does not say which program {} run{}, so every executable target ({}) is built",
            list(&unknown, 4),
            if unknown.len() == 1 { "s" } else { "" },
            exes.len()
        ));
    }
    plan
}

/// Executable targets whose name or program file name appears as a word in `test`'s command
/// or in a small script file of the project that the command runs.
fn programs_named_in(model: &Model, test: &TestEntry) -> Vec<String> {
    let mut words: BTreeSet<String> = BTreeSet::new();
    let mut add_words = |text: &str| {
        for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))) {
            if !word.is_empty() {
                words.insert(word.to_string());
            }
        }
    };
    for arg in &test.command {
        add_words(arg);
        let path = super::normalize(test.workdir.as_deref().unwrap_or(&model.root), Path::new(arg));
        if path.starts_with(&model.root)
            && !path.starts_with(&model.build_dir)
            && std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= SCRIPT_BYTES_CAP)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            add_words(&text);
        }
    }
    model
        .targets
        .iter()
        .filter(|t| t.is_exe)
        .filter(|t| {
            words.contains(&t.name)
                || t.artifacts.iter().any(|a| {
                    a.file_name()
                        .is_some_and(|f| words.contains(f.to_string_lossy().as_ref()))
                })
        })
        .map(|t| t.name.clone())
        .collect()
}

/// The exact commands that run `items`, one line each (`ctest -R '^(a|b)$'`, a program with
/// its `--gtest_filter=`, `make check`), for the answer.
pub(crate) fn run_lines(model: &Model, items: &[RunItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            RunItem::Ctest { names, .. } if names.is_empty() => "ctest (every registered test)".to_string(),
            RunItem::Ctest { names, .. } => {
                let pattern = format!(
                    "^({})$",
                    names.iter().map(|n| regex_escape(n)).collect::<Vec<_>>().join("|")
                );
                format!("ctest -R {}", clip(&shell_quote(&pattern), 600))
            }
            RunItem::Direct { argv, framework, cases, .. } => {
                let argv = filtered_argv(argv, *framework, cases);
                let shown: Vec<String> = argv
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let a = if i == 0 { model.rel(Path::new(a)).to_string() } else { a.clone() };
                        shell_quote(&a)
                    })
                    .collect();
                clip(&shown.join(" "), 600)
            }
            RunItem::MakeCheck { target, .. } => format!("make {target}"),
        })
        .collect()
}

/// POSIX shell quoting for display: bare when safe, else single quotes.
pub(crate) fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,+@%".contains(c))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `a, b, c (+N)`.
pub(crate) fn list(items: &[String], n: usize) -> String {
    let mut s = items.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > n {
        s.push_str(&format!(" (+{})", items.len() - n));
    }
    s
}

/// Problems shown from `git diff --check`.
const DIFF_CHECK_SHOWN: usize = 6;

/// `git diff --check <base>` over the work tree (tracked files): one line, `clean` or the
/// first problems git reports (whitespace errors, leftover conflict markers).
pub(crate) async fn diff_check(root: &Path, base: &str) -> String {
    let short = &base[..base.len().min(10)];
    let Some((_, out)) = crate::gitx::git_status_out(root, &["diff", "--check", base, "--"]).await else {
        return format!("git diff --check {short}: could not run");
    };
    let problems: Vec<String> = out
        .lines()
        .filter(|l| !l.starts_with('+') && !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    if problems.is_empty() {
        return format!("git diff --check {short}: clean");
    }
    format!(
        "git diff --check {short}: {} problem(s): {}",
        problems.len(),
        list(&problems, DIFF_CHECK_SHOWN)
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::testsel::DirKind;

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_working_in_the_tree_is_foreign_unless_ours() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("build");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        assert_eq!(foreign_process(&tree), None);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(tree.join("sub"))
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let found = foreign_process(&tree);
        assert_eq!(found.as_ref().map(|(p, _)| *p), Some(pid));
        assert!(found.is_some_and(|(_, cmd)| cmd == "sleep 30"));
        let ours = register_group(pid);
        assert_eq!(foreign_process(&tree), None);
        drop(ours);
        assert_eq!(foreign_process(&tree).map(|(p, _)| p), Some(pid));
        assert_eq!(
            foreign_process(dir.path().join("elsewhere").as_path()),
            None
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(foreign_process(&tree), None);
    }
    use crate::testsel::System;
    use crate::testsel::Target;

    fn model(root: &Path) -> Model {
        let exe = |name: &str| Target {
            name: name.to_string(),
            is_exe: true,
            artifacts: vec![root.join("build").join(name)],
            ..Target::default()
        };
        let test = |name: &str, command: &[&str], target: Option<&str>| TestEntry {
            name: name.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            target: target.map(str::to_string),
            ..TestEntry::default()
        };
        Model {
            system: System::CMake,
            root: root.to_path_buf(),
            build_dir: root.join("build"),
            dir_kind: DirKind::Scratch,
            version: Some((3, 28)),
            ninja: true,
            targets: vec![
                exe("a_test"),
                exe("b_test"),
                exe("tool"),
                Target {
                    name: "core".to_string(),
                    ..Target::default()
                },
            ],
            tests: vec![
                test("a", &["/x/build/a_test"], Some("a_test")),
                test("b", &["/x/build/b_test"], Some("b_test")),
                test("uses_tool", &["sh", "-c", "./tool --selftest"], None),
                test("script", &["python3", "check.py"], None),
            ],
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        }
    }

    #[test]
    fn mapped_tests_build_only_their_programs() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_targets(&model(dir.path()), &[0]);
        assert_eq!(plan.targets, vec!["a_test".to_string()]);
        assert!(!plan.build_all && plan.notes.is_empty());
    }

    #[test]
    fn an_unmapped_test_builds_the_programs_it_names_or_every_executable() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(dir.path());
        let plan = plan_targets(&m, &[0, 2]);
        assert_eq!(plan.targets, vec!["a_test".to_string(), "tool".to_string()]);
        assert!(plan.notes[0].contains("uses_tool runs no program of its own; built tool"), "{plan:?}");
        // A script whose text names a program.
        std::fs::write(dir.path().join("check.py"), "subprocess.run(['build/b_test'])\n").unwrap();
        let mut m2 = m.clone();
        m2.tests[3].workdir = Some(dir.path().to_path_buf());
        assert_eq!(plan_targets(&m2, &[3]).targets, vec!["b_test".to_string()]);
        // Nothing named: the smallest safe superset, every executable, and it says so.
        std::fs::write(dir.path().join("check.py"), "print('ok')\n").unwrap();
        let plan = plan_targets(&m2, &[3]);
        assert_eq!(plan.targets, vec!["a_test".to_string(), "b_test".to_string(), "tool".to_string()]);
        assert!(!plan.build_all);
        assert!(plan.notes[0].starts_with("fallback:"), "{plan:?}");
        // No target map at all: everything, stated.
        let mut bare = m;
        bare.targets.clear();
        let plan = plan_targets(&bare, &[0]);
        assert!(plan.build_all && plan.notes[0].contains("no target map"), "{plan:?}");
    }

    #[test]
    fn run_lines_state_the_exact_filter() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(dir.path());
        let lines = run_lines(
            &m,
            &[
                RunItem::Ctest { dir: m.build_dir.clone(), names: vec!["a".to_string(), "b.c".to_string()] },
                RunItem::Direct {
                    test: "a".to_string(),
                    argv: vec![dir.path().join("build/a_test").display().to_string()],
                    workdir: m.build_dir.clone(),
                    env: Vec::new(),
                    framework: Some(crate::testsel::symbols::Framework::GTest),
                    cases: vec!["S.One".to_string()],
                },
            ],
        );
        assert_eq!(lines, vec!["ctest -R '^(a|b\\.c)$'".to_string(), "build/a_test --gtest_filter=S.One".to_string()]);
    }
}
