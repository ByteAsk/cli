use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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
use serde_json::json;

use crate::gdb_common::Backend;
use crate::gdb_common::choose_backend_here;
use crate::gdb_common::first_word;
use crate::gdb_common::resolve_path;
use crate::gdbmi::GdbRegistry;
use crate::gdbmi::GdbSession;
use crate::report::report;

const TOOL_NAME: &str = "debug_session";

pub struct GdbSessionTool {
    pub cwd: PathBuf,
    pub sessions: Arc<GdbRegistry>,
    /// A live gdb the model drives runs the program outside the
    /// filesystem sandbox. See [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Action {
    Start,
    Cmd,
    Stop,
    List,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionArgs {
    action: Action,
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    core: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    backend: Option<Backend>,
    #[serde(default)]
    sysroot: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for GdbSessionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![json!("start"), json!("cmd"), json!("stop"), json!("list")],
                Some(
                    "start: open a session (returns its id); cmd: run one debugger command on a \
                     session (gdb syntax in a gdb session, lldb syntax in an lldb one); stop: end \
                     a session; list: show open sessions."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "session".to_string(),
            JsonSchema::integer(Some(
                "Session id (returned by start). Required for cmd and stop.".to_string(),
            )),
        );
        properties.insert(
            "program".to_string(),
            JsonSchema::string(Some(
                "start: path to the executable to debug, relative to the workspace or absolute."
                    .to_string(),
            )),
        );
        properties.insert(
            "args".to_string(),
            JsonSchema::string(Some(
                "start: optional program arguments, applied via `set args` and written like a shell \
                 command line: \
                 space-separated, quote with '…' or \"…\" to keep spaces, \"\" is an empty \
                 argument. No shell runs: pipes, redirection and $VAR are rejected."
                    .to_string(),
            )),
        );
        properties.insert(
            "core".to_string(),
            JsonSchema::string(Some(
                "start: optional core dump path; the session loads it and reports the crash \
                 backtrace."
                    .to_string(),
            )),
        );
        properties.insert(
            "command".to_string(),
            JsonSchema::string(Some(
                "cmd: the gdb command to run (e.g. \"break main\", \"run\", \"next\", \
                 \"print x\", \"bt\"). In an lldb session, lldb syntax (e.g. \"b main\", \
                 \"run\", \"next\", \"frame variable\", \"p x\", \"bt\", \"memory read -c 16 ptr\")."
                    .to_string(),
            )),
        );
        properties.insert(
            "backend".to_string(),
            JsonSchema::string_enum(
                vec![json!("gdb"), json!("lldb")],
                Some(
                    "start: \"gdb\" or \"lldb\". Default: lldb on macOS (gdb does not run on \
                     Apple Silicon Macs), otherwise gdb where it is installed, else lldb. The \
                     start result names the debugger; send commands in its syntax."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "sysroot".to_string(),
            JsonSchema::string(Some(
                "start, with `core`: for a core dump from ANOTHER machine (e.g. a Linux \
                 production core read on a Mac), a directory holding copies of that machine's \
                 files at their original absolute paths. Shared libraries are resolved from it \
                 so library frames get names and the stack unwinds through them; the start \
                 result says which were not found."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Interactive GDB session for C/C++: a persistent debugger you drive \
                across calls — set breakpoints, run, step, and inspect state to trace WHY code \
                produces a WRONG RESULT or takes a wrong branch (not only crashes) — unlike the \
                one-shot post-mortem debug. `action=\"start\"` (with `program` or `core`) \
                opens a session and returns its id; `action=\"cmd\"` runs one gdb command on \
                that `session` and returns clean console output (execution commands lead with a \
                one-line stop summary, and what the program printed follows); `action=\"stop\"` \
                ends it; `action=\"list\"` shows open sessions. The program's stdin is empty \
                (reads see end-of-file): give it input through a file argument. Host-side \
                escapes (shell/python/source) and inferior function calls are disabled for \
                safety. On macOS (gdb does not run on Apple Silicon) the session is lldb: \
                commands use lldb syntax, the same escapes and function calls are refused, and \
                a program still running after 30s is stopped so it can be inspected. C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["action".to_string()]),
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
        // A session is stateful and single-process: commands must serialize.
        false
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl GdbSessionTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: SessionArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        // Trust gate: `start` spawns gdb on the user's program and `cmd` runs
        // arbitrary gdb commands against it, both outside the sandbox.
        // `stop`/`list` only touch our own registry, so they stay open — a
        // refusal must never strand a session the model already created.
        if matches!(args.action, Action::Start | Action::Cmd) {
            crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)?;
        }

        match args.action {
            Action::Start => self.start(args).await,
            Action::Cmd => self.cmd(args).await,
            Action::Stop => self.stop(args),
            Action::List => Ok(self.list()),
        }
    }

    async fn start(&self, args: SessionArgs) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let id = self.sessions.next_id();
        let program = args.program.as_deref().map(|p| resolve_path(&self.cwd, p));
        let core = args.core.as_deref().map(|p| resolve_path(&self.cwd, p));
        let sysroot = args.sysroot.as_deref().map(|p| resolve_path(&self.cwd, p));
        let prog_args = crate::program_args::parse("args", args.args.as_deref().unwrap_or(""))
            .map_err(FunctionCallError::RespondToModel)?;
        if let Some(root) = &sysroot
            && !root.is_dir()
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "sysroot {} is not a directory",
                root.display()
            )));
        }
        let (backend, chosen_because) =
            choose_backend_here(args.backend).map_err(FunctionCallError::RespondToModel)?;

