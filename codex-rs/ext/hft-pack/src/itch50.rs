//! Nasdaq TotalView-ITCH 5.0 messages: decode, encode, and render as JSON.
//!
//! Written from the public specification (Nasdaq TotalView-ITCH 5.0, sections 1.1-1.5). Every
//! message starts with the same 11 bytes: type (1), stock locate (2), tracking number (2) and a
//! 6-byte timestamp in nanoseconds since midnight. Integers are big-endian; prices are
//! `Price(4)`, a 4-byte integer with four implied decimal places (1000000 = $100.0000); stock
//! symbols are 8 bytes, right-padded with spaces.
//!
//! Decoded here: S System Event (12 bytes), R Stock Directory (39), A Add Order (36), F Add Order
//! with MPID (40), E Order Executed (31), C Order Executed With Price (36), X Order Cancel (23),
//! D Order Delete (19), U Order Replace (35), P Trade (44). Every other type is kept as `Other`
//! (type and length), so a full-day file decodes without error and the caller decides what to do
//! with messages that do not change the displayed book. A known type with the wrong length is an
//! error: it means the file is not ITCH 5.0 or is misframed.

use serde_json::Value;
use serde_json::json;

/// The 11 bytes every message starts with (after the type byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub locate: u16,
    pub tracking: u16,
    /// Nanoseconds since midnight (48 bits on the wire).
    pub ts: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    SystemEvent {
        h: Header,
        code: u8,
    },
    StockDirectory {
        h: Header,
        stock: [u8; 8],
        market_category: u8,
        financial_status: u8,
        round_lot_size: u32,
        round_lots_only: u8,
        issue_classification: u8,
        issue_subtype: [u8; 2],
        authenticity: u8,
        short_sale_threshold: u8,
        ipo_flag: u8,
        luld_tier: u8,
        etp_flag: u8,
        etp_leverage: u32,
        inverse: u8,
    },
    AddOrder {
        h: Header,
        order_ref: u64,
        side: u8,
        shares: u32,
        stock: [u8; 8],
        price: u32,
        /// Present for type F (Add Order with MPID attribution).
        attribution: Option<[u8; 4]>,
    },
    OrderExecuted {
        h: Header,
        order_ref: u64,
        shares: u32,
        match_number: u64,
    },
    OrderExecutedWithPrice {
        h: Header,
        order_ref: u64,
        shares: u32,
        match_number: u64,
        printable: u8,
        price: u32,
    },
    OrderCancel {
        h: Header,
        order_ref: u64,
        shares: u32,
    },
    OrderDelete {
        h: Header,
        order_ref: u64,
    },
    OrderReplace {
        h: Header,
        original_ref: u64,
        new_ref: u64,
        shares: u32,
        price: u32,
    },
    Trade {
        h: Header,
        order_ref: u64,
        side: u8,
        shares: u32,
        stock: [u8; 8],
        price: u32,
        match_number: u64,
    },
    /// A message type this decoder does not interpret (kept so a full file still decodes).
    Other {
        kind: u8,
        len: usize,
    },
}

/// Wire length of the message types decoded here.
pub fn expected_len(kind: u8) -> Option<usize> {
    Some(match kind {
        b'S' => 12,
        b'R' => 39,
        b'A' => 36,
        b'F' => 40,
        b'E' => 31,
        b'C' => 36,
        b'X' => 23,
        b'D' => 19,
        b'U' => 35,
        b'P' => 44,
        _ => return None,
    })
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn u8(&self, at: usize) -> u8 {
        self.0[at]
    }
    fn u16(&self, at: usize) -> u16 {
        u16::from_be_bytes([self.0[at], self.0[at + 1]])
    }
    fn u32(&self, at: usize) -> u32 {
        u32::from_be_bytes([self.0[at], self.0[at + 1], self.0[at + 2], self.0[at + 3]])
    }
    fn u48(&self, at: usize) -> u64 {
        self.0[at..at + 6].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
    }
    fn u64(&self, at: usize) -> u64 {
        self.0[at..at + 8].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
    }
    fn arr<const N: usize>(&self, at: usize) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.0[at..at + N]);
        out
    }
    fn header(&self) -> Header {
        Header {
            locate: self.u16(1),
            tracking: self.u16(3),
            ts: self.u48(5),
        }
    }
}

