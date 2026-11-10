use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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

use crate::run::RunOutcome;
use crate::run::run_capped;

const TOOL_NAME: &str = "cpp_format";
/// clang-format answers in well under a second; this only bounds a hang.
const FORMAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// `git diff` of one file; bounds a hung repository lock.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Most `-`/`+` lines a preview shows.
const DIFF_LINE_CAP: usize = 200;

/// Counter feeding [`temp_source_path`] so concurrent inline-format calls in the
/// same process never collide on a temp filename.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct FormatTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FormatArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    in_place: bool,
    #[serde(default)]
    whole_file: bool,
}

impl<'call> ToolExecutor<ToolCall<'call>> for FormatTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "path".to_string(),
            JsonSchema::string(Some(
                "C/C++ file to format, relative to the workspace or absolute. Mutually \
                 exclusive with `source`."
                    .to_string(),
            )),
        );
        properties.insert(
            "source".to_string(),
            JsonSchema::string(Some(
                "Inline C/C++ snippet to format. Returns the formatted code. Mutually \
                 exclusive with `path`."
                    .to_string(),
            )),
        );
        properties.insert(
            "style".to_string(),
            JsonSchema::string(Some(
                "clang-format style: empty (default) uses the project's .clang-format if \
                 present, else clang-format's built-in default. Accepts Google, LLVM, \
                 Mozilla, Chromium, WebKit, Microsoft, GNU, or file."
                    .to_string(),
            )),
        );
        properties.insert(
            "in_place".to_string(),
            JsonSchema::boolean(Some(
                "Only with `path`: when true, rewrite the file in place (default false, \
                 which returns a diff preview)."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Format C/C++ code with clang-format — in place, as a diff preview, or \
                format an inline snippet. Respects the project's .clang-format. Use to normalize \
                style after edits. C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                // Exactly one of `path`/`source` (checked in validate_inputs);
                // `path` alone was marked required, which a `source` call broke.
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

impl FormatTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: FormatArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        validate_inputs(&args.path, &args.source)?;
        let style = style_args(args.style.as_deref());

        if let Some(source) = &args.source {
            // The snippet is formatted from a temp file, where clang-format
            // never finds the project's .clang-format — so find it here.
            let style = if style.is_empty() {
                project_style_file(&self.cwd)
                    .map(|file| vec![format!("--style=file:{}", file.display())])
                    .unwrap_or_default()
            } else {
                style
            };
            return format_source(source, &style).await;
        }

        // `validate_inputs` guarantees `path` is set when `source` is not.
        let path = args.path.as_deref().unwrap_or_default();
        let target = resolve_path(&self.cwd, path);
        // A regular file only: reading a FIFO (or a device) to take the
        // "before" copy would block the tool forever.
        if !target.is_file() {
            let what = if target.exists() { "not a regular file" } else { "no such file" };
            return Err(FunctionCallError::RespondToModel(format!(
                "{what}: {} (pass a C/C++ source file). Nothing was formatted.",
                target.display()
            )));
        }
        // A project without a .clang-format does not format with clang-format:
        // reformatting its files would rewrite code it formats by hand.
        let style_file = project_style_file_within(&target, &self.cwd);
        if style.is_empty() && style_file.is_none() {
            return Ok(crate::report::report(
                TOOL_NAME,
                path,
                NO_STYLE_FILE.to_string(),
                check_result::CheckResult::not_run(
                    TOOL_NAME,
                    check_result::NotRunReason::Skipped,
                    "This project has no .clang-format, so nothing was formatted.",
                    "match the surrounding style by hand, or pass `style` to format anyway.",
                ),
            ));
        }
        let lines = if args.whole_file {
            LineScope::Whole("whole file, as requested")
        } else {
            changed_lines(&target).await
        };
        let ranges = match &lines {
            LineScope::Unchanged => {
                return Ok(crate::report::report(
                    TOOL_NAME,
                    path,
                    UNCHANGED.to_string(),
                    check_result::CheckResult::not_run(
                        TOOL_NAME,
                        check_result::NotRunReason::NoCodeChange,
                        "No line of this file changed since git HEAD, so nothing was formatted.",
                        "pass whole_file=true to format the entire file.",
                    ),
                ));
            }
            LineScope::Changed(ranges) => ranges.clone(),
            LineScope::Whole(_) => Vec::new(),
        };
        let mut argv = style;
        argv.extend(ranges.iter().map(|(first, last)| format!("--lines={first}:{last}")));
        let scope = describe_line_scope(&lines, style_file.as_deref(), &self.cwd);
        if args.in_place {
            format_in_place(&target, path, &argv, &scope).await
        } else {
            format_diff(&target, path, &argv, &scope).await
        }
    }
}

const NO_STYLE_FILE: &str = "status: skipped — this project has no .clang-format, so it is not \
formatted with clang-format and reformatting would rewrite code it formats by hand. Nothing was \
changed. Match the surrounding code's style by hand, or pass `style` to format anyway.";

const UNCHANGED: &str = "No lines of this file changed since git HEAD, so there is nothing to \
format. Pass whole_file=true to format the entire file.";

/// Which lines of a file to hand clang-format.
#[derive(Debug, PartialEq, Eq)]
enum LineScope {
    /// Every line, and why.
    Whole(&'static str),
    /// These 1-based, inclusive line ranges of the current file (what changed
    /// since git HEAD).
    Changed(Vec<(u32, u32)>),
    /// The file is tracked and identical to HEAD.
    Unchanged,
}

/// The lines of `file` that differ from git HEAD. A file git does not track is
/// new, so all of it is "changed"; outside a repository (or with no HEAD yet)
/// there is nothing to compare with and the whole file is used.
async fn changed_lines(file: &Path) -> LineScope {
    let dir = file.parent().unwrap_or(Path::new("."));
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // --literal-pathspecs: a file name is a name, never a glob.
    let git_args = |args: &[&str]| -> Vec<String> {
        std::iter::once("--literal-pathspecs")
            .chain(args.iter().copied())
            .chain(["--", name.as_str()])
            .map(str::to_string)
            .collect()
    };
    let diff_args = git_args(&["diff", "-U0", "--no-color", "--no-ext-diff", "HEAD"]);
    let diff = match run_capped("git", &diff_args, dir, GIT_TIMEOUT).await {
        Ok(RunOutcome::Finished {
            success: true,
            stdout,
            ..
        }) => stdout,
        _ => return LineScope::Whole("no git history to compare with"),
    };
    let ranges = parse_hunk_ranges(&diff);
    if !ranges.is_empty() {
        return LineScope::Changed(ranges);
    }
    if !diff.trim().is_empty() {
        // Only deletions: nothing left to format.
        return LineScope::Unchanged;
    }
    let tracked_args = git_args(&["ls-files"]);
    match run_capped("git", &tracked_args, dir, GIT_TIMEOUT).await {
        Ok(RunOutcome::Finished {
            success: true,
            stdout,
            ..
        }) if stdout.trim().is_empty() => LineScope::Whole("new file, not yet tracked by git"),
        _ => LineScope::Unchanged,
    }
}

/// The new-file side of every `@@ -a[,b] +c[,d] @@` hunk header, as inclusive
/// line ranges; a hunk that only deletes (`d` = 0) contributes nothing.
fn parse_hunk_ranges(diff: &str) -> Vec<(u32, u32)> {
    diff.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("@@ ")?;
            let new_side = rest.split_whitespace().find(|part| part.starts_with('+'))?;
            let mut parts = new_side[1..].splitn(2, ',');
            let start = parts.next()?.parse::<u32>().ok()?;
            let count = match parts.next() {
                Some(count) => count.parse::<u32>().ok()?,
                None => 1,
            };
            (count > 0).then(|| (start, start + count - 1))
        })
        .collect()
}

