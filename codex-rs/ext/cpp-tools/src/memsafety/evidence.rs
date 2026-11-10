//! Root-cause evidence from a sanitizer report.
//!
//! The crash frame is where bad state is USED. A sanitizer report usually also
//! says where that state was CREATED: AddressSanitizer names the object it hit
//! (a heap region, a stack variable, a global) and prints its allocation and
//! free stacks; MemorySanitizer built with `-fsanitize-memory-track-origins`
//! prints the origin of an uninitialised value. The typical wrong fix of a
//! memory-safety bug is a guard at the crash frame, so this module pulls the
//! creation-side evidence out of the report into a short block that is shown
//! BEFORE the raw report: it survives the raw report's byte cap, and it is the
//! first thing read.
//!
//! Only the first report in the text is summarised (later reports are
//! usually consequences of the first).

use crate::tools::FrameOwner;
use crate::tools::asan_bug_class;
use crate::tools::frame_owner;
use crate::tools::is_frame_line;
use crate::tools::parse_frame;
use crate::tools::strip_pid_prefix;

/// How many frames of one stack the summary keeps.
const FRAMES_PER_STACK: usize = 5;
/// How many related stacks (allocation, free, origin, leak) the summary keeps.
const RELATED_CAP: usize = 4;
/// UBSan diagnostics start mid-line after program output.
const RUNTIME_ERROR: &str = ": runtime error: ";

/// One frame of a summarised stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub function: String,
    pub location: Option<(String, u32)>,
}

impl Site {
    fn render(&self) -> String {
        match &self.location {
            Some((file, line)) => format!("{} ({}:{line})", short_function(&self.function), basename(file)),
            None => short_function(&self.function),
        }
    }
}

/// What one sanitizer report says about the bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    /// `AddressSanitizer`, `MemorySanitizer`, `UndefinedBehaviorSanitizer`,
    /// `LeakSanitizer` or `ThreadSanitizer`.
    pub sanitizer: &'static str,
    /// The bug class: `heap-buffer-overflow`, `use-of-uninitialized-value`,
    /// the UBSan message, `detected memory leaks`, ...
    pub bug: String,
    /// `READ of size 4` / `WRITE of size 1025`, when the report has one.
    pub access: Option<String>,
    /// The crash stack: user frames first (runtime frames dropped).
    pub crash: Vec<Site>,
    /// What the address belongs to, addresses removed: `0 bytes to the right
    /// of 16-byte region`, `stack of thread T0 at offset 3600 in frame f`, ...
    pub object: Option<String>,
    /// For a stack object: the variable the access hit, as the report marks it.
    pub stack_object: Option<String>,
    /// Allocation / free / origin / leak stacks: (label, frames).
    pub related: Vec<(String, Vec<Site>)>,
}

impl Evidence {
    /// The crash-stack functions (user code), innermost first.
    pub fn crash_functions(&self) -> Vec<String> {
        self.crash.iter().map(|site| site.function.clone()).collect()
    }
}

/// Parse the first sanitizer report in `text`. `None` when there is none.
pub fn parse(text: &str) -> Option<Evidence> {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some((sanitizer, bug)) = report_header(line) {
            return Some(parse_body(&lines, i, sanitizer, bug));
        }
        if let Some(idx) = line.find(RUNTIME_ERROR) {
            // A UBSan diagnostic counts only with its stack trace after it:
            // program output can quote the text.
            let next_is_frame = lines.get(i + 1).is_some_and(|next| is_frame_line(next));
            if next_is_frame {
                let bug = line[idx + RUNTIME_ERROR.len()..].trim().to_string();
                return Some(parse_body(&lines, i, "UndefinedBehaviorSanitizer", bug));
            }
        }
    }
    None
}

/// `==PID==ERROR: AddressSanitizer: heap-buffer-overflow on address …` and the
/// MSan/TSan `WARNING:` forms → (sanitizer, bug class).
fn report_header(line: &str) -> Option<(&'static str, String)> {
    let rest = strip_pid_prefix(line)?;
    let rest = rest
        .strip_prefix("ERROR: ")
        .or_else(|| rest.strip_prefix("WARNING: "))?;
    for sanitizer in [
        "AddressSanitizer",
        "MemorySanitizer",
        "LeakSanitizer",
        "ThreadSanitizer",
    ] {
        if let Some(after) = rest.strip_prefix(sanitizer).and_then(|r| r.strip_prefix(": ")) {
            let bug = if sanitizer == "AddressSanitizer" {
                asan_bug_class(after)
            } else {
                strip_addresses(after.split(" (pid=").next().unwrap_or(after))
            };
            return Some((sanitizer, bug));
        }
    }
    None
}

