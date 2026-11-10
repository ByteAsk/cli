//! Configure the project with CMake in a build tree ByteAsk keeps outside the
//! project, choose the target, and build it.
//!
//! The build tree lives in the user's cache directory
//! (`~/.cache/byteask/trial/<project>/<build type>`), never in the project:
//! the trial must not leave files in the engineer's checkout. The CMake File
//! API query is written before configuring, so the context engine can read
//! the targets, their sources and their artifacts.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use check_result::CheckResult;
use check_result::NotRunReason;

use super::TrialArgs;
use super::which;

const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const FILE_API_QUERIES: [&str; 4] = ["codemodel-v2", "cache-v2", "toolchains-v1", "cmakeFiles-v1"];

/// One CMake target, from the File API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub kind: String,
    pub artifacts: Vec<PathBuf>,
    /// Names of the targets it depends on.
    pub deps: Vec<String>,
}

impl Target {
    fn is_executable(&self) -> bool {
        self.kind == "EXECUTABLE"
    }

    fn compiles_code(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "EXECUTABLE"
                | "STATIC_LIBRARY"
                | "SHARED_LIBRARY"
                | "MODULE_LIBRARY"
                | "OBJECT_LIBRARY"
        )
    }
}

/// What the build step leaves for the next steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    pub build_dir: PathBuf,
    pub target: String,
    /// The executable the sanitizer check runs, if there is one.
    pub run_target: Option<String>,
    /// The build setting's name in the index (the build type, lower case).
    pub setting: String,
}

/// `<cache>/byteask/trial/<project key>/<build type>`.
pub fn build_dir(
    root: &Path,
    build_type: &str,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let absolute = |value: std::ffi::OsString| {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    };
    let cache = if cfg!(target_os = "macos") {
        env("HOME")
            .and_then(absolute)
            .map(|home| home.join("Library").join("Caches"))
    } else {
        env("XDG_CACHE_HOME").and_then(absolute).or_else(|| {
            env("HOME")
                .and_then(absolute)
                .map(|home| home.join(".cache"))
        })
    }?;
    let setting: String = build_type
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    Some(
        cache
            .join("byteask")
            .join("trial")
            .join(results_store::project_key(root))
            .join(if setting.is_empty() {
                "default".to_string()
            } else {
                setting
            }),
    )
}

fn not_run(reason: NotRunReason, summary: &str, fix: &str) -> CheckResult {
    CheckResult::not_run("build", reason, summary, fix)
}

/// Run `program args` in `dir`, output to `log`, within `timeout`.
async fn run_logged(
    program: &Path,
    args: &[String],
    dir: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let file = std::fs::File::create(log)
        .map_err(|err| format!("could not write {}: {err}", log.display()))?;
    let err_file = file.try_clone().map_err(|err| err.to_string())?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(err_file)
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", program.display()))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("exited with {status}")),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: signalling the process group we created.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            Err(format!(
                "did not finish within {} minutes",
                timeout.as_secs() / 60
            ))
        }
    }
}

