//! ByteAsk settings (`/login`) — state model and restart policy.
//!
//! The settings UI used to live in the `byteask` shell wrapper: `/login` wrote a
//! marker file, the engine **exited**, the wrapper painted a raw-ANSI screen,
//! and the engine relaunched — reprinting its whole banner every time, even for
//! a read-only look. That screen also could not reuse any of the TUI's own
//! widgets, so it had no menu surface, no aligned columns, and ad-hoc inputs.
//!
//! The UI now lives in the bottom pane like `/model` and `/rename`. The wrapper
//! screen stays as the pre-TUI / non-TTY onboarding path only.
//!
//! Most edits no longer need a restart, because the local sidecar re-reads
//! [`store::ByokConfig`] on every request. See [`RestartNeed`] for what still does.

pub(crate) mod presets;
pub(crate) mod store;
pub(crate) mod views;

use std::path::Path;

use store::ByokConfig;
use store::Endpoint;
use store::KeyProvider;

/// Which provider `config.toml` currently points the engine at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveProvider {
    /// `byteask` — every call goes through the ByteAsk gateway, billed to the account.
    Managed,
    /// `byok-local` — the engine talks to the loopback sidecar, which routes
    /// per request: own key → straight to the provider, otherwise → the gateway.
    ByokLocal,
    /// Anything else (plain `openai`, a ChatGPT subscription): the engine owns
    /// its own auth and these settings do not apply.
    Other,
}

impl ActiveProvider {
    pub(crate) fn from_id(provider_id: &str) -> Self {
        match provider_id {
            "byteask" => ActiveProvider::Managed,
            "byok-local" => ActiveProvider::ByokLocal,
            _ => ActiveProvider::Other,
        }
    }
}

/// Whether applying an edit requires relaunching the engine.
///
/// The engine loads `model_provider`, its bearer token, and the model catalog
/// **once at startup**; nothing can swap them mid-session. Everything else is
/// read by the sidecar per request, so it takes effect immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartNeed {
    /// Applies immediately — the settings view stays open.
    None,
    /// Needs one relaunch; the wrapper performs it after we exit.
    Required,
}

impl RestartNeed {
    pub(crate) fn is_required(self) -> bool {
        matches!(self, RestartNeed::Required)
    }
}

/// An edit the settings UI applies in-process.
///
/// Adding and removing hosted models are deliberately absent: both also rebuild
/// the model catalog (and removal can rewrite `config.toml`'s active model),
/// which is startup-owned state driven by `byteask_models.py`. Those are handed
/// to the wrapper whole rather than half-applied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsAction {
    /// Save (or replace) one provider key.
    SetKey(KeyProvider),
    /// Delete one stored provider key.
    RemoveKey(KeyProvider),
    /// Drop every key and go back to the managed gateway.
    UseManaged,
}

/// A subscription the user can power ByteAsk with (their own vendor plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionProvider {
    Chatgpt,
}

impl SubscriptionProvider {
    /// Display name used in every row and heading. A plan name, not a slug.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SubscriptionProvider::Chatgpt => "ChatGPT subscription",
        }
    }
}

/// Per-provider "is subscription sign-in offered right now?" — the server-fetched
/// kill switches (`GET /byteask/v1/subscription-flags`, default false). A
/// fetch that never arrives leaves them false, so the rows are simply not shown
/// (fail-safe: we never offer a path the operator has not turned on).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionFlags {
    pub chatgpt: bool,
    /// Whether the "Use my Claude Code" launcher row is offered. NOT a
    /// subscription (it runs the user's own unmodified `claude` on their own
    /// plan), but it shares the same server-fetched
    /// kill-switch file so the operator can reveal it independently, default off.
    pub claude_launcher: bool,
}

impl SubscriptionFlags {
    pub(crate) fn any(self) -> bool {
        self.chatgpt
    }
}