fn parse_body(lines: &[&str], header: usize, sanitizer: &'static str, bug: String) -> Evidence {
    let mut evidence = Evidence {
        sanitizer,
        bug,
        access: None,
        crash: Vec::new(),
        object: None,
        stack_object: None,
        related: Vec::new(),
    };
    let mut at = header + 1;
    let mut crash_seen = false;
    while at < lines.len() {
        let line = lines[at];
        let trimmed = line.trim();
        if trimmed.starts_with("SUMMARY: ") || (at > header + 1 && report_header(line).is_some()) {
            break;
        }
        if is_frame_line(line) {
            let (frames, next) = stack_at(lines, at);
            if !crash_seen {
                evidence.crash = frames;
                crash_seen = true;
            }
            at = next;
            continue;
        }
        if evidence.access.is_none()
            && (trimmed.starts_with("READ of size ") || trimmed.starts_with("WRITE of size "))
        {
            let cut = trimmed.find(" at 0x").unwrap_or(trimmed.len());
            evidence.access = Some(trimmed[..cut].to_string());
        } else if let Some(pos) = trimmed.find(" is located ") {
            if evidence.object.is_none() {
                let mut object = strip_addresses(&trimmed[pos + " is located ".len()..]);
                // `… in frame` is followed by the frame line naming it.
                if object.ends_with(" in frame")
                    && let Some(frame) = lines.get(at + 1).and_then(|l| parse_frame(l))
                {
                    let site = site_of(&frame);
                    object = format!("{object} {}", site.render());
                    at += 1;
                }
                evidence.object = Some(object);
            }
        } else if trimmed.contains("<== Memory access at offset") {
            if evidence.stack_object.is_none() {
                evidence.stack_object = Some(trimmed.to_string());
            }
        } else if let Some(label) = related_label(trimmed) {
            let (frames, next) = match lines.get(at + 1) {
                Some(next) if is_frame_line(next) => stack_at(lines, at + 1),
                _ => (Vec::new(), at + 1),
            };
            if evidence.related.len() < RELATED_CAP {
                evidence.related.push((label, frames));
            }
            at = next;
            continue;
        }
        at += 1;
    }
    evidence
}

/// A line that introduces an allocation / free / origin / leak stack.
fn related_label(trimmed: &str) -> Option<String> {
    if let Some(label) = trimmed.strip_suffix(" here:") {
        return Some(label.to_string());
    }
    if trimmed.starts_with("Uninitialized value was ") {
        return Some(trimmed.trim_end_matches(':').to_string());
    }
    if trimmed.starts_with("Direct leak of ") || trimmed.starts_with("Indirect leak of ") {
        return Some(strip_addresses(trimmed.trim_end_matches(':')));
    }
    if trimmed.starts_with("Previous write") || trimmed.starts_with("Previous read") {
        return Some(strip_addresses(trimmed.trim_end_matches(':')));
    }
    None
}

/// The frames of one stack starting at `at`, filtered to user code (library
/// frames only when there is no user frame), and the index after the stack.
fn stack_at(lines: &[&str], at: usize) -> (Vec<Site>, usize) {
    let mut end = at;
    let mut all = Vec::new();
    while end < lines.len() && is_frame_line(lines[end]) {
        if let Some(frame) = parse_frame(lines[end]) {
            all.push(frame);
        }
        end += 1;
    }
    let owned = |wanted: FrameOwner| -> Vec<Site> {
        all.iter()
            .filter(|frame| {
                let file = frame.location.as_ref().map(|(f, _)| f.as_str()).unwrap_or("");
                frame_owner(&frame.function, file) == wanted
                    && !frame.function.is_empty()
                    && !is_entry_point(&frame.function)
            })
            .map(site_of)
            .collect()
    };
    let mut sites = owned(FrameOwner::User);
    if sites.is_empty() {
        sites = owned(FrameOwner::Library);
    }
    sites.truncate(FRAMES_PER_STACK);
    (sites, end)
}

/// The process entry point (`_start` and the libc start routines): below
/// every stack, never where a bug is.
fn is_entry_point(function: &str) -> bool {
    matches!(function, "_start" | "__libc_start_main" | "__libc_start_call_main")
}

fn site_of(frame: &crate::tools::Frame) -> Site {
    Site {
        function: frame.function.clone(),
        location: frame.location.clone(),
    }
}

