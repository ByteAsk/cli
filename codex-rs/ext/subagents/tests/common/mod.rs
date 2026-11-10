//! Shared fixtures: a tiny shell-script "project" in a temp git repo, and a stub child host
//! whose scripted children edit their worktrees and end with a report block.
#![allow(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_subagents::host::BoxFut;
use codex_subagents::host::ChildHandle;
use codex_subagents::host::ChildHost;
use codex_subagents::host::ChildSpec;
use codex_subagents::host::ChildState;
use codex_subagents::host::Role;

pub const CALC: &str = "#!/bin/sh\n# usage: calc.sh add A B | mul A B\ncase \"$1\" in\n  add) echo $(( $2 - $3 ));;\n  mul) echo $(( $2 * $3 ));;\nesac\n";
pub const BUILD: &str = "#!/bin/sh\n[ -f BROKEN ] && { echo 'error: broken build' >&2; exit 1; }\nsh -n calc.sh || exit 1\nmkdir -p out && cp calc.sh out/calc.sh\n";
pub const SUITE: &str = "#!/bin/sh\nr=$(sh out/calc.sh mul 2 3)\nif [ \"$r\" = 6 ]; then echo 'ok 1 - mul'; else echo 'not ok 1 - mul'; exit 1; fi\n";
pub const REPRO_TEST: &str = "#!/bin/sh\nr=$(sh out/calc.sh add 2 3)\nif [ \"$r\" = 5 ]; then echo 'ok 1 - add'; else echo 'not ok 1 - add'; exit 1; fi\n";

pub const FIX_GOOD: &str = "echo $(( $2 + $3 ))";
pub const FIX_SWAPPED: &str = "echo $(( $3 + $2 ))";
pub const FIX_WRONG: &str = "echo $(( $2 * $3 ))";
pub const BUG_LINE: &str = "echo $(( $2 - $3 ))";

pub const TASK: &str = "The calculator's add command returns the wrong result: `calc.sh add 2 3` prints -1 but it \
should print 5. Please fix the add command so that it returns the sum of its two arguments, and make sure \
the existing multiplication behaviour does not change. This is a bug report from a user of the tool.";

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(p, text).expect("write");
}

pub fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_default()
}

/// Replace the buggy add line in `dir/calc.sh`.
pub fn fix(dir: &Path, line: &str) {
    let text = read(dir, "calc.sh").replace(BUG_LINE, line);
    write(dir, "calc.sh", &text);
}

/// A committed repo with the calculator, its build script and the existing test suite.
pub fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&repo, &["init", "-q"]);
    write(&repo, "calc.sh", CALC);
    write(&repo, "build.sh", BUILD);
    write(&repo, "tests/run.sh", SUITE);
    write(&repo, ".gitignore", "*.log\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// What a scripted child does when it starts; returns its final message.
pub type Behave = Box<dyn Fn(&ChildSpec) -> Option<String> + Send + Sync>;

pub struct FakeHost {
    pub behave: Behave,
    pub root_messages: Mutex<Vec<String>>,
    pub spawned: Mutex<Vec<(Role, PathBuf, bool)>>,
    pub injected: Arc<Mutex<Vec<(Role, String)>>>,
    pub delay: Duration,
}

impl FakeHost {
    pub fn new(behave: Behave) -> Self {
        Self {
            behave,
            root_messages: Mutex::new(Vec::new()),
            spawned: Mutex::new(Vec::new()),
            injected: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::from_millis(150),
        }
    }

    pub fn roles(&self) -> Vec<Role> {
        self.spawned.lock().unwrap().iter().map(|(r, _, _)| *r).collect()
    }
}

struct FakeChild {
    id: String,
    role: Role,
    interrupted: std::sync::atomic::AtomicBool,
    ready_at: Instant,
    message: Option<String>,
    injected: Arc<Mutex<Vec<(Role, String)>>>,
}

impl ChildHandle for FakeChild {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn state(&self) -> BoxFut<'_, ChildState> {
        Box::pin(async move {
            if self.interrupted.load(std::sync::atomic::Ordering::SeqCst) {
                ChildState::Interrupted
            } else if Instant::now() >= self.ready_at {
                ChildState::Done(self.message.clone())
            } else {
                ChildState::Running
            }
        })
    }
    fn interrupt(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if Instant::now() < self.ready_at {
                self.interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
    }
    fn inject(&self, text: String) -> BoxFut<'_, bool> {
        Box::pin(async move {
            self.injected.lock().unwrap().push((self.role, text));
            Instant::now() < self.ready_at
        })
    }
    fn shutdown(&self) -> BoxFut<'_, ()> {
        Box::pin(async {})
    }
}

impl ChildHost for FakeHost {
    fn spawn(&self, spec: ChildSpec) -> BoxFut<'_, Result<Arc<dyn ChildHandle>, String>> {
        Box::pin(async move {
            let message = (self.behave)(&spec);
            let n = {
                let mut spawned = self.spawned.lock().unwrap();
                spawned.push((spec.role, spec.cwd.clone(), spec.high_effort));
                spawned.len()
            };
            Ok(Arc::new(FakeChild {
                id: format!("child-{n}"),
                role: spec.role,
                interrupted: std::sync::atomic::AtomicBool::new(false),
                ready_at: Instant::now() + self.delay,
                message,
                injected: self.injected.clone(),
            }) as Arc<dyn ChildHandle>)
        })
    }

    fn notify_root(&self, text: String) -> BoxFut<'_, bool> {
        Box::pin(async move {
            self.root_messages.lock().unwrap().push(text);
            true
        })
    }
}

/// The scripted independent test writer: adds a failing test and saves it as a patch.
pub fn repro_writer(spec: &ChildSpec) -> Option<String> {
    write(&spec.cwd, "tests/repro_add.sh", REPRO_TEST);
    let patch_dir = spec.cwd.parent().expect("scratch").join("repro");
    std::fs::create_dir_all(&patch_dir).expect("mkdir");
    git(&spec.cwd, &["add", "-N", "tests/repro_add.sh"]);
    let diff = git(&spec.cwd, &["diff", "--binary"]);
    std::fs::write(patch_dir.join("tests.patch"), diff).expect("write patch");
    Some(
        "Wrote one test.\nBYTEASK_REPORT {\"build\": \"sh build.sh\", \"run\": \"sh tests/repro_add.sh\", \
\"test\": \"sh tests/run.sh\", \"tests\": [{\"name\": \"add\", \"fails_on_base_because\": \"assertion\"}]}"
            .to_string(),
    )
}

pub fn settings(scratch: &Path) -> codex_subagents::settings::Settings {
    codex_subagents::settings::Settings {
        repro_tests: true,
        scratch_root: Some(scratch.to_path_buf()),
        build_timeout_secs: 60,
        test_timeout_secs: 60,
        ..codex_subagents::settings::Settings::default()
    }
}

/// Registered worktrees other than the main one.
pub fn extra_worktrees(repo: &Path) -> Vec<String> {
    git(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(str::to_string)
        .skip(1)
        .collect()
}

pub fn byteask_refs(repo: &Path) -> String {
    git(repo, &["for-each-ref", "refs/byteask"])
}
