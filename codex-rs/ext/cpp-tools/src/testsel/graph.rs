//! Changed files → affected build targets.
//!
//! A changed translation unit affects the targets that compile it. A changed
//! header affects every target compiling a TU that includes it, found two ways
//! and unioned: depfiles of an earlier build (exact, but only for what was
//! built, and stale for includes added since) and a textual `#include` closure
//! over the source tree (no build needed; over-approximates, which is the safe
//! direction). Targets that link an affected target are affected in turn.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use super::Model;
use super::System;
use super::is_cpp_file;
use super::is_header;
use super::is_source;
use super::normalize;
use crate::run::RunOutcome;
use crate::run::run_capped;

/// Most files the textual include scan reads.
const SCAN_FILE_CAP: usize = 40_000;
/// Largest file the scan reads (generated single-header copies are large).
const SCAN_BYTES_CAP: u64 = 8 * 1024 * 1024;
/// Most dependency-file bytes read from a build tree.
const DEPFILE_BYTES_CAP: u64 = 512 * 1024 * 1024;

/// What a change reaches in the build graph.
#[derive(Clone, Debug, Default)]
pub(crate) struct Affected {
    /// Affected targets, including everything that links them.
    pub targets: BTreeSet<String>,
    /// Targets that compile a changed file or a file including a changed
    /// header directly (distance 1), before the link closure.
    pub direct: BTreeSet<String>,
    /// TUs that include a changed header (transitively).
    pub includers: BTreeSet<PathBuf>,
    /// Changed C/C++ files no target compiles or includes.
    pub unmapped: Vec<PathBuf>,
    /// The include scan stopped early (too many files); results are partial.
    pub scan_truncated: bool,
    /// Depfiles of an earlier build were used.
    pub used_depfiles: bool,
}

/// Compute what `changed` (absolute paths) reaches in `model`.
pub(crate) async fn affected(model: &Model, changed: &[PathBuf]) -> Affected {
    let mut out = Affected::default();
    let changed: Vec<PathBuf> = changed
        .iter()
        .filter(|f| is_cpp_file(f))
        .map(|f| canonical(f))
        .collect();
    if changed.is_empty() {
        return out;
    }
    let headers: Vec<PathBuf> = changed.iter().filter(|f| !is_source(f)).cloned().collect();
    // Files that include a changed header, transitively (headers and TUs).
    let mut reach: BTreeSet<PathBuf> = BTreeSet::new();
    if !headers.is_empty() {
        let graph = IncludeGraph::scan(&model.root);
        out.scan_truncated = graph.truncated;
        reach = graph.includers_closure(&headers);
        out.includers = reach.iter().filter(|f| is_source(f)).cloned().collect();
    }
    let mut hit_files: BTreeSet<&PathBuf> = changed.iter().collect();
    hit_files.extend(reach.iter());
    let mut direct: BTreeSet<String> = BTreeSet::new();
    let mut mapped: BTreeSet<PathBuf> = BTreeSet::new();
    for target in &model.targets {
        for source in &target.sources {
            if hit_files.contains(source) {
                direct.insert(target.name.clone());
                mapped.insert(source.clone());
            }
        }
    }
    if !headers.is_empty() {
        let edges = dep_edges(model, &headers).await;
        out.used_depfiles = !edges.is_empty();
        for (file, target) in edges {
            if model.target(&target).is_some() {
                direct.insert(target);
                mapped.insert(file);
            }
        }
    }
    for file in &changed {
        let reached_by_includer = is_header(file) && !out.includers.is_empty();
        if !mapped.contains(file) && !reached_by_includer {
            out.unmapped.push(file.clone());
        }
    }
    out.targets = reverse_closure(model, &direct);
    out.direct = direct;
    out
}

/// Whether a file some target compiles includes, directly or through other files, any of `files` (sources
/// or headers: a unity source such as ada's src/ada.cpp includes .cpp files). None when the include scan was cut
/// short (unknown).
pub(crate) fn included_by_compiled(model: &Model, files: &[PathBuf]) -> Option<bool> {
    let graph = IncludeGraph::scan(&model.root);
    if graph.truncated {
        return None;
    }
    let seeds: Vec<PathBuf> = files.iter().map(|f| canonical(f)).collect();
    let reach = graph.includers_closure(&seeds);
    Some(
        model
            .targets
            .iter()
            .any(|target| target.sources.iter().any(|source| reach.contains(source))),
    )
}

