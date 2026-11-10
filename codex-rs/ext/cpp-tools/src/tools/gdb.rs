use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;

use crate::gdb_common::Backend;
use crate::gdb_common::INFERIOR_SHELL;
use crate::gdb_common::choose_backend_here;
use crate::gdb_common::inferior_args;
use crate::run::RunOutcome;
use crate::run::run_capped_env;
use crate::run::timeout_from;

const TOOL_NAME: &str = "debug";
/// Cap on the captured debugger output handed back to the model.
const OUTPUT_CAP: usize = 8000;
/// gdb commands run when the caller does not supply their own.
const DEFAULT_COMMANDS: [&str; 3] = ["bt full", "info locals", "info args"];
/// lldb equivalents of [`DEFAULT_COMMANDS`] for the `backend = "lldb"` path.
const DEFAULT_LLDB_COMMANDS: [&str; 3] = ["bt", "frame variable", "frame variable"];

pub struct GdbTool {
    pub cwd: PathBuf,
    /// gdb runs the program (and lets the model run arbitrary gdb
    /// commands) outside the filesystem sandbox. See [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GdbArgs {
    program: String,
    #[serde(default)]
    core: Option<String>,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    commands: Option<Vec<String>>,
    #[serde(default)]
    backend: Option<Backend>,
    #[serde(default)]
    sysroot: Option<String>,
}

