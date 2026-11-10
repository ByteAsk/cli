//! Unified-diff parsing for `cpp_impact`: which lines changed, and the pre/post text of each
//! hunk (with context) for the generated-copy check.

/// One hunk of a unified diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// First line and count on the old side (`-a,b`).
    pub old_start: u32,
    pub old_count: u32,
    /// First line and count on the new side (`+c,d`).
    pub new_start: u32,
    pub new_count: u32,
    /// Old-side text: context and removed lines, in order.
    pub old_lines: Vec<String>,
    /// New-side text: context and added lines, in order.
    pub new_lines: Vec<String>,
    /// New-side line numbers of added lines.
    pub added: Vec<u32>,
    /// Old-side line numbers of removed lines.
    pub removed: Vec<u32>,
}

/// Parse the hunks of a single-file unified diff (`git diff -U<n> base -- path`).
pub(crate) fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            let mut hunk = Hunk::default();
            for part in rest.split_whitespace() {
                if let Some(old) = part.strip_prefix('-') {
                    let (start, count) = range(old);
                    hunk.old_start = start;
                    hunk.old_count = count;
                } else if let Some(new) = part.strip_prefix('+') {
                    let (start, count) = range(new);
                    hunk.new_start = start;
                    hunk.new_count = count;
                } else if part == "@@" {
                    break;
                }
            }
            old_line = hunk.old_start;
            new_line = hunk.new_start;
            current = Some(hunk);
            continue;
        }
        let Some(hunk) = current.as_mut() else {
            continue;
        };
        if line.starts_with("\\ ") {
            // "\ No newline at end of file"
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            hunk.new_lines.push(text.to_string());
            hunk.added.push(new_line);
            new_line += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            hunk.old_lines.push(text.to_string());
            hunk.removed.push(old_line);
            old_line += 1;
        } else {
            let text = line.strip_prefix(' ').unwrap_or(line);
            hunk.old_lines.push(text.to_string());
            hunk.new_lines.push(text.to_string());
            old_line += 1;
            new_line += 1;
        }
    }
    if let Some(hunk) = current.take() {
        hunks.push(hunk);
    }
    hunks
}

fn range(spec: &str) -> (u32, u32) {
    let mut parts = spec.splitn(2, ',');
    let start = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let count = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    (start, count)
}

/// New-side lines touched by the diff. A pure deletion contributes the line after it, so the
/// enclosing entity is still found.
pub(crate) fn touched_new_lines(hunks: &[Hunk]) -> Vec<u32> {
    let mut lines = Vec::new();
    for hunk in hunks {
        if hunk.added.is_empty() {
            lines.push(hunk.new_start.max(1));
        } else {
            lines.extend(hunk.added.iter().copied());
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// Whitespace-collapsed line, or `None` when it carries no content worth matching
/// (blank, a lone brace, a comment marker).
pub(crate) fn normalise_line(line: &str) -> Option<String> {
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() < 4 {
        return None;
    }
    if matches!(
        collapsed.as_str(),
        "} else {" | "};" | "break;" | "return;" | "#endif" | "#else"
    ) {
        return None;
    }
    if collapsed.starts_with("//") && collapsed.len() < 12 {
        return None;
    }
    Some(collapsed)
}

/// The normalised, non-trivial lines of a block.
pub(crate) fn normalise_block(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| normalise_line(line))
        .collect()
}

/// Does `haystack` contain `needle` as a contiguous run?
pub(crate) fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let first = &needle[0];
    haystack
        .iter()
        .enumerate()
        .filter(|(_, line)| *line == first)
        .any(|(pos, _)| haystack[pos..].starts_with(needle))
}

/// 1-based line (in the original file) where `needle` starts inside `lines`, using the
/// normalised view. `index` maps each normalised line back to its original line number.
pub(crate) fn find_run_line(haystack: &[(u32, String)], needle: &[String]) -> Option<u32> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find_map(|pos| {
        haystack[pos..pos + needle.len()]
            .iter()
            .map(|(_, text)| text)
            .eq(needle.iter())
            .then(|| haystack[pos].0)
    })
}

/// Normalised lines of a whole file with their line numbers.
pub(crate) fn normalised_with_lines(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .enumerate()
        .filter_map(|(pos, line)| normalise_line(line).map(|n| (pos as u32 + 1, n)))
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const DIFF: &str = "diff --git a/x.h b/x.h\n\
index 1..2 100644\n\
--- a/x.h\n\
+++ b/x.h\n\
@@ -10,4 +10,5 @@ inline int f() {\n\
 int a = 1;\n\
-int b = 2;\n\
+int b = 3;\n\
+int c = 4;\n\
 return a + b;\n\
 }\n\
@@ -40 +41,0 @@ void g()\n\
-  drop();\n";

    #[test]
    fn parses_hunks_with_context() {
        let hunks = parse_hunks(DIFF);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].added, vec![11, 12]);
        assert_eq!(hunks[0].removed, vec![11]);
        assert_eq!(
            hunks[0].old_lines,
            vec!["int a = 1;", "int b = 2;", "return a + b;", "}"]
        );
        assert_eq!(
            hunks[0].new_lines,
            vec![
                "int a = 1;",
                "int b = 3;",
                "int c = 4;",
                "return a + b;",
                "}"
            ]
        );
        assert_eq!(hunks[1].removed, vec![40]);
        assert!(hunks[1].added.is_empty());
        assert_eq!(touched_new_lines(&hunks), vec![11, 12, 41]);
    }

    #[test]
    fn runs_are_found_whitespace_insensitively() {
        let old = normalise_block(&[
            "  int a = 1;".to_string(),
            "int  b = 2;".to_string(),
            "}".to_string(),
        ]);
        assert_eq!(old, vec!["int a = 1;", "int b = 2;"]);
        let file = "x\n    int a = 1;\n\n  int b = 2;\ny\n";
        let lines = normalised_with_lines(file);
        let hay: Vec<String> = lines.iter().map(|(_, t)| t.clone()).collect();
        assert!(contains_run(&hay, &old));
        assert_eq!(find_run_line(&lines, &old), Some(2));
        assert!(!contains_run(
            &hay,
            &["int b = 2;".to_string(), "int a = 1;".to_string()]
        ));
    }
}
