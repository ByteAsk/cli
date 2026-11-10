use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_login::default_client::create_client;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

const TOOL_NAME: &str = "cpp_godbolt";
/// Default compiler id (Compiler Explorer's gcc 14.2, C++). Override via `compiler`.
const DEFAULT_COMPILER: &str = "g142";
const DEFAULT_STD: &str = "c++20";
/// The default for `language: "c"` — a C++ standard fails every C compile.
const DEFAULT_STD_C: &str = "c17";
const DEFAULT_ACTION: &str = "asm";
const DEFAULT_LANGUAGE: &str = "cpp";
/// Public Compiler Explorer origin. Requests go directly from the user's
/// machine (their own IP), not through the ByteAsk gateway — this spreads load
/// across users' IPs instead of rate-limiting the single VM. Override with
/// `BYTEASK_GODBOLT_URL` (used by tests).
const GODBOLT_ORIGIN: &str = "https://godbolt.org";
/// Cap assembly output so a large translation unit can't flood the model.
const ASM_MAX_LINES: usize = 150;
/// Cap the error body we echo back to the model.
const ERROR_BODY_CAP: usize = 1000;

/// Headline Compiler Explorer (godbolt) tool. Posts a self-contained C/C++
/// snippet directly to the public Compiler Explorer API and shapes the result
/// into a compact, model-friendly transcript.
pub struct GodboltTool {
    #[allow(dead_code)]
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GodboltArgs {
    source: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    compiler: Option<String>,
    #[serde(default)]
    std: Option<String>,
    #[serde(default)]
    flags: Option<String>,
    #[serde(default)]
    libraries: Option<Vec<String>>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for GodboltTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "source".to_string(),
            JsonSchema::string(Some(
                "Self-contained C/C++ snippet to compile (a full translation unit, including any \
                 #includes and a main if you want to run it)."
                    .to_string(),
            )),
        );
        properties.insert(
            "action".to_string(),
            JsonSchema::string_enum(
                vec![json!("asm"), json!("run"), json!("link")],
                Some(
                    "\"asm\" (default) returns the generated assembly with source-mapped \
                     diagnostics; \"run\" compiles and executes the snippet and returns its \
                     stdout/stderr/exit code; \"link\" returns a shareable Compiler Explorer URL \
                     (godbolt.org/z/...) preloaded with this source, compiler, flags, and \
                     libraries. Use \"link\" ONLY when the user would value an interactive page \
                     to open/tweak themselves (e.g. they asked to share/open in godbolt, or will \
                     want to edit flags/compiler) — not for routine compiles."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "language".to_string(),
            JsonSchema::string_enum(
                vec![json!("c"), json!("cpp")],
                Some(
                    "Source language, \"c\" or \"cpp\" (default \"cpp\"). Advisory only: the \
                     `compiler` id ultimately selects the language, so for C pass a C compiler id \
                     and an appropriate `std`."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "compiler".to_string(),
            JsonSchema::string(Some(format!(
                "Compiler: either an exact Compiler Explorer id (e.g. \"g142\", \"armg1420\", \
                 \"clang_trunk\") OR a friendly name that is resolved against CE's catalogue, e.g. \
                 \"arm gcc 14\", \"x86-64 gcc 14\", \"clang 18\". Default \"{DEFAULT_COMPILER}\" \
                 (x86-64 gcc 14.2). On no/ambiguous match the tool returns candidate ids to retry with."
            ))),
        );
        properties.insert(
            "libraries".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Libraries to make available for #include, each as \"<name>\" or \
                     \"<name> <version>\" (e.g. \"fmt\", \"fmt 11\", \"boost\", \"range-v3\"). \
                     Resolved to CE library + version ids; version may be omitted (newest is used). \
                     REQUIRED whenever the source #includes a third-party library header."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "std".to_string(),
            JsonSchema::string(Some(format!(
                "Language standard, mapped to -std=<value> (default \"{DEFAULT_STD}\", or \
                 \"{DEFAULT_STD_C}\" for language \"c\"). Pass an empty string to omit."
            ))),
        );
        properties.insert(
            "flags".to_string(),
            JsonSchema::string(Some(
                "Extra compiler arguments appended verbatim, e.g. \"-O2 -Wall\".".to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Compile a self-contained C/C++ snippet via Compiler Explorer and return \
                the assembly (with source-mapped diagnostics) or run it. Use proactively to \
                inspect codegen, check a snippet compiles, compare optimization, or run a small \
                experiment. Do NOT use for the user's full project build (use the local compiler / \
                compile_commands.json) or non-C/C++."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["source".to_string()]),
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

impl GodboltTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: GodboltArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let action = args.action.as_deref().unwrap_or(DEFAULT_ACTION);
        let execute = match action {
            "asm" | "link" => false,
            "run" => true,
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "invalid action {other:?}; expected \"asm\", \"run\", or \"link\""
                )));
            }
        };
        let language = args.language.as_deref().unwrap_or(DEFAULT_LANGUAGE);
        if language != "c" && language != "cpp" {
            return Err(FunctionCallError::RespondToModel(format!(
                "invalid language {language:?}; expected \"c\" or \"cpp\""
            )));
        }
        let compiler_query = args.compiler.as_deref().unwrap_or(DEFAULT_COMPILER);
        let std = args.std.as_deref().unwrap_or_else(|| default_std(language));
        let flags = args.flags.as_deref().unwrap_or("");
        let lang_id = if language == "c" { "c" } else { "c++" };

        // Resolve the compiler query to a concrete CE id (an exact id passes
        // straight through; a friendly name is matched against CE's catalogue).
        let compilers = fetch_compilers(lang_id).await?;
        let compiler = resolve_compiler(compiler_query, &compilers)
            .map_err(FunctionCallError::RespondToModel)?;

        // Resolve any requested libraries to {id, version-id} pairs so the
        // headers are on the include path (e.g. <fmt/format.h>).
        let lib_specs = args.libraries.unwrap_or_default();
        let mut libraries: Vec<(String, String)> = Vec::new();
        if lib_specs.iter().any(|s| !s.trim().is_empty()) {
            let catalogue = fetch_libraries(lang_id).await?;
            for spec in lib_specs.iter().filter(|s| !s.trim().is_empty()) {
                let pair =
                    resolve_library(spec, &catalogue).map_err(FunctionCallError::RespondToModel)?;
                libraries.push(pair);
            }
        }

        // "link" short-circuits the compile path: build a shareable, fully
        // configured Compiler Explorer URL instead of inline asm/run output.
        if action == "link" {
            let link =
                create_shortlink(lang_id, &args.source, &compiler.id, std, flags, &libraries)
                    .await?;
            let lib_note = if libraries.is_empty() {
                String::new()
            } else {
                format!(" + {} library/libraries", libraries.len())
            };
            return Ok(crate::report::report(
                TOOL_NAME,
                &compiler.name,
                format!(
                    "Shareable Compiler Explorer link (opens with the source, {}{lib_note}, and \
                     your flags preloaded):\n{link}",
                    compiler.name
                ),
                check_result::CheckResult::pass(
                    TOOL_NAME,
                    "Made a shareable Compiler Explorer link.",
                ),
            ));
        }

        let body = build_request_body(&args.source, std, flags, execute, &libraries);
        let url = compile_url(&compiler.id);

        let client = create_client();
        let request = client
            .post(url.as_str())
            .header("Accept", "application/json")
            .json(&body);

        let response = request
            .send()
            .await
            .map_err(|err| unreachable("compile", &err))?;

        let status = response.status();

        if !status.is_success() {
            let mut body = response.text().await.unwrap_or_default();
            body.truncate(body.floor_char_boundary(ERROR_BODY_CAP));
            return Err(FunctionCallError::RespondToModel(format!(
                "Compiler Explorer (godbolt.org) returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        let value: Value = response.json().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid JSON from godbolt.org: {err}"))
        })?;

        let rendered = if execute {
            shape_run_output(&value, None)
        } else {
            shape_asm_output(&value, None)
        };
        let result = godbolt_result(&value, &compiler.name);

        Ok(crate::report::report(
            TOOL_NAME,
            &compiler.name,
            rendered,
            result,
        ))
    }
}

/// A request to Compiler Explorer that never got an answer (offline, DNS,
/// proxy, TLS). Not `Fatal`: core turns a tool's `Fatal` into the end of the
/// user's WHOLE turn, and "godbolt.org is unreachable" is a fact the
/// model can report and work around (e.g. compile locally with cpp_sanitize).
fn unreachable(what: &str, err: &dyn std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "Compiler Explorer (godbolt.org) is unavailable: the {what} request failed ({err}). \
         Nothing was compiled — this is a network problem, not a problem with the code."
    ))
}

