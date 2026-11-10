//! `cpp_impact`: a deterministic change-impact checklist for a C/C++ diff.
//!
//! Recall comes from text (a C/C++ scanner over every tracked source, plus `git grep` for
//! tests, docs and build files); precision is added by clangd where a compile database exists
//! (callers resolved with `textDocument/definition`). The checklist lists, for the entities the
//! diff touches: overloads and look-alike functions, `switch` siblings, related types that lack
//! a newly added operation, template specialisations, virtual overrides, declarations that
//! differ by build configuration, stale generated copies, callers, and the tests and docs that
//! mention them. It is capped at about 1.5k tokens.
//!
//! Every heuristic is structural (same scope, same switch, same template, same `#if` group,
//! shared operations, similar bodies); nothing is specific to a project.

pub(crate) mod diff;
pub(crate) mod scan;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::gitx;
use diff::Hunk;
use scan::Entity;
use scan::EntityKind;
use scan::FileScan;
use scan::TokKind;

/// Hard cap on the rendered checklist, in bytes (~1.5k tokens).
pub(crate) const RENDER_CAP_BYTES: usize = 6_000;
/// Default number of items.
pub(crate) const DEFAULT_MAX_ITEMS: usize = 30;
/// Files and bytes indexed by the textual tier.
const INDEX_MAX_FILES: usize = 4_000;
const INDEX_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const INDEX_MAX_TOTAL_BYTES: u64 = 96 * 1024 * 1024;
/// Changed files and entities considered.
const MAX_CHANGED_FILES: usize = 40;
const MAX_CHANGED_ENTITIES: usize = 16;

#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// The working tree against a base revision (plus untracked C/C++ files).
    Diff,
    /// Named symbols (`ns::Type::method`, `name`, or `path:line`), for planning before an edit.
    Symbols(Vec<String>),
}

#[derive(Clone, Debug)]
pub(crate) struct ImpactRequest {
    pub root: PathBuf,
    pub base_rev: String,
    pub source: Source,
    pub budget: Duration,
    pub use_clangd: bool,
    pub max_items: usize,
    /// Untracked paths that existed before the change (session start); `dir/` entries cover
    /// everything below them. They are not part of the diff.
    pub preexisting_untracked: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Facet {
    Peers,
    Configs,
    Copies,
    Overloads,
    LookAlikes,
    Switch,
    Specialisations,
    Overrides,
    Callers,
    Tests,
    Docs,
}

impl Facet {
    fn heading(self) -> &'static str {
        match self {
            Facet::Peers => "RELATED TYPES LACKING AN OPERATION YOU ADDED",
            Facet::Configs => "BUILD-CONFIGURATION VARIANTS (#if)",
            Facet::Copies => "GENERATED / AMALGAMATED COPIES",
            Facet::Overloads => "OVERLOADS",
            Facet::LookAlikes => "LOOK-ALIKE FUNCTIONS (parallel code paths)",
            Facet::Switch => "SWITCH/CASE SIBLINGS",
            Facet::Specialisations => "TEMPLATE SPECIALISATIONS (the library's extension pattern)",
            Facet::Overrides => "VIRTUAL OVERRIDES / BASE DECLARATIONS",
            Facet::Callers => "CALLERS",
            Facet::Tests => "TESTS THAT MENTION THE CHANGED SYMBOLS",
            Facet::Docs => "DOCS THAT MENTION THE CHANGED SYMBOLS",
        }
    }
}

/// One checklist item.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub facet: Facet,
    /// Stable identity across runs (facet + location + subject).
    pub key: String,
    pub path: String,
    pub line: u32,
    pub text: String,
    /// `Some(true)` confirmed by clangd, `None` textual only.
    pub confirmed: Option<bool>,
}

/// One entity the diff touches.
#[derive(Clone, Debug)]
pub(crate) struct Changed {
    pub path: String,
    pub entity: Entity,
    pub added: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ImpactReport {
    pub base_short: String,
    pub engine: String,
    pub files_changed: usize,
    pub changed: Vec<Changed>,
    pub removed: Vec<String>,
    pub items: Vec<Item>,
    pub notes: Vec<String>,
    pub elapsed_ms: u128,
    /// Files the checklist found to be generated copies of changed files.
    pub generated_copies: Vec<String>,
}

/// Run the analysis. Never fails: problems become notes. It runs as its own task so that a
/// bug in the heuristics (a panic) costs the checklist, never the user's turn.
pub(crate) async fn analyze(req: &ImpactRequest) -> ImpactReport {
    let owned = req.clone();
    match tokio::spawn(crate::sandbox::inherit(async move {
        analyze_inner(&owned).await
    }))
    .await
    {
        Ok(report) => report,
        Err(err) => ImpactReport {
            base_short: req.base_rev.chars().take(10).collect(),
            engine: "textual scan".to_string(),
            notes: vec![format!(
                "the analysis failed internally ({err}); no checklist"
            )],
            ..Default::default()
        },
    }
}

async fn analyze_inner(req: &ImpactRequest) -> ImpactReport {
    let started = Instant::now();
    let deadline = started + req.budget;
    let mut report = ImpactReport {
        base_short: req.base_rev.chars().take(10).collect(),
        engine: "textual scan".to_string(),
        ..Default::default()
    };
    // 1. What changed.
    let (file_changes, names_from_symbols) = match &req.source {
        Source::Diff => (
            collect_changes(&req.root, &req.base_rev, &req.preexisting_untracked).await,
            Vec::new(),
        ),
        Source::Symbols(symbols) => (Vec::new(), symbols.clone()),
    };
    report.files_changed = file_changes.len();

    // 2. Index every tracked C/C++ file (bounded), in a blocking task.
    let tracked = gitx::ls_files(&req.root).await;
    let extra: Vec<String> = file_changes
        .iter()
        .filter(|change| change.new_file)
        .map(|change| change.path.clone())
        .collect();
    let root = req.root.clone();
    let index = match tokio::task::spawn_blocking(move || build_index(&root, tracked, extra)).await
    {
        Ok(index) => index,
        Err(_) => {
            report
                .notes
                .push("the textual index could not be built".to_string());
            return report;
        }
    };
    if index.truncated {
        report.notes.push(format!(
            "large repository: only {} C/C++ files were indexed; items may be missing",
            index.files.len()
        ));
    }

    // 3. Changed entities.
    let mut changed = Vec::new();
    for change in &file_changes {
        changed.extend(change.changed_entities());
        if !index.name_generated.contains(&change.path) {
            report.removed.extend(change.removed_entities());
        }
    }
    if let Source::Symbols(_) = &req.source {
        changed = resolve_symbols(&index, &names_from_symbols);
        if changed.is_empty() {
            report
                .notes
                .push("none of the named symbols was found by the textual scan".to_string());
        }
    }
    // 4. Facets. Copies first: they tell the rest which files to ignore (an edit the agent
    // also made in a generated copy is not a second change).
    let mut items = Vec::new();
    let (copy_items, copies) = copies_facet(&req.root, &file_changes, &index.name_generated).await;
    report.generated_copies = copies.iter().cloned().collect();
    let generated: HashSet<String> = copies
        .iter()
        .cloned()
        .chain(index.name_generated.iter().cloned())
        .collect();
    items.extend(copy_items);
    if changed.iter().any(|c| !generated.contains(&c.path)) {
        changed.retain(|c| !generated.contains(&c.path));
    }
    dedupe_changed(&mut changed);
    changed.truncate(MAX_CHANGED_ENTITIES);
    report.changed = changed.clone();
    let changed_keys: HashSet<String> = changed.iter().map(|c| c.entity.key()).collect();
    let ctx = FacetCtx {
        index: &index,
        changed: &changed,
        changed_keys: &changed_keys,
        generated: &generated,
        changes: &file_changes,
    };
    items.extend(ctx.overloads());
    items.extend(ctx.look_alikes());
    items.extend(ctx.switch_siblings());
    items.extend(ctx.peers());
    items.extend(ctx.specialisations());
    items.extend(ctx.overrides());
    items.extend(ctx.configs());
    items.extend(config_axes(&req.root, &changed, &index).await);
    let mut callers = ctx.callers();

    // 5. clangd confirmation of callers (bounded; only with a compile database).
    if req.use_clangd && !callers.is_empty() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Some(engine) = confirm_callers(&req.root, &mut callers, remaining).await {
            report.engine = format!("textual scan + {engine}");
        }
    }
    items.extend(callers.into_iter().map(|c| c.item));
    items.extend(mentions_facet(&req.root, &changed, &index).await);
    if Instant::now() > deadline {
        report
            .notes
            .push("time budget reached: later facets may be incomplete".to_string());
    }

    // 6. Rank, dedupe, cap.
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.key.clone()));
    items.sort_by(|a, b| (a.facet, &a.path, a.line).cmp(&(b.facet, &b.path, b.line)));
    // Keep facet diversity: at most 6 items per facet before the global cap.
    let mut per_facet: HashMap<Facet, usize> = HashMap::new();
    items.retain(|item| {
        let count = per_facet.entry(item.facet).or_default();
        *count += 1;
        *count <= 6
    });
    let dropped = items.len().saturating_sub(req.max_items);
    items.truncate(req.max_items);
    if dropped > 0 {
        report
            .notes
            .push(format!("{dropped} lower-priority item(s) not shown"));
    }
    report.items = items;
    report.elapsed_ms = started.elapsed().as_millis();
    report
}

// ---------------------------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------------------------

