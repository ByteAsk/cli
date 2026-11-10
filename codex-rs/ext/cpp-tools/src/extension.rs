use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use codex_core::config::Config;
use codex_extension_api::ContentItemKind;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_features::Feature;
use codex_features::Features;
use codex_utils_redacted_string::RedactedString;
use codex_extension_api::ToolStartInput;
use codex_extension_api::TurnGateContributor;
use codex_extension_api::TurnGateFragments;
use codex_extension_api::TurnGateInput;
use codex_protocol::protocol::SessionSource;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;

use crate::detect;
use crate::envnote::EnvNote;
use crate::gdbmi::GdbRegistry;
use crate::gdbmi::remote::KernelRegistry;
use crate::guidance::CPP_GUIDANCE;
use crate::guidance::Enforcement;
use crate::guidance::cpp_guidance;
use crate::memsafety::FixVerifyTool;
use crate::session_head::SessionHead;
use crate::testsel::jobs::Jobs;
use crate::tools::AttachState;
use crate::tools::BenchTool;
use crate::tools::CheckStatusTool;
use crate::tools::BinaryTool;
use crate::tools::ClangTidyTool;
use crate::tools::ClangdNavTool;
use crate::tools::CompileDbTool;
use crate::tools::CppCheckTool;
use crate::tools::CppLayoutTool;
use crate::tools::DecompileTool;
use crate::tools::FinishCheckTool;
use crate::tools::FormatTool;
use crate::tools::GdbAttachTool;
use crate::tools::GdbSessionTool;
use crate::tools::GdbTool;
use crate::tools::GodboltTool;
use crate::tools::ImpactTool;
use crate::tools::InsightsTool;
use crate::tools::IntrinsicTool;
use crate::tools::KdbTool;
use crate::tools::PerfTool;
use crate::tools::RecordTool;
use crate::tools::RepoHistoryTool;
use crate::tools::SanitizeTool;
use crate::tools::SymbolizeTool;
use crate::workflow::Workflow;
use crate::tools::TestSelectTool;

/// Should this thread see none of our tools and none of our guidance?
///
/// Upstream starts *ephemeral structured threads* for internal work (auto-recap
/// via `tui/src/temporary_structured_request.rs`, guardian review, the memories
/// phase-2 agent). Those prompts carry untrusted transcript text, so upstream
/// disables every built-in and extension tool on them — each upstream extension
/// by its own `features.*` flag. Ours have no flag, so all 28 `cpp_*`/`*_ref`
/// tools reached those threads: tools that compile and run user code, plus
/// network calls to `mcp.byteask.ai` that are an exfiltration channel under
/// prompt injection. This is the containment boundary, not tidiness.
///
/// `ephemeral` alone is not enough: `byteask exec --ephemeral` is a real user
/// flag (`exec/src/cli.rs`) and must keep its tools. Shell alone is not enough
/// either: a user who merely turned the shell off would lose the read-only
/// formatters, navigation and all 9 documentation tools. Both together have no
/// false positive we could construct.
///
/// ⚠️ `shell_enabled` is `Feature::ShellTool` **alone**, deliberately NOT
/// `ShellTool || UnifiedExec`. `UnifiedExec` cannot be
/// switched off by ordinary config — `ManagedFeatures::normalize_candidate`
/// force-enables it unless an enterprise `requirements.toml` pins it (upstream's
/// own `legacy_unified_exec_disable_flags_do_not_disable_command_execution`
/// asserts exactly that), so `|| UnifiedExec` would be `|| true` and the gate
/// would never fire — including on the recap thread this exists for. `ShellTool`
/// is also the complete signal: both shell surfaces require it
/// (`core/src/tools/spec_plan.rs` `add_shell_tools`, and the `exec_command`
/// handler which needs `ShellTool && UnifiedExec`).
///
/// This function is duplicated verbatim in `codex-ref-tools` (a shared
/// crate for one boolean is heavier than the duplication). The two copies are
/// pinned to the same answer by `gate_decision_is_identical_in_both_crates`
/// below, which is why this crate dev-depends on `codex-ref-tools`.
pub fn tools_gated(ephemeral: bool, features: &Features) -> bool {
    let shell_enabled = features.enabled(Feature::ShellTool);
    ephemeral && !shell_enabled
}

/// [`tools_gated`] applied to the thread's resolved [`Config`].
fn tools_gated_for(config: &Config) -> bool {
    tools_gated(config.ephemeral, config.features.get())
}

/// The host joins the parts of one developer message with nothing in between, and this
/// guidance has no tags around it: each part ends with a blank line so the next one (the
/// environment note, the reference guidance) does not run into its last sentence.
pub(crate) fn paragraph(text: impl Into<String>) -> String {
    let mut text = text.into();
    if !text.ends_with('\n') {
        text.push_str("\n\n");
    }
    text
}

/// Stateless extension handle registered as every contributor flavor.
struct CppToolsExtension;

/// Marks a thread that runs the fast profile (`[byteask_harness] fast = true`), read by the tool,
/// guidance and workflow layers.
#[derive(Clone, Copy)]
struct FastProfile;

fn fast(thread_store: &ExtensionData) -> bool {
    thread_store.get::<FastProfile>().is_some()
}

/// Switch `[byteask_cpp] hide_missing_tools` (off by default): the tools left
/// out of this thread, probed once at thread start (see [`missing_tools`]).
struct HideMissing(Vec<&'static str>);

const HIDE_MISSING_KEY: &str = "hide_missing_tools";
/// Switch (off by default): see [`crate::testsel::tree::set_owner_mode`].
const TREE_OWNER_KEY: &str = "tree_owner";

/// The one program each of these tools cannot work without, looked up on `PATH` by that name.
const NEEDS_PROGRAM: &[(&str, &str)] = &[
    ("cpp_check", "cppcheck"),
    ("cpp_tidy", "clang-tidy"),
    ("cpp_format", "clang-format"),
    ("cpp_decompile", "r2"),
    ("debug_record", "rr"),
];

/// The tools whose program is missing (`have` answers for a program name), and `check_status`
/// in a non-interactive session: there no check runs before the final message (with the
/// automatic gate on it is never offered, see `tools`), so it can only say that none has run.
fn missing_tools(have: impl Fn(&str) -> bool, non_interactive: bool) -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = NEEDS_PROGRAM
        .iter()
        .filter(|(_, program)| !have(program))
        .map(|(tool, _)| *tool)
        .collect();
    if non_interactive {
        tools.push("check_status");
    }
    tools
}

