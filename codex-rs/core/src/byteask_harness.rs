//! ByteAsk harness settings: the `[byteask_harness]` config table.
//!
//! Every key is off unless set, except the ones the packaged defaults (`config/defaults.toml`, the
//! lowest config layer) turn on for ByteAsk 0.2.4; config.toml overrides them.
//! Set a key
//! in `config.toml` or per run with `-c byteask_harness.<key>=<value>`:
//!
//! | key | effect |
//! |---|---|
//! | `lean_prefix` (bool) | rarely used tools load on demand through `tool_search`; on by default with the ByteAsk provider |
//! | `defer_tools` (list of names, `*` suffix allowed) | replaces the built-in lean list |
//! | `disable_tools` (list of names, `prefix*` / `*suffix` / `!name` allowed) | those tools are removed from the session: not sent, not in `tool_search` |
//! | `context_url` (string) | the ByteAsk Context endpoint that turns `crate::byteask_context` on (tests, staging) |
//! | `context_auto_index` (bool) | ByteAsk Context may index an unindexed repository without asking |
//! | `context_pack_timeout_secs` (int) | how long the harness waits for ByteAsk Context's turn-0 pack (default 45; packaged default 120) |
//! | `context_lean_tools` (bool) | headless sessions with ByteAsk Context leave out [`CONTEXT_HEADLESS_DISABLED_TOOLS`] (packaged default on) |
//! | `strict_patch` (bool) | `apply_patch` refuses a hunk whose position is ambiguous |
//! | `time_budget_secs` (int) | remaining-time notes at 50% and 75% of the budget; the C/C++ finish gate ends a minute before it |
//! | `constraint_ledger` (bool) | user constraints kept across compaction, checked at finish |
//! | `constraints` (list of strings) | repository constraints added to the ledger |
//! | `context_pack` (bool) | the Context index's turn-0 pack before the first model turn |
//! | `context_finish_check` (bool) | the Context finish check before the agent ends its turn |
//! | `fast` (bool) | the fast profile: a two-line C/C++ note instead of the tool catalog; no checklist, finish gate or the tools they need |
//! | `shared_prefix_cache` (bool) | one prompt-cache key per model (and `prefix_cache_scope`), so sessions read the static prefix from the provider's cache instead of writing it again |
//! | `prefix_cache_scope` (string) | added to the shared key, e.g. a team, so unrelated tenants never share a cache key |
//!
//! Readers call these helpers with the thread's [`Config`]; extensions read the same table.

use std::path::Path;

use codex_config::McpServerTransportConfig;
use codex_protocol::protocol::SessionSource;

use crate::config::Config;

/// Name of the config table.
pub const TABLE: &str = "byteask_harness";

/// Tools deferred by `lean_prefix = true` when no `defer_tools` list is given. C/C++ sessions
/// call these rarely compared with the shell, `cpp_test_select` and `cpp_sanitize`;
/// `autosar_ref` is a lookup tool, deferred like `web.run`. That keeps the
/// prefix of an indexed C/C++ session under 15K tokens. Their definitions are unchanged:
/// `tool_search` returns them verbatim when the model asks, and their names are listed in
/// its description.
pub const LEAN_DEFERRED_TOOLS: &[&str] = &[
    // C/C++ debuggers, binary, performance and formatting tools
    "debug",
    "debug_*",
    "cpp_kdb",
    "cpp_perf*",
    "cpp_bench",
    "cpp_layout",
    "cpp_godbolt",
    "cpp_intrinsic",
    "cpp_insights",
    "cpp_binary",
    "cpp_symbolize",
    "cpp_decompile",
    "cpp_spike_trace",
    "cpp_nav",
    "cpp_repo_history",
    "cpp_fix_verify",
    "cpp_format",
    "port_check",
    // runs automatically after the first C/C++ edit (explicit calls are rare)
    "cpp_impact",
    // grounded reference search over vendor documents and standards
    "*_ref",
    "ref_*",
    // context engine tools other than context_bundle and context_find (at most 4 up front;
    // context_calls is rarely called)
    "context_calls",
    "context_search",
    "context_symbol",
    "context_template_versions",
    "context_preprocessor",
    "context_compile_line",
    "context_pack",
    "context_locate",
    "context_hierarchy",
    "context_example",
    "context_history",
    "context_memory",
    "context_remember",
    // upstream tools (and tool namespaces) unused in C/C++ repair sessions. Interactive tools
    // (request_user_input, view_image, web) stay loaded: Plan mode and interactive users rely on them.
    "collaboration",
    "clock",
    "image_gen",
    "create_goal",
    "get_goal",
    "update_goal",
];

