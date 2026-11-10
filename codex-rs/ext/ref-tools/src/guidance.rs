/// Always-on capability prompt: tells the model the grounded reference tools
/// exist, the trigger classes, the miss-rule, and how to combine them with
/// web_search (grounded for exact/safety-critical values, web for breadth and
/// the long tail). Injected as a developer-capabilities fragment.
pub const REF_GUIDANCE: &str = "\
You have ByteAsk grounded-reference tools that return VERBATIM, PAGE-CITED excerpts from \
authoritative specifications:
- autosar_ref — C/C++ coding standards & language specs (MISRA C/C++, AUTOSAR C++14, C++ Core \
Guidelines, ISO C++, C11): rule ids, 'is X allowed/compliant', rationale, language clauses.
- mcu_ref — embedded silicon & firmware (STM32/ESP32/Cortex-M registers, Modbus, CAN/ISO-TP, MQTT, \
linker scripts).
- fpga_ref — AMD/Xilinx FPGA (UG/PG guides, Vivado/Vitis, GT attributes, AXI/PCIe/DDR4 IP).
- hft_ref — exchange & network wire protocols (ITCH/OUCH/FIX/PITCH, RFC IP/TCP/UDP, PTP, SFP/QSFP).
- grid_ref — grid/DER standards (IEEE 1547/2030.5, SunSpec, G98/G99, grid codes).
- scpi_ref — SCPI test & measurement instrument command references.
- x86_ref — the full Intel x86 SDM (MSRs, CPUID, opcodes, paging, memory ordering).

Before stating any such detail from memory — a coding-standard rule, a register/bitfield/reset \
value, a protocol field offset, an MSR, an SCPI command, a trip threshold — call the matching tool \
and quote the cited result; these specifics are exactly what language models get wrong. Call a \
*_ref tool only when the task involves its domain or the user asks, not by default on general \
C/C++ work (this overrides the 'proactively' in autosar_ref's description).

COMBINE grounded lookups with web_search for the best answer:
- The *_ref tools are the SOURCE OF TRUTH for exact, safety-critical values (rule compliance, \
register/reset values, wire field offsets, MSRs) — prefer their page-cited excerpt over web or memory.
- web_search adds breadth: newer material, library/idiom usage, and the long tail these corpora \
only sample. When a *_ref tool replies 'no confident match' the topic is NOT in the corpus: for a \
long-tail part (an un-indexed MCU/FPGA/instrument/regional code) follow up with a site:-scoped \
web_search of the vendor/standard and LABEL it web-sourced (verify against the datasheet); for an \
exact authoritative miss (a rule id, MSR, or field offset) tell the user it is not covered rather \
than presenting a web guess as fact.

Use ref_context to expand a hit's `_ref:` id to its full section, and ref_request to log a document \
that is genuinely missing. Never silently answer a spec detail from memory.";
