//! `cpp_sanitize` — build a C/C++ translation unit under AddressSanitizer +
//! UBSan (or Valgrind), run it, and hand back **both** parsed findings and the
//! sanitizer's own report.
//!
//! # Truncation ownership
//!
//! This tool is layer 1 of the three-layer scheme documented on
//! [`crate::run`]. Its model-facing budget is:
//!
//! * [`STDOUT_CAP`] — 2 KB of the program's own stdout.
//! * [`TRACE_CAP`] — 8 KB of sanitizer stderr, **after** the semantic trim
//!   ([`strip_shadow_block`]: drop by meaning first, then by length).
//! * `render_findings(_, FINDINGS_CAP)` — 50 one-line findings.
//!
//! Total worst case ≈ **10 KB + 50 finding lines**. Layer 0
//! ([`crate::run::CAPTURE_CAP_BYTES`], 4 MiB/stream) is far above it, as
//! required. **A layer-2 renderer cap below ~12 KB puts the stack trace back
//! out of sight** — which is the failure this file exists to fix.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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

use check_result::CheckResult;
use check_result::NotRunReason;

use crate::finding::Finding;
use crate::finding::Severity;
use crate::finding::render_findings;
use crate::run::CAPTURE_CAP_BYTES;
use crate::run::RunOutcome;
use crate::run::compile_timeout_from;
use crate::run::run_capped;
use crate::run::run_capped_env;
use crate::run::run_capped_no_core;
use crate::run::timeout_from;

const TOOL_NAME: &str = "cpp_sanitize";
/// C++23 by default: `-std=c++20` fails on a C++23 project. A compile
/// database entry, when we find one, overrides this; an explicit `std` arg
/// overrides both.
const DEFAULT_STD: &str = "c++23";
/// The default standard when every source is C. The GNU dialect, as gcc
/// and clang themselves default to: strict `-std=c17` hides POSIX/GNU
/// declarations (`popen`, `strdup`, …) unless the program defines a
/// feature-test macro, and a call to an undeclared function then returns a
/// truncated `int`, which crashed test programs that were correct as written.
const DEFAULT_STD_C: &str = "gnu17";
const DEFAULT_SANITIZER: &str = "asan_ubsan";
/// Default compile budget for the single-file build. A reproducer that takes
/// longer than this to build is compile-bound (header-heavy C++ at -O0 under
/// instrumentation, or library sources recompiled via `extra_sources`); a larger
/// timeout is the wrong fix, so the timeout report says what to do instead.
/// Project mode keeps [`crate::run::DEFAULT_COMPILE_TIMEOUT_SECS`].
const SINGLE_TU_COMPILE_TIMEOUT_SECS: u64 = 180;
/// How many built library files a hint names, and how much of the tree the
/// search for them may visit.
const LIBRARY_HINT_CAP: usize = 3;
const LIBRARY_SCAN_BUDGET: usize = 20_000;
const LIBRARY_SCAN_DEPTH: usize = 5;
const DEFAULT_COMPILER: &str = "g++";
/// The default compiler when every source is C.
const DEFAULT_COMPILER_C: &str = "gcc";
/// The only compilers we will invoke. Not an arbitrary program name.
const COMPILER_ALLOWLIST: &[&str] = &["gcc", "g++", "clang", "clang++"];
const FINDINGS_CAP: usize = 50;
/// Cap on how much of the program's own stdout we echo back. Layer 1, see the
/// module docs.
const STDOUT_CAP: usize = 2000;
/// Cap on the raw sanitizer report we hand back. Layer 1. Applied *after*
/// [`strip_shadow_block`]: trim by meaning before length. A real
/// heap-use-after-free report measures 2,676 bytes of which 1,409 (53%) is the
/// shadow hex map, so the actionable header + three stacks fit comfortably.
const TRACE_CAP: usize = 8000;

/// The line AddressSanitizer emits immediately before its shadow-byte hex dump.
const SHADOW_MARKER: &str = "Shadow bytes around the buggy address:";

// ---- project mode ---------------------------------------------------------
//
// Everything below belongs to `project: true`: build the project's OWN target
// with sanitizers injected, instead of compiling `source` + `extra_sources`.
// The single-TU path above is untouched by all of it.

/// Most parallel jobs for a project build. Deliberately small: this runs on
/// whatever machine the user is on, a `-j$(nproc)` link step under ASan is how
/// a 16 GB box OOMs, and nothing here is a benchmark. Below this cap the count
/// follows available memory ([`crate::resources::jobs`]).
const PROJECT_BUILD_JOBS_MAX: usize = 2;

/// Parallel jobs for a project build started now.
fn project_build_jobs() -> String {
    crate::resources::jobs()
        .min(PROJECT_BUILD_JOBS_MAX)
        .to_string()
}
/// `make -n` only parses and prints the recipe lines; it must never inherit a
/// ten-minute build budget.
const MAKE_DRY_RUN_TIMEOUT_SECS: u64 = 120;
/// How deep under the build tree we look for the program the build produced.
const EXEC_SEARCH_DEPTH: usize = 6;
/// How many candidate executables we are willing to collect at all.
const EXEC_CANDIDATE_CAP: usize = 200;
/// How many candidates we name back when we cannot pick one.
const EXEC_LIST_CAP: usize = 20;
/// Makefile names, in GNU make's own lookup order.
const MAKEFILE_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];
/// Directories that never hold the program under test.
const EXEC_SKIP_DIRS: &[&str] = &[
    "CMakeFiles",
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    "node_modules",
];
/// Extensions that are never the program under test, even with the exec bit set.
const NON_PROGRAM_EXTS: &[&str] = &[
    "a", "bash", "c", "cache", "cc", "cmake", "cpp", "css", "cxx", "d", "dll", "dylib", "h", "hpp",
    "html", "hxx", "in", "inc", "js", "json", "lib", "log", "make", "md", "mk", "ninja", "o", "pl",
    "py", "rb", "sh", "so", "stamp", "toml", "ts", "txt", "xml", "yaml", "yml",
];
/// Program names that are a build system's own input, not its output.
const NON_PROGRAM_NAMES: &[&str] = &["GNUmakefile", "Makefile", "makefile"];
/// Front-ends we recognise as "this argv compiles code".
const COMPILER_BASENAMES: &[&str] = &["c++", "cc", "clang", "clang++", "gcc", "g++"];
/// Prefixes a real recipe line wraps the compiler in.
const COMPILER_WRAPPERS: &[&str] = &["ccache", "distcc", "env", "sccache", "time"];
/// Slack subtracted from the build's start time before deciding a file is too
/// old to have come from it: a filesystem that stores whole seconds can stamp a
/// file written at T+0.3s as T-0.2s.
const MTIME_SLACK_SECS: u64 = 2;

pub struct SanitizeTool {
    pub cwd: PathBuf,
    /// This tool compiles and runs code outside the filesystem
    /// sandbox. Resolved once at thread start; see [`crate::trust`].
    pub native_exec: bool,
}

/// Unknown fields are refused. A misnamed field used to be dropped
/// without a word — measured: `"args": "x y"` (for `run_args`) ran the program
/// with no arguments and reported "clean".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SanitizeArgs {
    source: String,
    /// Accepted only so `"project": false` is not an unknown field; the mode
    /// was already chosen from [`ModeProbe`].
    #[serde(default)]
    #[allow(dead_code)]
    project: bool,
    #[serde(default)]
    sanitizer: Option<String>,
    #[serde(default)]
    std: Option<String>,
    #[serde(default)]
    run_args: Option<String>,
    // ---- STRUCTURED project args only. ------------------------------------
    // Deliberately no free-form `flags[]`. The compile line hardcodes
    // `-fsanitize=address,undefined` and the verdict says "clean: exit 0, no
    // sanitizer errors" when nothing is found — a free-form flag list would let
    // the model pass `-fno-sanitize=all` and receive a clean bill of health on
    // memory-unsafe code. That verdict line is the entire product.
    #[serde(default)]
    include_dirs: Option<Vec<String>>,
    #[serde(default)]
    defines: Option<Vec<String>>,
    #[serde(default)]
    extra_sources: Option<Vec<String>>,
    #[serde(default)]
    compiler: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

/// Just enough of the arguments to pick a mode.
///
/// Parsed FIRST so that project mode, which has no `source` at all, never has
/// to satisfy the single-TU argument struct — and so `SanitizeArgs` keeps
/// `source` required exactly as it was.
#[derive(Deserialize)]
struct ModeProbe {
    #[serde(default)]
    project: bool,
    #[serde(default)]
    source: Option<String>,
}

/// Project mode (`project: true`): build the project's OWN target with the
/// sanitizer flags injected, then run it.
///
/// Deliberately a much smaller argument set than [`SanitizeArgs`]: the
/// project's build system owns the compiler, the standard, the include paths
/// and the defines, so `compiler`/`std`/`include_dirs`/`defines`/
/// `extra_sources` would each be a second, disagreeing source of truth.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    /// Always true here (it selected this mode).
    #[serde(default)]
    #[allow(dead_code)]
    project: bool,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    sanitizer: Option<String>,
    #[serde(default)]
    run_args: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
    /// A Makefile build happens in the user's own tree. Opt-in, not a default.
    #[serde(default)]
    allow_in_tree_build: bool,
}

impl<'call> ToolExecutor<ToolCall<'call>> for SanitizeTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "source".to_string(),
            JsonSchema::string(Some(
                "Path to the main .c/.cpp file, relative to the workspace or absolute. Required \
                 unless you pass project=true, which builds the project's own target instead of \
                 a named translation unit."
                    .to_string(),
            )),
        );
        properties.insert(
            "sanitizer".to_string(),
            JsonSchema::string(Some(
                "Which checker to use: \"asan_ubsan\" (AddressSanitizer + UBSan, the default — \
                 use-after-free / overflow / UB / leaks), \"tsan\" (ThreadSanitizer — DATA RACES \
                 in multithreaded code), or \"valgrind\" (memcheck)."
                    .to_string(),
            )),
        );
        properties.insert(
            "std".to_string(),
            JsonSchema::string(Some(format!(
                "C/C++ standard, e.g. \"c++23\". Defaults to the value in compile_commands.json \
                 when an entry for this file exists, else {DEFAULT_STD}."
            ))),
        );
        properties.insert(
            "run_args".to_string(),
            JsonSchema::string(Some(
                "Optional arguments passed to the compiled program, written like a shell command \
                 line: space-separated, quote with '…' or \"…\" to keep spaces, \"\" is an empty \
                 argument. No shell runs: pipes, redirection and $VAR are rejected."
                    .to_string(),
            )),
        );
        properties.insert(
            "include_dirs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Header search paths (become -I). Use this for a project layout like \
                     include/foo.hpp — without it the build fails with 'No such file or \
                     directory'. Ignored for paths already supplied by compile_commands.json."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "defines".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Preprocessor definitions as NAME or NAME=VALUE (become -D).".to_string()),
            ),
        );
        properties.insert(
            "extra_sources".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Additional .c/.cpp translation units to compile and link alongside `source`."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "compiler".to_string(),
            JsonSchema::string(Some(format!(
                "Compiler to use; one of {}. Default {DEFAULT_COMPILER}.",
                COMPILER_ALLOWLIST.join(", ")
            ))),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some(
                "Wall-clock cap applied to the build and, separately, to the run. Defaults: 600s \
                 build, 300s run. Raise it for heavy template instantiation, and for a whole-\
                 project build."
                    .to_string(),
            )),
        );
        properties.insert(
            "project".to_string(),
            JsonSchema::boolean(Some(
                "Build the PROJECT's own target with sanitizers injected, instead of compiling \
                 `source` plus `extra_sources`. Needs a CMakeLists.txt (preferred) or a Makefile \
                 in the workspace root. Use it for any real multi-file project: it removes the \
                 need to enumerate translation units, and it is the only way to sanitize the \
                 project's real binary rather than a reproducer. Defaults to false."
                    .to_string(),
            )),
        );
        properties.insert(
            "target".to_string(),
            JsonSchema::string(Some(
                "Project mode only: the build target / executable to build and run (e.g. \
                 \"hft_engine\"). Omit it when the project builds exactly one executable. If the \
                 build produces several and you omit this, the tool lists them and asks."
                    .to_string(),
            )),
        );
        properties.insert(
            "allow_in_tree_build".to_string(),
            JsonSchema::boolean(Some(
                "Project mode, Makefile projects only: permit building IN YOUR WORKING TREE. \
                 Make has no out-of-tree build, so object files and binaries are written next to \
                 your sources. CMake projects never need this — they are configured fresh in a \
                 temporary directory. Defaults to false."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Build C/C++ with a sanitizer and run it, returning the FULL report \
                (header + stack traces) plus structured findings. Default \"asan_ubsan\" catches \
                use-after-free, buffer overflow, UB and leaks; \"tsan\" catches DATA RACES in \
                multithreaded code (the tool to reach for on an intermittent/heisen concurrency \
                bug); \"valgrind\" runs memcheck. Use proactively to verify C/C++ you wrote or are \
                reviewing — this is the tool that PROVES a bug at runtime, so prefer it over \
                reasoning about the source. Do NOT tell the user that C/C++ you just wrote works \
                until you have run it here, especially when it parses untrusted or malformed input. It compiles the named `source` plus any files listed in \
                `extra_sources`, taking per-file flags (includes, defines, -std, -march/-mavx2) from \
                compile_commands.json when present; for a multi-file program you MUST list the other \
                .cpp it links against in `extra_sources` or the build fails with 'undefined \
                reference'. There is deliberately no free-form compiler-flag argument: the sanitizer \
                flags are fixed so a clean verdict cannot be faked. For a real project that has its \
                own build system, set `project`: true INSTEAD of passing `source` — cpp_sanitize \
                then configures a FRESH CMake build outside your workspace (or runs make) with the \
                sanitizer flags injected, builds the project's own target (name one with `target`), \
                and runs it, so you never have to enumerate translation units. Reach for project \
                mode when the single-TU build fails to link, when the bug needs the project's real \
                binary rather than a reproducer, or whenever the workspace root has a CMakeLists.txt. \
                A CMake build is checked for instrumentation by reading back the compile_commands.json \
                it generates; a Makefile build is best-effort, is audited with `make -n`, and must be \
                opted into with `allow_in_tree_build`: true because make writes into your working \
                tree. Either way, if the project's own flags would switch the sanitizer off, the \
                output says so instead of reporting a clean run."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                // `source` is required for the single-TU form and meaningless for
                // `project: true`, so the choice is enforced in handle_call —
                // which names both forms — rather than by the schema.
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

impl SanitizeTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let mode: ModeProbe = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        // Trust gate: compiling + executing workspace source happens outside the
        // filesystem sandbox. Refuse before we spawn anything. BOTH modes go
        // through this — project mode builds and runs strictly more of the
        // user's code than the single-TU path does.
        self.gate()?;

        if mode.project {
            return self.handle_project(arguments).await;
        }
        if mode.source.is_none() {
            return Err(FunctionCallError::RespondToModel(
                "cpp_sanitize needs either `source` (one translation unit, plus every other .cpp \
                 it links against in `extra_sources`) or `project`: true (build and run the \
                 project's own CMake/Make target with the sanitizers injected)."
                    .to_string(),
            ));
        }

        let args: SanitizeArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let sanitizer = args.sanitizer.as_deref().unwrap_or(DEFAULT_SANITIZER);
        if !is_accepted_sanitizer(sanitizer) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown sanitizer {sanitizer:?}; expected \"asan_ubsan\", \"valgrind\", \"tsan\", or \"msan\""
            )));
        }
        let run_args =
            crate::program_args::parse("run_args", args.run_args.as_deref().unwrap_or(""))
                .map_err(FunctionCallError::RespondToModel)?;
        let mut plan = self.build_plan(&args)?;
        if sanitizer == "msan" {
            plan.compiler = msan_compiler(&plan.compiler, args.compiler.is_some())
                .map_err(FunctionCallError::RespondToModel)?;
        }

        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize", "");

        let result = match sanitizer {
            "valgrind" => {
                self.run_valgrind(&plan, &tmp_bin, &run_args, args.timeout_sec)
                    .await
            }
            "msan" => {
                self.run_msan(&plan, &tmp_bin, &run_args, args.timeout_sec)
                    .await
            }
            "tsan" => {
                self.run_tsan(&plan, &tmp_bin, &run_args, args.timeout_sec)
                    .await
            }
            _ => {
                self.run_asan_ubsan(&plan, &tmp_bin, &run_args, args.timeout_sec)
                    .await
            }
        };

        // Best-effort cleanup of the compiled binary regardless of outcome.
        let _ = std::fs::remove_file(&tmp_bin);

        let (text, verdict) = result?;
        Ok(crate::report::report(
            TOOL_NAME,
            &args.source,
            text,
            verdict,
        ))
    }

    /// The trust gate for this tool. Separate so a test can prove the
    /// wiring, not just that `crate::trust` can say no.
    fn gate(&self) -> Result<(), FunctionCallError> {
        crate::trust::require_native_exec(self.native_exec, TOOL_NAME, &self.cwd)
    }

    /// Resolve the build inputs: compile database first, structured
    /// args as the fallback and as an additive overlay.
    fn build_plan(&self, args: &SanitizeArgs) -> Result<BuildPlan, FunctionCallError> {
        let source = resolve_path(&self.cwd, &args.source);
        if !source.exists() {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file does not exist: {}",
                source.display()
            )));
        }
        // Given a header, the compiler writes a PRECOMPILED HEADER to the
        // `-o` path instead of a program, and running that fails.
        if let Some(what) = not_a_translation_unit(&source) {
            return Err(FunctionCallError::RespondToModel(format!(
                "source {} {what}. cpp_sanitize compiles and RUNS a program, so `source` must be \
                 the .c/.cpp file that defines main(); headers reach the build through its \
                 #include lines, and other .c/.cpp files go in `extra_sources`.",
                source.display()
            )));
        }

        let explicit_compiler = match args.compiler.as_deref() {
            None => None,
            Some(c) if COMPILER_ALLOWLIST.contains(&c) => Some(c.to_string()),
            Some(other) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "compiler {other:?} is not allowed; choose one of {}",
                    COMPILER_ALLOWLIST.join(", ")
                )));
            }
        };

        // 1. Compile database, when one exists AND it knows this file.
        let db_path = super::compile_db::find_compile_db(&self.cwd);
        let db = db_path
            .as_deref()
            .and_then(|path| db_flags_for(path, &source));

        // 2. Merge. Explicit args win over the database; both are additive for
        //    include dirs and defines, so a stale DB can still be topped up.
        let mut include_dirs: Vec<String> = Vec::new();
        let mut defines: Vec<String> = Vec::new();
        let mut march: Vec<String> = Vec::new();
        let mut semantics: Vec<String> = Vec::new();
        let mut db_std = None;
        if let Some(db) = &db {
            include_dirs.extend(db.include_dirs.iter().cloned());
            defines.extend(db.defines.iter().cloned());
            march.extend(db.march.iter().cloned());
            semantics.extend(db.semantics.iter().cloned());
            db_std = db.std.clone();
        }
        for (i, dir) in args.include_dirs.iter().flatten().enumerate() {
            let dir = arg_value("include_dirs", i, dir, "-I")?;
            let resolved = resolve_path(&self.cwd, dir).display().to_string();
            if !include_dirs.contains(&resolved) {
                include_dirs.push(resolved);
            }
        }
        for (i, def) in args.defines.iter().flatten().enumerate() {
            let def = arg_value("defines", i, def, "-D")?.to_string();
            if !defines.contains(&def) {
                defines.push(def);
            }
        }

        // 3. Extra translation units. Pull each extra file's OWN compile-DB
        //    flags (its `-march`/`-mavx2` target features and include dirs) and
        //    union them in, so a multi-TU build doesn't fail with "requires
        //    target feature 'avx'" when only one TU carries `-mavx2`.
        let mut sources = vec![source];
        for extra in args.extra_sources.iter().flatten() {
            let path = resolve_path(&self.cwd, extra);
            if !path.exists() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "extra source does not exist: {}",
                    path.display()
                )));
            }
            if let Some(edb) = db_path.as_deref().and_then(|p| db_flags_for(p, &path)) {
                for m in edb.march {
                    if !march.contains(&m) {
                        march.push(m);
                    }
                }
                for flag in edb.semantics {
                    if !semantics.contains(&flag) {
                        semantics.push(flag);
                    }
                }
                for dir in edb.include_dirs {
                    if !include_dirs.contains(&dir) {
                        include_dirs.push(dir);
                    }
                }
                for def in edb.defines {
                    if !defines.contains(&def) {
                        defines.push(def);
                    }
                }
            }
            sources.push(path);
        }

        // A C program is built as C. The C++ driver and -std=c++23 used to
        // apply to .c files too: valid C failed (`invalid conversion from
        // 'void*'`) and C semantics silently changed (sizeof('a') == 1).
        let all_c = sources
            .iter()
            .filter(|s| !is_link_input(s))
            .all(|s| is_c_source(s));
        let compiler = explicit_compiler.unwrap_or_else(|| {
            if all_c {
                DEFAULT_COMPILER_C
            } else {
                DEFAULT_COMPILER
            }
            .to_string()
        });
        let std_is_default = args.std.is_none() && db_std.is_none();
        let std = args
            .std
            .clone()
            .or(db_std)
            .unwrap_or_else(|| if all_c { DEFAULT_STD_C } else { DEFAULT_STD }.to_string());

        let mut origin = describe_origin(
            db_path.as_deref(),
            db.is_some(),
            &args.source,
            &std,
            include_dirs.len(),
            defines.len(),
            sources.len(),
        );
        // Honesty: this is ONE compile command, so the unioned ISA flags reach
        // every TU, not just the one that asked for them. Say so rather than let
        // the model assume per-file codegen (AVX in a baseline TU would fault on
        // a host without it).
        if !march.is_empty() && sources.len() > 1 {
            origin.push_str(&format!(
                "\nnote: target flags [{}] apply to ALL {} translation units in this \
                 single-command build; sanitize a TU on its own if it needs a different \
                 ISA baseline.",
                march.join(" "),
                sources.len()
            ));
        }

        if !semantics.is_empty() {
            origin.push_str(&format!(
                "\nnote: carried from the compile database because they change what the \
                 program means: {}",
                semantics.join(" ")
            ));
        }

        Ok(BuildPlan {
            compiler,
            std,
            std_is_default,
            include_dirs,
            defines,
            march,
            semantics,
            sources,
            origin,
        })
    }

    /// Compile `plan` instrumented for `sanitizer` into `tmp_bin`.
    ///
    /// `Ok(Ok(note))`: built; `note` (possibly empty) belongs in the report's
    /// provenance. `Ok(Err(report))`: the build failed or timed out, and
    /// `report` is the answer.
    ///
    /// - Mixed C and C++ sources: each C file is compiled as C, to an object,
    ///   by the matching C compiler (a single `g++` command compiled .c files
    ///   as C++, which fails on valid C and changes name mangling, so a C
    ///   helper "did not link"; a single `clang++` command refuses the C++
    ///   `-std` for C outright).
    /// - when the standard is OUR default and this compiler does not know
    ///   it (clang 14 calls C++23 `c++2b`; every call with `compiler:
    ///   "clang++"` and no `std` failed on Debian 12's clang), retry with the
    ///   compiler's older names for it, and say so.
    async fn build(
        &self,
        plan: &BuildPlan,
        sanitizer: &str,
        tmp_bin: &Path,
        timeout_sec: Option<u64>,
    ) -> Result<Result<String, (String, CheckResult)>, FunctionCallError> {
        let flags: Vec<String> = sanitizer_build_flags(sanitizer)
            .into_iter()
            .map(String::from)
            .collect();
        let timeout = single_tu_compile_timeout(timeout_sec);
        let mixed = plan.sources.iter().any(|s| is_c_source(s))
            && plan
                .sources
                .iter()
                .any(|s| !is_c_source(s) && !is_link_input(s));
        // Compiler warnings of the successful steps, read for implicit
        // declarations (a harness bug, see `implicit_declaration_note`).
        let mut warnings = String::new();

        let mut objects: Vec<PathBuf> = Vec::new();
        let result = async {
            let mut inputs: Vec<PathBuf> = Vec::new();
            let mut c_std_used = DEFAULT_STD_C;
            for (n, source) in plan.sources.iter().enumerate() {
                if !(mixed && is_c_source(source)) {
                    inputs.push(source.clone());
                    continue;
                }
                let object = unique_tmp_path(&format!("byteask-cpp-sanitize-obj{n}"), ".o");
                objects.push(object.clone());
                let c_stds: Vec<&str> = std::iter::once(DEFAULT_STD_C)
                    .chain(std_fallbacks(DEFAULT_STD_C).iter().copied())
                    .collect();
                for (i, std) in c_stds.iter().enumerate() {
                    let mut argv = plan.compile_argv_with_std(std);
                    argv.extend(flags.iter().cloned());
                    argv.extend([
                        "-c".to_string(),
                        source.display().to_string(),
                        "-o".to_string(),
                        object.display().to_string(),
                    ]);
                    match run_capped(c_compiler_for(&plan.compiler), &argv, &self.cwd, timeout)
                        .await?
                    {
                        RunOutcome::TimedOut { secs } => {
                            return Ok(Err((
                                compile_timeout_message(
                                    plan,
                                    secs,
                                    &built_project_libraries(&self.cwd),
                                ),
                                build_timeout_result(),
                            )));
                        }
                        RunOutcome::Finished {
                            success: false,
                            stderr,
                            ..
                        } => {
                            if i + 1 < c_stds.len() && std_rejected(&stderr, std) {
                                continue;
                            }
                            return Ok(Err((
                                compile_failure(plan, &stderr),
                                build_failed_result(),
                            )));
                        }
                        RunOutcome::Finished { stderr, .. } => {
                            warnings.push_str(&stderr);
                            c_std_used = *std;
                            break;
                        }
                    }
                }
                inputs.push(object);
            }

            let mut candidates = vec![plan.std.clone()];
            if plan.std_is_default {
                candidates.extend(std_fallbacks(&plan.std).iter().map(|s| s.to_string()));
            }
            let mut note = String::new();
            if mixed {
                note.push_str(&format!(
                    "\nnote: C sources were compiled as C ({}, -std={c_std_used}) and linked in.",
                    c_compiler_for(&plan.compiler)
                ));
            }
            for (i, std) in candidates.iter().enumerate() {
                let mut argv = plan.compile_argv_with_std(std);
                argv.extend(flags.iter().cloned());
                argv.extend(inputs.iter().map(|p| p.display().to_string()));
                argv.push("-o".to_string());
                argv.push(tmp_bin.display().to_string());
                match run_capped(&plan.compiler, &argv, &self.cwd, timeout).await? {
                    RunOutcome::TimedOut { secs } => {
                        return Ok(Err((
                            compile_timeout_message(
                                plan,
                                secs,
                                &built_project_libraries(&self.cwd),
                            ),
                            build_timeout_result(),
                        )));
                    }
                    RunOutcome::Finished {
                        success: false,
                        stderr,
                        ..
                    } => {
                        if i + 1 < candidates.len() && std_rejected(&stderr, std) {
                            continue;
                        }
                        let mut report = compile_failure(plan, &stderr);
                        if has_unresolved_symbols(&stderr) {
                            report.push_str(&link_hint(&built_project_libraries(&self.cwd)));
                        }
                        return Ok(Err((report, build_failed_result())));
                    }
                    RunOutcome::Finished { stderr, .. } => {
                        warnings.push_str(&stderr);
                        note.push_str(&implicit_declaration_note(&implicitly_declared(&warnings)));
                        if i > 0 {
                            note.push_str(&format!(
                                "\nnote: {} does not accept -std={}; built with -std={std} (its \
                                 name for the same or the nearest standard).",
                                plan.compiler, plan.std
                            ));
                        }
                        return Ok(Ok(note));
                    }
                }
            }
            unreachable!("the candidate list is never empty")
        }
        .await;
        for object in &objects {
            let _ = std::fs::remove_file(object);
        }
        result
    }

    async fn run_asan_ubsan(
        &self,
        plan: &BuildPlan,
        tmp_bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        // NO `-fno-sanitize-recover=undefined`. With it, the first UBSan
        // error aborts the process and AddressSanitizer never gets to report —
        // so a seeded stack-buffer-overflow came back as a one-line "ubsan
        // error … insufficient space" instead of ASan's named bug class with a
        // frame table. Letting UBSan recover lets both checkers speak.
        self.run_memory_checker(
            plan,
            "asan_ubsan",
            &sanitizer_env(),
            tmp_bin,
            run_args,
            timeout_sec,
        )
        .await
    }

    /// MemorySanitizer (clang only) with origin tracking: the report names the
    /// stack that created an uninitialised value, not only where it was used.
    async fn run_msan(
        &self,
        plan: &BuildPlan,
        tmp_bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        self.run_memory_checker(plan, "msan", &msan_env(), tmp_bin, run_args, timeout_sec)
            .await
    }

    /// Build `plan` for `sanitizer` (ASan+UBSan or MSan), run it with `env`,
    /// and render the verdict, the root-cause evidence and the raw report.
    async fn run_memory_checker(
        &self,
        plan: &BuildPlan,
        sanitizer: &str,
        env: &[(String, String)],
        tmp_bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let note = match self.build(plan, sanitizer, tmp_bin, timeout_sec).await? {
            Ok(note) => note,
            Err(report) => return Ok(report),
        };

        let mut preamble = plan.origin.clone();
        preamble.push_str(&note);
        preamble.push('\n');

        let bin = tmp_bin.display().to_string();
        let run = run_capped_env(&bin, run_args, &self.cwd, timeout_from(timeout_sec), env).await?;

        let (stdout, stderr, exit_label, truncated) = match run {
            RunOutcome::TimedOut { secs } => {
                return Ok((run_timeout_message(&preamble, secs), run_timeout_result()));
            }
            RunOutcome::Finished {
                code,
                stdout,
                stderr,
                truncated,
                ..
            } => {
                let label = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                (stdout, stderr, label, truncated)
            }
        };

        preamble.push_str(&symbolizer_note(&stderr));
        let reports = parse_asan_ubsan(&stderr);
        let verdict = asan_ubsan_verdict(&reports, &exit_label, truncated);
        // MemorySanitizer never looks for leaks, so "leaks not checked" does
        // not qualify its clean run; for ASan it does (macOS has no LSan).
        let result = findings_run_result(
            &reports.findings,
            &exit_label,
            truncated,
            sanitizer != "asan_ubsan" || leak_detection_available(),
        );
        let trace = prepare_trace(&stderr);

        Ok((
            format_run_output_with_evidence(
                &preamble,
                &verdict,
                &stdout,
                &reports.findings,
                &crate::memsafety::evidence::render_from(&stderr),
                &trace,
            ),
            result,
        ))
    }

    /// ThreadSanitizer — the only tool here that PROVES a data race at
    /// runtime. Builds with `-fsanitize=thread` and runs the program; the
    /// verdict is driven by the presence of a `ThreadSanitizer:` report in
    /// stderr, never by exit code alone, and a clean run is reported as
    /// "no race THIS run" (races are timing-dependent, so absence is not proof).
    async fn run_tsan(
        &self,
        plan: &BuildPlan,
        tmp_bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let note = match self.build(plan, "tsan", tmp_bin, timeout_sec).await? {
            Ok(note) => note,
            Err(report) => return Ok(report),
        };

        let mut preamble = plan.origin.clone();
        preamble.push_str(&note);
        preamble.push('\n');

        let bin = tmp_bin.display().to_string();
        let run = run_capped_env(
            &bin,
            run_args,
            &self.cwd,
            timeout_from(timeout_sec),
            &tsan_env(),
        )
        .await?;

        let (stdout, stderr, exit_label, truncated) = match run {
            RunOutcome::TimedOut { secs } => {
                return Ok((run_timeout_message(&preamble, secs), run_timeout_result()));
            }
            RunOutcome::Finished {
                code,
                stdout,
                stderr,
                truncated,
                ..
            } => {
                let label = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                (stdout, stderr, label, truncated)
            }
        };

        // Verdict on the report TEXT, never the exit code: a race need not change
        // the exit status, and a missed structured parse must never read as clean.
        let mut findings = Vec::new();
        if let Some(f) = parse_tsan_report(&stderr) {
            findings.push(f);
        }
        preamble.push_str(&symbolizer_note(&stderr));
        let verdict = tsan_verdict(&stderr, &exit_label, truncated);
        let result = tsan_result(&stderr, &exit_label, truncated);
        let trace = prepare_trace(&stderr);

        Ok((
            format_run_output(&preamble, &verdict, &stdout, &findings, &trace),
            result,
        ))
    }

    async fn run_valgrind(
        &self,
        plan: &BuildPlan,
        tmp_bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
    ) -> Result<(String, CheckResult), FunctionCallError> {
        let note = match self.build(plan, "valgrind", tmp_bin, timeout_sec).await? {
            Ok(note) => note,
            Err(report) => return Ok(report),
        };

        let xml_path = unique_tmp_path("byteask-cpp-valgrind", ".xml");
        let preamble = format!("{}{note}\n", plan.origin);
        let (run, xml) = self
            .run_memcheck(tmp_bin, run_args, timeout_sec, &xml_path)
            .await?;
        let (stdout, stderr, exit_label, truncated) = match run {
            RunOutcome::TimedOut { secs } => {
                return Ok((run_timeout_message(&preamble, secs), run_timeout_result()));
            }
            RunOutcome::Finished {
                code,
                stdout,
                stderr,
                truncated,
                ..
            } => (stdout, stderr, exit_label_of(code), truncated),
        };

        let findings = parse_valgrind_xml(&xml);
        let verdict = valgrind_verdict(&findings, &exit_label, truncated, xml_is_complete(&xml));
        let result = valgrind_result(&findings, &exit_label, truncated, xml_is_complete(&xml));

        // valgrind writes its findings to the XML file, so its stderr is
        // normally empty; pass it through the same pipeline anyway so a
        // startup failure ("command not found", ptrace denied) is visible
        // instead of silently yielding "clean".
        let trace = prepare_trace(&stderr);
        Ok((
            format_run_output(&preamble, &verdict, &stdout, &findings, &trace),
            result,
        ))
    }

    /// Run `program run_args…` under memcheck with its XML report written to
    /// `xml_path`, returning the outcome and the report. Shared by the
    /// single-TU and project paths. The XML file is removed on EVERY path —
    /// a timed-out run that returned before the removal would leave one
    /// byteask-cpp-valgrind-*.xml in /tmp per call — and valgrind may not
    /// write a vgcore into the workspace when the program dies.
    async fn run_memcheck(
        &self,
        program: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
        xml_path: &Path,
    ) -> Result<(RunOutcome, String), FunctionCallError> {
        let mut argv = vec![
            "--tool=memcheck".to_string(),
            // A valgrind killed at the timeout leaves its vgdb FIFOs (and a
            // shared-memory file) in TMPDIR; nothing here uses vgdb.
            "--vgdb=no".to_string(),
            "--leak-check=full".to_string(),
            "--error-exitcode=99".to_string(),
            "--xml=yes".to_string(),
            format!("--xml-file={}", xml_path.display()),
            program.display().to_string(),
        ];
        argv.extend(run_args.iter().cloned());
        let run = run_capped_no_core("valgrind", &argv, &self.cwd, timeout_from(timeout_sec)).await;
        let xml = std::fs::read_to_string(xml_path).unwrap_or_default();
        let _ = std::fs::remove_file(xml_path);
        Ok((run?, xml))
    }

    // ---- project mode ----------------------------------------------------
    //
    // The single-TU path above compiles a compile line this tool writes itself,
    // which is what makes "clean: exit 0, no sanitizer errors" trustworthy: the
    // project cannot put `-fno-sanitize=` on it, because nothing outside the
    // -I/-D/-std/-m allowlist is ever copied across.
    //
    // A project build cannot work that way — the whole point is that the
    // project's own build system writes the compile lines. So the guarantee is
    // preserved from the other end: we inject our flags, then READ BACK the
    // command lines the build will really run (CMake's generated
    // compile_commands.json, `make -n`'s printed recipes) and refuse to let the
    // verdict read as clean when they carry something that switches the
    // sanitizer off. See `qualify_verdict`.

    async fn handle_project(
        &self,
        arguments: &str,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: ProjectArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let sanitizer = args.sanitizer.as_deref().unwrap_or(DEFAULT_SANITIZER);
        if !is_accepted_sanitizer(sanitizer) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown sanitizer {sanitizer:?}; expected \"asan_ubsan\", \"valgrind\", \"tsan\", or \"msan\""
            )));
        }
        if sanitizer == "msan" {
            return Err(FunctionCallError::RespondToModel(
                "sanitizer \"msan\" is single-file only: MemorySanitizer needs clang for EVERY \
                 object in the program (the project's libraries included), or it reports false \
                 positives, and a project build here keeps the project's own compiler. Sanitize \
                 the reproducer with `source` and sanitizer \"msan\", or build the project with \
                 clang and -fsanitize=memory -fsanitize-memory-track-origins=2 yourself."
                    .to_string(),
            ));
        }
        let run_args =
            crate::program_args::parse("run_args", args.run_args.as_deref().unwrap_or(""))
                .map_err(FunctionCallError::RespondToModel)?;
        let plan = self.project_plan(&args, sanitizer)?;
        let banner = plan.banner_target();

        let result = self.run_project(&plan, &args, &run_args).await;

        // Best-effort cleanup, exactly like `tmp_bin`. Only the CMake tree is
        // ours: a Makefile build wrote into the user's own working tree and
        // deleting that would destroy their files. A kept tree stays
        // so the next build of this project is incremental.
        if plan.kind == ProjectKind::Cmake && !crate::builds::is_stable_build_dir(&plan.build_dir) {
            let _ = std::fs::remove_dir_all(&plan.build_dir);
        }

        let (text, verdict) = result?;
        Ok(crate::report::report(TOOL_NAME, &banner, text, verdict))
    }

    /// Resolve project mode before anything is spawned: which build system,
    /// where the build tree goes, and the fixed sanitizer flags to inject.
    fn project_plan(
        &self,
        args: &ProjectArgs,
        sanitizer: &str,
    ) -> Result<ProjectPlan, FunctionCallError> {
        if let Some(target) = args.target.as_deref() {
            if !is_safe_target_name(target) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "target {target:?} is not a plausible build-target name; expected letters, \
                     digits and . _ - + / only. It is handed straight to cmake/make as its own \
                     argument, so a name starting with '-' would be read as an option."
                )));
            }
        }

        let flags = sanitizer_build_flags(sanitizer).join(" ");
        // The workspace itself, or, when it has no build file of its own and is a parent of
        // the repository, the repository below it.
        let has_build_file = |dir: &Path| {
            dir.join("CMakeLists.txt").is_file()
                || MAKEFILE_NAMES.iter().any(|n| dir.join(n).is_file())
        };
        let root = if has_build_file(&self.cwd) {
            self.cwd.clone()
        } else {
            crate::gitx::repo_roots_sync(&self.cwd)
                .into_iter()
                .find(|r| has_build_file(r))
                .unwrap_or_else(|| self.cwd.clone())
        };

        let cmake = root.join("CMakeLists.txt");
        if cmake.is_file() {
            // NEVER inside the workspace: a build/ directory we dropped into the
            // user's repo is a side effect they did not ask for, and the second
            // run would then inherit the first run's cache — including a cache
            // from a run with different sanitizer flags.
            //
            // One kept tree per project AND build setting, in the
            // per-user state directory, so the next build of the same project
            // with the same flags and compilers is incremental while a run with
            // other flags never shares a cache. Sessions take turns on it under
            // the project lock. No state directory: a throwaway temp dir, as
            // before.
            let setting = format!(
                "sanitize {sanitizer} {flags} CC={} CXX={}",
                std::env::var("CC").unwrap_or_default(),
                std::env::var("CXX").unwrap_or_default(),
            );
            let build_dir = crate::builds::stable_build_dir(&root, &setting)
                .unwrap_or_else(|| unique_tmp_path("byteask-cpp-sanitize-build", ""));
            return Ok(ProjectPlan {
                kind: ProjectKind::Cmake,
                build_file: cmake,
                build_dir,
                root,
                target: args.target.clone(),
                flags,
                sanitizer: sanitizer.to_string(),
                jobs: project_build_jobs(),
            });
        }

        let makefile = MAKEFILE_NAMES
            .iter()
            .map(|name| root.join(name))
            .find(|candidate| candidate.is_file());
        if let Some(makefile) = makefile {
            if !args.allow_in_tree_build {
                return Err(FunctionCallError::RespondToModel(in_tree_refusal(
                    &makefile,
                )));
            }
            let build_dir = root.clone();
            return Ok(ProjectPlan {
                kind: ProjectKind::Make,
                build_file: makefile,
                build_dir,
                root,
                target: args.target.clone(),
                flags,
                sanitizer: sanitizer.to_string(),
                jobs: project_build_jobs(),
            });
        }

        Err(FunctionCallError::RespondToModel(no_build_system_message(
            &root,
        )))
    }

    /// Build the project, find what it produced, run it, and report through the
    /// SAME verdict/finding/trace pipeline the single-TU paths use.
    async fn run_project(
        &self,
        plan: &ProjectPlan,
        args: &ProjectArgs,
        run_args: &[String],
    ) -> Result<(String, CheckResult), FunctionCallError> {
        // An in-tree `make` writes the project's objects and program, so
        // two calls on one tree (the model runs tools in parallel) link each
        // other's objects — asan+tsan failed 6/6 with the OTHER sanitizer's
        // symbols — or fail `Text file busy`. One build+run per project root,
        // across every session of this user: the kept CMake tree is
        // shared too.
        let _project =
            match crate::builds::project_lock(&plan.root, compile_timeout_from(args.timeout_sec))
                .await
            {
                Ok(guard) => guard,
                Err(busy) => {
                    return Ok((
                        format!("{}\n{}", plan.origin(), busy.summary()),
                        CheckResult::not_run(
                            TOOL_NAME,
                            NotRunReason::TimeLimit,
                            &busy.summary(),
                            crate::builds::ProjectBusy::FIX,
                        ),
                    ));
                }
            };
        if crate::builds::is_stable_build_dir(&plan.build_dir) {
            // Old kept trees of OTHER projects are removed in the background;
            // cheap unless something is past its keep time.
            let keep = plan.root.clone();
            drop(tokio::task::spawn_blocking(move || {
                crate::builds::prune_stale_builds(&keep, SystemTime::now())
            }));
        }
        let started = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(MTIME_SLACK_SECS))
            .unwrap_or(UNIX_EPOCH);

        // One engine-started build at a time per session (finish gate, test selection,
        // sub-agent checks); the wait is bounded by this call's own build timeout.
        let Some(slot) =
            crate::resources::build_slot(crate::run::compile_timeout_from(args.timeout_sec)).await
        else {
            return Ok((
                format!(
                    "{}\nPROJECT BUILD: not started — another ByteAsk build or test run was still in progress when this call's time limit ran out; nothing was built. Call again when it has finished.",
                    plan.origin()
                ),
                CheckResult::not_run(
                    TOOL_NAME,
                    NotRunReason::TimeLimit,
                    "Another ByteAsk build or test run held the build slot for this call's whole time limit, so nothing was built.",
                    "call again when the other build has finished.",
                ),
            ));
        };
        let step = match plan.kind {
            ProjectKind::Cmake => self.build_cmake(plan, args.timeout_sec).await?,
            ProjectKind::Make => self.build_make(plan, args.timeout_sec, started).await?,
        };
        drop(slot);
        let (weakened, partial, notes) = match step {
            BuildStep::Aborted(text, result) => return Ok((text, result)),
            BuildStep::Ok {
                weakened,
                partial,
                notes,
            } => (weakened, partial, notes),
        };

        let mut preamble = plan.origin();
        for note in &notes {
            preamble.push('\n');
            preamble.push_str(note);
        }

        // A Makefile build reuses whatever is already lying in the tree, so only
        // a file this build actually wrote can be the program under test. A
        // CMake build is in a directory that did not exist a moment ago.
        let newer_than = match plan.kind {
            ProjectKind::Make => Some(started),
            ProjectKind::Cmake => None,
        };
        let bin = match locate_executable(plan, newer_than) {
            Ok(bin) => bin,
            Err(message) => {
                return Ok((
                    format!("{preamble}\n{message}"),
                    CheckResult::not_run(
                        TOOL_NAME,
                        NotRunReason::ToolError,
                        "The build finished but the program to run could not be identified, so nothing was checked.",
                        "do what the message above says (usually: name the executable), then call cpp_sanitize again.",
                    ),
                ));
            }
        };

        preamble.push_str(&format!("\nran: {}", bin.display()));
        preamble.push('\n');

        self.run_project_binary(
            plan,
            &bin,
            run_args,
            args.timeout_sec,
            &preamble,
            (&weakened, &partial),
        )
        .await
    }

    /// Configure a fresh build tree with our flags, read back what the compiler
    /// will really be told, then build.
    async fn build_cmake(
        &self,
        plan: &ProjectPlan,
        timeout_sec: Option<u64>,
    ) -> Result<BuildStep, FunctionCallError> {
        let origin = plan.origin();

        let configure_argv = plan.cmake_configure_argv();
        // A kept tree whose cache no longer configures (another
        // generator or compiler since last time) is rebuilt from scratch ONCE,
        // so a stale cache of ours never reads as the project's build failing.
        let reused = crate::builds::is_stable_build_dir(&plan.build_dir)
            && plan.build_dir.join("CMakeCache.txt").is_file();
        let mut configure = run_capped(
            "cmake",
            &configure_argv,
            &plan.root,
            compile_timeout_from(timeout_sec),
        )
        .await?;
        if reused && matches!(configure, RunOutcome::Finished { success: false, .. }) {
            let _ = std::fs::remove_dir_all(&plan.build_dir);
            configure = run_capped(
                "cmake",
                &configure_argv,
                &plan.root,
                compile_timeout_from(timeout_sec),
            )
            .await?;
        }
        match configure {
            RunOutcome::TimedOut { secs } => {
                return Ok(BuildStep::aborted_timeout(project_timeout_message(
                    &origin,
                    "CMAKE CONFIGURE",
                    secs,
                )));
            }
            RunOutcome::Finished {
                success: false,
                stdout,
                stderr,
                ..
            } => {
                return Ok(BuildStep::aborted_build(project_build_failure(
                    &origin,
                    "CMAKE CONFIGURE",
                    &stdout,
                    &stderr,
                    plan.kind,
                )));
            }
            RunOutcome::Finished { .. } => {}
        }

        // The generated compile database holds the REAL compile line for every
        // translation unit in this build — the only place the project's own
        // `-fno-sanitize=` is visible BEFORE we write a verdict from it.
        let db = std::fs::read_to_string(plan.build_dir.join("compile_commands.json"))
            .unwrap_or_default();
        let audit = audit_commands(&compile_db_commands(&db), &plan.sanitizer);
        let mut notes = Vec::new();
        if db.trim().is_empty() {
            notes.push(
                "instrumentation: UNVERIFIED — this generator produced no compile_commands.json, \
                 so the real compile lines could not be read back."
                    .to_string(),
            );
        } else {
            notes.push(audit.summary(&plan.sanitizer));
        }

        let build_argv = plan.cmake_build_argv();
        let build = run_capped(
            "cmake",
            &build_argv,
            &plan.root,
            compile_timeout_from(timeout_sec),
        )
        .await?;
        match build {
            RunOutcome::TimedOut { secs } => Ok(BuildStep::aborted_timeout(
                project_timeout_message(&origin, "PROJECT BUILD", secs),
            )),
            RunOutcome::Finished {
                success: false,
                stdout,
                stderr,
                ..
            } => Ok(BuildStep::aborted_build(project_build_failure(
                &origin,
                "PROJECT BUILD",
                &stdout,
                &stderr,
                plan.kind,
            ))),
            RunOutcome::Finished { .. } => Ok(BuildStep::Ok {
                weakened: audit.weakened(&plan.sanitizer),
                partial: audit.partial(&plan.sanitizer).into_iter().collect(),
                notes,
            }),
        }
    }

    /// Best effort, and it says so. `make CXXFLAGS=…` overrides a plain
    /// assignment in the makefile, but NOT `override CXXFLAGS`, and not a recipe
    /// that spells its own compile line — so the dry run is what turns "we hope"
    /// into "we checked".
    async fn build_make(
        &self,
        plan: &ProjectPlan,
        timeout_sec: Option<u64>,
        started: SystemTime,
    ) -> Result<BuildStep, FunctionCallError> {
        let origin = plan.origin();
        let mut notes = vec![MAKE_CAVEAT.to_string()];
        let mut weakened = Vec::new();
        let mut partial = Vec::new();
        // The object files and archives the build links, per its dry run.
        let mut linked = Vec::new();
        let compile_flags_matter = sanitizer_probe_flag(&plan.sanitizer).is_some();

        let inherited = self.makefile_flag_values(plan).await;
        if !inherited.is_empty() {
            let names: Vec<&str> = inherited.iter().map(|(name, _)| name.as_str()).collect();
            notes.push(format!(
                "kept the makefile's own {} and appended our flags after them.",
                names.join(" / ")
            ));
        }
        let dry_argv = plan.make_argv(true, &inherited);
        let dry = run_capped(
            "make",
            &dry_argv,
            &plan.root,
            timeout_from(Some(MAKE_DRY_RUN_TIMEOUT_SECS)),
        )
        .await?;
        match dry {
            RunOutcome::Finished {
                success: true,
                stdout,
                ..
            } => {
                let audit = audit_commands(&make_dry_run_commands(&stdout), &plan.sanitizer);
                notes.push(audit.summary(&plan.sanitizer));
                weakened.extend(audit.weakened(&plan.sanitizer));
                partial.extend(audit.partial(&plan.sanitizer));
                linked = linked_inputs(&stdout, &plan.root);
                if make_nothing_to_do(&stdout) {
                    // The binary we would run predates this call and was built
                    // without our flags. Reporting it clean is the exact lie
                    // this tool exists to prevent.
                    weakened.push(
                        "make has nothing to rebuild, so the binary here is left over from an \
                         EARLIER, uninstrumented build — run `make clean` first"
                            .to_string(),
                    );
                }
            }
            _ => {
                notes.push(
                    "instrumentation: UNVERIFIED — `make -n` did not complete, so the real \
                     compile lines could not be read back."
                        .to_string(),
                );
                // UNVERIFIED in the preamble must never sit above a "clean"
                // verdict.
                if compile_flags_matter {
                    weakened.push(
                        "`make -n` did not complete, so no compile line of this build could be \
                         read back"
                            .to_string(),
                    );
                }
            }
        }

        let build_argv = plan.make_argv(false, &inherited);
        let build = run_capped(
            "make",
            &build_argv,
            &plan.root,
            compile_timeout_from(timeout_sec),
        )
        .await?;
        match build {
            RunOutcome::TimedOut { secs } => Ok(BuildStep::aborted_timeout(
                project_timeout_message(&origin, "PROJECT BUILD", secs),
            )),
            RunOutcome::Finished {
                success: false,
                stdout,
                stderr,
                ..
            } => Ok(BuildStep::aborted_build(project_build_failure(
                &origin,
                "PROJECT BUILD",
                &stdout,
                &stderr,
                plan.kind,
            ))),
            RunOutcome::Finished { .. } => {
                // make reuses every object it considers up to date, so a
                // file compiled by an EARLIER build without our flags is linked
                // straight in — measured: "2 of 2 compile command(s) carry",
                // then "clean", with the overflow in the stale util.o.
                let stale = stale_files(&linked, started, &plan.root);
                if compile_flags_matter && !stale.is_empty() {
                    partial.push(format!(
                        "{} {} linked in without being rebuilt by this build, so {} compiled \
                         WITHOUT our flags by an earlier one (run `make clean` first to rebuild \
                         everything)",
                        name_list(&stale),
                        if stale.len() == 1 { "was" } else { "were" },
                        if stale.len() == 1 {
                            "it was"
                        } else {
                            "they were"
                        },
                    ));
                }
                Ok(BuildStep::Ok {
                    weakened,
                    partial,
                    notes,
                })
            }
        }
    }

    /// The values the makefile (or the environment) gives CFLAGS / CXXFLAGS /
    /// LDFLAGS, from make's own variable database (`make -p`, with a goal that
    /// does not exist so no recipe runs). Empty when make cannot say.
    async fn makefile_flag_values(&self, plan: &ProjectPlan) -> Vec<(String, String)> {
        let argv = vec![
            "-p".to_string(),
            "-q".to_string(),
            "-C".to_string(),
            plan.root.display().to_string(),
            "__byteask_print_database__".to_string(),
        ];
        match run_capped(
            "make",
            &argv,
            &plan.root,
            timeout_from(Some(MAKE_DRY_RUN_TIMEOUT_SECS)),
        )
        .await
        {
            Ok(RunOutcome::Finished { stdout, .. }) => makefile_flag_values(&stdout),
            _ => Vec::new(),
        }
    }

    /// Run the project's own binary under the chosen checker.
    ///
    /// Deliberately the same parse → verdict → trace pipeline as the single-TU
    /// paths, so a project-mode report reads identically: same ASan/UBSan/TSan
    /// parsing, same truncation guard, same "no race in THIS run" wording.
    async fn run_project_binary(
        &self,
        plan: &ProjectPlan,
        bin: &Path,
        run_args: &[String],
        timeout_sec: Option<u64>,
        preamble: &str,
        (weakened, partial): (&[String], &[String]),
    ) -> Result<(String, CheckResult), FunctionCallError> {
        if plan.sanitizer == "valgrind" {
            let xml_path = unique_tmp_path("byteask-cpp-valgrind", ".xml");
            let (run, xml) = self
                .run_memcheck(bin, run_args, timeout_sec, &xml_path)
                .await?;
            let (stdout, stderr, exit_label, truncated) = match run {
                RunOutcome::TimedOut { secs } => {
                    return Ok((run_timeout_message(preamble, secs), run_timeout_result()));
                }
                RunOutcome::Finished {
                    code,
                    stdout,
                    stderr,
                    truncated,
                    ..
                } => (stdout, stderr, exit_label_of(code), truncated),
            };

            let findings = parse_valgrind_xml(&xml);
            let verdict =
                valgrind_verdict(&findings, &exit_label, truncated, xml_is_complete(&xml));
            let verdict = qualify_verdict(&verdict, weakened, partial);
            let result = qualify_result(
                valgrind_result(&findings, &exit_label, truncated, xml_is_complete(&xml)),
                weakened,
                partial,
            );
            let trace = prepare_trace(&stderr);
            return Ok((
                format_run_output(preamble, &verdict, &stdout, &findings, &trace),
                result,
            ));
        }

        let env = if plan.sanitizer == "tsan" {
            tsan_env()
        } else {
            sanitizer_env()
        };
        let program = bin.display().to_string();
        let run = run_capped_env(
            &program,
            run_args,
            &self.cwd,
            timeout_from(timeout_sec),
            &env,
        )
        .await?;
        let (stdout, stderr, exit_label, truncated) = match run {
            RunOutcome::TimedOut { secs } => {
                return Ok((run_timeout_message(preamble, secs), run_timeout_result()));
            }
            RunOutcome::Finished {
                code,
                stdout,
                stderr,
                truncated,
                ..
            } => (stdout, stderr, exit_label_of(code), truncated),
        };

        let preamble = format!("{preamble}{}", symbolizer_note(&stderr));
        let preamble = preamble.as_str();
        let mut findings = Vec::new();
        let (verdict, result) = if plan.sanitizer == "tsan" {
            if let Some(f) = parse_tsan_report(&stderr) {
                findings.push(f);
            }
            (
                tsan_verdict(&stderr, &exit_label, truncated),
                tsan_result(&stderr, &exit_label, truncated),
            )
        } else {
            let reports = parse_asan_ubsan(&stderr);
            let verdict = asan_ubsan_verdict(&reports, &exit_label, truncated);
            let result = findings_run_result(
                &reports.findings,
                &exit_label,
                truncated,
                leak_detection_available(),
            );
            findings = reports.findings;
            (verdict, result)
        };
        let verdict = qualify_verdict(&verdict, weakened, partial);
        let result = qualify_result(result, weakened, partial);
        let trace = prepare_trace(&stderr);
        let evidence = if plan.sanitizer == "tsan" {
            String::new()
        } else {
            crate::memsafety::evidence::render_from(&stderr)
        };
        Ok((
            format_run_output_with_evidence(
                preamble, &verdict, &stdout, &findings, &evidence, &trace,
            ),
            result,
        ))
    }
}