/// Decode one message (the bytes after the file's length prefix).
pub fn decode(bytes: &[u8]) -> Result<Message, String> {
    let Some(&kind) = bytes.first() else {
        return Err("empty message".to_string());
    };
    let Some(want) = expected_len(kind) else {
        return Ok(Message::Other {
            kind,
            len: bytes.len(),
        });
    };
    if bytes.len() != want {
        return Err(format!(
            "message type {:?} must be {want} bytes (ITCH 5.0), got {}",
            kind as char,
            bytes.len()
        ));
    }
    let r = Rd(bytes);
    let h = r.header();
    Ok(match kind {
        b'S' => Message::SystemEvent { h, code: r.u8(11) },
        b'R' => Message::StockDirectory {
            h,
            stock: r.arr(11),
            market_category: r.u8(19),
            financial_status: r.u8(20),
            round_lot_size: r.u32(21),
            round_lots_only: r.u8(25),
            issue_classification: r.u8(26),
            issue_subtype: r.arr(27),
            authenticity: r.u8(29),
            short_sale_threshold: r.u8(30),
            ipo_flag: r.u8(31),
            luld_tier: r.u8(32),
            etp_flag: r.u8(33),
            etp_leverage: r.u32(34),
            inverse: r.u8(38),
        },
        b'A' | b'F' => Message::AddOrder {
            h,
            order_ref: r.u64(11),
            side: r.u8(19),
            shares: r.u32(20),
            stock: r.arr(24),
            price: r.u32(32),
            attribution: (kind == b'F').then(|| r.arr(36)),
        },
        b'E' => Message::OrderExecuted {
            h,
            order_ref: r.u64(11),
            shares: r.u32(19),
            match_number: r.u64(23),
        },
        b'C' => Message::OrderExecutedWithPrice {
            h,
            order_ref: r.u64(11),
            shares: r.u32(19),
            match_number: r.u64(23),
            printable: r.u8(31),
            price: r.u32(32),
        },
        b'X' => Message::OrderCancel {
            h,
            order_ref: r.u64(11),
            shares: r.u32(19),
        },
        b'D' => Message::OrderDelete {
            h,
            order_ref: r.u64(11),
        },
        b'U' => Message::OrderReplace {
            h,
            original_ref: r.u64(11),
            new_ref: r.u64(19),
            shares: r.u32(27),
            price: r.u32(31),
        },
        b'P' => Message::Trade {
            h,
            order_ref: r.u64(11),
            side: r.u8(19),
            shares: r.u32(20),
            stock: r.arr(24),
            price: r.u32(32),
            match_number: r.u64(36),
        },
        _ => Message::Other {
            kind,
            len: bytes.len(),
        },
    })
}

struct Wr(Vec<u8>);

impl Wr {
    fn new(kind: u8, h: &Header) -> Self {
        let mut v = vec![kind];
        v.extend_from_slice(&h.locate.to_be_bytes());
        v.extend_from_slice(&h.tracking.to_be_bytes());
        v.extend_from_slice(&h.ts.to_be_bytes()[2..]);
        Self(v)
    }
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
}

/// Encode a message (without the file's length prefix). `Other` cannot be encoded.
pub fn encode(m: &Message) -> Option<Vec<u8>> {
    Some(
        match m {
            Message::SystemEvent { h, code } => Wr::new(b'S', h).u8(*code),
            Message::StockDirectory {
                h,
                stock,
                market_category,
                financial_status,
                round_lot_size,
                round_lots_only,
                issue_classification,
                issue_subtype,
                authenticity,
                short_sale_threshold,
                ipo_flag,
                luld_tier,
                etp_flag,
                etp_leverage,
                inverse,
            } => Wr::new(b'R', h)
                .bytes(stock)
                .u8(*market_category)
                .u8(*financial_status)
                .u32(*round_lot_size)
                .u8(*round_lots_only)
                .u8(*issue_classification)
                .bytes(issue_subtype)
                .u8(*authenticity)
                .u8(*short_sale_threshold)
                .u8(*ipo_flag)
                .u8(*luld_tier)
                .u8(*etp_flag)
                .u32(*etp_leverage)
                .u8(*inverse),
            Message::AddOrder { h, order_ref, side, shares, stock, price, attribution } => {
                let w = Wr::new(if attribution.is_some() { b'F' } else { b'A' }, h)
                    .u64(*order_ref)
                    .u8(*side)
                    .u32(*shares)
                    .bytes(stock)
                    .u32(*price);
                match attribution {
                    Some(a) => w.bytes(a),
                    None => w,
                }
            }
            Message::OrderExecuted { h, order_ref, shares, match_number } => {
                Wr::new(b'E', h).u64(*order_ref).u32(*shares).u64(*match_number)
            }
            Message::OrderExecutedWithPrice { h, order_ref, shares, match_number, printable, price } => {
                Wr::new(b'C', h)
                    .u64(*order_ref)
                    .u32(*shares)
                    .u64(*match_number)
                    .u8(*printable)
                    .u32(*price)
            }
            Message::OrderCancel { h, order_ref, shares } => Wr::new(b'X', h).u64(*order_ref).u32(*shares),
            Message::OrderDelete { h, order_ref } => Wr::new(b'D', h).u64(*order_ref),
            Message::OrderReplace { h, original_ref, new_ref, shares, price } => {
                Wr::new(b'U', h).u64(*original_ref).u64(*new_ref).u32(*shares).u32(*price)
            }
            Message::Trade { h, order_ref, side, shares, stock, price, match_number } => Wr::new(b'P', h)
                .u64(*order_ref)
                .u8(*side)
                .u32(*shares)
                .bytes(stock)
                .u32(*price)
                .u64(*match_number),
            Message::Other { .. } => return None,
        }
        .0,
    )
}

