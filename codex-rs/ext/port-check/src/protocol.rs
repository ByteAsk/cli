//! The line protocol between the harness and a strategy process (version 1).
//!
//! Synchronous and text-only, so both adapters stay small enough to audit and nothing about it
//! can reorder events: the harness writes ONE event line, the strategy answers with zero or more
//! action lines and then a line `done`, and only then does the harness move on.
//!
//! Event line: `<type> step=<n> ts=<ns> key=value ...`, every value a token (see
//! [`crate::model::valid_token`]) or a decimal integer. Types:
//! ```text
//! init          step ts seed syms=A,B depth p.<name>=<value>...
//! md            step ts [rec] sym kind [id side px qty new_id] bp1 bq1 .. apN aqN
//! ack           step ts id
//! fill          step ts id px qty leaves liq=taker|maker
//! reject        step ts id reason
//! cancelled     step ts id qty reason=user|ioc
//! cancel_reject step ts id reason
//! timer         step ts tag
//! end           step ts
//! ```
//! `md` carries the event that happened (`kind` = add, cancel, delete, exec, replace, trade, or
//! snapshot for the initial book) and the top `depth` levels of that symbol's book after it, as
//! `bp1/bq1` (best bid price/quantity) ... and `ap1/aq1` ...; a missing level is simply absent.
//!
//! Strategy lines, ended by `done`:
//! ```text
//! new id=<token> sym=<token> side=B|S px=<int> qty=<int> [tif=day|ioc]
//! cancel id=<token>
//! timer at=<ns> tag=<token>
//! decide <name>=<value>      a decision: compared EXACTLY (integer, decimal or token)
//! diag <name>=<number>       a diagnostic number: the only thing a tolerance may apply to
//! ```
//! Any other line is a protocol error, which ends the run at that step. Strict on purpose: an
//! adapter bug must surface as an error, never as a silently different reading of the line.

use serde_json::Map;
use serde_json::Number;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use crate::model::Side;
use crate::model::Tif;
use crate::model::valid_token;

pub const PROTOCOL_VERSION: u32 = 1;

