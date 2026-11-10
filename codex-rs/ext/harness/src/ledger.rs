//! The constraint ledger: the constraints a user (or the repository's config) sets on a
//! change, kept in one block that survives compaction and checked against the final diff.
//!
//! Extraction is mechanical (no model call): a sentence or bullet of the user's
//! own text that opens with a prohibition or preservation cue ("Do not", "Never", "Keep",
//! "Avoid", "Only", "No …") is a constraint. Text after an issue marker (`ISSUE:`, `TASK:`) is
//! the task itself, not a rule about the change, and is skipped, as the Context card hook does.
//!
//! Checking is mechanical where the constraint names paths ("Do not modify files under tests/"):
//! the changed files are compared with it and a violation is shown with the file names. Every
//! other constraint is quoted back once, next to the list of changed files, for the model to
//! check its own diff against.

use std::path::Path;

/// Most constraints kept (the first ones the user wrote).
const MAX_CONSTRAINTS: usize = 8;
/// Longest constraint kept, in characters.
const MAX_CONSTRAINT_CHARS: usize = 240;
/// Markers after which the text is the task itself (same list as the Context card hook).
const TASK_MARKERS: &[&str] = &["\nISSUE:\n", "\nTASK:\n", "\nIssue:\n"];
/// Opening words that make a sentence a constraint on the change (compared lowercase).
const CUES: &[&str] = &[
    "do not ",
    "don't ",
    "dont ",
    "never ",
    "must not ",
    "mustn't ",
    "should not ",
    "shouldn't ",
    "no ",
    "keep ",
    "avoid ",
    "only ",
    "without ",
    "preserve ",
    "leave ",
];
/// Verbs that make a prohibition about files.
const EDIT_VERBS: &[&str] = &[
    "modify", "change", "edit", "touch", "add", "create", "delete", "remove", "rename", "write",
];

/// One constraint, as the user wrote it, with what can be checked mechanically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    pub text: String,
    /// Path prefixes or directory names the change must not touch (empty = not mechanical).
    pub forbidden: Vec<PathRule>,
}

/// A path rule derived from a constraint's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathRule {
    /// A path prefix relative to the repository root, e.g. `tests/` or `src/vendor/`.
    Prefix(String),
    /// Any path with a directory component named like a test directory.
    TestDirectories,
}

impl PathRule {
    fn matches(&self, path: &str) -> bool {
        match self {
            PathRule::Prefix(prefix) => {
                let prefix = prefix.trim_start_matches("./");
                path.starts_with(prefix)
                    || (prefix.ends_with('/') && path.contains(&format!("/{prefix}")))
            }
            PathRule::TestDirectories => Path::new(path)
                .parent()
                .into_iter()
                .flat_map(Path::components)
                .any(|component| {
                    matches!(
                        component.as_os_str().to_str(),
                        Some("test" | "tests" | "testing" | "unittests" | "unit_tests")
                    )
                }),
        }
    }
}

/// The constraints in `text` (a user message), in order, without duplicates.
pub fn extract(text: &str) -> Vec<Constraint> {
    let mut rules_part = text;
    for marker in TASK_MARKERS {
        if let Some((head, _)) = text.split_once(marker) {
            if head.len() < rules_part.len() {
                rules_part = head;
            }
        }
    }
    let mut out: Vec<Constraint> = Vec::new();
    for line in rules_part.lines() {
        let line = strip_bullet(line.trim());
        for sentence in sentences(line) {
            let sentence = sentence.trim();
            let lower = sentence.to_lowercase();
            if !CUES.iter().any(|cue| lower.starts_with(cue)) {
                continue;
            }
            let text = clip(sentence);
            if out
                .iter()
                .any(|known| known.text.eq_ignore_ascii_case(&text))
            {
                continue;
            }
            out.push(Constraint {
                forbidden: path_rules(&lower),
                text,
            });
            if out.len() == MAX_CONSTRAINTS {
                return out;
            }
        }
    }
    out
}

/// Constraints from config (`[byteask_harness] constraints = [...]`, repository rules).
pub fn from_config(lines: &[String]) -> Vec<Constraint> {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .take(MAX_CONSTRAINTS)
        .map(|line| Constraint {
            forbidden: path_rules(&line.to_lowercase()),
            text: clip(line),
        })
        .collect()
}

/// Add `new` constraints to `ledger`, skipping ones already there. True when it grew.
pub fn merge(ledger: &mut Vec<Constraint>, new: Vec<Constraint>) -> bool {
    let before = ledger.len();
    for constraint in new {
        if ledger.len() >= MAX_CONSTRAINTS * 2 {
            break;
        }
        if !ledger
            .iter()
            .any(|known| known.text.eq_ignore_ascii_case(&constraint.text))
        {
            ledger.push(constraint);
        }
    }
    ledger.len() > before
}

/// The ledger block the model sees (after the user's message, and again after any compaction).
pub fn render_block(ledger: &[Constraint]) -> String {
    let mut out = String::from(
        "<constraint_ledger>\nConstraints set for this task (kept across context compaction; \
         your final diff is checked against them):\n",
    );
    for (index, constraint) in ledger.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", index + 1, constraint.text));
    }
    out.push_str("</constraint_ledger>");
    out
}

