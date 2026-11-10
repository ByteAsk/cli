//! One shared result for every ByteAsk check and tool.
//!
//! Every result is one of five verdicts:
//!
//! | Verdict       | Means                                                              |
//! |---------------|--------------------------------------------------------------------|
//! | `PASS`        | the check ran to completion and found nothing wrong / the tool did exactly what was asked |
//! | `FAIL`        | the check ran to completion and found a problem                    |
//! | `NOT RUN`     | the check did not run, or did not finish, so it says nothing; always carries a reason and a one-line fix |
//! | `NO CLAIM`    | it ran, but the evidence cannot support a claim either way (a speed difference inside the noise, no matching document) |
//! | `APPROXIMATE` | it ran, but the answer is not exact (a heuristic, a partial parse) |
//!
//! The rules this crate makes hard to break:
//!
//! * **Only `PASS` is success.** [`CheckResult::is_pass`] is the one question
//!   any caller (a TUI tick, a stop hook, a merge check) may ask; there is no
//!   "ok-ish" verdict.
//! * **A `NOT RUN` cannot be built without a reason and a non-empty fix**:
//!   the exact command or setting that makes it run.
//! * **Existing tool text stays as it is**: the result is appended as
//!   one last line, `Result: ...`, rendered by [`CheckResult::render_line`] and
//!   read back by [`CheckResult::from_tool_text`].
//!
//! The JSON form (`serde`) is the record other components store and read
//! (results store, merge check, value report). Field names and the `schema`
//! number are a contract: add fields, never rename them.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Version of the JSON record. Bump only for an incompatible change.
pub const SCHEMA: u32 = 1;

/// Every rendered result line starts with this.
pub const LINE_PREFIX: &str = "Result: ";

/// Separates the summary from the fix on a `NOT RUN` line.
const FIX_MARKER: &str = " Fix: ";

/// The five verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Fail,
    NotRun,
    NoClaim,
    Approximate,
}

impl Verdict {
    /// Every verdict, longest word first (the order [`CheckResult::from_tool_text`] matches in).
    pub const ALL: [Verdict; 5] = [
        Verdict::Approximate,
        Verdict::NoClaim,
        Verdict::NotRun,
        Verdict::Pass,
        Verdict::Fail,
    ];

    /// The word shown to people and models.
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::NotRun => "NOT RUN",
            Verdict::NoClaim => "NO CLAIM",
            Verdict::Approximate => "APPROXIMATE",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

/// Why a check did not run (or did not finish).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunReason {
    /// Cut off by a time limit (never PASS).
    TimeLimit,
    /// The output was cut, so the verdict in it could not be read in full.
    OutputTruncated,
    /// A program the check needs is not installed.
    ToolMissing,
    /// The project is not trusted to run native code.
    NotTrusted,
    /// No sandbox could start.
    SandboxUnavailable,
    /// No network, and no self-hosted instance is configured.
    Offline,
    /// A service the check needs did not answer.
    ServiceUnavailable,
    /// The repository has no build profile.
    NoBuildProfile,
    /// The code did not build, so the check itself never ran.
    BuildFailed,
    /// The call's arguments were wrong.
    BadArguments,
    /// Not possible on this machine or platform (no PMU, Linux-only tool).
    Unsupported,
    /// The account's plan does not include this tool here.
    NeedsPlan,
    /// Proposed to a person and waiting for their approval (pair debugging).
    NeedsApproval,
    /// The change touches no code or build files.
    NoCodeChange,
    /// A person skipped it, with a reason.
    Skipped,
    /// Cancelled before it finished.
    Cancelled,
    /// The tool failed in a way that has no more specific reason.
    ToolError,
    /// No trusted result exists for this exact version (the merge check): nothing
    /// that counts at merge checked it.
    NotChecked,
}

impl NotRunReason {
    pub const ALL: [NotRunReason; 18] = [
        NotRunReason::TimeLimit,
        NotRunReason::OutputTruncated,
        NotRunReason::ToolMissing,
        NotRunReason::NotTrusted,
        NotRunReason::SandboxUnavailable,
        NotRunReason::Offline,
        NotRunReason::ServiceUnavailable,
        NotRunReason::NoBuildProfile,
        NotRunReason::BuildFailed,
        NotRunReason::BadArguments,
        NotRunReason::Unsupported,
        NotRunReason::NeedsPlan,
        NotRunReason::NeedsApproval,
        NotRunReason::NoCodeChange,
        NotRunReason::Skipped,
        NotRunReason::Cancelled,
        NotRunReason::ToolError,
        NotRunReason::NotChecked,
    ];

