//! The C/C++ tools' own sandbox.
//!
//! The tools spawn compilers, the user's program, profilers and analysers as
//! direct children of the engine (or of the toolbox inside Claude Code or
//! Codex), outside any sandbox that wraps a shell tool. Every program a tool
//! starts now goes through [`wrap`], which puts it inside the engine's own
//! Linux sandbox helper (`codex-sandboxing`/`linux-sandbox`: bubblewrap
//! first, its landlock fallback second) with the tool's profile:
//!
//! | profile   | tools | may write | network |
//! |-----------|-------|-----------|---------|
//! | `Analyze` | cpp_check, cpp_tidy, cpp_nav, cpp_layout, cpp_binary, cpp_symbolize, cpp_decompile, cpp_godbolt, cpp_insights, cpp_intrinsic, debug_attach, cpp_impact, cpp_repo_history | temp dirs | none |
//! | `Build`   | cpp_sanitize, cpp_perf, cpp_bench, cpp_compile_db, cpp_format, cpp_test_select, cpp_fix_verify, cpp_perf_lab (ssh/git to the lab) | the workspace, temp dirs, ByteAsk's kept build trees, the compiler cache | allowed |
//!
//! A `Build` keeps the network and needs bubblewrap's PID namespace. The
//! engine helper's no-network mode also blocks `ptrace`, and LeakSanitizer
//! stops the program's threads with `ptrace` for every leak check, so a
//! no-network build would fail every default `cpp_sanitize` run; real CMake
//! projects also download dependencies while configuring. Inside the PID
//! namespace `ptrace` reaches only the build's own processes. Under the
//! landlock fallback there is no PID namespace, so a build runs there only
//! when the kernel's Yama setting already limits `ptrace` to a process's own
//! children (`kernel.yama.ptrace_scope` >= 1); otherwise it is NOT RUN.
//! | `Debug`   | debug, debug_session, debug_record | not sandboxed yet, and the output says so | |
//! | `Kernel`  | cpp_kdb | not sandboxed yet, and the output says so | |
//!
//! A tool with no profile cannot start a program at all, and a test fails when
//! a registry tool has none.
//!
//! If no sandbox can start (no bubblewrap or landlock: Docker, old kernels, WSL),
//! the tool runs without one and its answer says so in one line; never
//! silently. Only when the local policy requires the sandbox (`[sandbox]
//! required = true` in `~/.byteask/policy.toml`) does it still answer NOT RUN
//! (sandbox unavailable) and name the fixes. A project the user
//! relaxed with `byteask policy relax` (stored in `~/.byteask/policy.toml`,
//! outside every repository and every writable sandbox path) runs without
//! the sandbox, and its output says the sandbox was off.
//!
//! Not covered yet, each stated in the tool's output rather than hidden:
//! debuggers (the engine helper's network filter also blocks `ptrace`, and a
//! fresh `/dev` inside the sandbox hides the terminal `debug_session` gives the
//! program), `cpp_kdb` (its gdb must reach the target's host:port), and macOS
//! and Windows (Seatbelt and the Windows sandbox are not wired; that code
//! cannot be built or run on this machine).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use check_result::CheckResult;
use check_result::NotRunReason;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_tools::ToolExposure;
use codex_tools::ToolSearchInfo;

/// What one tool's programs may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Analyze,
    Build,
    Debug,
    Kernel,
}

/// The profile of every tool in the registry. `None`: this tool may not
/// start programs.
pub(crate) fn profile_for(tool: &str) -> Option<Profile> {
    Some(match tool {
        "cpp_check" | "cpp_tidy" | "cpp_nav" | "cpp_layout" | "cpp_binary" | "cpp_symbolize"
        | "cpp_decompile" | "cpp_godbolt" | "cpp_insights" | "cpp_intrinsic" | "debug_attach"
        | "cpp_impact" | "cpp_repo_history" | "check_status"
        // Integration: port_check's strategies run read-only with no network, so a strategy
        // under replay can never write the user's tree or reach a live venue.
        | "port_check" => Profile::Analyze,
        "cpp_sanitize" | "cpp_perf" | "cpp_bench" | "cpp_compile_db" | "cpp_format"
        | "cpp_test_select" | "cpp_fix_verify" | "cpp_finish_gate" | "cpp_finish_check"
        | "cpp_perf_lab" => Profile::Build,
        "debug" | "debug_session" | "debug_record" | "debug_reverse" | "debug_core" => Profile::Debug,
        "cpp_kdb" => Profile::Kernel,
        _ => return None,
    })
}

