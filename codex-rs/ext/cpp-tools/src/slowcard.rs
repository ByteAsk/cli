//! "Why is my build slow": when a check runs out of the fast tier's time,
//! the turn's message names the slowest files to compile and the heaviest template
//! instantiations, from clang's `-ftime-trace` data.
//!
//! Where the data comes from, in order:
//! 1. the context engine's per-file evidence (`bctx` records `frontend_us` and the
//!    20 heaviest instantiations of every file it indexes with clang): a JSON export of its
//!    `tu_versions` rows (`[{"tu_path": ..., "evidence_json": "{...}"}]`, what
//!    `sqlite3 -json <partition> 'select tu_path, evidence_json from tu_versions'` prints), or a
//!    directory of the raw trace files it collected, named by `BYTEASK_CPP_TIME_TRACE`;
//! 2. trace files clang wrote into the build tree (a project that builds with `-ftime-trace`).
//!
//! Costs are parsed exactly as the context engine parses them: the
//! `Total Frontend` event is the file's front-end time; `InstantiateFunction` and
//! `InstantiateClass` events are summed per (kind, name). Nothing here builds anything.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// Where the context engine's time-trace data is (a JSON export or a directory of traces).
pub(crate) const ENV_TIME_TRACE: &str = "BYTEASK_CPP_TIME_TRACE";

/// Files scanned for traces in one build tree, at most.
const SCAN_CAP: usize = 20_000;
/// Trace files read, at most (the newest first).
const READ_CAP: usize = 2_000;
/// A trace file bigger than this is skipped (traces of huge files run to hundreds of MB).
const MAX_TRACE_BYTES: u64 = 64 * 1024 * 1024;

/// One compiled file's costs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileCost {
    pub path: String,
    /// Front-end time, microseconds.
    pub frontend_us: u64,
    /// `"<function|class> <name>"` → microseconds spent instantiating it in this file.
    pub instantiations: BTreeMap<String, u64>,
}

/// Costs from one clang `-ftime-trace` JSON file.
pub(crate) fn parse_trace(path: &str, json: &str) -> Option<FileCost> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let events = value["traceEvents"].as_array()?;
    let mut cost = FileCost {
        path: path.to_string(),
        ..FileCost::default()
    };
    for event in events {
        let dur = event["dur"].as_u64().unwrap_or(0);
        match event["name"].as_str() {
            Some("InstantiateFunction") | Some("InstantiateClass") => {
                let kind = if event["name"] == "InstantiateFunction" {
                    "function"
                } else {
                    "class"
                };
                let detail = event["args"]["detail"].as_str().unwrap_or("");
                *cost
                    .instantiations
                    .entry(format!("{kind} {detail}"))
                    .or_default() += dur;
            }
            Some("Total Frontend") => cost.frontend_us = cost.frontend_us.max(dur),
            _ => {}
        }
    }
    Some(cost)
}

/// Costs from the context engine's `tu_versions` rows (its `evidence_json` shape:
/// `frontend_us` and `heaviest_instantiations` = `[["<kind> <name>", us], ...]`).
pub(crate) fn parse_context_export(json: &str) -> Vec<FileCost> {
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let path = row["tu_path"].as_str()?.to_string();
            let evidence: serde_json::Value = match &row["evidence_json"] {
                serde_json::Value::String(text) => serde_json::from_str(text).ok()?,
                other => other.clone(),
            };
            let frontend_us = evidence["frontend_us"].as_u64()?;
            let instantiations = evidence["heaviest_instantiations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pair| Some((pair[0].as_str()?.to_string(), pair[1].as_u64()?)))
                .collect();
            Some(FileCost {
                path,
                frontend_us,
                instantiations,
            })
        })
        .collect()
}

/// Every clang trace file under `dir` (newest first, capped).
fn trace_files(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .take(SCAN_CAP)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            (meta.len() <= MAX_TRACE_BYTES).then(|| {
                (
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    entry.into_path(),
                )
            })
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found
        .into_iter()
        .take(READ_CAP)
        .map(|(_, path)| path)
        .collect()
}

fn costs_in_dir(dir: &Path) -> Vec<FileCost> {
    trace_files(dir)
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            if !text.contains("\"traceEvents\"") {
                return None;
            }
            // clang names the trace after the object: `foo.cpp.o` -> `foo.cpp.json`.
            let name = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let name = name.strip_suffix(".json").unwrap_or(&name).to_string();
            parse_trace(&name, &text)
        })
        .filter(|cost| cost.frontend_us > 0)
        .collect()
}

/// All the cost data there is for a check whose build trees are `trees`.
pub(crate) fn collect(trees: &[PathBuf], env: impl Fn(&str) -> Option<String>) -> Vec<FileCost> {
    if let Some(source) = env(ENV_TIME_TRACE).filter(|s| !s.trim().is_empty()) {
        let source = PathBuf::from(source.trim());
        let costs = if source.is_dir() {
            costs_in_dir(&source)
        } else {
            std::fs::read_to_string(&source)
                .map(|text| parse_context_export(&text))
                .unwrap_or_default()
        };
        if !costs.is_empty() {
            return costs;
        }
    }
    trees
        .iter()
        .filter(|tree| tree.is_dir())
        .flat_map(|tree| costs_in_dir(tree))
        .collect()
}

fn secs(us: u64) -> String {
    format!("{:.1} s", us as f64 / 1e6)
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!(
            "{}...",
            text.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

/// The card, or `None` when there is no data.
pub(crate) fn card(costs: &[FileCost]) -> Option<String> {
    if costs.is_empty() {
        return None;
    }
    let mut files: Vec<&FileCost> = costs.iter().collect();
    files.sort_by(|a, b| b.frontend_us.cmp(&a.frontend_us).then(a.path.cmp(&b.path)));
    let total: u64 = costs.iter().map(|c| c.frontend_us).sum();
    let mut heavy: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    for cost in costs {
        for (name, us) in &cost.instantiations {
            let entry = heavy.entry(name.as_str()).or_default();
            entry.0 += us;
            entry.1 += 1;
        }
    }
    let mut heavy: Vec<(&str, (u64, usize))> = heavy.into_iter().collect();
    heavy.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let mut out = format!(
        "Why the build is slow (clang -ftime-trace, {} file(s), {} of front-end time):\n\
         Slowest files to compile:\n",
        costs.len(),
        secs(total)
    );
    for cost in files.iter().take(5) {
        out.push_str(&format!(
            "  {:>7}  {}\n",
            secs(cost.frontend_us),
            shorten(&cost.path, 100)
        ));
    }
    if !heavy.is_empty() {
        out.push_str("Heaviest template instantiations (summed over files):\n");
        for (name, (us, files)) in heavy.iter().take(5) {
            out.push_str(&format!(
                "  {:>7}  {} (in {files} file(s))\n",
                secs(*us),
                shorten(name, 120)
            ));
        }
    }
    out.push_str(
        "A warm build tree makes the next check incremental; explicit instantiation or moving \
         these templates out of widely included headers cuts the cold build.",
    );
    Some(out)
}

#[cfg(test)]
#[path = "slowcard_tests.rs"]
mod tests;
