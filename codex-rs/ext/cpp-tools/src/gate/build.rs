//! Build systems for the finish gate: find (or make) a build, build every target including the
//! tests, run the tests, parse per-test results, and prepare the same build for the base
//! snapshot in a scratch directory.
//!
//! Rules:
//! - A configured CMake/Meson build tree is reused incrementally (the agent's own, or one that
//!   existed at session start). Without one, a scratch build tree is configured outside the repo.
//! - A Make/Autotools project is built in place only when it is already configured (a tracked
//!   Makefile, or `config.status`, at the root or in an out-of-tree build directory); files the
//!   gate's own build creates in the source tree are named and left in place (nothing is
//!   deleted).
//! - A build that exits non-zero fails, recognised error line or not; tests never run on what
//!   an earlier build left. A test runner that finds no test reports "no tests ran", not a pass.
//! - An existing build tree of any kind is preferred over configuring a scratch one, and among
//!   CMake trees the project's own (present at session start, tests registered, not a
//!   sanitizer tree) over trees made during the session.
//! - Nothing is ever deleted from a pre-existing build tree.
//! - Per-test results: CTest/Meson/automake entries, plus the cases inside a test's output
//!   (GoogleTest, Catch2 and doctest failures, TAP, pytest), so a suite that is one entry
//!   (`make test`, one ctest test running a whole test program) is still compared case by
//!   case with the unmodified tree.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::run::RunOutcome;
use crate::run::run_capped_env;

/// Result of one test in one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestStatus {
    Passed,
    Failed,
    /// Killed by the per-test time limit.
    TimedOut,
    /// The executable was not built (compile failure counts as failure) or not found.
    NotRun,
    Skipped,
}

impl TestStatus {
    pub fn failing(self) -> bool {
        matches!(self, TestStatus::Failed | TestStatus::TimedOut | TestStatus::NotRun)
    }

    fn from_selection(status: crate::base_run_cache::TestStatus) -> Self {
        use crate::base_run_cache::TestStatus as S;
        match status {
            S::Pass | S::Flaky => TestStatus::Passed,
            S::Fail => TestStatus::Failed,
            S::Timeout => TestStatus::TimedOut,
            S::Skip => TestStatus::Skipped,
            S::NotBuilt | S::NotRun => TestStatus::NotRun,
        }
    }
}

/// Separator between a test entry and a case inside it (`entry::case`).
pub(crate) const CASE_SEP: &str = "::";
/// Case name meaning "every case of this entry not listed passed" (frameworks whose summary
/// lists only failures).
pub(crate) const OTHER_CASES: &str = "*";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuildKind {
    /// CMake build directory (+ generator name).
    CMake {
        dir: PathBuf,
        generator: String,
    },
    Meson {
        dir: PathBuf,
    },
    /// Make (plain Makefile or configured Autotools) in `dir`: the source root, or a
    /// configured out-of-tree build directory.
    Make {
        autotools: bool,
        dir: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct BuildPlan {
    pub kind: BuildKind,
    /// The build tree was created by the gate (scratch), not reused.
    pub scratch: bool,
    pub description: String,
}

/// Output of a build step.
#[derive(Clone, Debug, Default)]
pub(crate) struct BuildResult {
    pub ok: bool,
    pub timed_out: bool,
    /// The build program started (`false`: it could not be spawned; nothing was built).
    pub started: bool,
    /// The last lines of its output ([`log_tail`]).
    pub tail: String,
    /// Normalised first error lines (`path: error: message`, line numbers dropped for
    /// comparison) with the original text kept for display.
    pub errors: Vec<(String, String)>,
    pub log: PathBuf,
    pub secs: u64,
}

/// Output of a test step.
#[derive(Clone, Debug, Default)]
pub(crate) struct TestRun {
    /// Per-test status; a suite without per-test output has one pseudo-test named after the
    /// command (for make it is always there, so a failing exit status is never lost).
    pub results: BTreeMap<String, TestStatus>,
    /// Cases inside the entries' output, `entry::case` (see [`parse_cases`]).
    pub cases: BTreeMap<String, TestStatus>,
    pub timed_out: bool,
    /// The runner (CTest, Meson) finished and found no test at all: nothing was tested, which
    /// is never a pass.
    pub no_tests: bool,
    pub log: PathBuf,
    pub secs: u64,
    pub command: String,
}

/// Parallel jobs for builds and tests: memory-aware (see [`crate::resources::jobs`]).
pub(crate) fn jobs() -> usize {
    crate::resources::jobs()
}

/// Read `KEY:TYPE=VALUE` entries of a CMake cache.
pub(crate) fn read_cmake_cache(dir: &Path) -> BTreeMap<String, (String, String)> {
    let Ok(text) = std::fs::read_to_string(dir.join("CMakeCache.txt")) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with("//"))
        .filter_map(|line| {
            let (key_type, value) = line.split_once('=')?;
            let (key, kind) = key_type.split_once(':')?;
            Some((key.to_string(), (kind.to_string(), value.to_string())))
        })
        .collect()
}

/// CMake build trees for `root`: directories inside the repo (up to depth 2) whose cache names
/// `root` as the source directory. Newest first. Shared directories such as `/tmp` are never
/// searched: a tree someone else configured there must not be built with this user's rights.
pub(crate) fn cmake_build_dirs(root: &Path, exclude: &Path) -> Vec<PathBuf> {
    let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut candidates: Vec<PathBuf> = Vec::new();
    let visit = |dir: &Path, depth: usize, candidates: &mut Vec<PathBuf>| {
        fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, budget: &mut usize) {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            if dir.join("CMakeCache.txt").is_file() {
                out.push(dir.to_path_buf());
                return;
            }
            if depth == 0 {
                return;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name == ".git" || name == "node_modules" || name == "CMakeFiles" {
                    continue;
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(&path, depth - 1, out, budget);
                }
            }
        }
        let mut budget = 400;
        walk(dir, depth, candidates, &mut budget);
    };
    visit(root, 2, &mut candidates);
    candidates.sort();
    candidates.dedup();
    let exclude_canon = std::fs::canonicalize(exclude).unwrap_or_else(|_| exclude.to_path_buf());
    let mut matching: Vec<(std::time::SystemTime, PathBuf)> = candidates
        .into_iter()
        .filter(|dir| {
            let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            !canon.starts_with(&exclude_canon)
        })
        .filter_map(|dir| {
            let cache = read_cmake_cache(&dir);
            let home = cache.get("CMAKE_HOME_DIRECTORY")?.1.clone();
            let home = std::fs::canonicalize(&home).unwrap_or_else(|_| PathBuf::from(&home));
            (home == root_canon).then(|| {
                let mtime = [".ninja_log", "CMakeFiles/Makefile2", "CMakeCache.txt"]
                    .iter()
                    .filter_map(|f| std::fs::metadata(dir.join(f)).ok()?.modified().ok())
                    .max()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                (mtime, dir)
            })
        })
        .collect();
    matching.sort_by(|a, b| b.0.cmp(&a.0));
    matching.into_iter().map(|(_, dir)| dir).collect()
}

