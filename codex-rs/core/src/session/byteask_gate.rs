//! ByteAsk: host side of the extension turn gates ([`TurnGateContributor`]).
//!
//! Two fixed points in [`super::turn::run_turn`]'s loop:
//! - [`after_step`]: a sampling step ran tools and every output is recorded; before the next
//!   request. Contributors may add model-visible context (e.g. a change-impact checklist).
//! - [`before_stop`]: the model ended the turn with a final answer. Contributors may add
//!   feedback, which is recorded and makes the turn continue instead of ending.
//!
//! Kept in its own file so the turn loop carries only two call sites. With no contributor
//! registered both are no-ops.

use std::sync::Arc;

use codex_async_utils::OrCancelExt;
use codex_extension_api::TurnGateContributor;
use codex_extension_api::TurnGateInput;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tokio_util::sync::CancellationToken;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use codex_protocol::openai_models::ModelInfo;

#[derive(Clone, Copy)]
enum GatePoint {
    AfterStep,
    BeforeStop,
}

/// Runs every contributor's `after_step` and records what they return.
pub(super) async fn after_step(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    model_info: &ModelInfo,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    run(
        sess,
        turn_context,
        model_info,
        None,
        cancellation_token,
        GatePoint::AfterStep,
    )
    .await
    .map(|_| ())
}

/// Runs every contributor's `before_stop`. Returns `true` when feedback was recorded and the
/// turn must continue. When the turn ends, each contributor's `stop_notice` is shown to the
/// user as a warning (a gate's final verdict; never recorded for the model).
pub(super) async fn before_stop(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    model_info: &ModelInfo,
    last_agent_message: Option<&str>,
    cancellation_token: &CancellationToken,
) -> CodexResult<bool> {
    let continues = run(
        sess,
        turn_context,
        model_info,
        last_agent_message,
        cancellation_token,
        GatePoint::BeforeStop,
    )
    .await?;
    if !continues {
        stop_notices(sess, turn_context, last_agent_message, cancellation_token).await?;
    }
    Ok(continues)
}

async fn stop_notices(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: Option<&str>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let contributors: Vec<Arc<dyn TurnGateContributor>> =
        sess.services.extensions.turn_gate_contributors().to_vec();
    for contributor in contributors {
        let input = TurnGateInput {
            turn_id: turn_context.sub_id.as_str(),
            last_agent_message,
            conversation_history: sess.conversation_history_snapshot().await,
            session_store: &sess.services.session_extension_data,
            thread_store: &sess.services.thread_extension_data,
            turn_store: turn_context.extension_data.as_ref(),
        };
        let notice = contributor
            .stop_notice(input)
            .or_cancel(cancellation_token)
            .await
            .map_err(|_| CodexErr::TurnAborted)?;
        if let Some(message) = notice {
            sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
                .await;
        }
    }
    Ok(())
}

async fn run(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    model_info: &ModelInfo,
    last_agent_message: Option<&str>,
    cancellation_token: &CancellationToken,
    point: GatePoint,
) -> CodexResult<bool> {
    let contributors: Vec<Arc<dyn TurnGateContributor>> =
        sess.services.extensions.turn_gate_contributors().to_vec();
    if contributors.is_empty() {
        return Ok(false);
    }
    let mut items: Vec<ResponseItem> = Vec::new();
    for contributor in contributors {
        let input = TurnGateInput {
            turn_id: turn_context.sub_id.as_str(),
            last_agent_message,
            conversation_history: sess.conversation_history_snapshot().await,
            session_store: &sess.services.session_extension_data,
            thread_store: &sess.services.thread_extension_data,
            turn_store: turn_context.extension_data.as_ref(),
        };
        let fragments = match point {
            GatePoint::AfterStep => contributor.after_step(input),
            GatePoint::BeforeStop => contributor.before_stop(input),
        }
        .or_cancel(cancellation_token)
        .await
        .map_err(|_| CodexErr::TurnAborted)?;
        items.extend(
            fragments
                .into_iter()
                .map(ContextualUserFragment::into_boxed_response_item),
        );
    }
    if items.is_empty() {
        return Ok(false);
    }
    sess.record_conversation_items(turn_context, model_info, &items)
        .await;
    Ok(true)
}
