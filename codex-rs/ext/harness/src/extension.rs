//! Registration: thread start (settings), turn input (ledger capture), thread context (the
//! ledger after compaction), and the turn gate (time notes after a step, the ledger check at
//! finish). Root threads only: sub-agents and internal sessions finish into their parent.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_core::config::Config;
use codex_extension_api::ContentItemKind;
use codex_extension_api::ContextContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnGateContributor;
use codex_extension_api::TurnGateFragments;
use codex_extension_api::TurnGateInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_protocol::user_input::UserInput;

use crate::budget;
use crate::ledger;
use crate::ledger::ChangedFile;
use crate::ledger::Constraint;

/// How long one git query at finish may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A model-visible message from the harness (a separate developer message).
struct HarnessFragment {
    kind: &'static str,
    text: String,
}

impl ContextualUserFragment for HarnessFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(format!("byteask_harness.{}", self.kind))
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

fn fragment(kind: &'static str, text: String) -> Box<dyn ContextualUserFragment + Send> {
    Box::new(HarnessFragment { kind, text })
}

/// The wall-clock budget of a root thread.
struct Budget {
    started: Instant,
    total: Duration,
    given: AtomicU8,
}

/// The constraint ledger of a root thread.
struct Ledger {
    cwd: PathBuf,
    constraints: Mutex<Vec<Constraint>>,
    /// The block was shown to the model (so it belongs in the context after a compaction).
    shown: AtomicBool,
    /// The finish check ran (it runs once per thread).
    checked: AtomicBool,
    /// Files already changed when the thread started, with their line counts.
    baseline: tokio::sync::OnceCell<BTreeMap<String, (Option<u64>, Option<u64>)>>,
}

struct HarnessExtension;

impl ThreadLifecycleContributor<Config> for HarnessExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.session_source.is_non_root_agent() || input.config.ephemeral {
                return;
            }
            let config = input.config;
            if let Some(secs) =
                codex_core::byteask_harness::positive_int(config, "time_budget_secs")
            {
                input.thread_store.insert(Budget {
                    started: Instant::now(),
                    total: Duration::from_secs(secs),
                    given: AtomicU8::new(0),
                });
            }
            if codex_core::byteask_harness::flag(config, "constraint_ledger") {
                let seeded = codex_core::byteask_harness::strings(config, "constraints")
                    .map(|lines| ledger::from_config(&lines))
                    .unwrap_or_default();
                input.thread_store.insert(Ledger {
                    cwd: config.cwd.to_path_buf(),
                    constraints: Mutex::new(seeded),
                    shown: AtomicBool::new(false),
                    checked: AtomicBool::new(false),
                    baseline: tokio::sync::OnceCell::new(),
                });
                // What was already changed before the session is not the session's change.
                if let Some(state) = input.thread_store.get::<Ledger>() {
                    tokio::spawn(async move {
                        let files = changed_files(&state.cwd).await;
                        let _ = state.baseline.set(index(&files));
                    });
                }
            }
        })
    }
}

impl TurnInputContributor for HarnessExtension {
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        _extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(state) = thread_store.get::<Ledger>() else {
                return Vec::new();
            };
            let text = input
                .user_input
                .iter()
                .filter_map(|item| match item {
                    UserInput::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let block = {
                let mut constraints = lock(&state.constraints);
                let grew = ledger::merge(&mut constraints, ledger::extract(&text));
                let first = !state.shown.load(Ordering::Acquire);
                if constraints.is_empty() || !(grew || first) {
                    None
                } else {
                    Some(ledger::render_block(&constraints))
                }
            };
            match block {
                Some(block) => {
                    state.shown.store(true, Ordering::Release);
                    vec![fragment("constraint_ledger", block)]
                }
                None => Vec::new(),
            }
        })
    }
}

impl ContextContributor for HarnessExtension {
    /// After a compaction (or a resume) the initial context is rebuilt: the ledger goes in it.
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            let Some(state) = thread_store.get::<Ledger>() else {
                return Vec::new();
            };
            if !state.shown.load(Ordering::Acquire) {
                return Vec::new();
            }
            let constraints = lock(&state.constraints);
            if constraints.is_empty() {
                return Vec::new();
            }
            vec![PromptFragment::developer_policy(
                ledger::render_block(&constraints),
                ContentItemKind("byteask_harness.constraint_ledger".to_string()),
            )]
        })
    }
}