/// Decide how to build the working tree. `at_start` are the build trees that existed when the
/// session started ([`existing_trees`]).
///
/// Order: an existing CMake tree (the project's own first, see [`rank_cmake_trees`]), an
/// existing Meson tree, a configured Make/Autotools tree (in place or out of tree), and only
/// then a scratch CMake tree.
pub(crate) fn plan(root: &Path, scratch: &Path, at_start: &[PathBuf]) -> Option<BuildPlan> {
    let cmake_project = root.join("CMakeLists.txt").is_file();
    if cmake_project
        && let Some(dir) = rank_cmake_trees(cmake_build_dirs(root, scratch), at_start)
            .into_iter()
            .next()
    {
        let cache = read_cmake_cache(&dir);
        let generator = cache
            .get("CMAKE_GENERATOR")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "Unix Makefiles".to_string());
        return Some(BuildPlan {
            description: format!("CMake build tree {} (reused, incremental)", dir.display()),
            kind: BuildKind::CMake { dir, generator },
            scratch: false,
        });
    }
    if root.join("meson.build").is_file()
        && let Some(dir) = meson_build_dirs(root).into_iter().next()
    {
        return Some(BuildPlan {
            description: format!("Meson build tree {}", dir.display()),
            kind: BuildKind::Meson { dir },
            scratch: false,
        });
    }
    if let Some((dir, autotools)) = make_tree(root) {
        let place = if dir == root {
            "in place".to_string()
        } else {
            format!("in {}", dir.display())
        };
        return Some(BuildPlan {
            description: if autotools {
                format!("configured Autotools tree (make, {place})")
            } else {
                format!("Makefile (make, {place})")
            },
            kind: BuildKind::Make { autotools, dir },
            scratch: false,
        });
    }
    if cmake_project {
        let dir = scratch.join("cur-build");
        return Some(BuildPlan {
            description: format!("CMake scratch build tree {}", dir.display()),
            kind: BuildKind::CMake {
                dir,
                generator: String::new(),
            },
            scratch: true,
        });
    }
    None
}

/// Build trees of `root` that exist now (CMake and Meson), for the session-start record.
pub(crate) fn existing_trees(root: &Path, scratch: &Path) -> Vec<PathBuf> {
    let mut trees = cmake_build_dirs(root, scratch);
    trees.extend(meson_build_dirs(root));
    trees
}

/// CMake trees, the project's own first: present at session start, then tests registered
/// (`CTestTestfile.cmake`), then not a sanitizer build, then the most recently used. A tree
/// the agent configured during the session for a sanitizer run has no tests and is not what
/// the suite runs in.
pub(crate) fn rank_cmake_trees(trees: Vec<PathBuf>, at_start: &[PathBuf]) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let start: Vec<PathBuf> = at_start.iter().map(|p| canon(p)).collect();
    let mut keyed: Vec<((bool, bool, bool, usize), PathBuf)> = trees
        .into_iter()
        .enumerate()
        .map(|(recency, dir)| {
            let cache = read_cmake_cache(&dir);
            let sanitizer = ["CMAKE_CXX_FLAGS", "CMAKE_C_FLAGS", "CMAKE_EXE_LINKER_FLAGS"]
                .iter()
                .any(|k| cache.get(*k).is_some_and(|(_, v)| v.contains("-fsanitize")));
            let key = (
                !start.contains(&canon(&dir)),
                !dir.join("CTestTestfile.cmake").is_file(),
                sanitizer,
                recency,
            );
            (key, dir)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, dir)| dir).collect()
}

/// Meson build trees inside `root` (up to two levels down), newest first.
pub(crate) fn meson_build_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut level = vec![root.to_path_buf()];
    for _ in 0..2 {
        let mut next = Vec::new();
        for dir in level {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "node_modules" || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let path = entry.path();
                if path.join("meson-private").is_dir() {
                    let mtime = std::fs::metadata(path.join("meson-private"))
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    found.push((mtime, path));
                } else {
                    next.push(path);
                }
            }
        }
        level = next;
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.into_iter().map(|(_, dir)| dir).collect()
}

/// A configured Make tree: the root (a Makefile and, for Autotools, `config.status`), or an
/// out-of-tree build directory with `config.status` and a Makefile among the root's children.
pub(crate) fn make_tree(root: &Path) -> Option<(PathBuf, bool)> {
    let autotools = root.join("configure.ac").is_file() || root.join("configure.in").is_file();
    let has_makefile = |d: &Path| ["GNUmakefile", "makefile", "Makefile"].iter().any(|n| d.join(n).is_file());
    if has_makefile(root) && (!autotools || root.join("config.status").is_file()) {
        return Some((root.to_path_buf(), autotools));
    }
    let dir = crate::testsel::make::make_dir(root)?;
    (dir != root && dir.join("config.status").is_file()).then_some((dir, true))
}