/// Thread-scoped state seeded at thread start.
#[derive(Clone)]
struct CppToolsState {
    /// This thread is one of upstream's internal ephemeral structured threads:
    /// contribute no tools and no guidance. See [`tools_gated`].
    gated: bool,
    is_cpp: bool,
    cwd: PathBuf,
    /// Byteask provider base URL (the gateway), used by the godbolt tool.
    gateway_base: String,
    /// Per-user JWT for the gateway, if signed in (mode C).
    bearer: Option<RedactedString>,
    /// Live interactive GDB sessions the agent owns, scoped to this thread.
    gdb_sessions: Arc<GdbRegistry>,
    kernel_sessions: Arc<KernelRegistry>,
    /// Current pair-debugging attachment (debug_attach), scoped to this thread.
    gdb_attach: Arc<AttachState>,
    /// May the tools that compile and run code natively — i.e.
    /// OUTSIDE the filesystem sandbox — do so in this workspace? Resolved once
    /// here because `Config` is only in hand at thread start. See
    /// [`crate::trust`] for why it is trust OR an explicit env opt-in.
    native_exec: bool,
    /// The sandbox every program a tool starts runs in.
    sandbox: Arc<crate::sandbox::Settings>,
    /// Session-start environment facts shown after the guidance (C/C++
    /// workspaces only), gathered in the background from thread start.
    env_note: EnvNote,
    /// The session-start snapshot, and the change-impact checklist and finish gate when they are
    /// switched on by `[byteask_cpp]` / `BYTEASK_CPP_*` (C/C++ workspaces, root threads). See
    /// [`crate::workflow`].
    workflow: Option<Arc<Workflow>>,
    /// The commit that was HEAD when the thread started (history and base
    /// comparisons read it and its ancestors only).
    head: SessionHead,
    /// `cpp_test_select` runs of this thread (background runs and their notice).
    test_runs: Arc<Jobs>,
    /// `policy.toml` says local only, so the tools that send code or a
    /// question off this machine answer NOT RUN (offline) instead.
    local_only: bool,
    /// When the thread started (unix seconds): `check_status` marks results older than this as
    /// from an earlier session.
    started_unix: u64,
    /// The session runs with full permission (approval `never` and full access, see
    /// [`crate::trust::full_access`]): nothing may stop to ask the user.
    full_access: bool,
}

impl ThreadLifecycleContributor<Config> for CppToolsExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            // Decide FIRST, before any side effect: a gated thread must
            // not scan the user's workspace, materialize the helper, or read the
            // gateway bearer at all — not merely have its tools hidden afterwards.
            if tools_gated_for(input.config) {
                input.thread_store.insert(CppToolsState {
                    gated: true,
                    is_cpp: false,
                    cwd: input.config.cwd.to_path_buf(),
                    gateway_base: String::new(),
                    bearer: None,
                    gdb_sessions: GdbRegistry::new(),
                    kernel_sessions: KernelRegistry::new(),
                    gdb_attach: AttachState::new(),
                    native_exec: false,
                    sandbox: Arc::new(crate::sandbox::Settings {
                        helper: None,
                        workspace: input.config.cwd.to_path_buf(),
                        relaxed: false,
                        required: false,
                    }),
                    env_note: EnvNote::none(),
                    workflow: None,
                    head: SessionHead::none(input.config.cwd.to_path_buf()),
                    test_runs: Jobs::new(),
                    local_only: false,
                    started_unix: 0,
                    full_access: false,
                });
                return;
            }

            // Materialize the engine-owned helper script into the user's config
            // dir exactly once per process, independent of whether this is a
            // C/C++ workspace (the script ships with the binary and should be
            // present alongside the ByteAsk config regardless of task).
            static MATERIALIZED: OnceLock<()> = OnceLock::new();
            let codex_home = input.config.codex_home.clone();
            MATERIALIZED.get_or_init(|| crate::helper::materialize_helper(codex_home.as_path()));

            if codex_core::byteask_harness::flag(input.config, "fast") {
                input.thread_store.insert(FastProfile);
            }
            let cwd = input.config.cwd.to_path_buf();
            let is_cpp = detect::is_cpp_workspace(&cwd);
            let native_exec = crate::trust::native_exec_allowed(input.config);
            // With full permission nothing here stops to ask the user.
            let full_access = crate::trust::full_access(input.config);
            // The relax setting is read from the protected policy
            // file only, never from the repository.
            let relaxed = byteask_policy::load(input.config.codex_home.as_path(), std::slice::from_ref(&cwd))
                .policy
                .is_relaxed(&cwd);
            let sandbox = Arc::new(crate::sandbox::Settings {
                helper: input.config.codex_linux_sandbox_exe.clone(),
                workspace: cwd.clone(),
                relaxed,
                // Without a usable sandbox the tools run without one (and say so),
                // unless the protected policy file requires it.
                required: byteask_policy::sandbox_required(input.config.codex_home.as_path()).0,
            });
            // Resolve the gateway identity independently of the active provider,
            // so cpp_intrinsic still reaches the gateway in a subscription/BYOK
            // session (active provider `openai`/`byok-local`, identity retained
            // in the `byteask` block). See `Config::byteask_identity`.
            let (gateway_base, bearer) = match input.config.byteask_identity() {
                Some((base, token)) => (base, Some(token)),
                None => (String::new(), None),
            };
            let env_note = if is_cpp
                && !crate::ablation::Ablation::from_env()
                    .with_fast(codex_core::byteask_harness::flag(input.config, "fast"))
                    .is_off("env_note")
            {
                EnvNote::spawn(cwd.clone(), Some(input.config.codex_home.as_path().join("cache")))
            } else {
                EnvNote::none()
            };
            // Root threads only: sub-agents and internal sessions finish into their parent.
            let is_root = !matches!(
                input.session_source,
                SessionSource::SubAgent(_) | SessionSource::Internal(_)
            );
            let non_interactive = matches!(input.session_source, SessionSource::Exec);
            let cpp_flag = |key: &str| {
                input
                    .config
                    .config_layer_stack
                    .effective_config()
                    .get(crate::gate::CONFIG_TABLE)
                    .and_then(|table| table.get(key).and_then(|v| v.as_bool()))
                    .unwrap_or(false)
            };
            // Process-wide: the warm build and the checks consult it (one session per process).
            if cpp_flag(TREE_OWNER_KEY) {
                crate::testsel::tree::set_owner_mode(true);
            }
            // Optional switch (off by default): the engine runs cpp_finish_check itself on an unchecked final change.
            if cpp_flag(crate::tools::FINISH_REVIEW_KEY)
                || std::env::var(crate::tools::ENV_FINISH_REVIEW)
                    .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
            {
                crate::tools::set_finish_review(true);
            }
            if cpp_flag(HIDE_MISSING_KEY) {
                input.thread_store.insert(HideMissing(missing_tools(
                    |program| crate::envnote::find_on_path(program).is_some(),
                    non_interactive,
                )));
            }
            let settings = crate::workflow::settings_from(input.config, non_interactive);
            // Always in a C/C++ root thread (even with the automatic gate and checklist off):
            // `cpp_finish_check` compares with its session-start snapshot.
            let workflow = (is_cpp && is_root).then(|| {
                let workflow = Workflow::start(settings, native_exec, non_interactive, cwd.clone());
                // The gate's builds and tests run in the Build profile too.
                workflow.set_sandbox(sandbox.clone());
                // Its verdicts go into this thread's change in the results store.
                workflow.set_session(input.thread_store.level_id());
                // The agent's own build and test runs are recorded against this workflow.
                crate::tools::set_root_workflow(&workflow);
                workflow
            });
            let head = SessionHead::spawn(cwd.clone());
            input.thread_store.insert(CppToolsState {
                gated: false,
                is_cpp,
                cwd,
                gateway_base,
                bearer,
                gdb_sessions: GdbRegistry::new(),
                kernel_sessions: KernelRegistry::new(),
                gdb_attach: AttachState::new(),
                native_exec,
                sandbox,
                env_note,
                workflow,
                head,
                test_runs: Jobs::new(),
                local_only: byteask_policy::local_only(input.config.codex_home.as_path()).0,
                started_unix: crate::results_store::now_unix(),
                full_access,
            });
        })
    }
}

