//! The deterministic exchange simulator: ONE harness for both versions.
//!
//! It fixes everything a strategy could otherwise see vary between runs:
//! - initial book state: the optional `initial_book` orders, applied before the first event and
//!   shown to the strategy as one `snapshot` event per symbol;
//! - the clock: the strategy's only time is the `ts` of the event it is answering;
//! - randomness: `seed` is handed to the strategy in `init`, and the harness's own randomness
//!   (injected rejects) is a pure function of that seed and the order count;
//! - fills and rejects: decided here, by the rules below, never by the strategy;
//! - event order: one queue. Everything that happens is ordered by (time, class, sequence)
//!   with class 0 = recorded market data, 1 = a strategy order or cancel taking effect,
//!   2 = a strategy timer. So at equal times the market moves first, then orders arrive, then
//!   timers fire; inside a class, first come first served.
//!
//! Fill rules (deterministic, deliberately simple; they exist so both versions are treated
//! identically, not to predict real PnL):
//! - an arriving order first gets `ack`, then trades against the recorded book's displayed
//!   liquidity at the recorded levels' prices, best level first (`liq=taker`, one `fill` per
//!   level); an IOC remainder is `cancelled reason=ioc`, a DAY remainder rests;
//! - a resting strategy order fills, at its own price (`liq=maker`), when the recorded book
//!   crosses it (best ask <= its bid, best bid >= its ask), against the liquidity that crossed;
//! - shares the strategy has taken are remembered against the recorded orders, so the same
//!   displayed shares never fill twice; the feed the strategy sees is the recorded one;
//! - strategy orders never trade with each other.
//!
//! The session ends at the time of the last recorded event: orders still in flight are resolved,
//! timers set for later never fire, and the strategy gets `end`.
//!
//! Rejects, in this order: `duplicate_id`, `unknown_symbol`, `bad_qty`, `max_qty`, `bad_tick`,
//! `price_band`, `position_limit`, `injected` (every Nth order and/or a seeded rate), and
//! `closed` after the end of data. Cancels: `unknown_order`, `not_open`, `closed`.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt;

use crate::book::Book;
use crate::model::MdKind;
use crate::model::MdRecord;
use crate::model::Side;
use crate::model::Tif;
use crate::model::valid_token;
use crate::protocol::Action;
use crate::protocol::Event;
use crate::protocol::EventBody;
use crate::protocol::MdEvent;
use crate::protocol::Response;

pub const MAX_DEPTH: usize = 20;

/// Book levels as (price, quantity), best first.
type Levels = Vec<(i64, i64)>;

/// A recording as the simulator reads it.
pub type RecordStream<'a> = dyn Iterator<Item = Result<MdRecord, String>> + 'a;

fn one_i64() -> i64 {
    1
}

fn one_usize() -> usize {
    1
}

fn default_strict() -> bool {
    true
}

