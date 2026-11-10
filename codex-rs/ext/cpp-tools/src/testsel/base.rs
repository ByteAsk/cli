//! `compare_base`: the same selection on the session-start commit.
//!
//! The base source is `git archive <H0>` unpacked into this tool's scratch
//! directory (the repository gets no worktree entry, nothing is checked out in
//! it), built in a tree of its own with the same options as the build tree in
//! use, and run with the same items, paths remapped. Results go to the shared
//! base-run cache ([`crate::base_run_cache`]), so each test runs on base once
//! per session. Tests whose status differs are the flips; untouched cases next
//! to a flipped one that exercise the same names or messages are neighbours —
//! the tests that encode the same rule for another construct.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use super::DirKind;
use super::Model;
use super::System;
use super::cmake;
use super::exec;
use super::exec::Ctx;
use super::exec::RunItem;
use super::select::Selection;
use super::symbols::Changes;
use crate::base_run_cache;
use crate::base_run_cache::BaseRunKey;
use crate::base_run_cache::Flip;
use crate::base_run_cache::TestStatus;
use crate::gitx;
use crate::session_head::Head;

const TOOL: &str = "cpp_test_select";
const STAMP: &str = ".byteask-base-commit";

#[derive(Clone, Debug, Default)]
pub(crate) struct BaseOutcome {
    pub flips: Vec<Flip>,
    /// `flipped name → neighbour case names`.
    pub neighbours: BTreeMap<String, Vec<String>>,
    pub notes: Vec<String>,
    /// Names run on base now / answered from the cache.
    pub ran: usize,
    pub cached: usize,
}

/// Inputs describing the run being compared.
pub(crate) struct Compared<'a> {
    pub model: &'a Model,
    pub head: &'a Head,
    pub scratch: &'a Path,
    /// Model target names that were built for the run.
    pub targets: &'a [String],
    pub items: &'a [RunItem],
    pub now: &'a BTreeMap<String, TestStatus>,
    pub sel: &'a Selection,
    pub changes: &'a Changes,
}

pub(crate) async fn compare(ctx: &Ctx, c: &Compared<'_>) -> BaseOutcome {
    let mut out = BaseOutcome::default();
    let key = BaseRunKey::new(&c.model.root, &c.head.sha, &c.model.config);
    let names: Vec<String> = c.now.keys().cloned().collect();
    let missing: BTreeSet<String> = base_run_cache::missing(&key, &names).into_iter().cloned().collect();
    out.cached = names.len() - missing.len();
    if !missing.is_empty() {
        match run_on_base(ctx, c, &missing).await {
            Ok(results) => {
                out.ran = results.len();
                if let Err(e) = base_run_cache::record(&key, results) {
                    out.notes.push(format!("could not write the base-run cache: {e}"));
                }
            }
            Err(why) => out.notes.push(format!("base run unavailable: {why}")),
        }
    }
    let Some(base) = base_run_cache::load(&key) else {
        return out;
    };
    out.flips = base_run_cache::flips(&base, c.now);
    for flip in &out.flips {
        let near = neighbours(c, &flip.name, &out.flips);
        if !near.is_empty() {
            out.neighbours.insert(flip.name.clone(), near);
        }
    }
    out
}