impl ToolContributor for CppToolsExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<
        Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>,
    > {
        // The tools are exposed in every thread, not just C/C++ workspaces:
        // whether to use them is a per-task decision the model makes (it only
        // calls e.g. `cpp_godbolt` for an asm/compiler question), so gating on
        // the cwd would wrongly hide them for a C/C++ task run from a scratch
        // dir. The heavier guidance prompt stays C/C++-workspace-gated below.
        let Some(state) = thread_store.get::<CppToolsState>() else {
            return Vec::new();
        };
        // Upstream's ephemeral structured threads get nothing — see [`tools_gated`].
        if state.gated {
            return Vec::new();
        }
        let cwd = state.cwd.clone();
        let native_exec = state.native_exec;
        // `BYTEASK_CPP_ABLATE` removes single tools.
        let ablation = crate::ablation::Ablation::from_env().with_fast(fast(thread_store));
        // cpp_perf_lab only exists when a perf lab is configured.
        let perf_lab = crate::tools::perf_lab_tool(&cwd, native_exec, crate::tools::LabTarget::from_env());
        let mut tools: Vec<
            Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>,
        > = vec![
            Arc::new(CppCheckTool { cwd: cwd.clone() }),
            Arc::new(ClangTidyTool { cwd: cwd.clone() }),
            Arc::new(SanitizeTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(FixVerifyTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(SymbolizeTool { cwd: cwd.clone() }),
            Arc::new(CompileDbTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(KdbTool {
                cwd: cwd.clone(),
                sessions: state.kernel_sessions.clone(),
                native_exec,
            }),
            Arc::new(GdbTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(GdbSessionTool {
                cwd: cwd.clone(),
                sessions: state.gdb_sessions.clone(),
                native_exec,
            }),
            Arc::new(GdbAttachTool {
                state: state.gdb_attach.clone(),
                full_access: state.full_access,
            }),
            Arc::new(PerfTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(InsightsTool { cwd: cwd.clone() }),
            Arc::new(ClangdNavTool { cwd: cwd.clone() }),
            Arc::new(ImpactTool {
                cwd: cwd.clone(),
                workflow: state.workflow.clone(),
            }),
            Arc::new(FormatTool { cwd: cwd.clone() }),
            Arc::new(BinaryTool { cwd: cwd.clone() }),
            Arc::new(BenchTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(RecordTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            Arc::new(CppLayoutTool { cwd: cwd.clone() }),
            Arc::new(DecompileTool { cwd: cwd.clone() }),
            // cpp_godbolt calls the public Compiler Explorer API directly from
            // the user's machine, so it needs no gateway URL/bearer.
            Arc::new(GodboltTool { cwd: cwd.clone() }),
            Arc::new(IntrinsicTool {
                cwd: cwd.clone(),
                gateway_base: state.gateway_base.clone(),
                bearer: state.bearer.clone(),
            }),
            Arc::new(TestSelectTool {
                cwd,
                native_exec,
                head: state.head.clone(),
                jobs: state.test_runs.clone(),
                finish_gate_suite: state
                    .workflow
                    .as_ref()
                    .is_some_and(|workflow| workflow.gate_runs_suite()),
            }),
            Arc::new(RepoHistoryTool {
                head: state.head.clone(),
            }),
            // 0.2.0: the model-called advisory finish check (the automatic gate is opt-in).
            Arc::new(FinishCheckTool {
                cwd: state.cwd.clone(),
                native_exec,
                workflow: state.workflow.clone(),
            }),
        ];
        // Reads the results store; runs nothing. Not in non-interactive sessions:
        // there the gate runs only after the final message, so during the run the tool could
        // only say that no check has run yet.
        if !state
            .workflow
            .as_ref()
            .is_some_and(|workflow| workflow.non_interactive && workflow.settings.gate)
        {
            tools.push(Arc::new(CheckStatusTool {
                cwd: state.cwd.clone(),
                home: None,
                session_started: Some(state.started_unix),
            }));
        }
        if let Some(t) = perf_lab {
            tools.push(Arc::new(t));
        }
        thread_tool_layers(tools, &state, &ablation, thread_store)
    }
}

/// The layers every ByteAsk C/C++ tool of a thread gets, whichever contributor offers it
/// (the main list above, and `crate::runtime_extension`).
pub(crate) fn wrap_thread_tools(
    tools: Vec<Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>>,
    thread_store: &ExtensionData,
) -> Vec<Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>> {
    let Some(state) = thread_store.get::<CppToolsState>() else {
        return Vec::new();
    };
    if state.gated {
        return Vec::new();
    }
    thread_tool_layers(
        tools,
        &state,
        &crate::ablation::Ablation::from_env().with_fast(fast(thread_store)),
        thread_store,
    )
}

/// Tools that work only on Linux (rr, KGDB): registered only there.
pub(crate) const LINUX_ONLY: [&str; 3] = ["cpp_kdb", "debug_record", "debug_reverse"];

/// Whether `tool` is offered on this platform.
pub(crate) fn offered_here(tool: &str) -> bool {
    cfg!(target_os = "linux") || !LINUX_ONLY.contains(&tool)
}

#[cfg(test)]
pub(crate) fn test_extension() -> impl ToolContributor {
    CppToolsExtension
}

fn thread_tool_layers(
    tools: Vec<Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>>,
    state: &CppToolsState,
    ablation: &crate::ablation::Ablation,
    thread_store: &ExtensionData,
) -> Vec<Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>> {
        // Every error a tool returns carries a NOT RUN result line. Every
        // program a tool starts runs in its sandbox profile. `BYTEASK_CPP_ABLATE`
        // removes single tools. Specs and every other property
        // are delegated unchanged.
        let local_only = state.local_only;
        let hidden = thread_store.get::<HideMissing>();
        tools
            .into_iter()
            .filter(|tool| !ablation.tool_off(&tool.tool_name().name))
            .filter(|tool| {
                hidden
                    .as_ref()
                    .is_none_or(|hidden| !hidden.0.contains(&tool.tool_name().name.as_str()))
            })
            .filter(|tool| offered_here(&tool.tool_name().name))
            // In a local-only home the tools that send code or a question
            // off this machine answer NOT RUN (offline) and send nothing.
            .map(|tool| match crate::offline::destination(&tool.tool_name().name) {
                Some(destination) if local_only => crate::offline::Offline::wrap(tool, destination),
                _ => tool,
            })
            .map(|tool| crate::sandbox::Sandboxed::wrap(tool, state.sandbox.clone()))
            .map(crate::results::WithResult::wrap)
            // The result line goes into the local results store.
            .map(|tool| results_store::Recorded::wrap(tool, thread_store))
            .collect()
}

impl ToolLifecycleContributor for CppToolsExtension {
    /// Any tool call may edit files (apply_patch, a shell command): the next step boundary
    /// checks the tree for a first C/C++ change.
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        if let Some(workflow) = input
            .thread_store
            .get::<CppToolsState>()
            .and_then(|state| state.workflow.clone())
        {
            workflow.note_tool_call();
        }
        Box::pin(std::future::ready(()))
    }

    /// Backstop logging: [`crate::report::report`] logs successful tool outputs
    /// with a preview, but error/blocked/aborted calls never reach it. Record
    /// those here so the usage dashboard captures every `cpp_*` invocation.
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let tool = input.tool_name.to_string();
        let outcome = match input.outcome {
            // Completed (normal output) is already logged by report(); skip it.
            ToolCallOutcome::Completed { .. } => None,
            ToolCallOutcome::Blocked => Some("blocked"),
            ToolCallOutcome::Failed { .. } => Some("failed"),
            ToolCallOutcome::Aborted => Some("aborted"),
        };
        Box::pin(async move {
            if let Some(outcome) = outcome
                && tool.starts_with("cpp_")
            {
                crate::report::log_tool_outcome(&tool, outcome);
            }
        })
    }
}

impl ContextContributor for CppToolsExtension {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(async move {
            // Gated threads get no guidance either: otherwise the recap
            // ships — and pays for — instructions describing 25 absent tools.
            let Some(state) = thread_store
                .get::<CppToolsState>()
                .filter(|state| !state.gated && state.is_cpp)
            else {
                return Vec::new();
            };
            // The guidance names what this thread's workflow enforces (checklist, finish gate)
            // once, and keeps the prose for what it does not; see `guidance::cpp_guidance`.
            let enforcement = async {
                match &state.workflow {
                    Some(workflow) => workflow.enforcement().await,
                    None => Enforcement::NONE,
                }
            };
            let (enforcement, note) = tokio::join!(enforcement, state.env_note.text());
            let guidance = if fast(thread_store) {
                crate::guidance::FAST_CPP_GUIDANCE.to_string()
            } else {
                crate::ablation::Ablation::from_env().guidance(cpp_guidance(enforcement))
            };
            // With full permission the guidance does not ask the model to stop for
            // the user's consent.
            let guidance = if state.full_access {
                crate::guidance::full_access_guidance(&guidance)
            } else {
                guidance
            };
            // `BYTEASK_CPP_GUIDANCE_FILE` (see `crate::ablation`): another
            // guidance text for this process. The environment note below is unaffected.
            let guidance = crate::ablation::guidance_override(|key| std::env::var_os(key))
                .unwrap_or(guidance);
            let mut fragments = vec![PromptFragment::developer_capability(
                // The same guidance text, byte for byte, whatever the workflow enforces
                // (there is no variant per enforcement).
                paragraph(guidance),
                ContentItemKind("cpp_tools.guidance".to_string()),
            )];
            if !note.is_empty() {
                fragments.push(PromptFragment::developer_capability(
                    paragraph(note),
                    ContentItemKind("cpp_tools.environment".to_string()),
                ));
            }
            fragments
        })
    }

    /// Finished background `cpp_test_select` runs, announced once each on the
    /// next request so the model never polls for them. Contributes nothing in a
    /// thread that has not started a run.
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            match input.thread_store.get::<CppToolsState>() {
                Some(state) if !state.gated && state.test_runs.has_runs() => {
                    vec![state.test_runs.world_state_section()]
                }
                _ => Vec::new(),
            }
        })
    }
}