/// Default commands for the selected backend.
fn default_commands(backend: Backend) -> Vec<String> {
    match backend {
        Backend::Gdb => DEFAULT_COMMANDS.iter().map(|c| c.to_string()).collect(),
        Backend::Lldb => DEFAULT_LLDB_COMMANDS
            .iter()
            .map(|c| c.to_string())
            .collect(),
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for GdbTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "program".to_string(),
            JsonSchema::string(Some(
                "Path to the executable to debug, relative to the workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "core".to_string(),
            JsonSchema::string(Some(
                "Optional core dump path for post-mortem analysis (relative or absolute)."
                    .to_string(),
            )),
        );
        properties.insert(
            "args".to_string(),
            JsonSchema::string(Some(
                "Optional program arguments for a live run, written like a shell command line: \
                 space-separated, quote with '…' or \"…\" to keep spaces, \"\" is an empty \
                 argument. No shell runs: pipes, redirection and $VAR are rejected."
                    .to_string(),
            )),
        );
        properties.insert(
            "commands".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(format!(
                    "Optional debugger commands to run (gdb default: {}; lldb default: {}). A \
                     live program is run before them unless they include their own `run`/`start` \
                     — put breakpoints first, then `run`.",
                    DEFAULT_COMMANDS.join(", "),
                    DEFAULT_LLDB_COMMANDS.join(", ")
                )),
            ),
        );
        properties.insert(
            "backend".to_string(),
            JsonSchema::string_enum(
                vec![serde_json::json!("gdb"), serde_json::json!("lldb")],
                Some(
                    "Debugger backend: \"gdb\" or \"lldb\". Default: lldb on macOS (gdb does \
                     not run on Apple Silicon Macs), otherwise gdb where it is installed, else \
                     lldb; the result says when lldb was chosen. `commands` must be in the \
                     chosen debugger's syntax."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "sysroot".to_string(),
            JsonSchema::string(Some(
                "Optional, for a core dump from ANOTHER machine (e.g. a Linux production core \
                 read on a Mac or a different box): a directory holding copies of that \
                 machine's files at their original absolute paths (sysroot/usr/lib/…, \
                 sysroot/opt/app/lib/…). Shared libraries are resolved from it, so library \
                 frames get names and the stack unwinds through them: gdb uses `set sysroot`; \
                 with lldb every library the core mapped is loaded from it at the address the \
                 core recorded. The result says which libraries were not found."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description:
                "Run a debugger in batch mode for crash/core-dump post-mortem (backtrace, \
                locals, args) — non-interactive and hardened (no inferior calls, no auto-load). \
                Uses a gdb or lldb backend (lldb by default on macOS, where gdb does not run). \
                C/C++ only; for a core dump pass `core` (plus `sysroot` when the core comes from \
                another machine), for a live crash pass `program`+`args`."
                    .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["program".to_string()]),
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl GdbTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: GdbArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        self.gate()?;

        let (backend, chosen_because) =
            choose_backend_here(args.backend).map_err(FunctionCallError::RespondToModel)?;
        let program_label = args.program.clone();
        let program = resolve_path(&self.cwd, &args.program);
        let core = args.core.as_deref().map(|c| resolve_path(&self.cwd, c));
        let sysroot = args.sysroot.as_deref().map(|s| resolve_path(&self.cwd, s));
        let prog_args = crate::program_args::parse("args", args.args.as_deref().unwrap_or(""))
            .map_err(FunctionCallError::RespondToModel)?;
        let mut commands = args
            .commands
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| default_commands(backend));
        commands = vet_commands(backend, commands).map_err(FunctionCallError::RespondToModel)?;

        let mut notes: Vec<String> = chosen_because
            .map(|why| format!("note: {why}"))
            .into_iter()
            .collect();
        let mut sysroot_setup: Vec<String> = Vec::new();
        if let Some(root) = &sysroot {
            if !root.is_dir() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "sysroot {} is not a directory",
                    root.display()
                )));
            }
            match (backend, core.as_deref()) {
                (Backend::Gdb, _) => {
                    // `-iex`: before the program and core load, or the core's
                    // libraries are already resolved against this machine.
                    sysroot_setup.push("-iex".to_string());
                    sysroot_setup.push(format!("set sysroot {}", root.display()));
                    let debug = root.join("usr/lib/debug");
                    if debug.is_dir() {
                        sysroot_setup.push("-iex".to_string());
                        sysroot_setup.push(format!("set debug-file-directory {}", debug.display()));
                    }
                }
                (Backend::Lldb, Some(core_path)) => {
                    let plan = crate::corefile::sysroot_plan(core_path, root, Some(&program))
                        .map_err(FunctionCallError::RespondToModel)?;
                    notes.push(format!("note: {}", plan.describe(root)));
                    let mut with_modules = crate::corefile::lldb_load_commands(&plan);
                    with_modules.append(&mut commands);
                    commands = with_modules;
                }
                (Backend::Lldb, None) => notes.push(
                    "note: `sysroot` applies to a core dump; this live run ignored it".to_string(),
                ),
            }
        }

        let mut envs = Vec::new();
        let (bin, debugger_args) = match backend {
            Backend::Gdb => {
                let inferior =
                    inferior_args(&prog_args).map_err(FunctionCallError::RespondToModel)?;
                if inferior.needs_shell {
                    envs.push((INFERIOR_SHELL.0.to_string(), INFERIOR_SHELL.1.to_string()));
                }
                let mut argv = sysroot_setup;
                argv.extend(build_gdb_args(
                    &program,
                    core.as_deref(),
                    &inferior.commands,
                    &commands,
                ));
                ("gdb", argv)
            }
            Backend::Lldb => {
                if let Some(reason) = lldb_cannot_pass(&prog_args) {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "lldb cannot pass this argument list exactly: {reason}. Use \
                         backend=\"gdb\", or cpp_sanitize, which pass it exactly."
                    )));
                }
                (
                    "lldb",
                    build_lldb_args(&program, core.as_deref(), &prog_args, &commands),
                )
            }
        };

        // Batch debugging *runs the program*. With a raw `.output()`, a
        // program that never exits (or a gdb waiting on a prompt we did not
        // anticipate) would wedge the turn with no cap at all.
        let mut result =
            check_result::CheckResult::pass(TOOL_NAME, "The debugger ran the commands.");
        let combined = match run_capped_env(
            bin,
            &debugger_args,
            &self.cwd,
            timeout_from(None),
            &envs,
        )
        .await?
        {
            RunOutcome::TimedOut { secs } => {
                result = crate::results::timed_out(
                    TOOL_NAME,
                    "The debugger",
                    secs,
                    "give it a smaller reproducer, or pass `commands` that end the session (for example a final `quit`).",
                );
                format!(
                    "{bin} timed out after {secs}s and its whole process group was killed. The \
                     program under debug may be waiting for input or looping; give it a smaller \
                     reproducer, or pass explicit `commands` that terminate."
                )
            }
            RunOutcome::Finished {
                stdout,
                stderr,
                truncated,
                success,
                code,
            } => {
                let mut combined = String::new();
                for note in &notes {
                    combined.push_str(note);
                    combined.push('\n');
                }
                if let Some(note) = crate::run::crash_note(bin, success, code) {
                    combined.push_str(&note);
                    combined.push('\n');
                    result = check_result::CheckResult::approximate(
                        TOOL_NAME,
                        "The debugger itself crashed, so its output may be incomplete.",
                    );
                }
                if truncated {
                    result = check_result::CheckResult::approximate(
                        TOOL_NAME,
                        "The debugger's output was cut, so what is shown is incomplete.",
                    );
                    combined.push_str(
                        "note: debugger output exceeded the capture ceiling and was cut; what \
                         follows is incomplete.\n",
                    );
                }
                combined.push_str(&stdout);
                if !stderr.trim().is_empty() {
                    if !combined.is_empty() && !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push_str(&stderr);
                }
                combined
            }
        };

        if combined.len() > OUTPUT_CAP && result.is_pass() {
            result = check_result::CheckResult::approximate(
                TOOL_NAME,
                "The debugger's output was longer than this tool shows, so it was cut.",
            );
        }
        Ok(crate::report::report(
            TOOL_NAME,
            &program_label,
            truncate(combined, OUTPUT_CAP),
            result,
        ))
    }

    /// The trust gate. See [`crate::trust`].
    fn gate(&self) -> Result<(), FunctionCallError> {
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Check every command before anything runs; one refusal refuses the run.
/// gdb gets `debug_session`'s check; lldb the same allowlist, applied from
/// Rust (see [`crate::lldb_guard`]), with expressions rewritten so they cannot
/// call into the program. Measured before this: `shell`, `she`, `platform
/// shell` and `pla sh` ran on the host, and lldb's `p side()` called `side`.
fn vet_commands(backend: Backend, commands: Vec<String>) -> Result<Vec<String>, String> {
    commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            let checked = if command.contains(['\n', '\r']) {
                Err(
                    "it contains a line break, which makes it several commands; put each in its \
                     own `commands` entry"
                        .to_string(),
                )
            } else {
                match backend {
                    Backend::Gdb => match crate::tools::gdb_session::blocked_reason(command.trim())
                    {
                        Some(reason) => Err(reason),
                        None => Ok(command.clone()),
                    },
                    Backend::Lldb => crate::lldb_guard::check(&command),
                }
            };
            checked.map_err(|reason| {
                format!(
                    "Refused commands[{index}] `{command}`: {reason}. Nothing was run: this \
                     debugger blocks host-side escapes and calls into the program."
                )
            })
        })
        .collect()
}

