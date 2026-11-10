use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use check_result::CheckResult;
use check_result::NotRunReason;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;

use crate::run::RunOutcome;
use crate::run::compile_timeout_from;
use crate::run::run_capped;

const TOOL_NAME: &str = "cpp_compile_db";
/// Directories (relative to cwd) searched for an existing compile database.
const SEARCH_DIRS: &[&str] = &[".", "build", "out"];
const COMPILE_DB: &str = "compile_commands.json";
/// Cap on how much cmake/make stderr we echo back to the model on failure.
const STDERR_CAP: usize = 2000;

pub struct CompileDbTool {
    pub cwd: PathBuf,
    /// The Makefile generation path runs the project's own build
    /// (`bear -- make`) as a direct child of the engine, i.e. OUTSIDE the
    /// filesystem sandbox. Detection stays ungated; that path does not.
    /// Resolved once at thread start; see [`crate::trust`].
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompileDbArgs {
    #[serde(default)]
    generate: bool,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for CompileDbTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "generate".to_string(),
            JsonSchema::boolean(Some(
                "When true and no compile database is found, attempt to generate one: CMake \
                 projects are configured with -DCMAKE_EXPORT_COMPILE_COMMANDS=ON; a Makefile-only \
                 project is built once under `bear -- make`, which RUNS the project's real build \
                 and so requires a trusted project. Defaults to false."
                    .to_string(),
            )),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some(
                "Wall-clock cap on the `bear -- make` build (default 600s). Raise it for a large \
                 project whose full build is slow. It also caps a CMake configure."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Detect or generate compile_commands.json (the compile database that \
                powers clang-tidy, clangd, and sanitizer project builds). With generate=true it \
                handles both CMake (cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON) and Makefile-only \
                projects (bear -- make). C/C++ projects."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                None,
                Some(AdditionalProperties::from(false)),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl CompileDbTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: CompileDbArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        // 1. Detection: is there already a compile database nearby?
        if let Some(found) = find_compile_db(&self.cwd) {
            let count = count_entries(&found);
            let text = match count {
                Some(n) => format!("Found compile DB at {} ({n} entries)", found.display()),
                None => format!(
                    "Found compile DB at {} (could not parse it as a JSON array)",
                    found.display()
                ),
            };
            let result = match count {
                Some(n) => CheckResult::pass(
                    TOOL_NAME,
                    &format!("Found a compile database with {n} entries."),
                ),
                None => CheckResult::fail(
                    TOOL_NAME,
                    "A compile database exists but is not a valid JSON array.",
                ),
            };
            return Ok(crate::report::report_plain(TOOL_NAME, text, result));
        }

        // 2. Not found, generation not requested: explain how to make one.
        if !args.generate {
            return Ok(crate::report::report_plain(
                TOOL_NAME,
                no_db_guidance(),
                CheckResult::fail(
                    TOOL_NAME,
                    "No compile_commands.json was found; call cpp_compile_db with generate: true to make one.",
                ),
            ));
        }

        // 3. Not found, generation requested.
        self.generate(args.timeout_sec).await
    }

    async fn generate(
        &self,
        timeout_sec: Option<u64>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        // Generation writes into the project (build/, the copied DB);
        // concurrent calls on one project break each other's configure. The
        // lock holds across sessions, not just inside this process.
        let _project =
            match crate::builds::project_lock(&self.cwd, compile_timeout_from(timeout_sec)).await {
                Ok(guard) => guard,
                Err(busy) => {
                    return Ok(crate::report::report_plain(
                        TOOL_NAME,
                        busy.summary(),
                        CheckResult::not_run(
                            TOOL_NAME,
                            NotRunReason::TimeLimit,
                            &busy.summary(),
                            crate::builds::ProjectBusy::FIX,
                        ),
                    ));
                }
            };
        if self.cwd.join("CMakeLists.txt").is_file() {
            return self.generate_cmake(timeout_sec).await;
        }
        // make reads GNUmakefile, makefile and Makefile, in that order.
        if ["GNUmakefile", "makefile", "Makefile"]
            .iter()
            .any(|name| self.cwd.join(name).is_file())
        {
            return self.generate_bear(timeout_sec).await;
        }
        Ok(crate::report::report_plain(
            TOOL_NAME,
            "No CMakeLists.txt/Makefile found; cannot auto-generate".to_string(),
            CheckResult::not_run(
                TOOL_NAME,
                NotRunReason::NoBuildProfile,
                "There is no CMakeLists.txt or Makefile to generate a compile database from.",
                "generate compile_commands.json with your build system (for example `bear -- <your build command>`), then call cpp_compile_db again.",
            ),
        ))
    }

    /// The trust gate for generation, which executes the project's own
    /// build scripts either way (a Makefile's recipes, or CMake code such as
    /// `execute_process`). Separate so a test can prove the wiring, not just
    /// that `crate::trust` can say no.
    fn gate(&self) -> Result<(), FunctionCallError> {
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)
    }