/// One changed file and its line counts (`None` for binary files).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// The finish-time check: `None` when there is nothing to say (no constraint, no change).
pub fn finish_check(ledger: &[Constraint], changed: &[ChangedFile]) -> Option<String> {
    if ledger.is_empty() || changed.is_empty() {
        return None;
    }
    let mut violations = Vec::new();
    let mut lines = Vec::new();
    for (index, constraint) in ledger.iter().enumerate() {
        let number = index + 1;
        if constraint.forbidden.is_empty() {
            lines.push(format!("{number}. {}", constraint.text));
            continue;
        }
        let hits: Vec<&str> = changed
            .iter()
            .map(|file| file.path.as_str())
            .filter(|path| constraint.forbidden.iter().any(|rule| rule.matches(path)))
            .collect();
        if hits.is_empty() {
            lines.push(format!(
                "{number}. {} — checked: no changed file falls under it.",
                constraint.text
            ));
        } else {
            lines.push(format!(
                "{number}. {} — VIOLATED: {} changed.",
                constraint.text,
                list(&hits)
            ));
            violations.push(number);
        }
    }
    let files: Vec<String> = changed.iter().map(describe).collect();
    let mut out = format!(
        "Constraint check before you finish (runs once). Your change touches: {}.\n",
        files.join(", ")
    );
    out.push_str(&lines.join("\n"));
    out.push('\n');
    if violations.is_empty() {
        out.push_str(
            "Check your diff (`git diff`) against each constraint above that has no \"checked\" \
             note. If one is not met, fix it; otherwise finish as you were going to.",
        );
    } else {
        let numbers: Vec<String> = violations.iter().map(ToString::to_string).collect();
        out.push_str(&format!(
            "Undo or move the changes that break constraint {} unless the user asked for them, \
             then check the rest against your diff.",
            numbers.join(", ")
        ));
    }
    Some(out)
}

/// Parse `git diff --numstat` output.
pub fn parse_numstat(text: &str) -> Vec<ChangedFile> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?.trim();
            if path.is_empty() {
                return None;
            }
            // A rename is printed as `old => new` (or `dir/{old => new}`): keep the new path.
            let path = rename_target(path);
            Some(ChangedFile {
                path,
                added: added.parse().ok(),
                removed: removed.parse().ok(),
            })
        })
        .collect()
}

fn rename_target(path: &str) -> String {
    if let (Some(open), Some(close)) = (path.find('{'), path.find('}'))
        && open < close
        && let Some((_, new)) = path[open + 1..close].split_once(" => ")
    {
        return format!("{}{}{}", &path[..open], new, &path[close + 1..]).replace("//", "/");
    }
    match path.split_once(" => ") {
        Some((_, new)) => new.to_string(),
        None => path.to_string(),
    }
}

fn describe(file: &ChangedFile) -> String {
    match (file.added, file.removed) {
        (Some(added), Some(removed)) => format!("{} (+{added} -{removed})", file.path),
        _ => format!("{} (binary)", file.path),
    }
}

fn list(paths: &[&str]) -> String {
    const SHOWN: usize = 5;
    let mut shown: Vec<&str> = paths.iter().copied().take(SHOWN).collect();
    let more = paths.len().saturating_sub(SHOWN);
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    shown.dedup();
    format!("{}{tail}", shown.join(", "))
}

