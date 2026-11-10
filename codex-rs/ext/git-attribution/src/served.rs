//! ByteAsk: the server-side switch for commit attribution.
//!
//! It lives as a top-level `byteask_attribution` object in the served model catalog and is read
//! the way `byteask_efficiency` is: tolerantly, and never as part of the typed catalog, so a bad
//! value can never stop the catalog from loading (the engine would not start). Attribution is OFF
//! unless the catalog says `{"enabled": true}` exactly. A missing, malformed or wrong-typed value,
//! or no catalog at all, leaves it off.

use std::sync::OnceLock;

use serde_json::Value;

pub const SERVED_KEY: &str = "byteask_attribution";

static SERVED: OnceLock<bool> = OnceLock::new();

/// Whether the catalog's JSON text turns attribution on. Never fails.
pub fn enabled_in_catalog_json(text: &str) -> bool {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    root.get(SERVED_KEY)
        .and_then(|switch| switch.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Record the switch from the catalog the engine loaded. Once per process, like the efficiency
/// policy: later calls are ignored, so a running session keeps its value until restart.
pub fn install_from_catalog_json(text: &str) {
    install_into(&SERVED, text);
}

/// Whether the served catalog turned attribution on. Off when no catalog was loaded.
pub fn served_enabled() -> bool {
    read(&SERVED)
}

fn install_into(cell: &OnceLock<bool>, text: &str) {
    let _ = cell.set(enabled_in_catalog_json(text));
}

fn read(cell: &OnceLock<bool>) -> bool {
    cell.get().copied().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_true_turns_attribution_on() {
        assert!(enabled_in_catalog_json(
            r#"{"models":[],"byteask_attribution":{"enabled":true}}"#
        ));
        for off in [
            "not json at all",
            r#"{"models":[]}"#,
            r#"{"byteask_attribution":7}"#,
            r#"{"byteask_attribution":{"enabled":"yes"}}"#,
            r#"{"byteask_attribution":{"enabled":1}}"#,
            r#"{"byteask_attribution":{}}"#,
            r#"{"byteask_attribution":{"enabled":false}}"#,
            r#"{"byteask_attribution":{"enabled":null}}"#,
            "",
        ] {
            assert!(!enabled_in_catalog_json(off), "{off}");
        }
    }

    #[test]
    fn the_first_install_wins_and_nothing_installed_reads_off() {
        let cell = OnceLock::new();
        assert!(!read(&cell));
        install_into(&cell, r#"{"byteask_attribution":{"enabled":true}}"#);
        install_into(&cell, r#"{"byteask_attribution":{"enabled":false}}"#);
        assert!(read(&cell));
    }
}
