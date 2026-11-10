use codex_extension_api::ExtensionData;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_protocol::protocol::SessionSource;
use serde_json::Value;

use super::UserAttributionSetting;
use super::UserSettingRecorder;
use super::attribution_enabled;
use super::world_state::BYTEASK_PR_LINE;
use super::world_state::BYTEASK_TRAILER;
use super::world_state::DISABLED_INSTRUCTIONS;
use super::world_state::ENABLED_INSTRUCTIONS;
use super::world_state::LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS;
use super::world_state::UPSTREAM_PR_LINE;
use super::world_state::UPSTREAM_TRAILER;
use super::world_state::git_attribution_world_state_section;

fn wrapped(body: &str) -> String {
    format!("<git_attribution>{body}</git_attribution>")
}

#[test]
fn attribution_needs_the_served_switch_and_the_user_setting() {
    assert!(attribution_enabled(true, Some(true)));
    assert!(!attribution_enabled(false, Some(true)));
    assert!(!attribution_enabled(true, Some(false)));
    assert!(!attribution_enabled(false, Some(false)));
    // A conversation whose setting was never recorded counts as opted out.
    assert!(!attribution_enabled(true, None));
}

#[test]
fn the_enabled_fragment_carries_the_byteask_trailer_and_pr_line_once() {
    let fragment = git_attribution_world_state_section(true)
        .render_diff(PreviousWorldStateSection::Absent)
        .expect("a new conversation gets the enabled fragment");
    let body = fragment.body();
    assert_eq!(body.matches(BYTEASK_TRAILER).count(), 1, "{body}");
    assert_eq!(body.matches(BYTEASK_PR_LINE).count(), 1, "{body}");
    assert_eq!(
        BYTEASK_TRAILER,
        "Co-authored-by: ByteAsk <noreply@byteask.ai>"
    );
    assert_eq!(
        BYTEASK_PR_LINE,
        "Generated with [ByteAsk](https://byteask.ai/)."
    );
}

#[test]
fn no_emitted_text_names_another_product() {
    for text in [ENABLED_INSTRUCTIONS, DISABLED_INSTRUCTIONS] {
        assert!(!text.contains("Codex"), "{text}");
        assert!(!text.contains("openai.com"), "{text}");
    }
    assert!(DISABLED_INSTRUCTIONS.contains("ByteAsk"));
}

#[test]
fn the_user_and_repository_rules_win_over_the_enabled_notice() {
    assert!(!ENABLED_INSTRUCTIONS.contains("Ignore any earlier instructions disabling"));
    assert!(ENABLED_INSTRUCTIONS.contains("follow them and add none"));
}

#[test]
fn switching_off_mid_conversation_retracts_the_notice_once() {
    let off = git_attribution_world_state_section(false);
    let retract = off
        .render_diff(PreviousWorldStateSection::Known(&Value::Bool(true)))
        .expect("a conversation that had attribution on is told it is off");
    assert_eq!(retract.body(), DISABLED_INSTRUCTIONS);
    // A conversation that never had it on is told nothing.
    assert!(off.render_diff(PreviousWorldStateSection::Absent).is_none());
    assert!(
        off.render_diff(PreviousWorldStateSection::Known(&Value::Bool(false)))
            .is_none()
    );
    // Already on: nothing new to say.
    assert!(
        git_attribution_world_state_section(true)
            .render_diff(PreviousWorldStateSection::Known(&Value::Bool(true)))
            .is_none()
    );
}

#[test]
fn the_matcher_recognises_the_fragment_we_emit() {
    // The 09-21 rebrand sweep left this matcher looking for text nothing emits, which breaks
    // resume de-duplication the moment attribution is on.
    let emitted = wrapped(ENABLED_INSTRUCTIONS);
    for enabled in [true, false] {
        let section = git_attribution_world_state_section(enabled);
        assert!(
            section.matches_legacy_fragment("developer", &emitted),
            "enabled={enabled}"
        );
        assert!(!section.matches_legacy_fragment("user", &emitted));
    }
    assert!(
        git_attribution_world_state_section(true).matches_retained_fragment("developer", &emitted)
    );
}

#[test]
fn an_upstream_fragment_in_history_is_replaced_whichever_way_the_switch_is() {
    // Kept on purpose, and outside the rebranding script's reach.
    assert!(UPSTREAM_TRAILER.contains("Codex") && UPSTREAM_TRAILER.contains("openai.com"));
    let upstream = wrapped(&format!(
        "\nCommits must end with `{UPSTREAM_TRAILER}`.\nPR bodies must include `{UPSTREAM_PR_LINE}`.\n"
    ));
    // OFF: upstream's fragment is a previous version, so the disabled notice retracts it.
    let off = git_attribution_world_state_section(false);
    assert!(off.matches_legacy_fragment("developer", &upstream));
    assert!(off.matches_legacy_fragment("developer", LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS));
    // ON: it must NOT count as ours. A recognised fragment with no saved state makes the
    // previous state Unknown, ON + Unknown renders nothing, and the Codex instruction would
    // stay in force with no ByteAsk notice to replace it.
    let on = git_attribution_world_state_section(true);
    assert!(!on.matches_legacy_fragment("developer", &upstream));
    assert!(!on.matches_legacy_fragment("developer", LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS));
    assert!(!on.matches_retained_fragment("developer", &upstream));
    assert!(on.render_diff(PreviousWorldStateSection::Absent).is_some());
    assert!(ENABLED_INSTRUCTIONS.contains("replaces any earlier instruction in this conversation"));
}

#[tokio::test]
async fn the_user_setting_is_recorded_when_the_conversation_starts() {
    for setting in [true, false] {
        let recorder = UserSettingRecorder {
            enabled_for: move |config: &bool| *config && setting,
        };
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("thread");
        recorder
            .on_thread_start(ThreadStartInput {
                config: &true,
                session_source: &SessionSource::Cli,
                persistent_thread_state_available: false,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session_store,
                thread_store: &thread_store,
            })
            .await;
        let recorded = thread_store.get::<UserAttributionSetting>().map(|s| s.0);
        assert_eq!(recorded, Some(setting));
    }
}
