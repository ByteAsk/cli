//! Class G: gated lossy. Each one drops content a person would skim past, states exactly
//! what was dropped, and only runs when the original was spilled. Shipped dark.
//!
//! The redundancy guard (from caveman): a drop is allowed only when the units that
//! SURVIVE cover at least 90% of the dropped units' vocabulary. Identifiers the model has
//! never seen are therefore never removed.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

fn vocabulary<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for line in lines {
        for word in line.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            // Numbers vary between frames and locations without carrying new meaning.
            if word.len() >= 3 && !word.bytes().all(|b| b.is_ascii_digit()) {
                set.insert(word.to_string());
            }
        }
    }
    set
}

/// True when `kept` covers >= 90% of the words in `dropped`.
pub(crate) fn covered(dropped: &[&str], kept: &[&str]) -> bool {
    let d = vocabulary(dropped.iter().copied());
    if d.is_empty() {
        return true;
    }
    let k = vocabulary(kept.iter().copied());
    let hit = d.iter().filter(|w| k.contains(*w)).count();
    hit * 10 >= d.len() * 9
}

/// Fold runs of lines matching `is_member` to the first and last `keep` lines.
fn fold_runs(
    input: &str,
    keep: usize,
    min_run: usize,
    what: &str,
    is_member: &dyn Fn(&str) -> bool,
    guard: bool,
) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !is_member(lines[i]) {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && is_member(lines[j]) {
            j += 1;
        }
        let run = &lines[i..j];
        if run.len() >= min_run && run.len() > keep * 2 {
            let dropped = &run[keep..run.len() - keep];
            let kept: Vec<&str> = run[..keep]
                .iter()
                .chain(run[run.len() - keep..].iter())
                .copied()
                .collect();
            if !guard || covered(dropped, &kept) {
                out.extend(run[..keep].iter().map(|l| (*l).to_string()));
                out.push(format!("[byteask: {} {what} omitted]", dropped.len()));
                out.extend(run[run.len() - keep..].iter().map(|l| (*l).to_string()));
                changed = true;
                i = j;
                continue;
            }
        }
        out.extend(run.iter().map(|l| (*l).to_string()));
        i = j;
    }
    changed.then(|| out.join("\n"))
}

/// C++ template instantiation backtraces -> first and last 3 frames. The error itself is
/// not a member line, so it always stays.
pub fn cxx_templates(input: &str) -> Option<String> {
    let member = |l: &str| {
        (l.contains("required from")
            || l.contains("in instantiation of")
            || l.contains("instantiated from")
            || l.contains("required by substitution"))
            && !l.contains(" error: ")
    };
    fold_runs(input, 3, 8, "template instantiation frames", &member, false)
}

/// `note: candidate …` lists -> the first 5, then a count. A candidate may span several
/// lines (the `note:` line plus indented detail lines).
pub fn cxx_candidates(input: &str) -> Option<String> {
    const KEEP: usize = 5;
    let lines: Vec<&str> = input.split('\n').collect();
    let is_head = |l: &str| l.contains("note: candidate");
    let is_detail = |l: &str| {
        (l.starts_with(' ') || l.starts_with('\t') || l.contains("note:   "))
            && !l.contains(" error: ")
            && !l.trim().is_empty()
    };
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !is_head(lines[i]) {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        // Collect the whole candidate list.
        let start = i;
        let mut heads: Vec<usize> = Vec::new();
        while i < lines.len() && (is_head(lines[i]) || (!heads.is_empty() && is_detail(lines[i]))) {
            if is_head(lines[i]) {
                heads.push(i);
            }
            i += 1;
        }
        if heads.len() > KEEP + 2 {
            let cut = heads[KEEP];
            out.extend(lines[start..cut].iter().map(|l| (*l).to_string()));
            out.push(format!(
                "[byteask: {} more candidates omitted ({} lines)]",
                heads.len() - KEEP,
                i - cut
            ));
            changed = true;
        } else {
            out.extend(lines[start..i].iter().map(|l| (*l).to_string()));
        }
    }
    changed.then(|| out.join("\n"))
}

