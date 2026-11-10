//! What the trial keeps between runs (`$BYTEASK_HOME/trial/state.json`), the
//! index step, and the record that lets the context tools find the index.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use check_result::CheckResult;
use check_result::NotRunReason;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt as _;

use super::build::Built;
use super::install::Installed;

pub const SCHEMA: u32 = 1;
const INDEX_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// One step of a run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub name: String,
    pub result: CheckResult,
    pub seconds: f64,
}

/// What the index step measured. Counts only (the report is shareable).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexFacts {
    /// Translation units of the target's build closure that were indexed.
    pub translation_units: u64,
    /// Targets in that closure (the target and its first-party dependencies).
    pub targets: u64,
    /// First-party lines and files read, from the last COMPLETED snapshot;
    /// `None` = no count yet.
    pub lines: Option<u64>,
    pub files: Option<u64>,
    /// Share of lines only approximately indexed (0.0-1.0).
    pub approximate_share: f64,
    /// Seconds the index step took (the whole driver, incl. the overlay).
    pub seconds: f64,
    /// Production compiler kind and version (e.g. "gcc 12.2.0").
    pub compiler: String,
    /// "indexed" (this run indexed the commit) or "kept" (uncommitted changes:
    /// the last committed snapshot was kept and the changes went to the overlay).
    pub committed: String,
    /// The overlay of uncommitted changes: "clean", "indexed N", or why not.
    pub overlay: String,
    /// Compiled template versions in the snapshot (bctx's own count).
    pub template_versions: Option<u64>,
}

/// One check's results over the files it ran on.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckTally {
    pub check: String,
    pub runs: u64,
    pub pass: u64,
    pub fail: u64,
    pub no_claim: u64,
    pub approximate: u64,
    pub not_run: u64,
    /// Each distinct NOT RUN reason with its fix.
    pub not_run_reasons: Vec<(String, String)>,
    /// Files the target has that were not checked (over the cap).
    pub not_checked: u64,
}

/// The trial's state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrialState {
    pub schema: u32,
    pub project_root: PathBuf,
    pub target: String,
    pub build_type: String,
    pub steps: Vec<Step>,
    pub index: Option<IndexFacts>,
    pub checks: Vec<CheckTally>,
    pub updated_at: u64,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("trial")
}

pub fn load(home: &Path) -> Option<TrialState> {
    let text = std::fs::read_to_string(dir(home).join("state.json")).ok()?;
    serde_json::from_str::<TrialState>(&text)
        .ok()
        .filter(|state| state.schema == SCHEMA)
}

pub fn save(home: &Path, state: &TrialState) -> anyhow::Result<()> {
    let dir = dir(home);
    context_tools::engine::create_private_dir(&dir)?;
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, dir.join("state.json"))?;
    Ok(())
}

/// The index data root (bctx's own default, under the ByteAsk home).
pub fn data_dir(home: &Path) -> PathBuf {
    home.join("context")
}

/// The project's name in the index: its directory name plus a hash of its
/// path, so two checkouts called `feed` never share an entry.
pub fn project_name(root: &Path) -> String {
    results_store::project_key(root)
}

pub struct IndexOutcome {
    pub result: CheckResult,
    pub facts: Option<IndexFacts>,
    /// The compile DB of the indexed translation units (the checks use it).
    pub compdb: PathBuf,
}

fn driver_env(home: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "BCTX_TMP".to_string(),
            data_dir(home).join("tmp").to_string_lossy().into_owned(),
        ),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ];
    for key in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(key) {
            env.push((key.to_string(), value.to_string_lossy().into_owned()));
        }
    }
    env
}

/// Facts from the driver's JSON (`ok: true`).
pub fn facts_from(answer: &serde_json::Value) -> IndexFacts {
    let lines = &answer["lines"]["projects"][0];
    let status = &answer["status"]["settings"][0];
    let stats = &status["stats"];
    let overlay = &answer["overlay"];
    let overlay = match overlay["state"].as_str() {
        Some("clean") => "clean (no uncommitted changes)".to_string(),
        Some(state) => {
            let tus = overlay["tus"].as_array().map(Vec::len).unwrap_or(0);
            if tus > 0 {
                format!(
                    "{state}: {tus} translation unit(s) re-indexed from your uncommitted changes"
                )
            } else {
                state.to_string()
            }
        }
        // The reason can quote paths; the terminal showed it, the report does not.
        None => "not built".to_string(),
    };
    IndexFacts {
        translation_units: answer["tus"].as_u64().unwrap_or(0),
        targets: answer["targets_in_closure"]
            .as_array()
            .map(|t| t.len() as u64)
            .unwrap_or(0),
        lines: lines["lines"].as_u64(),
        files: lines["files"].as_u64(),
        approximate_share: status["approximate_line_share"].as_f64().unwrap_or(0.0),
        seconds: answer["seconds"].as_f64().unwrap_or(0.0),
        compiler: status["compiler"]
            .as_str()
            .map(|c| c.split(" (").next().unwrap_or(c).to_string())
            .unwrap_or_default(),
        committed: answer["committed"]
            .as_str()
            .unwrap_or("indexed")
            .to_string(),
        overlay,
        // Instantiations the compiler really made (bctx's "semantic" evidence).
        template_versions: stats["evidence"]["semantic"].as_u64(),
    }
}