    /// The label shown in brackets: `NOT RUN (time limit)`.
    pub fn label(self) -> &'static str {
        match self {
            NotRunReason::TimeLimit => "time limit",
            NotRunReason::OutputTruncated => "output truncated",
            NotRunReason::ToolMissing => "tool not installed",
            NotRunReason::NotTrusted => "project not trusted",
            NotRunReason::SandboxUnavailable => "sandbox unavailable",
            NotRunReason::Offline => "offline",
            NotRunReason::ServiceUnavailable => "service unavailable",
            NotRunReason::NoBuildProfile => "no build profile",
            NotRunReason::BuildFailed => "build failed",
            NotRunReason::BadArguments => "bad arguments",
            NotRunReason::Unsupported => "not supported here",
            NotRunReason::NeedsPlan => "needs a plan",
            NotRunReason::NeedsApproval => "needs approval",
            NotRunReason::NoCodeChange => "no code or build change",
            NotRunReason::Skipped => "skipped",
            NotRunReason::Cancelled => "cancelled",
            NotRunReason::ToolError => "tool error",
            NotRunReason::NotChecked => "not checked",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        NotRunReason::ALL
            .into_iter()
            .find(|reason| reason.label() == label)
    }

    /// A fix for callers that know nothing more specific. Every reason has
    /// one; a specific fix is always better.
    pub fn generic_fix(self) -> &'static str {
        match self {
            NotRunReason::TimeLimit => "run it again with a longer time limit.",
            NotRunReason::OutputTruncated => {
                "run it again on less code (one file or one test) so the whole report fits."
            }
            NotRunReason::ToolMissing => "install the program named above, then run it again.",
            NotRunReason::NotTrusted => {
                "set BYTEASK_ALLOW_NATIVE_TOOLS=1, or mark the project trusted in config.toml."
            }
            NotRunReason::SandboxUnavailable => {
                "allow unprivileged user namespaces, or run `byteask policy relax` for this project."
            }
            NotRunReason::Offline => {
                "connect to the internet, or configure a self-hosted instance."
            }
            NotRunReason::ServiceUnavailable => {
                "try again in a minute; if it persists, check your network."
            }
            NotRunReason::NoBuildProfile => "add a build profile in .byteask/checks.toml.",
            NotRunReason::BuildFailed => "fix the build error shown above, then run it again.",
            NotRunReason::BadArguments => "call it again with the arguments its schema lists.",
            NotRunReason::Unsupported => "run it on a machine that supports it (named above).",
            NotRunReason::NeedsPlan => "run `byteask` in a terminal and type /upgrade.",
            NotRunReason::NeedsApproval => {
                "show the proposal to the user; if they agree, send it again with approved: true."
            }
            NotRunReason::NoCodeChange => "nothing to do: change code or build files to run it.",
            NotRunReason::Skipped => "run it again without skipping.",
            NotRunReason::Cancelled => "run it again and let it finish.",
            NotRunReason::ToolError => "read the error above, correct the cause, and run it again.",
            NotRunReason::NotChecked => {
                "run the required checks on CI for this exact version (push it, or re-run the job)."
            }
        }
    }
}

impl fmt::Display for NotRunReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One check's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// [`SCHEMA`] of this record.
    pub schema: u32,
    /// What ran: a tool name (`cpp_sanitize`) or a check name.
    pub check: String,
    pub verdict: Verdict,
    /// Present exactly when `verdict` is `NOT_RUN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<NotRunReason>,
    /// Present exactly when `verdict` is `NOT_RUN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// One plain sentence.
    pub summary: String,
    /// Short evidence lines (file:line, numbers, the command that ran).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Content hash of exactly what was checked, when the caller knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_key: Option<String>,
}

/// Why a [`CheckResult`] read from outside is not acceptable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidResult(pub String);

impl fmt::Display for InvalidResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid check result: {}", self.0)
    }
}

impl std::error::Error for InvalidResult {}

/// One line, no line breaks, no fix marker: the rendered line must parse back.
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.replace(FIX_MARKER.trim(), "fix:")
}

impl CheckResult {
    fn new(check: &str, verdict: Verdict, summary: &str) -> Self {
        Self {
            schema: SCHEMA,
            check: check.to_string(),
            verdict,
            reason: None,
            fix: None,
            summary: one_line(summary),
            evidence: Vec::new(),
            content_key: None,
        }
    }

    pub fn pass(check: &str, summary: &str) -> Self {
        Self::new(check, Verdict::Pass, summary)
    }

