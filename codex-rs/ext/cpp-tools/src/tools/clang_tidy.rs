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
use crate::run::DEFAULT_TIMEOUT_SECS;
use crate::run::RunOutcome;
use crate::run::run_capped;

const TOOL_NAME: &str = "cpp_tidy";
const FINDINGS_CAP: usize = 50;
/// Language standard used when no compile database is available (single-file
/// mode), passed after `--` so clang-tidy hands it to the compiler.
const SINGLE_FILE_STD: &str = "-std=c++20";
const SINGLE_FILE_STD_C: &str = "-std=c17";

pub struct ClangTidyTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClangTidyArgs {
    path: String,
    #[serde(default)]
    checks: Option<String>,
    #[serde(default)]
    build_dir: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for ClangTidyTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_string(),
            JsonSchema::string(Some(
                "C/C++ source file to analyze, relative to the workspace or absolute.".to_string(),
            )),
        );
        properties.insert(
            "checks".to_string(),
            JsonSchema::string(Some(
                "Optional clang-tidy check filter passed as -checks=<checks> \
                 (e.g. \"bugprone-*,modernize-*\")."
                    .to_string(),
            )),
        );
        properties.insert(
            "build_dir".to_string(),
            JsonSchema::string(Some(
                "Optional directory containing compile_commands.json. If omitted, \
                 the workspace, build/, and out/ are searched."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Run clang-tidy static analysis on a C/C++ file and return structured \
                findings with check names and fix hints. Use proactively after editing C/C++ to \
                catch bugs and modern-C++ issues. Do NOT use for non-C/C++ files or comment-only \
                edits."
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

impl ClangTidyTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: ClangTidyArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let target = resolve_path(&self.cwd, &args.path);
        if !target.is_file() {
            return Err(FunctionCallError::RespondToModel(format!(
                "no such source file: {} (pass a C/C++ file, relative to the workspace or \
                 absolute). Nothing was analyzed.",
                target.display()
            )));
        }

        let version = crate::toolver::tool_version("clang-tidy", &self.cwd).await;
        let compile_db = match args.build_dir.as_deref() {
            Some(dir) => Some(resolve_path(&self.cwd, dir)),
            None => find_compile_db(&self.cwd),
        };
        let single = compile_db
            .is_none()
            .then(|| single_file_flags(&self.cwd, &target, version));
        let argv = tidy_argv(
            version,
            args.checks.as_deref(),
            compile_db.as_deref(),
            &target,
            single.as_ref(),
        );

        // Through run_capped: a wall-clock cap and a process group, where
        // a raw `.output()` blocks forever on a FIFO and a spawn error other
        // than "not found" would end the user's turn.
        let (success, code, stdout, stderr, truncated) = match run_capped(
            "clang-tidy",
            &argv,
            &self.cwd,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        )
        .await?
        {
            RunOutcome::Finished {
                success,
                code,
                stdout,
                stderr,
                truncated,
            } => (success, code, stdout, stderr, truncated),
            RunOutcome::TimedOut { secs } => {
                return Ok(crate::report::report_plain(
                    TOOL_NAME,
                    format!(
                        "clang-tidy did not finish within {secs}s and was stopped. NOT a clean \
                         result: nothing was reported."
                    ),
                    crate::results::timed_out(
                        TOOL_NAME,
                        "clang-tidy",
                        secs,
                        "call cpp_tidy again on one file, or with fewer `checks`.",
                    ),
                ));
            }
        };

        // clang-tidy writes findings to stdout.
        let findings = parse_findings(&stdout);
        let mut body = String::new();
        if let Some(single) = &single {
            body.push_str(&single.describe());
        }
        body.push_str(&tidy_body(&findings, success, code, &stdout, &stderr));
        Ok(crate::report::report(
            TOOL_NAME,
            &args.path,
            body,
            tidy_result(&findings, success, code, truncated),
        ))
    }
}

/// A clang-tidy that FAILED (unknown check pattern, "no checks enabled", a
/// file it could not process, a compile error it reports without a location)
/// used to read "No findings." — its exit status and stderr were ignored.
/// A failed run with no parsed finding is never presented as clean, and any
/// failed run carries its exit status and the tail of what it printed.
fn tidy_body(
    findings: &[Finding],
    success: bool,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> String {
    let rendered = render_findings(findings, FINDINGS_CAP);
    let unresolved = findings
        .iter()
        .filter(|f| f.message.ends_with("file not found"))
        .count();
    let mut status = if findings.is_empty() {
        "status: clean — clang-tidy ran and reported nothing.\n".to_string()
    } else {
        format!("status: {} finding(s)\n", findings.len())
    };
    if unresolved > 0 {
        status.push_str(&format!(
            "note: analysis PARTIAL — {unresolved} #include(s) could not be resolved, so code \
             depending on them was not checked. Pass `build_dir` (a directory with \
             compile_commands.json) or run cpp_compile_db.\n"
        ));
    }
    if success {
        return format!("{status}{rendered}");
    }
    let exit = code.map_or_else(|| "a signal".to_string(), |c| format!("exit status {c}"));
    // clang-tidy puts its own errors on stderr ("Error: no checks enabled.",
    // "Error while processing …") and may follow them with its whole usage
    // text on stdout. Lines that say `error` are the ones worth showing — the
    // tail alone ("1 error generated.") dropped the error itself.
    let detail = error_lines(stdout, stderr);
    if findings.is_empty() {
        let (reason, next) = if detail.contains("Unknown command line argument") {
            (
                "this clang-tidy does not accept an option the tool passed".to_string(),
                "run clang-tidy from the shell with fewer options, or use cpp_check".to_string(),
            )
        } else {
            (
                format!("clang-tidy exited with {exit} and reported no diagnostic"),
                "fix what the output below names (a check pattern, a flag, the file) and call \
                 again"
                    .to_string(),
            )
        };
        crate::toolver::did_not_run(&reason, &next, &detail)
    } else {
        format!("{status}{rendered}\n\nnote: clang-tidy exited with {exit}:\n{detail}")
    }
}

/// The lines of clang-tidy's output that report an error (with or without a
/// location), else the last 15 lines of stderr (or stdout).
fn error_lines(stdout: &str, stderr: &str) -> String {
    let errors: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("error:") || t.starts_with("Error") || l.contains(": error:")
        })
        .take(10)
        .collect();
    if !errors.is_empty() {
        return errors.join("\n");
    }
    if stderr.trim().is_empty() {
        tail_lines(stdout, 15)
    } else {
        tail_lines(stderr, 15)
    }
}

