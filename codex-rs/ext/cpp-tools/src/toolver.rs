//! Toolchain robustness helpers shared by the analysis tools.
//!
//! - [`tool_version`]: `<tool> --version`, run ONCE per engine process and
//!   cached, so a tool can build its argv from what the installed version
//!   actually accepts. Distro toolchains range from very old (cppcheck 1.x,
//!   clang-tidy 3.x/4.x) to current; a flag the old one does not know makes the
//!   whole run fail before anything is analysed.
//! - [`did_not_run`]: the one wording for "this tool could not run", so it can
//!   never be read as a clean result and never dumps a wall of raw output.
//! - [`project_is_cpp`]: whether a project is C++ (vs C), from its compile
//!   database or its source files, for choosing how to parse an ambiguous `.h`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use crate::run::RunOutcome;
use crate::run::run_capped;

/// A tool version as `(major, minor)`.
pub(crate) type Version = (u32, u32);

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `program --version`, parsed, cached for the life of the process. `None`
/// when the tool is missing, did not answer, or printed no version number —
/// callers treat `None` as "assume a current version", so behaviour with an
/// unrecognised (usually newer) build is unchanged.
pub(crate) async fn tool_version(program: &str, cwd: &Path) -> Option<Version> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Version>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(program)
    {
        return *hit;
    }
    let args = vec!["--version".to_string()];
    let version = match run_capped(program, &args, cwd, VERSION_PROBE_TIMEOUT).await {
        Ok(RunOutcome::Finished { stdout, stderr, .. }) => {
            parse_version(&stdout).or_else(|| parse_version(&stderr))
        }
        _ => None,
    };
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(program.to_string(), version);
    version
}

/// The first `N.N[.N]` token in a `--version` banner: "Cppcheck 2.10",
/// "Debian LLVM version 14.0.6", "LLVM version 3.8.0", "bear 3.1.1",
/// "valgrind-3.19.0".
pub(crate) fn parse_version(text: &str) -> Option<Version> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '-'))
        .find_map(|token| {
            let token = token.trim_start_matches(['v', 'V']);
            let mut parts = token.split('.');
            let major = parts.next()?.parse::<u32>().ok()?;
            let minor_part = parts.next()?;
            let digits: String = minor_part.chars().take_while(char::is_ascii_digit).collect();
            let minor = digits.parse::<u32>().ok()?;
            Some((major, minor))
        })
}

/// `version >= min`, treating an unknown version as new enough.
pub(crate) fn at_least(version: Option<Version>, min: Version) -> bool {
    version.is_none_or(|v| v >= min)
}

/// Render a version for a status line: " 1.90", or "" when unknown.
pub(crate) fn version_label(version: Option<Version>) -> String {
    version.map_or_else(String::new, |(major, minor)| format!(" {major}.{minor}"))
}

/// Max lines of the tool's own output quoted in a did-not-run report.
const DID_NOT_RUN_DETAIL_LINES: usize = 6;

/// The single "could not run" report: status first (Codex middle-truncates
/// long outputs), then what to do, then a few lines of what the tool said.
pub(crate) fn did_not_run(reason: &str, next: &str, raw: &str) -> String {
    let mut out = format!(
        "status: DID NOT RUN — {reason}. This is NOT a clean result: nothing was analysed.\n\
         next: {next}"
    );
    let lines: Vec<&str> = raw.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    if !lines.is_empty() {
        out.push_str("\ntool output:");
        for line in lines.iter().take(DID_NOT_RUN_DETAIL_LINES) {
            out.push_str("\n  ");
            out.push_str(line);
        }
        if lines.len() > DID_NOT_RUN_DETAIL_LINES {
            out.push_str(&format!(
                "\n  … {} more line(s) omitted",
                lines.len() - DID_NOT_RUN_DETAIL_LINES
            ));
        }
    }
    out
}

const CPP_SOURCE_EXTS: &[&str] = &["cc", "cpp", "cxx", "c++", "cp", "C", "CPP"];
const CPP_HEADER_EXTS: &[&str] = &["hh", "hpp", "hxx", "h++", "HPP"];
/// How many directory entries the source scan may visit before deciding on
/// what it has seen, so a huge tree costs milliseconds, not seconds.
const SCAN_BUDGET: usize = 4000;
const SCAN_DEPTH: usize = 5;
const SCAN_SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".cache", "third_party", "vendor"];

/// Is the project at `root` (or the directory holding the file being analysed)
/// C++ rather than C? Evidence, in order: the compile database's entries; the
/// C/C++ files next to `near`; a bounded scan of the project. A C header parses
/// fine as C++ far more often than the reverse, so any C++ majority (or a tie)
/// says C++. `None` when there is no evidence at all.
pub(crate) fn project_is_cpp(root: &Path, near: Option<&Path>, db: Option<&Path>) -> Option<bool> {
    if let Some(db) = db
        && let Some(verdict) = db_language(db)
    {
        return Some(verdict);
    }
    if let Some(dir) = near {
        let (cpp, c) = count_sources(dir, 1);
        if cpp + c > 0 {
            return Some(cpp >= c);
        }
    }
    let (cpp, c) = count_sources(root, SCAN_DEPTH);
    (cpp + c > 0).then_some(cpp >= c)
}

