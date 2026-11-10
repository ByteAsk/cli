//! The Meson build model, read from an existing build directory's
//! `meson-info/intro-targets.json` and `intro-tests.json` (the files
//! `meson introspect` prints; reading them needs no meson binary and changes
//! nothing). Links between the project's own targets are not in those files;
//! they come from `ninja -t query` on each library's output.

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
use crate::run::RunOutcome;
use crate::run::run_capped;

/// Most libraries whose dependents are looked up with `ninja -t query`.
const QUERY_CAP: usize = 200;

/// Meson build directories of this project: the root's subdirectories and one
/// level below `build*`, newest first.
pub(crate) fn existing_build_dirs(root: &Path) -> Vec<PathBuf> {
    let root_c = canonical(root);
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            candidates.push(path.clone());
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("build") || name == "out" || name == "_build" {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    candidates.extend(inner.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
                }
            }
        }
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = candidates
        .into_iter()
        .filter_map(|dir| {
            let info = dir.join("meson-info/intro-targets.json");
            let meta = std::fs::metadata(&info).ok()?;
            let source = std::fs::read_to_string(dir.join("meson-info/meson-info.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v["directories"]["source"].as_str().map(PathBuf::from));
            if source.is_some_and(|s| !canonical(&s).starts_with(&root_c)) {
                return None;
            }
            Some((meta.modified().unwrap_or(std::time::UNIX_EPOCH), dir))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.into_iter().map(|(_, d)| d).collect()
}

/// The model of the Meson build directory `build`.
pub(crate) async fn model(root: &Path, build: &Path) -> Result<Model, String> {
    let read = |name: &str| -> Result<Value, String> {
        let path = build.join("meson-info").join(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
    };
    let targets_json = read("intro-targets.json")?;
    let tests_json = read("intro-tests.json").unwrap_or(Value::Array(Vec::new()));
    let mut targets = parse_targets(&targets_json);
    let tests = parse_tests(&tests_json, &targets);
    if crate::tools::binary_on_path("ninja") {
        add_link_deps(build, &mut targets).await;
    }
    let options = std::fs::read_to_string(build.join("meson-info/intro-buildoptions.json"))
        .unwrap_or_default();
    Ok(Model {
        system: System::Meson,
        root: root.to_path_buf(),
        build_dir: build.to_path_buf(),
        dir_kind: DirKind::Existing,
        version: None,
        ninja: true,
        targets,
        tests,
        check_target: None,
        make_rebuilds: None,
        mapping_note: "targets and tests from meson-info (introspection files)".to_string(),
        notes: Vec::new(),
        config: format!(
            "meson|{:016x}",
            crate::base_run_cache::fnv(options.as_bytes())
        ),
    })
}

pub(crate) fn parse_targets(value: &Value) -> Vec<Target> {
    let Some(list) = value.as_array() else {
        return Vec::new();
    };
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    for t in list {
        if let Some(name) = t["name"].as_str() {
            *names.entry(name.to_string()).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for t in list {
        let Some(name) = t["name"].as_str() else {
            continue;
        };
        let id = t["id"].as_str().unwrap_or(name);
        // Target names need not be unique across subdirectories; ids are.
        let name = if names.get(name).copied().unwrap_or(0) > 1 {
            id.to_string()
        } else {
            name.to_string()
        };
        let kind = t["type"].as_str().unwrap_or("");
        let artifacts: Vec<PathBuf> = t["filename"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut sources: Vec<PathBuf> = Vec::new();
        if let Some(groups) = t["target_sources"].as_array() {
            for group in groups {
                for key in ["sources", "generated_sources"] {
                    if let Some(files) = group[key].as_array() {
                        sources.extend(
                            files
                                .iter()
                                .filter_map(|f| f.as_str())
                                .map(|f| canonical(Path::new(f))),
                        );
                    }
                }
            }
        }
        sources.sort();
        sources.dedup();
        out.push(Target {
            name,
            is_exe: kind == "executable",
            artifacts,
            sources,
            deps: Vec::new(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub(crate) fn parse_tests(value: &Value, targets: &[Target]) -> Vec<TestEntry> {
    let Some(list) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in list {
        let name = t["name"].as_str().unwrap_or_default().to_string();
        let command: Vec<String> = t["cmd"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env: Vec<(String, String)> = t["env"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let workdir = t["workdir"].as_str().map(PathBuf::from);
        let program = command.first().map(PathBuf::from);
        let target = targets
            .iter()
            .find(|tg| program.as_ref().is_some_and(|p| tg.artifacts.contains(p)))
            .map(|tg| tg.name.clone());
        out.push(TestEntry {
            name,
            command,
            workdir,
            env,
            target,
            disabled: false,
        });
    }
    out
}

/// Fill `deps` from `ninja -t query <library output>`: whatever lists the
/// library among its inputs depends on it.
async fn add_link_deps(build: &Path, targets: &mut [Target]) {
    let libraries: Vec<(String, PathBuf)> = targets
        .iter()
        .filter(|t| !t.is_exe)
        .filter_map(|t| t.artifacts.first().map(|a| (t.name.clone(), a.clone())))
        .take(QUERY_CAP)
        .collect();
    for (lib, artifact) in libraries {
        let rel = artifact
            .strip_prefix(build)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| artifact.clone());
        let args = vec![
            "-C".to_string(),
            build.display().to_string(),
            "-t".to_string(),
            "query".to_string(),
            rel.display().to_string(),
        ];
        let Ok(RunOutcome::Finished {
            success: true,
            stdout,
            ..
        }) = run_capped("ninja", &args, build, Duration::from_secs(20)).await
        else {
            continue;
        };
        for output in query_outputs(&stdout) {
            let output = build.join(output);
            for target in targets.iter_mut() {
                if target.name != lib
                    && target.artifacts.contains(&output)
                    && !target.deps.contains(&lib)
                {
                    target.deps.push(lib.clone());
                }
            }
        }
    }
}

/// The `outputs:` section of `ninja -t query`.
pub(crate) fn query_outputs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_outputs = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "outputs:" {
            in_outputs = true;
            continue;
        }
        if trimmed.ends_with(':') && !line.starts_with("    ") {
            in_outputs = false;
            continue;
        }
        if in_outputs && line.starts_with("    ") && !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn reads_introspection_files() {
        let targets = serde_json::json!([
            {"name": "core", "id": "core@sta", "type": "static library", "filename": ["/b/libcore.a"],
             "target_sources": [{"language": "cpp", "sources": ["/nonexistent/src/core.cpp"], "generated_sources": []}]},
            {"name": "core_test", "id": "core_test@exe", "type": "executable", "filename": ["/b/core_test"],
             "target_sources": [{"language": "cpp", "sources": ["/nonexistent/tests/core_test.cpp"]}]}
        ]);
        let tests = serde_json::json!([
            {"name": "core", "suite": ["fx"], "cmd": ["/b/core_test", "--x"], "env": {"K": "V"}, "workdir": null, "depends": ["core_test@exe"]},
            {"name": "script", "cmd": ["/bin/sh", "t.sh"], "env": {}, "workdir": "/w"}
        ]);
        let targets = parse_targets(&targets);
        assert_eq!(targets.len(), 2);
        assert!(targets[1].is_exe);
        assert_eq!(
            targets[0].sources,
            vec![PathBuf::from("/nonexistent/src/core.cpp")]
        );
        let tests = parse_tests(&tests, &targets);
        assert_eq!(tests[0].target.as_deref(), Some("core_test"));
        assert_eq!(tests[0].env, vec![("K".to_string(), "V".to_string())]);
        assert_eq!(tests[1].target, None);
        assert_eq!(tests[1].workdir, Some(PathBuf::from("/w")));
    }

    #[test]
    fn reads_ninja_query_outputs() {
        let text = "libcore.a:\n  input: STATIC_LINKER\n    core.p/core.cpp.o\n  outputs:\n    core_test\n    api_test\n";
        assert_eq!(
            query_outputs(text),
            vec!["core_test".to_string(), "api_test".to_string()]
        );
    }
}
