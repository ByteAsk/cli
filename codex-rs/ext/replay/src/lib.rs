//! One shared replay-and-compare library.
//!
//! The verify gate's replay tier, `port_check` and the perf lab all need the
//! same step: take recorded inputs, run two versions of a program on them, and
//! find the first place their outputs differ. Getting that comparison right
//! (record order, number rounding, missing or extra records, where exactly the
//! first difference is) is the hard part, so it lives here once, with its own
//! tests, instead of three copies that drift apart.
//!
//! The pieces:
//! - [`record`]: read recorded inputs and outputs (JSON Lines, plain lines,
//!   and length-prefixed binary frames such as exchange feed captures).
//! - [`rules`]: what "same" means: record ordering, ignored fields, and number
//!   tolerances per field. Default is exact on everything.
//! - [`compare`]: compare two record streams under those rules and report the
//!   first difference with its record index, field path and both values.
//! - [`run`]: feed one recorded input to two programs under a fixed
//!   environment and a time limit, then compare what they printed.
//! - [`report`]: one plain sentence for a comparison.
//!
//! This library reports "same", "different" or "not run"; it does not decide
//! what a pass is. Callers map those onto the shared result format.

pub mod compare;
pub mod path;
pub mod record;
pub mod report;
pub mod rules;
pub mod run;

pub use compare::Comparison;
pub use compare::CompareError;
pub use compare::DiffKind;
pub use compare::Difference;
pub use compare::FieldDiff;
pub use compare::Side;
pub use compare::compare_records;
pub use compare::compare_streams;
pub use compare::diff_values;
pub use record::RecordError;
pub use record::RecordResult;
pub use rules::Rules;
pub use rules::RulesError;
pub use rules::RulesSpec;
