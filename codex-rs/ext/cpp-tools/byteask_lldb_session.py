"""The lldb backend of ByteAsk's `debug_session` tool.

gdb does not run on Apple Silicon Macs, so there `debug_session` drives lldb
instead. The plain `lldb` command line cannot be driven over a pipe reliably:
`run` returns before the program stops, and while it runs lldb hands its stdin
to the program. lldb's own Python API in synchronous mode can: `run`,
`continue` and `next` return only once the program has stopped again.

Protocol (one JSON object per line):
  config  environment BYTEASK_LLDB_SESSION = {"program", "core", "args",
          "cwd", "modules": [[path, slide], ...]}
  stdout  first line {"ready": true, "state", "note"} or {"fatal": "..."};
          then, per command, {"pid": N} whenever the program is alive (so the
          engine can stop it) and finally {"out": "...", "state": "..."}
  stdin   one {"cmd": "..."} per command; end of input ends the session

A program that never stops is the ENGINE's job to interrupt: lldb's Python
calls hold the GIL while `continue` blocks, so no thread in this process can
run until it returns. The engine sends the program SIGSTOP after its timeout;
lldb reports that as an ordinary stop and the blocked command returns.

Safety mirrors the gdb session, which blocks host-side escapes and runs with
`set may-call-functions off`. lldb has no switch that forbids calling a
function in the program, and its escape surface is wider (`platform shell`,
`script`, aliases, format strings, breakpoint commands), so here every command
is first resolved by lldb itself (`ResolveCommand` expands aliases and
abbreviations: `pla sh` IS `platform shell`) and must then match an
allowlist. Expressions run with `--allow-jit false`, which leaves lldb's IR
interpreter unable to call anything. Anything else that evaluates an
expression (an address argument, a breakpoint condition) is refused when it
looks like a call. Best-effort: a defense layer, not a sandbox.
"""

import json
import os
import re
import shlex
import sys

# Commands resolved to one of these first words run with any arguments.
ANY_ARGS = {
    "_regexp-bt",
    "_regexp-list",
    "_regexp-up",
    "_regexp-down",
    "_regexp-jump",
    "disassemble",
    "help",
    "apropos",
    "version",
}
# First word -> the second words (subcommands) allowed after it.
SUBCOMMANDS = {
    "breakpoint": {"set", "list", "delete", "disable", "enable", "modify", "name", "clear"},
    "watchpoint": {"set", "list", "delete", "disable", "enable", "modify", "ignore"},
    "thread": {
        "backtrace", "list", "select", "step-in", "step-over", "step-out", "step-inst",
        "step-inst-over", "until", "return", "jump", "info", "continue",
    },
    "frame": {"select", "info", "variable"},
    "process": {"status", "interrupt", "kill", "continue", "detach", "signal", "handle", "launch"},
    "memory": {"read", "write", "find", "region"},
    "register": {"read", "write"},
    "source": {"list", "info"},
    "target": {"variable", "list", "modules", "symbols"},
    "language": {"cplusplus"},
    "type": {"lookup"},
    "settings": {"show", "list", "set"},
    "expression": None,  # handled separately
}
TARGET_MODULES = {"list", "lookup", "dump", "add", "load", "search-paths", "show-unwind"}
# Options that make a breakpoint or watchpoint run code when it is hit: a
# condition is an expression lldb may JIT (a call), a command list can hold
# `platform shell`, and the rest attach a Python class.
HIT_CODE_OPTIONS = ("--condition", "--command", "--script-class",
                    "--structured-data-key", "--structured-data-value")
HIT_CODE_SHORT = "cCPkv"
# Settings a debugging session has a reason to change. Format settings are
# not here: `${script.…}` in a format string runs Python.
SAFE_SETTINGS = {
    "target.source-map", "target.exec-search-paths", "target.debug-file-search-paths",
    "target.max-string-summary-length", "target.max-children-count",
    "target.max-memory-read-size", "target.x86-disassembly-flavor",
    "target.prefer-dynamic-value", "target.enable-synthetic-value", "target.skip-prologue",
    "target.process.thread.step-avoid-regexp", "target.process.thread.step-in-avoid-nodebug",
    "stop-line-count-before", "stop-line-count-after", "stop-disassembly-count",
    "stop-disassembly-display", "print-decls", "escape-non-printables",
    "auto-one-line-summaries",
}
# `name(` or `)(` or `](`: a call. The C/C++ operators that take parentheses
# and never call anything are exempt.
CALL = re.compile(r"([A-Za-z_][A-Za-z0-9_]*|[)\]])\s*\(")
NOT_CALLS = {"sizeof", "alignof", "_Alignof", "__alignof__", "decltype", "offsetof"}
# Output of one command handed back, and the program's own output.
OUTPUT_CAP = 12000
PROGRAM_OUTPUT_CAP = 4000