/// Assemble the clang-tidy command line from what this version accepts.
///
/// - `--quiet` exists from LLVM 5; older distro builds reject it.
/// - `--warnings-as-errors=-*` (LLVM 3.8+) overrides a project `.clang-tidy`
///   `WarningsAsErrors`, which otherwise turns ordinary findings into a failed
///   run; this tool reports findings, it does not gate on them.
fn tidy_argv(
    version: Option<crate::toolver::Version>,
    checks: Option<&str>,
    compile_db: Option<&Path>,
    target: &Path,
    single: Option<&SingleFile>,
) -> Vec<String> {
    let mut argv = Vec::new();
    if crate::toolver::at_least(version, (5, 0)) {
        argv.push("--quiet".to_string());
    }
    if crate::toolver::at_least(version, (3, 8)) {
        argv.push("--warnings-as-errors=-*".to_string());
    }
    if let Some(checks) = checks {
        argv.push(format!("-checks={checks}"));
    }
    if let Some(dir) = compile_db {
        argv.push("-p".to_string());
        argv.push(dir.display().to_string());
    }
    argv.push(target.display().to_string());
    if let Some(single) = single {
        // Arguments after `--` are the whole compile command.
        argv.push("--".to_string());
        argv.extend(single.flags());
    }
    argv
}

