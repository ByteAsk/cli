use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

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

use crate::finding::Finding;
use crate::finding::Severity;
use crate::finding::render_findings;
use crate::run::RunOutcome;
use crate::run::run_capped;

const TOOL_NAME: &str = "cpp_check";
const DEFAULT_ENABLE: &str = "warning,performance,portability";
const FINDINGS_CAP: usize = 50;
/// Default time budget. A single file that takes cppcheck longer than this is
/// usually being explored across many preprocessor configurations or a deep
/// include graph; waiting longer rarely produces a result.
const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 60;
/// cppcheck template that maps directly onto our [`Finding`] fields.
const CPPCHECK_TEMPLATE: &str = "{file}:{line}:{column}: {severity}: {message} [{id}]";

pub struct CppCheckTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CppCheckArgs {
    path: String,
    #[serde(default)]
    enable: Option<String>,
    /// Header search paths. cppcheck supports `-I` natively; without them
    /// every `#include "project/header.hpp"` is unresolvable and the
    /// analysis runs on a partial view of the code.
    #[serde(default)]
    include_dirs: Option<Vec<String>>,
    /// Use `compile_commands.json` (`--project=`), which gives cppcheck the
    /// real include paths and defines for the whole project at once.
    #[serde(default)]
    use_compile_db: Option<bool>,
    /// Wall-clock cap for the cppcheck run. Defaults to
    /// [`DEFAULT_CHECK_TIMEOUT_SECS`].
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    timeout_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for CppCheckTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_string(),
            JsonSchema::string(Some(
                "File or directory to analyze, relative to the workspace or absolute.".to_string(),
            )),
        );
        properties.insert(
            "enable".to_string(),
            JsonSchema::string(Some(format!(
                "Comma-separated cppcheck check categories (default: {DEFAULT_ENABLE})."
            ))),
        );
        properties.insert(
            "include_dirs".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Header search paths (become -I). Pass these for a project layout like \
                     include/foo.hpp, or cppcheck analyses the file with its includes unresolved."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "use_compile_db".to_string(),
            JsonSchema::boolean(Some(
                "Analyse via compile_commands.json (--project=) when one is present, so cppcheck \
                 sees the project's real include paths and defines. Defaults to true; set false \
                 to analyse just `path` in isolation."
                    .to_string(),
            )),
        );
        properties.insert(
            "timeout_sec".to_string(),
            JsonSchema::number(Some(
                "Wall-clock cap in seconds (default 300). Raise it for a large tree or a \
                 whole-project --project= run."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Run cppcheck static analysis on a C/C++ file or directory and return \
                structured findings (file:line: severity: message). Uses compile_commands.json \
                automatically when one exists; otherwise pass include_dirs for a project with a \
                separate include/ tree. Run it on C/C++ you just wrote or edited — including \
                brand-new code in an otherwise empty directory — BEFORE telling the user it \
                works; needs no build system."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["path".to_string()]),
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

impl CppCheckTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: CppCheckArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let enable = args.enable.as_deref().unwrap_or(DEFAULT_ENABLE);
        let target = resolve_path(&self.cwd, &args.path);

        let db = if args.use_compile_db.unwrap_or(true) {
            super::compile_db::find_compile_db(&self.cwd)
        } else {
            None
        };
        // One `cppcheck --version` per session: `--file-filter` only exists
        // from 2.0, and an unknown option fails the whole run.
        let version = crate::toolver::tool_version("cppcheck", &self.cwd).await;
        // Given a compile database, cppcheck analyses the whole DB and
        // ignores `path` — a file the DB does not list would never be analysed
        // and would read "No findings.". A single file is analysed through the DB only
        // when the DB lists it (and then only that file); otherwise directly.
        let mut scope = match &db {
            None => Scope::Path,
            Some(db) if !target.is_file() => Scope::WholeProject(db.clone()),
            Some(db) => match db_entry_path(db, &target) {
                Some(entry) if supports_file_filter(version) => {
                    Scope::FileInProject(db.clone(), entry)
                }
                // cppcheck < 2.0 has no --file-filter: hand it this entry's
                // own include paths and defines instead of --project.
                Some(_) => Scope::FileWithDbFlags(
                    db.clone(),
                    super::sanitizers::db_includes_and_defines(db, &target).unwrap_or_default(),
                ),
                // Not in the database (a header, a new file): borrow the -I/-D
                // flags of the nearest entry rather than none at all.
                None => match nearest_db_entry(db, &target).and_then(|entry| {
                    super::sanitizers::db_includes_and_defines(db, &entry)
                        .map(|flags| (entry, flags))
                }) {
                    Some((entry, flags)) => Scope::FileWithNearestFlags(db.clone(), entry, flags),
                    None => Scope::FileNotInProject(db.clone()),
                },
            },
        };
        if matches!(scope, Scope::Path | Scope::FileNotInProject(_)) && !target.exists() {
            return Err(FunctionCallError::RespondToModel(format!(
                "no such file or directory: {} — nothing was analyzed.",
                target.display()
            )));
        }
        // cppcheck checks only SOURCE files in a directory; one holding only
        // headers failed "could not find or open any of the paths given".
        // Its headers are analysed directly instead.
        if matches!(scope, Scope::Path)
            && target.is_dir()
            && let Some(headers) = headers_only(&target)
        {
            scope = Scope::Headers(headers);
        }
        let includes: Vec<PathBuf> = args
            .include_dirs
            .iter()
            .flatten()
            .map(|dir| resolve_path(&self.cwd, dir))
            .collect();

        // cppcheck parses a `.h` as C unless told otherwise, so a C++ header
        // came back as one "syntaxError: … is invalid C code". Decide from the
        // project (compile DB, sibling sources) or the header itself.
        let cpp_header = match &scope {
            Scope::Headers(headers) => headers
                .iter()
                .any(|h| is_ambiguous_header(h))
                .then(|| header_is_cpp(&self.cwd, &target, db.as_deref(), headers.first()))
                .unwrap_or(false),
            _ if target.is_file() && is_ambiguous_header(&target) => {
                header_is_cpp(&self.cwd, &target, db.as_deref(), Some(&target))
            }
            _ => false,
        };

        let argv = build_args(enable, &target, &scope, &includes, cpp_header);
        // Capped like every other spawning tool: cppcheck over a large tree (and
        // especially with `--project=`, which expands to every TU in the compile
        // database) can run for many minutes, and an uncapped `.output()` would
        // wedge the whole turn with nothing on screen. `run_capped` also puts it
        // in its own process group, so a timeout kills the tree rather than
        // orphaning it.
        let timeout = Duration::from_secs(
            args.timeout_sec
                .unwrap_or(DEFAULT_CHECK_TIMEOUT_SECS)
                .max(1),
        );
        let run = run_capped("cppcheck", &argv, &self.cwd, timeout).await?;

        let target_label = args.path.clone();
        let (success, code, stdout, stderr, truncated) = match run {
            RunOutcome::Finished {
                success,
                code,
                stdout,
                stderr,
                truncated,
            } => (success, code, stdout, stderr, truncated),
            RunOutcome::TimedOut { secs } => {
                // Not "clean": we have no verdict at all. Say so explicitly so a
                // timeout can never be read as "cppcheck found nothing", and
                // steer away from the retry that cost minutes and found nothing.
                let body = timeout_report(secs);
                let result = crate::results::timed_out(
                    TOOL_NAME,
                    "cppcheck",
                    secs,
                    "don't retry the same call; narrow `path` to the one file you changed.",
                );
                return Ok(crate::report::report(
                    TOOL_NAME,
                    &target_label,
                    body,
                    result,
                ));
            }
        };

        // cppcheck writes findings to stderr.
        let findings: Vec<Finding> = stderr.lines().filter_map(parse_finding).collect();

        let mut body = describe_scope(&scope, includes.len());
        if cpp_header {
            body.push_str(
                "language: C++ (the header was parsed as C++, from the project's sources)\n",
            );
        }
        // cppcheck reports its OWN failures ("could not find or open any
        // of the paths given", an unknown --enable name) on stdout with exit
        // 1; only stderr was read, so every failure read "No findings.".
        if !success {
            let status =
                code.map_or_else(|| "a signal".to_string(), |c| format!("exit status {c}"));
            let said = format!("{}\n{}", stdout.trim(), stderr.trim());
            let said = said.trim();
            if findings.is_empty() {
                body.push_str(&failure_report(said, &status, version, &args.path));
                let result = check_result::CheckResult::not_run(
                    TOOL_NAME,
                    check_result::NotRunReason::ToolError,
                    &format!("cppcheck itself failed ({status}), so nothing was checked."),
                    "do what the report above suggests, then call cpp_check again.",
                );
                return Ok(crate::report::report(
                    TOOL_NAME,
                    &target_label,
                    body,
                    result,
                ));
            }
            body.push_str(&format!("note: cppcheck exited with {status}: {said}\n"));
        }
        if truncated {
            body.push_str(
                "note: cppcheck output was truncated at the capture cap; the \
                 finding list below may be incomplete.\n",
            );
        }
        body.push_str(&status_line(&findings));
        body.push_str(&render_findings(&findings, FINDINGS_CAP));
        let result = crate::results::findings_result(TOOL_NAME, &findings, truncated);
        Ok(crate::report::report(
            TOOL_NAME,
            &target_label,
            body,
            result,
        ))
    }
}

/// What one cppcheck call analyses.
enum Scope {
    /// No compile database: the given path.
    Path,
    /// A directory with a compile database: everything the database lists.
    WholeProject(PathBuf),
    /// One file the database lists: that file, with the database's flags.
    FileInProject(PathBuf, String),
    /// One file the database does not list: the file on its own.
    FileNotInProject(PathBuf),
    /// One file the database lists, on a cppcheck without `--file-filter`
    /// (< 2.0): the file, with that entry's `-I` dirs and `-D` defines.
    FileWithDbFlags(PathBuf, (Vec<String>, Vec<String>)),
    /// One file the database does not list (a header, a new file): the file,
    /// with the `-I` dirs and `-D` defines of the nearest entry it does list.
    FileWithNearestFlags(PathBuf, PathBuf, (Vec<String>, Vec<String>)),
    /// A directory holding headers but no source file: those headers.
    Headers(Vec<PathBuf>),
}

/// `--file-filter` arrived in cppcheck 2.0 (1.90, still shipped by some LTS
/// distributions, rejects it as an unrecognized option).
fn supports_file_filter(version: Option<crate::toolver::Version>) -> bool {
    crate::toolver::at_least(version, (2, 0))
}

/// Max headers handed to cppcheck for a header-only directory.
const HEADERS_CAP: usize = 200;
const SOURCE_EXTS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "C", "tpp", "txx", "ipp", "ixx",
];
const HEADER_EXTS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "H"];

