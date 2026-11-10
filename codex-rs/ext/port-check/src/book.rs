//! The recorded order book (L3: every resting order by id), aggregated to price levels.
//!
//! The book holds what the recording says is resting. The harness's simulated fills for the
//! strategy draw on that displayed liquidity, and the shares they take are remembered as
//! "consumed" against the recorded orders in time priority, so the same displayed shares can
//! never fill the strategy twice. Consumption never changes what the feed shows: the levels
//! reported to the strategy are the recorded ones.
//!
//! Everything is kept in ordered maps, so iteration order (and therefore every fill) is a pure
//! function of the recording and the strategy's actions.

use std::collections::BTreeMap;

use crate::model::Side;

#[derive(Clone, Debug)]
struct Resting {
    side: Side,
    px: i64,
    qty: i64,
    consumed: i64,
    prio: u64,
}

#[derive(Clone, Debug, Default)]
struct Level {
    qty: i64,
    consumed: i64,
    queue: BTreeMap<u64, u64>,
}

/// One instrument's book.
#[derive(Clone, Debug, Default)]
pub struct Book {
    orders: BTreeMap<u64, Resting>,
    bids: BTreeMap<i64, Level>,
    asks: BTreeMap<i64, Level>,
    next_prio: u64,
}

/// What a recorded event did to a resting order (for the event shown to the strategy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Touched {
    pub side: Side,
    pub px: i64,
}

