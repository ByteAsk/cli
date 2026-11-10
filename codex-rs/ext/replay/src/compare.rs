//! Compare two record streams under [`Rules`] and report the first difference.
//!
//! "First" is defined so every caller gets the same answer:
//! - exact order: the lowest record index where the two streams disagree, or where one stream
//!   ends early;
//! - inside a record: fields are visited in sorted key order and arrays in index order, so the
//!   first differing path is the smallest such path in that order;
//! - any order / grouped: the lowest-index record (in either stream) that has no partner. The
//!   pairing is a maximum matching, not a greedy one, so a record that could be paired is never
//!   reported as unmatched just because a look-alike took its partner first.
//!
//! Exact-order and grouped comparison stream: memory stays bounded by one group, so recordings of
//! millions of records are fine. Any-order comparison holds both streams in memory.

use serde::Serialize;
use serde_json::Number;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt;

use crate::path::render;
use crate::record::RecordError;
use crate::record::RecordResult;
use crate::rules::NumberRule;
use crate::rules::Ordering;
use crate::rules::Rules;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    A,
    B,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::A => "A",
            Side::B => "B",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Same type, different value.
    ValueDiffers,
    /// For example a number in A and a string in B.
    TypeDiffers,
    /// A field or array element present in A is absent in B.
    MissingInB,
    /// A field or array element present in B is absent in A.
    ExtraInB,
    /// A record of A has no partner in B (B ended early, or nothing in B matches it).
    RecordMissingInB,
    /// A record of B has no partner in A.
    RecordExtraInB,
    /// Grouped comparison: the two streams' groups at this position have different keys.
    GroupKeyDiffers,
}

/// A difference inside one pair of records.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FieldDiff {
    pub kind: DiffKind,
    /// JSON Pointer inside the record ("" = the whole record).
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<Value>,
    /// The rule that decided: "exact" or the tolerance that was exceeded.
    pub rule: String,
}

/// The first difference between two streams.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Difference {
    pub kind: DiffKind,
    /// 0-based record index in A / B (absent when that side has no record here).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_a: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_b: Option<u64>,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<Value>,
    pub rule: String,
    /// The value of the rules' `key` field in the record (A's, else B's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_a: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_b: Option<Value>,
    /// Up to `rules.context` equal records of A just before the difference.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_a: Vec<Value>,
    /// Any-order / grouped only: how the unmatched record differs from the earliest unmatched
    /// record on the other side, as a hint for the reader.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closest: Option<FieldDiff>,
}

/// The result of a comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub same: bool,
    /// Records read from each side (all of them when `read_to_end`; the comparison stops at the
    /// first difference otherwise).
    pub records_a: u64,
    pub records_b: u64,
    pub read_to_end: bool,
    /// Any-order / grouped: how many records found no partner (in the reported group).
    pub unmatched_a: u64,
    pub unmatched_b: u64,
    /// One line naming the rules that decided.
    pub rules: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_difference: Option<Difference>,
}

/// A comparison that could not be completed. Never read this as "same" or "different".
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum CompareError {
    /// A record in one stream could not be read.
    BadRecord { side: Side, error: RecordError },
    /// Any-order matching would need more pairwise checks than the limit allows.
    TooManyLookAlikes { pairs: u64, limit: u64 },
}

impl fmt::Display for CompareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompareError::BadRecord { side, error } => {
                write!(f, "stream {side} could not be read: {error}")
            }
            CompareError::TooManyLookAlikes { pairs, limit } => write!(
                f,
                "any-order matching would need {pairs} pairwise checks among records that differ only in tolerance fields (limit {limit}); compare grouped_by a field that splits them, or in exact order"
            ),
        }
    }
}

impl std::error::Error for CompareError {}

// ---------------------------------------------------------------------------------------------
// One pair of values.
// ---------------------------------------------------------------------------------------------

/// The first difference between two records under the rules, or `None` when they are the same.
pub fn diff_values(a: &Value, b: &Value, rules: &Rules) -> Option<FieldDiff> {
    let mut path = Vec::new();
    walk(&mut path, a, b, rules)
}