def looks_like_a_call(text):
    for match in CALL.finditer(text):
        if match.group(1) not in NOT_CALLS:
            return True
    return False


def check(raw, resolved, run_forms=()):
    """Decide what to do with one command.

    `run_forms` are lldb's own resolutions of a bare `run` / `process launch`
    (the alias adds options that differ by platform: a shell on Linux,
    `-X true` on macOS). Returns ("run", None) for one of those, (command,
    None) to execute `command`, or (None, reason) to refuse it.
    """
    if "\n" in raw or "\r" in raw:
        return None, ("a command containing a line break is several commands; send one "
                      "command per call")
    if "`" in resolved:
        return None, "backticks make lldb evaluate what is inside them as an expression"
    try:
        words = shlex.split(resolved)
    except ValueError as err:
        return None, "could not parse the command (%s)" % err
    if not words:
        return None, "empty command"
    head = words[0]
    sub = words[1] if len(words) > 1 else ""

    if head in ("_regexp-break", "_regexp-tbreak"):
        if len(words) > 1 and words[1].startswith("-"):
            return None, ("give options to `breakpoint set` rather than `b`/`tbreak`, so they "
                          "can be checked")
        return guard_calls(resolved)
    if head in ANY_ARGS:
        return guard_calls(resolved)
    if head == "expression":
        return rewrite_expression(resolved)
    if head not in SUBCOMMANDS:
        return None, refusal(head)
    if sub not in SUBCOMMANDS[head]:
        return None, refusal("%s %s" % (head, sub))

    if head == "process" and sub in ("kill", "detach") and len(words) == 2:
        # Through HandleCommand both wait for a confirmation nobody can give,
        # and return having done nothing.
        return sub, None
    if head == "process" and sub == "launch":
        # The session launches the program itself, with the exact argv given at
        # start and no shell: lldb's `run` alias goes through $SHELL, where
        # `$(...)` in an argument would run on the host.
        if resolved in run_forms:
            return "run", None
        return None, ("the program's arguments are fixed when the session starts "
                      "(`args`); start a new session to change them. Use plain `run`")
    if head == "target" and sub == "modules":
        third = words[2] if len(words) > 2 else ""
        if third not in TARGET_MODULES:
            return None, refusal("target modules %s" % third)
    if head == "target" and sub == "symbols" and (len(words) < 3 or words[2] != "add"):
        return None, refusal(" ".join(words[:3]))
    if head == "language" and (len(words) < 3 or words[2] != "demangle"):
        return None, refusal(" ".join(words[:3]))
    if head == "thread" and sub == "return" and len(words) > 2:
        return None, ("`thread return` with a value evaluates an expression; use it bare, "
                      "then `frame variable` to inspect")
    if head == "settings" and sub == "set":
        key = words[2] if len(words) > 2 else ""
        if key.startswith("-") or key not in SAFE_SETTINGS:
            return None, ("changing `%s` is not allowed here. Settings that can be changed: %s"
                          % (key, ", ".join(sorted(SAFE_SETTINGS))))
    guarded = (head == "breakpoint" and sub in ("set", "modify", "name")) or (
        head == "watchpoint" and sub in ("set", "modify"))
    if guarded:
        for word in words[2:]:
            if word == "--":
                break
            if runs_code_on_hit(word):
                return None, ("breakpoint conditions and commands are not available in an lldb "
                              "session: lldb would run them with calls into the program allowed. "
                              "Break without a condition and inspect with `frame variable`, or "
                              "skip hits with `-i N`")
    return guard_calls(resolved)


def runs_code_on_hit(word):
    """lldb accepts any unambiguous prefix of a long option (`--comm` IS
    `--command`, measured on lldb 14), and a short option anywhere in a cluster
    (`-Hc` sets a condition)."""
    if word.startswith("--"):
        name = word.split("=", 1)[0]
        return len(name) > 2 and any(opt.startswith(name) for opt in HIT_CODE_OPTIONS)
    return word.startswith("-") and any(c in HIT_CODE_SHORT for c in word[1:])


