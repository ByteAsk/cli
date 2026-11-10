//! ByteAsk apply_patch echo.
//!
//! 14% of file re-reads follow an edit: the model re-opens the file to check that the
//! patch landed. The echo puts the post-patch text around each applied hunk into the
//! apply_patch result itself, so that call is not needed. It shows what the file NOW
//! contains, not that the patch is right. Off unless the policy turns `patch_echo` on.

use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
use similar::TextDiff;

const CONTEXT: usize = 3;
/// Whole-echo cap: a huge patch must not turn a one-line result into a file dump.
const MAX_BYTES: usize = 6_000;
const MAX_LINES_PER_HUNK: usize = 40;

/// Post-patch line ranges around each change in one file, as `N: text` rows.
pub(crate) fn echo_update(shown_path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    for group in diff.grouped_ops(CONTEXT) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let start = first.new_range().start;
        let end = last.new_range().end.min(new_lines.len());
        if start >= end {
            // A pure deletion at the end of the file: nothing remains to show there.
            out.push_str(&format!("{shown_path}: lines removed near line {}\n", start + 1));
            continue;
        }
        out.push_str(&format!("{shown_path} now, lines {}-{end}:\n", start + 1));
        let shown_end = end.min(start + MAX_LINES_PER_HUNK);
        for (idx, line) in new_lines[start..shown_end].iter().enumerate() {
            out.push_str(&format!("{}: {line}\n", start + idx + 1));
        }
        if shown_end < end {
            out.push_str(&format!("[byteask: {} more lines of this hunk not shown]\n", end - shown_end));
        }
    }
    out
}

/// The echo for a whole applied patch, or an empty string when there is nothing to add.
pub(crate) fn echo(delta: &AppliedPatchDelta) -> String {
    let mut out = String::new();
    for change in delta.changes() {
        let AppliedPatchFileChange::Update { move_path, old_content, new_content, .. } = &change.change
        else {
            // An added file was just written by the model; a deleted one has nothing to show.
            continue;
        };
        let shown = move_path.as_ref().unwrap_or(&change.path).inferred_native_path_string();
        let part = echo_update(&shown, old_content, new_content);
        if out.len() + part.len() > MAX_BYTES {
            out.push_str("[byteask: the remaining files are not echoed; read them if you need to check them]\n");
            break;
        }
        out.push_str(&part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn file(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn the_echo_shows_the_new_text_with_three_lines_of_context() {
        let old = file(20);
        let new = old.replace("line 10\n", "line ten, changed\n");
        assert_eq!(
            echo_update("a.txt", &old, &new),
            "a.txt now, lines 7-13:\n7: line 7\n8: line 8\n9: line 9\n10: line ten, changed\n11: line 11\n12: line 12\n13: line 13\n"
        );
    }

    #[test]
    fn distant_changes_are_echoed_separately() {
        let old = file(60);
        let new = old.replace("line 5\n", "FIVE\n").replace("line 50\n", "FIFTY\n");
        let got = echo_update("a.txt", &old, &new);
        assert!(got.contains("a.txt now, lines 2-8:"));
        assert!(got.contains("a.txt now, lines 47-53:"));
        assert!(!got.contains("line 25"));
    }

    #[test]
    fn a_big_hunk_is_capped_and_says_so() {
        let old = file(5);
        let added: String = (0..100).map(|i| format!("new {i}\n")).collect();
        let new = format!("{old}{added}");
        let got = echo_update("a.txt", &old, &new);
        assert!(got.contains("more lines of this hunk not shown]"));
        assert!(got.lines().count() < 50);
    }

    #[test]
    fn an_unchanged_file_echoes_nothing() {
        assert_eq!(echo_update("a.txt", &file(5), &file(5)), "");
    }
}