/// Single-file mode (no compile database): the compile flags we assume, said
/// out loud so a missing header reads as "the assumption was wrong", not as a
/// bug in the code.
#[derive(Debug, PartialEq, Eq)]
struct SingleFile {
    /// `Some("c++")` for a `.h` in a C++ project (clang parses `.h` as C).
    language: Option<&'static str>,
    std: String,
    include_dirs: Vec<PathBuf>,
}

impl SingleFile {
    fn flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if let Some(language) = self.language {
            flags.push("-x".to_string());
            flags.push(language.to_string());
        }
        flags.push(format!("-std={}", self.std));
        flags.extend(
            self.include_dirs
                .iter()
                .map(|d| format!("-I{}", d.display())),
        );
        flags
    }

    fn describe(&self) -> String {
        let dirs: Vec<String> = self
            .include_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        format!(
            "assumed (no compile_commands.json found): {}-std={}, include dirs: {}\n",
            self.language
                .map_or_else(String::new, |l| format!("-x {l}, ")),
            self.std,
            if dirs.is_empty() {
                "none".to_string()
            } else {
                dirs.join(", ")
            }
        )
    }
}

/// Conventional include roots tried in single-file mode, relative to the
/// workspace, when they exist.
const COMMON_INCLUDE_DIRS: &[&str] = &["include", "src", "lib"];

fn single_file_flags(
    root: &Path,
    target: &Path,
    version: Option<crate::toolver::Version>,
) -> SingleFile {
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_c = match ext {
        "c" => true,
        // clang parses `.h` as C; decide from the project.
        "h" => !crate::toolver::project_is_cpp(root, target.parent(), None).unwrap_or_else(|| {
            std::fs::read_to_string(target).is_ok_and(|t| crate::toolver::looks_like_cpp(&t))
        }),
        _ => false,
    };
    let language = (ext == "h" && !is_c).then_some("c++");
    let std = if is_c {
        c_std(version)
    } else {
        cpp_std(version)
    };
    let mut include_dirs = Vec::new();
    let mut push = |dir: PathBuf| {
        if dir.is_dir() && !include_dirs.contains(&dir) {
            include_dirs.push(dir);
        }
    };
    if let Some(parent) = target.parent() {
        push(parent.to_path_buf());
    }
    push(root.to_path_buf());
    for sub in COMMON_INCLUDE_DIRS {
        push(root.join(sub));
    }
    SingleFile {
        language,
        std: std.to_string(),
        include_dirs,
    }
}

/// The newest C++ standard spelling this clang-tidy's clang knows
/// (c++20 from 10, c++2a from 5, c++1z from 3.5).
fn cpp_std(version: Option<crate::toolver::Version>) -> &'static str {
    match version {
        None => SINGLE_FILE_STD.trim_start_matches("-std="),
        Some(v) if v >= (10, 0) => SINGLE_FILE_STD.trim_start_matches("-std="),
        Some(v) if v >= (5, 0) => "c++2a",
        Some(v) if v >= (3, 5) => "c++1z",
        Some(_) => "c++14",
    }
}

/// `-std=c17` is known from clang 6; older ones get c11.
fn c_std(version: Option<crate::toolver::Version>) -> &'static str {
    if crate::toolver::at_least(version, (6, 0)) {
        SINGLE_FILE_STD_C.trim_start_matches("-std=")
    } else {
        "c11"
    }
}

/// The verdict beside [`tidy_body`]. A failed run with no finding never
/// checked anything; a failed run WITH findings still reports them.
fn tidy_result(
    findings: &[Finding],
    success: bool,
    code: Option<i32>,
    truncated: bool,
) -> check_result::CheckResult {
    if !success && findings.is_empty() {
        let status = code.map_or_else(|| "a signal".to_string(), |c| format!("exit status {c}"));
        return check_result::CheckResult::not_run(
            TOOL_NAME,
            check_result::NotRunReason::ToolError,
            &format!("clang-tidy itself failed ({status}), so nothing was checked."),
            "correct what clang-tidy said above (a check pattern, a compile error, the path), then call cpp_tidy again.",
        );
    }
    crate::results::findings_result(TOOL_NAME, findings, truncated)
}

