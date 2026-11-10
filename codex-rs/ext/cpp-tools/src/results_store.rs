//! The local results store is one component, the `codex-results-store` crate: the
//! finish gate's verdicts with their freshness, every tool
//! result by change, the clean-up and the value report. This module keeps the
//! names the gate, the hooks and `check_status` use, and `default_home`, whose test variant
//! keeps this crate's unit tests out of the developer's real store.

use std::path::PathBuf;

pub(crate) use ::results_store::Entry;
pub(crate) use ::results_store::Freshness;
pub(crate) use ::results_store::all;
pub(crate) use ::results_store::describe;
pub(crate) use ::results_store::fingerprint;
pub(crate) use ::results_store::freshness;
pub(crate) use ::results_store::latest;
pub(crate) use ::results_store::now_unix;
pub(crate) use ::results_store::record;
pub(crate) use ::results_store::record_for_session;

/// Tests never write the developer's real results store.
#[cfg(test)]
pub(crate) fn default_home() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("byteask-test-results-{}", std::process::id())))
}

/// `$BYTEASK_HOME`, else `~/.byteask`.
#[cfg(not(test))]
pub(crate) fn default_home() -> Option<PathBuf> {
    std::env::var_os("BYTEASK_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".byteask")))
}