/// Every setting that shapes what the strategy sees. Recorded in the result.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Handed to the strategy in `init`; also seeds injected rejects.
    #[serde(default)]
    pub seed: u64,
    /// Delay between a strategy action and its effect, in nanoseconds of market time.
    #[serde(default)]
    pub latency_ns: i64,
    /// Book levels per side shown in every `md` event (1-20).
    #[serde(default = "one_usize")]
    pub depth: usize,
    /// Prices must be a multiple of this.
    #[serde(default = "one_i64")]
    pub tick: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_order_qty: Option<i64>,
    /// Largest absolute position per symbol, counting open orders on the same side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position: Option<i64>,
    /// Largest distance from the mid price, in price units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_band: Option<i64>,
    /// Reject every Nth new order (1-based count of all new orders), 0 = never.
    #[serde(default)]
    pub reject_every: u64,
    /// Reject new orders at this rate (parts per million), drawn from the seed.
    #[serde(default)]
    pub reject_ppm: u32,
    /// A recorded cancel/exec/delete/replace for an order the book does not hold is an error
    /// (true, the default) or is skipped and counted (false; for recordings that start
    /// mid-session without an `initial_book`).
    #[serde(default = "default_strict")]
    pub strict_book: bool,
    /// Strategy parameters, passed through in `init` as `p.<name>=<value>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl HarnessConfig {
    /// The defaults, identical to an empty JSON object: depth 1, tick 1, strict book, no
    /// limits, no injected rejects, seed 0, latency 0.
    pub fn standard() -> Self {
        Self {
            seed: 0,
            latency_ns: 0,
            depth: 1,
            tick: 1,
            max_order_qty: None,
            max_position: None,
            price_band: None,
            reject_every: 0,
            reject_ppm: 0,
            strict_book: true,
            params: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.latency_ns < 0 {
            return Err(format!("latency_ns must be >= 0 (got {})", self.latency_ns));
        }
        if self.depth == 0 || self.depth > MAX_DEPTH {
            return Err(format!("depth must be 1-{MAX_DEPTH} (got {})", self.depth));
        }
        if self.tick <= 0 {
            return Err(format!("tick must be > 0 (got {})", self.tick));
        }
        for (name, v) in [
            ("max_order_qty", self.max_order_qty),
            ("max_position", self.max_position),
            ("price_band", self.price_band),
        ] {
            if let Some(v) = v
                && v < 0
            {
                return Err(format!("{name} must be >= 0 (got {v})"));
            }
        }
        if self.reject_ppm > 1_000_000 {
            return Err(format!("reject_ppm must be at most 1000000 (got {})", self.reject_ppm));
        }
        for (k, v) in &self.params {
            if !valid_token(k) || !valid_token(v) {
                return Err(format!(
                    "param {k:?}={v:?}: names and values are 1-64 characters from A-Z a-z 0-9 . _ : + -"
                ));
            }
        }
        Ok(())
    }
}

/// Why a run cannot continue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimError {
    /// The recording is inconsistent (it was validated before the runs, so this means it changed).
    Recording(String),
    /// A harness limit was reached (likely a feedback loop in the strategy).
    Limit(String),
    /// Integer overflow in position or cash arithmetic.
    Overflow(String),
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimError::Recording(m) => write!(f, "recording error: {m}"),
            SimError::Limit(m) => write!(f, "harness limit: {m}"),
            SimError::Overflow(m) => write!(f, "arithmetic overflow: {m}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepLimits {
    pub max_steps: u64,
    pub max_steps_per_ts: u64,
}

impl Default for StepLimits {
    fn default() -> Self {
        Self {
            max_steps: 50_000_000,
            max_steps_per_ts: 100_000,
        }
    }
}

