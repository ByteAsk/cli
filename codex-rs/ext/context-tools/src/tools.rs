//! The context-engine tools, as the engine's tool registry sees them.
//!
//! Names, tool descriptions and parameter descriptions are the context
//! engine's own (`bctx/rpc.py` `TOOLS`, pinned there by
//! `tests/snapshots/tools.json`, copied here as `snapshots/bctx_tools.json`);
//! `cross_check_tests` fails if the two drift. The one addition: the engine's
//! tool schema type cannot carry `minimum`/`maximum`, so the integer
//! parameters say their range in a description, and the tool clamps them.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

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

use crate::backend::Backend;
use crate::render;

/// Every context tool name, in the context engine's order.
pub const TOOL_NAMES: [&str; 15] = [
    "context_search",
    "context_symbol",
    "context_template_versions",
    "context_preprocessor",
    "context_compile_line",
    "context_pack",
    "context_bundle",
    "context_locate",
    "context_find",
    "context_hierarchy",
    "context_calls",
    "context_example",
    "context_history",
    "context_memory",
    "context_remember",
];

/// How long one query may take before it is reported NOT RUN (time limit).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);

const SETTING_DESC: &str = "Build setting name (e.g. debug, release). Omit for every setting.";
const PARTITION_DESC: &str = "Team partition. Omit for every partition you can read.";
const WORKTREE_DESC: &str = "Absolute path of your worktree: adds your uncommitted changes (overlay) and says how many commits the index is behind.";

/// One integer parameter: its range (enforced here) and default.
#[derive(Clone, Copy, Debug)]
struct IntParam {
    name: &'static str,
    min: i64,
    max: i64,
    description: &'static str,
}

/// One tool's model-facing definition.
#[derive(Clone, Copy, Debug)]
struct Def {
    name: &'static str,
    description: &'static str,
    /// String parameters other than the three common ones, with their
    /// description (`None` = the context engine gives none).
    strings: &'static [(&'static str, Option<&'static str>)],
    /// String parameters that take one of a fixed set of values.
    enums: &'static [(&'static str, &'static [&'static str])],
    ints: &'static [IntParam],
    required: &'static str,
}