/// One line saying which lines were formatted, and with which style file.
fn describe_line_scope(lines: &LineScope, style_file: Option<&Path>, cwd: &Path) -> String {
    let which = match lines {
        LineScope::Changed(ranges) => format!(
            "lines changed since git HEAD ({})",
            ranges
                .iter()
                .map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LineScope::Whole(why) => format!("whole file ({why})"),
        LineScope::Unchanged => "nothing".to_string(),
    };
    match style_file {
        Some(file) => format!(
            "scope: {which}; style: {}\n",
            file.strip_prefix(cwd).unwrap_or(file).display()
        ),
        None => format!("scope: {which}\n"),
    }
}

/// The `.clang-format` that makes this a clang-format project: the nearest
/// one from the file's directory up to the workspace root (all the way up
/// when the file is outside the workspace). A stray one above the workspace,
/// e.g. in the home directory, does not count.
fn project_style_file_within(file: &Path, cwd: &Path) -> Option<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    let inside = dir.starts_with(cwd);
    dir.ancestors()
        .take_while(|d| !inside || d.starts_with(cwd))
        .flat_map(|d| [d.join(".clang-format"), d.join("_clang-format")])
        .find(|candidate| candidate.is_file())
}

/// Exactly one of `path`/`source` must be present.
fn validate_inputs(
    path: &Option<String>,
    source: &Option<String>,
) -> Result<(), FunctionCallError> {
    match (path, source) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(
            "provide exactly one of `path` or `source`, not both".to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "provide either `path` (a file) or `source` (inline code) to format".to_string(),
        )),
        _ => Ok(()),
    }
}