#[derive(Clone, Debug)]
struct Own {
    sym: String,
    side: Side,
    px: i64,
    leaves: i64,
    open: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Position {
    pub position: i64,
    pub cash: i64,
    pub bought: i64,
    pub sold: i64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub md: u64,
    pub orders: u64,
    pub accepted: u64,
    pub rejects: u64,
    pub fills: u64,
    pub cancels: u64,
    pub cancelled: u64,
    pub cancel_rejects: u64,
    pub timers: u64,
    pub decisions: u64,
    pub diags: u64,
    pub unknown_order_events: u64,
}

#[derive(Clone, Debug)]
enum Pending {
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
        tag: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Init,
    Snapshots(usize),
    Running,
    Ended,
    Done,
}

/// SplitMix64 finalizer: the harness's only source of randomness.
pub fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Apply one recorded event to the books. Shared by the simulator and the up-front recording
/// check, so both read a recording the same way. Returns the event as the strategy sees it
/// (without book levels), or `None` when a non-strict book skipped it.
pub fn apply_record(
    books: &mut BTreeMap<String, Book>,
    order_sym: &mut BTreeMap<u64, String>,
    strict: bool,
    rec_index: Option<u64>,
    r: &MdRecord,
) -> Result<Option<MdEvent>, String> {
    let mut md = MdEvent {
        rec: rec_index,
        sym: String::new(),
        kind: r.kind.name(),
        id: None,
        side: None,
        px: None,
        qty: None,
        new_id: None,
        bids: Vec::new(),
        asks: Vec::new(),
    };
    let known = |order_sym: &BTreeMap<u64, String>, id: u64| order_sym.get(&id).cloned();
    match &r.kind {
        MdKind::Add { sym, id, side, px, qty } => {
            let book = books.entry(sym.clone()).or_default();
            if strict || !book.contains(*id) {
                if order_sym.contains_key(id) && !book.contains(*id) {
                    return Err(format!("order id {id} is already resting in another symbol"));
                }
                book.add(*id, *side, *px, *qty)?;
            }
            order_sym.insert(*id, sym.clone());
            md.sym = sym.clone();
            md.id = Some(*id);
            md.side = Some(*side);
            md.px = Some(*px);
            md.qty = Some(*qty);
        }
        MdKind::Cancel { id, qty } | MdKind::Exec { id, qty, .. } => {
            let Some(sym) = known(order_sym, *id) else {
                return if strict {
                    Err(format!("order id {id} is not in the book"))
                } else {
                    Ok(None)
                };
            };
            let book = books.entry(sym.clone()).or_default();
            let touched = book.reduce(*id, *qty)?;
            if !book.contains(*id) {
                order_sym.remove(id);
            }
            md.sym = sym;
            md.id = Some(*id);
            md.side = Some(touched.side);
            md.px = Some(match &r.kind {
                MdKind::Exec { px: Some(p), .. } => *p,
                _ => touched.px,
            });
            md.qty = Some(*qty);
        }
        MdKind::Delete { id } => {
            let Some(sym) = known(order_sym, *id) else {
                return if strict {
                    Err(format!("order id {id} is not in the book"))
                } else {
                    Ok(None)
                };
            };
            let book = books.entry(sym.clone()).or_default();
            let (touched, gone) = book.delete(*id)?;
            order_sym.remove(id);
            md.sym = sym;
            md.id = Some(*id);
            md.side = Some(touched.side);
            md.px = Some(touched.px);
            md.qty = Some(gone);
        }
        MdKind::Replace { id, new_id, px, qty } => {
            let Some(sym) = known(order_sym, *id) else {
                return if strict {
                    Err(format!("order id {id} is not in the book"))
                } else {
                    Ok(None)
                };
            };
            if new_id != id && order_sym.contains_key(new_id) {
                return Err(format!("replace target id {new_id} is already resting"));
            }
            let book = books.entry(sym.clone()).or_default();
            let touched = book.replace(*id, *new_id, *px, *qty)?;
            order_sym.remove(id);
            order_sym.insert(*new_id, sym.clone());
            md.sym = sym;
            md.id = Some(*id);
            md.side = Some(touched.side);
            md.px = Some(*px);
            md.qty = Some(*qty);
            md.new_id = Some(*new_id);
        }
        MdKind::Trade { sym, side, px, qty } => {
            books.entry(sym.clone()).or_default();
            md.sym = sym.clone();
            md.side = Some(*side);
            md.px = Some(*px);
            md.qty = Some(*qty);
        }
    }
    Ok(Some(md))
}

/// The simulator. Drive it with [`Sim::next_event`] and [`Sim::respond`], alternately.
pub struct Sim {
    cfg: HarnessConfig,
    limits: StepLimits,
    symbols: Vec<String>,
    books: BTreeMap<String, Book>,
    order_sym: BTreeMap<u64, String>,
    own: BTreeMap<String, Own>,
    seen_ids: BTreeSet<String>,
    resting: BTreeMap<String, BTreeMap<u64, String>>,
    positions: BTreeMap<String, Position>,
    pending: BTreeMap<(i64, u8, u64), Pending>,
    outbox: VecDeque<EventBody>,
    next_rec: Option<(u64, MdRecord)>,
    rec_count: u64,
    last_rec_ts: Option<i64>,
    step: u64,
    now: i64,
    phase: Phase,
    seq: u64,
    new_orders: u64,
    same_ts: (i64, u64),
    /// Set once the recording is exhausted: the session's last market time.
    exhausted_at: Option<i64>,
    counts: Counts,
}

impl Sim {
    /// Build the simulator. `symbols` is every symbol of the recording and the initial book
    /// (sorted); `initial_book` holds `add` records only.
    pub fn new(
        cfg: HarnessConfig,
        limits: StepLimits,
        symbols: Vec<String>,
        initial_book: &[MdRecord],
    ) -> Result<Self, String> {
        cfg.validate()?;
        let mut books: BTreeMap<String, Book> =
            symbols.iter().map(|s| (s.clone(), Book::default())).collect();
        let mut order_sym = BTreeMap::new();
        for (i, r) in initial_book.iter().enumerate() {
            if !matches!(r.kind, MdKind::Add { .. }) {
                return Err(format!("initial_book entry {i} must be an \"add\""));
            }
            apply_record(&mut books, &mut order_sym, true, None, r)
                .map_err(|e| format!("initial_book entry {i}: {e}"))?;
        }
        let positions = books.keys().map(|s| (s.clone(), Position::default())).collect();
        let symbols = books.keys().cloned().collect();
        Ok(Self {
            cfg,
            limits,
            symbols,
            books,
            order_sym,
            own: BTreeMap::new(),
            seen_ids: BTreeSet::new(),
            resting: BTreeMap::new(),
            positions,
            pending: BTreeMap::new(),
            outbox: VecDeque::new(),
            next_rec: None,
            rec_count: 0,
            last_rec_ts: None,
            step: 0,
            now: 0,
            phase: Phase::Init,
            seq: 0,
            new_orders: 0,
            same_ts: (i64::MIN, 0),
            exhausted_at: None,
            counts: Counts::default(),
        })
    }

    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    fn peek(
        &mut self,
        rec: &mut RecordStream<'_>,
    ) -> Result<(), SimError> {
        if self.next_rec.is_some() {
            return Ok(());
        }
        match rec.next() {
            None => Ok(()),
            Some(Err(e)) => Err(SimError::Recording(e)),
            Some(Ok(r)) => {
                if let Some(last) = self.last_rec_ts
                    && r.ts < last
                {
                    return Err(SimError::Recording(format!(
                        "record {} goes back in time ({} after {last})",
                        self.rec_count, r.ts
                    )));
                }
                self.last_rec_ts = Some(r.ts);
                self.next_rec = Some((self.rec_count, r));
                self.rec_count += 1;
                Ok(())
            }
        }
    }

