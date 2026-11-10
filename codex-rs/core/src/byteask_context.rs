//! ByteAsk Context in the harness (CLI 0.2.0).
//!
//! **Gate.** Everything here applies only when the session has an enabled MCP server whose URL is
//! ByteAsk Context's (`https://context.byteask.ai/mcp`, what `byteask context connect` writes;
//! host, port and path are compared). For tests and staging, `[byteask_harness] context_url` or
//! the `BYTEASK_CONTEXT_URL` environment variable names another endpoint instead. Without such a
//! server every function here answers "off" and the session is exactly as before.
//!
//! | item | where |
//! |---|---|
//! | turn-0 pack in the user turn | [`turn0_pack`], called by `session::turn::run_turn` |
//! | repository identity filled client-side | [`fill_identity`] (calls), [`model_tool_info`] (schemas) |
//! | the check line: the tests covering place 1, picked locally when the pack names none | [`set_warm_build`] ([`WarmBuildStarted::check_line`]) |
//! | background build of the pack's test programs (or of the check line's) | [`set_warm_build`] |
//! | one prefix for every session | `config` (skills list), `session::world_state` (permissions, exec only), [`drops_prompt_fragment`] (C/C++ environment note) |
//! | lean prefix | [`always_loaded`] (the pack tool up front), [`deferred_tool_patterns`] |
//! | the instruction | `context::ByteAskContextInstructions`, added in `session::mod` |
//! | no ask-first text in exec | [`model_tool_info`] (server instructions), [`headless_tool_text`] (tool answers), auto-index |
//!
//! Further switches (the packaged defaults set some): `[byteask_harness] context_keep_prefix = true` keeps
//! the prefix of a session without Context (no shared or lean prefix: skills list, permissions block, C/C++
//! environment note, the usual tool deferral) while the other items stay; `context_instructions`
//! replaces the instruction text (`""` = no instruction). See [`shapes_prefix`] and [`instructions`].
//! With the gate off, `context_identity_server = "<name>"` gives that MCP server the filled identity alone (see
//! [`identity_server`]). `context_build_markers = true`: a top-level C/C++ build file counts
//! for the pack's C/C++ check (see [`is_cpp_repo_with`]).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_mcp::ToolInfo;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ContextPackEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::ByteAskContextPack;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// ByteAsk Context's hosted MCP endpoint.
pub const PRODUCTION_URL: &str = "https://context.byteask.ai/mcp";
/// `[byteask_harness]` key naming another Context endpoint (tests, staging).
pub const URL_KEY: &str = "context_url";
/// Environment variable naming another Context endpoint (when the config key is not set).
pub const URL_ENV: &str = "BYTEASK_CONTEXT_URL";
/// `[byteask_harness]` key: the user allowed ByteAsk to index an unindexed repository without
/// asking. Written by `byteask context auto-index on`.
pub const AUTO_INDEX_KEY: &str = "context_auto_index";
/// `[byteask_harness]` key: seconds to wait for the check line after the pack (default
/// [`CHECK_LINE_TIMEOUT`]).
pub const CHECK_LINE_TIMEOUT_KEY: &str = "context_check_timeout_secs";
/// `[byteask_harness]` key: keep the prefix of a session without Context (see [`shapes_prefix`]).
pub const KEEP_PREFIX_KEY: &str = "context_keep_prefix";
/// `[byteask_harness]` key: the instruction text instead of the built-in one (`""` = none).
pub const INSTRUCTIONS_KEY: &str = "context_instructions";
/// `[byteask_harness]` key: with the gate off, the filled identity for the MCP server of this name.
pub const IDENTITY_SERVER_KEY: &str = "context_identity_server";
/// `[byteask_harness]` key: seconds to wait for the turn-0 pack (default [`PACK_TIMEOUT`]).
pub const PACK_TIMEOUT_KEY: &str = "context_pack_timeout_secs";

