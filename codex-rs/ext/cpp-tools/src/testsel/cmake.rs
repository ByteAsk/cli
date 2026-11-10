//! The CMake build model.
//!
//! Where builds happen:
//! - by default in this tool's OWN tree outside the project
//!   ([`crate::testsel::tree::own_tree`]), configured once per repository and
//!   commit (with the options of the project's existing build tree, when there
//!   is one, and the project's build-tests options turned on: [`test_options`])
//!   and then built incrementally; `cpp_finish_check`, `cpp_test_select`
//!   and the background warm build all use it, so one warms the others. The
//!   user's own build tree is never built in unless the caller names it;
//! - a tree the caller names (`build_dir`) is built incrementally and never
//!   reconfigured, deleted or written to by this module — not even a File API
//!   query file.
//!
//! Where the target map comes from, first that works:
//! 1. a File API codemodel-v2 reply already in the build tree (ours always has
//!    one: the query file is written before our configure);
//! 2. `CMakeFiles/<target>.dir/DependInfo.cmake` + `link.txt` (Makefile
//!    generators, any CMake version);
//! 3. a configure-only "mapping" tree outside the project with the existing
//!    tree's user cache entries and a File API query (CMake ≥ 3.14);
//! 4. nothing: every test counts as affected.
//!
//! Tests: `ctest --show-only=json-v1` (CMake ≥ 3.14), else `CTestTestfile.cmake`.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use super::DirKind;
use super::Model;
use super::System;
use super::Target;
use super::TestEntry;
use super::graph::canonical;
use super::normalize;
use crate::run::RunOutcome;
use crate::run::run_capped;

const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(900);
const CTEST_TIMEOUT: Duration = Duration::from_secs(120);
const QUERY_CLIENT: &str = "client-byteask";

/// Switch (off unless set): `BYTEASK_FINISH_CHECK_CFG2=1` changes how the C/C++ tools configure their own
/// CMake tree, for projects whose tree would otherwise register no tests or fail to configure:
/// - a test option whose name says it skips or disables tests or needs the network (SKIP, DISABLE, NO_, NETWORK)
///   is not turned on;
/// - the tree whose options ours copies may also be one configured from this project outside it
///   ([`outside_build_dirs`]), and a configure of ours that never succeeded starts again from scratch so they apply;
/// - before the test options are dropped, configure again with what the error names ([`ladder_fixes`]);
/// - test options are also read from first-level subfolder CMakeLists.txt files and from `-D<NAME>=ON` lines of
///   the top-level README.
const ENV_CFG2: &str = "BYTEASK_FINISH_CHECK_CFG2";

fn cfg2() -> bool {
    std::env::var(ENV_CFG2).is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
}

/// `CMakeCache.txt` entries: name → (type, value).
pub(crate) type Cache = BTreeMap<String, (String, String)>;

pub(crate) fn read_cache(dir: &Path) -> Option<Cache> {
    let text = std::fs::read_to_string(dir.join("CMakeCache.txt")).ok()?;
    let mut cache = Cache::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (name, kind) = match key.rsplit_once(':') {
            Some((name, kind)) => (name, kind),
            None => (key, "UNINITIALIZED"),
        };
        cache.insert(name.to_string(), (kind.to_string(), value.to_string()));
    }
    Some(cache)
}

/// The base-run cache's configuration key of a CMake build tree (see
/// [`crate::base_run_cache::BaseRunKey`]): generator, build type, compiler, flags and the
/// user cache entries. The finish gate computes the same key for the tree it reuses, so a
/// base result recorded by either is found by the other.
pub(crate) fn config_key(cache: &Cache) -> String {
    format!(
        "cmake|{}|{}|{}|{}|{}",
        cache_value(cache, "CMAKE_GENERATOR").unwrap_or(""),
        cache_value(cache, "CMAKE_BUILD_TYPE").unwrap_or(""),
        cache_value(cache, "CMAKE_CXX_COMPILER").unwrap_or(""),
        cache_value(cache, "CMAKE_CXX_FLAGS").unwrap_or(""),
        crate::base_run_cache::fnv(init_cache_text(cache).as_bytes())
    )
}

/// [`config_key`] of the build tree at `build_dir`, if it has a CMake cache.
pub(crate) fn tree_config_key(build_dir: &Path) -> Option<String> {
    read_cache(build_dir).map(|cache| config_key(&cache))
}

fn cache_value<'a>(cache: &'a Cache, name: &str) -> Option<&'a str> {
    cache.get(name).map(|(_, v)| v.as_str())
}