/// One changed C/C++ file.
#[derive(Clone, Debug)]
pub(crate) struct FileChange {
    pub path: String,
    pub new_file: bool,
    pub cur: Arc<FileScan>,
    pub base: Option<Arc<FileScan>>,
    /// Zero-context hunks (exact changed lines).
    pub hunks: Vec<Hunk>,
    /// Three-line-context hunks (for the copies check).
    pub ctx_hunks: Vec<Hunk>,
}

impl FileChange {
    fn changed_entities(&self) -> Vec<Changed> {
        let base_keys: HashSet<String> = self
            .base
            .as_ref()
            .map(|base| base.entities.iter().map(Entity::key).collect())
            .unwrap_or_default();
        let base_names: HashSet<String> = self
            .base
            .as_ref()
            .map(|base| base.entities.iter().map(Entity::qualified).collect())
            .unwrap_or_default();
        let mut out: Vec<Changed> = Vec::new();
        let mut seen = HashSet::new();
        for line in diff::touched_new_lines(&self.hunks) {
            // Every entity whose span contains the line; keep the innermost function, plus a
            // class only when no member function contains the line.
            let Some(entity) = self.cur.entity_at(line) else {
                continue;
            };
            if !seen.insert(entity.key()) {
                continue;
            }
            // Modified, not added, when the base has the same name with the same parameter
            // types (a body or qualifier change); a new parameter list is a new overload.
            let added = !base_keys.contains(&entity.key())
                && !(base_names.contains(&entity.qualified()) && self.base_has_same_params(entity));
            out.push(Changed {
                path: self.path.clone(),
                entity: entity.clone(),
                added,
            });
        }
        out
    }

    fn base_has_same_params(&self, entity: &Entity) -> bool {
        self.base.as_ref().is_some_and(|base| {
            base.entities
                .iter()
                .any(|e| e.qualified() == entity.qualified() && e.param_types == entity.param_types)
        })
    }

    fn removed_entities(&self) -> Vec<String> {
        let Some(base) = &self.base else {
            return Vec::new();
        };
        let cur_keys: HashSet<String> = self.cur.entities.iter().map(Entity::key).collect();
        let removed_lines: BTreeSet<u32> = self
            .hunks
            .iter()
            .flat_map(|hunk| hunk.removed.iter().copied())
            .collect();
        let mut out = Vec::new();
        for entity in &base.entities {
            if entity.kind != EntityKind::Function || cur_keys.contains(&entity.key()) {
                continue;
            }
            if removed_lines.contains(&entity.name_line) {
                out.push(format!(
                    "{} ({}:{})",
                    short_sig(entity),
                    self.path,
                    entity.line
                ));
            }
        }
        out
    }
}

/// The changed C/C++ files of the working tree against `base`.
pub(crate) async fn collect_changes(
    root: &Path,
    base: &str,
    preexisting: &[String],
) -> Vec<FileChange> {
    let mut paths: Vec<(String, bool)> = gitx::changed_vs(root, base)
        .await
        .into_iter()
        .filter(|(status, path)| *status != 'D' && gitx::is_cpp_path(path))
        .map(|(status, path)| (path, status == 'A'))
        .collect();
    for path in gitx::untracked_files(root, 2_000).await {
        let old = preexisting
            .iter()
            .any(|p| *p == path || (p.ends_with('/') && path.starts_with(p.as_str())));
        if !old && gitx::is_cpp_path(&path) && !looks_like_build_output(&path) {
            paths.push((path, true));
        }
    }
    paths.truncate(MAX_CHANGED_FILES);
    let mut out = Vec::new();
    for (path, new_file) in paths {
        let full = root.join(&path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let base_text = if new_file {
            None
        } else {
            gitx::show(root, base, &path).await
        };
        let new_file = new_file || base_text.is_none();
        let (hunks, ctx_hunks) = if new_file {
            let count = text.lines().count() as u32;
            let hunk = Hunk {
                new_start: 1,
                new_count: count,
                new_lines: text.lines().map(str::to_string).collect(),
                added: (1..=count).collect(),
                ..Default::default()
            };
            (vec![hunk.clone()], Vec::new())
        } else {
            let zero = gitx::git(
                root,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-U0",
                    base,
                    "--",
                    &path,
                ],
            )
            .await
            .unwrap_or_default();
            let ctx = gitx::git(
                root,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-U3",
                    base,
                    "--",
                    &path,
                ],
            )
            .await
            .unwrap_or_default();
            (diff::parse_hunks(&zero), diff::parse_hunks(&ctx))
        };
        if hunks.is_empty() {
            continue;
        }
        let cur = Arc::new(scan::scan(&text));
        let base_scan = base_text.map(|b| Arc::new(scan::scan(&b)));
        out.push(FileChange {
            path,
            new_file,
            cur,
            base: base_scan,
            hunks,
            ctx_hunks,
        });
    }
    out
}

/// Scratch build trees and agent-made temporary directories are not part of a change.
pub(crate) fn looks_like_build_output(path: &str) -> bool {
    path.split('/').any(|part| {
        let lower = part.to_ascii_lowercase();
        lower.starts_with("build")
            || lower.starts_with("cmake-build")
            || lower == "cmakefiles"
            || lower == "_build"
            || lower == "out"
            || (lower.starts_with('.') && lower.len() > 1)
    })
}

fn dedupe_changed(changed: &mut Vec<Changed>) {
    let mut seen = HashSet::new();
    changed.retain(|c| seen.insert((c.path.clone(), c.entity.key())));
    // Functions first (they drive most facets), then classes.
    changed.sort_by_key(|c| {
        (
            c.entity.kind != EntityKind::Function,
            c.path.clone(),
            c.entity.line,
        )
    });
}

fn resolve_symbols(index: &Index, symbols: &[String]) -> Vec<Changed> {
    let mut out = Vec::new();
    for symbol in symbols {
        let symbol = symbol.trim();
        if let Some((path, line)) = symbol.rsplit_once(':')
            && let Ok(line) = line.parse::<u32>()
            && let Some(scan) = index.files.get(path)
            && let Some(entity) = scan.entity_at(line)
        {
            out.push(Changed {
                path: path.to_string(),
                entity: entity.clone(),
                added: false,
            });
            continue;
        }
        let wanted = symbol.trim_start_matches("::");
        for (path, scan) in &index.files {
            for entity in &scan.entities {
                let qualified = entity.qualified();
                if qualified == wanted
                    || qualified.ends_with(&format!("::{wanted}"))
                    || entity.name == wanted
                {
                    out.push(Changed {
                        path: path.clone(),
                        entity: entity.clone(),
                        added: false,
                    });
                }
            }
        }
    }
    out.truncate(MAX_CHANGED_ENTITIES);
    out
}

// ---------------------------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------------------------

pub(crate) struct Index {
    pub files: BTreeMap<String, Arc<FileScan>>,
    pub truncated: bool,
    /// Files whose path or banner says they are generated.
    pub name_generated: HashSet<String>,
}

fn build_index(root: &Path, tracked: Vec<String>, extra: Vec<String>) -> Index {
    let mut paths: Vec<String> = tracked
        .into_iter()
        .chain(extra)
        .filter(|path| gitx::is_cpp_path(path) && !gitx::is_vendored_path(path))
        .collect();
    paths.sort();
    paths.dedup();
    let truncated = paths.len() > INDEX_MAX_FILES;
    paths.truncate(INDEX_MAX_FILES);
    let mut total: u64 = 0;
    let mut selected = Vec::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(root.join(&path)) else {
            continue;
        };
        if !meta.is_file() || meta.len() > INDEX_MAX_FILE_BYTES {
            continue;
        }
        total += meta.len();
        if total > INDEX_MAX_TOTAL_BYTES {
            break;
        }
        selected.push(path);
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4);
    let chunks: Vec<Vec<String>> = selected
        .chunks(selected.len().div_ceil(threads).max(1))
        .map(<[String]>::to_vec)
        .collect();
    let mut files = BTreeMap::new();
    let mut name_generated = HashSet::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    for path in chunk {
                        let Ok(bytes) = std::fs::read(root.join(&path)) else {
                            continue;
                        };
                        let text = String::from_utf8_lossy(&bytes);
                        let generated = generated_banner(&path, &text);
                        out.push((path, Arc::new(scan::scan(&text)), generated));
                    }
                    out
                })
            })
            .collect();
        for handle in handles {
            if let Ok(results) = handle.join() {
                for (path, scan, generated) in results {
                    if generated {
                        name_generated.insert(path.clone());
                    }
                    files.insert(path, scan);
                }
            }
        }
    });
    Index {
        files,
        truncated: truncated || total > INDEX_MAX_TOTAL_BYTES,
        name_generated,
    }
}

/// A file whose path or first lines say it is generated (combined single-header builds).
fn generated_banner(path: &str, text: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains("single-header") || lower.contains("amalgam") {
        return true;
    }
    let head: String = text
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    head.contains("amalgamat")
        || head.contains("auto-generated")
        || head.contains("autogenerated")
        || head.contains("do not edit")
        || head.contains("generated by")
}

// ---------------------------------------------------------------------------------------------
// Facets
// ---------------------------------------------------------------------------------------------

struct FacetCtx<'a> {
    index: &'a Index,
    changed: &'a [Changed],
    changed_keys: &'a HashSet<String>,
    generated: &'a HashSet<String>,
    changes: &'a [FileChange],
}

/// A caller item plus what is needed to confirm it.
struct CallerItem {
    item: Item,
    target: Changed,
    sites: Vec<(String, u32)>,
}

