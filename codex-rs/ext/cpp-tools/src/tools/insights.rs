use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

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

const TOOL_NAME: &str = "cpp_insights";
const DEFAULT_STD: &str = "c++20";
/// Cap on the transformed source handed back to the model.
const MAX_LINES: usize = 400;
/// Public C++ Insights origin. Requests go directly from the user's machine to
/// the cppinsights.io HTTP API (no local `insights` binary required). Override
/// with `BYTEASK_CPPINSIGHTS_URL` (used by tests).
const CPPINSIGHTS_ORIGIN: &str = "https://cppinsights.io";
/// Cap the error body we echo back to the model.
const ERROR_BODY_CAP: usize = 1000;

pub struct InsightsTool {
    pub cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InsightsArgs {
    source: String,
    #[serde(default)]
    std: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for InsightsTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "source".to_string(),
            JsonSchema::string(Some(
                "Path to a .cpp file (relative to the workspace or absolute) OR inline C++ source \
                 code to transform."
                    .to_string(),
            )),
        );
        properties.insert(
            "std".to_string(),
            JsonSchema::string(Some(format!(
                "C++ standard to compile against (default: {DEFAULT_STD})."
            ))),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Show desugared C++ via C++ Insights — how the compiler sees the code \
                (template instantiations, implicit conversions/copies, generated special members, \
                lambda closures, range-for expansion). Use proactively to explain *why* C++ \
                behaves a certain way. Complements cpp_godbolt (which shows assembly). C++ only."
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

impl InsightsTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: InsightsArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let std = args.std.as_deref().unwrap_or(DEFAULT_STD);

        // The `source` may be an existing file path under the workspace, or
        // inline C++ source. The cppinsights.io API takes the source *text*, so
        // read the file's contents when `source` names one.
        // A path that does not exist must not be sent to the site AS CODE:
        // it answers "Compilation failed!" with the real reason hidden.
        if resolve_existing_file(&self.cwd, &args.source).is_none() && looks_like_path(&args.source) {
            return Err(FunctionCallError::RespondToModel(format!(
                "no such file: {} (relative to the workspace). Pass an existing file, or the \
                 C++ code itself.",
                args.source.trim()
            )));
        }
        let (code, source_label) = match resolve_existing_file(&self.cwd, &args.source) {
            Some(path) => {
                let label = path.display().to_string();
                let contents = tokio::fs::read_to_string(&path).await.map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to read {}: {err}",
                        path.display()
                    ))
                })?;
                (contents, label)
            }
            None => (args.source.clone(), "<inline>".to_string()),
        };

        let body = build_request_body(&code, std);
        let url = transform_url();

        let client = create_client();
        let response = client
            .post(url.as_str())
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "C++ Insights (cppinsights.io) is unavailable: request failed: {err}"
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            let mut text = response.text().await.unwrap_or_default();
            text.truncate(text.floor_char_boundary(ERROR_BODY_CAP));
            return Err(FunctionCallError::RespondToModel(format!(
                "C++ Insights (cppinsights.io) is unavailable: HTTP {}: {text}",
                status.as_u16()
            )));
        }

        let value: Value = response.json().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "C++ Insights (cppinsights.io) is unavailable: invalid JSON: {err}"
            ))
        })?;

        // A compile error comes back as returncode != 0 with the reason in
        // `stderr`; `stdout` alone says only "Compilation failed!".
        if let Some(failure) = compile_failure(&value) {
            return Ok(crate::report::report(
                TOOL_NAME,
                &source_label,
                failure,
                check_result::CheckResult::fail(TOOL_NAME, "C++ Insights could not compile the code (its error is above)."),
            ));
        }
        let transformed = parse_stdout(&value).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "C++ Insights (cppinsights.io) is unavailable: response missing `stdout`."
                    .to_string(),
            )
        })?;

        let text = truncate_lines(&transformed, MAX_LINES);
        let result = if text.ends_with("... [output truncated]") {
            check_result::CheckResult::approximate(TOOL_NAME, "The transformed code was longer than this tool shows, so it was cut.")
        } else {
            check_result::CheckResult::pass(TOOL_NAME, "C++ Insights showed what the compiler generates.")
        };
        Ok(crate::report::report(TOOL_NAME, &source_label, text, result))
    }
}

