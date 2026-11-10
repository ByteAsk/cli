//! Class L beyond `ansi` and `cr`: transforms that keep every fact, but only on a
//! POSITIVE family match, because on source text the model patches by line and even
//! whitespace is content.
//!
//! Every function returns `Some(new_text)` only when it changed something.

/// Marker line for run-length encoding. On its own line: a suffix on the repeated line
/// itself could be mistaken for output.
fn rle_marker(n: usize) -> String {
    format!("[byteask: previous line repeated {n} more times]")
}

/// Identical consecutive lines -> the line once, then a marker. Runs shorter than 3 are
/// left alone (the marker would cost more than it saves).
pub fn rle(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        if run >= 3 && !lines[i].trim().is_empty() {
            out.push(lines[i].to_string());
            out.push(rle_marker(run - 1));
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

/// Trailing whitespace removed; runs of 3+ blank lines become one blank line.
pub fn ws(input: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    let mut changed = false;
    for line in input.split('\n') {
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.len() != line.len() {
            changed = true;
        }
        if trimmed.is_empty() {
            blank_run += 1;
            continue;
        }
        if blank_run > 0 {
            let keep = if blank_run >= 3 { 1 } else { blank_run };
            if keep != blank_run {
                changed = true;
            }
            out.extend(std::iter::repeat_n("", keep));
            blank_run = 0;
        }
        out.push(trimmed);
    }
    // Preserve a single trailing newline if the input had one.
    if blank_run > 0 {
        let keep = if blank_run >= 3 { 1 } else { blank_run };
        if keep != blank_run {
            changed = true;
        }
        out.extend(std::iter::repeat_n("", keep));
    }
    changed.then(|| out.join("\n"))
}

/// `path:line:text` x N -> ripgrep's own `--heading` layout: the path once, then
/// `line:text` rows, a blank line between files.
///
/// Refuses (returns `None`) unless EVERY non-empty line parses unambiguously and every
/// path passes `path_exists`. A path containing `:` makes the split ambiguous, so such
/// output is left alone.
pub fn search_group(input: &str, path_exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut groups: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    let mut rows = 0usize;
    for line in input.split('\n') {
        if line.is_empty() {
            continue;
        }
        let (path, rest) = line.split_once(':')?;
        let (lineno, text) = rest.split_once(':')?;
        if path.is_empty()
            || lineno.is_empty()
            || !lineno.bytes().all(|b| b.is_ascii_digit())
            || path.starts_with(' ')
        {
            return None;
        }
        match groups.last_mut() {
            Some((p, v)) if *p == path => v.push((lineno, text)),
            _ => groups.push((path, vec![(lineno, text)])),
        }
        rows += 1;
    }
    // Nothing to gain unless paths repeat.
    if rows < 4 || groups.len() == rows {
        return None;
    }
    // The same path must not come back later: grouped output would reorder matches.
    let mut seen = std::collections::BTreeSet::new();
    for (path, _) in &groups {
        if !seen.insert(*path) || !path_exists(path) {
            return None;
        }
    }
    let mut out = String::with_capacity(input.len());
    for (idx, (path, matches)) in groups.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(path);
        out.push('\n');
        for (lineno, text) in matches {
            out.push_str(lineno);
            out.push(':');
            out.push_str(text);
            out.push('\n');
        }
    }
    Some(out)
}

/// Listings: fold the directory prefix shared by every line.
pub fn path_fold(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.len() < 8 {
        return None;
    }
    // Every line must look like a bare path: no spaces-then-text columns (`ls -l`).
    // A path with a space in it is possible but rare; a listing with columns is common.
    if lines.iter().any(|l| l.contains([' ', '\t'])) {
        return None;
    }
    let first = lines[0];
    let mut prefix_len = first.rfind('/').map(|i| i + 1)?;
    for line in &lines[1..] {
        let common = first
            .bytes()
            .zip(line.bytes())
            .take(prefix_len)
            .take_while(|(a, b)| a == b)
            .count();
        prefix_len = first[..common].rfind('/').map(|i| i + 1).unwrap_or(0);
        if prefix_len == 0 {
            return None;
        }
    }
    let prefix = &first[..prefix_len];
    // `./` alone is not worth a header line.
    if prefix.len() < 8 {
        return None;
    }
    let mut out = format!("[byteask: every path below is relative to {prefix}]\n");
    for line in input.split('\n') {
        match line.strip_prefix(prefix) {
            Some(rest) if !rest.is_empty() => out.push_str(rest),
            Some(_) => out.push('.'),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    if input.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rle_folds_long_runs_and_leaves_short_ones() {
        let input = "a\nwarn\nwarn\nwarn\nwarn\nb\nb\nc";
        assert_eq!(
            rle(input).unwrap(),
            "a\nwarn\n[byteask: previous line repeated 3 more times]\nb\nb\nc"
        );
        assert_eq!(rle("a\nb\nb\nc"), None);
        assert_eq!(rle("x\n\n\n\n\ny"), None, "blank runs are ws's job");
    }

    #[test]
    fn ws_trims_and_collapses_but_keeps_short_gaps() {
        assert_eq!(ws("a  \nb\n\n\n\n\nc\n").unwrap(), "a\nb\n\nc\n");
        assert_eq!(ws("a\n\nb\n"), None);
    }

    #[test]
    fn search_group_matches_ripgrep_heading_layout() {
        let input = "src/a.rs:10:fn a()\nsrc/a.rs:22:fn b()\nsrc/a.rs:30:x\nsrc/b.rs:5:fn c()\n";
        let got = search_group(input, &|_| true).unwrap();
        assert_eq!(got, "src/a.rs\n10:fn a()\n22:fn b()\n30:x\n\nsrc/b.rs\n5:fn c()\n");
    }

    #[test]
    fn search_group_refuses_anything_ambiguous() {
        let ok = "a.rs:1:x\na.rs:2:y\na.rs:3:z\nb.rs:4:w\n";
        assert!(search_group(ok, &|_| true).is_some());
        assert_eq!(search_group(ok, &|p| p != "b.rs"), None, "a path that does not exist");
        assert_eq!(search_group("a.rs:1:x\na.rs:2:y\nnot a match line\na.rs:3:z\n", &|_| true), None);
        assert_eq!(search_group("a.rs:1:x\nb.rs:2:y\na.rs:3:z\nb.rs:9:q\n", &|_| true), None, "path returns later");
        assert_eq!(search_group("a.rs:x:1\na.rs:2:y\na.rs:3:z\na.rs:4:w\n", &|_| true), None);
    }

    #[test]
    fn search_group_keeps_colons_inside_the_matched_text() {
        let input = "a.rs:1:let x: u8 = 1;\na.rs:2:y::z\na.rs:3:q\na.rs:4:w\n";
        assert_eq!(
            search_group(input, &|_| true).unwrap(),
            "a.rs\n1:let x: u8 = 1;\n2:y::z\n3:q\n4:w\n"
        );
    }

    #[test]
    fn path_fold_states_the_prefix_once() {
        let mut input = String::new();
        for i in 0..10 {
            input.push_str(&format!("/home/u/project/src/mod{i}/file.rs\n"));
        }
        let got = path_fold(&input).unwrap();
        assert!(got.starts_with("[byteask: every path below is relative to /home/u/project/src/]\n"));
        assert!(got.contains("\nmod3/file.rs\n"));
        assert!(!got.contains("/home/u/project/src/mod3"));
    }

    #[test]
    fn path_fold_refuses_short_or_columnar_listings() {
        assert_eq!(path_fold("/a/b/c\n/a/b/d\n"), None);
        let mut cols = String::new();
        for i in 0..10 {
            cols.push_str(&format!("-rw-r--r-- 1 u u 10 Sep 1 /long/prefix/dir/f{i}\n"));
        }
        assert_eq!(path_fold(&cols), None);
    }
}
