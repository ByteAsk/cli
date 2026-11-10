//! The command check for the one-shot `debug` tool's lldb backend.
//!
//! `debug_session` checks every lldb command inside lldb itself:
//! `byteask_lldb_session.py` resolves it with `ResolveCommand` (so `pla sh` IS
//! `platform shell`) and then applies an allowlist. The one-shot tool hands its
//! commands to `lldb -b -o …`, where nothing of ours runs inside lldb, so this
//! applies the same allowlist from Rust. With no lldb to resolve abbreviations,
//! only full command names and lldb's own short forms (`bt`, `p`, `v`, `b`, …)
//! are accepted, and the run passes `-x` so no `~/.lldbinit` can define an
//! alias that means something else. Anything unrecognised is refused.
//!
//! Measured on lldb 14 through the one-shot tool's argv before this existed:
//! `platform shell`, `pla sh` and `shell` ran commands on the host, `p side()`
//! called a function in the program, and `breakpoint set --comm "platform shell
//! …"` ran one when the breakpoint was hit (lldb accepts any unambiguous prefix
//! of a long option). Best-effort, like the session's check: a defense layer,
//! not a sandbox.

/// First words that run with any arguments once they pass [`call_reason`].
const ANY_ARGS: [&str; 9] = [
    "_regexp-bt",
    "_regexp-list",
    "_regexp-up",
    "_regexp-down",
    "_regexp-jump",
    "disassemble",
    "help",
    "apropos",
    "version",
];

/// lldb's built-in short forms, and the command each one stands for.
const ALIASES: [(&str, &[&str]); 26] = [
    ("bt", &["_regexp-bt"]),
    ("up", &["_regexp-up"]),
    ("down", &["_regexp-down"]),
    ("l", &["_regexp-list"]),
    ("list", &["_regexp-list"]),
    ("j", &["_regexp-jump"]),
    ("jump", &["_regexp-jump"]),
    ("b", &["_regexp-break"]),
    ("tbreak", &["_regexp-tbreak"]),
    ("di", &["disassemble"]),
    ("dis", &["disassemble"]),
    ("c", &["process", "continue"]),
    ("continue", &["process", "continue"]),
    ("n", &["thread", "step-over"]),
    ("next", &["thread", "step-over"]),
    ("s", &["thread", "step-in"]),
    ("step", &["thread", "step-in"]),
    ("finish", &["thread", "step-out"]),
    ("ni", &["thread", "step-inst-over"]),
    ("nexti", &["thread", "step-inst-over"]),
    ("si", &["thread", "step-inst"]),
    ("stepi", &["thread", "step-inst"]),
    ("f", &["frame", "select"]),
    ("v", &["frame", "variable"]),
    ("vo", &["frame", "variable"]),
    ("image", &["target", "modules"]),
];

/// First word -> the subcommands allowed after it (as `byteask_lldb_session.py`).
const SUBCOMMANDS: [(&str, &[&str]); 12] = [
    (
        "breakpoint",
        &[
            "set", "list", "delete", "disable", "enable", "modify", "name", "clear",
        ],
    ),
    (
        "watchpoint",
        &[
            "set", "list", "delete", "disable", "enable", "modify", "ignore",
        ],
    ),
    (
        "thread",
        &[
            "backtrace",
            "list",
            "select",
            "step-in",
            "step-over",
            "step-out",
            "step-inst",
            "step-inst-over",
            "until",
            "return",
            "jump",
            "info",
            "continue",
        ],
    ),
    ("frame", &["select", "info", "variable"]),
    (
        "process",
        &[
            "status",
            "interrupt",
            "kill",
            "continue",
            "detach",
            "signal",
            "handle",
            "launch",
        ],
    ),
    ("memory", &["read", "write", "find", "region"]),
    ("register", &["read", "write"]),
    ("source", &["list", "info"]),
    ("target", &["variable", "list", "modules", "symbols"]),
    ("language", &["cplusplus"]),
    ("type", &["lookup"]),
    ("settings", &["show", "list", "set"]),
];

const TARGET_MODULES: [&str; 7] = [
    "list",
    "lookup",
    "dump",
    "add",
    "load",
    "search-paths",
    "show-unwind",
];

/// Options that make a breakpoint or watchpoint run code when it is hit: a
/// condition is an expression lldb may JIT (a call), a command list can hold
/// `platform shell`, and the rest attach a Python class.
const HIT_CODE_LONG: [&str; 5] = [
    "--condition",
    "--command",
    "--script-class",
    "--structured-data-key",
    "--structured-data-value",
];
const HIT_CODE_SHORT: [char; 5] = ['c', 'C', 'P', 'k', 'v'];