impl TurnGateContributor for HarnessExtension {
    fn after_step<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let Some(budget) = input.thread_store.get::<Budget>() else {
                return Vec::new();
            };
            let given = budget.given.load(Ordering::Acquire);
            match budget::due(budget.started.elapsed(), budget.total, given) {
                Some((level, text))
                    if budget
                        .given
                        .compare_exchange(given, level, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok() =>
                {
                    vec![fragment("time_budget", text)]
                }
                _ => Vec::new(),
            }
        })
    }

    fn before_stop<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let Some(state) = input.thread_store.get::<Ledger>() else {
                return Vec::new();
            };
            if state.checked.load(Ordering::Acquire) {
                return Vec::new();
            }
            let constraints = lock(&state.constraints).clone();
            if constraints.is_empty() {
                return Vec::new();
            }
            let baseline = state.baseline.get().cloned().unwrap_or_default();
            let changed: Vec<ChangedFile> = changed_files(&state.cwd)
                .await
                .into_iter()
                .filter(|file| baseline.get(&file.path) != Some(&(file.added, file.removed)))
                .collect();
            match ledger::finish_check(&constraints, &changed) {
                Some(text) if !state.checked.swap(true, Ordering::AcqRel) => {
                    vec![fragment("constraint_check", text)]
                }
                _ => Vec::new(),
            }
        })
    }
}

fn index(files: &[ChangedFile]) -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    files
        .iter()
        .map(|file| (file.path.clone(), (file.added, file.removed)))
        .collect()
}