const DEFS: [Def; 15] = [
    Def {
        name: "context_search",
        description: "Find code by words: function-level chunks of the indexed C/C++ code base, ranked by an FTS5 lexical index, each with its file:lines, build setting, commit and layer (committed or your uncommitted overlay).",
        strings: &[("query", None)],
        enums: &[],
        ints: &[IntParam {
            name: "k",
            min: 1,
            max: 50,
            description: "How many chunks to return, 1-50 (default 10).",
        }],
        required: "query",
    },
    Def {
        name: "context_symbol",
        description: "Definitions and references of a C/C++ symbol, compiler-accurate (scip-clang on the pinned clang 21.1.8), per build setting, with the enclosing function of each reference. Files that could only be indexed approximately are marked with the reason.",
        strings: &[("name", Some("Plain name (push) or a full SCIP symbol."))],
        enums: &[],
        ints: &[IntParam {
            name: "limit",
            min: 1,
            max: 200,
            description: "Most references to return, 1-200 (default 50).",
        }],
        required: "name",
    },
    Def {
        name: "context_template_versions",
        description: "Which versions (instantiations) of a template each build setting really compiles, with separate evidence: semantic (the compiler instantiated it), time_trace (clang -ftime-trace), emitted (in an object file), linked (in the final binary).",
        strings: &[(
            "name",
            Some("Template name or part of it, e.g. Ring or kat::Ring<int"),
        )],
        enums: &[],
        ints: &[],
        required: "name",
    },
    Def {
        name: "context_preprocessor",
        description: "For one file and each build setting: every #if/#ifdef/#elif/#else branch and whether it is live or skipped (checked against the production compiler's own preprocessor), the macros defined in the file and the macros expanded in it.",
        strings: &[("path", Some("Project-relative path."))],
        enums: &[],
        ints: &[],
        required: "path",
    },
    Def {
        name: "context_compile_line",
        description: "Show the compile line: the production compiler command for a file (or, for a header, for the translation units that include it) and the exact command the indexer parsed it with, per build setting, with the reasons any result is approximate.",
        strings: &[("path", None)],
        enums: &[],
        ints: &[],
        required: "path",
    },
    Def {
        name: "context_pack",
        description: "A context pack for a task: the definitions, matching functions with their helpers' signatures, template versions and build-setting-dependent #if branches most relevant to the task, under a token budget (at most 8000).",
        strings: &[("task", None)],
        enums: &[],
        ints: &[IntParam {
            name: "budget_tokens",
            min: 500,
            max: 8000,
            description: "Token budget for the pack, 500-8000 (default 8000).",
        }],
        required: "task",
    },
    Def {
        name: "context_bundle",
        description: "START HERE for a bug report, issue or change request in this C/C++ code base. Paste the task text (with any example code, error messages and stack traces): returns, under a token budget, where the change most likely belongs (ranked, with the evidence for each: names in the task, what the task's example code really calls when compiled with this build's flags, quoted errors, stack frames, similar past commits), the code itself, what it calls and what calls it, the tests that exercise it, and how to verify a change.",
        strings: &[("task", Some("The full task or issue text."))],
        enums: &[],
        ints: &[IntParam {
            name: "budget_tokens",
            min: 1000,
            max: 8000,
            description: "Token budget for the bundle, 1000-8000 (default 8000).",
        }],
        required: "task",
    },
    Def {
        name: "context_locate",
        description: "Rank the functions, methods and classes a task most likely needs changed, with the reasons for each (one list; no code bodies). Use it to decide where to read; context_bundle also includes the code.",
        strings: &[("task", None)],
        enums: &[],
        ints: &[IntParam {
            name: "k",
            min: 1,
            max: 40,
            description: "How many places to return, 1-40 (default 15).",
        }],
        required: "task",
    },
    Def {
        name: "context_find",
        description: "Find the exact definition of a class or function by name, compiler-accurate: every overload with its signature, explicit and partial specializations, the base classes of a class, whether a method is virtual and what it overrides, and the template versions the build compiles. Unlike grep it never returns comments, call sites or forward declarations. kind: class or function.",
        strings: &[(
            "name",
            Some("Plain (Search) or qualified (fmt::context, ns::Book::update) name."),
        )],
        enums: &[("kind", &["class", "function"])],
        ints: &[],
        required: "name",
    },
    Def {
        name: "context_hierarchy",
        description: "Inheritance of a class: all bases up the chain and all derived classes down it. For a method: what it overrides and every override of it, i.e. the possible targets of a virtual call.",
        strings: &[("name", None)],
        enums: &[],
        ints: &[],
        required: "name",
    },
    Def {
        name: "context_calls",
        description: "Who calls a function and what it calls, as the compiler resolved each call (the overload actually chosen; calls into a template point at the template). For virtual methods it also lists every override as a possible target.",
        strings: &[("name", None)],
        enums: &[("direction", &["both", "callers", "callees"])],
        ints: &[],
        required: "name",
    },
    Def {
        name: "context_example",
        description: "Compile a piece of example code (from a bug report, or a test you are about to write) with this project's own build flags, without touching the tree, and report the project functions it really reaches (template specializations and overloads as the compiler picked them, how many calls deep) and the compiler's errors on it. Answers 'what does this snippet actually execute in our code?'.",
        strings: &[(
            "code",
            Some("C/C++ code; includes and main() are added when missing."),
        )],
        enums: &[],
        ints: &[],
        required: "code",
    },
    Def {
        name: "context_history",
        description: "What the project's history says: past commits similar to a task (and the files they changed), commits a task points at by hash or issue number, or the recent commits to a file and the files usually changed with it. Only history up to the indexed commit.",
        strings: &[
            (
                "query",
                Some("The task text, a commit hash, or words to search past commit messages for."),
            ),
            (
                "path",
                Some(
                    "Also list the recent commits to this file and the files usually changed with it.",
                ),
            ),
        ],
        enums: &[],
        ints: &[IntParam {
            name: "k",
            min: 1,
            max: 30,
            description: "How many commits to return, 1-30 (default 10).",
        }],
        required: "query",
    },
    Def {
        name: "context_memory",
        description: "Lessons from past fixes in this code base, recorded by earlier sessions after a verified fix: the root cause, what fixed it, where, and what was tried and did not work. Returns the single best-matching lesson (k for more), or nothing when no past lesson is close.",
        strings: &[("query", Some("The task, or what you are about to change."))],
        enums: &[],
        ints: &[IntParam {
            name: "k",
            min: 1,
            max: 5,
            description: "How many lessons to return, 1-5 (default 1).",
        }],
        required: "query",
    },
    Def {
        name: "context_remember",
        description: "Record a lesson after a VERIFIED fix (tests or the reproducer pass), so later sessions on this code base can reuse it: the task in one or two sentences, the root cause, the fix, where (functions or files), and pitfalls (approaches that failed and why). Keep it short and general enough to help with similar tasks; never paste transcripts or whole diffs.",
        strings: &[
            ("task", None),
            ("root_cause", None),
            ("fix", None),
            ("where", Some("Functions or files, comma-separated.")),
            ("pitfalls", None),
            ("verification", Some("How the fix was verified.")),
        ],
        enums: &[],
        ints: &[],
        required: "task",
    },
];