/// The last `n` non-empty lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// The directory holding the compile database every C/C++ tool uses (the
/// shared search in `cpp_compile_db`), for `-p`.
fn find_compile_db(cwd: &Path) -> Option<PathBuf> {
    super::compile_db::find_compile_db(cwd).and_then(|db| db.parent().map(Path::to_path_buf))
}

/// Every finding in clang-tidy's stdout, each with its suggested fix when it
/// has one. clang-tidy prints a fix-it as the line after the caret
/// line (`           nullptr` under `^~~~`); reading only the diagnostic lines
/// would drop the "fix hints" the description promises.
fn parse_findings(stdout: &str) -> Vec<Finding> {
    let lines: Vec<&str> = stdout.lines().collect();
    let mut findings = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(mut finding) = parse_finding(line) else {
            continue;
        };
        // diagnostic, source line, caret line, [fix-it]
        let caret = lines
            .get(i + 2)
            .is_some_and(|l| l.trim_start().starts_with('^'));
        if caret
            && let Some(fix) = lines.get(i + 3)
            && !fix.trim().is_empty()
            && parse_finding(fix).is_none()
            && !fix.trim_start().starts_with('^')
        {
            finding.message = format!("{} — suggested fix: `{}`", finding.message, fix.trim());
        }
        findings.push(finding);
    }
    findings
}

/// Parse a single clang-tidy diagnostic line into a [`Finding`]. Lines that do
/// not match `file:line:col: <severity>: message [check]` (source context,
/// blank lines, summaries) return `None`.
fn parse_finding(line: &str) -> Option<Finding> {
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }

    // Peel off a trailing ` [check-name]` rule when present and well-formed.
    let (head, rule) = match line.rfind(" [") {
        Some(idx) if line.ends_with(']') => {
            let inner = &line[idx + 2..line.len() - 1];
            if !inner.is_empty() && inner.chars().all(is_rule_char) {
                (&line[..idx], Some(inner.to_string()))
            } else {
                (line, None)
            }
        }
        _ => (line, None),
    };

    // Find the `: <severity>: ` marker; the severity must be exactly one of the
    // recognized levels, otherwise this is not a diagnostic line.
    let (location, severity, message) = split_severity(head)?;

    // `file:line:column`
    let mut location_parts = location.rsplitn(3, ':');
    let column = location_parts.next()?.parse::<u32>().ok()?;
    let line_no = location_parts.next()?.parse::<u32>().ok()?;
    let file = location_parts.next()?;
    if file.is_empty() {
        return None;
    }

    Some(Finding {
        source: "clang-tidy".to_string(),
        severity,
        file: file.to_string(),
        line: line_no,
        column,
        message: message.trim().to_string(),
        rule,
    })
}

