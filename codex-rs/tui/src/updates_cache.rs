use crate::legacy_core::config::Config;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// The last update feed ByteAsk fetched (see `update_versions::VersionFeed`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct VersionInfo {
    #[serde(default)]
    pub(crate) latest_version: Option<String>,
    #[serde(default)]
    pub(crate) min_supported: Option<String>,
    // ISO-8601 timestamp (RFC3339)
    pub(crate) last_checked_at: DateTime<Utc>,
}

/// ByteAsk-named so it never reads upstream's `version.json`, which cached a GitHub
/// release tag rather than ByteAsk's own feed.
const VERSION_FILENAME: &str = "byteask-version.json";

#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

#[cfg(test)]
#[path = "updates_cache_tests.rs"]
mod tests;
