//! ByteAsk account slash commands (`/usage`, `/referral-code`, `/upgrade`,
//! `/voucher`) and the occasional referral promo banner.
//!
//! These talk to the ByteAsk gateway using the same access pattern the
//! `cpp-tools` extension uses for its gateway-backed tools: the provider
//! `base_url` plus the per-user `experimental_bearer_token` carried on
//! [`Config::model_provider`]. Requests run on a background task, so the
//! composer never blocks on the network.
//!
//! **The four readouts never enter the transcript.** They open an
//! [`InfoPaneView`] that `esc` closes — account chrome (plan, quota, referral,
//! billing) is not part of the conversation, and a history cell can never be
//! dismissed. The one exception: a payment that lands *after* the
//! `/upgrade` pane was closed leaves one dim line, `Upgraded to Pro`, because a
//! plan change alters the session's limits.
//!
//! In screen-reader mode there is no pane at all: each command prints one
//! plain labelled cell, because a pane that repaints in place is exactly what a
//! screen reader cannot follow.

use super::*;

use crate::bottom_pane::info_pane_view::BodyRow;
use crate::bottom_pane::info_pane_view::ByteAskPane;
use crate::bottom_pane::info_pane_view::InfoPaneSpec;
use crate::bottom_pane::info_pane_view::PaneAction;
use crate::bottom_pane::info_pane_view::Primary;
use crate::bottom_pane::info_pane_view::close_hint;
use crate::byteask_a11y::glyph;
use crate::byteask_a11y::screen_reader_mode;
use crate::status::format_reset_timestamp;
use crate::status::render_status_limit_progress_bar;
use codex_login::default_client::CodexRequestBuilder;
use codex_login::default_client::create_client;
use rand::Rng as _;
use ratatui::style::Color;
use ratatui::text::Span;
use serde::Deserialize;
use std::time::Duration;

/// Odds the promo banner shows on a fresh session: 1 in N.
const PROMO_BANNER_ODDS: u32 = 12;

/// Every gateway call behind a pane gives up after this long and shows
/// `✗ Couldn't reach ByteAsk · r to retry`. The sign-in wait is
/// the only intentional long-runner, and it does not go through here.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// How long `✓ Copied` replaces the footer before it reverts.
const COPIED_FOOTER: Duration = Duration::from_secs(2);

/// The `/upgrade` link pane polls for the plan change this often…
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// …and keeps polling this long after the pane is closed, so a payment made in
/// a browser still produces the one `Upgraded to …` transcript line.
const UPGRADE_POLL_LIMIT: Duration = Duration::from_secs(15 * 60);

/// A window at or above this share of its budget earns the `Higher limits`
/// row — below it, a readout that mentions upgrading is just an advert.
const UPGRADE_ROW_THRESHOLD: i64 = 50;
/// Usage bars and percentages turn yellow here…
const WARN_PCT: i64 = 75;
/// …and red here.
const DANGER_PCT: i64 = 90;

/// Usage payload from `GET {gateway}/byteask/v1/usage`. The plan/window fields are
/// percentages only; the `extra_*` fields are the credit ("extra usage") pool in
/// dollars (referral + voucher, aggregated) — the one place we render $ amounts.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UsageInfo {
    #[serde(default)]
    plan: String,
    #[serde(default)]
    used_5h_pct: i64,
    #[serde(default)]
    used_week_pct: i64,
    #[serde(default)]
    reset_5h_sec: i64,
    #[serde(default)]
    reset_week_sec: i64,
    #[serde(default)]
    extra_granted_usd: f64,
    #[serde(default)]
    extra_balance_usd: f64,
    /// Credit expires. The SOONEST bucket -- the next amount that lapses
    /// and its last usable day (IST, `YYYY-MM-DD`). Absent on older gateways.
    #[serde(default)]
    extra_expiring_usd: f64,
    #[serde(default)]
    extra_expiry_day: String,
}

/// Success payload from `POST {gateway}/byteask/v1/voucher`.
///
/// A code may grant credit, a plan term, or both. `message` is the gateway's own
/// sentence and is rendered VERBATIM when present: that is what makes every future
/// voucher variant a server-side change reaching installed clients with no rebuild,
/// the same lever as `pro_reward_target` and the catalog's `availability_nux`. The
/// individual fields stay so a client can still format its own line if it wants one.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct VoucherInfo {
    #[serde(default)]
    amount_added_usd: f64,
    #[serde(default)]
    balance_usd: f64,
    /// `""` for an ordinary credit voucher; otherwise `pro` / `max`.
    #[serde(default)]
    plan: String,
    #[serde(default)]
    plan_days: i64,
    #[serde(default)]
    plan_expires_at: i64,
    /// The gateway's own sentence. Empty on an older gateway, which is why the
    /// renderers fall back to the credit line rather than showing nothing.
    #[serde(default)]
    message: String,
}

// NOTE: a plan grant deliberately does NOT try to refresh the model picker here.
// The per-user `locked` set is merged into `models-catalog.json` by the WRAPPER
// before the engine starts (cli/byteask:147 `_models_access_merge`); the engine has
// no client for `/byteask/v1/model-access` and no `locked_reason` plumbing at all in
// this tree, so there is nothing in-process to re-apply. `AppEvent::FetchModels` is
// no help either: it is gated on a live `/model` popup request and re-reads the same
// on-disk catalog. It is also moot today -- every row in `models_registry` is
// `standard`, so `locked` is empty for everyone. When a model is first marked
// premium, this becomes real and the fix belongs at the wrapper boundary.

/// Referral payload from `GET {gateway}/byteask/v1/referral-code`.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ReferralInfo {
    #[serde(default)]
    code: String,
    #[serde(default)]
    share_command: String,
    // The gateway sends a complete, copyable command per install medium. The pane
    // shows all three: this screen exists to be shared from, and half a command
    // ("install, then BYTEASK_REF=... byteask") is something the reader has to
    // finish themselves.
    #[serde(default)]
    share_pip: String,
    #[serde(default)]
    share_npm: String,
    // Free-Pro-month reward progress (gateway-provided; 0/absent on old gateways).
    #[serde(default)]
    successful_referrals: u32,
    #[serde(default)]
    pro_reward_target: u32,
    #[serde(default)]
    pro_reward_granted: bool,
}

/// Success payload from `POST {gateway}/byteask/v1/upgrade`.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UpgradeInfo {
    #[serde(default)]
    pay_url: String,
}

/// Error payload `{"error":{"message":"..."}}` from the upgrade endpoint.
#[derive(Debug, Default, Deserialize)]
struct UpgradeError {
    #[serde(default)]
    error: UpgradeErrorBody,
}

#[derive(Debug, Default, Deserialize)]
struct UpgradeErrorBody {
    #[serde(default)]
    message: String,
}

/// What a gateway call came back with. Panes render each arm differently, so the
/// *permanence* of a failure has to survive the request boundary: a 401 is a
/// sign-in problem, a 5xx is worth retrying, and a 4xx carries a sentence the
/// gateway wants shown verbatim.
#[derive(Debug, Clone)]
pub(crate) enum Fetched<T> {
    Ok(T),
    /// HTTP 401 — the ByteAsk session expired.
    Expired,
    /// HTTP 5xx.
    ServerError,
    /// A 4xx with a message meant for the user (a rule, not a breakage:
    /// `You're already on Pro or higher.`).
    Message(String),
    /// A 5xx that still carried a message (`Payment provider unavailable.`).
    /// Rendered like [`Self::Message`], but panes may offer support on it,
    /// because nothing the user types will fix it.
    ServerMessage(String),
    /// Connection failure, unparseable body, or the 8 s timeout.
    Unreachable,
}

/// A pane's network result, routed back to the UI thread by pane id. A result
/// whose pane has already closed is dropped — except
/// [`Self::UpgradePoll`], which is what produces the `Upgraded to …` transcript line.
#[derive(Debug)]
pub(crate) enum PaneOutcome {
    Usage(Fetched<UsageInfo>),
    /// The `/upgrade` pane's own plan probe: it needs to know which plan the
    /// user is on before it can draw the rows.
    UpgradePlan(Fetched<UsageInfo>),
    Referral(Fetched<ReferralInfo>),
    Upgrade {
        plan: String,
        result: Fetched<UpgradeInfo>,
    },
    /// The 5 s poll behind an open payment link saw the plan change.
    UpgradePoll {
        plan: String,
    },
    Voucher {
        code: String,
        result: Fetched<VoucherInfo>,
    },
    /// The device code arrived: swap the "requesting" pane for the one the user
    /// reads the URL and code off.
    SubscriptionCode {
        url: String,
        code: String,
    },
    /// The sign-in finished. `Err` carries the message to show, including the
    /// plan refusal raised before anything was stored.
    SubscriptionDone {
        error: Option<String>,
    },
}

impl ChatWidget {
    /// Open a ByteAsk info pane. Nothing here ever writes to history.
    fn open_byteask_pane(&mut self, spec: InfoPaneSpec) {
        self.bottom_pane.show_info_pane(spec);
    }

    /// Replace an open pane in place; a result whose pane has closed is dropped.
    fn update_byteask_pane(&mut self, spec: InfoPaneSpec) -> bool {
        self.bottom_pane.update_info_pane(spec)
    }

    fn byteask_pane_open(&self, pane: ByteAskPane) -> bool {
        self.bottom_pane.has_view_with_id(pane.view_id())
    }

    fn byteask_animations(&self) -> bool {
        self.config.animations
    }

