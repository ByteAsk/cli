//! The check line: when a task arrives and ByteAsk Context names where the change belongs but
//! no test programs, pick the tests that cover that place from the project's own build (CMake
//! targets and sources, the include graph, `ctest --show-only=json-v1`) and give the model one
//! command that builds and runs only those, in the tree the background warm build prepares.
//!
//! The ranking: tests whose program compiles the place's file (or a file
//! that includes it) first, then tests named like the place's file, then the smallest program;
//! up to [`MAX_TESTS`]. Without a covering CTest test: the smallest test-like program that
//! links the file, run directly; else the smallest target that compiles it, built only.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use super::Model;
use super::exec::regex_escape;
use super::graph::Affected;
use super::tree::shell_quote;

/// Most tests the line runs.
pub(crate) const MAX_TESTS: usize = 8;

/// The command for the model and the targets it builds (which the warm build builds first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckPlan {
    pub line: String,
    pub targets: Vec<String>,
}

/// The check for a change at `place` (absolute path), given what `place` reaches in `model`.
pub(crate) fn plan(
    model: &Model,
    affected: &Affected,
    place: &Path,
    jobs: usize,
) -> Option<CheckPlan> {
    let stem = place
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let size = |target: &str| model.target(target).map_or(usize::MAX, |t| t.sources.len());
    let tree = shell_quote(&model.build_dir.to_string_lossy());
    let build = |targets: &[String]| {
        format!(
            "cmake --build {tree} --target {} -j{jobs}",
            targets
                .iter()
                .map(|t| shell_quote(t))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    let mut covering: Vec<(usize, &str, &str)> = model
        .tests
        .iter()
        .filter(|test| !test.disabled)
        .filter_map(|test| {
            let target = test.target.as_deref()?;
            affected.targets.contains(target).then_some((
                rank(&test.name, target, affected, &stem),
                test.name.as_str(),
                target,
            ))
        })
        .collect();
    if !covering.is_empty() {
        covering.sort_by_key(|(rank, name, target)| (*rank, size(target), *name));
        let total = covering.len();
        let picked = &covering[..total.min(MAX_TESTS)];
        let mut targets: Vec<String> = Vec::new();
        for (_, _, target) in picked {
            if !targets.iter().any(|t| t == target) {
                targets.push((*target).to_string());
            }
        }
        let pattern = format!(
            "^({})$",
            picked
                .iter()
                .map(|(_, name, _)| regex_escape(name))
                .collect::<Vec<_>>()
                .join("|")
        );
        let count = if total > picked.len() {
            format!(" ({} of the {total} tests that cover it)", picked.len())
        } else {
            String::new()
        };
        return Some(CheckPlan {
            line: format!(
                "To check your change{count}: {} && ctest --test-dir {tree} -R {} --output-on-failure",
                build(&targets),
                shell_quote(&pattern)
            ),
            targets,
        });
    }

    // No CTest test covers it: the smallest test-like program that links it, else the smallest
    // target that compiles it.
    let smallest = |names: &mut dyn Iterator<Item = &String>| {
        names
            .min_by_key(|name| (size(name), name.as_str()))
            .cloned()
    };
    let programs: BTreeSet<&String> = affected
        .targets
        .iter()
        .filter(|name| {
            model
                .target(name)
                .is_some_and(|t| t.is_exe && !t.artifacts.is_empty())
                && name.to_lowercase().contains("test")
        })
        .collect();
    if let Some(program) = smallest(&mut programs.into_iter()) {
        let artifact = model
            .target(&program)
            .and_then(|t| t.artifacts.first().cloned())
            .unwrap_or_default();
        let targets = vec![program];
        return Some(CheckPlan {
            line: format!(
                "To check your change: {} && {}",
                build(&targets),
                shell_quote(&artifact.to_string_lossy())
            ),
            targets,
        });
    }
    let target =
        smallest(&mut affected.direct.iter()).or_else(|| smallest(&mut affected.targets.iter()))?;
    let targets = vec![target];
    Some(CheckPlan {
        line: format!(
            "To check that your change compiles (no test covers it in this build): {}",
            build(&targets)
        ),
        targets,
    })
}

/// The nearest of `tests` (indices into `model.tests`) to a change of `changed` files, in the
/// order of [`rank`] (named like any changed file), then the smallest program, then name, while
/// their programs number at most `programs` (at least one test). Tests the build model maps to
/// no program are left out (they need the stated superset).
pub(crate) fn nearest(
    model: &Model,
    affected: &Affected,
    tests: &[usize],
    changed: &[PathBuf],
    programs: usize,
) -> Vec<usize> {
    let stems: Vec<String> = changed
        .iter()
        .filter_map(|f| f.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .filter(|s| !s.is_empty())
        .collect();
    let size = |target: &str| model.target(target).map_or(usize::MAX, |t| t.sources.len());
    let mut ranked: Vec<(usize, usize, &str, usize, &str)> = tests
        .iter()
        .filter_map(|&i| {
            let test = &model.tests[i];
            let target = test.target.as_deref()?;
            let best = stems
                .iter()
                .map(|stem| rank(&test.name, target, affected, stem))
                .min()
                .unwrap_or_else(|| rank(&test.name, target, affected, ""));
            Some((best, size(target), test.name.as_str(), i, target))
        })
        .collect();
    ranked.sort();
    let mut chosen = Vec::new();
    let mut targets: BTreeSet<&str> = BTreeSet::new();
    for (_, _, _, i, target) in ranked {
        if !targets.contains(target) && targets.len() >= programs.max(1) {
            continue;
        }
        targets.insert(target);
        chosen.push(i);
    }
    chosen
}

/// 0: the test's program compiles the place's file or a file including it, and the test is
/// named like the file; 1: compiles it; 2: named like it; 3: links it.
fn rank(test: &str, target: &str, affected: &Affected, stem: &str) -> usize {
    let direct = affected.direct.contains(target);
    let named = !stem.is_empty()
        && (test.to_lowercase().contains(stem) || target.to_lowercase().contains(stem));
    match (direct, named) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

/// The repository-relative files a Context pack names as place 1 ("1. name - path:12-30"),
/// as absolute paths under `root`.
pub fn place_files(pack: &str, root: &Path) -> Vec<PathBuf> {
    let mut in_places = false;
    for line in pack.lines() {
        let line = line.trim();
        if line.starts_with("## ") {
            in_places = line.starts_with("## Where the change");
            continue;
        }
        if !in_places {
            continue;
        }
        let Some(rest) = line.strip_prefix("1. ") else {
            continue;
        };
        // "<name> — <path>:<lines>  `<signature>` [...]": the path is the first word after the
        // dash that has a line number.
        let after_dash = rest
            .split_once(" \u{2014} ")
            .or_else(|| rest.split_once(" - "))
            .map(|(_, after)| after)
            .unwrap_or(rest);
        let word = after_dash.split_whitespace().next().unwrap_or("");
        let Some((path, lines)) = word.rsplit_once(':') else {
            return Vec::new();
        };
        if path.is_empty() || !lines.starts_with(|c: char| c.is_ascii_digit()) {
            return Vec::new();
        }
        let path = Path::new(path);
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Vec::new();
        }
        return vec![root.join(path)];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsel::DirKind;
    use crate::testsel::System;
    use crate::testsel::Target;
    use crate::testsel::TestEntry;

    fn model(tests: &[(&str, &str)]) -> Model {
        let target = |name: &str, is_exe: bool, sources: usize| Target {
            name: name.to_string(),
            is_exe,
            artifacts: vec![PathBuf::from(format!("/b/{name}"))],
            sources: (0..sources)
                .map(|i| PathBuf::from(format!("/r/{name}{i}.cpp")))
                .collect(),
            deps: Vec::new(),
        };
        Model {
            system: System::CMake,
            root: PathBuf::from("/r"),
            build_dir: PathBuf::from("/b"),
            dir_kind: DirKind::Scratch,
            version: Some((3, 25)),
            ninja: true,
            targets: vec![
                target("core", false, 40),
                target("sum_test", true, 2),
                target("big_test", true, 30),
                target("other_test", true, 1),
                target("tool", true, 3),
            ],
            tests: tests
                .iter()
                .map(|(name, target)| TestEntry {
                    name: name.to_string(),
                    target: Some(target.to_string()),
                    ..TestEntry::default()
                })
                .collect(),
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        }
    }

    fn affected(direct: &[&str], all: &[&str]) -> Affected {
        Affected {
            direct: direct.iter().map(|s| s.to_string()).collect(),
            targets: all.iter().map(|s| s.to_string()).collect(),
            ..Affected::default()
        }
    }

    #[test]
    fn covering_tests_are_ranked_and_capped() {
        let mut tests: Vec<(String, String)> = vec![
            ("big".into(), "big_test".into()),
            ("sum.copy".into(), "sum_test".into()),
            ("unrelated".into(), "other_test".into()),
        ];
        for i in 0..10 {
            tests.push((format!("big{i}"), "big_test".into()));
        }
        let tests: Vec<(&str, &str)> = tests
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let m = model(&tests);
        let a = affected(&["core", "sum_test"], &["core", "sum_test", "big_test"]);
        let plan = plan(&m, &a, Path::new("/r/include/fn/sum.hpp"), 3).expect("plan");
        assert_eq!(
            plan.targets,
            vec!["sum_test".to_string(), "big_test".to_string()]
        );
        assert!(
            plan.line.starts_with(
                "To check your change (8 of the 12 tests that cover it): cmake --build /b --target sum_test big_test -j3 && ctest --test-dir /b -R '^(sum\\.copy|big|big0|"
            ),
            "{}",
            plan.line
        );
        assert!(
            plan.line.ends_with(")$' --output-on-failure"),
            "{}",
            plan.line
        );
        assert!(!plan.line.contains("unrelated"));
    }

    #[test]
    fn the_nearest_tests_come_first_within_the_program_cap() {
        let m = model(&[
            ("sum.copy", "sum_test"),
            ("big", "big_test"),
            ("other", "other_test"),
            ("tool", "tool"),
            ("big2", "big_test"),
        ]);
        let a = affected(
            &["core", "sum_test"],
            &["core", "sum_test", "big_test", "other_test", "tool"],
        );
        let all = [0, 1, 2, 3, 4];
        let sum = [PathBuf::from("/r/include/fn/sum.hpp")];
        // sum.copy: compiles the change and is named like it; then the smallest programs.
        assert_eq!(nearest(&m, &a, &all, &sum, 2), vec![0, 2]);
        assert_eq!(nearest(&m, &a, &all, &sum, 3), vec![0, 2, 3]);
        // Tests sharing a program count once.
        let big = [PathBuf::from("/r/src/big.cpp")];
        let a = affected(&["big_test"], &["big_test", "sum_test"]);
        assert_eq!(nearest(&m, &a, &all, &big, 1), vec![1, 4]);
    }

    #[test]
    fn without_a_ctest_test_the_smallest_test_program_runs() {
        let m = model(&[]);
        let a = affected(&["core"], &["core", "big_test", "sum_test", "tool"]);
        let plan = plan(&m, &a, Path::new("/r/src/core0.cpp"), 2).expect("plan");
        assert_eq!(plan.targets, vec!["sum_test".to_string()]);
        assert_eq!(
            plan.line,
            "To check your change: cmake --build /b --target sum_test -j2 && /b/sum_test"
        );
    }

    #[test]
    fn without_any_test_it_only_compiles() {
        let m = model(&[]);
        let a = affected(&["core"], &["core", "tool"]);
        let compile = plan(&m, &a, Path::new("/r/src/core0.cpp"), 2).expect("plan");
        assert_eq!(compile.targets, vec!["core".to_string()]);
        assert!(
            compile
                .line
                .starts_with("To check that your change compiles"),
            "{}",
            compile.line
        );
        assert!(plan(&m, &affected(&[], &[]), Path::new("/r/x.cpp"), 2).is_none());
    }

    #[test]
    fn place_one_is_read_from_a_pack() {
        let root = Path::new("/w/repo");
        let pack = "github.com/libfn/functional @ 2adeb99345c4 (no edits synced)\n\
            # ByteAsk core pack\n\n## Where the change most likely belongs\n\
            1. fn::sum<T0...>::operator= \u{2014} include/fn/sum.hpp:452-461  `constexpr sum &operator=(...)` [why]\n\
            2. fn::sum<T0...>::_reinit \u{2014} include/fn/other.hpp:428-441\n\
            ## The code of 1 (include/fn/sum.hpp)\n";
        assert_eq!(
            place_files(pack, root),
            vec![root.join("include/fn/sum.hpp")]
        );
        let ascii =
            "## Where the change most likely belongs\n1. Book::cancel - src/book.cpp:40-58\n";
        assert_eq!(place_files(ascii, root), vec![root.join("src/book.cpp")]);
        assert!(
            place_files("1. a - src/a.cpp:1\n", root).is_empty(),
            "only under the places heading"
        );
        assert!(
            place_files(
                "## Where the change most likely belongs\n1. a - ../x.cpp:1\n",
                root
            )
            .is_empty()
        );
        assert!(
            place_files(
                "## Where the change most likely belongs\n1. a - /etc/x.cpp:1\n",
                root
            )
            .is_empty()
        );
    }
}
