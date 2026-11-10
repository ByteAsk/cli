//! Menu definitions for the ByteAsk settings surface.
//!
//! These are pure builders: each returns a [`SelectionViewParams`] the chat
//! widget hands to `bottom_pane.show_selection_view`. Keeping them free of
//! `ChatWidget` means every screen can be rendered and snapshot-tested directly.
//!
//! Layout rules, so the screens read as one system:
//! - **name** = the thing you act on, **description** = what it does,
//!   **right_tag** = who pays / current status. Status never runs into prose.
//! - [`ColumnWidthMode::AutoAllRows`] so columns are measured across every row
//!   and stay put while scrolling.
//! - Title/subtitle carry the context; no `›`-joined breadcrumb strings.

use crate::app_event::AppEvent;
use crate::bottom_pane::ColumnWidthMode;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

use super::SettingsState;
use super::SubscriptionProvider;
use super::presets::ProviderPreset;
use super::store::Endpoint;
use super::store::KeyProvider;

/// A navigation or edit request raised by a settings menu row.
///
/// Rows only ever *describe* what the user asked for; the chat widget owns
/// performing it, so the menus stay pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsRequest {
    OpenRoot,
    OpenKeys,
    OpenRemoveKey,
    OpenHostedModels,
    OpenProviderPicker,
    /// Prompt for a key for this provider.
    PromptKey(KeyProvider),
    RemoveKey(KeyProvider),
    /// Drop every stored key and go back to the managed gateway.
    UseManaged,
    /// Ask for the email to sign in as.
    PromptSignInEmail,
    /// Start the magic-link sign-in flow for an email already collected here.
    SignInAs {
        email: String,
    },
    /// Add a hosted model on a server the user runs, entered by URL.
    AddCustomServer,
    /// Add a hosted model from a known preset.
    AddPreset {
        provider_id: String,
    },
    /// Name a hosted model that is being registered.
    PromptEndpointAlias {
        provider_id: String,
    },
    /// Ask for the server URL (custom endpoints only — a preset supplies its own).
    PromptEndpointUrl {
        provider_id: String,
        alias: String,
    },
    /// Ask for the optional API key, the last step before handing off.
    PromptEndpointKey {
        provider_id: String,
        alias: String,
        url: String,
    },
    /// Every field collected: hand the registration to the wrapper.
    RegisterEndpoint {
        provider_id: String,
        alias: String,
        url: String,
        key: String,
    },
    RemoveEndpoint {
        alias: String,
    },
    /// Show the "what you give up" consent screen before switching to a
    /// subscription session. Selecting a subscription row never switches
    /// directly — it always lands here first.
    ShowSubscriptionConsent {
        provider: SubscriptionProvider,
    },
    /// Confirmed on the consent screen: hand the subscription sign-in to the
    /// wrapper (it owns the OAuth flow + the provider swap + relaunch).
    UseSubscription {
        provider: SubscriptionProvider,
    },
    /// Launch the user's own Claude Code.
    /// The wrapper runs `claude` on the user's subscription; the engine just
    /// hands off.
    LaunchClaude,
}

fn request_action(request: SettingsRequest) -> Vec<crate::bottom_pane::SelectionAction> {
    vec![Box::new(
        move |tx: &crate::app_event_sender::AppEventSender| {
            tx.send(AppEvent::ByteAskSettings(request.clone()));
        },
    )]
}

/// Shared frame for every settings screen, so they cannot drift apart.
fn screen(title: &str, subtitle: &str, items: Vec<SelectionItem>) -> SelectionViewParams {
    SelectionViewParams {
        title: Some(title.to_string()),
        subtitle: Some(subtitle.to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        // Measure across all rows: a column that shifts as you scroll is exactly
        // the "no alignment" complaint this screen exists to fix.
        col_width_mode: ColumnWidthMode::AutoAllRows,
        ..Default::default()
    }
}

/// A row that navigates or acts, with an optional right-aligned status.
fn row(
    name: &str,
    description: &str,
    right_tag: Option<String>,
    request: SettingsRequest,
) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        description: Some(description.to_string()),
        right_tag,
        actions: request_action(request),
        dismiss_on_select: true,
        ..Default::default()
    }
}