        let mut notes: Vec<String> = chosen_because
            .map(|why| format!("note: {why}."))
            .into_iter()
            .collect();
        let session = match backend {
            Backend::Gdb => GdbSession::start_with_sysroot(
                id,
                program.as_deref(),
                core.as_deref(),
                &prog_args,
                &self.cwd,
                sysroot.as_deref(),
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?,
            Backend::Lldb => {
                let mut modules = Vec::new();
                if let (Some(root), Some(core_path)) = (&sysroot, &core) {
                    let plan = crate::corefile::sysroot_plan(core_path, root, program.as_deref())
                        .map_err(FunctionCallError::RespondToModel)?;
                    notes.push(format!("note: {}.", plan.describe(root)));
                    modules = plan.modules;
                }
                let (session, note) = crate::gdbmi::start_lldb(
                    id,
                    program.as_deref(),
                    core.as_deref(),
                    &prog_args,
                    &self.cwd,
                    &modules,
                )
                .await
                .map_err(FunctionCallError::RespondToModel)?;
                if !note.is_empty() && modules.is_empty() {
                    notes.push(format!("note: {note}."));
                }
                session
            }
        };

        let label = session.program.clone();
        let name = session.backend;
        let mut body = format!("Started {name} session #{id} on {label}.");
        for note in &notes {
            body.push('\n');
            body.push_str(note);
        }
        if core.is_some() {
            let backtrace = session
                .command(if backend == Backend::Gdb {
                    "backtrace"
                } else {
                    "bt"
                })
                .await;
            body.push_str("\nLoaded core dump — crash backtrace:\n");
            body.push_str(&backtrace);
        } else {
            let example = if backend == Backend::Gdb {
                "break main"
            } else {
                "b main"
            };
            body.push_str(&format!(
                "\nReady. Drive it with action=\"cmd\", session={id} \
                 (e.g. command=\"{example}\", then command=\"run\"). End with action=\"stop\"."
            ));
        }

        self.sessions.insert(session);
        Ok(report(
            TOOL_NAME,
            &label,
            body,
            check_result::CheckResult::pass(TOOL_NAME, "Started a debug session."),
        ))
    }

