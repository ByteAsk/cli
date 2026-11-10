//! The three tiers.
//!
//! - Tier 3, the safe superset: every test whose program the build graph says
//!   the change reaches, every test the model cannot map to a program (script
//!   and data-driven tests), and tests whose command names a changed file.
//!   Everything, when a build file changed or there is no target map.
//! - Tier 2: the tier-3 tests whose own test sources mention a changed symbol
//!   (or were edited). Textual, so not safe; with no symbol hits it falls back
//!   to the tests one step from the change in the build graph.
//! - Tier 1: within tier 2, just the test cases around those mentions, when the
//!   framework can select single cases (GoogleTest, Catch, doctest).
//!
//! In a header-only (or single-header) library tier 3 is nearly everything; the
//! output then says tiers 1–2 are a guess and tier 3 is the real answer.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use super::Model;
use super::TestEntry;
use super::graph;
use super::graph::Affected;
use super::is_build_file;
use super::is_cpp_file;
use super::is_header;
use super::symbols::Changes;
use super::symbols::Framework;
use super::symbols::TestCase;
use super::symbols::case_at;
use super::symbols::glob_match;
use super::symbols::index_cases;
use super::symbols::word_hits;

/// Largest test source read for the symbol scan.
const TEST_FILE_BYTES_CAP: u64 = 16 * 1024 * 1024;

/// One tier-1 entry: a test, and the cases to run in it (none = all of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pick {
    pub test: usize,
    pub cases: Vec<String>,
    pub framework: Option<Framework>,
}

/// A mention of a changed symbol (or an edited line) in a test source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    pub file: PathBuf,
    pub line: u32,
    pub word: String,
    /// Index into the file's case list.
    pub case: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Selection {
    /// Tests not disabled.
    pub enabled: Vec<usize>,
    pub tier3: Vec<usize>,
    pub tier2: Vec<usize>,
    pub tier1: Vec<Pick>,
    pub warnings: Vec<String>,
    pub affected: Affected,
    /// Test cases of every scanned test source.
    pub cases: BTreeMap<PathBuf, Vec<TestCase>>,
    pub hits: Vec<Hit>,
    pub header_only: bool,
    /// Tests reached only because they are unmapped (script/data tests).
    pub unmapped_in_tier3: usize,
}

impl Selection {
    pub(crate) fn tier1_case_count(&self) -> usize {
        self.tier1.iter().map(|p| p.cases.len()).sum()
    }
}

