//! Session-start environment note: a few facts about this machine and
//! project that change what is worth trying, gathered once per thread in the
//! background and shown to the model next to the C/C++ guidance.
//!
//! - the build system(s) at the project root, and build trees that already
//!   exist (so they are reused as they are, not reconfigured);
//! - the compile database, when there is one;
//! - the toolchain on PATH with versions, from the same cached `--version`
//!   probe the analysis tools use ([`crate::toolver::tool_version`]), and what
//!   is not installed — old compilers reject newer `-std=` spellings;
//! - the CPUs this session may use, as the job count to build with, so the model
//!   does not run `-j16` on a 3-CPU quota.
//!
//! It makes no network connection. The note is part of the cached prompt prefix, so
//! it must come out the same in every session on the same machine and workspace:
//! it holds no free-memory figure (which moves between sessions), prompt assembly
//! waits for the probes instead of cutting them off after a few seconds under
//! load, and probed versions are kept in `<home>/cache/cpp-toolchain.json`, keyed
//! by each binary's path, size and modification time, so a later session reads
//! the same answer at once instead of racing a slow `--version`.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::watch;

/// Tools listed in the note, in display order.
const TOOLCHAIN: &[&str] = &[
    "gcc",
    "clang",
    "cmake",
    "make",
    "ninja",
    "cppcheck",
    "clang-tidy",
    "gdb",
    "valgrind",
];
/// How long prompt assembly waits for the probes. Each `--version` has its own 10 s limit
/// and they run at once, so this is only reached when the machine is stalled.
const NOTE_WAIT: Duration = Duration::from_secs(30);
/// The probed-version cache under the engine's home.
const VERSION_CACHE: &str = "cpp-toolchain.json";
/// Most existing build trees named.
const BUILD_DIR_CAP: usize = 4;

/// The note for one thread: computed in the background from thread start,
/// read (with a short wait) when the prompt is assembled.
#[derive(Clone)]
pub(crate) struct EnvNote(watch::Receiver<Option<String>>);

impl EnvNote {
    /// Start gathering the note for the project at `cwd`; probed versions are cached in
    /// `cache_dir` (the engine home's `cache/`) when one is given.
    pub(crate) fn spawn(cwd: PathBuf, cache_dir: Option<PathBuf>) -> Self {
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            let note = gather(&cwd, cache_dir.as_deref()).await;
            let _ = tx.send(Some(note));
        });
        Self(rx)
    }

    /// No note: gated threads and workspaces that are not C/C++.
    pub(crate) fn none() -> Self {
        Self::ready(String::new())
    }

    /// A note that is already known.
    pub(crate) fn ready(text: String) -> Self {
        let (_tx, rx) = watch::channel(Some(text));
        Self(rx)
    }

    /// The note, or "" when there is none. Only a machine stalled past [`NOTE_WAIT`] gets
    /// the short form (the header and the CPU line).
    pub(crate) async fn text(&self) -> String {
        let mut rx = self.0.clone();
        match tokio::time::timeout(NOTE_WAIT, rx.wait_for(Option::is_some)).await {
            Ok(Ok(value)) => value.clone().unwrap_or_default(),
            _ => format!(
                "{HEADER}\n{}",
                resource_line(crate::resources::cpus(), probe_memory())
            ),
        }
    }
}

async fn gather(cwd: &Path, cache_dir: Option<&Path>) -> String {
    let tools = probe_toolchain(cwd, cache_dir).await;
    // A workspace without a build system of its own that is a parent of the repository: the
    // facts are those of the repository below it.
    let project = if build_systems(cwd).is_empty() {
        crate::gitx::repo_roots_sync(cwd)
            .into_iter()
            .find(|root| root != cwd && !build_systems(root).is_empty())
    } else {
        None
    };
    let root = project.as_deref().unwrap_or(cwd);
    let db = crate::tools::find_compile_db(root);
    render(&Facts {
        project: project
            .as_ref()
            .map(|p| p.strip_prefix(cwd).unwrap_or(p).display().to_string()),
        build_systems: build_systems(root),
        build_dirs: existing_build_dirs(root),
        compile_db: db.map(|db| db.strip_prefix(cwd).unwrap_or(&db).display().to_string()),
        tools,
        cpus: crate::resources::cpus(),
        memory: probe_memory(),
    })
}

const HEADER: &str = "C/C++ environment (probed at session start):";

