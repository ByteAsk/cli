//! Model-facing texts: the reproduction-test writer's brief, the delivery message for frozen
//! tests, and the engine-generated `run-repro.sh`. All generic C/C++ engineering guidance.

use std::path::Path;

use crate::report::REPORT_MARKER;

pub fn repro_brief(task: &str, worktree: &Path, tests_patch: &Path, max_steps: usize, minutes: u64) -> String {
    format!(
        "You are an independent test writer. Someone else is resolving the task below in a separate \
copy of this repository; you will never see their changes. Do NOT implement the fix.\n\n\
<task>\n{task}\n</task>\n\n---\n\
Your job: write 1-3 small tests, in the project's own test framework, style and assertion macros, \
that check the behaviour the task asks for. Work only in the current directory, `{wt}`: a private \
git worktree at the repository's starting state.\n\
1. Find how the project builds and runs its tests. Add your tests where similar tests live and \
register them in the build files if the framework needs that.\n\
2. Build and run them here. Keep ONLY tests that FAIL on this unmodified code: an assertion \
failure, or a compile error because the API the task asks for does not exist yet. Delete tests \
that pass. A test that fails for an unrelated reason (typo, wrong include, wrong path, wrong \
expectation) is not acceptable: fix or delete it.\n\
3. Change nothing except tests and their build registration.\n\
4. Save the kept tests as a patch: `git add -A -N . && git diff --binary > {patch}`.\n\
5. End your final message with a line `{REPORT_MARKER}` followed by one line of JSON: \
{{\"build\": \"<command, run from the worktree root, that builds the project including your \
tests>\", \"run\": \"<command that runs only your tests; non-zero exit when they fail>\", \
\"test\": \"<command that runs the project's existing tests, if you found one>\", \"tests\": \
[{{\"name\": \"<test name>\", \"fails_on_base_because\": \"assertion: ... | compile: missing \
...\"}}]}}\n\
Budget: about {max_steps} tool calls and {minutes} minutes. If you cannot produce a failing test \
within it, report \"tests\": [].\n",
        wt = worktree.display(),
        patch = tests_patch.display(),
    )
}

/// The message the main agent receives when frozen tests are ready.
pub fn repro_delivery(test_names: &[String], why: &str, patch: &Path, script: &Path) -> String {
    let names = if test_names.is_empty() {
        "(names not reported)".to_string()
    } else {
        test_names.join(", ")
    };
    format!(
        "[ByteAsk: independent reproduction tests are ready]\n\
An independent sub-agent (fresh context, no access to your changes) wrote tests in the project's \
own framework that FAIL on the unmodified repository ({why}). Treat them as target evidence: a \
correct fix should make them pass. They can be imperfect; if one contradicts the task text, the \
task text wins.\n\
Tests: {names}\n\
Frozen patch (read-only; never apply it to the repository you are editing, it must not end up in \
your final diff): {patch}\n\
To run them against your current changes: `sh {script}` (optionally with the directory to test \
as the first argument). It copies your current changes into a scratch worktree, applies the tests \
there, builds and runs them.\n",
        patch = patch.display(),
        script = script.display(),
    )
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The engine-generated runner for frozen tests. `$1` (default: the main repository) is the
/// tree whose current changes are tested; everything happens in a scratch worktree.
#[allow(clippy::too_many_arguments)]
pub fn run_repro_script(
    repo: &Path,
    snapshot: &str,
    patch: &Path,
    build: &str,
    run: &str,
    work_root: &Path,
    env: &[(String, String)],
) -> String {
    let exports: String = env
        .iter()
        .map(|(k, v)| format!("export {k}={}\n", sh_quote(v)))
        .collect();
    format!(
        r#"#!/bin/sh
# ByteAsk: run the frozen reproduction tests against the current changes of a tree, in a scratch
# worktree (the tree itself is never modified). Usage: sh run-repro.sh [TREE]
set -u
# Assignment context: no word splitting, and quote removal applies to the default.
TREE=${{1:-{repo}}}
SNAP={snap}
PATCH={patch}
ROOT={work_root}
{exports}key=$(printf '%s' "$TREE" | cksum | cut -d' ' -f1)
WT="$ROOT/run-$key"
mkdir -p "$ROOT"
if [ ! -d "$WT" ]; then
  git -C {repo} worktree add --detach "$WT" "$SNAP" >/dev/null 2>&1 || {{ echo "BYTEASK: cannot create a scratch worktree"; exit 2; }}
fi
cd "$WT" || exit 2
# Back to the snapshot: undo what the previous run applied (keeps build dirs for incremental builds).
[ -f "$WT.applied" ] && git apply -R --binary "$WT.applied" >/dev/null 2>&1
rm -f "$WT.applied"
IDX="$WT.idx"; rm -f "$IDX"
# Start from a copy of the tree's own index (fast: stat data is valid), else from the snapshot.
ix=$(git -C "$TREE" rev-parse --git-path index 2>/dev/null)
case "$ix" in /*) ;; *) ix="$TREE/$ix";; esac
cp "$ix" "$IDX" 2>/dev/null || GIT_INDEX_FILE="$IDX" git -C "$TREE" read-tree "$SNAP"
GIT_INDEX_FILE="$IDX" git -C "$TREE" add -A -- . ':(exclude)build' ':(exclude)build-*' ':(exclude)_build' ':(exclude)cmake-build-*' >/dev/null 2>&1
TREEOBJ=$(GIT_INDEX_FILE="$IDX" git -C "$TREE" write-tree) || {{ echo "BYTEASK: cannot read $TREE"; exit 2; }}
rm -f "$IDX"
git -C "$TREE" diff --binary "$SNAP^{{tree}}" "$TREEOBJ" > "$WT.changes"
cat "$WT.changes" "$PATCH" > "$WT.applied"
if ! git apply --binary "$WT.applied"; then
  echo "BYTEASK: the frozen tests do not apply on top of these changes (they touch the same lines)"; rm -f "$WT.applied"; exit 3
fi
echo "== build: {build_disp}"
( {build} ) || {{ echo "BYTEASK RESULT: FAIL (the tests do not build against these changes)"; exit 1; }}
echo "== run: {run_disp}"
if ( {run} ); then echo "BYTEASK RESULT: PASS"; exit 0; else echo "BYTEASK RESULT: FAIL"; exit 1; fi
"#,
        repo = sh_quote(&repo.to_string_lossy()),
        snap = sh_quote(snapshot),
        patch = sh_quote(&patch.to_string_lossy()),
        work_root = sh_quote(&work_root.to_string_lossy()),
        build_disp = build.replace('\n', " "),
        run_disp = run.replace('\n', " "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_and_delivery() {
        let p = Path::new("/tmp/x");
        let brief = repro_brief("fix it", p, p, 40, 10);
        assert!(brief.contains(REPORT_MARKER));
        assert!(brief.contains("Do NOT implement the fix"));
        let msg = repro_delivery(&["a".to_string()], "they fail when run", p, Path::new("/s.sh"));
        assert!(msg.contains("`sh /s.sh`"));
        assert!(msg.contains("must not end up in your final diff"));
    }

    #[test]
    fn script_quotes_paths() {
        let s = run_repro_script(
            Path::new("/r'o"),
            "abc",
            Path::new("/p"),
            "make",
            "./t",
            Path::new("/w"),
            &[("A".to_string(), "b c".to_string())],
        );
        assert!(s.contains("'/r'\\''o'"));
        assert!(s.contains("export A='b c'"));
    }
}
