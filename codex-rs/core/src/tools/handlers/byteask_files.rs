//! ByteAsk native `read_file` and `search`.
//!
//! Why they exist: 84.5% of model responses issue ONE tool call, and every call re-reads
//! the whole context. One `read_file` call reads N ranges; one `search` call returns the
//! matches WITH context, which removes the read that usually follows a search.
//!
//! Permission contract: every byte goes through the sandbox-aware filesystem layer, exactly
//! as `view_image` does (`get_filesystem()` + `sandbox_context`). These tools can read what
//! a sandboxed `cat` could read and nothing more. The one exception is a `spill:<id>`
//! handle, which names a file the ENGINE wrote (the full original of an output it
//! reduced) inside its own spill directory.
//!
//! Both are registered only when the server-driven policy turns them on.

use std::collections::BTreeMap;

use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::WalkEntryKind;
use codex_exec_server::WalkOptions;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub const READ_FILE_TOOL_NAME: &str = "read_file";
pub const SEARCH_TOOL_NAME: &str = "search";

/// Files larger than this are refused rather than loaded whole into memory.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TARGETS: usize = 16;
const DEFAULT_LIMIT: usize = 400;
/// Directories a search never descends into.
const PRUNED_DIRS: [&str; 9] = [
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
];
const SEARCH_MAX_FILE_BYTES: u64 = 1024 * 1024;
const SEARCH_DEFAULT_RESULTS: usize = 100;
const SEARCH_MAX_RESULTS: usize = 500;
/// Bounds of a directory search. Every bound must stay within the filesystem layer's own
/// maximums (`codex_file_system::MAX_WALK_*`): that layer REJECTS an oversized request
/// before walking anything. Asking for 60,000 entries (the cap is 50,000) made every
/// directory search in 0.1.13 and 0.1.14 fail with "filesystem walk limits exceed
/// maximums"; the model fell back to `rg`, so nothing looked broken.
const SEARCH_WALK: WalkOptions = WalkOptions {
    max_depth: 24,
    max_directories: 5_000,
    max_entries: codex_file_system::MAX_WALK_ENTRIES,
    follow_directory_symlinks: false,
    prune_hidden_directories: true,
};

fn byte_budget(invocation_budget: TruncationPolicy, key: &str) -> usize {
    let upstream = invocation_budget.byte_budget();
    byteask_output_reducers::policy::active()
        .budgets
        .get(key)
        .map(|tokens| TruncationPolicy::Tokens(*tokens).byte_budget())
        .map_or(upstream, |ours| ours.min(upstream))
}

// ---- Re-read stub ------------------------------------------------------------------
// 41% of file-read characters in the trial logs were RE-reads. When the same range of an
// unchanged file was returned moments ago and is still in the model's context, one line
// replaces the text. "Still in context" is the hard part: a compaction removes the earlier
// copy, so every compaction bumps an epoch and entries from an older epoch never match.

static BYTEASK_COMPACTION_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Called whenever history is compacted: earlier reads are no longer in context.
pub(crate) fn note_compaction() {
    BYTEASK_COMPACTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

const REREAD_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Clone, PartialEq, Eq, Hash)]
struct ReadKey {
    thread: String,
    path: String,
    offset: usize,
    limit: usize,
}

struct ReadSeen {
    modified_at_ms: i64,
    size: u64,
    epoch: u64,
    at: std::time::Instant,
}

#[derive(Default)]
pub(crate) struct ReadMemory(std::collections::HashMap<ReadKey, ReadSeen>);

impl ReadMemory {
    /// Returns how long ago this exact range was returned, when the file is unchanged and
    /// no compaction happened since. Records the read either way.
    pub(crate) fn seen(
        &mut self,
        thread: &str,
        path: &str,
        offset: usize,
        limit: usize,
        modified_at_ms: i64,
        size: u64,
        now: std::time::Instant,
        epoch: u64,
    ) -> Option<std::time::Duration> {
        if self.0.len() > 4_096 {
            self.0.clear();
        }
        let key = ReadKey {
            thread: thread.to_string(),
            path: path.to_string(),
            offset,
            limit,
        };
        let hit = self.0.get(&key).and_then(|prev| {
            let age = now.saturating_duration_since(prev.at);
            (prev.modified_at_ms == modified_at_ms
                && prev.size == size
                && prev.epoch == epoch
                && age <= REREAD_WINDOW)
                .then_some(age)
        });
        if hit.is_none() {
            // Only a FULL answer refreshes the entry: a stub must not keep itself alive.
            self.0.insert(
                key,
                ReadSeen {
                    modified_at_ms,
                    size,
                    epoch,
                    at: now,
                },
            );
        }
        hit
    }
}