/// The narrow, allowlisted set of build inputs `cpp_sanitize` will act on,
/// whichever source they came from.
#[derive(Debug, PartialEq, Eq)]
struct BuildPlan {
    compiler: String,
    std: String,
    /// `std` is our default, not the caller's or the compile database's, so a
    /// compiler that does not know it may be given its own name for it.
    std_is_default: bool,
    include_dirs: Vec<String>,
    defines: Vec<String>,
    /// Target-ISA / codegen flags, unioned across every translation unit.
    march: Vec<String>,
    /// Meaning-changing flags from the compile database, see
    /// [`DbFlags::semantics`].
    semantics: Vec<String>,
    sources: Vec<PathBuf>,
    /// One line of provenance shown to the model, so it can tell a compile-DB
    /// build from a guessed one.
    origin: String,
}

impl BuildPlan {
    /// The project-context half of the compile line. The sanitizer flags are
    /// appended by the caller and are never derived from user input.
    fn compile_argv(&self) -> Vec<String> {
        self.compile_argv_with_std(&self.std)
    }

    fn compile_argv_with_std(&self, std: &str) -> Vec<String> {
        let mut argv = vec![format!("-std={std}")];
        for dir in &self.include_dirs {
            argv.push(format!("-I{dir}"));
        }
        for def in &self.defines {
            argv.push(format!("-D{def}"));
        }
        for m in &self.march {
            argv.push(m.clone());
        }
        argv.extend(self.semantics.iter().cloned());
        argv
    }
}

/// Flags lifted out of a compile database entry — an allowlist: a compile DB
/// is trusted for project context, never for anything that could change what
/// the sanitizers check.
#[derive(Debug, Default, PartialEq, Eq)]
struct DbFlags {
    std: Option<String>,
    include_dirs: Vec<String>,
    defines: Vec<String>,
    /// Target-ISA / codegen flags (`-march=`, `-mavx2`, `-mfma`, `-m64`, ...).
    march: Vec<String>,
    /// Flags that change what the PROGRAM MEANS, as argv tokens:
    /// `-fwrapv`, `-f[no-]signed-char`/`-f[no-]unsigned-char`, `-U<name>`,
    /// `-include <file>`, `-imacros <file>`. Dropping them built a different
    /// program: measured, a false "signed integer overflow" (the project uses
    /// -fwrapv), `char` signed where the project makes it unsigned, and a
    /// forced include never seen. None of them can switch a sanitizer off.
    semantics: Vec<String>,
}

// ==========================================================================
// Project mode
// ==========================================================================

/// Which build system project mode is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectKind {
    Cmake,
    Make,
}

/// A project-mode build, resolved before anything is spawned so every argument
/// vector is unit-testable without a compiler on the machine.
#[derive(Debug, PartialEq, Eq)]
struct ProjectPlan {
    kind: ProjectKind,
    /// The `CMakeLists.txt` / `Makefile` we found, for the provenance line.
    build_file: PathBuf,
    /// The workspace root: where the build is driven from, and where the
    /// instrumented program is later run.
    root: PathBuf,
    /// Where objects and binaries land. For CMake, the project's kept tree for
    /// these flags in the per-user state directory, or a fresh temp dir
    /// when there is none; the project root for Make, which has no out-of-tree
    /// mode.
    build_dir: PathBuf,
    target: Option<String>,
    /// The sanitizer flags we inject, as one string. Ours, fixed, and never
    /// derived from project or model input — see [`sanitizer_build_flags`].
    flags: String,
    sanitizer: String,
    /// Parallel build jobs, fixed when the plan is made ([`project_build_jobs`]).
    jobs: String,
}

/// Outcome of the build phase.
#[derive(Debug)]
enum BuildStep {
    /// The build ran. `weakened` is every reason its verdict must not read as
    /// clean; `partial` every part of the program this build did NOT instrument
    /// (stale objects, uninstrumented sources); `notes` are provenance lines for the preamble.
    Ok {
        weakened: Vec<String>,
        partial: Vec<String>,
        notes: Vec<String>,
    },
    /// Nothing was run (configure/build failed or timed out). The payload is the
    /// complete model-facing text and its shared result.
    Aborted(String, CheckResult),
}

impl BuildStep {
    fn aborted_timeout(text: String) -> Self {
        BuildStep::Aborted(text, build_timeout_result())
    }

    fn aborted_build(text: String) -> Self {
        BuildStep::Aborted(text, build_failed_result())
    }
}

/// What the REAL compile lines of a project build turned out to carry.
#[derive(Debug, Default, PartialEq, Eq)]
struct Instrumentation {
    /// Compiler invocations examined that COMPILE a source (a link-only
    /// line compiles nothing, so carrying our flag proves nothing about the
    /// code it links).
    total: usize,
    /// …of which carry our sanitizer flag verbatim.
    instrumented: usize,
    /// Distinct tokens seen that switch our sanitizer off, or conflict with it.
    defeating: Vec<String>,
    /// The sources compiled WITHOUT our flag.
    uninstrumented: Vec<String>,
}

/// The standing warning on every Makefile build. Stated up front rather than
/// implied, because `make` is the one path here whose instrumentation this tool
/// cannot guarantee.
const MAKE_CAVEAT: &str = "caveat: a Makefile build cannot be GUARANTEED instrumented. A command-line CFLAGS/CXXFLAGS \
     assignment overrides a plain assignment in the makefile, but not `override CXXFLAGS`, and \
     not a recipe that spells out its own compile line. Our flags are appended to the makefile's \
     own CFLAGS / CXXFLAGS / LDFLAGS as `make -p` reports them. The \
     `make -n` audit below is what was actually checked. When the verdict has to be trustworthy, \
     prefer a CMake project or cpp_sanitize's single-translation-unit form.";

impl ProjectPlan {
    /// `cmake -S <root> -B <fresh tmp dir> …`
    ///
    /// Every `-D…_FLAGS=` value is exactly [`ProjectPlan::flags`] — a string
    /// built by [`sanitizer_build_flags`] with nothing from the project or the
    /// model spliced into it. That is what "ours last" means here: no caller can
    /// place a flag after our `-fsanitize=` inside these values and win the
    /// compiler's last-wins rule. A project can still append to them through its
    /// own `target_compile_options`, which is precisely why the generated
    /// compile database is read back afterwards.
    fn cmake_configure_argv(&self) -> Vec<String> {
        let mut argv = vec![
            "-S".to_string(),
            self.root.display().to_string(),
            "-B".to_string(),
            self.build_dir.display().to_string(),
            "-DCMAKE_BUILD_TYPE=Debug".to_string(),
            "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string(),
        ];
        for var in [
            "CMAKE_C_FLAGS",
            "CMAKE_CXX_FLAGS",
            "CMAKE_EXE_LINKER_FLAGS",
            "CMAKE_SHARED_LINKER_FLAGS",
        ] {
            argv.push(format!("-D{var}={}", self.flags));
        }
        argv
    }

    fn cmake_build_argv(&self) -> Vec<String> {
        let mut argv = vec![
            "--build".to_string(),
            self.build_dir.display().to_string(),
            "--parallel".to_string(),
            self.jobs.clone(),
        ];
        if let Some(target) = &self.target {
            // A CMake target is a NAME; `bin/app` names the file it produces.
            argv.push("--target".to_string());
            argv.push(target.rsplit('/').next().unwrap_or(target).to_string());
        }
        argv
    }

    /// `make -C <root> CFLAGS=… CXXFLAGS=… LDFLAGS=… [target]`, or the same
    /// thing under `-n` (print the recipes, build nothing) for the audit.
    fn make_argv(&self, dry_run: bool, inherited: &[(String, String)]) -> Vec<String> {
        let mut argv = Vec::new();
        if dry_run {
            argv.push("-n".to_string());
        }
        argv.push("-C".to_string());
        argv.push(self.root.display().to_string());
        if !dry_run {
            argv.push("-j".to_string());
            argv.push(self.jobs.clone());
        }
        for var in ["CFLAGS", "CXXFLAGS", "LDFLAGS"] {
            // A command-line assignment REPLACES the makefile's own value, so
            // that value (read from `make -p`) goes first and ours last: the
            // build keeps its -I / -std / -l flags and our -fsanitize / -O0
            // still win the compiler's last-wins rule.
            match inherited.iter().find(|(name, _)| name == var) {
                Some((_, value)) => argv.push(format!("{var}={value} {}", self.flags)),
                None => argv.push(format!("{var}={}", self.flags)),
            }
        }
        if let Some(target) = &self.target {
            argv.push(target.clone());
        }
        argv
    }

    fn target_phrase(&self) -> String {
        match &self.target {
            Some(target) => format!("target {target}"),
            None => "the default target".to_string(),
        }
    }

    fn banner_target(&self) -> String {
        match &self.target {
            Some(target) => format!("project target {target}"),
            None => "project".to_string(),
        }
    }

    /// The provenance line, in the same `build: …` shape [`describe_origin`]
    /// uses, so the model can tell a project build from a single-TU build at a
    /// glance.
    fn origin(&self) -> String {
        match self.kind {
            ProjectKind::Cmake => format!(
                "build: PROJECT mode — CMake ({}), configured in {} (this project's own \
                 build tree for these flags, kept outside your workspace so the next build is \
                 incremental) with CMAKE_BUILD_TYPE=Debug and our sanitizer flags [{}] injected via \
                 CMAKE_C_FLAGS / CMAKE_CXX_FLAGS / CMAKE_EXE_LINKER_FLAGS / \
                 CMAKE_SHARED_LINKER_FLAGS; built -j{}, {}.",
                self.build_file.display(),
                self.build_dir.display(),
                self.flags,
                self.jobs,
                self.target_phrase(),
            ),
            ProjectKind::Make => format!(
                "build: PROJECT mode — Makefile ({}), built IN-TREE (object files and binaries \
                 are written into your working tree) with our sanitizer flags [{}] passed as \
                 CFLAGS / CXXFLAGS / LDFLAGS; built -j{}, {}.",
                self.build_file.display(),
                self.flags,
                self.jobs,
                self.target_phrase(),
            ),
        }
    }
}

impl Instrumentation {
    /// One provenance line saying what was checked, and what it showed.
    fn summary(&self, sanitizer: &str) -> String {
        let Some(probe) = sanitizer_probe_flag(sanitizer) else {
            return format!(
                "instrumentation: n/a — valgrind instruments the binary at RUN time, so no \
                 compile flag can switch it off ({} compiler command(s) read back).",
                self.total
            );
        };
        if self.total == 0 {
            return "instrumentation: UNVERIFIED — no compile command lines could be read back."
                .to_string();
        }
        let mut line = format!(
            "instrumentation: {} of {} compile command(s) carry {probe}",
            self.instrumented, self.total
        );
        if !self.defeating.is_empty() {
            line.push_str(&format!(
                "; the project's own flags include {}, which switch it off or conflict with it",
                self.defeating.join(" ")
            ));
        }
        line.push('.');
        line
    }

    /// Every reason a verdict computed from this build must not read as clean.
    ///
    /// A PARTIALLY instrumented build is not condemned here — see
    /// [`Instrumentation::partial`]. Zero instrumented commands, no command
    /// read back at all, or a flag that actively disables the checker is a
    /// different thing entirely.
    fn weakened(&self, sanitizer: &str) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.defeating.is_empty() {
            reasons.push(format!(
                "the project's own compile line carries {}",
                self.defeating.join(" ")
            ));
        }
        let Some(probe) = sanitizer_probe_flag(sanitizer) else {
            return reasons;
        };
        if self.total == 0 {
            // Never "instrumentation: UNVERIFIED" in the preamble and
            // then "clean" in the verdict — e.g. a Makefile whose recipe calls
            // its own `./cxx.sh -O2` wrapper, which no audit recognises.
            reasons.push(format!(
                "no compile command in this build could be read back, so nothing shows {probe} \
                 ever reached the compiler"
            ));
        } else if self.instrumented == 0 {
            reasons.push(format!(
                "not one of the {} compile command(s) in this build carries {probe}",
                self.total
            ));
        }
        reasons
    }

    /// Some, but not all, compile commands carry our flag. The sanitizer
    /// simply does not check what it did not instrument: a finding is still
    /// real, but the absence of one says nothing about the files named here —
    /// measured: a CMake project whose `lib/` resets CMAKE_CXX_FLAGS read "1 of
    /// 2 carry" and then "clean: exit 0" with the overflow in that lib.
    fn partial(&self, sanitizer: &str) -> Option<String> {
        let probe = sanitizer_probe_flag(sanitizer)?;
        (self.instrumented > 0 && self.instrumented < self.total).then(|| {
            format!(
                "only {} of {} compile command(s) in this build carry {probe}, so {} {} NOT \
                 instrumented",
                self.instrumented,
                self.total,
                name_list(&self.uninstrumented),
                if self.uninstrumented.len() == 1 {
                    "was"
                } else {
                    "were"
                },
            )
        })
    }
}

/// CFLAGS / CXXFLAGS / LDFLAGS as defined by the makefile or the environment,
/// parsed from `make -p` output (`# makefile (from 'Makefile', line 3)` or
/// `# environment`, then `NAME = value` / `NAME := value`). `override`
/// variables are skipped: a command line cannot replace them anyway. A simply
/// expanded (`:=`) value is already expanded, so its `$` is escaped before it
/// is handed back as a recursively expanded command-line value.
fn makefile_flag_values(database: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut origin_ok = false;
    for line in database.lines() {
        if let Some(comment) = line.strip_prefix("# ") {
            origin_ok = comment.starts_with("makefile") || comment == "environment";
            continue;
        }
        if !origin_ok {
            continue;
        }
        origin_ok = false;
        for var in ["CFLAGS", "CXXFLAGS", "LDFLAGS"] {
            let Some(rest) = line.strip_prefix(var) else {
                continue;
            };
            let (value, simple) = if let Some(v) = rest.strip_prefix(" := ") {
                (v, true)
            } else if let Some(v) = rest.strip_prefix(" :: ") {
                (v, true)
            } else if let Some(v) = rest.strip_prefix(" = ") {
                (v, false)
            } else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() || out.iter().any(|(name, _)| name == var) {
                continue;
            }
            let value = if simple {
                value.replace('$', "$$")
            } else {
                value.to_string()
            };
            out.push((var.to_string(), value));
        }
    }
    out
}