fn default_std(language: &str) -> &'static str {
    if language == "c" {
        DEFAULT_STD_C
    } else {
        DEFAULT_STD
    }
}

/// Public Compiler Explorer origin: `BYTEASK_GODBOLT_URL` if set (tests),
/// else godbolt.org, with any trailing slash trimmed.
fn godbolt_origin() -> String {
    std::env::var("BYTEASK_GODBOLT_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GODBOLT_ORIGIN.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Compile endpoint URL: the public Compiler Explorer per-compiler endpoint,
/// `<origin>/api/compiler/<id>/compile`.
fn compile_url(compiler: &str) -> String {
    format!("{}/api/compiler/{compiler}/compile", godbolt_origin())
}

/// Join `std` and extra `flags` into Compiler Explorer's `userArguments` string.
fn user_arguments(std: &str, flags: &str) -> String {
    let mut parts = Vec::new();
    if !std.is_empty() {
        parts.push(format!("-std={std}"));
    }
    if !flags.trim().is_empty() {
        parts.push(flags.trim().to_string());
    }
    parts.join(" ")
}

fn build_request_body(
    source: &str,
    std: &str,
    flags: &str,
    execute: bool,
    libraries: &[(String, String)],
) -> Value {
    // Compiler Explorer's native per-compiler endpoint takes the compiler id in
    // the URL path, so the body carries only the source + options.
    let libs: Vec<Value> = libraries
        .iter()
        .map(|(id, version)| json!({ "id": id, "version": version }))
        .collect();
    json!({
        "source": source,
        "options": {
            "userArguments": user_arguments(std, flags),
            "filters": {
                "intel": true,
                "demangle": true,
                "labels": true,
                "directives": true,
                "commentOnly": true,
                "execute": execute,
            },
            "libraries": libs,
            "executeParameters": { "args": "", "stdin": "" },
        },
    })
}

/// Build the Compiler Explorer client-state for `POST /api/shortener`: a code
/// editor (session 1) plus a compiler pane wired to it, preloaded with the
/// resolved compiler id, user arguments, and libraries, so the shared link
/// opens fully configured rather than as a bare editor. Note the shortener's
/// library shape is `{name, ver}` (vs the compile endpoint's `{id, version}`).
fn shortlink_config(
    lang: &str,
    source: &str,
    compiler_id: &str,
    user_args: &str,
    libraries: &[(String, String)],
) -> Value {
    let libs: Vec<Value> = libraries
        .iter()
        .map(|(id, version)| json!({ "name": id, "ver": version }))
        .collect();
    json!({
        "config": {
            "version": 4,
            "content": [{
                "type": "row",
                "content": [
                    {
                        "type": "component",
                        "componentName": "codeEditor",
                        "componentState": { "id": 1, "lang": lang, "source": source },
                    },
                    {
                        "type": "component",
                        "componentName": "compiler",
                        "componentState": {
                            "id": 1,
                            "source": 1,
                            "lang": lang,
                            "compiler": compiler_id,
                            "options": user_args,
                            "libs": libs,
                            "filters": {
                                "intel": true,
                                "demangle": true,
                                "labels": true,
                                "directives": true,
                                "commentOnly": true,
                                "execute": false,
                            },
                        },
                    },
                ],
            }],
        }
    })
}

/// Create a Compiler Explorer shortlink for the given state and return its URL
/// (`https://godbolt.org/z/<id>`).
async fn create_shortlink(
    lang: &str,
    source: &str,
    compiler_id: &str,
    std: &str,
    flags: &str,
    libraries: &[(String, String)],
) -> Result<String, FunctionCallError> {
    let config = shortlink_config(
        lang,
        source,
        compiler_id,
        &user_arguments(std, flags),
        libraries,
    );
    let url = format!("{}/api/shortener", godbolt_origin());
    let response = create_client()
        .post(&url)
        .header("Accept", "application/json")
        .json(&config)
        .send()
        .await
        .map_err(|e| unreachable("shortener", &e))?;
    let status = response.status();
    if !status.is_success() {
        let mut body = response.text().await.unwrap_or_default();
        body.truncate(body.floor_char_boundary(ERROR_BODY_CAP));
        return Err(FunctionCallError::RespondToModel(format!(
            "godbolt shortener returned HTTP {}: {body}",
            status.as_u16()
        )));
    }
    let value: Value = response.json().await.map_err(|e| {
        FunctionCallError::RespondToModel(format!("invalid JSON from godbolt shortener: {e}"))
    })?;
    value
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel("godbolt shortener returned no url".to_string())
        })
}