fn is_rule_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Split `head` on the first recognized `: <severity>: ` marker, returning
/// `(location, severity, message)`.
fn split_severity(head: &str) -> Option<(&str, Severity, &str)> {
    const MARKERS: [(&str, Severity); 3] = [
        (": error: ", Severity::Error),
        (": warning: ", Severity::Warning),
        (": note: ", Severity::Note),
    ];

    let mut best: Option<(usize, &str, Severity)> = None;
    for (marker, severity) in MARKERS {
        if let Some(idx) = head.find(marker) {
            if best.map(|(b, _, _)| idx < b).unwrap_or(true) {
                best = Some((idx, marker, severity));
            }
        }
    }

    let (idx, marker, severity) = best?;
    let location = &head[..idx];
    let message = &head[idx + marker.len()..];
    Some((location, severity, message))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    async fn tidy(dir: &Path, args: serde_json::Value) -> Result<String, FunctionCallError> {
        let tool = ClangTidyTool {
            cwd: dir.to_path_buf(),
        };
        crate::test_support::call(&tool, args).await
    }

    #[tokio::test]
    async fn a_failed_clang_tidy_is_never_no_findings() {
        // An unknown check pattern ("no checks enabled") and a nonexistent
        // file both read "No findings." — exit status and stderr ignored.
        if !crate::program_args::fixture::have("clang-tidy") {
            eprintln!("skip: clang-tidy not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.cpp"), "int main() { return 0; }\n").unwrap();
        let out = tidy(
            dir.path(),
            serde_json::json!({"path": "ok.cpp", "checks": "-*,bugprone-use-after-mov"}),
        )
        .await
        .expect("a report");
        assert!(
            out.contains("DID NOT RUN") && out.contains("NOT a clean result"),
            "{out}"
        );
        assert!(out.contains("no checks enabled"), "{out}");
        assert!(!out.contains("No findings"), "{out}");

        let err = tidy(dir.path(), serde_json::json!({"path": "nope.cpp"}))
            .await
            .expect_err("missing file");
        assert!(
            matches!(err, FunctionCallError::RespondToModel(_)),
            "{err:?}"
        );
        let err = tidy(dir.path(), serde_json::json!({"path": "."}))
            .await
            .expect_err("a directory is not a source file");
        assert!(
            matches!(err, FunctionCallError::RespondToModel(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_c_file_is_analyzed_as_c() {
        // Single-file mode once passed `-std=c++20` for a .c file; clang
        // rejects that, so valid C came back as compile errors.
        if !crate::program_args::fixture::have("clang-tidy") {
            eprintln!("skip: clang-tidy not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.c"),
            "#include <stdlib.h>\nint main(void) { char *p = malloc(4); free(p); return 0; }\n",
        )
        .unwrap();
        let out = tidy(dir.path(), serde_json::json!({"path": "ok.c"}))
            .await
            .expect("a report");
        assert!(out.contains("No findings"), "{out}");
        assert!(
            !out.contains("DID NOT RUN") && !out.contains("error"),
            "{out}"
        );
    }

    #[test]
    fn a_fix_it_reaches_the_model() {
        // Real clang-tidy 14 output; the replacement text under the
        // caret was dropped.
        let out = "/w/a.cpp:3:12: warning: use nullptr [modernize-use-nullptr]\n  int *p = NULL;\n           ^~~~\n           nullptr\n/w/a.cpp:9:3: warning: no fix here [bugprone-x]\n  f();\n  ^\n/w/a.cpp:10:3: warning: next [bugprone-y]\n  g();\n  ^\n";
        let findings = parse_findings(out);
        assert_eq!(findings.len(), 3);
        assert!(
            findings[0].message.ends_with("suggested fix: `nullptr`"),
            "{}",
            findings[0].message
        );
        // A diagnostic that follows a caret is not a fix-it.
        assert!(
            !findings[1].message.contains("suggested fix"),
            "{}",
            findings[1].message
        );
        assert!(
            !findings[2].message.contains("suggested fix"),
            "{}",
            findings[2].message
        );
    }

    #[test]
    fn a_failed_run_with_findings_keeps_them_and_names_the_exit() {
        let finding = parse_finding("/w/a.cpp:3:5: error: boom [clang-diagnostic-error]").unwrap();
        let body = tidy_body(
            &[finding],
            false,
            Some(1),
            "",
            "Error while processing /w/a.cpp.",
        );
        assert!(body.contains("boom"), "{body}");
        assert!(
            body.contains("exit status 1") && body.contains("Error while processing"),
            "{body}"
        );
        let clean = tidy_body(&[], true, Some(0), "", "");
        assert!(clean.starts_with("status: clean"), "{clean}");
        assert!(
            clean.ends_with(&render_findings(&[], FINDINGS_CAP)),
            "{clean}"
        );
    }

    #[test]
    fn parses_finding_with_rule() {
        let finding =
            parse_finding("/work/main.cpp:10:5: warning: use after move [bugprone-use-after-move]")
                .expect("should parse");
        assert_eq!(
            finding,
            Finding {
                source: "clang-tidy".to_string(),
                severity: Severity::Warning,
                file: "/work/main.cpp".to_string(),
                line: 10,
                column: 5,
                message: "use after move".to_string(),
                rule: Some("bugprone-use-after-move".to_string()),
            }
        );
    }

    #[test]
    fn parses_error_without_rule() {
        let finding = parse_finding("src/util.c:3:1: error: expected ';'").expect("should parse");
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.file, "src/util.c");
        assert_eq!(finding.line, 3);
        assert_eq!(finding.column, 1);
        assert_eq!(finding.message, "expected ';'");
        assert_eq!(finding.rule, None);
    }

    #[test]
    fn parses_note_with_dotted_rule() {
        let finding = parse_finding(
            "a.cpp:1:1: note: dereference here [clang-analyzer-core.NullDereference]",
        )
        .expect("should parse");
        assert_eq!(finding.severity, Severity::Note);
        assert_eq!(
            finding.rule.as_deref(),
            Some("clang-analyzer-core.NullDereference")
        );
    }

    #[test]
    fn ignores_non_diagnostic_lines() {
        assert_eq!(parse_finding("    int x = move(y);"), None);
        assert_eq!(parse_finding(""), None);
        assert_eq!(parse_finding("12345 warnings generated."), None);
        // Unknown severity is not a diagnostic line.
        assert_eq!(parse_finding("a.cpp:1:1: remark: vectorized loop"), None);
    }

    #[test]
    fn does_not_peel_mid_message_or_spaced_brackets() {
        // Bracket is not at end of line: not a rule, message kept intact.
        let mid =
            parse_finding("a.cpp:2:3: warning: array index [4] is bad").expect("should parse");
        assert_eq!(mid.rule, None);
        assert_eq!(mid.message, "array index [4] is bad");

        // Trailing bracket containing spaces is not a valid check name.
        let spaced =
            parse_finding("a.cpp:2:3: warning: thing happened [not a rule]").expect("should parse");
        assert_eq!(spaced.rule, None);
        assert_eq!(spaced.message, "thing happened [not a rule]");
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

    #[test]
    fn compile_db_missing_returns_none() {
        let cwd = Path::new("/definitely/not/a/real/workspace/xyzzy");
        assert_eq!(find_compile_db(cwd), None);
    }

    // ---- Robustness across clang-tidy versions and project layouts --------

    #[test]
    fn flags_follow_the_installed_version() {
        let t = Path::new("/w/a.cpp");
        // Current (or unrecognised) clang-tidy: unchanged apart from the
        // WarningsAsErrors override.
        for version in [None, Some((14, 0))] {
            let argv = tidy_argv(version, None, Some(Path::new("/w/build")), t, None);
            assert_eq!(
                argv,
                vec![
                    "--quiet",
                    "--warnings-as-errors=-*",
                    "-p",
                    "/w/build",
                    "/w/a.cpp"
                ],
                "{version:?}"
            );
        }
        // LLVM 4.x has no --quiet.
        let argv = tidy_argv(Some((4, 0)), None, None, t, None);
        assert!(!argv.contains(&"--quiet".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"--warnings-as-errors=-*".to_string()),
            "{argv:?}"
        );
        let argv = tidy_argv(Some((3, 7)), None, None, t, None);
        assert!(
            !argv.iter().any(|a| a.starts_with("--warnings-as-errors")),
            "{argv:?}"
        );
    }

    #[test]
    fn single_file_mode_adds_include_roots_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src/detail")).unwrap();
        std::fs::write(root.join("src/detail/a.cpp"), "").unwrap();
        let single = single_file_flags(root, &root.join("src/detail/a.cpp"), None);
        assert_eq!(single.language, None);
        assert_eq!(single.std, "c++20");
        assert_eq!(
            single.include_dirs,
            vec![
                root.join("src/detail"),
                root.to_path_buf(),
                root.join("include"),
                root.join("src")
            ]
        );
        let flags = single.flags();
        assert!(
            flags.contains(&format!("-I{}", root.join("include").display())),
            "{flags:?}"
        );
        assert!(
            single
                .describe()
                .starts_with("assumed (no compile_commands.json found)")
        );
        // Old clang: a standard spelling it knows.
        assert_eq!(
            single_file_flags(root, &root.join("src/detail/a.cpp"), Some((4, 0))).std,
            "c++1z"
        );
        assert_eq!(c_std(Some((5, 0))), "c11");
        assert_eq!(c_std(None), "c17");
    }

    #[test]
    fn a_header_in_a_cpp_project_is_parsed_as_cpp() {
        // clang parses `.h` as C: `-std=c++20` on one failed "invalid
        // argument '-std=c++20' not allowed with 'C'".
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.h"), "int f();\n").unwrap();
        std::fs::write(root.join("a.cpp"), "").unwrap();
        let single = single_file_flags(root, &root.join("a.h"), None);
        assert_eq!(single.language, Some("c++"));
        assert_eq!(&single.flags()[..3], ["-x", "c++", "-std=c++20"]);
        std::fs::remove_file(root.join("a.cpp")).unwrap();
        std::fs::write(root.join("a.c"), "").unwrap();
        let single = single_file_flags(root, &root.join("a.h"), None);
        assert_eq!((single.language, single.std.as_str()), (None, "c17"));
    }

    #[test]
    fn the_error_line_is_kept_and_unresolved_includes_are_named() {
        // The tail ("68 warnings and 1 error generated.") dropped the error.
        let body = tidy_body(
            &[],
            false,
            Some(1),
            "",
            "error: invalid argument '-std=c++20' not allowed with 'C' [clang-diagnostic-error]\n\
             68 warnings and 1 error generated.\nError while processing /w/a.h.\n",
        );
        assert!(body.starts_with("status: DID NOT RUN"), "{body}");
        assert!(body.contains("not allowed with 'C'"), "{body}");
        let missing =
            parse_finding("/w/a.cpp:1:10: error: 'x.h' file not found [clang-diagnostic-error]")
                .unwrap();
        let body = tidy_body(&[missing], false, Some(1), "", "1 error generated.");
        assert!(body.contains("analysis PARTIAL — 1 #include(s)"), "{body}");
        let body = tidy_body(
            &[],
            false,
            Some(1),
            "",
            "clang-tidy: Unknown command line argument '--quiet'.",
        );
        assert!(body.contains("does not accept an option"), "{body}");
    }

    #[tokio::test]
    async fn live_project_warnings_as_errors_is_not_a_failure() {
        if !crate::program_args::fixture::have("clang-tidy") {
            eprintln!("skip: clang-tidy not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".clang-tidy"),
            "Checks: '-*,clang-analyzer-*'\nWarningsAsErrors: '*'\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::write(root.join("include/util.h"), "int get(int *p);\n").unwrap();
        std::fs::write(
            root.join("main.cpp"),
            "#include \"util.h\"\nint get(int *p) { int x; *p = x; return *p; }\n",
        )
        .unwrap();
        let out = tidy(root, serde_json::json!({"path": "main.cpp"}))
            .await
            .expect("a report");
        assert!(
            !out.contains("file not found"),
            "include/ must be searched: {out}"
        );
        assert!(
            !out.contains("DID NOT RUN") && !out.contains("exited with"),
            "{out}"
        );
        assert!(out.contains("finding(s)"), "{out}");
    }
}