fn read_memory() -> &'static std::sync::Mutex<ReadMemory> {
    static MEMORY: std::sync::OnceLock<std::sync::Mutex<ReadMemory>> = std::sync::OnceLock::new();
    MEMORY.get_or_init(Default::default)
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

// ---------------------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReadTarget {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ReadFileArgs {
    targets: Vec<ReadTarget>,
    #[serde(default)]
    environment_id: Option<String>,
}

/// Render lines `offset..offset+limit` (1-based `offset`) of `text` inside `budget` bytes.
/// Whole lines only. The header states the range; a footer states what is left and how to
/// get it, so a cut is never silent.
pub(crate) fn render_range(
    shown_path: &str,
    text: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    budget: usize,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.unwrap_or(1).max(1);
    if total == 0 {
        return format!("== {shown_path} (empty file) ==\n");
    }
    if start > total {
        return format!(
            "== {shown_path} ==\n[byteask: offset {start} is past the end; the file has {total} lines]\n"
        );
    }
    let want_end = start
        .saturating_add(limit.unwrap_or(DEFAULT_LIMIT))
        .saturating_sub(1)
        .min(total);
    let mut body = String::new();
    let mut end = start - 1;
    for line in &lines[start - 1..want_end] {
        if body.len() + line.len() + 1 > budget && end >= start {
            break;
        }
        // One very long line still has to be shown: cut it on a char boundary.
        if line.len() + 1 > budget {
            let mut cut = budget.saturating_sub(1).min(line.len());
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            body.push_str(&line[..cut]);
            body.push('\n');
            end += 1;
            break;
        }
        body.push_str(line);
        body.push('\n');
        end += 1;
    }
    let mut out = format!("== {shown_path} (lines {start}-{end} of {total}) ==\n{body}");
    if end < total {
        out.push_str(&format!(
            "[byteask: lines {}-{total} of {shown_path} not shown; continue with offset={}]\n",
            end + 1,
            end + 1
        ));
    }
    out
}

pub struct ReadFileHandler {
    include_environment_id: bool,
}