def guard_calls(resolved):
    if looks_like_a_call(resolved):
        return None, ("this looks like a function call, and this session cannot call functions "
                      "in the program. Inspect values with `frame variable` or `memory read` "
                      "(write a cast as `(T)x`, not `(T)(x)`)")
    return resolved, None


def rewrite_expression(resolved):
    """Run an expression with lldb's JIT off, so it cannot call a function."""
    rest = resolved[len("expression"):].strip()
    if rest.startswith("-") and " -- " in " %s " % rest:
        options, _, expr = (" %s " % rest).partition(" -- ")
        for word in options.split():
            name = word.split("=", 1)[0]
            if (len(name) > 2 and ("--allow-jit".startswith(name) or "--repl".startswith(name))) or (
                    not word.startswith("--") and word.startswith("-")
                    and ("j" in word or "r" in word)):
                return None, "`%s` cannot be changed in this session" % word
        options = options.strip()
    else:
        options, expr = "", rest
    expr = expr.strip()
    if not expr:
        return None, "`expression` needs an expression to evaluate"
    # Ours goes LAST: lldb keeps the last value it is given, and an earlier
    # `--allow-jit false` was undone by a later `--allow true`.
    command = "expression %s--allow-jit false -- %s" % (options + " " if options else "", expr)
    guarded, reason = guard_calls("expression -- " + expr)
    if guarded is None:
        return None, reason
    return command, None


def refusal(name):
    return ("`%s` is not available in this session (it can run code on the host, or is not "
            "part of debugging a program). Available: breakpoint, watchpoint, run/continue/"
            "next/step/finish, thread, frame, bt, up/down, print/expression (no function "
            "calls), memory, register, disassemble, source/list, target modules, settings show"
            % name.strip())


