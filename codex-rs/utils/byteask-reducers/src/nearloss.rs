//! Class N: near-lossless. Repetitive chatter becomes a count; every warning, error,
//! failure and summary line stays verbatim, and so does anything within two lines of one.
//! The marker states facts computed over exactly what was dropped.
//!
//! Shipped dark: each of these is switched on from the policy object only after its
//! harness arm passes.

use std::collections::BTreeMap;

/// Words that mean "a person would want to read this line".
const SIGNAL_WORDS: [&str; 14] = [
    "error", "fail", "warn", "panic", "traceback", "exception", "fatal", "denied",
    "not found", "cannot", "unable", "abort", "segfault", "undefined",
];

pub(crate) fn is_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    SIGNAL_WORDS.iter().any(|w| lower.contains(w))
}

/// Replace runs of droppable lines with one marker line.
///
/// `kind_of` names the kind of a droppable line, or `None` to keep it. A line is never
/// droppable when it carries a signal word or sits within `GUARD` lines of one.
fn drop_runs(
    input: &str,
    min_run: usize,
    kind_of: &dyn Fn(&str) -> Option<&'static str>,
) -> Option<String> {
    const GUARD: usize = 2;
    let lines: Vec<&str> = input.split('\n').collect();
    let signal: Vec<bool> = lines.iter().map(|l| is_signal(l)).collect();
    let kinds: Vec<Option<&'static str>> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let lo = i.saturating_sub(GUARD);
            let hi = (i + GUARD + 1).min(lines.len());
            if signal[lo..hi].iter().any(|s| *s) { None } else { kind_of(l) }
        })
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if kinds[i].is_none() {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        while j < lines.len() && kinds[j].is_some() {
            if let Some(k) = kinds[j] {
                *counts.entry(k).or_default() += 1;
            }
            j += 1;
        }
        if j - i >= min_run {
            let detail: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
            out.push(format!("[byteask: {} lines omitted: {}]", j - i, detail.join(", ")));
            changed = true;
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }
    changed.then(|| out.join("\n"))
}

fn progress_kind(line: &str) -> Option<&'static str> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    // `[ 12%] Building CXX object …` (cmake / ninja)
    if t.starts_with('[') && t.find("%]").is_some_and(|i| i <= 5) {
        return Some("build-step lines");
    }
    let has_pct = t.contains('%');
    let bar = ["━", "█", "▓", "░", "#####", "=====", "-----", ">>>>>"].iter().any(|b| t.contains(b));
    if has_pct && bar {
        return Some("progress-bar lines");
    }
    if ["MB/s", "kB/s", "KB/s", "MiB/s", "KiB/s", "GB/s", " eta ", " ETA "].iter().any(|s| t.contains(s)) {
        return Some("transfer-rate lines");
    }
    let first = t.chars().next()?;
    if "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(first) {
        return Some("spinner lines");
    }
    None
}

/// Spinner, download and `[ 12%] Building` lines -> counts.
pub fn progress(input: &str) -> Option<String> {
    drop_runs(input, 4, &progress_kind)
}

fn pkg_kind(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    const TABLE: [(&str, &str); 22] = [
        ("Requirement already satisfied", "\"Requirement already satisfied\""),
        ("Collecting ", "\"Collecting\""),
        ("Downloading ", "\"Downloading\""),
        ("Using cached ", "\"Using cached\""),
        ("Obtaining ", "\"Obtaining\""),
        ("Preparing metadata", "\"Preparing metadata\""),
        ("Building wheel", "\"Building wheel\""),
        ("Created wheel", "\"Created wheel\""),
        ("Stored in directory", "\"Stored in directory\""),
        ("Get:", "apt \"Get:\""),
        ("Hit:", "apt \"Hit:\""),
        ("Ign:", "apt \"Ign:\""),
        ("Selecting previously unselected", "apt \"Selecting\""),
        ("Preparing to unpack", "apt \"Preparing to unpack\""),
        ("Unpacking ", "apt \"Unpacking\""),
        ("Setting up ", "apt \"Setting up\""),
        ("Processing triggers", "apt \"Processing triggers\""),
        ("npm http fetch", "\"npm http fetch\""),
        ("npm timing", "\"npm timing\""),
        ("Downloaded ", "cargo \"Downloaded\""),
        ("==> Pouring", "brew \"Pouring\""),
        ("==> Downloading", "brew \"Downloading\""),
    ];
    TABLE.iter().find(|(p, _)| t.starts_with(p)).map(|(_, k)| *k)
}