fn log_tail(log: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// The targets of the newest File API reply.
pub fn read_targets(build_dir: &Path) -> Result<Vec<Target>, String> {
    let reply = build_dir
        .join(".cmake")
        .join("api")
        .join("v1")
        .join("reply");
    let mut indexes: Vec<PathBuf> = std::fs::read_dir(&reply)
        .map_err(|_| "CMake wrote no File API reply".to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("index-") && name.ends_with(".json"))
        })
        .collect();
    indexes.sort();
    let index = indexes.pop().ok_or("CMake wrote no File API reply")?;
    let read = |path: &Path| -> Result<serde_json::Value, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
    };
    let index = read(&index)?;
    let codemodel = index["objects"]
        .as_array()
        .and_then(|objects| objects.iter().find(|object| object["kind"] == "codemodel"))
        .and_then(|object| object["jsonFile"].as_str())
        .ok_or("the File API reply has no code model")?;
    let codemodel = read(&reply.join(codemodel))?;
    let build_root = PathBuf::from(codemodel["paths"]["build"].as_str().unwrap_or_default());
    let entries = codemodel["configurations"][0]["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names: std::collections::BTreeMap<String, String> = entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry["id"].as_str()?.to_string(),
                entry["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    let mut targets = Vec::new();
    for entry in &entries {
        let Some(file) = entry["jsonFile"].as_str() else {
            continue;
        };
        let target = read(&reply.join(file))?;
        targets.push(Target {
            name: target["name"].as_str().unwrap_or_default().to_string(),
            kind: target["type"].as_str().unwrap_or_default().to_string(),
            artifacts: target["artifacts"]
                .as_array()
                .map(|artifacts| {
                    artifacts
                        .iter()
                        .filter_map(|artifact| artifact["path"].as_str())
                        .map(|path| build_root.join(path))
                        .collect()
                })
                .unwrap_or_default(),
            deps: target["dependencies"]
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep["id"].as_str())
                        .filter_map(|id| names.get(id).cloned())
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(targets)
}

/// Executables CTest runs (`ctest --show-only=json-v1`), as target names.
async fn test_targets(ctest: Option<&Path>, build_dir: &Path, targets: &[Target]) -> Vec<String> {
    let Some(ctest) = ctest else {
        return Vec::new();
    };
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(ctest)
            .arg("--show-only=json-v1")
            .current_dir(build_dir)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    else {
        return Vec::new();
    };
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    tests_in(&listed, targets)
}

/// Executable targets CTest runs, from `ctest --show-only=json-v1`: matched by the
/// test's command path, or, when CTest gives no command (the executable is not built
/// yet), by the test's name.
pub fn tests_in(listed: &serde_json::Value, targets: &[Target]) -> Vec<String> {
    let tests = listed["tests"].as_array().cloned().unwrap_or_default();
    let commands: Vec<PathBuf> = tests
        .iter()
        .filter_map(|test| test["command"][0].as_str())
        .map(PathBuf::from)
        .collect();
    let unresolved: Vec<&str> = tests
        .iter()
        .filter(|test| test["command"][0].as_str().is_none())
        .filter_map(|test| test["name"].as_str())
        .collect();
    let mut names: Vec<String> = targets
        .iter()
        .filter(|target| target.is_executable())
        .filter(|target| {
            target.artifacts.iter().any(|artifact| commands.contains(artifact))
                || unresolved.contains(&target.name.as_str())
        })
        .map(|target| target.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn depends_on(targets: &[Target], from: &str, wanted: &str) -> bool {
    let mut todo = vec![from.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(name) = todo.pop() {
        if name == wanted {
            return true;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(target) = targets.iter().find(|target| target.name == name) {
            todo.extend(target.deps.iter().cloned());
        }
    }
    false
}

/// The target to index and check. `Err` = a one-line problem and its fix.
pub fn choose_target(
    targets: &[Target],
    tests: &[String],
    wanted: Option<&str>,
) -> Result<String, (String, String)> {
    let candidates: Vec<&Target> = targets
        .iter()
        .filter(|target| target.compiles_code())
        .collect();
    let list = || {
        let names: Vec<&str> = candidates
            .iter()
            .map(|target| target.name.as_str())
            .take(20)
            .collect();
        format!("pass --target with one of: {}", names.join(", "))
    };
    if let Some(wanted) = wanted {
        return if candidates.iter().any(|target| target.name == wanted) {
            Ok(wanted.to_string())
        } else {
            Err((
                format!("the project has no target named '{wanted}' that compiles code"),
                list(),
            ))
        };
    }
    if candidates.is_empty() {
        return Err((
            "the project defines no target that compiles code".to_string(),
            "check that CMake configures the project you meant, then run the trial again"
                .to_string(),
        ));
    }
    if tests.len() == 1 {
        return Ok(tests[0].clone());
    }
    if candidates.len() == 1 {
        return Ok(candidates[0].name.clone());
    }
    Err((
        format!(
            "the project has {} targets; the trial indexes one",
            candidates.len()
        ),
        list(),
    ))
}

/// The executable the sanitizer check runs.
pub fn choose_run_target(
    targets: &[Target],
    tests: &[String],
    target: &str,
    wanted: Option<&str>,
) -> Result<Option<String>, (String, String)> {
    let executables = || {
        let names: Vec<&str> = targets
            .iter()
            .filter(|target| target.is_executable())
            .map(|target| target.name.as_str())
            .take(20)
            .collect();
        format!("pass --run-target with one of: {}", names.join(", "))
    };
    if let Some(wanted) = wanted {
        return if targets
            .iter()
            .any(|t| t.name == wanted && t.is_executable())
        {
            Ok(Some(wanted.to_string()))
        } else {
            Err((
                format!("'{wanted}' is not an executable target"),
                executables(),
            ))
        };
    }
    if targets
        .iter()
        .any(|t| t.name == target && t.is_executable())
    {
        return Ok(Some(target.to_string()));
    }
    Ok(tests
        .iter()
        .find(|test| depends_on(targets, test, target))
        .cloned())
}

pub async fn configure_and_build(root: &Path, args: &TrialArgs) -> Result<Built, CheckResult> {
    let Some(cmake) = which("cmake", None) else {
        return Err(not_run(
            NotRunReason::ToolMissing,
            "cmake is not installed",
            "install CMake 3.20 or newer, then run `byteask company trial` again",
        ));
    };
    let Some(dir) = build_dir(root, &args.build_type, |key| std::env::var_os(key)) else {
        return Err(not_run(
            NotRunReason::ToolError,
            "found no cache directory for the build tree (HOME is not set)",
            "set HOME (or XDG_CACHE_HOME), then run `byteask company trial` again",
        ));
    };
    let query = dir.join(".cmake").join("api").join("v1").join("query");
    if let Err(err) = std::fs::create_dir_all(&query) {
        return Err(not_run(
            NotRunReason::ToolError,
            &format!("could not create the build tree {}: {err}", dir.display()),
            "free some disk space or fix the permissions, then run the trial again",
        ));
    }
    for name in FILE_API_QUERIES {
        let _ = std::fs::write(query.join(name), "");
    }
    let logs = dir.join("byteask-trial");
    let _ = std::fs::create_dir_all(&logs);

    let mut configure: Vec<String> = vec![
        "-S".to_string(),
        root.to_string_lossy().into_owned(),
        "-B".to_string(),
        dir.to_string_lossy().into_owned(),
        format!("-DCMAKE_BUILD_TYPE={}", args.build_type),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string(),
    ];
    // A tree configured once keeps its generator (CMake refuses to switch).
    if !dir.join("CMakeCache.txt").exists() && which("ninja", None).is_some() {
        configure.extend(["-G".to_string(), "Ninja".to_string()]);
    }
    configure.extend(args.cmake_args.iter().cloned());
    let log = logs.join("configure.log");
    if let Err(why) = run_logged(&cmake, &configure, root, &log, CONFIGURE_TIMEOUT).await {
        return Err(not_run(
            NotRunReason::BuildFailed,
            &format!("CMake could not configure the project ({why})"),
            &format!(
                "see {}; fix the configure (or pass --cmake-arg), then run the trial again",
                log.display()
            ),
        )
        .with_evidence(log_tail(&log, 5)));
    }
    let targets = match read_targets(&dir) {
        Ok(targets) => targets,
        Err(why) => {
            return Err(not_run(
                NotRunReason::ToolError,
                &why,
                "use CMake 3.20 or newer, then run the trial again",
            ));
        }
    };
    let tests = test_targets(which("ctest", None).as_deref(), &dir, &targets).await;
    let target = choose_target(&targets, &tests, args.target.as_deref())
        .map_err(|(problem, fix)| not_run(NotRunReason::BadArguments, &problem, &fix))?;
    let run_target = choose_run_target(&targets, &tests, &target, args.run_target.as_deref())
        .map_err(|(problem, fix)| not_run(NotRunReason::BadArguments, &problem, &fix))?;

    let jobs = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(2);
    let mut build: Vec<String> = vec![
        "--build".to_string(),
        dir.to_string_lossy().into_owned(),
        "--target".to_string(),
        target.clone(),
    ];
    if let Some(run) = run_target.as_ref().filter(|run| **run != target) {
        build.extend(["--target".to_string(), run.clone()]);
    }
    build.extend(["-j".to_string(), jobs.to_string()]);
    let log = logs.join("build.log");
    if let Err(why) = run_logged(&cmake, &build, root, &log, BUILD_TIMEOUT).await {
        return Err(not_run(
            NotRunReason::BuildFailed,
            &format!("the target did not build ({why})"),
            &format!(
                "see {}; fix the build, then run the trial again",
                log.display()
            ),
        )
        .with_evidence(log_tail(&log, 5)));
    }
    Ok(Built {
        build_dir: dir,
        setting: args.build_type.to_ascii_lowercase(),
        target,
        run_target,
    })
}