fn def(name: &str) -> Option<&'static Def> {
    DEFS.iter().find(|def| def.name == name)
}

fn spec_for(def: &Def) -> ToolSpec {
    let mut properties = BTreeMap::new();
    for (name, description) in def.strings {
        properties.insert(
            (*name).to_string(),
            JsonSchema::string(description.map(str::to_string)),
        );
    }
    for (name, values) in def.enums {
        properties.insert(
            (*name).to_string(),
            JsonSchema::string_enum(
                values
                    .iter()
                    .map(|value| serde_json::json!(value))
                    .collect(),
                None,
            ),
        );
    }
    for int in def.ints {
        properties.insert(
            int.name.to_string(),
            JsonSchema::integer(Some(int.description.to_string())),
        );
    }
    properties.insert(
        "setting".to_string(),
        JsonSchema::string(Some(SETTING_DESC.to_string())),
    );
    properties.insert(
        "partition".to_string(),
        JsonSchema::string(Some(PARTITION_DESC.to_string())),
    );
    properties.insert(
        "worktree".to_string(),
        JsonSchema::string(Some(WORKTREE_DESC.to_string())),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: def.name.to_string(),
        description: def.description.to_string(),
        strict: false,
        parameters: JsonSchema::object(
            properties,
            Some(vec![def.required.to_string()]),
            Some(AdditionalProperties::from(false)),
        ),
        output_schema: None,
        defer_loading: None,
    })
}

/// Every context tool's spec, in order (for snapshots and the parity check).
pub fn specs() -> Vec<ToolSpec> {
    DEFS.iter().map(spec_for).collect()
}

/// The arguments as the context engine should get them: unknown names and
/// wrong types refused (a misnamed field is never silently dropped), integers
/// clamped to their range, and `worktree` filled in with the project the
/// thread works in when the model did not name one.
pub fn prepare_arguments(
    tool: &str,
    raw: &str,
    default_worktree: Option<&str>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let def = def(tool).ok_or_else(|| format!("{tool} is not a context tool"))?;
    let value: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(raw).map_err(|err| format!("the arguments are not JSON: {err}"))?
    };
    let serde_json::Value::Object(mut args) = value else {
        return Err("the arguments must be a JSON object".to_string());
    };
    let strings: BTreeSet<&str> = def
        .strings
        .iter()
        .map(|(name, _)| *name)
        .chain(["setting", "partition", "worktree"])
        .collect();
    for (key, value) in args.iter_mut() {
        if let Some((_, values)) = def.enums.iter().find(|(name, _)| *name == key) {
            match value.as_str() {
                Some(text) if values.contains(&text) => continue,
                _ => {
                    return Err(format!("`{key}` must be one of: {}", values.join(", ")));
                }
            }
        }
        if strings.contains(key.as_str()) {
            if !value.is_string() {
                return Err(format!("`{key}` must be a string"));
            }
            continue;
        }
        let Some(int) = def.ints.iter().find(|int| int.name == key) else {
            let mut known: Vec<&str> = strings.iter().copied().collect();
            known.extend(def.enums.iter().map(|(name, _)| *name));
            known.extend(def.ints.iter().map(|int| int.name));
            return Err(format!(
                "unknown argument `{key}`; {tool} takes: {}",
                known.join(", ")
            ));
        };
        let number = value
            .as_i64()
            .or_else(|| value.as_f64().map(|f| f.round() as i64))
            .ok_or_else(|| format!("`{key}` must be a whole number"))?;
        *value = serde_json::json!(number.clamp(int.min, int.max));
    }
    match args.get(def.required).and_then(serde_json::Value::as_str) {
        Some(text) if !text.trim().is_empty() => {}
        _ => return Err(format!("`{}` is required", def.required)),
    }
    if !args.contains_key("worktree")
        && let Some(worktree) = default_worktree
    {
        args.insert("worktree".to_string(), serde_json::json!(worktree));
    }
    Ok(args)
}

