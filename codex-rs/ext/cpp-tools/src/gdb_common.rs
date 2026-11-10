//! Shared helpers for the two gdb-driving tools — the autonomous
//! [`crate::tools::GdbSessionTool`] (owns a headless `gdb -i=mi3`) and the
//! pair-debugging [`crate::tools::GdbAttachTool`] (attaches to the user's live
//! gdb over a socket). Kept in one place so the output cap, path resolution, and
//! the command-safety classifier can never drift between the two tools.

use std::path::Path;
use std::path::PathBuf;

/// Cap on rendered command output handed back to the model. Both tools truncate
/// to this on a char boundary so a huge `bt`/`x/2000x` can't flood the context.
pub(crate) const OUTPUT_CAP: usize = 8000;

/// Which debugger drives `debug` / `debug_session`.
#[derive(serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    Gdb,
    Lldb,
}

/// The debugger to use: the caller's choice; else lldb on macOS (gdb does not
/// run on Apple Silicon Macs, and needs code-signing on Intel ones); else gdb
/// where it is installed; else lldb. The second value says why, when the
/// choice was not the caller's and is not gdb, so the model knows which
/// syntax its commands must be in.
pub(crate) fn choose_backend(
    requested: Option<Backend>,
    have_gdb: bool,
    have_lldb: bool,
    macos: bool,
) -> Result<(Backend, Option<&'static str>), String> {
    let missing = |tool: &str| format!("{tool} not installed ({})", crate::probe::install_hint(tool));
    match requested {
        Some(Backend::Gdb) if !have_gdb => Err(if have_lldb {
            format!("{}. lldb is installed: pass backend=\"lldb\" (lldb command syntax)", missing("gdb"))
        } else {
            missing("gdb")
        }),
        Some(Backend::Lldb) if !have_lldb => Err(missing("lldb")),
        Some(backend) => Ok((backend, None)),
        None if macos && have_lldb => Ok((
            Backend::Lldb,
            Some("this ran under lldb, the macOS debugger, so commands use lldb syntax"),
        )),
        None if have_gdb => Ok((Backend::Gdb, None)),
        None if have_lldb => Ok((
            Backend::Lldb,
            Some("gdb is not installed here, so this ran under lldb; commands use lldb syntax"),
        )),
        None => Err(format!("no debugger found: {}; {}", missing("gdb"), missing("lldb"))),
    }
}

/// [`choose_backend`] for this machine.
pub(crate) fn choose_backend_here(
    requested: Option<Backend>,
) -> Result<(Backend, Option<&'static str>), String> {
    choose_backend(
        requested,
        crate::gdbmi::find_on_path("gdb").is_some(),
        crate::gdbmi::find_on_path("lldb").is_some(),
        cfg!(target_os = "macos"),
    )
}