/// Build trees of this project that existed before this tool ran: the root,
/// its immediate subdirectories, and one level below `build*`/`out`/`cmake-build*`.
/// Only trees whose `CMAKE_HOME_DIRECTORY` lies inside `root`, newest cache first.
pub(crate) fn existing_build_dirs(root: &Path) -> Vec<PathBuf> {
    let root_c = canonical(root);
    let mut candidates = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            candidates.push(path.clone());
            if name.starts_with("build") || name == "out" || name.starts_with("cmake-build") || name == "_build" {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for sub in inner.flatten() {
                        if sub.path().is_dir() {
                            candidates.push(sub.path());
                        }
                    }
                }
            }
        }
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = candidates
        .into_iter()
        .filter_map(|dir| {
            let cache = read_cache(&dir)?;
            let home = cache_value(&cache, "CMAKE_HOME_DIRECTORY")?;
            if !canonical(Path::new(home)).starts_with(&root_c) {
                return None;
            }
            let mtime = std::fs::metadata(dir.join("CMakeCache.txt"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((mtime, dir))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.into_iter().map(|(_, dir)| dir).collect()
}

/// The tree whose options ours copies: the project's newest existing tree ([`existing_build_dirs`]); with CFG2,
/// the newest of those and of the trees configured from the project outside it ([`outside_build_dirs`]).
fn like_tree(root: &Path) -> Option<PathBuf> {
    let inside = existing_build_dirs(root);
    if !cfg2() {
        return inside.into_iter().next();
    }
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = inside
        .into_iter()
        .map(|dir| (cache_mtime(&dir), dir))
        .chain(outside_build_dirs(root))
        .collect();
    all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    all.into_iter().next().map(|(_, dir)| dir)
}

/// Marker in our tree: the outside trees (path and cache time) it was already configured like.
const LIKE_TRIED: &str = ".byteask-like-tried";

/// CFG2: when our configured tree `dir` registers no test (its CTestTestfile.cmake files), the newest tree
/// configured from the project outside it that registers some and was not tried yet (recorded in [`LIKE_TRIED`]).
fn test_bearing_outside_tree(root: &Path, dir: &Path) -> Option<PathBuf> {
    if !dir.join("CMakeCache.txt").is_file() || !ctest_testfile_tests(dir).is_empty() {
        return None;
    }
    let tried = std::fs::read_to_string(dir.join(LIKE_TRIED)).unwrap_or_default();
    let mut outside = outside_build_dirs(root);
    outside.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (mtime, like) in outside {
        let secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let key = format!("{} {secs}", like.display());
        if tried.lines().any(|line| line == key) || ctest_testfile_tests(&like).is_empty() {
            continue;
        }
        let _ = std::fs::write(dir.join(LIKE_TRIED), format!("{tried}{key}\n"));
        return Some(like);
    }
    None
}

fn cache_mtime(dir: &Path) -> std::time::SystemTime {
    std::fs::metadata(dir.join("CMakeCache.txt"))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// CFG2: build trees configured from the project at `root` outside it, as an agent does with
/// `cmake -S <root> -B /tmp/build ...`: every CMakeCache.txt at most 3 folders below /tmp or $HOME whose
/// CMAKE_HOME_DIRECTORY is `root`, the C/C++ tools' own trees excepted. At most 4,000 folders are looked at.
fn outside_build_dirs(root: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let root_c = canonical(root);
    let ours = canonical(&crate::gate::scratch_root());
    let mut starts = vec![PathBuf::from("/tmp")];
    if let Some(home) = std::env::var_os("HOME") {
        starts.push(PathBuf::from(home));
    }
    let mut out = Vec::new();
    let mut budget = 4000usize;
    for start in starts {
        let mut stack = vec![(start, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let dir_c = canonical(&dir);
            if dir_c.starts_with(&ours) || dir_c.starts_with(&root_c) {
                continue;
            }
            if let Some(cache) = read_cache(&dir) {
                if cache_value(&cache, "CMAKE_HOME_DIRECTORY").is_some_and(|home| canonical(Path::new(home)) == root_c) {
                    out.push((cache_mtime(&dir), dir));
                }
                continue;
            }
            if depth >= 3 {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.file_name() != ".git" {
                        stack.push((entry.path(), depth + 1));
                    }
                }
            }
        }
    }
    out
}

/// Inputs for [`model`].
pub(crate) struct Request<'a> {
    pub root: &'a Path,
    /// A build tree the caller named (used as an existing tree).
    pub build_dir: Option<&'a Path>,
    /// This tool's own directory for the project (outside it).
    pub scratch: &'a Path,
    pub version: Option<(u32, u32)>,
    /// Changed CMake files: a tree of ours older than one of them is
    /// configured again.
    pub cmake_files: &'a [PathBuf],
    /// The commit the change is measured against: our tree is configured again
    /// (incrementally) when it was last configured at another commit.
    pub commit: Option<&'a str>,
}

/// Build the CMake model, configuring a tree of our own only when needed.
pub(crate) async fn model(req: &Request<'_>) -> Result<Model, String> {
    let (build_dir, dir_kind, extra_note) = match req.build_dir {
        Some(dir) => {
            let dir = normalize(req.root, dir);
            if !dir.join("CMakeCache.txt").is_file() {
                return Err(format!(
                    "{} has no CMakeCache.txt, so it is not a configured CMake build tree.",
                    dir.display()
                ));
            }
            (dir, DirKind::Existing, None)
        }
        None => {
            let dir = req.scratch.join("build");
            let mut note = None;
            // CFG2: a configure of ours that never succeeded starts again from scratch, so the options of a
            // tree configured since (e.g. by the agent, outside the project) can apply
            if cfg2() && req.commit.is_some() && configured_at(&dir).is_none() && dir.join("CMakeCache.txt").is_file() {
                let _ = std::fs::remove_file(dir.join("CMakeCache.txt"));
            }
            if stale(&dir, req.cmake_files) || configured_at(&dir).as_deref() != req.commit {
                let like = like_tree(req.root);
                let testing = configure(req, &dir, like.as_deref(), true).await?;
                if let Some(commit) = req.commit {
                    let _ = std::fs::write(dir.join(CONFIGURED_AT), commit);
                }
                let mut text = match like {
                    Some(like) => format!(
                        "configured its own tree outside the project with the options of {} (your tree is never built in)",
                        like.strip_prefix(req.root).unwrap_or(&like).display()
                    ),
                    None => "configured its own tree outside the project (no existing build tree found)".to_string(),
                };
                if let Some(testing) = testing {
                    text.push_str(&format!("; {testing}"));
                }
                note = Some(text);
            }
            // CFG2: our tree registers no tests while the agent's own tree of this project has some: configure
            // ours again from scratch with that tree's options, once per such tree
            if cfg2()
                && req.commit.is_some()
                && let Some(like) = test_bearing_outside_tree(req.root, &dir)
            {
                let _ = std::fs::remove_file(dir.join("CMakeCache.txt"));
                let _ = std::fs::remove_file(dir.join(CONFIGURED_AT));
                let _ = std::fs::remove_file(dir.join(NO_TEST_OPTIONS));
                let text = match configure(req, &dir, Some(&like), true).await {
                    Ok(testing) => format!(
                        "configured its own tree again with the options of {} (it registered no tests; that tree does){}",
                        like.display(),
                        testing.map(|t| format!("; {t}")).unwrap_or_default()
                    ),
                    Err(err) => {
                        let _ = std::fs::remove_file(dir.join("CMakeCache.txt"));
                        let _ = std::fs::remove_file(dir.join(NO_TEST_OPTIONS));
                        let back = existing_build_dirs(req.root).into_iter().next();
                        configure(req, &dir, back.as_deref(), true).await?;
                        format!(
                            "configuring its own tree like {} failed ({}), so it was configured back as before",
                            like.display(),
                            first_line(&err)
                        )
                    }
                };
                if let Some(commit) = req.commit {
                    let _ = std::fs::write(dir.join(CONFIGURED_AT), commit);
                }
                note = Some(match note {
                    Some(before) => format!("{before}; {text}"),
                    None => text,
                });
            }
            (dir, DirKind::Scratch, note)
        }
    };
    let cache = read_cache(&build_dir).unwrap_or_default();
    let generator = cache_value(&cache, "CMAKE_GENERATOR").unwrap_or("").to_string();
    let ninja = generator.contains("Ninja");
    let mut notes = Vec::new();
    if let Some(note) = extra_note {
        notes.push(note);
    }

    // Target map.
    let mut mapping_note = String::new();
    let mut targets = file_api_targets(&build_dir, &build_dir).unwrap_or_default();
    if !targets.is_empty() {
        mapping_note = "targets from the build tree's File API reply".to_string();
    }
    if targets.is_empty() && !ninja {
        targets = dependinfo_targets(&build_dir);
        if !targets.is_empty() {
            mapping_note = "targets from DependInfo.cmake (Makefile generator)".to_string();
        }
    }
    if targets.is_empty() && crate::toolver::at_least(req.version, (3, 14)) {
        let map_dir = req.scratch.join("map");
        let configured = if stale(&map_dir, req.cmake_files) {
            configure(req, &map_dir, Some(&build_dir), false).await.map(|_| ())
        } else {
            Ok(())
        };
        match configured {
            Ok(()) => {
                targets = file_api_targets(&map_dir, &build_dir).unwrap_or_default();
                mapping_note = format!(
                    "targets from a configure-only mapping tree outside the project ({}), with the build tree's options",
                    map_dir.display()
                );
            }
            Err(err) => notes.push(format!(
                "mapping configure failed ({}); every test counts as affected",
                first_line(&err)
            )),
        }
    }
    if targets.is_empty() {
        mapping_note = "no target map (every test counts as affected)".to_string();
    }

    let mut tests = ctest_tests(&build_dir, req.version).await?;
    link_tests_to_targets(&mut tests, &targets);
    if tests.is_empty() {
        notes.push(format!(
            "the build tree {} registers no CTest tests (testing disabled in its configuration?)",
            build_dir.display()
        ));
    }
    let config = config_key(&cache);
    Ok(Model {
        system: System::CMake,
        root: req.root.to_path_buf(),
        build_dir,
        dir_kind,
        version: req.version,
        ninja,
        targets,
        tests,
        check_target: None,
        make_rebuilds: None,
        mapping_note,
        notes,
        config,
    })
}

/// The stamp in our tree naming the commit it was last configured at.
const CONFIGURED_AT: &str = ".byteask-configured-commit";

/// The commit our tree at `dir` was last configured at.
pub(crate) fn configured_at(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(CONFIGURED_AT))
        .ok()
        .map(|text| text.trim().to_string())
}

/// A tree of ours needs a (re)configure: never configured, or older than a
/// changed CMake file.
fn stale(dir: &Path, cmake_files: &[PathBuf]) -> bool {
    let Ok(stamp) = std::fs::metadata(dir.join("CMakeCache.txt")).and_then(|m| m.modified()) else {
        return true;
    };
    cmake_files.iter().any(|f| {
        std::fs::metadata(f)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > stamp)
    })
}

fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(200).collect()
}