    async fn cmd(&self, args: SessionArgs) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let id = args.session.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "cmd requires `session` (the id returned by start).".to_string(),
            )
        })?;
        let command = args.command.unwrap_or_default().trim().to_string();
        if command.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "cmd requires a non-empty `command`.".to_string(),
            ));
        }
        let Some(session) = self.sessions.get(id) else {
            return Err(FunctionCallError::RespondToModel(format!(
                "No debug session #{id}. Use action=\"list\" to see open sessions, or \
                 action=\"start\" to open one."
            )));
        };
        // An lldb session checks its commands itself, against lldb's own
        // resolution of them (see byteask_lldb_session.py).
        if session.backend == "gdb"
            && let Some(reason) = blocked_reason(&command)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "Refused: {reason}. This interactive debugger blocks host-side escapes; \
                 inferior function calls are off (`set may-call-functions off`)."
            )));
        }

        let output = session.command(&command).await;
        let target = format!("#{id} {}", first_word(&command));
        Ok(report(
            TOOL_NAME,
            &target,
            output,
            check_result::CheckResult::pass(TOOL_NAME, "The command ran in the debug session."),
        ))
    }

    fn stop(&self, args: SessionArgs) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let id = args.session.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "stop requires `session` (the id returned by start).".to_string(),
            )
        })?;
        let Some(session) = self.sessions.remove(id) else {
            return Err(FunctionCallError::RespondToModel(format!(
                "No debug session #{id} to stop."
            )));
        };
        // Dropping the removed session closes its request channel, which makes
        // the actor exit the debugger cleanly and reap the process.
        let name = session.backend;
        Ok(report(
            TOOL_NAME,
            &format!("#{id}"),
            format!("Stopped {name} session #{id}."),
            check_result::CheckResult::pass(TOOL_NAME, "Stopped the debug session."),
        ))
    }

    fn list(&self) -> Box<dyn ToolOutput> {
        let rows = self.sessions.summaries();
        let body = if rows.is_empty() {
            "No open debug sessions.".to_string()
        } else {
            let mut out = String::from("Open debug sessions:");
            for (id, program, state) in rows {
                out.push_str(&format!("\n  #{id}  {program}  [{state}]"));
            }
            out
        };
        report(
            TOOL_NAME,
            "",
            body,
            check_result::CheckResult::pass(TOOL_NAME, "Listed the open debug sessions."),
        )
    }
}