impl ReadFileHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_FILE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let target = JsonSchema::object(
            BTreeMap::from([
                (
                    "path".to_string(),
                    JsonSchema::string(Some(
                        "File to read, absolute or relative to the working directory. A `spill:<id>` handle from a `[byteask: ...]` footer reads the full saved output of an earlier command.".to_string(),
                    )),
                ),
                (
                    "offset".to_string(),
                    JsonSchema::number(Some("1-based line to start at. Default 1.".to_string())),
                ),
                (
                    "limit".to_string(),
                    JsonSchema::number(Some(format!(
                        "Maximum number of lines to return. Default {DEFAULT_LIMIT}."
                    ))),
                ),
            ]),
            Some(vec!["path".to_string()]),
            Some(false.into()),
        );
        let mut properties = BTreeMap::from([(
            "targets".to_string(),
            JsonSchema::array(
                target,
                Some(format!(
                    "One entry per range to read, up to {MAX_TARGETS}. Put every file and range you need into ONE call instead of calling once per file."
                )),
            ),
        )]);
        if self.include_environment_id {
            properties.insert(
                "environment_id".to_string(),
                JsonSchema::string(Some(
                    "Environment id from <environment_context>. Omit to use the primary environment.".to_string(),
                )),
            );
        }
        ToolSpec::Function(ResponsesApiTool {
            name: READ_FILE_TOOL_NAME.to_string(),
            description: "Read line ranges from one or more text files in a single call. Prefer this over `cat`, `sed -n` or `head` in the shell: it reads several files or several ranges at once, returns exact file text with a header naming each range, and tells you how to continue when a range was cut. Each target is independent: if one fails, its error is reported and the others are still returned. Binary files are refused with their size. Output is the file's text exactly as stored, safe to use as context for apply_patch.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(properties, Some(vec!["targets".to_string()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ReadFileHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            payload,
            ..
        } = invocation;
        let thread = format!("{:?}", session.thread_id);
        let reread_stub = byteask_output_reducers::policy::active().flag("reread_stub");
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "read_file handler received unsupported payload".to_string(),
            ));
        };
        let ReadFileArgs {
            targets,
            environment_id,
        } = parse_arguments(&arguments)?;
        if targets.is_empty() || targets.len() > MAX_TARGETS {
            return Err(FunctionCallError::RespondToModel(format!(
                "read_file needs between 1 and {MAX_TARGETS} targets, got {}",
                targets.len()
            )));
        }
        let turn_environment =
            resolve_tool_environment(&step_context.environments, environment_id.as_deref())?;
        let sandbox = turn_environment.sandbox_context(/*additional_permissions*/ None);
        let fs = turn_environment.environment.get_filesystem();
        let total_budget = byte_budget(turn.model_info().truncation_policy.into(), "read");
        // The sum of all targets stays inside the budget: an even share each.
        let share = (total_budget / targets.len()).max(512);

        let mut out = String::new();
        let mut any_ok = false;
        for target in &targets {
            let mut unchanged_for: Option<std::time::Duration> = None;
            let bytes: Result<Vec<u8>, String> = if target.path.starts_with("spill:") {
                match byteask_output_reducers::spill::resolve(&target.path) {
                    Some(path) => std::fs::read(&path).map_err(|e| e.to_string()),
                    None => Err("no saved output with that handle".to_string()),
                }
            } else {
                match turn_environment.cwd().join(&target.path) {
                    Err(err) => Err(format!("cannot resolve path: {err}")),
                    Ok(uri) => match fs
                        .get_metadata(&uri, GetMetadataOptions::default(), Some(&sandbox))
                        .await
                    {
                        Err(err) => Err(err.to_string()),
                        Ok(meta) if !meta.is_file => Err("not a file".to_string()),
                        Ok(meta) if meta.size > MAX_FILE_BYTES => Err(format!(
                            "file is {} bytes, larger than the {MAX_FILE_BYTES}-byte limit; use the shell to inspect it",
                            meta.size
                        )),
                        Ok(meta) => {
                            if reread_stub && let Ok(mut memory) = read_memory().lock() {
                                unchanged_for = memory.seen(
                                    &thread,
                                    &target.path,
                                    target.offset.unwrap_or(1),
                                    target.limit.unwrap_or(DEFAULT_LIMIT),
                                    meta.modified_at_ms,
                                    meta.size,
                                    std::time::Instant::now(),
                                    BYTEASK_COMPACTION_EPOCH
                                        .load(std::sync::atomic::Ordering::Relaxed),
                                );
                            }
                            if unchanged_for.is_some() {
                                Ok(Vec::new())
                            } else {
                                fs.read_file(&uri, ReadFileOptions::default(), Some(&sandbox))
                                    .await
                                    .map_err(|e| e.to_string())
                            }
                        }
                    },
                }
            };
            if let Some(age) = unchanged_for {
                any_ok = true;
                out.push_str(&format!(
                    "== {} ==\n[byteask: unchanged since you read this same range {} s ago; that text is still in your context above. Ask for a different range if you need other lines.]\n\n",
                    target.path,
                    age.as_secs()
                ));
                continue;
            }
            match bytes {
                Err(err) => out.push_str(&format!("== {} ==\n[error: {err}]\n", target.path)),
                Ok(bytes) if looks_binary(&bytes) => out.push_str(&format!(
                    "== {} ==\n[binary file, {} bytes: not shown]\n",
                    target.path,
                    bytes.len()
                )),
                Ok(bytes) => {
                    any_ok = true;
                    let text = String::from_utf8_lossy(&bytes);
                    out.push_str(&render_range(
                        &target.path,
                        &text,
                        target.offset,
                        target.limit,
                        share,
                    ));
                }
            }
            out.push('\n');
        }
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            out,
            Some(any_ok),
        )))
    }
}