/// Settings a debugging run has a reason to change. Format settings are not
/// here: `${script.…}` in a format string runs Python.
const SAFE_SETTINGS: [&str; 19] = [
    "target.source-map",
    "target.exec-search-paths",
    "target.debug-file-search-paths",
    "target.max-string-summary-length",
    "target.max-children-count",
    "target.max-memory-read-size",
    "target.x86-disassembly-flavor",
    "target.prefer-dynamic-value",
    "target.enable-synthetic-value",
    "target.skip-prologue",
    "target.process.thread.step-avoid-regexp",
    "target.process.thread.step-in-avoid-nodebug",
    "stop-line-count-before",
    "stop-line-count-after",
    "stop-disassembly-count",
    "stop-disassembly-display",
    "print-decls",
    "escape-non-printables",
    "auto-one-line-summaries",
];

/// C/C++ operators that take parentheses and never call anything.
const NOT_CALLS: [&str; 6] = [
    "sizeof",
    "alignof",
    "_Alignof",
    "__alignof__",
    "decltype",
    "offsetof",
];

/// Check one lldb command of a one-shot `debug` run. `Ok` is the command to
/// run (an expression is rewritten so it cannot call into the program), `Err`
/// the reason it is refused.
pub(crate) fn check(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.contains(['\n', '\r']) {
        return Err(
            "a command containing a line break is several commands; put each in its own \
                    `commands` entry"
                .to_string(),
        );
    }
    if command.contains('`') {
        return Err(
            "backticks make lldb evaluate what is inside them as an expression".to_string(),
        );
    }
    let first = command.split_whitespace().next().unwrap_or("");
    if first.is_empty() {
        return Err("empty command".to_string());
    }
    let rest = command[first.len()..].trim_start();
    // `p/x EXPR`, `x/4xw ADDR`: a gdb-style format on the first word.
    let (base, format) = match first.split_once('/') {
        Some((base, format)) => (base, Some(format)),
        None => (first, None),
    };

    if matches!(base, "p" | "print" | "call" | "po" | "expression" | "expr") {
        let mut options = Vec::new();
        if base == "po" {
            options.push("-O".to_string());
        }
        if let Some(format) = format {
            if format.is_empty() || !format.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(format!(
                    "`{first}`: a format is letters and digits, like `p/x`"
                ));
            }
            options.push(format!("--gdb-format {format}"));
        }
        return rewrite_expression(rest, options);
    }
    if format.is_some() && base != "x" {
        return Err(refusal(first));
    }
    if matches!(base, "r" | "run") {
        return match rest.is_empty() {
            true => Ok(command.to_string()),
            false => Err(ARGS_REASON.to_string()),
        };
    }

    let words = split_words(rest)?;
    let mut canon: Vec<String> = match ALIASES.iter().find(|(alias, _)| *alias == base) {
        Some((_, stands_for)) => stands_for.iter().map(ToString::to_string).collect(),
        None if base == "x" => vec!["memory".to_string(), "read".to_string()],
        None => vec![base.to_string()],
    };
    canon.extend(words);
    let head = canon[0].as_str();
    let sub = canon.get(1).map(String::as_str).unwrap_or("");

    if head == "_regexp-break" || head == "_regexp-tbreak" {
        if sub.starts_with('-') {
            return Err(
                "give options to `breakpoint set` rather than `b`/`tbreak`, so they can be \
                        checked"
                    .to_string(),
            );
        }
        return guard_calls(command);
    }
    if ANY_ARGS.contains(&head) {
        return guard_calls(command);
    }
    let Some((_, allowed)) = SUBCOMMANDS.iter().find(|(name, _)| *name == head) else {
        return Err(refusal(head));
    };
    if !allowed.contains(&sub) {
        return Err(refusal(&format!("{head} {sub}")));
    }
    let third = canon.get(2).map(String::as_str).unwrap_or("");
    match (head, sub) {
        ("process", "launch") if canon.len() > 2 => return Err(ARGS_REASON.to_string()),
        ("target", "modules") if !TARGET_MODULES.contains(&third) => {
            return Err(refusal(&format!("target modules {third}")));
        }
        ("target", "symbols") if third != "add" => {
            return Err(refusal(&format!("target symbols {third}")));
        }
        ("language", _) if third != "demangle" => {
            return Err(refusal(&format!("language {sub} {third}")));
        }
        ("thread", "return") if canon.len() > 2 => {
            return Err(
                "`thread return` with a value evaluates an expression; use it bare, then \
                        `frame variable` to inspect"
                    .to_string(),
            );
        }
        ("settings", "set") if third.starts_with('-') || !SAFE_SETTINGS.contains(&third) => {
            return Err(format!(
                "changing `{third}` is not allowed here. Settings that can be changed: {}",
                SAFE_SETTINGS.join(", ")
            ));
        }
        ("breakpoint", "set" | "modify" | "name") | ("watchpoint", "set" | "modify")
            if canon[2..]
                .iter()
                .take_while(|w| *w != "--")
                .any(|w| runs_code_on_hit(w)) =>
        {
            return Err(
                "breakpoint conditions and commands are not available here: lldb would run \
                        them with calls into the program allowed. Break without a condition and \
                        inspect with `frame variable`, or skip hits with `-i N`"
                    .to_string(),
            );
        }
        _ => {}
    }
    guard_calls(command)
}