/// Everything the settings views render, read once per open.
#[derive(Debug, Clone)]
pub(crate) struct SettingsState {
    pub provider: ActiveProvider,
    /// Account email from the session token's `sub`, when signed in.
    pub email: Option<String>,
    pub signed_in: bool,
    pub keyed_providers: Vec<KeyProvider>,
    pub endpoints: Vec<Endpoint>,
    /// Model currently selected in `/model`, used to tell whether removing an
    /// endpoint would pull the rug out from under the running session.
    pub current_model: String,
    /// Which subscription sign-ins the gateway currently offers (kill switches).
    pub subscription: SubscriptionFlags,
    /// Whether the "Use my Claude Code" launcher row is offered — true only when
    /// the user's own `claude` is on PATH AND the `claude_launcher` server flag is
    /// on (default off).
    pub launch_claude_available: bool,
}

/// Is the user's own `claude` binary on PATH? Synchronous, no spawn (checking at
/// settings-load must be cheap and must never run Claude Code).
fn claude_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("claude").is_file())
}

/// Read the subscription kill-switch flags the wrapper wrote to
/// `$BYTEASK_HOME/subscription-flags` at launch (lines `chatgpt=<0|1>` /
/// `claude_launcher=<0|1>`, from `GET /byteask/v1/subscription-flags`). Missing,
/// unreadable or malformed => all false: fail-safe, so the engine never offers
/// a subscription path the operator has not turned on. The wrapper refreshes the
/// file each launch, so a dashboard toggle reaches clients on their next start.
fn read_subscription_flags(codex_home: &Path) -> SubscriptionFlags {
    let mut flags = SubscriptionFlags::default();
    let Ok(text) = std::fs::read_to_string(codex_home.join("subscription-flags")) else {
        return flags;
    };
    for line in text.lines() {
        match line.split_once('=') {
            Some(("chatgpt", v)) => flags.chatgpt = v.trim() == "1",
            Some(("claude_launcher", v)) => flags.claude_launcher = v.trim() == "1",
            _ => {}
        }
    }
    flags
}

impl SettingsState {
    pub(crate) fn load(codex_home: &Path, provider_id: &str, current_model: &str) -> Self {
        let subscription = read_subscription_flags(codex_home);
        Self::load_at(
            codex_home,
            provider_id,
            current_model,
            subscription,
            now_unix(),
        )
    }

    /// Test seam: `now_unix` fixed so token-expiry behaviour is deterministic.
    fn load_at(
        codex_home: &Path,
        provider_id: &str,
        current_model: &str,
        subscription: SubscriptionFlags,
        now: i64,
    ) -> Self {
        let config = ByokConfig::load(codex_home);
        let token = config.jwt();
        // Mirrors the wrapper's `_is_signed_in`: a token that cannot be decoded
        // counts as valid, so a parsing quirk never forces a pointless re-login.
        let signed_in = token.is_some_and(|token| !store::jwt_expired(token, now));
        Self {
            provider: ActiveProvider::from_id(provider_id),
            email: token.and_then(store::jwt_subject),
            signed_in,
            keyed_providers: config.keyed_providers(),
            endpoints: config.endpoints(),
            current_model: current_model.to_string(),
            subscription,
            launch_claude_available: claude_on_path() && subscription.claude_launcher,
        }
    }

    /// True when the active provider is neither managed nor BYOK — i.e. a
    /// subscription session (the engine owns its own auth; these settings mostly
    /// offer the way back to managed).
    pub(crate) fn is_subscription_session(&self) -> bool {
        self.provider == ActiveProvider::Other
    }

    pub(crate) fn has_key(&self, provider: KeyProvider) -> bool {
        self.keyed_providers.contains(&provider)
    }

    /// One-line account summary for the settings header.
    pub(crate) fn account_summary(&self) -> String {
        // A subscription session runs on the user's own plan; name that plainly so
        // the header does not read as a plain managed sign-in.
        if self.is_subscription_session() {
            return match &self.email {
                Some(email) => format!("{email} · subscription session"),
                None => "subscription session".to_string(),
            };
        }
        match (&self.email, self.signed_in) {
            (Some(email), true) => format!("{email} · signed in"),
            // An email with an expired token is worth naming: it tells the user
            // *which* account to sign back into.
            (Some(email), false) => format!("{email} · session expired"),
            (None, _) => "not signed in · own keys and hosted models only".to_string(),
        }
    }