    fn levels(&self, sym: &str) -> (Levels, Levels) {
        match self.books.get(sym) {
            Some(b) => (b.top(Side::Buy, self.cfg.depth), b.top(Side::Sell, self.cfg.depth)),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// The next event to deliver, or `None` once the session is over.
    pub fn next_event(
        &mut self,
        rec: &mut RecordStream<'_>,
    ) -> Result<Option<Event>, SimError> {
        let body = loop {
            match self.phase {
                Phase::Init => {
                    self.peek(rec)?;
                    self.now = self.next_rec.as_ref().map_or(0, |(_, r)| r.ts);
                    self.phase = Phase::Snapshots(0);
                    break EventBody::Init {
                        seed: self.cfg.seed,
                        symbols: self.symbols.clone(),
                        depth: self.cfg.depth,
                        params: self.cfg.params.clone(),
                    };
                }
                Phase::Snapshots(i) => {
                    if let Some(sym) = self.symbols.get(i).cloned() {
                        self.phase = Phase::Snapshots(i + 1);
                        let (bids, asks) = self.levels(&sym);
                        break EventBody::Md(MdEvent {
                            rec: None,
                            sym,
                            kind: "snapshot",
                            id: None,
                            side: None,
                            px: None,
                            qty: None,
                            new_id: None,
                            bids,
                            asks,
                        });
                    }
                    self.phase = Phase::Running;
                    continue;
                }
                _ => {}
            }
            if let Some(b) = self.outbox.pop_front() {
                break b;
            }
            if self.phase == Phase::Done {
                return Ok(None);
            }
            self.peek(rec)?;
            if self.next_rec.is_none() && self.exhausted_at.is_none() {
                // The session ends at the last recorded time: orders still in flight resolve,
                // but a timer set for later never fires (a strategy that always re-arms its
                // timer would otherwise run forever in market time).
                let end = self.last_rec_ts.unwrap_or(self.now);
                self.exhausted_at = Some(end);
                self.pending.retain(|(ts, class, _), _| !(*class == 2 && *ts > end));
            }
            let md_key = self.next_rec.as_ref().map(|(i, r)| (r.ts, 0u8, *i));
            let pend_key = self.pending.keys().next().copied();
            let take_md = match (md_key, pend_key) {
                (Some(m), Some(p)) => m <= p,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => {
                    if self.phase == Phase::Ended {
                        self.phase = Phase::Done;
                        return Ok(None);
                    }
                    self.phase = Phase::Ended;
                    break EventBody::End;
                }
            };
            if take_md {
                let Some((index, r)) = self.next_rec.take() else {
                    continue;
                };
                self.now = r.ts;
                match self.apply_md(index, &r)? {
                    Some(body) => break body,
                    None => continue,
                }
            } else {
                let Some(((ts, _, _), p)) = self.pending.pop_first() else {
                    continue;
                };
                self.now = self.now.max(ts);
                match p {
                    Pending::Timer { tag } => {
                        self.counts.timers += 1;
                        break EventBody::Timer { tag };
                    }
                    other => self.arrive(other)?,
                }
            }
        };
        if self.step >= self.limits.max_steps {
            return Err(SimError::Limit(format!(
                "more than {} steps in one run",
                self.limits.max_steps
            )));
        }
        if self.same_ts.0 == self.now {
            self.same_ts.1 += 1;
        } else {
            self.same_ts = (self.now, 1);
        }
        if self.same_ts.1 > self.limits.max_steps_per_ts {
            return Err(SimError::Limit(format!(
                "more than {} events at time {} (a strategy that answers every event with a new order loops forever at latency_ns=0)",
                self.limits.max_steps_per_ts, self.now
            )));
        }
        let event = Event {
            step: self.step,
            ts: self.now,
            body,
        };
        self.step += 1;
        Ok(Some(event))
    }

    fn apply_md(&mut self, index: u64, r: &MdRecord) -> Result<Option<EventBody>, SimError> {
        let md = apply_record(
            &mut self.books,
            &mut self.order_sym,
            self.cfg.strict_book,
            Some(index),
            r,
        )
        .map_err(|e| SimError::Recording(format!("record {index}: {e}")))?;
        let Some(mut md) = md else {
            self.counts.unknown_order_events += 1;
            return Ok(None);
        };
        self.counts.md += 1;
        let (bids, asks) = self.levels(&md.sym);
        md.bids = bids;
        md.asks = asks;
        let sym = md.sym.clone();
        self.match_resting(&sym)?;
        Ok(Some(EventBody::Md(md)))
    }

    fn fill(&mut self, id: &str, sym: &str, side: Side, px: i64, qty: i64, liq: &'static str) -> Result<(), SimError> {
        let overflow = || SimError::Overflow(format!("fill of {qty} at {px} on {sym}"));
        let notional = px.checked_mul(qty).ok_or_else(overflow)?;
        let pos = self.positions.entry(sym.to_string()).or_default();
        match side {
            Side::Buy => {
                pos.position = pos.position.checked_add(qty).ok_or_else(overflow)?;
                pos.cash = pos.cash.checked_sub(notional).ok_or_else(overflow)?;
                pos.bought = pos.bought.checked_add(qty).ok_or_else(overflow)?;
            }
            Side::Sell => {
                pos.position = pos.position.checked_sub(qty).ok_or_else(overflow)?;
                pos.cash = pos.cash.checked_add(notional).ok_or_else(overflow)?;
                pos.sold = pos.sold.checked_add(qty).ok_or_else(overflow)?;
            }
        }
        let leaves = match self.own.get_mut(id) {
            Some(o) => {
                o.leaves -= qty;
                o.leaves
            }
            None => 0,
        };
        self.counts.fills += 1;
        self.outbox.push_back(EventBody::Fill {
            id: id.to_string(),
            px,
            qty,
            leaves,
            liq,
        });
        Ok(())
    }

    /// Resting strategy orders on `sym` that the recorded book now crosses fill at their price.
    fn match_resting(&mut self, sym: &str) -> Result<(), SimError> {
        let Some(queue) = self.resting.get(sym).cloned() else {
            return Ok(());
        };
        for (arrival, id) in queue {
            let Some(order) = self.own.get(&id).cloned() else {
                continue;
            };
            let Some(book) = self.books.get_mut(sym) else {
                continue;
            };
            let crossed = match order.side {
                Side::Buy => book.best(Side::Sell).is_some_and(|a| a <= order.px),
                Side::Sell => book.best(Side::Buy).is_some_and(|b| b >= order.px),
            };
            if !crossed {
                continue;
            }
            let took: i64 = book
                .take(order.side.opposite(), order.px, order.leaves)
                .iter()
                .map(|(_, q)| q)
                .sum();
            if took == 0 {
                continue;
            }
            self.fill(&id, sym, order.side, order.px, took, "maker")?;
            if self.own.get(&id).is_some_and(|o| o.leaves == 0) {
                if let Some(o) = self.own.get_mut(&id) {
                    o.open = false;
                }
                if let Some(q) = self.resting.get_mut(sym) {
                    q.remove(&arrival);
                }
            }
        }
        Ok(())
    }

    fn reject_reason(&mut self, sym: &str, side: Side, px: i64, qty: i64) -> Option<&'static str> {
        let Some(book) = self.books.get(sym) else {
            return Some("unknown_symbol");
        };
        if qty <= 0 {
            return Some("bad_qty");
        }
        if self.cfg.max_order_qty.is_some_and(|m| qty > m) {
            return Some("max_qty");
        }
        if px.rem_euclid(self.cfg.tick) != 0 {
            return Some("bad_tick");
        }
        if let Some(band) = self.cfg.price_band
            && let (Some(bid), Some(ask)) = (book.best(Side::Buy), book.best(Side::Sell))
        {
            // |px - mid| > band, in integers: |2px - (bid + ask)| > 2 band.
            let dist = (i128::from(px) * 2 - i128::from(bid) - i128::from(ask)).abs();
            if dist > i128::from(band) * 2 {
                return Some("price_band");
            }
        }
        if let Some(max) = self.cfg.max_position {
            let pos = self.positions.get(sym).map_or(0, |p| p.position);
            // Open orders are exactly the resting ones (an IOC never outlives its arrival).
            let open_same: i64 = self
                .resting
                .get(sym)
                .into_iter()
                .flat_map(BTreeMap::values)
                .filter_map(|id| self.own.get(id))
                .filter(|o| o.open && o.side == side)
                .map(|o| o.leaves)
                .sum();
            let signed = match side {
                Side::Buy => i128::from(pos) + i128::from(open_same) + i128::from(qty),
                Side::Sell => i128::from(pos) - i128::from(open_same) - i128::from(qty),
            };
            if signed.abs() > i128::from(max) {
                return Some("position_limit");
            }
        }
        let n = self.new_orders;
        if self.cfg.reject_every > 0 && n.is_multiple_of(self.cfg.reject_every) {
            return Some("injected");
        }
        if self.cfg.reject_ppm > 0
            && mix64(self.cfg.seed ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15)) % 1_000_000
                < u64::from(self.cfg.reject_ppm)
        {
            return Some("injected");
        }
        None
    }

