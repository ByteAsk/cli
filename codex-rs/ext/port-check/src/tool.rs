//! The tool contract registered in the toolbox: name, description, input schema, and a
//! JSON-in / JSON-out entry point. The description is pinned by a snapshot test (once it
//! ships, never shorten it without a reviewed diff).

use serde_json::Value;
use serde_json::json;

use crate::check::PortCheckRequest;
use crate::check::PortCheckResult;
use crate::check::Status;
use crate::check::port_check;

pub const TOOL_NAME: &str = "port_check";

pub const TOOL_DESCRIPTION: &str = "Check a C++ port of a Python trading strategy against the original by replaying recorded market data through BOTH under one deterministic harness: the same initial book, market clock, seed, simulated fills and rejects, and event order. The Python version runs twice with different hash seeds and must reproduce itself exactly before anything is claimed; then the C++ version must send the same orders, cancels and timers and make the same decisions at every step, and end with the same positions and cash. Only `diag` numbers may differ, and only within `diag_tolerance`; orders and decisions are always exact. The C++ version also runs a second time with new heap memory filled with a pattern, to catch reads of uninitialised memory.\n\nReturns PASS, FAIL (the first step that differs, the field, both values and both rows), NOT RUN (with the reason, e.g. the Python version is not reproducible, a version could not start or timed out, the recording is inconsistent) or NO CLAIM (nothing traded, so nothing about order logic was checked). Only PASS is success.\n\nBoth versions must be built on the port_harness adapters (Python: port_harness.py, C++: port_harness.hpp, header-only C++17); get them with `byteask-port-check adapters DIR`. Build the C++ binary first; this tool runs it, it does not compile it. Prices and quantities are integers. The recording is JSON Lines of add/cancel/delete/exec/replace/trade events with integer `ts` nanoseconds, or a Nasdaq TotalView-ITCH 5.0 capture (`recording_format` itch50). A PASS covers this recording and these harness settings only, and is not a speed result.";

/// JSON Schema of the request.
pub fn input_schema() -> Value {
    let process = json!({
        "type": "object",
        "required": ["argv"],
        "additionalProperties": false,
        "properties": {
            "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                     "description": "Command and arguments, e.g. [\"python3\", \"strategy_main.py\"] or [\"./build/strategy\"]."},
            "env": {"type": "object", "additionalProperties": {"type": "string"},
                    "description": "Extra environment (the rest is cleared; PATH is kept). PYTHONPATH must reach port_harness.py."},
            "cwd": {"type": "string"}
        }
    });
    json!({
        "type": "object",
        "required": ["recording", "python", "cpp"],
        "additionalProperties": false,
        "properties": {
            "recording": {"type": "string", "description": "Path to the JSON Lines recording."},
            "recording_format": {"type": "string", "enum": ["events_jsonl", "itch50"],
                                 "description": "events_jsonl (default) or itch50: a Nasdaq TotalView-ITCH 5.0 capture with a 2-byte length before every message."},
            "initial_book": {"type": "string", "description": "JSON Lines of `add` records resting before the first event."},
            "python": process,
            "cpp": process,
            "harness": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "seed": {"type": "integer", "minimum": 0},
                    "latency_ns": {"type": "integer", "minimum": 0},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 20},
                    "tick": {"type": "integer", "minimum": 1},
                    "max_order_qty": {"type": "integer", "minimum": 0},
                    "max_position": {"type": "integer", "minimum": 0},
                    "price_band": {"type": "integer", "minimum": 0},
                    "reject_every": {"type": "integer", "minimum": 0},
                    "reject_ppm": {"type": "integer", "minimum": 0, "maximum": 1000000},
                    "strict_book": {"type": "boolean"},
                    "params": {"type": "object", "additionalProperties": {"type": "string"}}
                }
            },
            "diag_tolerance": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "abs": {"type": "number", "minimum": 0},
                    "rel": {"type": "number", "minimum": 0, "exclusiveMaximum": 1},
                    "ulps": {"type": "integer", "minimum": 0}
                }
            },
            "limits": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "first_response_timeout_ms": {"type": "integer", "minimum": 1},
                    "response_timeout_ms": {"type": "integer", "minimum": 1},
                    "run_timeout_ms": {"type": "integer", "minimum": 1},
                    "max_steps": {"type": "integer", "minimum": 1},
                    "max_steps_per_ts": {"type": "integer", "minimum": 1}
                }
            },
            "python_runs": {"type": "integer", "minimum": 2, "maximum": 5},
            "cpp_runs": {"type": "integer", "minimum": 1, "maximum": 5},
            "workdir": {"type": "string"}
        }
    })
}