/// The flags that turn a build into an instrumented one — for a project's own
/// build AND for the single-TU paths, which take them from here too, so a
/// project build checks exactly what a single-TU build checks.
///
/// Fixed, ours, and never derived from project input.
///
/// Always `-O0`. The optimizer may legally delete what a sanitizer exists
/// to catch — undefined or unobservable behaviour — and an access that was
/// deleted cannot be reported. Measured at the old `-O1`: g++ reported a store
/// after `free()`, a double `free()` and a leaked `malloc` all "clean: exit 0"
/// (a double free and a leak were clean under clang++ too, as was a
/// use-after-scope write), and an unused racy read vanished before TSan saw it
/// under both compilers. `-O1 -fno-builtin-malloc -fno-builtin-free` still
/// missed four of fifteen textbook cases; `-O0` missed none. The price is
/// speed (STL-heavy code ran 7x slower under ASan, 10x under TSan), which is
/// why the run-timeout message says so. `-fno-omit-frame-pointer` is implied
/// by `-O0` and kept so the set stays correct if the level ever changes.
fn sanitizer_build_flags(sanitizer: &str) -> Vec<&'static str> {
    match sanitizer {
        "tsan" => vec!["-fsanitize=thread", "-fno-omit-frame-pointer", "-g", "-O0"],
        // MemorySanitizer with origin tracking: a report then says where the
        // uninitialised value was created, not only where it was used.
        "msan" => vec![
            "-fsanitize=memory",
            "-fsanitize-memory-track-origins=2",
            "-fno-omit-frame-pointer",
            "-g",
            "-O0",
        ],
        // valgrind needs symbols and no optimisation, nothing more: it
        // instruments the finished binary at run time.
        "valgrind" => vec!["-g", "-O0"],
        _ => vec![
            "-fsanitize=address,undefined",
            "-fno-omit-frame-pointer",
            "-g",
            "-O0",
        ],
    }
}

/// The one flag whose presence on a REAL compile line proves instrumentation.
/// `None` for valgrind, which has nothing to prove at compile time.
fn sanitizer_probe_flag(sanitizer: &str) -> Option<&'static str> {
    match sanitizer {
        "tsan" => Some("-fsanitize=thread"),
        "msan" => Some("-fsanitize=memory"),
        "valgrind" => None,
        _ => Some("-fsanitize=address,undefined"),
    }
}

/// Tokens on a REAL compile line that switch our sanitizer off, or conflict
/// with it.
///
/// This is project mode's half of "a clean verdict cannot be faked". The
/// single-TU path enforces it by construction — [`extract_db_flags`] copies
/// nothing outside the `-I`/`-D`/`-std`/target-ISA allowlist, so a project's
/// `-fno-sanitize=all` never reaches the compile line. A project build runs the
/// project's own compile lines and we cannot filter them; what we can do is read
/// them and refuse to call the result clean.
fn sanitizer_defeating_tokens(tokens: &[String], sanitizer: &str) -> Vec<String> {
    // The checks our own flags asked for.
    let ours: &[&str] = match sanitizer {
        "tsan" => &["thread"],
        "msan" => &["memory"],
        "valgrind" => &[],
        _ => &["address", "undefined"],
    };
    // Sanitizers that cannot coexist with ours in one binary.
    const EXCLUSIVE: [&str; 3] = ["address", "memory", "thread"];

    let mut hits = Vec::new();
    for token in tokens {
        let token = token.as_str();

        // A conflicting sanitizer matters for EVERY checker, valgrind included:
        // two -fsanitize= families cannot coexist in one binary, and an
        // ASan-instrumented binary is not one memcheck can read.
        if let Some(value) = token.strip_prefix("-fsanitize=") {
            if value
                .split(',')
                .any(|v| EXCLUSIVE.contains(&v) && !ours.contains(&v))
            {
                push_unique(&mut hits, token);
            }
            continue;
        }

        // Everything below can only weaken a COMPILE-time sanitizer. valgrind
        // has none, so `ours` is empty there and a project's -fno-sanitize=all
        // is genuinely harmless — flagging it would be a false alarm on every
        // valgrind run.
        if ours.is_empty() {
            continue;
        }

        if token.starts_with("-mllvm") {
            // Forwards an arbitrary option to the LLVM backend — e.g.
            // -tsan-instrument-memory-accesses=0. Same reasoning as
            // `is_safe_target_flag`, arriving through the project's own build.
            push_unique(&mut hits, token);
        } else if token == "-fno-sanitize" {
            push_unique(&mut hits, token);
        } else if let Some(value) = token.strip_prefix("-fno-sanitize=") {
            // NOTE the `=`: `-fno-sanitize-recover=undefined` is a recovery
            // policy, not a disable, and must not be flagged as one.
            if value.split(',').any(|v| v == "all" || ours.contains(&v)) {
                push_unique(&mut hits, token);
            }
        } else if token.starts_with("-fsanitize-ignorelist=")
            || token.starts_with("-fsanitize-blacklist=")
        {
            // Suppresses instrumentation for whatever the list names, which may
            // be exactly the file under suspicion.
            push_unique(&mut hits, token);
        } else if let Some(value) = token.strip_prefix("-fsanitize-trap=") {
            // Traps instead of reporting: the process dies on SIGILL and there
            // is no report left to parse.
            if value.split(',').any(|v| v == "all" || ours.contains(&v)) {
                push_unique(&mut hits, token);
            }
        }
    }
    hits
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    if !out.iter().any(|existing| existing.as_str() == value) {
        out.push(value.to_string());
    }
}

/// Strip directories and a `.exe` suffix from an argv[0].
fn basename_no_exe(token: &str) -> &str {
    let base = token
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(token);
    base.strip_suffix(".exe").unwrap_or(base)
}

/// Is this argv a compiler invocation, and if so where does the compiler start?
/// Skips the wrappers a real recipe line puts in front (`ccache g++ …`) and
/// accepts the cross/versioned spellings (`clang++-17`, `x86_64-linux-gnu-gcc`).
fn compiler_tokens(tokens: &[String]) -> Option<&[String]> {
    let mut rest = tokens;
    loop {
        let first = rest.first()?;
        let base = basename_no_exe(first);
        if COMPILER_WRAPPERS.contains(&base) {
            rest = rest.get(1..)?;
            continue;
        }
        let is_compiler = COMPILER_BASENAMES.iter().any(|name| {
            base == *name
                || base.starts_with(&format!("{name}-"))
                || base.ends_with(&format!("-{name}"))
        });
        return if is_compiler { Some(rest) } else { None };
    }
}

/// Read a set of real command lines and report what they do to our sanitizer.
fn audit_commands(commands: &[Vec<String>], sanitizer: &str) -> Instrumentation {
    let probe = sanitizer_probe_flag(sanitizer);
    let mut audit = Instrumentation::default();
    for command in commands {
        let Some(tokens) = compiler_tokens(command) else {
            continue;
        };
        // Any compiler line can carry a defeating flag, a link line included.
        for hit in sanitizer_defeating_tokens(tokens, sanitizer) {
            push_unique(&mut audit.defeating, hit.as_str());
        }
        // Only a line that COMPILES code can instrument it. make_argv
        // passes our flags in LDFLAGS too, so a Makefile that compiles with a
        // hardcoded `-O2 -c` and links with $(LDFLAGS) read "1 of 3 compiler
        // command(s) carry" — the link — and then "clean" with no translation
        // unit instrumented at all.
        let sources = compiled_sources(tokens);
        if sources.is_empty() && !tokens.iter().any(|token| token == "-c") {
            continue;
        }
        audit.total += 1;
        if probe.is_some_and(|flag| tokens.iter().any(|token| token.as_str() == flag)) {
            audit.instrumented += 1;
        } else {
            for source in sources {
                push_unique(&mut audit.uninstrumented, source);
            }
        }
    }
    audit
}

/// The C/C++ sources a compiler command line compiles (the operands with a
/// source extension; the value of `-o` is an output, never a source).
fn compiled_sources(tokens: &[String]) -> Vec<&str> {
    const SOURCE_EXTS: &[&str] = &["c", "cc", "cp", "cxx", "cpp", "CPP", "c++", "C", "i", "ii"];
    let mut sources = Vec::new();
    let mut after_output_flag = false;
    for token in tokens.iter().skip(1) {
        let token = token.as_str();
        if std::mem::take(&mut after_output_flag) {
            continue;
        }
        if token == "-o" {
            after_output_flag = true;
            continue;
        }
        let is_source = !token.starts_with('-')
            && Path::new(token)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| SOURCE_EXTS.contains(&ext));
        if is_source {
            sources.push(token);
        }
    }
    sources
}

/// `a, b and 3 more` — a readable, bounded list of file names for a verdict.
fn name_list(names: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut list = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        list.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    list
}

/// Every command line in a compile database, tokenised. Reuses
/// [`entry_tokens`], so `command` and `arguments` entries behave the same here
/// as they do for the single-TU path.
fn compile_db_commands(json: &str) -> Vec<Vec<String>> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries.iter().filter_map(entry_tokens).collect()
}

/// Every command line `make -n` printed, tokenised.
///
/// A recipe line can chain several commands, so split on the shell separators
/// first — otherwise `cd build && g++ …` is filed as an invocation of `cd` and
/// the compile line is never audited at all.
fn make_dry_run_commands(stdout: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("make:")
            || line.starts_with("make[")
        {
            continue;
        }
        for part in line
            .split("&&")
            .flat_map(|piece| piece.split(';'))
            .flat_map(|piece| piece.split('|'))
        {
            let tokens = shell_split(part);
            if !tokens.is_empty() {
                commands.push(tokens);
            }
        }
    }
    commands
}

/// Did make decide there was nothing to rebuild? If so the binary in the tree
/// predates this call and carries none of our flags — which would otherwise
/// produce a perfectly clean, perfectly meaningless verdict.
fn make_nothing_to_do(stdout: &str) -> bool {
    stdout.contains("Nothing to be done for") || stdout.contains("is up to date")
}

/// Every object file and archive a `make -n` run says the build will LINK
/// (or archive), resolved against the directory its command runs in — the
/// `make[N]: Entering directory` lines of a recursive make and a recipe's own
/// `cd dir && …`.
fn linked_inputs(dry_run: &str, root: &Path) -> Vec<PathBuf> {
    const LINKED_EXTS: &[&str] = &["o", "a", "so", "lo"];
    let mut dirs = vec![root.to_path_buf()];
    let mut inputs: Vec<PathBuf> = Vec::new();
    for line in dry_run.lines() {
        let line = line.trim();
        if let Some(dir) = make_directory_change(line, "Entering directory ") {
            dirs.push(PathBuf::from(dir));
            continue;
        }
        if make_directory_change(line, "Leaving directory ").is_some() {
            if dirs.len() > 1 {
                dirs.pop();
            }
            continue;
        }
        let mut cwd = dirs.last().cloned().unwrap_or_else(|| root.to_path_buf());
        for part in line.split("&&").flat_map(|piece| piece.split(';')) {
            let tokens = shell_split(part);
            let Some(first) = tokens.first() else {
                continue;
            };
            if first == "cd" {
                if let Some(dir) = tokens.get(1) {
                    cwd = resolve_path(&cwd, dir);
                }
                continue;
            }
            let operands: Vec<&String> = if compiler_tokens(&tokens).is_some() {
                // A link line's inputs; `-o` names its output.
                let mut after_output_flag = false;
                tokens[1..]
                    .iter()
                    .filter(|token| {
                        let skip = std::mem::take(&mut after_output_flag);
                        after_output_flag = token.as_str() == "-o";
                        !skip && !token.starts_with('-')
                    })
                    .collect()
            } else if basename_no_exe(first) == "ar" || basename_no_exe(first).ends_with("-ar") {
                // `ar rcs libx.a a.o b.o`: the members, not the archive.
                tokens.iter().skip(3).collect()
            } else {
                continue;
            };
            for operand in operands {
                let is_linked = Path::new(operand)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| LINKED_EXTS.contains(&ext));
                let path = resolve_path(&cwd, operand);
                if is_linked && !inputs.contains(&path) {
                    inputs.push(path);
                }
            }
        }
    }
    inputs
}

/// `make[1]: Entering directory '/p/sub'` → `/p/sub` (either quote style).
fn make_directory_change<'a>(line: &'a str, verb: &str) -> Option<&'a str> {
    if !line.starts_with("make") {
        return None;
    }
    let (_, rest) = line.split_once(verb)?;
    Some(rest.trim_matches(|c| matches!(c, '\'' | '`' | '"')))
}

/// The `files` that exist but were last written before `started`: this build
/// did not produce them. Shown relative to `root` where possible.
fn stale_files(files: &[PathBuf], started: SystemTime, root: &Path) -> Vec<String> {
    files
        .iter()
        .filter(|file| {
            std::fs::metadata(file)
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified < started)
        })
        .map(|file| {
            file.strip_prefix(root)
                .unwrap_or(file)
                .display()
                .to_string()
        })
        .collect()
}

/// Project mode's integrity guard: a verdict computed over a build whose
/// instrumentation could not be trusted must never read as a clean bill of
/// health. Applied on top of the ordinary [`verdict_line`] / [`tsan_verdict`]
/// wording rather than replacing it, so the underlying verdict is still there.
///
/// A PARTIALLY instrumented build (`partial`) is not condemned: what
/// it did instrument was really checked, so a finding is real — but the
/// absence of one says nothing about the parts it did not, and the verdict
/// says so instead of reading "clean".
fn qualify_verdict(verdict: &str, weakened: &[String], partial: &[String]) -> String {
    if !weakened.is_empty() {
        let reasons: Vec<&str> = weakened.iter().chain(partial).map(String::as_str).collect();
        return format!(
            "NOT A CLEAN RESULT — this build's instrumentation could not be trusted: {}. Treat \
             the absence of findings below as UNKNOWN, not clean: sanitize the file under \
             suspicion with cpp_sanitize's single-translation-unit form, which writes the compile \
             line itself and cannot be weakened.\nunqualified verdict: {verdict}",
            reasons.join("; ")
        );
    }
    if !partial.is_empty() {
        return format!(
            "PARTIALLY CHECKED — {}. A bug in that code would not be reported, so this is NOT a \
             clean result for it; findings below are real. Sanitize that code with cpp_sanitize's \
             single-translation-unit form, which instruments every file it compiles.\nverdict \
             for the instrumented code: {verdict}",
            partial.join("; ")
        );
    }
    verdict.to_string()
}

/// The ThreadSanitizer verdict wording, shared by the single-TU and project paths so the
/// "no race in THIS run" phrasing can only ever have one definition.
fn tsan_verdict(stderr: &str, exit_label: &str, truncated: bool) -> String {
    if truncated {
        // The capture cap can discard the report itself, so "no race" here
        // would be a false clean — the one way the verdict's guarantee can break.
        return format!(
            "OUTPUT TRUNCATED at the capture cap (exit {exit_label}) — a ThreadSanitizer \
             report may have been discarded with it. This is NOT a clean result; re-run with \
             less stdout/stderr noise, then re-read the verdict."
        );
    }
    match classify_tsan(stderr) {
        TsanOutcome::Fatal(msg) => format!(
            "ThreadSanitizer FAILED TO RUN (exit {exit_label}): {msg}. The program was \
             never checked — this is neither a race nor a clean result."
        ),
        TsanOutcome::Report(kind) => {
            format!("ThreadSanitizer: {kind} detected (exit {exit_label}). See the report below.")
        }
        TsanOutcome::Silent => format!(
            "ThreadSanitizer: no race in THIS run (exit {exit_label}). A data race is \
             timing-dependent — a clean run is not proof of absence; re-run, add load, or \
             raise iteration counts."
        ),
    }
}

/// The exit label the run paths report: a numeric code, or "signal" when the
/// process died on one (which is how an ASan abort usually ends).
fn exit_label_of(code: Option<i32>) -> String {
    code.map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

/// A build-target name is handed straight to cmake/make as its own argv word,
/// so it must not be able to read as an option, and must not walk out of the
/// project.
fn is_safe_target_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '/'))
}

/// Could a file with this name be the program under test? The executable bit
/// does most of the work on unix; this rejects what is left — libraries, object
/// files, generated scripts, and the build system's own inputs.
fn is_plausible_executable_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || NON_PROGRAM_NAMES.contains(&name) {
        return false;
    }
    // libfoo.so.1 / libfoo.dylib.3 — a versioned library, whose last extension
    // is a number and therefore passes the extension test below.
    if name.contains(".so.") || name.contains(".dylib.") {
        return false;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => !NON_PROGRAM_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => true,
    }
}

#[cfg(unix)]
fn has_exec_bit(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_exec_bit(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Collect the plausible programs under `dir`, deterministically ordered.
///
/// `newer_than` is the Makefile guard: an in-tree build reuses whatever is
/// already there, so a file older than the build we just ran cannot be its
/// output.
fn collect_executables(
    dir: &Path,
    depth: usize,
    newer_than: Option<SystemTime>,
    out: &mut Vec<PathBuf>,
) {
    if depth == 0 || out.len() >= EXEC_CANDIDATE_CAP {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if out.len() >= EXEC_CANDIDATE_CAP {
            return;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if meta.is_dir() {
            if !EXEC_SKIP_DIRS.contains(&name.as_str()) {
                collect_executables(&path, depth - 1, newer_than, out);
            }
            continue;
        }
        if !meta.is_file() || !is_plausible_executable_name(&name) || !has_exec_bit(&meta) {
            continue;
        }
        if let Some(since) = newer_than {
            match meta.modified() {
                Ok(modified) if modified >= since => {}
                _ => continue,
            }
        }
        out.push(path);
    }
}

/// Find the program the project build produced, or say why we could not.
fn locate_executable(
    plan: &ProjectPlan,
    newer_than: Option<SystemTime>,
) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    collect_executables(
        &plan.build_dir,
        EXEC_SEARCH_DEPTH,
        newer_than,
        &mut candidates,
    );

    if let Some(target) = &plan.target {
        // `target: "bin/app"` matches the path's trailing components; compared
        // with the file NAME only, a program listed as `bin/app` would be
        // "not found".
        let with_exe = format!("{target}.exe");
        let wanted = candidates
            .iter()
            .find(|path| path.ends_with(target.as_str()) || path.ends_with(with_exe.as_str()));
        return match wanted {
            Some(path) => Ok(path.clone()),
            None => Err(format!(
                "the build reported success but no fresh executable named {target:?} was found \
                 under {}. Nothing was run, so this is neither clean nor a finding.{}",
                plan.build_dir.display(),
                list_candidates(&candidates)
            )),
        };
    }

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(format!(
            "the build reported success but produced no executable I can identify under {}. \
             Nothing was run, so this is neither clean nor a finding. Name the binary with \
             `target`, or use cpp_sanitize's single-translation-unit form.",
            plan.build_dir.display()
        )),
        _ => Err(format!(
            "the build produced {} executables and no `target` was given, so there is nothing to \
             run unambiguously. Re-send with the one you mean in `target`.{}",
            candidates.len(),
            list_candidates(&candidates)
        )),
    }
}

fn list_candidates(candidates: &[PathBuf]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let names: Vec<String> = candidates
        .iter()
        .take(EXEC_LIST_CAP)
        .map(|path| path.display().to_string())
        .collect();
    let mut out = format!("\nexecutables found: {}", names.join(", "));
    if candidates.len() > EXEC_LIST_CAP {
        out.push_str(&format!(" (+{} more)", candidates.len() - EXEC_LIST_CAP));
    }
    out
}

fn project_timeout_message(origin: &str, step: &str, secs: u64) -> String {
    // The timeout rule, applied to the project build: say which step, and say that
    // nothing ran — a slow build otherwise reads as a crash.
    format!(
        "{origin}\n{step} TIMED OUT after {secs}s (this is the project build, not a crash and not \
         a sanitizer finding). Nothing was run. Retry with a larger timeout_sec, or narrow the \
         build with `target`."
    )
}

/// A project build that failed: parsed diagnostics AND the build's own output,
/// the same shape [`compile_failure`] returns for a single-TU build.
fn project_build_failure(
    origin: &str,
    step: &str,
    stdout: &str,
    stderr: &str,
    kind: ProjectKind,
) -> String {
    // cmake and make forward the compiler's diagnostics to whichever stream the
    // generator chose, so scan both rather than guessing.
    let combined = format!("{stdout}\n{stderr}");
    let mut findings: Vec<Finding> = combined.lines().filter_map(parse_gcc_line).collect();
    dedupe_findings(&mut findings);

    let mut out = String::new();
    out.push_str(origin);
    out.push_str(&format!(
        "\n{step} FAILED — nothing was run, so this is neither clean nor a finding.\n"
    ));
    if kind == ProjectKind::Make {
        out.push_str(
            "note: our flags were passed as `make CFLAGS=… CXXFLAGS=… LDFLAGS=…`, after the \
             makefile's own values where `make -p` could read them. A value set per target or \
             under another name is still replaced, so if the errors below are missing headers, \
             an unknown standard or undefined references, that can be the cause — use \
             cpp_sanitize's single-translation-unit form instead.\n",
        );
    }
    out.push_str(&render_findings(&findings, FINDINGS_CAP));
    let trimmed = combined.trim();
    if !trimmed.is_empty() {
        out.push_str("\nbuild output:\n");
        out.push_str(&truncate(trimmed, TRACE_CAP));
    }
    out
}

fn in_tree_refusal(makefile: &Path) -> String {
    format!(
        "project mode found {} but no CMakeLists.txt, and make has no out-of-tree build: it \
         would write object files and binaries into your working tree, next to your sources. It \
         also cannot be guaranteed instrumented — a makefile that spells out its own compile \
         line, or uses `override CXXFLAGS`, silently discards the sanitizer flags. Re-send with \
         \"allow_in_tree_build\": true to accept both, or use cpp_sanitize's \
         single-translation-unit form (`source` plus `extra_sources`), which writes the compile \
         line itself and cannot be weakened.",
        makefile.display()
    )
}

fn no_build_system_message(root: &Path) -> String {
    format!(
        "project mode needs a build system and found none: no CMakeLists.txt and no Makefile in \
         {}. Use the single-translation-unit form instead — pass `source`, list every other .cpp \
         it links against in `extra_sources`, and add `include_dirs` / `defines` if the project \
         needs them. (If the project builds from a subdirectory, run cpp_sanitize from there.)",
        root.display()
    )
}

/// ASan + UBSan runtime options.
///
/// Whether leaks can be detected at all on this platform.
///
/// LeakSanitizer is not supported on Darwin (and is entirely absent on arm64
/// macOS), so [`sanitizer_env`] cannot ask for `detect_leaks=1` there. Both the
/// runtime option and the verdict that describes the run read THIS function, so
/// the instrumentation and the sentence reporting it cannot drift apart — which
/// is the whole failure mode: a run that silently stopped checking for leaks
/// while still calling itself clean.
fn leak_detection_available() -> bool {
    !cfg!(target_os = "macos")
}

/// Appended to an otherwise-clean ASan verdict on a platform with no
/// LeakSanitizer. Without it `cpp_sanitize` reports "clean" for a run that never
/// looked for a leak — the same false-clean class as a truncated capture and
/// a ThreadSanitizer that failed to run. The remedy has to be a platform the user can actually reach, not
/// "install LSan", because there is nothing to install.
const LEAKS_UNCHECKED_NOTE: &str = " — but LEAKS WERE NOT CHECKED: LeakSanitizer does not exist \
     on macOS, so this run covered memory errors and undefined behaviour only and a leak would \
     NOT have been reported. To check for leaks, run the same cpp_sanitize call on Linux (a VM, \
     container or CI box), where detect_leaks is enabled automatically.";

/// `detect_leaks` is a LeakSanitizer knob and LSan is **not** supported on
/// Darwin (unsupported on arm64 macOS entirely); passing it there produces a
/// runtime complaint instead of a leak report. Gated per platform.
/// UBSan keeps `print_stacktrace` but drops `halt_on_error`.
/// The sanitizer runtime environment for a checker, by name. Shared with the
/// fix verifier so it and cpp_sanitize read a report under the same options.
pub(crate) fn sanitizer_runtime_env(sanitizer: &str) -> Vec<(String, String)> {
    match sanitizer {
        "tsan" => tsan_env(),
        "msan" => msan_env(),
        _ => sanitizer_env(),
    }
}

fn sanitizer_env() -> Vec<(String, String)> {
    let asan = if leak_detection_available() {
        "symbolize=1:halt_on_error=1:detect_leaks=1"
    } else {
        "symbolize=1:halt_on_error=1"
    };
    with_symbolizer(
        vec![
            ("ASAN_OPTIONS".to_string(), asan.to_string()),
            (
                "UBSAN_OPTIONS".to_string(),
                "print_stacktrace=1".to_string(),
            ),
        ],
        discovered_symbolizer(),
    )
}

/// ThreadSanitizer runtime options. `halt_on_error=0` so one race does not
/// mask the rest; symbolize when a symbolizer is present.
fn tsan_env() -> Vec<(String, String)> {
    with_symbolizer(
        vec![(
            "TSAN_OPTIONS".to_string(),
            "symbolize=1:halt_on_error=0".to_string(),
        )],
        discovered_symbolizer(),
    )
}

/// MemorySanitizer runtime options: stop at the first report (later ones are
/// usually its consequences) and symbolize.
fn msan_env() -> Vec<(String, String)> {
    with_symbolizer(
        vec![(
            "MSAN_OPTIONS".to_string(),
            "symbolize=1:halt_on_error=1".to_string(),
        )],
        discovered_symbolizer(),
    )
}

/// The compiler for an MSan build: MemorySanitizer exists only in clang, so
/// the gcc defaults become their clang counterparts, and an explicitly chosen
/// gcc is refused with the reason.
fn msan_compiler(compiler: &str, explicit: bool) -> Result<String, String> {
    match compiler {
        "clang" | "clang++" => Ok(compiler.to_string()),
        "gcc" | "g++" if explicit => Err(format!(
            "sanitizer \"msan\" needs clang: MemorySanitizer does not exist in {compiler}. Pass \
             compiler \"clang\" (C) or \"clang++\" (C++), or omit `compiler`."
        )),
        "gcc" => Ok("clang".to_string()),
        _ => Ok("clang++".to_string()),
    }
}

/// Point the sanitizer runtimes at a symbolizer they would not find on their
/// own (see [`discovered_symbolizer`]): `external_symbolizer_path` in every
/// `*_OPTIONS`, plus `ASAN_SYMBOLIZER_PATH`. Unchanged when `symbolizer` is
/// `None`, i.e. whenever `llvm-symbolizer` is already on PATH.
fn with_symbolizer(
    mut env: Vec<(String, String)>,
    symbolizer: Option<&Path>,
) -> Vec<(String, String)> {
    let Some(path) = symbolizer.map(|p| p.display().to_string()) else {
        return env;
    };
    // `:` separates sanitizer options; such a path cannot be passed.
    if path.contains(':') {
        return env;
    }
    for (_, value) in env.iter_mut() {
        value.push_str(&format!(":external_symbolizer_path={path}"));
    }
    env.push(("ASAN_SYMBOLIZER_PATH".to_string(), path));
    env
}

/// An `llvm-symbolizer` the sanitizer runtime would NOT find by itself, found
/// once per session: the runtime looks for that exact name on PATH, while
/// distributions often install only `llvm-symbolizer-N` on PATH and the
/// unversioned binary in `/usr/lib/llvm-N/bin/`. `None` when the plain name
/// is on PATH (nothing to do) or none exists.
pub(crate) fn discovered_symbolizer() -> Option<&'static Path> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            if binary_on_path("llvm-symbolizer") {
                return None;
            }
            let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default();
            find_symbolizer(&path_dirs, Path::new("/usr/lib"))
        })
        .as_deref()
}

/// The newest usable symbolizer among `llvm-symbolizer-N` on `path_dirs` and
/// `<lib_root>/llvm-N/bin/llvm-symbolizer`. The runtime of LLVM 12 and older
/// refuses a symbolizer whose file name is not exactly `llvm-symbolizer`, so a
/// versioned name is used only through its real (unversioned) target, or when
/// N >= 13.
fn find_symbolizer(path_dirs: &[PathBuf], lib_root: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    let mut consider = |version: u32, path: PathBuf| {
        if best.as_ref().is_none_or(|(v, _)| version > *v) {
            best = Some((version, path));
        }
    };
    if let Ok(entries) = std::fs::read_dir(lib_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(version) = name
                .to_str()
                .and_then(|n| n.strip_prefix("llvm-"))
                .and_then(|v| v.parse::<u32>().ok())
            else {
                continue;
            };
            let candidate = entry.path().join("bin").join("llvm-symbolizer");
            if candidate.is_file() {
                consider(version, candidate);
            }
        }
    }
    for dir in path_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(version) = name
                .to_str()
                .and_then(|n| n.strip_prefix("llvm-symbolizer-"))
                .and_then(|v| v.parse::<u32>().ok())
            else {
                continue;
            };
            let real = std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
            if real.file_name().is_some_and(|n| n == "llvm-symbolizer") {
                consider(version, real);
            } else if version >= 13 {
                consider(version, entry.path());
            }
        }
    }
    best.map(|(_, path)| path)
}

/// A one-line note when the report's frames came out as bare addresses and no
/// symbolizer is available — and nothing otherwise. GCC's runtime symbolizes
/// through libbacktrace without llvm-symbolizer, so the old unconditional
/// "llvm-symbolizer not on PATH" note was noise on every GCC build.
fn symbolizer_note(stderr: &str) -> String {
    if has_unsymbolized_frames(stderr)
        && !binary_on_path("llvm-symbolizer")
        && discovered_symbolizer().is_none()
    {
        "note: stack frames are unsymbolized (no llvm-symbolizer found); install llvm (it \
         provides llvm-symbolizer) or map the addresses with cpp_symbolize.\n"
            .to_string()
    } else {
        String::new()
    }
}

/// Frames like `#1 0x4011d6 (/tmp/app+0x11d6)`: an address, no ` in <fn>`.
fn has_unsymbolized_frames(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix('#')
            .and_then(|rest| rest.split_once(' '))
            .is_some_and(|(num, rest)| {
                num.chars().all(|c| c.is_ascii_digit())
                    && rest.starts_with("0x")
                    && !rest.contains(" in ")
                    && rest.contains("+0x")
            })
    })
}

/// The clean-run verdict, split from the platform probe so BOTH branches are
/// testable on any host. The branch that matters is the macOS one, and a Linux
/// CI would never execute it if the decision were only `cfg!`-gated — the same
/// reason `trust::decide` is separated from `Config`.
fn clean_verdict(leaks_checked: bool) -> String {
    if leaks_checked {
        return "clean: exit 0, no sanitizer errors".to_string();
    }
    format!("clean: exit 0, no sanitizer errors{LEAKS_UNCHECKED_NOTE}")
}

fn verdict_line(findings: &[Finding], exit_label: &str, truncated: bool) -> String {
    if truncated {
        return truncated_verdict(findings.len());
    }
    if findings.is_empty() && exit_label == "0" {
        // On a platform with no LeakSanitizer this run never looked for a
        // leak, so a bare "clean" overstates what was checked.
        return clean_verdict(leak_detection_available());
    }
    format!("{} issue(s) (exit {exit_label})", findings.len())
}

/// The memcheck verdict, shared by the single-TU and project paths.
///
/// "Clean" only for a run that exited 0. It used to read "clean: no
/// memcheck errors (exit signal)" after an abort, a SIGKILL and a stack
/// overflow, and "(exit 3)" for a program that failed: a run that died or
/// failed is not a clean one, whatever memcheck saw before it ended — the
/// same rule the ASan path's "0 issue(s) (exit N)" already follows.
/// `xml_complete`: valgrind finished writing its XML report. When it did not
/// (it failed to start the program, or could not write the file), an empty
/// finding list is NOT evidence of a clean run — it used to read "clean".
fn valgrind_verdict(
    findings: &[Finding],
    exit_label: &str,
    truncated: bool,
    xml_complete: bool,
) -> String {
    if truncated {
        return truncated_verdict(findings.len());
    }
    if !xml_complete && findings.is_empty() {
        return format!(
            "NO REPORT (exit {exit_label}): valgrind wrote no complete memcheck report, so \
             nothing was checked — NOT a clean result. Its own output is below."
        );
    }
    if findings.is_empty() && exit_label == "0" {
        return "clean: no memcheck errors (exit 0)".to_string();
    }
    format!("{} issue(s) (exit {exit_label})", findings.len())
}

fn xml_is_complete(xml: &str) -> bool {
    xml.contains("</valgrindoutput>")
}

// ---- The shared result beside each verdict sentence -------------------------
//
// Each function below takes exactly the inputs its sentence-writing twin
// takes, so the line a person reads and the result a gate reads can only
// differ in wording, never in meaning. Only a run that finished, was read in
// full and reported nothing is a PASS.

fn build_failed_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::BuildFailed,
        "The program did not build, so the sanitizer never ran.",
        "fix the compile error above (or add the missing extra_sources / include dirs), then call cpp_sanitize again.",
    )
}

fn build_timeout_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::TimeLimit,
        "The build was stopped by the time limit, so the sanitizer never ran.",
        "call cpp_sanitize again with a larger `timeout_sec`, fewer `extra_sources`, or a smaller reproducer.",
    )
}

fn run_timeout_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::TimeLimit,
        "The program was still running at the time limit, so the run was not checked to the end.",
        "call cpp_sanitize again with a larger `timeout_sec`, or `run_args` that make the program finish sooner.",
    )
}

fn truncated_result() -> CheckResult {
    CheckResult::not_run(
        TOOL_NAME,
        NotRunReason::OutputTruncated,
        "The program printed more than the engine holds, so a sanitizer report may have been cut off.",
        "call cpp_sanitize again with the program's own logging turned down.",
    )
}

fn first_location(findings: &[Finding]) -> String {
    findings
        .first()
        .map(|finding| format!(", the first at {}:{}", finding.file, finding.line))
        .unwrap_or_default()
}

/// Beside [`verdict_line`] / [`asan_ubsan_verdict`] (and memcheck with a
/// complete report).
fn findings_run_result(
    findings: &[Finding],
    exit_label: &str,
    truncated: bool,
    leaks_checked: bool,
) -> CheckResult {
    if !findings.is_empty() {
        let more = if truncated {
            " The output was cut, so there may be more."
        } else {
            ""
        };
        return CheckResult::fail(
            TOOL_NAME,
            &format!(
                "{} sanitizer finding(s){}.{more}",
                findings.len(),
                first_location(findings)
            ),
        );
    }
    if truncated {
        return truncated_result();
    }
    if exit_label != "0" {
        return CheckResult::fail(
            TOOL_NAME,
            &format!("The program ended with exit {exit_label}, without a sanitizer report."),
        );
    }
    if leaks_checked {
        CheckResult::pass(
            TOOL_NAME,
            "The program ran to exit 0 with no sanitizer errors.",
        )
    } else {
        CheckResult::approximate(
            TOOL_NAME,
            "No sanitizer errors, but leaks were not checked: LeakSanitizer does not exist on this platform.",
        )
    }
}