    pub fn fail(check: &str, summary: &str) -> Self {
        Self::new(check, Verdict::Fail, summary)
    }

    pub fn no_claim(check: &str, summary: &str) -> Self {
        Self::new(check, Verdict::NoClaim, summary)
    }

    pub fn approximate(check: &str, summary: &str) -> Self {
        Self::new(check, Verdict::Approximate, summary)
    }

    /// A `NOT RUN`. An empty `fix` is replaced by the reason's generic fix, so
    /// a `NOT RUN` without a fix cannot exist.
    pub fn not_run(check: &str, reason: NotRunReason, summary: &str, fix: &str) -> Self {
        let fix = one_line(fix);
        let fix = if fix.is_empty() {
            reason.generic_fix().to_string()
        } else {
            fix
        };
        Self {
            reason: Some(reason),
            fix: Some(fix),
            ..Self::new(check, Verdict::NotRun, summary)
        }
    }

    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = String>) -> Self {
        self.evidence
            .extend(evidence.into_iter().map(|line| one_line(&line)));
        self
    }

    pub fn with_content_key(mut self, key: impl Into<String>) -> Self {
        self.content_key = Some(key.into());
        self
    }

    /// The only success question there is.
    pub fn is_pass(&self) -> bool {
        self.verdict == Verdict::Pass
    }

    /// Check the invariants of a record read from outside (JSON from disk,
    /// another process). Records built with the constructors always pass.
    pub fn validate(&self) -> Result<(), InvalidResult> {
        if self.schema != SCHEMA {
            return Err(InvalidResult(format!("unknown schema {}", self.schema)));
        }
        if self.check.trim().is_empty() {
            return Err(InvalidResult("no check name".to_string()));
        }
        if self.summary.trim().is_empty() {
            return Err(InvalidResult("no summary".to_string()));
        }
        match (self.verdict, self.reason, self.fix.as_deref()) {
            (Verdict::NotRun, Some(_), Some(fix)) if !fix.trim().is_empty() => Ok(()),
            (Verdict::NotRun, _, _) => Err(InvalidResult(
                "NOT RUN needs a reason and a fix".to_string(),
            )),
            (_, None, None) => Ok(()),
            (verdict, _, _) => Err(InvalidResult(format!(
                "{verdict} must not carry a reason or a fix"
            ))),
        }
    }

    /// The one line appended to a tool's existing text, e.g.
    /// `Result: NOT RUN (time limit): The program was still running after 300 s. Fix: rerun with "timeout_secs": 900.`
    pub fn render_line(&self) -> String {
        let mut line = format!("{LINE_PREFIX}{}", self.verdict.word());
        if let Some(reason) = self.reason {
            line.push_str(&format!(" ({})", reason.label()));
        }
        line.push_str(": ");
        line.push_str(&self.summary);
        if let Some(fix) = &self.fix {
            line.push_str(FIX_MARKER);
            line.push_str(fix);
        }
        line
    }

    /// `text` with this result appended as its last line, the text itself
    /// unchanged.
    pub fn append_to(&self, text: &str) -> String {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            self.render_line()
        } else {
            format!("{text}\n\n{}", self.render_line())
        }
    }

    /// Read the result back from a tool's text: its LAST line, if that line is
    /// a result line. Anything else (a program that printed "Result: ok" in
    /// the middle of its output) is not a result.
    pub fn from_tool_text(check: &str, text: &str) -> Option<Self> {
        let last = text.trim_end_matches('\n').rsplit('\n').next()?;
        let rest = last.strip_prefix(LINE_PREFIX)?;
        let verdict = Verdict::ALL
            .into_iter()
            .find(|verdict| rest.starts_with(verdict.word()))?;
        let rest = &rest[verdict.word().len()..];
        let (reason, rest) = match rest.strip_prefix(" (") {
            Some(bracketed) => {
                let (label, after) = bracketed.split_once(')')?;
                (Some(NotRunReason::from_label(label)?), after)
            }
            None => (None, rest),
        };
        let body = rest.strip_prefix(": ")?;
        let (summary, fix) = match body.rsplit_once(FIX_MARKER) {
            Some((summary, fix)) if verdict == Verdict::NotRun => (summary, Some(fix.to_string())),
            _ => (body, None),
        };
        let result = Self {
            schema: SCHEMA,
            check: check.to_string(),
            verdict,
            reason,
            fix,
            summary: summary.to_string(),
            evidence: Vec::new(),
            content_key: None,
        };
        result.validate().ok()?;
        Some(result)
    }

    /// The JSON record.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests;
