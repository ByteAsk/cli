//! `[results_store]` in the policy file: how much the local
//! results store keeps.
//!
//! ```toml
//! [results_store]
//! max_days = 90          # results older than this are deleted (open changes excepted)
//! max_bytes = 2147483648 # total size of stored tool output
//! ```
//!
//! A missing key, a bad value or an untrusted file gives the default. Unlike
//! the rest of the policy file these settings relax nothing, so a file inside
//! a workspace is still read.

use std::path::Path;

/// Results are kept this many days by default.
pub const DEFAULT_MAX_DAYS: u64 = 90;
/// Stored tool output is capped at this many bytes by default (2 GB).
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_DAYS_RANGE: std::ops::RangeInclusive<u64> = 1..=3650;
const MIN_BYTES: u64 = 1024 * 1024;

/// The effective limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultsStoreLimits {
    pub max_days: u64,
    pub max_bytes: u64,
}

impl Default for ResultsStoreLimits {
    fn default() -> Self {
        Self {
            max_days: DEFAULT_MAX_DAYS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Read `[results_store]` from `$BYTEASK_HOME/policy.toml`. Never fails: every
/// problem gives the default for that key and a plain-language reason.
pub fn results_store_limits(home: &Path) -> (ResultsStoreLimits, Vec<String>) {
    let mut limits = ResultsStoreLimits::default();
    let mut problems = Vec::new();
    let text = match super::read_trusted(&super::path(home)) {
        Ok(Some(text)) => text,
        Ok(None) => return (limits, problems),
        Err(problem) => {
            problems.push(problem);
            return (limits, problems);
        }
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        problems.push(
            "the policy file could not be read, so the results store keeps its defaults."
                .to_string(),
        );
        return (limits, problems);
    };
    let Some(section) = doc.get("results_store").and_then(toml::Value::as_table) else {
        return (limits, problems);
    };
    if let Some(value) = section.get("max_days") {
        match value.as_integer().and_then(|days| u64::try_from(days).ok()) {
            Some(days) if MAX_DAYS_RANGE.contains(&days) => limits.max_days = days,
            _ => problems.push(format!(
                "results_store.max_days must be a whole number from 1 to 3650; using {DEFAULT_MAX_DAYS}."
            )),
        }
    }
    if let Some(value) = section.get("max_bytes") {
        match value.as_integer().and_then(|bytes| u64::try_from(bytes).ok()) {
            Some(bytes) if bytes >= MIN_BYTES => limits.max_bytes = bytes,
            _ => problems.push(format!(
                "results_store.max_bytes must be a whole number of at least {MIN_BYTES}; using {DEFAULT_MAX_BYTES}."
            )),
        }
    }
    (limits, problems)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn write(home: &Path, text: &str) {
        std::fs::write(super::super::path(home), text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                super::super::path(home),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }

    #[test]
    fn no_file_or_no_section_gives_the_defaults() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            results_store_limits(home.path()),
            (
                ResultsStoreLimits {
                    max_days: 90,
                    max_bytes: 2 * 1024 * 1024 * 1024
                },
                Vec::new()
            )
        );
        write(home.path(), "version = 1\n");
        assert_eq!(
            results_store_limits(home.path()).0,
            ResultsStoreLimits::default()
        );
    }

    #[test]
    fn both_limits_are_configurable() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            "version = 1\n[results_store]\nmax_days = 30\nmax_bytes = 5242880\n",
        );
        assert_eq!(
            results_store_limits(home.path()),
            (
                ResultsStoreLimits {
                    max_days: 30,
                    max_bytes: 5 * 1024 * 1024
                },
                Vec::new()
            )
        );
    }

    #[test]
    fn a_bad_value_keeps_the_default_and_says_why() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            "[results_store]\nmax_days = 0\nmax_bytes = \"lots\"\n",
        );
        let (limits, problems) = results_store_limits(home.path());
        assert_eq!(limits, ResultsStoreLimits::default());
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_write_is_ignored() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "[results_store]\nmax_days = 5\n");
        std::fs::set_permissions(
            super::super::path(home.path()),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let (limits, problems) = results_store_limits(home.path());
        assert_eq!(limits, ResultsStoreLimits::default());
        assert_eq!(problems.len(), 1);
    }
}