/// What happened, as the strategy is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventBody {
    Init {
        seed: u64,
        symbols: Vec<String>,
        depth: usize,
        params: BTreeMap<String, String>,
    },
    Md(MdEvent),
    Ack {
        id: String,
    },
    Fill {
        id: String,
        px: i64,
        qty: i64,
        leaves: i64,
        liq: &'static str,
    },
    Reject {
        id: String,
        reason: &'static str,
    },
    Cancelled {
        id: String,
        qty: i64,
        reason: &'static str,
    },
    CancelReject {
        id: String,
        reason: &'static str,
    },
    Timer {
        tag: String,
    },
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdEvent {
    /// 0-based index of the recording line, absent for the initial snapshot.
    pub rec: Option<u64>,
    pub sym: String,
    pub kind: &'static str,
    pub id: Option<u64>,
    pub side: Option<Side>,
    pub px: Option<i64>,
    pub qty: Option<i64>,
    pub new_id: Option<u64>,
    pub bids: Vec<(i64, i64)>,
    pub asks: Vec<(i64, i64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub step: u64,
    pub ts: i64,
    pub body: EventBody,
}

impl Event {
    pub fn type_name(&self) -> &'static str {
        match &self.body {
            EventBody::Init { .. } => "init",
            EventBody::Md(_) => "md",
            EventBody::Ack { .. } => "ack",
            EventBody::Fill { .. } => "fill",
            EventBody::Reject { .. } => "reject",
            EventBody::Cancelled { .. } => "cancelled",
            EventBody::CancelReject { .. } => "cancel_reject",
            EventBody::Timer { .. } => "timer",
            EventBody::End => "end",
        }
    }

    /// The protocol line (without the newline).
    pub fn line(&self) -> String {
        let mut s = format!("{} step={} ts={}", self.type_name(), self.step, self.ts);
        let mut kv = |k: &str, v: String| {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(&v);
        };
        match &self.body {
            EventBody::Init {
                seed,
                symbols,
                depth,
                params,
            } => {
                kv("seed", seed.to_string());
                kv("syms", symbols.join(","));
                kv("depth", depth.to_string());
                for (k, v) in params {
                    kv(&format!("p.{k}"), v.clone());
                }
            }
            EventBody::Md(md) => {
                if let Some(rec) = md.rec {
                    kv("rec", rec.to_string());
                }
                kv("sym", md.sym.clone());
                kv("kind", md.kind.to_string());
                if let Some(id) = md.id {
                    kv("id", id.to_string());
                }
                if let Some(side) = md.side {
                    kv("side", side.token().to_string());
                }
                if let Some(px) = md.px {
                    kv("px", px.to_string());
                }
                if let Some(qty) = md.qty {
                    kv("qty", qty.to_string());
                }
                if let Some(new_id) = md.new_id {
                    kv("new_id", new_id.to_string());
                }
                for (i, (px, q)) in md.bids.iter().enumerate() {
                    kv(&format!("bp{}", i + 1), px.to_string());
                    kv(&format!("bq{}", i + 1), q.to_string());
                }
                for (i, (px, q)) in md.asks.iter().enumerate() {
                    kv(&format!("ap{}", i + 1), px.to_string());
                    kv(&format!("aq{}", i + 1), q.to_string());
                }
            }
            EventBody::Ack { id } => kv("id", id.clone()),
            EventBody::Fill {
                id,
                px,
                qty,
                leaves,
                liq,
            } => {
                kv("id", id.clone());
                kv("px", px.to_string());
                kv("qty", qty.to_string());
                kv("leaves", leaves.to_string());
                kv("liq", (*liq).to_string());
            }
            EventBody::Reject { id, reason } | EventBody::CancelReject { id, reason } => {
                kv("id", id.clone());
                kv("reason", (*reason).to_string());
            }
            EventBody::Cancelled { id, qty, reason } => {
                kv("id", id.clone());
                kv("qty", qty.to_string());
                kv("reason", (*reason).to_string());
            }
            EventBody::Timer { tag } => kv("tag", tag.clone()),
            EventBody::End => {}
        }
        s
    }

    /// The event as recorded in the transcript (`in`).
    pub fn json(&self) -> Value {
        let levels = |v: &Vec<(i64, i64)>| -> Value {
            Value::Array(v.iter().map(|(p, q)| json!([p, q])).collect())
        };
        match &self.body {
            EventBody::Init {
                seed,
                symbols,
                depth,
                params,
            } => {
                json!({"seed": seed, "symbols": symbols, "depth": depth, "params": params})
            }
            EventBody::Md(md) => {
                let mut m = Map::new();
                m.insert("rec".into(), md.rec.map_or(Value::Null, |r| json!(r)));
                m.insert("sym".into(), json!(md.sym));
                m.insert("kind".into(), json!(md.kind));
                if let Some(id) = md.id {
                    m.insert("id".into(), json!(id));
                }
                if let Some(side) = md.side {
                    m.insert("side".into(), json!(side.token()));
                }
                if let Some(px) = md.px {
                    m.insert("px".into(), json!(px));
                }
                if let Some(qty) = md.qty {
                    m.insert("qty".into(), json!(qty));
                }
                if let Some(new_id) = md.new_id {
                    m.insert("new_id".into(), json!(new_id));
                }
                m.insert("bids".into(), levels(&md.bids));
                m.insert("asks".into(), levels(&md.asks));
                Value::Object(m)
            }
            EventBody::Ack { id } => json!({"id": id}),
            EventBody::Fill {
                id,
                px,
                qty,
                leaves,
                liq,
            } => {
                json!({"id": id, "px": px, "qty": qty, "leaves": leaves, "liq": liq})
            }
            EventBody::Reject { id, reason } | EventBody::CancelReject { id, reason } => {
                json!({"id": id, "reason": reason})
            }
            EventBody::Cancelled { id, qty, reason } => {
                json!({"id": id, "qty": qty, "reason": reason})
            }
            EventBody::Timer { tag } => json!({"tag": tag}),
            EventBody::End => json!({}),
        }
    }
}