/// `seeds` plus every target that (transitively) depends on one of them.
pub(crate) fn reverse_closure(model: &Model, seeds: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = seeds.clone();
    loop {
        let before = out.len();
        for target in &model.targets {
            if !out.contains(&target.name) && target.deps.iter().any(|d| out.contains(d)) {
                out.insert(target.name.clone());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Textual include graph
// ---------------------------------------------------------------------------

/// `#include` directives of every C/C++ file under a root.
struct IncludeGraph {
    /// Included spec's file name → (includer, spec as written, quoted?).
    by_basename: HashMap<String, Vec<(PathBuf, String, bool)>>,
    truncated: bool,
}

impl IncludeGraph {
    fn scan(root: &Path) -> Self {
        let mut graph = IncludeGraph {
            by_basename: HashMap::new(),
            truncated: false,
        };
        let mut files = 0usize;
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !(e.file_type().is_dir() && e.depth() > 0 && super::skip_dir(e.path()))
            });
        for entry in walker.flatten() {
            if !entry.file_type().is_file() || !is_cpp_file(entry.path()) {
                continue;
            }
            files += 1;
            if files > SCAN_FILE_CAP {
                graph.truncated = true;
                break;
            }
            if entry.metadata().map(|m| m.len()).unwrap_or(0) > SCAN_BYTES_CAP {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let includer = canonical(entry.path());
            for (spec, quoted) in include_specs(&text) {
                let base = spec.rsplit('/').next().unwrap_or(&spec).to_string();
                graph
                    .by_basename
                    .entry(base)
                    .or_default()
                    .push((includer.clone(), spec, quoted));
            }
        }
        graph
    }

    /// Files that include `target` directly.
    fn includers_of(&self, target: &Path) -> Vec<PathBuf> {
        let Some(name) = target.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return Vec::new();
        };
        let Some(entries) = self.by_basename.get(&name) else {
            return Vec::new();
        };
        let target_str = target.to_string_lossy();
        entries
            .iter()
            .filter(|(includer, spec, quoted)| {
                if *quoted {
                    if let Some(dir) = includer.parent() {
                        if canonical(&normalize(dir, Path::new(spec))) == target {
                            return true;
                        }
                    }
                }
                let spec = spec.trim_start_matches("./");
                target_str.ends_with(&format!("/{spec}")) || target_str == spec
            })
            .map(|(includer, _, _)| includer.clone())
            .collect()
    }

    fn includers_closure(&self, seeds: &[PathBuf]) -> BTreeSet<PathBuf> {
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut queue: Vec<PathBuf> = seeds.to_vec();
        while let Some(file) = queue.pop() {
            for includer in self.includers_of(&file) {
                if seen.insert(includer.clone()) {
                    queue.push(includer);
                }
            }
        }
        seen
    }
}

/// `(spec, quoted)` for every `#include "x"` / `#include <x>` in `text`.
pub(crate) fn include_specs(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest
            .strip_prefix("include")
            .or_else(|| rest.strip_prefix("import"))
            .or_else(|| rest.strip_prefix("include_next"))
        else {
            continue;
        };
        let rest = rest.trim_start();
        let (open, close, quoted) = match rest.chars().next() {
            Some('"') => ('"', '"', true),
            Some('<') => ('<', '>', false),
            _ => continue,
        };
        let inner = &rest[open.len_utf8()..];
        if let Some(end) = inner.find(close) {
            let spec = inner[..end].trim();
            if !spec.is_empty() {
                out.push((spec.to_string(), quoted));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Depfiles of an earlier build
// ---------------------------------------------------------------------------

/// `(changed header, target)` for every object of an earlier build whose
/// dependency list contains one of `headers`. Read-only on the build tree.
pub(crate) async fn dep_edges(model: &Model, headers: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let wanted: BTreeSet<PathBuf> = headers.iter().map(|h| canonical(h)).collect();
    let mut out: BTreeSet<(PathBuf, String)> = BTreeSet::new();
    if model.ninja {
        let args = vec![
            "-C".to_string(),
            model.build_dir.display().to_string(),
            "-t".to_string(),
            "deps".to_string(),
        ];
        if let Ok(RunOutcome::Finished {
            success: true,
            stdout,
            ..
        }) = run_capped(
            "ninja",
            &args,
            &model.build_dir,
            std::time::Duration::from_secs(60),
        )
        .await
        {
            for (obj, deps) in parse_ninja_deps(&stdout) {
                let Some(target) = object_target(model, &obj) else {
                    continue;
                };
                for dep in deps {
                    let dep = canonical(&normalize(&model.build_dir, Path::new(&dep)));
                    if wanted.contains(&dep) {
                        out.insert((dep, target.clone()));
                    }
                }
            }
        }
    }
    if model.system == System::CMake && !model.ninja {
        let mut budget = DEPFILE_BYTES_CAP;
        for (dir, target) in cmake_target_dirs(&model.build_dir) {
            let base = dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| model.build_dir.clone());
            for file in depfiles_in(&dir) {
                let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                if len > budget {
                    break;
                }
                budget -= len;
                let Ok(text) = std::fs::read_to_string(&file) else {
                    continue;
                };
                for (_, deps) in parse_make_deps(&text) {
                    for dep in deps {
                        for candidate in [
                            normalize(&base, Path::new(&dep)),
                            normalize(&model.build_dir, Path::new(&dep)),
                        ] {
                            let candidate = if wanted.contains(&candidate) {
                                candidate
                            } else {
                                canonical(&candidate)
                            };
                            if wanted.contains(&candidate) {
                                out.insert((candidate, target.clone()));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Every `…/CMakeFiles/<target>.dir` under a CMake build tree, with the target name.
pub(crate) fn cmake_target_dirs(build: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(build)
        .max_depth(12)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_dir()
                && (name == "Testing"
                    || name == ".cmake"
                    || name.starts_with('.') && e.depth() > 0))
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let parent_is_cmakefiles = entry
            .path()
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|p| p == "CMakeFiles");
        if parent_is_cmakefiles {
            if let Some(target) = name.strip_suffix(".dir") {
                out.push((entry.path().to_path_buf(), target.to_string()));
            }
        }
    }
    out.sort();
    out
}

fn depfiles_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(8)
        .into_iter()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().is_file()
            && (name == "depend.make"
                || name == "compiler_depend.make"
                || name.ends_with(".o.d")
                || name.ends_with(".obj.d"))
        {
            out.push(entry.path().to_path_buf());
        }
    }
    out
}

/// The CMake target an object path belongs to (`…/CMakeFiles/<t>.dir/…`), or a
/// Meson target (`<output>.p/…`).
fn object_target(model: &Model, obj: &str) -> Option<String> {
    let parts: Vec<&str> = obj.split('/').collect();
    for window in parts.windows(2) {
        if window[0] == "CMakeFiles" {
            if let Some(name) = window[1].strip_suffix(".dir") {
                return Some(name.to_string());
            }
        }
    }
    for part in &parts {
        if let Some(output) = part.strip_suffix(".p") {
            return model
                .targets
                .iter()
                .find(|t| {
                    t.artifacts
                        .iter()
                        .any(|a| a.file_name().is_some_and(|n| n == output))
                })
                .map(|t| t.name.clone());
        }
    }
    None
}

/// Make-syntax dependency lines: `obj: dep dep \` continued.
pub(crate) fn parse_make_deps(text: &str) -> Vec<(String, Vec<String>)> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in joined.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `C:\x` style drive colons do not occur on the platforms this runs on.
        let Some(colon) = line
            .find(": ")
            .or_else(|| line.strip_suffix(':').map(str::len))
        else {
            continue;
        };
        let obj = line[..colon].trim().to_string();
        let deps = line[colon + 1..]
            .split_whitespace()
            .filter(|d| !d.is_empty() && *d != "\\")
            .map(str::to_string);
        out.entry(obj).or_default().extend(deps);
    }
    out.into_iter().collect()
}

/// `ninja -t deps` output: `obj: #deps N, …` followed by indented deps.
pub(crate) fn parse_ninja_deps(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let Some((_, deps)) = out.last_mut() {
                deps.push(line.trim().to_string());
            }
        } else if let Some((obj, _)) = line.split_once(": #deps") {
            out.push((obj.to_string(), Vec::new()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::testsel::DirKind;
    use crate::testsel::Target;

    #[test]
    fn parses_include_directives() {
        let text = "#include \"a/b.h\"\n  #  include <vector>\n#include MACRO\n#import \"x.h\"\n// #include \"no.h\" is a comment? kept as text\n";
        assert_eq!(
            include_specs(text),
            vec![
                ("a/b.h".to_string(), true),
                ("vector".to_string(), false),
                ("x.h".to_string(), true),
            ]
        );
    }

    #[test]
    fn parses_depfiles() {
        let make = "CMakeFiles/core.dir/src/core.cpp.o: /r/src/core.cpp \\\n  /r/include/fx/core.h /usr/include/stdio.h\n";
        assert_eq!(
            parse_make_deps(make),
            vec![(
                "CMakeFiles/core.dir/src/core.cpp.o".to_string(),
                vec![
                    "/r/src/core.cpp".to_string(),
                    "/r/include/fx/core.h".to_string(),
                    "/usr/include/stdio.h".to_string()
                ]
            )]
        );
        let ninja = "CMakeFiles/core.dir/src/core.cpp.o: #deps 2, deps mtime 123 (VALID)\n    ../src/core.cpp\n    ../include/fx/core.h\n\n";
        assert_eq!(
            parse_ninja_deps(ninja),
            vec![(
                "CMakeFiles/core.dir/src/core.cpp.o".to_string(),
                vec![
                    "../src/core.cpp".to_string(),
                    "../include/fx/core.h".to_string()
                ]
            )]
        );
    }

    fn model(root: &Path, targets: Vec<Target>) -> Model {
        Model {
            system: System::CMake,
            root: root.to_path_buf(),
            build_dir: root.join("nonexistent-build"),
            dir_kind: DirKind::Scratch,
            version: None,
            ninja: false,
            targets,
            tests: Vec::new(),
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        }
    }

    #[tokio::test]
    async fn a_header_change_reaches_includers_and_their_dependents() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        for (path, text) in [
            ("include/fx/core.h", "int twice(int);\n"),
            (
                "include/fx/util.h",
                "#include \"core.h\"\ninline int clampi(int x) { return x; }\n",
            ),
            (
                "src/core.cpp",
                "#include \"fx/core.h\"\nint twice(int x) { return 2 * x; }\n",
            ),
            ("src/util.cpp", "#include <fx/util.h>\n"),
            ("src/other.cpp", "int other() { return 0; }\n"),
            ("tests/api_test.cpp", "#include \"fx/util.h\"\n"),
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        }
        let t = |name: &str, exe: bool, sources: &[&str], deps: &[&str]| Target {
            name: name.to_string(),
            is_exe: exe,
            artifacts: Vec::new(),
            sources: sources.iter().map(|s| root.join(s)).collect(),
            deps: deps.iter().map(|d| (*d).to_string()).collect(),
        };
        let m = model(
            &root,
            vec![
                t("core", false, &["src/core.cpp"], &[]),
                t("util", false, &["src/util.cpp"], &[]),
                t("other", false, &["src/other.cpp"], &[]),
                t("core_test", true, &["tests/core_test.cpp"], &["core"]),
                t("api_test", true, &["tests/api_test.cpp"], &["util"]),
                t("other_test", true, &["tests/other_test.cpp"], &["other"]),
            ],
        );
        // A source change: its target and what links it.
        let a = affected(&m, &[root.join("src/core.cpp")]).await;
        assert_eq!(
            a.targets,
            ["core", "core_test"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
        // A header change: every TU including it, transitively, plus dependents.
        let a = affected(&m, &[root.join("include/fx/core.h")]).await;
        assert_eq!(
            a.targets,
            ["api_test", "core", "core_test", "util"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
        assert!(a.unmapped.is_empty());
        // A file nothing compiles is reported.
        std::fs::write(root.join("src/orphan.cpp"), "").unwrap();
        let a = affected(&m, &[root.join("src/orphan.cpp")]).await;
        assert!(a.targets.is_empty());
        assert_eq!(a.unmapped, vec![root.join("src/orphan.cpp")]);
    }
}