/// The developer instructions of a session with ByteAsk Context when the config sets none
/// (ByteAsk 0.2.4).
pub const WORKING_RULES: &str = "When a task asks you to change code in this repository:
1. Reproduce first. If the task describes behaviour, an example or an expected output, write it as a small program or test in a scratch folder outside the repository, using names exactly as the task gives them, and run it against this checkout before and after your change. Cover the nearby cases the task implies, not only the single example. A failing check means the work is not done: do not finish while it fails.
2. Make the change complete. After fixing one place, look for the other places where the same behaviour lives (overloads, specializations, related functions, the counterpart code path) and keep them consistent. Context's find_related_changes (pass it your diff) and get_symbol list candidates: check each one, and search for any they miss.
3. Work with this repository as it is at this commit: reuse its existing helpers, conventions and names, keep existing APIs rather than redesigning them, and do not rely on APIs remembered from other versions of the code. When the task names a new function, type, member or option, use exactly that name.
4. Before finishing, run cpp_finish_check (in a C/C++ project; otherwise the project's own tests): it builds and runs the existing tests your change reaches. A newly failing existing test is a regression to fix, unless the task's stated behaviour shows that the test encodes the old behaviour; then update the test.
You are authorized to edit this repository locally to resolve the task: contribution guidelines govern submitting changes upstream, not this local change. Reuse a check that already passed for the current change instead of running it again; optional analyzers that are not installed are not required to finish.
If a build or test command is killed, retry it with fewer parallel jobs. Build and run only the test targets your change reaches, not the whole test suite; when one program holds the whole suite, build a smaller runner from only the test files that cover your change.";

pub const PACK_TOOL: &str = "find_code_for_task";
pub const INDEX_TOOL: &str = "index_repository";
pub const LIST_TOOL: &str = "list_repositories";
/// Arguments the harness fills on every Context call and hides from the model.
pub const IDENTITY_PARAMS: &[&str] = &["repo_url", "commit_sha", "worktree", "workdir"];

/// How long the harness waits for the turn-0 pack (server start-up included). The hosted
/// server answers a ranked pack in 2-15 s; 45 s leaves room for a cold start and sign-in refresh
/// without holding a task for long.
pub const PACK_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the first model request waits for the check line: configuring the build tree it
/// names (once per repository and commit) is most of it. Past it the pack goes without the line;
/// the background build goes on.
pub const CHECK_LINE_TIMEOUT: Duration = Duration::from_secs(60);
/// A follow-up message counts as a new task from this length on.
const NEW_TASK_MIN_CHARS: usize = 400;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const INDEX_CALL_TIMEOUT: Duration = Duration::from_secs(20);

// ---- the gate ------------------------------------------------------------------------------

fn harness_table(config: &Config) -> Option<toml::Value> {
    config
        .config_layer_stack
        .effective_config()
        .get(crate::byteask_harness::TABLE)
        .cloned()
}

/// The endpoint that turns the harness on: the config key, else the environment, else production.
pub fn gate_url_from(harness_table: Option<&toml::Value>) -> String {
    harness_table
        .and_then(|table| table.get(URL_KEY))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            std::env::var(URL_ENV)
                .ok()
                .filter(|url| !url.trim().is_empty())
        })
        .unwrap_or_else(|| PRODUCTION_URL.to_string())
}

/// Same host, port and path (a trailing `/` ignored); the scheme's default port counts.
pub fn same_endpoint(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(a.trim()), url::Url::parse(b.trim())) else {
        return false;
    };
    a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
        && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
}

/// The first enabled MCP server, by name, at `gate_url`.
pub fn server_name_in(
    servers: &HashMap<String, McpServerConfig>,
    gate_url: &str,
) -> Option<String> {
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    names.into_iter().find_map(|name| {
        let server = &servers[name];
        match &server.transport {
            McpServerTransportConfig::StreamableHttp { url, .. }
                if server.enabled && same_endpoint(url, gate_url) =>
            {
                Some(name.clone())
            }
            _ => None,
        }
    })
}

/// The session's ByteAsk Context server, or `None` (the gate is off).
pub fn server_name(config: &Config) -> Option<String> {
    let gate_url = gate_url_from(harness_table(config).as_ref());
    server_name_in(config.mcp_servers.get(), &gate_url)
}

pub fn gated(config: &Config) -> bool {
    server_name(config).is_some()
}

/// The MCP server whose identity is filled and whose identity arguments are hidden: the gate's server,
/// else the enabled server named by `context_identity_server`.
pub fn identity_server(config: &Config) -> Option<String> {
    server_name(config).or_else(|| {
        let table = harness_table(config)?;
        let name = table.get(IDENTITY_SERVER_KEY)?.as_str()?;
        config
            .mcp_servers
            .get()
            .get(name)
            .is_some_and(|server| server.enabled)
            .then(|| name.to_string())
    })
}

/// The shared prefix and the lean prefix apply: the gate is on and `context_keep_prefix` is not set.
pub fn shapes_prefix(config: &Config) -> bool {
    gated(config) && !crate::byteask_harness::flag(config, KEEP_PREFIX_KEY)
}