/// The headers under `dir` when it holds headers but no source file cppcheck
/// would pick up on its own; `None` otherwise (then cppcheck is given the
/// directory, exactly as before).
fn headers_only(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut headers = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            visited += 1;
            if visited > 20_000 {
                return None;
            }
            if path.is_dir() {
                if depth < 8 {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if SOURCE_EXTS.contains(&ext) {
                return None;
            }
            if HEADER_EXTS.contains(&ext) && headers.len() < HEADERS_CAP {
                headers.push(path);
            }
        }
    }
    (!headers.is_empty()).then_some(headers)
}

/// A header whose extension does not say C or C++ (`.h`, `.H`); cppcheck
/// treats `.hpp`/`.hh`/`.hxx` as C++ already.
fn is_ambiguous_header(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("h" | "H"))
}

/// Should this `.h` be parsed as C++? The project's evidence first, then the
/// header's own syntax.
fn header_is_cpp(root: &Path, target: &Path, db: Option<&Path>, sample: Option<&PathBuf>) -> bool {
    let near = if target.is_dir() {
        Some(target)
    } else {
        target.parent()
    };
    if let Some(verdict) = crate::toolver::project_is_cpp(root, near, db) {
        return verdict;
    }
    sample
        .and_then(|h| std::fs::read_to_string(h).ok())
        .is_some_and(|text| crate::toolver::looks_like_cpp(&text))
}

