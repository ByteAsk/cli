//! `cpp_finish_check`'s test selection from ByteAsk Context's `select_tests`, when the session has
//! Context on for this repository (`codex_core::byteask_context::select_tests_for`).
//!
//! The change since the session-start commit (tracked files, plus new untracked C/C++ files) goes
//! to `select_tests` as a unified diff (past 4 MB: the changed files); its `tests` and `also` CTest names (the programs that
//! compile, include or link the change) are mapped onto the check's own build tree. Anything the
//! answer cannot be trusted for falls back to the local build graph's selection (tier 3):
//! confidence `unknown`, a `stale` or `truncated` answer, an error or timeout, or an answer none of
//! whose test names this tree registers (names it does not register are dropped and named). The clean verify after the agent still runs the recipe's full suite.

use std::collections::BTreeSet;
use std::path::Path;

use codex_core::byteask_context::SelectInput;
use serde_json::Value;

use super::Model;
use super::is_cpp_file;

/// Largest diff sent (the server takes up to 8 MB); past it only the changed files are sent.
const MAX_DIFF_BYTES: usize = 4 << 20;
/// Most untracked files added to the diff.
const MAX_UNTRACKED: usize = 200;
/// Most changed files sent when the diff is too large (the server takes up to 5000).
const MAX_FILES: usize = 5000;

/// What Context chose, mapped onto the check's build tree.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pick {
    /// Indices into `model.tests`.
    pub tests: Vec<usize>,
    /// Targets to build when no test reaches the change (compile what it touches).
    pub targets: Vec<String>,
    /// Programs Context named (tests + also), for the answer line.
    pub programs: usize,
    /// CTest names Context gave that this tree does not register (built only in another
    /// configuration, so they cannot run here); dropped and named in the answer.
    pub unregistered: Vec<String>,
}

/// Context's choice for the change, `None` when Context is not on for this repository, or
/// `Some(Err(why))` when it is but its answer is not used.
pub(crate) async fn pick(root: &Path, head: &str, model: &Model) -> Option<Result<Pick, String>> {
    let select = codex_core::byteask_context::select_tests_for(root)?;
    let input = match change(root, head).await {
        Ok(input) => input,
        Err(err) => return Some(Err(err)),
    };
    Some(match select(input).await {
        Ok(answer) => from_answer(model, &answer),
        Err(err) => Err(format!("no answer ({err})")),
    })
}

async fn git(root: &Path, args: &[&str]) -> Result<(i32, String), String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| format!("git: {err}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// The change for `select_tests`: `git diff <head>` plus a new-file diff for each untracked C/C++
/// file (`git diff HEAD` leaves new files out; a new test file is exactly what selection must see),
/// or, past [`MAX_DIFF_BYTES`], just the changed files.
pub(crate) async fn change(root: &Path, head: &str) -> Result<SelectInput, String> {
    let (code, mut text) = git(root, &["diff", "--no-color", "--no-ext-diff", head, "--"]).await?;
    if code != 0 {
        return Err(format!("git diff {head} failed"));
    }
    let (_, untracked) = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    for file in untracked
        .split('\0')
        .filter(|f| !f.is_empty() && is_cpp_file(Path::new(f)))
        .take(MAX_UNTRACKED)
    {
        // Exit 1 is git's "files differ", the normal case here.
        let (_, new_file) = git(
            root,
            &["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null", file],
        )
        .await?;
        text.push_str(&new_file);
        if text.len() > MAX_DIFF_BYTES {
            break;
        }
    }
    if text.trim().is_empty() {
        return Err("no change to select tests for".to_string());
    }
    if text.len() <= MAX_DIFF_BYTES {
        return Ok(SelectInput::Diff(text));
    }
    let (_, tracked) = git(root, &["diff", "--name-only", "-z", head, "--"]).await?;
    let files: Vec<String> = tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|f| !f.is_empty())
        .take(MAX_FILES)
        .map(str::to_string)
        .collect();
    Ok(SelectInput::Files(files))
}

fn strings(value: Option<&Value>) -> impl Iterator<Item = String> + '_ {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
}

/// How Context's choice and the local build graph's were combined.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Combined {
    /// Only the tests both selections name (Context's compiler facts, the graph's own reach).
    Both,
    /// Context's choice alone: the graph has blind spots for this change (a changed header it sees no
    /// includer of, a cut include scan), or reaches no test.
    ContextOnly,
    /// The two disagree completely (no common test): both, rather than risk a miss.
    Union,
}

/// The tests to run from Context's choice and the local graph's tier 3 (indices into `model.tests`).
/// Measured on 46 fresh commits of fmt, googletest and cJSON: the
/// intersection missed none and was never slower than the local graph on 4 CPUs.
pub(crate) fn combine(context: &[usize], graph: &[usize], graph_blind: bool) -> (Vec<usize>, Combined) {
    if graph_blind || graph.is_empty() || context.is_empty() {
        return (context.to_vec(), Combined::ContextOnly);
    }
    let both: Vec<usize> = context.iter().copied().filter(|i| graph.contains(i)).collect();
    if both.is_empty() {
        let mut union: Vec<usize> = context.iter().chain(graph.iter()).copied().collect();
        union.sort_unstable();
        union.dedup();
        return (union, Combined::Union);
    }
    (both, Combined::Both)
}