/// Beside [`valgrind_verdict`].
fn valgrind_result(
    findings: &[Finding],
    exit_label: &str,
    truncated: bool,
    xml_complete: bool,
) -> CheckResult {
    if !xml_complete && findings.is_empty() && !truncated {
        return CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::ToolError,
            "valgrind wrote no complete memcheck report, so nothing was checked.",
            "read valgrind's own output above (it often could not start the program), fix that, then call cpp_sanitize again.",
        );
    }
    findings_run_result(findings, exit_label, truncated, true)
}

/// Beside [`tsan_verdict`].
fn tsan_result(stderr: &str, exit_label: &str, truncated: bool) -> CheckResult {
    if truncated {
        return truncated_result();
    }
    match classify_tsan(stderr) {
        TsanOutcome::Fatal(_) => CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::ToolError,
            "ThreadSanitizer failed to start, so the program was never checked.",
            "fix the ThreadSanitizer error shown above (often the kernel's mmap randomisation), then call cpp_sanitize again.",
        ),
        TsanOutcome::Report(kind) => {
            CheckResult::fail(TOOL_NAME, &format!("ThreadSanitizer reported a {kind}."))
        }
        TsanOutcome::Silent if exit_label == "0" => CheckResult::pass(
            TOOL_NAME,
            "No race in this run (a race depends on timing, so one clean run is not proof of absence).",
        ),
        TsanOutcome::Silent => CheckResult::fail(
            TOOL_NAME,
            &format!("The program ended with exit {exit_label}, without a ThreadSanitizer report."),
        ),
    }
}

/// Beside [`qualify_verdict`]: a weakened build cannot support "clean", and a
/// partial one can support it only for what it instrumented. Findings stay
/// FAIL either way: what was instrumented was really checked.
fn qualify_result(result: CheckResult, weakened: &[String], partial: &[String]) -> CheckResult {
    if result.verdict != check_result::Verdict::Pass
        && result.verdict != check_result::Verdict::Approximate
    {
        return result;
    }
    if !weakened.is_empty() {
        return CheckResult::no_claim(
            TOOL_NAME,
            "The build's instrumentation could not be trusted, so no finding does not mean clean; sanitize the suspect file with the single-file form.",
        );
    }
    if !partial.is_empty() {
        return CheckResult::approximate(
            TOOL_NAME,
            "Clean for the instrumented code only; part of the program was not instrumented.",
        );
    }
    result
}

/// A truncated capture is not evidence of a clean run, and must never read
/// like one.
fn truncated_verdict(found: usize) -> String {
    format!(
        "{found} issue(s) so far — OUTPUT TRUNCATED at {CAPTURE_CAP_BYTES} bytes. The program \
         printed more than the engine will hold, so this is NOT a clean result: rerun with the \
         program's own logging turned down."
    )
}

/// `libraries`: what [`built_project_libraries`] found, for the way out.
fn compile_timeout_message(plan: &BuildPlan, secs: u64, libraries: &[String]) -> String {
    // Say BUILD explicitly. A slow template build otherwise reads as a
    // crash, and the model retries the wrong thing. A single-file build this
    // slow is compile-bound, and the retry that used to be suggested first
    // (a larger timeout) just waited longer for the same work.
    let instead = if libraries.is_empty() {
        "build the project's library once with its own build system and list the library \
         file in extra_sources in place of its sources"
            .to_string()
    } else {
        format!(
            "list the library the project already built in extra_sources in place of its \
             sources ({})",
            libraries.join(", ")
        )
    };
    format!(
        "{}\nBUILD TIMED OUT after {secs}s (this is the compile step, not a crash and not a \
         sanitizer finding). {} translation unit(s) at -std={} under ASan+UBSan. The time goes \
         into compiling, so a larger timeout_sec rarely helps: {instead}, drop extra_sources the \
         program does not need, or sanitize the project's own target with project=true.",
        plan.origin,
        plan.sources.len(),
        plan.std
    )
}

/// Library files a build of the project already produced (`lib*.a`,
/// `lib*.so[.N]`), newest first, at most [`LIBRARY_HINT_CAP`], relative to
/// `cwd`. Linking one is how a reproducer avoids recompiling the library's
/// sources (and re-instantiating its templates) under instrumentation. The
/// walk is bounded, so a huge tree costs milliseconds.
fn built_project_libraries(cwd: &Path) -> Vec<String> {
    const SKIP: &[&str] = &[".git", "CMakeFiles", "node_modules", ".cache"];
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut budget = LIBRARY_SCAN_BUDGET;
    let mut stack = vec![(cwd.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if depth < LIBRARY_SCAN_DEPTH && !SKIP.contains(&name.as_ref()) {
                    stack.push((path, depth + 1));
                }
            } else if kind.is_file() && is_library_name(&name) {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                found.push((modified, path));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .take(LIBRARY_HINT_CAP)
        .map(|(_, path)| {
            path.strip_prefix(cwd)
                .unwrap_or(&path)
                .display()
                .to_string()
        })
        .collect()
}

/// `libfoo.a`, `libfoo.so`, `libfoo.so.1.2`.
fn is_library_name(name: &str) -> bool {
    name.starts_with("lib")
        && (name.ends_with(".a") || name.ends_with(".so") || name.contains(".so."))
}

/// A prebuilt library or object listed in `extra_sources`: linked as given,
/// never compiled, and never counted when deciding C vs C++.
fn is_link_input(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("a" | "o" | "so" | "dylib")
    ) || name.contains(".so.")
}

/// The linker could not resolve symbols (GNU ld, gold, lld, ld64 wording).
fn has_unresolved_symbols(stderr: &str) -> bool {
    [
        "undefined reference",
        "Undefined symbols",
        "undefined symbol",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
}

/// On unresolved symbols, name the libraries the project already built.
fn link_hint(libraries: &[String]) -> String {
    if libraries.is_empty() {
        return String::new();
    }
    format!(
        "\nnote: the project has already-built libraries ({}); list the one that defines the \
         missing symbols in extra_sources to link it instead of adding its sources.",
        libraries.join(", ")
    )
}

/// Functions the compiler reports as called without a declaration: gcc's
/// "implicit declaration of function ‘f’" and clang's "implicit declaration of
/// function 'f'" / "call to undeclared function 'f'".
fn implicitly_declared(stderr: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let rest = [
            "implicit declaration of function",
            "call to undeclared function",
        ]
        .iter()
        .find_map(|marker| line.find(marker).map(|at| &line[at + marker.len()..]));
        let Some(rest) = rest else {
            continue;
        };
        let name: String = rest
            .trim_start()
            .trim_start_matches(['\'', '\u{2018}', '`', '"'])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// C assumes an undeclared function returns `int`, so a pointer result is cut
/// to 32 bits and the program crashes or misbehaves at that call. That is a bug
/// in the program under test here (usually the agent's own reproducer), and it
/// must not be read as a finding in the project's code.
fn implicit_declaration_note(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!(
        "\nnote: the compiler warned that {} {} called without a declaration (a missing \
         #include or feature-test macro). C then assumes an int result, so a crash or wrong value \
         at such a call is a bug in this test program, not a finding in the code under test: add \
         the declaration and rerun.",
        names.join(", "),
        if names.len() == 1 { "was" } else { "were" }
    )
}

/// Default for the single-file build step; see [`SINGLE_TU_COMPILE_TIMEOUT_SECS`].
fn single_tu_compile_timeout(timeout_sec: Option<u64>) -> Duration {
    Duration::from_secs(timeout_sec.unwrap_or(SINGLE_TU_COMPILE_TIMEOUT_SECS).max(1))
}

fn run_timeout_message(preamble: &str, secs: u64) -> String {
    // The kill reaches the program's process group. A child that detached
    // itself with setsid() has left that group, so "everything it spawned"
    // would be a claim the kill cannot back.
    // The build is -O0 on purpose, which makes CPU-heavy code much slower
    // than the user's own builds; a slow program must not read like a hang.
    format!(
        "{preamble}RUN TIMED OUT after {secs}s (the build succeeded; the instrumented program was \
         killed along with every process still in its process group — a child that detached \
         itself with setsid() is outside that group and may still be running). It may be \
         hanging, looping, or waiting on stdin — stdin is /dev/null here. Or it may just be slow: \
         it is built unoptimized (-O0, so the optimizer cannot delete a bug before the checker \
         sees it), which runs CPU-heavy code — STL-heavy C++ especially — several times slower \
         than a normal build; if so, retry with a larger timeout_sec. No verdict: this is \
         neither clean nor a finding."
    )
}

/// Compile failure: parsed diagnostics AND the compiler's own stderr. The
/// parsed line alone is not enough — a missing header is reported as
/// `fatal error:` and a template error spans many lines.
fn compile_failure(plan: &BuildPlan, stderr: &str) -> String {
    let findings: Vec<Finding> = stderr.lines().filter_map(parse_gcc_line).collect();
    let mut out = String::new();
    out.push_str(&plan.origin);
    out.push_str("\ncompile failed\n");
    out.push_str(&render_findings(&findings, FINDINGS_CAP));
    let trimmed = stderr.trim();
    if !trimmed.is_empty() {
        out.push_str("\ncompiler stderr:\n");
        out.push_str(&truncate(trimmed, TRACE_CAP));
    }
    out
}

/// Assemble the run output: provenance/preamble, verdict, the program's own
/// (truncated) stdout, the rendered findings, then the raw sanitizer report.
///
/// The raw report is the point. Before this, `format_run_output` returned
/// preamble + verdict + stdout + one-line findings and threw the sanitizer's
/// stderr away, so the complete tool output the model saw for a real
/// use-after-free was three lines — and it went shell-compiling to find the
/// stack trace itself.
fn format_run_output(
    preamble: &str,
    verdict: &str,
    stdout: &str,
    findings: &[Finding],
    trace: &str,
) -> String {
    format_run_output_with_evidence(preamble, verdict, stdout, findings, "", trace)
}

/// [`format_run_output`] plus the root-cause evidence block
/// ([`crate::memsafety::evidence`]), placed after the findings and BEFORE the
/// raw report: the allocation/free/origin stacks survive the raw report's
/// byte cap, and they are read before the crash frame invites a guard there.
fn format_run_output_with_evidence(
    preamble: &str,
    verdict: &str,
    stdout: &str,
    findings: &[Finding],
    evidence: &str,
    trace: &str,
) -> String {
    let mut out = String::new();
    out.push_str(preamble);
    out.push_str(verdict);
    out.push('\n');

    let stdout = stdout.trim_end();
    if !stdout.is_empty() {
        out.push_str("program stdout:\n");
        out.push_str(&truncate(stdout, STDOUT_CAP));
        out.push('\n');
    }
    // ASan and LSan end the program with _exit, which does not flush
    // stdio, and a captured stdout is block-buffered — so what the program
    // printed just before the bug is missing; say so.
    if trace.contains("ERROR: AddressSanitizer:") || trace.contains("ERROR: LeakSanitizer:") {
        out.push_str(
            "note: the sanitizer stopped the program without flushing its stdout (block-buffered \
             when captured), so program output printed just before the report may be missing \
             above. The report itself is complete. To see all of it, print to stderr or call \
             setvbuf(stdout, NULL, _IONBF, 0) at the top of main.\n",
        );
    }

    out.push_str(&render_findings(findings, FINDINGS_CAP));

    if !evidence.is_empty() {
        out.push('\n');
        out.push_str(evidence.trim_end());
    }

    if !trace.is_empty() {
        out.push_str("\n--- sanitizer report (raw stderr) ---\n");
        out.push_str(trace);
    }
    out
}

/// Trim by meaning, then by length.
///
/// The length cap keeps the head, so a program that writes its own logging
/// to stderr before the bug would push the sanitizer report out of the trace
/// entirely. When the text is over the cap, it starts at the first line the
/// sanitizer runtime wrote, and says how much program output went before it.
fn prepare_trace(stderr: &str) -> String {
    let trimmed = strip_shadow_block(stderr);
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() > TRACE_CAP
        && let Some(start) = report_start(trimmed)
        && start > 0
    {
        return format!(
            "({start} bytes of the program's own stderr before the report omitted)\n{}",
            truncate(&trimmed[start..], TRACE_CAP)
        );
    }
    truncate(trimmed, TRACE_CAP)
}

/// Byte offset of the first line a sanitizer runtime wrote: the `=====…`
/// separator it prints before a report, an `==PID==` line (ASan / LSan /
/// TSan), or a UBSan `runtime error:` line.
fn report_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end();
        let separator = bare.len() >= 20 && bare.bytes().all(|b| b == b'=');
        if separator || strip_pid_prefix(line).is_some() || line.contains(RUNTIME_ERROR) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Drop AddressSanitizer's shadow-byte hex map and its legend.
///
/// Measured on a real heap-use-after-free report: 2,676 bytes total, of which
/// **1,409 (53%) is the shadow map** — indispensable to a human in a debugger,
/// pure noise to a model, and the single largest thing standing between the
/// stack traces and a byte cap. The block is delimited by a fixed marker line
/// and runs until the next report boundary (`==<pid>==ABORTING`, or the `====`
/// separator that opens a following report), so this is a substring cut rather
/// than parsing.
fn strip_shadow_block(stderr: &str) -> String {
    if !stderr.contains(SHADOW_MARKER) {
        return stderr.to_string();
    }
    let mut out = String::with_capacity(stderr.len());
    let mut skipping = false;
    for line in stderr.lines() {
        if !skipping && line.trim_start().starts_with(SHADOW_MARKER) {
            skipping = true;
            out.push_str("  [shadow byte map elided — see the stacks above]\n");
            continue;
        }
        if skipping {
            if line.starts_with("==") {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Collapse identical findings, preserving first-seen order.
fn dedupe_findings(findings: &mut Vec<Finding>) {
    let mut seen = HashSet::new();
    findings.retain(|f| {
        seen.insert((
            f.source.clone(),
            f.file.clone(),
            f.line,
            f.column,
            f.message.clone(),
        ))
    });
}

fn describe_origin(
    db_path: Option<&Path>,
    matched: bool,
    source_label: &str,
    std: &str,
    includes: usize,
    defines: usize,
    tus: usize,
) -> String {
    match (db_path, matched) {
        (Some(path), true) => format!(
            "build: compile_commands.json ({}) entry for {source_label} — std={std}, \
             {includes} include dir(s), {defines} define(s), {tus} TU(s)",
            path.display()
        ),
        (Some(path), false) if !db_is_readable(path) => format!(
            "build: compile_commands.json ({}) could not be read as a JSON array, so it was \
             ignored; using the arguments given — std={std}, {includes} include dir(s), \
             {defines} define(s), {tus} TU(s)",
            path.display()
        ),
        (Some(path), false) => format!(
            "build: compile_commands.json ({}) has no entry for {source_label}; using the \
             arguments given — std={std}, {includes} include dir(s), {defines} define(s), \
             {tus} TU(s)",
            path.display()
        ),
        (None, _) => format!(
            "build: no compile_commands.json found; using the arguments given — std={std}, \
             {includes} include dir(s), {defines} define(s), {tus} TU(s). Run cpp_compile_db if \
             this project has a build system.",
        ),
    }
}

/// Find the compile-database entry for `source` and extract the allowlisted
/// flags from it. `None` when the DB is unreadable, is not an array, or has no
/// entry for this exact file (header-only TUs, files outside the build).
fn db_flags_for(db_path: &Path, source: &Path) -> Option<DbFlags> {
    let contents = std::fs::read_to_string(db_path).ok()?;
    let entries: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let entries = entries.as_array()?;
    let want = canonical(source);

    // A relative `directory` is relative to the database's own folder,
    // and one malformed entry (no `file`) must not end the search — it used to
    // (`?`), so a DB whose first entry lacked `file` "had no entry" for every
    // source.
    let db_dir = db_path.parent().unwrap_or(Path::new("."));
    for entry in entries {
        let directory = entry
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let directory = resolve_path(db_dir, directory);
        let Some(file) = entry.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let entry_path = resolve_path(&directory, file);
        if canonical(&entry_path) != want {
            continue;
        }
        let Some(tokens) = entry_tokens(entry) else {
            continue;
        };
        return Some(extract_db_flags(&tokens, &directory));
    }
    None
}

/// The `-I` directories and `-D` defines of `source`'s compile-DB entry, for
/// tools that must pass them by hand (cppcheck < 2.0 has no `--file-filter`).
pub(crate) fn db_includes_and_defines(
    db_path: &Path,
    source: &Path,
) -> Option<(Vec<String>, Vec<String>)> {
    db_flags_for(db_path, source).map(|flags| (flags.include_dirs, flags.defines))
}

/// Does `db_path` parse as a compile database at all? Distinguishes "the
/// database has no entry for this file" from "the database is unreadable".
fn db_is_readable(db_path: &Path) -> bool {
    std::fs::read_to_string(db_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|value| value.is_array())
}

/// One `include_dirs`/`defines` entry, without a flag spelling the model
/// may have copied from a command line. `["-DFOO"]` used to become `-D-DFOO`
/// (a compile error naming no define) and `["-Iinc"]` the path `<cwd>/-Iinc`;
/// an empty entry failed without saying which.
fn arg_value<'a>(
    field: &str,
    index: usize,
    value: &'a str,
    flag: &str,
) -> Result<&'a str, FunctionCallError> {
    let value = value.trim();
    let value = value
        .strip_prefix(flag)
        .map(str::trim_start)
        .unwrap_or(value);
    if value.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{field}[{index}] is empty; pass the bare value ({})",
            if flag == "-D" {
                "e.g. \"FOO\" or \"FOO=1\""
            } else {
                "a directory, e.g. \"include\""
            }
        )));
    }
    Ok(value)
}

/// Older names a compiler may know our default standard by.
fn std_fallbacks(std: &str) -> &'static [&'static str] {
    match std {
        DEFAULT_STD => &["c++2b", "c++20"],
        DEFAULT_STD_C => &["gnu11"],
        _ => &[],
    }
}

/// Did the compiler refuse `-std=<std>` itself (rather than the code)?
/// clang: "invalid value 'c++23' in '-std=c++23'"; gcc: "unrecognized
/// command-line option '-std=c++23'".
fn std_rejected(stderr: &str, std: &str) -> bool {
    stderr.contains(&format!("-std={std}'"))
        && (stderr.contains("invalid value") || stderr.contains("unrecognized command"))
}

/// The C driver of the same family as `compiler`.
fn c_compiler_for(compiler: &str) -> &'static str {
    if compiler.starts_with("clang") {
        "clang"
    } else {
        "gcc"
    }
}

/// A C (not C++) translation unit, by extension.
fn is_c_source(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("c")
}

/// A compile-DB entry carries either `arguments` (already split) or `command`
/// (one shell-quoted string).
fn entry_tokens(entry: &serde_json::Value) -> Option<Vec<String>> {
    if let Some(args) = entry.get("arguments").and_then(|v| v.as_array()) {
        return Some(
            args.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        );
    }
    let command = entry.get("command").and_then(|v| v.as_str())?;
    Some(shell_split(command))
}

/// Pull ONLY `-I` / `-isystem` / `-iquote` / `-D` / `-std=` out of a compile
/// line, resolving relative paths against the entry's `directory`.
///
/// This is the load-bearing half of the structured-arguments rule. A real compile line also carries
/// `-O3`, `-march=native`, `-f…`, `-W…`, `-c`, `-o` and the source list; some
/// projects carry `-fsanitize=thread` (which conflicts with ASan) or an
/// explicit `-fno-sanitize=…`. Splatting the DB's command line would hand the
/// project the power to make `"clean: exit 0, no sanitizer errors"` mean
/// nothing — the same defect a free-form `flags[]` argument would introduce,
/// arriving through a different door. So the database fills in exactly the
/// categories the structured arguments expose, and nothing else.
/// The sanitizers `cpp_sanitize` will actually run. Extracted from the guard in
/// `handle_call` so a test exercises the REAL accepted set instead of restating
/// it (a test that re-declares the set passes on any code).
fn is_accepted_sanitizer(name: &str) -> bool {
    matches!(name, "asan_ubsan" | "valgrind" | "tsan" | "msan")
}

/// May this compile-database token be copied onto the sanitizer build line
/// as a target-ISA flag?
///
/// DELIBERATELY an allowlist, not a `-m*` prefix test. Two things go wrong with
/// a blanket prefix:
///   * `-mllvm` forwards an arbitrary option to the LLVM backend — e.g.
///     `-mllvm -tsan-instrument-memory-accesses=0` switches race instrumentation
///     OFF. Copying it would break this tool's core guarantee that "a clean
///     sanitizer verdict cannot be faked".
///   * `-mllvm` (and a few Darwin options) take their value as a SEPARATE argv
///     token. We copy tokens individually, so the value is dropped and the flag
///     then swallows whatever we append next — our own `-fsanitize=...`.
///
/// Everything accepted here is self-contained and affects only instruction
/// selection, never instrumentation.
pub(crate) fn is_safe_target_flag(token: &str) -> bool {
    // Known backend passthroughs / separate-operand options: never copy.
    const DENY: [&str; 3] = ["-mthread-model", "-multiply_defined", "-multi_module"];
    if token.starts_with("-mllvm") || DENY.contains(&token) {
        return false;
    }
    // `=`-joined target selectors, with a non-empty value.
    for prefix in ["-march=", "-mtune=", "-mcpu=", "-mabi=", "-mfpmath="] {
        if let Some(value) = token.strip_prefix(prefix) {
            return !value.is_empty();
        }
    }
    // No other `=` form is on the allowlist.
    if token.contains('=') {
        return false;
    }
    // ABI width.
    if matches!(token, "-m32" | "-m64" | "-mx32") {
        return true;
    }
    // Bare CPU-feature toggle: -mavx2, -mfma, -msse4.2, -mpopcnt, -mno-avx512f …
    let Some(feature) = token.strip_prefix("-m") else {
        return false;
    };
    let feature = feature.strip_prefix("no-").unwrap_or(feature);
    !feature.is_empty()
        && feature
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

fn extract_db_flags(tokens: &[String], directory: &Path) -> DbFlags {
    let mut flags = DbFlags::default();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let take_path = |value: &str, out: &mut Vec<String>| {
            let resolved = resolve_path(directory, value).display().to_string();
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        };
        match token {
            "-I" | "-isystem" | "-iquote" => {
                if let Some(value) = tokens.get(i + 1) {
                    take_path(value, &mut flags.include_dirs);
                    i += 1;
                }
            }
            "-D" => {
                if let Some(value) = tokens.get(i + 1) {
                    if !flags.defines.contains(value) {
                        flags.defines.push(value.clone());
                    }
                    i += 1;
                }
            }
            "-include" | "-imacros" => {
                if let Some(value) = tokens.get(i + 1) {
                    let resolved = resolve_path(directory, value).display().to_string();
                    flags.semantics.push(token.to_string());
                    flags.semantics.push(resolved);
                    i += 1;
                }
            }
            "-U" => {
                if let Some(value) = tokens.get(i + 1) {
                    flags.semantics.push(format!("-U{value}"));
                    i += 1;
                }
            }
            "-fwrapv" | "-fno-wrapv" | "-fsigned-char" | "-funsigned-char" | "-fno-signed-char"
            | "-fno-unsigned-char"
                if !flags.semantics.iter().any(|f| f == token) =>
            {
                flags.semantics.push(token.to_string());
            }
            "-fwrapv" | "-fno-wrapv" | "-fsigned-char" | "-funsigned-char" | "-fno-signed-char"
            | "-fno-unsigned-char" => {}
            _ if token.starts_with("-include") && token.len() > "-include".len() => {
                let resolved = resolve_path(directory, &token["-include".len()..]);
                flags.semantics.push("-include".to_string());
                flags.semantics.push(resolved.display().to_string());
            }
            _ if token.starts_with("-U") && token.len() > 2 => {
                flags.semantics.push(token.to_string());
            }
            _ if token.starts_with("-I") => take_path(&token[2..], &mut flags.include_dirs),
            _ if token.starts_with("-isystem") => {
                take_path(&token["-isystem".len()..], &mut flags.include_dirs)
            }
            _ if token.starts_with("-iquote") => {
                take_path(&token["-iquote".len()..], &mut flags.include_dirs)
            }
            _ if token.starts_with("-D") => {
                let value = token[2..].to_string();
                if !value.is_empty() && !flags.defines.contains(&value) {
                    flags.defines.push(value);
                }
            }
            _ if token.starts_with("-std=") => {
                flags.std = Some(token["-std=".len()..].to_string());
            }
            // Target-ISA / codegen flags (-march=, -mtune=, -mavx2, -mfma,
            // -m64, ...). Without these a per-file `-mavx2` never reaches the
            // sanitizer build and a SIMD source fails to compile with
            // "always_inline ... requires target feature 'avx'".
            _ if is_safe_target_flag(token) => {
                if !flags.march.contains(&token.to_string()) {
                    flags.march.push(token.to_string());
                }
            }
            // Everything else is deliberately dropped. See the doc comment.
            _ => {}
        }
        i += 1;
    }
    flags
}

/// Minimal POSIX-ish splitter for a compile-DB `command` string: honours single
/// quotes, double quotes and backslash escapes, which is what the generators
/// (CMake, Bear) emit.
pub(crate) fn shell_split(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                started = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                current.push(next);
                            }
                        }
                        other => current.push(other),
                    }
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

/// Canonicalize for comparison, falling back to the literal path when the file
/// does not exist (so a stale DB entry simply fails to match).
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        text.to_string()
    } else {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... (truncated)", &text[..end])
    }
}

pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Why `path` cannot be the `source` of a program build, or `None` when it can.
///
/// The header extensions are GCC's own list of "C/C++ header file to be turned
/// into a precompiled header", matched case-sensitively: `.H` is a header while
/// `.C` is a C++ source.
fn not_a_translation_unit(path: &Path) -> Option<&'static str> {
    const HEADER_EXTS: &[&str] = &["h", "hh", "H", "hp", "hxx", "hpp", "HPP", "h++", "tcc"];
    if path.is_dir() {
        return Some("is a directory, not a C/C++ file");
    }
    let ext = path.extension().and_then(|ext| ext.to_str())?;
    HEADER_EXTS.contains(&ext).then_some(
        "is a header, and compiling a header produces a precompiled header, not a program",
    )
}

/// Best-effort PATH lookup for a binary; avoids extra deps.
pub(crate) fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).exists())
}

/// Build a unique path inside the system temp dir.
fn unique_tmp_path(prefix: &str, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}{suffix}"))
}

/// Parse a g++ diagnostic line: `file:line:col: error|warning: message`.
///
/// `fatal error` is included deliberately: a missing project header is reported
/// as `include/pool.hpp:3:10: fatal error: orderbook.hpp: No such file or
/// directory`; with that severity unmatched, the one failure a
/// real project hits first would render as `compile failed / No findings.`
fn parse_gcc_line(line: &str) -> Option<Finding> {
    let line = line.trim();
    // file:line:col: severity: message
    let (location, rest) = line.split_once(": ")?;
    let (severity_str, message) = rest.split_once(": ")?;
    let severity = match severity_str {
        "error" | "fatal error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };

    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.parse::<u32>().ok()?;
    let line_no = parts.next()?.parse::<u32>().ok()?;
    let file = parts.next()?.to_string();

    Some(Finding {
        source: "gcc".to_string(),
        severity,
        file,
        line: line_no,
        column,
        message: message.trim().to_string(),
        rule: None,
    })
}

/// What an ASan + UBSan run's stderr reports, shared by the single-TU and
/// project paths.
#[derive(Debug, Default)]
struct Reports {
    findings: Vec<Finding>,
    /// Lines shaped like a sanitizer report that are not one: quoted
    /// inside other output, or a `runtime error:` line with no stack trace
    /// after it. Never findings — but reported, so nothing is silently dropped.
    lookalikes: usize,
}

/// A UBSan diagnostic, wherever it starts on its line.
const RUNTIME_ERROR: &str = ": runtime error: ";

/// Parse every AddressSanitizer, LeakSanitizer and UBSan report in `stderr`.
///
/// Only the shapes the runtimes really print count, because a program's
/// OWN output can quote them (measured: a program logging
/// `expected message was '==1==ERROR: AddressSanitizer: heap-use-after-free…'`
/// and a `parser.c:10:5: runtime error: …` line got "2 issue(s) (exit 0)").
/// An ASan/LSan header starts its line with `==PID==` — the runtime prints a
/// separator line first, so it always does. A UBSan diagnostic does NOT always
/// start its line (after a partial line of program output it is printed
/// mid-line; measured with gcc and clang), so it is recognised instead by the
/// stack trace `print_stacktrace=1` prints after every real one.
fn parse_asan_ubsan(stderr: &str) -> Reports {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut reports = Reports::default();
    // Inside a LeakSanitizer report, and whether it has produced a leak yet.
    let mut leak_report: Option<bool> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some((tool, rest)) = sanitizer_error_header(line) {
            if leak_report == Some(false) {
                reports.findings.push(unlocated_leak());
            }
            if tool == "LeakSanitizer" {
                leak_report = Some(false);
                continue;
            }
            leak_report = None;
            let location = stack_after(&lines, i + 1).and_then(|frames| report_location(&frames));
            let source = if tool == "MemorySanitizer" {
                "msan"
            } else {
                "asan"
            };
            reports
                .findings
                .push(located_finding(source, location, 0, asan_bug_class(rest)));
        } else if let Some(leak) = leak_report.is_some().then(|| leak_block(line)).flatten() {
            leak_report = Some(true);
            let location = stack_after(&lines, i + 1).and_then(|frames| report_location(&frames));
            reports
                .findings
                .push(located_finding("lsan", location, 0, leak.to_string()));
        } else if let Some(idx) = line.find(RUNTIME_ERROR) {
            match stack_after(&lines, i + 1) {
                Some(frames) => {
                    let (file, line_no, column) = ubsan_location(&line[..idx], &frames);
                    reports.findings.push(Finding {
                        source: "ubsan".to_string(),
                        severity: Severity::Error,
                        file,
                        line: line_no,
                        column,
                        message: line[idx + RUNTIME_ERROR.len()..].trim().to_string(),
                        rule: None,
                    });
                }
                None => reports.lookalikes += 1,
            }
        } else if line.contains("ERROR: AddressSanitizer: ")
            || line.contains("ERROR: LeakSanitizer: ")
        {
            reports.lookalikes += 1;
        }
        if line.starts_with("SUMMARY: ") {
            if leak_report == Some(false) {
                reports.findings.push(unlocated_leak());
            }
            leak_report = None;
        }
    }
    if leak_report == Some(false) {
        reports.findings.push(unlocated_leak());
    }
    // A recovering UBSan reports every occurrence, so a bug
    // inside a loop would otherwise fill FINDINGS_CAP with one bug.
    dedupe_findings(&mut reports.findings);
    reports
}

/// A LeakSanitizer report whose leak blocks were not captured (cut output).
fn unlocated_leak() -> Finding {
    located_finding("lsan", None, 0, "detected memory leaks".to_string())
}

fn located_finding(
    source: &str,
    location: Option<(String, u32)>,
    column: u32,
    message: String,
) -> Finding {
    let (file, line) = location.unwrap_or_default();
    Finding {
        source: source.to_string(),
        severity: Severity::Error,
        file,
        line,
        column,
        message,
        rule: None,
    }
}

/// `==PID==ERROR: AddressSanitizer: …` / `==PID==ERROR: LeakSanitizer: …` at
/// the START of the line. Returns the sanitizer and the text after it.
fn sanitizer_error_header(line: &str) -> Option<(&'static str, &str)> {
    let rest = strip_pid_prefix(line)?;
    // MemorySanitizer reports as a WARNING; the program still stops on it.
    if let Some(bug) = rest
        .strip_prefix("WARNING: MemorySanitizer: ")
        .or_else(|| rest.strip_prefix("ERROR: MemorySanitizer: "))
    {
        return Some(("MemorySanitizer", bug));
    }
    let rest = rest.strip_prefix("ERROR: ")?;
    ["AddressSanitizer", "LeakSanitizer"]
        .into_iter()
        .find_map(|tool| Some((tool, rest.strip_prefix(tool)?.strip_prefix(": ")?)))
}