/// Head-first status for a run that finished: findings or clean.
fn status_line(findings: &[Finding]) -> String {
    if findings.is_empty() {
        "status: clean — cppcheck ran and reported nothing.\nNo findings.".to_string()
    } else {
        format!(
            "status: {} finding(s)\n{}",
            findings.len(),
            render_findings(findings, FINDINGS_CAP)
        )
    }
}

/// A cppcheck that failed before reporting anything: why, and what to do.
fn failure_report(
    said: &str,
    status: &str,
    version: Option<crate::toolver::Version>,
    path: &str,
) -> String {
    let ver = crate::toolver::version_label(version);
    let (reason, next) = if said.contains("unrecognized command line option") {
        (
            format!("this cppcheck{ver} does not accept an option the tool passed"),
            "run cppcheck from the shell with fewer options, or use cpp_tidy / a compiler \
             warning build instead"
                .to_string(),
        )
    } else if said.contains("could not find or open any of the paths") {
        (
            format!("cppcheck found no C/C++ file to analyse at `{path}`"),
            "pass an existing source file, or a header file directly".to_string(),
        )
    } else {
        (
            format!("cppcheck{ver} exited with {status} before analysing anything"),
            "fix the argument it names below (e.g. `enable`) and call again".to_string(),
        )
    };
    crate::toolver::did_not_run(&reason, &next, said)
}