impl Book {
    fn levels(&mut self, side: Side) -> &mut BTreeMap<i64, Level> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.orders.contains_key(&id)
    }

    /// A new resting order. A duplicate id is a recording error.
    pub fn add(&mut self, id: u64, side: Side, px: i64, qty: i64) -> Result<(), String> {
        if self.orders.contains_key(&id) {
            return Err(format!("order id {id} is already in the book"));
        }
        let prio = self.next_prio;
        self.next_prio += 1;
        self.orders.insert(
            id,
            Resting {
                side,
                px,
                qty,
                consumed: 0,
                prio,
            },
        );
        let level = self.levels(side).entry(px).or_default();
        level.qty = level
            .qty
            .checked_add(qty)
            .ok_or_else(|| format!("level quantity overflows at price {px}"))?;
        level.queue.insert(prio, id);
        Ok(())
    }

    /// Take `qty` off a resting order (a partial cancel or an execution). Taking more than rests
    /// is a recording error; taking all of it removes the order.
    pub fn reduce(&mut self, id: u64, qty: i64) -> Result<Touched, String> {
        let Some(order) = self.orders.get(&id).cloned() else {
            return Err(format!("order id {id} is not in the book"));
        };
        if qty > order.qty {
            return Err(format!(
                "takes {qty} from order id {id}, which has only {} resting",
                order.qty
            ));
        }
        let new_qty = order.qty - qty;
        let new_consumed = order.consumed.min(new_qty);
        let touched = Touched {
            side: order.side,
            px: order.px,
        };
        let levels = self.levels(order.side);
        let mut empty = false;
        if let Some(level) = levels.get_mut(&order.px) {
            level.qty -= qty;
            level.consumed -= order.consumed - new_consumed;
            if new_qty == 0 {
                level.queue.remove(&order.prio);
            }
            empty = level.queue.is_empty();
        }
        if empty {
            levels.remove(&order.px);
        }
        if new_qty == 0 {
            self.orders.remove(&id);
        } else if let Some(o) = self.orders.get_mut(&id) {
            o.qty = new_qty;
            o.consumed = new_consumed;
        }
        Ok(touched)
    }

    /// Remove a resting order entirely.
    pub fn delete(&mut self, id: u64) -> Result<(Touched, i64), String> {
        let qty = self
            .orders
            .get(&id)
            .map(|o| o.qty)
            .ok_or_else(|| format!("order id {id} is not in the book"))?;
        Ok((self.reduce(id, qty)?, qty))
    }

    /// Cancel-replace: the old order leaves, a new one (same side, new priority) enters.
    pub fn replace(&mut self, id: u64, new_id: u64, px: i64, qty: i64) -> Result<Touched, String> {
        if new_id != id && self.orders.contains_key(&new_id) {
            return Err(format!("replace target id {new_id} is already in the book"));
        }
        let (touched, _) = self.delete(id)?;
        self.add(new_id, touched.side, px, qty)?;
        Ok(touched)
    }

    /// The best `depth` levels of one side: (price, displayed quantity), best first.
    pub fn top(&self, side: Side, depth: usize) -> Vec<(i64, i64)> {
        match side {
            Side::Buy => self.bids.iter().rev().take(depth).map(|(p, l)| (*p, l.qty)).collect(),
            Side::Sell => self.asks.iter().take(depth).map(|(p, l)| (*p, l.qty)).collect(),
        }
    }

    pub fn best(&self, side: Side) -> Option<i64> {
        match side {
            Side::Buy => self.bids.keys().next_back().copied(),
            Side::Sell => self.asks.keys().next().copied(),
        }
    }

    /// Take up to `want` not-yet-consumed displayed shares from the `side` of the book, at levels
    /// a limit of `limit` reaches (for resting asks: price <= limit; for bids: price >= limit),
    /// best price first and in time priority inside a level. Returns (level price, shares) per
    /// level touched.
    pub fn take(&mut self, side: Side, limit: i64, want: i64) -> Vec<(i64, i64)> {
        let prices: Vec<i64> = match side {
            Side::Sell => self.asks.range(..=limit).map(|(p, _)| *p).collect(),
            Side::Buy => self.bids.range(limit..).rev().map(|(p, _)| *p).collect(),
        };
        let mut left = want;
        let mut out = Vec::new();
        for px in prices {
            if left == 0 {
                break;
            }
            let ids: Vec<u64> = match self.levels(side).get(&px) {
                Some(level) if level.qty > level.consumed => level.queue.values().copied().collect(),
                _ => continue,
            };
            let mut took = 0;
            for id in ids {
                if left == 0 {
                    break;
                }
                if let Some(o) = self.orders.get_mut(&id) {
                    let c = (o.qty - o.consumed).min(left);
                    o.consumed += c;
                    left -= c;
                    took += c;
                }
            }
            if let Some(level) = self.levels(side).get_mut(&px) {
                level.consumed += took;
            }
            if took > 0 {
                out.push((px, took));
            }
        }
        out
    }

    /// Check the aggregate invariants; used by tests after every operation.
    #[cfg(test)]
    pub fn check(&self) -> Result<(), String> {
        for (side, levels) in [(Side::Buy, &self.bids), (Side::Sell, &self.asks)] {
            for (px, level) in levels {
                let (mut q, mut c) = (0, 0);
                for id in level.queue.values() {
                    let o = self.orders.get(id).ok_or("queue names a missing order")?;
                    if o.side != side || o.px != *px {
                        return Err("order filed at the wrong level".into());
                    }
                    if o.consumed > o.qty || o.consumed < 0 {
                        return Err("consumed out of range".into());
                    }
                    q += o.qty;
                    c += o.consumed;
                }
                if (q, c) != (level.qty, level.consumed) || level.queue.is_empty() {
                    return Err(format!("level {px} aggregates are stale"));
                }
            }
        }
        let filed: usize = self.bids.values().chain(self.asks.values()).map(|l| l.queue.len()).sum();
        if filed != self.orders.len() {
            return Err("orders and levels disagree".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn levels_aggregate_and_report_best_first() {
        let mut b = Book::default();
        b.add(1, Side::Buy, 100, 5).expect("add");
        b.add(2, Side::Buy, 101, 3).expect("add");
        b.add(3, Side::Buy, 100, 2).expect("add");
        b.add(4, Side::Sell, 103, 4).expect("add");
        b.add(5, Side::Sell, 102, 1).expect("add");
        b.check().expect("invariants");
        assert_eq!(b.top(Side::Buy, 5), vec![(101, 3), (100, 7)]);
        assert_eq!(b.top(Side::Sell, 1), vec![(102, 1)]);
        assert_eq!((b.best(Side::Buy), b.best(Side::Sell)), (Some(101), Some(102)));
        assert!(b.add(1, Side::Sell, 1, 1).expect_err("dup").contains("already in the book"));
    }

    #[test]
    fn reduce_delete_and_replace_keep_levels_exact() {
        let mut b = Book::default();
        b.add(1, Side::Sell, 102, 10).expect("add");
        b.add(2, Side::Sell, 102, 5).expect("add");
        assert_eq!(b.reduce(1, 4).expect("reduce"), Touched { side: Side::Sell, px: 102 });
        assert_eq!(b.top(Side::Sell, 1), vec![(102, 11)]);
        let (_, gone) = b.delete(2).expect("delete");
        assert_eq!(gone, 5);
        b.replace(1, 9, 105, 3).expect("replace");
        assert_eq!(b.top(Side::Sell, 3), vec![(105, 3)]);
        assert!(!b.contains(1) && b.contains(9));
        b.check().expect("invariants");
        assert!(b.reduce(9, 4).expect_err("too much").contains("only 3 resting"));
        assert!(b.reduce(77, 1).expect_err("unknown").contains("not in the book"));
        b.reduce(9, 3).expect("all of it");
        assert_eq!(b.top(Side::Sell, 3), vec![]);
        b.check().expect("invariants");
    }

    #[test]
    fn take_walks_levels_best_first_and_never_takes_the_same_shares_twice() {
        let mut b = Book::default();
        b.add(1, Side::Sell, 101, 3).expect("add");
        b.add(2, Side::Sell, 102, 4).expect("add");
        b.add(3, Side::Sell, 104, 9).expect("add");
        assert_eq!(b.take(Side::Sell, 102, 5), vec![(101, 3), (102, 2)]);
        // Displayed quantities are unchanged: consumption is the harness's own ledger.
        assert_eq!(b.top(Side::Sell, 3), vec![(101, 3), (102, 4), (104, 9)]);
        assert_eq!(b.take(Side::Sell, 102, 5), vec![(102, 2)], "only 2 shares were left below 103");
        assert_eq!(b.take(Side::Sell, 103, 5), vec![]);
        b.check().expect("invariants");
        // Bids are taken from the top down.
        b.add(10, Side::Buy, 99, 2).expect("add");
        b.add(11, Side::Buy, 98, 2).expect("add");
        assert_eq!(b.take(Side::Buy, 98, 3), vec![(99, 2), (98, 1)]);
    }

    #[test]
    fn consumed_shares_shrink_with_the_order_they_were_taken_from() {
        let mut b = Book::default();
        b.add(1, Side::Sell, 101, 10).expect("add");
        assert_eq!(b.take(Side::Sell, 101, 6), vec![(101, 6)]);
        // The recorded order is partly executed by someone else: 7 of its 10 go.
        b.reduce(1, 7).expect("reduce");
        b.check().expect("invariants");
        // 3 remain and all 3 were already consumed by us.
        assert_eq!(b.take(Side::Sell, 101, 6), vec![]);
        b.add(2, Side::Sell, 101, 4).expect("add");
        assert_eq!(b.take(Side::Sell, 101, 10), vec![(101, 4)], "new liquidity is available");
        b.check().expect("invariants");
    }

    #[test]
    fn randomized_operations_never_break_the_book_invariants() {
        let mut state = 99u64;
        let mut rnd = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut b = Book::default();
        let mut live: Vec<u64> = Vec::new();
        let mut next_id = 1;
        for _ in 0..5000 {
            match rnd(5) {
                0 | 1 => {
                    let side = if rnd(2) == 0 { Side::Buy } else { Side::Sell };
                    let px = 95 + rnd(10) as i64;
                    b.add(next_id, side, px, 1 + rnd(20) as i64).expect("add");
                    live.push(next_id);
                    next_id += 1;
                }
                2 if !live.is_empty() => {
                    let id = live[rnd(live.len() as u64) as usize];
                    if b.reduce(id, 1).is_ok() && !b.contains(id) {
                        live.retain(|x| *x != id);
                    }
                }
                3 if !live.is_empty() => {
                    let i = rnd(live.len() as u64) as usize;
                    let id = live.swap_remove(i);
                    b.delete(id).expect("delete");
                }
                _ => {
                    let side = if rnd(2) == 0 { Side::Buy } else { Side::Sell };
                    b.take(side, 95 + rnd(10) as i64, 1 + rnd(30) as i64);
                }
            }
            b.check().expect("invariants hold after every operation");
        }
    }
}