/// First whitespace-separated token of a command (`""` if empty).
pub(crate) fn first_word(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

/// Resolve a program/core path: absolute as-is, relative against `cwd`.
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Truncate on a char boundary at or below `cap`, appending a marker.
pub(crate) fn truncate(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... [output truncated]");
    text
}

/// The gdb commands that hand an already-parsed argument list to the inferior.
pub(crate) struct InferiorArgs {
    /// Issue these, in order, before the program runs.
    pub commands: Vec<String>,
    /// The run goes through gdb's startup shell: spawn gdb with
    /// [`INFERIOR_SHELL`] in its environment.
    pub needs_shell: bool,
}

/// The shell gdb starts the inferior through when [`InferiorArgs::needs_shell`].
/// gdb uses `$SHELL`, which may be zsh (reads `~/.zshenv`) or csh (expands `!`
/// inside single quotes); `/bin/sh` is the one whose quoting we write for.
pub(crate) const INFERIOR_SHELL: (&str, &str) = ("SHELL", "/bin/sh");

/// Pass `args` to a gdb inferior exactly, byte for byte.
///
/// gdb runs with `startup-with-shell off` (hardening). Measured on gdb 13:
/// without a shell it splits the `set args` string on whitespace and passes
/// every piece RAW — exact for any argument that is non-empty and contains no
/// whitespace, quotes, backslashes and `$` included. (`--args` is worse: it
/// backslash-escapes those characters for a shell that is not there, so
/// `x"y` arrived as `x\"y`.) An empty argument, or one containing whitespace,
/// cannot survive that split, so for such a run only the shell is switched on
/// and every argument single-quoted: `/bin/sh` then has nothing to expand and
/// can only reassemble the words.
pub(crate) fn inferior_args(args: &[String]) -> Result<InferiorArgs, String> {
    if args.is_empty() {
        return Ok(InferiorArgs {
            commands: Vec::new(),
            needs_shell: false,
        });
    }
    if args.iter().any(|arg| arg.contains(['\n', '\r'])) {
        return Err("gdb cannot pass an argument that contains a line break: its command line \
                    ends there. Write the value to a file and pass the file's path instead."
            .to_string());
    }
    let needs_shell = args
        .iter()
        .any(|arg| arg.is_empty() || arg.chars().any(char::is_whitespace));
    if !needs_shell {
        return Ok(InferiorArgs {
            commands: vec![format!("set args {}", args.join(" "))],
            needs_shell,
        });
    }
    let quoted: Vec<String> = args.iter().map(|arg| crate::program_args::sh_quote(arg)).collect();
    let mut commands = vec![
        "set startup-with-shell on".to_string(),
        format!("set args {}", quoted.join(" ")),
    ];
    // gdb hands its own environment to the inferior, so put the user's SHELL
    // back: the program should see the environment it would see undebugged.
    match std::env::var("SHELL") {
        Ok(shell) if shell != INFERIOR_SHELL.1 => {
            commands.push(format!("set environment SHELL {shell}"));
        }
        Ok(_) => {}
        Err(_) => commands.push("unset environment SHELL".to_string()),
    }
    Ok(InferiorArgs {
        commands,
        needs_shell,
    })
}

// ─────────────────────────── pair-mode command safety ───────────────────────
//
// In PAIR mode the agent drives the *human's* live gdb. We cannot prove a
// denylist is safe there (aliases / user-defined commands / hooks hide what gdb
// actually runs, and `print f()` / `call f()` invoke inferior functions), so we
// flip to an ALLOWLIST: only known read-only / navigation commands auto-run.
// Everything else — state changes, execution control, inferior calls, and any
// unrecognized or aliased verb — is PROPOSED for the human to approve and run.
// Default is Propose (fail safe).
//
//   classify("bt")            -> Auto
//   classify("print node->x") -> Auto          (simple, call-free lvalue)
//   classify("print f()")     -> Propose(...)   ('(' => could invoke a function)
//   classify("set var x = 5") -> Propose(...)   (state change)
//   classify("continue")      -> Propose(...)   (execution control)
//   classify("wobble")        -> Propose(...)   (unknown / possibly an alias)

/// Verdict for one command on the pair-mode channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Read-only / navigation — safe to run automatically.
    Auto,
    /// State-changing, code-running, or unrecognized — return to the human to
    /// approve and run. Carries a short human-readable reason.
    Propose(String),
}

/// Verbs that are pure navigation/inspection with no expression to evaluate.
const NAV_VERBS: &[&str] = &[
    "bt",
    "backtrace",
    "where",
    "frame",
    "f",
    "up",
    "down",
    "info",
    "i",
    "list",
    "l",
    "thread",
    "whatis",
    "ptype",
    "disassemble",
    "disas",
];

/// Verbs that take an expression which gdb *evaluates* — safe only if the
/// expression is a simple, call-free lvalue (no `(`, `=`, `@`, …).
const EXPR_VERBS: &[&str] = &["print", "p", "output", "x"];