async fn run_on_base(
    ctx: &Ctx,
    c: &Compared<'_>,
    missing: &BTreeSet<String>,
) -> Result<Vec<(String, TestStatus)>, String> {
    let model = c.model;
    let (src, build) = match model.system {
        System::Meson => return Err("not supported for Meson build trees".to_string()),
        System::Make if model.dir_kind != DirKind::InTree => {
            return Err("the Makefile is generated (configured tree); compare with the unmodified tree via git stash instead".to_string());
        }
        System::Make => {
            let src = base_source(ctx, c).await?;
            (src.clone(), src)
        }
        System::CMake => {
            let src = base_source(ctx, c).await?;
            let build = c.scratch.join("base-build");
            if !build.join("CMakeCache.txt").is_file() {
                ctx.progress("configuring the base tree");
                let like = (model.dir_kind == DirKind::Existing).then_some(model.build_dir.as_path());
                let req = cmake::Request {
                    root: &src,
                    build_dir: None,
                    scratch: c.scratch,
                    version: model.version,
                    cmake_files: &[],
                    commit: None,
                };
                cmake::configure(&req, &build, like.or(Some(model.build_dir.as_path())), true).await?;
            }
            (src, build)
        }
    };
    let remap = |text: &str| -> String { remap_path(text, model, &src, &build) };
    // Build the same targets there.
    let mut base_model = model.clone();
    base_model.root = src.clone();
    base_model.build_dir = build.clone();
    for target in base_model.targets.iter_mut() {
        target.artifacts = target.artifacts.iter().map(|a| PathBuf::from(remap(&a.display().to_string()))).collect();
    }
    let mut not_built: BTreeSet<String> = BTreeSet::new();
    if !matches!(c.items.first(), Some(RunItem::MakeCheck { .. })) {
        ctx.progress("building the base tree");
        let built = exec::build(ctx, &base_model, &build, c.targets).await;
        if built.cancelled {
            return Err("cancelled".to_string());
        }
        not_built.extend(built.failed);
    }
    // The same items, remapped, restricted to what the cache lacks.
    let items: Vec<RunItem> = c
        .items
        .iter()
        .filter_map(|item| match item {
            RunItem::Ctest { names, .. } => {
                let keep: Vec<String> = names.iter().filter(|n| missing.contains(*n)).cloned().collect();
                (!keep.is_empty() || names.is_empty()).then(|| RunItem::Ctest { dir: build.clone(), names: keep })
            }
            RunItem::Direct {
                test,
                argv,
                workdir,
                env,
                framework,
                cases,
            } => {
                let wanted = missing.contains(test) || missing.iter().any(|m| m.starts_with(&format!("{test}::")));
                wanted.then(|| RunItem::Direct {
                    test: test.clone(),
                    argv: argv.iter().map(|a| remap(a)).collect(),
                    workdir: PathBuf::from(remap(&workdir.display().to_string())),
                    env: env.iter().map(|(k, v)| (k.clone(), remap(v))).collect(),
                    framework: *framework,
                    cases: cases.clone(),
                })
            }
            RunItem::MakeCheck { target, .. } => Some(RunItem::MakeCheck { dir: src.clone(), target: target.clone() }),
        })
        .collect();
    ctx.progress("running the selection on the base tree");
    let ran = exec::run(ctx, &items).await;
    if ran.cancelled {
        return Err("cancelled".to_string());
    }
    let mut results: Vec<(String, TestStatus)> = Vec::new();
    for name in missing {
        let test = name.split("::").next().unwrap_or(name);
        let target = model
            .tests
            .iter()
            .find(|t| t.name == test)
            .and_then(|t| t.target.clone());
        let status = if target.is_some_and(|t| not_built.contains(&t)) {
            TestStatus::NotBuilt
        } else {
            match ran.results.get(name) {
                Some(status) => *status,
                // A case the base program does not have (new in this change):
                // its filter matched nothing on base.
                None if name.contains("::") && ran.results.get(test) == Some(&TestStatus::NotRun) => TestStatus::NotRun,
                None => continue,
            }
        };
        results.push((name.clone(), status));
    }
    Ok(results)
}