    /// Makefile-only project: build once under Bear to record the real compile
    /// lines.
    ///
    /// Until this existed the tool merely *printed* `run bear -- make`, which
    /// left every Makefile-only C project without include paths and so starved
    /// `cpp_tidy` and `cpp_nav` — measured, and the single most
    /// common non-CMake layout.
    ///
    /// This runs arbitrary recipes as a direct child of the engine, outside the
    /// filesystem sandbox, so it is gated exactly like `cpp_sanitize` (as is the
    /// CMake configure, see [`Self::generate_cmake`]). Detection stays ungated.
    async fn generate_bear(
        &self,
        timeout_sec: Option<u64>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        // Refuse before we spawn anything, as cpp_sanitize does.
        self.gate()?;

        let bear = crate::probe::probe("bear", &["--version"], &self.cwd).await;
        if !bear.is_available() {
            let detail = bear
                .describe()
                .unwrap_or_else(|| "bear is unavailable".to_string());
            return Ok(crate::report::report_plain(
                TOOL_NAME,
                bear_unavailable(&detail),
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::ToolMissing,
                    "bear is needed to record a Makefile build and is not available.",
                    "install bear (for example `apt install bear`), then call cpp_compile_db again.",
                ),
            ));
        }

        let scratch = scratch_dir(&self.cwd);
        std::fs::create_dir_all(&scratch).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "could not create the scratch directory {}: {err}; nothing was generated.",
                scratch.display()
            ))
        })?;
        let dest = scratch.join(COMPILE_DB);
        // Bear writes from scratch; a stale file would read as this run's.
        let _ = std::fs::remove_file(&dest);
        let version = crate::toolver::tool_version("bear", &self.cwd).await;
        let timeout = compile_timeout_from(timeout_sec);
        match run_capped("bear", &bear_args(version, &dest), &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => Ok(crate::report::report_plain(
                TOOL_NAME,
                format!(
                    "`bear -- make` was killed after {secs}s and the whole process group torn \
                     down. A full build can outrun the default cap — re-run with a larger \
                     `timeout_sec`."
                ),
                crate::results::timed_out(
                    TOOL_NAME,
                    "The recorded build",
                    secs,
                    "call cpp_compile_db again with a larger `timeout_sec`.",
                ),
            )),
            RunOutcome::Finished {
                success, stderr, ..
            } => {
                // Bear writes the database from whatever it observed, even when
                // make later fails, so ask the filesystem before trusting the
                // exit status: a partial DB beats none for clang-tidy/clangd.
                if !dest.is_file() {
                    let stderr = truncate(stderr.trim(), STDERR_CAP);
                    return Ok(crate::report::report_plain(
                        TOOL_NAME,
                        format!(
                            "`bear -- make` ran but produced no {COMPILE_DB}. Build stderr:\n\
                             {stderr}"
                        ),
                        CheckResult::not_run(
                            TOOL_NAME,
                            NotRunReason::BuildFailed,
                            "The recorded build produced no compile database.",
                            "fix the build error above so `make` compiles, then call cpp_compile_db again.",
                        ),
                    ));
                }
                let entries = count_entries(&dest);
                let mut text = match entries {
                    Some(n) => format!(
                        "Generated compile DB with `bear -- make` at {} ({n} entries)",
                        dest.display()
                    ),
                    None => format!(
                        "Generated compile DB with `bear -- make` at {} (could not parse entry \
                         count)",
                        dest.display()
                    ),
                };
                // Both notes can be true at once (a build that failed before it
                // compiled anything writes an empty database), and the exit
                // status is the more important of the two — so they are
                // independent, not an either/or. Without the stderr, "0 entries,
                // run make clean" would hide "make: command not found".
                if !success {
                    let stderr = truncate(stderr.trim(), STDERR_CAP);
                    text.push_str(&format!(
                        "\nNote: make itself exited non-zero, so the database covers only what \
                         compiled before the failure. Build stderr:\n{stderr}"
                    ));
                }
                if entries == Some(0) {
                    // Bear can only record compilations it actually observes.
                    text.push_str(
                        "\nThe database is EMPTY: bear saw no compiler invocations, so make had \
                         nothing to rebuild (or never got as far as compiling). Run `make clean` \
                         (or delete the objects) and call this tool again.",
                    );
                }
                let result = if entries == Some(0) {
                    CheckResult::fail(TOOL_NAME, "The generated compile database is empty.")
                } else if !success {
                    CheckResult::approximate(
                        TOOL_NAME,
                        "make failed part-way, so the database covers only what compiled before the failure.",
                    )
                } else {
                    CheckResult::pass(
                        TOOL_NAME,
                        "Generated a compile database with `bear -- make`.",
                    )
                };
                Ok(crate::report::report_plain(TOOL_NAME, text, result))
            }
        }
    }

    /// CMake project: configure an out-of-tree `build/` with
    /// `CMAKE_EXPORT_COMPILE_COMMANDS=ON`.
    ///
    /// "Only configuring" still runs the project's CMake code, and CMake
    /// code can run anything (`execute_process`): measured, a CMakeLists that
    /// touched a marker and slept 12 s did both, ungated, while
    /// `timeout_sec: 2` was ignored (a raw `.output()` with no cap). Gated like
    /// the Makefile path and run through `run_capped`.
    async fn generate_cmake(
        &self,
        timeout_sec: Option<u64>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        self.gate()?;

        let build = scratch_dir(&self.cwd).join("build");
        let timeout = compile_timeout_from(timeout_sec);
        match run_capped("cmake", &cmake_argv(&build), &self.cwd, timeout).await? {
            RunOutcome::TimedOut { secs } => {
                return Ok(crate::report::report_plain(
                    TOOL_NAME,
                    format!(
                        "the CMake configure was killed after {secs}s and its process group torn \
                         down; no compile database was generated. Re-run with a larger \
                         `timeout_sec` if the configure is legitimately slow."
                    ),
                    crate::results::timed_out(
                        TOOL_NAME,
                        "The CMake configure",
                        secs,
                        "call cpp_compile_db again with a larger `timeout_sec`.",
                    ),
                ));
            }
            RunOutcome::Finished {
                success: false,
                stderr,
                ..
            } => {
                let stderr = truncate(stderr.trim(), STDERR_CAP);
                return Ok(crate::report::report_plain(
                    TOOL_NAME,
                    format!("cmake failed to generate a compile database. stderr:\n{stderr}"),
                    CheckResult::not_run(
                        TOOL_NAME,
                        NotRunReason::BuildFailed,
                        "The CMake configure failed, so no compile database was generated.",
                        "fix the CMake error above, then call cpp_compile_db again.",
                    ),
                ));
            }
            RunOutcome::Finished { .. } => {}
        }

        let generated = build.join(COMPILE_DB);
        if !generated.is_file() {
            return Ok(crate::report::report_plain(
                TOOL_NAME,
                format!(
                    "cmake succeeded but {} was not produced. Ensure your CMake version supports \
                     CMAKE_EXPORT_COMPILE_COMMANDS.",
                    generated.display()
                ),
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::ToolError,
                    "CMake ran but wrote no compile database.",
                    "use a CMake that supports CMAKE_EXPORT_COMPILE_COMMANDS (3.5 or later), then call cpp_compile_db again.",
                ),
            ));
        }

        // Nothing is copied into the project: the other tools look in the
        // scratch directory too (see `find_compile_db`).
        let entries = count_entries(&generated);
        let text = match entries {
            Some(n) => format!(
                "Generated compile DB at {} ({n} entries), configured outside the project; the \
                 project's own files and build directories were not touched. cpp_tidy, cpp_check, \
                 cpp_nav and cpp_sanitize use it automatically (for clang-tidy/clangd in the \
                 shell: -p / --compile-commands-dir={}).",
                generated.display(),
                build.display()
            ),
            None => format!(
                "Generated compile DB at {} (could not parse entry count)",
                generated.display()
            ),
        };
        let result = match entries {
            Some(0) => CheckResult::fail(TOOL_NAME, "The generated compile database is empty."),
            Some(n) => CheckResult::pass(
                TOOL_NAME,
                &format!("Generated a compile database with {n} entries."),
            ),
            None => CheckResult::fail(
                TOOL_NAME,
                "The generated compile database is not a valid JSON array.",
            ),
        };
        Ok(crate::report::report_plain(TOOL_NAME, text, result))
    }
}