/// Assemble the cppcheck command line.
///
/// With a compile database, `--project=` supplies every include path and define
/// and cppcheck analyses what the database lists; `--file-filter` narrows that
/// to the one file asked about. Any `include_dirs` still ride along, so a
/// database missing a path can be topped up.
fn build_args(
    enable: &str,
    target: &Path,
    scope: &Scope,
    includes: &[PathBuf],
    cpp_header: bool,
) -> Vec<String> {
    // One preprocessor configuration: by default cppcheck checks every
    // combination of the #ifdefs it sees (capped at 12), which is what made a
    // single file run for minutes. With the database's -D flags the one it
    // checks is the configuration the project builds.
    let mut argv = vec![
        format!("--enable={enable}"),
        format!("--template={CPPCHECK_TEMPLATE}"),
        "--max-configs=1".to_string(),
    ];
    if cpp_header {
        argv.push("--language=c++".to_string());
    }
    for dir in includes {
        argv.push(format!("-I{}", dir.display()));
    }
    match scope {
        Scope::WholeProject(db) => argv.push(format!("--project={}", db.display())),
        Scope::FileInProject(db, entry) => {
            argv.push(format!("--project={}", db.display()));
            argv.push(format!("--file-filter={entry}"));
        }
        Scope::FileWithDbFlags(_, (dirs, defines))
        | Scope::FileWithNearestFlags(_, _, (dirs, defines)) => {
            for dir in dirs {
                argv.push(format!("-I{dir}"));
            }
            for define in defines {
                argv.push(format!("-D{define}"));
            }
            argv.push(target.display().to_string());
        }
        Scope::Headers(headers) => {
            argv.extend(headers.iter().map(|h| h.display().to_string()));
        }
        Scope::Path | Scope::FileNotInProject(_) => argv.push(target.display().to_string()),
    }
    argv
}

/// One line of provenance so the model knows how wide the analysis actually was.
fn describe_scope(scope: &Scope, includes: usize) -> String {
    match scope {
        Scope::WholeProject(db) => format!(
            "scope: whole project via {} ({includes} extra include dir(s))\n",
            db.display()
        ),
        Scope::FileInProject(db, _) => format!(
            "scope: this file, with its flags from {} ({includes} extra include dir(s))\n",
            db.display()
        ),
        Scope::FileNotInProject(db) => format!(
            "scope: this file on its own — {} does not list it, so the project's include \
             paths and defines were NOT applied ({includes} extra include dir(s))\n",
            db.display()
        ),
        Scope::FileWithDbFlags(db, (dirs, defines)) => format!(
            "scope: this file, with its {} include dir(s) and {} define(s) from {} (this \
             cppcheck has no --file-filter) ({includes} extra include dir(s))\n",
            dirs.len(),
            defines.len(),
            db.display()
        ),
        Scope::FileWithNearestFlags(db, entry, (dirs, defines)) => format!(
            "scope: this file — {} does not list it, so it was analysed with the {} include \
             dir(s) and {} define(s) of the nearest entry, {} ({includes} extra include dir(s))\n",
            db.display(),
            dirs.len(),
            defines.len(),
            entry.display()
        ),
        Scope::Headers(headers) => format!(
            "scope: the {} header(s) in this directory (it holds no source file) \
             ({includes} extra include dir(s))\n",
            headers.len()
        ),
        Scope::Path if includes > 0 => {
            format!("scope: the given path, with {includes} include dir(s)\n")
        }
        Scope::Path => String::new(),
    }
}

/// The compile-database entry for `target`, spelled as cppcheck will see it
/// (`directory` joined with `file`), or `None` when the database does not
/// list it (or cannot be read).
fn db_entry_path(db: &Path, target: &Path) -> Option<String> {
    let text = std::fs::read_to_string(db).ok()?;
    let entries: serde_json::Value = serde_json::from_str(&text).ok()?;
    let want = super::sanitizers::canonical(target);
    let db_dir = db.parent().unwrap_or(Path::new("."));
    entries.as_array()?.iter().find_map(|entry| {
        let directory = entry
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let directory = super::sanitizers::resolve_path(db_dir, directory);
        let file = entry.get("file").and_then(|v| v.as_str())?;
        let path = super::sanitizers::resolve_path(&directory, file);
        (super::sanitizers::canonical(&path) == want).then(|| path.display().to_string())
    })
}