/// Measured on lldb 14, `-- prog args…` passes arguments exactly EXCEPT these,
/// which it would silently change (or, for a line break, fail to launch).
fn lldb_cannot_pass(args: &[String]) -> Option<&'static str> {
    if args.iter().any(String::is_empty) {
        return Some("it drops empty arguments");
    }
    if args.iter().any(|arg| arg.contains(['\n', '\r'])) {
        return Some("a line break inside an argument breaks its launch command");
    }
    if args
        .iter()
        .any(|arg| arg.trim() != arg && !arg.trim().is_empty())
    {
        return Some("it trims leading and trailing whitespace from each argument");
    }
    None
}

/// Does this debugger command start the program? A caller who writes their own
/// `run` (a common call shape: `["run", "bt 25"]`)
/// must not get ours as well — the program would run twice, its side effects
/// twice, and the backtrace would come from the second run, which for a flaky
/// crash may not crash at all. Checked for gdb and lldb spellings alike.
fn starts_program(command: &str) -> bool {
    let command = command.trim();
    let first = command.split_whitespace().next().unwrap_or("");
    matches!(first, "r" | "ru" | "run" | "start" | "starti")
        || command.starts_with("process launch")
}

/// Build the hardened, non-interactive gdb argv.
///
/// Layout:
/// - base flags + hardening `-ex` settings, then
/// - for a core dump: user `-ex <cmd>` per command, then positional
///   `<program> <core>` (already stopped — no `run`);
/// - for a live program: the `set args` setup from
///   [`crate::gdb_common::inferior_args`], then `-ex run` so it runs to the
///   crash/exit (unless the caller runs it themselves), then user `-ex <cmd>`
///   per command, then the positional `<program>`. Not `--args`: see
///   `inferior_args` for why that mangles arguments.
fn build_gdb_args(
    program: &Path,
    core: Option<&Path>,
    setup: &[String],
    commands: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-q".to_string(),
        "-nx".to_string(),
        "-batch".to_string(),
        "-ex".to_string(),
        "set may-call-functions off".to_string(),
        "-ex".to_string(),
        "set auto-load off".to_string(),
        "-ex".to_string(),
        "set startup-with-shell off".to_string(),
    ];

    match core {
        Some(core_path) => {
            for cmd in commands {
                argv.push("-ex".to_string());
                argv.push(cmd.clone());
            }
            argv.push(program.to_string_lossy().into_owned());
            argv.push(core_path.to_string_lossy().into_owned());
        }
        None => {
            for cmd in setup {
                argv.push("-ex".to_string());
                argv.push(cmd.clone());
            }
            if !commands.iter().any(|cmd| starts_program(cmd)) {
                argv.push("-ex".to_string());
                argv.push("run".to_string());
            }
            for cmd in commands {
                argv.push("-ex".to_string());
                argv.push(cmd.clone());
            }
            argv.push(program.to_string_lossy().into_owned());
        }
    }

    argv
}

/// Build the lldb batch argv for the `backend = "lldb"` path. Mirrors
/// [`build_gdb_args`]:
/// - core dump: `--core <core> <program>` (already stopped — no run),
///   then the user `-o <cmd>` per command;
/// - live program: `-o run` first so it runs to the crash/exit, then the user
///   `-o <cmd>` per command, then `-- <program> <args...>`.
///
/// CRITICAL: in `-b` (batch) mode an inferior crash drops lldb to an interactive
/// prompt and HANGS with no TTY. We ALWAYS append `-k "bt" -k "quit"` (the `-k`
/// commands run on crash) so a fault prints a backtrace and exits instead.
///
/// `-x` skips `~/.lldbinit`, as gdb's `-nx` skips `.gdbinit`: an alias defined
/// there would mean something other than what [`crate::lldb_guard`] checked.
fn build_lldb_args(
    program: &Path,
    core: Option<&Path>,
    prog_args: &[String],
    commands: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["-b".to_string(), "-x".to_string()];

    match core {
        Some(core_path) => {
            // `--core` and the positional program are argv elements: an
            // interpolated `target create -c {core} {program}` command split a
            // path with a space ("Cannot open '…/core'").
            argv.push("--core".to_string());
            argv.push(core_path.to_string_lossy().into_owned());
            argv.push(program.to_string_lossy().into_owned());
            for cmd in commands {
                argv.push("-o".to_string());
                argv.push(cmd.clone());
            }
            push_crash_guard(&mut argv, &[]);
        }
        None => {
            if !commands.iter().any(|cmd| starts_program(cmd)) {
                argv.push("-o".to_string());
                argv.push("run".to_string());
            }
            for cmd in commands {
                argv.push("-o".to_string());
                argv.push(cmd.clone());
            }
            // lldb stops executing `-o` commands once the program crashes and
            // runs only `-k` ones: the post-mortem commands — those after the
            // run, the defaults included — were silently dropped for exactly
            // the crash this tool exists to examine. They go in both lists; a
            // command before the caller's own `run` never does, or the crash
            // handler would start the program again.
            let post_mortem = match commands.iter().rposition(|cmd| starts_program(cmd)) {
                Some(run) => &commands[run + 1..],
                None => commands,
            };
            push_crash_guard(&mut argv, post_mortem);
            argv.push("--".to_string());
            argv.push(program.to_string_lossy().into_owned());
            argv.extend(prog_args.iter().cloned());
        }
    }

    argv
}

