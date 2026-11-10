/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The ByteAsk **client release** version — what the user actually has installed.
///
/// The wrapper exports `BYTEASK_CLIENT_VERSION` = the released version. The shipped
/// engine binary carries its own crate version, which lags the release train (crate
/// `0.1.0` vs release `v0.1.10`), so preferring the wrapper's value keeps things
/// honest without rebuilding the engine on every release bump. Falls back to the
/// compile-time crate version for direct/dev runs.
///
/// Used for **display** (session header, `/status`, statusline) *and* for the
/// **update check**: comparing the crate version against the release feed made
/// `byteask doctor` report "newer version is available" on every run, including the
/// newest build, because the two are different version spaces.
///
/// Resolved once and leaked into a `&'static str` so the many Display call sites
/// stay unchanged in kind.
pub fn display_version() -> &'static str {
    use std::sync::LazyLock;
    static V: LazyLock<&'static str> =
        LazyLock::new(|| resolve_display_version(std::env::var("BYTEASK_CLIENT_VERSION").ok()));
    *V
}

/// The released client version to run the update check against, or `None` when the
/// engine runs without the wrapper. Then only the crate version (`0.1.0`) is known,
/// which is a different version space from releases and would always read as behind.
pub fn update_check_version() -> Option<&'static str> {
    use std::sync::LazyLock;
    static V: LazyLock<Option<String>> = LazyLock::new(|| {
        resolve_update_check_version(std::env::var("BYTEASK_CLIENT_VERSION").ok())
    });
    V.as_deref()
}

fn resolve_update_check_version(env_value: Option<String>) -> Option<String> {
    env_value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Pure core of [`display_version`], split out so the fallback rules are testable
/// without mutating process env — the public function memoizes in a `LazyLock`, so
/// it can only ever observe one value per process.
fn resolve_display_version(env_value: Option<String>) -> &'static str {
    match env_value {
        Some(v) if !v.trim().is_empty() => Box::leak(v.trim().to_owned().into_boxed_str()),
        _ => CODEX_CLI_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn prefers_the_wrapper_supplied_client_version() {
        assert_eq!(
            resolve_display_version(Some("0.1.10".to_string())),
            "0.1.10"
        );
        assert_eq!(
            resolve_display_version(Some("  0.1.10 \n".to_string())),
            "0.1.10",
            "the wrapper's value is trimmed before use"
        );
    }

    /// The update check must only run against a real released version. Without the
    /// wrapper's export the engine only knows its crate version (`0.1.0`), which
    /// would read as "behind" every feed and prompt forever.
    #[test]
    fn update_check_version_is_known_only_when_the_wrapper_supplies_it() {
        assert_eq!(
            resolve_update_check_version(Some(" 0.1.12 ".to_string())),
            Some("0.1.12".to_string())
        );
        assert_eq!(resolve_update_check_version(None), None);
        assert_eq!(resolve_update_check_version(Some("  ".to_string())), None);
    }

    #[test]
    fn falls_back_to_the_crate_version_when_unset_or_blank() {
        // A direct/dev run of the engine has no wrapper, so the crate version is
        // the only thing available - and an empty export must not win over it.
        assert_eq!(resolve_display_version(None), CODEX_CLI_VERSION);
        assert_eq!(
            resolve_display_version(Some(String::new())),
            CODEX_CLI_VERSION
        );
        assert_eq!(
            resolve_display_version(Some("   ".to_string())),
            CODEX_CLI_VERSION
        );
    }
}