/// Guardrail for the interactive command channel. Inferior function calls are
/// already refused by GDB itself (`set may-call-functions off` at start), so we
/// only need to block host-side code-execution escapes and attempts to re-enable
/// the hardening. Best-effort — a defense layer, not a sandbox.
///
/// gdb runs ANY unambiguous prefix of a command name — measured on gdb 13,
/// `she`, `pip`, `ev`, `sou` and `interp` all execute `shell`, `pipe`, `eval`,
/// `source` and `interpreter-exec` — and executes every line of a multi-line
/// command. Matching whole first words let all of those through.
///
/// The one-shot `debug` tool applies the same check to each of its `commands`.
pub(crate) fn blocked_reason(command: &str) -> Option<String> {
    if command.contains(['\n', '\r']) {
        return Some(
            "a command containing a line break is several commands: every line would run, and \
             only the first is checked or shown. Send one command per call"
                .to_string(),
        );
    }
    if command.starts_with('!') || command.starts_with('|') {
        return Some("shell escapes (`!`, `|`) are disabled".to_string());
    }
    let mut words = command.split_whitespace();
    let first = words.next().unwrap_or("").to_ascii_lowercase();
    // `first` names a command in `names` if it IS one or is a prefix gdb would
    // resolve to it. One letter is never matched: gdb's one-letter aliases
    // (`s`, `p`, `c`, `n`) are safe commands.
    let names_one_of = |names: &[&str]| {
        first.len() >= 2 && names.iter().any(|name| name.starts_with(first.as_str()))
    };
    const HOST_ESCAPES: [&str; 13] = [
        "shell",
        "python",
        "python-interactive",
        "pipe",
        "eval",
        "source",
        "define",
        "define-prefix",
        "compile",
        "make",
        "guile",
        "interpreter-exec",
        "-interpreter-exec",
    ];
    if names_one_of(&HOST_ESCAPES) || first == "gr" {
        return Some(format!(
            "`{first}` is disabled (it can run code on the host)"
        ));
    }
    // These read their body from the following input lines, which here are
    // the next calls' commands.
    const BLOCKS: [&str; 4] = ["commands", "while", "if", "document"];
    if names_one_of(&BLOCKS) {
        return Some(format!(
            "`{first}` opens a multi-line block, and this channel sends one line per call. For \
             print-at-breakpoint use `dprintf LOCATION,\"format\",args`"
        ));
    }
    // The program's arguments come from the tool's `args`, which passes them
    // exactly. When one of them is empty or holds whitespace, gdb starts the
    // program through /bin/sh (see `inferior_args`), and arguments given here
    // would reach that shell: `run $(touch x)` runs `touch` on the host.
    const ARGS_REASON: &str = "program arguments are set by the tool's `args`, which passes them \
                               exactly; arguments given to a debugger command can reach a shell";
    if matches!(
        first.as_str(),
        "r" | "ru" | "run" | "start" | "starti" | "-exec-arguments"
    ) && (first == "-exec-arguments" || words.clone().next().is_some())
    {
        return Some(ARGS_REASON.to_string());
    }
    // Re-enabling the hardening, by any value (`on`, `1`, `yes`) and any verb
    // that changes a setting (`set`, MI `-gdb-set`, `with`/`w`).
    const GUARDED: [&str; 4] = [
        "may-call-functions",
        "startup-with-shell",
        "auto-load",
        "confirm",
    ];
    let changes_a_setting =
        first == "set" || first == "-gdb-set" || "with".starts_with(first.as_str());
    if changes_a_setting && !first.is_empty() {
        if let Some(setting) = words.next().map(str::to_ascii_lowercase) {
            if let Some(guard) = GUARDED.iter().find(|guard| {
                (setting.len() >= 3 && guard.starts_with(setting.as_str()))
                    || setting.starts_with(*guard)
            }) {
                return Some(format!("changing `{guard}` is disabled"));
            }
            if setting.len() >= 3 && "args".starts_with(setting.as_str()) {
                return Some(ARGS_REASON.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn session_tool(cwd: &std::path::Path) -> GdbSessionTool {
        GdbSessionTool {
            cwd: cwd.to_path_buf(),
            sessions: GdbRegistry::new(),
            native_exec: true,
        }
    }

    async fn call(tool: &GdbSessionTool, args: serde_json::Value) -> String {
        match crate::test_support::call(tool, args).await {
            Ok(text) => text,
            Err(err) => format!("ERR {err:?}"),
        }
    }

    /// A program that prints its first argument, and spins forever when that is
    /// "spin". `None` without a C compiler.
    fn spinner(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        std::fs::write(
            dir.join("spin.c"),
            "#include <stdio.h>\n#include <string.h>\nint add(int a,int b){return a+b;}\n\
             int main(int argc,char**argv){int x=add(41,1);printf(\"x=%d a1=[%s]\\n\",x,argc>1?argv[1]:\"\");\n\
             fflush(stdout);if(argc>1&&strcmp(argv[1],\"spin\")==0){volatile long n=0;for(;;)n++;}return 0;}\n",
        )
        .ok()?;
        let program = dir.join("spin");
        let ok = std::process::Command::new("cc")
            .args(["-g", "-O0", "-o"])
            .arg(&program)
            .arg(dir.join("spin.c"))
            .status()
            .ok()?
            .success();
        ok.then_some(program)
    }

    /// Start an lldb session, or None (skip) where lldb's Python module is not
    /// usable — Debian's lldb 14 package is broken; `BYTEASK_LLDB_PYTHONPATH`
    /// is the workaround.
    async fn start_lldb(tool: &GdbSessionTool, extra: serde_json::Value) -> Option<(String, u64)> {
        if crate::gdbmi::find_on_path("lldb").is_none() {
            eprintln!("skip: no lldb");
            return None;
        }
        let mut args = serde_json::json!({"action": "start", "backend": "lldb"});
        for (k, v) in extra.as_object().expect("object") {
            args[k] = v.clone();
        }
        let out = call(tool, args).await;
        if out.contains("Python module could not be loaded") {
            eprintln!("skip: lldb's Python module is not usable here: {out}");
            return None;
        }
        assert!(out.contains("Started lldb session #"), "{out}");
        let id = tool.sessions.summaries().last().expect("registered").0;
        Some((out, id))
    }

    #[tokio::test]
    async fn an_lldb_session_steps_a_program_and_refuses_host_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = spinner(dir.path()) else {
            return;
        };
        let tool = session_tool(dir.path());
        let Some((_, id)) = start_lldb(
            &tool,
            serde_json::json!({"program": program, "args": "'hello world'"}),
        )
        .await
        else {
            return;
        };
        let cmd = |command: &str| {
            call(
                &tool,
                serde_json::json!({"action": "cmd", "session": id, "command": command}),
            )
        };
        assert!(cmd("b add").await.contains("Breakpoint 1"));
        let run = cmd("run").await;
        assert!(run.contains("stop reason = breakpoint 1"), "{run}");
        assert!(cmd("frame variable").await.contains("(int) a ="));
        let finish = cmd("finish").await;
        assert!(
            finish.contains("Return value: (int)") && finish.contains("= 42"),
            "{finish}"
        );
        let marker = dir.path().join("pwned");
        for escape in [
            format!("platform shell touch {}", marker.display()),
            format!("pla sh touch {}", marker.display()),
            "script import os".to_string(),
            "command alias zz platform shell".to_string(),
            "p (int)getpid()".to_string(),
            format!("memory read (long)system(\"touch {}\")", marker.display()),
            "memory read `1+1`".to_string(),
            "br s -n main -c 1".to_string(),
            "run $(touch x)".to_string(),
            // lldb accepts any unambiguous prefix of a long option, and a short
            // one inside a cluster; each of these passed the check before.
            format!(
                "breakpoint set -n add --comm \"platform shell touch {}\"",
                marker.display()
            ),
            "breakpoint set -n add -Hc 1".to_string(),
            "expression --allow true -- a".to_string(),
        ] {
            let out = cmd(&escape).await;
            assert!(out.contains("Refused:"), "{escape}: {out}");
        }
        assert!(!marker.exists(), "an escape ran on the host");
        let finished = cmd("continue").await;
        assert!(finished.contains("exited with status = 0"), "{finished}");
        assert!(
            finished.contains("a1=[hello world]"),
            "the argument arrived whole: {finished}"
        );
        let stopped = call(&tool, serde_json::json!({"action": "stop", "session": id})).await;
        assert!(stopped.contains("Stopped lldb session"), "{stopped}");
    }

    #[tokio::test]
    async fn an_lldb_session_stops_a_program_that_never_stops() {
        let dir = tempfile::tempdir().unwrap();
        let Some(program) = spinner(dir.path()) else {
            return;
        };
        let tool = session_tool(dir.path());
        let Some((_, id)) = start_lldb(
            &tool,
            serde_json::json!({"program": program, "args": "spin"}),
        )
        .await
        else {
            return;
        };
        let cmd = |command: &str| {
            call(
                &tool,
                serde_json::json!({"action": "cmd", "session": id, "command": command}),
            )
        };
        // Before: `run` never returned, and killing the helper left the program
        // spinning (lldb's Python holds the GIL while `continue` blocks).
        let run = cmd("run").await;
        assert!(
            run.contains("still running") && run.contains("SIGSTOP"),
            "{run}"
        );
        let again = cmd("continue").await;
        assert!(again.contains("still running"), "{again}");
        call(&tool, serde_json::json!({"action": "stop", "session": id})).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let needle = program.to_string_lossy().into_owned();
        let survivors: Vec<String> = std::fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
                    .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
                    .filter(|c| c.starts_with(&needle))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            survivors.is_empty(),
            "the program outlived its session: {survivors:?}"
        );
    }

    #[tokio::test]
    async fn a_foreign_core_opens_through_the_sysroot_in_both_sessions() {
        let Some(fixture) = crate::corefile::foreign_core::build() else {
            eprintln!("skip: needs cc and gdb");
            return;
        };
        let neutral = tempfile::tempdir().unwrap();
        let tool = session_tool(neutral.path());
        let gdb = call(
            &tool,
            serde_json::json!({"action": "start", "backend": "gdb", "program": fixture.program,
                               "core": fixture.core, "sysroot": fixture.sysroot}),
        )
        .await;
        assert!(gdb.contains("queue_find"), "{gdb}");
        if let Some((lldb, _)) = start_lldb(
            &tool,
            serde_json::json!({"program": fixture.program, "core": fixture.core, "sysroot": fixture.sysroot}),
        )
        .await
        {
            assert!(lldb.contains("libqueue.so`queue_find"), "{lldb}");
        }
    }

    #[test]
    fn a_misnamed_field_is_an_error_not_silently_ignored() {
        // A field outside the schema used to be dropped without a word.
        let err = serde_json::from_str::<SessionArgs>(
            r#"{"action":"start","program":"a.out","run_args":"x"}"#,
        )
        .err()
        .expect("unknown field refused");
        assert!(
            err.to_string().contains("unknown field `run_args`"),
            "{err}"
        );
    }

    #[test]
    fn blocks_host_side_escapes() {
        assert!(blocked_reason("shell rm -rf /").is_some());
        assert!(blocked_reason("!touch /tmp/x").is_some());
        assert!(blocked_reason("python import os").is_some());
        assert!(blocked_reason("pi").is_some());
        assert!(blocked_reason("source ~/.evil").is_some());
        assert!(blocked_reason("define hook-stop").is_some());
        assert!(blocked_reason("make").is_some());
        assert!(blocked_reason("-interpreter-exec console \"shell id\"").is_some());
    }

    #[test]
    fn blocks_every_prefix_gdb_resolves_to_an_escape() {
        // Each of these was measured to execute on gdb 13.
        for command in [
            "she touch x",
            "shel touch x",
            "pip echo x | sh",
            "pipe echo x | sh",
            "| echo x | sh",
            "ev \"shell touch x\"",
            "eval \"shell touch x\"",
            "so s.gdb",
            "sour s.gdb",
            "python-i open('x','w')",
            "interpreter-exec console \"shell touch x\"",
            "interp console \"shell touch x\"",
            "mak",
            "compi code 1",
            "gr",
        ] {
            assert!(blocked_reason(command).is_some(), "{command}");
        }
    }

    #[test]
    fn a_line_break_is_refused_wherever_it_is() {
        // Otherwise the second line runs unchecked and its output is never shown.
        for command in [
            "print 1\nshell touch x",
            "print 2\npython open('x','w')",
            "run\nfinish",
            "bt\r",
        ] {
            assert!(blocked_reason(command).is_some(), "{command:?}");
        }
    }

    #[test]
    fn program_arguments_cannot_be_given_past_args() {
        // With an empty or spaced argument gdb starts the program through
        // /bin/sh, where these would run `touch` on the host.
        for command in [
            "run $(touch x)",
            "r `touch x`",
            "start ; touch x",
            "starti x",
            "set args $(touch x)",
            "set arg x",
            "with args $(touch x) -- run",
            "-exec-arguments $(touch x)",
        ] {
            let reason = blocked_reason(command).expect(command);
            assert!(reason.contains("`args`"), "{command}: {reason}");
        }
    }

    #[test]
    fn guards_cannot_be_reenabled_by_any_spelling() {
        for command in [
            "set may-call-functions 1",
            "set may-call on",
            "set startup-with-shell yes",
            "-gdb-set may-call-functions on",
            "with may-call-functions on -- print getpid()",
            "w may-call-functions on -- print getpid()",
            "set auto-load safe-path /",
            "set confirm on",
        ] {
            assert!(blocked_reason(command).is_some(), "{command}");
        }
    }

    #[test]
    fn multi_line_blocks_are_refused_with_a_one_line_alternative() {
        for command in ["commands 1", "comm", "while x < 3", "if x", "document foo"] {
            let reason = blocked_reason(command).expect(command);
            assert!(reason.contains("dprintf"), "{command}: {reason}");
        }
    }

    #[test]
    fn ordinary_commands_and_their_one_letter_aliases_still_run() {
        for command in [
            "s",
            "n",
            "c",
            "p x",
            "print x",
            "bt",
            "info locals",
            "break main",
            "run",
            "continue",
            "finish",
            "set var x = 5",
            "set x = 5",
            "set print pretty on",
            "with print pretty -- p s",
            "delete 1",
            "display x",
            "interrupt",
            "info sharedlibrary",
            "show version",
            "x/4x &buf",
            "frame 2",
            "up",
            "watch x",
            "dprintf main,\"hi\\n\"",
            "info inferiors",
            "detach",
            "kill",
            "-exec-interrupt",
            "-exec-continue",
        ] {
            assert_eq!(blocked_reason(command), None, "{command}");
        }
    }

    #[test]
    fn blocks_reenabling_guards() {
        assert!(blocked_reason("set may-call-functions on").is_some());
        assert!(blocked_reason("set  startup-with-shell  on").is_some());
        assert!(blocked_reason("set auto-load on").is_some());
    }

    #[test]
    fn allows_normal_debugging_commands() {
        for cmd in [
            "break main",
            "run",
            "next",
            "step",
            "continue",
            "print x",
            "bt",
            "info locals",
            "set var x = 5",
            "-exec-interrupt",
            "watch counter",
        ] {
            assert_eq!(blocked_reason(cmd), None, "should allow: {cmd}");
        }
    }
}
