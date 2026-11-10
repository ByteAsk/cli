//! Model-facing texts of Tier B: the attempt brief with its strategy card, and the pairwise
//! judge prompt. Generic C/C++ engineering guidance only.

use std::path::Path;

use crate::report::REPORT_MARKER;

/// First words of every attempt brief (also how scripted test models recognise one).
pub const ATTEMPT_MARK: &str = "[ByteAsk independent attempt]";

/// Strategy cards for attempts 2, 3, 4, ... (attempt 1 is the unchanged normal session).
pub const STRATEGY_CARDS: &[&str] = &[
    "Fix the problem at the point where the invalid state or wrong value is first created, not \
     where the symptom shows up; keep the invariants that nearby comments, asserts and the \
     documentation state, and check every consumer of the value you change.",
    "Prefer extending the existing API family: make the change with the helpers, traits, error \
     types and naming that the sibling code already uses, and apply it to every sibling the \
     request covers (overloads, related types, flags, build configurations).",
    "Pin the required behaviour first: write a small test in the project's own framework that \
     fails on the current code, plus a neighbouring case that must keep working; then make the \
     change the documented contract calls for until both pass with the existing tests.",
];

pub fn strategy_card(attempt: usize) -> &'static str {
    let i = attempt.saturating_sub(2) % STRATEGY_CARDS.len();
    STRATEGY_CARDS[i]
}

/// The brief for attempt `attempt`: the task, the private worktree, the strategy card, and how
/// to hand back the change and any new tests.
pub fn attempt_brief(task: &str, attempt: usize, repo: &Path, worktree: &Path, tests_patch: &Path) -> String {
    format!(
        "{ATTEMPT_MARK}\n\
Resolve the task below. You work alone in a private copy of the repository.\n\n\
<task>\n{task}\n</task>\n\n---\n\
Where you work: the current directory, `{wt}`, is a private git worktree of the repository at its \
starting state. Wherever the task mentions the repository or its path (`{repo}`), use `{wt}` \
instead. Do not read or change `{repo}` or any other copy of the repository.\n\n\
Approach for this attempt: {card}\n\n\
When you are done:\n\
1. Leave the fix as uncommitted changes in this worktree (no commits, no branches). Do not \
change existing tests.\n\
2. If you wrote new tests, save only them (with their build registration) as a patch: \
`git add -N <new files> && git diff --binary -- <test files and registration> > {patch}`; then \
remove them from the worktree so that only the fix remains.\n\
3. End your final message with a line `{REPORT_MARKER}` followed by one line of JSON: \
{{\"build\": \"<command, run from a fresh checkout's root, that configures if needed and builds \
the project with its tests>\", \"test\": \"<command that runs the project's existing tests>\", \
\"run\": \"<command that builds and runs your new tests once their patch is applied; non-zero \
exit when they fail; empty if you saved none>\"}}\n",
        wt = worktree.display(),
        repo = repo.display(),
        card = strategy_card(attempt),
        patch = tests_patch.display(),
    )
}

pub fn judge_prompt(task: &str, first: (&str, &str), second: (&str, &str)) -> String {
    format!(
        "You are reviewing two candidate patches for the same task. Do not run commands or edit \
files; answer from the material below only.\n\n<task>\n{task}\n</task>\n\n\
<patch id=\"A\">\n{a_diff}\n</patch>\n<checks id=\"A\">\n{a_checks}\n</checks>\n\n\
<patch id=\"B\">\n{b_diff}\n</patch>\n<checks id=\"B\">\n{b_checks}\n</checks>\n\n\
Which patch more likely resolves the task correctly and completely: every case and sibling the \
task implies, the documented contracts and invariants of the code it touches kept, existing \
behaviour not broken? Judge the code, not its length. Reply with exactly one line: `WINNER: A` \
or `WINNER: B`.\n",
        a_diff = first.0,
        a_checks = first.1,
        b_diff = second.0,
        b_checks = second.1,
    )
}

/// `WINNER: A|B` from a judge's final message (the last occurrence counts).
pub fn parse_judge(message: &str) -> Option<bool> {
    let upper = message.to_ascii_uppercase();
    let idx = upper.rfind("WINNER:")?;
    let rest = upper[idx + "WINNER:".len()..].trim_start();
    match rest.chars().next()? {
        'A' => Some(true),
        'B' => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn cards_rotate() {
        assert_eq!(strategy_card(2), STRATEGY_CARDS[0]);
        assert_eq!(strategy_card(4), STRATEGY_CARDS[2]);
        assert_eq!(strategy_card(5), STRATEGY_CARDS[0]);
        let brief = attempt_brief("fix it", 3, Path::new("/repo"), Path::new("/w/wt-3"), Path::new("/w/t.patch"));
        assert!(brief.starts_with(ATTEMPT_MARK));
        assert!(brief.contains(REPORT_MARKER));
        assert!(brief.contains(STRATEGY_CARDS[1]));
        assert!(brief.contains("use `/w/wt-3` instead"));
        assert!(brief.contains("> /w/t.patch"));
    }

    #[test]
    fn judge_parsing() {
        assert_eq!(parse_judge("Reasoning...\nWINNER: A"), Some(true));
        assert_eq!(parse_judge("winner: b"), Some(false));
        assert_eq!(parse_judge("WINNER: A\n...actually WINNER: B"), Some(false));
        assert_eq!(parse_judge("no idea"), None);
        let p = judge_prompt("t", ("da", "ca"), ("db", "cb"));
        assert!(p.contains("<patch id=\"A\">\nda\n</patch>") && p.contains("<checks id=\"B\">\ncb\n</checks>"));
    }
}