async fn run_logged(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    log: &Path,
) -> (Option<bool>, String, u64) {
    let (status, text, secs, _) = run_logged_full(program, args, cwd, timeout, log).await;
    (status, text, secs)
}

/// [`run_logged`], and whether the program started at all (`false`: it could not be spawned).
async fn run_logged_full(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    log: &Path,
) -> (Option<bool>, String, u64, bool) {
    let started = Instant::now();
    let envs = vec![
        ("LC_ALL".to_string(), "C".to_string()),
        ("CLICOLOR_FORCE".to_string(), "0".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
    ];
    // One engine-started build or test run at a time per session; the wait counts against
    // this command's own time limit.
    let _slot = if BUILD_TOOLS.contains(&program) {
        match crate::resources::build_slot(timeout).await {
            Some(slot) => Some(slot),
            None => {
                let secs = started.elapsed().as_secs();
                let text = format!(
                    "timed out after {secs}s waiting for another ByteAsk build or test run to finish"
                );
                write_log(log, cwd, program, args, &text);
                return (None, text, secs, true);
            }
        }
    } else {
        None
    };
    let timeout = timeout.saturating_sub(started.elapsed());
    let outcome = run_capped_env(program, args, cwd, timeout, &envs).await;
    let secs = started.elapsed().as_secs();
    let (status, text, spawned) = match outcome {
        Ok(RunOutcome::Finished {
            success,
            stdout,
            stderr,
            ..
        }) => (Some(success), format!("{stdout}\n{stderr}"), true),
        Ok(RunOutcome::TimedOut { secs }) => (None, format!("timed out after {secs}s"), true),
        Err(err) => (Some(false), format!("could not run {program}: {err:?}"), false),
    };
    write_log(log, cwd, program, args, &text);
    (status, text, secs, spawned)
}

/// The last lines of a build's output (what to show when no error line was recognised).
pub(crate) fn log_tail(text: &str) -> String {
    const LINES: usize = 20;
    const CHARS: usize = 2_000;
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n");
    if tail.len() <= CHARS {
        return tail;
    }
    let mut start = tail.len() - CHARS;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &tail[start..])
}

/// Programs that build or run tests: they take the session's build slot.
const BUILD_TOOLS: &[&str] = &["cmake", "make", "ninja", "ctest", "meson"];

fn write_log(log: &Path, cwd: &Path, program: &str, args: &[String], text: &str) {
    let _ = std::fs::create_dir_all(log.parent().unwrap_or(Path::new("/")));
    let _ = std::fs::write(
        log,
        format!(
            "$ (cd {} && {program} {})\n{text}",
            cwd.display(),
            args.join(" ")
        ),
    );
}