fn short_sig(entity: &Entity) -> String {
    truncate_chars(&entity.signature(), 110)
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn is_macro_like(entity: &Entity) -> bool {
    entity.bare
        && entity
            .name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The unqualified base type of a parameter type: `const lib::Widget<T>&` → `Widget`.
fn base_type(param: &str) -> String {
    let mut text = param.to_string();
    for word in [
        "const ",
        "volatile ",
        "struct ",
        "class ",
        "typename ",
        "enum ",
    ] {
        text = text.replace(word, "");
    }
    let before_args = text.split('<').next().unwrap_or(&text);
    let cleaned: String = before_args
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
        .collect();
    cleaned.rsplit("::").next().unwrap_or("").to_string()
}

/// Template arguments of a parameter type that are plain names: `result<dom::element>` → `element`.
fn type_args(param: &str) -> Vec<String> {
    let Some(start) = param.find('<') else {
        return Vec::new();
    };
    let inner = &param[start + 1..param.rfind('>').unwrap_or(param.len())];
    inner
        .split(',')
        .map(base_type)
        .filter(|name| !name.is_empty())
        .collect()
}

fn jaccard<T: Eq + std::hash::Hash>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

impl FacetCtx<'_> {
    fn indexed(&self) -> impl Iterator<Item = (&String, &Arc<FileScan>)> {
        self.index
            .files
            .iter()
            .filter(|(path, _)| !self.generated.contains(*path))
    }

    fn functions(&self) -> impl Iterator<Item = (&String, &Arc<FileScan>, &Entity)> {
        self.indexed().flat_map(|(path, scan)| {
            scan.entities
                .iter()
                .filter(|e| e.kind == EntityKind::Function)
                .map(move |e| (path, scan, e))
        })
    }

    fn changed_functions(&self) -> impl Iterator<Item = &Changed> {
        self.changed
            .iter()
            .filter(|c| c.entity.kind == EntityKind::Function && !is_macro_like(&c.entity))
    }

    /// Location of an entity is inside a changed hunk of the diff?
    fn edited(&self, path: &str, entity: &Entity) -> bool {
        self.changes.iter().any(|change| {
            change.path == path
                && diff::touched_new_lines(&change.hunks)
                    .iter()
                    .any(|line| entity.line <= *line && *line <= entity.end_line)
        })
    }

    fn overloads(&self) -> Vec<Item> {
        let mut out = Vec::new();
        // Members of two different classes are related through the class hierarchy (the
        // overrides facet), not as overloads.
        let class_names: HashSet<&str> = self
            .indexed()
            .flat_map(|(_, scan)| scan.entities.iter())
            .filter(|e| e.kind == EntityKind::Class)
            .map(|e| e.name.as_str())
            .collect();
        let is_member = |e: &Entity| e.owner().is_some_and(|owner| class_names.contains(owner));
        for c in self.changed_functions() {
            let mut same_scope = Vec::new();
            let mut other_scope = Vec::new();
            for (path, _, e) in self.functions() {
                if e.name != c.entity.name || self.changed_keys.contains(&e.key()) {
                    continue;
                }
                if e.param_types == c.entity.param_types && e.scope == c.entity.scope {
                    // The declaration/definition twin of the changed function itself.
                    continue;
                }
                if self.edited(path, e) {
                    continue;
                }
                if e.scope == c.entity.scope {
                    // Members of two specialisations share a scope name but not a class.
                    let different_class = !self.same_class(&c.path, &c.entity, path, e);
                    let test_only = gitx::is_test_path(path) && !gitx::is_test_path(&c.path);
                    if different_class || test_only {
                        continue;
                    }
                    same_scope.push((path, e));
                } else if !(is_member(e) && is_member(&c.entity)) && !gitx::is_test_path(path) {
                    other_scope.push((path, e));
                }
            }
            // Same-scope overloads: definitions first, one entry per signature.
            let mut seen = HashSet::new();
            same_scope.sort_by_key(|(path, e)| (!e.is_def, *path != &c.path, e.line));
            for (path, e) in same_scope
                .into_iter()
                .filter(|(_, e)| seen.insert(e.key()))
                .take(4)
            {
                out.push(Item {
                    facet: Facet::Overloads,
                    key: format!("overload|{}|{}", path, e.key()),
                    path: path.clone(),
                    line: e.name_line,
                    text: format!(
                        "{} — same scope as changed {}, not in the diff",
                        short_sig(e),
                        c.entity.name
                    ),
                    confirmed: None,
                });
            }
            // Same name in another class/namespace: only when the name is rare.
            if other_scope.len() <= 6
                && c.entity.name.len() >= 4
                && !c.entity.name.starts_with("operator")
            {
                for (path, e) in other_scope
                    .into_iter()
                    .filter(|(_, e)| seen.insert(e.key()))
                    .take(2)
                {
                    out.push(Item {
                        facet: Facet::Overloads,
                        key: format!("samename|{}|{}", path, e.key()),
                        path: path.clone(),
                        line: e.name_line,
                        text: format!(
                            "{} — same name as changed {} in another scope",
                            short_sig(e),
                            c.entity.qualified()
                        ),
                        confirmed: None,
                    });
                }
            }
        }
        out
    }

    /// The entity did not exist before the change (new file, or no entity with its key).
    fn newly_added(&self, path: &str, entity: &Entity) -> bool {
        self.changes.iter().any(|change| {
            change.path == path
                && (change.new_file
                    || change
                        .base
                        .as_ref()
                        .is_some_and(|base| !base.entities.iter().any(|e| e.key() == entity.key())))
        })
    }

    /// Two functions of one scope belong to the same class definition (or are free functions,
    /// or their class name is defined once, so out-of-line members are unambiguous).
    fn same_class(&self, a_path: &str, a: &Entity, b_path: &str, b: &Entity) -> bool {
        let a_class = self.enclosing_class(a_path, a);
        let b_class = self.enclosing_class(b_path, b);
        match (a_class, b_class) {
            (Some(x), Some(y)) => a_path == b_path && x.key() == y.key() && x.line == y.line,
            _ => {
                let owner = a.owner().or(b.owner());
                match owner {
                    None => true,
                    Some(owner) => {
                        let definitions = self
                            .indexed()
                            .flat_map(|(_, scan)| scan.entities.iter())
                            .filter(|e| e.kind == EntityKind::Class && e.name == owner && e.is_def)
                            .count();
                        definitions <= 1
                    }
                }
            }
        }
    }

    /// The class definition (same file) whose body holds `entity`.
    fn enclosing_class<'s>(&'s self, path: &str, entity: &Entity) -> Option<&'s Entity> {
        self.index.files.get(path).and_then(|scan| {
            scan.entities
                .iter()
                .filter(|e| {
                    e.kind == EntityKind::Class
                        && e.line <= entity.line
                        && entity.end_line <= e.end_line
                        && entity.line != e.line
                })
                .min_by_key(|e| e.end_line - e.line)
        })
    }

    /// The pre-change version of a modified function (siblings resemble what it looked like
    /// before the edit, not the edit itself).
    fn base_version(&self, c: &Changed) -> Option<(Arc<FileScan>, Entity)> {
        let change = self.changes.iter().find(|ch| ch.path == c.path)?;
        let base = change.base.as_ref()?;
        let entity = base
            .entities
            .iter()
            .find(|e| {
                e.qualified() == c.entity.qualified()
                    && e.body.is_some()
                    && (e.param_types == c.entity.param_types || c.added)
            })?
            .clone();
        Some((Arc::clone(base), entity))
    }

    fn look_alikes(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for c in self.changed_functions() {
            let Some(change_scan) = self.index.files.get(&c.path) else {
                continue;
            };
            let c_body = normalised_body(change_scan, &c.entity);
            let c_base_body = self
                .base_version(c)
                .map(|(scan, entity)| normalised_body(&scan, &entity))
                .unwrap_or_default();
            let c_params: HashSet<String> = c.entity.param_types.iter().cloned().collect();
            let c_words: Vec<String> = scan::name_words(&c.entity.name);
            let c_word_set: HashSet<String> = c_words.iter().cloned().collect();
            let mut scored: Vec<(f64, String, &String, &Entity)> = Vec::new();
            for (path, file_scan, e) in self.functions() {
                if e.name == c.entity.name
                    || e.body.is_none()
                    || self.changed_keys.contains(&e.key())
                {
                    continue;
                }
                if is_macro_like(e) || self.edited(path, e) {
                    continue;
                }
                let same_file = *path == c.path;
                let words = scan::name_words(&e.name);
                let word_set: HashSet<String> = words.iter().cloned().collect();
                let name_sim = jaccard(&c_word_set, &word_set);
                // Name family: same number of words, exactly one differs (backend twins), or
                // the changed name's words appear inside this name (an implementation or
                // wrapper of the same operation: `get_glyph_name` → `x_get_glyph_name`).
                let one_word_swap = words.len() == c_words.len()
                    && words.len() >= 2
                    && words.iter().zip(&c_words).filter(|(a, b)| a != b).count() == 1;
                let stem = c_words.len() >= 2
                    && words.len() > c_words.len()
                    && words.ends_with(c_words.as_slice());
                if !same_file && !one_word_swap && !stem {
                    continue;
                }
                if !same_file && gitx::is_test_path(path) {
                    continue;
                }
                let params: HashSet<String> = e.param_types.iter().cloned().collect();
                let param_sim = jaccard(&c_params, &params);
                let body = normalised_body(file_scan, e);
                let c_refs: Vec<&str> = c_body.iter().map(String::as_str).collect();
                let c_base_refs: Vec<&str> = c_base_body.iter().map(String::as_str).collect();
                let e_refs: Vec<&str> = body.iter().map(String::as_str).collect();
                let body_sim = scan::shingle_similarity(&c_refs, &e_refs)
                    .max(scan::shingle_similarity(&c_base_refs, &e_refs));
                let name_sim = name_sim.max(affix_similarity(&c.entity.name, &e.name));
                let keep = if same_file {
                    body_sim >= 0.5
                        || (name_sim >= 0.34 && param_sim >= 0.5 && body_sim >= 0.25)
                        || (one_word_swap && param_sim >= 0.5)
                        || stem
                } else if one_word_swap {
                    e.param_types.len() == c.entity.param_types.len()
                        && ((c_words.len() >= 3 && (body_sim >= 0.2 || param_sim >= 0.6))
                            || body_sim >= 0.35)
                } else {
                    // Name stem in another file: an implementation or wrapper.
                    true
                };
                if !keep {
                    continue;
                }
                let score = 0.5 * body_sim
                    + 0.3 * param_sim
                    + 0.2 * name_sim
                    + if stem { 0.2 } else { 0.0 };
                let why = if stem {
                    format!(
                        "same operation name as changed {} (an implementation or wrapper of it)",
                        c.entity.name
                    )
                } else if same_file {
                    format!(
                        "{}% similar to changed {} (same file)",
                        (score * 100.0).round() as u32,
                        c.entity.name
                    )
                } else {
                    format!(
                        "parallel implementation of changed {} ({}% similar body)",
                        c.entity.name,
                        (body_sim * 100.0).round() as u32
                    )
                };
                scored.push((score, why, path, e));
            }
            // Parallel overload family: another name in the same scope with an overload whose
            // parameter types equal one of the changed function's other overloads. Its
            // remaining overloads are the counterparts of the changed one.
            for (path, e, shared) in self.parallel_family(c) {
                if scored
                    .iter()
                    .any(|(_, _, p, x)| *p == path && x.key() == e.key())
                {
                    continue;
                }
                scored.push((
                    0.45,
                    format!(
                        "parallel overload family: {} and {} both take ({}); this is {}'s counterpart of the changed one",
                        e.name,
                        c.entity.name,
                        truncate_chars(&shared, 50),
                        e.name
                    ),
                    path,
                    e,
                ));
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            // At most 4, spread over files.
            let mut per_file: HashMap<&String, usize> = HashMap::new();
            let picked: Vec<(f64, String, &String, &Entity)> = scored
                .into_iter()
                .filter(|(_, _, path, _)| {
                    let n = per_file.entry(*path).or_default();
                    *n += 1;
                    *n <= 2
                })
                .take(4)
                .collect();
            for (_, why, path, e) in picked {
                out.push(Item {
                    facet: Facet::LookAlikes,
                    key: format!("lookalike|{}|{}", path, e.key()),
                    path: path.clone(),
                    line: e.name_line,
                    text: format!("{} — {why}", short_sig(e)),
                    confirmed: None,
                });
            }
        }
        out
    }

    fn parallel_family(&self, c: &Changed) -> Vec<(&String, &Entity, String)> {
        let mut own: Vec<&Vec<String>> = Vec::new();
        let mut members: Vec<(&String, &Entity)> = Vec::new();
        for (path, _, e) in self.functions() {
            if e.scope != c.entity.scope || !(e.is_def || path == &c.path) {
                continue;
            }
            if e.name == c.entity.name {
                if e.param_types != c.entity.param_types && !e.param_types.is_empty() {
                    own.push(&e.param_types);
                }
            } else {
                members.push((path, e));
            }
        }
        let mut out = Vec::new();
        // A name whose overload set mirrors the changed one: a similar name sharing a parameter
        // list (a uniform API signature alone says nothing).
        let mut shared_lists: HashMap<&str, BTreeSet<&Vec<String>>> = HashMap::new();
        for (_, e) in &members {
            if let Some(params) = own.iter().find(|params| ***params == e.param_types) {
                shared_lists
                    .entry(e.name.as_str())
                    .or_default()
                    .insert(*params);
            }
        }
        let names: BTreeSet<&str> = shared_lists
            .iter()
            .filter(|(name, _)| {
                affix_similarity(name, &c.entity.name) >= 0.3
                    || jaccard(
                        &scan::name_words(name).into_iter().collect(),
                        &scan::name_words(&c.entity.name).into_iter().collect(),
                    ) >= 0.5
            })
            .map(|(name, _)| *name)
            .collect();
        for name in names {
            let shared = members
                .iter()
                .find(|(_, e)| e.name == name && own.iter().any(|params| **params == e.param_types))
                .map(|(_, e)| e.param_types.join(", "))
                .unwrap_or_default();
            for (path, e) in &members {
                if e.name == name
                    && !own.iter().any(|params| **params == e.param_types)
                    && !self.changed_keys.contains(&e.key())
                    && !self.edited(path, e)
                    && !is_macro_like(e)
                {
                    out.push((*path, *e, shared.clone()));
                }
            }
        }
        out.truncate(4);
        out
    }

    fn switch_siblings(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for change in self
            .changes
            .iter()
            .filter(|ch| !self.generated.contains(&ch.path))
        {
            let touched = diff::touched_new_lines(&change.hunks);
            for sw in &change.cur.switches {
                let inside: Vec<u32> = touched
                    .iter()
                    .copied()
                    .filter(|l| sw.line <= *l && *l <= sw.end_line)
                    .collect();
                if inside.is_empty() || sw.cases.len() < 3 {
                    continue;
                }
                let mut changed_labels = Vec::new();
                let mut others = Vec::new();
                for (label, start, end) in &sw.cases {
                    if inside.iter().any(|l| start <= l && l <= end) {
                        changed_labels.push(label.clone());
                    } else {
                        others.push(label.clone());
                    }
                }
                if others.is_empty() {
                    continue;
                }
                let shown: Vec<String> = others.iter().take(24).cloned().collect();
                let more = others.len().saturating_sub(shown.len());
                let changed_text = if changed_labels.is_empty() {
                    "code outside any label".to_string()
                } else {
                    changed_labels.join(" ")
                };
                out.push(Item {
                    facet: Facet::Switch,
                    key: format!("switch|{}|{}", change.path, sw.expr),
                    path: change.path.clone(),
                    line: sw.line,
                    text: format!(
                        "switch ({}) — you changed {}; the other labels are: {}{} — does the same rule apply to any of them (or to combinations with them)?",
                        truncate_chars(&sw.expr, 40),
                        truncate_chars(&changed_text, 60),
                        shown.join(" "),
                        if more > 0 { format!(" (+{more} more)") } else { String::new() }
                    ),
                    confirmed: None,
                });
            }
        }
        out
    }

    /// Operation names of every class: members plus free functions taking the class.
    #[allow(clippy::type_complexity)]
    fn operations(
        &self,
    ) -> (
        HashMap<String, BTreeSet<String>>,
        HashMap<String, (String, u32, String)>,
        HashMap<String, Vec<String>>,
    ) {
        let mut classes: HashMap<String, (String, u32, String)> = HashMap::new();
        let mut bases: HashMap<String, Vec<String>> = HashMap::new();
        for (path, scan) in self.indexed() {
            for e in &scan.entities {
                if e.kind == EntityKind::Class && e.spec_args.is_none() && e.is_def {
                    classes
                        .entry(e.name.clone())
                        .or_insert_with(|| (path.clone(), e.name_line, e.scope.clone()));
                    if !e.bases.is_empty() {
                        bases
                            .entry(e.name.clone())
                            .or_insert_with(|| e.bases.clone());
                    }
                }
            }
        }
        let mut ops: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (_, _, e) in self.functions() {
            if let Some(owner) = e.owner()
                && classes.contains_key(owner)
            {
                if e.name != owner && !e.name.starts_with('~') {
                    ops.entry(owner.to_string())
                        .or_default()
                        .insert(e.name.clone());
                }
                // A hidden friend or member operator taking another class also counts for it.
            }
            for param in &e.param_types {
                let base = base_type(param);
                if classes.contains_key(&base) && e.owner() != Some(base.as_str()) {
                    ops.entry(base).or_default().insert(e.name.clone());
                }
            }
        }
        (ops, classes, bases)
    }

    fn peers(&self) -> Vec<Item> {
        let added: Vec<&Changed> = self.changed_functions().filter(|c| c.added).collect();
        if added.is_empty() {
            return Vec::new();
        }
        let (ops, classes, bases) = self.operations();
        let mut out = Vec::new();
        for a in added {
            let mut subjects: Vec<String> = Vec::new();
            if let Some(owner) = a.entity.owner()
                && classes.contains_key(owner)
            {
                subjects.push(owner.to_string());
            }
            for param in &a.entity.param_types {
                let base = base_type(param);
                if classes.contains_key(&base) {
                    subjects.push(base);
                }
                for arg in type_args(param) {
                    if classes.contains_key(&arg) {
                        subjects.push(arg);
                    }
                }
            }
            subjects.sort();
            subjects.dedup();
            for subject in subjects {
                let Some(subject_ops) = ops.get(&subject) else {
                    continue;
                };
                let reference: BTreeSet<&String> = subject_ops
                    .iter()
                    .filter(|name| **name != a.entity.name)
                    .collect();
                if reference.len() < 3 {
                    continue;
                }
                let subject_scope = classes
                    .get(&subject)
                    .map(|c| c.2.clone())
                    .unwrap_or_default();
                let mut found: Vec<(usize, String)> = Vec::new();
                // Classes derived from the same base as the subject are its family, whatever
                // their operation overlap.
                let subject_bases: BTreeSet<&String> =
                    bases.get(&subject).into_iter().flatten().collect();
                if !subject_bases.is_empty() {
                    for (peer, peer_bases) in &bases {
                        if *peer == subject || !classes.contains_key(peer) {
                            continue;
                        }
                        if peer_bases.iter().any(|b| subject_bases.contains(b))
                            && !ops.get(peer).is_some_and(|p| p.contains(&a.entity.name))
                        {
                            found.push((0, peer.clone()));
                        }
                    }
                    // A base shared by many classes (a plugin interface) is not a family.
                    if found.len() > 6 {
                        found.clear();
                    }
                }
                for (peer, (_, _, scope)) in &classes {
                    if *peer == subject || *scope != subject_scope {
                        continue;
                    }
                    let Some(peer_ops) = ops.get(peer) else {
                        continue;
                    };
                    if peer_ops.contains(&a.entity.name) {
                        continue;
                    }
                    let shared: Vec<&String> = reference
                        .iter()
                        .copied()
                        .filter(|name| peer_ops.contains(*name))
                        .collect();
                    if shared.len() >= 3 && shared.len() * 2 >= reference.len() {
                        found.retain(|(_, name)| name != peer);
                        found.push((shared.len(), peer.clone()));
                    }
                }
                found.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
                for (count, peer) in found.into_iter().take(5) {
                    let (path, line, _) = classes[&peer].clone();
                    let shared: Vec<String> = reference
                        .iter()
                        .filter(|name| ops.get(&peer).is_some_and(|p| p.contains(**name)))
                        .take(4)
                        .map(|s| (*s).clone())
                        .collect();
                    let text = if count == 0 {
                        format!(
                            "{peer} derives from the same base as {subject} but has no {} — you added it for {subject}",
                            a.entity.name
                        )
                    } else {
                        format!(
                            "{peer} shares {count}/{} operations with {subject} ({}, …) but has no {} — you added it for {subject}",
                            reference.len(),
                            shared.join(", "),
                            a.entity.name
                        )
                    };
                    out.push(Item {
                        facet: Facet::Peers,
                        key: format!("peer|{}|{}|{}", peer, subject, a.entity.name),
                        path,
                        line,
                        text,
                        confirmed: None,
                    });
                }
            }
        }
        merge_peer_items(out)
    }

    fn specialisations(&self) -> Vec<Item> {
        let mut out = Vec::new();
        let mut templates: BTreeSet<(String, String)> = BTreeSet::new();
        // A changed primary class template: its specialisations do not get the change.
        let mut primaries: BTreeSet<String> = BTreeSet::new();
        for c in self.changed {
            let class = if c.entity.kind == EntityKind::Class {
                Some(c.entity.clone())
            } else {
                c.entity.owner().and_then(|owner| {
                    self.index.files.get(&c.path).and_then(|scan| {
                        scan.entities
                            .iter()
                            .find(|e| {
                                e.kind == EntityKind::Class
                                    && e.name == owner
                                    && e.line <= c.entity.line
                                    && c.entity.end_line <= e.end_line
                            })
                            .cloned()
                    })
                })
            };
            if let Some(class) = class
                && class.template.is_some()
                && class.spec_args.is_none()
            {
                primaries.insert(class.name.clone());
            }
        }
        for name in &primaries {
            let mut specs: Vec<(&String, &Entity)> = Vec::new();
            for (path, scan) in self.indexed() {
                if gitx::is_test_path(path) {
                    continue;
                }
                for e in &scan.entities {
                    if e.kind == EntityKind::Class
                        && &e.name == name
                        && e.spec_args.is_some()
                        && !self.edited(path, e)
                    {
                        specs.push((path, e));
                    }
                }
            }
            for (path, e) in specs.into_iter().take(5) {
                out.push(Item {
                    facet: Facet::Specialisations,
                    key: format!("primaryspec|{}|{}", path, e.key()),
                    path: path.clone(),
                    line: e.name_line,
                    text: format!(
                        "{name}{} — a specialisation of the class template you changed; it does not inherit the change (its members are declared separately)",
                        truncate_chars(&e.spec_args.clone().unwrap_or_default(), 80)
                    ),
                    confirmed: None,
                });
            }
        }
        // The extension pattern matters when the diff ADDS a specialisation (a fix inside an
        // existing one is not a new extension).
        for c in self.changed {
            if c.entity.kind == EntityKind::Class && c.entity.spec_args.is_some() {
                if self.newly_added(&c.path, &c.entity) {
                    templates.insert((c.entity.name.clone(), c.entity.key()));
                }
            } else if let Some(owner) = c.entity.owner()
                && let Some(scan) = self.index.files.get(&c.path)
                && let Some(class) = scan.entities.iter().find(|e| {
                    e.kind == EntityKind::Class
                        && e.name == owner
                        && e.spec_args.is_some()
                        && e.line <= c.entity.line
                        && c.entity.end_line <= e.end_line
                })
                && self.newly_added(&c.path, class)
            {
                templates.insert((class.name.clone(), class.key()));
            }
        }
        for (name, changed_key) in templates {
            let mut specs: Vec<(&String, &Entity)> = Vec::new();
            for (path, scan) in self.indexed() {
                for e in &scan.entities {
                    if e.kind == EntityKind::Class
                        && e.name == name
                        && e.spec_args.is_some()
                        && e.key() != changed_key
                        && !self.changed_keys.contains(&e.key())
                    {
                        specs.push((path, e));
                    }
                }
            }
            if specs.is_empty() {
                continue;
            }
            // Constraint traits the existing specialisations use.
            let mut traits: BTreeMap<String, usize> = BTreeMap::new();
            for (_, e) in &specs {
                let text = format!(
                    "{} {}",
                    e.spec_args.clone().unwrap_or_default(),
                    e.template.clone().unwrap_or_default()
                );
                for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                    if word.starts_with("is_")
                        || word.starts_with("has_")
                        || word.contains("_like")
                        || word.starts_with("enable_if")
                        || word.starts_with("is")
                            && word.len() > 2
                            && word.chars().nth(2).is_some_and(char::is_uppercase)
                    {
                        *traits.entry(word.to_string()).or_default() += 1;
                    }
                }
            }
            let mut trait_list: Vec<(String, usize)> = traits.into_iter().collect();
            trait_list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let trait_text: Vec<String> =
                trait_list.iter().take(8).map(|(t, _)| t.clone()).collect();
            let (first_path, first) = specs[0];
            out.push(Item {
                facet: Facet::Specialisations,
                key: format!("specs|{name}"),
                path: first_path.clone(),
                line: first.name_line,
                text: format!(
                    "{} other specialisation(s) of {name}; constraints they use: {} — extend the library the same way instead of listing concrete types",
                    specs.len(),
                    if trait_text.is_empty() { "none (explicit types)".to_string() } else { trait_text.join(", ") }
                ),
                confirmed: None,
            });
            // Examples: the project's own code first, constrained (partial) specialisations first.
            specs.sort_by_key(|(path, e)| {
                (
                    gitx::is_test_path(path),
                    e.template.as_deref() == Some("<>"),
                    e.line,
                )
            });
            for (path, e) in specs.into_iter().take(3) {
                out.push(Item {
                    facet: Facet::Specialisations,
                    key: format!("spec|{}|{}", path, e.key()),
                    path: path.clone(),
                    line: e.name_line,
                    text: format!(
                        "{}{}",
                        name,
                        truncate_chars(&e.spec_args.clone().unwrap_or_default(), 100)
                    ),
                    confirmed: None,
                });
            }
        }
        out
    }

    fn overrides(&self) -> Vec<Item> {
        // Class definitions (path, entity); names can repeat (specialisations, namespaces).
        let classes: Vec<(&String, &Entity)> = self
            .indexed()
            .flat_map(|(path, scan)| {
                scan.entities
                    .iter()
                    .filter(|e| e.kind == EntityKind::Class && e.is_def)
                    .map(move |e| (path, e))
            })
            .collect();
        let mut name_count: HashMap<&str, usize> = HashMap::new();
        for (_, e) in &classes {
            *name_count.entry(e.name.as_str()).or_default() += 1;
        }
        // Members of one particular class definition: defined inside its body, or defined out
        // of line with its (unique) name as qualifier.
        let members_of =
            |class_path: &String, class: &Entity, name: &str| -> Vec<(&String, &Entity)> {
                self.functions()
                    .filter(|(path, _, e)| {
                        e.name == name
                            && ((*path == class_path
                                && class.line <= e.line
                                && e.end_line <= class.end_line)
                                || (name_count.get(class.name.as_str()) == Some(&1)
                                    && e.owner() == Some(class.name.as_str())))
                    })
                    .map(|(path, _, e)| (path, e))
                    .collect()
            };
        let mut out = Vec::new();
        for c in self.changed_functions() {
            let Some(owner_name) = c.entity.owner() else {
                continue;
            };
            // The class definition that holds the changed member.
            let Some((_, owner)) = classes
                .iter()
                .find(|(path, e)| {
                    e.name == owner_name
                        && ((**path == c.path
                            && e.line <= c.entity.line
                            && c.entity.end_line <= e.end_line)
                            || name_count.get(owner_name) == Some(&1))
                })
                .copied()
            else {
                continue;
            };
            let mut related: Vec<(&String, &Entity, &str)> = Vec::new();
            for (path, d) in &classes {
                if d.bases.iter().any(|b| b == owner_name) {
                    related.push((path, d, "override in derived class"));
                }
            }
            for base in &owner.bases {
                for (path, b) in classes.iter().filter(|(_, b)| &b.name == base) {
                    related.push((path, b, "declaration in base class"));
                    for (spath, sibling) in &classes {
                        if sibling.bases.contains(base) && sibling.name != owner.name {
                            related.push((spath, sibling, "sibling override"));
                        }
                    }
                    let _ = path;
                }
            }
            // A method overridden by many classes (a plugin interface) makes a list of siblings
            // meaningless; keep the base declaration and at most a few derived ones.
            let siblings = related
                .iter()
                .filter(|(_, _, r)| *r == "sibling override")
                .count();
            let derived = related
                .iter()
                .filter(|(_, _, r)| *r == "override in derived class")
                .count();
            related.retain(|(_, _, r)| {
                (*r != "sibling override" || siblings <= 4)
                    && (*r != "override in derived class" || derived <= 6)
            });
            let mut count = 0;
            for (class_path, class, relation) in related {
                for (path, e) in members_of(class_path, class, &c.entity.name) {
                    // Only a virtual function is an override; a same-named non-virtual member
                    // merely hides.
                    if !(c.entity.is_virtual || e.is_virtual) {
                        continue;
                    }
                    if count >= 5 || self.changed_keys.contains(&e.key()) || self.edited(path, e) {
                        continue;
                    }
                    out.push(Item {
                        facet: Facet::Overrides,
                        key: format!("override|{}|{}", path, e.key()),
                        path: path.clone(),
                        line: e.name_line,
                        text: format!(
                            "{} — {relation} of changed {}",
                            short_sig(e),
                            c.entity.qualified()
                        ),
                        confirmed: None,
                    });
                    count += 1;
                }
            }
        }
        out
    }

    fn configs(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for c in self.changed {
            let Some(change) = self.changes.iter().find(|ch| ch.path == c.path) else {
                continue;
            };
            let scan = &change.cur;
            let branches = scan::branches_at(&scan.branches, c.entity.name_line);
            let Some(innermost) = branches.last() else {
                // Not inside a conditional: look for same-name declarations that are.
                for e in &scan.entities {
                    if e.name == c.entity.name
                        && e.key() != c.entity.key()
                        && e.scope == c.entity.scope
                        && !scan::branches_at(&scan.branches, e.name_line).is_empty()
                        && !self.changed_keys.contains(&e.key())
                        && self.same_class(&c.path, &c.entity, &c.path, e)
                    {
                        let branch = scan::branches_at(&scan.branches, e.name_line)
                            .last()
                            .map(|b| b.condition.clone())
                            .unwrap_or_default();
                        out.push(Item {
                            facet: Facet::Configs,
                            key: format!("cfgdecl|{}|{}", c.path, e.key()),
                            path: c.path.clone(),
                            line: e.name_line,
                            text: format!("{} — same name, declared under `#{}`; check it matches your change in that configuration", short_sig(e), truncate_chars(&branch, 60)),
                            confirmed: None,
                        });
                    }
                }
                continue;
            };
            let siblings: Vec<&scan::Branch> = scan
                .branches
                .iter()
                .filter(|b| b.group == innermost.group && b.index != innermost.index)
                .collect();
            let mut matched = false;
            for other in &siblings {
                for e in &scan.entities {
                    if e.name == c.entity.name
                        && other.start <= e.name_line
                        && e.name_line <= other.end
                        && !self.changed_keys.contains(&e.key())
                    {
                        matched = true;
                        out.push(Item {
                            facet: Facet::Configs,
                            key: format!("cfgtwin|{}|{}", c.path, e.key()),
                            path: c.path.clone(),
                            line: e.name_line,
                            text: format!(
                                "{} — the `#{}` twin of changed {} (under `#{}`); the other build configuration compiles this one",
                                short_sig(e),
                                truncate_chars(&other.condition, 50),
                                c.entity.name,
                                truncate_chars(&innermost.condition, 50)
                            ),
                            confirmed: None,
                        });
                    }
                }
            }
            if !matched
                && let Some(other) = siblings.first()
                && other.end > other.start
            {
                out.push(Item {
                    facet: Facet::Configs,
                    key: format!("cfgbranch|{}|{}|{}", c.path, innermost.group, c.entity.key()),
                    path: c.path.clone(),
                    line: other.start.saturating_sub(1),
                    text: format!(
                        "changed {} is inside `#{}`; the other branch `#{}` (lines {}-{}) is what the other build configuration compiles — does it need the same change?",
                        c.entity.name,
                        truncate_chars(&innermost.condition, 50),
                        truncate_chars(&other.condition, 40),
                        other.start,
                        other.end
                    ),
                    confirmed: None,
                });
            }
        }
        out
    }

    fn callers(&self) -> Vec<CallerItem> {
        let mut out = Vec::new();
        let mut done_names: HashSet<String> = HashSet::new();
        for c in self.changed_functions() {
            if c.added
                || c.entity.name.starts_with("operator")
                || c.entity.name.starts_with('~')
                || c.entity.bare
                || c.entity.name.len() < 3
            {
                continue;
            }
            // One item per name: the textual tier cannot tell overloads' call sites apart.
            if !done_names.insert(c.entity.name.clone()) {
                continue;
            }
            let mut sites: Vec<(String, u32, String)> = Vec::new();
            let mut total = 0usize;
            for (path, scan) in self.indexed() {
                let toks = &scan.toks;
                for (k, tok) in toks.iter().enumerate() {
                    if tok.kind != TokKind::Ident || tok.text != c.entity.name {
                        continue;
                    }
                    let next = toks.get(k + 1).map(|t| t.text.as_str());
                    if next != Some("(") && next != Some("<") {
                        continue;
                    }
                    // Not a declaration of the same name, not a member access of another type
                    // named the same (kept: textual tier cannot tell).
                    if scan
                        .entities
                        .iter()
                        .any(|e| e.name == c.entity.name && e.name_line == tok.line)
                    {
                        continue;
                    }
                    let prev = k.checked_sub(1).map(|p| toks[p].text.as_str());
                    if matches!(prev, Some("::") | Some(".") | Some("->"))
                        && c.entity.owner().is_none()
                    {
                        // `obj.name(` / `Other::name(` for a free function: another entity.
                        if prev != Some("::") {
                            continue;
                        }
                    }
                    total += 1;
                    let enclosing = scan
                        .entity_at(tok.line)
                        .filter(|e| e.kind == EntityKind::Function)
                        .map(|e| e.name.clone())
                        .unwrap_or_else(|| "file scope".to_string());
                    sites.push((path.clone(), tok.line, enclosing));
                }
            }
            let cap = if c.entity.owner().is_some() {
                CALLERS_CAP / 2
            } else {
                CALLERS_CAP
            };
            if total == 0 || total > cap {
                // No callers, or a name too common for a textual list to mean anything.
                continue;
            }
            // Prefer sites in non-test files and spread over files.
            sites.sort_by_key(|(path, line, _)| (gitx::is_test_path(path), path.clone(), *line));
            let mut per_file: HashMap<String, usize> = HashMap::new();
            let shown: Vec<(String, u32, String)> = sites
                .iter()
                .filter(|(path, _, _)| {
                    let n = per_file.entry(path.clone()).or_default();
                    *n += 1;
                    *n <= 2
                })
                .take(5)
                .cloned()
                .collect();
            let files = sites
                .iter()
                .map(|s| s.0.clone())
                .collect::<BTreeSet<_>>()
                .len();
            let listing: Vec<String> = shown
                .iter()
                .map(|(path, line, enclosing)| format!("{path}:{line} ({enclosing})"))
                .collect();
            let text = format!(
                "{total} call site(s) of {} in {files} file(s): {}{} — does each still get what it expects?",
                c.entity.name,
                listing.join(", "),
                if total > shown.len() { ", …" } else { "" }
            );
            let (path, line) = shown
                .first()
                .map(|(p, l, _)| (p.clone(), *l))
                .unwrap_or_else(|| (c.path.clone(), c.entity.name_line));
            out.push(CallerItem {
                item: Item {
                    facet: Facet::Callers,
                    key: format!("callers|{}", c.entity.key()),
                    path,
                    line,
                    text,
                    confirmed: None,
                },
                target: c.clone(),
                sites: shown
                    .iter()
                    .map(|(path, line, _)| (path.clone(), *line))
                    .collect(),
            });
        }
        out
    }
}