impl TurnGateContributor for CppToolsExtension {
    fn after_step<'a>(&'a self, input: TurnGateInput<'a>) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let Some(workflow) = input
                .thread_store
                .get::<CppToolsState>()
                .and_then(|state| state.workflow.clone())
            else {
                return Vec::new();
            };
            workflow.after_step(input.turn_id).await
        })
    }

    fn before_stop<'a>(&'a self, input: TurnGateInput<'a>) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let Some(workflow) = input
                .thread_store
                .get::<CppToolsState>()
                .and_then(|state| state.workflow.clone())
            else {
                return Vec::new();
            };
            // Optional switch `finish_review`: an unchecked final change gets cpp_finish_check's answer first.
            if crate::tools::finish_review_on()
                && let Some((cwd, native_exec, sandbox)) = input
                    .thread_store
                    .get::<CppToolsState>()
                    .map(|state| (state.cwd.clone(), state.native_exec, state.sandbox.clone()))
            {
                let review =
                    crate::tools::finish_review(&cwd, native_exec, sandbox, &workflow, input.turn_id).await;
                if !review.is_empty() {
                    return review;
                }
            }
            workflow
                .before_stop(input.turn_id, input.conversation_history)
                .await
        })
    }

    fn stop_notice<'a>(&'a self, input: TurnGateInput<'a>) -> ExtensionFuture<'a, Option<String>> {
        Box::pin(async move {
            let workflow = input
                .thread_store
                .get::<CppToolsState>()
                .and_then(|state| state.workflow.clone())?;
            workflow.stop_notice(input.turn_id).await
        })
    }
}

/// Sandbox settings for a test thread: no helper, nothing relaxed.
#[cfg(test)]
fn test_sandbox() -> Arc<crate::sandbox::Settings> {
    Arc::new(crate::sandbox::Settings {
        helper: None,
        workspace: PathBuf::from("/workspace"),
        relaxed: false,
        required: false,
    })
}

/// Registers the C/C++ tooling extension's contributors with the host registry.
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(CppToolsExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.tool_contributor(extension.clone());
    builder.tool_lifecycle_contributor(extension.clone());
    builder.turn_gate_contributor(extension.clone());
    builder.prompt_contributor(extension);
    builder.tool_contributor(Arc::new(crate::runtime_extension::RuntimeTools));
}