/// Marker in our tree: configuring it with the project's test options failed once, so they are
/// left out from then on.
const NO_TEST_OPTIONS: &str = ".byteask-no-test-options";

/// Option names that only switch a project's tests on: `BUILD_TESTING` and the tests options a
/// project declares (`ADA_TESTING`, `FMT_TEST`, `SML_BUILD_TESTS`, `JSON_BuildTests`,
/// `QL_BUILD_TEST_SUITE`, `gtest_build_tests`). Many projects default them to OFF, and a tree
/// configured with the defaults registers no tests, so nothing can be checked.
fn is_test_option(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const ENDINGS: &[&str] = &["_TEST", "_TESTS", "_TESTING", "BUILDTESTS", "_TEST_SUITE"];
    // Tests that need hardware, tools or privileges the machine may lack, or are not tests.
    const NOT: &[&str] = &["FUZZ", "CUDA", "HIP", "SYCL", "COVERAGE", "INSTALL", "CONTAINER"];
    // CFG2: a switch that skips or disables tests, or needs the network, registers fewer tests when ON
    // (SPIRV_SKIP_TESTS registered none).
    const NOT_CFG2: &[&str] = &["SKIP", "DISABLE", "NO_", "NETWORK"];
    (ENDINGS.iter().any(|e| upper.ends_with(e)) || matches!(upper.as_str(), "ENABLE_TESTING" | "BUILD_TESTS"))
        && !NOT.iter().any(|n| upper.contains(n))
        && !(cfg2() && NOT_CFG2.iter().any(|n| upper.contains(n)))
}

/// The test options to turn on in our tree: `BUILD_TESTING` plus every [`is_test_option`]
/// declared with `option()` or `cmake_dependent_option()` in the top-level `CMakeLists.txt` or a
/// `cmake/*.cmake` file of the project.
pub(crate) fn test_options(root: &Path) -> Vec<String> {
    let mut files = vec![root.join("CMakeLists.txt")];
    if let Ok(entries) = std::fs::read_dir(root.join("cmake")) {
        let mut more: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "cmake"))
            .collect();
        more.sort();
        files.extend(more);
    }
    if cfg2() {
        // First-level subfolders' CMakeLists.txt (googletest/CMakeLists.txt declares gtest_build_tests)
        files.extend(first_level_cmakelists(root));
    }
    let mut names = std::collections::BTreeSet::from(["BUILD_TESTING".to_string()]);
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (command, args) in cmake_commands(&text) {
            let command = command.to_ascii_lowercase();
            if (command == "option" || command == "cmake_dependent_option")
                && let Some(name) = args.first()
                && is_test_option(name)
            {
                names.insert(name.clone());
            }
        }
    }
    if cfg2() {
        // `-D<NAME>=ON` lines of the top-level README (fkYAML documents FK_YAML_BUILD_TEST there)
        names.extend(readme_switches(root).into_iter().filter(|name| is_test_option(name)));
    }
    names.into_iter().collect()
}

/// CFG2: the CMakeLists.txt of each first-level subfolder of `root`.
fn first_level_cmakelists(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.file_name() != ".git")
        .map(|entry| entry.path().join("CMakeLists.txt"))
        .filter(|path| path.is_file())
        .collect();
    out.sort();
    out
}

/// CFG2: every `-D<NAME>=ON` (or 1, TRUE, YES) in a top-level README* file of `root`.
fn readme_switches(root: &Path) -> Vec<String> {
    let Ok(pattern) = regex_lite::Regex::new(r"-D([A-Za-z0-9_]+)=(?:ON|On|on|1|TRUE|True|true|YES|Yes|yes)\b") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.starts_with("readme") || !entry.path().is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        out.extend(
            pattern
                .captures_iter(&text)
                .filter_map(|capture| capture.get(1))
                .map(|name| name.as_str().to_string()),
        );
    }
    out
}

/// CFG2: option settings that may get a failed configure past `err` (its output): an ON-by-default option of
/// the project with the package CMake "Could NOT find" as a word of its name, OFF; after a GoogleTest the build
/// could not fetch, when find_package(GTest) works here, the project's *SYSTEM_GTEST* / *EXTERNAL_GTEST* options ON
/// and *USE_BUNDLED_GTEST* OFF.
async fn ladder_fixes(root: &Path, dir: &Path, err: &str) -> Vec<(String, bool)> {
    let declared = declared_options(root);
    let mut fixes: Vec<(String, bool)> = Vec::new();
    if let Ok(pattern) = regex_lite::Regex::new(r"Could NOT find ([A-Za-z0-9]+)") {
        for capture in pattern.captures_iter(err) {
            let Some(package) = capture.get(1) else {
                continue;
            };
            let package = package.as_str().to_ascii_uppercase();
            for (name, default_on) in &declared {
                if *default_on
                    && name.to_ascii_uppercase().split('_').any(|word| word == package)
                    && !fixes.iter().any(|(have, _)| have == name)
                {
                    fixes.push((name.clone(), false));
                }
            }
        }
    }
    let lower = err.to_ascii_lowercase();
    let gtest_fetch = (lower.contains("googletest") || lower.contains("google test") || lower.contains("gtest"))
        && ["clone", "download", "did not find", "could not resolve", "not found", "sources"]
            .iter()
            .any(|word| lower.contains(word));
    if gtest_fetch && declared.iter().any(|(name, _)| gtest_switch(name).is_some()) && gtest_found(dir).await {
        for (name, _) in &declared {
            if let Some(on) = gtest_switch(name)
                && !fixes.iter().any(|(have, _)| have == name)
            {
                fixes.push((name.clone(), on));
            }
        }
    }
    fixes
}