/// The built-in instruction, or the `context_instructions` text when set (`""` = no instruction).
pub fn instructions(config: &Config) -> String {
    harness_table(config)
        .and_then(|table| {
            table
                .get(INSTRUCTIONS_KEY)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::context::BYTEASK_CONTEXT_INSTRUCTIONS.to_string())
}

/// `exec` and the MCP server mode: no one can answer a question during the run.
pub fn headless(source: &SessionSource) -> bool {
    matches!(source, SessionSource::Exec | SessionSource::Mcp)
}

fn auto_index(config: &Config) -> bool {
    crate::byteask_harness::flag(config, AUTO_INDEX_KEY)
}

// ---- The shared prefix and the lean prefix ---------------------------------------------------

/// Prompt fragments left out under the gate because they differ between workspaces.
pub fn drops_prompt_fragment(content_kind: &str) -> bool {
    // cpp-tools' environment note names the workspace's compilers and paths.
    content_kind == "cpp_tools.environment"
}

/// Tools deferred behind `tool_search` under the gate when no `defer_tools` list is given.
/// Every `cpp_*` tool except the
/// verify tool goes; the Context server's own tools are deferred by the MCP exposure policy, all
/// but the pack tool (see [`always_loaded`]).
const GATED_DEFERRED_TOOLS: &[&str] = &[
    "debug",
    "debug_*",
    "cpp_*",
    "!cpp_finish_check",
    "port_check",
    "*_ref",
    "ref_*",
    "collaboration",
    "clock",
    "image_gen",
    "create_goal",
    "get_goal",
    "update_goal",
    "check_status",
];

/// Interactive tools, deferred only in exec (an interactive user
/// relies on them, e.g. Plan mode's question picker).
const GATED_HEADLESS_DEFERRED_TOOLS: &[&str] = &[
    "web",
    "view_image",
    "request_user_input",
    "request_user_input_async",
];

/// The tool-name patterns deferred under the gate.
pub fn deferred_tool_patterns(source: &SessionSource) -> Vec<String> {
    let mut patterns: Vec<String> = GATED_DEFERRED_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if headless(source) {
        patterns.extend(
            GATED_HEADLESS_DEFERRED_TOOLS
                .iter()
                .map(|s| (*s).to_string()),
        );
    }
    patterns
}

/// A Context tool sent up front (never behind `tool_search`): the one the server marks
/// `anthropic/alwaysLoad`, and the pack tool whatever the server says.
pub fn always_loaded(tool: &rmcp::model::Tool) -> bool {
    tool.name == PACK_TOOL
        || tool
            .meta
            .as_ref()
            .and_then(|meta| meta.0.get("anthropic/alwaysLoad"))
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
}

// ---- What the model sees of the Context server --------------------------------------

/// The Context server's tool as the model sees it: the identity arguments gone from the schema
/// and, in exec, the server instructions' ask-before-indexing paragraph replaced.
pub fn model_tool_info(tool: &ToolInfo, headless: bool) -> ToolInfo {
    let mut tool = tool.clone();
    let mut schema = JsonValue::Object((*tool.tool.input_schema).clone());
    strip_identity(&mut schema);
    if let JsonValue::Object(map) = schema {
        tool.tool.input_schema = Arc::new(map);
    }
    if headless {
        tool.namespace_description = tool
            .namespace_description
            .as_deref()
            .map(headless_instructions);
    }
    tool
}

/// Removes [`IDENTITY_PARAMS`] from a tool's input schema: not listed, not required.
pub fn strip_identity(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(JsonValue::Object(properties)) = object.get_mut("properties") {
        for param in IDENTITY_PARAMS {
            properties.remove(*param);
        }
    }
    if let Some(JsonValue::Array(required)) = object.get_mut("required") {
        required.retain(|name| {
            !name
                .as_str()
                .is_some_and(|name| IDENTITY_PARAMS.contains(&name))
        });
    }
}

const HEADLESS_NOT_INDEXED: &str = "- If a tool says the repository is not indexed, do the task \
without ByteAsk; do not stop to ask about indexing (no one can answer during this run).";

/// The server instructions for a run no one can answer: every line that tells the model to ask
/// about indexing becomes [`HEADLESS_NOT_INDEXED`].
pub fn headless_instructions(text: &str) -> String {
    static ASK: OnceLock<regex_lite::Regex> = OnceLock::new();
    let ask = ASK.get_or_init(|| {
        regex_lite::Regex::new(r"(?i)\bask\b").unwrap_or_else(|err| panic!("ask regex: {err}"))
    });
    let mut replaced = false;
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            if line.to_ascii_lowercase().contains("index") && ask.is_match(line) {
                if replaced {
                    return None;
                }
                replaced = true;
                return Some(HEADLESS_NOT_INDEXED);
            }
            Some(line)
        })
        .collect();
    lines.join("\n")
}

/// A Context tool answer for a run no one can answer. The not-indexed answer tells the model
/// to stop and ask; it becomes a plain "continue without ByteAsk". `None` = keep the answer.
pub fn headless_tool_text(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let not_indexed = trimmed.starts_with("STOP:") && trimmed.contains("not indexed");
    not_indexed.then(|| {
        "ByteAsk has not indexed this repository: do the task without ByteAsk, and do not stop \
to ask about indexing (no one can answer during this run)."
            .to_string()
    })
}

/// The repository a session works in, from git.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoIdentity {
    pub repo_url: Option<String>,
    pub commit_sha: Option<String>,
    pub worktree: Option<String>,
    /// The session's working directory (absolute).
    pub workdir: String,
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `git remote get-url origin`, `git rev-parse HEAD` and `git rev-parse --show-toplevel` in `cwd`.
pub async fn repo_identity(cwd: &Path) -> RepoIdentity {
    let (repo_url, commit_sha, worktree) = tokio::join!(
        git(cwd, &["remote", "get-url", "origin"]),
        git(cwd, &["rev-parse", "HEAD"]),
        git(cwd, &["rev-parse", "--show-toplevel"]),
    );
    RepoIdentity {
        repo_url,
        commit_sha,
        worktree,
        workdir: cwd.to_string_lossy().into_owned(),
    }
}

/// The identity arguments of one Context call, filled from `identity` (any value the model
/// passed is replaced). `list_repositories` takes none. `workdir` is passed only without a
/// `repo_url` (the server resolves the repository from one of them; both naming different
/// repositories is an error there).
pub fn fill_identity(
    tool: &str,
    arguments: &mut serde_json::Map<String, JsonValue>,
    identity: &RepoIdentity,
) {
    if tool == LIST_TOOL {
        return;
    }
    let mut set = |key: &str, value: &Option<String>| match value {
        Some(value) => {
            arguments.insert(key.to_string(), JsonValue::String(value.clone()));
        }
        None => {
            arguments.remove(key);
        }
    };
    set("repo_url", &identity.repo_url);
    set("commit_sha", &identity.commit_sha);
    set("worktree", &identity.worktree);
    let workdir = identity
        .repo_url
        .is_none()
        .then(|| identity.workdir.clone());
    set("workdir", &workdir);
}

// ---- The turn-0 pack -----------------------------------------------------------------------

/// Per-thread state of the turn-0 pack.
#[derive(Default)]
struct PackState {
    /// A user message has been seen (the next one is no longer the first).
    seen_first: AtomicBool,
    /// Pre-approved indexing was started once in this thread.
    index_started: AtomicBool,
    /// Background builds started for packs, cancelled when the thread goes away.
    warm_builds: std::sync::Mutex<Vec<WarmBuildCancel>>,
}