/// What a strategy asked for in one answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    New {
        id: String,
        sym: String,
        side: Side,
        px: i64,
        qty: i64,
        tif: Tif,
    },
    Cancel {
        id: String,
    },
    Timer {
        at: i64,
        tag: String,
    },
}

impl Action {
    pub fn json(&self) -> Value {
        match self {
            Action::New {
                id,
                sym,
                side,
                px,
                qty,
                tif,
            } => json!({
                "op": "new", "id": id, "sym": sym, "side": side.token(), "px": px, "qty": qty,
                "tif": tif.token()
            }),
            Action::Cancel { id } => json!({"op": "cancel", "id": id}),
            Action::Timer { at, tag } => json!({"op": "timer", "at": at, "tag": tag}),
        }
    }
}

/// One full answer to one event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Response {
    pub actions: Vec<Action>,
    /// Decisions in the order given: compared exactly.
    pub decisions: Vec<(String, Value)>,
    /// Diagnostic numbers by name: the only place a tolerance may apply.
    pub diag: BTreeMap<String, Value>,
}

impl Response {
    pub fn json(&self) -> Value {
        json!({
            "actions": self.actions.iter().map(Action::json).collect::<Vec<_>>(),
            "decisions": self
                .decisions
                .iter()
                .map(|(n, v)| json!({"name": n, "value": v}))
                .collect::<Vec<_>>(),
            "diag": self.diag,
        })
    }
}

fn kv_pairs<'a>(tokens: &[&'a str], line: &str) -> Result<BTreeMap<&'a str, &'a str>, String> {
    let mut out = BTreeMap::new();
    for t in tokens {
        let Some((k, v)) = t.split_once('=') else {
            return Err(format!("expected key=value, got {t:?} in {line:?}"));
        };
        if !valid_token(k) || !valid_token(v) {
            return Err(format!(
                "{t:?} in {line:?}: keys and values are 1-64 characters from A-Z a-z 0-9 . _ : + -"
            ));
        }
        if out.insert(k, v).is_some() {
            return Err(format!("{k:?} given twice in {line:?}"));
        }
    }
    Ok(out)
}

fn take<'a>(m: &mut BTreeMap<&'a str, &'a str>, key: &str, line: &str) -> Result<&'a str, String> {
    m.remove(key)
        .ok_or_else(|| format!("missing {key}= in {line:?}"))
}

fn int(v: &str, key: &str, line: &str) -> Result<i64, String> {
    v.parse::<i64>().map_err(|_| {
        format!(
            "{key}={v} in {line:?} must be a whole number (prices are integer units, never floats)"
        )
    })
}

fn no_extra(m: &BTreeMap<&str, &str>, line: &str) -> Result<(), String> {
    match m.keys().next() {
        None => Ok(()),
        Some(k) => Err(format!("unknown key {k:?} in {line:?}")),
    }
}

/// True for a plain decimal number such as `-12`, `0.5`, `1e-9` (not `nan`, `inf`, `0x10`).
fn is_decimal(v: &str) -> bool {
    let b = v.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - f;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

/// A decision value: an integer, a decimal number, or a token, in that order of preference.
/// `1` and `1.0` are the same decision; `nan` and `inf` stay tokens.
fn decision_value(v: &str) -> Value {
    if let Ok(i) = v.parse::<i64>() {
        return json!(i);
    }
    if is_decimal(v)
        && let Ok(f) = v.parse::<f64>()
        && let Some(n) = Number::from_f64(f)
    {
        return Value::Number(n);
    }
    json!(v)
}

/// A diagnostic value: a finite number, or `NaN` / `Infinity` / `-Infinity` (written by the
/// adapters as `nan`, `inf`, `-inf`), so two versions that both produce NaN agree.
fn diag_value(v: &str, line: &str) -> Result<Value, String> {
    match v.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => return Ok(json!("NaN")),
        "inf" | "+inf" | "infinity" | "+infinity" => return Ok(json!("Infinity")),
        "-inf" | "-infinity" => return Ok(json!("-Infinity")),
        _ => {}
    }
    if !is_decimal(v) {
        return Err(format!("diag value {v:?} in {line:?} must be a number"));
    }
    v.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
        .ok_or_else(|| format!("diag value {v:?} in {line:?} must be a finite number"))
}

