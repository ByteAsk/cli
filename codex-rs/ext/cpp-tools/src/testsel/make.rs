//! The Make build model (plain Makefiles and configured autotools trees). Make is never run
//! in the project's own tree for this: `make -n` still remakes out-of-date makefiles (an
//! autotools tree re-runs `config.status`, automake or autoconf), executes `$(MAKE)` recipe
//! lines and `$(shell …)`, and `-B` makes every makefile out of date, so a dry run in the
//! user's tree can rewrite a configured `config.mk` or `Makefile`.
//!
//! - The suite target (`check`, `test`, `tests`) is read from the makefile text
//!   ([`static_targets`]), without running anything.
//! - The program map comes from dry runs in a scratch COPY of the tree ([`ScratchCopy`]): every
//!   file gets the same timestamp, so nothing is out of date and no makefile is remade there
//!   either; absolute paths to the tree inside generated makefiles and dependency files are
//!   pointed at the copy. Then:
//!   - `make -n -W <changed file> … <check>` prints what a change would rebuild, using the
//!     project's own dependency tracking (headers included);
//!   - with the sources' timestamps moved forward, `make -n <check>` prints every compile and
//!     link command: programs, their objects and sources, archives, and which programs the
//!     suite runs.
//!   The copy is deleted afterwards. A tree too large to copy (or a build directory outside the
//!   repository) gets no program map: every test counts as affected.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use super::DirKind;
use super::Model;
use super::System;
use super::Target;
use super::TestEntry;
use super::graph::canonical;
use super::normalize;
use crate::run::RunOutcome;
use crate::run::run_capped;
use crate::run::run_capped_env;

const DRY_RUN_TIMEOUT: Duration = Duration::from_secs(120);
const COPY_TIMEOUT: Duration = Duration::from_secs(300);
const CHECK_TARGETS: &[&str] = &["check", "test", "tests"];
const MAKEFILE_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];
/// Largest tree (without `.git`) copied for dry runs.
const MAX_COPY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_COPY_FILES: usize = 300_000;
/// Largest generated file whose absolute paths are rewritten in the copy.
const MAX_REWRITE_BYTES: u64 = 16 * 1024 * 1024;