/// Configure a scratch CMake build tree for `source` (the repo or the base copy), copying the
/// project options of `reference` (an existing cache) when there is one.
pub(crate) async fn cmake_configure(
    source: &Path,
    build: &Path,
    reference: Option<&Path>,
    timeout: Duration,
    log: &Path,
) -> bool {
    let _ = std::fs::create_dir_all(build);
    let mut args = vec![
        "-S".to_string(),
        source.display().to_string(),
        "-B".to_string(),
        build.display().to_string(),
    ];
    let reference_cache = reference.map(read_cmake_cache).unwrap_or_default();
    let source_canon = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    if let Some((_, generator)) = reference_cache.get("CMAKE_GENERATOR") {
        args.push("-G".to_string());
        args.push(generator.clone());
    } else if crate::tools::binary_on_path("ninja") {
        args.push("-G".to_string());
        args.push("Ninja".to_string());
    }
    let keep_cmake = [
        "CMAKE_BUILD_TYPE",
        "CMAKE_CXX_STANDARD",
        "CMAKE_C_STANDARD",
        "CMAKE_CXX_FLAGS",
        "CMAKE_C_FLAGS",
        "CMAKE_CXX_COMPILER",
        "CMAKE_C_COMPILER",
        "CMAKE_CXX_EXTENSIONS",
    ];
    for (key, (kind, value)) in &reference_cache {
        let project_option = !key.starts_with("CMAKE_")
            && matches!(kind.as_str(), "BOOL" | "STRING")
            && !key.ends_with("_DIR")
            && !key.contains("INTERNAL");
        if !(keep_cmake.contains(&key.as_str()) || project_option) {
            continue;
        }
        // Never point the base build at the working tree or the reference build tree.
        if value.contains(&*source_canon.to_string_lossy())
            || reference.is_some_and(|r| value.contains(&*r.to_string_lossy()))
        {
            continue;
        }
        args.push(format!("-D{key}:{kind}={value}"));
    }
    if reference_cache.is_empty() {
        // A fresh configuration: ask for tests the conventional way.
        args.push("-DBUILD_TESTING=ON".to_string());
    }
    // A scratch tree shares compiled objects with every other checkout of the project
    // through the compiler cache, so a new session's first check is warm.
    if let Some(launcher) = compiler_cache_launcher(&source_canon, build, |key| std::env::var(key).ok()) {
        args.push(format!("-DCMAKE_C_COMPILER_LAUNCHER={launcher}"));
        args.push(format!("-DCMAKE_CXX_COMPILER_LAUNCHER={launcher}"));
    }
    args.push("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string());
    let (status, _, _) = run_logged("cmake", &args, source, timeout, log).await;
    status == Some(true)
}

/// Turns the shared compiler cache off (`off`/`0`), or names the `sccache` to use.
pub(crate) const ENV_COMPILER_CACHE: &str = "BYTEASK_CPP_GATE_COMPILER_CACHE";

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The compiler launcher for a scratch tree the gate configures, as a CMake list:
/// `env;SCCACHE_BASEDIRS=<source>:<build>;SCCACHE_DIR=<shared cache>;SCCACHE_SERVER_UDS=<this
/// tree's socket>;SCCACHE_IDLE_TIMEOUT=30;<sccache>`. The cache directory is shared by every
/// checkout of every project of this user (ByteAsk's own cache dir, which the Build sandbox may
/// write); the base dirs make paths relative, so the same commit compiled in another checkout
/// is a cache hit (measured on fmt: a fresh clone's first build 2.1 s instead of 36.4 s, 41 of
/// 41 compiles from the cache). sccache's server reads the base dirs when it starts, so each
/// tree gets its own server socket. `None` when sccache is not installed or it is switched off;
/// never touches a project's own build tree.
pub(crate) fn compiler_cache_launcher(
    source: &Path,
    build: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let setting = env(ENV_COMPILER_CACHE).unwrap_or_default();
    let setting = setting.trim();
    if matches!(setting, "off" | "0" | "false" | "no") {
        return None;
    }
    let sccache = if setting.is_empty() {
        find_on_path("sccache")?
    } else {
        let path = PathBuf::from(setting);
        path.is_file().then_some(path)?
    };
    let cache = crate::builds::state_dir()?.join("compiler-cache");
    std::fs::create_dir_all(&cache).ok()?;
    let build = std::fs::canonicalize(build).unwrap_or_else(|_| build.to_path_buf());
    let socket = build.join(".byteask-sccache.sock");
    // A Unix socket path is limited to ~107 bytes.
    if socket.as_os_str().len() > 100 {
        return None;
    }
    let parts = [
        "env".to_string(),
        format!("SCCACHE_BASEDIRS={}:{}", source.display(), build.display()),
        format!("SCCACHE_DIR={}", cache.display()),
        format!("SCCACHE_SERVER_UDS={}", socket.display()),
        "SCCACHE_IDLE_TIMEOUT=30".to_string(),
        sccache.display().to_string(),
    ];
    // A path with a ';' or ':' would break the CMake list or the base-dir list.
    if parts.iter().skip(1).any(|part| part.contains(';')) || source.to_string_lossy().contains(':') {
        return None;
    }
    Some(parts.join(";"))
}

/// Build only the named CMake targets (the executables of failing tests usually carry the
/// test's name). `None` when the plan is not CMake or a target does not exist.
pub(crate) async fn build_targets(
    plan: &BuildPlan,
    root: &Path,
    targets: &BTreeSet<String>,
    timeout: Duration,
    log: &Path,
) -> Option<BuildResult> {
    let BuildKind::CMake { dir, .. } = &plan.kind else {
        return None;
    };
    if targets.is_empty() || targets.len() > 8 {
        return None;
    }
    let mut args = vec![
        "--build".to_string(),
        dir.display().to_string(),
        "-j".to_string(),
        jobs().to_string(),
        "--target".to_string(),
    ];
    args.extend(targets.iter().cloned());
    let (status, text, secs) = run_logged("cmake", &args, root, timeout, log).await;
    let unknown = text.contains("No rule to make target")
        || text.contains("unknown target")
        || text.contains("Unknown argument");
    if unknown {
        return None;
    }
    Some(BuildResult {
        ok: status == Some(true),
        timed_out: status.is_none(),
        started: true,
        tail: log_tail(&text),
        errors: parse_build_errors(&text, root),
        log: log.to_path_buf(),
        secs,
    })
}

/// Build everything (keep going past errors).
pub(crate) async fn build(
    plan: &BuildPlan,
    root: &Path,
    timeout: Duration,
    log: &Path,
) -> BuildResult {
    let (program, args, cwd) = build_argv(plan, root, jobs());
    let (status, text, secs, started) = run_logged_full(program, &args, &cwd, timeout, log).await;
    BuildResult {
        ok: status == Some(true),
        timed_out: status.is_none(),
        started,
        tail: log_tail(&text),
        errors: parse_build_errors(&text, root),
        log: log.to_path_buf(),
        secs,
    }
}

/// The command that builds everything for `plan`, keeping going past errors, with `jobs`
/// parallel jobs (every build tool gets the memory-aware count; ninja would otherwise use all
/// CPUs + 2).
pub(crate) fn build_argv(plan: &BuildPlan, root: &Path, jobs: usize) -> (&'static str, Vec<String>, PathBuf) {
    let jobs = jobs.to_string();
    match &plan.kind {
        BuildKind::CMake { dir, generator } => {
            let keep_going = if generator.contains("Ninja")
                || (generator.is_empty() && dir.join("build.ninja").is_file())
            {
                vec!["--".to_string(), "-k".to_string(), "0".to_string()]
            } else {
                vec!["--".to_string(), "-k".to_string()]
            };
            let mut args = vec![
                "--build".to_string(),
                dir.display().to_string(),
                "-j".to_string(),
                jobs,
            ];
            args.extend(keep_going);
            ("cmake", args, root.to_path_buf())
        }
        BuildKind::Meson { dir } => (
            "ninja",
            vec![
                "-C".to_string(),
                dir.display().to_string(),
                "-j".to_string(),
                jobs,
                "-k".to_string(),
                "0".to_string(),
            ],
            root.to_path_buf(),
        ),
        BuildKind::Make { dir, .. } => (
            "make",
            vec!["-k".to_string(), format!("-j{jobs}")],
            dir.clone(),
        ),
    }
}

/// First distinct compiler/linker errors. The comparison key drops line/column numbers and
/// makes paths relative to the source tree, so the same error on the base build matches.
pub(crate) fn parse_build_errors(text: &str, root: &Path) -> Vec<(String, String)> {
    let root_text = format!("{}/", root.display());
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("make") || line.starts_with("gmake") || line.starts_with("ninja:") {
            continue;
        }
        let is_error = line.contains(": error:")
            || line.contains(": fatal error:")
            || line.contains("undefined reference to")
            || line.contains("multiple definition of");
        if !is_error {
            continue;
        }
        let display = line.replace(&root_text, "");
        let key = normalise_error(&display);
        if seen.insert(key.clone()) {
            out.push((key, display));
        }
        if out.len() >= 40 {
            break;
        }
    }
    out
}