/// Search `cwd`, `cwd/build`, `cwd/out`, then any other build directory at
/// the top of the project (`build-*`, `cmake-build-*`, `builddir`, `_build`),
/// then the scratch directory this tool generates into, for a
/// `compile_commands.json`. Every hit is only ever READ.
///
/// Shared with `cpp_sanitize`, `cpp_check`, `cpp_tidy`, `cpp_nav`
/// and `cpp_layout` so all tools agree on where a compile database lives —
/// this tool's own description promises it "powers clang-tidy, clangd, and
/// sanitizer project builds".
pub(crate) fn find_compile_db(cwd: &Path) -> Option<PathBuf> {
    SEARCH_DIRS
        .iter()
        .map(|dir| cwd.join(dir).join(COMPILE_DB))
        .chain(
            other_build_dirs(cwd)
                .into_iter()
                .map(|dir| dir.join(COMPILE_DB)),
        )
        .chain([
            scratch_dir(cwd).join(COMPILE_DB),
            scratch_dir(cwd).join("build").join(COMPILE_DB),
        ])
        .find(|candidate| candidate.is_file())
}

/// Top-level directories named like a build tree, sorted, excluding the
/// `SEARCH_DIRS` already tried.
fn other_build_dirs(cwd: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.starts_with("build") || name.starts_with("cmake-build") || name == "_build")
                && name != "build"
                && entry.path().is_dir()
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// Where this tool generates a compile database for the project at `cwd`:
/// `<system temp>/byteask-compiledb-<hash of the project path>`, outside the
/// source tree, so generation never reconfigures the project's own build/
/// and never drops files into the repository.
pub(crate) fn scratch_dir(cwd: &Path) -> PathBuf {
    let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // FNV-1a: stable across runs and Rust versions (DefaultHasher is not).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    std::env::temp_dir().join(format!("byteask-compiledb-{hash:016x}"))
}