/// What the runtime tools (`crate::runtime_extension`) need from this
/// thread's state: `(cwd, native_exec)`, or `None` for a gated thread or one
/// with no state. One source of truth for the containment gate.
pub(crate) fn runtime_context(thread_store: &ExtensionData) -> Option<(PathBuf, bool)> {
    let state = thread_store.get::<CppToolsState>()?;
    (!state.gated).then(|| (state.cwd.clone(), state.native_exec))
}

#[cfg(test)]
pub(crate) fn test_thread_store(gated: bool, native_exec: bool) -> ExtensionData {
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(CppToolsState {
        gated,
        is_cpp: true,
        cwd: PathBuf::from("/workspace"),
        gateway_base: String::new(),
        bearer: None,
        gdb_sessions: GdbRegistry::new(),
        kernel_sessions: KernelRegistry::new(),
        gdb_attach: AttachState::new(),
        native_exec,
        sandbox: test_sandbox(),
        env_note: EnvNote::none(),
        workflow: None,
        head: SessionHead::none(PathBuf::from("/tmp")),
        test_runs: Jobs::new(),
        local_only: false,
        started_unix: 0,
        full_access: false,
    });
    thread_store
}

#[cfg(test)]
mod tests {
    use codex_extension_api::ExtensionData;
    use codex_extension_api::ToolName;
    use pretty_assertions::assert_eq;

    use super::*;

    /// Number of tools this crate contributes to an ungated thread.
    /// 26 (0.2.0: + cpp_finish_check); the Linux-only cpp_kdb and debug_record are not
    /// registered elsewhere.
    const ALL_TOOLS: usize = if cfg!(target_os = "linux") { 26 } else { 24 };

    fn features(shell: bool, unified_exec: bool) -> Features {
        let mut features = Features::with_defaults();
        features.set_enabled(Feature::ShellTool, shell);
        features.set_enabled(Feature::UnifiedExec, unified_exec);
        features
    }

    /// Seed a thread store the way `on_thread_start` does for a C/C++ workspace
    /// whose config carries these knobs. The gate decision itself runs for real,
    /// so these tests exercise the policy, not a hand-written `gated` flag.
    fn thread_store_for(ephemeral: bool, shell: bool) -> ExtensionData {
        let thread_store = ExtensionData::new("thread");
        let gated = tools_gated(ephemeral, &features(shell, /*unified_exec*/ true));
        thread_store.insert(CppToolsState {
            gated,
            is_cpp: true,
            cwd: PathBuf::from("/tmp"),
            gateway_base: String::new(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: false,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(PathBuf::from("/tmp")),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        });
        thread_store
    }

    #[test]
    fn every_tool_has_a_sandbox_profile() {
        // A tool with no profile may not start programs, and this
        // fails before such a tool can ship.
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ false, /*shell*/ true);
        let tools = CppToolsExtension.tools(&session_store, &thread_store);
        assert_eq!(tools.len(), ALL_TOOLS);
        for tool in tools {
            let name = tool.tool_name().name;
            assert!(crate::sandbox::profile_for(&name).is_some(), "{name} has no sandbox profile");
        }
    }

    async fn guidance_fragments(thread_store: &ExtensionData) -> Vec<PromptFragment> {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        extension
            .contribute_thread_context(&session_store, thread_store)
            .await
    }