/// Compute the tiers for `changes` in `model`.
pub(crate) async fn select(model: &Model, changes: &Changes) -> Selection {
    let mut sel = Selection {
        enabled: (0..model.tests.len()).filter(|i| !model.tests[*i].disabled).collect(),
        ..Selection::default()
    };
    sel.affected = graph::affected(model, &changes.files).await;
    let build_files = changes.touches_build_files();
    let cpp_changed = changes.cpp_files().next().is_some();
    let data_files: Vec<&PathBuf> = changes
        .files
        .iter()
        .filter(|f| !is_cpp_file(f) && !is_build_file(f))
        .collect();
    let no_map = model.targets.is_empty();

    // Tier 3.
    for &i in &sel.enabled {
        let test = &model.tests[i];
        let by_target = test.target.as_ref().is_some_and(|t| sel.affected.targets.contains(t));
        let by_make = model.make_rebuilds.as_ref().is_some_and(|rebuilt| {
            test.command
                .first()
                .is_some_and(|c| rebuilt.iter().any(|r| r == Path::new(c)))
        });
        let by_file = mentions_any(test, &data_files);
        let unmapped = test.target.is_none() && (cpp_changed || !data_files.is_empty());
        if !build_files.is_empty() || (no_map && cpp_changed) || by_target || by_make || by_file || unmapped {
            sel.tier3.push(i);
            if unmapped && !by_target && !by_file {
                sel.unmapped_in_tier3 += 1;
            }
        }
    }
    if !build_files.is_empty() {
        sel.warnings.push(format!(
            "a build file changed ({}): every test counts as affected",
            build_files
                .iter()
                .take(3)
                .map(|f| model.rel(f).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if sel.affected.scan_truncated {
        sel.warnings
            .push("the #include scan stopped at its file cap; header reach may be incomplete".to_string());
    }

    // Test sources of tier-3 tests, and symbol / edit hits in them.
    let mut files_of: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();
    for &i in &sel.tier3 {
        let Some(target) = model.tests[i].target.as_ref().and_then(|t| model.target(t)) else {
            continue;
        };
        files_of.insert(i, target.sources.clone());
    }
    let mut scanned: BTreeSet<PathBuf> = BTreeSet::new();
    for files in files_of.values() {
        for file in files {
            if !scanned.insert(file.clone()) {
                continue;
            }
            if std::fs::metadata(file).map(|m| m.len()).unwrap_or(u64::MAX) > TEST_FILE_BYTES_CAP {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let label = model.rel(file).to_string();
            let cases = index_cases(&text, &label);
            let testish = !cases.is_empty() || label.to_ascii_lowercase().contains("test");
            if !testish {
                continue;
            }
            for (line, word) in word_hits(&text, &changes.symbols) {
                sel.hits.push(Hit {
                    file: file.clone(),
                    line,
                    word,
                    case: case_at(&cases, line),
                });
            }
            if let Some(ranges) = changes.lines.get(file) {
                for (a, b) in ranges {
                    for (idx, case) in cases.iter().enumerate() {
                        if case.start <= *b && *a <= case.end {
                            sel.hits.push(Hit {
                                file: file.clone(),
                                line: case.start,
                                word: "(edited)".to_string(),
                                case: Some(idx),
                            });
                        }
                    }
                    if cases.iter().all(|c| !(c.start <= *b && *a <= c.end)) {
                        sel.hits.push(Hit {
                            file: file.clone(),
                            line: *a,
                            word: "(edited)".to_string(),
                            case: None,
                        });
                    }
                }
            }
            sel.cases.insert(file.clone(), cases);
        }
    }

    // Tier 2.
    let hit_files: BTreeSet<&PathBuf> = sel.hits.iter().map(|h| &h.file).collect();
    sel.tier2 = sel
        .tier3
        .iter()
        .copied()
        .filter(|i| files_of.get(i).is_some_and(|fs| fs.iter().any(|f| hit_files.contains(f))))
        .collect();
    // Script tests that name a changed file are as specific as it gets.
    for &i in &sel.tier3 {
        if mentions_any(&model.tests[i], &data_files) && !sel.tier2.contains(&i) {
            sel.tier2.push(i);
        }
    }
    if sel.tier2.is_empty() && !sel.tier3.is_empty() {
        let why = if changes.symbols.is_empty() {
            "no changed function/type names were found".to_string()
        } else {
            format!("no test source mentions {}", changes.symbols.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
        };
        // One step from the change: the test program compiles a changed file
        // (or an includer of a changed header), or links a target that does.
        let near: Vec<usize> = sel
            .tier3
            .iter()
            .copied()
            .filter(|i| {
                model.tests[*i]
                    .target
                    .as_ref()
                    .and_then(|t| model.target(t))
                    .is_some_and(|t| {
                        sel.affected.direct.contains(&t.name)
                            || t.deps.iter().any(|d| sel.affected.direct.contains(d))
                    })
            })
            .collect();
        if near.is_empty() {
            sel.tier2 = sel.tier3.clone();
            sel.warnings.push(format!("{why}; tiers 1-2 = tier 3"));
        } else {
            sel.tier2 = near;
            sel.warnings.push(format!(
                "{why}; tiers 1-2 = the tests that compile or directly link the changed code"
            ));
        }
    }
    sel.tier2.sort_unstable();

    // Tier 1.
    for &i in &sel.tier2 {
        let test = &model.tests[i];
        let files = files_of.get(&i).cloned().unwrap_or_default();
        let hits: Vec<&Hit> = sel.hits.iter().filter(|h| files.contains(&h.file)).collect();
        let hit_cases: Vec<&TestCase> = hits
            .iter()
            .filter_map(|h| h.case.and_then(|c| sel.cases.get(&h.file).and_then(|cs| cs.get(c))))
            .collect();
        // A test registered per case (gtest_discover_tests and the like).
        if let Some(id) = registered_case(test) {
            let wanted = hit_cases.is_empty()
                || hit_cases
                    .iter()
                    .any(|c| glob_match(&c.filter, &id) || glob_match(&id, &c.filter) || c.name == id);
            if wanted {
                sel.tier1.push(Pick {
                    test: i,
                    cases: Vec::new(),
                    framework: None,
                });
            }
            continue;
        }
        let outside = hits.iter().any(|h| h.case.is_none()) || hits.is_empty();
        let framework = hit_cases.first().map(|c| c.framework);
        let direct = runs_its_own_program(model, test);
        if outside || !framework.is_some_and(Framework::filterable) || !direct {
            sel.tier1.push(Pick {
                test: i,
                cases: Vec::new(),
                framework: None,
            });
            continue;
        }
        let mut cases: Vec<String> = Vec::new();
        for case in hit_cases {
            if Some(case.framework) == framework && !cases.contains(&case.filter) {
                cases.push(case.filter.clone());
            }
        }
        sel.tier1.push(Pick {
            test: i,
            cases,
            framework,
        });
    }

    let all_headers = changes.cpp_files().all(|f| is_header(f)) && cpp_changed;
    if all_headers && sel.tier3.len() >= 3 && sel.tier3.len() * 4 >= sel.enabled.len() * 3 {
        sel.header_only = true;
        sel.warnings.push(format!(
            "header-only change: the build graph reaches {} of {} tests, so tiers 1-2 are picked by \
             name only and are NOT safe; run tier 3 (or the full suite) once before finishing",
            sel.tier3.len(),
            sel.enabled.len()
        ));
    }
    sel
}

/// The test's command names one of `files` (by path or file name).
fn mentions_any(test: &TestEntry, files: &[&PathBuf]) -> bool {
    files.iter().any(|f| {
        let full = f.display().to_string();
        let Some(name) = f.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return false;
        };
        test.command.iter().skip(1).any(|arg| {
            *arg == full
                || *arg == name
                || arg.ends_with(&format!("/{name}"))
                || arg.ends_with(&format!("={name}"))
        })
    })
}

/// The case id of a test registered for a single case: `--gtest_filter=X`,
/// `--test-case=X`, or a Catch test spec equal to the test's name.
pub(crate) fn registered_case(test: &TestEntry) -> Option<String> {
    for arg in test.command.iter().skip(1) {
        for prefix in ["--gtest_filter=", "--test-case=", "-tc="] {
            if let Some(value) = arg.strip_prefix(prefix) {
                return Some(value.to_string());
            }
        }
    }
    let spec = test.command.get(1)?;
    (!spec.starts_with('-') && (test.name == *spec || test.name.ends_with(spec.as_str()))).then(|| spec.clone())
}

/// The test runs the program its target builds (so the program can be run
/// directly with a case filter), not a wrapper script or interpreter.
pub(crate) fn runs_its_own_program(model: &Model, test: &TestEntry) -> bool {
    let Some(target) = test.target.as_ref().and_then(|t| model.target(t)) else {
        return false;
    };
    let Some(first) = test.command.first() else {
        return false;
    };
    target
        .artifacts
        .iter()
        .any(|a| a == Path::new(first) || graph::canonical(a) == graph::canonical(Path::new(first)))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::testsel::DirKind;
    use crate::testsel::System;
    use crate::testsel::Target;

    #[test]
    fn recognises_per_case_registrations() {
        let t = |name: &str, args: &[&str]| TestEntry {
            name: name.to_string(),
            command: std::iter::once("/b/prog").chain(args.iter().copied()).map(str::to_string).collect(),
            ..TestEntry::default()
        };
        assert_eq!(registered_case(&t("Core.Twice", &["--gtest_filter=Core.Twice"])).as_deref(), Some("Core.Twice"));
        assert_eq!(registered_case(&t("parses x", &["parses x", "--reporter", "xml"])).as_deref(), Some("parses x"));
        assert_eq!(registered_case(&t("whole", &["--verbose"])), None);
        assert_eq!(registered_case(&t("whole", &[])), None);
    }

    #[tokio::test]
    async fn tiers_narrow_from_graph_to_symbols_to_cases() {
        let dir = tempfile::tempdir().unwrap();
        let root = graph::canonical(dir.path());
        let files = [
            ("src/core.cpp", "int twice(int x) { return 2 * x; }\nint add(int a, int b) { return a + b; }\n"),
            ("tests/core_test.cpp", "TEST(Core, Twice) {\n  EXPECT_EQ(twice(2), 4);\n}\nTEST(Core, Add) {\n  EXPECT_EQ(add(1, 2), 3);\n}\n"),
            ("tests/api_test.cpp", "TEST(Api, Add) {\n  EXPECT_EQ(add(1, 1), 2);\n}\n"),
            ("tests/other_test.cpp", "TEST(Other, X) {\n}\n"),
        ];
        for (path, text) in files {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        }
        let target = |name: &str, exe: bool, src: &str, deps: &[&str]| Target {
            name: name.to_string(),
            is_exe: exe,
            artifacts: vec![root.join("build").join(name)],
            sources: vec![root.join(src)],
            deps: deps.iter().map(|d| (*d).to_string()).collect(),
        };
        let test = |name: &str| TestEntry {
            name: name.to_string(),
            command: vec![root.join("build").join(name).display().to_string()],
            target: Some(name.to_string()),
            ..TestEntry::default()
        };
        let model = Model {
            system: System::CMake,
            root: root.clone(),
            build_dir: root.join("build"),
            dir_kind: DirKind::Scratch,
            version: None,
            ninja: false,
            targets: vec![
                target("core", false, "src/core.cpp", &[]),
                target("core_test", true, "tests/core_test.cpp", &["core"]),
                target("api_test", true, "tests/api_test.cpp", &["core"]),
                target("other_test", true, "tests/other_test.cpp", &[]),
            ],
            tests: vec![
                test("core_test"),
                test("api_test"),
                test("other_test"),
                TestEntry {
                    name: "script".to_string(),
                    command: vec!["/bin/sh".to_string(), "run.sh".to_string()],
                    ..TestEntry::default()
                },
            ],
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        };
        let changes = Changes {
            files: vec![root.join("src/core.cpp")],
            symbols: vec!["twice".to_string()],
            ..Changes::default()
        };
        let sel = select(&model, &changes).await;
        // core_test and api_test link core; the script test is unmapped.
        assert_eq!(sel.tier3, vec![0, 1, 3]);
        assert_eq!(sel.unmapped_in_tier3, 1);
        // Only core_test mentions `twice`.
        assert_eq!(sel.tier2, vec![0]);
        assert_eq!(
            sel.tier1,
            vec![Pick {
                test: 0,
                cases: vec!["Core.Twice".to_string()],
                framework: Some(Framework::GTest),
            }]
        );
        assert!(!sel.header_only);

        // No symbol mentioned anywhere: tiers 1-2 fall back to the tests one
        // step from the change.
        let changes = Changes {
            files: vec![root.join("src/core.cpp")],
            symbols: vec!["nowhere_mentioned".to_string()],
            ..Changes::default()
        };
        let sel = select(&model, &changes).await;
        assert_eq!(sel.tier2, vec![0, 1]);
        assert!(sel.warnings.iter().any(|w| w.contains("no test source mentions")), "{:?}", sel.warnings);
    }
}
