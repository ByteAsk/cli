use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use check_result::Verdict;
use codex_extension_api::ConversationHistory;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::SessionSource;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;

use super::*;
use crate::store::CloseHow;
use crate::store::Record;
use crate::test_support::T;
use crate::test_support::recorder;
use crate::test_support::scripted_session;
use crate::test_support::store_root;
use crate::workspace::tests::repo_with_upstream;

/// The kinds of record in each change, in order, keyed by the change's repo
/// directory name and its position among that repo's changes.
fn kinds(root: &Path) -> Vec<(String, Vec<&'static str>)> {
    let mut out = Vec::new();
    for change in store::load(root).changes {
        let repo = change
            .opened()
            .map(|(_, repo, ..)| repo.to_string())
            .unwrap_or_default();
        let kinds: Vec<&'static str> = change
            .records
            .iter()
            .map(|record| match record {
                Record::Open { .. } => "open",
                Record::Base { .. } => "base",
                Record::Edit { .. } => "edit",
                Record::Result { result, .. } => match result.verdict {
                    Verdict::Pass => "pass",
                    Verdict::Fail => "fail",
                    Verdict::NotRun => "not_run",
                    Verdict::NoClaim => "no_claim",
                    Verdict::Approximate => "approximate",
                },
                Record::Close { how, .. } => match how {
                    CloseHow::Pushed => "close:pushed",
                    CloseHow::SessionEnded => "close:session_ended",
                    CloseHow::Idle => "close:idle",
                },
                Record::PayloadEvicted { .. } => "payload_evicted",
                Record::Capped { .. } => "capped",
            })
            .collect();
        out.push((repo, kinds));
    }
    out.sort_by_key(|(_, kinds)| kinds.len());
    out
}

#[test]
fn a_scripted_session_is_recorded_by_change_and_a_push_ends_the_change() {
    let scripted = scripted_session();
    let root = store_root(&scripted);
    let changes = store::load(&root).changes;
    assert_eq!(changes.len(), 4, "A before the push, A after it, B, C");
    let mut by_kinds: Vec<Vec<&str>> = kinds(&root).into_iter().map(|(_, kinds)| kinds).collect();
    by_kinds.sort();
    let mut expected = vec![
        // A, until the push: the first check after the push closes it.
        vec![
            "open",
            "pass",
            "fail",
            "edit",
            "fail",
            "pass",
            "edit",
            "pass",
            "pass",
            "close:pushed",
        ],
        // A, after the push: a new change with no edit, starting from the pushed code.
        vec!["open", "base", "pass"],
        // B.
        vec![
            "open", "edit", "not_run", "fail", "edit", "pass", "edit", "fail", "pass",
        ],
        // C.
        vec!["open", "pass"],
    ];
    expected.sort();
    assert_eq!(by_kinds, expected);

    // Every result in a git repository carries a version; C's does not.
    for change in &changes {
        let (_, repo, branch, ..) = change.opened().unwrap();
        for record in &change.records {
            if let Record::Result {
                version, payload, ..
            } = record
            {
                assert_eq!(version.is_some(), !branch.is_empty(), "{repo}");
                assert!(payload.is_some(), "the answer is kept as a payload");
            }
        }
    }
}

#[test]
fn the_versions_follow_the_edits() {
    let scripted = scripted_session();
    let root = store_root(&scripted);
    let a = store::load(&root)
        .changes
        .into_iter()
        .find(|change| change.records.len() == 10)
        .expect("change A");
    let versions: Vec<Option<String>> = a
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Result { version, .. } => Some(version.clone()),
            _ => None,
        })
        .collect();
    // base, base, after edit 1 (x2), after edit 2 (x2)
    assert_eq!(versions[0], versions[1]);
    assert_eq!(versions[2], versions[3]);
    assert_eq!(versions[4], versions[5]);
    assert_ne!(versions[1], versions[2]);
    assert_ne!(versions[3], versions[4]);
}