    /// The containment case: upstream's auto-recap / guardian / memories threads.
    /// Assert the absence directly — not "upstream disables the shell".
    #[tokio::test]
    async fn ephemeral_thread_without_shell_gets_no_tools_and_no_guidance() {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ true, /*shell*/ false);
        assert!(extension.tools(&session_store, &thread_store).is_empty());
        assert!(guidance_fragments(&thread_store).await.is_empty());
    }

    /// `byteask exec --ephemeral` is a real user flag and must keep every tool.
    #[tokio::test]
    async fn ephemeral_thread_with_shell_keeps_tools_and_guidance() {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ true, /*shell*/ true);
        assert_eq!(
            extension.tools(&session_store, &thread_store).len(),
            ALL_TOOLS
        );
        assert_eq!(guidance_fragments(&thread_store).await.len(), 1);
    }

    /// A user who merely turned the shell off keeps the read-only analysers.
    #[tokio::test]
    async fn non_ephemeral_thread_without_shell_keeps_tools_and_guidance() {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = thread_store_for(/*ephemeral*/ false, /*shell*/ false);
        assert_eq!(
            extension.tools(&session_store, &thread_store).len(),
            ALL_TOOLS
        );
        assert_eq!(guidance_fragments(&thread_store).await.len(), 1);
    }

    /// The session-start environment note follows the guidance as its own
    /// fragment when there is one, and never reaches a gated thread.
    #[tokio::test]
    async fn the_environment_note_follows_the_guidance() {
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: true,
            cwd: PathBuf::from("/tmp"),
            gateway_base: String::new(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: false,
            sandbox: test_sandbox(),
            env_note: EnvNote::ready("C/C++ environment (probed at session start):".to_string()),
            workflow: None,
            head: SessionHead::none(PathBuf::from("/tmp")),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        });
        let fragments = guidance_fragments(&thread_store).await;
        assert_eq!(fragments.len(), 2);
        // No workflow in this thread: nothing is enforced, so the prose variant.
        assert_eq!(fragments[0].text(), paragraph(cpp_guidance(Enforcement::NONE)));
        assert_eq!(fragments[1].text(), "C/C++ environment (probed at session start):\n\n");
        assert_eq!(fragments[1].content_kind().0, "cpp_tools.environment");

        let gated = thread_store_for(/*ephemeral*/ true, /*shell*/ false);
        assert!(guidance_fragments(&gated).await.is_empty());
    }

    /// A root thread whose workflow enforces the checklist and the finish gate gets the
    /// guidance that names them instead of asking for the same steps in prose; without
    /// native execution the gate cannot build or test, so that part stays prose.
    #[tokio::test]
    async fn the_guidance_names_what_the_workflow_enforces() {
        let Some(dir) = crate::impact::tests::fixture_base() else {
            eprintln!("skipped: git not available");
            return;
        };
        let state = |native_exec: bool| CppToolsState {
            gated: false,
            is_cpp: true,
            cwd: dir.path().to_path_buf(),
            gateway_base: String::new(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: Some(Workflow::start(
                crate::gate::GateSettings::default(),
                native_exec,
                true,
                dir.path().to_path_buf(),
            )),
            head: SessionHead::none(dir.path().to_path_buf()),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        };
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(state(true));
        let fragments = guidance_fragments(&thread_store).await;
        assert_eq!(fragments[0].text(), paragraph(crate::guidance::CPP_GUIDANCE));

        let thread_store = ExtensionData::new("thread");
        thread_store.insert(state(false));
        let fragments = guidance_fragments(&thread_store).await;
        assert_eq!(
            fragments[0].text(),
            paragraph(cpp_guidance(Enforcement {
                checklist: true,
                finish_suite: false,
            }))
        );

        // Outside a git work tree there is no snapshot, so nothing is enforced.
        let plain = tempfile::tempdir().expect("tempdir");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            cwd: plain.path().to_path_buf(),
            workflow: Some(Workflow::start(
                crate::gate::GateSettings::default(),
                true,
                true,
                plain.path().to_path_buf(),
            )),
            ..state(true)
        });
        let fragments = guidance_fragments(&thread_store).await;
        assert_eq!(fragments[0].text(), paragraph(cpp_guidance(Enforcement::NONE)));
    }

    /// Pins `shell_enabled` to `ShellTool` alone. `UnifiedExec` is force-enabled by
    /// `ManagedFeatures::normalize_candidate` for every non-enterprise config, so
    /// reading it as `shell_enabled = ShellTool || UnifiedExec` would make the
    /// gate unreachable — including on the recap thread it exists for.
    #[test]
    fn unified_exec_being_on_does_not_ungate_a_shell_less_ephemeral_thread() {
        assert!(tools_gated(
            /*ephemeral*/ true,
            &features(/*shell*/ false, /*unified_exec*/ true)
        ));
        assert!(tools_gated(
            /*ephemeral*/ true,
            &features(/*shell*/ false, /*unified_exec*/ false)
        ));
    }

    /// The gate is duplicated in `codex-ref-tools`, so the two copies must
    /// never drift — including in *which* features each one reads.
    #[test]
    fn gate_decision_is_identical_in_both_crates() {
        for ephemeral in [false, true] {
            for shell in [false, true] {
                for unified_exec in [false, true] {
                    let features = features(shell, unified_exec);
                    assert_eq!(
                        tools_gated(ephemeral, &features),
                        codex_ref_tools::tools_gated(ephemeral, &features),
                        "ephemeral={ephemeral} shell={shell} unified_exec={unified_exec}"
                    );
                }
            }
        }
    }

    #[test]
    fn tools_present_even_when_not_cpp_workspace() {
        // The tools follow the task, not the cwd: they must be exposed even when
        // the workspace isn't a detected C/C++ project (e.g. a scratch dir).
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: false,
            cwd: PathBuf::from("/tmp"),
            gateway_base: String::new(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: true,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(PathBuf::from("/tmp")),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        });
        assert_eq!(extension.tools(&session_store, &thread_store).len(), ALL_TOOLS);
    }

    #[test]
    fn no_tools_without_thread_state() {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        assert!(extension.tools(&session_store, &thread_store).is_empty());
    }

    #[test]
    fn materialize_helper_is_idempotent_across_thread_starts() {
        // on_thread_start calls materialize_helper once per process via a
        // OnceLock, but the function itself must be safe to call repeatedly
        // (e.g. across processes/tests). Guard the call-site contract: two
        // calls leave the embedded helper in place and never panic.
        let dir = tempfile::tempdir().unwrap();
        crate::helper::materialize_helper(dir.path());
        crate::helper::materialize_helper(dir.path());
        let written = std::fs::read(dir.path().join("byteask_gdb_bridge.py")).unwrap();
        assert_eq!(written, include_str!("../byteask_gdb_bridge.py").as_bytes());
    }

    /// The result contract: whatever a tool does with a call it cannot
    /// serve, its text ends with a valid shared result, an error is always
    /// NOT RUN with a fix, and nothing that did not run is a PASS.
    #[tokio::test]
    async fn every_tool_ends_every_answer_with_a_shared_result() {
        use codex_extension_api::ConversationHistory;
        use codex_extension_api::NoopTurnItemEmitter;
        use codex_extension_api::ToolCall;
        use codex_extension_api::ToolCallSource;
        use codex_extension_api::ToolPayload;

        let dir = tempfile::tempdir().unwrap();
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: true,
            cwd: dir.path().to_path_buf(),
            gateway_base: crate::test_support::closed_port_url(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: false,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(dir.path().to_path_buf()),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        });
        let tools = extension.tools(&session_store, &thread_store);
        assert_eq!(tools.len(), ALL_TOOLS);
        for args in [
            serde_json::json!({ "__not_an_argument__": 1 }),
            serde_json::json!({}),
        ] {
            for tool in &tools {
                let name = tool.tool_name().name;
                let payload = ToolPayload::Function {
                    arguments: args.to_string(),
                };
                let outcome = tool
                    .handle(ToolCall {
                        turn_id: "t".to_string(),
                        call_id: "c".to_string(),
                        tool_name: tool.tool_name(),
                        model: "m".to_string(),
                        codex_turn_metadata: None,
                        truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
                        source: ToolCallSource::Direct,
                        conversation_history: ConversationHistory::default(),
                        turn_item_emitter: Arc::new(NoopTurnItemEmitter),
                        environments: Vec::new(),
                        payload: payload.clone(),
                    })
                    .await;
                match outcome {
                    Err(codex_extension_api::FunctionCallError::RespondToModel(text))
                    | Err(codex_extension_api::FunctionCallError::Fatal(text)) => {
                        let result = check_result::CheckResult::from_tool_text(&name, &text)
                            .unwrap_or_else(|| panic!("{name} {args}: error without a result line: {text}"));
                        assert_eq!(result.verdict, check_result::Verdict::NotRun, "{name} {args}: {text}");
                        assert!(result.fix.is_some_and(|fix| !fix.is_empty()), "{name}: {text}");
                    }
                    Ok(output) => {
                        let text = match output.to_response_item("c", &payload) {
                            codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } => output
                                .content_items()
                                .unwrap_or_default()
                                .iter()
                                .filter_map(|item| match item {
                                    codex_protocol::models::FunctionCallOutputContentItem::InputText { text } => Some(text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                            other => panic!("{name}: unexpected item {other:?}"),
                        };
                        let result = check_result::CheckResult::from_tool_text(&name, &text)
                            .unwrap_or_else(|| panic!("{name} {args}: output without a result line: {text}"));
                        assert_eq!(output.success_for_logging(), result.is_pass(), "{name}");
                    }
                }
            }
        }
    }

    #[test]
    fn contributes_cpp_tools_when_cpp_workspace() {
        let extension = CppToolsExtension;
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: true,
            cwd: PathBuf::from("/tmp"),
            gateway_base: "https://code.byteask.ai/byteask/v1".to_string(),
            bearer: Some("test-token".into()),
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: true,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(PathBuf::from("/tmp")),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access: false,
        });
        let names = extension
            .tools(&session_store, &thread_store)
            .iter()
            .map(|tool| tool.tool_name())
            .collect::<Vec<_>>();
        let expected: Vec<ToolName> = vec![
                ToolName::plain("cpp_check"),
                ToolName::plain("cpp_tidy"),
                ToolName::plain("cpp_sanitize"),
                ToolName::plain("cpp_fix_verify"),
                ToolName::plain("cpp_symbolize"),
                ToolName::plain("cpp_compile_db"),
                ToolName::plain("cpp_kdb"),
                ToolName::plain("debug"),
                ToolName::plain("debug_session"),
                ToolName::plain("debug_attach"),
                ToolName::plain("cpp_perf"),
                ToolName::plain("cpp_insights"),
                ToolName::plain("cpp_nav"),
                ToolName::plain("cpp_impact"),
                ToolName::plain("cpp_format"),
                ToolName::plain("cpp_binary"),
                ToolName::plain("cpp_bench"),
                ToolName::plain("debug_record"),
                ToolName::plain("cpp_layout"),
                ToolName::plain("cpp_decompile"),
                ToolName::plain("cpp_godbolt"),
                ToolName::plain("cpp_intrinsic"),
                ToolName::plain("cpp_test_select"),
                ToolName::plain("cpp_repo_history"),
                ToolName::plain("cpp_finish_check"),
                ToolName::plain("check_status"),
            ]
            .into_iter()
            .filter(|name| offered_here(&name.name))
            .collect();
        assert_eq!(names, expected);
    }
}