const ARGS_REASON: &str = "program arguments are set by the tool's `args`, which passes them \
                           exactly; use a bare `run`";

/// Does this option word attach code to a breakpoint hit? A long option counts
/// by any prefix lldb would accept (`--comm` IS `--command`), a short one
/// anywhere in a cluster (`-Hc` sets a condition).
fn runs_code_on_hit(word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or("");
        return !name.is_empty() && HIT_CODE_LONG.iter().any(|opt| opt[2..].starts_with(name));
    }
    match word.strip_prefix('-') {
        Some(cluster) => cluster.chars().any(|c| HIT_CODE_SHORT.contains(&c)),
        None => false,
    }
}

/// Run an expression with lldb's JIT off, so it cannot call a function. Ours
/// goes LAST among the options: lldb keeps the last value it is given, and
/// `--allow-jit false --allow true` (a prefix of `--allow-jit`) called one.
fn rewrite_expression(rest: &str, mut options: Vec<String>) -> Result<String, String> {
    let padded = format!(" {rest} ");
    let expr = if rest.starts_with('-') && padded.contains(" -- ") {
        let (given, expr) = padded.split_once(" -- ").unwrap_or_default();
        for word in split_words(given)? {
            let changes_mode = match word.strip_prefix("--") {
                Some(long) => {
                    let name = long.split('=').next().unwrap_or("");
                    !name.is_empty() && ("allow-jit".starts_with(name) || "repl".starts_with(name))
                }
                None => word.starts_with('-') && word[1..].contains(['j', 'r']),
            };
            if changes_mode {
                return Err(format!("`{word}` cannot be changed here"));
            }
        }
        options.push(given.trim().to_string());
        expr.trim()
    } else {
        rest.trim()
    };
    if expr.is_empty() {
        return Err("`expression` needs an expression to evaluate".to_string());
    }
    guard_calls(&format!("expression -- {expr}"))?;
    options.retain(|o| !o.is_empty());
    options.push("--allow-jit false".to_string());
    Ok(format!("expression {} -- {expr}", options.join(" ")))
}

fn guard_calls(command: &str) -> Result<String, String> {
    match call_reason(command) {
        Some(reason) => Err(reason),
        None => Ok(command.to_string()),
    }
}

/// `name(`, `)(` or `](` is a call, unless `name` is an operator that only
/// looks like one (`sizeof(x)`).
fn call_reason(text: &str) -> Option<String> {
    for (at, _) in text.match_indices('(') {
        let before = text[..at].trim_end();
        let is_call = match before.as_bytes().last() {
            Some(b')' | b']') => true,
            Some(&c) if c == b'_' || c.is_ascii_alphanumeric() => {
                let start = before
                    .rfind(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
                    .map_or(0, |i| i + 1);
                let name = &before[start..];
                // An identifier starts with a letter or `_`; `2(` is not a call.
                !name.as_bytes()[0].is_ascii_digit() && !NOT_CALLS.contains(&name)
            }
            _ => false,
        };
        if is_call {
            return Some(
                "this looks like a function call, and the program's functions cannot be called \
                 here. Inspect values with `frame variable` or `memory read` (write a cast as \
                 `(T)x`, not `(T)(x)`)"
                    .to_string(),
            );
        }
    }
    None
}

fn refusal(name: &str) -> String {
    format!(
        "`{}` is not available here (it can run code on the host, or is not part of debugging a \
         program). Write commands with their full names or lldb's standard short forms; \
         abbreviations like `br s` are not accepted. Available: breakpoint, watchpoint, \
         run/continue/next/step/finish, thread, frame, bt, up/down, print/expression (no \
         function calls), memory, register, disassemble, source/list, target modules, settings \
         show",
        name.trim()
    )
}

/// Split into words the way lldb does: whitespace separates, `'…'` and `"…"`
/// group and are removed, a backslash escapes the next character.
fn split_words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some(end) if end == c => break,
                        Some('\\') if c == '"' => word.extend(chars.next()),
                        Some(other) => word.push(other),
                        None => {
                            return Err(format!("could not parse the command (unterminated {c})"));
                        }
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}