fn normalise_error(line: &str) -> String {
    // Drop `:123:45:` / `:123:` positions (all-digit segments); `::` scopes survive.
    line.split(':')
        .filter(|segment| segment.is_empty() || !segment.chars().all(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(":")
}

/// The command that runs the tests for this build, or `None` when there is no test target.
///
/// CMake: `ctest` when tests are registered, else a conventional custom target (`check`,
/// `tests`) read from the generated build files. Meson: `meson test`. Make: the `check`/
/// `test`/`tests` target read from the makefile text, else probed in a scratch copy (a dry run
/// in the tree could remake its makefiles). Parallelism is memory-aware.
pub(crate) async fn test_command(
    plan: &BuildPlan,
    root: &Path,
) -> Option<(String, Vec<String>, PathBuf)> {
    let jobs = jobs().to_string();
    match &plan.kind {
        BuildKind::CMake { dir, .. } => {
            if dir.join("CTestTestfile.cmake").is_file() {
                return Some((
                    "ctest".to_string(),
                    vec![
                        "-j".to_string(),
                        jobs,
                        "--output-on-failure".to_string(),
                        "--timeout".to_string(),
                        "300".to_string(),
                    ],
                    dir.clone(),
                ));
            }
            let target = cmake_custom_check_target(dir)?;
            Some((
                "cmake".to_string(),
                vec![
                    "--build".to_string(),
                    dir.display().to_string(),
                    "--target".to_string(),
                    target,
                    "-j".to_string(),
                    jobs,
                ],
                root.to_path_buf(),
            ))
        }
        BuildKind::Meson { dir } => Some((
            "meson".to_string(),
            vec![
                "test".to_string(),
                "-C".to_string(),
                dir.display().to_string(),
                "--print-errorlogs".to_string(),
                "--num-processes".to_string(),
                jobs,
            ],
            root.to_path_buf(),
        )),
        BuildKind::Make { dir, .. } => {
            let target = crate::testsel::make::check_target(root, dir).await?;
            Some((
                "make".to_string(),
                vec!["-k".to_string(), format!("-j{jobs}"), target],
                dir.clone(),
            ))
        }
    }
}

/// Custom targets that run a project's tests by convention, in order of preference.
const CUSTOM_CHECK_TARGETS: &[&str] = &["check", "tests"];

/// A conventional test-running custom target of a CMake build tree, read from its generated
/// files (`build.ninja` phony edges or the top-level Makefile's rules), never by running them.
pub(crate) fn cmake_custom_check_target(dir: &Path) -> Option<String> {
    let defined: BTreeSet<String> = if let Ok(text) = std::fs::read_to_string(dir.join("build.ninja")) {
        text.lines()
            .filter_map(|line| line.strip_prefix("build "))
            .filter_map(|rest| {
                let (name, rule) = rest.split_once(':')?;
                rule.trim_start().starts_with("phony").then(|| name.trim().to_string())
            })
            .collect()
    } else {
        crate::testsel::make::static_targets(dir)
    };
    CUSTOM_CHECK_TARGETS
        .iter()
        .find(|t| defined.contains(**t))
        .map(|t| (*t).to_string())
}

/// Run the tests (all, or only `only` where the runner can filter). `serial` runs one test at a
/// time (the re-run of failures: a test that fails only under parallel load is flaky).
pub(crate) async fn run_tests(
    command: &(String, Vec<String>, PathBuf),
    only: Option<&BTreeSet<String>>,
    timeout: Duration,
    log: &Path,
    serial: bool,
) -> TestRun {
    let (program, base_args, cwd) = command;
    let mut args = base_args.clone();
    if serial {
        set_jobs(program, &mut args, 1);
    }
    if let Some(names) = only
        && program == "ctest"
    {
        let pattern = names
            .iter()
            .map(|name| regex_escape(name))
            .collect::<Vec<_>>()
            .join("|");
        args.push("-R".to_string());
        args.push(format!("^({pattern})$"));
    }
    let (status, text, secs) = run_logged(program, &args, cwd, timeout, log).await;
    let mut results = parse_test_results(program, &text);
    let label = suite_label(program, base_args);
    // A runner that reports every test by name and reported none ran nothing: that is "no
    // tests ran", not one passing pseudo-test.
    let no_tests = only.is_none()
        && status == Some(true)
        && results.is_empty()
        && matches!(program.as_str(), "ctest" | "meson");
    if status.is_some()
        && !no_tests
        && (results.is_empty() || program == "make" || program == "cmake")
    {
        // The suite's own exit status as one pseudo-test (make and custom targets always,
        // since a failure can come from a part without per-test output).
        let suite_failed = status == Some(false)
            || results.values().any(|s| s.failing());
        results.insert(
            label.clone(),
            if suite_failed {
                TestStatus::Failed
            } else {
                TestStatus::Passed
            },
        );
    }
    let cases = if program == "ctest" {
        ctest_cases(&text)
    } else {
        parse_cases(&label, &text)
    };
    TestRun {
        results,
        cases,
        timed_out: status.is_none(),
        no_tests,
        log: log.to_path_buf(),
        secs,
        command: format!("{program} {}", args.join(" ")),
    }
}

/// The pseudo-test name of a suite command (`make check`, `cmake check`, `meson test`).
pub(crate) fn suite_label(program: &str, args: &[String]) -> String {
    let what = match program {
        "cmake" => args
            .iter()
            .position(|a| a == "--target")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_default(),
        "meson" => "test".to_string(),
        _ => args
            .iter()
            .rev()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap_or_default(),
    };
    format!("{program} {what}")
}

/// Replace the parallelism of a test command.
fn set_jobs(program: &str, args: &mut Vec<String>, jobs: usize) {
    match program {
        "ctest" | "cmake" => {
            if let Some(i) = args.iter().position(|a| a == "-j") {
                if let Some(value) = args.get_mut(i + 1) {
                    *value = jobs.to_string();
                }
            }
        }
        "meson" => {
            if let Some(i) = args.iter().position(|a| a == "--num-processes") {
                if let Some(value) = args.get_mut(i + 1) {
                    *value = jobs.to_string();
                }
            }
        }
        _ => {
            for arg in args.iter_mut() {
                if arg.starts_with("-j") {
                    *arg = format!("-j{jobs}");
                }
            }
        }
    }
}

fn regex_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Per-test results from ctest, meson, automake (`PASS:`/`FAIL:`) output.
pub(crate) fn parse_test_results(program: &str, text: &str) -> BTreeMap<String, TestStatus> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if program == "ctest" {
            // "  3/12 Test  #3: name .........   Passed    0.01 sec"
            // "  4/12 Test  #4: name .........***Failed    0.02 sec"
            // "Could not find executable …" / "***Not Run"
            let Some(rest) = line.split_once("Test").map(|(_, r)| r.trim_start()) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix('#') else {
                continue;
            };
            let Some((_, rest)) = rest.split_once(':') else {
                continue;
            };
            let rest = rest.trim_start();
            let name_end = rest
                .find(" .")
                .or_else(|| rest.find("***"))
                .unwrap_or(rest.len());
            let name = rest[..name_end].trim().to_string();
            let status_text = &rest[name_end..];
            let status = if status_text.contains("Passed") {
                TestStatus::Passed
            } else if status_text.contains("Not Run") || status_text.contains("Could not find") {
                TestStatus::NotRun
            } else if status_text.contains("Skipped") || status_text.contains("Disabled") {
                TestStatus::Skipped
            } else if status_text.contains("Timeout") {
                TestStatus::TimedOut
            } else if status_text.contains("Failed")
                || status_text.contains("Exception")
                || status_text.contains("SEGFAULT")
                || status_text.contains("Child aborted")
            {
                TestStatus::Failed
            } else {
                continue;
            };
            if !name.is_empty() {
                out.insert(name, status);
            }
        } else if program == "meson" {
            // " 1/3 name                OK              0.01s"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3
                && parts[0].contains('/')
                && parts[0].chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                let status = parts.iter().find_map(|p| match *p {
                    "OK" | "EXPECTEDFAIL" => Some(TestStatus::Passed),
                    "FAIL" | "TIMEOUT" | "UNEXPECTEDPASS" | "ERROR" => Some(TestStatus::Failed),
                    "SKIP" => Some(TestStatus::Skipped),
                    _ => None,
                });
                if let Some(status) = status {
                    out.insert(parts[1].to_string(), status);
                }
            }
        } else {
            for (prefix, status) in [
                ("PASS: ", TestStatus::Passed),
                ("XFAIL: ", TestStatus::Passed),
                ("FAIL: ", TestStatus::Failed),
                ("XPASS: ", TestStatus::Failed),
                ("ERROR: ", TestStatus::Failed),
                ("SKIP: ", TestStatus::Skipped),
            ] {
                if let Some(name) = line.strip_prefix(prefix) {
                    let name = name.split_whitespace().next().unwrap_or(name).to_string();
                    out.insert(name, status);
                }
            }
        }
    }
    out
}

