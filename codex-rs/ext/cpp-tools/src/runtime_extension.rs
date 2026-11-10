//! The runtime tools: backwards questions over rr recordings
//! (`debug_reverse`, Linux only) and core-file triage (`debug_core`).
//!
//! The Intel PT probe (`cpp_spike_trace`, which only answers NOT RUN until capture
//! exists) and `port_check` (the port-check crate) are unfinished and not registered; their code
//! and pinned text stay (the snapshot test below builds them directly).
//!
//! They get exactly the layers the main tools get (`extension::wrap_thread_tools`:
//! the shared result line, the tool sandbox, the results store, the per-part off switches).
//!
//! They come from a contributor of their own, next to the main C/C++ one, so
//! the pinned model-facing text of the existing 20 tools
//! (`snapshots/model_facing_text.json`) is untouched and these
//! carry a snapshot of their own (`snapshots/runtime_tools.json`). The
//! containment gate is the main extension's: a gated thread gets none of them.

use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;

use crate::tools::CoreTriageTool;
use crate::tools::ReverseTool;

pub(crate) struct RuntimeTools;

impl ToolContributor for RuntimeTools {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some((cwd, native_exec)) = crate::extension::runtime_context(thread_store) else {
            return Vec::new();
        };
        let tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> = vec![
            Arc::new(ReverseTool {
                cwd: cwd.clone(),
                native_exec,
            }),
            // Read-only: gdb on a core file with a fixed driver, auto-load and
            // inferior calls off. Like cpp_symbolize, it needs no trust gate.
            Arc::new(CoreTriageTool { cwd }),
        ];
        crate::extension::wrap_thread_tools(tools, thread_store)
    }
}

#[cfg(test)]
mod tests {
    use codex_extension_api::ToolName;
    use pretty_assertions::assert_eq;

    use super::*;

    fn names(thread_store: &ExtensionData) -> Vec<ToolName> {
        RuntimeTools
            .tools(&ExtensionData::new("session"), thread_store)
            .iter()
            .map(|t| t.tool_name())
            .collect()
    }

    #[test]
    fn a_gated_thread_gets_none_of_the_runtime_tools() {
        // The recap / guardian threads: no tool that runs code natively.
        assert!(names(&crate::extension::test_thread_store(true, true)).is_empty());
        assert!(names(&ExtensionData::new("thread")).is_empty());
    }

    #[test]
    fn an_ordinary_thread_gets_every_runtime_tool() {
        let expected: Vec<ToolName> = if cfg!(target_os = "linux") {
            vec![
                ToolName::plain("debug_reverse"),
                ToolName::plain("debug_core"),
            ]
        } else {
            vec![ToolName::plain("debug_core")]
        };
        assert_eq!(
            names(&crate::extension::test_thread_store(false, false)),
            expected
        );
    }

    /// 0.2.0: the unfinished tools are not offered anywhere.
    #[test]
    fn port_check_and_spike_trace_are_not_registered() {
        let thread_store = crate::extension::test_thread_store(false, true);
        let session_store = ExtensionData::new("session");
        let mut all: Vec<String> = names(&thread_store)
            .iter()
            .map(|n| n.name.clone())
            .collect();
        all.extend(
            codex_extension_api::ToolContributor::tools(
                &crate::extension::test_extension(),
                &session_store,
                &thread_store,
            )
            .iter()
            .map(|t| t.tool_name().name),
        );
        assert!(
            !all.iter()
                .any(|n| n == "port_check" || n == "cpp_spike_trace"),
            "{all:?}"
        );
    }