    /// `/usage`: plan, both windows, credit and — only when it is earned — the
    /// upgrade row. Opens a pane; the gateway answer replaces it in place.
    pub(super) fn show_byteask_usage(&mut self) {
        let Some((base, bearer)) = self.byteask_gateway() else {
            // No bearer configured at all: nobody is signed in (or the session
            // runs on the user's own key), so there is no account to report on.
            if screen_reader_mode() {
                self.add_to_history(labelled_cell("usage", USAGE_SIGNED_OUT));
            } else {
                self.open_byteask_pane(usage_signed_out_spec(self.byteask_animations()));
            }
            return;
        };
        if !screen_reader_mode() {
            self.open_byteask_pane(usage_loading_spec(self.byteask_animations()));
        }
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_usage(&base, &bearer).await;
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::Usage(result)));
        });
    }

    /// `/referral-code`: the code plus the two lines a friend can run. `enter`
    /// copies the curl line.
    pub(super) fn show_byteask_referral(&mut self) {
        let Some((base, bearer)) = self.byteask_gateway() else {
            if screen_reader_mode() {
                self.add_to_history(labelled_cell("referral code", REFERRAL_SIGNED_OUT));
            } else {
                self.open_byteask_pane(referral_signed_out_spec(self.byteask_animations()));
            }
            return;
        };
        if !screen_reader_mode() {
            self.open_byteask_pane(referral_loading_spec(self.byteask_animations()));
        }
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = fetch_referral(&base, &bearer).await;
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::Referral(result)));
        });
    }

    /// `/upgrade [pro|max|ultra]`: bare opens the plan picker (after a probe for the
    /// current plan); an explicit plan preselects it and requests the link now.
    pub(super) fn show_byteask_upgrade(&mut self, plan_arg: String) {
        let plan = plan_arg.trim().to_lowercase();
        let requested = matches!(plan.as_str(), "pro" | "max" | "ultra").then_some(plan);
        let Some((base, bearer)) = self.byteask_gateway() else {
            if screen_reader_mode() {
                self.add_to_history(labelled_cell("upgrade", UPGRADE_SIGNED_OUT));
            } else {
                self.open_byteask_pane(upgrade_signed_out_spec(self.byteask_animations()));
            }
            return;
        };
        match requested {
            Some(plan) => {
                if !screen_reader_mode() {
                    self.open_byteask_pane(upgrade_link_loading_spec(self.byteask_animations()));
                }
                self.request_byteask_upgrade(base, bearer, plan);
            }
            None => {
                if !screen_reader_mode() {
                    self.open_byteask_pane(upgrade_loading_spec(self.byteask_animations()));
                }
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let result = fetch_usage(&base, &bearer).await;
                    tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::UpgradePlan(
                        result,
                    )));
                });
            }
        }
    }

    fn request_byteask_upgrade(&mut self, base: String, bearer: String, plan: String) {
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = start_upgrade(&base, &bearer, &plan).await;
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::Upgrade {
                plan,
                result,
            }));
        });
    }

    /// `/voucher [code]`: the redemption form. A code on the command line
    /// submits as the pane opens.
    pub(crate) fn show_byteask_voucher(&mut self, code_arg: String) {
        let code = code_arg.trim().to_string();
        let Some((base, bearer)) = self.byteask_gateway() else {
            if screen_reader_mode() {
                self.add_to_history(labelled_cell("voucher", VOUCHER_SIGNED_OUT));
            } else {
                self.open_byteask_pane(voucher_signed_out_spec(self.byteask_animations()));
            }
            return;
        };
        if code.is_empty() {
            self.open_byteask_pane(voucher_form_spec(
                String::new(),
                /*error*/ None,
                self.byteask_animations(),
            ));
            return;
        }
        if !screen_reader_mode() {
            self.open_byteask_pane(voucher_loading_spec(self.byteask_animations()));
        }
        self.submit_byteask_voucher(base, bearer, code);
    }

    fn submit_byteask_voucher(&mut self, base: String, bearer: String, code: String) {
        let code = code.to_uppercase();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = submit_voucher(&base, &bearer, &code).await;
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::Voucher {
                code,
                result,
            }));
        });
    }

    /// A key pressed inside an info pane. `enter` performs exactly the action
    /// the footer named; nothing here closes a pane.
    pub(crate) fn handle_byteask_pane_action(&mut self, pane: ByteAskPane, action: PaneAction) {
        match action {
            // The app performs copies (it owns the clipboard worker); see `finish_byteask_pane_copy`.
            PaneAction::Copy(_) => {}
            PaneAction::Choose(plan) => {
                if pane == ByteAskPane::Subscription {
                    // The refusal screen. Retry starts a fresh sign-in in place;
                    // Back closes the pane and leaves the session exactly as it was
                    // (the refused attempt stored nothing).
                    if plan == SUBSCRIPTION_RETRY_ID {
                        self.start_byteask_subscription_login();
                    } else {
                        self.bottom_pane
                            .dismiss_view_by_id(ByteAskPane::Subscription.view_id());
                    }
                } else if pane == ByteAskPane::Upgrade {
                    if plan == UPGRADE_REDEEM_ID {
                        // OPEN, not update. `update_info_pane` looks for a view whose
                        // id matches the NEW spec (bottom_pane/mod.rs:657), so asking
                        // it to turn an Upgrade pane into a Voucher pane finds nothing
                        // and silently does nothing -- enter would appear dead.
                        // `push_view` already pops any open info pane before pushing
                        // (ByteAsk panes never stack), so this REPLACES the
                        // picker and the user is still one `esc` from the transcript.
                        self.open_byteask_pane(voucher_form_spec(
                            String::new(),
                            /*error*/ None,
                            self.byteask_animations(),
                        ));
                    } else if let Some((base, bearer)) = self.byteask_gateway() {
                        self.update_byteask_pane(upgrade_link_loading_spec(
                            self.byteask_animations(),
                        ));
                        self.request_byteask_upgrade(base, bearer, plan);
                    }
                }
            }
            PaneAction::Submit(text) => {
                if pane == ByteAskPane::Voucher
                    && let Some((base, bearer)) = self.byteask_gateway()
                {
                    self.update_byteask_pane(voucher_loading_spec(self.byteask_animations()));
                    self.submit_byteask_voucher(base, bearer, text);
                }
            }
            PaneAction::Retry => self.retry_byteask_pane(pane),
        }
    }

    /// Start (or restart) the ChatGPT-subscription sign-in, in this pane.
    ///
    /// Runs the device-code flow IN PROCESS via codex-login rather than shelling out
    /// to `"$ENGINE" login`. A subprocess writes to the terminal, and anything written
    /// there outlives every pane - which is why the old flow left a screenful of text
    /// behind and had no way back.
    pub(super) fn start_byteask_subscription_login(&mut self) {
        let animations = self.byteask_animations();
        self.open_byteask_pane(subscription_starting_spec(animations));

        if tokio::runtime::Handle::try_current().is_err() {
            self.update_byteask_pane(subscription_failed_spec(
                "Sign-in is unavailable in this session.",
                animations,
            ));
            return;
        }

        let opts = codex_login::ServerOptions::new(
            self.config.codex_home.to_path_buf(),
            codex_login::CLIENT_ID.to_string(),
            self.config.auth_config().effective_chatgpt_workspaces(),
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
            self.config.auth_route_config(),
        );
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let device_code = match codex_login::request_device_code(&opts).await {
                Ok(code) => code,
                Err(err) => {
                    tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::SubscriptionDone {
                        error: Some(format!("Could not start sign-in: {err}")),
                    }));
                    return;
                }
            };
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::SubscriptionCode {
                url: device_code.verification_url.clone(),
                code: device_code.user_code.clone(),
            }));
            // Polls until the user finishes, the code expires, or the plan gate
            // refuses. The gate runs before anything is persisted, so a refusal here
            // leaves no credential behind and "Try a different account" is clean.
            let error = codex_login::complete_device_code_login(opts, device_code)
                .await
                .err()
                .map(|err| err.to_string());
            tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::SubscriptionDone {
                error,
            }));
        });
    }

    fn retry_byteask_pane(&mut self, pane: ByteAskPane) {
        match pane {
            ByteAskPane::Usage => self.show_byteask_usage(),
            ByteAskPane::Referral => self.show_byteask_referral(),
            ByteAskPane::Upgrade => self.show_byteask_upgrade(String::new()),
            ByteAskPane::Voucher => {
                // `r` is never bound while the voucher field has focus, so a
                // retry here can only mean "open the empty form again".
                self.show_byteask_voucher(String::new());
            }
            // "Try a different account": start a fresh sign-in in place. The
            // previous attempt stored nothing (the plan gate runs before persist),
            // so there is no state to unwind first.
            ByteAskPane::Subscription => self.start_byteask_subscription_login(),
        }
    }

    /// Show the result of copying a pane's underlying string (the app performs the copy
    /// through upstream's clipboard worker, never with the rendered row).
    pub(crate) fn finish_byteask_pane_copy(
        &mut self,
        pane: ByteAskPane,
        result: crate::clipboard_copy::worker::CopyResult,
    ) {
        let footer = match result {
            Ok(crate::clipboard_copy::CopyStatus::Busy) => vec![
                format!("{} Copy already in progress", glyph("…")),
                close_hint(),
            ],
            Ok(_) => vec![format!("{} Copied", glyph("✓")), close_hint()],
            // The value stays on screen, so the fallback is a real instruction
            // rather than an apology.
            Err(_) => vec![
                format!("{} Couldn't copy", glyph("✗")),
                "select the line with the mouse".to_string(),
                close_hint(),
            ],
        };
        self.bottom_pane
            .set_info_pane_transient_footer(pane, footer, COPIED_FOOTER);
    }

    /// Deliver a gateway answer to its pane. Dropped when that pane is gone.
    pub(crate) fn handle_byteask_pane_result(&mut self, outcome: PaneOutcome) {
        let animations = self.byteask_animations();
        match outcome {
            PaneOutcome::Usage(result) => {
                if screen_reader_mode() {
                    self.add_to_history(labelled_cell("usage", &usage_label(&result)));
                    return;
                }
                self.update_byteask_pane(usage_spec(&result, animations));
            }
            PaneOutcome::UpgradePlan(result) => {
                let plan = match &result {
                    Fetched::Ok(info) => display_plan(&info.plan),
                    // A probe that fails must not block the sale: fall back to
                    // the full picker rather than an error the user cannot act on.
                    _ => String::new(),
                };
                if screen_reader_mode() {
                    self.add_to_history(labelled_cell("upgrade", UPGRADE_SCREEN_READER));
                    return;
                }
                self.update_byteask_pane(upgrade_options_spec(&plan, animations));
            }
            PaneOutcome::Referral(result) => {
                if screen_reader_mode() {
                    self.add_to_history(labelled_cell("referral code", &referral_label(&result)));
                    return;
                }
                self.update_byteask_pane(referral_result_spec(&result, animations));
            }
            PaneOutcome::SubscriptionCode { url, code } => {
                self.update_byteask_pane(subscription_code_spec(&url, &code, animations));
            }
            PaneOutcome::SubscriptionDone { error } => match error {
                Some(message) => {
                    self.update_byteask_pane(subscription_failed_spec(&message, animations));
                }
                // Signed in and the plan is usable. The engine cannot swap the
                // startup-loaded provider itself, so the wrapper finishes: it writes
                // config.toml and relaunches. It prints nothing - the sign-in already
                // happened here, so there is no second screen.
                None => self.request_byteask_auth_with("login", "subscription-ready chatgpt"),
            },
            PaneOutcome::Upgrade { plan, result } => {
                if matches!(result, Fetched::Ok(_)) {
                    // The pane rewrites itself when the plan changes; the poll
                    // outlives the pane so a payment made after `esc` still
                    // leaves the one `Upgraded to …` line.
                    if let Some((base, bearer)) = self.byteask_gateway() {
                        self.spawn_byteask_upgrade_poll(base, bearer, plan.clone());
                    }
                }
                if screen_reader_mode() {
                    self.add_to_history(labelled_cell("upgrade", &upgrade_label(&plan, &result)));
                    return;
                }
                self.update_byteask_pane(upgrade_result_spec(&plan, &result, animations));
            }
            PaneOutcome::UpgradePoll { plan } => {
                let label = plan_label_price(&plan).0;
                if self.byteask_pane_open(ByteAskPane::Upgrade) && !screen_reader_mode() {
                    self.update_byteask_pane(upgrade_paid_spec(label, animations));
                } else {
                    // The only line any of these four commands may write.
                    self.add_to_history(history_cell::new_info_event(
                        format!("Upgraded to {label}"),
                        /*hint*/ None,
                    ));
                }
            }
            PaneOutcome::Voucher { code, result } => {
                if screen_reader_mode() {
                    self.add_to_history(labelled_cell("voucher", &voucher_label(&code, &result)));
                    return;
                }
                self.update_byteask_pane(voucher_result_spec(&code, &result, animations));
            }
        }
    }

    fn spawn_byteask_upgrade_poll(&self, base: String, bearer: String, plan: String) {
        if cfg!(test) || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + UPGRADE_POLL_LIMIT;
            loop {
                tokio::time::sleep(UPGRADE_POLL_INTERVAL).await;
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                if let Some(info) = try_fetch_usage(&base, &bearer).await
                    && info.plan.trim().eq_ignore_ascii_case(&plan)
                {
                    tx.send(AppEvent::ByteAskPaneResult(PaneOutcome::UpgradePoll {
                        plan,
                    }));
                    return;
                }
            }
        });
    }

    /// Fire-and-forget: POST an in-chat session-feedback rating (1=bad, 2=fine,
    /// 3=good) to the gateway. No history cell and no user-visible error — a rating
    /// is best-effort telemetry, so a failed/absent gateway is silently ignored.
    /// No-op when not signed in to a gateway account.
    pub(crate) fn submit_byteask_feedback(&self, rating: u8, model: String) {
        let Some((base, bearer)) = self.byteask_gateway() else {
            return;
        };
        tokio::spawn(async move {
            post_feedback(base, bearer, rating, model).await;
        });
    }

    /// Occasionally drop a one-line, easy-to-scroll-past referral nudge into the
    /// console at session start. Never fires under test (so snapshots stay
    /// deterministic) and only ~1 in [`PROMO_BANNER_ODDS`] sessions otherwise.
    pub(super) fn maybe_emit_referral_promo_banner(&self) {
        if cfg!(test) {
            return;
        }
        if rand::rng().random_range(0..PROMO_BANNER_ODDS) != 0 {
            return;
        }
        self.app_event_tx.send(AppEvent::InsertHistoryCell(
            Box::new(referral_banner_cell()),
        ));
    }

    /// The ByteAsk gateway base URL + per-user bearer token, if we're signed in
    /// to a gateway-backed account. Resolved via [`Config::byteask_identity`],
    /// which reads the identity independently of the active provider, so this
    /// keeps working in a ChatGPT-subscription or BYOK session (where the active
    /// provider is `openai`/`byok-local`, not `byteask`).
    pub(super) fn byteask_gateway(&self) -> Option<(String, String)> {
        self.config
            .byteask_identity()
            .map(|(base, bearer)| (base, bearer.into_inner()))
    }

    /// `/login` and `/logout` are owned by the `byteask` wrapper: it holds the
    /// magic-link sign-in flow and the mode-C `experimental_bearer_token` in
    /// config.toml (which the engine's own logout never touches — that only
    /// clears the OAuth `auth.json`). The engine can't hot-swap the
    /// startup-loaded provider token, so the TUI drops a one-word marker the
    /// wrapper reads after we exit; the wrapper then (re)authenticates and
    /// relaunches us. `action` is `"login"` or `"logout"`.
    pub(super) fn request_byteask_auth(&self, action: &str) {
        self.request_byteask_auth_with(action, /*detail*/ "");
    }

    /// Same hand-off, plus a second marker line telling a current wrapper what to
    /// do without showing its own screen (`relaunch`, `add-provider <id>`, …).
    ///
    /// The verb on line 1 stays `login`/`logout` on purpose. The wrapper matches
    /// it with `case "$_act" in login*)` and **exits outright on anything it does
    /// not recognise**, so an engine newer than the installed wrapper must still
    /// lead with a known verb: it then falls back to the old interactive screen
    /// (degraded, but working) instead of quitting on the user.
    pub(super) fn request_byteask_auth_with(&self, action: &str, detail: &str) {
        let path = self.config.codex_home.join(".byteask-auth-request");
        let body = if detail.is_empty() {
            format!("{action}\n")
        } else {
            format!("{action}\n{detail}\n")
        };
        if let Err(e) = std::fs::write(&path, body) {
            tracing::warn!("byteask: could not write auth-request marker: {e}");
        }
        self.app_event_tx
            .send(AppEvent::Exit(crate::app_event::ExitMode::ShutdownFirst));
    }

    /// Fetch the weekly usage % once and publish it into `slot` (read each render
    /// by the composer's rotating footer). Fire-and-forget; no-op when not on a
    /// runtime or not signed in to a gateway.
    pub(super) fn spawn_byteask_usage_probe(
        &self,
        slot: std::sync::Arc<std::sync::atomic::AtomicI64>,
    ) {
        let Some((base, bearer)) = self.byteask_gateway() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            if let Some(info) = try_fetch_usage(&base, &bearer).await {
                slot.store(
                    info.used_week_pct.clamp(0, 100),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        });
    }
}

