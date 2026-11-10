#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;
use crate::update_versions::UpdateDecision;
use crate::update_versions::VersionFeed;
use crate::update_versions::parse_version_feed;
use crate::update_versions::update_decision;
use crate::updates_cache::VersionInfo;
use crate::updates_cache::read_version_info;
use crate::updates_cache::version_filepath;
use crate::version::update_check_version;
use chrono::Duration;
use chrono::Utc;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_login::default_client::default_headers;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// ByteAsk's own update feed. Never GitHub's releases/latest: that flips the moment
/// release assets are published, before the matching wrapper and helpers are served,
/// which prompted users onto an incomplete release.
const DEFAULT_VERSION_URL: &str = "https://code.byteask.ai/version.json";
const VERSION_URL_ENV: &str = "BYTEASK_VERSION_URL";

/// A newer release exists: shown as a one-line notice inside the session.
pub fn get_upgrade_version(config: &Config) -> Option<String> {
    match cached_decision(config) {
        UpdateDecision::Notice(version) => Some(version),
        UpdateDecision::None | UpdateDecision::Required(_) => None,
    }
}

/// The installed version is below the feed's `min_supported`: the blocking prompt
/// before the session opens.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    match cached_decision(config) {
        UpdateDecision::Required(version) => Some(version),
        UpdateDecision::None | UpdateDecision::Notice(_) => None,
    }
}

fn cached_decision(config: &Config) -> UpdateDecision {
    if !config.check_for_update_on_startup {
        return UpdateDecision::None;
    }
    let Some(current) = update_check_version() else {
        return UpdateDecision::None;
    };
    let version_file = version_filepath(config);
    let info = read_version_info(&version_file).ok();
    let stale = info
        .as_ref()
        .is_none_or(|info| info.last_checked_at < Utc::now() - Duration::hours(20));
    if stale {
        refresh_in_background(version_file, config.http_client_factory());
    }
    let Some(info) = info else {
        return UpdateDecision::None;
    };
    update_decision(
        Some(current),
        &VersionFeed {
            latest: info.latest_version,
            min_supported: info.min_supported,
            notes_url: None,
        },
    )
}

/// Refresh the cached feed without blocking startup; this launch acts on the
/// previous cache and the next launch on what this one fetched. Any failure is
/// ignored (fail-open: no notice, no prompt).
fn refresh_in_background(version_file: PathBuf, http_client_factory: HttpClientFactory) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        if let Err(err) = check_for_update(&version_file, http_client_factory).await {
            tracing::debug!("update check failed (ignored): {err}");
        }
    });
}

async fn check_for_update(
    version_file: &Path,
    http_client_factory: HttpClientFactory,
) -> anyhow::Result<()> {
    let url = std::env::var(VERSION_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_VERSION_URL.to_string());
    let client_pool = RouteAwareClientPool::with_chatgpt_cloudflare_cookies(
        http_client_factory,
        ClientRouteClass::Other,
    )
    .with_legacy_custom_ca_fallback();
    let body = client_pool
        .get(url)
        .headers(default_headers())
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    let feed = parse_version_feed(&body.to_string())
        .ok_or_else(|| anyhow::anyhow!("malformed update feed"))?;
    let info = VersionInfo {
        latest_version: feed.latest,
        min_supported: feed.min_supported,
        last_checked_at: Utc::now(),
    };
    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}