/// Tracked changes against HEAD plus untracked files, relative to the repository root.
/// Empty outside a git repository or when git does not answer in time.
async fn changed_files(cwd: &Path) -> Vec<ChangedFile> {
    let Some(root) = git(cwd, &["rev-parse", "--show-toplevel"]).await else {
        return Vec::new();
    };
    let root = PathBuf::from(root.trim());
    let mut files = git(&root, &["diff", "--numstat", "HEAD", "--"])
        .await
        .map(|out| ledger::parse_numstat(&out))
        .unwrap_or_default();
    if let Some(untracked) = git(&root, &["ls-files", "--others", "--exclude-standard"]).await {
        for path in untracked.lines().map(str::trim).filter(|p| !p.is_empty()) {
            let lines = std::fs::read_to_string(root.join(path))
                .ok()
                .map(|text| text.lines().count() as u64);
            files.push(ChangedFile {
                path: path.to_string(),
                added: lines,
                removed: lines.map(|_| 0),
            });
        }
    }
    files
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Register the harness controls with the host registry. Every control is off unless its
/// `[byteask_harness]` key is set (see `codex_core::byteask_harness`).
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(HarnessExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.turn_input_contributor(extension.clone());
    builder.prompt_contributor(extension.clone());
    builder.turn_gate_contributor(extension);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_extension_api::ConversationHistorySnapshot;
    use codex_extension_api::ResponseItem;
    use pretty_assertions::assert_eq;

    struct NoHistory;

    impl ConversationHistorySnapshot for NoHistory {
        fn history_version(&self) -> u64 {
            0
        }

        fn user_message_revision(&self) -> u64 {
            0
        }

        fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
            Box::new(std::iter::empty())
        }
    }

    fn gate_input<'a>(stores: &'a [ExtensionData; 3]) -> TurnGateInput<'a> {
        TurnGateInput {
            turn_id: "turn",
            last_agent_message: Some("done"),
            conversation_history: Arc::new(NoHistory),
            session_store: &stores[0],
            thread_store: &stores[1],
            turn_store: &stores[2],
        }
    }

    fn stores() -> [ExtensionData; 3] {
        [
            ExtensionData::new("session"),
            ExtensionData::new("thread"),
            ExtensionData::new("turn"),
        ]
    }

    fn text_of(fragments: &[Box<dyn ContextualUserFragment + Send>]) -> String {
        fragments
            .iter()
            .map(|f| f.body())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sh(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        sh(dir.path(), &["init", "-q"]);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/a.cpp"), "int a;\n").unwrap();
        std::fs::write(dir.path().join("tests/t.cpp"), "int t;\n").unwrap();
        sh(dir.path(), &["add", "-A"]);
        sh(
            dir.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        dir
    }

    fn ledger_state(cwd: &Path) -> Ledger {
        let state = Ledger {
            cwd: cwd.to_path_buf(),
            constraints: Mutex::new(Vec::new()),
            shown: AtomicBool::new(false),
            checked: AtomicBool::new(false),
            baseline: tokio::sync::OnceCell::new(),
        };
        let _ = state.baseline.set(BTreeMap::new());
        state
    }

    async fn submit(stores: &[ExtensionData; 3], text: &str) -> String {
        let input = TurnInputContext {
            turn_id: "turn".to_string(),
            user_input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            environments: Vec::new(),
        };
        let fragments = HarnessExtension
            .contribute(input, None, &stores[0], &stores[1], &stores[2])
            .await;
        text_of(&fragments)
    }

    #[tokio::test]
    async fn the_ledger_is_shown_kept_across_compaction_and_checked_once() {
        let repo = repo();
        let stores = stores();
        stores[1].insert(ledger_state(repo.path()));

        // Before the first turn the rebuilt context has no ledger.
        assert!(
            HarnessExtension
                .contribute_thread_context(&stores[0], &stores[1])
                .await
                .is_empty()
        );
        let shown = submit(&stores, "Fix a. Do not modify files under tests/.").await;
        assert!(
            shown.contains("1. Do not modify files under tests/."),
            "{shown}"
        );
        // The same message again adds nothing.
        assert_eq!(
            submit(&stores, "Do not modify files under tests/.").await,
            ""
        );
        // After a compaction the initial context carries it.
        let rebuilt = HarnessExtension
            .contribute_thread_context(&stores[0], &stores[1])
            .await;
        assert_eq!(rebuilt.len(), 1);
        assert!(rebuilt[0].text().contains("<constraint_ledger>"));

        // No change yet: nothing to check, and the check is still pending.
        assert!(
            HarnessExtension
                .before_stop(gate_input(&stores))
                .await
                .is_empty()
        );
        std::fs::write(repo.path().join("tests/t.cpp"), "int t2;\n").unwrap();
        std::fs::write(repo.path().join("src/new.cpp"), "int n;\n").unwrap();
        let check = text_of(&HarnessExtension.before_stop(gate_input(&stores)).await);
        assert!(check.contains("VIOLATED: tests/t.cpp changed"), "{check}");
        assert!(check.contains("src/new.cpp (+1 -0)"), "{check}");
        // once per thread
        assert!(
            HarnessExtension
                .before_stop(gate_input(&stores))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn files_changed_before_the_session_are_not_the_sessions_change() {
        let repo = repo();
        std::fs::write(repo.path().join("tests/t.cpp"), "int dirty;\n").unwrap();
        let stores = stores();
        let state = Ledger {
            cwd: repo.path().to_path_buf(),
            constraints: Mutex::new(ledger::extract("Do not modify files under tests/.")),
            shown: AtomicBool::new(true),
            checked: AtomicBool::new(false),
            baseline: tokio::sync::OnceCell::new(),
        };
        let _ = state.baseline.set(index(&changed_files(repo.path()).await));
        stores[1].insert(state);
        std::fs::write(repo.path().join("src/a.cpp"), "int a2;\n").unwrap();
        let check = text_of(&HarnessExtension.before_stop(gate_input(&stores)).await);
        assert!(check.contains("src/a.cpp"), "{check}");
        assert!(!check.contains("tests/t.cpp"), "{check}");
        assert!(
            check.contains("checked: no changed file falls under it"),
            "{check}"
        );
    }

    #[tokio::test]
    async fn time_notes_come_after_steps_once_per_threshold() {
        let stores = stores();
        stores[1].insert(Budget {
            started: Instant::now() - Duration::from_secs(31 * 60),
            total: Duration::from_secs(3600),
            given: AtomicU8::new(0),
        });
        let first = text_of(&HarnessExtension.after_step(gate_input(&stores)).await);
        assert!(first.starts_with("Time budget: half"), "{first}");
        assert!(
            HarnessExtension
                .after_step(gate_input(&stores))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn nothing_happens_without_the_settings() {
        let stores = stores();
        assert_eq!(
            submit(&stores, "Do not modify files under tests/.").await,
            ""
        );
        assert!(
            HarnessExtension
                .after_step(gate_input(&stores))
                .await
                .is_empty()
        );
        assert!(
            HarnessExtension
                .before_stop(gate_input(&stores))
                .await
                .is_empty()
        );
        assert!(
            HarnessExtension
                .contribute_thread_context(&stores[0], &stores[1])
                .await
                .is_empty()
        );
    }
}