/// Build the clang-format `--style=` argument list. An empty/whitespace/absent
/// style passes NOTHING, so clang-format auto-uses a `.clang-format` if present
/// and otherwise falls back to its own default — matching what a developer sees
/// running `clang-format` by hand.
fn style_args(style: Option<&str>) -> Vec<String> {
    match style.map(str::trim) {
        Some(s) if !s.is_empty() => vec![format!("--style={s}")],
        _ => Vec::new(),
    }
}

/// The `.clang-format` (or `_clang-format`) clang-format itself would use for a
/// file in `dir`: the nearest one in `dir` or its parents.
fn project_style_file(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .flat_map(|d| [d.join(".clang-format"), d.join("_clang-format")])
        .find(|file| file.is_file())
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Unique temp path for an inline-format request: process id + a monotonic
/// counter + nanosecond clock, with a `.cpp` extension so clang-format treats it
/// as C++.
fn temp_source_path() -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "byteask-cpp-format-{}-{}-{}.cpp",
        std::process::id(),
        n,
        nanos
    ))
}

/// What one clang-format run produced.
struct FormatRun {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run clang-format through `run_capped`: a wall-clock cap and a process
/// group where a raw `.output()` could block forever, and a spawn failure that
/// goes back to the model instead of being `Fatal` and ending the turn.
async fn run_clang_format(
    style: &[String],
    file: &Path,
    in_place: bool,
) -> Result<FormatRun, FunctionCallError> {
    let mut argv = style.to_vec();
    if in_place {
        argv.push("-i".to_string());
    }
    argv.push(file.display().to_string());
    let dir = file.parent().unwrap_or(Path::new("."));
    match run_capped("clang-format", &argv, dir, FORMAT_TIMEOUT).await? {
        RunOutcome::Finished {
            success,
            stdout,
            stderr,
            ..
        } => Ok(FormatRun {
            success,
            stdout,
            stderr,
        }),
        RunOutcome::TimedOut { secs } => Err(FunctionCallError::RespondToModel(format!(
            "clang-format did not finish within {secs}s and was stopped; nothing was formatted."
        ))),
    }
}

async fn format_source(
    source: &str,
    style: &[String],
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let temp = temp_source_path();
    // Not `Fatal`: an unwritable temp dir must not end the user's turn.
    std::fs::write(&temp, source).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "could not write the snippet to a temporary file ({err}); nothing was formatted."
        ))
    })?;
    let result = run_clang_format(style, &temp, false).await;
    let _ = std::fs::remove_file(&temp);
    let output = result?;
    if !output.success {
        return Err(FunctionCallError::RespondToModel(format!(
            "clang-format failed: {}",
            output.stderr.trim()
        )));
    }
    let formatted = output.stdout;
    Ok(crate::report::report_plain(
        TOOL_NAME,
        formatted,
        check_result::CheckResult::pass(TOOL_NAME, "clang-format formatted the snippet."),
    ))
}