#[test]
fn a_change_idle_for_thirty_days_is_closed_and_the_next_event_opens_a_new_one() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, work) = repo_with_upstream();
    let rec = recorder(home.path(), &work);
    let probe = rec.probe();
    rec.record_result(T, &probe, &CheckResult::pass("cpp_check", "ok"), None);
    rec.record_result(
        T + store::IDLE_CLOSE_SECS + 1,
        &probe,
        &CheckResult::pass("cpp_check", "ok"),
        None,
    );
    let kinds: Vec<Vec<&str>> = kinds(&store::root(home.path()))
        .into_iter()
        .map(|(_, kinds)| kinds)
        .collect();
    assert_eq!(
        kinds,
        vec![vec!["open", "pass"], vec!["open", "pass", "close:idle"]]
    );
}

#[test]
fn ending_the_session_closes_its_open_changes() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, work) = repo_with_upstream();
    let rec = recorder(home.path(), &work);
    let probe = rec.probe();
    rec.record_edit(T, &probe);
    rec.end_session(T + 9);
    let change = &store::load(&store::root(home.path())).changes[0];
    assert_eq!(change.closed(), Some((T + 9, CloseHow::SessionEnded)));
}

#[test]
fn a_change_stops_recording_at_its_result_cap_and_says_so_once() {
    let home = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let rec = recorder(home.path(), plain.path()).with_max_results(2);
    let probe = rec.probe();
    for at in 0..5 {
        rec.record_result(
            T + at,
            &probe,
            &CheckResult::pass("cpp_check", "ok"),
            Some("x"),
        );
    }
    let kinds: Vec<Vec<&str>> = kinds(&store::root(home.path()))
        .into_iter()
        .map(|(_, kinds)| kinds)
        .collect();
    assert_eq!(kinds, vec![vec!["open", "pass", "pass", "capped"]]);
}

#[test]
fn only_a_successful_apply_patch_is_an_ai_edit() {
    let edit = ToolName::plain(EDIT_TOOL);
    assert!(is_ai_edit(
        &edit,
        ToolCallOutcome::Completed { success: true }
    ));
    assert!(!is_ai_edit(
        &edit,
        ToolCallOutcome::Completed { success: false }
    ));
    assert!(!is_ai_edit(&edit, ToolCallOutcome::Blocked));
    assert!(!is_ai_edit(&edit, ToolCallOutcome::Aborted));
    assert!(!is_ai_edit(
        &edit,
        ToolCallOutcome::Failed {
            handler_executed: true
        }
    ));
    assert!(!is_ai_edit(
        &ToolName::plain("cpp_format"),
        ToolCallOutcome::Completed { success: true }
    ));
    assert!(!is_ai_edit(
        &ToolName::new(Some("some_mcp_server".to_string()), EDIT_TOOL),
        ToolCallOutcome::Completed { success: true }
    )); // Core reports its own tools in the default namespace `functions`
    // (without this case, no CLI edit was ever recorded).
    assert!(is_ai_edit(
        &ToolName::namespaced("functions", EDIT_TOOL),
        ToolCallOutcome::Completed { success: true }
    ));
}

/// A tool that answers with a fixed text (or error) and counts its calls.
struct FakeTool {
    answer: Result<String, String>,
    calls: AtomicUsize,
}

struct TextOutput(String);

impl ToolOutput for TextOutput {
    fn log_output(&self) -> String {
        String::new()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: self.0.clone(),
                },
            ]),
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for FakeTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("cpp_check")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "cpp_check".to_string(),
            description: "Run cppcheck.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: codex_extension_api::parse_tool_input_schema(
                &serde_json::json!({"type": "object", "properties": {}}),
            )
            .unwrap(),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, _call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer {
                Ok(text) => Ok(Box::new(TextOutput(text)) as Box<dyn ToolOutput>),
                Err(message) => Err(FunctionCallError::RespondToModel(message)),
            }
        })
    }
}