/// Transform endpoint URL: `<origin>/api/v1/transform`. Origin defaults to
/// cppinsights.io and is overridable via `BYTEASK_CPPINSIGHTS_URL` for tests.
fn transform_url() -> String {
    let origin = std::env::var("BYTEASK_CPPINSIGHTS_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CPPINSIGHTS_ORIGIN.to_string());
    let origin = origin.trim_end_matches('/');
    format!("{origin}/api/v1/transform")
}

/// Map the tool's `std` to a cppinsights `insightsOptions` value. cppinsights
/// uses the `cppNN` form (e.g. `cpp20`), not `c++NN`: a leading `c++` is
/// rewritten to `cpp`. An already-`cppNN` value passes through unchanged.
/// The site calls C++26 `cpp2c`; `cpp26` is not an option.
fn insights_option(std: &str) -> String {
    let option = match std.strip_prefix("c++") {
        Some(rest) => format!("cpp{rest}"),
        None => std.to_string(),
    };
    if option == "cpp26" { "cpp2c".to_string() } else { option }
}

/// The API's own report of a failed transformation: a nonzero `returncode`,
/// with the compiler's reason in `stderr`.
fn compile_failure(value: &Value) -> Option<String> {
    let code = value.get("returncode").and_then(Value::as_i64)?;
    if code == 0 {
        return None;
    }
    let stderr = value.get("stderr").and_then(Value::as_str).unwrap_or("").trim();
    Some(format!(
        "C++ Insights could not transform this code (the compiler failed, return code {code}):\n{}",
        truncate_lines(if stderr.is_empty() { "(no compiler output)" } else { stderr }, MAX_LINES)
    ))
}

/// Does `source` look like a file path rather than code? One line, a C/C++
/// source or header extension, and none of the characters code is made of.
fn looks_like_path(source: &str) -> bool {
    let source = source.trim();
    let ext = source.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase());
    !source.contains('\n')
        && !source.contains(['{', '}', ';', '(', ')', '<', '>', '#', '='])
        && ext.is_some_and(|ext| {
            ["c", "cc", "cpp", "cxx", "c++", "h", "hh", "hpp", "hxx", "ipp", "tpp", "inl"]
                .contains(&ext.as_str())
        })
}

/// Build the cppinsights.io request body: `{insightsOptions: [<option>], code}`.
fn build_request_body(code: &str, std: &str) -> Value {
    json!({
        "insightsOptions": [insights_option(std)],
        "code": code,
    })
}