async fn format_diff(
    target: &Path,
    label: &str,
    style: &[String],
    scope: &str,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let original = std::fs::read_to_string(target).map_err(|err| {
        FunctionCallError::RespondToModel(format!("cannot read {}: {err}", target.display()))
    })?;
    let output = run_clang_format(style, target, false).await?;
    if !output.success {
        return Err(FunctionCallError::RespondToModel(format!(
            "clang-format failed: {}",
            output.stderr.trim()
        )));
    }
    let formatted = output.stdout;
    // A preview is a format check: changes to make means "not formatted".
    let result = if original == formatted {
        check_result::CheckResult::pass(TOOL_NAME, "Already formatted; clang-format would change nothing.")
    } else {
        check_result::CheckResult::fail(TOOL_NAME, "Not formatted; clang-format would change it (diff above).")
    };
    Ok(crate::report::report(
        TOOL_NAME,
        label,
        format!("{scope}{}", render_diff(&original, &formatted)),
        result,
    ))
}

async fn format_in_place(
    target: &Path,
    label: &str,
    style: &[String],
    scope: &str,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let before = std::fs::read_to_string(target).ok();
    let output = run_clang_format(style, target, true).await?;
    if !output.success {
        return Err(FunctionCallError::RespondToModel(format!(
            "clang-format failed: {}",
            output.stderr.trim()
        )));
    }
    let after = std::fs::read_to_string(target).ok();
    let note = match (before, after) {
        (Some(b), Some(a)) if b == a => " (no changes)",
        _ => "",
    };
    Ok(crate::report::report(
        TOOL_NAME,
        label,
        format!("{scope}Formatted {label} in place{note}"),
        check_result::CheckResult::pass(TOOL_NAME, "clang-format formatted the file in place."),
    ))
}