/// One item per (related type, subject) listing every operation it lacks.
fn merge_peer_items(items: Vec<Item>) -> Vec<Item> {
    let mut merged: Vec<(String, String, Item, Vec<String>)> = Vec::new();
    for item in items {
        // key: peer|<peer>|<subject>|<operation>
        let parts: Vec<String> = item.key.splitn(4, '|').map(str::to_string).collect();
        let field = |n: usize| parts.get(n).cloned().unwrap_or_default();
        let (peer, subject, op) = (field(1), field(2), field(3));
        if let Some(entry) = merged
            .iter_mut()
            .find(|(p, s, _, _)| *p == peer && *s == subject)
        {
            entry.3.push(op);
            continue;
        }
        merged.push((peer, subject, item, vec![op]));
    }
    merged
        .into_iter()
        .map(|(peer, subject, mut item, ops)| {
            if ops.len() > 1 {
                let first = &ops[0];
                item.text = item.text.replace(
                    &format!("has no {first} "),
                    &format!("has none of {} ", ops.join(", ")),
                );
                item.key = format!("peer|{peer}|{subject}|{}", ops.join(","));
            }
            item
        })
        .collect()
}

/// Shared prefix or suffix of two names relative to the longer one (`gmtime`/`localtime`).
fn affix_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let longest = a.len().max(b.len());
    if longest == 0 || a == b {
        return 0.0;
    }
    let prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let suffix = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let shared = prefix.max(suffix);
    if shared < 3 {
        0.0
    } else {
        shared as f64 / longest as f64
    }
}