/// `- CPUs: …` and the job count to build with. Only the CPU count (cgroup quota included):
/// free memory changes between sessions and would change the cached prefix with it.
fn cpu_line(cpus: usize) -> String {
    let cpus = cpus.max(1);
    let jobs = probe_jobs(cpus);
    let noun = if jobs == 1 { "job" } else { "jobs" };
    format!(
        "- CPUs: {cpus} available to this session; build and test with at most {jobs} parallel \
         {noun} (make -j{jobs}, cmake --build <dir> -j {jobs}, ctest -j{jobs})"
    )
}

/// Test switch (off unless set): `BYTEASK_PROBE_JOBS=<1..64>` is the job count the CPU line states
/// (measured per build, not derived from the CPU count); unset or out of range: `default`.
fn probe_jobs(default: usize) -> usize {
    std::env::var("BYTEASK_PROBE_JOBS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|jobs| (1..=64).contains(jobs))
        .unwrap_or(default)
}

/// Test switch (off unless set): `BYTEASK_PROBE_MEMORY=1` puts the session's memory LIMIT on the CPU line
/// (a fixed number, so the cached prefix stays the same between sessions; never free memory) and takes the job
/// count from it too.
const ENV_PROBE_MEMORY: &str = "BYTEASK_PROBE_MEMORY";

/// The memory limit when [`ENV_PROBE_MEMORY`] is on.
fn probe_memory() -> Option<u64> {
    let on = std::env::var(ENV_PROBE_MEMORY)
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"));
    if on {
        crate::resources::memory_limit()
    } else {
        None
    }
}

/// [`cpu_line`], or with a memory limit: `- CPUs: 3 and memory: 5 GB available to this session; ... at most 2
/// parallel jobs (...)`, the job count bounded by [`crate::resources::jobs_for_limit`].
fn resource_line(cpus: usize, memory: Option<u64>) -> String {
    let Some(limit) = memory else {
        return cpu_line(cpus);
    };
    let cpus = cpus.max(1);
    let jobs = probe_jobs(crate::resources::jobs_for_limit(cpus, limit));
    let noun = if jobs == 1 { "job" } else { "jobs" };
    format!(
        "- CPUs: {cpus} and memory: {} available to this session; build and test with at most {jobs} \
         parallel {noun} (make -j{jobs}, cmake --build <dir> -j {jobs}, ctest -j{jobs})",
        gigabytes(limit)
    )
}

/// `5 GB`, or `5.5 GB` (GiB, to a tenth).
fn gigabytes(bytes: u64) -> String {
    let tenths = (u128::from(bytes) * 10 + (1 << 29)) >> 30;
    if tenths % 10 == 0 {
        format!("{} GB", tenths / 10)
    } else {
        format!("{}.{} GB", tenths / 10, tenths % 10)
    }
}

/// What the note says, gathered.
#[derive(Debug, Default)]
struct Facts {
    /// The repository below the workspace the facts describe (relative), when the workspace
    /// is a parent of it.
    project: Option<String>,
    build_systems: Vec<&'static str>,
    /// `(dir, what marks it as a build tree)`.
    build_dirs: Vec<(String, &'static str)>,
    compile_db: Option<String>,
    /// `(tool, None)` = not installed; `(tool, Some(""))` = installed, version unknown.
    tools: Vec<(&'static str, Option<String>)>,
    /// CPUs this session may use (0 = unknown: no CPU line).
    cpus: usize,
    /// With [`ENV_PROBE_MEMORY`]: the session's memory limit, stated with the CPUs.
    memory: Option<u64>,
}

fn render(facts: &Facts) -> String {
    let mut out = format!("{HEADER}\n");
    let systems = if facts.build_systems.is_empty() {
        "none detected at the workspace root".to_string()
    } else {
        facts.build_systems.join(", ")
    };
    out.push_str(&format!("- build system: {systems}"));
    if let Some(project) = &facts.project {
        out.push_str(&format!(" (project in {project}/)"));
    }
    if !facts.build_dirs.is_empty() {
        let prefix = facts.project.as_ref().map(|p| format!("{p}/")).unwrap_or_default();
        let dirs: Vec<String> = facts
            .build_dirs
            .iter()
            .map(|(dir, kind)| format!("{prefix}{dir}/ ({kind})"))
            .collect();
        out.push_str(&format!("; existing build tree(s): {}", dirs.join(", ")));
    }
    out.push('\n');
    out.push_str(&format!(
        "- compile_commands.json: {}\n",
        facts.compile_db.as_deref().unwrap_or("none")
    ));
    let installed: Vec<String> = facts
        .tools
        .iter()
        .filter_map(|(tool, version)| {
            version.as_ref().map(|v| {
                if v.is_empty() {
                    tool.to_string()
                } else {
                    format!("{tool} {v}")
                }
            })
        })
        .collect();
    let missing: Vec<&str> = facts
        .tools
        .iter()
        .filter(|(_, version)| version.is_none())
        .map(|(tool, _)| *tool)
        .collect();
    out.push_str(&format!(
        "- toolchain: {}",
        if installed.is_empty() {
            "none of the usual tools found".to_string()
        } else {
            installed.join(", ")
        }
    ));
    if !missing.is_empty() {
        out.push_str(&format!("; not installed: {}", missing.join(", ")));
    }
    out.push('\n');
    if facts.cpus > 0 {
        out.push_str(&resource_line(facts.cpus, facts.memory));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Every [`TOOLCHAIN`] entry with its version, probed concurrently, answered from the
/// version cache in `cache_dir` when the binary is the one that was probed before.
async fn probe_toolchain(
    cwd: &Path,
    cache_dir: Option<&Path>,
) -> Vec<(&'static str, Option<String>)> {
    let cache_file = cache_dir.map(|dir| dir.join(VERSION_CACHE));
    let mut cache = cache_file
        .as_deref()
        .map(read_version_cache)
        .unwrap_or_default();
    let mut probes = tokio::task::JoinSet::new();
    let mut versions: Vec<Option<String>> = vec![None; TOOLCHAIN.len()];
    for (index, tool) in TOOLCHAIN.iter().enumerate() {
        let Some(binary) = find_on_path(tool) else {
            continue;
        };
        let key = binary_key(&binary);
        if let Some(version) = key.as_ref().and_then(|key| cache.get(key)) {
            versions[index] = Some(version.clone());
            continue;
        }
        let cwd = cwd.to_path_buf();
        probes.spawn(async move {
            let version = crate::toolver::tool_version(tool, &cwd)
                .await
                .map(|(major, minor)| format!("{major}.{minor}"));
            (index, key, version)
        });
    }
    let mut learned = false;
    while let Some(joined) = probes.join_next().await {
        if let Ok((index, key, version)) = joined {
            // A probe that failed or timed out says "installed, version unknown" and is
            // not cached, so a later session tries again.
            if let (Some(key), Some(version)) = (key, version.as_ref()) {
                cache.insert(key, version.clone());
                learned = true;
            }
            versions[index] = Some(version.unwrap_or_default());
        }
    }
    if learned && let Some(file) = cache_file.as_deref() {
        write_version_cache(file, &cache);
    }
    TOOLCHAIN.iter().copied().zip(versions).collect()
}

/// The first executable named `name` on `PATH`.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `path|size|mtime` of the binary a name resolves to (symlinks followed), so a cached
/// version is used only for the very file it was probed from.
fn binary_key(binary: &Path) -> Option<String> {
    let real = std::fs::canonicalize(binary).ok()?;
    let meta = std::fs::metadata(&real).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{}|{}|{mtime}", real.display(), meta.len()))
}

fn read_version_cache(file: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Best-effort and atomic (temp file + rename); a failure only means the next session
/// probes again.
fn write_version_cache(file: &Path, cache: &std::collections::BTreeMap<String, String>) {
    let Some(dir) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(cache) else {
        return;
    };
    let tmp = dir.join(format!("{VERSION_CACHE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Build systems whose root file is in the project root, in a fixed order.
fn build_systems(root: &Path) -> Vec<&'static str> {
    const MARKERS: &[(&[&str], &str)] = &[
        (&["CMakeLists.txt"], "CMake"),
        (&["meson.build"], "Meson"),
        (&["configure.ac", "configure.in", "configure"], "Autotools"),
        (&["GNUmakefile", "makefile", "Makefile"], "Make"),
        (&["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"], "Bazel"),
        (&["SConstruct"], "SCons"),
    ];
    MARKERS
        .iter()
        .filter(|(files, _)| files.iter().any(|file| root.join(file).is_file()))
        .map(|(_, name)| *name)
        .collect()
}

/// Top-level directories that already hold a configured build tree.
fn existing_build_dirs(root: &Path) -> Vec<(String, &'static str)> {
    const MARKERS: &[(&str, &str)] = &[
        ("CMakeCache.txt", "CMake cache"),
        ("build.ninja", "Ninja"),
        ("meson-private", "Meson"),
        ("config.status", "configured"),
    ];
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, &'static str)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let path = entry.path();
            MARKERS
                .iter()
                .find(|(marker, _)| path.join(marker).exists())
                .map(|(_, kind)| (name, *kind))
        })
        .collect();
    dirs.sort();
    dirs.truncate(BUILD_DIR_CAP);
    dirs
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn detects_build_systems_and_existing_build_trees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("CMakeLists.txt"), "").unwrap();
        std::fs::write(root.join("Makefile"), "").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/CMakeCache.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/build.ninja"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(build_systems(root), vec!["CMake", "Make"]);
        assert_eq!(
            existing_build_dirs(root),
            vec![
                ("build".to_string(), "CMake cache"),
                ("out".to_string(), "Ninja")
            ]
        );
    }

    #[test]
    fn renders_a_compact_note() {
        let note = render(&Facts {
            project: None,
            build_systems: vec!["CMake"],
            build_dirs: vec![("build".to_string(), "CMake cache")],
            compile_db: Some("build/compile_commands.json".to_string()),
            tools: vec![
                ("gcc", Some("5.4".to_string())),
                ("valgrind", Some(String::new())),
                ("clang", None),
            ],
            cpus: 3,
            memory: None,
        });
        assert_eq!(
            note,
            "C/C++ environment (probed at session start):\n\
             - build system: CMake; existing build tree(s): build/ (CMake cache)\n\
             - compile_commands.json: build/compile_commands.json\n\
             - toolchain: gcc 5.4, valgrind; not installed: clang\n\
             - CPUs: 3 available to this session; build and test with at most 3 parallel jobs \
             (make -j3, cmake --build <dir> -j 3, ctest -j3)"
        );
        assert!(note.len() < 700, "{} bytes", note.len());
        let quiet = render(&Facts::default());
        assert!(!quiet.contains("network"), "{quiet}");
        assert!(quiet.contains("build system: none detected"), "{quiet}");
    }

    /// The job count is the CPU count, nothing that moves between sessions.
    #[test]
    fn the_cpu_line_names_the_job_limit() {
        assert_eq!(
            cpu_line(3),
            "- CPUs: 3 available to this session; build and test with at most 3 parallel jobs \
             (make -j3, cmake --build <dir> -j 3, ctest -j3)"
        );
        assert!(cpu_line(1).contains("at most 1 parallel job (make -j1"));
        assert!(cpu_line(0).contains("- CPUs: 1 available"));
    }

    /// The prefix is the same in every session: a version probed once is read back from
    /// the cache, and a binary that changed is probed again.
    #[tokio::test]
    async fn probed_versions_are_cached_per_binary() {
        let home = tempfile::tempdir().unwrap();
        let cache_dir = home.path().join("cache");
        let cwd = tempfile::tempdir().unwrap();
        let first = probe_toolchain(cwd.path(), Some(&cache_dir)).await;
        let cached = read_version_cache(&cache_dir.join(VERSION_CACHE));
        // Every tool that answered with a version is in the cache.
        let answered = first
            .iter()
            .filter(|(_, version)| version.as_deref().is_some_and(|v| !v.is_empty()))
            .count();
        assert_eq!(cached.len(), answered, "{cached:?}");
        // A second probe answers the same, from the cache.
        let second = probe_toolchain(cwd.path(), Some(&cache_dir)).await;
        assert_eq!(first, second);

        // A cached entry is keyed by the file: another file gets no cached answer.
        let fake = cwd.path().join("tool");
        std::fs::write(&fake, "x").unwrap();
        let key = binary_key(&fake).expect("key");
        let mut map = std::collections::BTreeMap::new();
        map.insert(key.clone(), "9.9".to_string());
        write_version_cache(&cache_dir.join(VERSION_CACHE), &map);
        assert_eq!(
            read_version_cache(&cache_dir.join(VERSION_CACHE))
                .get(&key)
                .map(String::as_str),
            Some("9.9")
        );
        std::fs::write(&fake, "longer").unwrap();
        assert_ne!(binary_key(&fake), Some(key));
    }

    #[tokio::test]
    async fn the_note_is_gathered_and_empty_when_there_is_none() {
        assert_eq!(EnvNote::none().text().await, "");
        assert_eq!(EnvNote::ready("x".to_string()).text().await, "x");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CMakeLists.txt"), "").unwrap();
        let note = gather(dir.path(), None).await;
        assert!(note.contains("- build system: CMake"), "{note}");
        assert!(note.contains("- toolchain: "), "{note}");
        assert!(note.contains("- CPUs: "), "{note}");
        assert!(!note.contains("network"), "{note}");
    }

    /// A workspace that is a parent of the repository describes the repository below it.
    #[tokio::test]
    async fn a_parent_workspace_describes_the_repository_below() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("build")).unwrap();
        std::fs::write(repo.join("CMakeLists.txt"), "").unwrap();
        std::fs::write(repo.join("build/CMakeCache.txt"), "").unwrap();
        let note = gather(dir.path(), None).await;
        assert!(
            note.contains("- build system: CMake (project in proj/); existing build tree(s): proj/build/ (CMake cache)"),
            "{note}"
        );
    }
}