/// Unpack `git archive <H0>` into the scratch directory once per commit.
async fn base_source(ctx: &Ctx, c: &Compared<'_>) -> Result<PathBuf, String> {
    let src = c.scratch.join("base-src");
    let stamp = src.join(STAMP);
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == c.head.sha) {
        return Ok(src);
    }
    ctx.progress("unpacking the session-start commit");
    if src.exists() {
        // Ours (inside this tool's scratch directory), from another commit.
        std::fs::remove_dir_all(&src).map_err(|e| format!("cannot clear {}: {e}", src.display()))?;
    }
    std::fs::create_dir_all(&src).map_err(|e| format!("cannot create {}: {e}", src.display()))?;
    let tar = c.scratch.join("base.tar");
    let archive = gitx::git_str(
        TOOL,
        None,
        &c.model.root,
        &["archive", "--format=tar", "-o", &tar.display().to_string(), &c.head.sha],
        std::time::Duration::from_secs(300),
    )
    .await;
    if !archive.ok {
        return Err(format!("git archive failed: {}", archive.stderr.trim()));
    }
    let untar = exec::run_logged(
        ctx,
        "tar",
        &["-xf".to_string(), tar.display().to_string(), "-C".to_string(), src.display().to_string()],
        c.scratch,
        &[],
        std::time::Duration::from_secs(300),
    )
    .await;
    let _ = std::fs::remove_file(&tar);
    if !untar.ok() {
        return Err(format!("unpacking the base commit failed: {}", untar.output.trim()));
    }
    let _ = std::fs::write(&stamp, &c.head.sha);
    Ok(src)
}

/// Rewrite paths inside the build tree to the base build tree and paths in
/// the source tree to the base source tree.
fn remap_path(text: &str, model: &Model, src: &Path, build: &Path) -> String {
    let b = model.build_dir.display().to_string();
    let r = model.root.display().to_string();
    if !b.is_empty() && text.starts_with(&b) {
        return format!("{}{}", build.display(), &text[b.len()..]);
    }
    if !r.is_empty() && text.starts_with(&r) {
        return format!("{}{}", src.display(), &text[r.len()..]);
    }
    text.replace(&format!("{b}/"), &format!("{}/", build.display()))
        .replace(&format!("{r}/"), &format!("{}/", src.display()))
}

/// Cases in the same file and group as the flipped `name` (`test::case`) that
/// mention the same changed names or literals and did not flip.
fn neighbours(c: &Compared<'_>, name: &str, flips: &[Flip]) -> Vec<String> {
    let Some((_, case_filter)) = name.split_once("::") else {
        return Vec::new();
    };
    let flipped: BTreeSet<&str> = flips
        .iter()
        .filter_map(|f| f.name.split_once("::").map(|(_, c)| c))
        .collect();
    let mut out = Vec::new();
    for (file, cases) in &c.sel.cases {
        let Some(me) = cases.iter().find(|k| k.filter == case_filter || k.name == case_filter) else {
            continue;
        };
        let words: BTreeSet<&str> = c
            .sel
            .hits
            .iter()
            .filter(|h| &h.file == file && h.case.is_some_and(|i| cases.get(i) == Some(me)))
            .map(|h| h.word.as_str())
            .collect();
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        for other in cases {
            if other == me || other.suite != me.suite || flipped.contains(other.filter.as_str()) {
                continue;
            }
            let body = lines
                .get(other.start.saturating_sub(1) as usize..(other.end as usize).min(lines.len()))
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            let shares_word = words.iter().any(|w| super::symbols::contains_word(&body, w));
            let shares_literal = c.changes.literals.iter().any(|l| body.contains(l.as_str()));
            if shares_word || shares_literal {
                out.push(format!("{} (line {})", other.name, other.start));
            }
            if out.len() >= 5 {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsel::DirKind;

    #[test]
    fn remaps_build_and_source_paths() {
        let model = Model {
            system: System::CMake,
            root: PathBuf::from("/r"),
            build_dir: PathBuf::from("/r/build"),
            dir_kind: DirKind::Existing,
            version: None,
            ninja: false,
            targets: Vec::new(),
            tests: Vec::new(),
            check_target: None,
            make_rebuilds: None,
            mapping_note: String::new(),
            notes: Vec::new(),
            config: String::new(),
        };
        let src = Path::new("/s/base-src");
        let build = Path::new("/s/base-build");
        assert_eq!(remap_path("/r/build/bin/t", &model, src, build), "/s/base-build/bin/t");
        assert_eq!(remap_path("/r/tests/data", &model, src, build), "/s/base-src/tests/data");
        assert_eq!(remap_path("--data=/r/tests/x", &model, src, build), "--data=/s/base-src/tests/x");
        assert_eq!(remap_path("--flag", &model, src, build), "--flag");
    }
}