/// Tool descriptions, parameter docs and guidance are
/// PROTECTED. They are what makes the model pick the right tool with the right arguments,
/// so no refactor, upstream merge or "token saving" may change one silently.
///
/// When this fails, the diff IS the review: check that no routing cue, constraint, default,
/// unit, example or caveat was lost, get a maintainer's sign-off, and only then regenerate with
/// `BYTEASK_UPDATE_TOOL_SNAPSHOT=1 just test -p codex-cpp-tools -E 'test(description_snapshot)'`.
#[cfg(test)]
mod description_snapshot_tests {
    use codex_extension_api::ExtensionData;
    use pretty_assertions::assert_eq;

    use super::*;

    const SNAPSHOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/snapshots/model_facing_text.json"
    );

    /// Tools added later, pinned in a snapshot of their own so the existing
    /// pinned file stays byte for byte as it was (new tools get new snapshots).
    const NEW_TOOLS: [&str; 5] = [
        "check_status",
        "cpp_fix_verify",
        "cpp_impact",
        "cpp_repo_history",
        "cpp_test_select",
    ];
    const NEW_TOOLS_SNAPSHOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/snapshots/model_facing_text_new_tools.json"
    );
    /// 0.2.0's new tool, pinned in a file of its own.
    const RELEASE_020_TOOLS: [&str; 1] = ["cpp_finish_check"];
    const RELEASE_020_SNAPSHOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/snapshots/finish_check.json"
    );
    /// The full-permission variants: the guidance and debug_attach.
    const FULL_ACCESS_SNAPSHOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/snapshots/full_access_variants.json"
    );

    fn current() -> String {
        render(false)
    }

    /// The pinned rendering: the guidance and every tool except [`NEW_TOOLS`] and
    /// [`RELEASE_020_TOOLS`] (`new_tools: false`), or only [`NEW_TOOLS`] (`true`, no guidance).
    fn render(new_tools: bool) -> String {
        let thread_store = state_store(false);
        let session_store = ExtensionData::new("session");
        let mut tools: Vec<serde_json::Value> = CppToolsExtension
            .tools(&session_store, &thread_store)
            .iter()
            .filter(|tool| !RELEASE_020_TOOLS.contains(&tool.tool_name().name.as_str()))
            .filter(|tool| NEW_TOOLS.contains(&tool.tool_name().name.as_str()) == new_tools)
            .map(|tool| sorted_keys(serde_json::to_value(tool.spec()).expect("spec serialises")))
            .collect();
        tools.sort_by_key(std::string::ToString::to_string);
        let doc = if new_tools {
            serde_json::json!({ "tools": tools })
        } else {
            serde_json::json!({ "guidance": CPP_GUIDANCE, "tools": tools })
        };
        format!("{}\n", serde_json::to_string_pretty(&doc).expect("json"))
    }

    fn tools_named(thread_store: &ExtensionData, names: &[&str]) -> Vec<serde_json::Value> {
        let mut tools: Vec<serde_json::Value> = CppToolsExtension
            .tools(&ExtensionData::new("session"), thread_store)
            .iter()
            .filter(|tool| names.contains(&tool.tool_name().name.as_str()))
            .map(|tool| sorted_keys(serde_json::to_value(tool.spec()).expect("spec serialises")))
            .collect();
        tools.sort_by_key(std::string::ToString::to_string);
        tools
    }

    fn check_or_update(path: &str, now: &str) {
        if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
            std::fs::write(path, now).expect("write snapshot");
            return;
        }
        let pinned = std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{path} missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1"));
        assert_eq!(pinned, now, "model-facing text in {path} changed; see the module comment");
    }

    #[test]
    fn finish_check_text_snapshot_is_byte_identical() {
        let tools = tools_named(&state_store(false), &RELEASE_020_TOOLS);
        assert_eq!(tools.len(), RELEASE_020_TOOLS.len());
        let now = format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({ "tools": tools })).expect("json")
        );
        check_or_update(RELEASE_020_SNAPSHOT, &now);
    }

    /// Both variants are pinned: the default ones in `model_facing_text.json`, the full-access
    /// ones here (the guidance as the thread really receives it, and debug_attach's spec).
    #[tokio::test]
    async fn full_access_variants_snapshot_is_byte_identical() {
        let thread_store = state_store(true);
        let fragments = ContextContributor::contribute_thread_context(
            &CppToolsExtension,
            &ExtensionData::new("session"),
            &thread_store,
        )
        .await;
        let guidance = fragments[0].text().to_string();
        assert_eq!(guidance, paragraph(crate::guidance::full_access_guidance(CPP_GUIDANCE)));
        let tools = tools_named(&thread_store, &["debug_attach"]);
        let now = format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({ "guidance": guidance, "tools": tools }))
                .expect("json")
        );
        check_or_update(FULL_ACCESS_SNAPSHOT, &now);
        // The default thread keeps the approved text.
        let fragments = ContextContributor::contribute_thread_context(
            &CppToolsExtension,
            &ExtensionData::new("session"),
            &state_store(false),
        )
        .await;
        assert_eq!(fragments[0].text(), paragraph(CPP_GUIDANCE));
    }

    fn state_store(full_access: bool) -> ExtensionData {
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: true,
            // A fixed path: a tool must never bake the working directory into its spec.
            cwd: PathBuf::from("/workspace"),
            gateway_base: String::new(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: false,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(PathBuf::from("/tmp")),
            test_runs: Jobs::new(),
            local_only: false,
            started_unix: 0,
            full_access,
        });
        thread_store
    }

    #[test]
    fn new_tools_text_snapshot_is_byte_identical() {
        let now = render(true);
        if std::env::var_os("BYTEASK_UPDATE_NEW_TOOL_SNAPSHOT").is_some() {
            std::fs::write(NEW_TOOLS_SNAPSHOT, &now).expect("write snapshot");
            return;
        }
        let pinned = std::fs::read_to_string(NEW_TOOLS_SNAPSHOT).expect("new-tools snapshot missing");
        assert_eq!(pinned, now, "model-facing text of a new tool changed");
        // Every new tool is really there, so the split cannot hide a missing one.
        for name in NEW_TOOLS {
            assert!(now.contains(&format!("\"name\": \"{name}\"")), "{name} is not pinned");
        }
    }

    /// Keys sorted at every level. The pinned file was written with sorted keys;
    /// a build that also compiles a crate turning on serde_json's `preserve_order`
    /// (the TUI, the toolbox) serialises in insertion order instead, which changed
    /// the rendering, and the tool order, without any text changing.
    fn sorted_keys(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                serde_json::Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key, sorted_keys(value)))
                        .collect(),
                )
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(sorted_keys).collect())
            }
            other => other,
        }
    }

    /// The pinned file lists cpp_kdb and debug_record, which only Linux registers.
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "pinned with the Linux-only tools")]
    fn description_snapshot_is_byte_identical() {
        let now = current();
        if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
            std::fs::create_dir_all(std::path::Path::new(SNAPSHOT).parent().expect("parent"))
                .expect("mkdir");
            std::fs::write(SNAPSHOT, &now).expect("write snapshot");
            return;
        }
        let pinned = std::fs::read_to_string(SNAPSHOT)
            .expect("snapshot missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1");
        assert_eq!(
            pinned, now,
            "model-facing tool text changed; see the module comment"
        );
    }

    #[test]
    fn hide_missing_tools_leaves_out_tools_without_their_program() {
        let have_none = missing_tools(|_| false, /*non_interactive*/ false);
        assert_eq!(
            have_none,
            [
                "cpp_check",
                "cpp_tidy",
                "cpp_format",
                "cpp_decompile",
                "debug_record"
            ]
        );
        let exec = missing_tools(
            |program| program != "cppcheck",
            /*non_interactive*/ true,
        );
        assert_eq!(exec, ["cpp_check", "check_status"]);

        let names = |thread_store: &ExtensionData| -> Vec<String> {
            CppToolsExtension
                .tools(&ExtensionData::new("session"), thread_store)
                .iter()
                .map(|tool| tool.tool_name().name)
                .collect()
        };
        let thread_store = state_store(false);
        let before = names(&thread_store);
        assert!(before.iter().any(|name| name == "cpp_check"));
        assert!(before.iter().any(|name| name == "check_status"));
        thread_store.insert(HideMissing(exec));
        let after = names(&thread_store);
        assert_eq!(after.len(), before.len() - 2);
        assert!(
            !after
                .iter()
                .any(|name| name == "cpp_check" || name == "check_status")
        );
        assert!(after.iter().any(|name| name == "cpp_tidy"));
    }
}

