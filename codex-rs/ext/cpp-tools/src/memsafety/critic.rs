//! Patch-shape critic: static checks on a crash fix's diff for the shapes that
//! stop one crashing input without fixing the bug, or that change behaviour.
//!
//! Every finding is ADVISORY. It is fed to the model before it finishes, as a
//! question to answer ("change the patch, or say why it is right"), never as a
//! verdict: each shape is also the right fix sometimes (a guard IS the fix when
//! the invariant is local to the function). The rules are generic C/C++
//! review practice for memory-safety fixes:
//!
//! | rule | shape |
//! |---|---|
//! | `crash-site-guard` | every edited function is on the crash stack and the change is mostly early exits / checks |
//! | `deleted-free` | a free / delete / release call is removed |
//! | `deleted-check` | an assert or an `if` check is removed and not replaced |
//! | `bigger-buffer` | an array or allocation is grown, or a fixed buffer made dynamic |
//! | `clamp` | a value is clamped (min/max) instead of rejected |
//! | `silenced-error` | `catch (...)`, `(void)` on a call, `#if 0`, sanitizer suppression, signal handlers |
//! | `magic-limit` | a new comparison against a large literal |
//! | `test-or-harness-edit` | a test, fuzz target or corpus is edited |
//! | `commented-out` | removed code comes back as a comment |
//! | `no-source-change` | the diff is empty or changes no C/C++ source |

use std::collections::BTreeSet;
use std::collections::HashSet;

use super::diff;
use super::diff::Change;
use super::diff::FileDiff;
use super::evidence::last_component;

/// One advisory finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapeFinding {
    pub rule: &'static str,
    pub file: String,
    pub line: u32,
    pub message: String,
}

/// Run every rule on `diff_text` (a `git diff`). `crash_functions` are the
/// crash-stack functions of the original report, innermost first; empty when
/// unknown (the crash-site rule is then skipped).
pub fn critique(diff_text: &str, crash_functions: &[String]) -> Vec<ShapeFinding> {
    let files = diff::parse(diff_text);
    let mut out = Vec::new();
    if files.iter().all(|f| f.hunks.is_empty()) {
        out.push(ShapeFinding {
            rule: "no-source-change",
            file: String::new(),
            line: 0,
            message: "the patch is empty: nothing was changed.".to_string(),
        });
        return out;
    }
    let mut source_changed = false;
    for file in &files {
        let changes = file.changes();
        if changes.is_empty() {
            continue;
        }
        if is_test_or_harness(file, &changes) {
            if !edits_test_or_harness(file, &changes) {
                continue; // a new regression test: fine, and not source
            }
            out.push(ShapeFinding {
                rule: "test-or-harness-edit",
                file: file.path.clone(),
                line: changes[0].line,
                message: "changes an existing test or a fuzz harness. A fix belongs in the code \
                          under test; editing a harness or an existing assertion hides the failure \
                          instead of fixing it. Adding a new regression test is fine; \
                          revert the edit unless the task asks for it."
                    .to_string(),
            });
            continue;
        }
        if !diff::is_cxx_path(&file.path) {
            continue;
        }
        source_changed = true;
        line_rules(file, &changes, &mut out);
    }
    if !source_changed && !out.iter().any(|f| f.rule == "test-or-harness-edit") {
        out.push(ShapeFinding {
            rule: "no-source-change",
            file: String::new(),
            line: 0,
            message: "the patch changes no C/C++ source file.".to_string(),
        });
    }
    if let Some(finding) = crash_site_guard(&files, crash_functions) {
        out.insert(0, finding);
    }
    out
}

/// Render findings for the model.
pub fn render(findings: &[ShapeFinding]) -> String {
    if findings.is_empty() {
        return "patch-shape review: no findings.".to_string();
    }
    let mut out = String::from(
        "patch-shape review (advisory, static): for each item, change the patch or say in one \
         line why it is right.\n",
    );
    for finding in findings {
        let at = if finding.file.is_empty() {
            String::new()
        } else {
            format!(" {}:{}", finding.file, finding.line)
        };
        out.push_str(&format!("- [{}]{at} {}\n", finding.rule, finding.message));
    }
    out
}

