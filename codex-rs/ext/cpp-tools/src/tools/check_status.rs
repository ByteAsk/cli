//! `check_status`: the latest automatic check result for this workspace, and whether it is
//! still current. It runs nothing: it reads the local results store the
//! finish gate and its background follow-up write.

use std::path::PathBuf;
use std::time::Duration;

use check_result::CheckResult;
use check_result::Verdict;
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

use crate::results_store;
use crate::results_store::Freshness;

const TOOL_NAME: &str = "check_status";
/// The freshness fingerprint gets this long.
const FRESHNESS_BUDGET: Duration = Duration::from_secs(2);
/// Earlier results listed after the newest.
const HISTORY: usize = 4;

pub struct CheckStatusTool {
    pub cwd: PathBuf,
    /// The results store's home (`$BYTEASK_HOME`); `None` = the default.
    pub home: Option<PathBuf>,
    /// When this session started (unix seconds). Results recorded before it come from an
    /// earlier session: they are listed as such and never reported as this session's verdict.
    pub session_started: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Args {}

impl<'call> ToolExecutor<ToolCall<'call>> for CheckStatusTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "The latest automatic ByteAsk check result for this workspace (the \
                finish gate's verdict, and a background follow-up's when one finished) and \
                whether it is still current for the code as it is now; a result taken before \
                the latest edit is marked STALE. Use it before telling the user a change works, \
                or after a background check finished. It runs nothing itself."
                .to_string(),
            strict: false,
            parameters: JsonSchema::object(
                Default::default(),
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

impl CheckStatusTool {
    async fn handle_call(&self, call: ToolCall<'_>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let arguments = call.function_arguments()?;
        let _: Args = serde_json::from_str(arguments)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let (body, result) = self.status().await;
        Ok(crate::report::report_plain(TOOL_NAME, body, result))
    }

    pub(crate) async fn status(&self) -> (String, CheckResult) {
        let Some(home) = self.home.clone().or_else(results_store::default_home) else {
            return (
                "No ByteAsk home directory, so there is no results store.".to_string(),
                CheckResult::no_claim(TOOL_NAME, "There is no results store to read."),
            );
        };
        let root = crate::gitx::toplevel(&self.cwd)
            .await
            .unwrap_or_else(|| self.cwd.clone());
        let entries = results_store::all(&home, &root);
        let Some(newest) = entries.first() else {
            return (
                format!("No automatic check has run for {} yet.", root.display()),
                CheckResult::no_claim(TOOL_NAME, "No automatic check has run for this workspace yet."),
            );
        };
        let now = results_store::now_unix();
        if let Some(started) = self.session_started
            && newest.at < started
        {
            // Nothing ran in this session: an earlier session's verdict is history, not a
            // current PASS or FAIL.
            let mut body = format!(
                "No check has run in this session yet.\nfrom earlier sessions (older; not this session's result):\n- {}",
                results_store::describe(newest, Freshness::Stale, now)
            );
            for entry in entries.iter().skip(1).take(HISTORY) {
                body.push_str(&format!("\n- {}", results_store::describe(entry, Freshness::Stale, now)));
            }
            return (
                body,
                CheckResult::no_claim(
                    TOOL_NAME,
                    "No check has run in this session; the results shown are from earlier sessions.",
                ),
            );
        }
        let freshness = results_store::freshness(newest, FRESHNESS_BUDGET).await;
        let mut body = format!("latest: {}", results_store::describe(newest, freshness, now));
        let earlier: Vec<String> = entries
            .iter()
            .skip(1)
            .take(HISTORY)
            .map(|entry| format!("- {}", results_store::describe(entry, Freshness::Stale, now)))
            .collect();
        if !earlier.is_empty() {
            body.push_str("\nearlier (older versions of the code):\n");
            body.push_str(&earlier.join("\n"));
        }
        (body, own_result(&newest.result, freshness))
    }
}

/// This call's result: the stored verdict while it is current for this exact code, otherwise
/// no claim at all (a stale PASS says nothing about the code now).
fn own_result(stored: &CheckResult, freshness: Freshness) -> CheckResult {
    if freshness != Freshness::Current {
        return CheckResult::no_claim(
            TOOL_NAME,
            if freshness == Freshness::Stale {
                "The latest result is for an older version of the code; nothing is known about the code as it is now."
            } else {
                "Whether the latest result still applies could not be checked in time."
            },
        );
    }
    let summary = format!("The latest automatic check, for the code as it is now: {}", stored.summary);
    match stored.verdict {
        Verdict::Pass => CheckResult::pass(TOOL_NAME, &summary),
        Verdict::Fail => CheckResult::fail(TOOL_NAME, &summary),
        Verdict::NoClaim => CheckResult::no_claim(TOOL_NAME, &summary),
        Verdict::Approximate => CheckResult::approximate(TOOL_NAME, &summary),
        Verdict::NotRun => CheckResult::not_run(
            TOOL_NAME,
            stored.reason.unwrap_or(check_result::NotRunReason::Skipped),
            &summary,
            stored.fix.as_deref().unwrap_or(""),
        ),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::impact::tests::git;

    fn repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        std::fs::write(dir.path().join("a.cpp"), "int a() { return 1; }\n").ok()?;
        (git(dir.path(), &["init", "-q"])
            && git(dir.path(), &["add", "-A"])
            && git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "b"]))
        .then_some(dir)
    }