/// Classify a response the way the panes need it: permanence first, body second.
/// Every call is wrapped in [`REQUEST_TIMEOUT`], so "the gateway never answered"
/// and "the gateway refused" can never be confused for each other.
async fn fetch_json<T: serde::de::DeserializeOwned>(request: CodexRequestBuilder) -> Fetched<T> {
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        _ => return Fetched::Unreachable,
    };
    let status = response.status();
    if status.is_success() {
        return match tokio::time::timeout(REQUEST_TIMEOUT, response.json::<T>()).await {
            Ok(Ok(value)) => Fetched::Ok(value),
            _ => Fetched::Unreachable,
        };
    }
    if status.as_u16() == 401 {
        return Fetched::Expired;
    }
    // Read the body on EVERY status, 5xx included. The gateway answers a failed
    // checkout with 502 + `payment provider unavailable`, which is the only thing
    // that tells the user what actually went wrong; returning ServerError here
    // replaced it with "ByteAsk returned an error" and lost the explanation.
    let message = match tokio::time::timeout(REQUEST_TIMEOUT, response.json::<UpgradeError>()).await
    {
        Ok(Ok(body)) => body.error.message.trim().to_string(),
        _ => String::new(),
    };
    match (message.is_empty(), status.is_server_error()) {
        (true, _) => Fetched::ServerError,
        (false, true) => Fetched::ServerMessage(as_sentence(&message)),
        (false, false) => Fetched::Message(as_sentence(&message)),
    }
}

/// The gateway's user-facing strings are sentences (`You're already on Pro or
/// higher.`) but a few are fragments (`payment provider unavailable`). The copy rules
/// want a sentence, so capitalise the first letter and close it — never touching
/// a message that already starts with punctuation, such as a backticked model id.
fn as_sentence(message: &str) -> String {
    let message = message.trim();
    let mut out = String::with_capacity(message.len() + 1);
    let mut chars = message.chars();
    if let Some(first) = chars.next() {
        if first.is_ascii_lowercase() {
            out.extend(first.to_uppercase());
        } else {
            out.push(first);
        }
        out.push_str(chars.as_str());
    }
    if !out.is_empty() && !out.ends_with(['.', '!', '?', ':']) {
        out.push('.');
    }
    out
}

async fn fetch_usage(base: &str, bearer: &str) -> Fetched<UsageInfo> {
    fetch_json(
        create_client()
            .get(usage_url(base))
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Accept", "application/json"),
    )
    .await
}

async fn try_fetch_usage(base: &str, bearer: &str) -> Option<UsageInfo> {
    let response = create_client()
        .get(usage_url(base))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<UsageInfo>().await.ok()
}

async fn fetch_referral(base: &str, bearer: &str) -> Fetched<ReferralInfo> {
    match fetch_json::<ReferralInfo>(
        create_client()
            .get(referral_url(base))
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Accept", "application/json"),
    )
    .await
    {
        // A 200 with no code is not a code: treat it as unreachable so the pane
        // offers `r` instead of rendering an empty accent row.
        Fetched::Ok(info) if info.code.trim().is_empty() => Fetched::Unreachable,
        other => other,
    }
}

/// Start a plan upgrade: POST the plan and hand back the payment link, or the
/// reason there isn't one.
async fn start_upgrade(base: &str, bearer: &str, plan: &str) -> Fetched<UpgradeInfo> {
    match fetch_json::<UpgradeInfo>(
        create_client()
            .post(upgrade_url(base))
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "plan": plan })),
    )
    .await
    {
        Fetched::Ok(info) if info.pay_url.trim().is_empty() => Fetched::Unreachable,
        other => other,
    }
}

/// Redeem a voucher: POST the code and hand back the top-up, or the gateway's
/// own sentence ("already used", "reached its limit").
async fn submit_voucher(base: &str, bearer: &str, code: &str) -> Fetched<VoucherInfo> {
    fetch_json::<VoucherInfo>(
        create_client()
            .post(voucher_url(base))
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Accept", "application/json")
            // Tells the gateway this build can render a plan-granting code. Its
            // ABSENCE is the signal: every engine shipped before this one sent no
            // version at all and renders a redemption through a hardcoded credit
            // template, so the gateway refuses plan codes for those clients with a
            // sentence they *can* show instead of printing "$0.00 of credit" over a
            // plan that really switched on. There is no way to add this header to a
            // binary already on someone's laptop, which is the whole point.
            .header("X-Byteask-Engine", env!("CARGO_PKG_VERSION"))
            .json(&serde_json::json!({ "code": code })),
    )
    .await
}

/// Endpoint URLs: take the provider base, strip a trailing `/v1`, then append
/// `/v1/<name>` — so both `.../byteask/v1` and `.../byteask` resolve correctly
/// (same shape as the `cpp_intrinsic` lookup URL).
fn usage_url(base: &str) -> String {
    format!("{}/v1/usage", gateway_origin(base))
}

fn referral_url(base: &str) -> String {
    format!("{}/v1/referral-code", gateway_origin(base))
}

fn upgrade_url(base: &str) -> String {
    format!("{}/v1/upgrade", gateway_origin(base))
}

fn voucher_url(base: &str) -> String {
    format!("{}/v1/voucher", gateway_origin(base))
}

fn feedback_url(base: &str) -> String {
    format!("{}/v1/feedback", gateway_origin(base))
}

/// POST a session-feedback rating; best-effort, result ignored (never surfaced).
async fn post_feedback(base: String, bearer: String, rating: u8, model: String) {
    let _ = create_client()
        .post(feedback_url(&base))
        .header("Authorization", format!("Bearer {bearer}"))
        .json(&serde_json::json!({ "rating": rating, "model": model }))
        .send()
        .await;
}

/// `("Pro", 20)` / `("Max", 100)` / `("Ultra", 200)` for a validated plan; defaults to Pro pricing.
fn plan_label_price(plan: &str) -> (&'static str, u32) {
    match plan {
        "max" => ("Max", 100),
        "ultra" => ("Ultra", 200),
        _ => ("Pro", 20),
    }
}

/// The plan's usage limits in dollars of model use, per rolling 5-hour window and per week:
/// the gateway's plan table (0.2.0). Shown instead of multiples of Free, which drift.
fn plan_limits(plan: &str) -> (u32, u32) {
    match plan {
        "max" => (10, 30),
        "ultra" => (20, 60),
        _ => (4, 8),
    }
}

fn gateway_origin(base: &str) -> String {
    let base = base.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_string()
}

// ---------------------------------------------------------------------------
// Pane copy and frames. Each builder is a pure function of the
// gateway's answer, which is what makes every state snapshot-testable.
// ---------------------------------------------------------------------------

/// `/usage` with no bearer at all: there is no account to report on.
const USAGE_SIGNED_OUT: &str = "Sign in with /login for plan limits.";
const REFERRAL_SIGNED_OUT: &str = "Sign in with /login to get a referral code.";
const UPGRADE_SIGNED_OUT: &str = "Sign in with /login to upgrade.";
const UPGRADE_SCREEN_READER: &str = "Run /upgrade pro or /upgrade max for a payment link.";
const VOUCHER_SIGNED_OUT: &str = "Sign in with /login to redeem a voucher.";
const SESSION_EXPIRED: &str = "Your session expired. Sign in with /login.";
const UNREACHABLE: &str = "Couldn't reach ByteAsk";
/// Every way a payment link can fail to be created says exactly this. The cause is
/// ours, never the user's, and naming it (`Payment provider unavailable`) tells
/// someone who wants to pay us nothing they can act on.
const UPGRADE_FAILED: &str = "Payment failed. Contact founders@byteask.ai";
const SERVER_ERROR: &str = "ByteAsk returned an error";
const RETRY_HINT: &str = "r to retry";

fn error_row(message: &str, retryable: bool) -> BodyRow {
    let mut spans = vec![format!("{} ", glyph("✗")).red(), message.to_string().into()];
    if retryable {
        spans.push(format!(" {} {RETRY_HINT}", glyph("·")).dim());
    }
    BodyRow::Text(Line::from(spans))
}

/// The body row for a failure that is not `Fetched::Ok`, plus whether `r` is
/// bound and what the pane's tag becomes.
fn failure_rows<T>(result: &Fetched<T>) -> Option<(BodyRow, bool, Option<&'static str>)> {
    match result {
        Fetched::Ok(_) => None,
        Fetched::Expired => Some((error_row(SESSION_EXPIRED, false), false, Some("signed out"))),
        Fetched::ServerError => Some((error_row(SERVER_ERROR, true), true, None)),
        Fetched::Message(message) => Some((error_row(message, false), false, None)),
        Fetched::ServerMessage(message) => Some((error_row(message, true), true, None)),
        Fetched::Unreachable => Some((error_row(UNREACHABLE, true), true, None)),
    }
}

// --- /usage ----------------------------------------------------------------

/// Usage bodies are at most 5 rows (both windows, credit, blank, upgrade); the
/// loading state reserves 4, which is the height of every state a user without
/// credit will ever see.
const USAGE_RESERVED_ROWS: u16 = 4;

// ---------------------------------------------------------------------------
// ChatGPT-subscription sign-in, drawn as a pane.
//
// This used to leave the TUI: the engine exited, the wrapper ran `"$ENGINE" login`
// as a subprocess, and everything it printed - the requirement line, the URL, the
// code, the result - landed in scrollback, where no pane can clean it up. There was
// also no way back: a refusal ended the process and dropped the user at their shell.
//
// It is a pane now, like every other /login screen: `esc` closes, a refusal offers
// "Try a different account", and nothing survives on screen. The login itself runs
// in-process through codex-login, so there is no subprocess to print anything.
// ---------------------------------------------------------------------------

/// Rows reserved so the pane does not resize under the user when the code arrives.
const SUBSCRIPTION_RESERVED_ROWS: u16 = 9;
const SUBSCRIPTION_RETRY_ID: &str = "subscription_retry";
const SUBSCRIPTION_BACK_ID: &str = "subscription_back";

fn subscription_title() -> Vec<Span<'static>> {
    vec!["Sign in with your ChatGPT subscription".bold()]
}

fn subscription_starting_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Subscription, subscription_title());
    spec.body = vec![BodyRow::Loading("Requesting a one-time code".to_string())];
    spec.reserve_rows = SUBSCRIPTION_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

/// The screen the user acts on. Same wording as the CLI's device-code screen
/// (login/src/device_code_auth.rs) so the two surfaces read alike.
fn subscription_code_spec(url: &str, code: &str, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Subscription, subscription_title());
    spec.body = vec![
        BodyRow::Text("1. Open this link in your browser and sign in".into()),
        BodyRow::Text(Line::from(vec![
            "   ".into(),
            url.to_string()
                .fg(crate::style::accent_color())
                .underlined(),
        ])),
        BodyRow::Blank,
        BodyRow::Text("2. Enter this one-time code (expires in 15 minutes)".into()),
        BodyRow::Text(Line::from(vec![
            "   ".into(),
            code.to_string().bold().fg(crate::style::accent_color()),
        ])),
        BodyRow::Blank,
        BodyRow::Text(
            "Continue only if you started this sign-in in ByteAsk."
                .dim()
                .into(),
        ),
    ];
    spec.footer = vec!["enter to copy the code".to_string()];
    spec.primary = Primary::Copy(code.to_string());
    spec.reserve_rows = SUBSCRIPTION_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

