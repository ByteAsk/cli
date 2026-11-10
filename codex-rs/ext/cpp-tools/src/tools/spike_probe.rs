//! `cpp_spike_trace`: the Intel PT latency-spike capture.
//!
//! Only its probe exists (Intel PT needs a bare-metal Intel host, and capture
//! is not implemented yet). Every call answers NOT RUN in the shared result
//! format, with the reason, so nothing here can ever read as a result.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use check_result::CheckResult;
use check_result::NotRunReason;
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

const TOOL_NAME: &str = "cpp_spike_trace";
/// Where the kernel lists an Intel PT PMU, relative to the sysfs root.
const INTEL_PT_PMU: &str = "bus/event_source/devices/intel_pt";

pub struct SpikeTraceTool {
    /// `/sys` in production; tests point it at a fake tree.
    pub sysfs_root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpikeArgs {
    program: String,
    #[serde(default)]
    #[allow(dead_code)]
    args: Option<String>,
}

fn has_intel_pt(sysfs_root: &Path) -> bool {
    sysfs_root.join(INTEL_PT_PMU).exists()
}

/// The answer, from the probe alone. Both branches are NOT RUN.
fn answer(intel_pt: bool) -> CheckResult {
    if intel_pt {
        CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::Unsupported,
            "this machine has Intel PT, but latency-spike capture is not built yet.",
            "none on this host yet: capture ships in a later release; for a sampled profile use cpp_perf.",
        )
    } else {
        CheckResult::not_run(
            TOOL_NAME,
            NotRunReason::Unsupported,
            "this machine has no Intel PT; it needs a bare-metal Intel host.",
            "run it on a bare-metal Intel Linux host (Broadwell or newer) where \
             /sys/bus/event_source/devices/intel_pt exists; cloud VMs do not expose Intel PT.",
        )
    }
}

fn probe_text(sysfs_root: &Path, intel_pt: bool) -> String {
    format!(
        "Intel PT probe: {} {}.",
        sysfs_root.join(INTEL_PT_PMU).display(),
        if intel_pt { "exists" } else { "does not exist" }
    )
}

impl<'call> ToolExecutor<ToolCall<'call>> for SpikeTraceTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "program".to_string(),
            JsonSchema::string(Some(
                "The executable whose latency spikes to capture.".to_string(),
            )),
        );
        properties.insert(
            "args".to_string(),
            JsonSchema::string(Some("Optional program arguments.".to_string())),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Intel PT latency-spike capture: NOT BUILT YET. It only checks whether this \
                machine has Intel PT and always answers NOT RUN with the reason. For where time goes, \
                use cpp_perf."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["program".to_string()]),
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

impl SpikeTraceTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: SpikeArgs = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let intel_pt = has_intel_pt(&self.sysfs_root);
        // The shared result line is appended once, by report().
        Ok(crate::report::report(
            TOOL_NAME,
            &args.program,
            probe_text(&self.sysfs_root, intel_pt),
            answer(intel_pt),
        ))
    }
}

#[cfg(test)]
mod tests {
    use check_result::Verdict;
    use pretty_assertions::assert_eq;

    use super::*;

    fn fake_sysfs(with_intel_pt: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        // What a typical cloud VM lists: a core PMU and the software ones, no intel_pt.
        for pmu in ["cpu", "software", "tracepoint", "msr", "uprobe"] {
            std::fs::create_dir_all(root.path().join("bus/event_source/devices").join(pmu))
                .unwrap();
        }
        if with_intel_pt {
            std::fs::create_dir_all(root.path().join(INTEL_PT_PMU).join("format")).unwrap();
        }
        root
    }

    async fn call(root: &Path) -> String {
        let tool = SpikeTraceTool {
            sysfs_root: root.to_path_buf(),
        };
        crate::test_support::call(
            &tool,
            serde_json::json!({"program": "./feed", "args": "--hot"}),
        )
        .await
        .expect("answers")
    }

    fn result_of(text: &str) -> CheckResult {
        CheckResult::from_tool_text(TOOL_NAME, text)
            .unwrap_or_else(|| panic!("no shared result line:\n{text}"))
    }

    #[tokio::test]
    async fn without_intel_pt_it_is_not_run_with_the_reason_and_a_fix() {
        let root = fake_sysfs(false);
        let text = call(root.path()).await;
        let result = result_of(&text);
        assert_eq!(result.verdict, Verdict::NotRun);
        assert!(!result.is_pass());
        assert_eq!(result.reason, Some(NotRunReason::Unsupported));
        assert_eq!(
            result.summary,
            "this machine has no Intel PT; it needs a bare-metal Intel host."
        );
        assert!(
            result
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("bare-metal Intel")),
            "{text}"
        );
        assert!(result.validate().is_ok());
        assert!(text.contains("intel_pt does not exist"), "{text}");
    }

    #[tokio::test]
    async fn with_intel_pt_it_is_still_not_run_because_capture_is_not_built() {
        let root = fake_sysfs(true);
        let text = call(root.path()).await;
        let result = result_of(&text);
        assert_eq!(result.verdict, Verdict::NotRun, "{text}");
        assert!(!result.is_pass());
        assert!(
            result.summary.contains("capture is not built yet"),
            "{text}"
        );
        assert!(
            result.fix.as_deref().is_some_and(|f| !f.trim().is_empty()),
            "{text}"
        );
        assert!(result.validate().is_ok());
        assert!(text.contains("intel_pt exists"), "{text}");
    }

    #[test]
    fn the_probe_looks_for_the_intel_pt_pmu_only() {
        assert!(!has_intel_pt(fake_sysfs(false).path()));
        assert!(has_intel_pt(fake_sysfs(true).path()));
        assert!(!has_intel_pt(Path::new("/nonexistent-sysfs-root")));
    }

    #[test]
    fn a_misnamed_argument_is_refused() {
        assert!(serde_json::from_str::<SpikeArgs>(r#"{"program":"a","threshold_us":5}"#).is_err());
    }
}