/// npm/pip/apt/cargo/brew install chatter -> summary.
pub fn pkg(input: &str) -> Option<String> {
    drop_runs(input, 4, &pkg_kind)
}

fn cmake_kind(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    const PREFIXES: [(&str, &str); 7] = [
        ("-- Looking for ", "\"Looking for\""),
        ("-- Performing Test ", "\"Performing Test\""),
        ("-- Found ", "\"Found\""),
        ("-- Check for working ", "\"Check for working\""),
        ("-- Detecting ", "\"Detecting\""),
        ("-- Checking ", "\"Checking\""),
        ("-- Check size of ", "\"Check size of\""),
    ];
    PREFIXES.iter().find(|(p, _)| t.starts_with(p)).map(|(_, k)| *k)
}

/// `-- Looking for / Performing Test / Found` lines -> counts.
pub fn cmake_conf(input: &str) -> Option<String> {
    drop_runs(input, 4, &cmake_kind)
}

fn test_pass_kind(line: &str) -> Option<&'static str> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    if t.ends_with(" PASSED") || (t.contains(" PASSED ") && t.ends_with("%]")) {
        return Some("pytest PASSED");
    }
    if t.ends_with("... ok") || t.ends_with(" ... ok") {
        return Some("\"... ok\"");
    }
    if t.starts_with("=== RUN") || t.starts_with("=== PAUSE") || t.starts_with("=== CONT") {
        return Some("go \"=== RUN\"");
    }
    if t.starts_with("--- PASS") {
        return Some("go \"--- PASS\"");
    }
    if t.starts_with("[ RUN      ]") {
        return Some("gtest \"[ RUN ]\"");
    }
    if t.starts_with("[       OK ]") {
        return Some("gtest \"[ OK ]\"");
    }
    if t.starts_with('✓') || t.starts_with('√') {
        return Some("passing checks");
    }
    if t.contains("   Passed ") && t.contains("Test ") {
        return Some("ctest Passed");
    }
    None
}

/// Passing-test lines -> counts; failures, errors and the summary verbatim.
pub fn tests(input: &str) -> Option<String> {
    drop_runs(input, 4, &test_pass_kind)
}

/// `In file included from a.h:1,` / `                 from b.h:2:` chains folded to the
/// first and last line.
pub fn incl_chain(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').collect();
    let is_chain = |l: &str| {
        let t = l.trim_start();
        t.starts_with("In file included from ") || (t.starts_with("from ") && l.starts_with(' '))
    };
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].trim_start().starts_with("In file included from ") {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && is_chain(lines[j]) && !lines[j].trim_start().starts_with("In file") {
            j += 1;
        }
        if j - i > 3 {
            out.push(lines[i].to_string());
            out.push(format!("[byteask: {} intermediate include lines omitted]", j - i - 2));
            out.push(lines[j - 1].to_string());
            changed = true;
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }
    changed.then(|| out.join("\n"))
}

/// `file:line:col: warning: message [-Wflag]` -> `(location, message)`.
fn warning_header(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(": warning: ")?;
    let loc = &line[..idx];
    // The location must end in `:digits` (line, or line:col).
    let last = loc.rsplit(':').next()?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((loc, &line[idx + ": warning: ".len()..]))
}

fn is_diag_header(line: &str) -> bool {
    [": warning: ", ": error: ", ": fatal error: "].iter().any(|m| line.contains(m))
}

