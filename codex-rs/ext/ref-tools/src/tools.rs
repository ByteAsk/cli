//! The reference tools: seven tribe-scoped `*_ref` grounded-search tools plus
//! two plumbing tools (`ref_context`, `ref_request`). All share one backend —
//! the ByteAsk Embedded MCP corpus — and differ only in name + description so
//! the model routes by domain vocabulary and the banner/analytics stay per-tribe.
//!
//! Descriptions encode the grounded-vs-hybrid policy:
//! * Long-tail domains (mcu/fpga/scpi/grid) index a *sample* of a vast space, so
//!   their copy tells the model to fall through to `web_search` for parts/topics
//!   not covered — a grounded+web hybrid.
//! * Authoritative domains (autosar/x86/hft) are the complete source of truth for
//!   exact, safety-critical values (a rule id, an MSR, a wire field offset), so a
//!   miss means "not covered" — never web-guess those.

use std::collections::BTreeMap;
use std::sync::Arc;

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

use crate::mcp;
use crate::report::report;
use crate::report::short_target;

const DEFAULT_LIMIT: i64 = 8;
const MAX_LIMIT: i64 = 10;

/// Every reference tool name, for the lifecycle logging backstop.
pub(crate) const REF_TOOL_NAMES: [&str; 9] = [
    "autosar_ref",
    "mcu_ref",
    "fpga_ref",
    "hft_ref",
    "grid_ref",
    "scpi_ref",
    "x86_ref",
    "ref_context",
    "ref_request",
];

pub(crate) fn is_ref_tool(name: &str) -> bool {
    REF_TOOL_NAMES.contains(&name)
}

// ---- descriptions (the model-facing product copy) ------------------------- //

const AUTOSAR_DESC: &str = "\
Search the full text of the C/C++ safety & coding standards and return VERBATIM, page-cited \
excerpts: AUTOSAR C++14 (all 397 rules), MISRA C++:2008 (all 228 rules), MISRA C:2012, the C++ \
Core Guidelines, ISO C++ (N4950) and C11 (N1570). Call this BEFORE stating any coding-standard or \
language-standard fact from memory (they are frequently misremembered), and PROACTIVELY while \
writing or reviewing C/C++: for any rule id (A5-2-2, M0-1-1, 0-1-1, Dir 4.6, ES.20 — type them \
plain, en-dashes optional), 'is X allowed / compliant', a rule's rationale/exceptions/examples, or \
a language clause (UB, lifetime, conversions, templates). Follow the rule it returns and cite the \
id. Core Guidelines cite by §section, not page. This corpus IS the authoritative rulebook: a 'no \
confident match' means the rule/topic is genuinely not covered (e.g. SEI CERT C++ is not indexed) \
— tell the user and do NOT answer a rule-compliance question from memory or a web guess. For modern \
library/idiom usage (not a rule), use web_search site:cppreference.com instead.";

const MCU_DESC: &str = "\
Search embedded silicon & firmware references and return VERBATIM, page-cited excerpts: STM32F4 \
Reference Manual (RM0090 — every peripheral register), Arm Cortex-M4 TRM + Generic User Guide, \
ESP32 TRM + ESP-IDF API, Modbus (application / serial-RTU / TCP), CAN (ISO 11898-1) + ISO-TP \
(ISO 15765-2), MQTT 3.1.1, GNU ld linker scripts, python-can / PyModbus / IntelHex / PCAN-USB FD, \
MCP2221A, LTC6812, AWS IoT Core. Call it BEFORE quoting any register offset/bitfield/reset value, \
NVIC/SysTick/MPU/CONTROL detail, Modbus function or exception code, or CAN/ISO-TP framing field. \
This corpus indexes a curated SET of parts, not every MCU: if the exact part (e.g. an STM32H7, \
nRF52, RP2040) returns 'no confident match', that part is not indexed — then use web_search \
site:st.com / site:espressif.com / the vendor's site for the datasheet, and label it as \
web-sourced (verify against the datasheet). Never invent a register or reset value.";