/// Above this many textual call sites a caller list is not specific enough to check.
const CALLERS_CAP: usize = 25;

/// Body tokens with the function's own name folded, so recursion does not hide similarity.
fn normalised_body(scan: &FileScan, entity: &Entity) -> Vec<String> {
    scan.body_tokens(entity)
        .into_iter()
        .map(|t| {
            if t == entity.name {
                "SELF".to_string()
            } else {
                t.to_string()
            }
        })
        .collect()
}

/// Macros in the `#if` conditions around changed code, and where the project's CI and build
/// files set them (the configurations the project actually builds).
async fn config_axes(root: &Path, changed: &[Changed], index: &Index) -> Vec<Item> {
    let mut macros: BTreeSet<String> = BTreeSet::new();
    let mut standard = false;
    for c in changed {
        let Some(scan) = index.files.get(&c.path) else {
            continue;
        };
        for branch in scan::branches_at(&scan.branches, c.entity.name_line) {
            for word in branch
                .condition
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            {
                if word == "__cplusplus"
                    || word.ends_with("_CPLUSPLUS")
                    || word.contains("CPP_STD")
                    || word.contains("HAS_CXX")
                {
                    standard = true;
                } else if word.len() >= 4
                    && word
                        .chars()
                        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
                    && !word.starts_with("__")
                    && word != "defined"
                {
                    macros.insert(word.to_string());
                }
            }
        }
    }
    if macros.is_empty() && !standard {
        return Vec::new();
    }
    let mut patterns: Vec<String> = macros.iter().take(6).cloned().collect();
    if standard {
        patterns.push("CMAKE_CXX_STANDARD".to_string());
        patterns.push("-std=".to_string());
    }
    let mut args: Vec<String> = vec!["grep".into(), "-n".into(), "-I".into(), "-F".into()];
    for pattern in &patterns {
        args.push("-e".into());
        args.push(pattern.clone());
    }
    args.push("--".into());
    for spec in [
        ".github/workflows",
        ".gitlab-ci.yml",
        ".travis.yml",
        "appveyor.yml",
        ".appveyor.yml",
        "azure-pipelines.yml",
        ".circleci",
        ":(glob)**/CMakeLists.txt",
        ":(glob)**/*.cmake",
        ":(glob)**/meson_options.txt",
        ":(glob)**/meson.build",
        ":(glob)**/configure.ac",
    ] {
        args.push(spec.to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some((_, out)) = gitx::git_status_out(root, &refs).await else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut per_pattern: HashMap<String, usize> = HashMap::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(num), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(num) = num.parse::<u32>() else {
            continue;
        };
        let Some(pattern) = patterns.iter().find(|p| text.contains(p.as_str())) else {
            continue;
        };
        let n = per_pattern.entry(pattern.clone()).or_default();
        *n += 1;
        if *n > 2 {
            continue;
        }
        items.push(Item {
            facet: Facet::Configs,
            key: format!("axis|{path}|{num}"),
            path: path.to_string(),
            line: num,
            text: format!(
                "the project builds a configuration with `{}` here: `{}` — build and test your change in it too",
                pattern,
                truncate_chars(text.trim(), 90)
            ),
            confirmed: None,
        });
        if items.len() >= 4 {
            break;
        }
    }
    items
}

/// Stale generated copies: another tracked file contains a changed hunk's OLD text but not
/// its new text. Returns the items and every file found to be a copy.
async fn copies_facet(
    root: &Path,
    changes: &[FileChange],
    name_generated: &HashSet<String>,
) -> (Vec<Item>, BTreeSet<String>) {
    let mut items = Vec::new();
    let mut copies = BTreeSet::new();
    let mut file_cache: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    // Sources before copies: files that do not look generated, smaller files first (a
    // combined copy is larger than each of its parts). A file found to be a copy is not
    // used as a source afterwards, so an edit made in both places is not reported twice.
    let mut order: Vec<&FileChange> = changes.iter().collect();
    order.sort_by_key(|change| {
        let size = std::fs::metadata(root.join(&change.path))
            .map(|m| m.len())
            .unwrap_or(0);
        (name_generated.contains(&change.path), size)
    });
    for change in order {
        if change.new_file || gitx::is_test_path(&change.path) || copies.contains(&change.path) {
            continue;
        }
        let mut reported: HashSet<String> = HashSet::new();
        for hunk in change.ctx_hunks.iter().take(20) {
            let pre = diff::normalise_block(&hunk.old_lines);
            let post = diff::normalise_block(&hunk.new_lines);
            if pre == post || pre.len() < 3 || pre.iter().map(String::len).sum::<usize>() < 60 {
                continue;
            }
            // Most distinctive line of the old text (as written, only trimmed) to find
            // candidate files.
            let Some(probe) = hunk
                .old_lines
                .iter()
                .map(|line| line.trim())
                .filter(|line| line.len() >= 12)
                .max_by_key(|line| line.len())
            else {
                continue;
            };
            let Some((_, out)) =
                gitx::git_status_out(root, &["grep", "-l", "-I", "-F", "-e", probe, "--", "."])
                    .await
            else {
                continue;
            };
            for candidate in out.lines().filter(|c| *c != change.path).take(6) {
                if !file_cache.contains_key(candidate) {
                    let text = std::fs::read_to_string(root.join(candidate)).unwrap_or_default();
                    file_cache.insert(candidate.to_string(), diff::normalised_with_lines(&text));
                }
                let lines = &file_cache[candidate];
                let hay: Vec<String> = lines.iter().map(|(_, t)| t.clone()).collect();
                let has_pre = diff::contains_run(&hay, &pre);
                let has_post = post.len() >= 2 && diff::contains_run(&hay, &post);
                if has_pre || has_post {
                    copies.insert(candidate.to_string());
                }
                if has_pre && !has_post && reported.insert(candidate.to_string()) {
                    let line = diff::find_run_line(lines, &pre).unwrap_or(1);
                    let generator = generator_hint(root, candidate).await;
                    items.push(Item {
                        facet: Facet::Copies,
                        key: format!("copy|{}|{}", candidate, change.path),
                        path: candidate.to_string(),
                        line,
                        text: format!(
                            "contains the OLD text of your change to {}:{} and not the new one — a generated (combined) copy that is now stale{}",
                            change.path,
                            hunk.new_start,
                            generator
                                .map(|g| format!("; regenerate it ({g}) or apply the same edit"))
                                .unwrap_or_else(|| "; regenerate it with the project's script or apply the same edit".to_string())
                        ),
                        confirmed: None,
                    });
                }
            }
        }
    }
    (items, copies)
}

/// A build-file or script line that mentions the copy's file name (how it is regenerated).
async fn generator_hint(root: &Path, copy: &str) -> Option<String> {
    let name = Path::new(copy).file_name()?.to_str()?.to_string();
    let (_, out) = gitx::git_status_out(
        root,
        &[
            "grep",
            "-n",
            "-I",
            "-F",
            "-e",
            &name,
            "--",
            ":(glob)**/CMakeLists.txt",
            ":(glob)**/*.cmake",
            ":(glob)**/Makefile*",
            ":(glob)**/*.mk",
            ":(glob)**/*.py",
            ":(glob)**/*.sh",
            ":(glob)**/meson.build",
        ],
    )
    .await?;
    let first = out
        .lines()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("amalgam")
                || lower.contains("single")
                || lower.contains("generat")
                || lower.contains(".py")
        })
        .or_else(|| out.lines().next())?;
    let mut parts = first.splitn(3, ':');
    let (path, num, text) = (parts.next()?, parts.next()?, parts.next()?);
    Some(format!(
        "{path}:{num}: `{}`",
        truncate_chars(text.trim(), 70)
    ))
}