/// Everything a registry needs to expose the tool.
pub fn tool_spec() -> Value {
    json!({"name": TOOL_NAME, "description": TOOL_DESCRIPTION, "input_schema": input_schema()})
}

/// Parse a request, run the check, return the result. A malformed request is a NOT RUN result,
/// never a panic or an empty answer.
pub fn run_request_json(input: &str) -> PortCheckResult {
    match serde_json::from_str::<PortCheckRequest>(input) {
        Ok(req) => port_check(&req),
        Err(e) => not_run_result(format!("the request is not valid: {e}")),
    }
}

/// A NOT RUN result for a check that could not even start.
pub fn not_run_result(reason: String) -> PortCheckResult {
    PortCheckResult {
        tool: "port_check",
        status: Status::NotRun,
        sentence: format!("NOT RUN: {reason}"),
        reason: Some(reason),
        evidence: json!({}),
        first_difference: None,
        perf: json!({"status": "NOT RUN", "reason": "the check did not run"}),
    }
}

/// JSON text in, JSON text out.
pub fn run_json(input: &str) -> String {
    let result = run_request_json(input);
    serde_json::to_string_pretty(&result)
        .unwrap_or_else(|e| format!("{{\"tool\":\"port_check\",\"status\":\"NOT RUN\",\"reason\":\"cannot encode the result: {e}\"}}"))
}

/// Exit status for the CLI: 0 PASS, 1 FAIL, 2 NOT RUN, 3 NO CLAIM.
pub fn exit_code(status: Status) -> u8 {
    match status {
        Status::Pass => 0,
        Status::Fail => 1,
        Status::NotRun => 2,
        Status::NoClaim => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn the_tool_spec_is_pinned_by_its_snapshot() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/port_check_tool.json");
        let now = serde_json::to_string_pretty(&tool_spec()).expect("encode") + "\n";
        if std::env::var("BYTEASK_UPDATE_TOOL_SNAPSHOT").as_deref() == Ok("1") {
            std::fs::write(&path, &now).expect("write snapshot");
        }
        let pinned = std::fs::read_to_string(&path).expect(
            "src/snapshots/port_check_tool.json is missing; create it with BYTEASK_UPDATE_TOOL_SNAPSHOT=1 after review",
        );
        // Compared as JSON, not as text: key order depends on serde_json's preserve_order
        // feature, which a full workspace build turns on.
        let pinned: Value = serde_json::from_str(&pinned).expect("snapshot is JSON");
        assert_eq!(
            pinned,
            tool_spec(),
            "the port_check tool spec changed; review and regenerate on purpose"
        );
        assert!(!TOOL_DESCRIPTION.to_ascii_lowercase().contains("codex"));
    }

    #[test]
    fn a_malformed_request_is_a_not_run_result() {
        let r = run_request_json("{\"recording\": 5}");
        assert_eq!(r.status, Status::NotRun);
        assert!(
            r.sentence.starts_with("NOT RUN: the request is not valid"),
            "{}",
            r.sentence
        );
        let r = run_request_json(
            "{\"recording\":\"r\",\"python\":{\"argv\":[\"x\"]},\"cpp\":{\"argv\":[\"y\"]},\"tolerance\":1}",
        );
        assert!(
            r.sentence.contains("unknown field"),
            "typos are refused: {}",
            r.sentence
        );
        let text = run_json("not json");
        let v: Value = serde_json::from_str(&text).expect("always JSON");
        assert_eq!(v["status"], json!("NOT RUN"));
    }

    #[test]
    fn every_status_has_its_own_exit_code() {
        let codes: Vec<u8> = [Status::Pass, Status::Fail, Status::NotRun, Status::NoClaim]
            .into_iter()
            .map(exit_code)
            .collect();
        assert_eq!(codes, vec![0, 1, 2, 3]);
    }
}
