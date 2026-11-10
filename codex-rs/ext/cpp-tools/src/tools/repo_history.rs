//! `cpp_repo_history`: what the repository's own history says about the files
//! being changed — which files and functions change together with them, how
//! past commits touching them were shaped (tests, docs, changelog, generated
//! copies, build files), and which commits introduced the changed lines.
//!
//! Leak-proof by construction: every git command names the session-start
//! commit H0 (or a commit verified to be one of its ancestors) and never
//! `--all`, branches, tags, the reflog, stashes or notes, so commits made after
//! the task's starting point — possibly the fix itself — are out of reach even
//! in a checkout that still has them. Every call is logged (`git_call` records
//! in the tool-call log) and counted in the output's last line.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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

use crate::gitx;
use crate::gitx::Audit;
use crate::session_head::Head;
use crate::session_head::SessionHead;
use crate::testsel::is_cpp_file;
use crate::testsel::symbols;

const TOOL_NAME: &str = "cpp_repo_history";
/// Model-facing cap (≈ 1,200 tokens; `show` ≈ 1,500).
const OUTPUT_CAP: usize = 4800;
const SHOW_CAP: usize = 6000;
const MIN_HISTORY: u64 = 20;
const DEFAULT_MAX_COMMITS: u64 = 2000;
/// Commits touching more files than this are bulk changes (reformat, rename
/// sweep, import) and say nothing about coupling.
const BULK_FILES: usize = 40;
const BASE_RATE_COMMITS: u64 = 5000;
const FUNCTION_LOG_COMMITS: u64 = 300;
const TWO_YEARS_SECS: f64 = 2.0 * 365.25 * 86400.0;
const LOG_TIMEOUT: Duration = Duration::from_secs(60);

pub struct RepoHistoryTool {
    /// The session-start commit (it also knows the workspace directory).
    pub head: SessionHead,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    symbols: Option<Vec<String>>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default, deserialize_with = "crate::args::whole_u64")]
    max_commits: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for RepoHistoryTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut p = BTreeMap::new();
        p.insert(
            "paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Files to ask about, relative to the repository root. Default: the files changed since the session started.".to_string()),
            ),
        );
        p.insert(
            "symbols".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Functions to find co-changing functions for. Default: the ones the diff changes.".to_string()),
            ),
        );
        p.insert(
            "mode".to_string(),
            JsonSchema::string(Some(
                "\"summary\" (default): co-changing files and functions, the usual shape of past \
                 commits (tests/docs/changelog/generated copies), example commits, and blame of \
                 the changed lines. \"blame\": only who introduced the changed lines, in more \
                 detail. \"show\": one past commit (sha) with up to 100 diff lines."
                    .to_string(),
            )),
        );
        p.insert(
            "sha".to_string(),
            JsonSchema::string(Some("For mode=show: a commit id (hex) from this history.".to_string())),
        );
        p.insert(
            "query".to_string(),
            JsonSchema::string(Some(
                "Words to rank example commits by (e.g. the issue title). A commit id in it is \
                 checked against the local history."
                    .to_string(),
            )),
        );
        p.insert(
            "max_commits".to_string(),
            JsonSchema::number(Some(format!(
                "Most commits read per query (default {DEFAULT_MAX_COMMITS})."
            ))),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "What this repository's history says about the files you are \
                changing: files and functions that usually change together with them, how past \
                commits touching them were structured (which tests, docs, changelog, generated \
                copies they also touched), example commits, and which commits introduced the \
                lines you changed. Reads only the commit the session started on and its \
                ancestors. Use it to find the other places a change like yours usually needs, or \
                to read how a regression came in."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(p, None, Some(AdditionalProperties::from(false))),
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

impl RepoHistoryTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: Args = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let mode = args.mode.clone().unwrap_or_else(|| "summary".to_string());
        let Some(head) = self.head.get().await else {
            return Ok(crate::report::report_plain(
                TOOL_NAME,
                crate::toolver::did_not_run(
                    "no git history here (not a git work tree, or no commit yet)",
                    "read the files directly; there is no history to mine",
                    "",
                ),
                check_result::CheckResult::not_run(
                    TOOL_NAME,
                    check_result::NotRunReason::Unsupported,
                    "There is no git history here (not a git work tree, or no commit yet).",
                    "read the files directly; there is no history to mine.",
                ),
            ));
        };
        let audit = Audit::default();
        let body = match mode.as_str() {
            "summary" | "blame" => summary(&head, &args, &audit, mode == "blame").await,
            "show" => show(&head, args.sha.as_deref().unwrap_or(""), &audit).await,
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unknown mode {other:?}: use summary, blame or show"
                )));
            }
        };
        let cap_at = if mode == "show" { SHOW_CAP } else { OUTPUT_CAP };
        let mut text = cap(body, cap_at);
        text.push('\n');
        text.push_str(&audit.footer(&format!("revisions: {} and its ancestors only", head.short())));
        // History is evidence to read, not a verdict on the code.
        let result = check_result::CheckResult::no_claim(
            TOOL_NAME,
            &format!("Read the git history ({mode}); it describes the past and judges nothing."),
        );
        Ok(crate::report::report(TOOL_NAME, &mode, text, result))
    }
}