/// Extract the transformed C++ from the API response's `stdout` string field.
fn parse_stdout(value: &Value) -> Option<String> {
    value
        .get("stdout")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// If `source` names an existing file (resolved against `cwd`), return its
/// resolved path. Otherwise `None` — the caller treats `source` as inline code.
fn resolve_existing_file(cwd: &Path, source: &str) -> Option<PathBuf> {
    let candidate = Path::new(source);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    if resolved.is_file() {
        Some(resolved)
    } else {
        None
    }
}

/// Truncate `text` to at most `max_lines` lines, appending a marker when cut.
fn truncate_lines(text: &str, max_lines: usize) -> String {
    let mut lines = text.lines();
    let kept: Vec<&str> = lines.by_ref().take(max_lines).collect();
    let mut out = kept.join("\n");
    if lines.next().is_some() {
        out.push_str("\n... [output truncated]");
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    /// `BYTEASK_CPPINSIGHTS_URL` is process-global and tests run on parallel
    /// threads: every test that sets it holds this lock.
    static INSIGHTS_URL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_missing_file_path_is_not_sent_as_code() {
        // "src/mian.cpp" (a typo) must not go to the site as C++ source.
        assert!(looks_like_path("src/mian.cpp"));
        assert!(looks_like_path("dir with space/a.hpp"));
        assert!(!looks_like_path("int main(){}"));
        assert!(!looks_like_path("#include <vector>\nint x;"));
        assert!(!looks_like_path("std::vector<int> v;"));
    }

    #[test]
    fn a_compile_error_is_shown_with_its_reason() {
        // returncode/stderr must be kept; otherwise only "Compilation failed!" shows.
        let failed = json!({"returncode": 1, "stdout": "Compilation failed!", "stderr": "a.cpp:1:5: error: expected ';'"});
        let text = compile_failure(&failed).expect("a failure");
        assert!(text.contains("expected ';'"), "{text}");
        assert_eq!(compile_failure(&json!({"returncode": 0, "stdout": "int x;"})), None);
    }

    #[test]
    fn cpp26_uses_the_sites_name() {
        // The site's option for C++26 is `cpp2c`.
        assert_eq!(insights_option("c++26"), "cpp2c");
        assert_eq!(insights_option("c++2c"), "cpp2c");
        assert_eq!(insights_option("c++20"), "cpp20");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the env lock must span the call
    async fn an_error_body_with_a_multibyte_char_at_the_cap_does_not_panic() {
        // `String::truncate(ERROR_BODY_CAP)` panics inside `—`.
        let base = crate::test_support::serve_http(vec![(
            "/",
            500,
            "text/html",
            crate::test_support::body_straddling(ERROR_BODY_CAP),
        )]);
        let _env = INSIGHTS_URL_ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: every test that sets this variable holds INSIGHTS_URL_ENV.
        unsafe { std::env::set_var("BYTEASK_CPPINSIGHTS_URL", &base) };
        let tool = InsightsTool {
            cwd: std::env::temp_dir(),
        };
        let out = crate::test_support::call(&tool, json!({"source": "int main(){}"})).await;
        unsafe { std::env::remove_var("BYTEASK_CPPINSIGHTS_URL") };
        let err = out.expect_err("500");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected a model-facing error, got {err:?}");
        };
        assert!(message.contains("HTTP 500"), "{message}");
    }

    #[test]
    fn resolves_existing_file_relative_and_absolute() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("byteask_insights_test_{}.cpp", std::process::id()));
        std::fs::write(&file, "int main(){}").unwrap();

        // Absolute path resolves to itself.
        assert_eq!(
            resolve_existing_file(Path::new("/nonexistent"), file.to_str().unwrap()),
            Some(file.clone())
        );

        // Relative path resolves against cwd.
        let name = file.file_name().unwrap().to_str().unwrap();
        assert_eq!(resolve_existing_file(&dir, name), Some(file.clone()));

        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn inline_code_is_not_an_existing_file() {
        let code = "int main() { return 0; }";
        assert_eq!(resolve_existing_file(Path::new("/work"), code), None);
    }

    #[test]
    fn truncate_lines_caps_output() {
        let text = (0..500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_lines(&text, MAX_LINES);
        assert_eq!(truncated.lines().count(), MAX_LINES + 1);
        assert!(truncated.ends_with("[output truncated]"));
    }

    #[test]
    fn truncate_lines_keeps_short_output() {
        let text = "line one\nline two";
        assert_eq!(truncate_lines(text, MAX_LINES), text);
    }

    /// A stand-in for the transformed source C++ Insights emits, with a known
    /// line count so we can exercise the truncation cap precisely.
    fn fake_transformed_source(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("// insight line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn truncate_lines_at_exact_cap_keeps_all() {
        let text = fake_transformed_source(MAX_LINES);
        let out = truncate_lines(&text, MAX_LINES);
        assert_eq!(out.lines().count(), MAX_LINES);
        assert!(!out.contains("[output truncated]"));
        assert_eq!(out, text);
    }

    #[test]
    fn truncate_lines_over_cap_marks_and_caps() {
        let text = fake_transformed_source(MAX_LINES + 25);
        let out = truncate_lines(&text, MAX_LINES);
        // MAX_LINES kept plus the appended truncation-marker line.
        assert_eq!(out.lines().count(), MAX_LINES + 1);
        assert!(out.ends_with("[output truncated]"));
    }

    #[test]
    fn truncate_lines_under_cap_unchanged() {
        let text = fake_transformed_source(MAX_LINES - 1);
        let out = truncate_lines(&text, MAX_LINES);
        assert_eq!(out, text);
        assert!(!out.contains("[output truncated]"));
    }

    #[test]
    fn detection_uses_file_directly_else_inline() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("byteask_insights_detect_{}.cpp", std::process::id()));
        std::fs::write(&file, "int main(){}").unwrap();
        let name = file.file_name().unwrap().to_str().unwrap();

        // Existing file under cwd -> resolved + used directly (Some).
        assert_eq!(resolve_existing_file(&dir, name), Some(file.clone()));
        // Inline (non-path) source -> None, so the caller sends it verbatim.
        assert_eq!(
            resolve_existing_file(&dir, "struct S{}; int main(){ S s; (void)s; }"),
            None
        );

        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn insights_option_maps_cxx_to_cpp() {
        // The canonical `c++NN` form is rewritten to `cppNN`.
        assert_eq!(insights_option("c++20"), "cpp20");
        assert_eq!(insights_option("c++17"), "cpp17");
        assert_eq!(insights_option("c++23"), "cpp23");
        // The crate default maps the same way.
        assert_eq!(insights_option(DEFAULT_STD), "cpp20");
    }

    #[test]
    fn insights_option_passes_through_already_cpp_form() {
        // Already in `cppNN` form -> unchanged (no double-prefix).
        assert_eq!(insights_option("cpp20"), "cpp20");
        assert_eq!(insights_option("cpp17"), "cpp17");
    }

    #[test]
    fn insights_option_leaves_unknown_forms_untouched() {
        // Anything without the `c++` prefix passes through verbatim.
        assert_eq!(insights_option("gnu++20"), "gnu++20");
        assert_eq!(insights_option(""), "");
    }

    #[test]
    fn std_defaults_when_absent_and_overrides_when_present() {
        // Absent `std` -> DEFAULT_STD, which maps to cpp20.
        let absent: InsightsArgs = serde_json::from_str(r#"{"source":"a.cpp"}"#).unwrap();
        let std = absent.std.as_deref().unwrap_or(DEFAULT_STD);
        assert_eq!(std, DEFAULT_STD);
        assert_eq!(insights_option(std), "cpp20");

        // Present `std` -> the override is honored and mapped.
        let present: InsightsArgs =
            serde_json::from_str(r#"{"source":"a.cpp","std":"c++17"}"#).unwrap();
        let std = present.std.as_deref().unwrap_or(DEFAULT_STD);
        assert_eq!(std, "c++17");
        assert_eq!(insights_option(std), "cpp17");
    }

    #[test]
    fn build_request_body_shapes_cppinsights_request() {
        let body = build_request_body("int main(){ auto x = 1; }", "c++20");
        assert_eq!(body["insightsOptions"], json!(["cpp20"]));
        assert_eq!(body["code"], "int main(){ auto x = 1; }");
        // Exactly the two documented fields.
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("insightsOptions"));
        assert!(obj.contains_key("code"));
    }

    #[test]
    fn build_request_body_maps_std_into_options() {
        // The std mapping flows through into the request body's insightsOptions.
        let body = build_request_body("x", "c++17");
        assert_eq!(body["insightsOptions"], json!(["cpp17"]));
        // Already-cpp form is not double-prefixed.
        let body = build_request_body("x", "cpp23");
        assert_eq!(body["insightsOptions"], json!(["cpp23"]));
    }

    #[test]
    fn build_request_body_uses_default_std() {
        let body = build_request_body("int main(){}", DEFAULT_STD);
        assert_eq!(body["insightsOptions"], json!(["cpp20"]));
        assert_eq!(body["code"], "int main(){}");
    }

    #[test]
    fn parse_stdout_extracts_transformed_source() {
        // A representative cppinsights.io response: stdout holds the transform.
        let sample = json!({
            "returncode": 0,
            "stderr": "Insights exited with result code: 0",
            "stdout": "int main()\n{\n  int x = 1;\n  return 0;\n}\n"
        });
        assert_eq!(
            parse_stdout(&sample),
            Some("int main()\n{\n  int x = 1;\n  return 0;\n}\n".to_string())
        );
    }

    #[test]
    fn parse_stdout_missing_field_is_none() {
        // No `stdout` key -> None (handler maps this to RespondToModel).
        let sample = json!({ "returncode": 1, "stderr": "boom" });
        assert_eq!(parse_stdout(&sample), None);
        // `stdout` present but not a string -> None.
        let sample = json!({ "stdout": 42 });
        assert_eq!(parse_stdout(&sample), None);
    }

    #[test]
    fn parse_stdout_empty_string_is_some_empty() {
        // An empty (but present, string) stdout is a successful empty transform.
        let sample = json!({ "stdout": "" });
        assert_eq!(parse_stdout(&sample), Some(String::new()));
    }

    #[test]
    fn transform_url_targets_public_cppinsights_endpoint() {
        // No override -> the public cppinsights.io transform endpoint.
        let _env = INSIGHTS_URL_ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: every test that sets this variable holds INSIGHTS_URL_ENV.
        unsafe { std::env::remove_var("BYTEASK_CPPINSIGHTS_URL") };
        assert_eq!(transform_url(), "https://cppinsights.io/api/v1/transform");
    }

    #[test]
    fn transform_url_honors_env_override_and_trims_slash() {
        let _env = INSIGHTS_URL_ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: every test that sets this variable holds INSIGHTS_URL_ENV.
        unsafe { std::env::set_var("BYTEASK_CPPINSIGHTS_URL", "http://localhost:9000/") };
        assert_eq!(transform_url(), "http://localhost:9000/api/v1/transform");
        unsafe { std::env::remove_var("BYTEASK_CPPINSIGHTS_URL") };
    }
}