impl CoreToolRuntime for ReadFileHandler {}

// ---------------------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    environment_id: Option<String>,
}

/// `*` matches within a path segment, `**` across segments, `?` one character. A pattern
/// without `/` is matched against the file name only, like ripgrep's `-g`.
pub(crate) fn glob_match(pattern: &str, rel_path: &str) -> bool {
    fn inner(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => {
                let rest = &rest[1..];
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=s.len()).any(|i| inner(rest, &s[i..]))
            }
            Some((b'*', rest)) => (0..=s.len())
                .take_while(|i| *i == 0 || s[i - 1] != b'/')
                .any(|i| inner(rest, &s[i..])),
            Some((b'?', rest)) => s.first().is_some_and(|c| *c != b'/') && inner(rest, &s[1..]),
            Some((c, rest)) => s.first() == Some(c) && inner(rest, &s[1..]),
        }
    }
    let subject = if pattern.contains('/') {
        rel_path
    } else {
        rel_path.rsplit('/').next().unwrap_or(rel_path)
    };
    inner(pattern.as_bytes(), subject.as_bytes())
}

/// Matches in one file, ripgrep `--heading` layout: `N:text` for a match, `N-text` for a
/// context line, `--` between non-adjacent groups. Returns the block and the match count.
pub(crate) fn search_text(
    text: &str,
    re: &regex_lite::Regex,
    context: usize,
    remaining: usize,
) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| re.is_match(l))
        .map(|(i, _)| i)
        .take(remaining)
        .collect();
    if hits.is_empty() {
        return (String::new(), 0);
    }
    let mut out = String::new();
    let mut last_printed: Option<usize> = None;
    for hit in &hits {
        let lo = hit.saturating_sub(context);
        let hi = (hit + context).min(lines.len() - 1);
        for idx in lo..=hi {
            if last_printed.is_some_and(|l| idx <= l) {
                continue;
            }
            if let Some(l) = last_printed
                && idx > l + 1
                && context > 0
            {
                out.push_str("--\n");
            }
            let sep = if hits.binary_search(&idx).is_ok() {
                ':'
            } else {
                '-'
            };
            let line = lines[idx];
            // A minified line would swamp the result.
            let mut cut = line.len().min(400);
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&format!("{}{sep}{}", idx + 1, &line[..cut]));
            if cut < line.len() {
                out.push_str(&format!(" [byteask: line cut, {} bytes]", line.len()));
            }
            out.push('\n');
            last_printed = Some(idx);
        }
    }
    (out, hits.len())
}

pub struct SearchHandler {
    include_environment_id: bool,
}

impl SearchHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