impl Drop for PackState {
    fn drop(&mut self) {
        let builds = match self.warm_builds.get_mut() {
            Ok(builds) => std::mem::take(builds),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        for cancel in builds {
            cancel();
        }
    }
}

/// Is this user message a task? The first one is; a later one is when it is long or carries
/// code, an error or a stack frame.
pub fn is_task(text: &str, first: bool) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if first || text.chars().count() >= NEW_TASK_MIN_CHARS || text.contains("```") {
        return true;
    }
    static TASK_MARKS: OnceLock<regex_lite::Regex> = OnceLock::new();
    let marks = TASK_MARKS.get_or_init(|| {
        regex_lite::Regex::new(concat!(
            // a source location: file.cpp:123
            r"\b[\w./-]+\.(?:c|cc|cpp|cxx|c\+\+|h|hh|hpp|hxx|ipp|inl|tcc):\d+",
            // a stack frame (gdb, sanitizers, backtraces)
            r"|#\d+\s+0x[0-9a-fA-F]+",
            r"|\bat [\w./-]+:\d+",
            // compiler, linker, sanitizer and test-failure output
            r"|\b(?:error|fatal error|undefined reference|Assertion|assertion failed)\b",
            r"|Segmentation fault|SIGSEGV|SIGABRT|AddressSanitizer|UndefinedBehaviorSanitizer",
            r"|ThreadSanitizer|LeakSanitizer|terminate called|core dumped|\[  FAILED  \]",
            // a code line: a statement or a block opening
            r"|;\s*$|\{\s*$",
        ))
        .unwrap_or_else(|err| panic!("task regex: {err}"))
    });
    text.lines().any(|line| marks.is_match(line))
}

const CPP_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "h", "hh", "hpp", "hxx", "ipp", "inl", "tcc",
];

/// Top-level files that make a repository count as C/C++ under `context_build_markers`.
const BUILD_MARKERS: &[&str] = &[
    "CMakeLists.txt",
    "meson.build",
    "configure.ac",
    "compile_commands.json",
];
/// `[byteask_harness]` key: [`BUILD_MARKERS`] count (sources 4+ folders deep).
pub const BUILD_MARKERS_KEY: &str = "context_build_markers";

/// A C or C++ repository: a C/C++ source or header within three levels of the top (build output,
/// dependency and hidden folders skipped; at most 4000 entries looked at), or with `markers` a
/// top-level C/C++ build file (webern/mx: its first source is four folders down, behind
/// thousands of fixture files).
pub fn is_cpp_repo_with(top: &Path, markers: bool) -> bool {
    (markers && BUILD_MARKERS.iter().any(|name| top.join(name).is_file())) || is_cpp_repo(top)
}

/// [`is_cpp_repo_with`] without the build markers.
pub fn is_cpp_repo(top: &Path) -> bool {
    let mut budget = 4000usize;
    let mut queue = std::collections::VecDeque::from([(top.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let skip = name.starts_with('.')
                    || matches!(
                        name.as_ref(),
                        "node_modules" | "target" | "build" | "out" | "venv" | "__pycache__"
                    );
                if !skip && depth < 2 {
                    queue.push_back((entry.path(), depth + 1));
                }
            } else if Path::new(name.as_ref())
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| CPP_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
            {
                return true;
            }
        }
    }
    false
}

/// How a pack call ended.
#[derive(Debug, PartialEq, Eq)]
pub enum PackAnswer {
    Pack(String),
    NotIndexed(String),
    Indexing,
    Error(String),
}

/// Sorts a `find_code_for_task` answer: success, not indexed (with the server's text), being
/// indexed, or another error.
pub fn classify_answer(is_error: bool, text: String) -> PackAnswer {
    if !is_error {
        return if text.trim().is_empty() {
            PackAnswer::Error("empty answer".to_string())
        } else {
            PackAnswer::Pack(text)
        };
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("STOP:") && trimmed.contains("not indexed") {
        PackAnswer::NotIndexed(text)
    } else if trimmed.contains(" is being indexed") {
        PackAnswer::Indexing
    } else {
        PackAnswer::Error(first_line(&text))
    }
}

fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    let mut short: String = line.chars().take(160).collect();
    if short.len() < line.len() {
        short.push_str("...");
    }
    short
}

/// The test programs a pack names as exercising its top places
/// ("Test programs that exercise 1-2: a, b (of N in this build).").
pub fn test_programs(pack: &str) -> Vec<String> {
    static LINE: OnceLock<regex_lite::Regex> = OnceLock::new();
    let line = LINE.get_or_init(|| {
        regex_lite::Regex::new(r"Test programs that exercise [^:]*: (.+?) \(of \d+ in this build\)")
            .unwrap_or_else(|err| panic!("test programs regex: {err}"))
    });
    let mut programs = Vec::new();
    for captures in line.captures_iter(pack) {
        for program in captures[1].split(", ") {
            let program = program.trim().to_string();
            if !program.is_empty() && !programs.contains(&program) {
                programs.push(program);
            }
        }
    }
    programs
}

/// Cancels a background build when called.
pub type WarmBuildCancel = Box<dyn FnOnce() + Send + Sync>;

/// One command that builds and runs the tests covering the pack's place 1, and the build
/// targets it names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckLine {
    pub line: String,
    pub targets: Vec<String>,
}