/// Cases inside the output CTest printed for failing tests (`--output-on-failure` shows a
/// failing test's output right after its status line), keyed `test::case`.
pub(crate) fn ctest_cases(text: &str) -> BTreeMap<String, TestStatus> {
    let mut out = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some((name, status)) = crate::testsel::parse::ctest_status_line(lines[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        if status.passing() || status == crate::base_run_cache::TestStatus::Skip {
            continue;
        }
        let start = i;
        while i < lines.len()
            && crate::testsel::parse::ctest_status_line(lines[i]).is_none()
            && !lines[i].trim_start().starts_with("Start ")
            && !lines[i].contains("tests passed")
            && !lines[i].starts_with("The following tests")
        {
            i += 1;
        }
        out.extend(parse_cases(&name, &lines[start..i].join("\n")));
    }
    out
}

/// Cases inside one test's (or suite's) output, keyed `label::case`: GoogleTest (every case),
/// TAP (every case, when a plan line is present), pytest (`-v` lines, or the failures of its
/// summary), Catch2 console and doctest (failures). For the frameworks whose summary lists only
/// failures, `label::*` = Passed records that the run completed, so an unlisted case passed.
pub(crate) fn parse_cases(label: &str, text: &str) -> BTreeMap<String, TestStatus> {
    let mut out = BTreeMap::new();
    let key = |case: &str| format!("{label}{CASE_SEP}{case}");
    let put = |out: &mut BTreeMap<String, TestStatus>, case: &str, status: TestStatus| {
        let entry = out.entry(key(case)).or_insert(status);
        if status.failing() {
            *entry = status;
        }
    };
    if text.contains("[ RUN      ]") {
        for (name, status) in crate::testsel::parse::gtest(text).results {
            put(&mut out, &name, TestStatus::from_selection(status));
        }
    }
    for (name, status) in tap_cases(text) {
        put(&mut out, &name, status);
    }
    let (pytest, pytest_complete) = pytest_cases(text);
    for (name, status) in &pytest {
        put(&mut out, name, *status);
    }
    let (catch, catch_complete) = catch_console_failures(text);
    for name in &catch {
        put(&mut out, name, TestStatus::Failed);
    }
    let doctest_complete = text.contains("[doctest] test cases:");
    if doctest_complete || text.contains("TEST CASE:") {
        for (name, status) in crate::testsel::parse::doctest(text, &[]).results {
            if !status.passing() {
                put(&mut out, &name, TestStatus::from_selection(status));
            }
        }
    }
    if pytest_complete || catch_complete || doctest_complete {
        out.entry(key(OTHER_CASES)).or_insert(TestStatus::Passed);
    }
    out
}

/// TAP: `ok N - name` / `not ok N - name` (`# SKIP` skipped, `# TODO` expected), only when a
/// plan line `1..N` is present.
fn tap_cases(text: &str) -> Vec<(String, TestStatus)> {
    let has_plan = text.lines().any(|l| {
        let l = l.trim();
        l.strip_prefix("1..").is_some_and(|n| !n.is_empty() && n.chars().take_while(|c| c.is_ascii_digit()).count() > 0)
    });
    if !has_plan {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let (ok, rest) = if let Some(rest) = line.strip_prefix("not ok") {
            (false, rest)
        } else if let Some(rest) = line.strip_prefix("ok") {
            (true, rest)
        } else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with(' ') {
            continue;
        }
        let rest = rest.trim_start();
        let number: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let after = rest[number.len()..].trim_start();
        let (desc, directive) = if let Some(dir) = after.strip_prefix('#') {
            ("", dir.trim().to_ascii_uppercase())
        } else {
            match after.split_once(" # ") {
                Some((d, dir)) => (d, dir.trim().to_ascii_uppercase()),
                None => (after, String::new()),
            }
        };
        let desc = desc.trim_start_matches("- ").trim();
        let name = if desc.is_empty() { number.clone() } else { desc.to_string() };
        if name.is_empty() {
            continue;
        }
        let status = if directive.starts_with("SKIP") {
            TestStatus::Skipped
        } else if directive.starts_with("TODO") || ok {
            TestStatus::Passed
        } else {
            TestStatus::Failed
        };
        out.push((name, status));
    }
    out
}

/// pytest: `path::name PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS` lines (`-v`) and the summary's
/// `FAILED path::name - …` / `ERROR path::name` lines. Also whether the final summary line
/// (`== … passed|failed … in 1.2s ==`) was printed.
fn pytest_cases(text: &str) -> (Vec<(String, TestStatus)>, bool) {
    let mut out = Vec::new();
    let mut complete = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("==") && line.ends_with("==") && line.contains(" in ") && (line.contains(" passed") || line.contains(" failed") || line.contains(" error")) {
            complete = true;
            continue;
        }
        for (prefix, status) in [("FAILED ", TestStatus::Failed), ("ERROR ", TestStatus::Failed)] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let name = rest.split(" - ").next().unwrap_or(rest).trim();
                if name.contains("::") {
                    out.push((name.to_string(), status));
                }
            }
        }
        if let Some((name, verdict)) = line.rsplit_once(' ') {
            let name = name.trim();
            if name.contains("::") && !name.contains(' ') {
                let status = match verdict {
                    "PASSED" | "XFAIL" => Some(TestStatus::Passed),
                    "FAILED" | "ERROR" | "XPASS" => Some(TestStatus::Failed),
                    "SKIPPED" => Some(TestStatus::Skipped),
                    _ => None,
                };
                if let Some(status) = status {
                    out.push((name.to_string(), status));
                }
            }
        }
    }
    (out, complete)
}

