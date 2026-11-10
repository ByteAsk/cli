//! ITCH 5.0 messages to the normalized market events the port_check harness replays.
//!
//! | ITCH            | event                                          |
//! |-----------------|------------------------------------------------|
//! | A / F           | `add` (sym, id = order reference, side, px, qty) |
//! | E               | `exec` (id, qty; the order's own price)        |
//! | C               | `exec` (id, qty, px = execution price)         |
//! | X               | `cancel` (id, qty)                             |
//! | D               | `delete` (id)                                  |
//! | U               | `replace` (id, new_id, px, qty)                |
//! | P               | `trade` (non-displayed print: sym, side, px, qty) |
//! | S, R, others    | nothing (they do not change the displayed book) |
//!
//! `ts` is ITCH's nanoseconds since midnight; prices stay in ITCH's Price(4) units. Each event
//! keeps `msg` (the ITCH type) for provenance; the harness ignores extra fields. An Add Order whose
//! stock field disagrees with the Stock Directory entry for its locate code is an error, because
//! it means the file is corrupt or the locates were mixed up.

use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use crate::itch50::Message;
use crate::itch50::stock_text;

fn valid_symbol(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'-'))
}

fn side(b: u8) -> Result<&'static str, String> {
    match b {
        b'B' => Ok("B"),
        b'S' => Ok("S"),
        other => Err(format!("buy/sell indicator {:?} is neither B nor S", other as char)),
    }
}

/// Counts of what was read, for the caller's report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: u64,
    pub events: u64,
    pub skipped_by_type: BTreeMap<char, u64>,
}

#[derive(Default)]
pub struct Normalizer {
    locate_sym: BTreeMap<u16, String>,
    pub stats: Stats,
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    fn symbol(&self, locate: u16, stock: [u8; 8]) -> Result<String, String> {
        let s = stock_text(&stock);
        if !valid_symbol(&s) {
            return Err(format!(
                "stock {s:?} cannot be a harness symbol (allowed: A-Z a-z 0-9 . _ : + -)"
            ));
        }
        if let Some(known) = self.locate_sym.get(&locate)
            && known != &s
        {
            return Err(format!(
                "stock {s:?} disagrees with the stock directory entry {known:?} for locate {locate}"
            ));
        }
        Ok(s)
    }

    /// One message in, at most one event out.
    pub fn push(&mut self, m: &Message) -> Result<Option<Value>, String> {
        self.stats.messages += 1;
        let ev = match m {
            Message::StockDirectory { h, stock, .. } => {
                self.locate_sym.insert(h.locate, stock_text(stock));
                None
            }
            Message::AddOrder { h, order_ref, side: s, shares, stock, price, attribution } => Some(json!({
                "ts": h.ts, "type": "add", "sym": self.symbol(h.locate, *stock)?, "id": order_ref,
                "side": side(*s)?, "px": price, "qty": shares,
                "msg": if attribution.is_some() { "F" } else { "A" },
            })),
            Message::OrderExecuted { h, order_ref, shares, .. } => Some(json!({
                "ts": h.ts, "type": "exec", "id": order_ref, "qty": shares, "msg": "E",
            })),
            Message::OrderExecutedWithPrice { h, order_ref, shares, price, .. } => Some(json!({
                "ts": h.ts, "type": "exec", "id": order_ref, "qty": shares, "px": price, "msg": "C",
            })),
            Message::OrderCancel { h, order_ref, shares } => Some(json!({
                "ts": h.ts, "type": "cancel", "id": order_ref, "qty": shares, "msg": "X",
            })),
            Message::OrderDelete { h, order_ref } => Some(json!({
                "ts": h.ts, "type": "delete", "id": order_ref, "msg": "D",
            })),
            Message::OrderReplace { h, original_ref, new_ref, shares, price } => Some(json!({
                "ts": h.ts, "type": "replace", "id": original_ref, "new_id": new_ref, "px": price,
                "qty": shares, "msg": "U",
            })),
            Message::Trade { h, side: s, shares, stock, price, .. } => Some(json!({
                "ts": h.ts, "type": "trade", "sym": self.symbol(h.locate, *stock)?, "side": side(*s)?,
                "px": price, "qty": shares, "msg": "P",
            })),
            Message::SystemEvent { .. } => {
                *self.stats.skipped_by_type.entry('S').or_default() += 1;
                None
            }
            Message::Other { kind, .. } => {
                *self.stats.skipped_by_type.entry(*kind as char).or_default() += 1;
                None
            }
        };
        if ev.is_some() {
            self.stats.events += 1;
        }
        Ok(ev)
    }
}