fn call() -> ToolCall<'static> {
    ToolCall {
        turn_id: "turn".to_string(),
        call_id: "call-1".to_string(),
        tool_name: ToolName::plain("cpp_check"),
        model: String::new(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(1 << 20),
        source: ToolCallSource::Direct,
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: Arc::new(NoopTurnItemEmitter),
        environments: Vec::new(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}

fn recorded_results(home: &Path) -> Vec<(CheckResult, Option<String>)> {
    store::load(&store::root(home))
        .changes
        .into_iter()
        .flat_map(|change| {
            let stem = change.stem.clone();
            change
                .records
                .into_iter()
                .filter_map(move |record| match record {
                    Record::Result {
                        result, payload, ..
                    } => Some((
                        result,
                        payload.map(|name| {
                            std::fs::read_to_string(
                                store::root(home).join("payloads").join(&stem).join(name),
                            )
                            .unwrap()
                        }),
                    )),
                    _ => None,
                })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wrapper_records_the_result_line_of_answers_and_errors_and_changes_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, work) = repo_with_upstream();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(recorder(home.path(), &work));

    let pass_text = "🔧 cpp_check · book.cpp\nno findings\n\nResult: PASS: No findings.";
    let inner: SharedTool = Arc::new(FakeTool {
        answer: Ok(pass_text.to_string()),
        calls: AtomicUsize::new(0),
    });
    let wrapped = Recorded::wrap(Arc::clone(&inner), &thread_store);
    assert_eq!(wrapped.spec(), inner.spec(), "the model sees the same tool");
    assert_eq!(wrapped.tool_name(), inner.tool_name());
    let output = wrapped.handle(call()).await.expect("an answer");
    assert_eq!(
        response_text(output.to_response_item("call-1", &call().payload)),
        pass_text,
        "the answer is passed through unchanged"
    );

    let error_text = "cppcheck is not installed.\n\nResult: NOT RUN (tool not installed): Nothing was checked. Fix: install cppcheck.";
    let failing: SharedTool = Arc::new(FakeTool {
        answer: Err(error_text.to_string()),
        calls: AtomicUsize::new(0),
    });
    let err = Recorded::wrap(failing, &thread_store)
        .handle(call())
        .await
        .err()
        .expect("the error is passed through");
    assert!(matches!(err, FunctionCallError::RespondToModel(ref message) if message == error_text));

    // An answer with no result line is not a result: nothing is recorded.
    let silent: SharedTool = Arc::new(FakeTool {
        answer: Ok("no result line here".to_string()),
        calls: AtomicUsize::new(0),
    });
    Recorded::wrap(silent, &thread_store)
        .handle(call())
        .await
        .unwrap();

    let recorded = recorded_results(home.path());
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(recorded[0].0.verdict, Verdict::Pass);
    assert_eq!(recorded[0].1.as_deref(), Some(pass_text));
    assert_eq!(recorded[1].0.verdict, Verdict::NotRun);
    assert_eq!(
        recorded[1].0.reason,
        Some(check_result::NotRunReason::ToolMissing)
    );
    assert_eq!(recorded[1].1.as_deref(), Some(error_text));
}

#[test]
fn a_thread_without_a_recorder_gets_its_tools_unwrapped() {
    let inner: SharedTool = Arc::new(FakeTool {
        answer: Ok(String::new()),
        calls: AtomicUsize::new(0),
    });
    let wrapped = Recorded::wrap(Arc::clone(&inner), &ExtensionData::new("thread"));
    assert!(Arc::ptr_eq(&inner, &wrapped));
}

/// What `install` registers: a recorder per thread when `settings` says so,
/// AI edits from the edit tool, and a closed change when the thread stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_installed_contributors_record_edits_and_end_the_session() {
    struct Cfg {
        home: PathBuf,
        cwd: PathBuf,
        records: bool,
    }
    fn settings(config: &Cfg) -> Option<Settings> {
        config.records.then(|| Settings {
            home: config.home.clone(),
            cwd: config.cwd.clone(),
        })
    }
    let home = tempfile::tempdir().unwrap();
    let (_dir, work) = repo_with_upstream();
    let mut builder = ExtensionRegistryBuilder::<Cfg>::new();
    install(&mut builder, settings);
    let registry = builder.build();
    async fn start(
        registry: &codex_extension_api::ExtensionRegistry<Cfg>,
        config: Cfg,
        thread_store: &ExtensionData,
    ) {
        let session_store = ExtensionData::new("session");
        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &SessionSource::Mcp,
                    persistent_thread_state_available: false,
                    environments: &[],
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store,
                })
                .await;
        }
    }

    let skipped = ExtensionData::new("gated");
    start(
        &registry,
        Cfg {
            home: home.path().to_path_buf(),
            cwd: work.clone(),
            records: false,
        },
        &skipped,
    )
    .await;
    assert!(skipped.get::<ThreadRecorder>().is_none());

    let thread_store = ExtensionData::new("thread");
    start(
        &registry,
        Cfg {
            home: home.path().to_path_buf(),
            cwd: work.clone(),
            records: true,
        },
        &thread_store,
    )
    .await;
    assert!(thread_store.get::<ThreadRecorder>().is_some());

    let session_store = ExtensionData::new("session");
    let turn_store = ExtensionData::new("turn");
    for (name, outcome) in [
        ("cpp_check", ToolCallOutcome::Completed { success: true }),
        (EDIT_TOOL, ToolCallOutcome::Completed { success: false }),
        (EDIT_TOOL, ToolCallOutcome::Completed { success: true }),
    ] {
        for contributor in registry.tool_lifecycle_contributors() {
            contributor
                .on_tool_finish(ToolFinishInput {
                    session_store: &session_store,
                    thread_store: &thread_store,
                    turn_store: &turn_store,
                    turn_id: "turn",
                    call_id: "call",
                    tool_name: &ToolName::plain(name),
                    source: ToolCallSource::Direct,
                    outcome,
                })
                .await;
        }
    }
    for contributor in registry.thread_lifecycle_contributors() {
        contributor
            .on_thread_stop(ThreadStopInput {
                session_store: &session_store,
                thread_store: &thread_store,
            })
            .await;
    }
    let kinds: Vec<Vec<&str>> = kinds(&store::root(home.path()))
        .into_iter()
        .map(|(_, kinds)| kinds)
        .collect();
    assert_eq!(kinds, vec![vec!["open", "edit", "close:session_ended"]]);
}