/// CFG2: `Some(true)` for a project's system or external GTest switch, `Some(false)` for its bundled one.
fn gtest_switch(name: &str) -> Option<bool> {
    let upper = name.to_ascii_uppercase();
    if upper.contains("USE_BUNDLED_GTEST") {
        Some(false)
    } else if upper.contains("SYSTEM_GTEST") || upper.contains("EXTERNAL_GTEST") {
        Some(true)
    } else {
        None
    }
}

/// CFG2: find_package(GTest) works with this machine's CMake and environment (a configure-only probe project
/// beside `dir`, removed afterwards).
async fn gtest_found(dir: &Path) -> bool {
    let probe = dir.with_extension("gtest-probe");
    let _ = std::fs::remove_dir_all(&probe);
    if std::fs::create_dir_all(&probe).is_err()
        || std::fs::write(
            probe.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.10)\nproject(gtest_probe CXX)\nfind_package(GTest REQUIRED)\n",
        )
        .is_err()
    {
        return false;
    }
    let args = vec![
        "-S".to_string(),
        probe.display().to_string(),
        "-B".to_string(),
        probe.join("build").display().to_string(),
    ];
    let found = matches!(
        run_capped("cmake", &args, &probe, Duration::from_secs(120)).await,
        Ok(RunOutcome::Finished { success: true, .. })
    );
    let _ = std::fs::remove_dir_all(&probe);
    found
}

/// CFG2: the options the project declares (option() and cmake_dependent_option()), each with whether its
/// default is ON: every CMakeLists.txt at most 2 folders deep and every *.cmake file at most 3 folders deep (AMReX
/// keeps them in Tools/CMake/AMReXOptions.cmake), build and hidden folders skipped, at most 400 files.
fn declared_options(root: &Path) -> Vec<(String, bool)> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if depth < 3 && !name.starts_with('.') && !name.starts_with("build") && name != "_deps" {
                        stack.push((entry.path(), depth + 1));
                    }
                }
                Ok(kind) if kind.is_file() => {
                    if (name == "CMakeLists.txt" && depth <= 2) || (name.ends_with(".cmake") && depth <= 3) {
                        files.push(entry.path());
                    }
                }
                _ => {}
            }
        }
    }
    files.sort();
    let mut out: Vec<(String, bool)> = Vec::new();
    for file in files.into_iter().take(400) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (command, args) in cmake_commands(&text) {
            let command = command.to_ascii_lowercase();
            if (command == "option" || command == "cmake_dependent_option")
                && let Some(name) = args.first()
                && !out.iter().any(|(have, _)| have == name)
            {
                let default_on = args
                    .get(2)
                    .is_some_and(|value| matches!(value.to_ascii_uppercase().as_str(), "ON" | "YES" | "TRUE" | "1" | "Y"));
                out.push((name.clone(), default_on));
            }
        }
    }
    out
}

