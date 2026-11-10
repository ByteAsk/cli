//! One plain sentence for a comparison, for people and for the agent reading a tool result.

use serde_json::Value;

use crate::compare::Comparison;
use crate::compare::DiffKind;
use crate::compare::Difference;

const MAX_VALUE_CHARS: usize = 120;

/// Compact JSON for a value, shortened for a sentence.
pub fn short(v: &Value) -> String {
    let text = v.to_string();
    if text.chars().count() <= MAX_VALUE_CHARS {
        text
    } else {
        let cut: String = text.chars().take(MAX_VALUE_CHARS).collect();
        format!("{cut}…")
    }
}

fn key_part(d: &Difference) -> String {
    d.key
        .as_ref()
        .map(|k| format!(" (key {})", short(k)))
        .unwrap_or_default()
}

fn field(path: &str) -> String {
    if path.is_empty() {
        "the record".to_string()
    } else {
        format!("field {path}")
    }
}

/// Describe a comparison in one sentence. `a`/`b` name the two sides ("Python", "C++", "old",
/// "new", ...).
pub fn sentence(c: &Comparison, a: &str, b: &str) -> String {
    let Some(d) = &c.first_difference else {
        return format!(
            "Same output: {} records from each side compared ({}).",
            c.records_a, c.rules
        );
    };
    let at_a = d.index_a.map(|i| format!("record {i}")).unwrap_or_default();
    let at_b = d.index_b.map(|i| format!("record {i}")).unwrap_or_default();
    let key = key_part(d);
    let val = |v: &Option<Value>| v.as_ref().map(short).unwrap_or_default();
    match d.kind {
        DiffKind::ValueDiffers | DiffKind::TypeDiffers => format!(
            "First difference at {at_a}{key}: {} is {} in {a} and {} in {b} (rule: {}).",
            field(&d.path),
            val(&d.a),
            val(&d.b),
            d.rule
        ),
        DiffKind::MissingInB => format!(
            "First difference at {at_a}{key}: {} is {} in {a} and absent in {b}.",
            field(&d.path),
            val(&d.a)
        ),
        DiffKind::ExtraInB => format!(
            "First difference at {at_a}{key}: {} is absent in {a} and {} in {b}.",
            field(&d.path),
            val(&d.b)
        ),
        DiffKind::RecordMissingInB => {
            let rec = val(&d.record_a);
            if d.closest.is_some() || c.unmatched_a > 0 {
                format!(
                    "{b} has no record matching {a}'s {at_a}{key}: {rec}{}.",
                    closest_part(d, a, b)
                )
            } else {
                format!(
                    "{b} ended after {} records; {a} continues with {at_a}{key}: {rec}.",
                    c.records_b
                )
            }
        }
        DiffKind::RecordExtraInB => {
            let rec = val(&d.record_b);
            if d.closest.is_some() || c.unmatched_b > 0 {
                format!(
                    "{a} has no record matching {b}'s {at_b}{key}: {rec}{}.",
                    closest_part(d, a, b)
                )
            } else {
                format!(
                    "{a} ended after {} records; {b} continues with {at_b}{key}: {rec}.",
                    c.records_a
                )
            }
        }
        DiffKind::GroupKeyDiffers => format!(
            "Groups out of step at {a} {at_a} / {b} {at_b}: {} is {} in {a} and {} in {b}.",
            field(&d.path),
            val(&d.a),
            val(&d.b)
        ),
    }
}

fn closest_part(d: &Difference, a: &str, b: &str) -> String {
    match &d.closest {
        None => String::new(),
        Some(fd) => format!(
            " (closest candidate differs at {}: {} in {a}, {} in {b})",
            if fd.path.is_empty() { "the whole record" } else { &fd.path },
            fd.a.as_ref().map(short).unwrap_or_else(|| "absent".to_string()),
            fd.b.as_ref().map(short).unwrap_or_else(|| "absent".to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::compare_records;
    use crate::rules::Rules;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn say(a: &[Value], b: &[Value], rules: &str) -> String {
        let r = Rules::from_json(rules).expect("rules");
        sentence(&compare_records(a, b, &r).expect("compare"), "Python", "C++")
    }

    #[test]
    fn sentences_name_the_record_the_field_both_values_and_the_rule() {
        assert_eq!(
            say(&[json!({"q":1})], &[json!({"q":1})], "{}"),
            "Same output: 1 records from each side compared (exact order; all values exact)."
        );
        assert_eq!(
            say(&[json!({"seq":4,"q":1})], &[json!({"seq":4,"q":2})], r#"{"key":"/seq"}"#),
            "First difference at record 0 (key 4): field /q is 1 in Python and 2 in C++ (rule: exact)."
        );
        assert_eq!(
            say(&[json!(1), json!(2)], &[json!(1)], "{}"),
            "C++ ended after 1 records; Python continues with record 1: 2."
        );
        assert_eq!(
            say(&[json!(1)], &[json!(1), json!({"x":3})], "{}"),
            "Python ended after 1 records; C++ continues with record 1: {\"x\":3}."
        );
        assert_eq!(
            say(&[json!({"a":1})], &[json!({})], "{}"),
            "First difference at record 0: field /a is 1 in Python and absent in C++."
        );
    }

    #[test]
    fn any_order_sentences_name_the_closest_candidate() {
        let s = say(
            &[json!({"id":1,"q":5})],
            &[json!({"id":1,"q":6})],
            r#"{"ordering":"unordered"}"#,
        );
        assert_eq!(
            s,
            "C++ has no record matching Python's record 0: {\"id\":1,\"q\":5} (closest candidate differs at /q: 5 in Python, 6 in C++)."
        );
    }

    #[test]
    fn long_values_are_shortened() {
        let long = json!("x".repeat(500));
        assert!(short(&long).chars().count() <= MAX_VALUE_CHARS + 1);
        assert!(short(&long).ends_with('…'));
    }
}