/// Classify one gdb command for the pair-mode allowlist. Default is `Propose`.
pub(crate) fn classify(command: &str) -> Verdict {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Verdict::Propose("empty command".to_string());
    }
    // The bridge runs the whole string, and gdb executes every line of it:
    // `bt\nset var x=777` was classified by its first word and changed the
    // human's live process without approval.
    if cmd.contains(['\n', '\r']) {
        return Verdict::Propose(
            "it contains a line break, so it is several commands and every line would run"
                .to_string(),
        );
    }
    let first = first_word(cmd);
    // Strip a trailing /format so `print/x`, `p/d`, `x/8xw` match their base verb.
    let base = first.split('/').next().unwrap_or("").to_ascii_lowercase();

    // `thread apply` / `frame apply` run an arbitrary sub-command on threads or
    // frames (`thread apply all call evil()`), so they are never auto. gdb
    // expands any unambiguous prefix, and `thread ap all call …` ran a
    // function in the live process while an exact "apply" check waved it on.
    let mut rest_words = cmd.split_whitespace();
    let _ = rest_words.next();
    if (base == "thread" || base == "frame")
        && rest_words.next().is_some_and(|word| "apply".starts_with(word))
    {
        return Verdict::Propose(format!("`{base} apply` runs a sub-command"));
    }

    if NAV_VERBS.contains(&base.as_str()) {
        // These are read-only, but gdb EVALUATES their argument — a count
        // (`bt N`, `up N`), an address (`frame address E`, `info line *E`,
        // `list *E`, `disassemble E,+N`) — and an expression can call a
        // function: measured, `bt f()`, `frame address f()`, `info symbol f()`
        // and `disassemble f(),+4` each ran `f` in the human's live process
        // without a proposal. The argument gets the same check as `print`'s.
        let rest = cmd[first.len()..].trim_start();
        if expr_is_simple(rest) {
            return Verdict::Auto;
        }
        return Verdict::Propose(format!(
            "`{base}` evaluates its argument, and this one could call a function or change state"
        ));
    }

    if EXPR_VERBS.contains(&base.as_str()) {
        // Everything after the first token (the /fmt is part of that token).
        let rest = cmd[first.len()..].trim_start();
        if expr_is_simple(rest) {
            return Verdict::Auto;
        }
        return Verdict::Propose(format!(
            "`{base}` with a call/assignment/increment/slice expression can run code or change state"
        ));
    }

    Verdict::Propose(format!(
        "`{base}` changes state, controls execution, or is unrecognized — approve it yourself"
    ))
}