fn strip_bullet(line: &str) -> &str {
    let line = line.trim_start_matches(['-', '*', '•', ' ']);
    // "1. " / "2) "
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// Split at sentence ends (". ", "! ", "? ", "; "), keeping the text of each sentence.
fn sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    for (index, window) in bytes.windows(2).enumerate() {
        if matches!(window[0], b'.' | b'!' | b'?' | b';') && window[1] == b' ' {
            out.push(&line[start..=index]);
            start = index + 2;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_CONSTRAINT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_CONSTRAINT_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Path rules in a prohibition about files ("do not modify files under tests/").
fn path_rules(lower: &str) -> Vec<PathRule> {
    let negative = [
        "do not ",
        "don't ",
        "dont ",
        "never ",
        "must not ",
        "mustn't ",
        "no ",
    ]
    .iter()
    .any(|cue| lower.starts_with(cue));
    if !negative || !EDIT_VERBS.iter().any(|verb| lower.contains(verb)) {
        return Vec::new();
    }
    let mut rules = Vec::new();
    if lower.contains("test director") || lower.contains("test folder") {
        rules.push(PathRule::TestDirectories);
    }
    for token in lower.split_whitespace() {
        let token = token.trim_matches(|c: char| {
            matches!(c, '`' | '"' | '\'' | ',' | '.' | ';' | ':' | '(' | ')')
        });
        if token.len() > 1
            && token.contains('/')
            && !token.contains("://")
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
        {
            let prefix = if token.ends_with('/') || token.contains('.') {
                token.to_string()
            } else {
                format!("{token}/")
            };
            rules.push(PathRule::Prefix(prefix));
        }
    }
    rules.dedup();
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const BENCH_STYLE: &str = "You are working in a C++ repository.\nResolve the issue by editing the \
        library's source code.\n\nRules:\n- Do not modify or add files under the test directories: the \
        maintainers' tests will be applied on top of your change.\n- Keep the change focused on the issue.\n\
        - Build the code and run the relevant existing tests.\n\nISSUE:\nThe parser must not crash. Do not \
        use exceptions here, says the reporter.\n";

    #[test]
    fn constraints_come_from_the_rules_not_from_the_issue() {
        let found = extract(BENCH_STYLE);
        let texts: Vec<&str> = found.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Do not modify or add files under the test directories: the maintainers' tests will \
                 be applied on top of your change.",
                "Keep the change focused on the issue.",
            ]
        );
        assert_eq!(found[0].forbidden, vec![PathRule::TestDirectories]);
        assert!(found[1].forbidden.is_empty());
    }

    #[test]
    fn inline_constraints_and_paths_are_found() {
        let found = extract(
            "Fix the overflow in Ring::push. Do not modify files under tests/ or third_party/zlib. \
             Keep the public API unchanged. No allocation on the hot path.",
        );
        let texts: Vec<&str> = found.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Do not modify files under tests/ or third_party/zlib.",
                "Keep the public API unchanged.",
                "No allocation on the hot path.",
            ]
        );
        assert_eq!(
            found[0].forbidden,
            vec![
                PathRule::Prefix("tests/".into()),
                PathRule::Prefix("third_party/zlib/".into())
            ]
        );
    }

    #[test]
    fn a_task_without_rules_has_an_empty_ledger() {
        assert!(extract("Make Ring::push overwrite the oldest element when full.").is_empty());
        // "Note" and other words merely starting with a cue's letters are not cues
        assert!(extract("Nothing else matters. Keeping it simple is fine.").is_empty());
    }

    fn changed(paths: &[&str]) -> Vec<ChangedFile> {
        paths
            .iter()
            .map(|path| ChangedFile {
                path: (*path).to_string(),
                added: Some(3),
                removed: Some(1),
            })
            .collect()
    }

    #[test]
    fn a_path_constraint_is_checked_mechanically() {
        let ledger = extract(BENCH_STYLE);
        let ok = finish_check(&ledger, &changed(&["src/ring.cpp"])).expect("message");
        assert!(ok.contains("src/ring.cpp (+3 -1)"), "{ok}");
        assert!(
            ok.contains("1. Do not modify or add files under the test directories"),
            "{ok}"
        );
        assert!(
            ok.contains("checked: no changed file falls under it"),
            "{ok}"
        );
        assert!(
            ok.contains("2. Keep the change focused on the issue."),
            "{ok}"
        );
        assert!(!ok.contains("VIOLATED"), "{ok}");

        let bad = finish_check(&ledger, &changed(&["src/ring.cpp", "tests/ring_test.cpp"]))
            .expect("message");
        assert!(
            bad.contains("VIOLATED: tests/ring_test.cpp changed"),
            "{bad}"
        );
        assert!(bad.contains("break constraint 1"), "{bad}");
        // nested test directories count too
        let nested = finish_check(&ledger, &changed(&["lib/core/test/x.cpp"])).expect("message");
        assert!(nested.contains("VIOLATED"), "{nested}");
    }

    #[test]
    fn nothing_to_check_without_a_change_or_a_constraint() {
        assert_eq!(finish_check(&extract(BENCH_STYLE), &[]), None);
        assert_eq!(finish_check(&[], &changed(&["a.cpp"])), None);
    }

    #[test]
    fn numstat_is_parsed_with_renames_and_binaries() {
        let files = parse_numstat("3\t1\tsrc/a.cpp\n-\t-\timg.png\n2\t0\tsrc/{old => new}/b.h\n");
        assert_eq!(
            files,
            vec![
                ChangedFile {
                    path: "src/a.cpp".into(),
                    added: Some(3),
                    removed: Some(1)
                },
                ChangedFile {
                    path: "img.png".into(),
                    added: None,
                    removed: None
                },
                ChangedFile {
                    path: "src/new/b.h".into(),
                    added: Some(2),
                    removed: Some(0)
                },
            ]
        );
    }

    #[test]
    fn the_block_lists_every_constraint() {
        let block = render_block(&extract(BENCH_STYLE));
        assert!(block.starts_with("<constraint_ledger>\n"), "{block}");
        assert!(
            block.contains("\n2. Keep the change focused on the issue.\n"),
            "{block}"
        );
        assert!(block.ends_with("</constraint_ledger>"), "{block}");
    }

    #[test]
    fn merge_skips_known_constraints() {
        let mut ledger = extract("Keep the API. Do not touch vendor/.");
        assert!(!merge(&mut ledger, extract("keep the API.")));
        assert!(merge(
            &mut ledger,
            from_config(&["No ABI change".to_string()])
        ));
        assert_eq!(ledger.len(), 3);
    }
}
