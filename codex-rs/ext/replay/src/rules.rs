//! What "same output" means: the comparison rules.
//!
//! The default is the strictest comparison: same records, same order, every value equal.
//! Everything looser has to be asked for by name, so a rules file is also a record of exactly
//! which differences a check chose to accept.
//!
//! JSON form (every field optional):
//! ```json
//! {
//!   "ordering": "exact" | "unordered" | {"grouped_by": "/seq"},
//!   "ignore":   ["/wall_ns"],
//!   "numbers":  [{"path": "/diag/*", "abs": 1e-9, "rel": 1e-12, "ulps": 4}],
//!   "key":      "/seq",
//!   "context":  3
//! }
//! ```
//! - `ordering`: `exact` compares record N of A with record N of B. `unordered` requires the
//!   same records in any order (both streams are held in memory). `grouped_by` treats consecutive
//!   records with the same value at that path as one group: groups must appear in the same order
//!   with the same values, records inside a group may appear in any order (typical: every output
//!   caused by one input event carries that event's sequence number).
//! - `ignore`: paths whose whole subtree never counts (present, absent or different).
//! - `numbers`: per-path tolerances; the FIRST rule whose path matches decides. Two numbers are
//!   equal when they are the same value, or when ANY nonzero limit holds: `|a-b| <= abs`,
//!   `|a-b| <= rel * max(|a|,|b|)`, or the two doubles are at most `ulps` representable values
//!   apart. A tolerance only ever applies between two numbers; a number never equals a string.
//! - `key`: the field naming which input produced a record. It changes nothing in the comparison;
//!   it is copied into the difference report so the reader can find that input.
//! - `context`: how many equal records before the first difference to keep in the report.
//!
//! Exact numbers compare by mathematical value: `1` equals `1.0`, `-0.0` equals `0`, and two
//! integers are compared as integers (never rounded through a double), so `9007199254740993`
//! does not equal `9007199254740992`.

use serde::Deserialize;
use serde::Serialize;
use std::fmt;

use crate::path::PathPattern;

/// The rules as written (JSON). Compile with [`Rules::from_spec`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RulesSpec {
    #[serde(default)]
    pub ordering: OrderingSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub numbers: Vec<NumberRuleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderingSpec {
    #[default]
    Exact,
    Unordered,
    GroupedBy(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NumberRuleSpec {
    pub path: String,
    #[serde(default)]
    pub abs: f64,
    #[serde(default)]
    pub rel: f64,
    #[serde(default)]
    pub ulps: u64,
}

/// A rules file that cannot be used, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulesError(pub String);

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RulesError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ordering {
    Exact,
    Unordered,
    /// The JSON Pointer (one field, no wildcard) whose value groups records.
    GroupedBy(String),
}

/// One compiled number tolerance.
#[derive(Clone, Debug, PartialEq)]
pub struct NumberRule {
    pub path: PathPattern,
    pub abs: f64,
    pub rel: f64,
    pub ulps: u64,
}

impl NumberRule {
    /// `exact`, or for example `abs<=1e-9 or ulps<=4`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.abs > 0.0 {
            parts.push(format!("abs<={:e}", self.abs));
        }
        if self.rel > 0.0 {
            parts.push(format!("rel<={:e}", self.rel));
        }
        if self.ulps > 0 {
            parts.push(format!("ulps<={}", self.ulps));
        }
        if parts.is_empty() {
            "exact".to_string()
        } else {
            parts.join(" or ")
        }
    }

    pub fn is_exact(&self) -> bool {
        self.abs == 0.0 && self.rel == 0.0 && self.ulps == 0
    }
}

/// Compiled, validated rules.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    pub ordering: Ordering,
    pub ignore: Vec<PathPattern>,
    pub numbers: Vec<NumberRule>,
    pub key: Option<String>,
    pub context: usize,
    /// Any-order matching refuses above this many pairwise checks in one look-alike group
    /// (default [`UNORDERED_PAIR_LIMIT`]; not part of the JSON form).
    pub pair_limit: usize,
    spec: RulesSpec,
}

pub const DEFAULT_CONTEXT: usize = 3;
const MAX_CONTEXT: usize = 100;