/// Root screen: where you are, and the three things you can change.
pub(crate) fn root_params(state: &SettingsState) -> SelectionViewParams {
    let key_count = state.keyed_providers.len();
    let keys_tag = match key_count {
        0 => "managed".to_string(),
        1 => "1 key".to_string(),
        n => format!("{n} keys"),
    };
    let hosted_tag = match state.endpoints.len() {
        0 => "none".to_string(),
        n => format!("{n} added"),
    };

    let mut items = vec![
        row(
            "API keys",
            "Use your own provider keys",
            Some(keys_tag),
            SettingsRequest::OpenKeys,
        ),
        row(
            "Hosted models",
            "Your own server or a cloud provider",
            Some(hosted_tag),
            SettingsRequest::OpenHostedModels,
        ),
        row(
            "Account",
            "Sign in or switch email",
            Some(
                if state.signed_in {
                    "signed in"
                } else {
                    "signed out"
                }
                .to_string(),
            ),
            SettingsRequest::PromptSignInEmail,
        ),
    ];

    // Subscription sign-in — shown only when the gateway currently offers it
    // (the server-fetched kill switches, default off). Named as the user's own
    // plan. Selecting one opens the consent screen, never switches directly.
    if state.subscription.chatgpt {
        items.push(row(
            "Use my ChatGPT subscription",
            "Run ByteAsk on your own ChatGPT plan",
            None,
            SettingsRequest::ShowSubscriptionConsent {
                provider: SubscriptionProvider::Chatgpt,
            },
        ));
    }

    // Run the user's own Claude Code, when it's installed.
    if state.launch_claude_available {
        items.push(row(
            "Use my Claude Code",
            "Run your own Claude Code on your subscription",
            None,
            SettingsRequest::LaunchClaude,
        ));
    }

    // Offer the way back when there is something to come back from: stored keys,
    // or a subscription session (whose active provider is not `byteask`).
    if key_count > 0 || state.is_subscription_session() {
        // An action, not a state, so it carries no status tag: the right column
        // means "how this row stands today" everywhere else on the screen.
        items.push(row(
            "Use ByteAsk managed",
            "Switch back to ByteAsk's models and billing",
            None,
            SettingsRequest::UseManaged,
        ));
    }

    screen("ByteAsk settings", &state.account_summary(), items)
}

/// A non-selectable context line (the feature-loss list on the consent screen).
/// Disabled so the cursor skips it; it is there to be read, not chosen.
fn info_line(text: &str) -> SelectionItem {
    SelectionItem {
        name: text.to_string(),
        is_disabled: true,
        actions: Vec::new(),
        dismiss_on_select: false,
        ..Default::default()
    }
}

/// Consent screen shown before switching to a subscription session.
///
/// A subscription session is a pure vendor session that never touches the
/// ByteAsk gateway, so it silently loses every gateway feature. The user must
/// see that list before committing — this is the one screen that exists to make
/// the trade explicit. `esc`/`Back` returns without changing anything; the only
/// forward action is the single Continue row (the wrapper then owns sign-in).
pub(crate) fn subscription_consent_params(
    provider: SubscriptionProvider,
) -> SelectionViewParams {
    let mut items = vec![
        row(
            &format!("Continue with your {}", provider.label()),
            "Sign in with your own plan, then relaunch",
            None,
            SettingsRequest::UseSubscription { provider },
        ),
        row(
            "Back",
            "Keep your current setup",
            None,
            SettingsRequest::OpenRoot,
        ),
    ];
    // The losses. Kept short and concrete; sentence fragments, no exclamation.
    items.push(info_line("This runs on your own plan and leaves ByteAsk managed."));
    items.push(info_line("While it is on you lose:"));
    items.push(info_line("  · /usage, billing and your plan limits"));
    items.push(info_line("  · credits, referrals and vouchers"));
    items.push(info_line("  · Auto, and ByteAsk's other models"));
    match provider {
        SubscriptionProvider::Chatgpt => items.push(info_line("  · Claude and Gemini models")),
    }
    items.push(info_line("Switch back anytime with Use ByteAsk managed."));

    screen(
        &format!("Use your {}", provider.label()),
        "Your own plan pays for model use, not ByteAsk",
        items,
    )
}

