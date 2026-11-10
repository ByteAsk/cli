//! ByteAsk Context's model-visible fragments: the turn-0 pack (user turn) and the instruction
//! (developer prefix). Both exist only in sessions with ByteAsk Context configured
//! (see `crate::byteask_context`).

use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

const PACK_START_MARKER: &str = "<byteask_context>";
const PACK_END_MARKER: &str = "</byteask_context>";

/// The instruction for sessions with ByteAsk Context (with the
/// verify tool `cpp_finish_check` and `get_symbol` for expansion).
pub(crate) const CONTEXT_INSTRUCTIONS: &str = "ByteAsk Context gave you, with the task, the code \
of the places the change most likely belongs, as the compiler resolves them, the repository's \
guidance, and the test programs that exercise them. If that code is enough, edit right away \
instead of searching and reading again. To check a change, call cpp_finish_check: it rebuilds \
incrementally, in a build tree that is already configured and built, only the test programs the \
change affects, runs them, and shows your diff with git's whitespace check. It takes from a few \
seconds to a few minutes: wait for it in one long wait instead of polling. When it passes and the \
change is complete, finish. Use get_symbol for bodies, callers and types the pack did not show. \
Do not write scratch test programs or build the whole project; use the targeted check, and build \
more only if it fails to build or cannot run the affected tests.";

/// The answer of `find_code_for_task` for the task in the same user turn, as the harness got it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextPack {
    answer: String,
}

impl ContextPack {
    pub(crate) fn new(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
        }
    }
}

impl ContextualUserFragment for ContextPack {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("byteask_context.pack".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (PACK_START_MARKER, PACK_END_MARKER)
    }

    fn body(&self) -> ContentItem {
        input_text(format!(
            "\nByteAsk Context's answer for this task (find_code_for_task, called by ByteAsk with the task text):\n{}\n",
            self.answer.trim_end()
        ))
    }
}

/// The developer-prefix instruction: [`CONTEXT_INSTRUCTIONS`] unless the config replaces it
/// (`crate::byteask_context::instructions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextInstructions(pub(crate) String);

impl ContextualUserFragment for ContextInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("byteask_context.instructions".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> ContentItem {
        // The host joins a developer message's parts with nothing in between.
        input_text(format!("{}\n\n", self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_pack_is_a_marked_user_fragment() {
        let pack = ContextPack::new("repo @ abc\n## Where the change most likely belongs\n1. f\n");
        assert_eq!(pack.role(), "user");
        let text = pack.render();
        assert_eq!(
            text,
            "<byteask_context>\nByteAsk Context's answer for this task (find_code_for_task, called by ByteAsk with the task text):\nrepo @ abc\n## Where the change most likely belongs\n1. f\n</byteask_context>"
        );
        assert!(ContextPack::matches_text(&text));
        assert!(!ContextPack::matches_text("fix the crash in parse()"));
    }

    #[test]
    fn the_instruction_carries_the_check_line_and_the_scratch_rule() {
        let text = ContextInstructions(CONTEXT_INSTRUCTIONS.to_string()).render();
        assert!(text.contains("call cpp_finish_check"));
        assert!(text.contains("Use get_symbol for bodies"));
        assert!(text.contains("Do not write scratch test programs or build the whole project"));
        assert!(!text.contains("context_verify") && !text.contains("context_expand"));
        assert_eq!(ContextInstructions(String::new()).role(), "developer");
    }
}