/// Inside one group of look-alike records (records that differ only in tolerance fields), the
/// unordered matcher builds an exact pairing that costs `n * m` value checks. Above this many
/// candidate pairs it refuses with a reason instead of guessing: a greedy pairing could report a
/// difference that an optimal pairing would not, and a guessed result is worse than none.
pub const UNORDERED_PAIR_LIMIT: usize = 4_000_000;

impl Default for Rules {
    fn default() -> Self {
        Self::exact()
    }
}

impl Rules {
    /// Exact on everything, records in order.
    pub fn exact() -> Self {
        Self {
            ordering: Ordering::Exact,
            ignore: Vec::new(),
            numbers: Vec::new(),
            key: None,
            context: DEFAULT_CONTEXT,
            pair_limit: UNORDERED_PAIR_LIMIT,
            spec: RulesSpec::default(),
        }
    }

    /// Parse and validate rules from JSON text.
    pub fn from_json(text: &str) -> Result<Self, RulesError> {
        let spec: RulesSpec = serde_json::from_str(text)
            .map_err(|e| RulesError(format!("rules are not valid: {e}")))?;
        Self::from_spec(&spec)
    }

    /// Validate a spec. Every refusal says which entry is wrong and why.
    pub fn from_spec(spec: &RulesSpec) -> Result<Self, RulesError> {
        let ordering = match &spec.ordering {
            OrderingSpec::Exact => Ordering::Exact,
            OrderingSpec::Unordered => Ordering::Unordered,
            OrderingSpec::GroupedBy(raw) => {
                Ordering::GroupedBy(single_field(raw, "grouped_by")?)
            }
        };
        let mut ignore = Vec::new();
        for raw in &spec.ignore {
            ignore.push(PathPattern::parse(raw).map_err(|e| RulesError(format!("ignore: {e}")))?);
        }
        let mut numbers = Vec::new();
        for (i, rule) in spec.numbers.iter().enumerate() {
            let path = PathPattern::parse(&rule.path)
                .map_err(|e| RulesError(format!("numbers[{i}]: {e}")))?;
            for (name, v) in [("abs", rule.abs), ("rel", rule.rel)] {
                if !v.is_finite() || v < 0.0 {
                    return Err(RulesError(format!(
                        "numbers[{i}] ({}): {name} must be a finite number >= 0, got {v}",
                        rule.path
                    )));
                }
            }
            if rule.rel >= 1.0 {
                return Err(RulesError(format!(
                    "numbers[{i}] ({}): rel {} would accept numbers of any size (it must be < 1)",
                    rule.path, rule.rel
                )));
            }
            numbers.push(NumberRule {
                path,
                abs: rule.abs,
                rel: rule.rel,
                ulps: rule.ulps,
            });
        }
        let key = spec
            .key
            .as_deref()
            .map(|raw| single_field(raw, "key"))
            .transpose()?;
        let context = spec.context.unwrap_or(DEFAULT_CONTEXT);
        if context > MAX_CONTEXT {
            return Err(RulesError(format!(
                "context {context} is too large (at most {MAX_CONTEXT} records)"
            )));
        }
        Ok(Self {
            ordering,
            ignore,
            numbers,
            key,
            context,
            pair_limit: UNORDERED_PAIR_LIMIT,
            spec: spec.clone(),
        })
    }

    /// The rules as written, for evidence in reports.
    pub fn spec(&self) -> &RulesSpec {
        &self.spec
    }

    /// The same rules with every number tolerance removed. Used for "does the same version
    /// reproduce itself" checks, where any difference, however small, means something in the
    /// run is not under control.
    pub fn without_tolerances(&self) -> Self {
        let mut spec = self.spec.clone();
        spec.numbers.clear();
        Self {
            numbers: Vec::new(),
            spec,
            ..self.clone()
        }
    }

    pub fn is_ignored(&self, path: &[String]) -> bool {
        self.ignore.iter().any(|p| p.matches(path))
    }

    /// The first number rule whose path matches, if it allows any slack at all.
    pub fn tolerance_at(&self, path: &[String]) -> Option<&NumberRule> {
        self.numbers
            .iter()
            .find(|r| r.path.matches(path))
            .filter(|r| !r.is_exact())
    }

