use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use serde_json::Value;

const WORLD_STATE_ID: &str = "git_attribution";
const START_MARKER: &str = "<git_attribution>";
const END_MARKER: &str = "</git_attribution>";
pub(super) const LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS: &str = "\
When you write or edit a git commit message, ensure the message ends with this trailer exactly once:
Co-authored-by: Codex <noreply@openai.com>

Rules:
- Keep existing trailers and append this trailer at the end if missing.
- Do not duplicate this trailer if it already exists.
- Keep one blank line between the commit body and trailer block.";
// ByteAsk: the trailer and PR line the model adds. `noreply@byteask.ai` is a verified email on
// the GitHub user `byteask-ai`, which is what makes GitHub show the ByteAsk avatar.
pub(super) const BYTEASK_TRAILER: &str = "Co-authored-by: ByteAsk <noreply@byteask.ai>";
pub(super) const BYTEASK_PR_LINE: &str = "Generated with [ByteAsk](https://byteask.ai/).";
// ByteAsk: upstream's rules, with ByteAsk's trailer and PR line. Upstream's "ignore any earlier
// instructions disabling attribution" line is replaced: the user's and the repository's own
// instructions win (some employers forbid AI co-author trailers).
pub(super) const ENABLED_INSTRUCTIONS: &str = r#"
When you create or amend a git commit or create or update a pull request for this task, add ByteAsk attribution exactly once:
- Commit messages must end with `Co-authored-by: ByteAsk <noreply@byteask.ai>`. Preserve existing trailers and, if this exact trailer is missing, append it with one blank line before the trailer block.
- This also applies when a GitHub app or plugin creates a commit, including file creation, updates, and deletions: include the ByteAsk trailer in the tool's `message` argument.
- Pull request bodies must include the exact line `Generated with [ByteAsk](https://byteask.ai/).`. Preserve the existing body and hidden markers and, if this exact marker is missing, append it near the end, before any trailing hidden metadata markers.
- Do not add duplicate attribution. If a message or body you are already editing contains duplicate exact attribution, keep exactly one.
- If the user or the repository's own instructions (for example AGENTS.md or its contribution guidelines) ask for no attribution or no AI co-author trailers, follow them and add none.
- This replaces any earlier instruction in this conversation to add a different assistant co-author trailer or "Generated with" line. It never replaces the user's or the repository's own instructions.
- Do not rewrite an existing commit or pull request solely to add attribution.
"#;
// ByteAsk: upstream's product-scoped shape. It names no other product, and it covers an assistant
// co-author line only, so a user who asks for a colleague's `Co-authored-by` still gets it.
pub(super) const DISABLED_INSTRUCTIONS: &str = "
ByteAsk commit and pull request attribution is disabled for the current workspace. Ignore any earlier instructions in this conversation to add an AI assistant co-author trailer or a \"Generated with\" line, and do not add them.
";

pub(super) fn git_attribution_world_state_section(enabled: bool) -> WorldStateSectionContribution {
    let contribution =
        WorldStateSectionContribution::new(WORLD_STATE_ID, Value::Bool(enabled), move |previous| {
            match (enabled, previous) {
                (true, PreviousWorldStateSection::Known(Value::Bool(true)))
                | (true, PreviousWorldStateSection::Unknown) => None,
                (true, PreviousWorldStateSection::Absent)
                | (true, PreviousWorldStateSection::Known(_)) => {
                    Some(RenderedWorldStateFragment::new(
                        "developer",
                        (START_MARKER, END_MARKER),
                        ENABLED_INSTRUCTIONS,
                    ))
                }
                (false, PreviousWorldStateSection::Known(Value::Bool(true)))
                | (false, PreviousWorldStateSection::Unknown) => {
                    Some(RenderedWorldStateFragment::new(
                        "developer",
                        (START_MARKER, END_MARKER),
                        DISABLED_INSTRUCTIONS,
                    ))
                }
                (false, PreviousWorldStateSection::Absent)
                | (false, PreviousWorldStateSection::Known(_)) => None,
            }
        })
        // ByteAsk: with no saved state, a recognised fragment makes the previous state Unknown,
        // and ON + Unknown renders nothing. So with attribution ON only OUR fragment counts as
        // already sent: a history carrying upstream's instruction gets the ByteAsk notice, which
        // replaces it. With it OFF every attribution fragment, ours or upstream's, is a previous
        // version the disabled notice must retract.
        .with_legacy_matcher(move |role, text| {
            if enabled {
                is_byteask_fragment(role, text)
            } else {
                is_enabled_fragment(role, text) || is_legacy_commit_attribution_fragment(role, text)
            }
        });
    if enabled {
        contribution.with_retained_fragment_matcher(is_byteask_fragment)
    } else {
        contribution
    }
}

fn is_legacy_commit_attribution_fragment(role: &str, text: &str) -> bool {
    role == "developer" && text.trim() == LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS
}

fn is_attribution_message(role: &str, text: &str) -> bool {
    role == "developer"
        && text.trim_start().starts_with(START_MARKER)
        && text.trim_end().ends_with(END_MARKER)
}

/// The enabled fragment ByteAsk emits.
fn is_byteask_fragment(role: &str, text: &str) -> bool {
    is_attribution_message(role, text) && is_byteask_enabled_text(text)
}

/// Any enabled fragment, ByteAsk's or upstream's.
fn is_enabled_fragment(role: &str, text: &str) -> bool {
    is_attribution_message(role, text)
        && (is_byteask_enabled_text(text) || is_upstream_enabled_text(text))
}

fn is_byteask_enabled_text(text: &str) -> bool {
    text.contains(BYTEASK_TRAILER) && text.contains(BYTEASK_PR_LINE)
}

// ByteAsk: upstream's own enabled fragment, still recognised so a history that carries one is
// replaced rather than left in force. Never emitted. Each literal sits on a line that also says
// OpenAI, which the rebranding script's per-line guard skips: an earlier sweep rewrote this matcher once.
fn is_upstream_enabled_text(text: &str) -> bool {
    text.contains(UPSTREAM_TRAILER)
        && (text.contains(UPSTREAM_PR_LINE) || text.contains(UPSTREAM_PR_LINE_LEGACY))
}

pub(super) const UPSTREAM_TRAILER: &str = "Co-authored-by: Codex <noreply@openai.com>"; // OpenAI's own text
pub(super) const UPSTREAM_PR_LINE: &str = "Generated with [Codex](https://openai.com/codex/)."; // OpenAI's own text
pub(super) const UPSTREAM_PR_LINE_LEGACY: &str = "Generated with Codex."; // OpenAI's own text