const FPGA_DESC: &str = "\
Search 33 AMD/Xilinx + interconnect references and return VERBATIM, page-cited excerpts: UltraScale \
GTH/GTY/GTM (UG576/UG578/UG581), 7-series GTX (UG476), SelectIO/clocking (UG571/UG471/UG572), \
Vivado constraints/synth/impl/methodology (UG903/UG901/UG904/UG949), Vitis HLS (UG1399), Versal \
(AM011), device datasheets (DS922/DS923/DS182); LogiCORE IP — 10G/25G+100G Ethernet (PG210/PG203), \
PCIe (PG213/PG156), XDMA/QDMA/AXI-DMA (PG195/PG302/PG021), DDR4 (PG150), PCS/PMA, AXI Interconnect/\
Stream; plus AMBA AXI/ACE (IHI0022H), AXI4-Stream (IHI0051B), PCIe Base 4.0. Call it for GT \
attributes/DRP maps/defaults, XDC syntax, HLS pragmas, IP register maps, AXI signals & response \
codes (TKEEP, BRESP/DECERR), or TLP formats — never guess an attribute default. Coverage is \
AMD/Xilinx-centric: for Intel/Altera/Lattice parts or an un-indexed UG/PG a 'no confident match' is \
expected — then use web_search site:docs.amd.com / the vendor's site to cover the long tail.";

const HFT_DESC: &str = "\
Search trading-system & low-latency networking specs and return VERBATIM, page-cited excerpts: \
Nasdaq TotalView-ITCH 5.0, OUCH 4.2/5.0, NYSE Pillar (integrated feed + binary gateway), Cboe PITCH \
(multicast + TCP) + BOE/BOE3, FIX 4.4 + FIX SBE; RFC 791/9293/768/826/894/1071 (IPv4/TCP/UDP/ARP/\
framing/checksum), IEEE 1588-2019 PTP; SFF-8472/8431/8636 (SFP+/QSFP diagnostics), Cisco K35-Q/V9P \
SmartNICs. Call it for exact message/field offsets & lengths, type codes, endianness, price scaling, \
PTP message layouts & clockClass, or DDM byte maps. These are the authoritative wire specs and a \
WRONG offset is dangerous, so a 'no confident match' means it is genuinely not indexed (e.g. CME \
MDP 3.0 — its encoding is FIX SBE, which IS indexed; IEEE 802.3 base is not) — tell the user; do NOT \
present a web-guessed field offset as authoritative.";

const GRID_DESC: &str = "\
Search grid-interconnection & DER (solar/storage) standards and return VERBATIM, page-cited \
excerpts: IEEE 1547.1-2020 conformance tests, IEEE 2030.5 (SEP2), SunSpec Modbus 1547 profile + \
information model, ENA G98 + G99 (UK), AS/NZS 4777.2 (AU), BS EN 50549-1 (EU), IEC 62116 \
anti-islanding, CSIP-AUS, Austrian TOR, Spanish RD 244/2019. Call it for trip/ride-through \
thresholds & timings, volt-var / volt-watt curves, LFSM/FSM settings, SunSpec model registers \
(e.g. DERVoltVar 705), or conformance procedures — never quote a protection setting from memory. \
Coverage is a set of major standards + regional codes: for a jurisdiction or standard not indexed \
(a 'no confident match'; note VDE-AR-N 4105 is a text-less scan) use web_search for the regional \
grid code, labeled as web-sourced.";

const SCPI_DESC: &str = "\
Search test & measurement instrument command references and return VERBATIM, page-cited excerpts: \
the SCPI-1999 standard, VISA (VPP-4.3); Yokogawa WT5000/WT3000/WT1800/WT500 power analyzers + DL750 \
ScopeCorder; Tektronix MDO/MSO/DPO4000 scopes; Hioki PW3335; Chroma 61800/618xx grid simulators; \
Kikusui PCR-LE; Pacific Power AMX; AMETEK Asterion; NH Research 9410; Cinergia GE+. Call it for the \
exact SCPI command path/syntax for a specific instrument, *common commands (*IDN?, *ESR?), the \
status model, or remote-control setup. This indexes a SET of instruments: for a model not covered \
(Keysight, Rigol, R&S, etc. — a 'no confident match') use web_search site:<vendor> for that \
instrument's programming manual, labeled as web-sourced. Never invent a command path.";