fn cap(text: String, at: usize) -> String {
    if text.len() <= at {
        return text;
    }
    let mut end = at;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (capped)", &text[..end])
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// One past commit.
#[derive(Clone, Debug, PartialEq)]
struct Commit {
    sha: String,
    time: i64,
    subject: String,
    /// (status letter, path) — renames give the new path.
    files: Vec<(char, String)>,
}

impl Commit {
    fn touches(&self, path: &str) -> bool {
        self.files.iter().any(|(_, f)| f == path)
    }
}

/// `git log --name-status --format=%x00%H%x09%at%x09%s` output.
fn parse_log(text: &str) -> Vec<Commit> {
    let mut out: Vec<Commit> = Vec::new();
    for chunk in text.split('\0').filter(|c| !c.trim().is_empty()) {
        let mut lines = chunk.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut parts = header.splitn(3, '\t');
        let sha = parts.next().unwrap_or_default().to_string();
        let time = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let subject = parts.next().unwrap_or_default().to_string();
        let mut files = Vec::new();
        for line in lines {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 2 {
                continue;
            }
            let status = cols[0].chars().next().unwrap_or('M');
            let path = cols.last().copied().unwrap_or_default().to_string();
            files.push((status, path));
        }
        if !sha.is_empty() {
            out.push(Commit { sha, time, subject, files });
        }
    }
    out
}

/// Role of a file in a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Test,
    Doc,
    Changelog,
    Build,
    Generated,
    Code,
    Other,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Test => "test",
            Role::Doc => "doc",
            Role::Changelog => "changelog",
            Role::Build => "build",
            Role::Generated => "generated copy",
            Role::Code => "code",
            Role::Other => "other",
        }
    }
}

fn role(path: &str) -> Role {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let dirs: Vec<&str> = lower.split('/').collect();
    if ["changelog", "news", "changes", "release-notes", "release_notes", "history"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        return Role::Changelog;
    }
    // Single-header copies generated from the real sources.
    if dirs.iter().any(|d| d.starts_with("single")) || lower.contains("amalgam") || lower.contains("/generated/") {
        return Role::Generated;
    }
    if dirs[..dirs.len().saturating_sub(1)]
        .iter()
        .any(|d| d.starts_with("test") || *d == "t" || d.ends_with("tests") || *d == "unittest" || *d == "spec")
        || name.contains("test")
        || name.contains("_spec")
    {
        return Role::Test;
    }
    if crate::testsel::is_build_file(Path::new(path)) || name.starts_with("build") && name.ends_with(".gn") {
        return Role::Build;
    }
    if dirs.first().is_some_and(|d| *d == "doc" || *d == "docs" || *d == "man")
        || name.ends_with(".md")
        || name.ends_with(".rst")
        || name.ends_with(".txt") && name != "cmakelists.txt"
        || name.ends_with(".adoc")
        || name.ends_with(".texi")
    {
        return Role::Doc;
    }
    if is_cpp_file(Path::new(path)) {
        return Role::Code;
    }
    Role::Other
}