/// Catch2 console reporter: the names of failing test cases (a header is a dashed line, the
/// case name and its section lines, a dashed line; a case failed when its block shows
/// `FAILED:`), and whether its summary was printed.
fn catch_console_failures(text: &str) -> (Vec<String>, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let ruled = |l: &str, c: char| {
        let l = l.trim_end();
        l.len() >= 40 && l.chars().all(|x| x == c)
    };
    let complete = lines.iter().any(|l| {
        let l = l.trim();
        (l.starts_with("test cases:") && l.contains('|')) || l.starts_with("All tests passed (")
    });
    let mut headers: Vec<(usize, String, usize)> = Vec::new();
    let mut i = 0;
    while i + 2 < lines.len() {
        if ruled(lines[i], '-')
            && let Some(close) = (i + 2..lines.len().min(i + 8)).find(|&k| ruled(lines[k], '-'))
            && lines[i + 1..close]
                .iter()
                .all(|l| !l.trim().is_empty() && !ruled(l, '-'))
        {
            headers.push((i, lines[i + 1].trim().to_string(), close));
            i = close + 1;
            continue;
        }
        i += 1;
    }
    let mut out: Vec<String> = Vec::new();
    for (n, (_, name, close)) in headers.iter().enumerate() {
        let next = headers.get(n + 1).map(|h| h.0).unwrap_or(lines.len());
        let end = (close + 1..next).find(|&j| ruled(lines[j], '=')).unwrap_or(next);
        if lines[close + 1..end].iter().any(|l| l.contains("FAILED:")) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    (out, complete)
}

/// Prepare the base snapshot for building: copy the working tree (without `.git` and without
/// paths the agent created), put back the session-start content of every file the agent
/// changed, and remove files the agent added. Returns `false` when the copy cannot be made.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_base_tree(
    root: &Path,
    base: &Path,
    head: &str,
    marker: &str,
    changed_tracked: &[(char, String)],
    start_copy: Option<&Path>,
    exclude: &[String],
    log: &Path,
) -> bool {
    // Reused only when it was made from the same session-start state (`marker`).
    let ready = base.join(".byteask-base-ready");
    if std::fs::read_to_string(&ready).is_ok_and(|recorded| recorded == marker) {
        return true;
    }
    let index = base.with_extension("idx");
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_dir_all(base);
    if std::fs::create_dir_all(base).is_err() {
        return false;
    }
    let mut tar_args = vec![
        "-C".to_string(),
        root.display().to_string(),
        "--exclude=./.git".to_string(),
    ];
    for path in exclude {
        tar_args.push(format!("--exclude=./{}", path.trim_end_matches('/')));
    }
    tar_args.extend(["-cf".to_string(), "-".to_string(), ".".to_string()]);
    let script = format!(
        "tar {} | tar -C {} -xf -",
        tar_args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
        shell_quote(&base.display().to_string())
    );
    let (status, _, _) = run_logged(
        "sh",
        &["-c".to_string(), script],
        root,
        Duration::from_secs(600),
        log,
    )
    .await;
    if status != Some(true) {
        return false;
    }
    // Revert the agent's changes inside the copy.
    let git_dir = root.join(".git");
    for (status, path) in changed_tracked {
        let target = base.join(path);
        if let Some(copy) = start_copy.map(|dir| dir.join(path)).filter(|p| p.is_file()) {
            let _ = std::fs::copy(copy, &target);
            continue;
        }
        if *status == 'A' {
            let _ = std::fs::remove_file(&target);
            continue;
        }
        let envs = vec![("GIT_INDEX_FILE".to_string(), index.display().to_string())];
        let git_dir_arg = format!("--git-dir={}", git_dir.display());
        let work_tree_arg = format!("--work-tree={}", base.display());
        let read = vec![
            git_dir_arg.clone(),
            work_tree_arg.clone(),
            "read-tree".to_string(),
            head.to_string(),
        ];
        if !index.is_file() {
            let _ = run_capped_env("git", &read, root, Duration::from_secs(120), &envs).await;
        }
        let checkout = vec![
            git_dir_arg,
            work_tree_arg,
            "checkout-index".to_string(),
            "-f".to_string(),
            "--".to_string(),
            path.clone(),
        ];
        let _ = run_capped_env("git", &checkout, root, Duration::from_secs(120), &envs).await;
    }
    let _ = std::fs::write(&ready, marker);
    true
}