/// Append lldb's mandatory batch-mode crash guard: on an inferior crash, run
/// the post-mortem commands (a backtrace when there are none) then quit, so the
/// run never hangs at an interactive prompt.
fn push_crash_guard(argv: &mut Vec<String>, post_mortem: &[String]) {
    if post_mortem.is_empty() {
        argv.push("-k".to_string());
        argv.push("bt".to_string());
    }
    for cmd in post_mortem {
        argv.push("-k".to_string());
        argv.push(cmd.clone());
    }
    argv.push("-k".to_string());
    argv.push("quit".to_string());
}

fn truncate(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    // Truncate on a char boundary at or below `cap`.
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... [output truncated]");
    text
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn a_misnamed_field_is_an_error_not_silently_ignored() {
        // A field outside the schema used to be dropped without a word.
        let err = serde_json::from_str::<GdbArgs>(r#"{"program":"a.out","run_args":"x"}"#)
            .err()
            .expect("unknown field refused");
        assert!(
            err.to_string().contains("unknown field `run_args`"),
            "{err}"
        );
    }

    use super::*;

    fn base_prefix() -> Vec<String> {
        vec![
            "-q".to_string(),
            "-nx".to_string(),
            "-batch".to_string(),
            "-ex".to_string(),
            "set may-call-functions off".to_string(),
            "-ex".to_string(),
            "set auto-load off".to_string(),
            "-ex".to_string(),
            "set startup-with-shell off".to_string(),
        ]
    }

    #[test]
    fn builds_core_dump_argv() {
        let program = PathBuf::from("/work/a.out");
        let core = PathBuf::from("/work/core.123");
        let commands = vec!["bt full".to_string(), "info locals".to_string()];
        let argv = build_gdb_args(&program, Some(core.as_path()), &[], &commands);

        let mut expected = base_prefix();
        expected.extend([
            "-ex".to_string(),
            "bt full".to_string(),
            "-ex".to_string(),
            "info locals".to_string(),
            "/work/a.out".to_string(),
            "/work/core.123".to_string(),
        ]);
        assert_eq!(argv, expected);
        // A core dump is already stopped: no `run`.
        assert!(!argv.contains(&"run".to_string()));
    }

    #[test]
    fn builds_live_program_argv_with_run_before_commands() {
        let program = PathBuf::from("/work/a.out");
        let prog_args = vec!["--flag".to_string(), "value".to_string()];
        let setup = inferior_args(&prog_args).expect("simple args").commands;
        let commands = vec!["bt full".to_string()];
        let argv = build_gdb_args(&program, None, &setup, &commands);

        let mut expected = base_prefix();
        expected.extend([
            "-ex".to_string(),
            "set args --flag value".to_string(),
            "-ex".to_string(),
            "run".to_string(),
            "-ex".to_string(),
            "bt full".to_string(),
            "/work/a.out".to_string(),
        ]);
        assert_eq!(argv, expected);

        // `run` must precede the user command so the program runs to its crash.
        let run_idx = argv.iter().position(|a| a == "run").unwrap();
        let cmd_idx = argv.iter().position(|a| a == "bt full").unwrap();
        assert!(run_idx < cmd_idx);
    }

    #[test]
    fn a_caller_run_replaces_ours_instead_of_running_twice() {
        // A common call shape: the caller passes its own `run`.
        let program = PathBuf::from("/work/a.out");
        for commands in [
            vec!["run".to_string(), "bt 25".to_string()],
            vec![
                "set pagination off".to_string(),
                "run".to_string(),
                "bt full".to_string(),
            ],
            vec![
                "break parse".to_string(),
                "r".to_string(),
                "info locals".to_string(),
            ],
            vec!["start".to_string(), "next".to_string()],
        ] {
            let gdb = build_gdb_args(&program, None, &[], &commands);
            let runs = gdb.iter().filter(|arg| starts_program(arg)).count();
            assert_eq!(runs, 1, "gdb {commands:?}: {gdb:?}");
            let lldb = build_lldb_args(&program, None, &[], &commands);
            let runs = lldb.iter().filter(|arg| starts_program(arg)).count();
            assert_eq!(runs, 1, "lldb {commands:?}: {lldb:?}");
        }
        // Without one, ours still comes first so the default post-mortem works.
        let argv = build_gdb_args(&program, None, &[], &["bt full".to_string()]);
        let run_idx = argv.iter().position(|a| a == "run").expect("implicit run");
        assert!(run_idx < argv.iter().position(|a| a == "bt full").unwrap());
        assert!(starts_program("process launch -- x"));
        assert!(!starts_program("bt"));
        assert!(!starts_program("return"));
    }

    /// Run the real gdb exactly as `handle_call` would for a live program.
    async fn run_real_gdb(program: &Path, cwd: &Path, args: &[String], commands: &[String]) {
        let inferior = inferior_args(args).expect("gdb can pass these");
        let envs: Vec<(String, String)> = if inferior.needs_shell {
            vec![(INFERIOR_SHELL.0.to_string(), INFERIOR_SHELL.1.to_string())]
        } else {
            Vec::new()
        };
        let argv = build_gdb_args(program, None, &inferior.commands, commands);
        run_capped_env("gdb", &argv, cwd, timeout_from(Some(60)), &envs)
            .await
            .expect("gdb ran");
    }

    #[tokio::test]
    async fn real_gdb_passes_every_argument_byte_for_byte() {
        use crate::program_args::fixture;
        if !fixture::have("gdb") {
            eprintln!("skip: gdb not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let cases = fixture::hard_cases();
        for args in &cases {
            run_real_gdb(&program, dir.path(), args, &["bt".to_string()]).await;
        }
        let runs = fixture::runs(dir.path());
        assert_eq!(runs.len(), cases.len(), "{runs:?}");
        for (run, want) in runs.iter().zip(&cases) {
            assert_eq!(&run.args, want);
            // The quoted-shell path must not leak its /bin/sh into the program.
            assert_eq!(run.shell, std::env::var("SHELL").ok(), "{want:?}");
        }
    }

    #[tokio::test]
    async fn real_gdb_runs_the_program_once_when_the_caller_writes_run() {
        use crate::program_args::fixture;
        if !fixture::have("gdb") {
            eprintln!("skip: gdb not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let args = vec!["once".to_string()];
        run_real_gdb(
            &program,
            dir.path(),
            &args,
            &["run".to_string(), "bt 25".to_string()],
        )
        .await;
        assert_eq!(
            fixture::runs(dir.path()).len(),
            1,
            "the program must run exactly once"
        );
        // And with no caller `run`, ours still runs it (the post-mortem default).
        run_real_gdb(&program, dir.path(), &args, &default_commands(Backend::Gdb)).await;
        assert_eq!(fixture::runs(dir.path()).len(), 2);
    }

    #[tokio::test]
    async fn real_lldb_passes_the_arguments_it_accepts() {
        use crate::program_args::fixture;
        if !fixture::have("lldb") {
            eprintln!("skip: lldb not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let args: Vec<String> = ["a b", "x\"y", "back\\slash", "$HOME", "*", "-v", "mid  dle"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert!(lldb_cannot_pass(&args).is_none());
        let argv = build_lldb_args(&program, None, &args, &["run".to_string()]);
        run_capped_env("lldb", &argv, dir.path(), timeout_from(Some(60)), &[])
            .await
            .expect("lldb ran");
        let runs = fixture::runs(dir.path());
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].args, args);
    }

    #[test]
    fn lldb_refuses_only_the_arguments_it_would_change() {
        let refused = |args: &[&str]| {
            lldb_cannot_pass(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };
        assert!(refused(&[""]).is_some());
        assert!(refused(&["x", " lead"]).is_some());
        assert!(refused(&["trail\t"]).is_some());
        assert!(refused(&["a\nb"]).is_some());
        // Measured exact on lldb 14.
        assert!(
            refused(&[
                "a b",
                "x\"y",
                "back\\slash",
                "$HOME",
                "*",
                "-v",
                " ",
                "mid  dle"
            ])
            .is_none()
        );
    }

    #[test]
    fn args_setup_precedes_run_and_the_program_is_positional() {
        let program = PathBuf::from("/work/a.out");
        let setup = inferior_args(&["".to_string(), "a b".to_string()])
            .expect("shell path")
            .commands;
        let argv = build_gdb_args(&program, None, &setup, &["bt".to_string()]);
        let set_args = argv
            .iter()
            .position(|a| a == "set args '' 'a b'")
            .expect("set args");
        let shell = argv
            .iter()
            .position(|a| a == "set startup-with-shell on")
            .expect("shell on");
        let run = argv.iter().position(|a| a == "run").expect("run");
        assert!(shell < run && set_args < run);
        assert!(!argv.contains(&"--args".to_string()));
        assert_eq!(argv.last(), Some(&"/work/a.out".to_string()));
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "a.out"), PathBuf::from("/work/a.out"));
        assert_eq!(resolve_path(cwd, "/abs/a.out"), PathBuf::from("/abs/a.out"));
    }

    #[test]
    fn default_commands_differ_by_backend() {
        assert_eq!(
            default_commands(Backend::Gdb),
            vec![
                "bt full".to_string(),
                "info locals".to_string(),
                "info args".to_string()
            ]
        );
        assert_eq!(
            default_commands(Backend::Lldb),
            vec![
                "bt".to_string(),
                "frame variable".to_string(),
                "frame variable".to_string()
            ]
        );
    }

    #[test]
    fn builds_lldb_live_argv_with_run_and_crash_guard() {
        let program = PathBuf::from("/work/a.out");
        let prog_args = vec!["--flag".to_string(), "value".to_string()];
        let commands = vec!["bt".to_string(), "frame variable".to_string()];
        let argv = build_lldb_args(&program, None, &prog_args, &commands);

        assert_eq!(
            argv,
            vec![
                "-b".to_string(),
                "-x".to_string(),
                "-o".to_string(),
                "run".to_string(),
                "-o".to_string(),
                "bt".to_string(),
                "-o".to_string(),
                "frame variable".to_string(),
                "-k".to_string(),
                "bt".to_string(),
                "-k".to_string(),
                "frame variable".to_string(),
                "-k".to_string(),
                "quit".to_string(),
                "--".to_string(),
                "/work/a.out".to_string(),
                "--flag".to_string(),
                "value".to_string(),
            ]
        );

        // `run` must precede the user commands so the program runs to its crash.
        let run_idx = argv.iter().position(|a| a == "run").unwrap();
        let cmd_idx = argv.iter().position(|a| a == "frame variable").unwrap();
        assert!(run_idx < cmd_idx);
    }

    #[test]
    fn builds_lldb_core_argv() {
        let program = PathBuf::from("/work/a.out");
        let core = PathBuf::from("/work/core.123");
        let commands = vec!["bt".to_string()];
        let argv = build_lldb_args(&program, Some(core.as_path()), &[], &commands);

        assert_eq!(
            argv,
            vec![
                "-b".to_string(),
                "-x".to_string(),
                "--core".to_string(),
                "/work/core.123".to_string(),
                "/work/a.out".to_string(),
                "-o".to_string(),
                "bt".to_string(),
                "-k".to_string(),
                "bt".to_string(),
                "-k".to_string(),
                "quit".to_string(),
            ]
        );
        // A core dump is already stopped: no `run`.
        assert!(!argv.contains(&"run".to_string()));
    }

    /// A core from another machine: the crash is inside a library that only
    /// the sysroot has. Without `sysroot` the frame is a bare address on both
    /// debuggers (the control); with it, the function is named.
    #[tokio::test]
    async fn a_foreign_core_resolves_its_libraries_through_the_sysroot() {
        use crate::program_args::fixture::have;
        let Some(fixture) = crate::corefile::foreign_core::build() else {
            eprintln!("skip: needs cc and gdb");
            return;
        };
        // Not the sysroot: gdb also looks for a missing library relative to its
        // working directory, which would resolve the control by accident.
        let neutral = tempfile::tempdir().unwrap();
        let tool = GdbTool {
            cwd: neutral.path().to_path_buf(),
            native_exec: true,
        };
        let mut backends = vec!["gdb"];
        if have("lldb") {
            backends.push("lldb");
        }
        for backend in backends {
            let call = |sysroot: bool| {
                let mut args = serde_json::json!({
                    "program": fixture.program,
                    "core": fixture.core,
                    "backend": backend,
                    "commands": ["bt"],
                });
                if sysroot {
                    args["sysroot"] = serde_json::json!(fixture.sysroot);
                }
                crate::test_support::call(&tool, args)
            };
            let without = call(false).await.expect("debug ran");
            assert!(
                !without.contains("queue_find"),
                "{backend} control resolved it anyway:\n{without}"
            );
            let with = call(true).await.expect("debug ran");
            assert!(
                with.contains("queue_find"),
                "{backend} with sysroot:\n{with}"
            );
            if backend == "lldb" {
                assert!(
                    with.contains("sysroot") && with.contains("loaded"),
                    "{with}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_sysroot_that_is_not_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GdbTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let err = crate::test_support::call(
            &tool,
            serde_json::json!({"program": "a.out", "core": "core", "sysroot": "nope"}),
        )
        .await
        .expect_err("refused");
        assert!(format!("{err:?}").contains("not a directory"), "{err:?}");
    }

    #[tokio::test]
    async fn real_lldb_runs_the_post_mortem_commands_after_a_crash() {
        use crate::program_args::fixture::have;
        if !have("lldb") || !have("cc") {
            eprintln!("skip: needs lldb and cc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crash.c");
        let program = dir.path().join("crash");
        std::fs::write(
            &src,
            // Computed, so 424242 appears only if a command printed the local
            // (lldb echoes the crashing source line itself).
            "int main(int argc, char **argv){ volatile int sentinel = argc * 6 * 70707;\n\
             int *p = 0; (void)argv; return *p + sentinel; }\n",
        )
        .unwrap();
        assert!(
            std::process::Command::new("cc")
                .args(["-g", "-O0", "-o"])
                .arg(&program)
                .arg(&src)
                .status()
                .unwrap()
                .success()
        );
        for commands in [
            default_commands(Backend::Lldb),
            vec!["frame variable sentinel".to_string()],
        ] {
            let argv = build_lldb_args(&program, None, &[], &commands);
            let out = match run_capped_env("lldb", &argv, dir.path(), timeout_from(Some(60)), &[])
                .await
            {
                Ok(RunOutcome::Finished { stdout, stderr, .. }) => format!("{stdout}{stderr}"),
                other => panic!("{:?}", other.is_ok()),
            };
            // Before: only `run`, `bt`, `quit` executed after the crash.
            assert!(out.contains("424242"), "{commands:?}:\n{out}");
        }
    }

    #[tokio::test]
    async fn real_lldb_opens_a_core_whose_path_has_a_space() {
        use crate::program_args::fixture::have;
        if !have("lldb") || !have("cc") {
            eprintln!("skip: needs lldb and cc");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crash.c");
        let program = dir.path().join("crash");
        std::fs::write(&src, "int main(void){ int *p = 0; return *p; }\n").unwrap();
        assert!(
            std::process::Command::new("cc")
                .args(["-g", "-o"])
                .arg(&program)
                .arg(&src)
                .status()
                .unwrap()
                .success()
        );
        let cores = dir.path().join("core dir");
        std::fs::create_dir(&cores).unwrap();
        // Dump a core with gdb itself: independent of core_pattern/ulimit.
        let core = cores.join("core");
        let dumped = std::process::Command::new("gdb")
            .args(["-q", "-nx", "-batch", "-ex", "run", "-ex"])
            .arg(format!("generate-core-file {}", core.display()))
            .arg(&program)
            .output();
        if !dumped.is_ok_and(|out| out.status.success()) || !core.exists() {
            eprintln!("skip: could not produce a core");
            return;
        }
        let argv = build_lldb_args(&program, Some(&core), &[], &["bt".to_string()]);
        let out = match run_capped_env("lldb", &argv, dir.path(), timeout_from(Some(60)), &[]).await
        {
            Ok(RunOutcome::Finished { stdout, stderr, .. }) => format!("{stdout}{stderr}"),
            other => panic!("{:?}", other.is_ok()),
        };
        assert!(!out.contains("Cannot open"), "{out}");
        assert!(out.contains("SIGSEGV") && out.contains("main"), "{out}");
    }

    #[test]
    fn lldb_argv_always_appends_crash_guard() {
        // The mandatory hang-guard: a crash in batch mode without a TTY must run
        // `bt` then `quit` rather than dropping to an interactive prompt.
        for core in [None, Some(Path::new("/work/core"))] {
            let argv = build_lldb_args(Path::new("/work/a.out"), core, &[], &["bt".to_string()]);
            let k_positions: Vec<usize> = argv
                .iter()
                .enumerate()
                .filter(|(_, a)| a.as_str() == "-k")
                .map(|(i, _)| i)
                .collect();
            assert_eq!(k_positions.len(), 2, "expected exactly two -k crash hooks");
            assert_eq!(argv[k_positions[0] + 1], "bt");
            assert_eq!(argv[k_positions[1] + 1], "quit");
        }
    }

    #[test]
    fn host_escapes_are_refused_on_both_backends() {
        for (backend, command) in [
            (Backend::Gdb, "shell touch x"),
            (Backend::Gdb, "she touch x"),
            (Backend::Gdb, "python import os"),
            (Backend::Gdb, "set may-call-functions on"),
            (Backend::Gdb, "run $(touch x)"),
            (Backend::Gdb, "set args $(touch x)"),
            (Backend::Gdb, "bt\nshell touch x"),
            (Backend::Lldb, "platform shell touch x"),
            (Backend::Lldb, "pla sh touch x"),
            (Backend::Lldb, "shell touch x"),
            (Backend::Lldb, "script import os"),
            (Backend::Lldb, "command alias zz platform shell"),
            (Backend::Lldb, "command source evil.lldb"),
            (Backend::Lldb, "br s -n main"),
            (
                Backend::Lldb,
                "breakpoint set -n main --comm \"platform shell touch x\"",
            ),
            (Backend::Lldb, "breakpoint set -n main -Hc 1"),
            (Backend::Lldb, "breakpoint command add 1"),
            (
                Backend::Lldb,
                "target stop-hook add -o \"platform shell touch x\"",
            ),
            (Backend::Lldb, "display x"),
            (Backend::Lldb, "settings set prompt x"),
            (Backend::Lldb, "p side()"),
            (Backend::Lldb, "expression --allow true -- x"),
            (Backend::Lldb, "memory read `1+1`"),
            (Backend::Lldb, "run $(touch x)"),
            (Backend::Lldb, "process launch --shell=/bin/sh -- x"),
            (Backend::Lldb, "attach 1"),
            (Backend::Lldb, "bt\nplatform shell touch x"),
        ] {
            let err = vet_commands(backend, vec!["bt".to_string(), command.to_string()])
                .expect_err(command);
            assert!(err.starts_with("Refused commands[1]"), "{command}: {err}");
        }
    }

    #[test]
    fn ordinary_commands_pass_and_lldb_expressions_cannot_call() {
        let gdb: Vec<String> = [
            "bt full",
            "info locals",
            "info args",
            "run",
            "r",
            "start",
            "break parse",
            "print x",
            "x/4x &buf",
            "frame 2",
            "set pagination off",
            "set var x = 5",
            "finish",
        ]
        .iter()
        .map(|c| c.to_string())
        .collect();
        assert_eq!(vet_commands(Backend::Gdb, gdb.clone()), Ok(gdb));
        let lldb: Vec<String> = [
            "bt",
            "bt all",
            "frame variable",
            "frame variable sentinel",
            "v x",
            "run",
            "r",
            "process launch",
            "b main",
            "b t.c:12",
            "breakpoint set -f t.c -l 3 -i 2",
            "image list",
            "target modules lookup -a 0x1000",
            "x/4xw &x",
            "memory read -c 16 $sp",
            "register read",
            "thread backtrace all",
            "thread list",
            "frame select 1",
            "up",
            "down 2",
            "disassemble -n main",
            "c",
            "n",
            "s",
            "finish",
            "settings set target.source-map /a /b",
            "watchpoint set variable -w write x",
        ]
        .iter()
        .map(|c| c.to_string())
        .collect();
        assert_eq!(vet_commands(Backend::Lldb, lldb.clone()), Ok(lldb));
        for (given, runs) in [
            ("p x", "expression --allow-jit false -- x"),
            ("print a + b", "expression --allow-jit false -- a + b"),
            ("call a + b", "expression --allow-jit false -- a + b"),
            ("po obj", "expression -O --allow-jit false -- obj"),
            ("p/x x", "expression --gdb-format x --allow-jit false -- x"),
            (
                "expression -G x -- x",
                "expression -G x --allow-jit false -- x",
            ),
            (
                "expr sizeof(S)",
                "expression --allow-jit false -- sizeof(S)",
            ),
            ("p -x", "expression --allow-jit false -- -x"),
        ] {
            assert_eq!(
                vet_commands(Backend::Lldb, vec![given.to_string()]),
                Ok(vec![runs.to_string()]),
                "{given}"
            );
        }
    }

    /// The whole tool on the real debuggers: a refused command stops the call
    /// before the debugger starts, so neither the escape nor the program runs.
    #[tokio::test]
    async fn an_escape_is_refused_before_the_debugger_starts() {
        use crate::program_args::fixture;
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = fixture::build(dir.path()) else {
            eprintln!("skip: no C compiler");
            return;
        };
        let tool = GdbTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let marker = dir.path().join("pwned");
        let touch = format!("touch {}", marker.display());
        let mut cases = Vec::new();
        if fixture::have("gdb") {
            cases.push(("gdb", "", format!("shell {touch}")));
            // An empty argument makes gdb start the program through /bin/sh.
            cases.push(("gdb", "''", format!("run $({touch})")));
        }
        if fixture::have("lldb") {
            cases.push(("lldb", "", format!("pla sh {touch}")));
            cases.push((
                "lldb",
                "",
                format!("breakpoint set -n main --comm \"platform shell {touch}\""),
            ));
        }
        for (backend, args, escape) in cases {
            let result = crate::test_support::call(
                &tool,
                serde_json::json!({"program": program, "backend": backend, "args": args,
                                   "commands": [escape, "run"]}),
            )
            .await;
            let err = format!("{:?}", result.expect_err("refused"));
            assert!(
                err.contains("Refused commands[0]"),
                "{backend} {escape}: {err}"
            );
            assert!(!marker.exists(), "{backend}: `{escape}` ran on the host");
            assert!(
                fixture::runs(dir.path()).is_empty(),
                "{backend}: the program ran"
            );
        }
    }

    /// lldb calls an overloaded operator for `p a + b` (measured: the operator
    /// ran), and there is no call syntax to refuse; `--allow-jit false` is what
    /// stops it.
    #[tokio::test]
    async fn real_lldb_expressions_cannot_call_into_the_program() {
        use crate::program_args::fixture::have;
        if !have("lldb") || !have("c++") {
            eprintln!("skip: needs lldb and c++");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("called");
        let src = dir.path().join("op.cpp");
        std::fs::write(
            &src,
            format!(
                "#include <cstdio>\nstruct S {{ int v; }};\n\
                 S operator+(S a, S b) {{ if (FILE *f = std::fopen(\"{}\", \"w\")) std::fclose(f); return S{{a.v + b.v}}; }}\n\
                 int main() {{ S a{{1}}, b{{2}}; volatile int stop = a.v + b.v; return stop - 3; }}\n",
                marker.display()
            ),
        )
        .unwrap();
        let program = dir.path().join("op");
        assert!(
            std::process::Command::new("c++")
                .args(["-g", "-O0", "-o"])
                .arg(&program)
                .arg(&src)
                .status()
                .unwrap()
                .success()
        );
        let tool = GdbTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let out = crate::test_support::call(
            &tool,
            serde_json::json!({"program": program, "backend": "lldb",
                               "commands": ["b 4", "run", "p/x a.v", "p a + b"]}),
        )
        .await
        .expect("debug ran");
        assert!(out.contains("stop reason = breakpoint"), "{out}");
        assert!(
            out.contains("expression --allow-jit false -- a + b"),
            "{out}"
        );
        assert!(
            out.contains("(int) $0 = 0x"),
            "a plain read still works:\n{out}"
        );
        assert!(!marker.exists(), "operator+ ran inside the program:\n{out}");
    }

    #[test]
    fn truncate_caps_long_output() {
        let truncated = truncate("x".repeat(10_000), OUTPUT_CAP);
        assert!(truncated.starts_with(&"x".repeat(OUTPUT_CAP)));
        assert!(truncated.ends_with("[output truncated]"));
    }
}