/// The directory make runs in: the root when it has a Makefile, else a
/// configured out-of-tree build directory (autotools VPATH) among its children.
pub(crate) fn make_dir(root: &Path) -> Option<PathBuf> {
    let has_makefile = |d: &Path| MAKEFILE_NAMES.iter().any(|m| d.join(m).is_file());
    if has_makefile(root) {
        return Some(root.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("config.status").is_file() && has_makefile(p))
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

// ---------------------------------------------------------------------------
// Targets from the makefile text (nothing runs)
// ---------------------------------------------------------------------------

/// Rule targets (and `.PHONY` names) the makefile in `dir` defines, including literal
/// `include`d files. Targets spelled with variables or patterns are left out.
pub(crate) fn static_targets(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(first) = MAKEFILE_NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file()) else {
        return out;
    };
    let mut queue = vec![first];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(file) = queue.pop() {
        if seen.len() >= 64 || !seen.insert(file.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut in_define = false;
        for line in logical_lines(&text) {
            if line.starts_with('\t') {
                continue; // a recipe line
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let first_word = trimmed.split_whitespace().next().unwrap_or("");
            if in_define {
                in_define = first_word != "endef";
                continue;
            }
            if first_word == "define" || trimmed.starts_with("define ") {
                in_define = true;
                continue;
            }
            if matches!(first_word, "include" | "-include" | "sinclude") {
                for word in trimmed.split_whitespace().skip(1) {
                    if !word.contains('$') {
                        let path = dir.join(word);
                        if path.is_file() {
                            queue.push(path);
                        }
                    }
                }
                continue;
            }
            if let Some(names) = rule_targets(trimmed) {
                out.extend(names);
            }
        }
    }
    out
}

/// The targets of a rule line (`a b: deps`, `a:: deps`, `.PHONY: a b`), `None` for anything
/// else (assignments, directives).
fn rule_targets(line: &str) -> Option<Vec<String>> {
    let colon = line.find(':')?;
    let (before, after) = line.split_at(colon);
    if before.contains('=') || after.starts_with(":=") || after.starts_with("::=") {
        return None;
    }
    let first = before.split_whitespace().next()?;
    if matches!(
        first,
        "ifeq" | "ifneq" | "ifdef" | "ifndef" | "else" | "endif" | "export" | "unexport" | "override" | "vpath"
    ) {
        return None;
    }
    let plain = |w: &&str| !w.contains('$') && !w.contains('%');
    if before.trim() == ".PHONY" {
        let rest = after.trim_start_matches(':');
        let rest = rest.split(';').next().unwrap_or(rest);
        return Some(rest.split_whitespace().filter(plain).map(str::to_string).collect());
    }
    Some(before.split_whitespace().filter(plain).map(str::to_string).collect())
}

/// Lines with backslash continuations joined.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
            continue;
        }
        current.push_str(raw);
        out.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The target that runs the whole suite, from the makefile text.
pub(crate) fn check_target_static(dir: &Path) -> Option<String> {
    let targets = static_targets(dir);
    CHECK_TARGETS
        .iter()
        .find(|t| targets.contains(**t))
        .map(|t| (*t).to_string())
}

/// The suite target: from the makefile text, else (rules generated by `$(eval …)` or spelled
/// with variables) by a `make -n <target>` dry run in a scratch copy of the tree — never in the
/// tree itself.
pub(crate) async fn check_target(root: &Path, dir: &Path) -> Option<String> {
    if let Some(target) = check_target_static(dir) {
        return Some(target);
    }
    let copy = ScratchCopy::create(root, dir).await.ok()?;
    copy.probe_check_target().await
}

// ---------------------------------------------------------------------------
// Dry runs in a scratch copy
// ---------------------------------------------------------------------------

/// A disposable copy of the tree for dry runs (deleted on drop).
pub(crate) struct ScratchCopy {
    /// The repository root the copy mirrors.
    root: PathBuf,
    /// The copy of `root`.
    copy: PathBuf,
    /// The copy's directory make runs in.
    make_dir: PathBuf,
    /// The common timestamp of every file in the copy.
    stamp: SystemTime,
}

impl Drop for ScratchCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.copy);
    }
}