/// Undefined references grouped by symbol: the first per symbol stays, the rest become a count.
pub fn link_group(input: &str) -> Option<String> {
    const NEEDLE: &str = "undefined reference to ";
    let lines: Vec<&str> = input.split('\n').collect();
    let symbol = |l: &str| {
        l.find(NEEDLE)
            .map(|i| l[i + NEEDLE.len()..].trim().to_string())
    };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for l in &lines {
        if let Some(s) = symbol(l) {
            *counts.entry(s).or_default() += 1;
        }
    }
    if !counts.values().any(|n| *n > 2) {
        return None;
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, l) in lines.iter().enumerate() {
        match symbol(l) {
            Some(s) if counts[&s] > 2 => {
                if seen.insert(s.clone()) {
                    out.push((*l).to_string());
                    out.push(format!(
                        "[byteask: {} more undefined references to {s} omitted]",
                        counts[&s] - 1
                    ));
                }
                // A repeat: drop it, and the `in function` line just above it if present.
            }
            _ => {
                let next_is_dropped = lines
                    .get(idx + 1)
                    .and_then(|n| symbol(n))
                    .is_some_and(|s| counts[&s] > 2 && seen.contains(&s));
                if next_is_dropped && l.contains(": in function ") {
                    continue;
                }
                out.push((*l).to_string());
            }
        }
    }
    Some(out.join("\n"))
}

/// Strip the frame index and address so recursion shows up as identical lines.
fn frame_key(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t.strip_prefix('#')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let mut body = rest[digits..].trim_start();
    if let Some(after) = body.strip_prefix("0x") {
        let hex = after.bytes().take_while(u8::is_ascii_hexdigit).count();
        body = after[hex..].trim_start();
        body = body.strip_prefix("in ").unwrap_or(body);
    }
    // Arguments differ per frame in a recursion; the function and location do not.
    let func = body.split(" (").next().unwrap_or(body);
    let at = body.rfind(" at ").map(|i| &body[i..]).unwrap_or("");
    Some(format!("{func}{at}"))
}

/// Repeated stack frames (recursion) folded: first, a count, last.
pub fn stack_fold(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let Some(key) = frame_key(lines[i]) else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < lines.len() && frame_key(lines[j]).as_deref() == Some(key.as_str()) {
            j += 1;
        }
        if j - i >= 5 {
            out.push(lines[i].to_string());
            out.push(format!(
                "[byteask: {} identical frames of {key} omitted]",
                j - i - 2
            ));
            out.push(lines[j - 1].to_string());
            changed = true;
        } else {
            out.extend(lines[i..j].iter().map(|l| (*l).to_string()));
        }
        i = j;
    }
    changed.then(|| out.join("\n"))
}

/// A large homogeneous JSON array -> first K + last K + count + key set + numeric ranges.
/// A document with many DIFFERENT shapes is left alone: it is a document, not a list.
pub fn json_array(input: &str) -> Option<String> {
    const K: usize = 5;
    const MIN_LEN: usize = 30;
    let value: serde_json::Value = serde_json::from_str(input.trim()).ok()?;
    let items = value.as_array()?;
    if items.len() < MIN_LEN {
        return None;
    }
    let keys_of = |v: &serde_json::Value| -> Option<Vec<String>> {
        let mut k: Vec<String> = v.as_object()?.keys().cloned().collect();
        k.sort();
        Some(k)
    };
    let shape = keys_of(&items[0])?;
    if items.iter().any(|it| keys_of(it).as_ref() != Some(&shape)) {
        return None;
    }
    let dropped = &items[K..items.len() - K];
    let mut ranges: Vec<String> = Vec::new();
    for key in &shape {
        let nums: Vec<f64> = dropped
            .iter()
            .filter_map(|it| it.get(key)?.as_f64())
            .collect();
        if nums.len() == dropped.len() {
            let lo = nums.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            ranges.push(format!("{key} {lo}..{hi}"));
        }
    }
    let render = |v: &serde_json::Value| serde_json::to_string(v).unwrap_or_default();
    let mut out = String::from("[\n");
    for it in &items[..K] {
        out.push_str(&render(it));
        out.push_str(",\n");
    }
    out.push_str(&format!(
        "[byteask: {} of {} array elements omitted; every element has the keys {}{}]\n",
        dropped.len(),
        items.len(),
        shape.join(", "),
        if ranges.is_empty() {
            String::new()
        } else {
            format!("; omitted ranges: {}", ranges.join(", "))
        }
    ));
    for (n, it) in items[items.len() - K..].iter().enumerate() {
        out.push_str(&render(it));
        out.push_str(if n + 1 == K { "\n" } else { ",\n" });
    }
    out.push(']');
    (out.len() < input.len()).then_some(out)
}