/// Whitespace-free form, for "was this line moved rather than deleted".
fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn code_of(text: &str) -> &str {
    let trimmed = text.trim();
    let comment_continuation =
        trimmed == "*" || trimmed.starts_with("* ") || trimmed.starts_with("*/");
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || comment_continuation {
        ""
    } else {
        trimmed
    }
}

/// Is this file a fuzz harness (its entry point, options or dictionary)?
fn is_harness(file: &FileDiff, changes: &[Change]) -> bool {
    let lower = file.path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or("");
    let fuzz_dir = lower
        .split('/')
        .any(|dir| matches!(dir, "fuzz" | "fuzzer" | "fuzzers" | "fuzzing" | "oss-fuzz"));
    fuzz_dir
        || name.contains("fuzzer")
        || name.starts_with("fuzz_")
        || name.contains("_fuzz.")
        || name.ends_with(".options")
        || name.ends_with(".dict")
        || changes
            .iter()
            .any(|c| c.text.contains("LLVMFuzzerTestOneInput") || c.text.contains("LLVMFuzzerInitialize"))
}

/// A test/harness change that can hide a failure: any change to a fuzz
/// harness, or a change that REMOVES lines of an existing test (an edited or
/// deleted assertion). Adding a new regression test next to a source fix is
/// good practice, not this shape.
fn edits_test_or_harness(file: &FileDiff, changes: &[Change]) -> bool {
    if is_harness(file, changes) {
        return true;
    }
    is_test_or_harness(file, changes) && changes.iter().any(|c| !c.added && !code_of(&c.text).is_empty())
}

fn is_test_or_harness(file: &FileDiff, changes: &[Change]) -> bool {
    let lower = file.path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').collect();
    let name = parts.last().copied().unwrap_or("");
    const DIRS: &[&str] = &[
        "test", "tests", "testing", "unittest", "unittests", "unit_tests", "testsuite", "fuzz",
        "fuzzer", "fuzzers", "fuzzing", "oss-fuzz", "corpus", "corpora", "regress", "regression",
    ];
    let in_test_dir = parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|dir| DIRS.contains(dir));
    let test_name = name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_tests.")
        || name.contains("_unittest.")
        || name.contains("fuzzer")
        || name.starts_with("fuzz_")
        || name.contains("_fuzz.")
        || name.ends_with(".options")
        || name.ends_with(".dict");
    let harness_code = changes
        .iter()
        .any(|c| c.text.contains("LLVMFuzzerTestOneInput") || c.text.contains("LLVMFuzzerInitialize"));
    in_test_dir || test_name || harness_code
}

