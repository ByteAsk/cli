//! ByteAsk harness controls:
//!
//! - remaining-time notes at 50% and 75% of `[byteask_harness] time_budget_secs`
//!   ([`budget`]);
//! - the constraint ledger (`constraint_ledger = true`): the user's constraints on the change
//!   in one block that survives compaction, checked against the final diff ([`ledger`]).
//!
//! Every control is off unless its key is set; the keys are documented in
//! `codex_core::byteask_harness`, which also holds the lean-prefix and strict-patch
//! switches the core reads itself.

pub mod budget;
mod extension;
pub mod ledger;

pub use extension::install;