impl ToolExecutor<ToolInvocation> for SearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::from([
            (
                "pattern".to_string(),
                JsonSchema::string(Some(
                    "Regular expression to search for, matched line by line. Escape regex metacharacters to search for literal text.".to_string(),
                )),
            ),
            (
                "path".to_string(),
                JsonSchema::string(Some(
                    "File or directory to search, absolute or relative to the working directory. Default: the working directory. A `spill:<id>` handle searches the full saved output of an earlier command.".to_string(),
                )),
            ),
            (
                "glob".to_string(),
                JsonSchema::string(Some(
                    "Only search files matching this glob, for example `*.rs` or `src/**/*.ts`. A glob without `/` matches the file name.".to_string(),
                )),
            ),
            (
                "context".to_string(),
                JsonSchema::number(Some(
                    "Lines of context to show before and after each match (0-10). Use it instead of a follow-up read_file when you need to see the code around a match. Default 0.".to_string(),
                )),
            ),
            (
                "files_only".to_string(),
                JsonSchema::boolean(Some(
                    "Return only the paths of matching files with their match counts. Default false.".to_string(),
                )),
            ),
            (
                "case_insensitive".to_string(),
                JsonSchema::boolean(Some("Match case-insensitively. Default false.".to_string())),
            ),
            (
                "max_results".to_string(),
                JsonSchema::number(Some(format!(
                    "Maximum number of matching lines to return. Default {SEARCH_DEFAULT_RESULTS}, maximum {SEARCH_MAX_RESULTS}."
                ))),
            ),
        ]);
        if self.include_environment_id {
            properties.insert(
                "environment_id".to_string(),
                JsonSchema::string(Some(
                    "Environment id from <environment_context>. Omit to use the primary environment.".to_string(),
                )),
            );
        }
        ToolSpec::Function(ResponsesApiTool {
            name: SEARCH_TOOL_NAME.to_string(),
            description: "Search file contents with a regular expression. Results are grouped by file: the path once, then `line:text` for each match and `line-text` for context lines. Hidden directories and dependency or build directories (.git, node_modules, target, build, dist, virtualenvs) are skipped, as are binary files and files over 1 MB. Set `context` to see the surrounding code in the same call, or `files_only` to list matching files. The footer says when results were cut and how to narrow the search.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(properties, Some(vec!["pattern".to_string()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl SearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            turn,
            step_context,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "search handler received unsupported payload".to_string(),
            ));
        };
        let args: SearchArgs = parse_arguments(&arguments)?;
        let pattern = if args.case_insensitive {
            format!("(?i){}", args.pattern)
        } else {
            args.pattern.clone()
        };
        let re = regex_lite::Regex::new(&pattern).map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid regular expression: {err}"))
        })?;
        let context = args.context.unwrap_or(0).min(10);
        let max_results = args
            .max_results
            .unwrap_or(SEARCH_DEFAULT_RESULTS)
            .clamp(1, SEARCH_MAX_RESULTS);
        let budget = byte_budget(turn.model_info().truncation_policy.into(), "search");

        // A saved command output: search it directly.
        if let Some(handle) = args.path.as_deref().filter(|p| p.starts_with("spill:")) {
            let Some(path) = byteask_output_reducers::spill::resolve(handle) else {
                return Err(FunctionCallError::RespondToModel(
                    "no saved output with that handle".to_string(),
                ));
            };
            let bytes = std::fs::read(&path)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let text = String::from_utf8_lossy(&bytes);
            let (block, n) = search_text(&text, &re, context, max_results);
            let body = if n == 0 {
                "no matches\n".to_string()
            } else {
                format!("{handle}\n{block}")
            };
            return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                finish_search(body, n, max_results, budget),
                Some(true),
            )));
        }

        let turn_environment =
            resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?;
        let sandbox = turn_environment.sandbox_context(/*additional_permissions*/ None);
        let fs = turn_environment.environment.get_filesystem();
        let cwd = turn_environment.cwd();
        let root = cwd
            .join(args.path.as_deref().unwrap_or("."))
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("cannot resolve search path: {err}"))
            })?;
        let root_native = root.inferred_native_path_string();
        let meta = fs
            .get_metadata(&root, GetMetadataOptions::default(), Some(&sandbox))
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("cannot search `{root_native}`: {err}"))
            })?;

        let mut files = Vec::new();
        let mut walk_truncated = false;
        if meta.is_file {
            files.push(root.clone());
        } else {
            let outcome = fs
                .walk(&root, SEARCH_WALK, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("cannot walk `{root_native}`: {err}"))
                })?;
            walk_truncated = outcome.truncated;
            for entry in outcome.entries {
                if entry.kind == WalkEntryKind::File {
                    files.push(entry.path);
                }
            }
        }

        let rel = |native: &str| -> String {
            let base = root_native.trim_end_matches('/');
            native
                .strip_prefix(base)
                .map(|r| r.trim_start_matches('/').to_string())
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| native.to_string())
        };
        let mut body = String::new();
        let mut total = 0usize;
        let mut file_counts: Vec<(String, usize)> = Vec::new();
        for uri in files {
            if total >= max_results || body.len() > budget {
                break;
            }
            let native = uri.inferred_native_path_string();
            let shown = rel(&native);
            if shown.split('/').any(|seg| PRUNED_DIRS.contains(&seg)) {
                continue;
            }
            if args.glob.as_deref().is_some_and(|g| !glob_match(g, &shown)) {
                continue;
            }
            let Ok(m) = fs
                .get_metadata(&uri, GetMetadataOptions::default(), Some(&sandbox))
                .await
            else {
                continue;
            };
            if !m.is_file || m.size > SEARCH_MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = fs
                .read_file(&uri, ReadFileOptions::default(), Some(&sandbox))
                .await
            else {
                continue;
            };
            if looks_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let (block, n) = search_text(&text, &re, context, max_results - total);
            if n == 0 {
                continue;
            }
            total += n;
            if args.files_only {
                file_counts.push((shown, n));
            } else {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&shown);
                body.push('\n');
                body.push_str(&block);
            }
        }
        if args.files_only {
            for (path, n) in &file_counts {
                body.push_str(&format!("{path} ({n})\n"));
            }
        }
        if total == 0 {
            body = "no matches\n".to_string();
        }
        let mut text = finish_search(body, total, max_results, budget);
        if walk_truncated {
            text.push_str("[byteask: the directory tree is very large and was only partly walked; pass a narrower `path`]\n");
        }
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            text,
            Some(true),
        )))
    }
}