// ---- Compiler / library resolution against the public CE catalogue ----

#[derive(Clone, Debug, Deserialize)]
struct CompilerInfo {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LibVersion {
    id: String,
    version: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LibraryInfo {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    versions: Vec<LibVersion>,
}

/// Process-lifetime caches of the CE catalogue, keyed by CE language id
/// ("c++"/"c"). The lists are large but stable; fetched once on first use.
fn compiler_cache() -> &'static Mutex<HashMap<String, Arc<Vec<CompilerInfo>>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<Vec<CompilerInfo>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn library_cache() -> &'static Mutex<HashMap<String, Arc<Vec<LibraryInfo>>>> {
    static L: OnceLock<Mutex<HashMap<String, Arc<Vec<LibraryInfo>>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn fetch_compilers(lang: &str) -> Result<Arc<Vec<CompilerInfo>>, FunctionCallError> {
    {
        let guard = compiler_cache().lock().unwrap();
        if let Some(cached) = guard.get(lang).cloned() {
            return Ok(cached);
        }
    }
    let url = format!("{}/api/compilers/{lang}?fields=id,name", godbolt_origin());
    let list: Vec<CompilerInfo> = create_client()
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| unreachable("compiler list", &e))?
        .json()
        .await
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "invalid compilers JSON from godbolt.org: {e}"
            ))
        })?;
    let arc = Arc::new(list);
    compiler_cache()
        .lock()
        .unwrap()
        .insert(lang.to_string(), arc.clone());
    Ok(arc)
}

async fn fetch_libraries(lang: &str) -> Result<Arc<Vec<LibraryInfo>>, FunctionCallError> {
    {
        let guard = library_cache().lock().unwrap();
        if let Some(cached) = guard.get(lang).cloned() {
            return Ok(cached);
        }
    }
    let url = format!("{}/api/libraries/{lang}", godbolt_origin());
    let list: Vec<LibraryInfo> = create_client()
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| unreachable("library list", &e))?
        .json()
        .await
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "invalid libraries JSON from godbolt.org: {e}"
            ))
        })?;
    let arc = Arc::new(list);
    library_cache()
        .lock()
        .unwrap()
        .insert(lang.to_string(), arc.clone());
    Ok(arc)
}

/// Extract the integer groups from a name/version (e.g. "ARM GCC 14.2.0" ->
/// [14,2,0]) so versions sort numerically, not lexically ("9" < "14").
fn version_key(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(cur.parse().unwrap_or(0));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.parse().unwrap_or(0));
    }
    out
}