/// One context tool bound to its thread's context engine.
pub struct ContextTool {
    def: &'static Def,
    backend: Arc<dyn Backend>,
}

impl ContextTool {
    pub fn all(backend: Arc<dyn Backend>) -> Vec<ContextTool> {
        DEFS.iter()
            .map(|def| ContextTool {
                def,
                backend: Arc::clone(&backend),
            })
            .collect()
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ContextTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.def.name)
    }

    fn spec(&self) -> ToolSpec {
        spec_for(self.def)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // Read-only lookups; the backend serialises them on its one server.
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let raw = call.function_arguments()?.to_string();
            Ok(self.run(&raw).await)
        })
    }
}

impl ContextTool {
    async fn run(&self, raw: &str) -> Box<dyn ToolOutput> {
        let outcome = match prepare_arguments(self.def.name, raw, self.backend.worktree()) {
            Ok(args) => self.backend.call(self.def.name, args, CALL_TIMEOUT).await,
            Err(problem) => crate::backend::Outcome::BadArguments(problem),
        };
        render::output(self.def.name, raw, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn unknown_arguments_are_refused_by_name() {
        let err =
            prepare_arguments("context_symbol", r#"{"symbol":"push"}"#, None).expect_err("refused");
        assert!(err.contains("unknown argument `symbol`"), "{err}");
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn the_required_argument_must_be_a_non_empty_string() {
        assert!(prepare_arguments("context_search", "{}", None).is_err());
        assert!(prepare_arguments("context_search", r#"{"query":"  "}"#, None).is_err());
        assert!(prepare_arguments("context_search", r#"{"query":5}"#, None).is_err());
    }

    #[test]
    fn integers_are_clamped_to_the_context_engines_range() {
        let args =
            prepare_arguments("context_search", r#"{"query":"ring","k":1000}"#, None).expect("ok");
        assert_eq!(args["k"], serde_json::json!(50));
        let args = prepare_arguments("context_pack", r#"{"task":"t","budget_tokens":10}"#, None)
            .expect("ok");
        assert_eq!(args["budget_tokens"], serde_json::json!(500));
        let args =
            prepare_arguments("context_symbol", r#"{"name":"x","limit":3.4}"#, None).expect("ok");
        assert_eq!(args["limit"], serde_json::json!(3));
        assert!(
            prepare_arguments("context_symbol", r#"{"name":"x","limit":"many"}"#, None).is_err()
        );
    }

    #[test]
    fn the_worktree_defaults_to_the_project_but_the_model_may_name_one() {
        let args = prepare_arguments("context_symbol", r#"{"name":"push"}"#, Some("/src/feed"))
            .expect("ok");
        assert_eq!(args["worktree"], serde_json::json!("/src/feed"));
        let args = prepare_arguments(
            "context_symbol",
            r#"{"name":"push","worktree":"/src/feed-wt2"}"#,
            Some("/src/feed"),
        )
        .expect("ok");
        assert_eq!(args["worktree"], serde_json::json!("/src/feed-wt2"));
    }

    #[test]
    fn enum_arguments_must_be_one_of_their_values() {
        let args = prepare_arguments(
            "context_calls",
            r#"{"name":"f","direction":"callers"}"#,
            None,
        )
        .expect("ok");
        assert_eq!(args["direction"], serde_json::json!("callers"));
        let err = prepare_arguments("context_calls", r#"{"name":"f","direction":"up"}"#, None)
            .expect_err("refused");
        assert!(err.contains("both, callers, callees"), "{err}");
        assert!(prepare_arguments("context_find", r#"{"name":"f","kind":3}"#, None).is_err());
    }

    #[test]
    fn every_tool_in_the_context_engines_order() {
        let names: Vec<String> = specs()
            .iter()
            .map(|spec| match spec {
                ToolSpec::Function(function) => function.name.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, TOOL_NAMES.map(str::to_string).to_vec());
    }
}
