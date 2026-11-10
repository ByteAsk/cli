use std::collections::BTreeMap;
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
use codex_utils_redacted_string::RedactedString;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

const TOOL_NAME: &str = "cpp_intrinsic";
/// Number of results to request when searching by substring.
const SEARCH_LIMIT: u64 = 20;
/// Cap the gateway error body we echo back to the model.
const ERROR_BODY_CAP: usize = 1000;

/// Intel intrinsics lookup tool. Posts an intrinsic name (or substring query) to
/// the ByteAsk gateway, which serves an authoritative, deterministic lookup from
/// the Intel Intrinsics Guide dataset, and shapes the result into readable text.
pub struct IntrinsicTool {
    #[allow(dead_code)]
    pub cwd: PathBuf,
    /// ByteAsk provider base URL, e.g. `https://code.byteask.ai/byteask/v1`.
    pub gateway_base: String,
    /// Per-user JWT for the gateway, sent as `Authorization: Bearer <token>`.
    pub bearer: Option<RedactedString>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntrinsicArgs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for IntrinsicTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "name".to_string(),
            JsonSchema::string(Some(
                "Exact intrinsic name to look up, e.g. \"_mm_add_ps\". Returns its signature, \
                 required CPUID feature, header, underlying instruction, category and semantics."
                    .to_string(),
            )),
        );
        properties.insert(
            "query".to_string(),
            JsonSchema::string(Some(
                "Substring to search intrinsic names by, e.g. \"_mm_add\". Returns a list of \
                 matching names with their signatures. Use when you don't know the exact name."
                    .to_string(),
            )),
        );

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Look up an Intel SIMD intrinsic (e.g. _mm_add_ps) — exact signature, \
                required CPUID feature, header, underlying instruction, and semantics — or search \
                intrinsics by substring. Authoritative (no hallucination). Use when writing/reading \
                SIMD/vector C/C++. C/C++ only."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec![]),
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

impl IntrinsicTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: IntrinsicArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;

        let name = args.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let query = args.query.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let (body, is_search, label) = build_request(name, query)?;

        let url = lookup_url(&self.gateway_base);

        let client = create_client();
        let mut request = client.post(url.as_str()).json(&body);
        if let Some(bearer) = &self.bearer {
            request = request.header("Authorization", format!("Bearer {}", bearer.as_str()));
        }

        // Not `Fatal`: that ends the user's whole turn; offline or
        // signed out is something the model can report.
        let response = request.send().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "the intrinsic lookup service is unavailable: the request failed ({err}). \
                 Nothing was looked up; answer from the Intel Intrinsics Guide \
                 (site:intel.com) instead, and say so."
            ))
        })?;

        let status = response.status();
        if !status.is_success() {
            let mut body = response.text().await.unwrap_or_default();
            body.truncate(body.floor_char_boundary(ERROR_BODY_CAP));
            return Err(FunctionCallError::RespondToModel(format!(
                "intrinsic gateway returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        let value: Value = response.json().await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid JSON from intrinsic gateway: {err}"))
        })?;

        let rendered = if is_search {
            shape_search_output(&value)
        } else {
            shape_lookup_output(&value)
        };

        let result = if rendered.starts_with("no matching intrinsic") {
            check_result::CheckResult::no_claim(TOOL_NAME, "No intrinsic matched in the Intel Intrinsics Guide data.")
        } else {
            check_result::CheckResult::pass(TOOL_NAME, "Looked up the Intel Intrinsics Guide data.")
        };
        Ok(crate::report::report(TOOL_NAME, &label, rendered, result))
    }
}

/// Build the gateway request body, search flag, and target label from the parsed
/// args. `name` takes precedence over `query` when both are supplied; at least
/// one of `name`/`query` must be present.
fn build_request(
    name: Option<&str>,
    query: Option<&str>,
) -> Result<(Value, bool, String), FunctionCallError> {
    match (name, query) {
        (Some(name), _) => Ok((json!({ "name": name }), false, name.to_string())),
        (None, Some(query)) => Ok((
            json!({ "query": query, "limit": SEARCH_LIMIT }),
            true,
            query.to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "provide either \"name\" (exact intrinsic) or \"query\" (substring search)".to_string(),
        )),
    }
}

