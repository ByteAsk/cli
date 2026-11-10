pub mod cache;
mod catalog;
pub mod collaboration_mode_presets;
pub(crate) mod config;
pub mod manager;
pub mod model_info;
pub mod model_presets;
mod originator;
pub mod test_support;

pub use catalog::CatalogModel;
pub use catalog::ModelOrigin;
pub use codex_protocol::auth::AuthMode;
pub use config::ModelsManagerConfig;
pub use originator::ModelCatalogOriginator;

/// Load the bundled model catalog shipped with `codex-models-manager`.
pub fn bundled_models_response()
-> std::result::Result<codex_protocol::openai_models::ModelsResponse, serde_json::Error> {
    serde_json::from_str(include_str!("../models.json"))
}

/// Convert the client version string to a whole version string (e.g. "1.2.3-alpha.4" -> "1.2.3").
///
/// ByteAsk override: the OpenAI ChatGPT backend gates the
/// model list on this version — at ByteAsk's baked `0.1.0` it returns an EMPTY
/// list and rejects every model. The resolution
/// lives in `codex_login::default_client::resolve_reported_version` (shared with
/// the User-Agent so the two never disagree): the wrapper sends
/// `BYTEASK_CODEX_VERSION_OVERRIDE=auto` for a ChatGPT-subscription session, which
/// maps to the real upstream Codex version stamped into the binary at build time
/// — so it tracks upstream on every rebuild with no literal to tune. A concrete
/// value still wins as a manual escape hatch; unset => the real build version,
/// unchanged for every managed/BYOK/gateway path.
pub fn client_version_to_whole() -> String {
    let env_override = std::env::var("BYTEASK_CODEX_VERSION_OVERRIDE").ok();
    codex_login::default_client::resolve_reported_version(env_override.as_deref(), &build_version())
}

fn build_version() -> String {
    format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    )
}

#[cfg(test)]
mod version_override_tests {
    use super::build_version;
    use codex_login::default_client::UPSTREAM_CODEX_VERSION;
    use codex_login::default_client::resolve_reported_version;

    #[test]
    fn auto_maps_to_upstream_and_never_the_rejected_build_version() {
        // The whole point of the override: `auto` (what the wrapper sends) must resolve to a
        // real upstream Codex version, NOT the baked 0.1.0 the ChatGPT backend rejects.
        assert_eq!(
            resolve_reported_version(Some("auto"), &build_version()),
            UPSTREAM_CODEX_VERSION
        );
        assert_eq!(
            resolve_reported_version(Some("UPSTREAM"), &build_version()),
            UPSTREAM_CODEX_VERSION
        );
        assert_ne!(
            resolve_reported_version(Some("auto"), &build_version()),
            build_version()
        );
        assert!(UPSTREAM_CODEX_VERSION.split('.').count() >= 2);
        assert_ne!(UPSTREAM_CODEX_VERSION, build_version());
    }

    #[test]
    fn explicit_wins_blank_and_absent_fall_back() {
        // A concrete literal is still honoured (manual escape hatch)...
        assert_eq!(
            resolve_reported_version(Some("0.153.4"), &build_version()),
            "0.153.4"
        );
        // ...but blank / unset stay on the managed build version.
        assert_eq!(
            resolve_reported_version(Some("   "), &build_version()),
            build_version()
        );
        assert_eq!(
            resolve_reported_version(None, &build_version()),
            build_version()
        );
    }
}