fn is_generated_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    const LOCKS: [&str; 12] = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "poetry.lock",
        "Pipfile.lock",
        "composer.lock",
        "Gemfile.lock",
        "go.sum",
        "flake.lock",
        "uv.lock",
        "bun.lockb",
    ];
    LOCKS.contains(&name)
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".map")
        || path
            .split('/')
            .any(|seg| matches!(seg, "node_modules" | "vendor" | "third_party"))
}

/// In a unified diff, the hunks of files MATCHED as lockfiles, minified bundles, source
/// maps or vendored trees become one line. Hunks of source files are never touched.
pub fn gen_diff(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let Some(rest) = lines[i].strip_prefix("diff --git a/") else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < lines.len() && !lines[j].starts_with("diff --git ") {
            j += 1;
        }
        let path = rest.split(" b/").next().unwrap_or(rest);
        if is_generated_path(path) {
            let body = &lines[i + 1..j];
            let plus = body
                .iter()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                .count();
            let minus = body
                .iter()
                .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                .count();
            if plus + minus > 6 {
                out.push(lines[i].to_string());
                out.push(format!(
                    "[byteask: {path}: generated file, +{plus} -{minus} lines, hunks omitted]"
                ));
                changed = true;
                i = j;
                continue;
            }
        }
        out.extend(lines[i..j].iter().map(|l| (*l).to_string()));
        i = j;
    }
    changed.then(|| out.join("\n"))
}

/// Children of dependency and build directories in listings -> counts.
pub fn vendor_ls(input: &str) -> Option<String> {
    const DIRS: [&str; 7] = [
        "node_modules",
        ".git",
        "target",
        "build",
        "dist",
        "__pycache__",
        ".venv",
    ];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let root_of = |line: &str| -> Option<String> {
        let mut acc = String::new();
        for seg in line.split('/') {
            if !acc.is_empty() || line.starts_with('/') {
                acc.push('/');
            }
            acc.push_str(seg);
            if DIRS.contains(&seg) {
                return (acc.len() < line.len()).then_some(acc);
            }
        }
        None
    };
    for line in input.split('\n') {
        if let Some(root) = root_of(line) {
            *counts.entry(root).or_default() += 1;
        }
    }
    if !counts.values().any(|n| *n >= 5) {
        return None;
    }
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for line in input.split('\n') {
        match root_of(line) {
            Some(root) if counts[&root] >= 5 => {
                if emitted.insert(root.clone()) {
                    out.push(format!(
                        "{root}/ [byteask: {} entries below omitted]",
                        counts[&root]
                    ));
                }
            }
            _ => out.push(line.to_string()),
        }
    }
    Some(out.join("\n"))
}

/// HTML: drop `<script>`, `<style>`, `<svg>` bodies and data URIs.
pub fn html_text(input: &str) -> Option<String> {
    let head = input
        .trim_start()
        .get(..200.min(input.trim_start().len()))?
        .to_ascii_lowercase();
    if !(head.starts_with("<!doctype html") || head.starts_with("<html")) {
        return None;
    }
    let mut text = input.to_string();
    let mut dropped = 0usize;
    for tag in ["script", "style", "svg"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut from = 0;
        while let Some(s) = find_ci(&text, &open, from) {
            let Some(e) = find_ci(&text, &close, s) else {
                break;
            };
            let end = e + close.len();
            dropped += end - s;
            let marker = format!("<{tag}>[byteask: {} bytes omitted]</{tag}>", end - s);
            text.replace_range(s..end, &marker);
            from = s + marker.len();
        }
    }
    (dropped > 0 && text.len() < input.len()).then_some(text)
}

fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || from >= h.len() {
        return None;
    }
    (from..=h.len().saturating_sub(n.len()))
        .find(|&i| hay.is_char_boundary(i) && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// A line that is one base64/hex/minified run longer than 2K chars -> head + length.
/// Never a normal source line: under 2% of it may be whitespace. (Zero whitespace was the
/// first rule and it never fired on REAL minified code, which still says `var a=` and
/// `return x`; ordinary prose and source run at 10-20% whitespace.)
pub fn blob_line(input: &str) -> Option<String> {
    const LIMIT: usize = 2_000;
    const HEAD: usize = 200;
    let mut changed = false;
    let mut out: Vec<String> = Vec::new();
    for line in input.split('\n') {
        let blank = line.bytes().filter(|b| *b == b' ' || *b == b'\t').count();
        if line.len() > LIMIT && blank * 50 < line.len() {
            let mut cut = HEAD;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push(line[..cut].to_string());
            out.push(format!(
                "[byteask: the line above continues for {} more bytes, omitted]",
                line.len() - cut
            ));
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }
    changed.then(|| out.join("\n"))
}

/// CSV/TSV with a constant column count -> head + tail + row count.
pub fn table(input: &str) -> Option<String> {
    const HEAD: usize = 20;
    const TAIL: usize = 10;
    let lines: Vec<&str> = input.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.len() < 80 {
        return None;
    }
    let delim = [',', '\t', '|'].into_iter().find(|d| {
        let n = lines[0].matches(*d).count();
        n >= 2 && lines.iter().all(|l| l.matches(*d).count() == n)
    })?;
    let _ = delim;
    let mut out: Vec<String> = lines[..HEAD].iter().map(|l| (*l).to_string()).collect();
    out.push(format!(
        "[byteask: {} of {} table rows omitted]",
        lines.len() - HEAD - TAIL,
        lines.len()
    ));
    out.extend(lines[lines.len() - TAIL..].iter().map(|l| (*l).to_string()));
    Some(out.join("\n"))
}

/// A date prefix repeated on every line -> stated once. The time of day stays on each line.
pub fn log_prefix(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.len() < 20 {
        return None;
    }
    let is_date = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 11
            && b[..4].iter().all(u8::is_ascii_digit)
            && b[4] == b'-'
            && b[5..7].iter().all(u8::is_ascii_digit)
            && b[7] == b'-'
            && b[8..10].iter().all(u8::is_ascii_digit)
            && (b[10] == b'T' || b[10] == b' ')
    };
    let prefix = lines[0].get(..11)?;
    if !is_date(prefix) || !lines.iter().all(|l| l.starts_with(prefix)) {
        return None;
    }
    let mut out = format!("[byteask: every line below began with \"{prefix}\", removed]\n");
    for line in input.split('\n') {
        out.push_str(line.strip_prefix(prefix).unwrap_or(line));
        out.push('\n');
    }
    out.pop();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn template_backtraces_keep_both_ends_and_the_error() {
        let mut input = String::new();
        for i in 0..12 {
            input.push_str(&format!("a.h:{i}:3:   required from 'struct W<{i}>'\n"));
        }
        input.push_str("a.h:99:5: error: static assertion failed\n");
        let got = cxx_templates(&input).unwrap();
        assert!(got.contains("required from 'struct W<0>'"));
        assert!(got.contains("required from 'struct W<11>'"));
        assert!(!got.contains("struct W<5>"));
        assert!(got.contains("[byteask: 6 template instantiation frames omitted]"));
        assert!(got.contains("error: static assertion failed"));
    }

    #[test]
    fn candidate_lists_keep_five() {
        let mut input = String::from("x.cc:1:1: error: no matching function for call to 'f'\n");
        for i in 0..9 {
            input.push_str(&format!(
                "h.h:{i}:1: note: candidate: 'void f(T{i})'\n  no known conversion\n"
            ));
        }
        let got = cxx_candidates(&input).unwrap();
        assert!(got.contains("void f(T4)"));
        assert!(!got.contains("void f(T5)"));
        assert!(got.contains("[byteask: 4 more candidates omitted (8 lines)]"));
        assert!(got.contains("error: no matching function"));
    }

    #[test]
    fn undefined_references_are_grouped_by_symbol() {
        let input = "ld: a.o: in function `m':\na.c:(.text+0x1): undefined reference to `foo'\nld: b.o: in function `n':\nb.c:(.text+0x2): undefined reference to `foo'\nc.c:(.text+0x3): undefined reference to `foo'\nd.c:1: undefined reference to `bar'\ncollect2: error: ld returned 1\n";
        let got = link_group(input).unwrap();
        assert_eq!(got.matches("undefined reference to `foo'").count(), 1);
        assert!(got.contains("[byteask: 2 more undefined references to `foo' omitted]"));
        assert!(got.contains("undefined reference to `bar'"));
        assert!(got.contains("collect2: error"));
        assert!(!got.contains("b.o: in function"));
    }

    #[test]
    fn recursion_frames_fold() {
        let mut input = String::from("#0  0x01 in leaf (x=1) at a.c:3\n");
        for i in 1..9 {
            input.push_str(&format!("#{i}  0x{i:02x} in rec (n={i}) at a.c:10\n"));
        }
        input.push_str("#9  0xff in main () at a.c:20\n");
        let got = stack_fold(&input).unwrap();
        assert!(got.contains("in leaf"));
        assert!(got.contains("in main"));
        assert!(got.contains("[byteask: 6 identical frames of rec at a.c:10 omitted]"));
    }

    #[test]
    fn homogeneous_arrays_shrink_and_mixed_documents_do_not() {
        let items: Vec<String> = (0..40)
            .map(|i| format!("{{\"id\": {i}, \"name\": \"n{i}\"}}"))
            .collect();
        let input = format!("[{}]", items.join(",\n  "));
        let got = json_array(&input).unwrap();
        assert!(got.contains("30 of 40 array elements omitted"));
        assert!(got.contains("keys id, name"));
        assert!(got.contains("id 5..34"));
        assert!(got.contains("\"id\":39"));
        let mixed = format!("[{},{{\"other\":1}}]", items.join(","));
        assert_eq!(json_array(&mixed), None);
    }

    #[test]
    fn only_generated_files_lose_their_hunks() {
        let mut input = String::from(
            "diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/package-lock.json b/package-lock.json\n@@ -1,9 +1,9 @@\n",
        );
        for i in 0..8 {
            input.push_str(&format!("-  \"v\": {i}\n+  \"v\": {}\n", i + 1));
        }
        let got = gen_diff(&input).unwrap();
        assert!(got.contains("-old\n+new"));
        assert!(
            got.contains(
                "[byteask: package-lock.json: generated file, +8 -8 lines, hunks omitted]"
            )
        );
        assert!(!got.contains("\"v\": 3"));
    }

    #[test]
    fn vendor_children_become_a_count() {
        let mut input = String::from("./src/a.rs\n");
        for i in 0..6 {
            input.push_str(&format!("./node_modules/p{i}/index.js\n"));
        }
        let got = vendor_ls(&input).unwrap();
        assert!(got.contains("./src/a.rs"));
        assert!(got.contains("./node_modules/ [byteask: 6 entries below omitted]"));
        assert!(!got.contains("p3"));
    }

    #[test]
    fn html_loses_scripts_only() {
        let input = "<!DOCTYPE html><html><head><script>var a = 1; var b = 2; var c = 3; var d = 4; var e = 5;</script></head><body><p>Hello</p></body></html>";
        let got = html_text(input).unwrap();
        assert!(got.contains("<p>Hello</p>"));
        assert!(!got.contains("var a"));
        assert_eq!(html_text("plain text <script>x</script>"), None);
    }

    #[test]
    fn blobs_are_cut_and_source_lines_are_not() {
        let blob = "A".repeat(5000);
        let got = blob_line(&format!("x\n{blob}\ny")).unwrap();
        assert!(got.contains("continues for 4800 more bytes"));
        let code = format!("let s = \"{}\";", "a ".repeat(2000));
        assert_eq!(blob_line(&code), None);
        // Real minified code has a few spaces in it and must still count as a blob.
        let minified = format!("app.min.js:1:var a={};return a", "f(1);".repeat(900));
        assert!(blob_line(&minified).is_some());
        // A long line of ordinary prose does not.
        let prose = "the quick brown fox jumps over the lazy dog ".repeat(80);
        assert_eq!(blob_line(&prose), None);
    }

    #[test]
    fn tables_and_log_prefixes() {
        let rows: Vec<String> = (0..100).map(|i| format!("{i},a{i},b{i}")).collect();
        let got = table(&rows.join("\n")).unwrap();
        assert!(got.contains("[byteask: 70 of 100 table rows omitted]"));
        let logs: Vec<String> = (0..30)
            .map(|i| format!("2026-09-19T10:00:{i:02} INFO msg {i}"))
            .collect();
        let got = log_prefix(&logs.join("\n")).unwrap();
        assert!(got.starts_with(
            "[byteask: every line below began with \"2026-09-19T\", removed]\n10:00:00 INFO msg 0"
        ));
    }

    #[test]
    fn the_guard_refuses_to_drop_unseen_words() {
        assert!(covered(&["alpha beta"], &["alpha beta gamma"]));
        assert!(!covered(&["alpha secretname"], &["alpha"]));
    }
}