/// The driver's answer as the index step's result.
pub fn index_result(answer: &serde_json::Value) -> (CheckResult, Option<IndexFacts>) {
    if answer["ok"].as_bool() != Some(true) {
        let error = answer["error"]
            .as_str()
            .unwrap_or("the index driver gave no answer");
        let fix = answer["fix"]
            .as_str()
            .unwrap_or("run `byteask company trial` again");
        let reason = if error.contains("uncommitted changes") {
            NotRunReason::Skipped
        } else if error.contains("no CMake target") || error.contains("compiles no") {
            NotRunReason::BadArguments
        } else {
            NotRunReason::ToolError
        };
        return (CheckResult::not_run("index", reason, error, fix), None);
    }
    let facts = facts_from(answer);
    let lines = match (facts.lines, facts.files) {
        (Some(lines), Some(files)) => format!("{lines} lines in {files} files"),
        _ => "no line count yet".to_string(),
    };
    let summary = format!(
        "{} translation unit(s), {lines}, at {} build setting",
        facts.translation_units,
        answer["setting"].as_str().unwrap_or("its")
    );
    let result = if facts.approximate_share > 0.0 {
        CheckResult::approximate(
            "index",
            &format!(
                "{summary}; {:.1}% of lines only approximately (each such answer says why)",
                facts.approximate_share * 100.0
            ),
        )
    } else {
        CheckResult::pass("index", &summary)
    };
    (result, Some(facts))
}

/// Run the index driver and read its one JSON line.
pub async fn index(
    home: &Path,
    installed: &Installed,
    python: &Path,
    root: &Path,
    built: &Built,
) -> IndexOutcome {
    let compdb = built
        .build_dir
        .join("byteask-trial")
        .join("compile_commands.json");
    let data = data_dir(home);
    let _ = std::fs::create_dir_all(data.join("tmp"));
    let workers = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 8))
        .unwrap_or(1);
    let mut command = tokio::process::Command::new(python);
    command
        .arg("-I")
        .arg(installed.dir.join(super::install::INDEX_DRIVER))
        .args(["--data"])
        .arg(&data)
        .args(["--partition", super::PARTITION, "--user"])
        .arg(super::local_user())
        .arg("--project-root")
        .arg(root)
        .arg("--project-name")
        .arg(project_name(root))
        .arg("--build-dir")
        .arg(&built.build_dir)
        .args([
            "--setting",
            &built.setting,
            "--target",
            &built.target,
            "--workers",
        ])
        .arg(workers.to_string())
        .env_clear()
        .envs(driver_env(home))
        .current_dir(&installed.dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let failed = |why: String| IndexOutcome {
        result: CheckResult::not_run(
            "index",
            NotRunReason::ToolError,
            &why,
            "run `byteask company trial` again; it reinstalls the context engine if needed",
        ),
        facts: None,
        compdb: compdb.clone(),
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return failed(format!("could not start the index: {err}")),
    };
    // The context engine's progress lines go to the terminal as they come.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                #[allow(clippy::print_stderr)]
                {
                    eprintln!("      {line}");
                }
            }
        });
    }
    let output = match tokio::time::timeout(INDEX_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return failed(format!("the index stopped: {err}")),
        Err(_) => {
            return IndexOutcome {
                result: CheckResult::not_run(
                    "index",
                    NotRunReason::TimeLimit,
                    "the index did not finish within 3 hours",
                    "pick a smaller target with --target, then run the trial again",
                ),
                facts: None,
                compdb,
            };
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(answer) = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
    else {
        return failed(format!(
            "the index gave no answer (exit {})",
            output.status.code().unwrap_or(-1)
        ));
    };
    let (result, facts) = index_result(&answer);
    IndexOutcome {
        result,
        facts,
        compdb: answer["compdb"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or(compdb),
    }
}

/// Record (or update) this project in the context engine record the context
/// tools read, so the CLI, Claude Code and Codex offer them in this project.
pub fn record_engine(
    home: &Path,
    installed: &Installed,
    python: &Path,
    root: &Path,
    built: &Built,
) -> std::io::Result<()> {
    use context_tools::engine;
    let mut projects = engine::read(home)
        .ok()
        .flatten()
        .map(|existing| existing.projects)
        .unwrap_or_default();
    projects.retain(|project| project.root != root);
    projects.push(engine::Project {
        root: root.to_path_buf(),
        partition: super::PARTITION.to_string(),
        setting: built.setting.clone(),
        target: built.target.clone(),
    });
    engine::write(
        home,
        &engine::EngineFile {
            schema: engine::SCHEMA,
            version: installed.version.clone(),
            install: installed.dir.clone(),
            python: python.to_path_buf(),
            data: data_dir(home),
            user: super::local_user(),
            projects,
        },
    )
}