fn table(config: &Config) -> Option<toml::Value> {
    config
        .config_layer_stack
        .effective_config()
        .get(TABLE)
        .cloned()
}

/// The prompt-cache key for `shared_prefix_cache = true`, else `None` (the session id stays the key).
///
/// Every session sends the same tools and instructions first; with one key per session the provider
/// writes that prefix into its cache again for each session (billed above uncached input), with a
/// shared key the second session onwards reads it (a small fraction of the price).
pub fn shared_prefix_cache_key(config: &Config) -> Option<String> {
    if !flag(config, "shared_prefix_cache") {
        return None;
    }
    let scope = table(config)
        .and_then(|t| {
            t.get("prefix_cache_scope")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let model = config.model.as_deref().unwrap_or("default");
    Some(format!("byteask-prefix:{model}:{scope}"))
}

/// A boolean key; absent or not a boolean = `false`.
pub fn flag(config: &Config, key: &str) -> bool {
    table(config)
        .and_then(|t| t.get(key).and_then(toml::Value::as_bool))
        .unwrap_or(false)
}

/// A positive integer key; absent, zero, negative or not an integer = `None`.
pub fn positive_int(config: &Config, key: &str) -> Option<u64> {
    table(config)
        .and_then(|t| t.get(key).and_then(toml::Value::as_integer))
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
}

/// A list of strings; absent or malformed = `None`.
pub fn strings(config: &Config, key: &str) -> Option<Vec<String>> {
    let table = table(config)?;
    let list = table.get(key)?.as_array()?;
    Some(
        list.iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
    )
}

/// The model provider `lean_prefix` defaults to on for.
const LEAN_BY_DEFAULT_PROVIDER: &str = "byteask";

/// The tool-name patterns this session defers, or empty when nothing is deferred.
///
/// `lean_prefix` is on unless set to `false` when the session uses the ByteAsk provider.
///
/// With ByteAsk Context configured (and no `defer_tools` list) the list is
/// [`crate::byteask_context::deferred_tool_patterns`].
pub fn deferred_tool_patterns(config: &Config, session_source: &SessionSource) -> Vec<String> {
    if let Some(list) = strings(config, "defer_tools") {
        return list;
    }
    if crate::byteask_context::shapes_prefix(config) {
        return crate::byteask_context::deferred_tool_patterns(session_source);
    }
    let lean = table(config)
        .and_then(|t| t.get("lean_prefix").and_then(toml::Value::as_bool))
        .unwrap_or(config.model_provider_id == LEAN_BY_DEFAULT_PROVIDER);
    if lean {
        return LEAN_DEFERRED_TOOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    Vec::new()
}

/// `name` matches one of `patterns`: exact, `prefix*`, or `*suffix`; a pattern starting with
/// `!` excludes the names it matches (e.g. `cpp_*` with `!cpp_finish_check`).
pub fn matches_any(patterns: &[String], name: &str) -> bool {
    let matches = |pattern: &str| {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix)
        } else if let Some(suffix) = pattern.strip_prefix('*') {
            name.ends_with(suffix)
        } else {
            name == pattern
        }
    };
    let excluded = patterns
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('!'))
        .any(matches);
    !excluded
        && patterns
            .iter()
            .filter(|pattern| !pattern.starts_with('!'))
            .any(|pattern| matches(pattern))
}

/// `disable_tools`: tool-name patterns (as [`matches_any`]) removed from the session's tool
/// list altogether: neither sent up front nor findable through `tool_search`. Empty by default.
pub fn disabled_tool_patterns(config: &Config) -> Vec<String> {
    strings(config, "disable_tools").unwrap_or_default()
}

/// Key: headless sessions with ByteAsk Context leave out [`CONTEXT_HEADLESS_DISABLED_TOOLS`]
/// (on in the packaged defaults; `false` keeps them).
pub const CONTEXT_LEAN_TOOLS_KEY: &str = "context_lean_tools";