/// Parse a compile database as a JSON array and return its length.
fn count_entries(path: &Path) -> Option<usize> {
    let contents = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value.as_array().map(|entries| entries.len())
}

/// `cmake_args` for this machine.
fn cmake_argv(build: &Path) -> Vec<String> {
    cmake_args(build, ninja_on_path())
}

/// Build the `cmake` argument vector. Always configures `build` — a scratch
/// directory outside the project, never the project's own `build/` — with
/// `CMAKE_EXPORT_COMPILE_COMMANDS=ON` (written at configure time, nothing is
/// built); appends `-G Ninja` only when the Ninja generator is on PATH.
fn cmake_args(build: &Path, with_ninja: bool) -> Vec<String> {
    let mut args = vec![
        "-S".to_string(),
        ".".to_string(),
        "-B".to_string(),
        build.display().to_string(),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string(),
    ];
    if with_ninja {
        args.push("-G".to_string());
        args.push("Ninja".to_string());
    }
    args
}

/// Bear intercepts the compiler invocations of an ordinary `make` run and
/// writes them as a compile database.
///
/// Deliberately no user-supplied make targets or flags: this tool's contract is
/// "produce a compile database", and a free-form argument would turn a
/// trust-gated build step into an arbitrary command runner.
///
/// The database goes to `output` (the scratch directory), in the syntax of the
/// installed Bear: 3.x `--output <file> -- make`; 2.x `-o <file> make` (2.x
/// has no `--` form). An unrecognised version gets the 3.x form.
fn bear_args(version: Option<crate::toolver::Version>, output: &Path) -> Vec<String> {
    let output = output.display().to_string();
    if version.is_some_and(|v| v < (3, 0)) {
        return vec!["-o".to_string(), output, "make".to_string()];
    }
    vec![
        "--output".to_string(),
        output,
        "--".to_string(),
        "make".to_string(),
    ]
}

/// Message when Bear itself cannot run. Keeps the manual advice the old
/// print-only branch gave, so the model is never left at a dead end.
fn bear_unavailable(detail: &str) -> String {
    format!(
        "Makefile project, but bear could not be used here ({detail}), so \
         {COMPILE_DB} cannot be generated automatically.\n\
         Install Bear and call this tool again, or run the build yourself:\n\
         \x20     bear -- make\n\
         {}",
        crate::probe::install_hint("bear")
    )
}

