//! The normalized market data model the harness replays, and the small value types shared by the
//! simulator and the protocol.
//!
//! A recording is JSON Lines, one market event per line, in time order:
//! ```text
//! {"ts":34200000000123,"type":"add","sym":"AAPL","id":123,"side":"B","px":1500000,"qty":100}
//! {"ts":...,"type":"cancel","id":123,"qty":40}          partial cancel of a resting order
//! {"ts":...,"type":"delete","id":123}                   the order leaves the book
//! {"ts":...,"type":"exec","id":123,"qty":60}            a trade against a resting order
//! {"ts":...,"type":"exec","id":123,"qty":60,"px":1500100}   ... at a price other than its limit
//! {"ts":...,"type":"replace","id":123,"new_id":124,"px":1500200,"qty":80}
//! {"ts":...,"type":"trade","sym":"AAPL","side":"B","px":1500100,"qty":10}   non-displayed print
//! ```
//! Prices are integers in the instrument's price unit (ticks or fixed-point, e.g. 1/10000 of a
//! dollar as in Nasdaq ITCH). This is deliberate: an exact comparison of orders is only
//! meaningful when prices are exact, and a port that rounds a float price differently is a real
//! difference that must be caught, not hidden. `ts` is integer nanoseconds. Extra fields are
//! allowed and ignored, so a decoder can keep its own provenance (message type, match number).

use serde_json::Value;
use std::fmt;

/// Order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn token(self) -> &'static str {
        match self {
            Side::Buy => "B",
            Side::Sell => "S",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "B" => Some(Side::Buy),
            "S" => Some(Side::Sell),
            _ => None,
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// Time in force of a strategy order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tif {
    /// Rests in the book until filled or cancelled.
    Day,
    /// Fills what it can on arrival; the rest is cancelled.
    Ioc,
}

impl Tif {
    pub fn token(self) -> &'static str {
        match self {
            Tif::Day => "day",
            Tif::Ioc => "ioc",
        }
    }

    pub fn parse(s: &str) -> Option<Tif> {
        match s {
            "day" => Some(Tif::Day),
            "ioc" => Some(Tif::Ioc),
            _ => None,
        }
    }
}

/// Longest token (order id, symbol, tag, decision name) the protocol accepts.
pub const MAX_TOKEN: usize = 64;

/// A protocol token: 1-64 characters from `A-Z a-z 0-9 . _ : + -`. No spaces and no `=`, so a
/// line of `key=value` pairs can always be split without quoting.
pub fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_TOKEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'-'))
}

/// One normalized market data event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdKind {
    Add {
        sym: String,
        id: u64,
        side: Side,
        px: i64,
        qty: i64,
    },
    Cancel {
        id: u64,
        qty: i64,
    },
    Delete {
        id: u64,
    },
    Exec {
        id: u64,
        qty: i64,
        px: Option<i64>,
    },
    Replace {
        id: u64,
        new_id: u64,
        px: i64,
        qty: i64,
    },
    Trade {
        sym: String,
        side: Side,
        px: i64,
        qty: i64,
    },
}