/// True if an expression is a simple, call-free lvalue we are willing to auto-run.
/// Rejects the markers that indicate a function call, assignment, artificial
/// array slice, cast-to-aggregate, or multiple statements.
fn expr_is_simple(rest: &str) -> bool {
    // `++`/`--` write to the live process just as `=` does: `print x++`.
    !rest.contains(['(', ')', '=', '@', ';', '{', '}', '`']) && !rest.contains("++") && !rest.contains("--")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_debugger_is_chosen_that_exists_here_and_the_choice_is_explained() {
        use super::Backend::Gdb;
        use super::Backend::Lldb;
        use super::choose_backend;
        // (requested, have_gdb, have_lldb, macos) -> backend, explained?
        assert_eq!(choose_backend(None, true, true, false), Ok((Gdb, None)));
        assert_eq!(choose_backend(None, true, true, true).map(|c| (c.0, c.1.is_some())), Ok((Lldb, true)));
        assert_eq!(choose_backend(None, false, true, false).map(|c| (c.0, c.1.is_some())), Ok((Lldb, true)));
        assert_eq!(choose_backend(None, true, false, true), Ok((Gdb, None)));
        assert_eq!(choose_backend(Some(Gdb), true, true, true), Ok((Gdb, None)));
        assert_eq!(choose_backend(Some(Lldb), true, true, false), Ok((Lldb, None)));
        let no_gdb = choose_backend(Some(Gdb), false, true, true).expect_err("gdb missing");
        assert!(no_gdb.contains("backend=\"lldb\""), "{no_gdb}");
        assert!(choose_backend(None, false, false, false).is_err());
    }

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn first_word_handles_empty_and_leading_space() {
        assert_eq!(first_word("  break main"), "break");
        assert_eq!(first_word(""), "");
    }

    #[test]
    fn resolves_relative_and_absolute() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "a.out"), PathBuf::from("/work/a.out"));
        assert_eq!(resolve_path(cwd, "/abs/a.out"), PathBuf::from("/abs/a.out"));
    }

    #[test]
    fn truncate_caps_and_marks() {
        let out = truncate("x".repeat(OUTPUT_CAP + 100), OUTPUT_CAP);
        assert!(out.starts_with(&"x".repeat(OUTPUT_CAP)));
        assert!(out.ends_with("[output truncated]"));
        // Short input is returned unchanged.
        assert_eq!(truncate("hi".to_string(), OUTPUT_CAP), "hi");
    }

    #[test]
    fn classify_auto_runs_read_only_navigation() {
        for cmd in [
            "bt",
            "backtrace",
            "where",
            "frame 2",
            "up",
            "down",
            "info locals",
            "info registers",
            "info breakpoints",
            "list",
            "list foo.c:20",
            "thread 3",
            "whatis x",
            "ptype node",
            "disassemble",
            "disassemble main",
        ] {
            assert_eq!(classify(cmd), Verdict::Auto, "should auto-run: {cmd}");
        }
    }

    #[test]
    fn classify_auto_runs_simple_print_expressions() {
        for cmd in [
            "print x",
            "p node->next->data",
            "print *ptr",
            "print &var",
            "print arr[3]",
            "print/x flags",
            "p $pc",
            "output count",
            "x/8xw &buffer",
            "x/i $pc",
        ] {
            assert_eq!(classify(cmd), Verdict::Auto, "should auto-run: {cmd}");
        }
    }

    #[test]
    fn classify_proposes_calls_and_assignments() {
        for cmd in [
            "print f()",
            "p compute(1,2)",
            "call system(\"id\")",
            "print x = 5",
            "print arr@10",
            "output do_thing()",
            "x func()",
        ] {
            assert!(
                matches!(classify(cmd), Verdict::Propose(_)),
                "should propose: {cmd}"
            );
        }
    }

    #[test]
    fn classify_proposes_anything_after_a_line_break() {
        // Each of these auto-ran against a live gdb and the second line
        // executed — `set var` changed the process, `next` resumed it.
        for cmd in [
            "bt\nset var g_scratch=777",
            "print g_marker\nnext",
            "info locals\nkill",
            "frame 1\r\ncontinue",
        ] {
            assert!(
                matches!(classify(cmd), Verdict::Propose(_)),
                "should propose: {cmd:?}"
            );
        }
        // A trailing line break is trimmed away: still one command.
        assert_eq!(classify("list\n"), Verdict::Auto);
    }

    #[test]
    fn classify_proposes_a_navigation_verb_whose_argument_can_call() {
        // Each of these ran `touch_marker` in a live process (14 marker files)
        // while classified Auto.
        for cmd in [
            "bt touch_marker(\"/tmp/m\")",
            "backtrace full touch_marker(\"/tmp/m\")",
            "up touch_marker(\"/tmp/m\")-0x401000",
            "down touch_marker(\"/tmp/m\")",
            "frame address touch_marker(\"/tmp/m\")",
            "frame level touch_marker(\"/tmp/m\")-0x401000",
            "info line *touch_marker(\"/tmp/m\")",
            "info symbol touch_marker(\"/tmp/m\")",
            "info frame address touch_marker(\"/tmp/m\")",
            "list *touch_marker(\"/tmp/m\")",
            "disassemble touch_marker(\"/tmp/m\"),+4",
            "thread ap all call touch_marker(\"/tmp/m\")",
            "thread appl 1 print touch_marker(\"/tmp/m\")",
            "frame ap all print touch_marker(\"/tmp/m\")",
            "up x = 3",
        ] {
            assert!(
                matches!(classify(cmd), Verdict::Propose(_)),
                "should propose: {cmd}"
            );
        }
        // Ordinary navigation still runs by itself.
        for cmd in [
            "bt", "bt full", "bt 10", "backtrace -full", "where", "frame 2", "frame function main",
            "up", "down 2", "info locals", "info frame", "info line foo.c:20", "info symbol 0x401000",
            "list foo.c:20", "list main", "disassemble main", "thread 2", "thread", "whatis x",
            "ptype struct node",
        ] {
            assert_eq!(classify(cmd), Verdict::Auto, "should auto-run: {cmd}");
        }
    }

    #[test]
    fn classify_proposes_increments_and_decrements() {
        for cmd in ["print x++", "p ++x", "print counter--", "output --n", "x/x p++"] {
            assert!(
                matches!(classify(cmd), Verdict::Propose(_)),
                "should propose: {cmd}"
            );
        }
    }

    #[test]
    fn classify_proposes_state_execution_and_unknown() {
        for cmd in [
            "set var x = 5",
            "run",
            "start",
            "continue",
            "c",
            "next",
            "step",
            "finish",
            "until",
            "break main",
            "tbreak foo",
            "delete",
            "watch counter",
            "return 0",
            "jump *0x400000",
            "kill",
            "shell rm -rf /",
            "python import os",
            "source ~/.evil",
            "thread apply all call boom()",
            "frame apply all print x",
            "wobble", // unknown / possibly a user alias
        ] {
            assert!(
                matches!(classify(cmd), Verdict::Propose(_)),
                "should propose: {cmd}"
            );
        }
    }

    #[test]
    fn classify_empty_is_propose() {
        assert!(matches!(classify("   "), Verdict::Propose(_)));
    }
}