    /// Right-hand tag for the keys row: whose key pays for a given provider.
    pub(crate) fn key_tag(&self, provider: KeyProvider) -> &'static str {
        if self.has_key(provider) {
            "your key"
        } else {
            "managed"
        }
    }

    /// Whether an action needs the engine relaunched to take effect.
    pub(crate) fn restart_need(&self, action: SettingsAction) -> RestartNeed {
        match action {
            // Adding the first key flips `config.toml` to `byok-local` and needs
            // the sidecar in front of the engine; once there, later key edits are
            // picked up per request by the sidecar.
            SettingsAction::SetKey(_) => match self.provider {
                ActiveProvider::ByokLocal => RestartNeed::None,
                _ => RestartNeed::Required,
            },
            // Dropping the last key hands routing back to the managed provider.
            SettingsAction::RemoveKey(provider) => {
                let last_key = self.keyed_providers == vec![provider];
                if last_key || self.provider != ActiveProvider::ByokLocal {
                    RestartNeed::Required
                } else {
                    RestartNeed::None
                }
            }
            SettingsAction::UseManaged => match self.provider {
                ActiveProvider::Managed => RestartNeed::None,
                _ => RestartNeed::Required,
            },
        }
    }

    /// True when `alias` is the model the session is currently running.
    pub(crate) fn is_active_endpoint(&self, alias: &str) -> bool {
        self.current_model
            .strip_prefix("self/")
            .is_some_and(|current| current == alias)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn state_with(provider: ActiveProvider, keys: &[KeyProvider]) -> SettingsState {
        SettingsState {
            provider,
            email: Some("a@b.com".to_string()),
            signed_in: true,
            keyed_providers: keys.to_vec(),
            endpoints: Vec::new(),
            current_model: "gpt-5.4".to_string(),
            subscription: SubscriptionFlags::default(),
            launch_claude_available: false,
        }
    }

    #[test]
    fn read_subscription_flags_is_false_when_the_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Fail-safe: no file at all => never offer a subscription path.
        assert_eq!(
            read_subscription_flags(dir.path()),
            SubscriptionFlags::default()
        );
    }

    #[test]
    fn read_subscription_flags_parses_each_provider_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("subscription-flags"),
            "chatgpt=1\nclaude_launcher=1\n",
        )
        .unwrap();
        let flags = read_subscription_flags(dir.path());
        assert!(flags.chatgpt);
        assert!(flags.claude_launcher);
    }

    #[test]
    fn claude_launcher_defaults_off_and_ignores_an_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        // A key this build does not know (`claude=1`) must NOT turn the launcher on.
        std::fs::write(dir.path().join("subscription-flags"), "claude=1\n").unwrap();
        let flags = read_subscription_flags(dir.path());
        assert_eq!(flags, SubscriptionFlags::default());
        assert!(
            !flags.claude_launcher,
            "launcher stays off unless its own flag is set"
        );
    }

    #[test]
    fn read_subscription_flags_ignores_junk_and_stays_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        // Only an explicit `=1` turns a provider on; anything else stays off.
        std::fs::write(
            dir.path().join("subscription-flags"),
            "chatgpt=yes\ngarbage\nclaude_launcher=1\n",
        )
        .unwrap();
        let flags = read_subscription_flags(dir.path());
        assert!(!flags.chatgpt);
        assert!(flags.claude_launcher);
    }

    #[test]
    fn read_subscription_flags_takes_the_last_line_for_a_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("subscription-flags"),
            "chatgpt=yes\nchatgpt=1\n",
        )
        .unwrap();
        let flags = read_subscription_flags(dir.path());
        assert!(flags.chatgpt, "last explicit chatgpt=1 wins");
    }

    #[test]
    fn subscription_session_summary_names_the_plan() {
        let mut s = state_with(ActiveProvider::Other, &[]);
        s.email = Some("dev@x.com".to_string());
        assert_eq!(s.account_summary(), "dev@x.com · subscription session");
        assert!(s.is_subscription_session());
    }

    #[test]
    fn key_edits_under_byok_apply_without_a_restart() {
        // The sidecar re-reads byok-config.json per request, so there is nothing
        // for a relaunch to pick up.
        let state = state_with(ActiveProvider::ByokLocal, &[KeyProvider::OpenAi]);
        assert_eq!(
            state.restart_need(SettingsAction::SetKey(KeyProvider::Anthropic)),
            RestartNeed::None
        );
        assert_eq!(
            state.restart_need(SettingsAction::SetKey(KeyProvider::OpenAi)),
            RestartNeed::None
        );
    }

    #[test]
    fn first_key_from_managed_requires_a_restart() {
        let state = state_with(ActiveProvider::Managed, &[]);
        assert_eq!(
            state.restart_need(SettingsAction::SetKey(KeyProvider::OpenAi)),
            RestartNeed::Required
        );
    }

    #[test]
    fn removing_the_last_key_requires_a_restart_but_others_do_not() {
        let two = state_with(
            ActiveProvider::ByokLocal,
            &[KeyProvider::OpenAi, KeyProvider::Gemini],
        );
        assert_eq!(
            two.restart_need(SettingsAction::RemoveKey(KeyProvider::OpenAi)),
            RestartNeed::None
        );

        let last = state_with(ActiveProvider::ByokLocal, &[KeyProvider::OpenAi]);
        assert_eq!(
            last.restart_need(SettingsAction::RemoveKey(KeyProvider::OpenAi)),
            RestartNeed::Required
        );
    }

    #[test]
    fn switching_to_managed_when_already_managed_is_a_no_op() {
        assert_eq!(
            state_with(ActiveProvider::Managed, &[]).restart_need(SettingsAction::UseManaged),
            RestartNeed::None
        );
        assert_eq!(
            state_with(ActiveProvider::ByokLocal, &[KeyProvider::OpenAi])
                .restart_need(SettingsAction::UseManaged),
            RestartNeed::Required
        );
    }

    #[test]
    fn active_endpoint_matches_only_the_selected_self_model() {
        let mut state = state_with(ActiveProvider::ByokLocal, &[]);
        state.current_model = "self/mybox".to_string();
        assert!(state.is_active_endpoint("mybox"));
        assert!(!state.is_active_endpoint("otherbox"));

        state.current_model = "gpt-5.4".to_string();
        assert!(!state.is_active_endpoint("mybox"));
    }

    #[test]
    fn expired_token_reads_as_signed_out_but_keeps_the_email() {
        let dir = tempfile::tempdir().expect("tempdir");
        // {"sub":"a@b.com","exp":1000}
        std::fs::write(
            ByokConfig::path_for(dir.path()),
            r#"{"jwt":"h.eyJzdWIiOiJhQGIuY29tIiwiZXhwIjoxMDAwfQ.s"}"#,
        )
        .expect("write");

        let live = SettingsState::load_at(
            dir.path(),
            "byteask",
            "gpt-5.4",
            SubscriptionFlags::default(),
            999,
        );
        assert!(live.signed_in);
        assert_eq!(live.account_summary(), "a@b.com · signed in");

        let expired = SettingsState::load_at(
            dir.path(),
            "byteask",
            "gpt-5.4",
            SubscriptionFlags::default(),
            1001,
        );
        assert!(!expired.signed_in);
        // Naming the account tells the user which one to sign back into.
        assert_eq!(expired.account_summary(), "a@b.com · session expired");
    }

    #[test]
    fn provider_ids_map_to_the_three_modes() {
        assert_eq!(ActiveProvider::from_id("byteask"), ActiveProvider::Managed);
        assert_eq!(
            ActiveProvider::from_id("byok-local"),
            ActiveProvider::ByokLocal
        );
        assert_eq!(ActiveProvider::from_id("openai"), ActiveProvider::Other);
    }

    #[test]
    fn key_tag_names_who_pays() {
        let state = state_with(ActiveProvider::ByokLocal, &[KeyProvider::Anthropic]);
        assert_eq!(state.key_tag(KeyProvider::Anthropic), "your key");
        assert_eq!(state.key_tag(KeyProvider::OpenAi), "managed");
    }
}