/// Lookup endpoint URL: take the provider base, strip a trailing `/v1`, then
/// append `/v1/intrinsics/lookup`. Both `.../byteask/v1` and `.../byteask` map to
/// `.../byteask/v1/intrinsics/lookup`.
fn lookup_url(gateway_base: &str) -> String {
    let base = gateway_base.trim_end_matches('/');
    let origin = base.strip_suffix("/v1").unwrap_or(base);
    format!("{origin}/v1/intrinsics/lookup")
}

fn field<'a>(rec: &'a Value, key: &str) -> &'a str {
    rec.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Render a single intrinsic record (the `{result}` payload) as readable text.
fn shape_lookup_output(value: &Value) -> String {
    let rec = value.get("result").unwrap_or(&Value::Null);
    if rec.is_null() {
        return "no matching intrinsic found".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!("name:        {}\n", field(rec, "name")));
    out.push_str(&format!("signature:   {}\n", field(rec, "signature")));
    out.push_str(&format!("cpuid:       {}\n", field(rec, "cpuid")));
    out.push_str(&format!("header:      {}\n", field(rec, "header")));
    out.push_str(&format!("instruction: {}\n", field(rec, "instruction")));
    out.push_str(&format!("category:    {}\n", field(rec, "category")));
    out.push_str(&format!("description: {}", field(rec, "description")));
    let operation = field(rec, "operation");
    if !operation.is_empty() {
        out.push_str("\noperation:\n");
        out.push_str(operation);
    }
    out
}