/// Per-thread sandbox settings, resolved at thread start.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// The engine's Linux sandbox helper (`codex_linux_sandbox_exe`).
    pub(crate) helper: Option<PathBuf>,
    /// The workspace a `Build` profile may write.
    pub(crate) workspace: PathBuf,
    /// The user relaxed this project (`byteask policy relax`).
    pub(crate) relaxed: bool,
    /// The local policy requires the sandbox (`[sandbox] required = true`): when none can
    /// start, refuse instead of running without one.
    pub(crate) required: bool,
}

/// The context of one tool call, set by [`Sandboxed`] for the call's whole
/// future.
#[derive(Clone, Debug)]
struct Call {
    tool: String,
    settings: Arc<Settings>,
    /// The one line this call's output carries about the sandbox; set by the
    /// first program it starts.
    note: Arc<std::sync::Mutex<Option<String>>>,
}

tokio::task_local! {
    static CALL: Call;
}

/// The note for the current tool call's output, if it started a program:
/// whether it ran sandboxed, unsandboxed by the user's choice, or unsandboxed
/// because this profile or platform is not covered yet.
pub(crate) fn current_note() -> Option<String> {
    CALL.try_with(|call| {
        call.note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
    .ok()
    .flatten()
}

/// What [`wrap`] decided for one program.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wrapped {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    /// The program runs under the sandbox helper. Bubblewrap reports a program
    /// killed by signal N as exit code 128+N; the caller turns that back into
    /// "killed by a signal" so a crash still reads as a crash.
    pub(crate) sandboxed: bool,
    /// Protected names (`.git`, `.agents`, `.codex`) missing in a writable root before the run.
    /// The helper creates empty placeholders there for the run and removes them when it exits
    /// normally; a helper that was killed cannot, and an empty `.git` left in a workspace makes
    /// it look like a repository. [`remove_placeholders`] removes what is left.
    pub(crate) placeholders: Vec<PathBuf>,
}

/// The names the engine's helper protects inside every writable root.
const PROTECTED_NAMES: [&str; 3] = [".git", ".agents", ".codex"];

/// Remove the placeholders a run left behind: only paths that did not exist before it and are
/// now an empty directory or an empty file (anything with content is real and is kept).
pub(crate) fn remove_placeholders(paths: &[PathBuf]) {
    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            // Fails (and keeps it) unless it is empty.
            let _ = std::fs::remove_dir(path);
        } else if meta.is_file() && meta.len() == 0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn missing_protected_names(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .flat_map(|root| PROTECTED_NAMES.iter().map(move |name| root.join(name)))
        .filter(|path| std::fs::symlink_metadata(path).is_err())
        .collect()
}

/// The writable roots of a profile (what the helper protects inside).
fn writable_roots(profile: Profile, workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp"), std::env::temp_dir()];
    if profile == Profile::Build {
        roots.push(workspace.to_path_buf());
        roots.extend(build_writable_roots());
    }
    roots.sort();
    roots.dedup();
    roots
}

/// The program and arguments to actually spawn for `program args` in `cwd`.
///
/// Outside a tool call (a unit test of one tool, helper code) there is no
/// context and the command is returned unchanged: every production call goes
/// through [`Sandboxed`], which sets one.
pub(crate) fn wrap(
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Wrapped, FunctionCallError> {
    let Ok(call) = CALL.try_with(Call::clone) else {
        return Ok(unchanged(program, args));
    };
    let decision = decide(
        &call.tool,
        &call.settings,
        program,
        args,
        cwd,
        &backend(call.settings.helper.as_deref()),
    );
    if let Ok((_, Some(note))) = &decision {
        let mut slot = call
            .note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(note.clone());
        }
    }
    decision.map(|(wrapped, _)| wrapped)
}

fn unchanged(program: &str, args: &[String]) -> Wrapped {
    Wrapped {
        program: program.to_string(),
        args: args.to_vec(),
        sandboxed: false,
        placeholders: Vec::new(),
    }
}

/// Which sandbox works on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Bubblewrap,
    Landlock,
    /// Neither could start; the last error.
    Unavailable(String),
    /// Not Linux: the platform sandbox is not wired yet.
    NotWired,
}