fn ninja_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join("ninja").is_file() || dir.join("ninja.exe").is_file())
}

fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        text.to_string()
    } else {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(truncated)", &text[..end])
    }
}

fn no_db_guidance() -> String {
    "No compile_commands.json found in the workspace (looked in ., build/, out/, other \
     build*/ directories, and this tool's scratch directory).\n\n\
     To create one:\n\
     - CMake: configure with -DCMAKE_EXPORT_COMPILE_COMMANDS=ON into a directory of your \
     own (e.g. `cmake -S . -B /tmp/cdb -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`; configuring does \
     not build), then point tools at it (clang-tidy -p, clangd --compile-commands-dir).\n\
     - Non-CMake (Make, etc.): wrap the build with Bear, e.g. `bear -- make`.\n\n\
     Or call this tool again with generate=true and it will do whichever of those two applies, \
     writing the database outside the project (the Bear path runs the project's real build, \
     so it needs a trusted project).\n\n\
     A compile database lets clang-tidy, clangd, and sanitizer project builds see the exact \
     compiler flags for every file. Single-file tools still work without it."
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use codex_extension_api::ToolPayload;

    use super::*;

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "cpp_compile_db_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Serialize a tool output to its response item and return it as a string so
    /// tests can assert on the body text (mirrors the report.rs test approach).
    fn output_text(out: &dyn ToolOutput) -> String {
        let item = out.to_response_item(
            "call-test",
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );
        serde_json::to_string(&item).expect("serialize response item")
    }

    /// Point the JSONL log at a throwaway file so `report_plain` never writes to
    /// a real home directory during async-branch tests.
    fn isolate_log(dir: &Path) {
        // SAFETY: tests are single-threaded per process for env access here; the
        // path is unique per temp dir so concurrent tests don't collide on data.
        unsafe { std::env::set_var("BYTEASK_TOOL_LOG", dir.join("tool-calls.jsonl")) };
    }

    #[test]
    fn detects_db_in_cwd() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, "[]").unwrap();

        let found = find_compile_db(&dir).expect("should find db in cwd");
        assert_eq!(found, db);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_db_in_build_subdir() {
        let dir = temp_dir();
        let build = dir.join("build");
        fs::create_dir_all(&build).unwrap();
        let db = build.join(COMPILE_DB);
        fs::write(&db, r#"[{"file":"a.cpp"}]"#).unwrap();

        let found = find_compile_db(&dir).expect("should find db in build/");
        assert_eq!(found, db);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_db_in_out_subdir() {
        let dir = temp_dir();
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        let db = out.join(COMPILE_DB);
        fs::write(&db, "[]").unwrap();

        let found = find_compile_db(&dir).expect("should find db in out/");
        assert_eq!(found, db);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cwd_takes_precedence_over_subdirs() {
        // SEARCH_DIRS order is [".", "build", "out"]; a cwd db wins.
        let dir = temp_dir();
        fs::create_dir_all(dir.join("build")).unwrap();
        fs::write(dir.join("build").join(COMPILE_DB), "[]").unwrap();
        let cwd_db = dir.join(COMPILE_DB);
        fs::write(&cwd_db, "[]").unwrap();

        assert_eq!(find_compile_db(&dir), Some(cwd_db));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn returns_none_when_absent() {
        let dir = temp_dir();
        assert_eq!(find_compile_db(&dir), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counts_entries_of_json_array() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, r#"[{"file":"a.cpp"},{"file":"b.cpp"}]"#).unwrap();

        assert_eq!(count_entries(&db), Some(2));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counts_entries_of_larger_array() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, r#"[1,2,3,4,5]"#).unwrap();

        assert_eq!(count_entries(&db), Some(5));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_entries_empty_array_is_zero() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, "[]").unwrap();

        assert_eq!(count_entries(&db), Some(0));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_entries_rejects_non_array() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, r#"{"not":"an array"}"#).unwrap();

        assert_eq!(count_entries(&db), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_entries_handles_malformed_json() {
        let dir = temp_dir();
        let db = dir.join(COMPILE_DB);
        fs::write(&db, "[ this is not valid json").unwrap();

        assert_eq!(count_entries(&db), None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_entries_none_for_missing_file() {
        let dir = temp_dir();
        assert_eq!(count_entries(&dir.join(COMPILE_DB)), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_db_guidance_mentions_cmake_and_bear() {
        let text = no_db_guidance();
        assert!(
            text.contains("CMake"),
            "guidance should mention CMake: {text}"
        );
        assert!(
            text.contains("CMAKE_EXPORT_COMPILE_COMMANDS"),
            "guidance should give the cmake flag: {text}"
        );
        assert!(
            text.contains("Bear"),
            "guidance should mention Bear: {text}"
        );
        assert!(
            text.contains("bear -- make"),
            "guidance should show the bear invocation: {text}"
        );
    }

    #[test]
    fn no_db_guidance_lists_search_dirs() {
        let text = no_db_guidance();
        assert!(
            text.contains('.') && text.contains("build/") && text.contains("out/"),
            "guidance should list the searched dirs: {text}"
        );
    }

    #[test]
    fn cmake_args_without_ninja_omits_generator() {
        let args = cmake_args(Path::new("/tmp/scratch/build"), false);
        assert_eq!(
            args,
            vec![
                "-S",
                ".",
                "-B",
                "/tmp/scratch/build",
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"
            ]
        );
        assert!(!args.contains(&"-G".to_string()), "no -G without ninja");
    }

    #[test]
    fn cmake_args_with_ninja_appends_generator() {
        let args = cmake_args(Path::new("/tmp/scratch/build"), true);
        assert_eq!(
            args,
            vec![
                "-S",
                ".",
                "-B",
                "/tmp/scratch/build",
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
                "-G",
                "Ninja",
            ]
        );
    }

    #[test]
    fn generation_never_targets_the_project_tree() {
        // The old argv configured `<repo>/build` (a directory the project, its
        // IDE or its CI may own) and copied the DB into the repo root.
        let dir = temp_dir();
        let scratch = scratch_dir(&dir);
        assert!(
            scratch.starts_with(std::env::temp_dir()),
            "{}",
            scratch.display()
        );
        assert!(!scratch.starts_with(&dir), "{}", scratch.display());
        assert_eq!(scratch, scratch_dir(&dir), "stable per project");
        assert_ne!(
            scratch,
            scratch_dir(&dir.join("other")),
            "distinct per project"
        );
        let args = cmake_argv(&scratch.join("build"));
        assert_eq!(args[3], scratch.join("build").display().to_string());
        assert!(!args.contains(&"build".to_string()), "{args:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_db_in_any_build_dir_or_the_scratch_dir_is_reused() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("cmake-build-debug")).unwrap();
        fs::write(dir.join("cmake-build-debug").join(COMPILE_DB), "[]").unwrap();
        assert_eq!(
            find_compile_db(&dir),
            Some(dir.join("cmake-build-debug").join(COMPILE_DB))
        );
        fs::remove_dir_all(dir.join("cmake-build-debug")).unwrap();
        // A database this tool generated earlier.
        let scratch = scratch_dir(&dir).join("build");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join(COMPILE_DB), "[]").unwrap();
        assert_eq!(find_compile_db(&dir), Some(scratch.join(COMPILE_DB)));
        // An in-tree database still wins.
        fs::write(dir.join(COMPILE_DB), "[]").unwrap();
        assert_eq!(find_compile_db(&dir), Some(dir.join(COMPILE_DB)));
        fs::remove_dir_all(scratch_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_arg_defaults_to_false() {
        let args: CompileDbArgs = serde_json::from_str("{}").unwrap();
        assert!(!args.generate);
    }

    #[test]
    fn generate_arg_parses_true() {
        let args: CompileDbArgs = serde_json::from_str(r#"{"generate":true}"#).unwrap();
        assert!(args.generate);
        assert!(args.timeout_sec.is_none());
    }

    #[test]
    fn timeout_sec_arg_parses_and_reaches_the_compile_cap() {
        let args: CompileDbArgs =
            serde_json::from_str(r#"{"generate":true,"timeout_sec":1800}"#).unwrap();
        assert_eq!(args.timeout_sec, Some(1800));
        assert_eq!(compile_timeout_from(args.timeout_sec).as_secs(), 1800);
        // Unset falls back to the shared compile default, not the run default.
        assert_eq!(compile_timeout_from(None).as_secs(), 600);
    }

    #[test]
    fn bear_argv_runs_make_under_bear_with_no_extra_args() {
        // FAILS ON OLD CODE: there was no bear argv at all — the Makefile branch
        // only printed a hint. The `--` separator is what makes bear treat the
        // rest as the build command rather than its own options.
        let out = Path::new("/tmp/s/compile_commands.json");
        assert_eq!(
            bear_args(None, out),
            vec!["--output", "/tmp/s/compile_commands.json", "--", "make"]
        );
        assert_eq!(bear_args(Some((3, 1)), out), bear_args(None, out));
        // Bear 2.x: no `--` form, `-o <file>`.
        assert_eq!(
            bear_args(Some((2, 4)), out),
            vec!["-o", "/tmp/s/compile_commands.json", "make"]
        );
    }

    #[test]
    fn bear_unavailable_keeps_the_manual_advice_and_names_the_reason() {
        // FAILS ON OLD CODE: `bear_unavailable` did not exist.
        let text = bear_unavailable("bear not installed (install bear and ensure it is on PATH)");
        assert!(
            text.contains("bear -- make"),
            "keeps the manual advice: {text}"
        );
        assert!(text.contains("not installed"), "names the reason: {text}");
        assert!(
            text.contains(COMPILE_DB),
            "names what could not be produced: {text}"
        );
    }

    #[tokio::test]
    async fn makefile_generation_is_trust_gated() {
        // FAILS ON OLD CODE: the Makefile branch unconditionally returned
        // Ok(<bear hint>), so there was nothing to refuse. `bear -- make` runs
        // the project's real build outside the sandbox, so it must be gated
        // exactly like cpp_sanitize.
        let dir = temp_dir();
        isolate_log(&dir);
        fs::write(dir.join("Makefile"), "all:\n\techo hi\n").unwrap();

        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: false,
        };
        let Err(err) = tool.generate(None).await else {
            panic!("an untrusted project must be refused, not built");
        };
        let FunctionCallError::RespondToModel(msg) = err else {
            panic!("the refusal must be model-facing, not fatal");
        };
        assert!(
            msg.contains(crate::trust::ALLOW_ENV),
            "refusal must name the escape hatch: {msg}"
        );
        assert!(msg.contains(TOOL_NAME), "refusal must name the tool: {msg}");
        // Nothing was executed: bear would have written the database here.
        assert!(!dir.join(COMPILE_DB).exists(), "must not run the build");

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn makefile_generation_actually_runs_bear_when_trusted() {
        // FAILS ON OLD CODE: the old branch returned the literal string
        // "Makefile project — run `bear -- make` to generate
        // compile_commands.json", which contains NEITHER marker asserted below,
        // and never wrote a database.
        let dir = temp_dir();
        isolate_log(&dir);
        // A no-op recipe: bear still writes an (empty) database, which is the
        // "nothing to rebuild" case the message calls out.
        fs::write(dir.join("Makefile"), "all:\n\t@true\n").unwrap();

        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: true,
        };
        let out = tool.generate(Some(120)).await.expect("bear branch is Ok");
        let text = output_text(out.as_ref());
        // All three outcomes are real BEHAVIOUR (bear ran / bear is absent on
        // this host / bear ran and produced nothing) and none of the three
        // marker strings appears in the old print-only branch, so this stays
        // red on the old code while tolerating a host without bear.
        assert!(
            text.contains("Generated compile DB with")
                || text.contains("could not be used here")
                || text.contains("ran but produced no"),
            "must report what the bear run actually did: {text}"
        );
        assert!(
            !dir.join(COMPILE_DB).exists(),
            "nothing is written into the project: {text}"
        );
        if text.contains("Generated compile DB with") {
            assert!(
                scratch_dir(&dir).join(COMPILE_DB).is_file(),
                "a reported DB must exist on disk: {text}"
            );
            assert!(
                text.contains("make clean"),
                "an empty DB must tell the model why and what to do: {text}"
            );
        }

        fs::remove_dir_all(scratch_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cmake_generation_is_trust_gated() {
        // The CMake configure runs the project's CMake code — measured
        // running an `execute_process` — and was not gated at all.
        let dir = temp_dir();
        isolate_log(&dir);
        fs::write(
            dir.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.10)\nproject(x NONE)\n\
             execute_process(COMMAND touch ${CMAKE_SOURCE_DIR}/RAN)\n",
        )
        .unwrap();
        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: false,
        };
        let Err(FunctionCallError::RespondToModel(msg)) = tool.generate(None).await else {
            panic!("an untrusted CMake project must be refused, not configured");
        };
        assert!(msg.contains(crate::trust::ALLOW_ENV), "{msg}");
        assert!(
            !dir.join("RAN").exists(),
            "the project's CMake code must not run"
        );
        assert!(!dir.join("build").exists(), "nothing may be configured");
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_cmake_configure_is_bounded_by_timeout_sec() {
        // `timeout_sec: 2` was once ignored and a configure that slept 12 s
        // ran to the end (raw `.output()`, no cap).
        if !crate::program_args::fixture::have("cmake") {
            eprintln!("skip: cmake not installed");
            return;
        }
        let dir = temp_dir();
        isolate_log(&dir);
        fs::write(
            dir.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.10)\nproject(x NONE)\n\
             execute_process(COMMAND sleep 30)\n",
        )
        .unwrap();
        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: true,
        };
        let start = std::time::Instant::now();
        let out = tool.generate(Some(2)).await.expect("a report");
        let text = output_text(out.as_ref());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
        assert!(text.contains("killed after 2s"), "{text}");
        fs::remove_dir_all(scratch_dir(&dir)).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn every_makefile_name_make_reads_is_a_makefile_project() {
        // When only `Makefile` was recognised, `makefile` and `GNUmakefile`
        // projects read "No CMakeLists.txt/Makefile found".
        for name in ["makefile", "GNUmakefile"] {
            let dir = temp_dir();
            isolate_log(&dir);
            fs::write(dir.join(name), "all:\n\t@true\n").unwrap();
            let tool = CompileDbTool {
                cwd: dir.clone(),
                native_exec: false,
            };
            // Untrusted, so the Makefile path refuses — which proves it was taken.
            let Err(FunctionCallError::RespondToModel(msg)) = tool.generate(None).await else {
                panic!("{name}: must route to the Makefile path");
            };
            assert!(msg.contains(crate::trust::ALLOW_ENV), "{name}: {msg}");
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[tokio::test]
    async fn generate_neither_present_branch() {
        let dir = temp_dir();
        isolate_log(&dir);

        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: true,
        };
        let out = tool.generate(None).await.expect("neither branch is Ok");
        let text = output_text(out.as_ref());
        assert!(
            text.contains("cannot auto-generate"),
            "expected neither-present message: {text}"
        );
        assert!(!dir.join("build").exists(), "must not invoke cmake");

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn generate_prefers_cmake_when_both_present() {
        // With CMakeLists.txt present, generate() routes to generate_cmake()
        // rather than the Makefile branch — we assert routing without running
        // cmake by checking the output is NOT the Makefile bear path. (cmake may
        // be absent on the test host, in which case generate_cmake returns a
        // RespondToModel error; either way the Makefile path must not appear.)
        //
        let dir = temp_dir();
        isolate_log(&dir);
        fs::write(dir.join("CMakeLists.txt"), "project(x NONE)\n").unwrap();
        fs::write(dir.join("Makefile"), "all:\n\techo hi\n").unwrap();

        let tool = CompileDbTool {
            cwd: dir.clone(),
            native_exec: true,
        };
        match tool.generate(None).await {
            Ok(out) => {
                let text = output_text(out.as_ref());
                assert!(
                    !text.contains("bear -- make"),
                    "CMake project must not fall through to the Makefile path: {text}"
                );
            }
            Err(FunctionCallError::RespondToModel(msg)) => {
                // cmake missing on host — acceptable; confirms we tried cmake.
                assert!(msg.contains("cmake"), "expected cmake error: {msg}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn report_plain_output_carries_banner() {
        // The refactor routes success through report_plain — confirm the
        // workspace-scoped banner (no target) wraps the body.
        let dir = temp_dir();
        isolate_log(&dir);
        let out = crate::report::report_plain(
            TOOL_NAME,
            "hello body".to_string(),
            CheckResult::pass(TOOL_NAME, "Found it."),
        );
        let text = output_text(out.as_ref());
        assert!(text.contains("hello body"), "body missing: {text}");
        assert!(
            text.contains("cpp_compile_db"),
            "tool banner missing: {text}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_respects_cap() {
        assert_eq!(truncate("short", 100), "short");
        assert!(truncate("0123456789", 4).starts_with("0123"));
    }
}