def cap(text, limit):
    if len(text) <= limit:
        return text
    return text[: limit // 2] + "\n... [%d bytes omitted] ...\n" % (len(text) - limit) + \
        text[-limit // 2:]


def main():
    config = json.loads(os.environ.get("BYTEASK_LLDB_SESSION", "{}"))
    out = sys.stdout

    def reply(**fields):
        out.write(json.dumps(fields) + "\n")
        out.flush()

    try:
        import lldb  # noqa: PLC0415 — located through PYTHONPATH by the engine
        lldb.SBDebugger  # noqa: B018 — Debian's broken package imports but is empty
    except Exception as err:  # noqa: BLE001
        reply(fatal="lldb's Python module could not be loaded (%s: %s)" % (type(err).__name__, err))
        return

    debugger = lldb.SBDebugger.Create(False)  # no ~/.lldbinit
    debugger.SetAsync(False)
    interp = debugger.GetCommandInterpreter()

    def run_raw(command):
        result = lldb.SBCommandReturnObject()
        interp.HandleCommand(command, result)
        return (result.GetOutput() or "") + (result.GetError() or "")

    for setting in (
        "settings set target.load-script-from-symbol-file false",
        "settings set target.load-cwd-lldbinit false",
        "settings set auto-confirm true",
        "settings set target.input-path /dev/null",
    ):
        run_raw(setting)

    cwd = config.get("cwd") or os.getcwd()
    program = config.get("program")
    core = config.get("core")
    error = lldb.SBError()
    target = debugger.CreateTarget(program or "", None, None, True, error)
    if not target or not target.IsValid():
        reply(fatal="lldb could not load %s: %s" % (program or "(no program)", error.GetCString()))
        return
    notes = []
    state = {"process": None}
    if core:
        process = target.LoadCore(core, error)
        if not process or not process.IsValid():
            reply(fatal="lldb could not load the core dump %s: %s" % (core, error.GetCString()))
            return
        state["process"] = process
        loaded = 0
        for path, slide in config.get("modules") or []:
            text = run_raw('target modules add "%s"' % path)
            if "error" in text.lower():
                notes.append("could not add %s: %s" % (path, text.strip()))
                continue
            run_raw('target modules load --file "%s" --slide %d' % (path, slide))
            loaded += 1
        if config.get("modules"):
            notes.append("loaded %d file(s) from the sysroot at the core's recorded addresses"
                         % loaded)

    import tempfile  # noqa: PLC0415

    fd, output_path = tempfile.mkstemp(prefix="byteask-lldb-", suffix=".out")
    os.close(fd)
    read_from = [0]

    def program_output():
        try:
            with open(output_path, "rb") as f:
                f.seek(read_from[0])
                data = f.read()
                read_from[0] += len(data)
        except OSError:
            return ""
        return data.decode("utf-8", "replace")

    def launch():
        if core:
            return ("error: this session holds a core dump, which cannot be run. Start a new "
                    "session without `core` to run the program")
        process = state["process"]
        if process is not None and process.IsValid() and process.GetState() not in (
                lldb.eStateExited, lldb.eStateDetached, lldb.eStateInvalid):
            process.Kill()
        open(output_path, "wb").close()
        read_from[0] = 0
        info = lldb.SBLaunchInfo(list(config.get("args") or []))
        info.SetWorkingDirectory(cwd)
        info.SetShellExpandArguments(False)
        info.AddOpenFileAction(0, "/dev/null", True, False)
        info.AddOpenFileAction(1, output_path, False, True)
        info.AddOpenFileAction(2, output_path, False, True)
        # Stopped at entry, so the pid is known (and handed to the engine)
        # before anything can run away.
        info.SetLaunchFlags(info.GetLaunchFlags() | lldb.eLaunchFlagStopAtEntry)
        launch_error = lldb.SBError()
        process = target.Launch(info, launch_error)
        if not launch_error.Success() or not process or not process.IsValid():
            return "error: could not launch the program: %s" % launch_error.GetCString()
        state["process"] = process
        announce_pid()
        return "Process %d launched: '%s'\n%s" % (
            process.GetProcessID(), program, run_raw("process continue"))

    def end_process(how):
        process = state["process"] or target.GetProcess()
        if process is None or not process.IsValid() or process.GetState() in (
                lldb.eStateExited, lldb.eStateDetached, lldb.eStateInvalid):
            return "error: there is no running program"
        pid = process.GetProcessID()
        result = process.Kill() if how == "kill" else process.Detach()
        if not result.Success():
            return "error: could not %s process %d: %s" % (how, pid, result.GetCString())
        return "Process %d %s." % (pid, "killed" if how == "kill" else "detached")

    def announce_pid():
        process = state["process"]
        if process is not None and process.IsValid() and process.GetState() in (
                lldb.eStateStopped, lldb.eStateRunning, lldb.eStateStepping,
                lldb.eStateSuspended):
            reply(pid=process.GetProcessID())

    def drain_events():
        # The public process state only moves when its events are pulled: a
        # killed program otherwise still reads "stopped at breakpoint 1".
        event = lldb.SBEvent()
        while debugger.GetListener().GetNextEvent(event):
            pass

    def process_state():
        drain_events()
        process = state["process"] or target.GetProcess()
        if process is None or not process.IsValid():
            return "ready"
        return {
            lldb.eStateStopped: "stopped",
            lldb.eStateCrashed: "stopped",
            lldb.eStateSuspended: "stopped",
            lldb.eStateExited: "exited",
            lldb.eStateRunning: "running",
            lldb.eStateStepping: "running",
        }.get(process.GetState(), "ready")

    def resolve(command):
        result = lldb.SBCommandReturnObject()
        interp.ResolveCommand(command, result)
        return (result.GetOutput() or "").strip() if result.Succeeded() else ""

    run_forms = {form for form in map(resolve, ("run", "r", "process launch")) if form}
    reply(ready=True, state=process_state(), note="\n".join(notes))

    for line in sys.stdin:
        try:
            raw = json.loads(line).get("cmd", "").strip()
        except ValueError:
            continue
        resolved_result = lldb.SBCommandReturnObject()
        interp.ResolveCommand(raw, resolved_result)
        resolved = (resolved_result.GetOutput() or "").strip()
        if not raw or not resolved_result.Succeeded() or not resolved:
            reply(out=(resolved_result.GetError() or "error: empty command").strip(),
                  state=process_state())
            continue
        command, reason = check(raw, resolved, run_forms)
        if command is None:
            reply(out="Refused: %s." % reason, state=process_state())
            continue

        if command == "run":
            text = launch()
        elif command in ("kill", "detach"):
            text = end_process(command)
        else:
            announce_pid()
            text = run_raw(command)
        drain_events()
        if target.GetProcess().IsValid():
            state["process"] = target.GetProcess()
        text = cap(text.rstrip("\n"), OUTPUT_CAP) or "(no output)"
        printed = program_output()
        if printed:
            text += "\nprogram output (stdout+stderr):\n" + cap(printed, PROGRAM_OUTPUT_CAP)
        reply(out=text, state=process_state())

    process = state["process"]
    if process is not None and process.IsValid() and not core:
        process.Kill()
    try:
        os.unlink(output_path)
    except OSError:
        pass
    lldb.SBDebugger.Destroy(debugger)


if __name__ == "__main__":
    main()
