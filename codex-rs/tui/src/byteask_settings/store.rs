//! Read/merge access to `~/.byteask/byok-config.json` — the store the settings
//! UI mutates.
//!
//! This file is the source of truth the **sidecar re-reads on every request**
//! (`byok_sidecar.py::_load_config`), which is what lets most settings edits
//! apply without restarting the engine. The shell wrapper writes the same file
//! (`_byok_merge`), so the two must agree on shape:
//!
//! ```json
//! { "keys": {"openai": "...", "anthropic": "..."},
//!   "jwt": "...", "local_token": "...", "gateway": "https://...",
//!   "endpoints": {"<alias>": {"base_url": "...", "wire": "chat", ...}} }
//! ```
//!
//! Edits are merged into the parsed document rather than serialized from a typed
//! struct: the wrapper, `byteask_models.py`, and the sidecar each own fields the
//! TUI does not model, and rewriting from a struct would silently drop them.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde_json::Map;
use serde_json::Value;

/// Providers the user can supply their own key for. Mirrors the wrapper's
/// `_scr_provname` and the sidecar's `_provider_of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum KeyProvider {
    OpenAi,
    Anthropic,
    Gemini,
}

impl KeyProvider {
    /// Every provider, in the order the keys menu lists them.
    pub(crate) const ALL: [KeyProvider; 3] = [
        KeyProvider::OpenAi,
        KeyProvider::Anthropic,
        KeyProvider::Gemini,
    ];

    /// The identifier used as the JSON key under `keys`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            KeyProvider::OpenAi => "openai",
            KeyProvider::Anthropic => "anthropic",
            KeyProvider::Gemini => "gemini",
        }
    }

    /// Human-readable name for menus and messages.
    pub(crate) fn label(self) -> &'static str {
        match self {
            KeyProvider::OpenAi => "OpenAI",
            KeyProvider::Anthropic => "Anthropic",
            KeyProvider::Gemini => "Gemini",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.id() == id)
    }
}

/// One registered self-hosted / cloud-preset endpoint, surfaced as `self/<alias>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub alias: String,
    pub base_url: String,
    /// Preset this was added from (`ollama`, `openrouter`, …), when it came from one.
    pub provider_id: Option<String>,
    /// `false` once `byteask models test` proved the model cannot emit structured
    /// tool calls — the agent loop silently does nothing on such a model.
    pub tools_ok: Option<bool>,
}

/// The parsed `byok-config.json` document plus the path it came from.
#[derive(Debug, Clone)]
pub(crate) struct ByokConfig {
    path: PathBuf,
    root: Map<String, Value>,
}

impl ByokConfig {
    pub(crate) fn path_for(codex_home: &Path) -> PathBuf {
        codex_home.join("byok-config.json")
    }

    /// Read the config, treating "absent" and "unparseable" alike as empty.
    ///
    /// Fail-soft is deliberate: the settings screen must still open (and let the
    /// user re-add a key) when the file is missing or was corrupted.
    pub(crate) fn load(codex_home: &Path) -> Self {
        let path = Self::path_for(codex_home);
        let root = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        Self { path, root }
    }

    fn object(&self, field: &str) -> Option<&Map<String, Value>> {
        self.root.get(field).and_then(Value::as_object)
    }