/// Strip the `==PID==` prefix the sanitizer runtimes put on their own lines.
pub(crate) fn strip_pid_prefix(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("==")?;
    let (pid, rest) = rest.split_once("==")?;
    (!pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then_some(rest)
}

/// `Direct leak of 32 byte(s) in 1 object(s) allocated from:` → the part a
/// finding should carry.
fn leak_block(line: &str) -> Option<&str> {
    if !(line.starts_with("Direct leak of ") || line.starts_with("Indirect leak of ")) {
        return None;
    }
    Some(line.trim_end().trim_end_matches(" allocated from:"))
}

/// The bug class of an ASan header, from the text after `AddressSanitizer: `.
/// Cut before the address, never at the first word (or a
/// double free renders as "attempting"):
/// `attempting double-free on 0x602000000010 in thread T0:` → `attempting
/// double-free`, `SEGV on unknown address 0x0 (pc …)` → `SEGV`.
pub(crate) fn asan_bug_class(rest: &str) -> String {
    let end = [" on address 0x", " on unknown address", " on 0x", ": 0x"]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    rest[..end].trim().trim_end_matches(':').to_string()
}

/// Where a UBSan `runtime error:` happened, from the text before it on its
/// line and the report's own stack.
///
/// The line's `file:line:col` is exact when the report starts its line; when
/// it started mid-line, program output precedes the file name. Either way the
/// stack's frame for the same line names the file cleanly, so that is used
/// whenever it agrees.
fn ubsan_location(prefix: &str, frames: &[Frame]) -> (String, u32, u32) {
    let mut parts = prefix.rsplitn(3, ':');
    let column = parts.next().and_then(|c| c.parse::<u32>().ok());
    let line = parts.next().and_then(|l| l.parse::<u32>().ok());
    let file = parts.next().filter(|f| !f.is_empty());
    match (file, line, column, report_location(frames)) {
        (Some(file), Some(line), Some(column), Some((frame_file, frame_line)))
            if frame_line == line && file.ends_with(basename(&frame_file)) =>
        {
            (frame_file, line, column)
        }
        (Some(file), Some(line), Some(column), _) => (file.to_string(), line, column),
        (_, _, _, Some((frame_file, frame_line))) => (frame_file, frame_line, 0),
        _ => (String::new(), 0, 0),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One `#N …` line of a sanitizer stack trace.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) function: String,
    /// `None` for an unsymbolized frame (`(module+0xoff)` only, or `<null>`).
    pub(crate) location: Option<(String, u32)>,
}

/// Parse a stack-frame line. ASan/LSan/UBSan print `#N 0xADDR in FUNCTION
/// FILE:LINE[:COL]`, TSan `#N FUNCTION FILE:LINE[:COL] (MODULE+0xOFF)`, and
/// clang appends ` (BuildId: …)` to either.
pub(crate) fn parse_frame(line: &str) -> Option<Frame> {
    let rest = line.trim_start().strip_prefix('#')?;
    let (index, rest) = rest.split_once(' ')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut rest = rest.trim();
    if let Some(address) = rest.strip_prefix("0x") {
        // ASan shape. With no ` in FUNCTION` the frame is an address and a
        // module only.
        rest = match address.split_once(" in ") {
            Some((_, after)) => after,
            None => "",
        };
    }
    // Peel the trailing `(module+0xoff)` / `(BuildId: …)` groups. A function
    // signature can end in `)` too, so only groups of those two shapes go.
    while rest.ends_with(')') {
        match rest.rfind(" (") {
            Some(at) if rest[at..].contains("+0x") || rest[at..].starts_with(" (BuildId: ") => {
                rest = rest[..at].trim_end();
            }
            _ => break,
        }
    }
    Some(match split_function_and_location(rest) {
        Some((function, file, line)) => Frame {
            function,
            location: Some((file, line)),
        },
        None => Frame {
            function: rest.to_string(),
            location: None,
        },
    })
}

/// Split `FUNCTION FILE:LINE[:COL]` into (function, file, line).
///
/// The file AND the function may contain spaces (`…/cm spaces/util.cpp`,
/// `operator new[](unsigned long)`), so the location would be cut to
/// `spaces/util.cpp` if taken as the last whitespace-separated token. The
/// file is found by its shape instead: an absolute path starts at the first
/// ` /`, a relative one at the first ` ./` or ` ../`; only a bare relative
/// name falls back to the last space.
fn split_function_and_location(text: &str) -> Option<(String, String, u32)> {
    let (rest, last) = text.rsplit_once(':')?;
    let last = last.parse::<u32>().ok()?;
    let (head, line) = match rest.rsplit_once(':') {
        Some((head, line)) => match line.parse::<u32>() {
            Ok(line) => (head, line),
            Err(_) => (rest, last),
        },
        None => (rest, last),
    };
    let file_start = [" /", " ./", " ../"]
        .iter()
        .filter_map(|marker| head.find(marker))
        .min()
        .or_else(|| head.rfind(' '))
        .map_or(0, |at| at + 1);
    let file = &head[file_start..];
    if file.is_empty() {
        return None;
    }
    Some((
        head[..file_start].trim().to_string(),
        file.to_string(),
        line,
    ))
}

pub(crate) fn is_frame_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix('#')
        .and_then(|rest| rest.split_once(' '))
        .is_some_and(|(index, _)| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
}

/// The stack trace that follows the line before `from`: the consecutive frame
/// lines after at most a few lines of report detail (`READ of size…`, a UBSan
/// note and its hex dump). `None` when another report, a separator or a blank
/// line comes first — a line that only looked like a report must not borrow
/// the stack of a real one printed after it.
fn stack_after(lines: &[&str], from: usize) -> Option<Vec<Frame>> {
    const MAX_DETAIL_LINES: usize = 8;
    let mut at = from;
    while at < lines.len() && !is_frame_line(lines[at]) {
        let line = lines[at];
        if at - from >= MAX_DETAIL_LINES
            || line.trim().is_empty()
            || line.starts_with("===")
            || sanitizer_error_header(line).is_some()
            || line.contains(RUNTIME_ERROR)
        {
            return None;
        }
        at += 1;
    }
    let frames = frames_from(lines, at);
    (!frames.is_empty()).then_some(frames)
}

/// The consecutive frame lines starting at `at`.
fn frames_from(lines: &[&str], at: usize) -> Vec<Frame> {
    lines
        .get(at..)
        .unwrap_or_default()
        .iter()
        .take_while(|line| is_frame_line(line))
        .filter_map(|line| parse_frame(line))
        .collect()
}

/// Whose code a stack frame is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameOwner {
    User,
    /// The C/C++ standard library or libc: a fallback location only.
    Library,
    /// The sanitizer's (or valgrind's) own runtime: never a location.
    Runtime,
}

/// Frame #0 of many real reports is the sanitizer's own interceptor —
/// `__interceptor_free` in `asan_malloc_linux.cpp:52` for a double free,
/// `__interceptor_memcpy` in `sanitizer_common_interceptors.inc:827` for a
/// memcpy overflow, valgrind's `vg_replace_malloc.c:381` for every leak — or a
/// standard-library header. A finding located there points at code the user
/// cannot change; the frame that called it is the one to fix.
pub(crate) fn frame_owner(function: &str, file: &str) -> FrameOwner {
    const RUNTIME_FILES: &[&str] = &[
        "/libsanitizer/",
        "compiler-rt/lib/",
        "sanitizer_common_interceptors",
        "vg_replace_",
        "/valgrind/",
    ];
    const RUNTIME_FUNCTIONS: &[&str] = &[
        "__interceptor_",
        "___interceptor_",
        "__interception::",
        "__asan_",
        "__lsan_",
        "__tsan_",
        "__ubsan_",
        "__sanitizer_",
    ];
    if RUNTIME_FILES.iter().any(|marker| file.contains(marker))
        || RUNTIME_FUNCTIONS
            .iter()
            .any(|prefix| function.starts_with(prefix))
    {
        return FrameOwner::Runtime;
    }
    if file.starts_with("/usr/include/")
        || file.contains("/include/c++/")
        || file.contains("sysdeps/")
        || file.starts_with("csu/")
        || file.starts_with("../csu/")
        || function.starts_with("__libc_")
    {
        return FrameOwner::Library;
    }
    FrameOwner::User
}

/// The location a finding should carry for one stack: the first USER frame,
/// else the first library frame, never a runtime frame.
fn report_location(frames: &[Frame]) -> Option<(String, u32)> {
    let located = |wanted: FrameOwner| {
        frames.iter().find_map(|frame| {
            let (file, line) = frame.location.as_ref()?;
            (frame_owner(&frame.function, file) == wanted).then(|| (file.clone(), *line))
        })
    };
    located(FrameOwner::User).or_else(|| located(FrameOwner::Library))
}

/// The ASan+UBSan verdict for one run, shared by the single-TU and project
/// paths so a look-alike line reads the same in both.
fn asan_ubsan_verdict(reports: &Reports, exit_label: &str, truncated: bool) -> String {
    let mut verdict = verdict_line(&reports.findings, exit_label, truncated);
    if reports.lookalikes > 0 {
        verdict.push_str(&format!(
            " — note: {} line(s) of stderr resemble a sanitizer report but are not one (quoted \
             inside other output, or a `runtime error:` line with no stack trace after it), so \
             they were not counted; they are in the raw stderr below.",
            reports.lookalikes
        ));
    }
    verdict
}

/// What a ThreadSanitizer run's captured stderr actually says.
#[derive(Debug, PartialEq, Eq)]
enum TsanOutcome {
    /// The runtime itself failed, e.g. "FATAL: ThreadSanitizer: unexpected
    /// memory mapping". The program was never checked: neither a race nor a
    /// clean run, and it must never be reported as either.
    Fatal(String),
    /// A real finding: the `<kind>` of "WARNING: ThreadSanitizer: <kind>". TSan
    /// reports more than races here (lock-order-inversion, thread leak,
    /// signal-unsafe call), so the kind is carried through verbatim instead of
    /// being hardcoded to "data race".
    Report(String),
    /// No ThreadSanitizer diagnostic in the captured output.
    Silent,
}

/// `LEVEL: ThreadSanitizer: …` at the START of a line (optionally behind the
/// `==PID==` prefix), returning the text after it. Anchored for the same
/// reason as [`parse_asan_ubsan`]: a program that logs a quoted TSan
/// warning did not race. TSan prints a separator line before every report, so
/// a real one always starts its line.
fn tsan_line<'a>(line: &'a str, level: &str) -> Option<&'a str> {
    let line = strip_pid_prefix(line).unwrap_or(line);
    line.strip_prefix(level)?.strip_prefix(": ThreadSanitizer:")
}

fn classify_tsan(stderr: &str) -> TsanOutcome {
    // A FATAL line wins regardless of what else was printed: the run is invalid.
    for line in stderr.lines() {
        if let Some(message) = tsan_line(line, "FATAL") {
            return TsanOutcome::Fatal(message.trim().to_string());
        }
    }
    for line in stderr.lines() {
        for level in ["WARNING", "ERROR"] {
            if let Some(after) = tsan_line(line, level) {
                let after = after.trim_start();
                let end = after.find(" (").unwrap_or(after.len());
                return TsanOutcome::Report(after[..end].trim().to_string());
            }
        }
    }
    TsanOutcome::Silent
}

/// Turn a ThreadSanitizer "WARNING: ThreadSanitizer: data race" report into
/// a structured finding, located at the first user frame of its first stack.
fn parse_tsan_report(stderr: &str) -> Option<Finding> {
    // Only a real report becomes a finding — a FATAL startup failure is not a
    // race and must not be rendered as one.
    let TsanOutcome::Report(kind) = classify_tsan(stderr) else {
        return None;
    };
    let lines: Vec<&str> = stderr.lines().collect();
    let header = lines.iter().position(|line| {
        tsan_line(line, "WARNING")
            .or_else(|| tsan_line(line, "ERROR"))
            .is_some()
    })?;
    // The first stack after the header, however far: a race's follows a
    // `Read of size 4 … by thread T2:` line, a lock-order-inversion's a blank
    // line and a cycle description.
    let location = lines[header + 1..]
        .iter()
        .position(|line| is_frame_line(line))
        .and_then(|offset| report_location(&frames_from(&lines, header + 1 + offset)));
    Some(located_finding("tsan", location, 0, kind))
}

/// Hand-rolled scan of valgrind's XML output. Extracts each `<error>` element's
/// `<kind>`, its descriptive text (`<xwhat><text>` or `<what>`), and the first
/// frame that carries `<file>`/`<line>`.
fn parse_valgrind_xml(xml: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find("<error>") {
        let after_start = &rest[start + "<error>".len()..];
        let (body, next) = match after_start.find("</error>") {
            Some(end) => (&after_start[..end], &after_start[end + "</error>".len()..]),
            None => (after_start, ""),
        };
        rest = next;

        let kind = inner_text(body, "kind").unwrap_or_else(|| "Error".to_string());
        let message = inner_text(body, "text")
            .or_else(|| inner_text(body, "what"))
            .unwrap_or_else(|| kind.clone());

        // The first user frame with a <file> and <line>.
        let (file, line_no) = first_frame_location(body).unwrap_or_else(|| (String::new(), 0));

        findings.push(Finding {
            source: "valgrind".to_string(),
            severity: Severity::Error,
            file,
            line: line_no,
            column: 0,
            message: format!("{kind}: {message}"),
            rule: None,
        });
    }

    findings
}

/// Extract the text of the first `<tag>...</tag>` within `body`.
fn inner_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    let text = body[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// The location of an error's first `<frame>` in user code, else in a
/// library, never in valgrind's own replacements (or every leak is
/// reported at `vg_replace_malloc.c:381`, valgrind's `malloc`).
fn first_frame_location(body: &str) -> Option<(String, u32)> {
    let mut frames = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<frame>") {
        let after = &rest[start + "<frame>".len()..];
        let (frame, next) = match after.find("</frame>") {
            Some(end) => (&after[..end], &after[end + "</frame>".len()..]),
            None => (after, ""),
        };
        rest = next;

        if let (Some(file), Some(line_str)) = (inner_text(frame, "file"), inner_text(frame, "line"))
        {
            if let Ok(line_no) = line_str.parse::<u32>() {
                // Ownership is judged on the full path (`<obj>` and `<dir>`
                // name the runtime), the finding keeps the file name as before.
                let path = format!(
                    "{} {}/{file}",
                    inner_text(frame, "obj").unwrap_or_default(),
                    inner_text(frame, "dir").unwrap_or_default()
                );
                let owner = frame_owner(&inner_text(frame, "fn").unwrap_or_default(), &path);
                frames.push((owner, file, line_no));
            }
        }
    }
    [FrameOwner::User, FrameOwner::Library]
        .into_iter()
        .find_map(|wanted| {
            frames
                .iter()
                .find(|(owner, _, _)| *owner == wanted)
                .map(|(_, file, line)| (file.clone(), *line))
        })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The first finding `parse_asan_ubsan` makes of `stderr`.
    fn first_finding(stderr: &str) -> Option<Finding> {
        parse_asan_ubsan(stderr).findings.into_iter().next()
    }

    /// A UBSan diagnostic as the runtime prints it with `print_stacktrace=1`:
    /// the line, then its stack, frame #0 at the same file and line.
    fn ubsan_report(line: &str) -> String {
        let (location, _) = line.split_once(RUNTIME_ERROR).expect("a UBSan line");
        let (file_and_line, _column) = location.rsplit_once(':').expect("file:line:col");
        format!("{line}\n    #0 0x55 in main {file_and_line}\n")
    }

    #[test]
    fn extract_db_flags_captures_target_isa_flags() {
        // -march / -mavx2 / -mfma must survive so a SIMD TU compiles under
        // the sanitizer instead of failing "requires target feature 'avx'".
        let toks: Vec<String> = [
            "c++",
            "-std=c++17",
            "-Iinc",
            "-mavx2",
            "-mfma",
            "-march=native",
            "-O2",
            "-c",
            "dot.cpp",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let f = extract_db_flags(&toks, Path::new("/proj"));
        assert!(f.march.contains(&"-mavx2".to_string()), "{:?}", f.march);
        assert!(f.march.contains(&"-mfma".to_string()));
        assert!(f.march.contains(&"-march=native".to_string()));
        // -O2 is deliberately NOT captured (not ISA; keeps the build line minimal).
        assert!(!f.march.iter().any(|m| m == "-O2"));
        // -std still handled by its own arm, not swept into march.
        assert_eq!(f.std.as_deref(), Some("c++17"));
    }

    #[test]
    fn tsan_is_an_accepted_sanitizer() {
        // Exercise the REAL guard used by handle_call, not a restated set.
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"a.cpp","sanitizer":"tsan"}"#).unwrap();
        assert!(is_accepted_sanitizer(args.sanitizer.as_deref().unwrap()));
        assert!(is_accepted_sanitizer("asan_ubsan"));
        assert!(is_accepted_sanitizer("valgrind"));
        // and the guard still rejects everything else
        assert!(is_accepted_sanitizer("msan"));
        assert!(!is_accepted_sanitizer("thread"));
        assert!(!is_accepted_sanitizer("memory"));
        assert!(!is_accepted_sanitizer(""));
    }

    #[test]
    fn mllvm_and_operand_taking_flags_never_reach_the_build() {
        // Security: -mllvm forwards an arbitrary option to the LLVM backend
        // (it can switch sanitizer instrumentation OFF) and takes its value as a
        // SEPARATE token, so copying it would also swallow our own -fsanitize.
        assert!(!is_safe_target_flag("-mllvm"));
        assert!(!is_safe_target_flag(
            "-mllvm=-tsan-instrument-memory-accesses=0"
        ));
        assert!(!is_safe_target_flag("-multiply_defined"));
        assert!(!is_safe_target_flag("-m"));
        assert!(!is_safe_target_flag("-fsanitize=address"));
        // …while the real ISA flags the allowlist exists for still pass.
        for ok in [
            "-mavx2",
            "-mfma",
            "-msse4.2",
            "-mno-avx512f",
            "-mpopcnt",
            "-m64",
            "-march=native",
            "-mtune=skylake",
        ] {
            assert!(is_safe_target_flag(ok), "should be allowed: {ok}");
        }
        // end-to-end through the extractor
        let toks: Vec<String> = [
            "clang++",
            "-mllvm",
            "-tsan-instrument-memory-accesses=0",
            "-mavx2",
            "-c",
            "a.cpp",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let f = extract_db_flags(&toks, Path::new("/proj"));
        assert_eq!(
            f.march,
            vec!["-mavx2".to_string()],
            "only the ISA flag survives"
        );
    }

    #[test]
    fn tsan_classification_distinguishes_fatal_report_and_silence() {
        // A FATAL startup failure is NOT a race and NOT clean.
        let fatal = "FATAL: ThreadSanitizer: unexpected memory mapping 0x55\n";
        assert_eq!(
            classify_tsan(fatal),
            TsanOutcome::Fatal("unexpected memory mapping 0x55".to_string())
        );
        assert!(
            parse_tsan_report(fatal).is_none(),
            "FATAL must not become a race finding"
        );
        // A non-race TSan report keeps its own kind.
        let lock =
            "==1==WARNING: ThreadSanitizer: lock-order-inversion (potential deadlock) (pid=1)\n";
        assert_eq!(
            classify_tsan(lock),
            TsanOutcome::Report("lock-order-inversion".to_string())
        );
        assert_eq!(classify_tsan("all quiet\n"), TsanOutcome::Silent);
    }

    #[test]
    fn tsan_frame_parse_survives_a_cpp_signature_with_a_space_paren() {
        // Splitting on the first " (" would cut `f(void (*)())` and lose the location.
        let stderr = "==1==WARNING: ThreadSanitizer: data race (pid=1)\n    \
                      #0 f(void (*)()) src/a.cpp:7 (a.out+0x1)\n";
        assert_eq!(
            parse_tsan_report(stderr).map(|f| (f.file, f.line)),
            Some(("src/a.cpp".to_string(), 7))
        );
    }

    #[test]
    fn parse_tsan_report_extracts_the_race_and_location() {
        let stderr = "==1==WARNING: ThreadSanitizer: data race (pid=1)\n  \
                      Write of size 8 at 0x7b by thread T2:\n    \
                      #0 wq::WorkQueue::worker_loop() src/queue.cpp:59 (a.out+0x12ab)\n";
        let f = parse_tsan_report(stderr).expect("should parse a data race");
        assert_eq!(f.source, "tsan");
        assert_eq!(f.message, "data race");
        assert_eq!(f.file, "src/queue.cpp");
        assert_eq!(f.line, 59);
    }

    #[test]
    fn parse_tsan_report_none_when_clean() {
        assert!(parse_tsan_report("no warnings here\nexit 0\n").is_none());
    }

    #[test]
    fn parses_gcc_error_line() {
        let finding =
            parse_gcc_line("main.cpp:10:5: error: expected ';' before '}' token").unwrap();
        assert_eq!(finding.source, "gcc");
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.file, "main.cpp");
        assert_eq!(finding.line, 10);
        assert_eq!(finding.column, 5);
        assert_eq!(finding.message, "expected ';' before '}' token");
    }

    #[test]
    fn ignores_non_diagnostic_gcc_lines() {
        assert_eq!(parse_gcc_line("In file included from main.cpp:1:"), None);
        assert_eq!(parse_gcc_line(""), None);
    }

    #[test]
    fn parses_asan_heap_use_after_free() {
        let stderr = "\
=================================================================
==12345==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000010 at pc 0x...
READ of size 4 at 0x602000000010 thread T0
    #0 0x4f1234 in main /home/u/bug.cpp:7:12
    #1 0x7f0000 in __libc_start_main
";
        let finding = first_finding(stderr).expect("asan finding");
        assert_eq!(finding.source, "asan");
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.message, "heap-use-after-free");
        assert_eq!(finding.file, "/home/u/bug.cpp");
        assert_eq!(finding.line, 7);
    }

    #[test]
    fn asan_report_without_frame_still_reports_bug_class() {
        let stderr = "==1==ERROR: AddressSanitizer: stack-buffer-overflow on address 0x...";
        let finding = first_finding(stderr).expect("asan finding");
        assert_eq!(finding.message, "stack-buffer-overflow");
        assert_eq!(finding.file, "");
        assert_eq!(finding.line, 0);
    }

    #[test]
    fn no_asan_finding_in_clean_output() {
        assert_eq!(first_finding("hello world\n"), None);
    }

    #[test]
    fn parses_ubsan_runtime_error() {
        let line = "bug.cpp:4:9: runtime error: signed integer overflow: 2147483647 + 1 cannot be represented in type 'int'";
        let finding = first_finding(&ubsan_report(line)).expect("ubsan finding");
        assert_eq!(finding.source, "ubsan");
        assert_eq!(finding.file, "bug.cpp");
        assert_eq!(finding.line, 4);
        assert_eq!(finding.column, 9);
        assert_eq!(
            finding.message,
            "signed integer overflow: 2147483647 + 1 cannot be represented in type 'int'"
        );
    }

    #[test]
    fn ubsan_ignores_unrelated_lines() {
        assert!(
            parse_asan_ubsan("    #0 0x.. in main bug.cpp:4")
                .findings
                .is_empty()
        );
    }

    #[test]
    fn parses_valgrind_xml_error() {
        let xml = "\
<?xml version=\"1.0\"?>
<valgrindoutput>
<error>
  <unique>0x1</unique>
  <kind>InvalidRead</kind>
  <xwhat>
    <text>Invalid read of size 4</text>
  </xwhat>
  <stack>
    <frame>
      <ip>0x108AED</ip>
      <fn>main</fn>
      <file>leak.cpp</file>
      <line>9</line>
    </frame>
  </stack>
</error>
</valgrindoutput>
";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.source, "valgrind");
        assert_eq!(f.file, "leak.cpp");
        assert_eq!(f.line, 9);
        assert_eq!(f.message, "InvalidRead: Invalid read of size 4");
    }

    #[test]
    fn parses_valgrind_leak_with_what_fallback() {
        let xml = "\
<error>
  <kind>Leak_DefinitelyLost</kind>
  <what>40 bytes in 1 blocks are definitely lost</what>
  <stack>
    <frame><fn>operator new</fn></frame>
    <frame><fn>main</fn><file>leak.cpp</file><line>5</line></frame>
  </stack>
</error>
";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, "leak.cpp");
        assert_eq!(f.line, 5);
        assert_eq!(
            f.message,
            "Leak_DefinitelyLost: 40 bytes in 1 blocks are definitely lost"
        );
    }

    #[test]
    fn empty_valgrind_xml_yields_no_findings() {
        assert!(parse_valgrind_xml("<valgrindoutput></valgrindoutput>").is_empty());
        assert!(parse_valgrind_xml("").is_empty());
    }

    #[test]
    fn truncates_long_stdout() {
        let s = "x".repeat(10);
        assert_eq!(truncate(&s, 4), "xxxx... (truncated)");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path(cwd, "src/a.cpp"),
            PathBuf::from("/work/src/a.cpp")
        );
        assert_eq!(resolve_path(cwd, "/abs/b.cpp"), PathBuf::from("/abs/b.cpp"));
    }

    // ---- helpers -----------------------------------------------------------

    fn finding(source: &str, file: &str, line: u32, message: &str) -> Finding {
        Finding {
            source: source.to_string(),
            severity: Severity::Error,
            file: file.to_string(),
            line,
            column: 0,
            message: message.to_string(),
            rule: None,
        }
    }

    // ---- ASan: every bug class, with and without a symbolized frame --------

    #[test]
    fn parses_asan_heap_buffer_overflow_with_frame() {
        let stderr = "\
==9==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602 at pc 0x...
WRITE of size 4 at 0x602 thread T0
    #0 0x4011ab in main /src/heap.cpp:12:7
    #1 0x7f in __libc_start_main
";
        let f = first_finding(stderr).expect("asan finding");
        assert_eq!(f.message, "heap-buffer-overflow");
        assert_eq!(f.file, "/src/heap.cpp");
        assert_eq!(f.line, 12);
    }

    #[test]
    fn parses_asan_stack_buffer_overflow_with_frame() {
        let stderr = "\
==1==ERROR: AddressSanitizer: stack-buffer-overflow on address 0x7ff at pc 0x...
    #0 0x401000 in run(int) /src/stack.cpp:3:5
";
        let f = first_finding(stderr).expect("asan finding");
        assert_eq!(f.message, "stack-buffer-overflow");
        assert_eq!(f.file, "/src/stack.cpp");
        assert_eq!(f.line, 3);
    }

    #[test]
    fn parses_asan_global_buffer_overflow_with_frame() {
        let stderr = "\
==1==ERROR: AddressSanitizer: global-buffer-overflow on address 0x55 at pc 0x...
    #0 0x401234 in main /src/global.cpp:8:1
";
        let f = first_finding(stderr).expect("asan finding");
        assert_eq!(f.message, "global-buffer-overflow");
        assert_eq!(f.file, "/src/global.cpp");
        assert_eq!(f.line, 8);
    }

    #[test]
    fn asan_heap_buffer_overflow_without_frame() {
        let f =
            first_finding("==1==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x...")
                .expect("asan finding");
        assert_eq!(f.message, "heap-buffer-overflow");
        assert_eq!(f.file, "");
        assert_eq!(f.line, 0);
    }

    #[test]
    fn asan_global_buffer_overflow_without_frame() {
        let f =
            first_finding("==1==ERROR: AddressSanitizer: global-buffer-overflow on address 0x...")
                .expect("asan finding");
        assert_eq!(f.message, "global-buffer-overflow");
        assert_eq!(f.file, "");
        assert_eq!(f.line, 0);
    }

    // ---- frame-0 parsing ---------------------------------------------------

    fn frame_location(line: &str) -> Option<(String, u32)> {
        parse_frame(line).and_then(|frame| frame.location)
    }

    #[test]
    fn parse_frame0_with_column() {
        assert_eq!(
            frame_location("    #0 0x4f1234 in main /home/u/bug.cpp:7:12"),
            Some(("/home/u/bug.cpp".to_string(), 7))
        );
    }

    #[test]
    fn parse_frame0_without_column() {
        assert_eq!(
            frame_location("    #0 0x1 in f a.cpp:5"),
            Some(("a.cpp".to_string(), 5))
        );
    }

    #[test]
    fn parse_frame0_rejects_malformed() {
        // Missing the " in " segment.
        assert_eq!(frame_location("#0 0xabc main a.cpp:5"), None);
        // Not a frame line at all.
        assert_eq!(parse_frame("a.cpp:1:2: note: here"), None);
        assert_eq!(parse_frame("#x 0xabc in main a.cpp:5"), None);
        // Frames past #0 parse too: they are needed to find the first frame in
        // the user's own code.
        assert_eq!(
            frame_location("#1 0xabc in main a.cpp:5"),
            Some(("a.cpp".to_string(), 5))
        );
    }

    // ---- UBSan: distinct runtime-error classes -----------------------------

    #[test]
    fn parses_ubsan_null_pointer_deref() {
        let f = first_finding(&ubsan_report(
            "x.cpp:3:5: runtime error: load of null pointer of type 'int'",
        ))
        .expect("ubsan finding");
        assert_eq!(f.source, "ubsan");
        assert_eq!(f.file, "x.cpp");
        assert_eq!(f.line, 3);
        assert_eq!(f.column, 5);
        assert_eq!(f.message, "load of null pointer of type 'int'");
    }

    #[test]
    fn parses_ubsan_shift_exponent() {
        let f = first_finding(&ubsan_report(
            "x.cpp:2:7: runtime error: shift exponent 32 is too large for 32-bit type 'int'",
        ))
        .expect("ubsan finding");
        assert_eq!(f.line, 2);
        assert_eq!(f.column, 7);
        assert_eq!(
            f.message,
            "shift exponent 32 is too large for 32-bit type 'int'"
        );
    }

    // ---- g++ compile diagnostics: error vs warning vs note -----------------

    #[test]
    fn parses_gcc_warning_line() {
        let f = parse_gcc_line("a.cpp:9:3: warning: unused variable 'x' [-Wunused-variable]")
            .expect("gcc finding");
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.file, "a.cpp");
        assert_eq!(f.line, 9);
        assert_eq!(f.column, 3);
        assert_eq!(f.message, "unused variable 'x' [-Wunused-variable]");
    }

    #[test]
    fn gcc_note_severity_is_ignored() {
        // Only "error" and "warning" are recognised; "note" yields None.
        assert_eq!(
            parse_gcc_line("a.cpp:1:1: note: in expansion of macro"),
            None
        );
    }

    // ---- valgrind XML: more kinds, multiple errors, frame fallbacks --------

    #[test]
    fn parses_valgrind_invalid_write() {
        let xml = "\
<error>
  <kind>InvalidWrite</kind>
  <xwhat><text>Invalid write of size 4</text></xwhat>
  <stack><frame><fn>main</fn><file>w.cpp</file><line>11</line></frame></stack>
</error>
";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "InvalidWrite: Invalid write of size 4");
        assert_eq!(findings[0].file, "w.cpp");
        assert_eq!(findings[0].line, 11);
    }

    #[test]
    fn parses_valgrind_uninitialised_value() {
        // UninitValue uses <what>, not <xwhat>.
        let xml = "\
<error>
  <kind>UninitValue</kind>
  <what>Use of uninitialised value of size 8</what>
  <stack><frame><fn>main</fn><file>u.cpp</file><line>6</line></frame></stack>
</error>
";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "UninitValue: Use of uninitialised value of size 8"
        );
        assert_eq!(findings[0].file, "u.cpp");
        assert_eq!(findings[0].line, 6);
    }

    #[test]
    fn parses_valgrind_multiple_errors() {
        let xml = "\
<valgrindoutput>
<error><kind>InvalidRead</kind><what>r</what>
  <stack><frame><file>a.cpp</file><line>1</line></frame></stack></error>
<error><kind>InvalidWrite</kind><what>w</what>
  <stack><frame><file>b.cpp</file><line>2</line></frame></stack></error>
</valgrindoutput>
";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].message, "InvalidRead: r");
        assert_eq!(findings[0].file, "a.cpp");
        assert_eq!(findings[1].message, "InvalidWrite: w");
        assert_eq!(findings[1].file, "b.cpp");
    }

    #[test]
    fn parses_valgrind_error_without_frame() {
        let xml = "<error><kind>InvalidFree</kind><what>bad free</what></error>";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "InvalidFree: bad free");
        assert_eq!(findings[0].file, "");
        assert_eq!(findings[0].line, 0);
    }

    #[test]
    fn malformed_valgrind_xml_still_yields_finding() {
        // Unterminated <error>: the parser consumes to end-of-input.
        let xml = "<error><kind>InvalidRead</kind><xwhat><text>bad read</text></xwhat>";
        let findings = parse_valgrind_xml(xml);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "InvalidRead: bad read");
        assert_eq!(findings[0].file, "");
        assert_eq!(findings[0].line, 0);
    }

    #[test]
    fn inner_text_empty_and_missing_tags() {
        assert_eq!(inner_text("<text></text>", "text"), None);
        assert_eq!(inner_text("<a>x</a>", "b"), None);
        assert_eq!(inner_text("<a>  x  </a>", "a"), Some("x".to_string()));
    }

    // ---- stdout truncation at the cap boundary -----------------------------

    #[test]
    fn truncate_at_exact_cap_boundary() {
        // Exactly at cap: returned unchanged.
        assert_eq!(truncate("xxxx", 4), "xxxx");
    }

    #[test]
    fn truncate_one_over_cap() {
        assert_eq!(truncate("xxxxx", 4), "xxxx... (truncated)");
    }

    #[test]
    fn truncate_respects_multibyte_boundary() {
        // "aéb" is 4 bytes; cap=2 lands mid-'é', so it backs off to a boundary.
        assert_eq!(truncate("aéb", 2), "a... (truncated)");
    }

    // ---- run-output assembly: verdict (clean vs N issues) ------------------

    #[test]
    fn format_run_output_clean_verdict() {
        let out = format_run_output("", "clean: exit 0, no sanitizer errors", "", &[], "");
        assert_eq!(out, "clean: exit 0, no sanitizer errors\nNo findings.");
    }

    #[test]
    fn format_run_output_with_issues_and_stdout() {
        let findings = vec![
            finding("asan", "/x.cpp", 7, "heap-use-after-free"),
            finding("ubsan", "y.cpp", 4, "signed integer overflow"),
        ];
        let out = format_run_output("", "2 issue(s) (exit 1)", "hello\n", &findings, "");
        assert_eq!(
            out,
            "2 issue(s) (exit 1)\n\
             program stdout:\n\
             hello\n\
             asan error /x.cpp:7:0 heap-use-after-free\n\
             ubsan error y.cpp:4:0 signed integer overflow"
        );
    }

    #[test]
    fn format_run_output_skips_empty_stdout_includes_preamble() {
        let out = format_run_output(
            "note: llvm-symbolizer not on PATH\n",
            "clean: exit 0, no sanitizer errors",
            "   \n",
            &[],
            "",
        );
        assert_eq!(
            out,
            "note: llvm-symbolizer not on PATH\n\
             clean: exit 0, no sanitizer errors\n\
             No findings."
        );
    }

    // ======================================================================
    // The raw sanitizer report is returned, not thrown away
    // ======================================================================

    /// A verbatim heap-use-after-free report captured from g++ 12 / ASan on
    /// this machine (2,676 bytes; the shadow block is 1,409 of them).
    const REAL_ASAN_REPORT: &str = "\
=================================================================
==2241832==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000014 at pc 0x56104628a23c bp 0x7fff05aa1940 sp 0x7fff05aa1938
READ of size 4 at 0x602000000014 thread T0
    #0 0x56104628a23b in main /tmp/asan-probe/uaf.cpp:2
    #1 0x7f30cf245249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58

0x602000000014 is located 4 bytes inside of 16-byte region [0x602000000010,0x602000000020)
freed by thread T0 here:
    #0 0x7f30d00b76a8 in __interceptor_free ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:52
    #1 0x56104628a1d6 in main /tmp/asan-probe/uaf.cpp:2

previously allocated by thread T0 here:
    #0 0x7f30d00b89cf in __interceptor_malloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:69
    #1 0x56104628a1cb in main /tmp/asan-probe/uaf.cpp:2

SUMMARY: AddressSanitizer: heap-use-after-free /tmp/asan-probe/uaf.cpp:2 in main
Shadow bytes around the buggy address:
  0x0c047fff7fb0: fa fa fd fd fa fa 00 00 fa fa 00 00 fa fa 00 00
  0x0c047fff7fc0: fa fa 00 00 fa fa 00 00 fa fa 00 00 fa fa 00 00
Shadow byte legend (one shadow byte represents 8 application bytes):
  Addressable:           00
  Heap left redzone:       fa
  Freed heap region:       fd