impl ScratchCopy {
    /// Copy `root` (without `.git`) to a fresh scratch directory and prepare it for dry runs in
    /// `make_dir` (inside `root`).
    pub(crate) async fn create(root: &Path, make_dir: &Path) -> Result<Self, String> {
        let rel = make_dir
            .strip_prefix(root)
            .map_err(|_| format!("the build directory {} is outside the repository", make_dir.display()))?
            .to_path_buf();
        let (bytes, files) = tree_size(root);
        if bytes > MAX_COPY_BYTES || files > MAX_COPY_FILES {
            return Err(format!(
                "the tree is too large to copy for a dry run ({} MB, {files} files)",
                bytes / (1024 * 1024)
            ));
        }
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let copy = std::env::temp_dir().join(format!("byteask-make-copy-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&copy).map_err(|e| format!("cannot create {}: {e}", copy.display()))?;
        let stamp = SystemTime::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let made = Self {
            root: root.to_path_buf(),
            make_dir: copy.join(&rel),
            copy,
            stamp,
        };
        let script = format!(
            "tar -C {} --exclude=./.git -cf - . | tar -C {} -xf -",
            crate::gate::build::shell_quote(&root.display().to_string()),
            crate::gate::build::shell_quote(&made.copy.display().to_string())
        );
        match run_capped("sh", &["-c".to_string(), script], root, COPY_TIMEOUT).await {
            Ok(RunOutcome::Finished { success: true, .. }) => {}
            _ => return Err("copying the tree for a dry run failed".to_string()),
        }
        made.rewrite_absolute_paths();
        made.set_all_times(made.stamp);
        Ok(made)
    }

    /// Point absolute references to the tree in generated makefiles and dependency files at
    /// the copy, so nothing a dry run reads or runs reaches back into the tree.
    fn rewrite_absolute_paths(&self) {
        rewrite_absolute_paths(&self.copy, &self.root);
    }

    fn set_all_times(&self, stamp: SystemTime) {
        for entry in walkdir::WalkDir::new(&self.copy).follow_links(false).into_iter().flatten() {
            if !entry.file_type().is_symlink() {
                set_time(entry.path(), stamp);
            }
        }
    }

    /// Run `make -n <args>` in the copy (no inherited make flags, C locale).
    async fn dry_run(&self, args: &[String]) -> Option<String> {
        let mut full = vec!["-C".to_string(), self.make_dir.display().to_string(), "-n".to_string()];
        full.extend(args.iter().cloned());
        let env = vec![
            ("MAKEFLAGS".to_string(), String::new()),
            ("MFLAGS".to_string(), String::new()),
            ("LC_ALL".to_string(), "C".to_string()),
        ];
        match run_capped_env("make", &full, &self.make_dir, DRY_RUN_TIMEOUT, &env).await {
            Ok(RunOutcome::Finished { stdout, stderr, .. }) => Some(format!("{stdout}\n{stderr}")),
            _ => None,
        }
    }

    /// The first suite target a dry run in the copy accepts.
    pub(crate) async fn probe_check_target(&self) -> Option<String> {
        for target in CHECK_TARGETS {
            if self.dry_run_status(&[(*target).to_string()]).await == Some(true) {
                return Some((*target).to_string());
            }
        }
        None
    }

    async fn dry_run_status(&self, args: &[String]) -> Option<bool> {
        let mut full = vec!["-C".to_string(), self.make_dir.display().to_string(), "-n".to_string()];
        full.extend(args.iter().cloned());
        let env = vec![
            ("MAKEFLAGS".to_string(), String::new()),
            ("MFLAGS".to_string(), String::new()),
            ("LC_ALL".to_string(), "C".to_string()),
        ];
        match run_capped_env("make", &full, &self.make_dir, DRY_RUN_TIMEOUT, &env).await {
            Ok(RunOutcome::Finished { success, .. }) => Some(success),
            _ => None,
        }
    }

    /// What a change to `changed` (absolute paths in the tree) would rebuild.
    pub(crate) async fn rebuilds(&self, changed: &[PathBuf], goal: &str) -> Option<String> {
        let mut args = Vec::new();
        for file in changed {
            // make matches -W names as written in the Makefile (relative to its directory).
            let rel = file.strip_prefix(&self.root).unwrap_or(file);
            let in_copy = self.copy.join(rel);
            args.push("-W".to_string());
            args.push(in_copy.strip_prefix(&self.make_dir).unwrap_or(&in_copy).display().to_string());
        }
        args.push(goal.to_string());
        self.dry_run(&args).await
    }

    /// Every compile and link command of `goal`: the sources are made newer than everything
    /// else in the copy first.
    pub(crate) async fn all_commands(&self, goal: &str) -> Option<String> {
        let newer = self.stamp + Duration::from_secs(60);
        for entry in walkdir::WalkDir::new(&self.copy).follow_links(false).into_iter().flatten() {
            if entry.file_type().is_file() && super::is_source(entry.path()) {
                set_time(entry.path(), newer);
            }
        }
        self.dry_run(&[goal.to_string()]).await
    }

    /// A path in the copy as the same path in the tree.
    pub(crate) fn to_tree(&self, path: &Path) -> PathBuf {
        if let Ok(rel) = path.strip_prefix(&self.copy) {
            return self.root.join(rel);
        }
        let canon = canonical(&self.copy);
        match path.strip_prefix(&canon) {
            Ok(rel) => self.root.join(rel),
            Err(_) => path.to_path_buf(),
        }
    }
}

/// In a copy of `root` at `copy`, point absolute references to `root` inside generated build
/// files (makefiles, `*.mk`, dependency files, `config.status`, libtool) at the copy, keeping
/// each rewritten file's timestamps. Running make in the copy then never reads or writes the
/// original tree through a configured absolute path.
pub(crate) fn rewrite_absolute_paths(copy: &Path, root: &Path) {
    let mut prefixes: Vec<String> = vec![root.display().to_string()];
    let canon = canonical(root).display().to_string();
    if !prefixes.contains(&canon) {
        prefixes.push(canon);
    }
    let target = copy.display().to_string();
    for entry in walkdir::WalkDir::new(copy).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() || !is_generated_build_file(entry.path()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() > MAX_REWRITE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read(entry.path()) else {
            continue;
        };
        let mut changed = text.clone();
        for prefix in &prefixes {
            changed = replace_path_prefix(&changed, prefix.as_bytes(), target.as_bytes());
        }
        if changed != text && std::fs::write(entry.path(), changed).is_ok()
            && let Ok(modified) = meta.modified()
        {
            set_time(entry.path(), modified);
        }
    }
}

/// Bytes and files under `root`, `.git` excluded.
fn tree_size(root: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git");
    for entry in walker.flatten() {
        if entry.file_type().is_file() {
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if files > MAX_COPY_FILES || bytes > MAX_COPY_BYTES {
                break;
            }
        }
    }
    (bytes, files)
}

/// Generated build files that carry absolute paths: makefiles, `*.mk`, dependency files,
/// `config.status`, libtool scripts.
fn is_generated_build_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    MAKEFILE_NAMES.contains(&name.as_str())
        || name == "config.status"
        || name == "libtool"
        || [".mk", ".make", ".d", ".Po", ".Plo", ".Tpo", ".dep"]
            .iter()
            .any(|ext| name.ends_with(ext))
}

/// Replace `from` with `to` where it is a whole path prefix (followed by `/`, a quote,
/// whitespace, `:` or the end).
pub(crate) fn replace_path_prefix(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(from) {
            let next = text.get(i + from.len()).copied();
            let boundary = match next {
                None => true,
                Some(c) => !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.'),
            };
            if boundary {
                out.extend_from_slice(to);
                i += from.len();
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

fn set_time(path: &Path, stamp: SystemTime) {
    let times = std::fs::FileTimes::new().set_accessed(stamp).set_modified(stamp);
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.set_times(times);
    }
}

type MapResult = (Programs, Archives, BTreeSet<PathBuf>);

fn remap_parsed(copy: &ScratchCopy, (programs, archives, invoked): MapResult) -> MapResult {
    let map = |p: &PathBuf| copy.to_tree(p);
    let sources = |set: &BTreeSet<PathBuf>| -> BTreeSet<PathBuf> {
        set.iter().map(|p| canonical(&copy.to_tree(p))).collect()
    };
    let programs = programs
        .iter()
        .map(|(k, (srcs, libs))| (map(k), (sources(srcs), libs.iter().map(map).collect())))
        .collect();
    let archives = archives.iter().map(|(k, srcs)| (map(k), sources(srcs))).collect();
    let invoked = invoked.iter().map(map).collect();
    (programs, archives, invoked)
}

/// The model of the Make tree at `dir`; `changed` are absolute paths.
pub(crate) async fn model(root: &Path, dir: &Path, changed: &[PathBuf]) -> Result<Model, String> {
    let mut check_target = check_target_static(dir);
    let mut notes = Vec::new();
    let mut rebuilds = None;
    let copy = ScratchCopy::create(root, dir).await;
    if check_target.is_none()
        && let Ok(copy) = &copy
    {
        check_target = copy.probe_check_target().await;
    }
    let goal = check_target.clone().unwrap_or_else(|| "all".to_string());
    let (programs, archives, invoked, mapping_note) = match copy {
        Ok(copy) => {
            if !changed.is_empty() {
                rebuilds = copy.rebuilds(changed, &goal).await.map(|text| {
                    let (progs, archs, _) = remap_parsed(&copy, parse_commands(&text, &copy.make_dir));
                    progs.into_keys().chain(archs.into_keys()).collect::<Vec<_>>()
                });
            }
            match copy.all_commands(&goal).await {
                Some(text) => {
                    let (programs, archives, invoked) =
                        remap_parsed(&copy, parse_commands(&text, &copy.make_dir));
                    (
                        programs,
                        archives,
                        invoked,
                        format!(
                            "programs and sources from `make -n {goal}`, affected ones from `make -n -W <changed> {goal}`, both in a scratch copy of the tree (make never runs in the tree itself)"
                        ),
                    )
                }
                None => {
                    notes.push(format!("`make -n {goal}` did not finish in the scratch copy; no program map"));
                    (BTreeMap::new(), BTreeMap::new(), BTreeSet::new(), "no program map".to_string())
                }
            }
        }
        Err(reason) => {
            notes.push(format!("no program map: {reason}; every test counts as affected"));
            (BTreeMap::new(), BTreeMap::new(), BTreeSet::new(), "no program map".to_string())
        }
    };

    let mut targets = Vec::new();
    for (archive, sources) in &archives {
        targets.push(Target {
            name: rel_name(dir, archive),
            is_exe: false,
            artifacts: vec![archive.clone()],
            sources: sources.iter().cloned().collect(),
            deps: Vec::new(),
        });
    }
    let mut tests = Vec::new();
    for (program, (sources, libs)) in &programs {
        let name = rel_name(dir, program);
        let deps: Vec<String> = libs
            .iter()
            .filter(|l| archives.contains_key(*l))
            .map(|l| rel_name(dir, l))
            .collect();
        targets.push(Target {
            name: name.clone(),
            is_exe: true,
            artifacts: vec![program.clone()],
            sources: sources.iter().cloned().collect(),
            deps,
        });
        if invoked.contains(program) || looks_like_test(program) {
            tests.push(TestEntry {
                name: program
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone()),
                command: vec![program.display().to_string()],
                workdir: program.parent().map(Path::to_path_buf),
                env: Vec::new(),
                target: Some(name),
                disabled: false,
            });
        }
    }
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    if check_target.is_none() {
        notes.push("no `check`/`test` target: the suite cannot be run as a whole".to_string());
    }
    let tracked_makefile = !dir.join("config.status").is_file();
    Ok(Model {
        system: System::Make,
        root: root.to_path_buf(),
        build_dir: dir.to_path_buf(),
        dir_kind: if canonical(dir) == canonical(root) { DirKind::InTree } else { DirKind::Existing },
        version: None,
        ninja: false,
        targets,
        tests,
        check_target,
        make_rebuilds: rebuilds,
        mapping_note,
        notes,
        config: format!("make|{}|{}", if tracked_makefile { "plain" } else { "configured" }, std::env::var("CC").unwrap_or_default()),
    })
}

fn rel_name(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn looks_like_test(program: &Path) -> bool {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.contains("test") || name.contains("check") || name.ends_with("_spec")
}

type Programs = BTreeMap<PathBuf, (BTreeSet<PathBuf>, BTreeSet<PathBuf>)>;
type Archives = BTreeMap<PathBuf, BTreeSet<PathBuf>>;

/// Parse a `make -n` transcript: programs (with sources and linked archives),
/// archives (with sources), and programs the commands run.
pub(crate) fn parse_commands(text: &str, dir: &Path) -> (Programs, Archives, BTreeSet<PathBuf>) {
    let mut cwd_stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    let mut objects: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut programs: Programs = BTreeMap::new();
    let mut archives: Archives = BTreeMap::new();
    let mut commands: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.split_once("Entering directory").map(|(_, r)| r) {
            let path = rest.trim().trim_matches(|c| c == '\'' || c == '`' || c == '"');
            cwd_stack.push(PathBuf::from(path));
            continue;
        }
        if line.contains("Leaving directory") {
            if cwd_stack.len() > 1 {
                cwd_stack.pop();
            }
            continue;
        }
        let cwd = cwd_stack.last().cloned().unwrap_or_else(|| dir.to_path_buf());
        for part in line.split("&&").flat_map(|p| p.split(';')) {
            let tokens = shlex::split(part.trim()).unwrap_or_else(|| part.split_whitespace().map(str::to_string).collect());
            if tokens.is_empty() {
                continue;
            }
            // `cd sub && …` in a recipe changes the directory for what follows.
            commands.push((cwd.clone(), tokens));
        }
    }
    let mut local_cwd: Option<PathBuf> = None;
    for (cwd, tokens) in &commands {
        let base = local_cwd.clone().unwrap_or_else(|| cwd.clone());
        if tokens[0] == "cd" {
            local_cwd = tokens.get(1).map(|d| normalize(cwd, Path::new(d)));
            continue;
        }
        local_cwd = None;
        let output = tokens
            .iter()
            .position(|t| t == "-o")
            .and_then(|i| tokens.get(i + 1))
            .map(|o| normalize(&base, Path::new(o)));
        let compiles = tokens.iter().any(|t| t == "-c");
        if compiles {
            if let Some(obj) = output {
                if let Some(src) = tokens.iter().find(|t| !t.starts_with('-') && super::is_source(Path::new(t.as_str()))) {
                    objects.insert(obj, canonical(&normalize(&base, Path::new(src))));
                }
            }
            continue;
        }
        let program_name = Path::new(&tokens[0])
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_ar = program_name == "ar" || program_name.ends_with("-ar");
        if is_ar && tokens.len() >= 3 && !tokens[1].starts_with('-') {
            let archive = normalize(&base, Path::new(&tokens[2]));
            let members: BTreeSet<PathBuf> = tokens[3..]
                .iter()
                .filter(|t| is_object(t))
                .map(|t| normalize(&base, Path::new(t)))
                .collect();
            archives.entry(archive).or_default().extend(members);
            continue;
        }
        if let Some(out) = output {
            if is_object(&out.to_string_lossy()) || is_library(&out.to_string_lossy()) {
                continue;
            }
            let entry = programs.entry(out).or_default();
            for token in tokens.iter().filter(|t| !t.starts_with('-')) {
                let path = normalize(&base, Path::new(token));
                if is_object(token) {
                    entry.0.insert(path);
                } else if is_library(token) {
                    entry.1.insert(path);
                }
            }
        }
    }
    // Objects → sources.
    let resolve = |set: &BTreeSet<PathBuf>| -> BTreeSet<PathBuf> {
        set.iter()
            .map(|obj| objects.get(obj).cloned().unwrap_or_else(|| obj.clone()))
            .collect()
    };
    for (_, (objs, _)) in programs.iter_mut() {
        *objs = resolve(objs);
    }
    for (_, objs) in archives.iter_mut() {
        *objs = resolve(objs);
    }
    // Programs the transcript runs (tests): a command whose program is a built
    // one, directly or behind a wrapper (env, timeout, valgrind, a shell…).
    // Compile and link lines name programs as outputs, not as what runs.
    const WRAPPERS: &[&str] = &["env", "timeout", "valgrind", "sh", "bash", "exec", "libtool", "stdbuf", "nice"];
    let mut invoked = BTreeSet::new();
    for (cwd, tokens) in &commands {
        if tokens.iter().any(|t| t == "-o" || t == "-c") {
            continue;
        }
        let mut position = 0;
        while position < tokens.len().min(4) {
            let token = &tokens[position];
            let path = normalize(cwd, Path::new(token));
            if programs.contains_key(&path) {
                invoked.insert(path);
                break;
            }
            let name = Path::new(token).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !(WRAPPERS.contains(&name.as_str()) || token.starts_with('-') || token.contains('=')) {
                break;
            }
            position += 1;
        }
    }
    (programs, archives, invoked)
}

fn is_object(token: &str) -> bool {
    token.ends_with(".o") || token.ends_with(".lo") || token.ends_with(".obj")
}

fn is_library(token: &str) -> bool {
    token.ends_with(".a") || token.ends_with(".la") || token.ends_with(".so") || token.contains(".so.")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_a_make_transcript() {
        let text = "gcc -Iinclude -c src/core.c -o build/core.o\n\
ar rcs build/libcore.a build/core.o\n\
g++ -c tests/core_test.cc -o build/core_test.o\n\
g++ -o build/core_test build/core_test.o build/libcore.a -lpthread\n\
make[1]: Entering directory '/r/sub'\n\
cc -c util.c -o util.o && cc -o util_check util.o\n\
make[1]: Leaving directory '/r/sub'\n\
./build/core_test --gtest_color=no\n";
        let (programs, archives, invoked) = parse_commands(text, Path::new("/r"));
        let names: Vec<String> = programs.keys().map(|p| p.display().to_string()).collect();
        assert_eq!(names, vec!["/r/build/core_test", "/r/sub/util_check"]);
        let (sources, libs) = &programs[Path::new("/r/build/core_test")];
        assert_eq!(sources.iter().cloned().collect::<Vec<_>>(), vec![PathBuf::from("/r/tests/core_test.cc")]);
        assert_eq!(libs.iter().cloned().collect::<Vec<_>>(), vec![PathBuf::from("/r/build/libcore.a")]);
        assert_eq!(
            archives[Path::new("/r/build/libcore.a")].iter().cloned().collect::<Vec<_>>(),
            vec![PathBuf::from("/r/src/core.c")]
        );
        assert!(invoked.contains(Path::new("/r/build/core_test")));
        assert!(!invoked.contains(Path::new("/r/sub/util_check")));
    }

    #[test]
    fn targets_are_read_from_the_makefile_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Makefile"),
            "CC := gcc\nURL = http://x:80/y\nobjs = a.o \\\n  b.o\ninclude rules.mk\n-include $(DEPS)\n\
define RECIPE\nfake: target\nendef\n\
all: prog\n\tcc -o prog $(objs)\n%.o: %.c\n\tcc -c $<\n$(OBJ): hdr.h\n.PHONY: all clean\n\
ifeq ($(X),y)\nendif\nexport PATH := $(PATH):/x\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("rules.mk"), "check:: all\n\t./prog\ninstall: all ; cp prog /x\n").expect("write");
        let targets = static_targets(dir.path());
        let names: Vec<&str> = targets.iter().map(String::as_str).collect();
        assert_eq!(names, vec!["all", "check", "clean", "install"]);
        assert_eq!(check_target_static(dir.path()).as_deref(), Some("check"));
        let none = tempfile::tempdir().expect("tempdir");
        assert_eq!(check_target_static(none.path()), None);
    }

    #[test]
    fn path_prefixes_are_replaced_only_at_path_boundaries() {
        let text = b"top = /w/repo\ninc = -I/w/repo/include -I/w/repo2/x '/w/repo'\n/w/repo.old";
        let out = replace_path_prefix(text, b"/w/repo", b"/tmp/c");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "top = /tmp/c\ninc = -I/tmp/c/include -I/w/repo2/x '/tmp/c'\n/w/repo.old"
        );
    }

