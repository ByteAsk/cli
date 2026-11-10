use std::sync::Arc;

use codex_context_fragments::ContextualUserFragment;

use crate::ConversationHistorySnapshot;
use crate::ExtensionData;
use crate::ExtensionFuture;

/// Model-visible fragments a gate asks the host to record.
pub type TurnGateFragments = Vec<Box<dyn ContextualUserFragment + Send>>;

/// Input supplied to a turn gate at one of its fixed points.
pub struct TurnGateInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// The model's final message for this sampling step, when it ended the turn with one.
    pub last_agent_message: Option<&'a str>,
    /// Read-only snapshot of the conversation, taken after the step's tool outputs were recorded.
    pub conversation_history: Arc<dyn ConversationHistorySnapshot>,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Engine-enforced checks at two fixed points of a turn (ByteAsk).
///
/// Both callbacks run inline in the turn loop, so they may take as long as the check needs
/// (the host cancels them when the turn is interrupted). Implementations own their own time
/// budget and must return an empty list when they have nothing to say.
pub trait TurnGateContributor: Send + Sync {
    /// Called after a sampling step whose tool calls have all completed and been recorded,
    /// before the next model request of the same turn. Returned fragments are recorded, in
    /// order, after the step's tool outputs.
    fn after_step<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let _self = self;
            let _input = input;
            Vec::new()
        })
    }

    /// Called when the model ends its turn (a final answer with no tool calls), before any
    /// Stop hook. Returning fragments records them and continues the turn instead of ending it;
    /// an empty list lets the turn end.
    fn before_stop<'a>(
        &'a self,
        input: TurnGateInput<'a>,
    ) -> ExtensionFuture<'a, TurnGateFragments> {
        Box::pin(async move {
            let _self = self;
            let _input = input;
            Vec::new()
        })
    }

    /// Called once the turn is ending (every `before_stop` returned an empty list). A returned
    /// line is shown to the user and is not recorded for the model: a gate uses it to state the
    /// verdict the turn ended with, e.g. a failure that is still open after the retry limit.
    fn stop_notice<'a>(&'a self, input: TurnGateInput<'a>) -> ExtensionFuture<'a, Option<String>> {
        Box::pin(async move {
            let _self = self;
            let _input = input;
            None
        })
    }
}