    pub fn has_tolerances(&self) -> bool {
        self.numbers.iter().any(|r| !r.is_exact())
    }

    /// One line naming the rules in force, for reports.
    pub fn describe(&self) -> String {
        let order = match &self.ordering {
            Ordering::Exact => "exact order".to_string(),
            Ordering::Unordered => "any order".to_string(),
            Ordering::GroupedBy(p) => format!("grouped by {p}, any order inside a group"),
        };
        let mut parts = vec![order];
        let tol: Vec<String> = self
            .numbers
            .iter()
            .filter(|r| !r.is_exact())
            .map(|r| format!("{} {}", r.path, r.describe()))
            .collect();
        if tol.is_empty() {
            parts.push("all values exact".to_string());
        } else {
            parts.push(format!("number tolerance: {}", tol.join("; ")));
        }
        if !self.spec.ignore.is_empty() {
            parts.push(format!("ignored: {}", self.spec.ignore.join(", ")));
        }
        parts.join("; ")
    }
}

fn single_field(raw: &str, what: &str) -> Result<String, RulesError> {
    let pat = PathPattern::parse(raw).map_err(|e| RulesError(format!("{what}: {e}")))?;
    if pat.has_wildcard() || raw.is_empty() {
        return Err(RulesError(format!(
            "{what} {raw:?} must name one field (no '*', not the whole record)"
        )));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn p(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_rules_are_exact_in_order() {
        let r = Rules::from_json("{}").expect("parse");
        assert_eq!(r, Rules::exact());
        assert_eq!(r.describe(), "exact order; all values exact");
        assert!(!r.has_tolerances());
    }

    #[test]
    fn all_three_orderings_parse() {
        assert_eq!(
            Rules::from_json(r#"{"ordering":"unordered"}"#).expect("parse").ordering,
            Ordering::Unordered
        );
        assert_eq!(
            Rules::from_json(r#"{"ordering":{"grouped_by":"/seq"}}"#)
                .expect("parse")
                .ordering,
            Ordering::GroupedBy("/seq".to_string())
        );
    }

    #[test]
    fn typos_and_bad_tolerances_are_refused_never_read_as_exact() {
        for (bad, want) in [
            (r#"{"tolerance": 1e-9}"#, "unknown field"),
            (r#"{"numbers":[{"path":"/x","tol":1}]}"#, "unknown field"),
            (r#"{"numbers":[{"path":"/x","abs":-1}]}"#, "abs must be a finite number >= 0"),
            (r#"{"numbers":[{"path":"/x","rel":1.0}]}"#, "must be < 1"),
            (r#"{"numbers":[{"path":"x","abs":1}]}"#, "must start with '/'"),
            (r#"{"key":"/a/*"}"#, "must name one field"),
            (r#"{"ordering":{"grouped_by":""}}"#, "must name one field"),
            (r#"{"ordering":"sorted"}"#, "unknown variant"),
            (r#"{"context":1000}"#, "too large"),
        ] {
            let err = Rules::from_json(bad).expect_err(bad);
            assert!(err.0.contains(want), "{bad}: {err}");
        }
    }

    #[test]
    fn first_matching_number_rule_wins_and_an_exact_rule_shadows_a_broad_one() {
        let r = Rules::from_json(
            r#"{"numbers":[{"path":"/diag/qty"},{"path":"/diag/pnl","abs":0.5},{"path":"/diag/*","ulps":4}]}"#,
        )
        .expect("parse");
        assert_eq!(r.tolerance_at(&p(&["diag", "qty"])), None);
        assert_eq!(
            r.tolerance_at(&p(&["diag", "pnl"])).map(NumberRule::describe),
            Some("abs<=5e-1".to_string())
        );
        assert_eq!(
            r.tolerance_at(&p(&["diag", "edge"])).map(NumberRule::describe),
            Some("ulps<=4".to_string())
        );
        let strict = r.without_tolerances();
        assert_eq!(strict.tolerance_at(&p(&["diag", "pnl"])), None);
        assert!(strict.spec().numbers.is_empty());
    }
}