/// Drop hex addresses and the `[0x…,0x…)` / `(0x…)` groups that carry them.
pub(crate) fn strip_addresses(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix(" [0x").or_else(|| rest.strip_prefix(" (0x")) {
            // Skip to the closing bracket of the group.
            match stripped.find([')', ']']) {
                Some(close) => {
                    rest = &stripped[close + 1..];
                    continue;
                }
                None => break,
            }
        }
        if rest.starts_with("0x") {
            let len = rest[2..]
                .bytes()
                .take_while(u8::is_ascii_hexdigit)
                .count();
            if len > 0 {
                rest = &rest[2 + len..];
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `ns::Class::method(int, std::vector<…> const&)` → `ns::Class::method`.
pub(crate) fn short_function(function: &str) -> String {
    strip_params(function).to_string()
}

/// The function name without its parameter list (and a trailing `const`).
/// `(anonymous namespace)::f(int)` keeps its leading group.
pub(crate) fn strip_params(function: &str) -> &str {
    let mut name = function.trim();
    if let Some(stripped) = name.strip_suffix(" const") {
        name = stripped;
    }
    if !name.ends_with(')') {
        return name;
    }
    let bytes = name.as_bytes();
    let mut depth = 0i32;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    let head = &name[..i];
                    // `operator()` keeps its own parentheses.
                    if head.ends_with("operator") || head.is_empty() {
                        return name;
                    }
                    return head.trim_end();
                }
            }
            _ => {}
        }
    }
    name
}

/// The last `::` component of a function name, parameters removed, for
/// matching a stack frame against a function edited in a diff.
pub(crate) fn last_component(function: &str) -> &str {
    let name = strip_params(function);
    // Drop template arguments at the end: `f<int>` → `f`.
    let name = match name.find('<') {
        Some(at) if name.ends_with('>') => &name[..at],
        _ => name,
    };
    name.rsplit("::").next().unwrap_or(name).trim()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Where to look for the cause, by bug class. Generic C/C++ practice.
fn cause_hint(evidence: &Evidence) -> &'static str {
    let bug = evidence.bug.to_ascii_lowercase();
    if bug.contains("use-after-free")
        || bug.contains("double-free")
        || bug.contains("bad-free")
        || bug.contains("invalid free")
        || bug.contains("use-after-return")
        || bug.contains("use-after-scope")
    {
        "the object was freed (or went out of scope) while another owner still held it: fix the \
         lifetime or ownership at the free site or where the stale reference is kept, not with a \
         check where it crashes."
    } else if (bug.contains("overflow") && !bug.contains("signed integer overflow"))
        || bug.contains("out of bounds")
        || bug.contains("out-of-bounds")
        || bug.contains("underflow")
    {
        "the size or index was computed or accepted earlier: find where the length is parsed or \
         the buffer is sized (the object line above), and make them agree there; a bounds check \
         only at the crash line leaves every other reader of the same bad length."
    } else if evidence.sanitizer == "MemorySanitizer" || bug.contains("uninitiali") {
        "the origin stack above is where the value should have been initialised, or where a \
         failed step left it unset: fix that producer."
    } else if bug.contains("leak") {
        "find the owner that should free it on this path (often an error path)."
    } else if bug.contains("segv") || bug.contains("null") {
        "find the producer that returned or left the null or wild pointer, handle the failure \
         there and propagate it; a check only at the dereference hides the broken state."
    } else if evidence.sanitizer == "UndefinedBehaviorSanitizer" {
        "find where the operands come from and check the range where the value enters, in the \
         type the arithmetic uses."
    } else {
        "fix where the bad state is created, not where it is first noticed."
    }
}

/// The model-facing summary block. Empty when there is no evidence.
pub fn render(evidence: &Evidence) -> String {
    let mut out = String::from(
        "root-cause evidence (the crash frame is where the bad state is USED; look for where it \
         was CREATED):\n",
    );
    match &evidence.access {
        Some(access) => out.push_str(&format!("- bug: {} ({access})\n", evidence.bug)),
        None => out.push_str(&format!("- bug: {}\n", evidence.bug)),
    }
    if !evidence.crash.is_empty() {
        out.push_str(&format!("- crash stack: {}\n", join_sites(&evidence.crash)));
    }
    if let Some(object) = &evidence.object {
        out.push_str(&format!("- object: {object}\n"));
    }
    if let Some(stack_object) = &evidence.stack_object {
        out.push_str(&format!("- variable hit: {stack_object}\n"));
    }
    for (label, frames) in &evidence.related {
        if frames.is_empty() {
            out.push_str(&format!("- {label}\n"));
        } else {
            out.push_str(&format!("- {label}: {}\n", join_sites(frames)));
        }
    }
    out.push_str(&format!("- where to look: {}\n", cause_hint(evidence)));
    out.push_str(
        "- a guard at the crash site is the fix only when the violated invariant is local to that \
         function; check the finished fix with cpp_fix_verify.\n",
    );
    out
}