/// A name mentioned more often than this (tests plus docs) is too common to point at a change.
const MENTION_CAP: usize = 40;

/// Prose mentions of ordinary words are not API docs: require the name written as code
/// (`name(`, `` `name ``, `::name`).
fn code_mention(text: &str, name: &str) -> bool {
    text.contains(&format!("{name}("))
        || text.contains(&format!("`{name}"))
        || text.contains(&format!("::{name}"))
        || text.contains(&format!(".{name}"))
}

fn word_in(text: &str, word: &str) -> bool {
    find_word(text, word).is_some()
}

/// Tests and docs that mention the changed names (git grep over tracked files).
async fn mentions_facet(root: &Path, changed: &[Changed], index: &Index) -> Vec<Item> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    // Member functions count in a test file only when their class is named there too.
    let mut owners: HashMap<String, BTreeSet<String>> = HashMap::new();
    for c in changed {
        let name = &c.entity.name;
        if name.len() >= 3
            && !name.starts_with("operator")
            && !name.starts_with('~')
            && !is_macro_like(&c.entity)
        {
            names.insert(name.clone());
            if c.entity.kind == EntityKind::Function
                && let Some(owner) = c.entity.owner()
                && index.files.values().any(|scan| {
                    scan.entities
                        .iter()
                        .any(|e| e.kind == EntityKind::Class && e.name == owner)
                })
            {
                owners
                    .entry(name.clone())
                    .or_default()
                    .insert(owner.to_string());
            } else {
                owners
                    .entry(name.clone())
                    .or_default()
                    .insert(String::new());
            }
        }
    }
    let mut file_text: HashMap<String, String> = HashMap::new();
    if names.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "grep".into(),
        "-n".into(),
        "-I".into(),
        "-w".into(),
        "-F".into(),
    ];
    for name in names.iter().take(8) {
        args.push("-e".into());
        args.push(name.clone());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some((_, out)) = gitx::git_status_out(root, &refs).await else {
        return Vec::new();
    };
    // Names mentioned in too many places say nothing about this change: count per name and
    // drop the common ones.
    let mut per_name: HashMap<&str, usize> = HashMap::new();
    let mut parsed: Vec<(&str, u32, &str)> = Vec::new();
    for line in out.lines().take(20_000) {
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(num), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(num) = num.parse::<u32>() else {
            continue;
        };
        if gitx::is_vendored_path(path) {
            continue;
        }
        if !(gitx::is_test_path(path) || gitx::is_doc_path(path)) {
            continue;
        }
        for name in &names {
            if word_in(text, name) {
                *per_name.entry(name.as_str()).or_default() += 1;
            }
        }
        parsed.push((path, num, text));
    }
    let common: HashSet<&str> = per_name
        .iter()
        .filter(|(_, count)| **count > MENTION_CAP)
        .map(|(name, _)| *name)
        .collect();
    let mut tests: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
    let mut docs: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (path, num, text) in parsed {
        if !names
            .iter()
            .any(|name| !common.contains(name.as_str()) && word_in(text, name))
        {
            continue;
        }
        if gitx::is_test_path(path) {
            let text_of = file_text
                .entry(path.to_string())
                .or_insert_with(|| std::fs::read_to_string(root.join(path)).unwrap_or_default());
            let relevant = names.iter().any(|name| {
                !common.contains(name.as_str())
                    && word_in(text, name)
                    && owners.get(name).is_some_and(|set| {
                        set.iter()
                            .any(|owner| owner.is_empty() || word_in(text_of, owner))
                    })
            });
            if !relevant {
                continue;
            }
            let case = index
                .files
                .get(path)
                .and_then(|scan| {
                    scan.entity_at(num).map(|e| {
                        if e.bare {
                            format!("{}({})", e.name, truncate_chars(&e.params, 50))
                        } else {
                            e.name.clone()
                        }
                    })
                })
                .unwrap_or_default();
            tests.entry(path.to_string()).or_default().push((num, case));
        } else if gitx::is_doc_path(path)
            && !is_history_file(path)
            && names
                .iter()
                .any(|name| !common.contains(name.as_str()) && code_mention(text, name))
        {
            docs.entry(path.to_string()).or_default().push(num);
        }
    }
    let mut items = Vec::new();
    let mut test_hits: Vec<(String, Vec<(u32, String)>)> = tests.into_iter().collect();
    test_hits.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (path, mut hits) in test_hits.into_iter().take(4) {
        hits.dedup_by(|a, b| a.1 == b.1 && !a.1.is_empty());
        let count = hits.len();
        let shown: Vec<String> = hits
            .iter()
            .take(4)
            .map(|(line, case)| {
                if case.is_empty() {
                    format!("L{line}")
                } else {
                    format!("{case} L{line}")
                }
            })
            .collect();
        items.push(Item {
            facet: Facet::Tests,
            key: format!("tests|{path}"),
            path: path.clone(),
            line: hits.first().map(|h| h.0).unwrap_or(1),
            text: format!(
                "{}{} — existing tests of the changed code: run them; if your change alters what they assert, check the other cases in the same group",
                shown.join(", "),
                if count > shown.len() { format!(" (+{} more)", count - shown.len()) } else { String::new() }
            ),
            confirmed: None,
        });
    }
    for (path, lines) in docs.into_iter().take(3) {
        items.push(Item {
            facet: Facet::Docs,
            key: format!("docs|{path}"),
            path: path.clone(),
            line: lines[0],
            text: format!(
                "documents the changed symbol(s) (lines {}) — update it if the behaviour or signature changed",
                lines.iter().take(4).map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            confirmed: None,
        });
    }
    items
}

/// Changelogs mention names historically; they are not documentation to keep in sync.
fn is_history_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    ["changelog", "changes", "news", "history", "release"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Confirm caller sites with clangd `textDocument/definition`. Returns the engine label when
/// clangd ran.
async fn confirm_callers(
    root: &Path,
    callers: &mut [CallerItem],
    budget: Duration,
) -> Option<String> {
    let db = crate::tools::find_compile_db(root)?;
    let deadline = Instant::now() + budget.min(Duration::from_secs(45));
    let mut session =
        crate::lsp::LspSession::start_clangd(root, db.parent(), Duration::from_secs(20))
            .await
            .ok()?;
    let label = format!(
        "clangd{} ({})",
        session.major.map(|m| format!(" {m}")).unwrap_or_default(),
        db.strip_prefix(root).unwrap_or(&db).display()
    );
    'outer: for caller in callers.iter_mut() {
        let mut confirmed = 0usize;
        let mut tried = 0usize;
        for (path, line) in caller.sites.iter().take(3) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break 'outer;
            }
            let full = root.join(path);
            let Ok(text) = std::fs::read_to_string(&full) else {
                continue;
            };
            let Some(line_text) = text.lines().nth(line.saturating_sub(1) as usize) else {
                continue;
            };
            let Some(col) = find_word(line_text, &caller.target.entity.name) else {
                continue;
            };
            tried += 1;
            let Ok(locations) = session
                .at_position(
                    "textDocument/definition",
                    &full,
                    *line,
                    col as u32 + 1,
                    remaining,
                )
                .await
            else {
                continue;
            };
            let target_file = root.join(&caller.target.path);
            let target_file = std::fs::canonicalize(&target_file).unwrap_or(target_file);
            if locations.iter().any(|loc| {
                let loc_path = std::fs::canonicalize(&loc.path).unwrap_or(loc.path.clone());
                (loc_path == target_file
                    && caller.target.entity.line <= loc.line
                    && loc.line <= caller.target.entity.end_line)
                    || locations_match_decl(&loc_path, loc.line, &caller.target.entity.name)
            }) {
                confirmed += 1;
            }
        }
        if tried > 0 {
            caller.item.confirmed = Some(confirmed > 0);
            caller.item.text.push_str(&format!(
                " [clangd: {confirmed}/{tried} sampled sites resolve to it]"
            ));
        }
    }
    session.shutdown().await;
    Some(label)
}