/// `select_tests`'s JSON answer mapped onto `model`, or why it is not used.
pub(crate) fn from_answer(model: &Model, answer: &Value) -> Result<Pick, String> {
    let confidence = answer.get("confidence").and_then(Value::as_str).unwrap_or("");
    if confidence != "selected" {
        let reason = answer.get("reason").and_then(Value::as_str).unwrap_or("no reason given");
        return Err(format!("confidence {confidence:?} ({reason})"));
    }
    if answer.get("stale").and_then(Value::as_bool).unwrap_or(false) {
        return Err("its index is older than this commit (stale)".to_string());
    }
    if answer.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        return Err("the change reaches too many tests to list (truncated)".to_string());
    }
    let entries: Vec<&Value> = ["tests", "also"]
        .iter()
        .filter_map(|key| answer.get(*key).and_then(Value::as_array))
        .flatten()
        .collect();
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut programs: BTreeSet<String> = BTreeSet::new();
    for entry in &entries {
        names.extend(strings(entry.get("ctest")));
        if let Some(target) = entry.get("target").and_then(Value::as_str) {
            programs.insert(target.to_string());
        }
    }
    let mut tests = Vec::new();
    let mut unregistered = Vec::new();
    for name in &names {
        match model.tests.iter().position(|t| t.name == *name) {
            Some(i) => tests.push(i),
            None => unregistered.push(name.clone()),
        }
    }
    // A name this tree does not register cannot run here, so it is dropped; when none of the
    // names is registered the answer describes another configuration and is not used.
    if tests.is_empty() && !names.is_empty() {
        return Err(format!(
            "none of the tests it names is registered in this build tree (e.g. {})",
            names.iter().next().map(String::as_str).unwrap_or("?")
        ));
    }
    tests.sort_unstable();
    let targets = if tests.is_empty() {
        strings(answer.get("build_targets"))
            .chain(programs.iter().cloned())
            .filter(|t| model.target(t).is_some())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    Ok(Pick {
        tests,
        targets,
        programs: programs.len(),
        unregistered,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::testsel::DirKind;
    use crate::testsel::System;
    use crate::testsel::Target;
    use crate::testsel::TestEntry;

    fn model() -> Model {
        let target = |name: &str| Target {
            name: name.to_string(),
            is_exe: true,
            ..Target::default()
        };
        let test = |name: &str, target: &str| TestEntry {
            name: name.to_string(),
            target: Some(target.to_string()),
            ..TestEntry::default()
        };
        Model {
            system: System::CMake,
            root: PathBuf::from("/r"),
            build_dir: PathBuf::from("/b"),
            dir_kind: DirKind::Scratch,
            version: None,
            ninja: true,
            targets: vec![target("core"), target("a_test"), target("b_test")],
            tests: vec![test("a.one", "a_test"), test("a.two", "a_test"), test("b", "b_test")],
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        }
    }

    #[test]
    fn selected_answers_map_tests_and_also_onto_the_tree() {
        let answer = json!({
            "confidence": "selected", "stale": false, "truncated": false,
            "build_targets": ["a_test"],
            "tests": [{"target": "a_test", "ctest": ["a.two"], "tier": "direct", "why": "compiles book.cpp"}],
            "also": [{"target": "b_test", "ctest": ["b"], "tier": "links", "why": "links book"}],
        });
        assert_eq!(
            from_answer(&model(), &answer),
            Ok(Pick { tests: vec![1, 2], targets: vec![], programs: 2, unregistered: vec![] })
        );
        // A name this tree does not register is dropped (it cannot run here), the rest are used.
        let answer = json!({"confidence": "selected", "tests": [{"target": "b_test", "ctest": ["b", "py_only"]}]});
        assert_eq!(
            from_answer(&model(), &answer),
            Ok(Pick { tests: vec![2], targets: vec![], programs: 1, unregistered: vec!["py_only".to_string()] })
        );
    }

    #[test]
    fn no_test_reaches_the_change_builds_its_targets() {
        let answer = json!({"confidence": "selected", "build_targets": ["core", "gone"], "tests": [], "also": []});
        assert_eq!(
            from_answer(&model(), &answer),
            Ok(Pick { tests: vec![], targets: vec!["core".to_string()], programs: 0, unregistered: vec![] })
        );
    }

    #[test]
    fn context_and_graph_combine() {
        assert_eq!(combine(&[1, 2, 5], &[2, 3, 5, 7], false), (vec![2, 5], Combined::Both));
        // Blind spots in the graph, or no test it reaches: Context alone.
        assert_eq!(combine(&[1, 2], &[2, 3], true), (vec![1, 2], Combined::ContextOnly));
        assert_eq!(combine(&[1, 2], &[], false), (vec![1, 2], Combined::ContextOnly));
        // Context names no test (compile only): unchanged.
        assert_eq!(combine(&[], &[2, 3], false), (vec![], Combined::ContextOnly));
        // Complete disagreement: both sets.
        assert_eq!(combine(&[1], &[3], false), (vec![1, 3], Combined::Union));
    }

    #[test]
    fn untrusted_answers_fall_back() {
        let m = model();
        let why = |answer: Value| from_answer(&m, &answer).unwrap_err();
        assert!(why(json!({"confidence": "unknown", "reason": "no build model"})).contains("no build model"));
        assert!(why(json!({"confidence": "selected", "stale": true})).contains("stale"));
        assert!(why(json!({"confidence": "selected", "truncated": true})).contains("truncated"));
        assert!(
            why(json!({"confidence": "selected", "tests": [{"target": "x", "ctest": ["nope"]}]}))
                .contains("none of the tests it names is registered")
        );
    }
}
