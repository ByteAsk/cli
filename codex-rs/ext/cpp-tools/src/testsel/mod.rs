//! Test selection for `cpp_test_select`: which of a project's tests a change can
//! affect, read from the project's own build system, and how to build and run
//! only those.
//!
//! ```text
//!   changed files (git diff vs the session-start HEAD + untracked)
//!     │  symbols::changes            changed files, changed symbols, string literals
//!     ▼
//!   build model (one of)             targets: sources, artifacts, link deps; tests: argv, dir, env
//!     cmake::model   File API reply (existing dir) / DependInfo.cmake (Makefile generator)
//!                    / scratch configure with a File API query (never the existing dir)
//!                    tests: `ctest --show-only=json-v1` (≥ 3.14) or CTestTestfile.cmake
//!     meson::model   meson-info/intro-targets.json + intro-tests.json
//!     make::model    suite target from the makefile text; `make -n <check>` / `make -n -W <file>
//!                    <check>` dry runs in a scratch copy (make never runs in the tree for this)
//!     │  graph::affected             changed file → TUs (depfiles ∪ textual #include closure)
//!     ▼                              → targets → reverse link closure → tests
//!   select::select                   tier 3 = affected + unmapped; tier 2 = tests whose sources
//!     │                              reference a changed symbol; tier 1 = those test cases
//!     ▼
//!   exec::execute                    build the needed targets incrementally, run, parse,
//!                                    re-run failures once; optional base comparison
//! ```
//!
//! Builds happen in the tools' own tree for the repository ([`tree`]): `cpp_test_select`,
//! `cpp_finish_check` ([`verify`]) and the harness's background warm build ([`warm`]) share
//! it, one build at a time.

pub(crate) mod base;
pub(crate) mod check;
pub(crate) mod context;
pub(crate) mod cmake;
pub(crate) mod exec;
pub(crate) mod graph;
pub(crate) mod jobs;
pub(crate) mod make;
pub(crate) mod meson;
pub(crate) mod parse;
pub(crate) mod select;
pub(crate) mod symbols;
pub(crate) mod tree;
pub(crate) mod verify;
pub(crate) mod warm;

use std::path::Path;
use std::path::PathBuf;

/// A build target (library or program).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Target {
    /// The name the build tool builds it by (`cmake --build --target <name>`,
    /// `ninja <name>`, `make <name>`).
    pub name: String,
    pub is_exe: bool,
    /// Absolute paths of what it produces.
    pub artifacts: Vec<PathBuf>,
    /// Absolute paths of its translation units (and listed headers).
    pub sources: Vec<PathBuf>,
    /// Names of targets it links against or otherwise depends on.
    pub deps: Vec<String>,
}

/// One registered test (a CTest test, a Meson test, or a test program).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TestEntry {
    pub name: String,
    /// Full argv; `command[0]` is usually a built program.
    pub command: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Build target that produces the program, when known.
    pub target: Option<String>,
    pub disabled: bool,
}

/// Which build system a model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum System {
    CMake,
    Meson,
    Make,
}

impl System {
    pub(crate) fn label(self) -> &'static str {
        match self {
            System::CMake => "CMake",
            System::Meson => "Meson",
            System::Make => "Make",
        }
    }
}

/// Where builds happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirKind {
    /// A build tree that existed before this tool: built incrementally, never
    /// reconfigured, never deleted.
    Existing,
    /// A tree this tool configured outside the project.
    Scratch,
    /// The source tree itself (in-tree Make).
    InTree,
}

/// Everything the build system says about the project.
#[derive(Clone, Debug)]
pub(crate) struct Model {
    pub system: System,
    /// Project source root (the git work tree).
    pub root: PathBuf,
    pub build_dir: PathBuf,
    pub dir_kind: DirKind,
    /// Build-tool version, e.g. CMake (3, 25).
    pub version: Option<(u32, u32)>,
    /// `ninja` or `make` builds this tree (CMake/Meson generators).
    pub ninja: bool,
    pub targets: Vec<Target>,
    pub tests: Vec<TestEntry>,
    /// Make only: the target that runs the whole suite (`check`, `test`).
    pub check_target: Option<String>,
    /// Make only: programs a `make -n -W <changed>` dry run would rebuild.
    pub make_rebuilds: Option<Vec<PathBuf>>,
    /// How the mapping was obtained, for the output's first line.
    pub mapping_note: String,
    /// Free-form facts worth one line each.
    pub notes: Vec<String>,
    /// The fingerprint of options that change test results (base-run cache key).
    pub config: String,
}

impl Model {
    pub(crate) fn target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.name == name)
    }

    pub(crate) fn rel<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, str> {
        match path.strip_prefix(&self.root) {
            Ok(rel) => rel.to_string_lossy(),
            Err(_) => path.to_string_lossy(),
        }
    }
}

/// C/C++ source and header extensions.
pub(crate) const SOURCE_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "cp", "C", "m", "mm", "cu"];
pub(crate) const HEADER_EXTS: &[&str] = &[
    "h", "hh", "hpp", "hxx", "h++", "H", "inl", "ipp", "tpp", "tcc", "inc", "cuh",
];

pub(crate) fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&e))
}

pub(crate) fn is_source(path: &Path) -> bool {
    has_ext(path, SOURCE_EXTS)
}

pub(crate) fn is_header(path: &Path) -> bool {
    has_ext(path, HEADER_EXTS)
}

pub(crate) fn is_cpp_file(path: &Path) -> bool {
    is_source(path) || is_header(path)
}

/// Build files whose change can alter every target.
pub(crate) fn is_build_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    name == "CMakeLists.txt"
        || name.ends_with(".cmake")
        || name == "meson.build"
        || name == "meson_options.txt"
        || name == "meson.options"
        || name.starts_with("Makefile")
        || name == "GNUmakefile"
        || name == "configure.ac"
        || name.ends_with(".mk")
        || name.ends_with(".am")
}

/// Lexically normalise `path` (resolve `.` and `..` without touching the disk)
/// against `base` when relative.
pub(crate) fn normalize(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Directories never scanned: VCS metadata, dependency caches, and any build
/// tree (a directory holding a CMake cache, Ninja file or Meson private dir).
pub(crate) fn skip_dir(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    matches!(
        name.as_str(),
        ".git" | ".hg" | ".svn" | "node_modules" | ".cache" | ".ccls-cache" | ".clangd" | "CMakeFiles"
    ) || path.join("CMakeCache.txt").is_file()
        || path.join("build.ninja").is_file()
        || path.join("meson-private").is_dir()
}

/// `[a-zA-Z0-9_]` test for identifier scans.
pub(crate) fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(
            normalize(Path::new("/b/sub"), Path::new("../src/./a.cc")),
            PathBuf::from("/b/src/a.cc")
        );
        assert_eq!(normalize(Path::new("/b"), Path::new("/x/y")), PathBuf::from("/x/y"));
    }

    #[test]
    fn classifies_files() {
        assert!(is_source(Path::new("a/b.cpp")));
        assert!(is_header(Path::new("a/b.hpp")));
        assert!(!is_cpp_file(Path::new("README.md")));
        assert!(is_build_file(Path::new("x/CMakeLists.txt")));
        assert!(is_build_file(Path::new("cmake/Foo.cmake")));
        assert!(!is_build_file(Path::new("src/make.c")));
    }
}