/// The same warning at N locations -> the first 3 blocks verbatim, then one marker with
/// the count and the remaining locations. ALL errors stay verbatim.
pub fn warn_group(input: &str) -> Option<String> {
    const KEEP: usize = 3;
    const LIST: usize = 8;
    let lines: Vec<&str> = input.split('\n').collect();
    // A block = a warning header plus the snippet/caret/note lines that belong to it.
    let mut blocks: Vec<(usize, usize, &str, &str)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((loc, msg)) = warning_header(lines[i]) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            let belongs = (l.starts_with(' ') || l.starts_with('\t') || l.contains(": note: "))
                && !is_diag_header(l);
            if !belongs || l.trim().is_empty() {
                break;
            }
            j += 1;
        }
        blocks.push((i, j, loc, msg));
        i = j;
    }
    let mut by_msg: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (idx, b) in blocks.iter().enumerate() {
        by_msg.entry(b.3).or_default().push(idx);
    }
    let mut drop_line = vec![false; lines.len()];
    let mut marker_at: BTreeMap<usize, String> = BTreeMap::new();
    for ids in by_msg.values() {
        if ids.len() <= KEEP + 1 {
            continue;
        }
        let dropped = &ids[KEEP..];
        for id in dropped {
            let (s, e, _, _) = blocks[*id];
            drop_line[s..e].iter_mut().for_each(|d| *d = true);
        }
        let mut locs: Vec<&str> = dropped.iter().take(LIST).map(|id| blocks[*id].2).collect();
        let more = dropped.len().saturating_sub(LIST);
        let tail = if more > 0 { format!(", and {more} more") } else { String::new() };
        locs.dedup();
        marker_at.insert(
            blocks[dropped[0]].0,
            format!(
                "[byteask: the same warning at {} more locations: {}{}]",
                dropped.len(),
                locs.join(", "),
                tail
            ),
        );
    }
    if marker_at.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        if let Some(m) = marker_at.get(&idx) {
            out.push(m.clone());
        }
        if !drop_line[idx] {
            out.push((*line).to_string());
        }
    }
    Some(out.join("\n"))
}

/// Whole-output JSON (or NDJSON) with insignificant whitespace removed LEXICALLY.
/// Never parse-then-serialise: that drops duplicate keys, rewrites number lexemes and can
/// reorder keys. The parse here only VALIDATES.
pub fn json_min(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let valid = |s: &str| serde_json::from_str::<serde_json::Value>(s).is_ok();
    let out = if valid(trimmed) {
        strip_json_ws(trimmed)
    } else {
        let docs: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
        if docs.len() < 2 || !docs.iter().all(|d| valid(d)) {
            return None;
        }
        docs.iter().map(|d| strip_json_ws(d)).collect::<Vec<_>>().join("\n")
    };
    (out.len() < input.len()).then_some(out)
}

