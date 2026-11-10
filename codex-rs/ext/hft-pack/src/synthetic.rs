//! A synthetic ITCH 5.0 session, generated from a seed, for fixtures and tests.
//!
//! It is invented data, not a capture of any real market: two symbols (`AAA`, `BBB`), a start of
//! day (System Event O, S, Q and Stock Directory messages), then a book that never crosses built
//! from every book message type (A, F, E, C, X, D, U, P), and the end of day (M, E, C). The same
//! seed always gives the same bytes; the checked-in fixture is pinned to that.

use std::collections::BTreeMap;

use crate::itch50::Header;
use crate::itch50::Message;
use crate::itch50::stock_field;

/// One price tick in Price(4) units ($0.01).
pub const TICK: u32 = 100;
const MID: u32 = 1_000_000; // $100.0000

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[derive(Clone)]
struct Live {
    buy: bool,
    price: u32,
    shares: u32,
}

/// Generate `events` book messages for the session (plus the start and end of day).
pub fn session(seed: u64, events: usize) -> Vec<Message> {
    let mut rng = Rng(seed | 1);
    let mut out = Vec::new();
    let mut ts: u64 = 34_200_000_000_000; // 09:30:00.000000000
    let mut tracking: u16 = 0;
    let mut hdr = |locate: u16, ts: u64| {
        tracking = tracking.wrapping_add(1);
        Header { locate, tracking, ts }
    };
    for code in [b'O', b'S'] {
        out.push(Message::SystemEvent { h: hdr(0, ts - 30_000_000_000), code });
    }
    let stocks = ["AAA", "BBB"];
    for (i, s) in stocks.iter().enumerate() {
        out.push(Message::StockDirectory {
            h: hdr(i as u16 + 1, ts - 20_000_000_000),
            stock: stock_field(s),
            market_category: b'Q',
            financial_status: b'N',
            round_lot_size: 100,
            round_lots_only: b'N',
            issue_classification: b'C',
            issue_subtype: *b"Z ",
            authenticity: b'P',
            short_sale_threshold: b'N',
            ipo_flag: b'N',
            luld_tier: b'1',
            etp_flag: b'N',
            etp_leverage: 0,
            inverse: b'N',
        });
    }
    out.push(Message::SystemEvent { h: hdr(0, ts - 1), code: b'Q' });
    let mut books: Vec<BTreeMap<u64, Live>> = vec![BTreeMap::new(), BTreeMap::new()];
    let mut next_ref: u64 = 1;
    let mut match_number: u64 = 1;
    let best = |b: &BTreeMap<u64, Live>, buy: bool| -> Option<u32> {
        let prices = b.values().filter(|o| o.buy == buy).map(|o| o.price);
        if buy { prices.max() } else { prices.min() }
    };
    for i in 0..events {
        if rng.below(4) != 0 {
            ts += 1 + rng.below(250_000);
        }
        let s = rng.below(2) as usize;
        let locate = s as u16 + 1;
        let book = &mut books[s];
        let refs: Vec<u64> = book.keys().copied().collect();
        let op = if book.len() < 8 || i < 16 { 0 } else { rng.below(100) };
        let pick = |rng: &mut Rng| refs[rng.below(refs.len() as u64) as usize];
        if op < 40 {
            let buy = rng.below(2) == 0;
            let mut price = MID - 4 * TICK + rng.below(9) as u32 * TICK;
            if buy {
                if let Some(a) = best(book, false) {
                    price = price.min(a - TICK);
                }
            } else if let Some(b) = best(book, true) {
                price = price.max(b + TICK);
            }
            let shares = 100 * (1 + rng.below(9) as u32);
            let order_ref = next_ref;
            next_ref += 1;
            book.insert(order_ref, Live { buy, price, shares });
            let attribution = (rng.below(4) == 0).then_some(*b"GSCO");
            out.push(Message::AddOrder {
                h: hdr(locate, ts),
                order_ref,
                side: if buy { b'B' } else { b'S' },
                shares,
                stock: stock_field(stocks[s]),
                price,
                attribution,
            });
        } else if op < 52 {
            let r = pick(&mut rng);
            let o = book.get(&r).cloned().unwrap_or(Live { buy: true, price: MID, shares: 0 });
            if o.shares > 100 {
                let cut = 100 * (1 + rng.below(u64::from(o.shares / 100 - 1)) as u32);
                if let Some(x) = book.get_mut(&r) {
                    x.shares -= cut;
                }
                out.push(Message::OrderCancel { h: hdr(locate, ts), order_ref: r, shares: cut });
            } else {
                book.remove(&r);
                out.push(Message::OrderDelete { h: hdr(locate, ts), order_ref: r });
            }
        } else if op < 64 {
            let r = pick(&mut rng);
            book.remove(&r);
            out.push(Message::OrderDelete { h: hdr(locate, ts), order_ref: r });
        } else if op < 82 {
            // Execute against the best order of one side, sometimes at a price improvement.
            let buy = rng.below(2) == 0;
            let Some(px) = best(book, buy) else { continue };
            let Some((r, o)) = book
                .iter()
                .find(|(_, o)| o.buy == buy && o.price == px)
                .map(|(r, o)| (*r, o.clone()))
            else {
                continue;
            };
            let shares = (100 * (1 + rng.below(u64::from(o.shares / 100)) as u32)).min(o.shares);
            let left = o.shares - shares;
            if left == 0 {
                book.remove(&r);
            } else if let Some(x) = book.get_mut(&r) {
                x.shares = left;
            }
            let m = match_number;
            match_number += 1;
            if rng.below(3) == 0 {
                let price = if buy { px + TICK } else { px - TICK };
                out.push(Message::OrderExecutedWithPrice {
                    h: hdr(locate, ts),
                    order_ref: r,
                    shares,
                    match_number: m,
                    printable: b'Y',
                    price,
                });
            } else {
                out.push(Message::OrderExecuted { h: hdr(locate, ts), order_ref: r, shares, match_number: m });
            }
        } else if op < 94 {
            let r = pick(&mut rng);
            let Some(mut o) = book.remove(&r) else { continue };
            let mut price = (o.price + TICK).saturating_sub(rng.below(3) as u32 * TICK);
            if o.buy {
                if let Some(a) = best(book, false) {
                    price = price.min(a - TICK);
                }
            } else if let Some(b) = best(book, true) {
                price = price.max(b + TICK);
            }
            o.price = price;
            o.shares = 100 * (1 + rng.below(9) as u32);
            let new_ref = next_ref;
            next_ref += 1;
            out.push(Message::OrderReplace {
                h: hdr(locate, ts),
                original_ref: r,
                new_ref,
                shares: o.shares,
                price: o.price,
            });
            book.insert(new_ref, o);
        } else {
            let m = match_number;
            match_number += 1;
            out.push(Message::Trade {
                h: hdr(locate, ts),
                order_ref: 0,
                side: b'B',
                shares: 100 * (1 + rng.below(3) as u32),
                stock: stock_field(stocks[s]),
                price: MID,
                match_number: m,
            });
        }
    }
    for code in [b'M', b'E', b'C'] {
        ts += 1_000_000;
        out.push(Message::SystemEvent { h: hdr(0, ts), code });
    }
    out
}

/// A capture file: every message preceded by its 2-byte big-endian length.
pub fn to_file(messages: &[Message]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in messages {
        if let Some(bytes) = crate::itch50::encode(m) {
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(&bytes);
        }
    }
    out
}