    fn arrive(&mut self, p: Pending) -> Result<(), SimError> {
        match p {
            Pending::New { id, sym, side, px, qty, tif } => {
                self.new_orders += 1;
                let reason = if self.seen_ids.contains(&id) {
                    Some("duplicate_id")
                } else {
                    self.reject_reason(&sym, side, px, qty)
                };
                self.seen_ids.insert(id.clone());
                if let Some(reason) = reason {
                    self.counts.rejects += 1;
                    self.outbox.push_back(EventBody::Reject { id, reason });
                    return Ok(());
                }
                self.counts.accepted += 1;
                self.own.insert(
                    id.clone(),
                    Own {
                        sym: sym.clone(),
                        side,
                        px,
                        leaves: qty,
                        open: true,
                    },
                );
                self.outbox.push_back(EventBody::Ack { id: id.clone() });
                let fills = match self.books.get_mut(&sym) {
                    Some(book) => book.take(side.opposite(), px, qty),
                    None => Vec::new(),
                };
                for (level_px, q) in fills {
                    self.fill(&id, &sym, side, level_px, q, "taker")?;
                }
                let leaves = self.own.get(&id).map_or(0, |o| o.leaves);
                if leaves == 0 {
                    if let Some(o) = self.own.get_mut(&id) {
                        o.open = false;
                    }
                } else if tif == Tif::Ioc {
                    if let Some(o) = self.own.get_mut(&id) {
                        o.open = false;
                        o.leaves = 0;
                    }
                    self.counts.cancelled += 1;
                    self.outbox.push_back(EventBody::Cancelled {
                        id,
                        qty: leaves,
                        reason: "ioc",
                    });
                } else {
                    let arrival = self.seq;
                    self.seq += 1;
                    self.resting.entry(sym).or_default().insert(arrival, id);
                }
            }
            Pending::Cancel { id } => match self.own.get_mut(&id) {
                Some(o) if o.open => {
                    let qty = o.leaves;
                    o.open = false;
                    o.leaves = 0;
                    let sym = o.sym.clone();
                    if let Some(q) = self.resting.get_mut(&sym) {
                        q.retain(|_, v| *v != id);
                    }
                    self.counts.cancelled += 1;
                    self.outbox.push_back(EventBody::Cancelled {
                        id,
                        qty,
                        reason: "user",
                    });
                }
                Some(_) => {
                    self.counts.cancel_rejects += 1;
                    self.outbox.push_back(EventBody::CancelReject { id, reason: "not_open" });
                }
                None => {
                    self.counts.cancel_rejects += 1;
                    self.outbox.push_back(EventBody::CancelReject {
                        id,
                        reason: "unknown_order",
                    });
                }
            },
            Pending::Timer { tag } => self.outbox.push_back(EventBody::Timer { tag }),
        }
        Ok(())
    }