/// Tools a headless session (`exec`, the Cloud Agent) with ByteAsk Context does not offer
/// (ByteAsk 0.2.4): the model does not call them in such runs,
/// and the interactive ones cannot work without a person. Interactive sessions keep
/// them. A `!name` entry in `disable_tools` keeps one.
pub const CONTEXT_HEADLESS_DISABLED_TOOLS: &[&str] = &[
    // debuggers, kernel, performance and binary tools
    "debug",
    "debug_session",
    "debug_attach",
    "debug_reverse",
    "debug_core",
    "cpp_kdb",
    "cpp_bench",
    "cpp_perf",
    "cpp_binary",
    "cpp_layout",
    "cpp_symbolize",
    "cpp_compile_db",
    // other C/C++ tools headless runs do not call
    "cpp_sanitize",
    "cpp_fix_verify",
    "cpp_repo_history",
    "cpp_impact",
    "cpp_nav",
    // native file tools (the model reads and searches through the shell) and interactive tools
    "search",
    "read_file",
    "request_user_input",
    "view_image",
    // MCP resource browsing
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

/// The patterns a session removes: `disable_tools`, plus [`CONTEXT_HEADLESS_DISABLED_TOOLS`] when
/// the session is headless, has ByteAsk Context and `context_lean_tools` is on.
pub fn session_disabled_tool_patterns(config: &Config, source: &SessionSource) -> Vec<String> {
    let mut patterns = disabled_tool_patterns(config);
    if crate::byteask_context::headless(source)
        && crate::byteask_context::gated(config)
        && flag(config, CONTEXT_LEAN_TOOLS_KEY)
    {
        patterns.extend(
            CONTEXT_HEADLESS_DISABLED_TOOLS
                .iter()
                .map(|name| (*name).to_string()),
        );
    }
    patterns
}

/// The server runs the Context engine's MCP adapter: `python -m bctx.cli ... mcp`, a `bctx`
/// entry point, or an installed engine's `serve.py` (which sits beside `python/bctx/cli.py`).
pub fn is_context_engine_mcp(command: &str, args: &[String]) -> bool {
    let module = args
        .windows(2)
        .any(|pair| pair[0] == "-m" && matches!(pair[1].as_str(), "bctx.cli" | "bctx"));
    let entry_point = Path::new(command)
        .file_name()
        .is_some_and(|name| name == "bctx");
    let installed = args.iter().any(|arg| {
        let path = Path::new(arg);
        path.file_name().is_some_and(|name| name == "serve.py")
            && path
                .parent()
                .is_some_and(|dir| dir.join("python").join("bctx").join("cli.py").is_file())
    });
    (module || entry_point || installed) && args.iter().any(|arg| arg == "mcp")
}

/// How a session's MCP server entry starts the Context engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextEngineServer {
    pub command: String,
    pub args: Vec<String>,
    /// The entry's `env`, sorted by name.
    pub env: Vec<(String, String)>,
}

/// The first enabled MCP server, by name, that runs the Context engine (stdio only).
pub fn context_engine_server(config: &Config) -> Option<ContextEngineServer> {
    let servers = config.mcp_servers.get();
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    names.into_iter().find_map(|name| {
        let server = &servers[name];
        match &server.transport {
            McpServerTransportConfig::Stdio {
                command, args, env, ..
            } if server.enabled && is_context_engine_mcp(command, args) => {
                let mut env: Vec<(String, String)> = env
                    .iter()
                    .flatten()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                env.sort();
                Some(ContextEngineServer {
                    command: command.clone(),
                    args: args.clone(),
                    env,
                })
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn patterns_match_exact_prefix_and_suffix() {
        let patterns = pats(&["debug_*", "*_ref", "cpp_kdb"]);
        assert!(matches_any(&patterns, "debug_session"));
        assert!(matches_any(&patterns, "autosar_ref"));
        assert!(matches_any(&patterns, "cpp_kdb"));
        assert!(!matches_any(&patterns, "cpp_kdb2"));
        assert!(!matches_any(&patterns, "exec_command"));
        assert!(!matches_any(&patterns, "debug"));
    }

    /// The tools every C/C++ repair session needs are never in the lean list.
    #[test]
    fn the_lean_list_keeps_the_working_tools() {
        let lean = pats(LEAN_DEFERRED_TOOLS);
        for kept in [
            "exec_command",
            "write_stdin",
            "apply_patch",
            "read_file",
            "search",
            "update_plan",
            "tool_search",
            "cpp_test_select",
            "cpp_check",
            "cpp_tidy",
            "cpp_sanitize",
            "cpp_compile_db",
            "check_status",
            "context_bundle",
            "context_find",
            "context_expand",
            "context_finish_check",
            // interactive tools: Plan mode's question picker, images, web search
            "request_user_input",
            "request_user_input_async",
            "view_image",
            "web",
        ] {
            assert!(!matches_any(&lean, kept), "{kept} must stay up front");
        }
    }
}
