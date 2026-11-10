use super::TurnError;
use crate::JsonSchema;
use crate::RequestId;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuthRecoveryNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub provider: String,
    pub message: String,
}

/// ByteAsk Context: one turn-0 pack attempt for a user message (sent only in sessions with
/// ByteAsk Context configured).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextPackNotification {
    pub thread_id: String,
    pub turn_id: String,
    /// The harness called `find_code_for_task` for this message.
    pub attempted: bool,
    /// The answer went into the user turn before the first model request.
    pub injected: bool,
    /// Why nothing was injected (`not_a_task`, `not_cpp_repo`, `not_git_repo`, `not_indexed`,
    /// `indexing`, `timeout`, `error`); null when injected.
    pub reason: Option<String>,
    pub latency_ms: Option<u64>,
    pub answer_bytes: Option<u64>,
    pub approx_tokens: Option<u64>,
    /// The test programs the answer names as exercising its top places.
    pub test_programs: Vec<String>,
    /// A background build of those test programs was started.
    pub warm_build_started: bool,
    /// Pre-approved indexing of an unindexed repository: `started` or `failed`.
    pub auto_index: Option<String>,
    /// The one line for the user, if any.
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DeprecationNoticeNotification {
    /// Concise summary of what is deprecated.
    pub summary: String,
    /// Optional extra guidance, such as migration steps or rationale.
    pub details: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WarningNotification {
    /// Optional thread target when the warning applies to a specific thread.
    pub thread_id: Option<String>,
    /// Concise warning message for the user.
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GuardianWarningNotification {
    /// Thread target for the guardian warning.
    pub thread_id: String,
    /// Concise guardian warning message for the user.
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct StrictReviewRequiredNotification {
    pub thread_id: String,
    pub turn_id: String,
    /// Unix timestamp (in milliseconds) when this review started.
    #[ts(type = "number")]
    pub started_at_ms: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ErrorNotification {
    pub error: TurnError,
    // Set to true if the error is transient and the app-server process will automatically retry.
    // If true, this will not interrupt a turn.
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerRequestResolvedNotification {
    pub thread_id: String,
    pub request_id: RequestId,
}