    /// Take the strategy's answer to the event just delivered.
    pub fn respond(&mut self, response: &Response) {
        self.counts.decisions += response.decisions.len() as u64;
        self.counts.diags += response.diag.len() as u64;
        let closed = matches!(self.phase, Phase::Ended | Phase::Done);
        for action in &response.actions {
            match action {
                Action::New { id, sym, side, px, qty, tif } => {
                    self.counts.orders += 1;
                    if closed {
                        self.seen_ids.insert(id.clone());
                        self.counts.rejects += 1;
                        self.outbox.push_back(EventBody::Reject {
                            id: id.clone(),
                            reason: "closed",
                        });
                        continue;
                    }
                    let at = self.now.saturating_add(self.cfg.latency_ns);
                    self.pending.insert(
                        (at, 1, self.seq),
                        Pending::New {
                            id: id.clone(),
                            sym: sym.clone(),
                            side: *side,
                            px: *px,
                            qty: *qty,
                            tif: *tif,
                        },
                    );
                    self.seq += 1;
                }
                Action::Cancel { id } => {
                    self.counts.cancels += 1;
                    if closed {
                        self.counts.cancel_rejects += 1;
                        self.outbox.push_back(EventBody::CancelReject {
                            id: id.clone(),
                            reason: "closed",
                        });
                        continue;
                    }
                    let at = self.now.saturating_add(self.cfg.latency_ns);
                    self.pending.insert((at, 1, self.seq), Pending::Cancel { id: id.clone() });
                    self.seq += 1;
                }
                Action::Timer { at, tag } => {
                    let at = (*at).max(self.now);
                    if closed || self.exhausted_at.is_some_and(|end| at > end) {
                        continue;
                    }
                    self.pending.insert((at, 2, self.seq), Pending::Timer { tag: tag.clone() });
                    self.seq += 1;
                }
            }
        }
    }

    /// Positions, cash and counts at the end of the run (compared exactly between versions).
    pub fn summary(&self) -> Value {
        let mut positions = serde_json::Map::new();
        for (sym, p) in &self.positions {
            let book = self.books.get(sym);
            let mark = match p.position.signum() {
                1 => book.and_then(|b| b.best(Side::Buy)),
                -1 => book.and_then(|b| b.best(Side::Sell)),
                _ => Some(0),
            };
            let pnl = mark.and_then(|m| {
                i128::from(p.cash)
                    .checked_add(i128::from(p.position) * i128::from(m))
                    .and_then(|v| i64::try_from(v).ok())
            });
            positions.insert(
                sym.clone(),
                json!({
                    "position": p.position, "cash": p.cash, "bought": p.bought, "sold": p.sold,
                    "pnl_at_exit_price": pnl,
                }),
            );
        }
        let open = self.own.values().filter(|o| o.open).count();
        json!({"positions": positions, "counts": self.counts, "open_orders": open, "steps": self.step})
    }
}

#[cfg(test)]
#[path = "sim_tests.rs"]
mod tests;
