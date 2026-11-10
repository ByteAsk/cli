//! `/login` — the ByteAsk settings surface, driven from the bottom pane.
//!
//! Navigation and edits arrive as [`SettingsRequest`]s raised by menu rows; this
//! module performs them and re-opens the relevant screen, so moving between
//! screens never leaves the TUI and never reprints the session banner.
//!
//! Edits are applied to `byok-config.json`, which the local sidecar re-reads per
//! request. Only the changes the engine cannot pick up mid-session — a provider
//! swap, the managed bearer token, the model catalog — hand off to the wrapper
//! for a single relaunch. See [`RestartNeed`].

use super::*;

use crate::bottom_pane::custom_prompt_view::CustomPromptView;
use crate::byteask_settings::RestartNeed;
use crate::byteask_settings::SettingsAction;
use crate::byteask_settings::SettingsState;
use crate::byteask_settings::SubscriptionProvider;
use crate::byteask_settings::presets;
use crate::byteask_settings::presets::ProviderPreset;
use crate::byteask_settings::store::ByokConfig;
use crate::byteask_settings::store::KeyProvider;
use crate::byteask_settings::views;
use crate::byteask_settings::views::SettingsRequest;

/// The gateway's sign-in prompts for a signed-out session all end by naming /login
/// ("Sign in to continue — type /login."). The message stays in history; the engine
/// also opens the email prompt, so the user signs in without having to type the command.
pub(super) fn is_gateway_sign_in_prompt(message: &str) -> bool {
    message.contains("type /login")
}

impl ChatWidget {
    /// Entry point for `/login`.
    pub(super) fn open_byteask_settings(&mut self) {
        self.show_byteask_settings(SettingsRequest::OpenRoot);
    }

    /// Perform one settings request and show whatever screen should follow.
    pub(crate) fn handle_byteask_settings(&mut self, request: SettingsRequest) {
        self.show_byteask_settings(request);
    }

    fn byteask_settings_state(&self) -> SettingsState {
        SettingsState::load(
            &self.config.codex_home,
            &self.config.model_provider_id,
            self.current_model(),
        )
    }

    /// Provider presets, re-read per screen.
    ///
    /// Cheap enough (one short-lived helper process, only on the two screens
    /// that need it) that caching it would risk showing a stale table after a
    /// `byteask --update` more than it would save.
    fn byteask_presets(&self) -> Vec<ProviderPreset> {
        presets::load(&self.config.codex_home)
    }

    fn show_byteask_settings(&mut self, request: SettingsRequest) {
        let state = self.byteask_settings_state();
        match request {
            SettingsRequest::OpenRoot => {
                self.bottom_pane
                    .show_selection_view(views::root_params(&state));
            }
            SettingsRequest::OpenKeys => {
                self.bottom_pane
                    .show_selection_view(views::keys_params(&state));
            }
            SettingsRequest::OpenRemoveKey => {
                self.bottom_pane
                    .show_selection_view(views::remove_key_params(&state));
            }
            SettingsRequest::OpenHostedModels => {
                let presets = self.byteask_presets();
                self.bottom_pane
                    .show_selection_view(views::hosted_models_params(&state, &presets));
            }
            SettingsRequest::OpenProviderPicker => {
                let presets = self.byteask_presets();
                if presets.is_empty() {
                    // Without the helper there are no presets to choose between,
                    // so go straight to the custom-server path.
                    self.show_byteask_settings(SettingsRequest::PromptEndpointAlias {
                        provider_id: "custom".to_string(),
                    });
                } else {
                    self.bottom_pane
                        .show_selection_view(views::provider_picker_params(&presets));
                }
            }
            SettingsRequest::PromptKey(provider) => self.prompt_byteask_key(provider),
            SettingsRequest::RemoveKey(provider) => self.remove_byteask_key(provider),
            SettingsRequest::UseManaged => self.use_byteask_managed(),
            SettingsRequest::PromptSignInEmail => self.prompt_byteask_email(),
            SettingsRequest::SignInAs { email } => self.request_byteask_signin(&email),
            SettingsRequest::AddCustomServer => {
                self.show_byteask_settings(SettingsRequest::PromptEndpointAlias {
                    provider_id: "custom".to_string(),
                })
            }
            SettingsRequest::AddPreset { provider_id } => {
                self.show_byteask_settings(SettingsRequest::PromptEndpointAlias { provider_id })
            }
            SettingsRequest::PromptEndpointAlias { provider_id } => {
                self.prompt_endpoint_alias(provider_id)
            }
            SettingsRequest::PromptEndpointUrl { provider_id, alias } => {
                self.prompt_endpoint_url(provider_id, alias)
            }
            SettingsRequest::PromptEndpointKey {
                provider_id,
                alias,
                url,
            } => self.prompt_endpoint_key(provider_id, alias, url),
            SettingsRequest::RegisterEndpoint {
                provider_id,
                alias,
                url,
                key,
            } => self.register_hosted_endpoint(&provider_id, &alias, &url, &key),
            SettingsRequest::RemoveEndpoint { alias } => self.remove_hosted_endpoint(&alias),
            SettingsRequest::ShowSubscriptionConsent { provider } => {
                self.bottom_pane
                    .show_selection_view(views::subscription_consent_params(provider));
            }
            SettingsRequest::UseSubscription { provider } => match provider {
                // ChatGPT: the engine now runs the device-code flow itself, in a pane.
                // It used to hand off here, and the wrapper's subprocess printed the
                // URL, the code and the result into scrollback where no pane could
                // clear them. The hand-off still happens - but only once the user is
                // signed in, to do the config swap the engine cannot do.
                SubscriptionProvider::Chatgpt => self.start_byteask_subscription_login(),
            },
            SettingsRequest::LaunchClaude => {
                // The engine cannot run `claude` itself (startup-owned launch); the
                // wrapper does, after we exit. Verb stays `login` for wrapper-skew
                // safety; line 2 is `launch-claude`.
                self.request_byteask_auth_with("login", "launch-claude")
            }
        }
    }