/// Probed once per process and helper: start `true` in each backend in turn.
fn backend(helper: Option<&Path>) -> Backend {
    if !cfg!(target_os = "linux") {
        return Backend::NotWired;
    }
    let Some(helper) = helper else {
        return Backend::Unavailable("the engine's sandbox helper was not found".to_string());
    };
    type Probed = std::collections::HashMap<PathBuf, Backend>;
    static PROBED: OnceLock<std::sync::Mutex<Probed>> = OnceLock::new();
    let mut probed = PROBED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    probed
        .entry(helper.to_path_buf())
        .or_insert_with(|| probe(helper))
        .clone()
}

fn probe(helper: &Path) -> Backend {
    let cwd = std::env::temp_dir();
    let mut last = String::new();
    for (legacy, backend) in [(false, Backend::Bubblewrap), (true, Backend::Landlock)] {
        let args = helper_args(Profile::Analyze, "true", &[], &cwd, &cwd, legacy);
        match std::process::Command::new(helper)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => return backend,
            Ok(output) => {
                last = String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .rfind(|line| !line.trim().is_empty())
                    .unwrap_or("the sandbox helper failed")
                    .trim()
                    .to_string();
            }
            Err(err) => last = err.to_string(),
        }
    }
    Backend::Unavailable(last)
}

/// The decision, separate from the probe and the task-local so every branch is
/// testable on any machine. `Ok` carries the note for the tool's output.
pub(crate) fn decide(
    tool: &str,
    settings: &Settings,
    program: &str,
    args: &[String],
    cwd: &Path,
    backend: &Backend,
) -> Result<(Wrapped, Option<String>), FunctionCallError> {
    let Some(profile) = profile_for(tool) else {
        return Err(FunctionCallError::RespondToModel(
            CheckResult::not_run(
                tool,
                NotRunReason::SandboxUnavailable,
                "This tool has no sandbox profile, so it may not start programs.",
                "report this to ByteAsk: every tool that runs programs needs a sandbox profile.",
            )
            .append_to(&format!("{tool} has no sandbox profile; nothing was run.")),
        ));
    };
    if settings.relaxed {
        return Ok((
            unchanged(program, args),
            Some(
                "sandbox: off for this project (you relaxed it with `byteask policy relax`)."
                    .to_string(),
            ),
        ));
    }
    match profile {
        Profile::Debug => {
            return Ok((
                unchanged(program, args),
                Some(
                    "sandbox: not applied — ByteAsk's tool sandbox does not cover debuggers yet, so \
                     the debugger ran with your permissions."
                        .to_string(),
                ),
            ));
        }
        Profile::Kernel => {
            return Ok((
                unchanged(program, args),
                Some(
                    "sandbox: not applied — ByteAsk's tool sandbox does not cover kernel debugging \
                     yet, so gdb ran with your permissions."
                        .to_string(),
                ),
            ));
        }
        Profile::Analyze | Profile::Build => {}
    }
    let legacy = match backend {
        Backend::Bubblewrap => false,
        Backend::Landlock if profile == Profile::Build && !ptrace_limited_to_children() => {
            return no_sandbox(
                tool,
                settings,
                program,
                args,
                "only the landlock fallback is available, and without bubblewrap's PID namespace a \
                 build could trace your other programs (kernel.yama.ptrace_scope is 0)",
            );
        }
        Backend::Landlock => true,
        Backend::NotWired => {
            return Ok((
                unchanged(program, args),
                Some(format!(
                    "sandbox: not applied — ByteAsk's tool sandbox is not available on {} yet, so \
                     this ran with your permissions.",
                    std::env::consts::OS
                )),
            ));
        }
        Backend::Unavailable(why) => {
            return no_sandbox(
                tool,
                settings,
                program,
                args,
                &format!("no sandbox could start: {why}"),
            );
        }
    };
    // The helper takes these as UTF-8 strings and aborts on anything else.
    if cwd.to_str().is_none() || settings.workspace.to_str().is_none() {
        return no_sandbox(
            tool,
            settings,
            program,
            args,
            "the sandbox cannot run in a directory whose path is not valid UTF-8",
        );
    }
    let Some(helper) = settings.helper.as_deref() else {
        return no_sandbox(
            tool,
            settings,
            program,
            args,
            "the sandbox helper was not found",
        );
    };
    // Resolve the program here, outside the sandbox, so a missing compiler is
    // still reported as "not installed" by the caller's own spawn error (the
    // unchanged command fails to spawn; nothing runs unsandboxed).
    let Some(resolved) = resolve_program(program, cwd) else {
        return Ok((unchanged(program, args), None));
    };
    let note = match (profile, legacy) {
        (Profile::Build, false) => {
            "sandbox: on (bubblewrap) — may write only the workspace, temp \
            dirs and build caches; network allowed."
        }
        (Profile::Build, true) => {
            "sandbox: on (landlock) — may write only the workspace, temp \
            dirs and build caches; network allowed."
        }
        (_, false) => "sandbox: on (bubblewrap) — read-only except temp dirs; no network.",
        (_, true) => "sandbox: on (landlock) — read-only except temp dirs; no network.",
    };
    Ok((
        Wrapped {
            program: helper.display().to_string(),
            args: helper_args(
                profile,
                &resolved.display().to_string(),
                args,
                cwd,
                &settings.workspace,
                legacy,
            ),
            sandboxed: true,
            placeholders: missing_protected_names(&writable_roots(profile, &settings.workspace)),
        },
        Some(note.to_string()),
    ))
}

