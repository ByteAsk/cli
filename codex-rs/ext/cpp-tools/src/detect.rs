use std::path::Path;

/// Files that are themselves proof of a C/C++ project.
///
/// `Makefile` is deliberately NOT here. Python, Go and JS repos ship one
/// routinely, and treating it as proof handed those projects the full C/C++
/// guidance prompt (~1.9k tokens of advice about a language they do not use).
/// A Makefile-driven C project is still detected — by its sources.
const DEFINITIVE_MARKERS: [&str; 2] = ["CMakeLists.txt", "compile_commands.json"];

/// File extensions that indicate C/C++ source or headers.
const SOURCE_EXTENSIONS: [&str; 9] = ["c", "cc", "cpp", "cxx", "c++", "h", "hpp", "hxx", "hh"];

/// Directories that never hold a project's own sources and are often enormous.
const SKIP_DIRS: [&str; 13] = [
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    "venv",
    "__pycache__",
    ".tox",
    ".cache",
    ".idea",
    ".vscode",
    "Pods",
    "vendor",
];

/// How many directory levels below `root` to search.
const MAX_DEPTH: usize = 4;

/// Hard cap on directories read. This runs at every thread start, so detection
/// must never become a visible pause on a large monorepo.
const MAX_DIRS: usize = 400;

/// Returns true when `root` looks like a C/C++ workspace.
///
/// Evidence is an actual C/C++ source or header, or a compile database /
/// `CMakeLists.txt`. The search descends up to [`MAX_DEPTH`] levels (bounded by
/// [`MAX_DIRS`] directories) because real repos bury the C++: a monorepo whose
/// root is `package.json` and TypeScript but which builds
/// `services/pricing/engine/src/book.cpp` IS a C/C++ workspace, and the old
/// root-plus-`src/include/lib` scan missed exactly that shape.
pub fn is_cpp_workspace(root: &Path) -> bool {
    let mut budget = MAX_DIRS;
    scan(root, 0, &mut budget)
}

fn scan(dir: &Path, depth: usize, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };

    let mut subdirs: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // `file_type` does not follow symlinks, so a symlinked directory is
        // treated as a file and never traversed — no cycles, no escaping root.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if depth < MAX_DEPTH && !name.starts_with('.') && !SKIP_DIRS.contains(&name) {
                subdirs.push(path);
            }
            continue;
        }
        if DEFINITIVE_MARKERS.contains(&name) {
            return true;
        }
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str())
            && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        {
            return true;
        }
    }

    // Every file in this directory is checked before descending, so the common
    // case (C/C++ at or near the root) costs one `read_dir`.
    for sub in subdirs {
        if scan(&sub, depth + 1, budget) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// Minimal self-cleaning temp dir to avoid an extra dev-dependency.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "codex-cpp-tools-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            path.push(unique);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn empty_dir_is_not_cpp() {
        let dir = TempDir::new("empty");
        assert!(!is_cpp_workspace(dir.path()));
    }

    #[test]
    fn detects_source_file_in_root() {
        let dir = TempDir::new("src-root");
        fs::write(dir.path().join("main.cpp"), "int main(){}").unwrap();
        assert!(is_cpp_workspace(dir.path()));
    }

    #[test]
    fn detects_marker_file() {
        let dir = TempDir::new("cmake");
        fs::write(dir.path().join("CMakeLists.txt"), "").unwrap();
        assert!(is_cpp_workspace(dir.path()));
    }

    #[test]
    fn detects_source_in_subdir() {
        let dir = TempDir::new("subdir");
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("lib.cc"), "").unwrap();
        assert!(is_cpp_workspace(dir.path()));
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = TempDir::new("rusty");
        fs::write(dir.path().join("main.rs"), "fn main(){}").unwrap();
        fs::write(dir.path().join("README.md"), "hi").unwrap();
        assert!(!is_cpp_workspace(dir.path()));
    }

    #[test]
    fn a_makefile_alone_is_not_a_cpp_workspace() {
        // Measured false positive: a Python service with a Makefile was handed
        // the whole C/C++ guidance prompt. A Makefile is not evidence of C.
        let dir = TempDir::new("pyproj");
        fs::write(dir.path().join("Makefile"), "test:\n\tpytest\n").unwrap();
        fs::write(dir.path().join("app.py"), "print(1)").unwrap();
        fs::write(dir.path().join("requirements.txt"), "pytest\n").unwrap();
        assert!(!is_cpp_workspace(dir.path()));
    }

    #[test]
    fn a_makefile_driven_c_project_is_still_detected() {
        // …and dropping Makefile as a marker must not lose real C projects:
        // the sources are the evidence.
        let dir = TempDir::new("cproj");
        fs::write(dir.path().join("Makefile"), "all:\n\tgcc -o a main.c\n").unwrap();
        fs::write(dir.path().join("main.c"), "int main(void){return 0;}").unwrap();
        assert!(is_cpp_workspace(dir.path()));
    }

    #[test]
    fn detects_cpp_buried_in_a_monorepo() {
        // The other measured miss: a JS/TS root whose C++ lives four levels
        // down got no C/C++ guidance at all.
        let dir = TempDir::new("monorepo");
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        let engine = dir.path().join("services/pricing/engine/src");
        fs::create_dir_all(&engine).unwrap();
        fs::write(engine.join("book.cpp"), "int f(){return 0;}").unwrap();
        assert!(is_cpp_workspace(dir.path()));
    }

    #[test]
    fn skips_dependency_directories() {
        // A C++ file vendored inside node_modules does not make this a C++ repo.
        let dir = TempDir::new("nodeproj");
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        let dep = dir.path().join("node_modules/sharp/src");
        fs::create_dir_all(&dep).unwrap();
        fs::write(dep.join("native.cpp"), "int f(){return 0;}").unwrap();
        assert!(!is_cpp_workspace(dir.path()));
    }

    #[test]
    fn stops_at_the_depth_limit() {
        // Documents the bound: detection is cheap by construction, so a source
        // buried deeper than MAX_DEPTH with nothing shallower is not found.
        let dir = TempDir::new("deep");
        let deep = dir.path().join("l1/l2/l3/l4/l5");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.cpp"), "int f(){return 0;}").unwrap();
        assert!(!is_cpp_workspace(dir.path()));
    }
}