    /// Show a one-field prompt whose submitted text becomes the next request.
    ///
    /// Every settings field goes through here, so they all get the same treatment
    /// `/rename` has (accent gutter, bold title, dim placeholder) instead of the
    /// shell's bare `printf`/`read`. Submitting an empty value cancels back to
    /// `on_empty` rather than registering a half-built entry.
    fn prompt_settings_field<F>(
        &mut self,
        title: String,
        placeholder: String,
        context_label: Option<String>,
        on_empty: SettingsRequest,
        next: F,
    ) where
        F: Fn(String) -> SettingsRequest + Send + Sync + 'static,
    {
        let tx = self.app_event_tx.clone();
        let view = CustomPromptView::new(
            title,
            placeholder,
            /*initial_text*/ String::new(),
            context_label,
            Box::new(move |text: String| {
                let text = text.trim().to_string();
                let request = if text.is_empty() {
                    on_empty.clone()
                } else {
                    next(text)
                };
                tx.send(AppEvent::ByteAskSettings(request));
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    /// Ask for an API key using the same input treatment as `/rename`.
    fn prompt_byteask_key(&mut self, provider: KeyProvider) {
        let tx = self.app_event_tx.clone();
        let label = provider.label();
        let title = format!("{label} API key");
        let view = CustomPromptView::new(
            title,
            format!("Paste your {label} key and press Enter"),
            /*initial_text*/ String::new(),
            // Say who gets billed at the moment the user commits to it.
            Some(format!(
                "Used directly with {label} — they bill you, not ByteAsk"
            )),
            Box::new(move |key: String| {
                tx.send(AppEvent::ByteAskSettingsKeyEntered {
                    provider_id: provider.id().to_string(),
                    key,
                });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    /// Store a freshly entered key.
    pub(crate) fn apply_byteask_key(&mut self, provider_id: &str, key: String) {
        let Some(provider) = KeyProvider::from_id(provider_id) else {
            self.add_error_message(format!("Unknown provider '{provider_id}'."));
            return;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            self.show_byteask_settings(SettingsRequest::OpenKeys);
            return;
        }

        let state = self.byteask_settings_state();
        let mut config = ByokConfig::load(&self.config.codex_home);
        if let Err(err) = config.set_key(provider, &key) {
            self.add_error_message(format!(
                "Could not save your {} key: {err:#}",
                provider.label()
            ));
            return;
        }

        let restart = state.restart_need(SettingsAction::SetKey(provider));
        self.finish_byteask_edit(
            format!("Saved your {} key.", provider.label()),
            restart,
            SettingsRequest::OpenKeys,
        );
    }

    fn remove_byteask_key(&mut self, provider: KeyProvider) {
        let state = self.byteask_settings_state();
        let mut config = ByokConfig::load(&self.config.codex_home);
        if let Err(err) = config.set_key(provider, "") {
            self.add_error_message(format!(
                "Could not remove your {} key: {err:#}",
                provider.label()
            ));
            return;
        }

        let restart = state.restart_need(SettingsAction::RemoveKey(provider));
        self.finish_byteask_edit(
            format!("Removed your {} key.", provider.label()),
            restart,
            SettingsRequest::OpenKeys,
        );
    }

    fn use_byteask_managed(&mut self) {
        let state = self.byteask_settings_state();
        // ⚠️ A SUBSCRIPTION session has NO keyed providers, so the BYOK path below
        // would hit the `is_empty()` early return and do nothing at all — the row
        // that exists precisely to be the way back would be a silent no-op, leaving the
        // user stuck in a session whose models may not work.
        // The engine does not own config.toml, so hand it to the wrapper, which
        // restores the remembered managed model + catalog.
        if state.is_subscription_session() {
            self.request_byteask_auth_with("login", "managed");
            return;
        }
        if state.keyed_providers.is_empty() {
            self.show_byteask_settings(SettingsRequest::OpenRoot);
            return;
        }

        let mut config = ByokConfig::load(&self.config.codex_home);
        for provider in state.keyed_providers.clone() {
            if let Err(err) = config.set_key(provider, "") {
                self.add_error_message(format!("Could not remove your keys: {err:#}"));
                return;
            }
        }

        let restart = state.restart_need(SettingsAction::UseManaged);
        self.finish_byteask_edit(
            "Switched to ByteAsk managed.".to_string(),
            restart,
            SettingsRequest::OpenRoot,
        );
    }

    /// Ask which email to sign in as, here rather than at a shell prompt.
    pub(super) fn prompt_byteask_email(&mut self) {
        self.prompt_settings_field(
            "Sign in to ByteAsk".to_string(),
            "Enter your email and press Enter".to_string(),
            Some("We'll email you a sign-in link".to_string()),
            SettingsRequest::OpenRoot,
            |email| SettingsRequest::SignInAs { email },
        );
    }

    /// Completing sign-in still belongs to the wrapper: it owns the magic-link
    /// round-trip and the `experimental_bearer_token` line the engine only reads
    /// at startup. The address is collected above, so the shell is left with the
    /// waiting, not with a form.
    ///
    /// The detail matters — a bare `login` marker sends the wrapper into
    /// `do_account`, which for a signed-in user opens the *old* shell settings
    /// screen: the very menu this surface replaces.
    fn request_byteask_signin(&mut self, email: &str) {
        // Newlines would forge extra marker lines; the wrapper reads line 2 only,
        // but keep the value on one line so a paste can never smuggle a verb.
        let email = sanitize_marker_field(email);
        if email.is_empty() {
            self.request_byteask_auth_with("login", "signin");
        } else {
            self.request_byteask_auth_with("login", &format!("signin {email}"));
        }
    }

    /// Name the model being registered (`self/<alias>` in `/model`).
    fn prompt_endpoint_alias(&mut self, provider_id: String) {
        let for_url = provider_id == "custom";
        self.prompt_settings_field(
            "Name this model".to_string(),
            "Short name, e.g. my-dgx".to_string(),
            Some(if for_url {
                "It appears in /model as self/<name>".to_string()
            } else {
                format!("Runs on {provider_id}, under your key")
            }),
            SettingsRequest::OpenHostedModels,
            move |alias| SettingsRequest::PromptEndpointUrl {
                provider_id: provider_id.clone(),
                alias,
            },
        );
    }

    /// Ask for the server URL. A preset already knows its own, so it skips ahead.
    fn prompt_endpoint_url(&mut self, provider_id: String, alias: String) {
        if provider_id != "custom" {
            self.show_byteask_settings(SettingsRequest::PromptEndpointKey {
                provider_id,
                alias,
                url: String::new(),
            });
            return;
        }
        self.prompt_settings_field(
            "Server URL".to_string(),
            "e.g. http://dgx-host:8000".to_string(),
            Some("Any OpenAI-compatible server — vLLM, TGI, Ollama, LM Studio".to_string()),
            SettingsRequest::OpenHostedModels,
            move |url| SettingsRequest::PromptEndpointKey {
                provider_id: provider_id.clone(),
                alias: alias.clone(),
                url,
            },
        );
    }

    /// Ask for the optional API key — the last field before the hand-off.
    fn prompt_endpoint_key(&mut self, provider_id: String, alias: String, url: String) {
        let empty = SettingsRequest::RegisterEndpoint {
            provider_id: provider_id.clone(),
            alias: alias.clone(),
            url: url.clone(),
            key: String::new(),
        };
        self.prompt_settings_field(
            "API key".to_string(),
            "Press Enter to skip if your server needs none".to_string(),
            Some("Stored locally in byok-config.json, owner-only".to_string()),
            empty,
            move |key| SettingsRequest::RegisterEndpoint {
                provider_id: provider_id.clone(),
                alias: alias.clone(),
                url: url.clone(),
                key,
            },
        );
    }

    /// Hand a fully described endpoint to the wrapper to finish registering.
    ///
    /// Only the *fields* are collected here. Registration runs model discovery, a
    /// wire probe and a catalog merge — all implemented in `byteask_models.py` and
    /// driven by the wrapper's `models_add`, which `test/models.sh` covers. The new
    /// `self/*` row also cannot reach `/model` without a relaunch, so this path has
    /// to leave the session either way.
    ///
    /// The key never travels in the marker: that file is written world-readable, and
    /// this project already rejected passing keys through argv for the same reason.
    /// It goes in a sibling owner-only file the wrapper consumes and deletes.
    fn register_hosted_endpoint(&mut self, provider_id: &str, alias: &str, url: &str, key: &str) {
        let alias = sanitize_marker_field(alias);
        if alias.is_empty() {
            self.show_byteask_settings(SettingsRequest::OpenHostedModels);
            return;
        }
        let provider_id = sanitize_marker_field(provider_id);
        let url = sanitize_marker_field(url);

        if !key.trim().is_empty() {
            if let Err(err) = write_auth_secret(&self.config.codex_home, key.trim()) {
                // Losing the key silently would register an endpoint that 401s on
                // every turn with nothing on screen to explain it.
                self.add_error_message(format!("Could not stage your API key: {err:#}"));
                return;
            }
        }

        // `-` marks "no URL of its own" so the field count is fixed and a preset id
        // can never be mistaken for a URL.
        let url_field = if url.is_empty() { "-" } else { url.as_str() };
        let provider_field = if provider_id.is_empty() {
            "custom"
        } else {
            provider_id.as_str()
        };
        self.request_byteask_auth_with(
            "login",
            &format!("register-model {provider_field} {alias} {url_field}"),
        );
    }

    /// Hand removal to the wrapper, for the same reason as adding.
    ///
    /// Dropping the registry entry is only a third of the job: `models_remove`
    /// also rebuilds the model catalog (so the row leaves `/model`) and, when the
    /// removed endpoint was the *active* model, rewrites `config.toml` off it —
    /// otherwise the next launch sits on `model = "self/<gone>"` and fail-closes
    /// on every turn. Both of those are startup-owned state, so doing it here
    /// would leave a stale `/model` and a dead default behind.
    fn remove_hosted_endpoint(&mut self, alias: &str) {
        self.request_byteask_auth_with("login", &format!("remove-model {alias}"));
    }

    /// Report an applied edit, then either reopen the screen or hand off the one
    /// relaunch the change needs.
    fn finish_byteask_edit(
        &mut self,
        message: String,
        restart: RestartNeed,
        next: SettingsRequest,
    ) {
        if restart.is_required() {
            self.add_info_message(format!("{message} Restarting ByteAsk to apply it."), None);
            self.request_byteask_auth_with("login", "relaunch");
        } else {
            self.add_info_message(message, /*hint*/ None);
            self.show_byteask_settings(next);
        }
    }
}

/// Flatten a value to one whitespace-free marker field.
///
/// The marker is a line-oriented file the wrapper parses positionally, so a value
/// carrying a newline could forge a verb and a value carrying a space could shift
/// every field after it. Stripping whitespace makes both impossible, and costs
/// nothing real: aliases, provider ids and URLs never contain any.
fn sanitize_marker_field(value: &str) -> String {
    value.split_whitespace().collect::<String>()
}

/// Stage a secret for the wrapper in an owner-only file beside the marker.
///
/// Mode 0600 is set on the temp file *before* the rename, so there is never a
/// window where a world-readable file holds the key — the same rule
/// `byok-config.json` follows. The wrapper reads this file once and deletes it.
fn write_auth_secret(codex_home: &std::path::Path, secret: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;

    std::fs::create_dir_all(codex_home)
        .with_context(|| format!("creating {}", codex_home.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".byteask-auth-secret")
        .suffix(".tmp")
        .tempfile_in(codex_home)
        .with_context(|| format!("creating a temp file in {}", codex_home.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("restricting the staged secret's permissions")?;
    }
    temp.write_all(secret.as_bytes())
        .context("writing the staged secret")?;
    temp.flush().context("flushing the staged secret")?;
    temp.as_file()
        .sync_all()
        .context("syncing the staged secret")?;
    temp.persist(codex_home.join(".byteask-auth-secret"))
        .context("staging the API key for the wrapper")?;
    Ok(())
}