/// No usable sandbox here (`why`): run without one and say so in one line of the answer, unless
/// the local policy requires the sandbox; then refuse with NOT RUN (sandbox unavailable).
fn no_sandbox(
    tool: &str,
    settings: &Settings,
    program: &str,
    args: &[String],
    why: &str,
) -> Result<(Wrapped, Option<String>), FunctionCallError> {
    if settings.required {
        return Err(FunctionCallError::RespondToModel(
            sandbox_unavailable(tool, &settings.workspace).append_to(&format!(
                "No usable sandbox here ({why}), and your ByteAsk policy requires one \
                 ([sandbox] required = true in policy.toml); nothing was run."
            )),
        ));
    }
    Ok((
        unchanged(program, args),
        Some(format!(
            "sandbox: unavailable on this machine ({why}), so this ran without ByteAsk's tool sandbox."
        )),
    ))
}

/// The text of a refusal, for spawn sites that report errors as strings.
pub(crate) fn error_text(err: FunctionCallError) -> String {
    match err {
        FunctionCallError::RespondToModel(text) | FunctionCallError::Fatal(text) => text,
    }
}

/// `Err` with the NOT RUN (sandbox unavailable) result when `tool`'s programs could not start
/// in its sandbox here; for engine-driven work (the finish gate) that must say so up front
/// instead of failing step by step.
pub(crate) fn check_available(tool: &str, settings: &Settings) -> Result<(), CheckResult> {
    match decide(
        tool,
        settings,
        "true",
        &[],
        &settings.workspace,
        &backend(settings.helper.as_deref()),
    ) {
        Ok(_) => Ok(()),
        Err(err) => {
            let text = error_text(err);
            Err(CheckResult::from_tool_text(tool, &text)
                .unwrap_or_else(|| sandbox_unavailable(tool, &settings.workspace)))
        }
    }
}

/// NOT RUN (sandbox unavailable) naming both ways forward.
pub(crate) fn sandbox_unavailable(tool: &str, workspace: &Path) -> CheckResult {
    CheckResult::not_run(
        tool,
        NotRunReason::SandboxUnavailable,
        "No sandbox could start on this machine, and this tool never runs programs unsandboxed.",
        &format!(
            "allow unprivileged user namespaces (on Ubuntu: sudo sysctl -w \
             kernel.apparmor_restrict_unprivileged_userns=0), or run `byteask policy relax {}` \
             yourself in a terminal to run this project's tools without the sandbox.",
            workspace.display()
        ),
    )
}

/// `program` as an absolute path, the way `execvp` would find it when run in
/// `cwd`: a name with a slash is relative to `cwd`, a bare name is searched
/// on `PATH`.
fn resolve_program(program: &str, cwd: &Path) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return Some(cwd.join(candidate));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Yama limits `ptrace` to a process's own descendants (or stricter).
fn ptrace_limited_to_children() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(|scope| scope >= 1)
}

/// Directories a `Build` profile may write besides the workspace and temp
/// dirs: ByteAsk's kept build trees and the user's compiler cache, which
/// a project's CMake commonly puts in front of every compile.
fn build_writable_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    crate::builds::state_dir()
        .into_iter()
        .chain(std::env::var_os("CCACHE_DIR").map(PathBuf::from))
        .chain(
            home.iter()
                .flat_map(|home| [home.join(".ccache"), home.join(".cache").join("ccache")]),
        )
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .collect()
}