/// The definition clangd returned is a declaration of the same name (header vs source).
fn locations_match_decl(path: &Path, line: u32, name: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .nth(line.saturating_sub(1) as usize)
                .map(|l| l.contains(name))
        })
        .unwrap_or(false)
}

fn find_word(line: &str, word: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let at = start + pos;
        let before = at.checked_sub(1).map(|p| bytes[p]);
        let after = bytes.get(at + word.len()).copied();
        let is_word = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !is_word(before) && !is_word(after) {
            return Some(at);
        }
        start = at + word.len();
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// Stable numbering of items across the first run and the finish-gate re-run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Numbering {
    pub by_key: BTreeMap<String, usize>,
    pub next: usize,
}

impl Numbering {
    pub fn number(&mut self, key: &str) -> usize {
        if let Some(n) = self.by_key.get(key) {
            return *n;
        }
        self.next += 1;
        self.by_key.insert(key.to_string(), self.next);
        self.next
    }
}

pub(crate) const DISPOSITION_RULE: &str = "Listing is not an instruction: change an item only when the request's behaviour needs it. Answer EVERY item in a message, one line each: `[n] edited` or `[n] n/a: <reason>`.";

/// Render the checklist. `only` restricts the rendered items to these keys (finish-gate delta).
pub(crate) fn render(
    report: &ImpactReport,
    numbering: &mut Numbering,
    title: &str,
    only: Option<&HashSet<String>>,
) -> String {
    let mut out = String::new();
    let changed_names: Vec<String> = report
        .changed
        .iter()
        .take(6)
        .map(|c| {
            format!(
                "{} {}:{}{}",
                truncate_chars(&c.entity.qualified(), 60),
                c.path,
                c.entity.name_line,
                if c.added { " (added)" } else { "" }
            )
        })
        .collect();
    out.push_str(&format!(
        "{title} — diff vs {}: {} C/C++ file(s), {} entit{} changed [engine: {}]\n",
        if report.base_short.is_empty() {
            "HEAD"
        } else {
            &report.base_short
        },
        report.files_changed,
        report.changed.len(),
        if report.changed.len() == 1 {
            "y"
        } else {
            "ies"
        },
        report.engine
    ));
    out.push_str(DISPOSITION_RULE);
    out.push('\n');
    if !changed_names.is_empty() {
        out.push_str(&format!(
            "CHANGED: {}{}\n",
            changed_names.join("; "),
            if report.changed.len() > changed_names.len() {
                "; …"
            } else {
                ""
            }
        ));
    }
    if !report.removed.is_empty() {
        out.push_str(&format!(
            "REMOVED: {}\n",
            report
                .removed
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut last_facet = None;
    let mut shown = 0usize;
    for item in &report.items {
        if only.is_some_and(|keys| !keys.contains(&item.key)) {
            continue;
        }
        let n = numbering.number(&item.key);
        let mut entry = String::new();
        if last_facet != Some(item.facet) {
            entry.push_str(item.facet.heading());
            entry.push('\n');
        }
        let mark = match item.confirmed {
            Some(true) => "",
            Some(false) => " (?)",
            None => "",
        };
        entry.push_str(&format!(
            " [{n}] {}:{} {}{mark}\n",
            item.path, item.line, item.text
        ));
        if out.len() + entry.len() > RENDER_CAP_BYTES {
            let rest = report.items.len() - shown;
            out.push_str(&format!(
                "({rest} more item(s) cut to stay within the size limit)\n"
            ));
            break;
        }
        out.push_str(&entry);
        last_facet = Some(item.facet);
        shown += 1;
    }
    if shown == 0 {
        out.push_str(
            "No related sites found by the scan (the checklist is empty; nothing to answer).\n",
        );
    }
    for note in &report.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    out
}

#[cfg(test)]
mod replay;
#[cfg(test)]
pub(crate) mod tests;