    /// Integration: the runtime tools get the main tools' layers (`wrap_thread_tools`). A
    /// bare tool returns a bad-argument error with no result line; wrapped, every error ends
    /// with the shared NOT RUN line, as for every other ByteAsk tool.
    #[tokio::test]
    async fn the_runtime_tools_get_the_shared_layers() {
        let tools = RuntimeTools.tools(
            &ExtensionData::new("session"),
            &crate::extension::test_thread_store(false, true),
        );
        let core = tools
            .iter()
            .find(|t| t.tool_name() == ToolName::plain("debug_core"))
            .expect("debug_core");
        let err = crate::test_support::call(core.as_ref(), serde_json::json!({ "core": 7 }))
            .await
            .expect_err("a wrong argument type is refused");
        let text = crate::sandbox::error_text(err);
        let result =
            check_result::CheckResult::from_tool_text("debug_core", &text).expect("a result line");
        assert_eq!(result.verdict, check_result::Verdict::NotRun, "{text}");
    }

    /// The newer tools: their model-facing text is pinned the same way
    /// as the existing tools', one file per batch of tools, so adding a tool
    /// never rewrites a snapshot already pinned. Regenerate only after a
    /// maintainer-approved diff:
    /// `BYTEASK_UPDATE_TOOL_SNAPSHOT=1 <test-binary> runtime_extension::tests::runtime_tool_snapshots`
    const SNAPSHOTS: [(&str, &[&str]); 3] = [
        ("runtime_tools.json", &["debug_core", "debug_reverse"]),
        ("spike_trace.json", &["cpp_spike_trace"]),
        // Integration: port_check's text belongs to its own crate, pinned there too (port-check
        // src/snapshots/port_check_tool.json); this pins it as the engine sends it.
        ("port_check.json", &["port_check"]),
    ];

    #[test]
    fn runtime_tool_snapshots_are_byte_identical() {
        // The registered runtime tools, plus the two kept but unregistered ones (built
        // directly, so their pinned text stays protected), plus debug_reverse wherever it is
        // not registered (Linux only), so the files are the same on every platform.
        let mut tools = RuntimeTools.tools(
            &ExtensionData::new("session"),
            &crate::extension::test_thread_store(false, false),
        );
        tools.push(Arc::new(crate::tools::SpikeTraceTool {
            sysfs_root: std::path::PathBuf::from("/sys"),
        }));
        tools.push(Arc::new(crate::tools::PortCheckTool {
            cwd: std::path::PathBuf::from("/workspace"),
            native_exec: false,
        }));
        if !cfg!(target_os = "linux") {
            tools.push(Arc::new(ReverseTool {
                cwd: std::path::PathBuf::from("/workspace"),
                native_exec: false,
            }));
        }
        let specs: Vec<(String, serde_json::Value)> = tools
            .iter()
            .map(|tool| {
                (
                    tool.tool_name().to_string(),
                    crate::test_support::sorted_keys(
                        serde_json::to_value(tool.spec()).expect("spec serialises"),
                    ),
                )
            })
            .collect();
        // Every tool is pinned by exactly one snapshot.
        for (name, _) in &specs {
            let pinned_by = SNAPSHOTS
                .iter()
                .filter(|(_, names)| names.contains(&name.as_str()))
                .count();
            assert_eq!(pinned_by, 1, "{name} must be in exactly one snapshot file");
        }
        for (file, names) in SNAPSHOTS {
            let path = format!("{}/src/snapshots/{file}", env!("CARGO_MANIFEST_DIR"));
            let mut tools: Vec<serde_json::Value> = specs
                .iter()
                .filter(|(n, _)| names.contains(&n.as_str()))
                .map(|(_, v)| v.clone())
                .collect();
            tools.sort_by_key(std::string::ToString::to_string);
            let now = format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({ "tools": tools })).expect("json")
            );
            if std::env::var_os("BYTEASK_UPDATE_TOOL_SNAPSHOT").is_some() {
                std::fs::write(&path, &now).expect("write snapshot");
                continue;
            }
            let pinned = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                panic!("snapshot {file} missing: run once with BYTEASK_UPDATE_TOOL_SNAPSHOT=1")
            });
            assert_eq!(
                pinned, now,
                "model-facing text in {file} changed; see the doc comment"
            );
        }
    }
}