/// `NAME=OFF, OTHER=ON`.
fn fixes_text(fixes: &[(String, bool)]) -> String {
    fixes
        .iter()
        .map(|(name, on)| format!("{name}={}", if *on { "ON" } else { "OFF" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Configure `dir` (ours, outside the project) from the project root, with a
/// File API query, the user cache entries of `like` (an existing tree), the
/// project's test options turned on ([`test_options`]), and ccache when
/// available. `-G Ninja` when `like` uses Ninja, or for a fresh tree when ninja
/// is on PATH. When a first configure with the test options fails, the tree is
/// configured without them (and stays so). `Ok` carries a note on the test options.
pub(crate) async fn configure(
    req: &Request<'_>,
    dir: &Path,
    like: Option<&Path>,
    fresh_tree: bool,
) -> Result<Option<String>, String> {
    let first = !dir.join("CMakeCache.txt").is_file();
    let options = if dir.join(NO_TEST_OPTIONS).is_file() {
        Vec::new()
    } else {
        test_options(req.root)
    };
    let declared: Vec<&String> = options.iter().filter(|o| *o != "BUILD_TESTING").collect();
    match configure_with(req, dir, like, fresh_tree, &options, &[]).await {
        Ok(()) if declared.is_empty() => Ok(None),
        Ok(()) => Ok(Some(format!(
            "with the project's test options on ({})",
            declared.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ))),
        Err(err) if first && !options.is_empty() => {
            // CFG2: up to 3 more configures with what the error names, before the test options are dropped
            let mut fixes: Vec<(String, bool)> = Vec::new();
            if cfg2() {
                let mut last = err.clone();
                for _ in 0..3 {
                    let more: Vec<(String, bool)> = ladder_fixes(req.root, dir, &last)
                        .await
                        .into_iter()
                        .filter(|(name, _)| !fixes.iter().any(|(have, _)| have == name))
                        .collect();
                    if more.is_empty() {
                        break;
                    }
                    fixes.extend(more);
                    let _ = std::fs::remove_file(dir.join("CMakeCache.txt"));
                    match configure_with(req, dir, like, fresh_tree, &options, &fixes).await {
                        Ok(()) => {
                            return Ok(Some(format!(
                                "with the project's test options on ({}) and {} (the first configure failed: {})",
                                options.join(", "),
                                fixes_text(&fixes),
                                first_line(&err)
                            )));
                        }
                        Err(again) => last = again,
                    }
                }
            }
            let _ = std::fs::remove_file(dir.join("CMakeCache.txt"));
            configure_with(req, dir, like, fresh_tree, &[], &fixes).await?;
            let _ = std::fs::write(dir.join(NO_TEST_OPTIONS), &err);
            Ok(Some(format!(
                "without the project's test options: configuring with {} on failed ({}){}",
                options.join(", "),
                first_line(&err),
                if fixes.is_empty() {
                    String::new()
                } else {
                    format!("; with {}", fixes_text(&fixes))
                }
            )))
        }
        Err(err) => Err(err),
    }
}

async fn configure_with(
    req: &Request<'_>,
    dir: &Path,
    like: Option<&Path>,
    fresh_tree: bool,
    test_options: &[String],
    fixes: &[(String, bool)],
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let query = dir.join(".cmake/api/v1/query").join(QUERY_CLIENT);
    let _ = std::fs::create_dir_all(&query);
    let _ = std::fs::write(query.join("codemodel-v2"), "");
    // A tree configured before keeps its generator and cache: configured again as it is
    // (a different `-G` would be refused, and its objects stay valid).
    let reconfigure = dir.join("CMakeCache.txt").is_file();
    let like_cache = if reconfigure { None } else { like.and_then(read_cache) };
    let fresh_tree = fresh_tree && !reconfigure;
    let mut args: Vec<String> = Vec::new();
    let generator = like_cache
        .as_ref()
        .and_then(|c| cache_value(c, "CMAKE_GENERATOR").map(str::to_string));
    match generator {
        Some(name) if !name.is_empty() => {
            if !name.contains("Ninja") || crate::tools::binary_on_path("ninja") {
                args.extend(["-G".to_string(), name]);
            }
        }
        _ if fresh_tree && crate::tools::binary_on_path("ninja") => {
            args.extend(["-G".to_string(), "Ninja".to_string()]);
        }
        _ => {}
    }
    if let Some(cache) = &like_cache {
        let init = dir.with_extension("init.cmake");
        std::fs::write(&init, init_cache_text(cache))
            .map_err(|e| format!("cannot write {}: {e}", init.display()))?;
        args.extend(["-C".to_string(), init.display().to_string()]);
        for (name, value) in fetchcontent_sources(like.unwrap_or(dir)) {
            args.push(format!("-D{name}={value}"));
        }
    }
    if !reconfigure {
        args.push("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string());
    }
    if fresh_tree && crate::tools::binary_on_path("ccache") {
        args.push("-DCMAKE_C_COMPILER_LAUNCHER=ccache".to_string());
        args.push("-DCMAKE_CXX_COMPILER_LAUNCHER=ccache".to_string());
    }
    // After `-C`, so they win over an existing tree's OFF.
    for name in test_options {
        args.push(format!("-D{name}:BOOL=ON"));
    }
    // CFG2: what a failed configure named, after the test options
    for (name, on) in fixes {
        args.push(format!("-D{name}:BOOL={}", if *on { "ON" } else { "OFF" }));
    }
    let (program_args, cwd) = if crate::toolver::at_least(req.version, (3, 13)) {
        let mut a = vec![
            "-S".to_string(),
            req.root.display().to_string(),
            "-B".to_string(),
            dir.display().to_string(),
        ];
        a.extend(args);
        (a, req.root.to_path_buf())
    } else {
        let mut a = args;
        a.push(req.root.display().to_string());
        (a, dir.to_path_buf())
    };
    match run_capped("cmake", &program_args, &cwd, CONFIGURE_TIMEOUT).await {
        Ok(RunOutcome::Finished { success: true, .. }) => Ok(()),
        Ok(RunOutcome::Finished { stdout, stderr, .. }) => {
            let text = if stderr.trim().is_empty() { stdout } else { stderr };
            let tail: Vec<&str> = text.lines().rev().take(20).collect();
            Err(format!(
                "cmake configure of {} failed:\n{}",
                dir.display(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ))
        }
        Ok(RunOutcome::TimedOut { secs }) => Err(format!("cmake configure timed out after {secs}s")),
        Err(err) => Err(err.to_string()),
    }
}

/// The user-facing cache entries of an existing tree as an initial-cache
/// script (`cmake -C`): project options and the few CMAKE_ settings that
/// change the build. Find results and internals are left to be found again.
pub(crate) fn init_cache_text(cache: &Cache) -> String {
    const CMAKE_KEEP: &[&str] = &[
        "CMAKE_BUILD_TYPE",
        "CMAKE_C_COMPILER",
        "CMAKE_CXX_COMPILER",
        "CMAKE_C_FLAGS",
        "CMAKE_CXX_FLAGS",
        "CMAKE_C_STANDARD",
        "CMAKE_CXX_STANDARD",
        "CMAKE_CXX_EXTENSIONS",
        "CMAKE_TOOLCHAIN_FILE",
        "CMAKE_PREFIX_PATH",
        "CMAKE_EXE_LINKER_FLAGS",
        "CMAKE_SHARED_LINKER_FLAGS",
        "CMAKE_POSITION_INDEPENDENT_CODE",
        "CMAKE_C_COMPILER_LAUNCHER",
        "CMAKE_CXX_COMPILER_LAUNCHER",
    ];
    let mut out = String::from("# written by cpp_test_select from an existing build tree's cache\n");
    for (name, (kind, value)) in cache {
        let keep_kind = matches!(kind.as_str(), "BOOL" | "STRING" | "PATH" | "FILEPATH" | "UNINITIALIZED");
        let cmake_ok = !name.starts_with("CMAKE_") || CMAKE_KEEP.contains(&name.as_str());
        if !keep_kind
            || !cmake_ok
            || name.starts_with("FETCHCONTENT_BASE_DIR")
            || name.ends_with("-ADVANCED")
            || value.ends_with("-NOTFOUND")
            || value.is_empty() && name.starts_with("CMAKE_")
        {
            continue;
        }
        let kind = if kind == "UNINITIALIZED" { "STRING" } else { kind.as_str() };
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$");
        out.push_str(&format!("set({name} \"{escaped}\" CACHE {kind} \"\" FORCE)\n"));
    }
    out
}

/// `FETCHCONTENT_SOURCE_DIR_<NAME>` for every dependency an existing tree has
/// already populated, so our configure reuses those sources instead of
/// downloading (and never builds into the existing tree's `_deps`).
fn fetchcontent_sources(existing: &Path) -> Vec<(String, String)> {
    let deps = existing.join("_deps");
    let Ok(entries) = std::fs::read_dir(&deps) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let dep = name.strip_suffix("-src")?;
            e.path().is_dir().then(|| {
                (
                    format!("FETCHCONTENT_SOURCE_DIR_{}", dep.to_uppercase()),
                    e.path().display().to_string(),
                )
            })
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// File API
// ---------------------------------------------------------------------------

/// Targets from the newest codemodel-v2 reply in `reply_tree`. Artifact paths
/// are rebased onto `build_dir` (they are relative to the tree that wrote the reply).
pub(crate) fn file_api_targets(reply_tree: &Path, build_dir: &Path) -> Option<Vec<Target>> {
    let reply = reply_tree.join(".cmake/api/v1/reply");
    let mut indexes: Vec<PathBuf> = std::fs::read_dir(&reply)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("index-") && n.to_string_lossy().ends_with(".json"))
        })
        .collect();
    indexes.sort();
    let index: Value = serde_json::from_str(&std::fs::read_to_string(indexes.last()?).ok()?).ok()?;
    let codemodel_file = index["objects"]
        .as_array()?
        .iter()
        .find(|o| o["kind"] == "codemodel" && o["version"]["major"] == 2)?["jsonFile"]
        .as_str()?
        .to_string();
    let codemodel: Value =
        serde_json::from_str(&std::fs::read_to_string(reply.join(codemodel_file)).ok()?).ok()?;
    let source_root = PathBuf::from(codemodel["paths"]["source"].as_str()?);
    let reply_build = PathBuf::from(codemodel["paths"]["build"].as_str().unwrap_or_default());
    let configs = codemodel["configurations"].as_array()?;
    let config = configs.first()?;
    let mut id_to_name: BTreeMap<String, String> = BTreeMap::new();
    let mut raw: Vec<Value> = Vec::new();
    for t in config["targets"].as_array()? {
        let Some(file) = t["jsonFile"].as_str() else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(reply.join(file)) else {
            continue;
        };
        let Ok(target) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let (Some(id), Some(name)) = (target["id"].as_str(), target["name"].as_str()) {
            id_to_name.insert(id.to_string(), name.to_string());
        }
        raw.push(target);
    }
    let mut targets = Vec::new();
    for t in raw {
        let Some(name) = t["name"].as_str() else {
            continue;
        };
        let kind = t["type"].as_str().unwrap_or("");
        if kind == "INTERFACE_LIBRARY" {
            continue;
        }
        let artifacts = t["artifacts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["path"].as_str())
                    .map(|p| {
                        let abs = normalize(&reply_build, Path::new(p));
                        match abs.strip_prefix(&reply_build) {
                            Ok(rel) => build_dir.join(rel),
                            Err(_) => abs,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sources = t["sources"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["path"].as_str())
                    .map(|p| canonical(&normalize(&source_root, Path::new(p))))
                    .collect()
            })
            .unwrap_or_default();
        let deps = t["dependencies"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d["id"].as_str())
                    .filter_map(|id| id_to_name.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default();
        targets.push(Target {
            name: name.to_string(),
            is_exe: kind == "EXECUTABLE",
            artifacts,
            sources,
            deps,
        });
    }
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    Some(targets)
}

// ---------------------------------------------------------------------------
// DependInfo.cmake (Makefile generators, any version)
// ---------------------------------------------------------------------------

pub(crate) fn dependinfo_targets(build: &Path) -> Vec<Target> {
    let mut targets = Vec::new();
    for (dir, name) in super::graph::cmake_target_dirs(build) {
        let Ok(info) = std::fs::read_to_string(dir.join("DependInfo.cmake")) else {
            continue;
        };
        let subdir_build = dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| build.to_path_buf());
        let mut sources = Vec::new();
        let mut deps = Vec::new();
        for (command, args) in cmake_commands(&info) {
            if command != "set" || args.is_empty() {
                continue;
            }
            let var = args[0].as_str();
            let values = &args[1..];
            if var.starts_with("CMAKE_DEPENDS_CHECK_") {
                for pair in values.chunks(2) {
                    sources.push(canonical(&normalize(&subdir_build, Path::new(&pair[0]))));
                }
            } else if var == "CMAKE_DEPENDS_DEPENDENCY_FILES" {
                for quad in values.chunks(4) {
                    if !quad[0].is_empty() {
                        sources.push(canonical(&normalize(&subdir_build, Path::new(&quad[0]))));
                    }
                }
            } else if var == "CMAKE_TARGET_LINKED_INFO_FILES" {
                for file in values {
                    if let Some(dep) = Path::new(file)
                        .parent()
                        .and_then(Path::file_name)
                        .and_then(|n| n.to_string_lossy().strip_suffix(".dir").map(str::to_string))
                    {
                        deps.push(dep);
                    }
                }
            }
        }
        sources.sort();
        sources.dedup();
        let (artifacts, is_exe) = link_artifact(&dir, &subdir_build);
        if sources.is_empty() && artifacts.is_empty() {
            continue;
        }
        targets.push(Target {
            name,
            is_exe,
            artifacts,
            sources,
            deps,
        });
    }
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    targets
}

/// The artifact named in `link.txt` (`-o <path>`, or the archive of an
/// `ar qc <lib>` line) and whether it is a program.
fn link_artifact(target_dir: &Path, subdir_build: &Path) -> (Vec<PathBuf>, bool) {
    let Ok(text) = std::fs::read_to_string(target_dir.join("link.txt")) else {
        return (Vec::new(), false);
    };
    let tokens: Vec<String> = text.lines().flat_map(|l| shlex_split(l)).collect();
    let shared = tokens.iter().any(|t| t == "-shared" || t == "-dynamiclib");
    if let Some(pos) = tokens.iter().position(|t| t == "-o") {
        if let Some(out) = tokens.get(pos + 1) {
            return (vec![normalize(subdir_build, Path::new(out))], !shared);
        }
    }
    for (i, token) in tokens.iter().enumerate() {
        let base = Path::new(token).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if base == "ar" || base.ends_with("-ar") || base == "ar.exe" {
            if let Some(archive) = tokens.get(i + 2) {
                return (vec![normalize(subdir_build, Path::new(archive))], false);
            }
        }
    }
    (Vec::new(), false)
}

fn shlex_split(line: &str) -> Vec<String> {
    shlex::split(line).unwrap_or_else(|| line.split_whitespace().map(str::to_string).collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The build tree's CTest tests.
pub(crate) async fn ctest_tests(build: &Path, version: Option<(u32, u32)>) -> Result<Vec<TestEntry>, String> {
    if crate::toolver::at_least(version, (3, 14)) {
        let args = vec!["--show-only=json-v1".to_string()];
        match run_capped("ctest", &args, build, CTEST_TIMEOUT).await {
            Ok(RunOutcome::Finished {
                success: true,
                stdout,
                ..
            }) => {
                if let Some(mut tests) = parse_ctest_json(&stdout) {
                    // ctest leaves `command` out while the program does not
                    // exist yet (nothing built): take it from the test files.
                    if tests.iter().any(|t| t.command.is_empty()) {
                        let from_files = ctest_testfile_tests(build);
                        for test in tests.iter_mut().filter(|t| t.command.is_empty()) {
                            if let Some(found) = from_files.iter().find(|f| f.name == test.name) {
                                test.command = found.command.clone();
                            }
                        }
                    }
                    return Ok(tests);
                }
            }
            Ok(RunOutcome::Finished { stderr, .. }) if stderr.contains("No such file") => {}
            _ => {}
        }
    }
    Ok(ctest_testfile_tests(build))
}

pub(crate) fn parse_ctest_json(text: &str) -> Option<Vec<TestEntry>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for t in value["tests"].as_array()? {
        let name = t["name"].as_str().unwrap_or_default().to_string();
        let command: Vec<String> = t["command"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut entry = TestEntry {
            name,
            command,
            ..TestEntry::default()
        };
        if let Some(props) = t["properties"].as_array() {
            for prop in props {
                match prop["name"].as_str() {
                    Some("WORKING_DIRECTORY") => {
                        entry.workdir = prop["value"].as_str().map(PathBuf::from);
                    }
                    Some("ENVIRONMENT") => {
                        entry.env = prop["value"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(split_env).collect())
                            .unwrap_or_default();
                    }
                    Some("DISABLED") => entry.disabled = prop["value"].as_bool().unwrap_or(false),
                    _ => {}
                }
            }
        }
        out.push(entry);
    }
    Some(out)
}

fn split_env(item: &str) -> Option<(String, String)> {
    item.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
}

/// Tests from `CTestTestfile.cmake` files (any CMake version), following
/// `subdirs()`.
pub(crate) fn ctest_testfile_tests(build: &Path) -> Vec<TestEntry> {
    let mut out: Vec<TestEntry> = Vec::new();
    let mut queue = vec![build.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = queue.pop() {
        seen += 1;
        if seen > 2000 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("CTestTestfile.cmake")) else {
            continue;
        };
        let mut subdirs = Vec::new();
        for (command, args) in cmake_commands(&text) {
            // Commands are case-insensitive; the ones a test file uses are
            // add_test, set_tests_properties and subdirs.
            let command = command.to_ascii_lowercase();
            match command.as_str() {
                "add_test" if args.len() >= 2 => out.push(TestEntry {
                    name: args[0].clone(),
                    command: args[1..].to_vec(),
                    workdir: Some(dir.clone()),
                    ..TestEntry::default()
                }),
                "set_tests_properties" => {
                    let Some(pos) = args.iter().position(|a| a == "PROPERTIES") else {
                        continue;
                    };
                    let names = &args[..pos];
                    let props = &args[pos + 1..];
                    for entry in out.iter_mut().filter(|e| names.contains(&e.name)) {
                        for kv in props.chunks(2) {
                            if kv.len() < 2 {
                                continue;
                            }
                            match kv[0].as_str() {
                                "WORKING_DIRECTORY" => entry.workdir = Some(PathBuf::from(&kv[1])),
                                "ENVIRONMENT" => {
                                    entry.env = kv[1].split(';').filter_map(split_env).collect();
                                }
                                "DISABLED" => {
                                    entry.disabled = matches!(kv[1].to_ascii_uppercase().as_str(), "ON" | "TRUE" | "1" | "YES");
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "subdirs" => {
                    for sub in args {
                        subdirs.push(normalize(&dir, Path::new(&sub)));
                    }
                }
                _ => {}
            }
        }
        // Visit subdirectories in file order.
        for sub in subdirs.into_iter().rev() {
            queue.push(sub);
        }
    }
    out
}

/// Record which target builds each test's program: `command[0]` (or another
/// argument) equal to a program's artifact, else a target named like it.
pub(crate) fn link_tests_to_targets(tests: &mut [TestEntry], targets: &[Target]) {
    for test in tests.iter_mut() {
        let paths: Vec<PathBuf> = test
            .command
            .iter()
            .map(PathBuf::from)
            .collect();
        let hit = targets.iter().find(|t| {
            t.artifacts.iter().any(|art| paths.iter().any(|p| same_file(p, art)))
        });
        let hit = hit.or_else(|| {
            let first = test.command.first()?;
            let base = Path::new(first).file_name()?.to_string_lossy().to_string();
            let stem = base.strip_suffix(".exe").unwrap_or(&base).to_string();
            targets.iter().find(|t| t.is_exe && t.name == stem)
        });
        test.target = hit.map(|t| t.name.clone());
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || (a.is_absolute() && b.is_absolute() && canonical(a) == canonical(b))
}

/// CMake command invocations: `name(arg arg "quoted" [=[bracket]=])`, with `;`
/// list separators inside quoted arguments left in place.
pub(crate) fn cmake_commands(text: &str) -> Vec<(String, Vec<String>)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '#' {
            // Bracket comment `#[[ ]]` or line comment.
            if chars.get(i + 1) == Some(&'[') {
                if let Some((_, next)) = bracket(&chars, i + 1) {
                    i = next;
                    continue;
                }
            }
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if chars.get(j) != Some(&'(') {
                continue;
            }
            let (args, next) = arguments(&chars, j + 1);
            out.push((name, args));
            i = next;
            continue;
        }
        i += 1;
    }
    out
}

/// Arguments from just after `(` to the matching `)`.
fn arguments(chars: &[char], mut i: usize) -> (Vec<String>, usize) {
    let mut args = Vec::new();
    let mut depth = 0usize;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ')' if depth == 0 => return (args, i + 1),
            ')' => {
                depth -= 1;
                i += 1;
            }
            '(' => {
                depth += 1;
                i += 1;
            }
            '"' => {
                let mut value = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                        match chars[i] {
                            'n' => value.push('\n'),
                            't' => value.push('\t'),
                            ';' => value.push_str("\\;"),
                            '\n' => {}
                            other => value.push(other),
                        }
                    } else {
                        value.push(chars[i]);
                    }
                    i += 1;
                }
                args.push(value);
                i += 1;
            }
            '[' => match bracket(chars, i) {
                Some((value, next)) => {
                    args.push(value);
                    i = next;
                }
                None => {
                    let (value, next) = unquoted(chars, i);
                    args.push(value);
                    i = next;
                }
            },
            _ => {
                let (value, next) = unquoted(chars, i);
                args.push(value);
                i = next;
            }
        }
    }
    (args, i)
}

fn unquoted(chars: &[char], mut i: usize) -> (String, usize) {
    let mut value = String::new();
    while i < chars.len() && !chars[i].is_whitespace() && chars[i] != ')' && chars[i] != '(' {
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
        }
        value.push(chars[i]);
        i += 1;
    }
    (value, i)
}

/// `[==[ … ]==]` starting at `i` (the first `[`): (content, index after).
fn bracket(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i + 1;
    let mut level = 0usize;
    while chars.get(j) == Some(&'=') {
        level += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'[') {
        return None;
    }
    let start = j + 1;
    let close: Vec<char> = std::iter::once(']')
        .chain(std::iter::repeat_n('=', level))
        .chain(std::iter::once(']'))
        .collect();
    let mut k = start;
    while k + close.len() <= chars.len() {
        if chars[k..k + close.len()] == close[..] {
            let mut content: String = chars[start..k].iter().collect();
            if content.starts_with('\n') {
                content.remove(0);
            }
            return Some((content, k + close.len()));
        }
        k += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn finds_the_projects_test_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "option(QL_BUILD_TEST_SUITE \"Build test suite\" ON)\n\
             option(QL_BUILD_FUZZ_TEST_SUITE \"fuzz\" OFF)\n\
             option(QL_ENABLE_PARALLEL_UNIT_TEST_RUNNER \"runner\" OFF)\n\
             option(QL_INSTALL_TEST_SUITE \"install\" ON)\n\
             option(FMT_TEST \"Generate the test target.\" ${FMT_MASTER_PROJECT})\n\
             option(FMT_CUDA_TEST \"cuda\" OFF)\n\
             option(JSON_BuildTests \"tests\" ON)\n\
             option(JSON_VALIDATOR_TEST_COVERAGE \"coverage\" OFF)\n\
             cmake_dependent_option(SML_BUILD_TESTS \"Build tests\" OFF \"X\" OFF)\n\
             option(SML_BUILD_EXAMPLES \"examples\" OFF)\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("cmake")).unwrap();
        std::fs::write(dir.path().join("cmake/flags.cmake"), "option(ADA_TESTING \"Build tests\" OFF)\n").unwrap();
        assert_eq!(
            test_options(dir.path()),
            ["ADA_TESTING", "BUILD_TESTING", "FMT_TEST", "JSON_BuildTests", "QL_BUILD_TEST_SUITE", "SML_BUILD_TESTS"]
        );
    }

    #[test]
    fn tokenizes_cmake_commands() {
        let text = "# comment\nadd_test([=[core_test]=] \"/b/core_test\" \"--flag\")\n\
set_tests_properties([=[core_test]=] PROPERTIES  WORKING_DIRECTORY \"/b/t\" ENVIRONMENT \"A=1;B=2\" _BACKTRACE_TRIPLES \"x\")\n\
subdirs(\"tests\")\nsubdirs(more)\n";
        let cmds = cmake_commands(text);
        assert_eq!(cmds[0], ("add_test".to_string(), vec!["core_test".to_string(), "/b/core_test".to_string(), "--flag".to_string()]));
        assert_eq!(cmds[2], ("subdirs".to_string(), vec!["tests".to_string()]));
        assert_eq!(cmds[3], ("subdirs".to_string(), vec!["more".to_string()]));
    }

    #[test]
    fn reads_old_style_ctest_files() {
        // The format CMake 3.5 writes: unquoted names, no brackets.
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path();
        std::fs::write(
            b.join("CTestTestfile.cmake"),
            "# CMake generated Testfile for\nadd_test(unit \"/b/unit\" \"--x\")\nset_tests_properties(unit PROPERTIES  ENVIRONMENT \"K=V\" DISABLED \"FALSE\")\nsubdirs(sub)\n",
        )
        .unwrap();
        std::fs::create_dir_all(b.join("sub")).unwrap();
        std::fs::write(b.join("sub/CTestTestfile.cmake"), "ADD_TEST(inner \"/b/sub/inner\")\n").unwrap();
        let tests = ctest_testfile_tests(b);
        let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["unit", "inner"]);
        assert_eq!(tests[0].env, vec![("K".to_string(), "V".to_string())]);
        assert_eq!(tests[0].command, vec!["/b/unit".to_string(), "--x".to_string()]);
        assert_eq!(tests[1].workdir.as_deref(), Some(b.join("sub").as_path()));
    }

    #[test]
    fn parses_ctest_json() {
        let json = r#"{"kind":"ctestInfo","tests":[{"name":"a","command":["/b/bin/a","--x"],"properties":[{"name":"WORKING_DIRECTORY","value":"/b/t"},{"name":"ENVIRONMENT","value":["K=V"]},{"name":"DISABLED","value":true}]},{"name":"b"}]}"#;
        let tests = parse_ctest_json(json).unwrap();
        assert_eq!(tests[0].command, vec!["/b/bin/a".to_string(), "--x".to_string()]);
        assert_eq!(tests[0].workdir, Some(PathBuf::from("/b/t")));
        assert_eq!(tests[0].env, vec![("K".to_string(), "V".to_string())]);
        assert!(tests[0].disabled);
        assert!(tests[1].command.is_empty());
    }

    #[test]
    fn init_cache_keeps_options_and_drops_internals() {
        let mut cache = Cache::new();
        for (name, kind, value) in [
            ("CMAKE_BUILD_TYPE", "STRING", "Release"),
            ("CMAKE_INSTALL_PREFIX", "PATH", "/usr/local"),
            ("FMT_TEST", "BOOL", "ON"),
            ("MY_OPT", "UNINITIALIZED", "a;b"),
            ("Foo_DIR", "PATH", "Foo_DIR-NOTFOUND"),
            ("CMAKE_HOME_DIRECTORY", "INTERNAL", "/src"),
            ("WEIRD", "STRING", "has \"quote\" and $var"),
        ] {
            cache.insert(name.to_string(), (kind.to_string(), value.to_string()));
        }
        let text = init_cache_text(&cache);
        assert!(text.contains("set(CMAKE_BUILD_TYPE \"Release\" CACHE STRING \"\" FORCE)"), "{text}");
        assert!(text.contains("set(FMT_TEST \"ON\" CACHE BOOL \"\" FORCE)"), "{text}");
        assert!(text.contains("set(MY_OPT \"a;b\" CACHE STRING \"\" FORCE)"), "{text}");
        assert!(text.contains("set(WEIRD \"has \\\"quote\\\" and \\$var\" CACHE STRING \"\" FORCE)"), "{text}");
        assert!(!text.contains("CMAKE_INSTALL_PREFIX"), "{text}");
        assert!(!text.contains("Foo_DIR"), "{text}");
        assert!(!text.contains("CMAKE_HOME_DIRECTORY"), "{text}");
    }

    #[test]
    fn links_tests_to_program_targets() {
        let targets = vec![
            Target {
                name: "core_test".to_string(),
                is_exe: true,
                artifacts: vec![PathBuf::from("/b/bin/core_test")],
                ..Target::default()
            },
            Target {
                name: "tool".to_string(),
                is_exe: true,
                artifacts: vec![PathBuf::from("/b/bin/tool")],
                ..Target::default()
            },
        ];
        let mut tests = vec![
            TestEntry {
                name: "t1".to_string(),
                command: vec!["/b/bin/core_test".to_string()],
                ..TestEntry::default()
            },
            TestEntry {
                name: "t2".to_string(),
                command: vec!["/usr/bin/python3".to_string(), "run.py".to_string(), "/b/bin/tool".to_string()],
                ..TestEntry::default()
            },
            TestEntry {
                name: "t3".to_string(),
                command: vec!["/bin/sh".to_string(), "x.sh".to_string()],
                ..TestEntry::default()
            },
        ];
        link_tests_to_targets(&mut tests, &targets);
        let got: Vec<Option<String>> = tests.iter().map(|t| t.target.clone()).collect();
        assert_eq!(got, vec![Some("core_test".to_string()), Some("tool".to_string()), None]);
    }
}