/// Does `text` (a header's contents) use syntax only C++ has? Used when the
/// project gives no evidence either way.
pub(crate) fn looks_like_cpp(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("namespace ")
            || line.starts_with("template")
            || line.starts_with("class ")
            || line.starts_with("using ")
            || line.starts_with("public:")
            || line.starts_with("private:")
            || line.contains("std::")
    })
}

fn db_language(db: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(db).ok()?;
    let entries: serde_json::Value = serde_json::from_str(&text).ok()?;
    let (mut cpp, mut c) = (0usize, 0usize);
    for entry in entries.as_array()? {
        let Some(file) = entry.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        match classify(Path::new(file)) {
            Some(true) => cpp += 1,
            Some(false) => c += 1,
            None => {}
        }
    }
    (cpp + c > 0).then_some(cpp >= c)
}

/// `Some(true)` for a C++ source/header, `Some(false)` for a `.c` file.
fn classify(path: &Path) -> Option<bool> {
    let ext = path.extension()?.to_str()?;
    if ext == "c" {
        Some(false)
    } else if CPP_SOURCE_EXTS.contains(&ext) || CPP_HEADER_EXTS.contains(&ext) {
        Some(true)
    } else {
        None
    }
}

/// `(c++ files, c files)` under `dir`, at most `depth` levels deep.
fn count_sources(dir: &Path, depth: usize) -> (usize, usize) {
    let mut counts = (0, 0);
    let mut budget = SCAN_BUDGET;
    let mut stack = vec![(dir.to_path_buf(), 1usize)];
    while let Some((dir, level)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                return counts;
            }
            budget -= 1;
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if level < depth && !name.starts_with('.') && !SCAN_SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push((path, level + 1));
                }
            } else {
                match classify(&path) {
                    Some(true) => counts.0 += 1,
                    Some(false) => counts.1 += 1,
                    None => {}
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_version_banners_of_every_vintage() {
        assert_eq!(parse_version("Cppcheck 2.10\n"), Some((2, 10)));
        assert_eq!(parse_version("Cppcheck 1.90"), Some((1, 90)));
        assert_eq!(parse_version("Cppcheck 2.14 dev"), Some((2, 14)));
        assert_eq!(
            parse_version("Debian LLVM version 14.0.6\n  Optimized build.\n  Default target: x86_64-pc-linux-gnu"),
            Some((14, 0))
        );
        assert_eq!(
            parse_version("LLVM (http://llvm.org/):\n  LLVM version 3.8.0\n"),
            Some((3, 8))
        );
        assert_eq!(parse_version("bear 3.1.1"), Some((3, 1)));
        assert_eq!(parse_version("bear 2.4.3"), Some((2, 4)));
        assert_eq!(parse_version("valgrind-3.19.0"), Some((3, 19)));
        assert_eq!(
            parse_version("gcc (Ubuntu 5.4.0-6ubuntu1~16.04.12) 5.4.0 20160609"),
            Some((5, 4))
        );
        assert_eq!(parse_version("GNU gdb (Debian 13.1-3) 13.1"), Some((13, 1)));
        assert_eq!(parse_version("no numbers here"), None);
    }

    #[test]
    fn an_unknown_version_counts_as_current() {
        assert!(at_least(None, (2, 0)));
        assert!(at_least(Some((2, 0)), (2, 0)));
        assert!(!at_least(Some((1, 90)), (2, 0)));
        assert_eq!(version_label(Some((1, 90))), " 1.90");
        assert_eq!(version_label(None), "");
    }

    #[test]
    fn did_not_run_is_status_first_and_bounded() {
        let raw = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let text = did_not_run("cppcheck rejected an option", "retry without it", &raw);
        assert!(text.starts_with("status: DID NOT RUN — cppcheck rejected an option."), "{text}");
        assert!(text.contains("NOT a clean result"), "{text}");
        assert!(text.contains("next: retry without it"), "{text}");
        assert!(text.contains("line 6") && !text.contains("line 7\n"), "{text}");
        assert!(text.contains("14 more line(s) omitted"), "{text}");
        assert!(!did_not_run("r", "n", "").contains("tool output"));
    }

    #[test]
    fn project_language_follows_the_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("inc")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("inc/a.h"), "").unwrap();
        // No sources at all: no evidence.
        assert_eq!(project_is_cpp(root, Some(&root.join("inc")), None), None);
        std::fs::write(root.join("src/a.cpp"), "").unwrap();
        std::fs::write(root.join("src/b.cc"), "").unwrap();
        std::fs::write(root.join("src/c.c"), "").unwrap();
        assert_eq!(project_is_cpp(root, Some(&root.join("inc")), None), Some(true));
        // Sources next to the header beat the project-wide count.
        std::fs::write(root.join("inc/impl.c"), "").unwrap();
        assert_eq!(project_is_cpp(root, Some(&root.join("inc")), None), Some(false));
        // The compile database beats both.
        let db = root.join("compile_commands.json");
        std::fs::write(&db, r#"[{"directory":"/w","file":"x.cpp","command":"c++ -c x.cpp"}]"#)
            .unwrap();
        assert_eq!(project_is_cpp(root, Some(&root.join("inc")), Some(&db)), Some(true));
    }

    #[test]
    fn cpp_only_syntax_is_recognised() {
        assert!(looks_like_cpp("#pragma once\nnamespace util {\n}\n"));
        assert!(looks_like_cpp("template <typename T> T f(T);"));
        assert!(!looks_like_cpp("#ifndef A_H\nint f(int class_);\nstruct s { int x; };\n#endif\n"));
    }
}