/// A started background build.
pub struct WarmBuildStarted {
    pub cancel: WarmBuildCancel,
    /// Resolves to the check line once the build tree is configured, when the pack named no
    /// test programs and the session may run the line's commands unasked.
    pub check_line:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = Option<CheckLine>> + Send>>>,
}

/// Starts a background build of `test_programs` in `workspace` (verify's own build tree),
/// when the session may run the project's build; with no `test_programs`, of the tests that
/// cover the place `pack` names first, found from the project's build. Returns how to cancel it
/// (and the check line), or `None` when nothing was started. Registered by the host once (the
/// C/C++ tools own the build tree).
pub type WarmBuild = fn(
    config: &Config,
    workspace: &Path,
    test_programs: &[String],
    pack: &str,
) -> Option<WarmBuildStarted>;

static WARM_BUILD: OnceLock<WarmBuild> = OnceLock::new();

/// Registers the background build (the first registration wins).
pub fn set_warm_build(start: WarmBuild) {
    let _ = WARM_BUILD.set(start);
}

fn start_warm_build(
    config: &Config,
    workspace: &Path,
    programs: &[String],
    pack: &str,
) -> Option<WarmBuildStarted> {
    WARM_BUILD
        .get()
        .and_then(|start| start(config, workspace, programs, pack))
}