/// Does `token` appear in `name`? A NUMERIC (version) token must appear
/// bounded by non-digits, so "18" matches "x86-64 clang 18.1.0" but NOT
/// "z180-clang 15.0.7" — the substring match that used to send "clang 18" to a
/// Z80 cross-compiler. Non-numeric tokens keep plain substring matching.
fn token_matches_name(name: &str, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    // A WORD token is bounded by non-alphanumerics too, so "arm" is not
    // found inside "arm64" and "gcc" not inside "gccrs".
    let numeric = token.bytes().all(|b| b.is_ascii_digit());
    let bounds = |b: u8| {
        if numeric {
            !b.is_ascii_digit()
        } else {
            !b.is_ascii_alphanumeric()
        }
    };
    let bytes = name.as_bytes();
    let mut from = 0;
    while let Some(pos) = name[from..].find(token) {
        let start = from + pos;
        let end = start + token.len();
        let before_ok = start == 0 || bounds(bytes[start - 1]);
        let after_ok = end == bytes.len() || bounds(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Resolve a `compiler` argument to a concrete CE compiler. Accepts an exact CE
/// id (matched first, verbatim) or a friendly query whose lowercased tokens must
/// all appear in the compiler's name; among matches the highest version wins.
/// On no match, returns an error listing candidate ids for the model to retry.
fn resolve_compiler(query: &str, compilers: &[CompilerInfo]) -> Result<CompilerInfo, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("empty compiler query".to_string());
    }
    // 1. Exact CE id wins (ids are case-sensitive slugs like "armg1420").
    if let Some(c) = compilers.iter().find(|c| c.id == q) {
        return Ok(c.clone());
    }
    // 2. All lowercased tokens must appear in the compiler's display name.
    let tokens: Vec<String> = q
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let mut matches: Vec<&CompilerInfo> = compilers
        .iter()
        .filter(|c| {
            let name = c.name.to_lowercase();
            tokens.iter().all(|t| token_matches_name(&name, t))
        })
        .collect();
    if matches.is_empty() {
        let key = tokens
            .iter()
            .max_by_key(|t| t.len())
            .cloned()
            .unwrap_or_default();
        let mut cands: Vec<&CompilerInfo> = compilers
            .iter()
            .filter(|c| c.name.to_lowercase().contains(key.as_str()))
            .collect();
        cands.sort_by(|a, b| version_key(&b.name).cmp(&version_key(&a.name)));
        let listed = cands
            .iter()
            .take(8)
            .map(|c| format!("{} ({})", c.id, c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let hint = if listed.is_empty() {
            "(try e.g. \"x86-64 gcc 14\", \"arm gcc 14\", \"clang 18\")".to_string()
        } else {
            listed
        };
        return Err(format!(
            "no Compiler Explorer compiler matches {query:?}. Candidates: {hint}"
        ));
    }
    // 3. An unqualified query means the host. "gcc" once resolved to MSP430
    //    gcc 6.2.1 and "gcc 14" to s390x, because the sort key collected EVERY
    //    digit group of the name (430 and 390 beat 64). When x86-64 compilers
    //    match, only they compete; a query that names another architecture
    //    matches no x86-64 compiler, so it is unaffected.
    if matches.iter().any(|c| is_host_compiler(&c.name)) {
        matches.retain(|c| is_host_compiler(&c.name));
    }
    // 4. Still spanning architectures: a guess would silently compile for the
    //    wrong target, so list the candidates instead.
    matches.sort_by(|a, b| {
        compiler_version(&b.name)
            .cmp(&compiler_version(&a.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut arches: Vec<String> = matches.iter().map(|c| arch_word(&c.name)).collect();
    arches.sort();
    arches.dedup();
    if arches.len() > 1 {
        let listed = matches
            .iter()
            .take(10)
            .map(|c| format!("{} ({})", c.id, c.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{query:?} matches compilers for {} different targets ({}), so none was picked. \
             Pass one of these ids, or add the target to the name: {listed}",
            arches.len(),
            arches.join(", ")
        ));
    }
    Ok(matches[0].clone())
}

/// Compiler Explorer names host (x86-64) compilers "x86-64 gcc 14.2",
/// "x86-64 clang 18.1.0".
fn is_host_compiler(name: &str) -> bool {
    name.to_lowercase().starts_with("x86-64 ")
}

/// The target word of a CE compiler name: its first word ("ARM", "s390x",
/// "z180-clang"), lowercased.
fn arch_word(name: &str) -> String {
    name.split_whitespace().next().unwrap_or("").to_lowercase()
}

/// The compiler's own version: the LAST whitespace-separated word that is a
/// dotted number ("14.2", "18.1.0", "v19.40"). NOT every digit group of the
/// name, which ranked "s390x"/"MSP430"/"ARM64" by their architecture digits.
fn compiler_version(name: &str) -> Vec<u32> {
    name.split_whitespace()
        .rev()
        .map(|word| word.trim_start_matches(['v', 'V']))
        .find(|word| {
            word.starts_with(|c: char| c.is_ascii_digit())
                && word.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
        .map(|word| word.split('.').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_default()
}

/// Resolve a library spec ("fmt", "fmt 11", "fmt 11.0.0", "boost:177") to a CE
/// `(library-id, version-id)` pair. Version may be the display version (matched
/// exactly or by `major.` prefix) or the CE version id; omitted -> newest
/// non-trunk (else trunk). Errors list available ids/versions to retry with.
fn resolve_library(spec: &str, libs: &[LibraryInfo]) -> Result<(String, String), String> {
    let spec = spec.trim();
    let (name_part, ver_part) = match spec.split_once(|c: char| c == ':' || c.is_whitespace()) {
        Some((n, v)) => (n.trim(), v.trim()),
        None => (spec, ""),
    };
    let nl = name_part.to_lowercase();
    let lib = libs
        .iter()
        .find(|l| l.id.to_lowercase() == nl)
        .or_else(|| libs.iter().find(|l| l.name.to_lowercase() == nl))
        .or_else(|| {
            libs.iter()
                .find(|l| l.id.to_lowercase().contains(&nl) || l.name.to_lowercase().contains(&nl))
        });
    let lib = match lib {
        Some(l) => l,
        None => {
            let mut cands: Vec<&str> = libs
                .iter()
                .map(|l| l.id.as_str())
                .filter(|id| id.to_lowercase().contains(&nl))
                .take(10)
                .collect();
            if cands.is_empty() {
                cands = libs.iter().take(12).map(|l| l.id.as_str()).collect();
            }
            return Err(format!(
                "no Compiler Explorer library matches {name_part:?}. Available ids include: {}",
                cands.join(", ")
            ));
        }
    };
    if lib.versions.is_empty() {
        return Err(format!(
            "library {:?} has no versions on Compiler Explorer",
            lib.id
        ));
    }
    let chosen = if ver_part.is_empty() {
        lib.versions
            .iter()
            .filter(|v| v.id != "trunk" && !v.version.eq_ignore_ascii_case("trunk"))
            .max_by(|a, b| version_key(&a.version).cmp(&version_key(&b.version)))
            .or_else(|| lib.versions.last())
            .expect("versions non-empty")
    } else {
        let vl = ver_part.to_lowercase();
        let prefix = format!("{ver_part}.");
        lib.versions
            .iter()
            .find(|v| v.id.to_lowercase() == vl || v.version.to_lowercase() == vl)
            .or_else(|| {
                lib.versions
                    .iter()
                    .filter(|v| v.version == ver_part || v.version.starts_with(&prefix))
                    .max_by(|a, b| version_key(&a.version).cmp(&version_key(&b.version)))
            })
            .ok_or_else(|| {
                let avail = lib
                    .versions
                    .iter()
                    .map(|v| v.version.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "library {:?} has no version matching {ver_part:?}. Available: {avail}",
                    lib.id
                )
            })?
    };
    Ok((lib.id.clone(), chosen.id.clone()))
}

/// Map Compiler Explorer's severity (numeric 1/2/3 or a string) to a label.
fn severity_label(value: Option<&Value>) -> &'static str {
    match value {
        Some(Value::Number(n)) => match n.as_u64() {
            Some(3) => "error",
            Some(2) => "warning",
            _ => "note",
        },
        Some(Value::String(s)) => match s.as_str() {
            "error" => "error",
            "warning" => "warning",
            _ => "note",
        },
        _ => "note",
    }
}

/// Render compile diagnostics from the top-level `stderr[]`, surfacing the
/// structured `tag` as `file:line:col: severity: text` when present.
fn render_diagnostics(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(items) = value.get("stderr").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        match item.get("tag").filter(|tag| !tag.is_null()) {
            Some(tag) => {
                let file = tag
                    .get("file")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("<source>");
                let line = tag.get("line").and_then(Value::as_u64).unwrap_or(0);
                let column = tag.get("column").and_then(Value::as_u64).unwrap_or(0);
                let severity = severity_label(tag.get("severity"));
                let text = tag
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("text").and_then(Value::as_str))
                    .unwrap_or("");
                out.push(format!("{file}:{line}:{column}: {severity}: {text}"));
            }
            None => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Concatenate the `text` field of every object in `value[key]`, skipping nulls.
fn collect_text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn push_cache_note(out: &mut String, cache: Option<&str>) {
    if let Some(cache) = cache {
        out.push_str(&format!(" (cache: {cache})"));
    }
}

fn push_diagnostics(out: &mut String, value: &Value) {
    let diagnostics = render_diagnostics(value);
    if !diagnostics.is_empty() {
        out.push_str("diagnostics:\n");
        for line in diagnostics {
            out.push_str(&line);
            out.push('\n');
        }
    }
}

/// Did the code compile on that compiler? A program's own exit code is
/// its business (it is shown above), not this tool's verdict.
fn godbolt_result(value: &Value, compiler: &str) -> check_result::CheckResult {
    match value.get("code").and_then(Value::as_i64) {
        Some(0) => check_result::CheckResult::pass(TOOL_NAME, &format!("Compiled on {compiler}.")),
        Some(code) => check_result::CheckResult::fail(
            TOOL_NAME,
            &format!("Did not compile on {compiler} (compiler exit {code})."),
        ),
        None => check_result::CheckResult::not_run(
            TOOL_NAME,
            check_result::NotRunReason::ServiceUnavailable,
            "Compiler Explorer's answer carried no compile result.",
            "call cpp_godbolt again; if it repeats, Compiler Explorer may be degraded.",
        ),
    }
}

fn shape_asm_output(value: &Value, cache: Option<&str>) -> String {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let mut out = format!("compile exit code: {code}");
    push_cache_note(&mut out, cache);
    out.push('\n');

    push_diagnostics(&mut out, value);

    let asm_lines: Vec<&str> = value
        .get("asm")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    out.push_str("asm:\n");
    if asm_lines.is_empty() {
        out.push_str("(no assembly produced)");
    } else {
        let shown = asm_lines.len().min(ASM_MAX_LINES);
        out.push_str(&asm_lines[..shown].join("\n"));
        if asm_lines.len() > shown {
            let elided = asm_lines.len() - shown;
            out.push_str(&format!(
                "\n... {elided} more asm line(s) elided (showing {shown})."
            ));
        }
    }
    out
}

fn shape_run_output(value: &Value, cache: Option<&str>) -> String {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let mut out = format!("compile exit code: {code}");
    push_cache_note(&mut out, cache);
    out.push('\n');

    push_diagnostics(&mut out, value);

    // Execution details live under `execResult`. If absent, the program never ran
    // (e.g. compilation failed), so don't conflate the compile `code` with a
    // program exit code.
    match value.get("execResult") {
        Some(exec) => {
            match exec.get("code").and_then(Value::as_i64) {
                Some(exit) => out.push_str(&format!("program exit code: {exit}\n")),
                None => out.push_str("program exit code: unknown\n"),
            }
            let stdout = collect_text(exec, "stdout");
            if !stdout.is_empty() {
                out.push_str("stdout:\n");
                out.push_str(&stdout);
                out.push('\n');
            }
            let stderr = collect_text(exec, "stderr");
            if !stderr.is_empty() {
                out.push_str("stderr:\n");
                out.push_str(&stderr);
                out.push('\n');
            }
        }
        None => out.push_str("program did not run\n"),
    }

    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn numeric_token_matches_on_version_boundary_not_arch_digits() {
        // "18" must match "clang 18.1.0" but NOT "z180-clang 15.0.7".
        assert!(token_matches_name("x86-64 clang 18.1.0", "18"));
        assert!(!token_matches_name("z180-clang 15.0.7", "18"));
        // non-numeric tokens keep plain substring matching
        assert!(token_matches_name("z180-clang 15.0.7", "clang"));
        // a version that really is a prefix of a larger number is rejected
        assert!(!token_matches_name("gcc 1400", "14"));
        assert!(token_matches_name("x86-64 gcc 14.2", "14"));
    }

    #[test]
    fn resolve_clang_18_does_not_pick_a_z80_cross_compiler() {
        // End to end: "clang 18" must not resolve to a z80 cross compiler.
        let compilers = vec![
            compiler("z180clang1507", "z180-clang 15.0.7"),
            compiler("clang1810", "x86-64 clang 18.1.0"),
        ];
        let got = resolve_compiler("clang 18", &compilers).expect("should resolve");
        assert_eq!(got.id, "clang1810");
    }

    use super::*;

    /// `BYTEASK_GODBOLT_URL` is process-global, and the two tests below both
    /// mutate it — one clears it, the other sets it. Their old comments claimed
    /// "single-threaded test", which is not true: the harness runs tests on
    /// parallel threads, so they raced and the suite failed intermittently
    /// (reproducible by running just the two together). Serialize them.
    static GODBOLT_URL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn compile_url_targets_public_ce_per_compiler_endpoint() {
        let _guard = GODBOLT_URL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // No override -> the public Compiler Explorer per-compiler endpoint.
        // SAFETY: the guard above makes this the only thread touching the var.
        unsafe { std::env::remove_var("BYTEASK_GODBOLT_URL") };
        assert_eq!(
            compile_url("g142"),
            "https://godbolt.org/api/compiler/g142/compile"
        );
    }

    /// The compiler catalogue a mock CE serves: one x86-64 gcc.
    fn one_compiler() -> String {
        r#"[{"id":"g141","name":"x86-64 gcc 14.1"}]"#.to_string()
    }

    #[allow(clippy::await_holding_lock)] // the env lock must span the call
    async fn run_against(base: &str, action: &str) -> Result<String, FunctionCallError> {
        let _env = GODBOLT_URL_ENV
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: every test that sets this variable holds GODBOLT_URL_ENV.
        unsafe { std::env::set_var("BYTEASK_GODBOLT_URL", base) };
        compiler_cache().lock().unwrap().clear();
        let tool = GodboltTool {
            cwd: std::env::temp_dir(),
        };
        let out = crate::test_support::call(
            &tool,
            serde_json::json!({"source": "int main(){}", "compiler": "g141", "action": action}),
        )
        .await;
        unsafe { std::env::remove_var("BYTEASK_GODBOLT_URL") };
        out
    }

    #[tokio::test]
    async fn an_unreachable_compiler_explorer_does_not_end_the_turn() {
        // Offline / DNS / proxy must not be `Fatal` — that ends the turn.
        let err = run_against(&crate::test_support::closed_port_url(), "asm")
            .await
            .expect_err("nothing listens there");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("must go back to the model, not end the turn: {err:?}");
        };
        assert!(message.contains("unavailable"), "{message}");
    }

    #[tokio::test]
    async fn error_bodies_with_a_multibyte_char_at_the_cap_do_not_panic() {
        // Both the compile and the shortener error paths must not panic on
        // `String::truncate` inside a multi-byte character.
        let page = crate::test_support::body_straddling(ERROR_BODY_CAP);
        let base = crate::test_support::serve_http(vec![
            ("/api/compilers", 200, "application/json", one_compiler()),
            ("/api/compiler/", 500, "text/html", page.clone()),
            ("/api/shortener", 502, "text/html", page),
        ]);
        for (action, status) in [("asm", "HTTP 500"), ("link", "HTTP 502")] {
            let err = run_against(&base, action).await.expect_err("error status");
            let FunctionCallError::RespondToModel(message) = err else {
                panic!("{action}: expected a model-facing error, got {err:?}");
            };
            assert!(message.contains(status), "{action}: {message}");
        }
    }

    #[test]
    fn compile_url_honors_env_override_and_trims_slash() {
        let _guard = GODBOLT_URL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the guard above makes this the only thread touching the var.
        unsafe { std::env::set_var("BYTEASK_GODBOLT_URL", "http://localhost:9000/") };
        assert_eq!(
            compile_url("clang_trunk"),
            "http://localhost:9000/api/compiler/clang_trunk/compile"
        );
        unsafe { std::env::remove_var("BYTEASK_GODBOLT_URL") };
    }

    #[test]
    fn user_arguments_combines_std_and_flags() {
        assert_eq!(user_arguments("c++20", "-O2 -Wall"), "-std=c++20 -O2 -Wall");
        assert_eq!(user_arguments("c++20", ""), "-std=c++20");
        assert_eq!(user_arguments("", "-O2"), "-O2");
        assert_eq!(user_arguments("", ""), "");
    }

    #[test]
    fn build_request_body_shapes_ce_request() {
        let body = build_request_body("int main(){}", "c++20", "-O2", true, &[]);
        // The compiler id lives in the URL path now, not the body.
        assert!(body.get("compiler").is_none());
        assert_eq!(body["source"], "int main(){}");
        assert_eq!(body["options"]["userArguments"], "-std=c++20 -O2");
        assert_eq!(body["options"]["filters"]["intel"], json!(true));
        assert_eq!(body["options"]["filters"]["demangle"], json!(true));
        assert_eq!(body["options"]["filters"]["execute"], json!(true));
        assert_eq!(body["options"]["executeParameters"]["args"], "");
        // no libraries -> empty array
        assert_eq!(body["options"]["libraries"], json!([]));
        // asm action -> execute false
        let asm = build_request_body("x", "c++20", "", false, &[]);
        assert_eq!(asm["options"]["filters"]["execute"], json!(false));
    }

    #[test]
    fn shape_asm_output_renders_code_diagnostics_and_asm() {
        let sample = json!({
            "code": 0,
            "asm": [
                { "text": "main:", "source": null },
                { "text": "  ret", "source": { "line": 1 } },
                { "text": null }
            ],
            "stderr": [
                {
                    "text": "<source>:2:5: warning: unused variable 'x'",
                    "tag": {
                        "line": 2,
                        "column": 5,
                        "severity": 2,
                        "text": "unused variable 'x'",
                        "file": "example.cpp"
                    }
                },
                { "text": "Compilation finished" }
            ]
        });
        let out = shape_asm_output(&sample, Some("hit"));
        // Note: CE asm indentation is preserved verbatim ("  ret"); avoid Rust's
        // backslash-line-continuation here since it would strip that leading whitespace.
        assert_eq!(
            out,
            "compile exit code: 0 (cache: hit)\ndiagnostics:\nexample.cpp:2:5: warning: unused variable 'x'\nCompilation finished\nasm:\nmain:\n  ret"
        );
    }

    #[test]
    fn shape_asm_output_truncates_long_assembly() {
        let asm: Vec<Value> = (0..200)
            .map(|i| json!({ "text": format!("line{i}") }))
            .collect();
        let sample = json!({ "code": 0, "asm": asm });
        let out = shape_asm_output(&sample, None);
        assert!(out.starts_with("compile exit code: 0\nasm:\nline0\n"));
        assert!(out.contains("... 50 more asm line(s) elided (showing 150)."));
    }

    #[test]
    fn shape_run_output_renders_exec_result() {
        let sample = json!({
            "code": 0,
            "stderr": [],
            "execResult": {
                "code": 42,
                "stdout": [{ "text": "hello" }, { "text": "world" }],
                "stderr": [{ "text": "oops" }]
            }
        });
        let out = shape_run_output(&sample, Some("miss"));
        assert_eq!(
            out,
            "compile exit code: 0 (cache: miss)\n\
             program exit code: 42\n\
             stdout:\n\
             hello\n\
             world\n\
             stderr:\n\
             oops"
        );
    }

    #[test]
    fn shape_run_output_notes_when_program_did_not_run() {
        let sample = json!({ "code": 1, "stderr": [{ "text": "error: build failed" }] });
        let out = shape_run_output(&sample, None);
        assert_eq!(
            out,
            "compile exit code: 1\n\
             diagnostics:\n\
             error: build failed\n\
             program did not run"
        );
    }

    #[test]
    fn user_arguments_trims_flag_whitespace() {
        // Leading/trailing whitespace around flags is stripped before joining.
        assert_eq!(user_arguments("c++20", "  -O2  "), "-std=c++20 -O2");
        // Whitespace-only flags are treated as empty (no trailing space).
        assert_eq!(user_arguments("c++17", "   "), "-std=c++17");
        // Whitespace-only flags with empty std produce an empty string.
        assert_eq!(user_arguments("", "   "), "");
    }

    #[test]
    fn build_request_body_uses_defaults_and_all_filters() {
        // Driving the body with the crate defaults documents default handling
        // (default compiler id + default std → -std=c++20).
        let body = build_request_body("x", DEFAULT_STD, "", false, &[]);
        assert!(body.get("compiler").is_none());
        assert_eq!(body["source"], "x");
        assert_eq!(body["options"]["userArguments"], "-std=c++20");
        let filters = &body["options"]["filters"];
        assert_eq!(filters["intel"], json!(true));
        assert_eq!(filters["demangle"], json!(true));
        assert_eq!(filters["labels"], json!(true));
        assert_eq!(filters["directives"], json!(true));
        assert_eq!(filters["commentOnly"], json!(true));
        assert_eq!(filters["execute"], json!(false));
        assert_eq!(body["options"]["executeParameters"]["args"], "");
        assert_eq!(body["options"]["executeParameters"]["stdin"], "");
    }

    #[test]
    fn build_request_body_run_action_sets_execute_true() {
        let body = build_request_body("int main(){}", "", "-O0", true, &[]);
        // No std → userArguments is just the flags.
        assert_eq!(body["options"]["userArguments"], "-O0");
        assert_eq!(body["options"]["filters"]["execute"], json!(true));
    }

    #[test]
    fn severity_label_maps_numeric_and_string() {
        // Numeric severities: 3 -> error, 2 -> warning, anything else -> note.
        assert_eq!(severity_label(Some(&json!(3))), "error");
        assert_eq!(severity_label(Some(&json!(2))), "warning");
        assert_eq!(severity_label(Some(&json!(1))), "note");
        assert_eq!(severity_label(Some(&json!(99))), "note");
        // String severities.
        assert_eq!(severity_label(Some(&json!("error"))), "error");
        assert_eq!(severity_label(Some(&json!("warning"))), "warning");
        assert_eq!(severity_label(Some(&json!("info"))), "note");
        // Missing / null severity defaults to note.
        assert_eq!(severity_label(Some(&Value::Null)), "note");
        assert_eq!(severity_label(None), "note");
    }

    #[test]
    fn render_diagnostics_skips_empty_untagged_lines() {
        // Untagged entries with blank text are dropped; real text is kept verbatim.
        let sample = json!({
            "stderr": [
                { "text": "   " },
                { "text": "" },
                { "text": "real message" }
            ]
        });
        assert_eq!(
            render_diagnostics(&sample),
            vec!["real message".to_string()]
        );
        // No stderr key at all -> no diagnostics.
        assert!(render_diagnostics(&json!({ "code": 0 })).is_empty());
    }

    #[test]
    fn render_diagnostics_tag_defaults_to_source_and_item_text() {
        // A tag missing file/line/column falls back to <source>:0:0, and a tag
        // without its own text borrows the outer item `text`.
        let sample = json!({
            "stderr": [
                { "text": "outer text", "tag": { "severity": 2 } }
            ]
        });
        assert_eq!(
            render_diagnostics(&sample),
            vec!["<source>:0:0: warning: outer text".to_string()]
        );
    }

    #[test]
    fn shape_asm_output_renders_error_severity_and_no_asm() {
        // severity 3 -> error label; empty asm array -> "(no assembly produced)";
        // cache None -> no cache note.
        let sample = json!({
            "code": 1,
            "asm": [],
            "stderr": [
                {
                    "text": "ignored",
                    "tag": {
                        "line": 4,
                        "column": 9,
                        "severity": 3,
                        "text": "no matching function",
                        "file": "x.cpp"
                    }
                }
            ]
        });
        let out = shape_asm_output(&sample, None);
        assert_eq!(
            out,
            "compile exit code: 1\ndiagnostics:\nx.cpp:4:9: error: no matching function\nasm:\n(no assembly produced)"
        );
    }

    #[test]
    fn shape_asm_output_missing_asm_field_and_code_defaults() {
        // No `code` -> defaults to -1; no `asm` key -> "(no assembly produced)".
        let sample = json!({ "stderr": [] });
        let out = shape_asm_output(&sample, None);
        assert_eq!(out, "compile exit code: -1\nasm:\n(no assembly produced)");
    }

    #[test]
    fn collect_text_skips_nulls_and_missing_key() {
        let exec = json!({
            "stdout": [{ "text": "a" }, { "text": null }, { "text": "b" }]
        });
        assert_eq!(collect_text(&exec, "stdout"), "a\nb");
        // Missing key -> empty string.
        assert_eq!(collect_text(&exec, "stderr"), "");
    }

    #[test]
    fn push_cache_note_appends_only_when_present() {
        let mut hit = String::new();
        push_cache_note(&mut hit, Some("hit"));
        assert_eq!(hit, " (cache: hit)");

        let mut miss = String::new();
        push_cache_note(&mut miss, Some("miss"));
        assert_eq!(miss, " (cache: miss)");

        let mut none = String::new();
        push_cache_note(&mut none, None);
        assert_eq!(none, "");
    }

    #[test]
    fn shape_run_output_unknown_program_exit_code() {
        // execResult present but without a `code` -> "program exit code: unknown".
        let sample = json!({
            "code": 0,
            "stderr": [],
            "execResult": { "stdout": [{ "text": "hi" }] }
        });
        let out = shape_run_output(&sample, None);
        assert_eq!(
            out,
            "compile exit code: 0\nprogram exit code: unknown\nstdout:\nhi"
        );
    }

    #[test]
    fn shape_run_output_exec_result_code_only() {
        // execResult with a code but no stdout/stderr streams.
        let sample = json!({ "code": 0, "execResult": { "code": 0 } });
        let out = shape_run_output(&sample, None);
        assert_eq!(out, "compile exit code: 0\nprogram exit code: 0");
    }

    // ---- resolution helpers ----

    fn compiler(id: &str, name: &str) -> CompilerInfo {
        CompilerInfo {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn sample_compilers() -> Vec<CompilerInfo> {
        vec![
            compiler("g142", "x86-64 gcc 14.2"),
            compiler("g131", "x86-64 gcc 13.1"),
            compiler("armg1410", "ARM GCC 14.1.0"),
            compiler("armg1420", "ARM GCC 14.2.0"),
            compiler("armg1430", "ARM GCC 14.3.0"),
            compiler("armg1320", "ARM GCC 13.2.0"),
            compiler("clang1810", "x86-64 clang 18.1.0"),
        ]
    }

    fn lib(id: &str, name: &str, versions: &[(&str, &str)]) -> LibraryInfo {
        LibraryInfo {
            id: id.to_string(),
            name: name.to_string(),
            versions: versions
                .iter()
                .map(|(vid, ver)| LibVersion {
                    id: vid.to_string(),
                    version: ver.to_string(),
                })
                .collect(),
        }
    }

    fn sample_libs() -> Vec<LibraryInfo> {
        vec![
            lib(
                "fmt",
                "{fmt}",
                &[
                    ("1021", "10.2.1"),
                    ("1100", "11.0.0"),
                    ("1200", "12.0.0"),
                    ("trunk", "trunk"),
                ],
            ),
            lib(
                "rangesv3",
                "range-v3",
                &[("0110", "0.11.0"), ("trunk", "trunk")],
            ),
        ]
    }

    #[test]
    fn version_key_extracts_numeric_groups() {
        assert_eq!(version_key("ARM GCC 14.2.0"), vec![14, 2, 0]);
        assert_eq!(version_key("x86-64 gcc 9.5"), vec![86, 64, 9, 5]);
        assert!(version_key("trunk").is_empty());
    }

    #[test]
    fn resolve_compiler_exact_id_passes_through() {
        let got = resolve_compiler("armg1420", &sample_compilers()).unwrap();
        assert_eq!(got.id, "armg1420");
    }

    #[test]
    fn resolve_compiler_friendly_name_picks_highest_version() {
        // "arm gcc 14" must resolve to the newest 14.x ARM gcc, not 14.1/14.2.
        let got = resolve_compiler("arm gcc 14", &sample_compilers()).unwrap();
        assert_eq!(got.id, "armg1430");
        let x = resolve_compiler("x86-64 gcc 14", &sample_compilers()).unwrap();
        assert_eq!(x.id, "g142");
        let c = resolve_compiler("clang 18", &sample_compilers()).unwrap();
        assert_eq!(c.id, "clang1810");
    }

    #[test]
    fn an_unqualified_name_resolves_to_the_host_compiler() {
        // Measured on the real catalogue before the fix: "gcc" -> MSP430 gcc 6.2.1,
        // "gcc 14" -> s390x gcc 14.3.0, "clang" -> z180-clang, "arm gcc 14"
        // -> ARM64 (the sort key read the architecture's digits).
        let compilers = vec![
            compiler("g142", "x86-64 gcc 14.2"),
            compiler("g131", "x86-64 gcc 13.1"),
            compiler("msp430g621", "MSP430 gcc 6.2.1"),
            compiler("gccs390x1430", "s390x gcc 14.3.0"),
            compiler("z180clang1507", "z180-clang 15.0.7"),
            compiler("clang1810", "x86-64 clang 18.1.0"),
            compiler("armg1420", "ARM gcc 14.2 (linux)"),
            compiler("arm64g1420", "ARM64 gcc 14.2.0"),
        ];
        assert_eq!(resolve_compiler("gcc", &compilers).unwrap().id, "g142");
        assert_eq!(resolve_compiler("gcc 14", &compilers).unwrap().id, "g142");
        assert_eq!(
            resolve_compiler("clang", &compilers).unwrap().id,
            "clang1810"
        );
        assert_eq!(
            resolve_compiler("arm gcc 14", &compilers).unwrap().id,
            "armg1420"
        );
        assert_eq!(
            resolve_compiler("arm64 gcc", &compilers).unwrap().id,
            "arm64g1420"
        );
        assert_eq!(
            resolve_compiler("s390x gcc", &compilers).unwrap().id,
            "gccs390x1430"
        );
    }

    #[test]
    fn a_c_snippet_gets_a_c_standard_by_default() {
        // If `-std=c++20` is applied to language "c", every C compile
        // fails ("command-line option '-std=c++20' is valid for C++ ...").
        assert_eq!(default_std("c"), "c17");
        assert_eq!(default_std("cpp"), "c++20");
    }

    #[test]
    fn a_query_spanning_targets_with_no_host_match_lists_candidates() {
        let compilers = vec![
            compiler("mipsg1320", "mips gcc 13.2.0"),
            compiler("rv64g1320", "RISC-V (64-bits) gcc 13.2.0"),
        ];
        let err = resolve_compiler("gcc 13", &compilers).unwrap_err();
        assert!(
            err.contains("mipsg1320") && err.contains("rv64g1320"),
            "{err}"
        );
    }

    #[test]
    fn resolve_compiler_no_match_lists_candidates() {
        let err = resolve_compiler("arm gcc 99", &sample_compilers()).unwrap_err();
        assert!(err.contains("no Compiler Explorer compiler matches"));
        // suggestions are drawn from the strongest token ("gcc") and are real ids
        assert!(err.contains("armg1430") || err.contains("g142"));
    }

    #[test]
    fn resolve_library_newest_when_version_omitted() {
        // newest NON-trunk version is chosen.
        let (id, ver) = resolve_library("fmt", &sample_libs()).unwrap();
        assert_eq!(id, "fmt");
        assert_eq!(ver, "1200");
    }

    #[test]
    fn resolve_library_major_prefix_and_exact() {
        // "fmt 11" -> 11.0.0 (version id 1100), the documented gotcha.
        assert_eq!(
            resolve_library("fmt 11", &sample_libs()).unwrap(),
            ("fmt".into(), "1100".into())
        );
        // exact display version
        assert_eq!(
            resolve_library("fmt 10.2.1", &sample_libs()).unwrap(),
            ("fmt".into(), "1021".into())
        );
        // colon-separated CE version id
        assert_eq!(
            resolve_library("fmt:1100", &sample_libs()).unwrap(),
            ("fmt".into(), "1100".into())
        );
        // friendly name maps to id (range-v3 -> rangesv3), newest non-trunk
        assert_eq!(
            resolve_library("range-v3", &sample_libs()).unwrap(),
            ("rangesv3".into(), "0110".into())
        );
    }

    #[test]
    fn resolve_library_unknown_and_bad_version_error() {
        let e1 = resolve_library("boost", &sample_libs()).unwrap_err();
        assert!(e1.contains("no Compiler Explorer library matches"));
        let e2 = resolve_library("fmt 99", &sample_libs()).unwrap_err();
        assert!(e2.contains("has no version matching") && e2.contains("11.0.0"));
    }

    #[test]
    fn build_request_body_includes_resolved_libraries() {
        let libs = vec![("fmt".to_string(), "1100".to_string())];
        let body = build_request_body("x", "c++23", "-O3", false, &libs);
        assert_eq!(
            body["options"]["libraries"],
            json!([{ "id": "fmt", "version": "1100" }])
        );
    }

    #[test]
    fn shortlink_config_wires_editor_to_configured_compiler() {
        let libs = vec![("fmt".to_string(), "1100".to_string())];
        let cfg = shortlink_config("c++", "int main(){}", "armg1420", "-O3 -std=c++23", &libs);
        let panes = &cfg["config"]["content"][0]["content"];
        // editor pane carries the source + lang
        assert_eq!(panes[0]["componentName"], "codeEditor");
        assert_eq!(panes[0]["componentState"]["source"], "int main(){}");
        assert_eq!(panes[0]["componentState"]["lang"], "c++");
        // compiler pane reads editor 1 and is preloaded with id/options/libs
        let comp = &panes[1]["componentState"];
        assert_eq!(panes[1]["componentName"], "compiler");
        assert_eq!(comp["source"], 1);
        assert_eq!(comp["compiler"], "armg1420");
        assert_eq!(comp["options"], "-O3 -std=c++23");
        // shortener uses {name, ver} (NOT the compile endpoint's {id, version})
        assert_eq!(comp["libs"], json!([{ "name": "fmt", "ver": "1100" }]));
        assert_eq!(comp["filters"]["execute"], json!(false));
    }
}