/// Render the `{results}` list as `name  signature` lines.
fn shape_search_output(value: &Value) -> String {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return "no matching intrinsics found".to_string();
    };
    if results.is_empty() {
        return "no matching intrinsics found".to_string();
    }

    // The service returns at most SEARCH_LIMIT results and no total,
    // so a full page is "the first N", not "N matching".
    let mut out = if results.len() as u64 >= SEARCH_LIMIT {
        format!(
            "first {} matching intrinsics (the search returns at most {SEARCH_LIMIT}; there may be \
             more — narrow the query):\n",
            results.len()
        )
    } else {
        format!("{} matching intrinsic(s):\n", results.len())
    };
    for rec in results {
        out.push_str(&format!(
            "{:<28} {}\n",
            field(rec, "name"),
            field(rec, "signature")
        ));
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn tool(gateway_base: String) -> IntrinsicTool {
        IntrinsicTool {
            cwd: std::env::temp_dir(),
            gateway_base,
            bearer: None,
        }
    }

    #[tokio::test]
    async fn an_unreachable_gateway_does_not_end_the_turn() {
        // Offline / signed out must not be `Fatal` — that ends the turn.
        let err = crate::test_support::call(
            &tool(crate::test_support::closed_port_url()),
            json!({"name": "_mm_add_ps"}),
        )
        .await
        .expect_err("nothing listens there");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("must go back to the model, not end the turn: {err:?}");
        };
        assert!(message.contains("unavailable"), "{message}");
    }

    #[tokio::test]
    async fn an_error_body_with_a_multibyte_char_at_the_cap_does_not_panic() {
        // `String::truncate(ERROR_BODY_CAP)` panics inside `—`.
        let base = crate::test_support::serve_http(vec![(
            "/",
            503,
            "text/html",
            crate::test_support::body_straddling(ERROR_BODY_CAP),
        )]);
        let err = crate::test_support::call(&tool(base), json!({"name": "_mm_add_ps"}))
            .await
            .expect_err("503");
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected a model-facing error, got {err:?}");
        };
        assert!(message.contains("HTTP 503"), "{message}");
    }

    #[test]
    fn lookup_url_strips_trailing_v1() {
        assert_eq!(
            lookup_url("https://code.byteask.ai/byteask/v1"),
            "https://code.byteask.ai/byteask/v1/intrinsics/lookup"
        );
    }

    #[test]
    fn lookup_url_handles_base_without_v1_and_trailing_slash() {
        assert_eq!(
            lookup_url("https://code.byteask.ai/byteask"),
            "https://code.byteask.ai/byteask/v1/intrinsics/lookup"
        );
        assert_eq!(
            lookup_url("https://code.byteask.ai/byteask/v1/"),
            "https://code.byteask.ai/byteask/v1/intrinsics/lookup"
        );
    }

    #[test]
    fn build_request_name_constructs_name_body() {
        let (body, is_search, label) = build_request(Some("_mm_add_ps"), None).expect("ok");
        assert_eq!(body, json!({ "name": "_mm_add_ps" }));
        assert!(!is_search);
        assert_eq!(label, "_mm_add_ps");
    }

    #[test]
    fn build_request_query_constructs_query_body_with_limit() {
        let (body, is_search, label) = build_request(None, Some("_mm_add")).expect("ok");
        assert_eq!(body, json!({ "query": "_mm_add", "limit": 20 }));
        assert!(is_search);
        assert_eq!(label, "_mm_add");
    }

    #[test]
    fn build_request_name_takes_precedence_over_query() {
        let (body, is_search, label) =
            build_request(Some("_mm_add_ps"), Some("_mm_add")).expect("ok");
        assert_eq!(body, json!({ "name": "_mm_add_ps" }));
        assert!(!is_search);
        assert_eq!(label, "_mm_add_ps");
    }

    #[test]
    fn build_request_requires_name_or_query() {
        match build_request(None, None) {
            Err(FunctionCallError::RespondToModel(msg)) => {
                assert!(msg.contains("provide either"), "unexpected message: {msg}");
            }
            other => panic!("expected RespondToModel error, got a different result: {:?}", other.is_ok()),
        }
    }

    #[test]
    fn shape_lookup_output_renders_all_fields() {
        let sample = json!({
            "result": {
                "name": "_mm_add_ps",
                "signature": "__m128 _mm_add_ps(__m128 a, __m128 b)",
                "cpuid": "SSE",
                "header": "xmmintrin.h",
                "instruction": "ADDPS",
                "category": "Arithmetic",
                "description": "Add packed single-precision elements in \"a\" and \"b\".",
                "operation": "FOR j := 0 to 3\n\tdst[i+31:i] := a[i+31:i] + b[i+31:i]\nENDFOR"
            }
        });
        let out = shape_lookup_output(&sample);
        assert_eq!(
            out,
            "name:        _mm_add_ps\n\
             signature:   __m128 _mm_add_ps(__m128 a, __m128 b)\n\
             cpuid:       SSE\n\
             header:      xmmintrin.h\n\
             instruction: ADDPS\n\
             category:    Arithmetic\n\
             description: Add packed single-precision elements in \"a\" and \"b\".\n\
             operation:\n\
             FOR j := 0 to 3\n\tdst[i+31:i] := a[i+31:i] + b[i+31:i]\nENDFOR"
        );
    }

    #[test]
    fn shape_lookup_output_omits_empty_operation() {
        let sample = json!({
            "result": {
                "name": "_mm_pause",
                "signature": "void _mm_pause(void)",
                "cpuid": "SSE2",
                "header": "emmintrin.h",
                "instruction": "PAUSE",
                "category": "General Support",
                "description": "Provide a hint to the processor.",
                "operation": ""
            }
        });
        let out = shape_lookup_output(&sample);
        assert!(out.ends_with("description: Provide a hint to the processor."));
        assert!(!out.contains("operation:"));
    }

    #[test]
    fn shape_lookup_output_omits_missing_operation_key() {
        // No `operation` key at all (not just empty) → still omitted.
        let sample = json!({
            "result": {
                "name": "_mm_pause",
                "signature": "void _mm_pause(void)",
                "cpuid": "SSE2",
                "header": "emmintrin.h",
                "instruction": "PAUSE",
                "category": "General Support",
                "description": "Provide a hint to the processor."
            }
        });
        let out = shape_lookup_output(&sample);
        assert!(out.ends_with("description: Provide a hint to the processor."));
        assert!(!out.contains("operation:"));
    }

    #[test]
    fn shape_lookup_output_renders_missing_optional_fields_as_empty() {
        // Malformed/partial record: only `name` present, every other field absent.
        let sample = json!({ "result": { "name": "_mm_foo" } });
        let out = shape_lookup_output(&sample);
        assert_eq!(
            out,
            "name:        _mm_foo\n\
             signature:   \n\
             cpuid:       \n\
             header:      \n\
             instruction: \n\
             category:    \n\
             description: "
        );
        // No operation key → no operation section.
        assert!(!out.contains("operation:"));
    }

    #[test]
    fn shape_lookup_output_handles_null_result() {
        let sample = json!({ "result": null });
        assert_eq!(shape_lookup_output(&sample), "no matching intrinsic found");
    }

    #[test]
    fn shape_lookup_output_handles_missing_result_key() {
        // Body without a `result` key at all → treated as no match.
        assert_eq!(shape_lookup_output(&json!({})), "no matching intrinsic found");
    }

    #[test]
    fn a_full_page_of_results_is_not_called_the_total() {
        let full: Vec<serde_json::Value> = (0..SEARCH_LIMIT)
            .map(|i| json!({"name": format!("_mm_x{i}"), "signature": "void f()"}))
            .collect();
        let out = shape_search_output(&json!({ "results": full }));
        assert!(out.starts_with("first 20 matching intrinsics"), "{out}");
        assert!(out.contains("there may be more"), "{out}");
    }

    #[test]
    fn shape_search_output_lists_names_and_signatures() {
        let sample = json!({
            "results": [
                { "name": "_mm_add_ps", "signature": "__m128 _mm_add_ps(__m128 a, __m128 b)" },
                { "name": "_mm_add_pd", "signature": "__m128d _mm_add_pd(__m128d a, __m128d b)" }
            ]
        });
        let out = shape_search_output(&sample);
        assert_eq!(
            out,
            "2 matching intrinsic(s):\n\
             _mm_add_ps                   __m128 _mm_add_ps(__m128 a, __m128 b)\n\
             _mm_add_pd                   __m128d _mm_add_pd(__m128d a, __m128d b)"
        );
    }

    #[test]
    fn shape_search_output_renders_single_result() {
        let sample = json!({
            "results": [
                { "name": "_mm_add_ps", "signature": "__m128 _mm_add_ps(__m128 a, __m128 b)" }
            ]
        });
        assert_eq!(
            shape_search_output(&sample),
            "1 matching intrinsic(s):\n\
             _mm_add_ps                   __m128 _mm_add_ps(__m128 a, __m128 b)"
        );
    }

    #[test]
    fn shape_search_output_handles_empty() {
        assert_eq!(
            shape_search_output(&json!({ "results": [] })),
            "no matching intrinsics found"
        );
    }

    #[test]
    fn shape_search_output_handles_missing_results_key() {
        // No `results` array (missing or non-array) → no-match message, not a panic.
        assert_eq!(
            shape_search_output(&json!({})),
            "no matching intrinsics found"
        );
        assert_eq!(
            shape_search_output(&json!({ "results": "oops" })),
            "no matching intrinsics found"
        );
    }

    #[test]
    fn shape_search_output_renders_entries_with_missing_fields_as_empty() {
        // Malformed entries: missing name/signature render as empty columns.
        let sample = json!({ "results": [ {}, { "name": "_mm_add_ps" } ] });
        assert_eq!(
            shape_search_output(&sample),
            "2 matching intrinsic(s):\n\
             {empty}\n\
             _mm_add_ps".replace("{empty}", &format!("{:<28} ", ""))
        );
    }
}