const X86_DESC: &str = "\
Search the complete Intel 64 / IA-32 Software Developer's Manual, Vols 1-4 (5,342 pages, doc \
325462) and return VERBATIM, page-cited excerpts. Call it for MSR names/addresses (e.g. \
IA32_TSC_DEADLINE = 6E0H), CPUID leaves & feature bits, CR0-CR4 bits, instruction \
semantics/opcodes/flag effects, paging structures, the memory-ordering model, or APIC/TSC/VMX \
detail — before quoting any from memory. This is the authoritative x86 reference: a 'no confident \
match' means it is not in the SDM — tell the user; do NOT quote a web-guessed MSR address or opcode \
as fact (for a quick opcode cross-check, web_search site:felixcloutier.com and label it).";

const REF_CONTEXT_DESC: &str = "\
Expand a reference search hit to its full section. Pass the `_ref:` id returned by any *_ref search \
tool (autosar_ref, mcu_ref, fpga_ref, hft_ref, grid_ref, scpi_ref, x86_ref). Use when a returned \
snippet is truncated or you need the surrounding clauses / adjacent fields / the whole table.";

const REF_REQUEST_DESC: &str = "\
Record a request for a document ByteAsk has NOT indexed. Use ONLY after a *_ref search returned \
'no confident match' AND the user genuinely needs that spec indexed. Pass a short free-text \
description (title / standard / version). This does NOT answer anything — it logs the gap so the \
document can be added later.";

// ---- search tools --------------------------------------------------------- //

/// A misnamed field is refused, not silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
}

/// A tribe-scoped grounded search tool. All instances call the same MCP
/// `search_docs`; `name`/`description` differ so the model routes by domain.
pub struct RefSearchTool {
    name: &'static str,
    description: &'static str,
}

impl<'call> ToolExecutor<ToolCall<'call>> for RefSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.name)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "query".to_string(),
            JsonSchema::string(Some(
                "What to look up, in the engineer's own words or exact tokens (a rule id, register \
                 name, field, command, MSR, threshold, doc id). Natural keywords work best; you do \
                 not need to phrase it as a question."
                    .to_string(),
            )),
        );
        properties.insert(
            "limit".to_string(),
            JsonSchema::integer(Some(format!(
                "Max snippets to return, 1-{MAX_LIMIT} (default {DEFAULT_LIMIT}). Use a small limit \
                 for a precise lookup, larger to survey a topic."
            ))),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: self.name.to_string(),
            description: self.description.to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["query".to_string()]),
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

impl RefSearchTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: SearchArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "query must not be empty".to_string(),
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let text = mcp::call_tool(
            "search_docs",
            serde_json::json!({ "query": query, "limit": limit }),
        )
        .await?;
        let result = crate::results::search_result(self.name, &text);
        Ok(report(self.name, &short_target(query), text, result))
    }
}

// ---- ref_context ---------------------------------------------------------- //

/// A misnamed field is refused, not silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextArgs {
    ref_id: String,
}

pub struct RefContextTool;

impl<'call> ToolExecutor<ToolCall<'call>> for RefContextTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("ref_context")
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "ref_id".to_string(),
            JsonSchema::string(Some(
                "The `_ref:` id from a prior *_ref search hit whose full section you want."
                    .to_string(),
            )),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: "ref_context".to_string(),
            description: REF_CONTEXT_DESC.to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["ref_id".to_string()]),
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
        Box::pin(async move {
            let arguments = call.function_arguments()?;
            let args: ContextArgs = serde_json::from_str(arguments)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            let ref_id = args.ref_id.trim();
            if ref_id.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "ref_id must not be empty".to_string(),
                ));
            }
            let text =
                mcp::call_tool("get_context", serde_json::json!({ "result_id": ref_id })).await?;
            let result = crate::results::context_result(&text);
            Ok(report("ref_context", &short_target(ref_id), text, result))
        })
    }
}

// ---- ref_request ---------------------------------------------------------- //

/// A misnamed field is refused, not silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestArgs {
    request: String,
}

pub struct RefRequestTool;