/// Parse a strategy's answer (the lines before `done`).
pub fn parse_response(lines: &[String]) -> Result<Response, String> {
    let mut r = Response::default();
    for line in lines {
        let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
        let Some((op, rest)) = tokens.split_first() else {
            continue;
        };
        let mut m = kv_pairs(rest, line)?;
        match *op {
            "new" => {
                let id = take(&mut m, "id", line)?.to_string();
                let sym = take(&mut m, "sym", line)?.to_string();
                let side_s = take(&mut m, "side", line)?;
                let side = Side::parse(side_s)
                    .ok_or_else(|| format!("side={side_s} in {line:?} must be B or S"))?;
                let px = int(take(&mut m, "px", line)?, "px", line)?;
                let qty = int(take(&mut m, "qty", line)?, "qty", line)?;
                let tif = match m.remove("tif") {
                    None => Tif::Day,
                    Some(t) => Tif::parse(t)
                        .ok_or_else(|| format!("tif={t} in {line:?} must be day or ioc"))?,
                };
                no_extra(&m, line)?;
                r.actions.push(Action::New {
                    id,
                    sym,
                    side,
                    px,
                    qty,
                    tif,
                });
            }
            "cancel" => {
                let id = take(&mut m, "id", line)?.to_string();
                no_extra(&m, line)?;
                r.actions.push(Action::Cancel { id });
            }
            "timer" => {
                let at = int(take(&mut m, "at", line)?, "at", line)?;
                let tag = take(&mut m, "tag", line)?.to_string();
                no_extra(&m, line)?;
                r.actions.push(Action::Timer { at, tag });
            }
            "decide" | "diag" => {
                if m.len() != 1 {
                    return Err(format!("{op} takes exactly one name=value, got {line:?}"));
                }
                let (name, value) = m.pop_first().unwrap_or_default();
                if *op == "decide" {
                    r.decisions.push((name.to_string(), decision_value(value)));
                } else {
                    let v = diag_value(value, line)?;
                    if r.diag.insert(name.to_string(), v).is_some() {
                        return Err(format!("diag {name} given twice in one answer"));
                    }
                }
            }
            other => {
                return Err(format!(
                    "unknown line {line:?} (expected new, cancel, timer, decide, diag or done; got {other:?})"
                ));
            }
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn event_lines_carry_every_field_in_a_fixed_order() {
        let e = Event {
            step: 7,
            ts: 1_000,
            body: EventBody::Md(MdEvent {
                rec: Some(5),
                sym: "AAPL".into(),
                kind: "replace",
                id: Some(3),
                side: Some(Side::Sell),
                px: Some(-12),
                qty: Some(4),
                new_id: Some(9),
                bids: vec![(100, 5)],
                asks: vec![(101, 2), (102, 7)],
            }),
        };
        assert_eq!(
            e.line(),
            "md step=7 ts=1000 rec=5 sym=AAPL kind=replace id=3 side=S px=-12 qty=4 new_id=9 bp1=100 bq1=5 ap1=101 aq1=2 ap2=102 aq2=7"
        );
        assert_eq!(e.json()["asks"], json!([[101, 2], [102, 7]]));
        let init = Event {
            step: 0,
            ts: 5,
            body: EventBody::Init {
                seed: 42,
                symbols: vec!["A".into(), "B".into()],
                depth: 2,
                params: BTreeMap::from([("z".into(), "1".into()), ("a".into(), "x".into())]),
            },
        };
        assert_eq!(
            init.line(),
            "init step=0 ts=5 seed=42 syms=A,B depth=2 p.a=x p.z=1"
        );
    }

    #[test]
    fn a_full_answer_parses_into_actions_decisions_and_diagnostics() {
        let r = parse_response(&lines(&[
            "new id=o1 sym=AAPL side=B px=100 qty=5",
            "new id=o2 sym=AAPL side=S px=-3 qty=1 tif=ioc",
            "cancel id=o0",
            "timer at=2000 tag=requote",
            "decide signal=1",
            "decide regime=calm",
            "decide edge=1.0",
            "diag fair=100.25",
            "diag z=nan",
            "",
        ]))
        .expect("parses");
        assert_eq!(r.actions.len(), 4);
        assert_eq!(
            r.actions[1],
            Action::New {
                id: "o2".into(),
                sym: "AAPL".into(),
                side: Side::Sell,
                px: -3,
                qty: 1,
                tif: Tif::Ioc
            }
        );
        assert_eq!(
            r.decisions,
            vec![
                ("signal".to_string(), json!(1)),
                ("regime".to_string(), json!("calm")),
                ("edge".to_string(), json!(1.0)),
            ]
        );
        assert_eq!(r.diag.get("fair"), Some(&json!(100.25)));
        assert_eq!(r.diag.get("z"), Some(&json!("NaN")));
    }

    #[test]
    fn python_and_cpp_spellings_of_the_same_double_read_as_the_same_number() {
        // Python repr() and C++ %.17g print 0.1 differently; both must become the same f64.
        let py = parse_response(&lines(&["diag x=0.1"])).expect("py");
        let cc = parse_response(&lines(&["diag x=0.10000000000000001"])).expect("c++");
        assert_eq!(py.diag, cc.diag);
        let py = parse_response(&lines(&["diag x=-inf"])).expect("py");
        let cc = parse_response(&lines(&["diag x=-INF"])).expect("c++");
        assert_eq!(py.diag, cc.diag);
    }

    #[test]
    fn malformed_answers_are_protocol_errors_never_guesses() {
        for (bad, want) in [
            ("new id=o1 sym=X side=B px=100.5 qty=1", "whole number"),
            ("new id=o1 sym=X side=buy px=100 qty=1", "must be B or S"),
            ("new id=o1 sym=X side=B px=100", "missing qty="),
            ("new id=o1 sym=X side=B px=100 qty=1 venue=Q", "unknown key"),
            ("new id=o1 id=o2 sym=X side=B px=1 qty=1", "given twice"),
            ("new id=o 1 sym=X side=B px=1 qty=1", "expected key=value"),
            ("cancel", "missing id="),
            ("decide a=1 b=2", "exactly one"),
            ("diag x=fast", "must be a number"),
            ("diag x=0x10", "must be a number"),
            ("buy id=1", "unknown line"),
            ("new id=o1 sym=X side=B px=1 qty=1 tif=gtc", "day or ioc"),
        ] {
            let err = parse_response(&lines(&[bad])).expect_err(bad);
            assert!(err.contains(want), "{bad}: {err}");
        }
        let err = parse_response(&lines(&["diag x=1", "diag x=2"])).expect_err("dup diag");
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn decimal_detection_rejects_everything_but_plain_decimals() {
        for ok in ["0", "-1", "+2.5", ".5", "5.", "1e9", "1E-9", "-0.0"] {
            assert!(is_decimal(ok), "{ok}");
        }
        for bad in [
            "", "-", ".", "e5", "1e", "1e+", "nan", "inf", "0x1", "1.2.3", "1_000",
        ] {
            assert!(!is_decimal(bad), "{bad}");
        }
    }
}