/// The database entry nearest to `target` (longest shared directory prefix;
/// the first such entry on a tie), for a file the database does not list.
fn nearest_db_entry(db: &Path, target: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(db).ok()?;
    let entries: serde_json::Value = serde_json::from_str(&text).ok()?;
    let want = super::sanitizers::canonical(target);
    let db_dir = db.parent().unwrap_or(Path::new("."));
    entries
        .as_array()?
        .iter()
        .filter_map(|entry| {
            let directory = entry
                .get("directory")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let directory = super::sanitizers::resolve_path(db_dir, directory);
            let file = entry.get("file").and_then(|v| v.as_str())?;
            Some(super::sanitizers::resolve_path(&directory, file))
        })
        .min_by_key(|path| {
            let path = super::sanitizers::canonical(path);
            let shared = path
                .components()
                .zip(want.components())
                .take_while(|(a, b)| a == b)
                .count();
            std::cmp::Reverse(shared)
        })
}

/// A run that hit the time budget: no verdict, and no point repeating it.
fn timeout_report(secs: u64) -> String {
    crate::toolver::did_not_run(
        &format!(
            "cppcheck did not finish within {secs}s (it was still expanding preprocessor \
             configurations or includes)"
        ),
        "don't retry the same call; narrow `path` to the one file you changed, or rely on the \
         compiler's warnings and the project's tests",
        "",
    )
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Parse a single cppcheck line emitted with [`CPPCHECK_TEMPLATE`] into a
/// [`Finding`]. Non-matching lines (progress output, etc.) return `None`.
fn parse_finding(line: &str) -> Option<Finding> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Peel off the trailing `[id]` rule, when present.
    let (head, rule) = match (line.rfind(" ["), line.strip_suffix(']')) {
        (Some(idx), Some(_)) => {
            let rule = line[idx + 2..line.len() - 1].to_string();
            let rule = if rule.is_empty() { None } else { Some(rule) };
            (&line[..idx], rule)
        }
        _ => (line, None),
    };

    // `file:line:column: severity: message`. The file may itself contain
    // ": " ("Project: v2/bug.cpp"), so the location ends at the first ": "
    // whose left side ends in `:<line>:<column>` — not simply the first ": ",
    // which dropped every finding under such a path.
    let (file, line_no, column, rest) = head.match_indices(": ").find_map(|(at, _)| {
        let location = &head[..at];
        let mut parts = location.rsplitn(3, ':');
        let column = parts.next()?.parse::<u32>().ok()?;
        let line_no = parts.next()?.parse::<u32>().ok()?;
        let file = parts.next()?.to_string();
        Some((file, line_no, column, &head[at + 2..]))
    })?;
    let (severity_str, message) = rest.split_once(": ")?;

    Some(Finding {
        source: "cppcheck".to_string(),
        severity: severity_from_str(severity_str),
        file,
        line: line_no,
        column,
        message: message.trim().to_string(),
        rule,
    })
}