fn text_of(result: &codex_protocol::mcp::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| {
            (item.get("type").and_then(JsonValue::as_str) == Some("text"))
                .then(|| item.get("text").and_then(JsonValue::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One Context tool call by the harness (not the model): `Ok((is_error, text))`.
async fn call_context_tool(
    sess: &Arc<Session>,
    server: &str,
    tool: &str,
    arguments: serde_json::Map<String, JsonValue>,
    timeout: Duration,
) -> Result<(bool, String), String> {
    let call = async {
        let prepared = sess
            .prepare_mcp_call_by_name(server, tool)
            .await
            .ok_or_else(|| {
                "ByteAsk Context is not connected (byteask context connect)".to_string()
            })?;
        let result = prepared
            // The outer timeout below is the one that counts (and is reported as a timeout).
            .call(Some(JsonValue::Object(arguments)), None, None)
            .await
            .map_err(|err| first_line(&err.to_string()))?;
        Ok((result.is_error.unwrap_or(false), text_of(&result)))
    };
    tokio::pin!(call);
    match tokio::time::timeout(timeout, &mut call).await {
        Ok(result) => result,
        // `BYTEASK_RELATED_GRACE_SECS`: a related answer that arrived while the runtime was busy is still taken
        Err(_) => match (tool == RELATED_TOOL).then(related_grace).flatten() {
            Some(grace) => match tokio::time::timeout(grace, &mut call).await {
                Ok(result) => result,
                Err(_) => Err("timeout".to_string()),
            },
            None => Err("timeout".to_string()),
        },
    }
}

/// The user's text in this turn's input.
fn input_text(user_input: &[UserInput]) -> String {
    user_input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The turn-0 pack for this turn's user message, as an item for the user turn, or `None`.
///
/// Under the gate, in a root thread, for a message that is a task in a C/C++ git repository: the
/// harness calls `find_code_for_task` with the message verbatim and the repository's identity,
/// and returns the answer as a user-role item recorded right after the message, before the first
/// model request. A timeout, an unindexed repository, a sign-in problem or any error injects
/// nothing; the user sees one line. Every attempt is reported as a [`ContextPackEvent`] and
/// logged.
pub(crate) async fn turn0_pack(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    user_input: &[UserInput],
    cancellation_token: &CancellationToken,
) -> Option<ResponseItem> {
    let config = turn_context.config.as_ref();
    let server = server_name(config)?;
    if matches!(
        turn_context.session_source,
        SessionSource::SubAgent(_) | SessionSource::Internal(_)
    ) || (config.ephemeral && !config.features.enabled(codex_features::Feature::ShellTool))
    {
        // Sub-agents, internal sessions and upstream's tool-less structured threads.
        return None;
    }
    let text = input_text(user_input);
    if text.trim().is_empty() {
        return None;
    }
    let state = sess
        .services
        .thread_extension_data
        .get_or_init(PackState::default);
    let first = !state.seen_first.swap(true, Ordering::SeqCst);
    let timeout = crate::byteask_harness::positive_int(config, PACK_TIMEOUT_KEY)
        .map(Duration::from_secs)
        .unwrap_or(PACK_TIMEOUT);
    if first {
        // The Context tools are in the very first request (not added once the server has
        // started), whatever this message is.
        let _ = tokio::time::timeout(timeout, sess.wait_for_mcp_server(&server)).await;
    }
    let mut event = ContextPackEvent::default();
    let item = async {
        if !is_task(&text, first) {
            event.reason = Some("not_a_task".to_string());
            return None;
        }
        #[allow(deprecated)]
        let cwd = turn_context.cwd.to_path_buf();
        let identity = repo_identity(&cwd).await;
        let Some(top) = identity.worktree.clone() else {
            event.reason = Some("not_git_repo".to_string());
            return None;
        };
        let top = PathBuf::from(top);
        let markers = crate::byteask_harness::flag(&turn_context.config, BUILD_MARKERS_KEY);
        let cpp = tokio::task::spawn_blocking({
            let top = top.clone();
            move || is_cpp_repo_with(&top, markers)
        })
        .await
        .unwrap_or(false);
        if !cpp {
            event.reason = Some("not_cpp_repo".to_string());
            return None;
        }
        // The C/C++ tools' check asks this server which tests a change reaches (select_tests).
        register_select_tests(sess, &server, &top);
        // ... and, when switched on, which related places the change left alone.
        if crate::byteask_harness::flag(&turn_context.config, FINISH_RELATED_KEY) {
            register_related_changes(sess, &server, &top);
        }
        let mut arguments = serde_json::Map::new();
        arguments.insert("task".to_string(), JsonValue::String(text.clone()));
        fill_identity(PACK_TOOL, &mut arguments, &identity);
        event.attempted = true;
        let started = Instant::now();
        let answer = call_context_tool(sess, &server, PACK_TOOL, arguments, timeout).await;
        event.latency_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        let answer = match answer {
            Ok((is_error, text)) => {
                if !is_error {
                    let bytes = text.len() as u64;
                    event.answer_bytes = Some(bytes);
                    event.approx_tokens = Some(bytes.div_ceil(4));
                }
                classify_answer(is_error, text)
            }
            Err(err) if err == "timeout" => {
                event.reason = Some("timeout".to_string());
                event.message = Some(format!(
                    "ByteAsk Context did not answer within {} s; continuing without its code pack.",
                    timeout.as_secs()
                ));
                return None;
            }
            Err(err) => PackAnswer::Error(err),
        };
        match answer {
            PackAnswer::Pack(mut pack) => {
                event.injected = true;
                event.test_programs = test_programs(&pack);
                if let Some(started) = start_warm_build(config, &top, &event.test_programs, &pack)
                {
                    event.warm_build_started = true;
                    if let Ok(mut builds) = state.warm_builds.lock() {
                        builds.push(started.cancel);
                    }
                    // The pack named no test programs; the line names the ones picked
                    // locally (reported as the event's test_programs), as the pack's last line.
                    if let Some(check_line) = started.check_line {
                        let wait = crate::byteask_harness::positive_int(
                            config,
                            CHECK_LINE_TIMEOUT_KEY,
                        )
                        .map(Duration::from_secs)
                        .unwrap_or(CHECK_LINE_TIMEOUT);
                        let line = tokio::select! {
                            line = tokio::time::timeout(wait, check_line) => line.ok().flatten(),
                            _ = cancellation_token.cancelled() => None,
                        };
                        match line {
                            Some(line) => {
                                pack.push_str("\n\n");
                                pack.push_str(&line.line);
                                event.test_programs = line.targets;
                            }
                            None => tracing::info!(
                                target: "byteask_context",
                                "check line: none within {} s",
                                wait.as_secs()
                            ),
                        }
                    }
                }
                Some(ContextualUserFragment::into(ByteAskContextPack::new(pack)))
            }
            PackAnswer::NotIndexed(_) => {
                event.reason = Some("not_indexed".to_string());
                if auto_index(config) {
                    let outcome = if state.index_started.swap(true, Ordering::SeqCst) {
                        "already"
                    } else {
                        start_indexing(sess, config, &server, &identity, &top).await
                    };
                    event.auto_index = Some(outcome.to_string());
                    event.message = Some(match outcome {
                        "failed" => "ByteAsk Context: this repository is not indexed and automatic indexing could not start (see the log); continuing without its code pack.".to_string(),
                        _ => "ByteAsk Context: this repository is not indexed yet; indexing it in the background. Continuing without its code pack.".to_string(),
                    });
                } else {
                    event.message = Some(
                        "ByteAsk Context: this repository is not indexed yet; continuing without its code pack (index it in an interactive byteask session, or allow it with: byteask context auto-index on)."
                            .to_string(),
                    );
                }
                None
            }
            PackAnswer::Indexing => {
                event.reason = Some("indexing".to_string());
                event.message = Some(
                    "ByteAsk Context: this repository is still being indexed; continuing without its code pack."
                        .to_string(),
                );
                None
            }
            PackAnswer::Error(err) => {
                event.reason = Some("error".to_string());
                event.message = Some(format!(
                    "ByteAsk Context: no code pack for this task ({err}); continuing without it."
                ));
                None
            }
        }
    }
    .await;
    if cancellation_token.is_cancelled() {
        return None;
    }
    tracing::info!(
        target: "byteask_context",
        "turn-0 pack: {}",
        serde_json::to_string(&event).unwrap_or_default()
    );
    sess.send_event(turn_context, EventMsg::ContextPack(event))
        .await;
    item
}

// ---- select_tests for the C/C++ tools' check --------------------------------------------------

/// Context's `select_tests` for one repository: the change in ([`SelectInput`]), its JSON answer
/// out (`confidence`, `tests`, `also`, `build_targets`, `stale`, `truncated`, ...), or why there is
/// none. `cpp_finish_check` uses it, when registered, to choose the tests it builds and runs.
pub type SelectTestsFn = Arc<
    dyn Fn(
            SelectInput,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>>
        + Send
        + Sync,
>;

/// What `select_tests` is asked about: the change as a unified diff (preferred, line-precise), or
/// only the changed files (repo-relative; whole files select a little wider, never narrower).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectInput {
    Diff(String),
    Files(Vec<String>),
}

pub const SELECT_TOOL: &str = "select_tests";
/// How long the check waits for `select_tests` (it is model-free; a slow answer means "select locally").
const SELECT_TIMEOUT: Duration = Duration::from_secs(20);

static SELECT_TESTS: OnceLock<std::sync::Mutex<HashMap<PathBuf, SelectTestsFn>>> = OnceLock::new();

/// Registers `select` for the repository whose top-level directory is `root` (the last
/// registration wins). The turn-0 pack registers the session's Context server; tests may register
/// a stand-in.
pub fn register_select_tests_fn(root: &Path, select: SelectTestsFn) {
    let Ok(mut map) = SELECT_TESTS.get_or_init(Default::default).lock() else {
        return;
    };
    if let Ok(canonical) = std::fs::canonicalize(root) {
        map.insert(canonical, Arc::clone(&select));
    }
    map.insert(root.to_path_buf(), select);
}

/// The `select_tests` registered for the repository at `root`, if any.
pub fn select_tests_for(root: &Path) -> Option<SelectTestsFn> {
    let map = SELECT_TESTS.get()?.lock().ok()?;
    map.get(root).cloned().or_else(|| {
        let canonical = std::fs::canonicalize(root).ok()?;
        map.get(&canonical).cloned()
    })
}

fn register_select_tests(sess: &Arc<Session>, server: &str, top: &Path) {
    // Weak: the registry must not keep an ended session (and its MCP connections) alive.
    let session = Arc::downgrade(sess);
    let server = server.to_string();
    let root = top.to_path_buf();
    let select: SelectTestsFn = Arc::new(move |input: SelectInput| {
        let session = session.clone();
        let server = server.clone();
        let root = root.clone();
        Box::pin(async move {
            let sess = session
                .upgrade()
                .ok_or_else(|| "the session has ended".to_string())?;
            let arguments = change_arguments(SELECT_TOOL, input, &root).await;
            let (is_error, text) =
                call_context_tool(&sess, &server, SELECT_TOOL, arguments, SELECT_TIMEOUT).await?;
            if is_error {
                return Err(first_line(&text));
            }
            serde_json::from_str(&text).map_err(|err| format!("answer is not JSON ({err})"))
        })
    });
    register_select_tests_fn(top, select);
}

/// A change-describing Context call's arguments: `diff` or `changed_files`, and the identity.
async fn change_arguments(
    tool: &str,
    input: SelectInput,
    root: &Path,
) -> serde_json::Map<String, JsonValue> {
    let identity = repo_identity(root).await;
    let mut arguments = serde_json::Map::new();
    match input {
        SelectInput::Diff(diff) => {
            arguments.insert("diff".to_string(), JsonValue::String(diff));
        }
        SelectInput::Files(files) => {
            arguments.insert(
                "changed_files".to_string(),
                JsonValue::Array(files.into_iter().map(JsonValue::String).collect()),
            );
        }
    }
    fill_identity(tool, &mut arguments, &identity);
    arguments
}

// ---- find_related_changes for the C/C++ tools' check ----------------------------

/// `[byteask_harness]` key (on in the packaged defaults): `cpp_finish_check` lists the related
/// places Context's `find_related_changes` names for the change (siblings, overloads, the other
/// half of a declaration/definition pair, callers) under "Related places not changed".
pub const FINISH_RELATED_KEY: &str = "finish_related";
pub const RELATED_TOOL: &str = "find_related_changes";
/// How long the check waits for `find_related_changes` (it runs beside the build and tests).
pub const RELATED_TIMEOUT: Duration = Duration::from_secs(15);

/// `BYTEASK_RELATED_TIMEOUT_SECS` (1..=600): the wait for `find_related_changes`, end to end; else
/// [`RELATED_TIMEOUT`].
pub fn related_timeout() -> Duration {
    std::env::var("BYTEASK_RELATED_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| (1..=600).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or(RELATED_TIMEOUT)
}

/// `BYTEASK_RELATED_GRACE_SECS` (1..=60): extra seconds the harness's `find_related_changes` call (and the
/// check's wait for its places) allows after its deadline, for an answer that is ready but not yet processed.
pub fn related_grace() -> Option<Duration> {
    std::env::var("BYTEASK_RELATED_GRACE_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| (1..=60).contains(secs))
        .map(Duration::from_secs)
}

// ---- Switch `BYTEASK_FINISH_REVIEW_OWN_TESTS`: the agent's finished commands ----------

/// One of the agent's shell commands finished: its text, whether one of its simple commands builds and one runs
/// tests (the headless runner's classification), its exit code and its output. Registered by the C/C++ tools.
pub type AgentCommandHook =
    fn(command: &str, builds: bool, tests: bool, exit_code: Option<i32>, output: &str);

static AGENT_COMMAND_HOOK: OnceLock<AgentCommandHook> = OnceLock::new();

/// Registers the agent-command hook (the first registration wins).
pub fn set_agent_command_hook(hook: AgentCommandHook) {
    let _ = AGENT_COMMAND_HOOK.set(hook);
}

/// The headless runner calls this for each of the agent's finished shell commands that builds or runs tests;
/// nothing happens without a registered hook.
pub fn note_agent_command(
    command: &str,
    builds: bool,
    tests: bool,
    exit_code: Option<i32>,
    output: &str,
) {
    if let Some(hook) = AGENT_COMMAND_HOOK.get() {
        hook(command, builds, tests, exit_code, output);
    }
}

/// Context's `find_related_changes` for one repository: the change in, the answer's text out (or
/// why there is none).
pub type RelatedChangesFn = Arc<
    dyn Fn(
            SelectInput,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

static RELATED_CHANGES: OnceLock<std::sync::Mutex<HashMap<PathBuf, RelatedChangesFn>>> =
    OnceLock::new();

/// Registers `related` for the repository whose top-level directory is `root` (the last
/// registration wins); tests may register a stand-in.
pub fn register_related_changes_fn(root: &Path, related: RelatedChangesFn) {
    let Ok(mut map) = RELATED_CHANGES.get_or_init(Default::default).lock() else {
        return;
    };
    if let Ok(canonical) = std::fs::canonicalize(root) {
        map.insert(canonical, Arc::clone(&related));
    }
    map.insert(root.to_path_buf(), related);
}

/// The `find_related_changes` registered for the repository at `root`, if any.
pub fn related_changes_for(root: &Path) -> Option<RelatedChangesFn> {
    let map = RELATED_CHANGES.get()?.lock().ok()?;
    map.get(root).cloned().or_else(|| {
        let canonical = std::fs::canonicalize(root).ok()?;
        map.get(&canonical).cloned()
    })
}

fn register_related_changes(sess: &Arc<Session>, server: &str, top: &Path) {
    let session = Arc::downgrade(sess);
    let server = server.to_string();
    let root = top.to_path_buf();
    let related: RelatedChangesFn = Arc::new(move |input: SelectInput| {
        let session = session.clone();
        let server = server.clone();
        let root = root.clone();
        Box::pin(async move {
            let sess = session
                .upgrade()
                .ok_or_else(|| "the session has ended".to_string())?;
            let arguments = change_arguments(RELATED_TOOL, input, &root).await;
            let (is_error, text) =
                call_context_tool(&sess, &server, RELATED_TOOL, arguments, related_timeout())
                    .await?;
            if is_error {
                return Err(first_line(&text));
            }
            Ok(text)
        })
    });
    register_related_changes_fn(top, related);
}

// ---- Pre-approved indexing ---------------------------------------------------------------

/// The parts of an `index_repository` answer the background indexing needs: the service URL,
/// the collect script's published digest and the one-time upload key.
#[derive(Debug, PartialEq, Eq)]
pub struct IndexSteps {
    pub service_url: String,
    pub script_sha256: String,
    pub key: String,
}

pub fn parse_index_steps(text: &str) -> Option<IndexSteps> {
    let url =
        regex_lite::Regex::new(r"curl -fsSL (https?://[A-Za-z0-9.:\-/_]+?)/v1/collect\.py").ok()?;
    let sha = regex_lite::Regex::new(r"!= '([0-9a-f]{64})'").ok()?;
    let key = regex_lite::Regex::new(r"BYTEASK_KEY=(bck_[A-Za-z0-9_\-]+)").ok()?;
    Some(IndexSteps {
        service_url: url.captures(text)?[1].to_string(),
        script_sha256: sha.captures(text)?[1].to_string(),
        key: key.captures(text)?[1].to_string(),
    })
}

/// The steps `index_repository` lists, without questions (the user pre-approved indexing):
/// download the collect script, check its published digest, preview, upload what the preview
/// listed. Values come in through the environment, never into the script text.
const INDEX_SCRIPT: &str = r#"set -eu
dir="$HOME/.cache/byteask-context"
mkdir -p "$dir"
curl -fsSL "$BYTEASK_CTX_URL/v1/collect.py" -o "$dir/collect.py"
python3 -c 'import hashlib,sys; sys.exit(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest() != sys.argv[2])' "$dir/collect.py" "$BYTEASK_CTX_SHA"
pid=$(python3 "$dir/collect.py" preview | sed -n 's/^Preview id \([0-9a-f]*\).*/\1/p' | head -n 1)
test -n "$pid"
BYTEASK_KEY="$BYTEASK_CTX_KEY" python3 "$dir/collect.py" upload --preview "$pid"
"#;

/// Starts indexing the repository in the background: `started`, `already` (the server is already
/// indexing it) or `failed`.
async fn start_indexing(
    sess: &Arc<Session>,
    config: &Config,
    server: &str,
    identity: &RepoIdentity,
    top: &Path,
) -> &'static str {
    let mut arguments = serde_json::Map::new();
    fill_identity(INDEX_TOOL, &mut arguments, identity);
    let answer = call_context_tool(sess, server, INDEX_TOOL, arguments, INDEX_CALL_TIMEOUT).await;
    let text = match answer {
        Ok((false, text)) => text,
        Ok((true, text)) | Err(text) => {
            tracing::warn!(target: "byteask_context", "index_repository failed: {}", first_line(&text));
            return "failed";
        }
    };
    if text.contains("is being indexed now") {
        return "already";
    }
    let Some(steps) = parse_index_steps(&text) else {
        tracing::warn!(target: "byteask_context", "index_repository: steps not understood");
        return "failed";
    };
    let log_path = config.log_dir.join("byteask-context-index.log");
    let log = std::fs::create_dir_all(&config.log_dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
    });
    let Ok(log) = log else {
        return "failed";
    };
    let Ok(log_err) = log.try_clone() else {
        return "failed";
    };
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(INDEX_SCRIPT)
        .current_dir(top)
        .env("BYTEASK_CTX_URL", &steps.service_url)
        .env("BYTEASK_CTX_SHA", &steps.script_sha256)
        .env("BYTEASK_CTX_KEY", &steps.key)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: the run ending (or Ctrl-C) does not stop the upload.
        command.process_group(0);
    }
    match command.spawn() {
        Ok(_child) => {
            tracing::info!(target: "byteask_context", "indexing started in the background; log: {}", log_path.display());
            "started"
        }
        Err(err) => {
            tracing::warn!(target: "byteask_context", "indexing could not start: {err}");
            "failed"
        }
    }
}

#[cfg(test)]
#[path = "byteask_context_tests.rs"]
mod tests;
