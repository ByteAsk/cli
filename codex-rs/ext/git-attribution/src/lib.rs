// ByteAsk: upstream's policy fetch (the ChatGPT backend's workspace setting) is kept so upstream
// merges stay small, and its tests still run, but nothing calls it any more.
#[cfg_attr(not(test), allow(dead_code))]
mod policy;
mod served;
mod world_state;

use std::sync::Arc;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;

pub use crate::served::install_from_catalog_json;
use crate::world_state::git_attribution_world_state_section;

/// Contributes model instructions for agent-created git commits and pull requests.
#[derive(Clone)]
// ByteAsk: these are the inputs of upstream's policy fetch, kept for merges and no longer read.
#[allow(dead_code)]
struct GitAttributionExtension {
    auth_manager: Arc<AuthManager>,
    base_url: String,
    http_client_factory: HttpClientFactory,
}

impl ContextContributor for GitAttributionExtension {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            // ByteAsk: attribution follows our two switches and never the ChatGPT backend's
            // workspace setting, so a subscription session behaves like every other one and makes
            // no network call here.
            let user_setting = input
                .thread_store
                .get::<UserAttributionSetting>()
                .map(|setting| setting.0);
            vec![git_attribution_world_state_section(attribution_enabled(
                served::served_enabled(),
                user_setting,
            ))]
        })
    }
}

/// The user's `commit_attribution` feature, read once when the conversation starts. A running
/// conversation keeps it: the engine does not refresh features mid-conversation.
struct UserAttributionSetting(bool);

/// Attribution needs BOTH the served catalog switch (off by default) and the user's feature. A
/// conversation whose setting was never recorded counts as opted out: the opt-out must hold on
/// every path.
fn attribution_enabled(served: bool, user_setting: Option<bool>) -> bool {
    served && user_setting == Some(true)
}

/// Records the user's setting in the thread store at conversation start.
struct UserSettingRecorder<F> {
    enabled_for: F,
}

impl<C, F> ThreadLifecycleContributor<C> for UserSettingRecorder<F>
where
    C: Sync,
    F: Fn(&C) -> bool + Send + Sync,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        input
            .thread_store
            .insert(UserAttributionSetting((self.enabled_for)(input.config)));
        Box::pin(async {})
    }
}

/// Installs the git-attribution contributor into the extension registry.
///
/// `user_enabled` reads the user's opt-out from the host configuration.
pub fn install<C, F>(
    registry: &mut ExtensionRegistryBuilder<C>,
    auth_manager: Arc<AuthManager>,
    base_url: String,
    http_client_factory: HttpClientFactory,
    user_enabled: F,
) where
    C: Sync + 'static,
    F: Fn(&C) -> bool + Send + Sync + 'static,
{
    registry.thread_lifecycle_contributor(Arc::new(UserSettingRecorder {
        enabled_for: user_enabled,
    }));
    registry.prompt_contributor(Arc::new(GitAttributionExtension {
        auth_manager,
        base_url,
        http_client_factory,
    }));
}

#[cfg(test)]
#[path = "git_attribution_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "byteask_tests.rs"]
mod byteask_tests;