impl<'call> ToolExecutor<ToolCall<'call>> for RefRequestTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("ref_request")
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "request".to_string(),
            JsonSchema::string(Some(
                "Short free-text description of the missing document (title / standard / version)."
                    .to_string(),
            )),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: "ref_request".to_string(),
            description: REF_REQUEST_DESC.to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["request".to_string()]),
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
        Box::pin(async move {
            let arguments = call.function_arguments()?;
            let args: RequestArgs = serde_json::from_str(arguments)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            let request = args.request.trim();
            if request.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "request must not be empty".to_string(),
                ));
            }
            let text =
                mcp::call_tool("request_document", serde_json::json!({ "request": request })).await?;
            let result = check_result::CheckResult::pass("ref_request", "The document request was recorded.");
            Ok(report("ref_request", "", text, result))
        })
    }
}

/// Build all nine reference tool executors (7 tribe search tools + 2 plumbing).
pub(crate) fn ref_tools() -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
    vec![
        Arc::new(RefSearchTool {
            name: "autosar_ref",
            description: AUTOSAR_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "mcu_ref",
            description: MCU_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "fpga_ref",
            description: FPGA_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "hft_ref",
            description: HFT_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "grid_ref",
            description: GRID_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "scpi_ref",
            description: SCPI_DESC,
        }),
        Arc::new(RefSearchTool {
            name: "x86_ref",
            description: X86_DESC,
        }),
        Arc::new(RefContextTool),
        Arc::new(RefRequestTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_misnamed_argument_is_refused_not_silently_dropped() {
        // A real call carried an unknown field, and it was silently dropped.
        let err = serde_json::from_str::<SearchArgs>(r#"{"query":"RCC","max_results":3}"#)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("unknown field `max_results`"), "{err}");
        assert!(serde_json::from_str::<SearchArgs>(r#"{"query":"RCC","limit":3}"#).is_ok());
        assert!(serde_json::from_str::<ContextArgs>(r#"{"ref_id":"x","query":"y"}"#).is_err());
        assert!(serde_json::from_str::<RequestArgs>(r#"{"request":"x","title":"y"}"#).is_err());
    }

    #[test]
    fn builds_nine_tools_with_expected_names() {
        let names: Vec<String> = ref_tools()
            .iter()
            .map(|tool| tool.tool_name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "autosar_ref",
                "mcu_ref",
                "fpga_ref",
                "hft_ref",
                "grid_ref",
                "scpi_ref",
                "x86_ref",
                "ref_context",
                "ref_request",
            ]
        );
    }

    #[test]
    fn is_ref_tool_matches_our_tools_only() {
        assert!(is_ref_tool("autosar_ref"));
        assert!(is_ref_tool("ref_context"));
        assert!(!is_ref_tool("cpp_check"));
        assert!(!is_ref_tool("web_search"));
    }

    #[test]
    fn search_args_parse_and_default_limit() {
        let a: SearchArgs = serde_json::from_str(r#"{"query":"MISRA casts"}"#).unwrap();
        assert_eq!(a.query, "MISRA casts");
        assert_eq!(a.limit, None);
        let b: SearchArgs = serde_json::from_str(r#"{"query":"x","limit":3}"#).unwrap();
        assert_eq!(b.limit, Some(3));
    }

    #[test]
    fn every_search_description_states_the_miss_rule() {
        // The "no confident match" contract must appear in each search tool so the
        // model never silently falls back to memory.
        for desc in [
            AUTOSAR_DESC, MCU_DESC, FPGA_DESC, HFT_DESC, GRID_DESC, SCPI_DESC, X86_DESC,
        ] {
            assert!(desc.contains("no confident match"), "missing miss-rule: {desc:.40}");
            assert!(desc.contains("VERBATIM, page-cited"), "missing grounding pitch");
        }
    }

    #[test]
    fn longtail_tools_point_to_web_search_authoritative_do_not() {
        // Hybrid policy: long-tail domains tell the model to fall through to
        // web_search; authoritative domains warn against web-guessing exact values.
        for desc in [MCU_DESC, FPGA_DESC, SCPI_DESC, GRID_DESC] {
            assert!(desc.contains("web_search"), "long-tail tool should mention web_search");
        }
        for desc in [HFT_DESC, X86_DESC] {
            assert!(desc.contains("do NOT"), "authoritative tool should warn against guessing");
        }
    }
}