    fn have_make() -> bool {
        std::process::Command::new("make")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A suite target generated by `$(eval …)` is invisible to the makefile reader; the dry run
    /// in the scratch copy finds it, and the tree is not touched.
    #[tokio::test]
    async fn a_generated_suite_target_is_found_in_the_copy() {
        if !have_make() {
            eprintln!("skipped: make not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Makefile"),
            "define SUITE\n$(1): ; @echo running $(1)\nendef\n$(eval $(call SUITE,test))\nall: ; @true\n",
        )
        .expect("write");
        assert_eq!(check_target_static(root), None);
        assert_eq!(check_target(root, root).await.as_deref(), Some("test"));
        let names: Vec<String> = std::fs::read_dir(root)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["Makefile".to_string()]);
    }

    /// A dry run can be destructive: `make -n -B` in a configured tree remakes an
    /// out-of-date `config.mk` (here: a rule that regenerates it from its template). The model
    /// runs make only in a scratch copy, so the tree keeps its content and timestamps, and
    /// the program map is still found.
    #[tokio::test]
    async fn discovery_never_writes_into_the_tree() {
        if !have_make() {
            eprintln!("skipped: make not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Makefile"),
            "include config.mk\n\
config.mk: config.mk.in\n\tcp config.mk.in config.mk\n\
check: prog_test\n\t./prog_test\n\
prog_test: main.o libcore.a\n\t$(CC) -o prog_test main.o libcore.a\n\
libcore.a: core.o\n\tar rcs libcore.a core.o\n\
main.o: main.c\n\t$(CC) -c main.c -o main.o\n\
core.o: core.c\n\t$(CC) -c core.c -o core.o\n",
        )
        .expect("write");
        std::fs::write(root.join("config.mk.in"), "CC = cc\n# template\n").expect("write");
        std::fs::write(root.join("config.mk"), "CC = cc\n# configured by the user\n").expect("write");
        std::fs::write(root.join("main.c"), "int core(void);\nint main(void){return core();}\n").expect("write");
        std::fs::write(root.join("core.c"), "int core(void){return 0;}\n").expect("write");
        // The template is newer than the configured file: any make run in the tree remakes it.
        let old = SystemTime::now() - Duration::from_secs(7200);
        set_time(&root.join("config.mk"), old);
        let before = std::fs::metadata(root.join("config.mk")).and_then(|m| m.modified()).expect("mtime");
        let listing = |root: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(root)
                .expect("read_dir")
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            names
        };
        let files_before = listing(root);