/// A symbol field without its right padding.
pub fn stock_text(stock: &[u8; 8]) -> String {
    String::from_utf8_lossy(stock).trim_end_matches(' ').to_string()
}

/// An 8-byte, space-padded symbol field.
pub fn stock_field(s: &str) -> [u8; 8] {
    let mut out = [b' '; 8];
    for (o, b) in out.iter_mut().zip(s.bytes()) {
        *o = b;
    }
    out
}

fn ch(b: u8) -> String {
    (b as char).to_string()
}

/// The message as one JSON object (field names from the specification, in snake case).
pub fn to_json(m: &Message) -> Value {
    let head = |kind: &str, h: &Header| json!({"type": kind, "locate": h.locate, "tracking": h.tracking, "ts": h.ts});
    let mut v = match m {
        Message::SystemEvent { h, code } => {
            let mut v = head("S", h);
            v["event_code"] = json!(ch(*code));
            v
        }
        Message::StockDirectory { h, stock, market_category, round_lot_size, etp_leverage, .. } => {
            let mut v = head("R", h);
            v["stock"] = json!(stock_text(stock));
            v["market_category"] = json!(ch(*market_category));
            v["round_lot_size"] = json!(round_lot_size);
            v["etp_leverage"] = json!(etp_leverage);
            v
        }
        Message::AddOrder { h, order_ref, side, shares, stock, price, attribution } => {
            let mut v = head(if attribution.is_some() { "F" } else { "A" }, h);
            v["order_ref"] = json!(order_ref);
            v["side"] = json!(ch(*side));
            v["shares"] = json!(shares);
            v["stock"] = json!(stock_text(stock));
            v["price"] = json!(price);
            if let Some(a) = attribution {
                v["attribution"] = json!(String::from_utf8_lossy(a).trim_end().to_string());
            }
            v
        }
        Message::OrderExecuted { h, order_ref, shares, match_number } => {
            let mut v = head("E", h);
            v["order_ref"] = json!(order_ref);
            v["shares"] = json!(shares);
            v["match_number"] = json!(match_number);
            v
        }
        Message::OrderExecutedWithPrice { h, order_ref, shares, match_number, printable, price } => {
            let mut v = head("C", h);
            v["order_ref"] = json!(order_ref);
            v["shares"] = json!(shares);
            v["match_number"] = json!(match_number);
            v["printable"] = json!(ch(*printable));
            v["price"] = json!(price);
            v
        }
        Message::OrderCancel { h, order_ref, shares } => {
            let mut v = head("X", h);
            v["order_ref"] = json!(order_ref);
            v["shares"] = json!(shares);
            v
        }
        Message::OrderDelete { h, order_ref } => {
            let mut v = head("D", h);
            v["order_ref"] = json!(order_ref);
            v
        }
        Message::OrderReplace { h, original_ref, new_ref, shares, price } => {
            let mut v = head("U", h);
            v["original_ref"] = json!(original_ref);
            v["new_ref"] = json!(new_ref);
            v["shares"] = json!(shares);
            v["price"] = json!(price);
            v
        }
        Message::Trade { h, order_ref, side, shares, stock, price, match_number } => {
            let mut v = head("P", h);
            v["order_ref"] = json!(order_ref);
            v["side"] = json!(ch(*side));
            v["shares"] = json!(shares);
            v["stock"] = json!(stock_text(stock));
            v["price"] = json!(price);
            v["match_number"] = json!(match_number);
            v
        }
        Message::Other { kind, len } => json!({"type": ch(*kind), "len": len, "decoded": false}),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|_, x| !x.is_null());
    }
    v
}