impl MdKind {
    pub fn name(&self) -> &'static str {
        match self {
            MdKind::Add { .. } => "add",
            MdKind::Cancel { .. } => "cancel",
            MdKind::Delete { .. } => "delete",
            MdKind::Exec { .. } => "exec",
            MdKind::Replace { .. } => "replace",
            MdKind::Trade { .. } => "trade",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdRecord {
    pub ts: i64,
    pub kind: MdKind,
}

fn int(v: &Value, key: &str) -> Result<i64, String> {
    match v.get(key) {
        None => Err(format!("missing \"{key}\"")),
        Some(x) => x
            .as_i64()
            .ok_or_else(|| format!("\"{key}\" must be an integer (got {x})")),
    }
}

fn id(v: &Value, key: &str) -> Result<u64, String> {
    match v.get(key) {
        None => Err(format!("missing \"{key}\"")),
        Some(x) => x
            .as_u64()
            .ok_or_else(|| format!("\"{key}\" must be a non-negative integer (got {x})")),
    }
}

fn qty(v: &Value, key: &str) -> Result<i64, String> {
    let q = int(v, key)?;
    if q <= 0 {
        return Err(format!("\"{key}\" must be > 0 (got {q})"));
    }
    Ok(q)
}

fn sym(v: &Value) -> Result<String, String> {
    match v.get("sym").and_then(Value::as_str) {
        Some(s) if valid_token(s) => Ok(s.to_string()),
        Some(s) => Err(format!(
            "\"sym\" {s:?} must be 1-{MAX_TOKEN} characters from A-Z a-z 0-9 . _ : + -"
        )),
        None => Err("missing \"sym\" (a string)".to_string()),
    }
}

fn side(v: &Value) -> Result<Side, String> {
    v.get("side")
        .and_then(Value::as_str)
        .and_then(Side::parse)
        .ok_or_else(|| "\"side\" must be \"B\" or \"S\"".to_string())
}

/// Parse one recording line.
pub fn parse_md(v: &Value) -> Result<MdRecord, String> {
    let ts = int(v, "ts")?;
    let kind = match v.get("type").and_then(Value::as_str) {
        Some("add") => MdKind::Add {
            sym: sym(v)?,
            id: id(v, "id")?,
            side: side(v)?,
            px: int(v, "px")?,
            qty: qty(v, "qty")?,
        },
        Some("cancel") => MdKind::Cancel {
            id: id(v, "id")?,
            qty: qty(v, "qty")?,
        },
        Some("delete") => MdKind::Delete { id: id(v, "id")? },
        Some("exec") => MdKind::Exec {
            id: id(v, "id")?,
            qty: qty(v, "qty")?,
            px: if v.get("px").is_some() {
                Some(int(v, "px")?)
            } else {
                None
            },
        },
        Some("replace") => MdKind::Replace {
            id: id(v, "id")?,
            new_id: id(v, "new_id")?,
            px: int(v, "px")?,
            qty: qty(v, "qty")?,
        },
        Some("trade") => MdKind::Trade {
            sym: sym(v)?,
            side: side(v)?,
            px: int(v, "px")?,
            qty: qty(v, "qty")?,
        },
        Some(other) => {
            return Err(format!(
                "unknown \"type\" {other:?} (expected add, cancel, delete, exec, replace or trade)"
            ));
        }
        None => return Err("missing \"type\"".to_string()),
    };
    Ok(MdRecord { ts, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn every_record_type_parses_and_extra_fields_are_ignored() {
        let rec = parse_md(
            &json!({"ts":5,"type":"add","sym":"AAPL","id":1,"side":"B","px":100,"qty":3,"msg":"A"}),
        )
        .expect("add");
        assert_eq!(
            rec,
            MdRecord {
                ts: 5,
                kind: MdKind::Add {
                    sym: "AAPL".into(),
                    id: 1,
                    side: Side::Buy,
                    px: 100,
                    qty: 3
                }
            }
        );
        assert_eq!(
            parse_md(&json!({"ts":6,"type":"exec","id":1,"qty":2}))
                .expect("exec")
                .kind,
            MdKind::Exec {
                id: 1,
                qty: 2,
                px: None
            }
        );
        assert_eq!(
            parse_md(&json!({"ts":6,"type":"exec","id":1,"qty":2,"px":-5}))
                .expect("exec px")
                .kind,
            MdKind::Exec {
                id: 1,
                qty: 2,
                px: Some(-5)
            },
            "negative prices exist (spreads, some futures)"
        );
        for v in [
            json!({"ts":1,"type":"cancel","id":1,"qty":1}),
            json!({"ts":1,"type":"delete","id":1}),
            json!({"ts":1,"type":"replace","id":1,"new_id":2,"px":3,"qty":4}),
            json!({"ts":1,"type":"trade","sym":"X","side":"S","px":3,"qty":4}),
        ] {
            assert!(parse_md(&v).is_ok(), "{v}");
        }
    }

    #[test]
    fn float_prices_and_bad_fields_are_refused_with_the_field_named() {
        for (v, want) in [
            (
                json!({"ts":1,"type":"add","sym":"X","id":1,"side":"B","px":100.5,"qty":1}),
                "\"px\" must be an integer",
            ),
            (
                json!({"ts":1,"type":"add","sym":"X","id":1,"side":"buy","px":100,"qty":1}),
                "\"side\"",
            ),
            (
                json!({"ts":1,"type":"add","sym":"A B","id":1,"side":"B","px":100,"qty":1}),
                "\"sym\"",
            ),
            (
                json!({"ts":1,"type":"cancel","id":1,"qty":0}),
                "must be > 0",
            ),
            (
                json!({"ts":1,"type":"cancel","id":-1,"qty":1}),
                "non-negative",
            ),
            (json!({"type":"delete","id":1}), "missing \"ts\""),
            (json!({"ts":1,"type":"quote"}), "unknown \"type\""),
        ] {
            let err = parse_md(&v).expect_err(&v.to_string());
            assert!(err.contains(want), "{v}: {err}");
        }
    }

    #[test]
    fn tokens_exclude_spaces_and_equals_signs() {
        assert!(valid_token("AAPL"));
        assert!(valid_token("BRK.B"));
        assert!(valid_token("o-17:x+1_2"));
        assert!(!valid_token(""));
        assert!(!valid_token("a b"));
        assert!(!valid_token("a=b"));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN + 1)));
    }
}