pub(crate) fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The base-tree equivalent of `plan` (`root` is the working tree `base_tree` copies). `None`
/// when the build directory is outside the copied tree.
pub(crate) fn base_plan(plan: &BuildPlan, root: &Path, base_tree: &Path, scratch: &Path) -> Option<BuildPlan> {
    Some(match &plan.kind {
        BuildKind::CMake { generator, .. } => BuildPlan {
            kind: BuildKind::CMake {
                dir: scratch.join("base-build"),
                generator: generator.clone(),
            },
            scratch: true,
            description: "base snapshot, CMake scratch build".to_string(),
        },
        BuildKind::Meson { .. } => BuildPlan {
            kind: BuildKind::Meson {
                dir: scratch.join("base-meson"),
            },
            scratch: true,
            description: "base snapshot, Meson scratch build".to_string(),
        },
        BuildKind::Make { autotools, dir } => BuildPlan {
            kind: BuildKind::Make {
                autotools: *autotools,
                dir: base_tree.join(dir.strip_prefix(root).ok()?),
            },
            scratch: true,
            description: format!("base snapshot copy {}", base_tree.display()),
        },
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_ctest_output() {
        let text = "Test project /b\n\
      Start  1: alpha\n\
 1/4 Test  #1: alpha ............................   Passed    0.01 sec\n\
 2/4 Test  #2: beta-test ........................***Failed    0.02 sec\n\
 3/4 Test  #3: gamma ............................***Not Run   0.00 sec\n\
 4/4 Test  #4: delta ............................***Timeout 300.00 sec\n\
Could not find executable /b/gamma\n";
        let results = parse_test_results("ctest", text);
        assert_eq!(results.get("alpha"), Some(&TestStatus::Passed));
        assert_eq!(results.get("beta-test"), Some(&TestStatus::Failed));
        assert_eq!(results.get("gamma"), Some(&TestStatus::NotRun));
        assert_eq!(results.get("delta"), Some(&TestStatus::TimedOut));
        assert!(TestStatus::TimedOut.failing());
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn parses_automake_and_meson_output() {
        let text =
            "PASS: tests/a.test\nFAIL: tests/b.test 2 - thing\nSKIP: tests/c\nXFAIL: tests/d\n";
        let results = parse_test_results("make", text);
        assert_eq!(results.get("tests/a.test"), Some(&TestStatus::Passed));
        assert_eq!(results.get("tests/b.test"), Some(&TestStatus::Failed));
        assert_eq!(results.get("tests/c"), Some(&TestStatus::Skipped));
        assert_eq!(results.get("tests/d"), Some(&TestStatus::Passed));
        let meson = " 1/2 unit   OK   0.01s\n 2/2 other  FAIL 0.20s   exit status 1\n";
        let results = parse_test_results("meson", meson);
        assert_eq!(results.get("unit"), Some(&TestStatus::Passed));
        assert_eq!(results.get("other"), Some(&TestStatus::Failed));
    }

    #[test]
    fn build_errors_are_normalised_for_comparison() {
        let root = Path::new("/w/repo");
        let text = "/w/repo/src/a.cc:10:5: error: 'x' was not declared in this scope\n\
                    /w/repo/src/a.cc:10:5: error: 'x' was not declared in this scope\n\
                    make[2]: *** [x] Error 1\n\
                    /usr/bin/ld: a.o: undefined reference to `f()'\n";
        let errors = parse_build_errors(text, root);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(
            errors[0].1,
            "src/a.cc:10:5: error: 'x' was not declared in this scope"
        );
        assert_eq!(
            errors[0].0,
            "src/a.cc: error: 'x' was not declared in this scope"
        );
        let base = parse_build_errors(
            "/tmp/g/base/src/a.cc:12:5: error: 'x' was not declared in this scope\n",
            Path::new("/tmp/g/base"),
        );
        assert_eq!(base[0].0, errors[0].0);
    }

    #[test]
    fn every_build_command_carries_the_job_count() {
        let root = Path::new("/w");
        let plans = [
            BuildKind::CMake { dir: PathBuf::from("/w/build"), generator: "Ninja".to_string() },
            BuildKind::Meson { dir: PathBuf::from("/w/build") },
            BuildKind::Make { autotools: false, dir: PathBuf::from("/w") },
        ];
        for kind in plans {
            let plan = BuildPlan { kind, scratch: false, description: String::new() };
            let (program, args, _) = build_argv(&plan, root, 3);
            let joined = args.join(" ");
            assert!(joined.contains("-j 3") || joined.contains("-j3"), "{program} {joined}");
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a/b-c"), "a/b-c");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(regex_escape("a.b+c"), "a\\.b\\+c");
    }
}