        let model = model(root, root, &[root.join("core.c")]).await.expect("model");

        assert_eq!(
            std::fs::read_to_string(root.join("config.mk")).expect("read"),
            "CC = cc\n# configured by the user\n",
            "config.mk was rewritten"
        );
        let after = std::fs::metadata(root.join("config.mk")).and_then(|m| m.modified()).expect("mtime");
        assert_eq!(before, after, "config.mk was touched");
        assert_eq!(listing(root), files_before, "files appeared in the tree");
        assert_eq!(model.check_target.as_deref(), Some("check"));
        let program = model.target("prog_test").expect("program mapped");
        assert!(program.sources.iter().any(|s| s.ends_with("main.c")), "{:?}", program.sources);
        assert_eq!(program.deps, vec!["libcore.a".to_string()]);
        assert!(model.tests.iter().any(|t| t.name == "prog_test"), "{:?}", model.tests);
        assert!(
            model.tests[0].command[0].starts_with(&*root.to_string_lossy())
                || model.tests[0].command[0].starts_with(&*canonical(root).to_string_lossy()),
            "paths point back at the tree: {:?}",
            model.tests[0].command
        );
        let rebuilds = model.make_rebuilds.clone().unwrap_or_default();
        assert!(rebuilds.iter().any(|p| p.ends_with("libcore.a")), "{rebuilds:?}");
        assert!(!rebuilds.iter().any(|p| p.ends_with("main.o")), "{rebuilds:?}");
        assert!(model.mapping_note.contains("scratch copy"), "{}", model.mapping_note);
    }
}
