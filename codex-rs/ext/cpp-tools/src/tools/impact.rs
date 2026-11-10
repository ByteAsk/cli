use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

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

use crate::gitx;
use crate::impact;

const TOOL_NAME: &str = "cpp_impact";
const DEFAULT_BUDGET_SECS: u64 = 60;
const MAX_BUDGET_SECS: u64 = 300;

pub struct ImpactTool {
    pub cwd: PathBuf,
    /// The thread's forced workflow, when there is one: its checklist numbering is shared.
    pub workflow: Option<std::sync::Arc<crate::workflow::Workflow>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImpactArgs {
    #[serde(default)]
    symbols: Option<Vec<String>>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    max_items: Option<usize>,
    #[serde(default)]
    budget_sec: Option<u64>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for ImpactTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "symbols".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Analyze these symbols instead of the working-tree diff (before editing): \
                     `ns::Type::method`, a plain name, or `path:line`."
                        .to_string(),
                ),
            ),
        );
        properties.insert(
            "base".to_string(),
            JsonSchema::string(Some(
                "Revision to diff the working tree against (default HEAD).".to_string(),
            )),
        );
        properties.insert(
            "max_items".to_string(),
            JsonSchema::integer(Some("Maximum checklist items (default 30).".to_string())),
        );
        properties.insert(
            "budget_sec".to_string(),
            JsonSchema::integer(Some(
                "Time budget in seconds (default 60, max 300).".to_string(),
            )),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Change-impact checklist for a C/C++ change: for the functions and types \
                your diff touches (or the named symbols), lists overloads and look-alike \
                functions, other labels of the same switch, related types that lack an operation \
                you added, template specialisations, virtual overrides, declarations in other \
                #if configurations, stale generated copies (combined single headers), callers, and the tests \
                and docs that mention them. Textual scan confirmed by clangd when a compile \
                database exists. Answer each item `[n] edited` or `[n] n/a: <reason>`; listing \
                is not an instruction to edit."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                properties,
                None,
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

impl ImpactTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let args: ImpactArgs = if arguments.trim().is_empty() {
            ImpactArgs {
                symbols: None,
                base: None,
                max_items: None,
                budget_sec: None,
            }
        } else {
            serde_json::from_str(arguments)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?
        };
        let Some(root) = gitx::primary_root(&self.cwd).await else {
            return Err(FunctionCallError::RespondToModel(
                "cpp_impact needs a git repository (it diffs the working tree against a revision); \
                 none was found at the workspace. Nothing was analyzed."
                    .to_string(),
            ));
        };
        let base = args.base.unwrap_or_else(|| "HEAD".to_string());
        if base.starts_with('-') {
            return Err(FunctionCallError::RespondToModel(format!(
                "invalid revision: {base}"
            )));
        }
        let base_rev = gitx::git(
            &root,
            &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")],
        )
        .await
        .map(|out| out.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "revision '{base}' does not exist in this repository. Nothing was analyzed."
            ))
        })?;
        let source = match args.symbols.filter(|s| !s.is_empty()) {
            Some(symbols) => impact::Source::Symbols(symbols),
            None => impact::Source::Diff,
        };
        let req = impact::ImpactRequest {
            root,
            base_rev,
            source,
            budget: Duration::from_secs(
                args.budget_sec
                    .unwrap_or(DEFAULT_BUDGET_SECS)
                    .clamp(5, MAX_BUDGET_SECS),
            ),
            use_clangd: true,
            max_items: args
                .max_items
                .unwrap_or(impact::DEFAULT_MAX_ITEMS)
                .clamp(1, 60),
            preexisting_untracked: Vec::new(),
        };
        let report = impact::analyze(&req).await;
        // The thread's checklist numbering, so an answer `[n]` to this list counts at finish.
        let text = match &self.workflow {
            Some(workflow) => workflow.render_shared(&report, TOOL_NAME).await,
            None => {
                let mut numbering = impact::Numbering::default();
                impact::render(&report, &mut numbering, TOOL_NAME, None)
            }
        };
        // A checklist to answer, not a judgement of the code.
        let result = if report.changed.is_empty() {
            check_result::CheckResult::no_claim(
                TOOL_NAME,
                "No changed C/C++ entity was found, so there is nothing to list.",
            )
        } else {
            check_result::CheckResult::no_claim(
                TOOL_NAME,
                &format!(
                    "{} change-impact item(s) to answer; this lists them and judges nothing.",
                    report.items.len()
                ),
            )
        };
        Ok(crate::report::report(
            TOOL_NAME,
            &format!("{} item(s)", report.items.len()),
            text,
            result,
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn runs_on_a_fixture_repository() {
        let Some(dir) = crate::impact::tests::fixture() else {
            eprintln!("skipped: git not available");
            return;
        };
        let tool = ImpactTool {
            cwd: dir.path().to_path_buf(),
            workflow: None,
        };
        let out = crate::test_support::call(&tool, json!({})).await.unwrap();
        assert!(out.contains("cpp_impact"), "{out}");
        assert!(out.contains("to_local"), "{out}");
        assert!(out.contains("[1]"), "{out}");
    }

    #[tokio::test]
    async fn refuses_outside_a_repository_and_bad_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ImpactTool {
            cwd: dir.path().to_path_buf(),
            workflow: None,
        };
        let err = crate::test_support::call(&tool, json!({}))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("git repository"), "{err:?}");
        let Some(repo) = crate::impact::tests::fixture() else {
            return;
        };
        let tool = ImpactTool {
            cwd: repo.path().to_path_buf(),
            workflow: None,
        };
        let err = crate::test_support::call(&tool, json!({"base": "no-such-rev"}))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("does not exist"), "{err:?}");
    }
}