/// Does `text` call `name` (`name(` or `name (`, not part of a longer word)?
fn calls(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(pos) = text[from..].find(name) {
        let start = from + pos;
        let end = start + name.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after = text[end..].trim_start();
        if before_ok && after.starts_with('(') {
            return true;
        }
        from = end;
    }
    false
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A call that frees or releases memory or a handle.
fn frees(text: &str) -> bool {
    let code = code_of(text);
    if code.is_empty() {
        return false;
    }
    const EXACT: &[&str] = &["free", "cfree", "realloc_free", "fclose", "close", "munmap"];
    if EXACT.iter().any(|name| calls(code, name)) {
        return true;
    }
    if code.starts_with("delete ") || code.starts_with("delete[]") || code.contains(" delete ") {
        return true;
    }
    // Project wrappers: foo_free(), xfree(), g_free(), obj_unref(), buf_release(), *_destroy().
    let mut rest = code;
    while let Some(open) = rest.find('(') {
        let head = rest[..open].trim_end();
        let word: String = head
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let lower = word.to_ascii_lowercase();
        if lower.ends_with("free")
            || lower.ends_with("_unref")
            || lower.ends_with("_release")
            || lower.ends_with("_destroy")
            || lower.ends_with("_delete")
        {
            return true;
        }
        rest = &rest[open + 1..];
    }
    false
}

fn is_assert(code: &str) -> bool {
    ["assert", "ASSERT", "CHECK", "DCHECK", "BUG_ON", "static_assert", "abort", "assume"]
        .iter()
        .any(|name| calls(code, name))
        || code.contains("_ASSERT(")
        || code.contains("_ASSERT (")
}

/// The first function called in `code` (`if (f(x) == 0)` → `f`, `(void) g(y);`
/// → `g`), skipping control keywords. Qualified names keep their last part.
fn primary_call(code: &str) -> Option<&str> {
    let mut i = 0;
    while let Some(pos) = code[i..].find('(') {
        let open = i + pos;
        let head = code[..open].trim_end();
        let mut start = head.len();
        while start > 0 && is_ident(head.as_bytes()[start - 1]) {
            start -= 1;
        }
        let name = &head[start..];
        if !name.is_empty()
            && !matches!(name, "if" | "while" | "for" | "switch" | "return" | "sizeof" | "void")
            && !name.as_bytes()[0].is_ascii_digit()
        {
            return Some(name);
        }
        i = open + 1;
    }
    None
}

/// The code (non-comment) text of the removed lines.
fn removed_code_lines(changes: &[Change]) -> impl Iterator<Item = &str> {
    changes
        .iter()
        .filter(|c| !c.added)
        .map(|c| code_of(&c.text))
        .filter(|code| !code.is_empty())
}

fn starts_if(code: &str) -> bool {
    code.starts_with("if (") || code.starts_with("if(") || code.starts_with("} else if")
}

fn exits(code: &str) -> bool {
    code.contains("return")
        || code.contains("goto ")
        || code.starts_with("break")
        || code.starts_with("continue")
        || code.contains("throw ")
        || code.contains("abort")
        || code.contains("exit(")
}

/// `name[123]` / `name[0x40]` declarators on a line: (name, size).
fn array_sizes(code: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0;
    while let Some(pos) = code[i..].find('[') {
        let open = i + pos;
        let close = match code[open..].find(']') {
            Some(c) => open + c,
            None => break,
        };
        let inner = code[open + 1..close].trim();
        let mut start = open;
        while start > 0 && is_ident(bytes[start - 1]) {
            start -= 1;
        }
        let name = &code[start..open];
        if !name.is_empty()
            && is_declarator_prefix(&code[..start])
            && let Some(size) = parse_int_literal(inner)
        {
            out.push((name.to_string(), size));
        }
        i = close + 1;
    }
    out
}

/// Is the text before `name[N]` a type, so `name[N]` declares an array
/// (`static char buf[64];`) rather than indexing one (`x = a[1] + b`)?
fn is_declarator_prefix(prefix: &str) -> bool {
    let prefix = prefix.trim();
    !prefix.is_empty()
        && prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '*' | ':' | '<' | '>' | ','))
        && prefix.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn parse_int_literal(text: &str) -> Option<u64> {
    let text = text.trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    text.parse::<u64>().ok()
}

const ALLOCATORS: &[&str] = &["malloc", "calloc", "realloc", "alloca", "xmalloc", "xcalloc", "xrealloc"];

fn allocates(code: &str) -> bool {
    ALLOCATORS.iter().any(|name| calls(code, name))
        || code.contains("new ")
        || code.contains("std::vector<")
        || code.contains("std::string ")
        || code.contains("resize(")
        || code.contains("reserve(")
}