/// A refusal the user can act on, instead of a process that exits.
///
/// The plan gate runs BEFORE the credential is stored (login/src/byteask_plan_gate.rs),
/// so "Try a different account" has nothing to undo: no token was written.
fn subscription_failed_spec(message: &str, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Subscription, subscription_title());
    // Wrapped to the pane frame like every other note. The refusal arrives as one
    // long sentence from the login crate, and an unwrapped line runs off the frame.
    let mut body: Vec<BodyRow> = wrap_note(message)
        .into_iter()
        .map(|line| BodyRow::Text(line.into()))
        .collect();
    body.push(BodyRow::Blank);
    body.push(BodyRow::Choice {
        id: SUBSCRIPTION_RETRY_ID.to_string(),
        line: "Try a different account".into(),
        selectable: true,
    });
    body.push(BodyRow::Choice {
        id: SUBSCRIPTION_BACK_ID.to_string(),
        line: "Back".into(),
        selectable: true,
    });
    spec.body = body;
    // Without this, `enter` is dead: Primary defaults to None, and only
    // `Primary::Choose` submits the selected Choice row. The rows rendered and the
    // cursor moved, so the screen looked finished while doing nothing.
    spec.primary = Primary::Choose;
    spec.footer = vec!["enter to select".to_string()];
    spec.reserve_rows = SUBSCRIPTION_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

fn usage_loading_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    spec.body = vec![BodyRow::Loading("Loading usage".to_string())];
    spec.reserve_rows = USAGE_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

/// `Usage · Pro plan`. The plan leads the title in bold accent: it used to be a dim
/// right-aligned tag, which read as an afterthought and is the first thing a
/// narrow pane drops.
fn usage_title(plan_label: &str) -> Vec<Span<'static>> {
    vec![
        "Usage".bold(),
        format!(" {} ", glyph("·")).into(),
        plan_label
            .to_string()
            .bold()
            .fg(crate::style::accent_color()),
    ]
}

fn usage_signed_out_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    spec.body = vec![BodyRow::Text(Line::from(USAGE_SIGNED_OUT))];
    spec.animations = animations;
    spec
}

fn usage_spec(result: &Fetched<UsageInfo>, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Usage, vec!["Usage".bold()]);
    spec.animations = animations;
    if let Some((row, retry, tag)) = failure_rows(result) {
        spec.tag = tag.unwrap_or_default().to_string();
        spec.body = vec![row];
        spec.retry = retry;
        return spec;
    }
    let Fetched::Ok(info) = result else {
        return spec;
    };
    spec.title = usage_title(&format!("{} plan", display_plan(&info.plan)));
    let mut body = vec![
        usage_window_row("5-hour", info.used_5h_pct, info.reset_5h_sec),
        usage_window_row("Weekly", info.used_week_pct, info.reset_week_sec),
    ];
    // The Extra row appears only when credit was ever granted — an account that
    // has never had any does not need a row saying so.
    if info.extra_granted_usd > 0.0 {
        body.push(BodyRow::Text(Line::from(format!(
            "{:<8}${:.2} left of ${:.2} {} covers spend past your limits",
            "Extra",
            info.extra_balance_usd,
            info.extra_granted_usd,
            glyph("·"),
        ))));
        if let Some(day) = expiry_label(&info.extra_expiry_day) {
            if info.extra_expiring_usd > 0.0 {
                body.push(BodyRow::Text(Line::from(format!(
                    "{:<8}${:.2} expires after {day}",
                    "", info.extra_expiring_usd,
                ))));
            }
        }
    }
    let highest = info.used_5h_pct.max(info.used_week_pct);
    if highest >= UPGRADE_ROW_THRESHOLD && !info.plan.trim().eq_ignore_ascii_case("ultra") {
        body.push(BodyRow::Blank);
        body.push(BodyRow::Text(Line::from("Higher limits with /upgrade")));
    }
    spec.body = body;
    spec
}

/// One window row: `Label   <bar> NN%   resets in 4h 59m (03:06)`.
///
/// The bar is upstream's [`render_status_limit_progress_bar`] (20 segments,
/// `█`/`░`). Its parameter is named `percent_remaining` because upstream fills
/// the bar with what is LEFT; ByteAsk's bars fill with what is SPENT (1 %
/// used draws an empty bar, 82 % used draws 16 of 20), so the used percentage is
/// what goes in. The percentage is always printed as text, so colour is never
/// the only channel.
fn usage_window_row(label: &str, pct: i64, reset_sec: i64) -> BodyRow {
    let pct = pct.clamp(0, 100);
    let bar = usage_bar(pct);
    let filled = bar.chars().filter(|c| *c != '░' && *c != '-').count();
    let (filled_part, empty_part): (String, String) = {
        let chars: Vec<char> = bar.chars().collect();
        (
            chars.iter().take(filled).collect(),
            chars.iter().skip(filled).collect(),
        )
    };
    let colour = usage_colour(pct);
    let filled_span = match colour {
        Some(colour) => filled_part.fg(colour),
        None => filled_part.fg(accent_color()),
    };
    let pct_text = format!("{pct:>3}%");
    let pct_span = match colour {
        Some(colour) => pct_text.fg(colour),
        None => pct_text.into(),
    };
    let mut spans: Vec<Span<'static>> = vec![
        format!("{label:<8}").into(),
        filled_span,
        empty_part.dim(),
        " ".into(),
        pct_span,
    ];
    // The gateway sends 0 when the window has not started yet: nothing has been
    // spent, so there is nothing to reset. "resets in now" is worse than silence
    // -- the bar and the percentage already say the window is empty.
    if reset_sec > 0 {
        spans.push("   ".into());
        spans.push(format!("resets in {}", human_reset(reset_sec)).dim());
    }
    BodyRow::Text(Line::from(spans))
}

/// Yellow from 75 %, red from 90 % — semantic colour, never the accent.
fn usage_colour(pct: i64) -> Option<Color> {
    if pct >= DANGER_PCT {
        Some(Color::Red)
    } else if pct >= WARN_PCT {
        Some(Color::Yellow)
    } else {
        None
    }
}

/// Upstream's bar without its brackets, capped at 19 of 20 segments until the
/// window is actually spent — a 99 % bar that looks full is a lie the user acts on.
fn usage_bar(pct: i64) -> String {
    let raw = render_status_limit_progress_bar(pct as f64);
    let bar = raw.trim_start_matches('[').trim_end_matches(']');
    let mut chars: Vec<char> = bar.chars().collect();
    if pct < 100
        && let Some(last) = chars.last_mut()
        && *last == '█'
    {
        *last = '░';
    }
    let bar: String = chars.into_iter().collect();
    if crate::byteask_a11y::ascii_glyphs() {
        bar.replace('█', glyph("█")).replace('░', glyph("░"))
    } else {
        bar
    }
}

// --- /referral-code (80 columns) -------------------------------------------

/// The referral body is 6 rows (code, blank, sentence, blank, curl, pip/npm).
const REFERRAL_RESERVED_ROWS: u16 = 6;

fn referral_loading_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.body = vec![BodyRow::Loading("Loading your referral code".to_string())];
    spec.reserve_rows = REFERRAL_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

fn referral_signed_out_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.body = vec![BodyRow::Text(Line::from(REFERRAL_SIGNED_OUT))];
    spec.animations = animations;
    spec
}

fn referral_result_spec(result: &Fetched<ReferralInfo>, animations: bool) -> InfoPaneSpec {
    match result {
        Fetched::Ok(info) => referral_spec(info, animations),
        _ => {
            let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
            spec.animations = animations;
            if let Some((row, retry, tag)) = failure_rows(result) {
                spec.tag = tag.unwrap_or_default().to_string();
                spec.body = vec![row];
                spec.retry = retry;
            }
            spec
        }
    }
}

fn referral_spec(info: &ReferralInfo, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Referral, vec!["Referral code".bold()]);
    spec.animations = animations;
    let code = info.code.trim().to_string();
    // Progress toward the free Pro month; old gateways send no target, so the
    // tag is simply absent rather than invented.
    spec.tag = if info.pro_reward_target == 0 {
        String::new()
    } else if info.pro_reward_granted {
        format!("{} Pro month claimed", glyph("✓"))
    } else {
        let done = info.successful_referrals.min(info.pro_reward_target);
        format!(
            "{done} of {} referrals toward a free month of Pro",
            info.pro_reward_target
        )
    };
    let mut body = vec![
        BodyRow::Text(Line::from(code.clone().fg(accent_color()).bold())),
        BodyRow::Blank,
        BodyRow::Text(Line::from(
            "You and each friend who installs with it get $10, valid 30 days.",
        )),
    ];
    let share = info.share_command.trim().to_string();
    if share.is_empty() {
        // A gateway that sends no share command still has a code worth showing.
        spec.body = body;
        return spec;
    }
    body.push(BodyRow::Blank);
    body.push(BodyRow::Text(Line::from(share.clone())));
    for line in [info.share_pip.trim(), info.share_npm.trim()] {
        if !line.is_empty() {
            body.push(BodyRow::Text(Line::from(line.to_string())));
        }
    }
    spec.body = body;
    spec.footer = vec!["enter to copy the curl line".to_string(), close_hint()];
    spec.primary = Primary::Copy(share);
    spec
}

// --- /upgrade --------------------------------------------------------------

/// The picker body is 8 rows: four choices (Pro, Max, Ultra, Redeem a code), one blank,
/// and three wrapped note lines. The loading pane reserves exactly this many so the
/// frame does not jump when the plan probe resolves, which means the number is
/// arithmetic over `UPGRADE_NOTE`, not a guess -- `upgrade_reserved_rows_matches_body`
/// fails if either the note or the row count changes without it.
const UPGRADE_RESERVED_ROWS: u16 = 8;
/// The term the product actually sells (the gateway's payment term is 30 days).
const UPGRADE_TERM_DAYS: u32 = 30;
/// `$4 per 5h, $8 per week`: the plan's real limits (see [`plan_limits`]).
fn limits_label(plan: &str) -> String {
    let (h5, week) = plan_limits(plan);
    format!("${h5} per 5h, ${week} per week")
}
/// ⚠️ Every claim in this sentence must match what the gateway does and what
/// `byteask.ai/pricing` says:
///   * "One payment for 30 days" -- the gateway's payment term is 30 days and there is no
///     recurring product, so "monthly subscription" would be false.
///   * "Renewing extends" -- the gateway refuses only a strictly LOWER
///     plan, and extends a same-rank purchase from
///     max(now, expiry).
///   * "the higher plan runs first" -- an upgrade demotes the displaced term into the
///     fallback slot instead of destroying it, so the remaining days resume when
///     the higher plan lapses.
const UPGRADE_NOTE: &str = concat!(
    "One payment for 30 days. No auto-renew. Renewing extends the plan you are on. ",
    "Moving up runs the higher plan first, then your remaining days resume.",
);

fn upgrade_title() -> Vec<Span<'static>> {
    vec!["Upgrade your plan".bold()]
}

fn upgrade_loading_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
    spec.body = vec![BodyRow::Loading("Loading your plan".to_string())];
    spec.reserve_rows = UPGRADE_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

fn upgrade_link_loading_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
    spec.body = vec![BodyRow::Loading("Creating your payment link".to_string())];
    spec.reserve_rows = UPGRADE_RESERVED_ROWS;
    spec.animations = animations;
    spec
}

fn upgrade_signed_out_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
    spec.body = vec![BodyRow::Text(Line::from(UPGRADE_SIGNED_OUT))];
    spec.animations = animations;
    spec
}

/// The plan picker. On Pro or Max the current row renews and the cursor starts on the
/// next plan up, so `enter` never renews by reflex (a lower plan is not offered: the
/// gateway refuses it); on Ultra the pane is one line, because there is nothing to sell.
fn upgrade_options_spec(current_plan: &str, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
    spec.animations = animations;
    if !current_plan.is_empty() {
        spec.tag = format!("{current_plan} plan");
    }
    if current_plan.eq_ignore_ascii_case("ultra") {
        spec.body = vec![BodyRow::Text(Line::from(
            "You're on Ultra, the highest plan.",
        ))];
        return spec;
    }
    let on_pro = current_plan.eq_ignore_ascii_case("pro");
    let on_max = current_plan.eq_ignore_ascii_case("max");
    let mut plans: Vec<BodyRow> = Vec::new();
    if !on_max {
        plans.push(upgrade_choice_row("pro", on_pro));
    }
    plans.push(upgrade_choice_row("max", on_max));
    plans.push(upgrade_choice_row("ultra", /*current*/ false));
    spec.body = plans;
    spec.body.extend([
        // The third door. A partner deal, a campus programme or a gift arrives as a
        // code, and `/upgrade` is where someone looks when they want more usage --
        // so the code lives here rather than only behind a command they have to know
        // the name of. Choosing it swaps this pane into the voucher form, because a
        // pane cannot have both a chooser and a text field (Primary is one or the
        // other), and because the picker stays priced rows and one door.
        upgrade_redeem_row(),
        BodyRow::Blank,
    ]);
    for line in wrap_note(UPGRADE_NOTE) {
        spec.body.push(BodyRow::Text(Line::from(line)));
    }
    spec.footer = vec!["enter to get a payment link".to_string(), close_hint()];
    spec.primary = Primary::Choose;
    if on_pro || on_max {
        spec.cursor = 1; // the next plan up, among the selectable choices
    }
    spec
}