/// An engine thread records under its thread id; the toolbox, serving another agent, under
/// that agent's own session id (so its hooks' edits and verdicts join the same change),
/// else a random id of its own.
#[test]
fn the_session_is_the_thread_or_the_serving_agent_s_session() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    assert_eq!(
        session_for(
            &SessionSource::Exec,
            "thread-9",
            env(&[("CLAUDE_CODE_SESSION_ID", "c-1")])
        ),
        "thread-9"
    );
    assert_eq!(
        session_for(
            &SessionSource::Mcp,
            "toolbox-thread",
            env(&[("CLAUDE_CODE_SESSION_ID", "c-1")])
        ),
        "c-1"
    );
    assert_eq!(
        session_for(
            &SessionSource::Mcp,
            "toolbox-thread",
            env(&[("CODEX_THREAD_ID", "x-2")])
        ),
        "x-2"
    );
    let own = session_for(
        &SessionSource::Mcp,
        "toolbox-thread",
        env(&[("CLAUDE_CODE_SESSION_ID", " ")]),
    );
    assert!(own.starts_with("toolbox-") && own.len() > 20, "{own}");
    assert_ne!(
        own,
        session_for(&SessionSource::Mcp, "toolbox-thread", env(&[])),
        "two toolbox processes never share a session by accident"
    );
}