/// Keep the result inside the budget on a line boundary and say so when it was cut.
fn finish_search(mut body: String, total: usize, max_results: usize, budget: usize) -> String {
    let mut cut_by_bytes = false;
    if body.len() > budget {
        let mut cut = budget;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        let cut = body[..cut].rfind('\n').map_or(cut, |i| i + 1);
        body.truncate(cut);
        cut_by_bytes = true;
    }
    if cut_by_bytes || total >= max_results {
        body.push_str(&format!(
            "[byteask: results cut at {total} matching lines; narrow `pattern`, `path` or `glob`, or raise `max_results`]\n"
        ));
    }
    body
}

impl CoreToolRuntime for SearchHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn search_walk_bounds_are_within_the_filesystem_limits() {
        assert!(SEARCH_WALK.max_depth <= codex_file_system::MAX_WALK_DEPTH);
        assert!(SEARCH_WALK.max_directories <= codex_file_system::MAX_WALK_DIRECTORIES);
        assert!(SEARCH_WALK.max_entries <= codex_file_system::MAX_WALK_ENTRIES);
    }

    /// The bounds check above can only pin the limits it knows about; this asks the real
    /// filesystem layer, which is what refused every directory search in 0.1.13/0.1.14.
    #[tokio::test]
    async fn search_walk_is_accepted_by_the_real_filesystem_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/deep/queue.hpp"),
            "remove_order_from_level\n",
        )
        .expect("write");
        let root = codex_utils_path_uri::PathUri::from_host_native_path(dir.path())
            .expect("absolute temp path");
        let outcome = codex_exec_server::LOCAL_FS
            .walk(&root, SEARCH_WALK, None)
            .await
            .expect("the filesystem layer must accept the search's walk bounds");
        let files: Vec<String> = outcome
            .entries
            .iter()
            .filter(|entry| entry.kind == WalkEntryKind::File)
            .map(|entry| entry.path.inferred_native_path_string())
            .collect();
        assert!(
            files.iter().any(|f| f.ends_with("src/deep/queue.hpp")),
            "walk found {files:?}"
        );
    }

    #[test]
    fn a_reread_is_stubbed_only_while_the_earlier_copy_is_provably_still_there() {
        let mut m = ReadMemory::default();
        let t0 = std::time::Instant::now();
        let later = |s| t0 + std::time::Duration::from_secs(s);
        assert_eq!(
            m.seen("t", "a.rs", 1, 400, 100, 50, t0, 0),
            None,
            "first read is real"
        );
        assert_eq!(
            m.seen("t", "a.rs", 1, 400, 100, 50, later(30), 0)
                .map(|d| d.as_secs()),
            Some(30)
        );
        // A different range, thread, or a changed file is always a real read.
        assert_eq!(m.seen("t", "a.rs", 50, 400, 100, 50, later(31), 0), None);
        assert_eq!(m.seen("other", "a.rs", 1, 400, 100, 50, later(31), 0), None);
        assert_eq!(
            m.seen("t", "a.rs", 1, 400, 999, 50, later(32), 0),
            None,
            "mtime changed"
        );
        assert_eq!(
            m.seen("t", "a.rs", 1, 400, 999, 51, later(33), 0),
            None,
            "size changed"
        );
        // A compaction removed the earlier copy from context.
        assert_eq!(m.seen("t", "a.rs", 1, 400, 999, 51, later(34), 1), None);
        assert!(m.seen("t", "a.rs", 1, 400, 999, 51, later(35), 1).is_some());
        // Too long ago to rely on.
        assert_eq!(
            m.seen("t", "a.rs", 1, 400, 999, 51, later(34 + 601), 1),
            None
        );
    }

    #[test]
    fn a_range_is_exact_text_with_a_header_and_a_continue_footer() {
        let text = "one\ntwo\nthree\nfour\nfive\n";
        let got = render_range("a.txt", text, Some(2), Some(2), 10_000);
        assert_eq!(
            got,
            "== a.txt (lines 2-3 of 5) ==\ntwo\nthree\n[byteask: lines 4-5 of a.txt not shown; continue with offset=4]\n"
        );
        assert_eq!(
            render_range("a.txt", text, None, None, 10_000),
            "== a.txt (lines 1-5 of 5) ==\none\ntwo\nthree\nfour\nfive\n"
        );
    }

    #[test]
    fn the_budget_cuts_on_a_line_and_never_silently() {
        let text = "aaaa\nbbbb\ncccc\ndddd\n";
        let got = render_range("f", text, None, None, 11);
        assert!(got.contains("(lines 1-2 of 4)"));
        assert!(got.contains("aaaa\nbbbb\n"));
        assert!(!got.contains("cccc\n"));
        assert!(got.contains("continue with offset=3"));
    }

    #[test]
    fn edges_of_a_range() {
        assert!(render_range("f", "", None, None, 100).contains("empty file"));
        assert!(
            render_range("f", "a\nb\n", Some(9), None, 100)
                .contains("past the end; the file has 2 lines")
        );
        // One line longer than the whole budget is cut, not dropped.
        let got = render_range("f", &"x".repeat(500), None, None, 100);
        assert!(got.contains("(lines 1-1 of 1)"));
        assert!(got.len() < 250);
    }

    #[test]
    fn search_blocks_use_ripgrep_heading_layout() {
        let re = regex_lite::Regex::new("needle").unwrap();
        let text = "a\nb needle\nc\nd\ne\nf needle\ng\n";
        let (block, n) = search_text(text, &re, 0, 100);
        assert_eq!((block.as_str(), n), ("2:b needle\n6:f needle\n", 2));
        let (block, _) = search_text(text, &re, 1, 100);
        assert_eq!(block, "1-a\n2:b needle\n3-c\n--\n5-e\n6:f needle\n7-g\n");
        let (block, n) = search_text(text, &re, 0, 1);
        assert_eq!((block.as_str(), n), ("2:b needle\n", 1));
    }

    #[test]
    fn overlapping_context_is_printed_once() {
        let re = regex_lite::Regex::new("x").unwrap();
        let (block, _) = search_text("x1\nmid\nx2\n", &re, 1, 100);
        assert_eq!(block, "1:x1\n2-mid\n3:x2\n");
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.rs", "src/deep/main.rs"));
        assert!(!glob_match("*.rs", "src/main.py"));
        assert!(glob_match("src/**/*.ts", "src/a/b/c.ts"));
        assert!(glob_match("src/**/*.ts", "src/c.ts"));
        assert!(!glob_match("src/*.ts", "src/a/c.ts"));
        assert!(glob_match("test_?.py", "test_a.py"));
    }

    #[test]
    fn a_cut_result_says_so() {
        let body = "p\n1:a\n2:b\n3:c\n".to_string();
        let got = finish_search(body.clone(), 3, 100, 8);
        assert!(got.starts_with("p\n1:a\n"));
        assert!(got.contains("results cut at 3 matching lines"));
        assert_eq!(finish_search(body.clone(), 3, 100, 1000), body);
    }
}