/// The `id` of the picker row that opens the code form. Not a plan name, so it can
/// never be mistaken for one by `request_byteask_upgrade`.
const UPGRADE_REDEEM_ID: &str = "redeem-code";

/// `Pro    $20 · 30 days     $4 per 5h, $8 per week`. The price column is padded
/// to a fixed width so `$20` and `$100` do not step the last column sideways.
fn upgrade_choice_row(plan: &str, current: bool) -> BodyRow {
    let (label, price) = plan_label_price(plan);
    let price = format!("${price} {} {UPGRADE_TERM_DAYS} days", glyph("·"));
    // The current plan stays selectable: the gateway sells a same-plan renewal
    // (it extends from max(now, expiry)), and an unselectable row left a Pro user
    // with one choice, which made Up/Down do nothing.
    let trailing = if current {
        format!("current plan {} renew", glyph("·"))
    } else {
        limits_label(plan)
    };
    BodyRow::Choice {
        id: plan.to_string(),
        line: Line::from(format!("{label:<7}{price:<18}{trailing}")),
        selectable: true,
    }
}

/// `Redeem a code            partner and campus deals`
fn upgrade_redeem_row() -> BodyRow {
    BodyRow::Choice {
        id: UPGRADE_REDEEM_ID.to_string(),
        line: Line::from(format!(
            "{:<25}{}",
            "Redeem a code", "partner and campus deals"
        )),
        selectable: true,
    }
}

fn upgrade_result_spec(
    plan: &str,
    result: &Fetched<UpgradeInfo>,
    animations: bool,
) -> InfoPaneSpec {
    match result {
        Fetched::Ok(info) => upgrade_link_spec(plan, info.pay_url.trim(), animations),
        // A 4xx is a rule, not a breakage: `You're already on Pro or higher.`
        // answers itself, and sending that user to support would be noise.
        Fetched::Message(_) | Fetched::Expired => {
            let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
            spec.animations = animations;
            if let Some((row, retry, tag)) = failure_rows(result) {
                spec.tag = tag.unwrap_or_default().to_string();
                spec.body = vec![row];
                spec.retry = retry;
            }
            spec
        }
        // Everything else -- unreachable, 5xx, a 5xx that explained itself -- is
        // one blanket line. Whatever went wrong is ours to fix, so the only useful
        // thing we can hand someone who is trying to pay us is a human.
        _ => {
            let mut spec = InfoPaneSpec::new(ByteAskPane::Upgrade, upgrade_title());
            spec.animations = animations;
            spec.body = vec![error_row(UPGRADE_FAILED, /*retryable*/ true)];
            spec.retry = true;
            spec
        }
    }
}

/// The payment link, rendered verbatim as the gateway sent it (the URL is a
/// ByteAsk one, so no payment provider's domain reaches the user).
fn upgrade_link_spec(plan: &str, pay_url: &str, animations: bool) -> InfoPaneSpec {
    let (label, price) = plan_label_price(plan);
    let mut spec = InfoPaneSpec::new(
        ByteAskPane::Upgrade,
        vec![
            format!(
                "Upgrade to {label} {} ${price} for {UPGRADE_TERM_DAYS} days",
                glyph("·")
            )
            .bold(),
        ],
    );
    spec.animations = animations;
    spec.body = vec![
        BodyRow::Text(Line::from("Open this link to pay:")),
        BodyRow::Text(Line::from(pay_url.to_string().fg(accent_color()))),
        BodyRow::Blank,
        BodyRow::Text(Line::from(
            "This pane updates as soon as your payment lands.",
        )),
    ];
    spec.footer = vec!["enter to copy the link".to_string(), close_hint()];
    spec.primary = Primary::Copy(pay_url.to_string());
    spec
}

fn upgrade_paid_spec(label: &str, animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(
        ByteAskPane::Upgrade,
        vec![
            format!("{} ", glyph("✓")).fg(accent_color()),
            format!("You're on {label}").bold(),
        ],
    );
    spec.animations = animations;
    spec.blank_after_title = false;
    spec.tag = "/usage shows your new limits".to_string();
    spec
}

/// Wrap the note to the 72-column frame minus the body inset.
fn wrap_note(note: &str) -> Vec<String> {
    const NOTE_WIDTH: usize = 69;
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in note.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= NOTE_WIDTH {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

// --- /voucher --------------------------------------------------------------

const VOUCHER_HINT: &str = "Credit shows as Extra in /usage and covers spend past your limits.";

fn voucher_loading_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, vec!["Redeem a voucher".bold()]);
    spec.blank_after_title = false;
    spec.body = vec![BodyRow::Loading("Redeeming".to_string())];
    spec.reserve_rows = 2;
    spec.animations = animations;
    spec
}

fn voucher_signed_out_spec(animations: bool) -> InfoPaneSpec {
    let mut spec = InfoPaneSpec::new(
        ByteAskPane::Voucher,
        vec![format!("{} ", glyph("✗")).red(), VOUCHER_SIGNED_OUT.into()],
    );
    spec.blank_after_title = false;
    spec.animations = animations;
    spec
}

/// The form. `error` becomes the title — the field
/// keeps the code the user typed so a retry is one keystroke.
fn voucher_form_spec(code: String, error: Option<&str>, animations: bool) -> InfoPaneSpec {
    let title: Vec<Span<'static>> = match error {
        Some(message) => vec![format!("{} ", glyph("✗")).red(), message.to_string().into()],
        None => vec!["Redeem a voucher".bold()],
    };
    let mut spec = InfoPaneSpec::new(ByteAskPane::Voucher, title);
    spec.animations = animations;
    spec.blank_after_title = false;
    spec.body = match error {
        Some(_) => vec![BodyRow::Field],
        None => vec![
            BodyRow::Field,
            BodyRow::Text(Line::from(VOUCHER_HINT.dim())),
        ],
    };
    spec.field = Some(code);
    spec.footer = vec![
        match error {
            Some(_) => "enter to try again".to_string(),
            None => "enter to redeem".to_string(),
        },
        close_hint(),
    ];
    spec.primary = Primary::Submit;
    spec
}

fn voucher_result_spec(
    code: &str,
    result: &Fetched<VoucherInfo>,
    animations: bool,
) -> InfoPaneSpec {
    match result {
        Fetched::Ok(info) => {
            let mut spec = InfoPaneSpec::new(
                ByteAskPane::Voucher,
                vec![
                    format!("{} ", glyph("✓")).fg(accent_color()),
                    "Voucher redeemed".bold(),
                ],
            );
            spec.animations = animations;
            spec.blank_after_title = false;
            spec.body = wrap_note(&voucher_success_line(code, info))
                .into_iter()
                .map(|line| BodyRow::Text(Line::from(line)))
                .collect();
            spec
        }
        Fetched::Expired => {
            let mut spec = InfoPaneSpec::new(
                ByteAskPane::Voucher,
                vec![format!("{} ", glyph("✗")).red(), SESSION_EXPIRED.into()],
            );
            spec.animations = animations;
            spec.blank_after_title = false;
            spec
        }
        // Everything else keeps the field, so the code is one keystroke away
        // from another try. `r` is never bound while a field has focus.
        Fetched::Message(message) | Fetched::ServerMessage(message) => {
            voucher_form_spec(code.to_string(), Some(message), animations)
        }
        Fetched::ServerError | Fetched::Unreachable => voucher_form_spec(
            code.to_string(),
            Some("Couldn't redeem that code"),
            animations,
        ),
    }
}

// --- screen-reader mode ---------------------------------------------------

///One plain labelled line instead of a pane. A screen reader cannot follow a
/// surface that repaints in place, so these commands go back to the transcript
/// in this mode — deliberately, and only in this mode.
fn labelled_cell(label: &str, body: &str) -> PlainHistoryCell {
    PlainHistoryCell::new(vec![Line::from(format!("{label}: {body}"))])
}

fn usage_label(result: &Fetched<UsageInfo>) -> String {
    match result {
        Fetched::Ok(info) => {
            let sep = glyph("·");
            let mut text = format!(
                "{} plan {sep} 5-hour {}% {sep} resets in {} {sep} weekly {}% {sep} resets in {}",
                display_plan(&info.plan),
                info.used_5h_pct.clamp(0, 100),
                human_reset(info.reset_5h_sec),
                info.used_week_pct.clamp(0, 100),
                human_reset(info.reset_week_sec),
            );
            if info.extra_granted_usd > 0.0 {
                text.push_str(&format!(
                    " {sep} extra ${:.2} left of ${:.2}",
                    info.extra_balance_usd, info.extra_granted_usd
                ));
                if let Some(day) = expiry_label(&info.extra_expiry_day) {
                    if info.extra_expiring_usd > 0.0 {
                        text.push_str(&format!(
                            " (${:.2} expires after {day})",
                            info.extra_expiring_usd
                        ));
                    }
                }
            }
            text
        }
        Fetched::Expired => SESSION_EXPIRED.to_string(),
        Fetched::Message(message) | Fetched::ServerMessage(message) => message.clone(),
        Fetched::ServerError => format!("{SERVER_ERROR}."),
        Fetched::Unreachable => format!("{UNREACHABLE}."),
    }
}

fn referral_label(result: &Fetched<ReferralInfo>) -> String {
    match result {
        Fetched::Ok(info) => {
            let share = info.share_command.trim();
            if share.is_empty() {
                info.code.trim().to_string()
            } else {
                format!("{} {} {share}", info.code.trim(), glyph("·"))
            }
        }
        Fetched::Expired => SESSION_EXPIRED.to_string(),
        Fetched::Message(message) | Fetched::ServerMessage(message) => message.clone(),
        Fetched::ServerError => format!("{SERVER_ERROR}."),
        Fetched::Unreachable => format!("{UNREACHABLE}."),
    }
}

fn upgrade_label(plan: &str, result: &Fetched<UpgradeInfo>) -> String {
    let (label, price) = plan_label_price(plan);
    match result {
        Fetched::Ok(info) => format!(
            "{label} ${price} for {UPGRADE_TERM_DAYS} days {} {}",
            glyph("·"),
            info.pay_url.trim()
        ),
        Fetched::Expired => SESSION_EXPIRED.to_string(),
        Fetched::Message(message) => message.clone(),
        Fetched::ServerError | Fetched::ServerMessage(_) | Fetched::Unreachable => {
            UPGRADE_FAILED.to_string()
        }
    }
}

/// The sentence shown after a successful redemption.
///
/// The gateway authors it, so a code that grants three months of Pro reads as three
/// months of Pro rather than as `$0.00 of credit`. The local fallback exists only for
/// an older gateway that sends no `message`; it is deliberately the pre-existing
/// credit wording, because that is the only case an old gateway can produce.
fn voucher_success_line(code: &str, info: &VoucherInfo) -> String {
    let message = info.message.trim();
    if !message.is_empty() {
        return message.to_string();
    }
    format!(
        "{} added ${:.2} of credit. Extra balance: ${:.2}.",
        code.trim().to_uppercase(),
        info.amount_added_usd,
        info.balance_usd
    )
}

fn voucher_label(code: &str, result: &Fetched<VoucherInfo>) -> String {
    match result {
        Fetched::Ok(info) => voucher_success_line(code, info),
        Fetched::Expired => SESSION_EXPIRED.to_string(),
        Fetched::Message(message) | Fetched::ServerMessage(message) => message.clone(),
        Fetched::ServerError | Fetched::Unreachable => "Couldn't redeem that code.".to_string(),
    }
}

fn referral_banner_cell() -> PlainHistoryCell {
    PlainHistoryCell::new(vec![Line::from(
        "You and your friend each get $10 per referral, valid 30 days · /referral-code"
            .fg(accent_color()),
    )])
}

fn accent_color() -> Color {
    crate::style::accent_color()
}

/// Exact time-until-reset plus the local wall-clock reset time, e.g.
/// `4h 59m (03:06)` or `6d 23h (22:06 on 14 Sep)`.
///
/// The relative figure answers *when can I continue*; the absolute one answers
/// *is that before I sleep* — which is why the absolute follows. The
/// clock is upstream's [`format_reset_timestamp`], so ByteAsk's times read the
/// same 24-hour way as upstream's. The gateway sends a duration, not an instant,
/// because it does not know the user's timezone.
/// `2026-11-08` -> `8 Nov`: the last day extra credit can be used. None for anything
/// that is not a plain `YYYY-MM-DD` (an older gateway sends nothing at all).
fn expiry_label(day: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = day.trim().split('-');
    let (_y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let month: usize = m.parse().ok()?;
    let dom: u32 = d.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&dom) {
        return None;
    }
    Some(format!("{dom} {}", MONTHS[month - 1]))
}

fn human_reset(sec: i64) -> String {
    if sec <= 0 {
        return "now".to_string();
    }
    let remaining = format_remaining(sec);
    let now = chrono::Local::now();
    let at = now + chrono::Duration::seconds(sec);
    format!(
        "{remaining} ({})",
        format_reset_timestamp(at, now, crate::clock_format::ClockFormat::system())
    )
}