    fn string(&self, field: &str) -> Option<&str> {
        self.root
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Providers with a non-empty key stored, in menu order.
    pub(crate) fn keyed_providers(&self) -> Vec<KeyProvider> {
        let Some(keys) = self.object("keys") else {
            return Vec::new();
        };
        KeyProvider::ALL
            .into_iter()
            .filter(|provider| {
                keys.get(provider.id())
                    .and_then(Value::as_str)
                    .is_some_and(|key| !key.trim().is_empty())
            })
            .collect()
    }

    /// The stored session token. Only the presence/expiry is ever used by the UI.
    pub(crate) fn jwt(&self) -> Option<&str> {
        self.string("jwt")
    }

    /// Registered endpoints, sorted by alias so the menu order is stable.
    pub(crate) fn endpoints(&self) -> Vec<Endpoint> {
        let Some(endpoints) = self.object("endpoints") else {
            return Vec::new();
        };
        let sorted: BTreeMap<&String, &Value> = endpoints.iter().collect();
        sorted
            .into_iter()
            .filter_map(|(alias, value)| {
                let entry = value.as_object()?;
                let base_url = entry
                    .get("base_url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| !url.is_empty())?;
                Some(Endpoint {
                    alias: alias.clone(),
                    base_url: base_url.to_string(),
                    provider_id: entry
                        .get("provider_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                    tools_ok: entry.get("tools_ok").and_then(Value::as_bool),
                })
            })
            .collect()
    }

    /// Store (or, with an empty `key`, clear) one provider key and persist.
    pub(crate) fn set_key(&mut self, provider: KeyProvider, key: &str) -> Result<()> {
        let keys = self
            .root
            .entry("keys".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !keys.is_object() {
            *keys = Value::Object(Map::new());
        }
        // `is_object` is checked/forced immediately above.
        #[expect(clippy::expect_used)]
        let keys = keys
            .as_object_mut()
            .expect("keys was just forced to object");

        let key = key.trim();
        if key.is_empty() {
            keys.remove(provider.id());
        } else {
            keys.insert(provider.id().to_string(), Value::String(key.to_string()));
        }
        self.save()
    }

    /// Write the document back atomically with owner-only permissions.
    ///
    /// The file holds live API keys and a session token, so the 0600 mode is set
    /// on the temp file *before* it is renamed into place — never a window where
    /// a world-readable file exists at the real path.
    fn save(&self) -> Result<()> {
        let dir = self
            .path
            .parent()
            .context("byok-config.json has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let mut temp = tempfile::Builder::new()
            .prefix(".byok-config")
            .suffix(".tmp")
            .tempfile_in(dir)
            .with_context(|| format!("creating a temp file in {}", dir.display()))?;
        restrict_permissions(temp.as_file())?;

        serde_json::to_writer(&mut temp, &Value::Object(self.root.clone()))
            .context("serializing byok-config.json")?;
        use std::io::Write as _;
        temp.flush().context("flushing byok-config.json")?;
        temp.as_file()
            .sync_all()
            .context("syncing byok-config.json")?;
        temp.persist(&self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_permissions(file: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("restricting byok-config.json permissions")
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &std::fs::File) -> Result<()> {
    // Windows inherits the user-profile ACL, which is already owner-only.
    Ok(())
}

/// Extract the `sub` (email) claim from a JWT without verifying the signature.
///
/// The gateway is the only party that can validate this token; the TUI reads it
/// purely to label the account in the UI, so an undecodable token simply shows
/// as "not signed in" rather than being treated as an error.
pub(crate) fn jwt_subject(token: &str) -> Option<String> {
    let claims = jwt_claims(token)?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sub| !sub.is_empty())
        .map(str::to_string)
}

/// True when the token carries an `exp` in the past.
///
/// Matches the wrapper's `_is_signed_in`: an *undecodable* token is treated as
/// still valid (fail-safe), because wrongly declaring a working session expired
/// would push the user through a pointless re-login.
pub(crate) fn jwt_expired(token: &str, now_unix: i64) -> bool {
    jwt_claims(token)
        .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
        .is_some_and(|exp| exp < now_unix)
}

fn jwt_claims(token: &str) -> Option<Map<String, Value>> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    match serde_json::from_slice::<Value>(&decoded).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn write_config(dir: &Path, body: &str) {
        std::fs::write(ByokConfig::path_for(dir), body).expect("write config");
    }

    #[test]
    fn missing_or_corrupt_config_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ByokConfig::load(dir.path());
        assert!(config.keyed_providers().is_empty());
        assert_eq!(config.jwt(), None);

        write_config(dir.path(), "{ this is not json");
        let config = ByokConfig::load(dir.path());
        assert!(config.keyed_providers().is_empty());
    }

    #[test]
    fn reads_keys_endpoints_and_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"{
                "keys": {"anthropic": "sk-ant-x", "gemini": "  ", "openai": "sk-o"},
                "jwt": "tok", "gateway": "https://code.byteask.ai",
                "endpoints": {
                    "zed": {"base_url": "http://localhost:1", "tools_ok": false},
                    "abe": {"base_url": "https://api.groq.com/openai/v1", "provider_id": "groq"},
                    "bad": {"base_url": "   "}
                }
            }"#,
        );
        let config = ByokConfig::load(dir.path());

        // Whitespace-only keys do not count as configured.
        assert_eq!(
            config.keyed_providers(),
            vec![KeyProvider::OpenAi, KeyProvider::Anthropic]
        );
        assert_eq!(config.jwt(), Some("tok"));

        let endpoints = config.endpoints();
        // Sorted by alias, and the base_url-less entry is dropped.
        assert_eq!(
            endpoints
                .iter()
                .map(|e| e.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["abe", "zed"]
        );
        assert_eq!(endpoints[0].provider_id.as_deref(), Some("groq"));
        assert_eq!(endpoints[1].tools_ok, Some(false));
    }

    #[test]
    fn set_key_preserves_unmodelled_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"{"keys":{"openai":"sk-o"},"local_token":"lt","future_field":{"a":1}}"#,
        );

        let mut config = ByokConfig::load(dir.path());
        config
            .set_key(KeyProvider::Anthropic, "sk-ant-new")
            .expect("set key");

        let raw = std::fs::read_to_string(ByokConfig::path_for(dir.path())).expect("read back");
        let value: Value = serde_json::from_str(&raw).expect("parse");
        assert_eq!(value["keys"]["openai"], "sk-o");
        assert_eq!(value["keys"]["anthropic"], "sk-ant-new");
        // Fields owned by the wrapper / sidecar survive a TUI write.
        assert_eq!(value["local_token"], "lt");
        assert_eq!(value["future_field"]["a"], 1);
    }

    #[test]
    fn empty_key_removes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(dir.path(), r#"{"keys":{"openai":"sk-o","gemini":"g"}}"#);

        let mut config = ByokConfig::load(dir.path());
        config
            .set_key(KeyProvider::OpenAi, "  ")
            .expect("clear key");

        let config = ByokConfig::load(dir.path());
        assert_eq!(config.keyed_providers(), vec![KeyProvider::Gemini]);
    }

    #[test]
    fn saved_config_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = ByokConfig::load(dir.path());
        config.set_key(KeyProvider::Gemini, "secret").expect("save");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(ByokConfig::path_for(dir.path()))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "byok-config.json must stay 0600");
        }
    }

    #[test]
    fn jwt_subject_and_expiry() {
        // {"sub":"a@b.com","exp":1000}
        let token = "h.eyJzdWIiOiJhQGIuY29tIiwiZXhwIjoxMDAwfQ.sig";
        assert_eq!(jwt_subject(token).as_deref(), Some("a@b.com"));
        assert!(jwt_expired(token, 1001));
        assert!(!jwt_expired(token, 999));
    }

    #[test]
    fn undecodable_jwt_is_treated_as_valid() {
        // Fail-safe: never force a re-login just because the token cannot be parsed.
        assert_eq!(jwt_subject("garbage"), None);
        assert!(!jwt_expired("garbage", i64::MAX));
    }
}