fn recency(time: i64, now: i64) -> f64 {
    let age = (now - time).max(0) as f64;
    0.5f64.powf(age / TWO_YEARS_SECS)
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

async fn summary(head: &Head, args: &Args, audit: &Audit, blame_only: bool) -> String {
    let root = &head.root;
    let mut out = String::new();
    let count = gitx::git_str(TOOL_NAME, Some(audit), root, &["rev-list", "--count", &head.sha], LOG_TIMEOUT).await;
    let commits: u64 = count.stdout.trim().parse().unwrap_or(0);
    let git_dir = gitx::git_str(TOOL_NAME, Some(audit), root, &["rev-parse", "--git-dir"], gitx::GIT_TIMEOUT).await;
    let shallow = git_dir.ok && {
        let dir = PathBuf::from(git_dir.stdout.trim());
        let dir = if dir.is_absolute() { dir } else { root.join(dir) };
        dir.join("shallow").is_file()
    };

    // What to ask about.
    let changes = symbols::changes(root, &head.sha, String::new(), None, args.symbols.as_deref()).await.ok();
    let paths: Vec<String> = match &args.paths {
        Some(paths) => paths.clone(),
        None => changes
            .as_ref()
            .map(|c| {
                c.files
                    .iter()
                    .filter_map(|f| f.strip_prefix(root).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let functions: Vec<String> = changes.as_ref().map(|c| c.symbols.clone()).unwrap_or_default();
    out.push_str(&format!(
        "cpp_repo_history — {commits} ancestor commit(s) of {}{}; {} file(s) queried{}\n",
        head.short(),
        if shallow { " (shallow clone: older history missing)" } else { "" },
        paths.len(),
        if functions.is_empty() { String::new() } else { format!(", functions: {}", functions.iter().take(6).cloned().collect::<Vec<_>>().join(", ")) }
    ));
    if let Some(query) = &args.query {
        for sha in query.split(|c: char| !c.is_ascii_hexdigit()).filter(|t| gitx::looks_like_sha(t) && t.len() >= 7) {
            out.push_str(&named_commit(head, sha, audit).await);
        }
    }
    if paths.is_empty() {
        out.push_str("Nothing to ask about: no changed files yet and no `paths` given.\n");
        return out;
    }
    if commits < MIN_HISTORY {
        out.push_str(&format!(
            "History too short to mine ({commits} commit(s): the repository was probably imported \
             or truncated).\n"
        ));
        out.push_str(&tree_only(root, &paths));
        return out;
    }
    if !blame_only {
        let max = args.max_commits.unwrap_or(DEFAULT_MAX_COMMITS).clamp(50, 20_000).to_string();
        let mut log_args: Vec<String> = [
            "log",
            head.sha.as_str(),
            "--no-merges",
            "-M",
            "--full-diff",
            "--name-status",
            "--format=%x00%H%x09%at%x09%s",
            "-n",
            max.as_str(),
            "--",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        log_args.extend(paths.iter().cloned());
        let log = gitx::git_logged(TOOL_NAME, Some(audit), root, &log_args, LOG_TIMEOUT).await;
        let touching = parse_log(&log.stdout);
        let base_rates = base_rates(head, audit).await;
        out.push_str(&cochange_section(&touching, &paths, &base_rates, log.truncated || log.timed_out));
        out.push_str(&structure_section(&touching, &paths));
        let funcs = function_index(head, &paths, audit).await;
        out.push_str(&function_section(&funcs, &functions));
        out.push_str(&examples_section(&touching, &funcs, &functions, args.query.as_deref()));
    }
    out.push_str(&blame_section(head, &paths, audit, if blame_only { 8 } else { 4 }).await);
    out
}

/// Files per commit over recent history: `P(file)` for lift.
async fn base_rates(head: &Head, audit: &Audit) -> (usize, BTreeMap<String, usize>) {
    let n = BASE_RATE_COMMITS.to_string();
    let log = gitx::git_str(
        TOOL_NAME,
        Some(audit),
        &head.root,
        &["log", &head.sha, "--no-merges", "--name-only", "--format=%x00", "-n", &n],
        LOG_TIMEOUT,
    )
    .await;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    for chunk in log.stdout.split('\0') {
        let files: Vec<&str> = chunk.lines().filter(|l| !l.trim().is_empty()).collect();
        if files.is_empty() || files.len() > BULK_FILES {
            continue;
        }
        total += 1;
        for f in files {
            *counts.entry(f.to_string()).or_default() += 1;
        }
    }
    (total, counts)
}

fn cochange_section(touching: &[Commit], query: &[String], base: &(usize, BTreeMap<String, usize>), partial: bool) -> String {
    let kept_all: Vec<&Commit> = touching.iter().filter(|c| c.files.len() <= BULK_FILES).collect();
    let bulk = touching.len() - kept_all.len();
    if kept_all.is_empty() {
        return format!("CO-CHANGE: no non-bulk commit touches these files ({bulk} bulk).\n");
    }
    // TARMAQ: the transactions with the largest overlap with the query.
    let overlap = |c: &Commit| query.iter().filter(|q| c.touches(q)).count();
    let kmax = kept_all.iter().map(|c| overlap(c)).max().unwrap_or(0);
    let kept: Vec<&Commit> = kept_all.iter().copied().filter(|c| overlap(c) == kmax).collect();
    let now = kept.iter().map(|c| c.time).max().unwrap_or(0);
    let total_weight: f64 = kept.iter().map(|c| recency(c.time, now)).sum();
    let mut support: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
    for commit in &kept {
        let w = recency(commit.time, now);
        for (_, file) in &commit.files {
            if query.contains(file) {
                continue;
            }
            let entry = support.entry(file.as_str()).or_default();
            entry.0 += 1;
            entry.1 += w;
        }
    }
    let (base_total, base_counts) = base;
    let mut rows: Vec<(String, usize, f64, f64)> = support
        .iter()
        .map(|(file, (n, w))| {
            let confidence = if total_weight > 0.0 { w / total_weight } else { 0.0 };
            let p = base_counts.get(*file).copied().unwrap_or(*n) as f64 / (*base_total).max(1) as f64;
            let lift = if p > 0.0 { confidence / p } else { 0.0 };
            ((*file).to_string(), *n, confidence, lift)
        })
        .collect();
    let min_support = if rows.iter().any(|r| r.1 >= 3) { 3 } else { 2 };
    rows.retain(|r| r.1 >= min_support);
    rows.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.3.total_cmp(&a.3)));
    let mut out = format!(
        "CO-CHANGE (files; support/confidence/lift over {} commit(s) touching {} of the queried files{}{})\n",
        kept.len(),
        kmax,
        if bulk > 0 { format!("; {bulk} bulk commit(s) >{BULK_FILES} files ignored") } else { String::new() },
        if partial { "; history read partially" } else { "" }
    );
    if rows.is_empty() {
        out.push_str("  none with support ≥ 2\n");
    }
    for (file, n, confidence, lift) in rows.into_iter().take(8) {
        out.push_str(&format!("  {file:<40} {n}/{confidence:.2}/{lift:.1}  [{}]\n", role(&file).label()));
    }
    out
}

fn structure_section(touching: &[Commit], query: &[String]) -> String {
    let commits: Vec<&Commit> = touching.iter().filter(|c| c.files.len() <= BULK_FILES).collect();
    if commits.is_empty() {
        return String::new();
    }
    let n = commits.len() as f64;
    let share = |pred: &dyn Fn(&Commit) -> bool| -> f64 { commits.iter().filter(|c| pred(c)).count() as f64 * 100.0 / n };
    let has_role = |r: Role| move |c: &Commit| c.files.iter().any(|(_, f)| !query.contains(f) && role(f) == r);
    let tests_added = share(&|c: &Commit| c.files.iter().any(|(s, f)| *s == 'A' && role(f) == Role::Test));
    let mut test_touches: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &commits {
        for (_, f) in &c.files {
            if role(f) == Role::Test && !query.contains(f) {
                *test_touches.entry(f.as_str()).or_default() += 1;
            }
        }
    }
    let mut top: Vec<(&str, usize)> = test_touches.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let top: Vec<String> = top.into_iter().take(3).map(|(f, c)| format!("{f} {c}")).collect();
    format!(
        "STRUCTURE of the {} past commit(s) touching these files: tests {:.0}% (new test files {:.0}%{}); docs {:.0}%; changelog {:.0}%; build files {:.0}%; generated copies {:.0}%\n",
        commits.len(),
        share(&has_role(Role::Test)),
        tests_added,
        if top.is_empty() { String::new() } else { format!("; most touched: {}", top.join(", ")) },
        share(&has_role(Role::Doc)),
        share(&has_role(Role::Changelog)),
        share(&has_role(Role::Build)),
        share(&has_role(Role::Generated)),
    )
}

/// commit → functions its hunks in the queried C/C++ files touch.
async fn function_index(head: &Head, paths: &[String], audit: &Audit) -> BTreeMap<String, BTreeSet<String>> {
    let cpp: Vec<&String> = paths.iter().filter(|p| is_cpp_file(Path::new(p))).collect();
    if cpp.is_empty() {
        return BTreeMap::new();
    }
    let n = FUNCTION_LOG_COMMITS.to_string();
    let mut args: Vec<String> = vec![
        "-c".to_string(),
        format!("core.attributesFile={}", symbols::attributes_file().display()),
        "log".to_string(),
        head.sha.clone(),
        "--no-merges".to_string(),
        "-p".to_string(),
        "-U0".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--no-color".to_string(),
        "--format=%x00%H".to_string(),
        "-n".to_string(),
        n,
        "--".to_string(),
    ];
    args.extend(cpp.into_iter().cloned());
    let log = gitx::git_logged(TOOL_NAME, Some(audit), &head.root, &args, LOG_TIMEOUT).await;
    let mut index: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for chunk in log.stdout.split('\0').filter(|c| !c.trim().is_empty()) {
        let mut lines = chunk.lines();
        let sha = lines.next().unwrap_or_default().trim().to_string();
        let funcs = index.entry(sha).or_default();
        for line in lines {
            if let Some(rest) = line.strip_prefix("@@ ") {
                if let Some((_, context)) = rest.split_once(" @@") {
                    if let Some(name) = symbols::enclosing_name(context) {
                        funcs.insert(name);
                    }
                }
            } else if let Some(body) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
                if !line.starts_with("+++") && !line.starts_with("---") {
                    funcs.extend(symbols::declarations(body));
                }
            }
        }
    }
    index
}

fn function_section(index: &BTreeMap<String, BTreeSet<String>>, functions: &[String]) -> String {
    if index.is_empty() || functions.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(String, String, usize, usize)> = Vec::new();
    for f in functions.iter().take(6) {
        let with_f: Vec<&BTreeSet<String>> = index.values().filter(|fs| fs.contains(f)).collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for fs in &with_f {
            for g in fs.iter() {
                if g != f && !functions.contains(g) {
                    *counts.entry(g.as_str()).or_default() += 1;
                }
            }
        }
        for (g, n) in counts {
            if n >= 2 {
                rows.push((f.clone(), g.to_string(), n, with_f.len()));
            }
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
    let mut out = String::from("CO-CHANGE (functions, from hunk headers; approximate)\n");
    for (f, g, n, m) in rows.into_iter().take(6) {
        out.push_str(&format!("  {f} ~ {g}  {n}/{m} commits\n"));
    }
    out
}

fn examples_section(
    touching: &[Commit],
    funcs: &BTreeMap<String, BTreeSet<String>>,
    functions: &[String],
    query: Option<&str>,
) -> String {
    let commits: Vec<&Commit> = touching.iter().filter(|c| c.files.len() <= BULK_FILES).collect();
    if commits.is_empty() {
        return String::new();
    }
    let now = commits.iter().map(|c| c.time).max().unwrap_or(0);
    let want: BTreeSet<&str> = functions.iter().map(String::as_str).collect();
    let words: BTreeSet<String> = query
        .unwrap_or("")
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect();
    let mut scored: Vec<(f64, &Commit)> = commits
        .iter()
        .map(|c| {
            let fs: BTreeSet<&str> = funcs.get(&c.sha).map(|s| s.iter().map(String::as_str).collect()).unwrap_or_default();
            let inter = fs.intersection(&want).count() as f64;
            let union = fs.union(&want).count().max(1) as f64;
            let subject: BTreeSet<String> = c
                .subject
                .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .map(str::to_ascii_lowercase)
                .collect();
            let q = if words.is_empty() { 0.0 } else { words.intersection(&subject).count() as f64 / words.len() as f64 };
            (0.5 * inter / union + 0.3 * q + 0.2 * recency(c.time, now), *c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out = String::from("EXAMPLES (most similar past commits; `mode=show sha=…` for the diff)\n");
    for (_, c) in scored.into_iter().take(3) {
        let mut by_role: BTreeMap<Role, Vec<&str>> = BTreeMap::new();
        for (_, f) in &c.files {
            by_role.entry(role(f)).or_default().push(f.as_str());
        }
        let files: Vec<String> = by_role
            .iter()
            .map(|(r, fs)| {
                let shown: Vec<&str> = fs.iter().take(3).copied().collect();
                let more = fs.len().saturating_sub(3);
                format!("{}: {}{}", r.label(), shown.join(" "), if more > 0 { format!(" (+{more})") } else { String::new() })
            })
            .collect();
        out.push_str(&format!(
            "  {} {} \"{}\" — {}\n",
            &c.sha[..c.sha.len().min(9)],
            date(c.time),
            c.subject.chars().take(80).collect::<String>(),
            files.join("; ")
        ));
    }
    out
}

/// Pre-image line ranges of each changed hunk, blamed at H0.
async fn blame_section(head: &Head, paths: &[String], audit: &Audit, max_commits: usize) -> String {
    let mut shas: Vec<(String, usize, String)> = Vec::new(); // (sha, lines, file)
    let mut meta: BTreeMap<String, (i64, String)> = BTreeMap::new();
    for path in paths.iter().filter(|p| is_cpp_file(Path::new(p))).take(3) {
        let diff = gitx::git_str(
            TOOL_NAME,
            Some(audit),
            &head.root,
            &["diff", "-U0", "--no-color", "--no-ext-diff", "--no-textconv", &head.sha, "--", path],
            gitx::GIT_TIMEOUT,
        )
        .await;
        let ranges = old_ranges(&diff.stdout);
        for (a, b) in ranges.into_iter().take(4) {
            let range = format!("{a},{b}");
            let blame = gitx::git_str(
                TOOL_NAME,
                Some(audit),
                &head.root,
                &["blame", "-w", "-M", "--line-porcelain", "-L", &range, &head.sha, "--", path],
                LOG_TIMEOUT,
            )
            .await;
            for (sha, time, summary) in parse_blame(&blame.stdout) {
                meta.entry(sha.clone()).or_insert((time, summary));
                match shas.iter_mut().find(|(s, _, f)| *s == sha && f == path) {
                    Some(entry) => entry.1 += 1,
                    None => shas.push((sha, 1, path.clone())),
                }
            }
        }
    }
    if shas.is_empty() {
        return String::new();
    }
    shas.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = String::from("BLAME of the changed lines (commits that introduced them)\n");
    for (sha, lines, file) in shas.iter().take(max_commits) {
        let (time, summary) = meta.get(sha).cloned().unwrap_or_default();
        out.push_str(&format!(
            "  {} {} \"{}\" — {lines} line(s) of {file}\n",
            &sha[..sha.len().min(9)],
            date(time),
            summary.chars().take(80).collect::<String>()
        ));
    }
    out
}

/// `@@ -a,b +c,d @@` → the old-side `(a, a+b-1)` for hunks that replace or
/// delete lines (pure additions have no pre-image to blame).
fn old_ranges(diff: &str) -> Vec<(u32, u32)> {
    diff.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("@@ -")?;
            let old = rest.split_whitespace().next()?;
            let mut parts = old.splitn(2, ',');
            let start: u32 = parts.next()?.parse().ok()?;
            let count: u32 = match parts.next() {
                Some(c) => c.parse().ok()?,
                None => 1,
            };
            (count > 0 && start > 0).then(|| (start, start + count - 1))
        })
        .collect()
}

/// `git blame --line-porcelain`: (sha, author-time, summary) per line.
fn parse_blame(text: &str) -> Vec<(String, i64, String)> {
    let mut out = Vec::new();
    let mut sha = String::new();
    let mut time = 0i64;
    for line in text.lines() {
        if line.starts_with('\t') {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or_default();
        if first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit()) {
            sha = first.to_string();
        } else if let Some(t) = line.strip_prefix("author-time ") {
            time = t.trim().parse().unwrap_or(0);
        } else if let Some(s) = line.strip_prefix("summary ") {
            out.push((sha.clone(), time, s.to_string()));
        }
    }
    out
}

/// A commit id named by the caller: in local history (an ancestor of H0)?
async fn named_commit(head: &Head, sha: &str, audit: &Audit) -> String {
    if gitx::is_ancestor(TOOL_NAME, Some(audit), &head.root, sha, &head.sha).await {
        let subject = gitx::git_str(
            TOOL_NAME,
            Some(audit),
            &head.root,
            &["log", "-1", "--format=%h %ad %s", "--date=short", sha],
            gitx::GIT_TIMEOUT,
        )
        .await;
        format!("commit {sha}: in local history — {}\n", subject.stdout.trim())
    } else {
        format!("commit {sha}: not in local history (absent, or not an ancestor of the session-start commit)\n")
    }
}

/// With no usable history: where tests for the touched files live, by name.
fn tree_only(root: &Path, paths: &[String]) -> String {
    let stems: Vec<String> = paths
        .iter()
        .filter_map(|p| Path::new(p).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()))
        .filter(|s| s.len() >= 3)
        .collect();
    let mut hits: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(6)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && e.depth() > 0 && crate::testsel::skip_dir(e.path())))
        .flatten()
    {
        let rel = entry.path().strip_prefix(root).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if entry.file_type().is_file() && role(&rel) == Role::Test && stems.iter().any(|s| rel.to_ascii_lowercase().contains(s.as_str())) {
            hits.push(rel);
        }
        if hits.len() >= 6 {
            break;
        }
    }
    let changelog = ["ChangeLog", "CHANGELOG.md", "NEWS", "CHANGES", "ChangeLog.md", "NEWS.md"]
        .iter()
        .find(|f| root.join(f).is_file());
    format!(
        "TREE-ONLY: tests named like the touched files: {}; changelog: {}\n",
        if hits.is_empty() { "none found".to_string() } else { hits.join(", ") },
        changelog.copied().unwrap_or("none")
    )
}

async fn show(head: &Head, sha: &str, audit: &Audit) -> String {
    if !gitx::looks_like_sha(sha) {
        return "mode=show needs `sha`: a commit id (7-40 hex digits) from this history.".to_string();
    }
    if !gitx::is_ancestor(TOOL_NAME, Some(audit), &head.root, sha, &head.sha).await {
        return format!(
            "commit {sha} is not in the local history of the session-start commit {}; only that \
             commit and its ancestors can be shown.",
            head.short()
        );
    }
    let out = gitx::git_str(
        TOOL_NAME,
        Some(audit),
        &head.root,
        &[
            "show",
            "--stat",
            "--patch",
            "-U3",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--format=commit %H%nDate: %ad%n%n    %s%n%n%b",
            "--date=short",
            sha,
        ],
        gitx::GIT_TIMEOUT,
    )
    .await;
    if !out.ok {
        return format!("git show failed: {}", out.stderr.trim());
    }
    let lines: Vec<&str> = out.stdout.lines().collect();
    let diff_start = lines.iter().position(|l| l.starts_with("diff --git")).unwrap_or(lines.len());
    let mut text: Vec<&str> = lines[..diff_start].to_vec();
    let diff = &lines[diff_start..];
    text.extend(diff.iter().take(100));
    let mut body = text.join("\n");
    if diff.len() > 100 {
        body.push_str(&format!("\n… {} more diff line(s)", diff.len() - 100));
    }
    body
}

fn date(time: i64) -> String {
    // Civil date from Unix seconds (UTC), no dependency needed.
    let days = time.div_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::session_head::test_repo;
    use crate::test_support::call;

    #[test]
    fn classifies_roles_and_dates() {
        assert_eq!(role("tests/widget_test.cc"), Role::Test);
        assert_eq!(role("test/foo.cpp"), Role::Test);
        assert_eq!(role("docs/api.md"), Role::Doc);
        assert_eq!(role("ChangeLog"), Role::Changelog);
        assert_eq!(role("CMakeLists.txt"), Role::Build);
        assert_eq!(role("single/lib.hpp"), Role::Generated);
        assert_eq!(role("src/widget.cc"), Role::Code);
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn parses_log_blame_and_ranges() {
        let log = "\0abc123\t1700000000\tAdd size()\nM\tsrc/a.cc\nA\ttests/a_test.cc\nR100\told.h\tnew.h\n\0def456\t1600000000\tFix\nM\tsrc/a.cc\n";
        let commits = parse_log(log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].files, vec![('M', "src/a.cc".to_string()), ('A', "tests/a_test.cc".to_string()), ('R', "new.h".to_string())]);
        assert_eq!(old_ranges("@@ -3,2 +3,2 @@ f\n@@ -9,0 +10,1 @@\n@@ -20 +21 @@\n"), vec![(3, 4), (20, 20)]);
        let blame = format!("{} 3 3 1\nauthor x\nauthor-time 1700000000\nsummary Normalize once\nfilename a.c\n\tcode\n", "a".repeat(40));
        assert_eq!(parse_blame(&blame), vec![("a".repeat(40), 1_700_000_000, "Normalize once".to_string())]);
    }

    /// A history where every change to `widget.c` also touched its test and a
    /// generated copy, plus a commit that is NOT an ancestor of H0 (on
    /// another branch) that the tool must never see.
    #[tokio::test]
    async fn mines_only_the_session_start_ancestry() {
        crate::base_run_cache::use_test_cache_dir();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let write = |path: &str, text: &str| {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        };
        write("src/widget.c", "int size(void) {\n  return 0;\n}\nint cap(void) {\n  return 1;\n}\n");
        write("README.md", "x\n");
        test_repo::init_and_commit(&root);
        for i in 0..24 {
            write("src/widget.c", &format!("int size(void) {{\n  return {i};\n}}\nint cap(void) {{\n  return {};\n}}\n", i + 1));
            write("tests/widget_test.c", &format!("// {i}\n"));
            write("single/lib.h", &format!("// copy {i}\n"));
            if i % 3 == 0 {
                write("docs/api.md", &format!("v{i}\n"));
            }
            test_repo::commit_all(&root, &format!("widget: change {i}"));
        }
        let h0 = test_repo::git(&root, &["rev-parse", "HEAD"]);
        // A later commit on another branch mentioning a leaked file.
        test_repo::git(&root, &["checkout", "-q", "-b", "future"]);
        write("src/widget.c", "int size(void) { return 99; }\n");
        write("src/leaked_fix.c", "int fixed;\n");
        let future = test_repo::commit_all(&root, "the upstream fix");
        test_repo::git(&root, &["checkout", "-q", &h0]);
        // The agent's working-tree change.
        write("src/widget.c", "int size(void) {\n  return -1;\n}\nint cap(void) {\n  return 25;\n}\n");

        let t = RepoHistoryTool {
            head: SessionHead::ready(
                Some(Head {
                    sha: h0.clone(),
                    root: root.clone(),
                }),
                root.clone(),
            ),
        };
        let text = call(&t, json!({"query": format!("regression since {}", &future[..10])})).await.unwrap();
        assert!(text.contains("25 ancestor commit(s)"), "{text}");
        assert!(text.contains("tests/widget_test.c"), "{text}");
        assert!(text.contains("single/lib.h"), "{text}");
        assert!(text.contains("[generated copy]"), "{text}");
        assert!(text.contains("tests 96%"), "{text}");
        assert!(text.contains("BLAME of the changed lines"), "{text}");
        assert!(text.contains("not in local history"), "{text}");
        assert!(!text.contains("leaked_fix"), "{text}");
        assert!(!text.contains("the upstream fix"), "{text}");
        assert!(text.contains(&format!("revisions: {} and its ancestors only", &h0[..10])), "{text}");

        // show refuses a commit outside the ancestry, and shows one inside it.
        let text = call(&t, json!({"mode": "show", "sha": &future[..12]})).await.unwrap();
        assert!(text.contains("is not in the local history"), "{text}");
        assert!(!text.contains("leaked_fix"), "{text}");
        let old = test_repo::git(&root, &["rev-parse", "HEAD~3"]);
        let text = call(&t, json!({"mode": "show", "sha": &old[..12]})).await.unwrap();
        assert!(text.contains("widget: change 20"), "{text}");
        let err = call(&t, json!({"mode": "bogus"})).await.unwrap_err().to_string();
        assert!(err.contains("unknown mode"), "{err}");
    }

    #[tokio::test]
    async fn short_history_degrades_to_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/parser.c"), "int p;\n").unwrap();
        std::fs::write(root.join("tests/parser_test.c"), "int t;\n").unwrap();
        std::fs::write(root.join("NEWS"), "n\n").unwrap();
        let h0 = test_repo::init_and_commit(&root);
        std::fs::write(root.join("src/parser.c"), "int q;\n").unwrap();
        let t = RepoHistoryTool {
            head: SessionHead::ready(Some(Head { sha: h0, root: root.clone() }), root.clone()),
        };
        let text = call(&t, json!({})).await.unwrap();
        assert!(text.contains("History too short"), "{text}");
        assert!(text.contains("tests/parser_test.c"), "{text}");
        assert!(text.contains("changelog: NEWS"), "{text}");
    }
}