/// [`parse`] + [`render`], or an empty string when `text` holds no report.
pub fn render_from(text: &str) -> String {
    parse(text).map(|evidence| render(&evidence)).unwrap_or_default()
}

fn join_sites(sites: &[Site]) -> String {
    sites
        .iter()
        .map(Site::render)
        .collect::<Vec<_>>()
        .join(" <- ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const UAF: &str = "\
=================================================================
==1035721==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000010 at pc 0x5594b218850b bp 0x7ffe3872b800 sp 0x7ffe3872b7f8
READ of size 8 at 0x602000000010 thread T0
    #0 0x5594b218850a in use /tmp/e2/uaf.c:7
    #1 0x5594b218862a in main /tmp/e2/uaf.c:8
    #2 0x7f37b5c45249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58
    #3 0x5594b2188170 in _start (/tmp/e2/uaf+0x2170)

0x602000000010 is located 0 bytes inside of 16-byte region [0x602000000010,0x602000000020)
freed by thread T0 here:
    #0 0x7f37b66b76a8 in __interceptor_free ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:52
    #1 0x5594b21884b1 in drop /tmp/e2/uaf.c:6
    #2 0x5594b218861e in main /tmp/e2/uaf.c:8

previously allocated by thread T0 here:
    #0 0x7f37b66b89cf in __interceptor_malloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:69
    #1 0x5594b218824f in make /tmp/e2/uaf.c:5
    #2 0x5594b2188608 in main /tmp/e2/uaf.c:8

SUMMARY: AddressSanitizer: heap-use-after-free /tmp/e2/uaf.c:7 in use
Shadow bytes around the buggy address:
  0x0c047fff7fb0: 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
";

    const STACK_OVERFLOW: &str = "\
==8305==ERROR: AddressSanitizer: stack-buffer-overflow on address 0x779bf17c2610 at pc 0x000000519c56 bp 0x7ffe3d93c110 sp 0x7ffe3d93b8d8
READ of size 1025 at 0x779bf17c2610 thread T0
SCARINESS: 41 (multi-byte-read-stack-buffer-overflow)
    #0 0x519c55 in strdup /src/llvm-project/compiler-rt/lib/asan/asan_interceptors.cpp:438:5
    #1 0x3f5dd79 in parse_header /src/proj/src/reader.c:1221:23
    #2 0x3f57b6d in read_file /src/proj/src/reader.c:12856:23
    #3 0x56c934 in LLVMFuzzerTestOneInput /src/proj/fuzz/fuzz.c:113:11
    #4 0x43def3 in fuzzer::Fuzzer::ExecuteCallback(unsigned char const*, unsigned long) /src/llvm-project/compiler-rt/lib/fuzzer/FuzzerLoop.cpp:611:15

Address 0x779bf17c2610 is located in stack of thread T0 at offset 3600 in frame
    #0 0x3f5bb7f in parse_header /src/proj/src/reader.c:1025

  This frame has 3 object(s):
    [32, 112) 'field' (line 1043)
    [2576, 3600) 'buf2' (line 1221)
    [3728, 4752) 'buf3' (line 1222) <== Memory access at offset 3600 partially underflows this variable
SUMMARY: AddressSanitizer: stack-buffer-overflow /src/llvm-project/compiler-rt/lib/asan/asan_interceptors.cpp:438:5 in strdup
";

    const MSAN: &str = "\
==1036144==WARNING: MemorySanitizer: use-of-uninitialized-value
    #0 0x55d8367566ea in main /tmp/e2/um2.c:6:7
    #1 0x7fade63a8249 in __libc_start_call_main csu/../sysdeps/nptl/libc_start_call_main.h:58:16

  Uninitialized value was created by a heap allocation
    #0 0x55d836704c90 in __interceptor_malloc (/tmp/e2/um2+0x55c90) (BuildId: cca19f1c5aeced9d1d244e38f22de45c2618a0f1)
    #1 0x55d8367564a0 in main /tmp/e2/um2.c:4:12

SUMMARY: MemorySanitizer: use-of-uninitialized-value /tmp/e2/um2.c:6:7 in main
";

    #[test]
    fn use_after_free_keeps_the_free_and_allocation_stacks() {
        let evidence = parse(UAF).expect("a report");
        assert_eq!(evidence.sanitizer, "AddressSanitizer");
        assert_eq!(evidence.bug, "heap-use-after-free");
        assert_eq!(evidence.access.as_deref(), Some("READ of size 8"));
        assert_eq!(evidence.crash_functions(), vec!["use", "main"]);
        assert_eq!(
            evidence.object.as_deref(),
            Some("0 bytes inside of 16-byte region")
        );
        let labels: Vec<&str> = evidence.related.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["freed by thread T0", "previously allocated by thread T0"]);
        // The interceptor frame is the runtime's, never shown.
        assert_eq!(evidence.related[0].1[0].function, "drop");
        assert_eq!(evidence.related[1].1[0].function, "make");
        let text = render(&evidence);
        assert!(text.contains("- freed by thread T0: drop (uaf.c:6) <- main (uaf.c:8)"), "{text}");
        assert!(text.contains("- previously allocated by thread T0: make (uaf.c:5)"), "{text}");
        assert!(text.contains("ownership"), "{text}");
    }

    #[test]
    fn stack_overflow_names_the_frame_and_the_variable() {
        let evidence = parse(STACK_OVERFLOW).expect("a report");
        assert_eq!(evidence.bug, "stack-buffer-overflow");
        assert_eq!(evidence.access.as_deref(), Some("READ of size 1025"));
        // strdup is the interceptor; the libFuzzer driver frames are runtime.
        assert_eq!(
            evidence.crash_functions(),
            vec!["parse_header", "read_file", "LLVMFuzzerTestOneInput"]
        );
        assert_eq!(
            evidence.object.as_deref(),
            Some("in stack of thread T0 at offset 3600 in frame parse_header (reader.c:1025)")
        );
        assert!(evidence.stack_object.as_deref().unwrap_or("").starts_with("[3728, 4752) 'buf3'"));
        let text = render(&evidence);
        assert!(text.contains("the size or index was computed"), "{text}");
    }

    #[test]
    fn msan_origin_is_kept() {
        let evidence = parse(MSAN).expect("a report");
        assert_eq!(evidence.sanitizer, "MemorySanitizer");
        assert_eq!(evidence.bug, "use-of-uninitialized-value");
        assert_eq!(evidence.related.len(), 1);
        assert_eq!(
            evidence.related[0].0,
            "Uninitialized value was created by a heap allocation"
        );
        assert_eq!(evidence.related[0].1[0].function, "main");
        assert!(render(&evidence).contains("origin stack"));
    }

    #[test]
    fn ubsan_needs_its_stack() {
        let quoted = "log: parser.c:10:5: runtime error: signed integer overflow\nnext line\n";
        assert!(parse(quoted).is_none());
        let real = "parser.c:10:5: runtime error: signed integer overflow: 2147483647 + 1\n    #0 0x1 in add /src/p/parser.c:10:5\n    #1 0x2 in main /src/p/main.c:3:1\n";
        let evidence = parse(real).expect("a report");
        assert_eq!(evidence.sanitizer, "UndefinedBehaviorSanitizer");
        assert!(evidence.bug.starts_with("signed integer overflow"));
        assert_eq!(evidence.crash_functions(), vec!["add", "main"]);
    }

    #[test]
    fn a_leak_report_keeps_the_allocation_stack() {
        let text = "==9==ERROR: LeakSanitizer: detected memory leaks\n\nDirect leak of 32 byte(s) in 1 object(s) allocated from:\n    #0 0x1 in __interceptor_malloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:69\n    #1 0x2 in make_node /src/p/tree.c:40\n\nSUMMARY: AddressSanitizer: 32 byte(s) leaked in 1 allocation(s).\n";
        let evidence = parse(text).expect("a report");
        assert_eq!(evidence.bug, "detected memory leaks");
        assert_eq!(evidence.related[0].0, "Direct leak of 32 byte(s) in 1 object(s) allocated from");
        assert_eq!(evidence.related[0].1[0].function, "make_node");
    }

    #[test]
    fn no_report_no_block() {
        assert_eq!(render_from("hello\nworld\n"), "");
    }

    #[test]
    fn addresses_and_parameters_are_stripped() {
        assert_eq!(
            strip_addresses("4 bytes to the right of global variable 'g' defined in 'f.c:3:5' (0x55d8) of size 40"),
            "4 bytes to the right of global variable 'g' defined in 'f.c:3:5' of size 40"
        );
        assert_eq!(strip_params("ns::A::f(int, std::vector<int> const&) const"), "ns::A::f");
        assert_eq!(strip_params("(anonymous namespace)::g(char*)"), "(anonymous namespace)::g");
        assert_eq!(strip_params("S::operator()(int)"), "S::operator()");
        assert_eq!(last_component("ns::Widget::rebuildTree(TreeNode*, std::set<unsigned int>&)"), "rebuildTree");
        assert_eq!(last_component("tpl<int>"), "tpl");
    }
}