    #[tokio::test]
    async fn a_pass_is_reported_only_while_it_is_current() {
        let Some(dir) = repo() else {
            eprintln!("skipped: git not available");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let tool = CheckStatusTool {
            cwd: root.clone(),
            home: Some(home.path().to_path_buf()),
            session_started: None,
        };
        let (text, result) = tool.status().await;
        assert_eq!(result.verdict, Verdict::NoClaim, "{text}");

        let fresh = results_store::fingerprint(&root, Duration::from_secs(10)).await.unwrap();
        results_store::record(
            home.path(),
            results_store::Entry {
                result: CheckResult::pass(crate::gate::CHECK, "every target built").with_content_key("k"),
                root: root.clone(),
                at: results_store::now_unix(),
                fresh,
                tier: "full".to_string(),
                source: "engine".to_string(),
            },
        );
        let (text, result) = tool.status().await;
        assert_eq!(result.verdict, Verdict::Pass, "{text}");
        assert!(text.contains("current"), "{text}");

        std::fs::write(root.join("a.cpp"), "int a() { return 2; }\n").unwrap();
        let (text, result) = tool.status().await;
        assert_eq!(result.verdict, Verdict::NoClaim, "a stale PASS must not read as PASS: {text}");
        assert!(text.contains("STALE"), "{text}");
    }

    /// A FAIL recorded by an earlier session is never shown as this session's FAIL.
    #[tokio::test]
    async fn results_from_earlier_sessions_are_marked_older_not_current() {
        let Some(dir) = repo() else {
            eprintln!("skipped: git not available");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let fresh = results_store::fingerprint(&root, Duration::from_secs(10)).await.unwrap();
        let at = results_store::now_unix();
        results_store::record(
            home.path(),
            results_store::Entry {
                result: CheckResult::fail(crate::gate::CHECK, "1 problem(s) open").with_content_key("k"),
                root: root.clone(),
                at: at - 100,
                fresh,
                tier: "full".to_string(),
                source: "engine".to_string(),
            },
        );
        let tool = CheckStatusTool {
            cwd: root.clone(),
            home: Some(home.path().to_path_buf()),
            session_started: Some(at),
        };
        let (text, result) = tool.status().await;
        assert_eq!(result.verdict, Verdict::NoClaim, "{text}");
        assert!(text.contains("from earlier sessions"), "{text}");
        assert!(text.contains("No check has run in this session"), "{text}");
        // The same result recorded in this session is current.
        let same_session = CheckStatusTool {
            session_started: Some(at - 1_000),
            ..tool
        };
        assert_eq!(same_session.status().await.1.verdict, Verdict::Fail);
    }

    #[test]
    fn a_current_not_run_keeps_its_reason_and_fix() {
        let stored = CheckResult::not_run(
            crate::gate::CHECK,
            check_result::NotRunReason::TimeLimit,
            "out of time",
            "give it more time",
        );
        let own = own_result(&stored, Freshness::Current);
        assert_eq!(own.verdict, Verdict::NotRun);
        assert_eq!(own.reason, Some(check_result::NotRunReason::TimeLimit));
        assert!(own.validate().is_ok());
    }
}