/// Numeric literals in `code` (decimal or hex), for padding comparisons.
fn literals(code: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() && (i == 0 || !is_ident(bytes[i - 1])) {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            if let Some(n) = parse_int_literal(&code[start..i]) {
                out.push(n);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn line_rules(file: &FileDiff, changes: &[Change], out: &mut Vec<ShapeFinding>) {
    let added: HashSet<String> = changes
        .iter()
        .filter(|c| c.added)
        .map(|c| squash(&c.text))
        .collect();
    // Removed lines that were code (not comments), for "code turned into a comment".
    let removed_code: HashSet<String> = changes
        .iter()
        .filter(|c| !c.added && !code_of(&c.text).is_empty())
        .map(|c| squash(&c.text))
        .collect();
    let added_code: Vec<&Change> = changes.iter().filter(|c| c.added).collect();
    let added_any_if = added_code.iter().any(|c| starts_if(code_of(&c.text)));
    let added_any_assert = added_code.iter().any(|c| is_assert(code_of(&c.text)));
    let mut push = |rule: &'static str, line: u32, message: String| {
        // One finding per rule and file is enough to raise the question.
        if !out.iter().any(|f| f.rule == rule && f.file == file.path) {
            out.push(ShapeFinding {
                rule,
                file: file.path.clone(),
                line,
                message,
            });
        }
    };

    for change in changes {
        let code = code_of(&change.text);
        if code.is_empty() {
            // A comment line: is it removed code coming back commented out?
            if change.added {
                let body = change
                    .text
                    .trim()
                    .trim_start_matches("//")
                    .trim_start_matches("/*")
                    .trim_end_matches("*/");
                let body = squash(body);
                if body.len() > 3 && removed_code.contains(&body) {
                    push(
                        "commented-out",
                        change.line,
                        "turns removed code into a comment. Delete it or keep it; commented-out \
                         code hides what the fix changed."
                            .to_string(),
                    );
                }
            }
            continue;
        }
        if !change.added {
            if added.contains(&squash(&change.text)) {
                continue; // moved, not deleted
            }
            if frees(code) {
                push(
                    "deleted-free",
                    change.line,
                    format!(
                        "removes a free/release (`{code}`). Dropping it turns a use-after-free or \
                         double free into a leak instead of fixing who owns the object; fix the \
                         ownership, and keep freeing on every path."
                    ),
                );
            } else if is_assert(code) && !added_any_assert {
                push(
                    "deleted-check",
                    change.line,
                    format!(
                        "removes an assertion (`{code}`). If it fires on the input, the invariant \
                         it states is broken earlier: fix that, and keep the check."
                    ),
                );
            } else if starts_if(code)
                && !added_any_if
                && !primary_call(code).is_some_and(|name| {
                    added_code.iter().any(|a| calls(code_of(&a.text), name))
                })
            {
                push(
                    "deleted-check",
                    change.line,
                    format!(
                        "removes a condition (`{code}`) without adding one. Say why the check is \
                         wrong, or keep it."
                    ),
                );
            }
            continue;
        }
        // Added lines.
        if code.contains("catch (...)") || code.contains("catch(...)") {
            push(
                "silenced-error",
                change.line,
                "adds a catch-all handler. Swallowing every exception hides the failure instead \
                 of handling the one that is expected."
                    .to_string(),
            );
        }
        if code.starts_with("#if 0") || code.starts_with("#if false") {
            push(
                "silenced-error",
                change.line,
                "disables code with `#if 0`. Removing a code path is a behaviour change; fix the \
                 path instead."
                    .to_string(),
            );
        }
        if code.contains("no_sanitize")
            || code.contains("NO_SANITIZE")
            || code.contains("__no_sanitize")
            || code.contains("-fno-sanitize")
            || code.contains("_OPTIONS=")
        {
            push(
                "silenced-error",
                change.line,
                "suppresses a sanitizer. The report disappears, the bug does not.".to_string(),
            );
        }
        if calls(code, "signal") && (code.contains("SIGSEGV") || code.contains("SIGABRT") || code.contains("SIGBUS")) {
            push(
                "silenced-error",
                change.line,
                "installs a crash-signal handler. That hides the crash; fix the access.".to_string(),
            );
        }
        if let Some(name) = code.strip_prefix("(void)").and_then(primary_call)
            && removed_code_lines(changes).any(|r| calls(r, name) && !r.starts_with("(void)"))
        {
            push(
                "silenced-error",
                change.line,
                format!(
                    "discards a result with a (void) cast (`{code}`). If the call can fail, \
                     handle the failure."
                ),
            );
        }
        if ["std::min", "std::max", "std::clamp", "MIN", "MAX", "min", "max", "CLAMP", "clamp"]
            .iter()
            .any(|name| calls(code, name))
        {
            push(
                "clamp",
                change.line,
                format!(
                    "clamps a value (`{code}`). The input is silently changed rather than \
                     rejected; check that valid inputs keep their behaviour and that every \
                     consumer of the value agrees with the clamp."
                ),
            );
        }
        if starts_if(code)
            && (code.contains('<') || code.contains('>'))
            && literals(code).iter().any(|n| *n >= 256)
        {
            push(
                "magic-limit",
                change.line,
                format!(
                    "compares against a hard-coded limit (`{code}`). Derive the bound from the \
                     real object size or the format's rule, or valid large inputs are rejected \
                     and bigger bad ones still pass."
                ),
            );
        }
    }

    bigger_buffer(changes, &mut push);
}

/// Enlarged arrays, fixed buffers made dynamic, padded allocations.
fn bigger_buffer(changes: &[Change], push: &mut impl FnMut(&'static str, u32, String)) {
    let message = "enlarges a buffer or allocation. That makes this input fit, but the length that \
                   overflowed is still unchecked, and the limit or the output for long inputs \
                   changes. If the old size is the intended limit, enforce it where the data is \
                   written (truncate or reject), and check the other writers.";
    let removed: Vec<&Change> = changes.iter().filter(|c| !c.added).collect();
    let added: Vec<&Change> = changes.iter().filter(|c| c.added).collect();
    for r in &removed {
        let r_code = code_of(&r.text);
        for (name, old_size) in array_sizes(r_code) {
            for a in &added {
                let a_code = code_of(&a.text);
                let grown = array_sizes(a_code)
                    .iter()
                    .any(|(n, size)| *n == name && *size > old_size);
                let indexed = format!("{name}[");
                let made_dynamic = old_size >= 16
                    && allocates(a_code)
                    && a_code.contains(name.as_str())
                    && !a_code.contains(indexed.as_str());
                if grown || made_dynamic {
                    push("bigger-buffer", a.line, message.to_string());
                    return;
                }
            }
        }
        // `malloc(n)` → `malloc(n + 16)` in the same function.
        if ALLOCATORS.iter().any(|name| calls(r_code, name)) {
            let r_literals: BTreeSet<u64> = literals(r_code).into_iter().collect();
            let r_ops = r_code.matches(['+', '*']).count();
            for a in &added {
                let a_code = code_of(&a.text);
                if a.function != r.function || !ALLOCATORS.iter().any(|name| calls(a_code, name)) {
                    continue;
                }
                let new_literal = literals(a_code).iter().any(|n| !r_literals.contains(n));
                let more_ops = a_code.matches(['+', '*']).count() > r_ops;
                if new_literal && more_ops {
                    push("bigger-buffer", a.line, message.to_string());
                    return;
                }
            }
        }
    }
}

/// Is a guard line: an `if` with an exit on the same line, a bare exit after
/// an added `if`, or a null test.
fn guard_lines(changes: &[Change]) -> (usize, usize, Option<u32>) {
    let mut guards = 0;
    let mut other = 0;
    let mut first: Option<u32> = None;
    let mut previous_if = false;
    for change in changes.iter().filter(|c| c.added) {
        let code = code_of(&change.text);
        if code.is_empty() || code == "{" || code == "}" || code == "else" || code.starts_with("#") {
            continue;
        }
        let is_guard = (starts_if(code) && (exits(code) || null_test(code) || code.ends_with(')') || code.ends_with('{')))
            || (previous_if && exits(code));
        if is_guard {
            guards += 1;
            first.get_or_insert(change.line);
        } else {
            other += 1;
        }
        previous_if = starts_if(code);
    }
    (guards, other, first)
}

fn null_test(code: &str) -> bool {
    code.contains("== NULL")
        || code.contains("!= NULL")
        || code.contains("== nullptr")
        || code.contains("!= nullptr")
        || code.contains("(!")
        || code.contains("|| !")
        || code.contains("&& !")
}

fn crash_site_guard(files: &[FileDiff], crash_functions: &[String]) -> Option<ShapeFinding> {
    if crash_functions.is_empty() {
        return None;
    }
    let crash: HashSet<String> = crash_functions
        .iter()
        .map(|f| last_component(f).to_string())
        .collect();
    let mut edited: BTreeSet<String> = BTreeSet::new();
    let mut unknown = false;
    let mut guards = 0;
    let mut other = 0;
    let mut first: Option<(String, u32)> = None;
    for file in files.iter().filter(|f| diff::is_cxx_path(&f.path)) {
        let changes = file.changes();
        if is_test_or_harness(file, &changes) {
            continue;
        }
        for change in &changes {
            if code_of(&change.text).is_empty() {
                continue;
            }
            match &change.function {
                Some(function) => {
                    edited.insert(last_component(function).to_string());
                }
                None => unknown = true,
            }
        }
        let (g, o, line) = guard_lines(&changes);
        guards += g;
        other += o;
        if first.is_none()
            && let Some(line) = line
        {
            first = Some((file.path.clone(), line));
        }
    }
    if edited.is_empty() || unknown || guards == 0 {
        return None;
    }
    if !edited.iter().all(|f| crash.contains(f)) {
        return None;
    }
    // Mostly guards: a real restructuring on the stack is not this shape.
    if other > guards * 3 {
        return None;
    }
    let (file, line) = first.unwrap_or_default();
    let edited_list = edited.iter().cloned().collect::<Vec<_>>().join(", ");
    let top = last_component(&crash_functions[0]).to_string();
    let only_top = edited.len() == 1 && edited.contains(&top);
    let where_ = if only_top {
        format!("only the crashing function ({top})")
    } else {
        format!("only crash-stack functions ({edited_list})")
    };
    Some(ShapeFinding {
        rule: "crash-site-guard",
        file,
        line,
        message: format!(
            "the patch edits {where_} and mostly adds early exits or checks. The crash frame is \
             where the bad state is used; the report's allocation/free/origin stacks say where it \
             is created. Is the violated invariant truly local here? If another caller or \
             producer can create the same state, fix it there; then check the fix with \
             cpp_fix_verify."
        ),
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn rules(findings: &[ShapeFinding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.rule).collect()
    }

    const GUARD: &str = "\
diff --git a/src/reader.c b/src/reader.c
--- a/src/reader.c
+++ b/src/reader.c
@@ -10,4 +10,6 @@ parse_header (Chain *dat, Doc *doc)
   int n = read_len (dat);
   char buf[64];
+  if (n > 64)
+    return -1;
   memcpy (buf, dat->p, n);
";

    #[test]
    fn a_guard_in_the_crashing_function_is_flagged() {
        let findings = critique(GUARD, &["parse_header".to_string(), "main".to_string()]);
        assert_eq!(rules(&findings), vec!["crash-site-guard"]);
        assert!(findings[0].message.contains("only the crashing function (parse_header)"));
        assert_eq!((findings[0].file.as_str(), findings[0].line), ("src/reader.c", 12));
    }

    #[test]
    fn the_same_guard_off_the_stack_is_not() {
        let findings = critique(GUARD, &["memcpy_wrapper".to_string(), "main".to_string()]);
        assert!(findings.is_empty(), "{findings:?}");
        // Unknown crash stack: the rule cannot judge.
        assert!(critique(GUARD, &[]).is_empty());
    }

    #[test]
    fn deleted_free_and_moved_free() {
        let deleted = "\
diff --git a/a.c b/a.c
--- a/a.c
+++ b/a.c
@@ -1,4 +1,3 @@ void drop(item *it)
 {
-  free (it->name);
   free (it);
";
        assert_eq!(rules(&critique(deleted, &[])), vec!["deleted-free"]);
        let moved = "\
diff --git a/a.c b/a.c
--- a/a.c
+++ b/a.c
@@ -1,4 +1,4 @@ void drop(item *it)
 {
-  free (it->name);
   free (it);
+  free (it->name);
";
        assert!(critique(moved, &[]).is_empty());
        let wrapper = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,1 @@ void f(void)\n-  obj_unref (o);\n   x = 1;\n";
        assert_eq!(rules(&critique(wrapper, &[])), vec!["deleted-free"]);
    }

    #[test]
    fn deleted_assert_and_condition() {
        let text = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,3 +1,2 @@ int f(int n)\n-  assert (n < 8);\n   return n;\n";
        assert_eq!(rules(&critique(text, &[])), vec!["deleted-check"]);
        let cond = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,4 +1,2 @@ int f(int n)\n-  if (n >= 8)\n-    return -1;\n   return n;\n";
        assert_eq!(rules(&critique(cond, &[])), vec!["deleted-check"]);
        // A condition replaced by another is a change, not a deletion.
        let replaced = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,4 +1,4 @@ int f(int n)\n-  if (n > 8)\n+  if (n >= 8)\n     return -1;\n";
        assert!(critique(replaced, &[]).is_empty());
    }

    #[test]
    fn grown_and_dynamic_buffers() {
        let grown = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,2 @@ void f(void)\n-  char dest[1024];\n+  char dest[4096];\n";
        assert_eq!(rules(&critique(grown, &[])), vec!["bigger-buffer"]);
        let dynamic = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,3 @@ void f(void)\n-  char dest[1024];\n+  char *dest;\n+  dest = (char *)malloc (7 * len + 8);\n";
        assert!(rules(&critique(dynamic, &[])).contains(&"bigger-buffer"));
        let padded = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,2 @@ void f(int n)\n-  p = malloc (n);\n+  p = malloc (n + 16);\n";
        assert_eq!(rules(&critique(padded, &[])), vec!["bigger-buffer"]);
    }

    #[test]
    fn silenced_errors_clamps_and_limits() {
        let text = "diff --git a/a.cpp b/a.cpp\n--- a/a.cpp\n+++ b/a.cpp\n@@ -1,2 +1,6 @@ void f(int n)\n+  try { g(); } catch (...) {}\n+  n = std::min(n, cap);\n+  if (n > 65536) return;\n+  (void)close_it(fd);\n";
        let found = rules(&critique(text, &[]));
        assert!(found.contains(&"silenced-error"), "{found:?}");
        assert!(found.contains(&"clamp"), "{found:?}");
        assert!(found.contains(&"magic-limit"), "{found:?}");
    }

    #[test]
    fn test_and_harness_edits() {
        let text = "diff --git a/tests/fuzz/parse_fuzzer.cc b/tests/fuzz/parse_fuzzer.cc\n--- a/tests/fuzz/parse_fuzzer.cc\n+++ b/tests/fuzz/parse_fuzzer.cc\n@@ -1,2 +1,3 @@ int LLVMFuzzerTestOneInput(const uint8_t *d, size_t n)\n+  if (n > 100) return 0;\n   parse(d, n);\n";
        assert_eq!(rules(&critique(text, &["parse".to_string()])), vec!["test-or-harness-edit"]);
    }

    #[test]
    fn empty_and_non_source_patches() {
        assert_eq!(rules(&critique("", &[])), vec!["no-source-change"]);
        let docs = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(rules(&critique(docs, &[])), vec!["no-source-change"]);
    }

    #[test]
    fn a_real_fix_off_the_crash_frame_is_quiet() {
        // The size is validated where it is parsed, a function the crash stack
        // does not name, with an error propagated: no finding.
        let text = "\
diff --git a/src/lengths.c b/src/lengths.c
--- a/src/lengths.c
+++ b/src/lengths.c
@@ -20,5 +20,7 @@ int read_len (Chain *dat, size_t *out)
   size_t n = get_u32 (dat);
+  if (n > dat->size - dat->pos)
+    return ERR_TRUNCATED;
   *out = n;
   return 0;
";
        assert!(critique(text, &["parse_header".to_string(), "read_file".to_string()]).is_empty());
    }

    #[test]
    fn a_new_regression_test_next_to_a_fix_is_fine_but_editing_one_is_not() {
        let added_test = "\
diff --git a/src/parse.c b/src/parse.c
--- a/src/parse.c
+++ b/src/parse.c
@@ -5,2 +5,4 @@ int read_len (Chain *dat, size_t *out)
   size_t n = get_u32 (dat);
+  if (n > dat->size - dat->pos)
+    return ERR_TRUNCATED;
diff --git a/tests/unit/parse_test.c b/tests/unit/parse_test.c
--- a/tests/unit/parse_test.c
+++ b/tests/unit/parse_test.c
@@ -40,2 +40,5 @@ static void test_lengths(void)
   check (read_len (&ok, &n) == 0);
+  /* A length past the end is rejected. */
+  check (read_len (&short_input, &n) == ERR_TRUNCATED);
";
        assert!(critique(added_test, &[]).is_empty(), "{:?}", critique(added_test, &[]));
        let edited_test = "\
diff --git a/tests/unit/parse_test.c b/tests/unit/parse_test.c
--- a/tests/unit/parse_test.c
+++ b/tests/unit/parse_test.c
@@ -40,2 +40,2 @@ static void test_lengths(void)
-  check (read_len (&bad, &n) == ERR_TRUNCATED);
+  check (read_len (&bad, &n) == 0);
";
        assert_eq!(rules(&critique(edited_test, &[])), vec!["test-or-harness-edit"]);
    }

    #[test]
    fn index_expressions_are_not_array_declarations() {
        let text = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,2 @@ void f(void)\n-  x = a[0] + b[1];\n+  x = a[1] + b[2] + c[3];\n";
        assert!(critique(text, &[]).is_empty(), "{:?}", critique(text, &[]));
        assert!(is_declarator_prefix("static unsigned char "));
        assert!(!is_declarator_prefix("x = y + "));
    }

    #[test]
    fn a_dereference_is_code_not_a_comment() {
        assert_eq!(code_of("  *option = NULL;"), "*option = NULL;");
        assert_eq!(code_of("   * text of a block comment"), "");
        assert_eq!(code_of("   */"), "");
    }

    #[test]
    fn a_moved_comment_is_not_commented_out_code() {
        let text = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,3 +1,3 @@ void f(void)\n- * Scale the values to 16 bits.\n   g();\n+ * Scale the values to 16 bits.\n";
        assert!(critique(text, &[]).is_empty(), "{:?}", critique(text, &[]));
        let code = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,2 @@ void f(void)\n-  release_all (ctx);\n+  // release_all (ctx);\n";
        assert!(rules(&critique(code, &[])).contains(&"commented-out"));
    }

    #[test]
    fn a_condition_folded_into_an_expression_is_not_deleted() {
        let text = "diff --git a/a.cpp b/a.cpp\n--- a/a.cpp\n+++ b/a.cpp\n@@ -1,5 +1,3 @@ bool parse(std::string_view s, int &v)\n-  if (convert(s, v) == 0)\n-    return true;\n-  else\n-    return false;\n+  auto rc = convert(s, v);\n+  return rc == 0 && consumed_all(s);\n";
        assert!(critique(text, &[]).is_empty(), "{:?}", critique(text, &[]));
    }

    #[test]
    fn a_void_cast_counts_only_when_a_checked_result_is_now_discarded() {
        let idiom = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,1 +1,2 @@ void f(char *d, const char *s)\n+  (void) memcpy (d, s, 4);\n";
        assert!(critique(idiom, &[]).is_empty(), "{:?}", critique(idiom, &[]));
        let silenced = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1,2 +1,1 @@ int f(int fd)\n-  if (close_it (fd) < 0) return -1;\n+  (void) close_it (fd);\n";
        assert!(rules(&critique(silenced, &[])).contains(&"silenced-error"));
    }

    #[test]
    fn rendering() {
        assert_eq!(render(&[]), "patch-shape review: no findings.");
        let text = render(&critique(GUARD, &["parse_header".to_string()]));
        assert!(text.starts_with("patch-shape review (advisory, static)"), "{text}");
        assert!(text.contains("- [crash-site-guard] src/reader.c:12 "), "{text}");
    }
}