fn strip_json_ws(doc: &str) -> String {
    let mut out = String::with_capacity(doc.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in doc.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ' ' | '\n' | '\r' | '\t' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests_mod {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn passing_tests_become_a_count_and_failures_stay() {
        let mut input = String::from("running 9 tests\n");
        for i in 0..6 {
            input.push_str(&format!("test mod::t{i} ... ok\n"));
        }
        input.push_str("test mod::broken ... FAILED\n");
        for i in 6..12 {
            input.push_str(&format!("test mod::t{i} ... ok\n"));
        }
        input.push_str("\ntest result: FAILED. 12 passed; 1 failed\n");
        let got = tests(&input).unwrap();
        assert!(got.contains("test mod::broken ... FAILED"));
        assert!(got.contains("test result: FAILED. 12 passed; 1 failed"));
        assert!(got.contains("[byteask: 4 lines omitted: 4 \"... ok\"]"));
        // The two lines on each side of the failure are kept.
        assert!(got.contains("test mod::t5 ... ok"));
        assert!(got.contains("test mod::t6 ... ok"));
        assert!(!got.contains("test mod::t1 ... ok"));
    }

    #[test]
    fn short_runs_are_left_alone() {
        assert_eq!(tests("test a ... ok\ntest b ... ok\ndone\n"), None);
    }

    #[test]
    fn pip_chatter_is_counted_and_warnings_survive() {
        let mut input = String::new();
        for i in 0..5 {
            input.push_str(&format!("Collecting pkg{i}\n  Downloading pkg{i}.whl (1 kB)\n"));
        }
        input.push_str("WARNING: pip is old\nSuccessfully installed x\n");
        let got = pkg(&input).unwrap();
        assert!(got.contains("WARNING: pip is old"));
        assert!(got.contains("Successfully installed x"));
        assert!(got.contains("\"Collecting\""));
    }

    #[test]
    fn cmake_probe_lines_are_counted_but_a_miss_is_kept() {
        let mut input = String::new();
        for i in 0..6 {
            input.push_str(&format!("-- Looking for h{i}.h\n-- Looking for h{i}.h - found\n"));
        }
        input.push_str("-- Looking for zlib.h - not found\n-- Configuring done\n");
        let got = cmake_conf(&input).unwrap();
        assert!(got.contains("zlib.h - not found"));
        assert!(got.contains("-- Configuring done"));
        assert!(got.contains("lines omitted"));
    }

    #[test]
    fn progress_lines_are_counted() {
        let mut input = String::from("start\n");
        for i in 0..8 {
            input.push_str(&format!("[ {:>2}%] Building CXX object f{i}.o\n", i * 10));
        }
        input.push_str("[100%] Built target app\n");
        let got = progress(&input).unwrap();
        assert!(got.starts_with("start\n[byteask: 9 lines omitted: 9 build-step lines]"));
    }

    #[test]
    fn include_chains_keep_first_and_last() {
        let input = "In file included from a.h:1,\n                 from b.h:2,\n                 from c.h:3,\n                 from d.h:4,\n                 from main.c:5:\nx.h:9:1: error: boom\n";
        let got = incl_chain(input).unwrap();
        assert_eq!(
            got,
            "In file included from a.h:1,\n[byteask: 3 intermediate include lines omitted]\n                 from main.c:5:\nx.h:9:1: error: boom\n"
        );
    }

    #[test]
    fn repeated_warnings_keep_three_and_list_the_rest_errors_untouched() {
        let mut input = String::new();
        for i in 0..7 {
            input.push_str(&format!("f{i}.c:{}:5: warning: unused variable 'x' [-Wunused-variable]\n    int x;\n        ^\n", i + 1));
        }
        input.push_str("g.c:1:1: error: expected ';'\n");
        let got = warn_group(&input).unwrap();
        assert!(got.contains("f0.c:1:5: warning"));
        assert!(got.contains("f2.c:3:5: warning"));
        assert!(!got.contains("f3.c:4:5: warning"));
        assert!(got.contains("[byteask: the same warning at 4 more locations: f3.c:4:5, f4.c:5:5, f5.c:6:5, f6.c:7:5]"));
        assert!(got.contains("g.c:1:1: error: expected ';'"));
    }

    #[test]
    fn json_min_is_lexical() {
        // Duplicate keys, number lexemes and key order all survive.
        let input = "{\n  \"a\": 1.0,\n  \"a\": 2e0,\n  \"s\": \"x  y\\\" z\",\n  \"l\": [ 1, 2 ]\n}\n";
        assert_eq!(json_min(input).unwrap(), "{\"a\":1.0,\"a\":2e0,\"s\":\"x  y\\\" z\",\"l\":[1,2]}");
        assert_eq!(json_min("{ not json }"), None);
        assert_eq!(json_min("plain text"), None);
        assert_eq!(json_min("{\"a\": 1}\n{\"b\": 2}\n").unwrap(), "{\"a\":1}\n{\"b\":2}");
    }
}