/// Render a clang-format preview as a hand-rolled line diff (no diff crate):
/// the lines both versions share at the start and at the end are dropped, and
/// the region between them is shown as `-old`/`+new` lines — paired by
/// position when the region keeps its length, so unchanged lines inside it are
/// not repeated. Identical content reports "no changes". Only the changed
/// region is shown, never the whole formatted file: with clang-format limited
/// to the changed lines that region is small, and a full copy of a large file
/// was most of what this tool used to return.
fn render_diff(original: &str, formatted: &str) -> String {
    if original == formatted {
        return "No changes — already formatted.".to_string();
    }
    let orig_lines: Vec<&str> = original.lines().collect();
    let fmt_lines: Vec<&str> = formatted.lines().collect();
    let prefix = orig_lines
        .iter()
        .zip(&fmt_lines)
        .take_while(|(o, f)| o == f)
        .count();
    let room = orig_lines.len().min(fmt_lines.len()) - prefix;
    let suffix = orig_lines
        .iter()
        .rev()
        .zip(fmt_lines.iter().rev())
        .take(room)
        .take_while(|(o, f)| o == f)
        .count();
    let old = &orig_lines[prefix..orig_lines.len() - suffix];
    let new = &fmt_lines[prefix..fmt_lines.len() - suffix];

    let mut rows: Vec<String> = Vec::new();
    let mut changed = 0usize;
    if old.len() == new.len() {
        for (o, f) in old.iter().zip(new) {
            if o != f {
                changed += 1;
                rows.push(format!("-{o}"));
                rows.push(format!("+{f}"));
            }
        }
    } else {
        changed = old.len().max(new.len());
        rows.extend(old.iter().map(|o| format!("-{o}")));
        rows.extend(new.iter().map(|f| format!("+{f}")));
    }
    let mut diff = String::new();
    for row in rows.iter().take(DIFF_LINE_CAP) {
        diff.push_str(row);
        diff.push('\n');
    }
    if rows.len() > DIFF_LINE_CAP {
        diff.push_str(&format!("… {} more diff line(s) omitted\n", rows.len() - DIFF_LINE_CAP));
    }
    format!(
        "{changed} line(s) would change ({} → {} lines), from line {}.\n\n--- diff ---\n{diff}",
        orig_lines.len(),
        fmt_lines.len(),
        prefix + 1
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn an_inline_snippet_follows_the_projects_clang_format() {
        // The snippet goes through a temp file under /tmp, where the
        // project's .clang-format is never found unless it is looked up here.
        if !crate::program_args::fixture::have("clang-format") {
            eprintln!("skip: clang-format not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".clang-format"), "BasedOnStyle: GNU\n").unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let tool = FormatTool { cwd: sub };
        let out = crate::test_support::call(
            &tool,
            serde_json::json!({"source": "int f(int a){if(a){return 1;}return 0;}"}),
        )
        .await
        .expect("formatted");
        // GNU style: the return type on its own line, a space before `(`.
        assert!(out.contains("int\nf (int a)"), "{out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_instead_of_blocking_forever() {
        // format_diff reads the file first; a FIFO would block that read — and
        // the user's turn — forever.
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::make_fifo(&dir.path().join("pipe.cpp"));
        let tool = FormatTool {
            cwd: dir.path().to_path_buf(),
        };
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::test_support::call(&tool, serde_json::json!({"path": "pipe.cpp"})),
        )
        .await
        .expect("must not block on a FIFO");
        let err = out.expect_err("a FIFO is not a source file");
        assert!(matches!(err, FunctionCallError::RespondToModel(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_project_without_clang_format_is_skipped_not_reformatted() {
        // A whole-file rewrite was proposed for projects that never used
        // clang-format; nothing may change there unless a style is passed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.cpp"), "int  f( ){return 1;}\n").unwrap();
        let tool = FormatTool {
            cwd: dir.path().to_path_buf(),
        };
        let out = crate::test_support::call(
            &tool,
            serde_json::json!({"path": "a.cpp", "in_place": true}),
        )
        .await
        .expect("a report");
        assert!(out.contains("status: skipped"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.cpp")).unwrap(),
            "int  f( ){return 1;}\n"
        );
    }

    #[test]
    fn a_style_file_above_the_workspace_does_not_count() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join(".clang-format"), "BasedOnStyle: LLVM\n").unwrap();
        let ws = outer.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let file = ws.join("src/a.cpp");
        assert_eq!(project_style_file_within(&file, &ws), None);
        std::fs::write(ws.join(".clang-format"), "BasedOnStyle: LLVM\n").unwrap();
        assert_eq!(project_style_file_within(&file, &ws), Some(ws.join(".clang-format")));
    }

    #[test]
    fn hunk_headers_become_new_side_line_ranges() {
        let diff = "diff --git a/a.cpp b/a.cpp\n--- a/a.cpp\n+++ b/a.cpp\n\
                    @@ -1,2 +1,3 @@ int f()\n+x\n@@ -10 +11 @@\n-y\n+z\n@@ -20,2 +22,0 @@\n-gone\n";
        assert_eq!(parse_hunk_ranges(diff), vec![(1, 3), (11, 11)]);
        assert_eq!(parse_hunk_ranges(""), Vec::<(u32, u32)>::new());
        let line = describe_line_scope(
            &LineScope::Changed(vec![(1, 3), (11, 11)]),
            Some(Path::new("/w/.clang-format")),
            Path::new("/w"),
        );
        assert_eq!(line, "scope: lines changed since git HEAD (1-3, 11); style: .clang-format\n");
    }

    #[test]
    fn render_diff_shows_only_the_changed_region() {
        let original = "a;\nb;\nint  x;\nc;\nd;\n";
        let formatted = "a;\nb;\nint x;\nc;\nd;\n";
        let out = render_diff(original, formatted);
        assert!(out.contains("1 line(s) would change (5 → 5 lines), from line 3."), "{out}");
        assert!(out.contains("-int  x;\n+int x;\n"), "{out}");
        assert!(!out.contains("-a;") && !out.contains("+d;"), "{out}");
    }

    #[tokio::test]
    async fn live_only_the_lines_changed_since_head_are_formatted() {
        use std::process::Command;
        if !crate::program_args::fixture::have("clang-format")
            || !crate::program_args::fixture::have("git")
        {
            eprintln!("skip: clang-format or git not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(root.join(".clang-format"), "BasedOnStyle: LLVM\n").unwrap();
        // Hand-formatted code the project keeps as it is.
        std::fs::write(root.join("a.cpp"), "int  keep( ){return 1;}\nint g() { return 2; }\n")
            .unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::write(root.join("a.cpp"), "int  keep( ){return 1;}\nint g( ){return  3;}\n")
            .unwrap();
        let tool = FormatTool {
            cwd: root.to_path_buf(),
        };
        let out = crate::test_support::call(&tool, serde_json::json!({"path": "a.cpp"}))
            .await
            .expect("a preview");
        assert!(out.contains("scope: lines changed since git HEAD (2)"), "{out}");
        assert!(out.contains("+int g() { return 3; }"), "{out}");
        assert!(!out.contains("-int  keep"), "an untouched line was reformatted: {out}");
    }

    fn opt(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn validate_rejects_neither() {
        assert!(validate_inputs(&None, &None).is_err());
    }

    #[test]
    fn validate_rejects_both() {
        assert!(validate_inputs(&opt("a.cpp"), &opt("int x;")).is_err());
    }

    #[test]
    fn validate_accepts_path_only() {
        assert!(validate_inputs(&opt("a.cpp"), &None).is_ok());
    }

    #[test]
    fn validate_accepts_source_only() {
        assert!(validate_inputs(&None, &opt("int x;")).is_ok());
    }

    #[test]
    fn style_args_empty_or_absent_pass_nothing() {
        assert_eq!(style_args(None), Vec::<String>::new());
        assert_eq!(style_args(Some("")), Vec::<String>::new());
        assert_eq!(style_args(Some("   ")), Vec::<String>::new());
    }

    #[test]
    fn style_args_known_styles_build_flag() {
        assert_eq!(style_args(Some("Google")), vec!["--style=Google".to_string()]);
        assert_eq!(style_args(Some("LLVM")), vec!["--style=LLVM".to_string()]);
        assert_eq!(style_args(Some("file")), vec!["--style=file".to_string()]);
    }

    #[test]
    fn style_args_trims_and_passes_unknown_through() {
        // Unknown styles are forwarded verbatim; clang-format validates them.
        assert_eq!(style_args(Some("  Mozilla ")), vec!["--style=Mozilla".to_string()]);
        assert_eq!(style_args(Some("Frobnicate")), vec!["--style=Frobnicate".to_string()]);
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "src/a.cpp"), PathBuf::from("/work/src/a.cpp"));
        assert_eq!(resolve_path(cwd, "/abs/b.cpp"), PathBuf::from("/abs/b.cpp"));
    }

    #[test]
    fn temp_paths_are_unique_and_cpp() {
        let a = temp_source_path();
        let b = temp_source_path();
        assert_ne!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("cpp"));
        let name = a.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.contains(&std::process::id().to_string()));
    }

    #[test]
    fn render_diff_reports_no_change_when_equal() {
        assert_eq!(render_diff("int x;\n", "int x;\n"), "No changes — already formatted.");
    }

    #[test]
    fn render_diff_empty_inputs_are_unchanged() {
        assert_eq!(render_diff("", ""), "No changes — already formatted.");
    }

    #[test]
    fn render_diff_marks_changed_lines() {
        let out = render_diff("int  x;\n", "int x;\n");
        assert!(out.contains("1 line(s) would change"));
        assert!(out.contains("-int  x;"));
        assert!(out.contains("+int x;"));
        // The preview is the delta only, not a second copy of the file.
        assert!(!out.contains("--- formatted ---"));
    }

    #[test]
    fn render_diff_counts_added_tail_lines() {
        // Formatter split one line into two: position 0 differs, position 1 is added.
        let out = render_diff("a;b;\n", "a;\nb;\n");
        assert!(out.contains("2 line(s) would change (1 → 2 lines)"));
        assert!(out.contains("+a;"));
        assert!(out.contains("+b;"));
    }

    #[test]
    fn render_diff_counts_removed_tail_lines() {
        let out = render_diff("a;\nb;\n", "a;b;\n");
        assert!(out.contains("(2 → 1 lines)"));
        assert!(out.contains("-b;"));
    }
}