/// Bring-your-own-key screen.
pub(crate) fn keys_params(state: &SettingsState) -> SelectionViewParams {
    let mut items: Vec<SelectionItem> = KeyProvider::ALL
        .into_iter()
        .map(|provider| {
            let has_key = state.has_key(provider);
            SelectionItem {
                name: provider.label().to_string(),
                description: Some(
                    if has_key {
                        "Replace your key"
                    } else {
                        "Add your key"
                    }
                    .to_string(),
                ),
                right_tag: Some(state.key_tag(provider).to_string()),
                actions: request_action(SettingsRequest::PromptKey(provider)),
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect();

    if !state.keyed_providers.is_empty() {
        items.push(row(
            "Remove a key",
            "Forget one saved key",
            None,
            SettingsRequest::OpenRemoveKey,
        ));
    }
    items.push(row(
        "Back",
        "Return to settings",
        None,
        SettingsRequest::OpenRoot,
    ));

    screen(
        "API keys",
        "Your key goes straight to the provider — they bill you",
        items,
    )
}

/// Which stored key to forget.
pub(crate) fn remove_key_params(state: &SettingsState) -> SelectionViewParams {
    let mut items: Vec<SelectionItem> = state
        .keyed_providers
        .iter()
        .map(|provider| {
            row(
                provider.label(),
                "Forget this key",
                Some("your key".to_string()),
                SettingsRequest::RemoveKey(*provider),
            )
        })
        .collect();
    items.push(row(
        "Back",
        "Keep every key",
        None,
        SettingsRequest::OpenKeys,
    ));

    screen(
        "Remove a key",
        "Removing only forgets it here — add it again anytime",
        items,
    )
}

/// Headline status for an already-registered endpoint.
///
/// The right column answers "the one thing you need to know about this row".
/// For a working endpoint that is who pays; for one that failed the tool
/// round-trip it is that fact, which outranks billing — such a model answers
/// prose but never calls a tool, so edits, shell and search silently do nothing.
///
/// A preset tells us who owns the hardware. A hand-entered URL does not, so it
/// is described as the user's own server rather than guessed at — claiming
/// "never billed" about a paid endpoint would be worse than saying less.
fn endpoint_status(endpoint: &Endpoint, presets: &[ProviderPreset]) -> String {
    if endpoint.tools_ok == Some(false) {
        return "no tool calls".to_string();
    }
    endpoint
        .provider_id
        .as_deref()
        .and_then(|id| presets.iter().find(|preset| preset.id == id))
        .map(|preset| preset.kind.billing_tag().to_string())
        .unwrap_or_else(|| "your server".to_string())
}

/// Registered `self/<alias>` models.
pub(crate) fn hosted_models_params(
    state: &SettingsState,
    presets: &[ProviderPreset],
) -> SelectionViewParams {
    let mut items: Vec<SelectionItem> = state
        .endpoints
        .iter()
        .map(|endpoint| SelectionItem {
            name: format!("self/{}", endpoint.alias),
            description: Some(endpoint.base_url.clone()),
            right_tag: Some(endpoint_status(endpoint, presets)),
            is_current: state.is_active_endpoint(&endpoint.alias),
            actions: request_action(SettingsRequest::RemoveEndpoint {
                alias: endpoint.alias.clone(),
            }),
            dismiss_on_select: true,
            ..Default::default()
        })
        .collect();

    items.push(row(
        "Add a model",
        "Pick a provider or enter a URL",
        None,
        SettingsRequest::OpenProviderPicker,
    ));
    items.push(row(
        "Back",
        "Return to settings",
        None,
        SettingsRequest::OpenRoot,
    ));

    let subtitle = if state.endpoints.is_empty() {
        "Run any OpenAI-compatible server — vLLM, TGI, Ollama, LM Studio, DGX"
    } else {
        "Selecting a model removes it — add it again anytime"
    };
    screen("Hosted models", subtitle, items)
}

/// Provider presets, cloud first, with billing pinned to the right.
pub(crate) fn provider_picker_params(presets: &[ProviderPreset]) -> SelectionViewParams {
    let mut items: Vec<SelectionItem> = presets
        .iter()
        .map(|preset| SelectionItem {
            name: preset.id.clone(),
            description: Some(preset.base_url.clone()),
            right_tag: Some(preset.kind.billing_tag().to_string()),
            actions: request_action(SettingsRequest::AddPreset {
                provider_id: preset.id.clone(),
            }),
            dismiss_on_select: true,
            ..Default::default()
        })
        .collect();

    items.push(SelectionItem {
        name: "Custom server".to_string(),
        description: Some("Enter your own URL".to_string()),
        right_tag: Some("never billed".to_string()),
        actions: request_action(SettingsRequest::AddCustomServer),
        dismiss_on_select: true,
        ..Default::default()
    });
    items.push(SelectionItem {
        name: "Back".to_string(),
        description: Some("Return to hosted models".to_string()),
        actions: request_action(SettingsRequest::OpenHostedModels),
        dismiss_on_select: true,
        ..Default::default()
    });

    screen(
        "Add a hosted model",
        "Cloud providers run on your key; a local server never bills you",
        items,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event_sender::AppEventSender;
    use crate::bottom_pane::ListSelectionView;
    use crate::byteask_settings::ActiveProvider;
    use crate::byteask_settings::SubscriptionFlags;
    use crate::byteask_settings::presets::PresetKind;
    use crate::render::renderable::Renderable as _;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc::unbounded_channel;

    /// Render a screen exactly as the bottom pane would, so snapshots capture
    /// the real menu surface, cursor, and column alignment.
    fn render(params: SelectionViewParams, width: u16) -> String {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let view = ListSelectionView::new(
            params,
            AppEventSender::new(tx_raw),
            crate::keymap::RuntimeKeymap::defaults().list,
        );
        let height = view.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| {
                        let symbol = buf[(col, row)].symbol();
                        if symbol.is_empty() {
                            " ".to_string()
                        } else {
                            symbol.to_string()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn state(keys: &[KeyProvider], endpoints: Vec<Endpoint>) -> SettingsState {
        SettingsState {
            provider: ActiveProvider::ByokLocal,
            email: Some("dev@example.com".to_string()),
            signed_in: true,
            keyed_providers: keys.to_vec(),
            endpoints,
            current_model: "gpt-5.4".to_string(),
            subscription: SubscriptionFlags::default(),
            launch_claude_available: false,
        }
    }

    fn names(params: &SelectionViewParams) -> Vec<String> {
        params.items.iter().map(|i| i.name.clone()).collect()
    }

    #[test]
    fn root_hides_subscription_rows_when_the_flags_are_off() {
        // Default flags = both off => no subscription rows offered (fail-safe).
        let names = names(&root_params(&state(&[], Vec::new())));
        assert!(!names.iter().any(|n| n.contains("subscription")));
    }

    #[test]
    fn root_shows_only_the_enabled_subscription_rows() {
        let mut s = state(&[], Vec::new());
        s.subscription = SubscriptionFlags {
            chatgpt: true,
            ..Default::default()
        };
        let names = names(&root_params(&s));
        assert!(names.iter().any(|n| n == "Use my ChatGPT subscription"));
        assert_eq!(
            names.iter().filter(|n| n.contains("subscription")).count(),
            1,
            "one subscription row: {names:?}"
        );
    }

    #[test]
    fn subscription_session_offers_the_way_back_to_managed() {
        // Other provider (a subscription session) with no keys still gets the
        // "Use ByteAsk managed" row, or the user would be stuck.
        let mut s = state(&[], Vec::new());
        s.provider = ActiveProvider::Other;
        assert!(names(&root_params(&s)).iter().any(|n| n == "Use ByteAsk managed"));
    }

    #[test]
    fn consent_screen_lists_losses_and_one_forward_action() {
        let p = subscription_consent_params(SubscriptionProvider::Chatgpt);
        let ns = names(&p);
        // Exactly one forward (Continue) + Back are selectable; the rest are the
        // disabled loss lines the user must read first.
        assert!(ns.iter().any(|n| n.starts_with("Continue with your ChatGPT subscription")));
        assert!(ns.iter().any(|n| n == "Back"));
        assert!(ns.iter().any(|n| n.contains("/usage")));
        assert!(ns.iter().any(|n| n.contains("Claude and Gemini")),
                "chatgpt consent names the model families it loses");
        let selectable = p.items.iter().filter(|i| !i.is_disabled).count();
        assert_eq!(selectable, 2, "only Continue + Back are selectable");
    }

    #[test]
    fn launcher_row_shows_only_when_claude_is_on_path() {
        let mut s = state(&[], Vec::new());
        assert!(!names(&root_params(&s)).iter().any(|n| n == "Use my Claude Code"),
                "no launcher row when claude is absent");
        s.launch_claude_available = true;
        assert!(names(&root_params(&s)).iter().any(|n| n == "Use my Claude Code"),
                "launcher row appears when claude is on PATH");
    }

    fn sample_presets() -> Vec<ProviderPreset> {
        vec![
            ProviderPreset {
                id: "groq".to_string(),
                base_url: "https://api.groq.com/openai/v1".to_string(),
                needs_key: true,
                kind: PresetKind::Cloud,
            },
            ProviderPreset {
                id: "openrouter".to_string(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                needs_key: true,
                kind: PresetKind::Cloud,
            },
            ProviderPreset {
                id: "ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                needs_key: false,
                kind: PresetKind::Local,
            },
        ]
    }

    #[test]
    fn renders_root_screen() {
        let view = state(
            &[KeyProvider::Anthropic, KeyProvider::Gemini],
            vec![Endpoint {
                alias: "mybox".to_string(),
                base_url: "http://localhost:8000".to_string(),
                provider_id: None,
                tools_ok: None,
            }],
        );
        assert_snapshot!("byteask_settings_root", render(root_params(&view), 76));
    }

    #[test]
    fn renders_keys_screen() {
        let view = state(&[KeyProvider::Anthropic, KeyProvider::Gemini], Vec::new());
        assert_snapshot!("byteask_settings_keys", render(keys_params(&view), 76));
    }

    #[test]
    fn renders_remove_key_screen() {
        let view = state(&[KeyProvider::Anthropic, KeyProvider::Gemini], Vec::new());
        assert_snapshot!(
            "byteask_settings_remove_key",
            render(remove_key_params(&view), 76)
        );
    }

    #[test]
    fn renders_hosted_models_screen() {
        let view = state(
            &[],
            vec![
                Endpoint {
                    alias: "dgx".to_string(),
                    base_url: "https://dgx.internal:8000".to_string(),
                    provider_id: None,
                    tools_ok: Some(true),
                },
                Endpoint {
                    alias: "tiny".to_string(),
                    base_url: "http://localhost:11434".to_string(),
                    provider_id: Some("ollama".to_string()),
                    tools_ok: Some(false),
                },
            ],
        );
        assert_snapshot!(
            "byteask_settings_hosted_models",
            render(hosted_models_params(&view, &sample_presets()), 76)
        );
    }

    /// The screen from the bug report: billing used to be glued onto the URL
    /// text. It must now sit in its own right-aligned column.
    #[test]
    fn renders_provider_picker_with_billing_in_its_own_column() {
        let rendered = render(provider_picker_params(&sample_presets()), 76);
        assert_snapshot!("byteask_settings_provider_picker", rendered);
    }

    #[test]
    fn provider_picker_billing_tags_are_flush_right_and_aligned() {
        let rendered = render(provider_picker_params(&sample_presets()), 76);
        let tag_columns: Vec<usize> = rendered
            .lines()
            .filter(|line| line.contains("they bill you") || line.contains("never billed"))
            .filter_map(|line| {
                line.find("they bill you")
                    .or_else(|| line.find("never billed"))
            })
            .collect();
        assert!(
            tag_columns.len() >= 3,
            "expected a tag on every provider row, got {tag_columns:?}"
        );
        // Every tag ends at the same column => a real column, not trailing prose.
        let ends: Vec<usize> = rendered
            .lines()
            .filter(|line| line.contains("they bill you") || line.contains("never billed"))
            .map(|line| line.trim_end().chars().count())
            .collect();
        assert!(
            ends.windows(2).all(|pair| pair[0] == pair[1]),
            "billing tags must all end at the same column, got {ends:?}"
        );
    }

    /// The shaded menu background, padding and `›` cursor all come from
    /// `ListSelectionView::render` -> `render_menu_surface` — the same treatment
    /// `/model` gets, and the thing the old shell screen could never have. The
    /// background colour itself depends on the terminal palette (absent in
    /// tests), so assert the surface *geometry* it produces instead; a screen
    /// hand-rolled outside `ListSelectionView` would not have it.
    #[test]
    fn settings_screens_use_the_shared_menu_surface() {
        let rendered = render(root_params(&state(&[], Vec::new())), 76);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some(""),
            "surface pads a blank row above the title"
        );
        let title = lines.get(1).copied().unwrap_or_default();
        assert!(
            title.starts_with("  ") && !title.starts_with("   "),
            "content sits inside the two-column surface inset: {title:?}"
        );
    }

    #[test]
    fn root_hides_managed_row_until_a_key_exists() {
        let managed_only = state(&[], Vec::new());
        let names: Vec<String> = root_params(&managed_only)
            .items
            .iter()
            .map(|item| item.name.clone())
            .collect();
        assert!(
            !names.iter().any(|name| name == "Use ByteAsk managed"),
            "nothing to switch back from yet: {names:?}"
        );

        let with_key = state(&[KeyProvider::OpenAi], Vec::new());
        let names: Vec<String> = root_params(&with_key)
            .items
            .iter()
            .map(|item| item.name.clone())
            .collect();
        assert!(names.iter().any(|name| name == "Use ByteAsk managed"));
    }

    #[test]
    fn keys_screen_offers_removal_only_when_a_key_exists() {
        let none = keys_params(&state(&[], Vec::new()));
        assert!(!none.items.iter().any(|item| item.name == "Remove a key"));

        let some = keys_params(&state(&[KeyProvider::Gemini], Vec::new()));
        assert!(some.items.iter().any(|item| item.name == "Remove a key"));
    }

    #[test]
    fn key_rows_tag_who_pays() {
        let params = keys_params(&state(&[KeyProvider::Anthropic], Vec::new()));
        let tags: Vec<(String, Option<String>)> = params
            .items
            .iter()
            .map(|item| (item.name.clone(), item.right_tag.clone()))
            .collect();
        assert_eq!(tags[0], ("OpenAI".to_string(), Some("managed".to_string())));
        assert_eq!(
            tags[1],
            ("Anthropic".to_string(), Some("your key".to_string()))
        );
    }

    #[test]
    fn custom_endpoint_billing_is_not_guessed() {
        let custom = Endpoint {
            alias: "x".to_string(),
            base_url: "https://who-knows.example".to_string(),
            provider_id: None,
            tools_ok: None,
        };
        assert_eq!(endpoint_status(&custom, &sample_presets()), "your server");

        let preset = Endpoint {
            alias: "y".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            provider_id: Some("groq".to_string()),
            tools_ok: None,
        };
        assert_eq!(endpoint_status(&preset, &sample_presets()), "they bill you");
    }

    #[test]
    fn tool_incapable_models_say_so_instead_of_who_bills() {
        // A model that cannot call tools is unusable for agent work, so that
        // outranks billing in the one status column.
        let broken = Endpoint {
            alias: "tiny".to_string(),
            base_url: "http://localhost:11434".to_string(),
            provider_id: Some("ollama".to_string()),
            tools_ok: Some(false),
        };
        assert_eq!(endpoint_status(&broken, &sample_presets()), "no tool calls");

        let view = state(&[], vec![broken]);
        let params = hosted_models_params(&view, &sample_presets());
        assert_eq!(
            params.items[0].right_tag.as_deref(),
            Some("no tool calls"),
            "silent tool failure must be visible on the row"
        );
    }

    #[test]
    fn every_row_dispatches_exactly_one_request() {
        let view = state(&[KeyProvider::OpenAi], Vec::new());
        for params in [
            root_params(&view),
            keys_params(&view),
            remove_key_params(&view),
            hosted_models_params(&view, &sample_presets()),
            provider_picker_params(&sample_presets()),
        ] {
            for item in &params.items {
                assert_eq!(
                    item.actions.len(),
                    1,
                    "row {:?} must raise exactly one request",
                    item.name
                );
            }
        }
    }
}