fn walk(path: &mut Vec<String>, a: &Value, b: &Value, rules: &Rules) -> Option<FieldDiff> {
    if rules.is_ignored(path) {
        return None;
    }
    let field = |kind, a: Option<&Value>, b: Option<&Value>, rule: String| FieldDiff {
        kind,
        path: render(path),
        a: a.cloned(),
        b: b.cloned(),
        rule,
    };
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let rule = rules.tolerance_at(path);
            if numbers_equal(x, y, rule) {
                None
            } else {
                Some(field(
                    DiffKind::ValueDiffers,
                    Some(a),
                    Some(b),
                    rule.map_or_else(|| "exact".to_string(), NumberRule::describe),
                ))
            }
        }
        (Value::Object(ma), Value::Object(mb)) => {
            let keys: BTreeSet<&String> = ma.keys().chain(mb.keys()).collect();
            for key in keys {
                path.push(key.clone());
                let found = match (ma.get(key), mb.get(key)) {
                    (Some(va), Some(vb)) => walk(path, va, vb, rules),
                    (Some(va), None) if !rules.is_ignored(path) => Some(FieldDiff {
                        kind: DiffKind::MissingInB,
                        path: render(path),
                        a: Some(va.clone()),
                        b: None,
                        rule: "exact".to_string(),
                    }),
                    (None, Some(vb)) if !rules.is_ignored(path) => Some(FieldDiff {
                        kind: DiffKind::ExtraInB,
                        path: render(path),
                        a: None,
                        b: Some(vb.clone()),
                        rule: "exact".to_string(),
                    }),
                    _ => None,
                };
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        (Value::Array(xa), Value::Array(xb)) => {
            for i in 0..xa.len().max(xb.len()) {
                path.push(i.to_string());
                let found = match (xa.get(i), xb.get(i)) {
                    (Some(va), Some(vb)) => walk(path, va, vb, rules),
                    (Some(va), None) if !rules.is_ignored(path) => Some(FieldDiff {
                        kind: DiffKind::MissingInB,
                        path: render(path),
                        a: Some(va.clone()),
                        b: None,
                        rule: "exact".to_string(),
                    }),
                    (None, Some(vb)) if !rules.is_ignored(path) => Some(FieldDiff {
                        kind: DiffKind::ExtraInB,
                        path: render(path),
                        a: None,
                        b: Some(vb.clone()),
                        rule: "exact".to_string(),
                    }),
                    _ => None,
                };
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Null, Value::Null) => {
            if a == b {
                None
            } else {
                Some(field(
                    DiffKind::ValueDiffers,
                    Some(a),
                    Some(b),
                    "exact".to_string(),
                ))
            }
        }
        _ => Some(field(
            DiffKind::TypeDiffers,
            Some(a),
            Some(b),
            "exact".to_string(),
        )),
    }
}

enum ExactNum {
    Int(i128),
    Float(f64),
}

fn exact_of(n: &Number) -> ExactNum {
    if let Some(i) = n.as_i64() {
        ExactNum::Int(i128::from(i))
    } else if let Some(u) = n.as_u64() {
        ExactNum::Int(i128::from(u))
    } else {
        // Without serde_json's arbitrary_precision every non-integer is an f64.
        ExactNum::Float(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// Largest float magnitude below which an integral f64 converts to i128 exactly.
const I128_SAFE: f64 = 1.0e38;

fn float_equals_int(f: f64, i: i128) -> bool {
    f.is_finite() && f.fract() == 0.0 && f.abs() < I128_SAFE && (f as i128) == i
}

/// Same mathematical value: integers as integers, `1 == 1.0`, `-0.0 == 0`.
fn exact_number_eq(x: &Number, y: &Number) -> bool {
    match (exact_of(x), exact_of(y)) {
        (ExactNum::Int(a), ExactNum::Int(b)) => a == b,
        (ExactNum::Float(a), ExactNum::Float(b)) => a == b,
        (ExactNum::Int(i), ExactNum::Float(f)) | (ExactNum::Float(f), ExactNum::Int(i)) => {
            float_equals_int(f, i)
        }
    }
}

fn numbers_equal(x: &Number, y: &Number, rule: Option<&NumberRule>) -> bool {
    if exact_number_eq(x, y) {
        return true;
    }
    let Some(rule) = rule else {
        return false;
    };
    let (Some(fa), Some(fb)) = (x.as_f64(), y.as_f64()) else {
        return false;
    };
    let diff = (fa - fb).abs();
    (rule.abs > 0.0 && diff <= rule.abs)
        || (rule.rel > 0.0 && diff <= rule.rel * fa.abs().max(fb.abs()))
        || (rule.ulps > 0 && ulps_apart(fa, fb) <= rule.ulps)
}

/// Map a double onto a line where adjacent representable values are adjacent integers.
fn ordered_bits(f: f64) -> i64 {
    let bits = f.to_bits() as i64;
    if bits < 0 { i64::MIN - bits } else { bits }
}

/// How many representable doubles apart two finite values are.
fn ulps_apart(a: f64, b: f64) -> u64 {
    let d = i128::from(ordered_bits(a)) - i128::from(ordered_bits(b));
    u64::try_from(d.unsigned_abs()).unwrap_or(u64::MAX)
}

/// A canonical text for a number such that two numbers have the same text exactly when
/// [`exact_number_eq`] holds.
fn canonical_number(n: &Number, out: &mut String) {
    match exact_of(n) {
        ExactNum::Int(i) => out.push_str(&i.to_string()),
        ExactNum::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < I128_SAFE => {
            out.push_str(&(f as i128).to_string());
        }
        ExactNum::Float(f) => out.push_str(&format!("{f:?}")),
    }
}

/// Canonical text of a record with ignored subtrees removed and every number that falls under a
/// tolerance replaced by `#`. Two records that are equal under the rules always have the same
/// signature, so only records sharing a signature can ever be partners.
fn signature(v: &Value, rules: &Rules) -> String {
    let mut out = String::new();
    let mut path = Vec::new();
    sig(&mut path, v, rules, &mut out);
    out
}

fn sig(path: &mut Vec<String>, v: &Value, rules: &Rules, out: &mut String) {
    // Only the root can arrive here ignored: ignored keys and elements are dropped by the
    // caller, because an ignored field must not count whether it is present or not.
    if rules.is_ignored(path) {
        out.push('~');
        return;
    }
    match v {
        Value::Number(n) => {
            if rules.tolerance_at(path).is_some() {
                out.push('#');
            } else {
                canonical_number(n, out);
            }
        }
        Value::Object(map) => {
            out.push('{');
            let keys: BTreeSet<&String> = map.keys().collect();
            for key in keys {
                path.push(key.clone());
                if !rules.is_ignored(path) {
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    if let Some(child) = map.get(key) {
                        sig(path, child, rules, out);
                    }
                    out.push(',');
                }
                path.pop();
            }
            out.push('}');
        }
        Value::Array(items) => {
            // Index-tagged and without the length: arrays of different lengths are equal when
            // every index past the shorter one is ignored, exactly as `diff_values` decides.
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                path.push(i.to_string());
                if !rules.is_ignored(path) {
                    out.push_str(&i.to_string());
                    out.push(':');
                    sig(path, item, rules, out);
                    out.push(',');
                }
                path.pop();
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Streams.
// ---------------------------------------------------------------------------------------------

fn take<I: Iterator<Item = RecordResult>>(
    it: &mut I,
    side: Side,
) -> Result<Option<Value>, CompareError> {
    match it.next() {
        None => Ok(None),
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(error)) => Err(CompareError::BadRecord { side, error }),
    }
}

fn key_of(rules: &Rules, a: Option<&Value>, b: Option<&Value>) -> Option<Value> {
    let ptr = rules.key.as_deref()?;
    a.and_then(|v| v.pointer(ptr))
        .or_else(|| b.and_then(|v| v.pointer(ptr)))
        .cloned()
}

fn push_context(ctx: &mut VecDeque<Value>, v: Value, cap: usize) {
    if cap == 0 {
        return;
    }
    if ctx.len() == cap {
        ctx.pop_front();
    }
    ctx.push_back(v);
}

/// Compare two record streams under the rules.
pub fn compare_streams<A, B>(a: A, b: B, rules: &Rules) -> Result<Comparison, CompareError>
where
    A: IntoIterator<Item = RecordResult>,
    B: IntoIterator<Item = RecordResult>,
{
    match &rules.ordering {
        Ordering::Exact => compare_exact(a.into_iter(), b.into_iter(), rules),
        Ordering::Unordered => {
            let va = a
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CompareError::BadRecord {
                    side: Side::A,
                    error,
                })?;
            let vb = b
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| CompareError::BadRecord {
                    side: Side::B,
                    error,
                })?;
            compare_unordered(&va, &vb, rules)
        }
        Ordering::GroupedBy(key) => compare_grouped(a.into_iter(), b.into_iter(), key, rules),
    }
}

/// Compare two in-memory record lists under the rules.
pub fn compare_records(
    a: &[Value],
    b: &[Value],
    rules: &Rules,
) -> Result<Comparison, CompareError> {
    compare_streams(a.iter().cloned().map(Ok), b.iter().cloned().map(Ok), rules)
}

fn compare_exact<A, B>(mut a: A, mut b: B, rules: &Rules) -> Result<Comparison, CompareError>
where
    A: Iterator<Item = RecordResult>,
    B: Iterator<Item = RecordResult>,
{
    let mut index: u64 = 0;
    let mut ctx = VecDeque::new();
    let result = |records_a, records_b, read_to_end, first: Option<Difference>| Comparison {
        same: first.is_none(),
        records_a,
        records_b,
        read_to_end,
        unmatched_a: 0,
        unmatched_b: 0,
        rules: rules.describe(),
        first_difference: first,
    };
    loop {
        let ra = take(&mut a, Side::A)?;
        let rb = take(&mut b, Side::B)?;
        match (ra, rb) {
            (None, None) => return Ok(result(index, index, true, None)),
            (Some(va), Some(vb)) => {
                if let Some(fd) = diff_values(&va, &vb, rules) {
                    let diff = Difference {
                        kind: fd.kind,
                        index_a: Some(index),
                        index_b: Some(index),
                        path: fd.path,
                        a: fd.a,
                        b: fd.b,
                        rule: fd.rule,
                        key: key_of(rules, Some(&va), Some(&vb)),
                        record_a: Some(va),
                        record_b: Some(vb),
                        context_a: ctx.into_iter().collect(),
                        closest: None,
                    };
                    return Ok(result(index + 1, index + 1, false, Some(diff)));
                }
                push_context(&mut ctx, va, rules.context);
                index += 1;
            }
            (Some(va), None) => {
                let diff = Difference {
                    kind: DiffKind::RecordMissingInB,
                    index_a: Some(index),
                    index_b: None,
                    path: String::new(),
                    a: None,
                    b: None,
                    rule: "exact".to_string(),
                    key: key_of(rules, Some(&va), None),
                    record_a: Some(va),
                    record_b: None,
                    context_a: ctx.into_iter().collect(),
                    closest: None,
                };
                return Ok(result(index + 1, index, false, Some(diff)));
            }
            (None, Some(vb)) => {
                let diff = Difference {
                    kind: DiffKind::RecordExtraInB,
                    index_a: None,
                    index_b: Some(index),
                    path: String::new(),
                    a: None,
                    b: None,
                    rule: "exact".to_string(),
                    key: key_of(rules, None, Some(&vb)),
                    record_a: None,
                    record_b: Some(vb),
                    context_a: ctx.into_iter().collect(),
                    closest: None,
                };
                return Ok(result(index, index + 1, false, Some(diff)));
            }
        }
    }
}

/// Which records of `a` and `b` found no partner, both in ascending index order.
struct Unmatched {
    a: Vec<usize>,
    b: Vec<usize>,
}

/// Maximum matching between two record lists under the rules.
fn match_multiset(a: &[Value], b: &[Value], rules: &Rules) -> Result<Unmatched, CompareError> {
    let mut buckets: BTreeMap<String, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, v) in a.iter().enumerate() {
        buckets.entry(signature(v, rules)).or_default().0.push(i);
    }
    for (j, v) in b.iter().enumerate() {
        buckets.entry(signature(v, rules)).or_default().1.push(j);
    }
    let tolerant = rules.has_tolerances();
    let mut un_a = Vec::new();
    let mut un_b = Vec::new();
    for (sig_text, (ia, ib)) in buckets {
        if !tolerant || !sig_text.contains('#') {
            // Same signature with no tolerance leaf means exactly equal: pair by count.
            let k = ia.len().min(ib.len());
            un_a.extend_from_slice(&ia[k..]);
            un_b.extend_from_slice(&ib[k..]);
            continue;
        }
        let pairs = (ia.len() as u64) * (ib.len() as u64);
        if pairs > rules.pair_limit as u64 {
            return Err(CompareError::TooManyLookAlikes {
                pairs,
                limit: rules.pair_limit as u64,
            });
        }
        let adj: Vec<Vec<usize>> = ia
            .iter()
            .map(|&i| {
                ib.iter()
                    .enumerate()
                    .filter(|&(_, &j)| diff_values(&a[i], &b[j], rules).is_none())
                    .map(|(local, _)| local)
                    .collect()
            })
            .collect();
        let owner_of_b = max_matching(ia.len(), ib.len(), &adj);
        let mut a_matched = vec![false; ia.len()];
        for (local_b, owner) in owner_of_b.iter().enumerate() {
            match owner {
                Some(local_a) => a_matched[*local_a] = true,
                None => un_b.push(ib[local_b]),
            }
        }
        for (local_a, matched) in a_matched.iter().enumerate() {
            if !matched {
                un_a.push(ia[local_a]);
            }
        }
    }
    un_a.sort_unstable();
    un_b.sort_unstable();
    Ok(Unmatched { a: un_a, b: un_b })
}

/// Kuhn's augmenting-path maximum bipartite matching. Returns, for each right vertex, its left
/// partner. Deterministic: vertices are tried in index order.
fn max_matching(left: usize, right: usize, adj: &[Vec<usize>]) -> Vec<Option<usize>> {
    let mut owner: Vec<Option<usize>> = vec![None; right];
    let mut has_partner = vec![false; left];
    // Greedy first pass: most records pair with the first free look-alike.
    for (l, edges) in adj.iter().enumerate() {
        if let Some(&r) = edges.iter().find(|&&r| owner[r].is_none()) {
            owner[r] = Some(l);
            has_partner[l] = true;
        }
    }
    for (l, partnered) in has_partner.iter_mut().enumerate() {
        if *partnered {
            continue;
        }
        let mut seen = vec![false; right];
        if augment(l, adj, &mut owner, &mut seen) {
            *partnered = true;
        }
    }
    owner
}

fn augment(l: usize, adj: &[Vec<usize>], owner: &mut [Option<usize>], seen: &mut [bool]) -> bool {
    for &r in &adj[l] {
        if seen[r] {
            continue;
        }
        seen[r] = true;
        let free = match owner[r] {
            None => true,
            Some(other) => augment(other, adj, owner, seen),
        };
        if free {
            owner[r] = Some(l);
            return true;
        }
    }
    false
}

/// Build the report for the first unmatched record of a pair of lists. `base_a`/`base_b` turn
/// list positions into stream indices.
fn unmatched_difference(
    a: &[Value],
    b: &[Value],
    un: &Unmatched,
    base_a: u64,
    base_b: u64,
    rules: &Rules,
    context: Vec<Value>,
) -> Option<Difference> {
    let first_a = un.a.first().copied();
    let first_b = un.b.first().copied();
    let report_a = match (first_a, first_b) {
        (Some(i), Some(j)) => base_a + i as u64 <= base_b + j as u64,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => return None,
    };
    let closest = match (first_a, first_b) {
        (Some(i), Some(j)) => diff_values(&a[i], &b[j], rules),
        _ => None,
    };
    let (kind, index_a, index_b, record_a, record_b) = if report_a {
        let i = first_a?;
        (
            DiffKind::RecordMissingInB,
            Some(base_a + i as u64),
            None,
            Some(a[i].clone()),
            None,
        )
    } else {
        let j = first_b?;
        (
            DiffKind::RecordExtraInB,
            None,
            Some(base_b + j as u64),
            None,
            Some(b[j].clone()),
        )
    };
    Some(Difference {
        kind,
        index_a,
        index_b,
        path: String::new(),
        a: None,
        b: None,
        rule: "no partner record under these rules".to_string(),
        key: key_of(rules, record_a.as_ref(), record_b.as_ref()),
        record_a,
        record_b,
        context_a: context,
        closest,
    })
}

fn compare_unordered(a: &[Value], b: &[Value], rules: &Rules) -> Result<Comparison, CompareError> {
    let un = match_multiset(a, b, rules)?;
    let first = unmatched_difference(a, b, &un, 0, 0, rules, Vec::new());
    Ok(Comparison {
        same: first.is_none(),
        records_a: a.len() as u64,
        records_b: b.len() as u64,
        read_to_end: true,
        unmatched_a: un.a.len() as u64,
        unmatched_b: un.b.len() as u64,
        rules: rules.describe(),
        first_difference: first,
    })
}

/// Reads consecutive runs of records that share a key value.
struct Groups<I> {
    inner: I,
    side: Side,
    key: String,
    pending: Option<Value>,
}

impl<I: Iterator<Item = RecordResult>> Groups<I> {
    fn new(inner: I, side: Side, key: &str) -> Self {
        Self {
            inner,
            side,
            key: key.to_string(),
            pending: None,
        }
    }

    fn key_of(&self, v: &Value) -> Value {
        v.pointer(&self.key).cloned().unwrap_or(Value::Null)
    }

    fn next_group(&mut self) -> Result<Option<(Value, Vec<Value>)>, CompareError> {
        let first = match self.pending.take() {
            Some(v) => v,
            None => match take(&mut self.inner, self.side)? {
                Some(v) => v,
                None => return Ok(None),
            },
        };
        let key = self.key_of(&first);
        let key_sig = signature(&key, &Rules::exact());
        let mut members = vec![first];
        while let Some(v) = take(&mut self.inner, self.side)? {
            if signature(&self.key_of(&v), &Rules::exact()) == key_sig {
                members.push(v);
            } else {
                self.pending = Some(v);
                break;
            }
        }
        Ok(Some((key, members)))
    }
}

fn compare_grouped<A, B>(a: A, b: B, key: &str, rules: &Rules) -> Result<Comparison, CompareError>
where
    A: Iterator<Item = RecordResult>,
    B: Iterator<Item = RecordResult>,
{
    let mut ga = Groups::new(a, Side::A, key);
    let mut gb = Groups::new(b, Side::B, key);
    let (mut start_a, mut start_b) = (0u64, 0u64);
    let mut ctx = VecDeque::new();
    let done =
        |records_a, records_b, read_to_end, unmatched: (u64, u64), first: Option<Difference>| {
            Comparison {
                same: first.is_none(),
                records_a,
                records_b,
                read_to_end,
                unmatched_a: unmatched.0,
                unmatched_b: unmatched.1,
                rules: rules.describe(),
                first_difference: first,
            }
        };
    loop {
        let x = ga.next_group()?;
        let y = gb.next_group()?;
        match (x, y) {
            (None, None) => return Ok(done(start_a, start_b, true, (0, 0), None)),
            (Some((ka, ra)), Some((kb, rb))) => {
                let end_a = start_a + ra.len() as u64;
                let end_b = start_b + rb.len() as u64;
                if signature(&ka, &Rules::exact()) != signature(&kb, &Rules::exact()) {
                    let diff = Difference {
                        kind: DiffKind::GroupKeyDiffers,
                        index_a: Some(start_a),
                        index_b: Some(start_b),
                        path: key.to_string(),
                        a: Some(ka),
                        b: Some(kb),
                        rule: "groups must appear in the same order".to_string(),
                        key: key_of(rules, ra.first(), rb.first()),
                        record_a: ra.first().cloned(),
                        record_b: rb.first().cloned(),
                        context_a: ctx.into_iter().collect(),
                        closest: None,
                    };
                    return Ok(done(end_a, end_b, false, (0, 0), Some(diff)));
                }
                let un = match_multiset(&ra, &rb, rules)?;
                if let Some(diff) = unmatched_difference(
                    &ra,
                    &rb,
                    &un,
                    start_a,
                    start_b,
                    rules,
                    ctx.iter().cloned().collect(),
                ) {
                    let counts = (un.a.len() as u64, un.b.len() as u64);
                    return Ok(done(end_a, end_b, false, counts, Some(diff)));
                }
                for v in ra {
                    push_context(&mut ctx, v, rules.context);
                }
                start_a = end_a;
                start_b = end_b;
            }
            (Some((_, ra)), None) => {
                let first = ra.first().cloned();
                let diff = Difference {
                    kind: DiffKind::RecordMissingInB,
                    index_a: Some(start_a),
                    index_b: None,
                    path: String::new(),
                    a: None,
                    b: None,
                    rule: "exact".to_string(),
                    key: key_of(rules, first.as_ref(), None),
                    record_a: first,
                    record_b: None,
                    context_a: ctx.into_iter().collect(),
                    closest: None,
                };
                return Ok(done(
                    start_a + ra.len() as u64,
                    start_b,
                    false,
                    (0, 0),
                    Some(diff),
                ));
            }
            (None, Some((_, rb))) => {
                let first = rb.first().cloned();
                let diff = Difference {
                    kind: DiffKind::RecordExtraInB,
                    index_a: None,
                    index_b: Some(start_b),
                    path: String::new(),
                    a: None,
                    b: None,
                    rule: "exact".to_string(),
                    key: key_of(rules, None, first.as_ref()),
                    record_a: None,
                    record_b: first,
                    context_a: ctx.into_iter().collect(),
                    closest: None,
                };
                return Ok(done(
                    start_a,
                    start_b + rb.len() as u64,
                    false,
                    (0, 0),
                    Some(diff),
                ));
            }
        }
    }
}

#[cfg(test)]
#[path = "compare_tests.rs"]
mod tests;