fn severity_from_str(severity: &str) -> Severity {
    match severity {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => Severity::Note,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// cppcheck was the last spawning tool still on a raw `.output()`, i.e. able
    /// to wedge a whole turn with no time limit — a `--project=` run expands to
    /// every TU in the compile database. It now goes through `run_capped`, and
    /// the cap has to stay reachable from the tool call.
    #[test]
    fn exposes_a_timeout_argument() {
        let args: CppCheckArgs = serde_json::from_str(r#"{"path":"src","timeout_sec":900}"#)
            .expect("timeout_sec must be accepted");
        assert_eq!(args.timeout_sec, Some(900));

        // And the model has to be able to discover it, or the cap is only ever
        // the 60s default and a large tree cannot be analysed at all.
        let tool = CppCheckTool {
            cwd: std::path::PathBuf::from("/tmp"),
        };
        let spec = serde_json::to_string(&tool.spec()).expect("spec should serialize");
        assert!(spec.contains("timeout_sec"), "{spec}");
    }

    #[test]
    fn parses_finding_with_rule() {
        let finding = parse_finding("main.cpp:10:5: error: Null pointer dereference [nullPointer]")
            .expect("should parse");
        assert_eq!(
            finding,
            Finding {
                source: "cppcheck".to_string(),
                severity: Severity::Error,
                file: "main.cpp".to_string(),
                line: 10,
                column: 5,
                message: "Null pointer dereference".to_string(),
                rule: Some("nullPointer".to_string()),
            }
        );
    }

    #[test]
    fn parses_finding_without_rule() {
        let finding =
            parse_finding("src/util.c:3:1: warning: unused variable").expect("should parse");
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.file, "src/util.c");
        assert_eq!(finding.line, 3);
        assert_eq!(finding.column, 1);
        assert_eq!(finding.message, "unused variable");
        assert_eq!(finding.rule, None);
    }

    #[test]
    fn maps_unknown_severity_to_note() {
        let finding = parse_finding("a.cpp:1:1: performance: slow thing [perf]").unwrap();
        assert_eq!(finding.severity, Severity::Note);
    }

    #[test]
    fn ignores_progress_lines() {
        assert_eq!(parse_finding("Checking main.cpp ..."), None);
        assert_eq!(parse_finding(""), None);
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

    // ---- Include paths and the compile database ---------------------------

    #[test]
    fn passes_no_include_paths_by_default() {
        // The pre-B3b behaviour, pinned: bare invocation (one configuration),
        // target last.
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/a.cpp"),
            &Scope::Path,
            &[],
            false,
        );
        assert_eq!(
            argv,
            vec![
                format!("--enable={DEFAULT_ENABLE}"),
                format!("--template={CPPCHECK_TEMPLATE}"),
                "--max-configs=1".to_string(),
                "/w/a.cpp".to_string(),
            ]
        );
    }

    #[test]
    fn include_dirs_become_dash_i() {
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/a.cpp"),
            &Scope::Path,
            &[PathBuf::from("/w/include"), PathBuf::from("/w/vendor")],
            false,
        );
        assert!(argv.contains(&"-I/w/include".to_string()), "{argv:?}");
        assert!(argv.contains(&"-I/w/vendor".to_string()), "{argv:?}");
        assert_eq!(argv.last().unwrap(), "/w/a.cpp", "the target stays last");
    }

    #[test]
    fn compile_db_replaces_the_bare_target() {
        // cppcheck warns when --project is combined with an explicit path, and
        // the database already names every TU.
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w"),
            &Scope::WholeProject(PathBuf::from("/w/compile_commands.json")),
            &[PathBuf::from("/w/extra")],
            false,
        );
        assert!(
            argv.contains(&"--project=/w/compile_commands.json".to_string()),
            "{argv:?}"
        );
        assert!(!argv.contains(&"/w".to_string()), "{argv:?}");
        assert!(argv.contains(&"-I/w/extra".to_string()), "{argv:?}");
        // One file the DB lists: the DB's flags, narrowed to that file.
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/a.cpp"),
            &Scope::FileInProject(PathBuf::from("/w/compile_commands.json"), "/w/a.cpp".into()),
            &[],
            false,
        );
        assert!(
            argv.contains(&"--file-filter=/w/a.cpp".to_string()),
            "{argv:?}"
        );
        // One file the DB does NOT list: that file, directly.
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/new.cpp"),
            &Scope::FileNotInProject(PathBuf::from("/w/compile_commands.json")),
            &[],
            false,
        );
        assert_eq!(argv.last().unwrap(), "/w/new.cpp");
        assert!(!argv.iter().any(|a| a.starts_with("--project")), "{argv:?}");
    }

    #[test]
    fn a_path_containing_colon_space_keeps_its_findings() {
        // A split on the FIRST ": " drops every finding under such a path.
        let f = parse_finding(
            "Project: v2/bug.cpp:1:30: error: Array 'a[2]' accessed at index 3. [arrayIndexOutOfBounds]",
        )
        .expect("a finding");
        assert_eq!(
            (f.file.as_str(), f.line, f.column),
            ("Project: v2/bug.cpp", 1, 30)
        );
        assert!(f.message.starts_with("Array"), "{}", f.message);
    }

    async fn check(dir: &Path, args: serde_json::Value) -> Result<String, FunctionCallError> {
        let tool = CppCheckTool {
            cwd: dir.to_path_buf(),
        };
        crate::test_support::call(&tool, args).await
    }

    #[tokio::test]
    async fn a_failed_cppcheck_is_never_no_findings() {
        // cppcheck's own errors go to stdout with exit 1; reading only stderr
        // makes them read "No findings.".
        if !crate::program_args::fixture::have("cppcheck") {
            eprintln!("skip: cppcheck not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bug.cpp"),
            "int main(){int a[2]; return a[3];}\n",
        )
        .unwrap();
        let out = check(
            dir.path(),
            serde_json::json!({"path": "bug.cpp", "enable": "warnings"}),
        )
        .await
        .expect("a report");
        assert!(
            out.contains("DID NOT RUN") && out.contains("unknown name 'warnings'"),
            "{out}"
        );
        assert!(out.contains("NOT a clean result"), "{out}");
        assert!(!out.contains("No findings"), "{out}");
        let err = check(dir.path(), serde_json::json!({"path": "nope.cpp"}))
            .await
            .expect_err("missing");
        assert!(
            matches!(err, FunctionCallError::RespondToModel(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_file_outside_the_compile_db_is_still_analysed() {
        // With a compile DB present, `path` must not be ignored — or a buggy
        // file the DB does not list reads "No findings.".
        if !crate::program_args::fixture::have("cppcheck") {
            eprintln!("skip: cppcheck not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.cpp"), "int f(){ return 0; }\n").unwrap();
        std::fs::write(
            root.join("new.cpp"),
            "int main(){int a[2]; a[0]=1; return a[3];}\n",
        )
        .unwrap();
        let db = serde_json::json!([{
            "directory": root.display().to_string(),
            "file": "old.cpp",
            "command": "c++ -c old.cpp",
        }]);
        std::fs::write(root.join("compile_commands.json"), db.to_string()).unwrap();
        let out = check(root, serde_json::json!({"path": "new.cpp"}))
            .await
            .expect("a report");
        assert!(out.contains("arrayIndexOutOfBounds"), "{out}");
        assert!(out.contains("does not list it"), "{out}");
    }

    #[test]
    fn scope_line_states_how_wide_the_analysis_was() {
        assert!(
            describe_scope(
                &Scope::WholeProject(PathBuf::from("/w/compile_commands.json")),
                0
            )
            .contains("whole project")
        );
        assert!(describe_scope(&Scope::Path, 2).contains("2 include dir(s)"));
        // No database and no includes: nothing new to say, output unchanged.
        assert_eq!(describe_scope(&Scope::Path, 0), "");
    }

    #[test]
    fn compile_db_use_is_opt_out() {
        let default: CppCheckArgs = serde_json::from_str(r#"{"path":"a.cpp"}"#).unwrap();
        assert_eq!(
            default.use_compile_db, None,
            "absent means the true default"
        );
        assert!(default.use_compile_db.unwrap_or(true));
        let off: CppCheckArgs =
            serde_json::from_str(r#"{"path":"a.cpp","use_compile_db":false}"#).unwrap();
        assert_eq!(off.use_compile_db, Some(false));
    }

    // ---- Robustness across cppcheck versions and project layouts ----------

    #[test]
    fn every_scope_checks_one_preprocessor_configuration() {
        let db = PathBuf::from("/w/compile_commands.json");
        for scope in [
            Scope::Path,
            Scope::WholeProject(db.clone()),
            Scope::FileInProject(db.clone(), "/w/a.cpp".into()),
            Scope::FileNotInProject(db),
        ] {
            let argv = build_args(DEFAULT_ENABLE, Path::new("/w/a.cpp"), &scope, &[], false);
            assert!(argv.contains(&"--max-configs=1".to_string()), "{argv:?}");
        }
    }

    #[test]
    fn a_file_the_db_does_not_list_gets_its_nearest_entrys_flags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/a")).unwrap();
        std::fs::create_dir_all(root.join("src/b")).unwrap();
        std::fs::write(root.join("src/a/x.cpp"), "").unwrap();
        std::fs::write(root.join("src/b/y.cpp"), "").unwrap();
        std::fs::write(root.join("src/b/new.h"), "").unwrap();
        let db = root.join("compile_commands.json");
        let entries = serde_json::json!([
            {"directory": root.display().to_string(), "file": "src/a/x.cpp",
             "command": "c++ -Iinc_a -DSIDE_A=1 -c src/a/x.cpp"},
            {"directory": root.display().to_string(), "file": "src/b/y.cpp",
             "command": "c++ -Iinc_b -DSIDE_B=1 -c src/b/y.cpp"},
        ]);
        std::fs::write(&db, entries.to_string()).unwrap();
        let target = root.join("src/b/new.h");
        let entry = nearest_db_entry(&db, &target).expect("an entry");
        assert!(entry.ends_with("src/b/y.cpp"), "{}", entry.display());
        let flags = super::super::sanitizers::db_includes_and_defines(&db, &entry).expect("flags");
        assert_eq!(flags.1, vec!["SIDE_B=1".to_string()]);
        let scope = Scope::FileWithNearestFlags(db.clone(), entry, flags);
        let argv = build_args(DEFAULT_ENABLE, &target, &scope, &[], false);
        assert!(argv.contains(&"-DSIDE_B=1".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("--project")), "{argv:?}");
        assert_eq!(argv.last().unwrap(), &target.display().to_string());
        let line = describe_scope(&scope, 0);
        assert!(
            line.contains("does not list it") && line.contains("nearest entry"),
            "{line}"
        );
    }

    #[test]
    fn a_timeout_is_did_not_run_and_advises_against_a_retry() {
        assert_eq!(DEFAULT_CHECK_TIMEOUT_SECS, 60);
        let out = timeout_report(60);
        assert!(out.starts_with("status: DID NOT RUN"), "{out}");
        assert!(out.contains("within 60s"), "{out}");
        assert!(out.contains("don't retry the same call"), "{out}");
        assert!(out.contains("NOT a clean result"), "{out}");
    }

    #[test]
    fn file_filter_is_used_only_where_cppcheck_has_it() {
        assert!(supports_file_filter(Some((2, 0))));
        assert!(supports_file_filter(Some((2, 13))));
        assert!(!supports_file_filter(Some((1, 90))));
        // An unrecognised banner keeps today's argv.
        assert!(supports_file_filter(None));
        // The < 2.0 form: the entry's own flags and the file, no --project.
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/a.cpp"),
            &Scope::FileWithDbFlags(
                PathBuf::from("/w/compile_commands.json"),
                (vec!["/w/include".into()], vec!["NDEBUG".into()]),
            ),
            &[],
            false,
        );
        assert!(argv.contains(&"-I/w/include".to_string()), "{argv:?}");
        assert!(argv.contains(&"-DNDEBUG".to_string()), "{argv:?}");
        assert_eq!(argv.last().unwrap(), "/w/a.cpp");
        assert!(
            !argv
                .iter()
                .any(|a| a.starts_with("--project") || a.starts_with("--file-filter")),
            "{argv:?}"
        );
    }

    #[test]
    fn a_cpp_header_is_parsed_as_cpp() {
        let argv = build_args(DEFAULT_ENABLE, Path::new("/w/a.h"), &Scope::Path, &[], true);
        assert!(argv.contains(&"--language=c++".to_string()), "{argv:?}");
        let argv = build_args(
            DEFAULT_ENABLE,
            Path::new("/w/a.c"),
            &Scope::Path,
            &[],
            false,
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--language")),
            "{argv:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("include/api.h"), "int f(void);\n").unwrap();
        // A C++ project: its .h is C++.
        std::fs::write(root.join("src/a.cpp"), "").unwrap();
        let header = root.join("include/api.h");
        assert!(header_is_cpp(root, &header, None, Some(&header)));
        // A C project: its .h stays C.
        std::fs::remove_file(root.join("src/a.cpp")).unwrap();
        std::fs::write(root.join("src/a.c"), "").unwrap();
        assert!(!header_is_cpp(root, &header, None, Some(&header)));
        assert!(is_ambiguous_header(Path::new("a.h")));
        assert!(!is_ambiguous_header(Path::new("a.hpp")));
    }

    #[test]
    fn a_header_only_directory_is_expanded_to_its_headers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("inc/sub")).unwrap();
        std::fs::write(root.join("inc/a.h"), "").unwrap();
        std::fs::write(root.join("inc/sub/b.hpp"), "").unwrap();
        std::fs::write(root.join("inc/README"), "").unwrap();
        let headers = headers_only(&root.join("inc")).expect("headers");
        assert_eq!(headers.len(), 2, "{headers:?}");
        // Any source file: cppcheck handles the directory itself, as before.
        std::fs::write(root.join("inc/sub/impl.cpp"), "").unwrap();
        assert_eq!(headers_only(&root.join("inc")), None);
    }

    #[test]
    fn a_failure_says_did_not_run_and_what_to_do() {
        let text = failure_report(
            "cppcheck: error: unrecognized command line option: \"--file-filter=/w/a.cpp\".",
            "exit status 1",
            Some((1, 90)),
            "a.cpp",
        );
        assert!(text.starts_with("status: DID NOT RUN"), "{text}");
        assert!(text.contains("cppcheck 1.90 does not accept"), "{text}");
        let text = failure_report(
            "cppcheck: error: could not find or open any of the paths given.",
            "exit status 1",
            None,
            "inc",
        );
        assert!(text.contains("no C/C++ file to analyse at `inc`"), "{text}");
        assert!(status_line(&[]).starts_with("status: clean"));
    }

    #[tokio::test]
    async fn live_a_cpp_header_and_a_header_directory_are_analysed() {
        if !crate::program_args::fixture::have("cppcheck") {
            eprintln!("skip: cppcheck not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.cpp"), "int main() { return 0; }\n").unwrap();
        std::fs::write(
            root.join("include/lib.h"),
            "namespace lib {\ninline int get() { int a[2] = {0, 0}; return a[3]; }\n}\n",
        )
        .unwrap();
        let out = check(root, serde_json::json!({"path": "include/lib.h"}))
            .await
            .expect("a report");
        assert!(!out.contains("invalid C code"), "{out}");
        assert!(out.contains("arrayIndexOutOfBounds"), "{out}");
        let out = check(root, serde_json::json!({"path": "include"}))
            .await
            .expect("a report");
        assert!(!out.contains("could not find or open"), "{out}");
        assert!(out.contains("header(s) in this directory"), "{out}");
    }
}