/// In a local-only home the four online tools answer NOT RUN (offline)
/// without running; every other tool is untouched.
#[cfg(test)]
mod local_only_tests {
    use std::sync::Arc;

    use codex_extension_api::ConversationHistory;
    use codex_extension_api::ExtensionData;
    use codex_extension_api::NoopTurnItemEmitter;
    use codex_extension_api::ToolCall;
    use codex_extension_api::ToolCallSource;
    use codex_extension_api::ToolPayload;
    use pretty_assertions::assert_eq;

    use super::*;

    fn state(dir: &std::path::Path, local_only: bool) -> ExtensionData {
        let thread_store = ExtensionData::new("thread");
        thread_store.insert(CppToolsState {
            gated: false,
            is_cpp: true,
            cwd: dir.to_path_buf(),
            gateway_base: crate::test_support::closed_port_url(),
            bearer: None,
            gdb_sessions: GdbRegistry::new(),
            kernel_sessions: KernelRegistry::new(),
            gdb_attach: AttachState::new(),
            native_exec: false,
            sandbox: test_sandbox(),
            env_note: EnvNote::none(),
            workflow: None,
            head: SessionHead::none(dir.to_path_buf()),
            test_runs: Jobs::new(),
            local_only,
            started_unix: 0,
            full_access: false,
        });
        thread_store
    }

    async fn answer(
        tool: &Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>,
        args: serde_json::Value,
    ) -> String {
        let payload = ToolPayload::Function {
            arguments: args.to_string(),
        };
        match tool
            .handle(ToolCall {
                turn_id: "t".to_string(),
                call_id: "c".to_string(),
                tool_name: tool.tool_name(),
                model: "m".to_string(),
                codex_turn_metadata: None,
                truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
                source: ToolCallSource::Direct,
                conversation_history: ConversationHistory::default(),
                turn_item_emitter: Arc::new(NoopTurnItemEmitter),
                environments: Vec::new(),
                payload: payload.clone(),
            })
            .await
        {
            Err(codex_extension_api::FunctionCallError::RespondToModel(text))
            | Err(codex_extension_api::FunctionCallError::Fatal(text)) => text,
            Ok(output) => output.log_output(),
        }
    }

    #[tokio::test]
    async fn online_tools_answer_offline_and_send_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let session_store = ExtensionData::new("session");
        let local = CppToolsExtension.tools(&session_store, &state(dir.path(), true));
        let normal = CppToolsExtension.tools(&session_store, &state(dir.path(), false));
        assert_eq!(local.len(), normal.len(), "the tool list does not change");
        let args = serde_json::json!({"source": "int main(){}", "code": "int main(){}",
            "path": "a.cpp", "name": "_mm_add_ps", "query": "_mm_add_ps"});
        for name in ["cpp_godbolt", "cpp_insights", "cpp_bench", "cpp_intrinsic"] {
            let tool = local.iter().find(|tool| tool.tool_name().name == name).expect(name);
            let unwrapped = normal.iter().find(|tool| tool.tool_name().name == name).expect(name);
            assert_eq!(
                serde_json::to_value(tool.spec()).expect("spec"),
                serde_json::to_value(unwrapped.spec()).expect("spec"),
                "{name}: the spec is unchanged"
            );
            let text = answer(tool, args.clone()).await;
            let result = check_result::CheckResult::from_tool_text(name, &text).expect("result line");
            assert_eq!(result.reason, Some(check_result::NotRunReason::Offline), "{name}: {text}");
            assert!(text.contains("local only"), "{text}");
            assert!(
                result.fix.as_deref().unwrap_or_default().contains("byteask policy local-only off"),
                "{text}"
            );
        }
        // A tool that works on this machine is not touched.
        let check = local.iter().find(|tool| tool.tool_name().name == "cpp_check").expect("cpp_check");
        let text = answer(check, serde_json::json!({"path": "missing.cpp"})).await;
        assert!(!text.contains("local only"), "{text}");
    }
}