fn permission_profile(
    profile: Profile,
    workspace: &Path,
) -> codex_protocol::models::PermissionProfile {
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSandboxPolicy;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use codex_utils_absolute_path::AbsolutePathBuf;

    match profile {
        Profile::Build => {
            let extra: Vec<AbsolutePathBuf> = build_writable_roots()
                .iter()
                .filter_map(|dir| AbsolutePathBuf::from_absolute_path(dir).ok())
                .collect();
            let roots: Vec<AbsolutePathBuf> = AbsolutePathBuf::from_absolute_path(workspace)
                .into_iter()
                .collect();
            PermissionProfile::workspace_write_with(
                &extra,
                NetworkSandboxPolicy::Enabled,
                /*exclude_tmpdir_env_var*/ false,
                /*exclude_slash_tmp*/ false,
            )
            .materialize_project_roots_with_workspace_roots(&roots)
        }
        Profile::Analyze | Profile::Debug | Profile::Kernel => {
            let entry = |value, access| {
                FileSystemSandboxEntry::new(FileSystemPath::Special { value }, access)
            };
            let policy = FileSystemSandboxPolicy::restricted(vec![
                entry(FileSystemSpecialPath::Root, FileSystemAccessMode::Read),
                entry(FileSystemSpecialPath::SlashTmp, FileSystemAccessMode::Write),
                entry(FileSystemSpecialPath::Tmpdir, FileSystemAccessMode::Write),
            ]);
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted)
        }
    }
}

fn helper_args(
    profile: Profile,
    program: &str,
    args: &[String],
    cwd: &Path,
    workspace: &Path,
    legacy_landlock: bool,
) -> Vec<String> {
    let mut command = vec![program.to_string()];
    command.extend(args.iter().cloned());
    codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
        command,
        cwd,
        &permission_profile(profile, workspace),
        workspace,
        legacy_landlock,
        /*managed_network*/ None,
    )
}

/// Turn bubblewrap's "exit 128+N" for a program killed by signal N back into
/// "no exit code" (killed by a signal), so a crash inside the sandbox still
/// reads as a crash. Only for sandboxed runs: a program that exits 139 on its
/// own is indistinguishable here, which shells accept too.
pub(crate) fn signal_exit(sandboxed: bool, code: Option<i32>) -> Option<i32> {
    match code {
        Some(code) if sandboxed && (129..=128 + 64).contains(&code) => None,
        other => other,
    }
}

/// A tool whose every program runs under its sandbox profile. Everything else
/// is delegated unchanged, so the spec the model sees is byte-identical.
pub(crate) struct Sandboxed {
    inner: Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>,
    settings: Arc<Settings>,
}

impl Sandboxed {
    pub(crate) fn wrap(
        inner: Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>,
        settings: Arc<Settings>,
    ) -> Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>> {
        Arc::new(Self { inner, settings })
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for Sandboxed {
    fn tool_name(&self) -> ToolName {
        self.inner.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn exposure(&self) -> ToolExposure {
        self.inner.exposure()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.inner.search_info()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.inner.supports_parallel_tool_calls()
    }

    fn manages_turn_items(&self) -> bool {
        self.inner.manages_turn_items()
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        let context = Call {
            tool: self.inner.tool_name().name,
            settings: self.settings.clone(),
            note: Arc::default(),
        };
        Box::pin(CALL.scope(context, self.inner.handle(call)))
    }
}

/// Run `future` as `tool` under `settings`: work the engine starts itself rather than
/// through a tool call, such as the finish gate's build and test run.
pub(crate) async fn scope<F: std::future::Future>(
    tool: &str,
    settings: Arc<Settings>,
    future: F,
) -> F::Output {
    let context = Call {
        tool: tool.to_string(),
        settings,
        note: Arc::default(),
    };
    CALL.scope(context, future).await
}

/// `future` with the current call's sandbox context, for work a tool hands to a spawned task
/// (a task does not inherit task-locals, so its programs would otherwise run unsandboxed).
pub(crate) fn inherit<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let call = CALL.try_with(Call::clone).ok();
    async move {
        match call {
            Some(call) => CALL.scope(call, future).await,
            None => future.await,
        }
    }
}

/// Run `future` as tool `tool` with `settings`, as [`Sandboxed`] does; for
/// tests that drive the spawn layer directly.
#[cfg(test)]
pub(crate) async fn scope_for_test<F: std::future::Future>(
    tool: &str,
    settings: Settings,
    future: F,
) -> F::Output {
    scope(tool, Arc::new(settings), future).await
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