==2241832==ABORTING
";

    #[test]
    fn run_output_carries_the_stack_traces_not_just_a_one_liner() {
        // The complete tool output the model receives for a real bug must not be
        // three lines. It must contain the ASan header and every stack.
        let findings = vec![finding(
            "asan",
            "/tmp/asan-probe/uaf.cpp",
            2,
            "heap-use-after-free",
        )];
        let out = format_run_output(
            "",
            "1 issue(s) (exit nonzero)",
            "",
            &findings,
            &prepare_trace(REAL_ASAN_REPORT),
        );
        assert!(
            out.contains("ERROR: AddressSanitizer: heap-use-after-free"),
            "{out}"
        );
        assert!(out.contains("READ of size 4"), "{out}");
        assert!(out.contains("freed by thread T0 here:"), "{out}");
        assert!(
            out.contains("previously allocated by thread T0 here:"),
            "{out}"
        );
        assert!(out.contains("__interceptor_free"), "{out}");
        assert!(out.contains("SUMMARY: AddressSanitizer"), "{out}");
        assert!(out.contains("sanitizer report (raw stderr)"), "{out}");
    }

    // ---- Shadow-byte map dropped before any byte cap ----------------------

    #[test]
    fn strip_shadow_block_removes_the_hex_map_and_legend() {
        let trimmed = strip_shadow_block(REAL_ASAN_REPORT);
        assert!(!trimmed.contains("Shadow bytes around"), "{trimmed}");
        assert!(!trimmed.contains("0x0c047fff7fb0"), "{trimmed}");
        assert!(!trimmed.contains("Shadow byte legend"), "{trimmed}");
        assert!(!trimmed.contains("Heap left redzone"), "{trimmed}");
        assert!(trimmed.contains("[shadow byte map elided"), "{trimmed}");
    }

    #[test]
    fn strip_shadow_block_keeps_the_actionable_half_and_the_report_boundary() {
        let trimmed = strip_shadow_block(REAL_ASAN_REPORT);
        assert!(trimmed.contains("ERROR: AddressSanitizer"), "{trimmed}");
        assert!(trimmed.contains("#0 0x56104628a23b in main"), "{trimmed}");
        assert!(trimmed.contains("SUMMARY: AddressSanitizer"), "{trimmed}");
        // The boundary line that closes the shadow block is NOT swallowed.
        assert!(trimmed.contains("==2241832==ABORTING"), "{trimmed}");
    }

    #[test]
    fn strip_shadow_block_recovers_a_second_report_after_the_block() {
        // Two concatenated reports: the cut must stop at the `====` separator
        // that opens the next one, not run to EOF.
        let two = format!("{REAL_ASAN_REPORT}=====\n==9==ERROR: AddressSanitizer: bad-free\n");
        let trimmed = strip_shadow_block(&two);
        assert!(
            trimmed.contains("bad-free"),
            "second report lost: {trimmed}"
        );
        assert!(!trimmed.contains("Shadow byte legend"), "{trimmed}");
    }

    #[test]
    fn strip_shadow_block_is_a_noop_without_the_marker() {
        let plain = "==1==ERROR: AddressSanitizer: stack-buffer-overflow\n";
        assert_eq!(strip_shadow_block(plain), plain);
    }

    #[test]
    fn shadow_map_is_the_bulk_of_the_report() {
        // The measurement that motivates the semantic trim: dropping the map must be a
        // large win, or it is not worth trimming by meaning first.
        let before = REAL_ASAN_REPORT.len();
        let after = strip_shadow_block(REAL_ASAN_REPORT).len();
        assert!(
            after < before,
            "the trim must actually shrink the report ({before} -> {after})"
        );
    }

    #[test]
    fn prepare_trace_applies_the_byte_cap_after_the_semantic_trim() {
        let noisy = format!("{}{}", REAL_ASAN_REPORT, "z".repeat(TRACE_CAP * 2));
        let trace = prepare_trace(&noisy);
        assert!(trace.len() <= TRACE_CAP + "... (truncated)".len());
        // The order matters: the header survives the cap because the shadow
        // block went first.
        assert!(trace.contains("ERROR: AddressSanitizer"), "{trace}");
        assert!(trace.contains("... (truncated)"), "{trace}");
    }

    #[test]
    fn program_stderr_before_the_report_cannot_push_it_out() {
        // 20 KB of the program's own logging, then the real report.
        let noisy = format!(
            "{}{}",
            "log line from the program\n".repeat(800),
            REAL_ASAN_REPORT
        );
        let trace = prepare_trace(&noisy);
        assert!(trace.contains("ERROR: AddressSanitizer"), "{trace}");
        assert!(trace.contains("before the report omitted"), "{trace}");
        assert!(trace.len() <= TRACE_CAP + 200);
    }

    #[test]
    fn a_sanitizer_abort_says_buffered_program_output_may_be_missing() {
        // printf output before the crash is lost; the answer must say so.
        let out = format_run_output("", "1 issue(s) (exit 1)", "", &[], REAL_ASAN_REPORT);
        assert!(out.contains("may be missing"), "{out}");
        // A clean run, or a recovering UBSan report, flushes normally.
        let out = format_run_output("", "clean: exit 0, no sanitizer errors", "hi\n", &[], "");
        assert!(!out.contains("may be missing"), "{out}");
    }

    #[test]
    fn prepare_trace_is_empty_for_a_silent_run() {
        assert_eq!(prepare_trace(""), "");
        assert_eq!(prepare_trace("   \n\n"), "");
    }

    #[test]
    fn layer1_budget_stays_far_under_the_layer0_capture_cap() {
        // The ordering invariant, asserted rather than described.
        assert!(TRACE_CAP + STDOUT_CAP < CAPTURE_CAP_BYTES);
    }

    // ======================================================================
    // UBSan must not pre-empt ASan
    // ======================================================================

    #[test]
    fn ubsan_is_allowed_to_recover_so_asan_can_still_report() {
        let env = sanitizer_env();
        let ubsan = env
            .iter()
            .find(|(k, _)| k == "UBSAN_OPTIONS")
            .map(|(_, v)| v.clone())
            .expect("UBSAN_OPTIONS set");
        assert!(
            !ubsan.contains("halt_on_error"),
            "halt_on_error=1 aborts on the first UBSan error and ASan never reports: {ubsan}"
        );
        assert!(ubsan.contains("print_stacktrace=1"), "{ubsan}");
    }

    #[test]
    fn asan_still_halts_on_its_own_error() {
        let env = sanitizer_env();
        let asan = env
            .iter()
            .find(|(k, _)| k == "ASAN_OPTIONS")
            .map(|(_, v)| v.clone())
            .expect("ASAN_OPTIONS set");
        // Memory is already corrupt after an ASan hit; continuing is not
        // meaningful the way it is for a recovered UBSan check.
        assert!(asan.contains("halt_on_error=1"), "{asan}");
        assert!(asan.contains("symbolize=1"), "{asan}");
    }

    #[test]
    fn leak_detection_is_gated_on_the_platform() {
        // LeakSanitizer is unsupported on Darwin.
        let asan = sanitizer_env()
            .into_iter()
            .find(|(k, _)| k == "ASAN_OPTIONS")
            .map(|(_, v)| v)
            .unwrap();
        if cfg!(target_os = "macos") {
            assert!(!asan.contains("detect_leaks"), "{asan}");
        } else {
            assert!(asan.contains("detect_leaks=1"), "{asan}");
        }
    }

    #[test]
    fn repeated_ubsan_errors_collapse_to_one_finding() {
        // A recovering UBSan reports every occurrence; a bug in a loop would
        // otherwise fill FINDINGS_CAP with one bug.
        let mut findings = vec![
            finding("ubsan", "a.cpp", 4, "signed integer overflow"),
            finding("ubsan", "a.cpp", 4, "signed integer overflow"),
            finding("ubsan", "a.cpp", 9, "signed integer overflow"),
        ];
        dedupe_findings(&mut findings);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line, 4);
        assert_eq!(findings[1].line, 9);
    }

    // ======================================================================
    // A truncated capture never reads as clean
    // ======================================================================

    #[test]
    fn truncated_output_is_never_a_clean_verdict() {
        let verdict = verdict_line(&[], "0", true);
        // It may *mention* cleanliness, but only to deny it — it must never be
        // the "clean: …" verdict a caller pattern-matches on.
        assert!(!verdict.starts_with("clean"), "{verdict}");
        assert!(verdict.contains("TRUNCATED"), "{verdict}");
        assert!(verdict.contains("NOT a clean result"), "{verdict}");
    }

    #[test]
    fn untruncated_zero_exit_with_no_findings_is_clean() {
        let verdict = verdict_line(&[], "0", false);
        assert!(
            verdict.starts_with("clean: exit 0, no sanitizer errors"),
            "{verdict}"
        );
        // Where LeakSanitizer does not exist, "clean" alone would claim
        // more than the run checked.
        if leak_detection_available() {
            assert_eq!(verdict, "clean: exit 0, no sanitizer errors");
        } else {
            assert!(verdict.contains("LEAKS WERE NOT CHECKED"), "{verdict}");
        }
    }

    #[test]
    fn the_clean_verdict_names_the_leak_gap_on_a_platform_without_lsan() {
        // Drives BOTH branches regardless of the host, so the macOS behaviour
        // is actually covered by a Linux test run.
        assert_eq!(
            clean_verdict(true),
            "clean: exit 0, no sanitizer errors",
            "a platform WITH LSan must keep the bare verdict"
        );
        let without = clean_verdict(false);
        assert!(
            without.starts_with("clean: exit 0, no sanitizer errors"),
            "{without}"
        );
        assert!(without.contains("LEAKS WERE NOT CHECKED"), "{without}");
        // The remedy must be one the user can actually follow: there is no
        // LeakSanitizer to install on macOS, so pointing at a package would be
        // a dead end.
        assert!(without.contains("Linux"), "{without}");
        assert!(!without.contains("install"), "{without}");
    }

    #[test]
    fn a_clean_run_without_leak_detection_says_leaks_were_not_checked() {
        // Asserted independently of the host platform: the note and the
        // runtime option must agree, because they are the same decision. The
        // failure this pins is silent — on macOS `sanitizer_env` drops
        // detect_leaks, so a leaky program exits 0 with no findings and the
        // tool used to call that "clean".
        let asan = sanitizer_env()
            .into_iter()
            .find(|(k, _)| k == "ASAN_OPTIONS")
            .map(|(_, v)| v)
            .unwrap();
        let verdict = verdict_line(&[], "0", false);
        assert_eq!(
            asan.contains("detect_leaks=1"),
            !verdict.contains("LEAKS WERE NOT CHECKED"),
            "instrumentation and verdict disagree: {asan} / {verdict}"
        );
        // And the remedy must be reachable: there is no LSan to install on
        // macOS, so the only honest advice is another platform.
        if !leak_detection_available() {
            assert!(verdict.contains("Linux"), "{verdict}");
            assert!(!verdict.contains("install"), "{verdict}");
        }
    }

    #[test]
    fn findings_produce_an_issue_count_even_on_exit_zero() {
        // UBSan recovery makes this reachable: a recovered UBSan error exits 0.
        let findings = vec![finding("ubsan", "a.cpp", 4, "overflow")];
        assert_eq!(verdict_line(&findings, "0", false), "1 issue(s) (exit 0)");
    }

    // ======================================================================
    // Compile and run timeouts report distinctly
    // ======================================================================

    #[test]
    fn compile_and_run_timeouts_are_distinguishable() {
        let plan = BuildPlan {
            compiler: "g++".to_string(),
            std: "c++23".to_string(),
            std_is_default: false,
            include_dirs: vec!["/w/include".to_string()],
            defines: vec![],
            march: vec![],
            semantics: Vec::new(),
            sources: vec![PathBuf::from("/w/a.cpp")],
            origin: "build: test".to_string(),
        };
        let compile = compile_timeout_message(&plan, 600, &[]);
        let run = run_timeout_message("build: test\n", 300);
        assert!(compile.contains("BUILD TIMED OUT"), "{compile}");
        assert!(compile.contains("not a crash"), "{compile}");
        assert!(compile.contains("timeout_sec"), "{compile}");
        assert!(run.contains("RUN TIMED OUT"), "{run}");
        assert!(run.contains("build succeeded"), "{run}");
        assert!(!run.contains("BUILD TIMED OUT"), "{run}");
        // Neither may be mistaken for a verdict.
        assert!(!compile.contains("issue(s)"), "{compile}");
        assert!(!run.contains("issue(s)"), "{run}");
        assert!(run.contains("neither clean nor a finding"), "{run}");
    }

    #[test]
    fn the_run_timeout_message_claims_only_what_the_kill_reaches() {
        // The kill is kill(-pgid). A setsid() child is outside the group,
        // so "killed, along with everything it spawned" would not be true.
        let run = run_timeout_message("", 300);
        assert!(!run.contains("everything it spawned"), "{run}");
        assert!(run.contains("process group"), "{run}");
        assert!(run.contains("setsid"), "{run}");
    }

    #[test]
    fn compile_gets_more_headroom_than_the_run() {
        assert!(compile_timeout_from(None) > timeout_from(None));
        // …but one explicit override governs both.
        assert_eq!(compile_timeout_from(Some(30)), timeout_from(Some(30)));
    }

    #[test]
    fn a_single_file_build_defaults_to_three_minutes() {
        // A reproducer that needs longer is compile-bound; 600s + 120s retries
        // were spent waiting on exactly that.
        assert_eq!(single_tu_compile_timeout(None), Duration::from_secs(180));
        assert_eq!(single_tu_compile_timeout(Some(30)), Duration::from_secs(30));
        assert_eq!(single_tu_compile_timeout(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn a_build_timeout_points_at_the_built_library_not_a_longer_wait() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("build/lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("build/CMakeFiles/x.dir")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("build/lib/libcore.a"), "!<arch>\n").unwrap();
        std::fs::write(dir.path().join("build/CMakeFiles/x.dir/libtmp.a"), "").unwrap();
        std::fs::write(dir.path().join("src/core.cpp"), "").unwrap();
        let libraries = built_project_libraries(dir.path());
        assert_eq!(libraries, vec!["build/lib/libcore.a".to_string()]);

        let plan = BuildPlan {
            compiler: "g++".to_string(),
            std: "c++23".to_string(),
            std_is_default: true,
            include_dirs: vec![],
            defines: vec![],
            march: vec![],
            semantics: Vec::new(),
            sources: vec![
                PathBuf::from("/w/repro.cpp"),
                PathBuf::from("/w/src/core.cpp"),
            ],
            origin: "build: test".to_string(),
        };
        let with = compile_timeout_message(&plan, 180, &libraries);
        assert!(with.contains("BUILD TIMED OUT after 180s"), "{with}");
        assert!(with.contains("rarely helps"), "{with}");
        assert!(with.contains("build/lib/libcore.a"), "{with}");
        assert!(with.contains("extra_sources"), "{with}");
        let without = compile_timeout_message(&plan, 180, &[]);
        assert!(
            without.contains("build the project's library once"),
            "{without}"
        );

        assert!(has_unresolved_symbols(
            "a.cpp:(.text+0x5): undefined reference to `f()'"
        ));
        assert!(!has_unresolved_symbols("error: expected ';'"));
        assert!(link_hint(&libraries).contains("build/lib/libcore.a"));
        assert_eq!(link_hint(&[]), "");
    }

    #[test]
    fn a_prebuilt_library_is_linked_and_does_not_change_the_language() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "int main(void){return 0;}\n").unwrap();
        std::fs::write(dir.path().join("libutil.a"), "!<arch>\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .build_plan(
                &serde_json::from_str(r#"{"source":"main.c","extra_sources":["libutil.a"]}"#)
                    .unwrap(),
            )
            .expect("plan");
        assert_eq!(
            (plan.compiler.as_str(), plan.std.as_str()),
            ("gcc", "gnu17")
        );
        assert!(is_link_input(Path::new("build/libutil.a")));
        assert!(is_link_input(Path::new("libutil.so.1.2")));
        assert!(is_link_input(Path::new("obj/x.o")));
        assert!(!is_link_input(Path::new("x.cpp")));
        assert!(!is_link_input(Path::new("x.c")));
    }

    #[test]
    fn c_defaults_to_the_gnu_dialect_with_an_older_fallback() {
        // Strict c17 hid POSIX declarations: a correct test program calling
        // popen() got an implicit int, a truncated pointer and a false crash.
        assert_eq!(DEFAULT_STD_C, "gnu17");
        assert_eq!(std_fallbacks(DEFAULT_STD_C), &["gnu11"]);
        assert!(std_rejected(
            "gcc: error: unrecognized command-line option '-std=gnu17'",
            "gnu17"
        ));
    }

    #[test]
    fn implicit_declarations_are_reported_as_a_test_program_bug() {
        let gcc_ascii = "t.c:5:9: warning: implicit declaration of function 'popen'; did you mean 'fopen'? [-Wimplicit-function-declaration]";
        let gcc_utf8 = "t.c:6:3: warning: implicit declaration of function \u{2018}pclose\u{2019} [-Wimplicit-function-declaration]";
        let clang = "t.c:5:9: error: call to undeclared function 'popen'; ISO C99 and later do not support implicit function declarations";
        let stderr = format!("{gcc_ascii}\n{gcc_utf8}\n{clang}\n");
        assert_eq!(implicitly_declared(&stderr), vec!["popen", "pclose"]);
        assert_eq!(
            implicitly_declared("t.c:1:1: warning: unused variable 'x'"),
            Vec::<String>::new()
        );
        let note = implicit_declaration_note(&implicitly_declared(&stderr));
        assert!(
            note.contains("popen, pclose were called without a declaration"),
            "{note}"
        );
        assert!(
            note.contains("not a finding in the code under test"),
            "{note}"
        );
        assert_eq!(implicit_declaration_note(&[]), "");
    }

    // ======================================================================
    // Compile database and structured args
    // ======================================================================

    #[test]
    fn extracts_only_allowlisted_flags_from_a_compile_line() {
        let tokens = shell_split(
            "/usr/bin/c++ -DFOO=1 -D BAR -I/abs/inc -Irel/inc -isystem /sys/inc \
             -iquote quoted -std=c++23 -O3 -march=native -Wall -g -c src/a.cpp -o a.o",
        );
        let flags = extract_db_flags(&tokens, Path::new("/proj"));
        assert_eq!(flags.std.as_deref(), Some("c++23"));
        assert_eq!(flags.defines, vec!["FOO=1", "BAR"]);
        assert_eq!(
            flags.include_dirs,
            vec![
                "/abs/inc".to_string(),
                "/proj/rel/inc".to_string(),
                "/sys/inc".to_string(),
                "/proj/quoted".to_string(),
            ],
            "relative include paths resolve against the entry's directory"
        );
    }

    #[test]
    fn compile_db_cannot_weaken_the_sanitizers() {
        // The verdict line is the product. A project whose real build carries
        // -fno-sanitize=all / -fsanitize=thread must not be able to turn
        // "clean: exit 0, no sanitizer errors" into a lie, so nothing outside
        // the -I/-D/-std allowlist survives extraction.
        let tokens = shell_split(
            "clang++ -fno-sanitize=all -fsanitize=thread -fno-sanitize-recover=undefined \
             -DKEEP=1 -o out src/a.cpp",
        );
        let flags = extract_db_flags(&tokens, Path::new("/proj"));
        assert_eq!(flags.defines, vec!["KEEP=1"]);
        assert!(flags.include_dirs.is_empty());
        assert_eq!(flags.std, None);

        // …and the plan built from them puts nothing else on the compile line.
        let plan = BuildPlan {
            compiler: "g++".to_string(),
            std: "c++23".to_string(),
            std_is_default: false,
            include_dirs: flags.include_dirs,
            defines: flags.defines,
            // Feed the extracted march through too, so this invariant test
            // also proves a sanitizer-weakening flag cannot ride in via `march`.
            march: flags.march,
            semantics: flags.semantics,
            sources: vec![PathBuf::from("/proj/src/a.cpp")],
            origin: String::new(),
        };
        let argv = plan.compile_argv();
        assert_eq!(argv, vec!["-std=c++23", "-DKEEP=1"]);
        assert!(!argv.iter().any(|a| a.contains("sanitize")));
    }

    #[test]
    fn no_free_form_flags_argument_exists() {
        // Pinned: adding a `flags` field later must break this test.
        // An unknown field is refused outright, so the flags cannot
        // even be carried silently.
        let json = r#"{"source":"a.cpp","flags":["-fno-sanitize=all"]}"#;
        let err = serde_json::from_str::<SanitizeArgs>(json)
            .err()
            .expect("an unknown field is refused");
        assert!(err.to_string().contains("unknown field `flags`"), "{err}");
    }

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(
            shell_split(r#"cc -I"/a b/inc" -D'X=1 2' -Ic\ d"#),
            vec!["cc", "-I/a b/inc", "-DX=1 2", "-Ic d"]
        );
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn db_flags_match_the_entry_for_this_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.cpp");
        std::fs::write(&src, "int main(){}\n").unwrap();
        let other = dir.path().join("other.cpp");
        std::fs::write(&other, "int f(){return 0;}\n").unwrap();
        let db = dir.path().join("compile_commands.json");
        std::fs::write(
            &db,
            serde_json::to_string(&serde_json::json!([
                {
                    "directory": dir.path().to_string_lossy(),
                    "file": "other.cpp",
                    "command": "c++ -DWRONG=1 -Iwrong -std=c++11 -c other.cpp",
                },
                {
                    "directory": dir.path().to_string_lossy(),
                    "file": "main.cpp",
                    "arguments": ["c++", "-DRIGHT=1", "-Iinclude", "-std=c++23", "-c", "main.cpp"],
                },
            ]))
            .unwrap(),
        )
        .unwrap();

        let flags = db_flags_for(&db, &src).expect("entry for main.cpp");
        assert_eq!(flags.defines, vec!["RIGHT=1"]);
        assert_eq!(flags.std.as_deref(), Some("c++23"));
        assert_eq!(
            flags.include_dirs,
            vec![canonical(dir.path()).join("include").display().to_string()]
        );
    }

    #[test]
    fn db_without_an_entry_for_this_file_falls_back() {
        // A known failure mode: a header-only TU, or a file outside the
        // build, must degrade to the structured args rather than borrow
        // someone else's flags.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("scratch.cpp");
        std::fs::write(&src, "int main(){}\n").unwrap();
        let db = dir.path().join("compile_commands.json");
        std::fs::write(
            &db,
            format!(
                r#"[{{"directory":"{}","file":"other.cpp","command":"c++ -DX=1 -c other.cpp"}}]"#,
                dir.path().display()
            ),
        )
        .unwrap();
        assert_eq!(db_flags_for(&db, &src), None);
    }

    #[test]
    fn a_misnamed_argument_is_refused_not_silently_dropped() {
        // Measured before the fix: `"args": "x y"` (for `run_args`) was dropped, the
        // program ran with no arguments, and the verdict said "clean".
        let err = serde_json::from_str::<SanitizeArgs>(r#"{"source":"argv.c","args":"x y"}"#)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("unknown field `args`"), "{err}");
        // The mode selector itself is still a field of both argument sets.
        assert!(
            serde_json::from_str::<SanitizeArgs>(r#"{"source":"a.c","project":false}"#).is_ok()
        );
        assert!(
            serde_json::from_str::<ProjectArgs>(r#"{"project":true,"timeout_sec":90.0}"#).is_ok()
        );
        let err = serde_json::from_str::<ProjectArgs>(r#"{"project":true,"source":"a.c"}"#)
            .err()
            .expect("project mode has no `source`");
        assert!(err.to_string().contains("unknown field `source`"), "{err}");
    }

    #[test]
    fn meaning_changing_db_flags_are_carried() {
        // Dropping -fwrapv / -funsigned-char / -include / -U
        // builds a different program (a false "signed integer overflow").
        let tokens: Vec<String> = [
            "cc",
            "-fwrapv",
            "-funsigned-char",
            "-include",
            "forced.h",
            "-UNDEBUG",
            "-U",
            "X",
            "-O2",
            "-fno-sanitize=address",
            "-c",
            "a.c",
        ]
        .iter()
        .map(|t| t.to_string())
        .collect();
        let flags = extract_db_flags(&tokens, Path::new("/proj"));
        assert_eq!(
            flags.semantics,
            vec![
                "-fwrapv",
                "-funsigned-char",
                "-include",
                "/proj/forced.h",
                "-UNDEBUG",
                "-UX"
            ]
        );
        assert!(!flags.semantics.iter().any(|f| f.contains("sanitize")));
    }

    #[test]
    fn a_db_entry_is_found_past_a_malformed_one_and_with_a_relative_directory() {
        // An earlier entry without "file" must not end the search, and a
        // relative "directory" must not be resolved against the process cwd.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let src = dir.path().join("src/main.c");
        std::fs::write(&src, "int main(void){return 0;}\n").unwrap();
        let db = dir.path().join("compile_commands.json");
        std::fs::write(
            &db,
            r#"[{"directory":".","command":"cc -c nothing.c"},
                {"directory":"src","file":"main.c","command":"cc -DFOUND=1 -c main.c"}]"#,
        )
        .unwrap();
        let flags = db_flags_for(&db, &src).expect("the entry exists");
        assert_eq!(flags.defines, vec!["FOUND=1"]);
        // An unreadable DB is named as unreadable, not as "no entry".
        std::fs::write(&db, "{not json").unwrap();
        let origin = describe_origin(Some(&db), false, "src/main.c", "c17", 0, 0, 1);
        assert!(origin.contains("could not be read"), "{origin}");
    }

    #[test]
    fn a_c_program_is_built_as_c() {
        // .c files must not go through g++ -std=c++23.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "int main(void){return 0;}\n").unwrap();
        std::fs::write(dir.path().join("b.cpp"), "int main(){return 0;}\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .build_plan(&serde_json::from_str(r#"{"source":"a.c"}"#).unwrap())
            .expect("plan");
        assert_eq!(
            (plan.compiler.as_str(), plan.std.as_str()),
            ("gcc", "gnu17")
        );
        let plan = tool
            .build_plan(&serde_json::from_str(r#"{"source":"b.cpp"}"#).unwrap())
            .expect("plan");
        assert_eq!(
            (plan.compiler.as_str(), plan.std.as_str()),
            ("g++", "c++23")
        );
        // An explicit choice still wins.
        let plan = tool
            .build_plan(&serde_json::from_str(r#"{"source":"a.c","compiler":"clang"}"#).unwrap())
            .expect("plan");
        assert_eq!(plan.compiler, "clang");
    }

    #[tokio::test]
    async fn valid_c_and_a_wrapv_project_are_not_false_findings() {
        // Live: `char *p = malloc(4);` is valid C (it failed as
        // C++), and signed overflow under the project's -fwrapv is defined
        // (UBSan reported it when the flag was dropped).
        use crate::program_args::fixture::have;
        if !have("gcc") {
            eprintln!("skip: gcc not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("wrap.c");
        std::fs::write(
            &src,
            "#include <stdlib.h>\n#include <limits.h>\n#include <stdio.h>\n\
             int main(int argc, char **argv){ char *p = malloc(4); free(p); (void)argv;\n\
             int x = INT_MAX - 1 + argc; x = x + 1; printf(\"%d\\n\", x); return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("compile_commands.json"),
            format!(
                r#"[{{"directory":"{}","file":"wrap.c","command":"cc -fwrapv -c wrap.c"}}]"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"wrap.c"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-test", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(60))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        assert!(out.contains("clean"), "{out}");
        assert!(!out.contains("signed integer overflow"), "{out}");
        assert!(!out.contains("invalid conversion"), "{out}");
    }

    #[test]
    fn malformed_compile_db_is_ignored_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.cpp");
        std::fs::write(&src, "int main(){}\n").unwrap();
        let db = dir.path().join("compile_commands.json");
        std::fs::write(&db, "{ not an array").unwrap();
        assert_eq!(db_flags_for(&db, &src), None);
        std::fs::write(&db, r#"{"file":"a.cpp"}"#).unwrap();
        assert_eq!(db_flags_for(&db, &src), None);
    }

    #[test]
    fn build_plan_reads_the_compile_db_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        let src = dir.path().join("main.cpp");
        std::fs::write(&src, "int main(){}\n").unwrap();
        std::fs::write(
            dir.path().join("compile_commands.json"),
            serde_json::to_string(&serde_json::json!([{
                "directory": dir.path().to_string_lossy(),
                "file": "main.cpp",
                "command": "c++ -Iinclude -DFROM_DB=1 -std=c++20 -O2 -c main.cpp",
            }]))
            .unwrap(),
        )
        .unwrap();

        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"main.cpp"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");

        assert_eq!(
            plan.std, "c++20",
            "the DB's std wins over the c++23 default"
        );
        assert_eq!(plan.defines, vec!["FROM_DB=1"]);
        assert!(plan.include_dirs[0].ends_with("include"));
        assert!(
            plan.origin.contains("compile_commands.json"),
            "{}",
            plan.origin
        );
        assert!(
            plan.origin.contains("entry for main.cpp"),
            "{}",
            plan.origin
        );
        // -O2 is not ours to carry.
        assert!(!plan.compile_argv().iter().any(|a| a == "-O2"));
    }

    #[test]
    fn explicit_std_beats_the_compile_db() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.cpp");
        std::fs::write(&src, "int main(){}\n").unwrap();
        std::fs::write(
            dir.path().join("compile_commands.json"),
            serde_json::to_string(&serde_json::json!([{
                "directory": dir.path().to_string_lossy(),
                "file": "main.cpp",
                "command": "c++ -std=c++11 -c main.cpp",
            }]))
            .unwrap(),
        )
        .unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"main.cpp","std":"c++23"}"#).unwrap();
        assert_eq!(tool.build_plan(&args).unwrap().std, "c++23");
    }

    #[test]
    fn structured_args_build_the_reporters_project_layout() {
        // A common layout: include/pool.hpp + main.cpp at the root,
        // no build system. Without -Iinclude the compile dies with
        // "fatal error: … No such file or directory".
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("include/pool.hpp"), "#pragma once\n").unwrap();
        std::fs::write(dir.path().join("main.cpp"), "int main(){}\n").unwrap();
        std::fs::write(dir.path().join("book.cpp"), "int f(){return 0;}\n").unwrap();

        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(
            r#"{"source":"main.cpp","include_dirs":["include"],"defines":["NDEBUG"],
                "extra_sources":["book.cpp"]}"#,
        )
        .unwrap();
        let plan = tool.build_plan(&args).expect("plan");

        assert_eq!(plan.std, "c++23", "default is c++23, not c++20");
        assert_eq!(plan.sources.len(), 2);
        let argv = plan.compile_argv();
        assert_eq!(argv[0], "-std=c++23");
        assert!(
            argv.iter()
                .any(|a| a.ends_with("include") && a.starts_with("-I")),
            "{argv:?}"
        );
        assert!(argv.contains(&"-DNDEBUG".to_string()), "{argv:?}");
        assert!(
            plan.origin.contains("no compile_commands.json"),
            "{}",
            plan.origin
        );
    }

    #[test]
    fn compiler_argument_is_allowlisted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.cpp"), "int main(){}\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };

        for good in COMPILER_ALLOWLIST {
            let args: SanitizeArgs =
                serde_json::from_str(&format!(r#"{{"source":"a.cpp","compiler":"{good}"}}"#))
                    .unwrap();
            assert_eq!(&tool.build_plan(&args).unwrap().compiler, good);
        }

        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"a.cpp","compiler":"/bin/sh"}"#).unwrap();
        match tool.build_plan(&args) {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("not allowed"), "{msg}");
                assert!(msg.contains("clang++"), "{msg}");
            }
            other => panic!("expected a model-facing refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_header_or_a_directory_is_refused_as_source_before_anything_runs() {
        // Given `hdr.hpp`, g++ writes a PRECOMPILED HEADER to the
        // `-o` path, and executing that failed with EACCES — which run.rs used
        // to report as Fatal, ending the user's whole turn. A directory (or an
        // empty `source`, which resolves to the workspace itself) reached the
        // linker as "file format not recognized".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hdr.hpp"), "#pragma once\n").unwrap();
        std::fs::write(dir.path().join("hdr.h"), "int g(void);\n").unwrap();
        std::fs::write(dir.path().join("single.C"), "int main(){}\n").unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        for (source, expect) in [
            ("hdr.hpp", "is a header"),
            ("hdr.h", "is a header"),
            ("adir", "is a directory"),
            ("", "is a directory"),
        ] {
            let args: SanitizeArgs =
                serde_json::from_str(&format!(r#"{{"source":"{source}"}}"#)).unwrap();
            match tool.build_plan(&args) {
                Err(FunctionCallError::RespondToModel(msg)) => {
                    assert!(msg.contains(expect), "{source:?}: {msg}");
                    assert!(msg.contains("main()"), "the way forward is named: {msg}");
                }
                other => panic!("{source:?}: expected a refusal, got {other:?}"),
            }
        }
        // `.C` is a C++ SOURCE (only `.H` is a header), so it must still build.
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"single.C"}"#).unwrap();
        assert!(tool.build_plan(&args).is_ok());
    }

    #[test]
    fn missing_extra_source_is_reported_before_compiling() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.cpp"), "int main(){}\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"a.cpp","extra_sources":["nope.cpp"]}"#).unwrap();
        match tool.build_plan(&args) {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("extra source does not exist"), "{msg}")
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn missing_header_compile_failure_is_legible() {
        // The exact diagnostic that layout produces. It must not render as
        // "compile failed / No findings." (a parse that drops `fatal error`
        // with the raw stderr never returned).
        let stderr = "In file included from main.cpp:1:\n\
                      include/pool.hpp:3:10: fatal error: orderbook.hpp: No such file or directory\n\
                      compilation terminated.\n";
        let plan = BuildPlan {
            compiler: "g++".to_string(),
            std: "c++23".to_string(),
            std_is_default: false,
            include_dirs: vec![],
            defines: vec![],
            march: vec![],
            semantics: Vec::new(),
            sources: vec![PathBuf::from("main.cpp")],
            origin: "build: no compile_commands.json found".to_string(),
        };
        let out = compile_failure(&plan, stderr);
        assert!(out.contains("compile failed"), "{out}");
        assert!(
            out.contains(
                "gcc error include/pool.hpp:3:10 orderbook.hpp: No such file or directory"
            ),
            "the fatal-error diagnostic must be parsed: {out}"
        );
        assert!(out.contains("compiler stderr:"), "{out}");
        assert!(out.contains("compilation terminated."), "{out}");
        assert!(!out.contains("No findings."), "{out}");
    }

    #[test]
    fn parses_fatal_error_severity() {
        let f = parse_gcc_line("include/pool.hpp:3:10: fatal error: orderbook.hpp: No such file")
            .expect("fatal error must parse");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.file, "include/pool.hpp");
        assert_eq!(f.line, 3);
    }

    // ======================================================================
    // The trust gate
    // ======================================================================

    #[test]
    fn untrusted_project_is_refused_and_told_how_to_proceed() {
        let tool = SanitizeTool {
            cwd: PathBuf::from("/tmp/hft"),
            native_exec: false,
        };
        match tool.gate() {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("cpp_sanitize"), "{msg}");
                assert!(msg.contains("BYTEASK_ALLOW_NATIVE_TOOLS=1"), "{msg}");
                assert!(msg.contains("trust_level = \"trusted\""), "{msg}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn trusted_or_opted_in_project_passes_the_gate() {
        let tool = SanitizeTool {
            cwd: PathBuf::from("/tmp/hft"),
            native_exec: true,
        };
        assert!(tool.gate().is_ok());
    }

    #[test]
    fn default_std_is_cpp23() {
        assert_eq!(DEFAULT_STD, "c++23");
    }

    // ======================================================================
    // live tests (#[ignore]; run with --include-ignored)
    //
    // Unit tests alone are not enough here: several real defects passed
    // every unit test because each lived *between* units. These
    // two run the real compiler and the real sanitizer end to end, which is the
    // only place those seams are visible.
    // ======================================================================

    #[tokio::test]
    #[ignore = "needs g++ with ASan; run with --include-ignored"]
    async fn live_run_args_reach_the_program_byte_for_byte() {
        // The reported case: `""` is an empty argument, not two quote chars.
        use crate::program_args::fixture;
        let dir = tempfile::tempdir().unwrap();
        assert!(fixture::build(dir.path()).is_some(), "cc");
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"argv_recorder.c"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        // The recorder takes its output path from -DOUT; the plan compiles it
        // without one, so write a variant with the path baked in.
        let source = dir.path().join("argv_recorder.c");
        let text = std::fs::read_to_string(&source).unwrap().replace(
            "fopen(OUT,",
            &format!("fopen(\"{}\",", dir.path().join("argv.out").display()),
        );
        std::fs::write(&source, text).unwrap();
        let run_args =
            crate::program_args::parse("run_args", r#""" 'a b' x\"y '$HOME' 日本"#).expect("parse");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-live", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &run_args, Some(120))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        assert!(out.contains("clean"), "{out}");
        let runs = fixture::runs(dir.path());
        assert_eq!(runs.len(), 1, "{out}");
        assert_eq!(runs[0].args, vec!["", "a b", "x\"y", "$HOME", "日本"]);
    }

    #[tokio::test]
    #[ignore = "needs g++ with ASan; run with --include-ignored"]
    async fn live_reporters_project_layout_builds_and_proves_the_bug() {
        // A reproduction of that layout: include/pool.hpp written past
        // the end of its array, main.cpp at the root. Two things must
        // hold: the build must succeed (it needs -Iinclude), and the
        // result must carry the sanitizer's stacks, not a one-line summary.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(
            dir.path().join("include/pool.hpp"),
            "#pragma once\n#include <cstdint>\n#include <cstddef>\n\
             template <std::size_t NumBlocks> struct ArenaPool {\n\
             \x20   std::uint16_t free_list[NumBlocks];\n\
             \x20   ArenaPool() { for (std::size_t i = 0; i <= NumBlocks; ++i)\n\
             \x20                     free_list[i] = static_cast<std::uint16_t>(i); }\n};\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("main.cpp"),
            "#include \"pool.hpp\"\nint main(){ ArenaPool<16> p; return p.free_list[0]; }\n",
        )
        .unwrap();

        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"main.cpp","include_dirs":["include"]}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-live", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(120))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);

        eprintln!("---- live cpp_sanitize output ----\n{out}\n---- end ----");
        assert!(
            !out.contains("compile failed"),
            "-Iinclude must make the include/ + main.cpp layout build: {out}"
        );
        assert!(
            out.contains("issue(s)"),
            "the seeded overflow must be found: {out}"
        );
        assert!(
            out.contains("sanitizer report (raw stderr)"),
            "the raw report must come back: {out}"
        );
        assert!(
            out.contains("stack-buffer-overflow") || out.contains("runtime error"),
            "{out}"
        );
        assert!(
            !out.contains("Shadow byte legend"),
            "no shadow-byte legend in the output: {out}"
        );
    }

    #[tokio::test]
    #[ignore = "needs gcc with ASan; run with --include-ignored"]
    async fn live_a_c_program_using_posix_calls_builds_and_runs_clean_by_default() {
        // The default C dialect used to be strict c17: popen() was undeclared,
        // returned a truncated int, and a correct program "crashed".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("posix.c"),
            "#include <stdio.h>\n\
             int main(void) {\n\
               FILE *p = popen(\"echo ok\", \"r\");\n\
               char buf[16] = {0};\n\
               if (!p || !fgets(buf, sizeof buf, p)) return 1;\n\
               return pclose(p) == 0 ? 0 : 2;\n\
             }\n",
        )
        .unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"posix.c"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-live", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(120))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        assert!(out.contains("clean"), "{out}");
        assert!(!out.contains("without a declaration"), "{out}");

        // Asked for strict C17, the same program calls popen undeclared: the
        // report must say that is the test program's bug.
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"posix.c","std":"c17"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-live", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(120))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        assert!(out.contains("popen"), "{out}");
        assert!(
            out.contains("not a finding in the code under test"),
            "{out}"
        );
    }

    #[tokio::test]
    #[ignore = "needs g++ with ASan; run with --include-ignored"]
    async fn live_missing_include_failure_names_the_missing_header() {
        // The same project WITHOUT include_dirs — the exact failure the report
        // hit. It must say which header is missing, not "No findings."
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("include/pool.hpp"), "#pragma once\n").unwrap();
        std::fs::write(
            dir.path().join("main.cpp"),
            "#include \"pool.hpp\"\nint main(){ return 0; }\n",
        )
        .unwrap();

        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"main.cpp"}"#).unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-live", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(120))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);

        eprintln!("---- live compile failure ----\n{out}\n---- end ----");
        assert!(out.contains("compile failed"), "{out}");
        assert!(out.contains("pool.hpp"), "must name the header: {out}");
        assert!(out.contains("compiler stderr:"), "{out}");
        assert!(!out.contains("No findings."), "{out}");
    }

    // ---- What counts as a report, and where it is -------------------------
    //
    // Every fixture below is real runtime output from gcc 12 / clang 14 on
    // this machine, with only the paths shortened.

    /// g++ LeakSanitizer output: two direct and two indirect leaks. One
    /// indirect leak is allocated inside libstdc++ with no user frame at all.
    const REAL_LSAN_REPORT: &str = "\
=================================================================
==3410062==ERROR: LeakSanitizer: detected memory leaks

Direct leak of 32 byte(s) in 1 object(s) allocated from:
    #0 0x7fab9cab94c8 in operator new(unsigned long) ../../../../src/libsanitizer/asan/asan_new_delete.cpp:95
    #1 0x561799d073bb in main /w/leakmix.cpp:4
    #2 0x7fab9c845249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58

Direct leak of 16 byte(s) in 1 object(s) allocated from:
    #0 0x7fab9cab94c8 in operator new(unsigned long) ../../../../src/libsanitizer/asan/asan_new_delete.cpp:95
    #1 0x561799d072da in main /w/leakmix.cpp:5
    #2 0x7fab9c845249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58

Indirect leak of 101 byte(s) in 1 object(s) allocated from:
    #0 0x7fab9cab94c8 in operator new(unsigned long) ../../../../src/libsanitizer/asan/asan_new_delete.cpp:95
    #1 0x7fab9c73f95b in std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> >::_M_construct(unsigned long, char) (/lib/x86_64-linux-gnu/libstdc++.so.6+0x13f95b)

Indirect leak of 10 byte(s) in 1 object(s) allocated from:
    #0 0x7fab9cab89cf in __interceptor_malloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:69
    #1 0x561799d07334 in main /w/leakmix.cpp:4
    #2 0x7fab9c845249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58

SUMMARY: AddressSanitizer: 159 byte(s) leaked in 4 allocation(s).
";

    #[test]
    fn every_leak_the_leak_sanitizer_reports_is_a_finding() {
        // If only `ERROR: AddressSanitizer:` is matched, this report
        // renders as "0 issue(s) (exit 1)" / "No findings." with four leaks
        // printed right below it.
        let reports = parse_asan_ubsan(REAL_LSAN_REPORT);
        let got: Vec<(&str, &str, u32, &str)> = reports
            .findings
            .iter()
            .map(|f| {
                (
                    f.source.as_str(),
                    f.file.as_str(),
                    f.line,
                    f.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "lsan",
                    "/w/leakmix.cpp",
                    4,
                    "Direct leak of 32 byte(s) in 1 object(s)"
                ),
                (
                    "lsan",
                    "/w/leakmix.cpp",
                    5,
                    "Direct leak of 16 byte(s) in 1 object(s)"
                ),
                // No user or library frame carries a location: better no
                // location than one inside the ASan runtime.
                ("lsan", "", 0, "Indirect leak of 101 byte(s) in 1 object(s)"),
                (
                    "lsan",
                    "/w/leakmix.cpp",
                    4,
                    "Indirect leak of 10 byte(s) in 1 object(s)"
                ),
            ]
        );
        assert_eq!(
            asan_ubsan_verdict(&reports, "1", false),
            "4 issue(s) (exit 1)"
        );
        // clang's runtime frames are unsymbolized and carry a BuildId.
        let clang = "==3410082==ERROR: LeakSanitizer: detected memory leaks\n\n\
            Direct leak of 16 byte(s) in 1 object(s) allocated from:\n    \
            #0 0x55652bdc3f5d in operator new(unsigned long) (/w/l_clang+0xdef5d) (BuildId: 0239e0edb9047a78edc1aef524e58ea77c96ab85)\n    \
            #1 0x55652bdc6621 in main /w/leakmix.cpp:4:23\n";
        let f = &parse_asan_ubsan(clang).findings[0];
        assert_eq!((f.file.as_str(), f.line), ("/w/leakmix.cpp", 4));
    }

    #[test]
    fn a_report_is_located_at_the_first_user_frame_not_in_the_runtime() {
        // Frame #0 of these is the sanitizer's own interceptor (or a
        // standard-library header), which the user cannot change.
        let double_free = "\
==3346834==ERROR: AddressSanitizer: attempting double-free on 0x602000000010 in thread T0:
    #0 0x7f5e178b76a8 in __interceptor_free ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:52
    #1 0x55f0e0e5a1f3 in main /w/dfree.c:2
    #2 0x7f5e17645249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58
";
        let memcpy = "\
==3279403==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602000000018 at pc 0x7f6d7da48061 bp 0x7fffca419b90 sp 0x7fffca419340
WRITE of size 9 at 0x602000000018 thread T0
    #0 0x7f6d7da48060 in __interceptor_memcpy ../../../../src/libsanitizer/sanitizer_common/sanitizer_common_interceptors.inc:827
    #1 0x55e384ffc228 in main /w/memcpy_ovf.c:4
";
        let stl = "\
==1==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602000000020 at pc 0x1 bp 0x2 sp 0x3
READ of size 4 at 0x602000000020 thread T0
    #0 0x55e9d929544f in std::vector<int, std::allocator<int> >::operator[](unsigned long) /usr/include/c++/12/bits/stl_vector.h:1123
    #1 0x55e9d9295450 in main /w/vec.cpp:3
";
        for (report, file, line, class) in [
            (double_free, "/w/dfree.c", 2, "attempting double-free"),
            (memcpy, "/w/memcpy_ovf.c", 4, "heap-buffer-overflow"),
            (stl, "/w/vec.cpp", 3, "heap-buffer-overflow"),
        ] {
            let findings = parse_asan_ubsan(report).findings;
            assert_eq!(findings.len(), 1, "{report}");
            assert_eq!(
                (
                    findings[0].file.as_str(),
                    findings[0].line,
                    findings[0].message.as_str()
                ),
                (file, line, class)
            );
        }
        // TSan: gcc's interceptor frames are symbolized, clang's are `<null>`.
        let gcc = "==================\nWARNING: ThreadSanitizer: data race (pid=3413479)\n  \
            Read of size 8 at 0x5614cc96b080 by main thread:\n    \
            #0 memcpy ../../../../src/libsanitizer/sanitizer_common/sanitizer_common_interceptors.inc:827 (libtsan.so.2+0x5dda6)\n    \
            #1 memcpy ../../../../src/libsanitizer/sanitizer_common/sanitizer_common_interceptors.inc:819 (libtsan.so.2+0x5dda6)\n    \
            #2 main /w/memcpyrace.c:6 (mr_gcc+0x124f)\n";
        let clang = "==================\nWARNING: ThreadSanitizer: data race (pid=3413487)\n  \
            Read of size 8 at 0x56452f412260 by main thread:\n    \
            #0 memcpy <null> (mr_clang+0x5b61e) (BuildId: 70e1bf89ca4b499e37e9270ace4ab18ff509149c)\n    \
            #1 main /w/memcpyrace.c:6:68 (mr_clang+0xd0173) (BuildId: 70e1bf89ca4b499e37e9270ace4ab18ff509149c)\n";
        for report in [gcc, clang] {
            let f = parse_tsan_report(report).expect("a race");
            assert_eq!(
                (f.file.as_str(), f.line),
                ("/w/memcpyrace.c", 6),
                "{report}"
            );
        }
        // valgrind: a leak's first frame is valgrind's own malloc replacement.
        let xml = "<error><kind>Leak_DefinitelyLost</kind><what>237 bytes lost</what><stack>\
            <frame><obj>/usr/libexec/valgrind/vgpreload_memcheck-amd64-linux.so</obj><fn>malloc</fn>\
            <dir>/build/valgrind/coregrind/m_replacemalloc</dir><file>vg_replace_malloc.c</file><line>381</line></frame>\
            <frame><obj>/w/leak2</obj><fn>mk</fn><dir>/w</dir><file>leak2.c</file><line>3</line></frame>\
            </stack></error>";
        let f = &parse_valgrind_xml(xml)[0];
        assert_eq!((f.file.as_str(), f.line), ("leak2.c", 3));
    }

    #[test]
    fn a_path_with_spaces_reaches_the_finding_whole() {
        // Taken as the last whitespace-separated token, the location
        // `/w/cm spaces/src/util.cpp:2` would become `spaces/src/util.cpp:2`.
        let asan = "\
==3254989==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602000000018 at pc 0x564c8f50e3bc bp 0x7ffc490a3490 sp 0x7ffc490a3488
WRITE of size 1 at 0x602000000018 thread T0
    #0 0x564c8f50e3bb in overflow(int) /w/cm spaces/src/util.cpp:2
    #1 0x564c8f50e26e in main /w/cm spaces/src/app.cpp:3
";
        let f = &parse_asan_ubsan(asan).findings[0];
        assert_eq!((f.file.as_str(), f.line), ("/w/cm spaces/src/util.cpp", 2));
        let tsan = "==================\nWARNING: ThreadSanitizer: data race (pid=3282895)\n  \
            Read of size 4 at 0x5630e1c4206c by thread T2:\n    \
            #0 w(void*) /w/sp dir/race.c:4 (byteask-cpp-sanitize-3282885+0x11e6)\n";
        let f = parse_tsan_report(tsan).expect("a race");
        assert_eq!((f.file.as_str(), f.line), ("/w/sp dir/race.c", 4));
    }

    #[test]
    fn program_output_quoting_a_report_is_not_a_finding() {
        // A program that logs two lines that QUOTE
        // sanitizer output and exits 0. Unanchored matching reports "2
        // issue(s) (exit 0)" for a clean program.
        let stderr = "test log: expected message was '==1==ERROR: AddressSanitizer: \
                      heap-use-after-free on address 0x1'\n\
                      parser.c:10:5: runtime error: this is a log line quoting a UBSan message\n";
        let reports = parse_asan_ubsan(stderr);
        assert!(reports.findings.is_empty(), "{:?}", reports.findings);
        assert_eq!(reports.lookalikes, 2);
        let verdict = asan_ubsan_verdict(&reports, "0", false);
        // Not a finding — but not silently dropped either.
        assert!(verdict.starts_with("clean"), "{verdict}");
        assert!(verdict.contains("2 line(s)"), "{verdict}");
        assert!(verdict.contains("not counted"), "{verdict}");
        // A quoted TSan warning is not a race either.
        assert_eq!(
            classify_tsan("log: saw 'WARNING: ThreadSanitizer: data race (pid=1)' earlier\n"),
            TsanOutcome::Silent
        );
        // …and a look-alike line cannot borrow the stack of a REAL report
        // that follows it.
        let mixed = "parser.c:10:5: runtime error: quoted in a log line\n\
            =================================================================\n\
            ==9==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000010 at pc 0x1 bp 0x2 sp 0x3\n\
            WRITE of size 4 at 0x602000000010 thread T0\n    \
            #0 0x56 in main /w/uaf.c:3\n";
        let reports = parse_asan_ubsan(mixed);
        assert_eq!(reports.findings.len(), 1, "{:?}", reports.findings);
        assert_eq!(reports.findings[0].message, "heap-use-after-free");
        assert_eq!(reports.lookalikes, 1);
    }

    #[test]
    fn a_ubsan_report_that_starts_mid_line_is_still_found() {
        // The other edge, measured with gcc and clang: after a partial line
        // of program output UBSan's report does NOT start its own line, so
        // anchoring on a line start alone would drop a real bug — and UBSan
        // recovers, so the run exits 0 and would read "clean".
        let stderr = "\
working on item 7... partial.c:3:94: runtime error: signed integer overflow: 2147483647 + 1 cannot be represented in type 'int'
    #0 0x557621514253 in main /w/partial.c:3
    #1 0x7fe47aa45249 in __libc_start_call_main ../sysdeps/nptl/libc_start_call_main.h:58

done
";
        let reports = parse_asan_ubsan(stderr);
        assert_eq!(reports.lookalikes, 0);
        let f = &reports.findings[0];
        assert_eq!(
            (f.source.as_str(), f.file.as_str(), f.line, f.column),
            ("ubsan", "/w/partial.c", 3, 94)
        );
        assert!(
            f.message.starts_with("signed integer overflow"),
            "{}",
            f.message
        );
        // A report with notes between the line and its stack (object-size).
        let with_notes = "\
/w/canon_ovf.c:2:50: runtime error: store to address 0x602000000018 with insufficient space for an object of type 'char'
0x602000000018: note: pointer points here
 be be be be  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  00 00 00 00
              ^
    #0 0x55e664c6c1da in main /w/canon_ovf.c:2
";
        let f = &parse_asan_ubsan(with_notes).findings[0];
        assert_eq!(
            (f.file.as_str(), f.line, f.column),
            ("/w/canon_ovf.c", 2, 50)
        );
    }

    #[test]
    fn the_bug_class_is_the_whole_name_not_its_first_word() {
        // If the class is the first whitespace token, a
        // double free renders as "asan error … attempting".
        for (header, class) in [
            (
                "heap-use-after-free on address 0x602000000010 at pc 0x1 bp 0x2 sp 0x3",
                "heap-use-after-free",
            ),
            (
                "attempting double-free on 0x602000000010 in thread T0:",
                "attempting double-free",
            ),
            (
                "attempting free on address which was not malloc()-ed: 0x7fffcb58c800 in thread T0",
                "attempting free on address which was not malloc()-ed",
            ),
            (
                "alloc-dealloc-mismatch (operator new [] vs operator delete) on 0x602000000010",
                "alloc-dealloc-mismatch (operator new [] vs operator delete)",
            ),
            (
                "SEGV on unknown address 0x000000000000 (pc 0x1 bp 0x2 sp 0x3 T0)",
                "SEGV",
            ),
            (
                "stack-overflow on address 0x7ffd70324c28 (pc 0x1 bp 0x2 sp 0x3 T0)",
                "stack-overflow",
            ),
        ] {
            assert_eq!(asan_bug_class(header), class);
        }
    }

    // ---- valgrind -----------------------------------------------------------

    #[test]
    fn a_valgrind_run_that_died_or_failed_is_never_clean() {
        // The verdict must not be "clean: no memcheck errors (exit signal)" after
        // an abort, a SIGKILL and a stack overflow, or "(exit 3)" for a
        // program that failed. The ASan path says "0 issue(s) (exit
        // N)"; memcheck says the same.
        for exit in ["signal", "3", "99"] {
            let verdict = valgrind_verdict(&[], exit, false, true);
            assert!(!verdict.starts_with("clean"), "{exit}: {verdict}");
            assert_eq!(verdict, format!("0 issue(s) (exit {exit})"));
        }
        assert_eq!(
            valgrind_verdict(&[], "0", false, true),
            "clean: no memcheck errors (exit 0)"
        );
        assert!(valgrind_verdict(&[], "0", true, true).contains("TRUNCATED"));
        let one = vec![finding(
            "valgrind",
            "a.c",
            3,
            "InvalidWrite: Invalid write of size 4",
        )];
        assert_eq!(
            valgrind_verdict(&one, "99", false, true),
            "1 issue(s) (exit 99)"
        );
        // No complete XML report: nothing was checked, whatever the exit code.
        let none = valgrind_verdict(&[], "0", false, false);
        assert!(
            none.contains("NO REPORT") && none.contains("NOT a clean result"),
            "{none}"
        );
        assert!(!xml_is_complete("") && xml_is_complete("<valgrindoutput></valgrindoutput>"));
    }

    #[tokio::test]
    async fn a_valgrind_run_that_times_out_leaves_no_report_file() {
        // The timeout path must not return before the XML report is removed,
        // or it leaves one byteask-cpp-valgrind-*.xml in /tmp per timed-out call.
        use crate::program_args::fixture::have;
        if !have("valgrind") {
            eprintln!("skip: valgrind not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let xml = dir.path().join("report.xml");
        let (run, _) = tool
            .run_memcheck(Path::new("/bin/sleep"), &["30".to_string()], Some(2), &xml)
            .await
            .expect("run");
        assert!(matches!(run, RunOutcome::TimedOut { .. }), "{run:?}");
        assert!(!xml.exists(), "the XML report was left behind");
    }

    #[tokio::test]
    async fn a_program_that_aborts_under_valgrind_is_not_clean() {
        // End to end: a program that aborts under valgrind.
        use crate::program_args::fixture::have;
        if !have("valgrind") || !have("g++") {
            eprintln!("skip: valgrind or g++ not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("abort.cpp"),
            "#include <cassert>\nint main(){ int x = 1; assert(x == 2); return 0; }\n",
        )
        .unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"abort.cpp","sanitizer":"valgrind","std":"c++17"}"#)
                .unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-test", "");
        let out = tool
            .run_valgrind(&plan, &tmp_bin, &[], Some(60))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        assert!(!out.contains("clean"), "{out}");
        assert!(out.contains("0 issue(s) (exit signal)"), "{out}");
    }

    // ---- The optimizer must not delete the evidence -------------------------

    #[test]
    fn every_checker_builds_unoptimized_in_both_modes() {
        // At -O1 the optimizer legally deletes UB it can see — a store
        // after free(), a second free(), an unused allocation, an unused racy
        // read — and a deleted access is one no sanitizer can report. The
        // single-TU paths now take their flags from the same function project
        // mode injects, so the two cannot drift apart again.
        for sanitizer in ["asan_ubsan", "tsan", "msan", "valgrind"] {
            let flags = sanitizer_build_flags(sanitizer);
            assert!(flags.contains(&"-O0"), "{sanitizer}: {flags:?}");
            assert_eq!(
                flags.iter().filter(|f| f.starts_with("-O")).count(),
                1,
                "{sanitizer}: {flags:?}"
            );
        }
    }

    /// Build `source_text` as `name` with `compiler` and run it under the
    /// default ASan+UBSan checker, returning the tool's text.
    async fn run_asan_live(compiler: &str, name: &str, source_text: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), source_text).unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(&format!(
            r#"{{"source":"{name}","compiler":"{compiler}","std":"c++17"}}"#
        ))
        .unwrap();
        let plan = tool.build_plan(&args).expect("plan");
        let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-test", "");
        let out = tool
            .run_asan_ubsan(&plan, &tmp_bin, &[], Some(60))
            .await
            .map(|(text, _)| text)
            .expect("run");
        let _ = std::fs::remove_file(&tmp_bin);
        out
    }

    #[tokio::test]
    async fn the_optimizer_cannot_delete_textbook_bugs_before_asan_sees_them() {
        // Measured: at -O1 g++ reported all three of these "clean: exit
        // 0" (clang++ caught only the use-after-free).
        use crate::program_args::fixture::have;
        let cases = [
            (
                "uaf.cpp",
                "#include <cstdlib>\n#include <cstdio>\nint main(){ int *p = (int*)std::malloc(sizeof(int)); \
                 *p = 1; std::free(p); *p = 2; std::puts(\"done\"); return 0; }\n",
                "heap-use-after-free",
            ),
            (
                "dfree.cpp",
                "#include <cstdlib>\nint main(int argc, char**){ char *p = (char*)std::malloc(8 + argc); \
                 std::free(p); std::free(p); return 0; }\n",
                "double-free",
            ),
            (
                "leak.cpp",
                "#include <cstdlib>\n#include <cstdio>\nint main(){ void *p = std::malloc(123); p = nullptr; \
                 std::puts(\"done\"); return 0; }\n",
                "LeakSanitizer",
            ),
        ];
        for compiler in ["g++", "clang++"] {
            if !have(compiler) {
                eprintln!("skip: {compiler} not installed");
                continue;
            }
            for (name, text, expect) in cases {
                if expect == "LeakSanitizer" && !leak_detection_available() {
                    continue;
                }
                let out = run_asan_live(compiler, name, text).await;
                assert!(out.contains(expect), "{compiler} {name}: {out}");
                assert!(!out.contains("\nclean"), "{compiler} {name}: {out}");
            }
        }
    }

    #[tokio::test]
    async fn the_optimizer_cannot_delete_a_racy_read_before_tsan_sees_it() {
        // The same for TSan, measured with gcc and clang: an unused read of a racy
        // global is deleted at -O1 and the run reads "no race in THIS run".
        use crate::program_args::fixture::have;
        let text = "#include <pthread.h>\nint x;\nvoid *w(void*){ x = 1; return nullptr; }\n\
                    int main(){ pthread_t t; pthread_create(&t, nullptr, w, nullptr); int y = x; \
                    (void)y; pthread_join(t, nullptr); return 0; }\n";
        for compiler in ["g++", "clang++"] {
            if !have(compiler) {
                eprintln!("skip: {compiler} not installed");
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("race.cpp"), text).unwrap();
            let tool = SanitizeTool {
                cwd: dir.path().to_path_buf(),
                native_exec: true,
            };
            let args: SanitizeArgs = serde_json::from_str(&format!(
                r#"{{"source":"race.cpp","compiler":"{compiler}","std":"c++17"}}"#
            ))
            .unwrap();
            let plan = tool.build_plan(&args).expect("plan");
            let tmp_bin = unique_tmp_path("byteask-cpp-sanitize-test", "");
            let out = tool
                .run_tsan(&plan, &tmp_bin, &[], Some(60))
                .await
                .map(|(text, _)| text)
                .expect("run");
            let _ = std::fs::remove_file(&tmp_bin);
            assert!(out.contains("data race detected"), "{compiler}: {out}");
        }
    }

    #[test]
    fn a_run_timeout_says_the_build_is_unoptimized_and_how_to_proceed() {
        // The cost of -O0: it runs STL-heavy code several times slower (measured
        // 7x under ASan, 10x under TSan on a std::sort loop), so a merely
        // slow program must not read like a hang.
        let run = run_timeout_message("", 300);
        assert!(run.contains("-O0"), "{run}");
        assert!(run.contains("timeout_sec"), "{run}");
    }

    // ---- argument parsing (params) + dispatch input ------------------------

    #[test]
    fn sanitize_args_defaults() {
        let args: SanitizeArgs = serde_json::from_str(r#"{"source":"a.cpp"}"#).unwrap();
        assert_eq!(args.source, "a.cpp");
        assert_eq!(args.sanitizer, None);
        assert_eq!(args.std, None);
        assert_eq!(args.run_args, None);
    }

    #[test]
    fn sanitize_args_full() {
        let args: SanitizeArgs = serde_json::from_str(
            r#"{"source":"a.cpp","sanitizer":"valgrind","std":"c++17","run_args":"-v 2"}"#,
        )
        .unwrap();
        assert_eq!(args.sanitizer.as_deref(), Some("valgrind"));
        assert_eq!(args.std.as_deref(), Some("c++17"));
        assert_eq!(args.run_args.as_deref(), Some("-v 2"));
    }

    #[test]
    fn sanitize_args_unknown_sanitizer_accepted() {
        // serde accepts any string; the unknown value reaches the dispatch
        // match, which is where it is rejected at runtime.
        let args: SanitizeArgs =
            serde_json::from_str(r#"{"source":"a.cpp","sanitizer":"tsan"}"#).unwrap();
        assert_eq!(args.sanitizer.as_deref(), Some("tsan"));
    }

    #[test]
    fn sanitize_args_missing_source_errors() {
        // The required `source` field: handle_call surfaces this as a model error.
        assert!(serde_json::from_str::<SanitizeArgs>(r#"{"sanitizer":"valgrind"}"#).is_err());
    }

    #[test]
    fn sanitizer_default_selection() {
        // Mirrors handle_call's `args.sanitizer.as_deref().unwrap_or(DEFAULT_SANITIZER)`:
        // None falls back to the default; an unknown value flows through to the
        // error branch of the dispatch match.
        let none: Option<String> = None;
        assert_eq!(none.as_deref().unwrap_or(DEFAULT_SANITIZER), "asan_ubsan");
        assert_eq!(
            Some("tsan".to_string())
                .as_deref()
                .unwrap_or(DEFAULT_SANITIZER),
            "tsan"
        );
    }

    // ======================================================================
    // Project mode
    //
    // Every test in this block would fail on the old code for the same root
    // reason, noted individually: there was no project mode at all, so a
    // multi-file project could only be sanitized by hand-listing every other
    // .cpp in `extra_sources`, and the project's real target could not be
    // sanitized in any form.
    // ======================================================================

    fn cmake_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.16)\nproject(x)\n",
        )
        .unwrap();
        dir
    }

    fn make_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "all:\n\t$(CXX) $(CXXFLAGS) a.cpp -o app\n",
        )
        .unwrap();
        dir
    }

    fn project_args(json: &str) -> ProjectArgs {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn cmake_project_injects_our_sanitizer_flags_and_nothing_can_follow_them() {
        // Old code: no project mode, so there was no cmake argv at all and a
        // CMake project's own target could not be sanitized.
        let dir = cmake_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .project_plan(
                &project_args(r#"{"project":true,"target":"app"}"#),
                "asan_ubsan",
            )
            .expect("plan");

        let argv = plan.cmake_configure_argv();
        let ours = sanitizer_build_flags("asan_ubsan").join(" ");
        let cxx = argv
            .iter()
            .find(|arg| arg.starts_with("-DCMAKE_CXX_FLAGS="))
            .expect("CMAKE_CXX_FLAGS is set");
        assert!(cxx.contains("-fsanitize=address,undefined"), "{cxx}");
        // The value is ENTIRELY ours: nothing from the project or the model is
        // spliced into it, so no caller can place a flag after our -fsanitize=
        // and win the compiler's last-wins rule.
        assert_eq!(cxx, &format!("-DCMAKE_CXX_FLAGS={ours}"));
        for var in [
            "CMAKE_C_FLAGS",
            "CMAKE_EXE_LINKER_FLAGS",
            "CMAKE_SHARED_LINKER_FLAGS",
        ] {
            assert!(
                argv.contains(&format!("-D{var}={ours}")),
                "{var} must carry the same flags: {argv:?}"
            );
        }
        assert!(
            argv.contains(&"-DCMAKE_BUILD_TYPE=Debug".to_string()),
            "{argv:?}"
        );
        // Without this there is no compile database to read back, and the
        // instrumentation audit has nothing to audit.
        assert!(
            argv.contains(&"-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string()),
            "{argv:?}"
        );

        let build = plan.cmake_build_argv();
        assert!(
            build
                .windows(2)
                .any(|pair| pair[0] == "--parallel" && pair[1] == plan.jobs),
            "build parallelism must stay bounded on a shared box: {build:?}"
        );
        assert!(
            build
                .windows(2)
                .any(|pair| pair[0] == "--target" && pair[1] == "app"),
            "{build:?}"
        );
    }

    #[test]
    fn the_project_build_tree_is_never_inside_the_workspace() {
        // Old code: nothing configured a build tree, so nothing could put one
        // in the user's repo — this pins the rule now that something does.
        let dir = cmake_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .project_plan(&project_args(r#"{"project":true}"#), "asan_ubsan")
            .expect("plan");
        // The kept per-project tree in the state directory.
        assert!(
            crate::builds::is_stable_build_dir(&plan.build_dir),
            "{}",
            plan.build_dir.display()
        );
        assert!(
            !plan.build_dir.starts_with(dir.path()),
            "a build dir inside the workspace is a side effect the user did not ask for: {}",
            plan.build_dir.display()
        );
        // …and the configure line really points at it.
        let argv = plan.cmake_configure_argv();
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-B" && pair[1] == plan.build_dir.display().to_string()),
            "{argv:?}"
        );
    }

    #[test]
    fn makefile_project_passes_the_flags_through_cflags_cxxflags_ldflags() {
        // Old code: `make` was never invoked, so a Makefile project could not be
        // built at all.
        let dir = make_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .project_plan(
                &project_args(r#"{"project":true,"allow_in_tree_build":true,"target":"app"}"#),
                "asan_ubsan",
            )
            .expect("plan");

        let ours = sanitizer_build_flags("asan_ubsan").join(" ");
        let argv = plan.make_argv(false, &[]);
        for var in ["CFLAGS", "CXXFLAGS", "LDFLAGS"] {
            assert!(argv.contains(&format!("{var}={ours}")), "{argv:?}");
        }
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-j" && pair[1] == plan.jobs),
            "{argv:?}"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("app"),
            "the target is a bare word and goes last: {argv:?}"
        );

        // The dry run is the audit, so it must carry the SAME assignments…
        let dry = plan.make_argv(true, &[]);
        assert_eq!(dry.first().map(String::as_str), Some("-n"));
        for var in ["CFLAGS", "CXXFLAGS", "LDFLAGS"] {
            assert!(dry.contains(&format!("{var}={ours}")), "{dry:?}");
        }
        // …and must not build anything while it is at it.
        assert!(!dry.iter().any(|arg| arg == "-j"), "{dry:?}");
    }

    #[test]
    fn a_makefile_project_must_opt_in_to_an_in_tree_build() {
        // Old code: nothing ever wrote into the user's tree, so there was
        // nothing to consent to. Now there is, so it is refused by default.
        let dir = make_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        match tool.project_plan(&project_args(r#"{"project":true}"#), "asan_ubsan") {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("allow_in_tree_build"), "{msg}");
                assert!(msg.contains("working tree"), "{msg}");
                assert!(msg.contains("extra_sources"), "the way out is named: {msg}");
            }
            other => panic!("expected a refusal naming the opt-in, got {other:?}"),
        }
    }

    #[test]
    fn project_mode_without_a_build_system_points_back_at_the_single_tu_form() {
        // Old code: `project` was an unknown key, silently ignored, and the call
        // failed on the missing `source` with serde's own message instead.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.cpp"), "int main(){}\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        match tool.project_plan(&project_args(r#"{"project":true}"#), "asan_ubsan") {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("CMakeLists.txt"), "{msg}");
                assert!(msg.contains("Makefile"), "{msg}");
                assert!(msg.contains("`source`"), "{msg}");
                assert!(msg.contains("extra_sources"), "{msg}");
            }
            other => panic!("expected an actionable refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_project_flag_that_disables_the_sanitizer_is_surfaced_not_swallowed() {
        // Old code: nothing ever read a project's real compile lines, because
        // nothing built a project. This is project mode's half of "a clean
        // verdict cannot be faked" — the single-TU path enforces it by never
        // copying such a flag (compile_db_cannot_weaken_the_sanitizers); here the
        // project writes its own compile line, so instead we read it back and
        // refuse to call the result clean.
        let db = serde_json::to_string(&serde_json::json!([
            {
                "directory": "/p",
                "file": "a.cpp",
                "arguments": ["c++", "-fsanitize=address,undefined", "-fno-sanitize=all", "-c", "a.cpp"],
            },
            {
                "directory": "/p",
                "file": "b.cpp",
                "arguments": ["c++", "-fsanitize=address,undefined", "-c", "b.cpp"],
            },
        ]))
        .unwrap();
        let audit = audit_commands(&compile_db_commands(&db), "asan_ubsan");
        assert_eq!((audit.total, audit.instrumented), (2, 2));
        assert_eq!(audit.defeating, vec!["-fno-sanitize=all".to_string()]);
        assert!(audit.summary("asan_ubsan").contains("-fno-sanitize=all"));

        let weakened = audit.weakened("asan_ubsan");
        assert!(!weakened.is_empty(), "{audit:?}");

        // The verdict the run would otherwise have produced.
        let clean = verdict_line(&[], "0", false);
        assert!(clean.starts_with("clean:"), "{clean}");
        let qualified = qualify_verdict(&clean, &weakened, &[]);
        assert!(!qualified.starts_with("clean"), "{qualified}");
        assert!(qualified.contains("NOT A CLEAN RESULT"), "{qualified}");
        assert!(qualified.contains("-fno-sanitize=all"), "{qualified}");
        // …and a build with nothing wrong is left exactly as verdict_line wrote it.
        assert_eq!(qualify_verdict(&clean, &[], &[]), clean);
    }

    #[test]
    fn a_build_where_our_flags_never_reached_the_compiler_is_not_clean() {
        // Old code: unreachable — there was no project build whose flags could
        // go missing. This is the CMakeLists that does
        // `set(CMAKE_CXX_FLAGS "-O2")`, wiping the cache value we passed in.
        let db = serde_json::to_string(&serde_json::json!([
            {"directory": "/p", "file": "a.cpp", "arguments": ["c++", "-O2", "-c", "a.cpp"]},
        ]))
        .unwrap();
        let audit = audit_commands(&compile_db_commands(&db), "asan_ubsan");
        assert_eq!((audit.total, audit.instrumented), (1, 0));
        let weakened = audit.weakened("asan_ubsan");
        assert!(
            weakened.iter().any(|reason| reason.contains("not one of")),
            "{weakened:?}"
        );
        assert!(
            qualify_verdict("clean: exit 0, no sanitizer errors", &weakened, &[])
                .contains("NOT A CLEAN RESULT")
        );
    }

    #[test]
    fn a_partly_instrumented_build_is_reported_but_not_condemned() {
        // Old code: unreachable. ASan simply does not check what it did not
        // instrument — a weaker result, not a false one — so this must NOT flip
        // the verdict, or every project with one vendored dependency would read
        // as untrustworthy.
        let db = serde_json::to_string(&serde_json::json!([
            {"directory": "/p", "file": "a.cpp", "arguments": ["c++", "-fsanitize=address,undefined", "-c", "a.cpp"]},
            {"directory": "/p", "file": "v.cpp", "arguments": ["c++", "-O2", "-c", "v.cpp"]},
        ]))
        .unwrap();
        let audit = audit_commands(&compile_db_commands(&db), "asan_ubsan");
        assert_eq!((audit.total, audit.instrumented), (2, 1));
        assert!(audit.weakened("asan_ubsan").is_empty(), "{audit:?}");
        assert!(
            audit.summary("asan_ubsan").contains("1 of 2"),
            "the ratio is still reported: {}",
            audit.summary("asan_ubsan")
        );
    }

    #[test]
    fn a_partly_instrumented_build_does_not_read_clean_for_what_it_skipped() {
        // Measured on a CMake project whose lib/ resets CMAKE_CXX_FLAGS:
        // "1 of 2 compiler command(s) carry" and then "clean: exit 0, no
        // sanitizer errors" with the overflow in that lib.
        let db = serde_json::to_string(&serde_json::json!([
            {"directory": "/p", "file": "app.cpp", "arguments": ["c++", "-fsanitize=address,undefined", "-c", "/p/app.cpp", "-o", "app.o"]},
            {"directory": "/p/lib", "file": "util.cpp", "arguments": ["c++", "-O2", "-c", "/p/lib/util.cpp", "-o", "util.o"]},
        ]))
        .unwrap();
        let audit = audit_commands(&compile_db_commands(&db), "asan_ubsan");
        assert_eq!(audit.uninstrumented, vec!["/p/lib/util.cpp".to_string()]);
        let partial: Vec<String> = audit.partial("asan_ubsan").into_iter().collect();
        assert_eq!(partial.len(), 1, "{audit:?}");
        let clean = verdict_line(&[], "0", false);
        let qualified = qualify_verdict(&clean, &[], &partial);
        assert!(!qualified.starts_with("clean"), "{qualified}");
        assert!(qualified.starts_with("PARTIALLY CHECKED"), "{qualified}");
        assert!(qualified.contains("/p/lib/util.cpp"), "{qualified}");
        assert!(qualified.contains("NOT a clean result"), "{qualified}");
        // The instrumented part's own verdict is still there, unaltered.
        assert!(qualified.ends_with(&clean), "{qualified}");
        // valgrind instruments at run time, so compile coverage cannot be partial.
        assert_eq!(
            audit_commands(&compile_db_commands(&db), "valgrind").partial("valgrind"),
            None
        );
    }

    #[test]
    fn a_link_line_carrying_our_flags_instruments_nothing() {
        // A Makefile that compiles with a hardcoded `-O2 -c` and
        // links with $(LDFLAGS), where our flags also go. Counting the link
        // reads "1 of 3 compiler command(s) carry" and then "clean" with zero
        // translation units instrumented.
        let stdout = "make: Entering directory '/p'\n\
                      g++ -O2 -c app.cpp -o app.o\n\
                      g++ -O2 -c util.cpp -o util.o\n\
                      g++ -fsanitize=address,undefined -fno-omit-frame-pointer -g -O0 app.o util.o -o app\n\
                      make: Leaving directory '/p'\n";
        let audit = audit_commands(&make_dry_run_commands(stdout), "asan_ubsan");
        assert_eq!((audit.total, audit.instrumented), (2, 0), "{audit:?}");
        assert!(!audit.weakened("asan_ubsan").is_empty(), "{audit:?}");
        // …but a compile-and-link line with no -c still compiles its sources.
        let one_step = "g++ -fsanitize=address,undefined app.cpp util.cpp -o bin/app\n";
        let audit = audit_commands(&make_dry_run_commands(one_step), "asan_ubsan");
        assert_eq!((audit.total, audit.instrumented), (1, 1), "{audit:?}");
    }

    #[test]
    fn a_build_whose_compile_lines_cannot_be_read_back_is_not_clean() {
        // The preamble says "instrumentation: UNVERIFIED — no compiler
        // command lines could be read back"; the verdict must not say "clean"
        // (measured on a Makefile whose recipe calls its own `./cxx.sh -O2`).
        let audit = audit_commands(
            &make_dry_run_commands("./cxx.sh app.cpp util.cpp -o app\n"),
            "asan_ubsan",
        );
        assert_eq!(audit.total, 0);
        let weakened = audit.weakened("asan_ubsan");
        assert!(!weakened.is_empty(), "{audit:?}");
        let qualified = qualify_verdict(&verdict_line(&[], "0", false), &weakened, &[]);
        assert!(qualified.starts_with("NOT A CLEAN RESULT"), "{qualified}");
        // valgrind needs no compile flag, so nothing to verify there.
        assert!(
            audit_commands(&[], "valgrind")
                .weakened("valgrind")
                .is_empty()
        );
    }

    #[test]
    fn objects_a_make_build_did_not_rebuild_are_named() {
        // make links every object it thinks is up to date, so util.o from
        // an earlier -O2 build goes straight into the "instrumented" program:
        // "2 of 2 compile command(s) carry", then "clean", bug missed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        for (name, old) in [("app.o", false), ("util.o", true), ("sub/x.o", true)] {
            let file = std::fs::File::create(root.join(name)).unwrap();
            if old {
                file.set_modified(long_ago).unwrap();
            }
        }
        let stdout = format!(
            "make: Entering directory '{root}'\n\
             g++ -fsanitize=address,undefined -c app.cpp -o app.o\n\
             make -C sub\n\
             make[1]: Entering directory '{root}/sub'\n\
             ar rcs libx.a x.o\n\
             make[1]: Leaving directory '{root}/sub'\n\
             g++ -fsanitize=address,undefined app.o util.o sub/libx.a -o app\n",
            root = root.display()
        );
        let linked = linked_inputs(&stdout, root);
        assert_eq!(
            linked,
            vec![
                root.join("sub/x.o"),
                root.join("app.o"),
                root.join("util.o"),
                root.join("sub/libx.a")
            ]
        );
        let started = SystemTime::now() - std::time::Duration::from_secs(60);
        assert_eq!(
            stale_files(&linked, started, root),
            vec!["sub/x.o".to_string(), "util.o".to_string()],
            "app.o was rebuilt; libx.a does not exist yet"
        );
    }

    #[tokio::test]
    async fn a_make_build_that_reuses_an_uninstrumented_object_is_not_clean() {
        // End to end, a project with a stale object: built once with -O2,
        // then app.cpp edited. The overflow is in util.cpp, whose -O2 object
        // make links in unchanged.
        use crate::program_args::fixture::have;
        if !have("make") || !have("g++") {
            eprintln!("skip: make or g++ not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Makefile"),
            "CXX = g++\nCXXFLAGS = -O2\napp: app.o util.o\n\t$(CXX) $(CXXFLAGS) $(LDFLAGS) app.o util.o -o app\n\
             %.o: %.cpp\n\t$(CXX) $(CXXFLAGS) -c $< -o $@\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app.cpp"),
            "int overflow(int n);\nint main(int argc, char**){ return overflow(argc - 1); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("util.cpp"),
            "#include <cstdlib>\nint overflow(int n){ char *p = (char*)std::malloc(8); p[8 + n] = 1; \
             std::free(p); return 0; }\n",
        )
        .unwrap();
        let built = std::process::Command::new("make")
            .arg("-s")
            .current_dir(root)
            .status()
            .unwrap();
        assert!(built.success());
        // Age the earlier build, then "edit" app.cpp: make rebuilds app.o and
        // relinks, and reuses util.o.
        let hours_ago = |h: u64| SystemTime::now() - std::time::Duration::from_secs(3600 * h);
        for (name, age) in [("util.cpp", 2), ("util.o", 1), ("app.o", 1), ("app", 1)] {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(root.join(name))
                .unwrap();
            file.set_modified(hours_ago(age)).unwrap();
        }

        let tool = SanitizeTool {
            cwd: root.to_path_buf(),
            native_exec: true,
        };
        let args = project_args(r#"{"project":true,"allow_in_tree_build":true,"timeout_sec":120}"#);
        let plan = tool.project_plan(&args, "asan_ubsan").expect("plan");
        let out = tool
            .run_project(&plan, &args, &[])
            .await
            .map(|(text, _)| text)
            .expect("run");
        assert!(out.contains("PARTIALLY CHECKED"), "{out}");
        assert!(out.contains("util.o"), "{out}");
        assert!(!out.contains("\nclean"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_project_builds_of_one_tree_do_not_trample_each_other() {
        // The model runs tools in parallel; an unlocked asan and tsan project
        // build of one Makefile tree linked each other's objects (6 of 6 runs
        // failed with the OTHER sanitizer's symbols) or hit `Text file busy`.
        use crate::program_args::fixture::have;
        if !have("make") || !have("g++") {
            eprintln!("skip: make/g++ not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Makefile"),
            "app: a.o b.o\n\t$(CXX) $(CXXFLAGS) $(LDFLAGS) a.o b.o -o app\n\
             %.o: %.cpp\n\t$(CXX) $(CXXFLAGS) -c $< -o $@\n",
        )
        .unwrap();
        std::fs::write(root.join("a.cpp"), "int b();\nint main(){ return b(); }\n").unwrap();
        std::fs::write(root.join("b.cpp"), "int b(){ return 0; }\n").unwrap();
        let tool = SanitizeTool {
            cwd: root.to_path_buf(),
            native_exec: true,
        };
        let args = project_args(r#"{"project":true,"allow_in_tree_build":true,"timeout_sec":120}"#);
        let asan = tool.project_plan(&args, "asan_ubsan").expect("plan");
        let tsan = tool.project_plan(&args, "tsan").expect("plan");
        let (a, t) = tokio::join!(
            tool.run_project(&asan, &args, &[]),
            tool.run_project(&tsan, &args, &[])
        );
        for out in [
            a.map(|(text, _)| text).expect("asan run"),
            t.map(|(text, _)| text).expect("tsan run"),
        ] {
            assert!(!out.contains("Text file busy"), "{out}");
            assert!(!out.contains("undefined reference to `__tsan"), "{out}");
            assert!(!out.contains("undefined reference to `__asan"), "{out}");
        }
    }

    /// A small real CMake project: `app` links `util`.
    fn real_cmake_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.16)\nproject(t5 CXX)\nadd_executable(app app.cpp util.cpp)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app.cpp"),
            "int util();\nint main(){ return util(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("util.cpp"), "int util(){ return 0; }\n").unwrap();
        dir
    }

    fn find_named(dir: &Path, name: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == name) {
                return Some(path);
            }
            if path.is_dir()
                && let Some(found) = find_named(&path, name)
            {
                return Some(found);
            }
        }
        None
    }

    #[tokio::test]
    async fn a_second_cmake_build_of_a_project_is_incremental() {
        // Without the kept tree, every call would configure and build the whole
        // project again in a fresh temp dir, then delete it.
        use crate::program_args::fixture::have;
        if !have("cmake") || (!have("g++") && !have("c++")) {
            eprintln!("skip: cmake/c++ not installed");
            return;
        }
        let dir = real_cmake_project();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args = project_args(r#"{"project":true,"target":"app","timeout_sec":300}"#);
        let plan = tool.project_plan(&args, "asan_ubsan").expect("plan");
        let (text, result) = tool
            .run_project(&plan, &args, &[])
            .await
            .expect("first run");
        assert!(result.is_pass(), "{text}");
        let object =
            find_named(&plan.build_dir, "util.cpp.o").expect("util object in the kept tree");
        let built = std::fs::metadata(&object).unwrap().modified().unwrap();

        let again = tool.project_plan(&args, "asan_ubsan").expect("plan");
        assert_eq!(
            again.build_dir, plan.build_dir,
            "the same tree is used again"
        );
        let (text, result) = tool
            .run_project(&again, &args, &[])
            .await
            .expect("second run");
        assert!(result.is_pass(), "{text}");
        assert_eq!(
            std::fs::metadata(&object).unwrap().modified().unwrap(),
            built,
            "util.cpp did not change, so it must not be compiled again"
        );
        let _ = std::fs::remove_dir_all(plan.build_dir.parent().unwrap());
    }

    #[tokio::test]
    async fn a_kept_tree_that_no_longer_configures_is_rebuilt_not_reported_as_a_build_failure() {
        // Our own stale cache must never read as the project failing
        // to build. Here the cache claims another source directory, which
        // CMake refuses to reconfigure.
        use crate::program_args::fixture::have;
        if !have("cmake") || (!have("g++") && !have("c++")) {
            eprintln!("skip: cmake/c++ not installed");
            return;
        }
        let dir = real_cmake_project();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args = project_args(r#"{"project":true,"target":"app","timeout_sec":300}"#);
        let plan = tool.project_plan(&args, "asan_ubsan").expect("plan");
        let (text, result) = tool
            .run_project(&plan, &args, &[])
            .await
            .expect("first run");
        assert!(result.is_pass(), "{text}");

        let cache = plan.build_dir.join("CMakeCache.txt");
        let stale: String = std::fs::read_to_string(&cache)
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with("CMAKE_HOME_DIRECTORY:") {
                    "CMAKE_HOME_DIRECTORY:INTERNAL=/somewhere/else/entirely".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&cache, stale).unwrap();

        let (text, result) = tool
            .run_project(&plan, &args, &[])
            .await
            .expect("second run");
        assert!(
            result.is_pass(),
            "a stale cache of ours became a verdict: {text}"
        );
        let _ = std::fs::remove_dir_all(plan.build_dir.parent().unwrap());
    }

    #[test]
    fn a_target_given_as_a_path_is_found_and_cmake_gets_its_name() {
        // `bin/app` must not be "not found" when the candidate list shows it.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let app = bin.join("app");
        std::fs::write(&app, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        std::fs::write(dir.path().join("Makefile"), "all:\n").unwrap();
        let mut plan = tool
            .project_plan(
                &project_args(r#"{"project":true,"target":"bin/app","allow_in_tree_build":true}"#),
                "asan_ubsan",
            )
            .expect("plan");
        plan.build_dir = dir.path().to_path_buf();
        assert_eq!(locate_executable(&plan, None).expect("found"), app);
        assert!(
            plan.cmake_build_argv()
                .ends_with(&["--target".to_string(), "app".to_string()])
        );
    }

    #[test]
    fn make_dry_run_reads_the_real_recipe_lines() {
        // Old code: unreachable. A makefile that spells out `g++` ignores
        // CXXFLAGS entirely, so `make -n` is the only place the miss is visible
        // before a verdict is written from the binary it produces.
        let stdout = "make: Entering directory '/p'\n\
                      g++ -O2 -c a.cpp -o a.o\n\
                      cd sub && clang++ -fsanitize=address,undefined -c b.cpp -o b.o\n\
                      ar rcs libx.a a.o b.o\n\
                      mkdir -p bin\n";
        let audit = audit_commands(&make_dry_run_commands(stdout), "asan_ubsan");
        assert_eq!(
            audit.total, 2,
            "ar/mkdir/cd are not compiler invocations: {audit:?}"
        );
        assert_eq!(
            audit.instrumented, 1,
            "the `cd … && clang++` line must still be audited: {audit:?}"
        );
    }

    #[test]
    fn make_with_nothing_to_rebuild_means_the_binary_is_from_an_older_build() {
        // Old code: unreachable, and it is the quietest way project mode could
        // lie — make does nothing, we run a binary from a previous
        // uninstrumented build, and it is perfectly clean.
        assert!(make_nothing_to_do("make: Nothing to be done for 'all'.\n"));
        assert!(make_nothing_to_do("make: 'app' is up to date.\n"));
        assert!(!make_nothing_to_do("g++ -c a.cpp -o a.o\n"));
    }

    #[test]
    fn project_audit_rejects_the_same_flags_the_compile_db_allowlist_does() {
        // Old code: `is_safe_target_flag` already refused -mllvm on the way IN;
        // this is the same reasoning applied to a project's own compile line,
        // which we cannot filter and can only report.
        let toks: Vec<String> = [
            "clang++",
            "-mllvm",
            "-tsan-instrument-memory-accesses=0",
            "-fsanitize=thread",
            "-c",
            "a.cpp",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(
            sanitizer_defeating_tokens(&toks, "tsan")
                .iter()
                .any(|t| t == "-mllvm"),
            "{toks:?}"
        );

        // An ASan run against a project that builds itself with TSan is a
        // conflict, not a clean result.
        let toks: Vec<String> = ["c++", "-fsanitize=thread", "-c", "a.cpp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            sanitizer_defeating_tokens(&toks, "asan_ubsan"),
            vec!["-fsanitize=thread".to_string()]
        );

        // …while a harmless recovery policy must NOT be read as a disable.
        let toks: Vec<String> = ["c++", "-fno-sanitize-recover=undefined", "-c", "a.cpp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            sanitizer_defeating_tokens(&toks, "asan_ubsan").is_empty(),
            "{toks:?}"
        );

        // …and our OWN flag is never reported as defeating itself.
        let toks: Vec<String> = ["c++", "-fsanitize=address,undefined", "-c", "a.cpp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            sanitizer_defeating_tokens(&toks, "asan_ubsan").is_empty(),
            "{toks:?}"
        );
    }

    #[test]
    fn valgrind_project_builds_have_no_compile_flag_to_defeat() {
        // Old code: unreachable. valgrind instruments the finished binary, so a
        // project -fno-sanitize cannot touch it — but a project that builds
        // itself with ASan does break memcheck, and that IS reported.
        assert_eq!(sanitizer_build_flags("valgrind"), vec!["-g", "-O0"]);
        assert_eq!(sanitizer_probe_flag("valgrind"), None);
        let harmless: Vec<String> = ["c++", "-fno-sanitize=all", "-c", "a.cpp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(sanitizer_defeating_tokens(&harmless, "valgrind").is_empty());
        let conflicting: Vec<String> = ["c++", "-fsanitize=address", "-c", "a.cpp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            sanitizer_defeating_tokens(&conflicting, "valgrind"),
            vec!["-fsanitize=address".to_string()]
        );
    }

    #[test]
    fn a_target_name_cannot_look_like_a_build_system_option() {
        // Old code: there was no `target`, so nothing model-supplied reached a
        // build system's argv. Now one does, and it must not be readable as a
        // flag — least of all one that could undo the -D we just set.
        assert!(is_safe_target_name("hft_engine"));
        assert!(is_safe_target_name("bin/app"));
        assert!(is_safe_target_name("app-v2.1"));
        assert!(!is_safe_target_name("-DCMAKE_CXX_FLAGS=-fno-sanitize=all"));
        assert!(!is_safe_target_name("--target"));
        assert!(!is_safe_target_name("../../etc/passwd"));
        assert!(!is_safe_target_name("a b"));
        assert!(!is_safe_target_name(""));

        let dir = cmake_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        match tool.project_plan(
            &project_args(r#"{"project":true,"target":"-DCMAKE_CXX_FLAGS=-fno-sanitize=all"}"#),
            "asan_ubsan",
        ) {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("not a plausible build-target name"), "{msg}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn library_and_script_names_are_never_mistaken_for_the_program() {
        // Old code: unreachable — the binary's path was ours (`tmp_bin`), so
        // nothing had to be located. In a project build tree the executable bit
        // is set on plenty of things that are not the program under test.
        assert!(is_plausible_executable_name("a.out"));
        assert!(is_plausible_executable_name("hft_engine"));
        assert!(is_plausible_executable_name("app.exe"));
        assert!(!is_plausible_executable_name("libx.so"));
        assert!(!is_plausible_executable_name("libx.so.1"));
        assert!(!is_plausible_executable_name("x.o"));
        assert!(!is_plausible_executable_name("build.ninja"));
        assert!(!is_plausible_executable_name("cmake_install.cmake"));
        assert!(!is_plausible_executable_name("run_tests.sh"));
        assert!(!is_plausible_executable_name("Makefile"));
        assert!(!is_plausible_executable_name(".ninja_deps"));
    }

    #[test]
    fn tsan_verdict_wording_has_exactly_one_definition() {
        // Extracted from run_tsan so project mode reuses it verbatim. Copied
        // instead of shared, the two paths could drift and one of them would
        // stop saying that a clean TSan run is not proof of absence.
        let silent = tsan_verdict("all quiet\n", "0", false);
        assert!(silent.contains("no race in THIS run (exit 0)"), "{silent}");
        assert!(silent.contains("not proof of absence"), "{silent}");
        let fatal = tsan_verdict(
            "FATAL: ThreadSanitizer: unexpected memory mapping\n",
            "1",
            false,
        );
        assert!(fatal.contains("FAILED TO RUN"), "{fatal}");
        assert!(
            fatal.contains("neither a race nor a clean result"),
            "{fatal}"
        );
        let report = tsan_verdict("WARNING: ThreadSanitizer: data race (pid=1)\n", "66", false);
        assert!(report.contains("data race detected (exit 66)"), "{report}");
        let cut = tsan_verdict("all quiet\n", "0", true);
        assert!(cut.contains("OUTPUT TRUNCATED"), "{cut}");
        assert!(!cut.contains("no race in THIS run"), "{cut}");
    }

    #[test]
    fn the_single_tu_path_is_untouched_by_project_mode() {
        // The dispatch added to handle_call: without `project`, arguments still
        // parse as SanitizeArgs and build exactly the compile line they did
        // before — no CMake variable, no project flag, nothing new.
        let probe: ModeProbe = serde_json::from_str(r#"{"source":"a.cpp"}"#).unwrap();
        assert!(!probe.project);
        assert_eq!(probe.source.as_deref(), Some("a.cpp"));
        let probe: ModeProbe = serde_json::from_str(r#"{"project":true}"#).unwrap();
        assert!(probe.project);
        assert_eq!(probe.source, None);
        // …and the mode probe never needs `source`, which is what lets
        // SanitizeArgs keep requiring it (sanitize_args_missing_source_errors).
        let probe: ModeProbe = serde_json::from_str("{}").unwrap();
        assert!(!probe.project);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("main.cpp"), "int main(){}\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let args: SanitizeArgs = serde_json::from_str(
            r#"{"source":"main.cpp","include_dirs":["include"],"defines":["NDEBUG"]}"#,
        )
        .unwrap();
        let argv = tool.build_plan(&args).expect("plan").compile_argv();
        assert_eq!(argv[0], "-std=c++23");
        assert!(argv.contains(&"-DNDEBUG".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|arg| arg.contains("CMAKE")), "{argv:?}");
        assert!(!argv.iter().any(|arg| arg.contains("sanitize")), "{argv:?}");
    }

    #[test]
    fn locate_executable_picks_the_one_binary_and_names_the_others() {
        // Old code: unreachable. Nothing located a binary because the tool
        // compiled its own; in a real build tree this is where a project build
        // either finds its program or has to say why it cannot.
        let dir = tempfile::tempdir().unwrap();
        let plan = ProjectPlan {
            kind: ProjectKind::Cmake,
            build_file: dir.path().join("CMakeLists.txt"),
            root: dir.path().to_path_buf(),
            build_dir: dir.path().to_path_buf(),
            target: None,
            flags: sanitizer_build_flags("asan_ubsan").join(" "),
            sanitizer: "asan_ubsan".to_string(),
            jobs: project_build_jobs(),
        };

        // Nothing built yet: an actionable refusal, never a verdict.
        let err = locate_executable(&plan, None).expect_err("no executables yet");
        assert!(err.contains("neither clean nor a finding"), "{err}");
        assert!(err.contains("`target`"), "{err}");

        std::fs::write(dir.path().join("app"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("libx.so"), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for name in ["app", "libx.so"] {
                std::fs::set_permissions(
                    dir.path().join(name),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
        }
        // The shared object is executable too, and is still not the program.
        assert_eq!(
            locate_executable(&plan, None).ok(),
            Some(dir.path().join("app"))
        );
    }

    // ---- Symbolizer discovery, quieter note, makefile flags kept ----------

    #[test]
    fn a_versioned_symbolizer_is_found_under_its_real_name() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(lib.join("llvm-7/bin")).unwrap();
        std::fs::create_dir_all(lib.join("llvm-11/bin")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(lib.join("llvm-7/bin/llvm-symbolizer"), "").unwrap();
        std::fs::write(lib.join("llvm-11/bin/llvm-symbolizer"), "").unwrap();
        // A versioned name on PATH that is NOT a link to an unversioned file:
        // LLVM <= 12 rejects it, so it must not win.
        std::fs::write(bin.join("llvm-symbolizer-12"), "").unwrap();
        assert_eq!(
            find_symbolizer(&[bin.clone()], &lib),
            Some(lib.join("llvm-11/bin/llvm-symbolizer"))
        );
        // From 13 the versioned name itself is accepted.
        std::fs::write(bin.join("llvm-symbolizer-15"), "").unwrap();
        assert_eq!(
            find_symbolizer(&[bin.clone()], &lib),
            Some(bin.join("llvm-symbolizer-15"))
        );
        // Nothing anywhere: nothing.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            find_symbolizer(&[empty.path().to_path_buf()], empty.path()),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_versioned_link_resolves_to_the_unversioned_binary() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(lib.join("llvm-9/bin")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let real = lib.join("llvm-9/bin/llvm-symbolizer");
        std::fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, bin.join("llvm-symbolizer-9")).unwrap();
        let other_lib = dir.path().join("nolib");
        let found = find_symbolizer(&[bin], &other_lib).expect("found");
        assert_eq!(found.file_name().unwrap(), "llvm-symbolizer");
    }

    #[test]
    fn the_runtime_is_pointed_at_a_discovered_symbolizer() {
        let base = vec![
            ("ASAN_OPTIONS".to_string(), "symbolize=1".to_string()),
            (
                "UBSAN_OPTIONS".to_string(),
                "print_stacktrace=1".to_string(),
            ),
        ];
        assert_eq!(with_symbolizer(base.clone(), None), base);
        let env = with_symbolizer(base, Some(Path::new("/usr/lib/llvm-9/bin/llvm-symbolizer")));
        assert_eq!(
            env[0].1,
            "symbolize=1:external_symbolizer_path=/usr/lib/llvm-9/bin/llvm-symbolizer"
        );
        assert!(
            env[1]
                .1
                .ends_with(":external_symbolizer_path=/usr/lib/llvm-9/bin/llvm-symbolizer")
        );
        assert!(env.contains(&(
            "ASAN_SYMBOLIZER_PATH".to_string(),
            "/usr/lib/llvm-9/bin/llvm-symbolizer".to_string()
        )));
    }

    #[test]
    fn only_bare_address_frames_count_as_unsymbolized() {
        // GCC's runtime symbolizes by itself: no note for these.
        assert!(!has_unsymbolized_frames(
            "    #0 0x55d1 in main /tmp/a.c:3\n    #1 0x7f12 in __libc_start_main (/lib/libc.so.6+0x21b96)\n"
        ));
        assert!(has_unsymbolized_frames(
            "    #0 0x4011d6  (/tmp/app+0x11d6)\n"
        ));
        assert!(!has_unsymbolized_frames("#include <x>\nclean run\n"));
    }

    #[test]
    fn makefile_flag_values_come_from_the_make_database() {
        let db = "# Variables\n\n# default\nCC = cc\n# makefile (from 'Makefile', line 3)\n\
                  LDFLAGS := -lz -lm\n# makefile (from 'Makefile', line 2)\nCFLAGS = $(OPT) -Iinc\n\
                  # environment\nCXXFLAGS = -DENV\n# makefile (from 'Makefile', line 9)\n\
                  override LDFLAGS = -lbad\n# automatic\n@D = $(patsubst %/,%,$(dir $@))\n";
        assert_eq!(
            makefile_flag_values(db),
            vec![
                ("LDFLAGS".to_string(), "-lz -lm".to_string()),
                ("CFLAGS".to_string(), "$(OPT) -Iinc".to_string()),
                ("CXXFLAGS".to_string(), "-DENV".to_string()),
            ]
        );
        // A simply expanded value is already expanded: its `$` is escaped.
        assert_eq!(
            makefile_flag_values("# makefile (from 'M', line 1)\nLDFLAGS := -Wl,-rpath,$ORIGIN\n"),
            vec![("LDFLAGS".to_string(), "-Wl,-rpath,$$ORIGIN".to_string())]
        );
        assert!(makefile_flag_values("").is_empty());
    }

    #[test]
    fn the_makefiles_own_flags_are_kept_and_ours_come_last() {
        let dir = make_workspace();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .project_plan(
                &project_args(r#"{"project":true,"allow_in_tree_build":true}"#),
                "asan_ubsan",
            )
            .expect("plan");
        let ours = sanitizer_build_flags("asan_ubsan").join(" ");
        let inherited = vec![("LDFLAGS".to_string(), "-lz".to_string())];
        let argv = plan.make_argv(false, &inherited);
        assert!(argv.contains(&format!("LDFLAGS=-lz {ours}")), "{argv:?}");
        assert!(argv.contains(&format!("CFLAGS={ours}")), "{argv:?}");
        assert_eq!(
            plan.make_argv(true, &inherited)
                .iter()
                .filter(|a| a.starts_with("LDFLAGS=-lz "))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn live_a_makefile_that_links_its_own_libs_still_builds() {
        if !crate::program_args::fixture::have("make") || !crate::program_args::fixture::have("cc")
        {
            eprintln!("skip: make/cc not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "LDFLAGS = -lm\napp: a.c\n\t$(CC) $(CFLAGS) a.c -o app $(LDFLAGS)\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.c"), "#include <math.h>\nint main(int c, char **v) { (void)v; return (int)sqrt((double)c) - 1; }\n").unwrap();
        let tool = SanitizeTool {
            cwd: dir.path().to_path_buf(),
            native_exec: true,
        };
        let plan = tool
            .project_plan(
                &project_args(r#"{"project":true,"allow_in_tree_build":true}"#),
                "asan_ubsan",
            )
            .expect("plan");
        let inherited = tool.makefile_flag_values(&plan).await;
        assert_eq!(inherited, vec![("LDFLAGS".to_string(), "-lm".to_string())]);
    }
}

/// The shared result beside every verdict sentence. Each case pairs the
/// sentence a person reads with the result a gate reads, from the same inputs.
#[cfg(test)]
mod result_tests {
    use check_result::Verdict;
    use pretty_assertions::assert_eq;

    use super::*;

    fn finding() -> Finding {
        Finding {
            source: "asan".to_string(),
            severity: Severity::Error,
            file: "main.cpp".to_string(),
            line: 12,
            column: 3,
            message: "heap-use-after-free".to_string(),
            rule: None,
        }
    }

    #[test]
    fn only_a_complete_clean_exit_zero_run_is_a_pass() {
        assert!(findings_run_result(&[], "0", false, true).is_pass());
        assert!(verdict_line(&[], "0", false).starts_with("clean"));
    }

    #[test]
    fn leaks_not_checked_is_approximate_not_pass() {
        let result = findings_run_result(&[], "0", false, false);
        assert_eq!(result.verdict, Verdict::Approximate);
        assert!(clean_verdict(false).contains("LEAKS WERE NOT CHECKED"));
    }

    #[test]
    fn a_truncated_capture_is_never_clean() {
        assert_eq!(
            findings_run_result(&[], "0", true, true).verdict,
            Verdict::NotRun
        );
        assert!(verdict_line(&[], "0", true).contains("NOT a clean result"));
        assert_eq!(
            valgrind_result(&[], "0", true, true).verdict,
            Verdict::NotRun
        );
        assert_eq!(tsan_result("", "0", true).verdict, Verdict::NotRun);
        // Findings read before the cut are still real.
        assert_eq!(
            findings_run_result(&[finding()], "0", true, true).verdict,
            Verdict::Fail
        );
    }

    #[test]
    fn findings_and_failing_exits_fail() {
        let result = findings_run_result(&[finding()], "signal", false, true);
        assert_eq!(result.verdict, Verdict::Fail);
        assert!(result.summary.contains("main.cpp:12"), "{}", result.summary);
        assert_eq!(
            findings_run_result(&[], "3", false, true).verdict,
            Verdict::Fail
        );
    }

    #[test]
    fn memcheck_with_no_report_is_not_run() {
        let result = valgrind_result(&[], "0", false, false);
        assert_eq!(result.verdict, Verdict::NotRun);
        assert!(valgrind_verdict(&[], "0", false, false).contains("NOT a clean result"));
        assert!(valgrind_result(&[], "0", false, true).is_pass());
    }

    #[test]
    fn tsan_failing_to_start_is_not_run_and_a_race_is_a_fail() {
        let fatal = "FATAL: ThreadSanitizer: unexpected memory mapping 0x1-0x2\n";
        assert_eq!(tsan_result(fatal, "66", false).verdict, Verdict::NotRun);
        let race = "==1==WARNING: ThreadSanitizer: data race (pid=1)\n";
        assert_eq!(tsan_result(race, "66", false).verdict, Verdict::Fail);
        assert!(tsan_result("", "0", false).is_pass());
        assert_eq!(tsan_result("", "1", false).verdict, Verdict::Fail);
    }

    #[test]
    fn a_weakened_build_cannot_pass_and_a_partial_one_is_approximate() {
        let clean = findings_run_result(&[], "0", false, true);
        let weakened = vec!["-fno-sanitize=address on 1 compile line".to_string()];
        assert_eq!(
            qualify_result(clean.clone(), &weakened, &[]).verdict,
            Verdict::NoClaim
        );
        let partial = vec!["lib/x.c was not instrumented".to_string()];
        assert_eq!(
            qualify_result(clean.clone(), &[], &partial).verdict,
            Verdict::Approximate
        );
        assert!(qualify_result(clean, &[], &[]).is_pass());
        let failed = findings_run_result(&[finding()], "0", false, true);
        assert_eq!(
            qualify_result(failed, &weakened, &[]).verdict,
            Verdict::Fail
        );
    }

    #[test]
    fn build_and_run_stops_are_not_run_with_fixes() {
        for result in [
            build_failed_result(),
            build_timeout_result(),
            run_timeout_result(),
        ] {
            assert_eq!(result.verdict, Verdict::NotRun);
            assert!(result.fix.is_some_and(|fix| fix.contains("cpp_sanitize")));
        }
    }
}