/// Exact remaining as the two largest non-zero units, dropping a zero lower
/// unit: e.g. `5d 19h`, `3h 47m`, `3h`, `4m 12s`, `47s`.
fn format_remaining(sec: i64) -> String {
    let (d, h, m, s) = (
        sec / 86_400,
        (sec % 86_400) / 3_600,
        (sec % 3_600) / 60,
        sec % 60,
    );
    let (hi, hi_u, lo, lo_u) = if d > 0 {
        (d, "d", h, "h")
    } else if h > 0 {
        (h, "h", m, "m")
    } else if m > 0 {
        (m, "m", s, "s")
    } else {
        (s, "s", 0, "")
    };
    if lo > 0 {
        format!("{hi}{hi_u} {lo}{lo_u}")
    } else {
        format!("{hi}{hi_u}")
    }
}

/// Present the plan name with a capitalized first letter; empty stays "—".
fn display_plan(plan: &str) -> String {
    let plan = plan.trim();
    if plan.is_empty() {
        return "—".to_string();
    }
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bottom_pane::info_pane_view::InfoPaneView;
    use crate::chatwidget::tests::helpers::make_chatwidget_manual_with_sender;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// The designed frames are the pane's *content* width. Upstream's
    /// `render_menu_surface` insets two columns on each side, so a 72-column
    /// frame needs a 76-column terminal; below that the rows truncate with `…`.
    const SURFACE_INSET: u16 = 2;
    const FRAME_WIDTH: u16 = 72;

    /// A screen whose rows render and whose cursor moves can still do nothing.
    ///
    /// The refusal pane shipped exactly like that: `Primary` defaults to `None`, and
    /// only `Primary::Choose` submits the selected `Choice` row, so `enter` on "Back"
    /// was dead while the screen looked complete. Rendering alone does not prove a
    /// screen works - the binding has to be asserted.
    #[test]
    fn the_refusal_screen_can_actually_be_acted_on() {
        let spec = subscription_failed_spec("Refused.", /*animations*/ false);

        assert_eq!(
            spec.primary,
            Primary::Choose,
            "enter must submit the selected row, or both choices are dead"
        );

        let choices: Vec<&str> = spec
            .body
            .iter()
            .filter_map(|row| match row {
                BodyRow::Choice { id, selectable, .. } if *selectable => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            choices,
            vec![SUBSCRIPTION_RETRY_ID, SUBSCRIPTION_BACK_ID],
            "both rows must be selectable; one selectable row makes the arrows dead too"
        );
    }

    /// The refusal text arrives from the login crate as one long sentence. Unwrapped,
    /// it runs off the pane frame instead of breaking at the 72-column body.
    #[test]
    fn the_refusal_message_is_wrapped_to_the_frame() {
        let long = "This ChatGPT account is on the \"go\" plan, and OpenAI serves no \
                    model to ByteAsk on it - every model is refused, so the session \
                    would fail on the first message. ChatGPT Plus or higher is required.";
        let spec = subscription_failed_spec(long, /*animations*/ false);

        let widest = spec
            .body
            .iter()
            .filter_map(|row| match row {
                BodyRow::Text(line) => Some(line.width()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        assert!(
            widest <= FRAME_WIDTH as usize,
            "a body line is {widest} columns wide and will run off the {FRAME_WIDTH}-column frame"
        );
    }
    const REFERRAL_FRAME_WIDTH: u16 = 80;

    fn view(spec: InfoPaneSpec) -> InfoPaneView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        InfoPaneView::new(spec, crate::app_event_sender::AppEventSender::new(tx))
    }

    /// Render at `content_width` + the surface inset and return the content rows
    /// (leading inset stripped, trailing blanks dropped).
    fn frame(spec: InfoPaneSpec, content_width: u16) -> Vec<String> {
        let view = view(spec);
        let width = content_width + SURFACE_INSET * 2;
        let height = view.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        let mut out: Vec<String> = (0..height)
            .map(|y| {
                (SURFACE_INSET..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        // The first row is the surface's vertical inset.
        out.remove(0);
        for row in &out {
            assert!(
                row.chars().count() <= content_width as usize,
                "row wider than the {content_width}-column frame: {row:?}"
            );
        }
        out
    }

    /// The absolute reset clock comes from `Local::now()`, so pin the shape and
    /// not the value.
    fn stable_times(rows: Vec<String>) -> Vec<String> {
        rows.into_iter()
            .map(|row| match row.rfind(" (") {
                Some(at) if row.ends_with(')') && row.contains("resets in ") => {
                    format!("{}{}", &row[..at], " (TIME)")
                }
                _ => row,
            })
            .collect()
    }

    /// A title row: the title on the left, the tag flush right, filling `width`.
    fn title_row(title: &str, tag: &str, width: usize) -> String {
        let gap = width - title.chars().count() - tag.chars().count();
        format!("{title}{}{tag}", " ".repeat(gap))
    }

    fn usage_info(plan: &str, used_5h: i64, used_week: i64) -> UsageInfo {
        UsageInfo {
            plan: plan.to_string(),
            used_5h_pct: used_5h,
            used_week_pct: used_week,
            reset_5h_sec: 47 * 60,
            reset_week_sec: 6 * 86_400 + 23 * 3600,
            ..Default::default()
        }
    }

    // --- /usage -------------------------------------------------------------

    #[test]
    fn usage_free_plan_frame() {
        let rows = stable_times(frame(
            usage_spec(
                &Fetched::Ok(usage_info("free", 1, 0)),
                /*animations*/ false,
            ),
            FRAME_WIDTH,
        ));
        assert_eq!(
            rows,
            vec![
                "Usage · Free plan".to_string(),
                "".to_string(),
                "  5-hour  ░░░░░░░░░░░░░░░░░░░░   1%   resets in 47m (TIME)".to_string(),
                "  Weekly  ░░░░░░░░░░░░░░░░░░░░   0%   resets in 6d 23h (TIME)".to_string(),
                "".to_string(),
                "esc to close".to_string(),
            ]
        );
    }

    #[test]
    fn usage_frame_shows_when_the_next_credit_expires() {
        // Extra credit lapses; the gateway sends the soonest bucket.
        let mut info = usage_info("free", 10, 10);
        info.extra_granted_usd = 20.0;
        info.extra_balance_usd = 15.0;
        info.extra_expiring_usd = 5.0;
        info.extra_expiry_day = "2026-11-08".to_string();
        let rows = frame(
            usage_spec(&Fetched::Ok(info), /*animations*/ false),
            FRAME_WIDTH,
        );
        assert!(
            rows.iter()
                .any(|r| r == "          $5.00 expires after 8 Nov"),
            "expected the expiry row under Extra:\n{rows:#?}"
        );
    }

    #[test]
    fn usage_frame_without_an_expiry_is_unchanged() {
        // An older gateway sends no expiry: no extra row, no stray text.
        let mut info = usage_info("free", 10, 10);
        info.extra_granted_usd = 20.0;
        info.extra_balance_usd = 15.0;
        let rows = frame(
            usage_spec(&Fetched::Ok(info), /*animations*/ false),
            FRAME_WIDTH,
        );
        assert!(!rows.iter().any(|r| r.contains("expires")), "{rows:#?}");
    }

    #[test]
    fn expiry_label_formats_only_a_real_date() {
        assert_eq!(expiry_label("2026-11-08").as_deref(), Some("8 Nov"));
        assert_eq!(expiry_label("2027-01-31").as_deref(), Some("31 Jan"));
        assert_eq!(expiry_label(""), None);
        assert_eq!(expiry_label("2026-13-01"), None);
        assert_eq!(expiry_label("8 Nov"), None);
    }

    #[test]
    fn usage_pro_frame_has_extra_and_the_upgrade_row_only_when_earned() {
        let mut info = usage_info("pro", 82, 31);
        info.extra_granted_usd = 15.0;
        info.extra_balance_usd = 12.0;
        let rows = stable_times(frame(
            usage_spec(&Fetched::Ok(info), /*animations*/ false),
            FRAME_WIDTH,
        ));
        assert_eq!(
            rows,
            vec![
                "Usage · Pro plan".to_string(),
                "".to_string(),
                "  5-hour  ████████████████░░░░  82%   resets in 47m (TIME)".to_string(),
                "  Weekly  ██████░░░░░░░░░░░░░░  31%   resets in 6d 23h (TIME)".to_string(),
                "  Extra   $12.00 left of $15.00 · covers spend past your limits".to_string(),
                "".to_string(),
                "  Higher limits with /upgrade".to_string(),
                "".to_string(),
                "esc to close".to_string(),
            ]
        );
    }

    /// The plan used to be a dim right-aligned tag, easy to miss and dropped
    /// first on a narrow pane. It now leads the title, bold in the accent.
    #[test]
    fn usage_plan_leads_the_title_in_the_accent_at_every_width() {
        let spec = usage_spec(&Fetched::Ok(usage_info("pro", 1, 0)), false);
        assert!(
            spec.tag.is_empty(),
            "no redundant right tag: {:?}",
            spec.tag
        );
        let plan = spec
            .title
            .iter()
            .find(|span| span.content == "Pro plan")
            .expect("plan span in the title");
        assert!(
            plan.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(plan.style.fg, Some(crate::style::accent_color()));

        let narrow = frame(
            usage_spec(&Fetched::Ok(usage_info("pro", 1, 0)), false),
            /*content_width*/ 40, // the narrowest width a pane renders at
        );
        assert!(narrow[0].starts_with("Usage · Pro plan"), "{narrow:?}");
    }

    #[test]
    fn usage_hides_extra_without_credit_and_the_upgrade_row_below_half() {
        let rows = frame(
            usage_spec(&Fetched::Ok(usage_info("free", 49, 10)), false),
            FRAME_WIDTH,
        );
        let text = rows.join("\n");
        assert!(!text.contains("Extra"), "{text}");
        assert!(!text.contains("Higher limits"), "{text}");
    }

    #[test]
    fn usage_never_sells_an_upgrade_to_an_ultra_subscriber() {
        let rows = frame(
            usage_spec(&Fetched::Ok(usage_info("ultra", 99, 99)), false),
            FRAME_WIDTH,
        );
        let text = rows.join("\n");
        assert!(text.contains("Ultra plan"), "{text}");
        assert!(!text.contains("/upgrade"), "{text}");
    }

    #[test]
    fn usage_offers_ultra_to_a_max_subscriber_near_the_limit() {
        let rows = frame(
            usage_spec(&Fetched::Ok(usage_info("max", 99, 99)), false),
            FRAME_WIDTH,
        );
        let text = rows.join("\n");
        assert!(text.contains("/upgrade"), "{text}");
    }

    #[test]
    fn usage_signed_out_frame() {
        let rows = frame(usage_signed_out_spec(false), FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                "Usage".to_string(),
                "".to_string(),
                "  Sign in with /login for plan limits.".to_string(),
                "".to_string(),
                "esc to close".to_string(),
            ]
        );
    }

    #[test]
    fn usage_error_frames_name_the_cause_and_the_one_fix() {
        let unreachable = frame(
            usage_spec(&Fetched::<UsageInfo>::Unreachable, false),
            FRAME_WIDTH,
        );
        assert_eq!(unreachable[2], "  ✗ Couldn't reach ByteAsk · r to retry");

        let server = frame(
            usage_spec(&Fetched::<UsageInfo>::ServerError, false),
            FRAME_WIDTH,
        );
        assert_eq!(server[2], "  ✗ ByteAsk returned an error · r to retry");

        let expired = frame(
            usage_spec(&Fetched::<UsageInfo>::Expired, false),
            FRAME_WIDTH,
        );
        assert_eq!(
            expired[0],
            title_row("Usage", "signed out", FRAME_WIDTH as usize)
        );
        assert_eq!(expired[2], "  ✗ Your session expired. Sign in with /login.");
        // An expired session is not retryable: `r` would just fail again.
        assert!(!expired[2].contains("r to retry"));
    }

    /// Semantic colour warns, but never alone: the percentage is text on every
    /// row, so a `NO_COLOR` terminal reads the same warning.
    #[test]
    fn usage_bars_and_numbers_turn_yellow_at_75_and_red_at_90() {
        assert_eq!(usage_colour(74), None);
        assert_eq!(usage_colour(75), Some(Color::Yellow));
        assert_eq!(usage_colour(89), Some(Color::Yellow));
        assert_eq!(usage_colour(90), Some(Color::Red));

        // The colour lands on both the bar and the number, and the number is
        // still printed either way.
        let BodyRow::Text(line) = usage_window_row("5-hour", 91, 600) else {
            panic!("expected a text row");
        };
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains(" 91%"), "{text}");
        let reds = line
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::Red))
            .count();
        assert_eq!(reds, 2, "the filled bar and the percentage both warn");
    }

    #[test]
    fn usage_bar_fills_with_what_is_spent_and_never_reads_full_below_100() {
        assert_eq!(usage_bar(0), "░".repeat(20));
        assert_eq!(
            usage_bar(50),
            format!("{}{}", "█".repeat(10), "░".repeat(10))
        );
        assert_eq!(usage_bar(100), "█".repeat(20));
        // 99 % rounds to 20 filled segments; a bar that reads full at 99 % is a
        // lie the user acts on, so the last segment stays empty until 100 %.
        assert_eq!(usage_bar(99), format!("{}{}", "█".repeat(19), "░"));
    }

    // --- /referral-code -----------------------------------------------------

    fn referral_info() -> ReferralInfo {
        ReferralInfo {
            code: "REFCODE1".to_string(),
            share_command:
                "curl -fsSL https://code.byteask.ai/install.sh | BYTEASK_REF=REFCODE1 sh"
                    .to_string(),
            share_pip: "pip install byteask && BYTEASK_REF=REFCODE1 byteask".to_string(),
            share_npm: "npm install -g @byteask/cli && BYTEASK_REF=REFCODE1 byteask".to_string(),
            successful_referrals: 0,
            pro_reward_target: 3,
            pro_reward_granted: false,
        }
    }

    #[test]
    fn referral_frame_at_eighty_columns() {
        let rows = frame(referral_spec(&referral_info(), false), REFERRAL_FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                title_row(
                    "Referral code",
                    "0 of 3 referrals toward a free month of Pro",
                    REFERRAL_FRAME_WIDTH as usize
                ),
                "".to_string(),
                "  REFCODE1".to_string(),
                "".to_string(),
                "  You and each friend who installs with it get $10, valid 30 days.".to_string(),
                "".to_string(),
                "  curl -fsSL https://code.byteask.ai/install.sh | BYTEASK_REF=REFCODE1 sh"
                    .to_string(),
                "  pip install byteask && BYTEASK_REF=REFCODE1 byteask".to_string(),
                "  npm install -g @byteask/cli && BYTEASK_REF=REFCODE1 byteask".to_string(),
                "".to_string(),
                "enter to copy the curl line · esc to close".to_string(),
            ]
        );
    }

    #[test]
    fn referral_enter_copies_the_underlying_string_not_the_rendered_row() {
        let spec = referral_spec(&referral_info(), false);
        assert_eq!(
            spec.primary,
            Primary::Copy(
                "curl -fsSL https://code.byteask.ai/install.sh | BYTEASK_REF=REFCODE1 sh"
                    .to_string()
            )
        );
    }

    #[test]
    fn referral_tag_reads_claimed_after_the_third_referral_and_is_absent_on_old_gateways() {
        let mut info = referral_info();
        info.pro_reward_granted = true;
        assert_eq!(referral_spec(&info, false).tag, "✓ Pro month claimed");

        info.pro_reward_granted = false;
        info.pro_reward_target = 0;
        assert_eq!(referral_spec(&info, false).tag, "");
    }

    #[test]
    fn referral_without_share_lines_shows_the_code_only_and_offers_no_enter() {
        let mut info = referral_info();
        info.share_command = String::new();
        let spec = referral_spec(&info, false);
        assert_eq!(spec.primary, Primary::None);
        assert_eq!(spec.footer, vec!["esc to close".to_string()]);
        let rows = frame(spec, REFERRAL_FRAME_WIDTH);
        assert!(rows.iter().any(|row| row.contains("REFCODE1")), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains("curl")), "{rows:?}");
    }

    #[test]
    fn referral_signed_out_frame() {
        let rows = frame(referral_signed_out_spec(false), REFERRAL_FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                "Referral code",
                "",
                "  Sign in with /login to get a referral code.",
                "",
                "esc to close",
            ]
        );
    }

    // --- /upgrade -----------------------------------------------------------

    #[test]
    fn upgrade_picker_frame_on_free() {
        let rows = frame(upgrade_options_spec("Free", false), FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                title_row("Upgrade your plan", "Free plan", FRAME_WIDTH as usize),
                "".to_string(),
                "› Pro    $20 · 30 days     $4 per 5h, $8 per week".to_string(),
                "  Max    $100 · 30 days    $10 per 5h, $30 per week".to_string(),
                "  Ultra  $200 · 30 days    $20 per 5h, $60 per week".to_string(),
                "  Redeem a code            partner and campus deals".to_string(),
                "".to_string(),
                "  One payment for 30 days. No auto-renew. Renewing extends the plan you"
                    .to_string(),
                "  are on. Moving up runs the higher plan first, then your remaining".to_string(),
                "  days resume.".to_string(),
                "".to_string(),
                "enter to get a payment link · esc to close".to_string(),
            ]
        );
    }

    #[test]
    fn upgrade_picker_on_pro_marks_the_current_plan_and_starts_on_max() {
        let spec = upgrade_options_spec("Pro", false);
        let rows = frame(spec, FRAME_WIDTH);
        assert_eq!(rows[2], "  Pro    $20 · 30 days     current plan · renew");
        assert_eq!(
            rows[3],
            "› Max    $100 · 30 days    $10 per 5h, $30 per week"
        );
    }

    #[test]
    fn upgrade_picker_on_pro_lets_the_arrows_reach_pro_to_renew() {
        // 0.1.12 shipped the current-plan row unselectable, so a Pro user's picker
        // had ONE selectable row, `move_cursor` ignored Up/Down, and renewing Pro --
        // which the gateway allows -- was unreachable.
        use crate::bottom_pane::BottomPaneView;
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = InfoPaneView::new(
            upgrade_options_spec("Pro", false),
            crate::app_event_sender::AppEventSender::new(tx),
        );
        view.handle_key_event(KeyEvent::from(KeyCode::Up));
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));
        match rx.try_recv() {
            Ok(crate::app_event::AppEvent::ByteAskPaneAction { action, .. }) => assert_eq!(
                action,
                crate::bottom_pane::info_pane_view::PaneAction::Choose("pro".to_string())
            ),
            Ok(_) => panic!("expected a pane Choose action"),
            Err(e) => panic!("no action sent: {e:?}"),
        }
    }

    #[test]
    fn upgrade_on_ultra_is_one_line_with_nothing_to_sell() {
        let spec = upgrade_options_spec("Ultra", false);
        assert_eq!(spec.primary, Primary::None);
        let rows = frame(spec, FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                title_row("Upgrade your plan", "Ultra plan", FRAME_WIDTH as usize),
                "".to_string(),
                "  You're on Ultra, the highest plan.".to_string(),
                "".to_string(),
                "esc to close".to_string(),
            ]
        );
    }

    #[test]
    fn upgrade_link_frame_renders_the_gateway_url_verbatim() {
        let rows = frame(
            upgrade_link_spec("pro", "https://code.byteask.ai/pay/example1", false),
            FRAME_WIDTH,
        );
        assert_eq!(
            rows,
            vec![
                "Upgrade to Pro · $20 for 30 days",
                "",
                "  Open this link to pay:",
                "  https://code.byteask.ai/pay/example1",
                "",
                "  This pane updates as soon as your payment lands.",
                "",
                "enter to copy the link · esc to close",
            ]
        );
    }

    #[test]
    fn upgrade_paid_frame() {
        let rows = frame(upgrade_paid_spec("Pro", false), FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                title_row(
                    "✓ You're on Pro",
                    "/usage shows your new limits",
                    FRAME_WIDTH as usize
                ),
                "".to_string(),
                "esc to close".to_string(),
            ]
        );
    }

    /// A user who cannot pay gets one blanket line and a human, whatever broke —
    /// naming the cause tells them nothing they can act on. The single exception
    /// is a 4xx rule, which answers itself and must not send anyone to support.
    #[test]
    fn every_payment_breakage_gives_the_same_line_and_a_human() {
        for broken in [
            Fetched::<UpgradeInfo>::Unreachable,
            Fetched::<UpgradeInfo>::ServerError,
            // a 5xx that explained itself: the explanation is still not shown
            Fetched::<UpgradeInfo>::ServerMessage("Payment provider unavailable.".to_string()),
        ] {
            let rows = frame(upgrade_result_spec("pro", &broken, false), FRAME_WIDTH);
            assert_eq!(
                rows[2], "  ✗ Payment failed. Contact founders@byteask.ai · r to retry",
                "{broken:?} must not leak a cause"
            );
        }

        let refused = frame(
            upgrade_result_spec(
                "pro",
                &Fetched::<UpgradeInfo>::Message("You're already on Pro or higher.".to_string()),
                false,
            ),
            FRAME_WIDTH,
        );
        assert_eq!(refused[2], "  ✗ You're already on Pro or higher.");
        assert!(
            !refused.iter().any(|r| r.contains("founders@byteask.ai")),
            "a plan rule is not a breakage and must not offer support"
        );
    }

    // --- /voucher -----------------------------------------------------------

    #[test]
    fn voucher_form_frame() {
        let rows = frame(
            voucher_form_spec("GIFT5".to_string(), None, false),
            FRAME_WIDTH,
        );
        assert_eq!(
            rows,
            vec![
                "Redeem a voucher",
                "▌ GIFT5",
                "  Credit shows as Extra in /usage and covers spend past your limits.",
                "",
                "enter to redeem · esc to close",
            ]
        );
    }

    #[test]
    fn voucher_success_frame() {
        let rows = frame(
            voucher_result_spec(
                "gift5",
                &Fetched::Ok(VoucherInfo {
                    amount_added_usd: 5.0,
                    balance_usd: 12.0,
                    ..Default::default()
                }),
                false,
            ),
            FRAME_WIDTH,
        );
        assert_eq!(
            rows,
            vec![
                "✓ Voucher redeemed",
                "  GIFT5 added $5.00 of credit. Extra balance: $12.00.",
                "",
                "esc to close",
            ]
        );
    }

    #[test]
    fn voucher_error_keeps_the_code_in_the_field() {
        let rows = frame(
            voucher_result_spec(
                "GIFT5",
                &Fetched::<VoucherInfo>::Message("That code has already been used.".to_string()),
                false,
            ),
            FRAME_WIDTH,
        );
        assert_eq!(
            rows,
            vec![
                "✗ That code has already been used.",
                "▌ GIFT5",
                "",
                "enter to try again · esc to close",
            ]
        );
    }

    #[test]
    fn voucher_unreachable_uses_our_own_sentence_and_keeps_the_field() {
        let rows = frame(
            voucher_result_spec("GIFT5", &Fetched::<VoucherInfo>::Unreachable, false),
            FRAME_WIDTH,
        );
        assert_eq!(rows[0], "✗ Couldn't redeem that code");
        assert_eq!(rows[1], "▌ GIFT5");
    }

    #[test]
    fn voucher_signed_out_frame() {
        let rows = frame(voucher_signed_out_spec(false), FRAME_WIDTH);
        assert_eq!(
            rows,
            vec![
                "✗ Sign in with /login to redeem a voucher.",
                "",
                "esc to close",
            ]
        );
    }

    // --- copy rules --------------------------------------------------------

    #[test]
    fn no_pane_copy_uses_a_forbidden_word_or_an_exclamation_mark() {
        let specs = vec![
            usage_spec(&Fetched::Ok(usage_info("pro", 82, 31)), false),
            usage_spec(&Fetched::<UsageInfo>::Expired, false),
            usage_signed_out_spec(false),
            referral_spec(&referral_info(), false),
            referral_signed_out_spec(false),
            upgrade_options_spec("Free", false),
            upgrade_link_spec("pro", "https://code.byteask.ai/pay/x", false),
            upgrade_paid_spec("Pro", false),
            upgrade_signed_out_spec(false),
            voucher_form_spec(String::new(), None, false),
            voucher_signed_out_spec(false),
        ];
        for spec in specs {
            let text = frame(spec, REFERRAL_FRAME_WIDTH).join("\n");
            for banned in ["Codex", "OpenAI", "Fireworks", "Vertex", "Razorpay", "!"] {
                assert!(
                    !text.contains(banned),
                    "pane copy must not contain {banned:?}: {text}"
                );
            }
            // `Auto` is a display name; the slug never appears in prose.
            assert!(!text.contains(" auto "), "{text}");
        }
    }

    // --- the transcript rule ------------------------------------------------

    /// The transcript IS the stream of `InsertHistoryCell` events — there is no
    /// history `Vec` to diff (`insert_history.rs`: the terminal's own scrollback
    /// holds finalized history). Draining the channel and reading the cells back
    /// is therefore the exact test for "nothing entered the transcript".
    fn transcript_lines(rx: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                for line in cell.display_lines(/*width*/ 80) {
                    out.push(
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>(),
                    );
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn opening_and_closing_every_pane_leaves_the_transcript_byte_identical() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        // Drain anything the constructor emitted so only the panes are measured.
        let _ = transcript_lines(&mut rx);

        for open in [
            ByteAskPane::Usage,
            ByteAskPane::Referral,
            ByteAskPane::Upgrade,
            ByteAskPane::Voucher,
            ByteAskPane::Subscription,
        ] {
            match open {
                ByteAskPane::Usage => chat.show_byteask_usage(),
                ByteAskPane::Referral => chat.show_byteask_referral(),
                ByteAskPane::Upgrade => chat.show_byteask_upgrade(String::new()),
                ByteAskPane::Voucher => chat.show_byteask_voucher(String::new()),
                // Opened directly rather than through start_byteask_subscription_login:
                // that would spawn a real device-code request at auth.openai.com, and
                // this test is about the pane contract - it opens, `esc` closes it -
                // not about the login. The sign-in itself is covered in codex-login.
                ByteAskPane::Subscription => {
                    let animations = chat.byteask_animations();
                    chat.open_byteask_pane(subscription_starting_spec(animations));
                }
            }
            assert!(
                chat.byteask_pane_open(open),
                "{open:?} should have opened a pane"
            );
            // `esc` closes it, exactly as it closes upstream's own views.
            chat.bottom_pane
                .handle_key_event(crossterm::event::KeyEvent::from(
                    crossterm::event::KeyCode::Esc,
                ));
            assert!(!chat.byteask_pane_open(open), "{open:?} should have closed");
        }

        assert_eq!(
            transcript_lines(&mut rx),
            Vec::<String>::new(),
            "no readout may enter the transcript"
        );
        assert!(chat.transcript.active_cell.is_none());
    }

    #[tokio::test]
    async fn a_result_for_a_closed_pane_is_dropped() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        let _ = transcript_lines(&mut rx);

        // Nothing is open: every readout result is discarded, not inserted.
        chat.handle_byteask_pane_result(PaneOutcome::Usage(Fetched::Ok(usage_info("pro", 10, 10))));
        chat.handle_byteask_pane_result(PaneOutcome::Referral(Fetched::Ok(referral_info())));
        chat.handle_byteask_pane_result(PaneOutcome::Voucher {
            code: "GIFT5".to_string(),
            result: Fetched::Ok(VoucherInfo::default()),
        });
        chat.handle_byteask_pane_result(PaneOutcome::Upgrade {
            plan: "pro".to_string(),
            result: Fetched::Ok(UpgradeInfo {
                pay_url: "https://code.byteask.ai/pay/x".to_string(),
            }),
        });

        assert_eq!(transcript_lines(&mut rx), Vec::<String>::new());
        assert!(!chat.byteask_pane_open(ByteAskPane::Usage));
    }

    #[tokio::test]
    async fn a_payment_that_lands_after_the_pane_closed_leaves_exactly_one_line() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        let _ = transcript_lines(&mut rx);

        chat.handle_byteask_pane_result(PaneOutcome::UpgradePoll {
            plan: "pro".to_string(),
        });

        let after = transcript_lines(&mut rx);
        assert_eq!(
            after.len(),
            1,
            "the upgrade notice is one line, not a card: {after:?}"
        );
        assert!(after[0].contains("Upgraded to Pro"), "{after:?}");
    }

    #[tokio::test]
    async fn a_payment_that_lands_while_the_pane_is_open_rewrites_the_pane_instead() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        let _ = transcript_lines(&mut rx);
        chat.open_byteask_pane(upgrade_link_spec(
            "pro",
            "https://code.byteask.ai/pay/x",
            false,
        ));

        chat.handle_byteask_pane_result(PaneOutcome::UpgradePoll {
            plan: "pro".to_string(),
        });

        assert!(chat.byteask_pane_open(ByteAskPane::Upgrade));
        assert_eq!(transcript_lines(&mut rx), Vec::<String>::new());
    }

    #[tokio::test]
    async fn pushing_an_interactive_view_dismisses_an_open_readout() {
        let (mut chat, _tx, _rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        chat.open_byteask_pane(usage_signed_out_spec(false));
        assert!(chat.byteask_pane_open(ByteAskPane::Usage));

        // Any other pane replaces it: info panes never stack.
        chat.open_byteask_pane(voucher_signed_out_spec(false));
        assert!(!chat.byteask_pane_open(ByteAskPane::Usage));
        assert!(chat.byteask_pane_open(ByteAskPane::Voucher));
    }

    // --- screen-reader mode ------------------------------------------------

    #[test]
    fn screen_reader_labels_are_one_plain_line_per_command() {
        let mut info = usage_info("free", 1, 0);
        info.extra_granted_usd = 10.0;
        info.extra_balance_usd = 7.0;
        let label = usage_label(&Fetched::Ok(info));
        assert!(
            label.starts_with("Free plan · 5-hour 1% · resets in 47m ("),
            "{label}"
        );
        assert!(
            label.contains("· weekly 0% · resets in 6d 23h ("),
            "{label}"
        );
        assert!(label.ends_with("· extra $7.00 left of $10.00"), "{label}");

        assert_eq!(
            referral_label(&Fetched::Ok(referral_info())),
            "REFCODE1 · curl -fsSL https://code.byteask.ai/install.sh | BYTEASK_REF=REFCODE1 sh"
        );
        assert_eq!(
            voucher_label(
                "gift5",
                &Fetched::Ok(VoucherInfo {
                    amount_added_usd: 5.0,
                    balance_usd: 12.0,
                    ..Default::default()
                })
            ),
            "GIFT5 added $5.00 of credit. Extra balance: $12.00."
        );
    }

    // --- plan-granting vouchers + the /upgrade code row ---------------------

    fn plan_voucher(message: &str) -> VoucherInfo {
        VoucherInfo {
            amount_added_usd: 0.0,
            balance_usd: 0.0,
            plan: "pro".to_string(),
            plan_days: 90,
            plan_expires_at: 1_773_360_000,
            message: message.to_string(),
        }
    }

    #[test]
    fn a_plan_grant_renders_the_gateways_own_sentence() {
        // The whole point of `message`: a code that grants three months of Pro must
        // not render through the credit template as "added $0.00 of credit".
        let info = plan_voucher("PROMO unlocked Pro for 90 days. Active until 12 Dec 2026.");
        assert_eq!(
            voucher_success_line("promo", &info),
            "PROMO unlocked Pro for 90 days. Active until 12 Dec 2026."
        );
    }

    #[test]
    fn an_older_gateway_that_sends_no_message_still_renders_the_credit_line() {
        let info = VoucherInfo {
            amount_added_usd: 5.0,
            balance_usd: 12.0,
            ..Default::default()
        };
        assert_eq!(
            voucher_success_line("gift5", &info),
            "GIFT5 added $5.00 of credit. Extra balance: $12.00."
        );
    }

    #[test]
    fn a_plan_grant_frame_shows_the_message_not_the_dollars() {
        let rows = frame(
            voucher_result_spec(
                "promo",
                &Fetched::Ok(plan_voucher("PROMO unlocked Pro for 90 days.")),
                false,
            ),
            FRAME_WIDTH,
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("PROMO unlocked Pro for 90 days."))
        );
        assert!(
            !rows.iter().any(|r| r.contains("$0.00")),
            "a plan grant must never print a dollar amount: {rows:?}"
        );
    }

    #[test]
    fn the_upgrade_picker_offers_a_code_row_that_is_not_a_plan() {
        let spec = upgrade_options_spec("free", false);
        let ids: Vec<String> = spec
            .body
            .iter()
            .filter_map(|row| match row {
                BodyRow::Choice { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["pro", "max", "ultra", UPGRADE_REDEEM_ID]);
        // ⚠️ The id must never collide with a plan name: request_byteask_upgrade
        // would otherwise POST it to the gateway as a plan.
        assert!(!ids.contains(&"free".to_string()));
        assert_ne!(UPGRADE_REDEEM_ID, "pro");
        assert_ne!(UPGRADE_REDEEM_ID, "max");
        assert_ne!(UPGRADE_REDEEM_ID, "ultra");
    }

    #[test]
    fn a_max_user_can_renew_or_move_up_to_ultra() {
        let spec = upgrade_options_spec("max", false);
        let ids: Vec<String> = spec
            .body
            .iter()
            .filter_map(|row| match row {
                BodyRow::Choice { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        // Pro is lower than Max, which the gateway refuses, so it is not offered.
        assert_eq!(ids, vec!["max", "ultra", UPGRADE_REDEEM_ID]);
        assert_eq!(
            spec.cursor, 1,
            "the cursor starts on Ultra, never on the renewal"
        );
    }

    #[test]
    fn plan_rows_show_the_real_limits() {
        assert_eq!(plan_label_price("ultra"), ("Ultra", 200));
        assert_eq!(limits_label("pro"), "$4 per 5h, $8 per week");
        assert_eq!(limits_label("max"), "$10 per 5h, $30 per week");
        assert_eq!(limits_label("ultra"), "$20 per 5h, $60 per week");
    }

    #[test]
    fn the_ultra_user_pane_offers_no_rows_at_all() {
        // Nothing to sell and nothing to redeem into: one line.
        let spec = upgrade_options_spec("ultra", false);
        assert!(
            !spec
                .body
                .iter()
                .any(|row| matches!(row, BodyRow::Choice { .. })),
            "an Ultra user should see one line, not a picker"
        );
    }

    #[test]
    fn upgrade_reserved_rows_matches_the_body_it_reserves_for() {
        // The loading pane reserves this many rows so the frame does not jump when
        // the plan probe resolves. Adding a row or a word to UPGRADE_NOTE changes
        // the real height, and this is what notices.
        let spec = upgrade_options_spec("free", false);
        assert_eq!(
            spec.body.len() as u16,
            UPGRADE_RESERVED_ROWS,
            "UPGRADE_RESERVED_ROWS is out of date with the picker body"
        );
    }

    // --- unchanged plumbing -------------------------------------------------

    #[test]
    fn endpoint_urls_match_gateway_contract() {
        assert_eq!(
            usage_url("https://code.byteask.ai/byteask/v1"),
            "https://code.byteask.ai/byteask/v1/usage"
        );
        assert_eq!(
            referral_url("https://code.byteask.ai/byteask/v1/"),
            "https://code.byteask.ai/byteask/v1/referral-code"
        );
        assert_eq!(
            usage_url("https://code.byteask.ai/byteask"),
            "https://code.byteask.ai/byteask/v1/usage"
        );
        assert_eq!(
            upgrade_url("https://code.byteask.ai/byteask"),
            "https://code.byteask.ai/byteask/v1/upgrade"
        );
        assert_eq!(
            voucher_url("https://code.byteask.ai/byteask/v1"),
            "https://code.byteask.ai/byteask/v1/voucher"
        );
    }

    #[test]
    fn reset_times_are_twenty_four_hour_and_relative_first() {
        assert_eq!(human_reset(0), "now");
        let soon = human_reset(47 * 60);
        assert!(soon.starts_with("47m ("), "{soon}");
        // Upstream's `format_reset_timestamp`: `HH:MM`, no am/pm.
        assert!(!soon.contains("PM") && !soon.contains("AM"), "{soon}");
        let week = human_reset(6 * 86_400 + 23 * 3600);
        assert!(week.starts_with("6d 23h ("), "{week}");
        assert!(
            week.contains(" on "),
            "a multi-day reset names the day: {week}"
        );
    }

    #[test]
    fn display_plan_capitalizes_and_handles_empty() {
        assert_eq!(display_plan("free"), "Free");
        assert_eq!(display_plan("pro"), "Pro");
        assert_eq!(display_plan("max"), "Max");
        assert_eq!(display_plan("ultra"), "Ultra");
        assert_eq!(display_plan(""), "—");
    }

    // --- clipboard ---------------------------------------------------------

    #[tokio::test]
    async fn a_confirmed_copy_is_reported_in_the_footer() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        chat.open_byteask_pane(referral_spec(&referral_info(), /*animations*/ false));
        let _ = transcript_lines(&mut rx);

        chat.finish_byteask_pane_copy(
            ByteAskPane::Referral,
            Ok(crate::clipboard_copy::CopyStatus::Confirmed),
        );

        assert!(chat.byteask_pane_open(ByteAskPane::Referral));
        assert_eq!(
            transcript_lines(&mut rx),
            Vec::<String>::new(),
            "a copy confirmation belongs in the footer, not the transcript"
        );
    }

    #[tokio::test]
    async fn a_failed_copy_keeps_the_value_on_screen_and_says_what_to_do() {
        let (mut chat, _tx, mut rx, _op_rx) = make_chatwidget_manual_with_sender().await;
        chat.open_byteask_pane(referral_spec(&referral_info(), /*animations*/ false));
        let _ = transcript_lines(&mut rx);

        chat.finish_byteask_pane_copy(ByteAskPane::Referral, Err("no clipboard".to_string()));

        assert!(chat.byteask_pane_open(ByteAskPane::Referral));
        assert_eq!(transcript_lines(&mut rx), Vec::<String>::new());
    }
}
