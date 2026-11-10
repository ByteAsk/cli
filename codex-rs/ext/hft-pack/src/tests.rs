//! Tests for the ITCH 5.0 pack. The known-answer vectors are assembled byte by byte from the
//! specification's offset tables (not with the encoder), so the encoder and the decoder cannot
//! share a mistake and still pass.

use super::*;
use crate::itch50::Header;
use crate::itch50::Message;
use crate::itch50::decode;
use crate::itch50::encode;
use crate::itch50::to_json;
use byteask_replay::Rules;
use byteask_replay::compare_records;
use byteask_replay::record::JsonLines;
use byteask_replay::record::sha256_hex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::PathBuf;

/// Seed and size of the checked-in fixture.
pub const FIXTURE_SEED: u64 = 20261018;
pub const FIXTURE_EVENTS: usize = 400;
/// sha256 of fixtures/itch50/synthetic-20261018.itch. Changing the generator changes it; that is
/// a deliberate act (regenerate with BYTEASK_UPDATE_FIXTURES=1 and update this line).
const FIXTURE_SHA256: &str = "bf978321b2a64af17f7a1b9f04581547654602a639a7c2b24a97c554de58d3c2";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/itch50")
}

/// type, locate, tracking, 6-byte timestamp: the 11 bytes every message starts with.
fn head(kind: u8, locate: u16, tracking: u16, ts: u64) -> Vec<u8> {
    let mut v = vec![kind];
    v.extend_from_slice(&locate.to_be_bytes());
    v.extend_from_slice(&tracking.to_be_bytes());
    for shift in [40, 32, 24, 16, 8, 0] {
        v.push((ts >> shift) as u8);
    }
    v
}

const TS: u64 = 0x1234_5678_9abc; // 48 bits, so every timestamp byte matters

#[test]
fn add_order_bytes_decode_at_the_specified_offsets() {
    // §1.3.1: ref @11 (8), side @19, shares @20 (4), stock @24 (8), price @32 (4) = 36 bytes.
    let mut b = head(b'A', 7, 2, TS);
    b.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
    b.push(b'S');
    b.extend_from_slice(&300u32.to_be_bytes());
    b.extend_from_slice(b"AAPL    ");
    b.extend_from_slice(&1_234_500u32.to_be_bytes());
    assert_eq!(b.len(), 36);
    let m = decode(&b).expect("decode");
    assert_eq!(
        m,
        Message::AddOrder {
            h: Header {
                locate: 7,
                tracking: 2,
                ts: TS
            },
            order_ref: 0x0102_0304_0506_0708,
            side: b'S',
            shares: 300,
            stock: *b"AAPL    ",
            price: 1_234_500,
            attribution: None,
        }
    );
    assert_eq!(encode(&m), Some(b.clone()), "encode is the exact inverse");
    // §1.3.2: the same plus attribution @36 (4) = 40 bytes, type F.
    let mut f = b.clone();
    f[0] = b'F';
    f.extend_from_slice(b"GSCO");
    let m = decode(&f).expect("decode F");
    assert!(matches!(m, Message::AddOrder { attribution: Some(a), .. } if &a == b"GSCO"));
    assert_eq!(encode(&m), Some(f));
}

#[test]
fn execution_cancel_delete_replace_and_trade_bytes_decode_at_the_specified_offsets() {
    // E §1.4.1: ref @11, shares @19, match @23 = 31 bytes.
    let mut e = head(b'E', 1, 1, TS);
    e.extend_from_slice(&42u64.to_be_bytes());
    e.extend_from_slice(&100u32.to_be_bytes());
    e.extend_from_slice(&9_000_000_001u64.to_be_bytes());
    assert_eq!(
        decode(&e).expect("E"),
        Message::OrderExecuted {
            h: Header {
                locate: 1,
                tracking: 1,
                ts: TS
            },
            order_ref: 42,
            shares: 100,
            match_number: 9_000_000_001
        }
    );
    // C §1.4.2: ... printable @31, price @32 = 36 bytes.
    let mut c = head(b'C', 1, 1, TS);
    c.extend_from_slice(&42u64.to_be_bytes());
    c.extend_from_slice(&50u32.to_be_bytes());
    c.extend_from_slice(&7u64.to_be_bytes());
    c.push(b'N');
    c.extend_from_slice(&999_900u32.to_be_bytes());
    assert!(matches!(
        decode(&c).expect("C"),
        Message::OrderExecutedWithPrice {
            shares: 50,
            printable: b'N',
            price: 999_900,
            ..
        }
    ));
    // X §1.4.3: ref @11, cancelled shares @19 = 23 bytes.
    let mut x = head(b'X', 1, 1, TS);
    x.extend_from_slice(&42u64.to_be_bytes());
    x.extend_from_slice(&25u32.to_be_bytes());
    assert!(matches!(
        decode(&x).expect("X"),
        Message::OrderCancel {
            order_ref: 42,
            shares: 25,
            ..
        }
    ));
    // D §1.4.4: ref @11 = 19 bytes.
    let mut d = head(b'D', 1, 1, TS);
    d.extend_from_slice(&42u64.to_be_bytes());
    assert!(matches!(
        decode(&d).expect("D"),
        Message::OrderDelete { order_ref: 42, .. }
    ));
    // U §1.4.5: original @11, new @19, shares @27, price @31 = 35 bytes.
    let mut u = head(b'U', 1, 1, TS);
    u.extend_from_slice(&42u64.to_be_bytes());
    u.extend_from_slice(&43u64.to_be_bytes());
    u.extend_from_slice(&700u32.to_be_bytes());
    u.extend_from_slice(&1_000_100u32.to_be_bytes());
    assert!(matches!(
        decode(&u).expect("U"),
        Message::OrderReplace {
            original_ref: 42,
            new_ref: 43,
            shares: 700,
            price: 1_000_100,
            ..
        }
    ));
    // P §1.5.1: ref @11, side @19, shares @20, stock @24, price @32, match @36 = 44 bytes.
    let mut p = head(b'P', 1, 1, TS);
    p.extend_from_slice(&0u64.to_be_bytes());
    p.push(b'B');
    p.extend_from_slice(&200u32.to_be_bytes());
    p.extend_from_slice(b"MSFT    ");
    p.extend_from_slice(&3_100_000u32.to_be_bytes());
    p.extend_from_slice(&5u64.to_be_bytes());
    assert!(matches!(
        decode(&p).expect("P"),
        Message::Trade {
            shares: 200,
            price: 3_100_000,
            match_number: 5,
            ..
        }
    ));
    for bytes in [e, c, x, d, u, p] {
        let m = decode(&bytes).expect("decode");
        assert_eq!(encode(&m), Some(bytes), "{m:?}");
    }
}

#[test]
fn system_event_and_stock_directory_decode() {
    let mut s = head(b'S', 0, 0, TS);
    s.push(b'Q');
    assert!(matches!(
        decode(&s).expect("S"),
        Message::SystemEvent { code: b'Q', .. }
    ));
    // R §1.2.1: stock @11, market category @19, financial status @20, round lot @21 (4),
    // round lots only @25, issue class @26, subtype @27 (2), authenticity @29, short sale @30,
    // IPO @31, LULD tier @32, ETP flag @33, ETP leverage @34 (4), inverse @38 = 39 bytes.
    let mut r = head(b'R', 5, 0, TS);
    r.extend_from_slice(b"QQQ     ");
    r.extend_from_slice(b"QN");
    r.extend_from_slice(&100u32.to_be_bytes());
    r.extend_from_slice(b"NCZ PNN1Y");
    r.extend_from_slice(&3u32.to_be_bytes());
    r.push(b'N');
    assert_eq!(r.len(), 39);
    let m = decode(&r).expect("R");
    assert!(
        matches!(
            m,
            Message::StockDirectory {
                round_lot_size: 100,
                etp_flag: b'Y',
                etp_leverage: 3,
                luld_tier: b'1',
                ..
            }
        ),
        "{m:?}"
    );
    assert_eq!(encode(&m), Some(r));
    assert_eq!(to_json(&m)["stock"], json!("QQQ"));
}

#[test]
fn wrong_lengths_are_errors_and_unknown_types_are_kept_not_guessed() {
    let mut a = head(b'A', 1, 1, TS);
    a.extend_from_slice(&[0u8; 26]); // 11 + 26 = 37 bytes, one too many
    let err = decode(&a).expect_err("37 bytes");
    assert!(err.contains("must be 36 bytes"), "{err}");
    assert!(decode(&[]).is_err());
    assert_eq!(
        decode(b"Hxyz").expect("other"),
        Message::Other { kind: b'H', len: 4 }
    );
}

#[test]
fn a_truncated_capture_is_an_error_that_names_the_message() {
    let mut file = synthetic::to_file(&synthetic::session(3, 20));
    let full: Vec<_> = read_messages(&file[..]).collect();
    assert!(full.iter().all(Result::is_ok));
    file.truncate(file.len() - 3);
    let got: Vec<_> = read_messages(&file[..]).collect();
    let err = got.last().cloned().expect("last").expect_err("truncated");
    assert!(err.contains("truncated"), "{err}");
}

#[test]
fn normalization_maps_every_book_message_and_checks_the_directory() {
    let mut n = normalize::Normalizer::new();
    let h = Header {
        locate: 1,
        tracking: 0,
        ts: 10,
    };
    let dir = decode(&{
        let mut r = head(b'R', 1, 0, 1);
        r.extend_from_slice(b"AAA     QN");
        r.extend_from_slice(&100u32.to_be_bytes());
        r.extend_from_slice(b"NCZ PNN1N");
        r.extend_from_slice(&0u32.to_be_bytes());
        r.push(b'N');
        r
    })
    .expect("R");
    assert_eq!(n.push(&dir).expect("dir"), None);
    let add = Message::AddOrder {
        h,
        order_ref: 5,
        side: b'B',
        shares: 100,
        stock: itch50::stock_field("AAA"),
        price: 999_900,
        attribution: None,
    };
    assert_eq!(
        n.push(&add).expect("add"),
        Some(
            json!({"ts":10,"type":"add","sym":"AAA","id":5,"side":"B","px":999_900,"qty":100,"msg":"A"})
        )
    );
    let wrong = Message::AddOrder {
        h,
        order_ref: 9,
        side: b'B',
        shares: 100,
        stock: itch50::stock_field("BBB"),
        price: 999_900,
        attribution: None,
    };
    assert!(
        n.push(&wrong)
            .expect_err("mismatch")
            .contains("disagrees with the stock directory")
    );
    let c = Message::OrderExecutedWithPrice {
        h,
        order_ref: 5,
        shares: 40,
        match_number: 1,
        printable: b'Y',
        price: 1_000_000,
    };
    assert_eq!(
        n.push(&c).expect("C"),
        Some(json!({"ts":10,"type":"exec","id":5,"qty":40,"px":1_000_000,"msg":"C"}))
    );
    let u = Message::OrderReplace {
        h,
        original_ref: 5,
        new_ref: 6,
        shares: 10,
        price: 3,
    };
    assert_eq!(
        n.push(&u).expect("U"),
        Some(json!({"ts":10,"type":"replace","id":5,"new_id":6,"px":3,"qty":10,"msg":"U"}))
    );
    assert_eq!(
        n.push(&Message::SystemEvent { h, code: b'O' }).expect("S"),
        None
    );
    assert_eq!(n.stats.skipped_by_type.get(&'S'), Some(&1));
}

fn golden(name: &str) -> Vec<Value> {
    let f = std::fs::File::open(fixtures().join(name))
        .expect("golden file (regenerate with BYTEASK_UPDATE_FIXTURES=1)");
    JsonLines::new(std::io::BufReader::new(f))
        .map(|r| r.expect("golden line"))
        .collect()
}

fn decoded_messages(bytes: &[u8]) -> Vec<Value> {
    read_messages(bytes)
        .map(|m| to_json(&m.expect("message")))
        .collect()
}

fn decoded_events(bytes: &[u8]) -> Vec<Value> {
    read_events(bytes).map(|e| e.expect("event")).collect()
}

#[test]
fn the_checked_in_fixture_is_exactly_what_the_seeded_generator_produces() {
    let name = format!("synthetic-{FIXTURE_SEED}");
    let bytes = synthetic::to_file(&synthetic::session(FIXTURE_SEED, FIXTURE_EVENTS));
    if std::env::var("BYTEASK_UPDATE_FIXTURES").as_deref() == Ok("1") {
        std::fs::create_dir_all(fixtures()).expect("dir");
        std::fs::write(fixtures().join(format!("{name}.itch")), &bytes).expect("write");
        let lines = |vs: Vec<Value>| vs.iter().map(|v| format!("{v}\n")).collect::<String>();
        std::fs::write(
            fixtures().join(format!("{name}.messages.jsonl")),
            lines(decoded_messages(&bytes)),
        )
        .expect("write");
        std::fs::write(
            fixtures().join(format!("{name}.events.jsonl")),
            lines(decoded_events(&bytes)),
        )
        .expect("write");
        eprintln!("fixture sha256 {}", sha256_hex(&bytes));
    }
    let on_disk = std::fs::read(fixtures().join(format!("{name}.itch"))).expect("fixture");
    assert_eq!(
        sha256_hex(&on_disk),
        FIXTURE_SHA256,
        "the checked-in fixture changed"
    );
    assert!(
        on_disk == bytes,
        "the generator no longer reproduces the checked-in fixture"
    );
}

#[test]
fn the_fixture_decodes_to_its_golden_files_under_the_replay_library() {
    let name = format!("synthetic-{FIXTURE_SEED}");
    let bytes = std::fs::read(fixtures().join(format!("{name}.itch"))).expect("fixture");
    let exact = Rules::exact();
    for (got, file) in [
        (decoded_messages(&bytes), format!("{name}.messages.jsonl")),
        (decoded_events(&bytes), format!("{name}.events.jsonl")),
    ] {
        let c = compare_records(&golden(&file), &got, &exact).expect("compare");
        assert!(
            c.same,
            "{file}: {}",
            byteask_replay::report::sentence(&c, "golden", "decoded")
        );
    }
    // Decoding is a pure function of the bytes: twice gives the same records.
    let again =
        compare_records(&decoded_events(&bytes), &decoded_events(&bytes), &exact).expect("compare");
    assert!(again.same);
    // Every book message type is in the fixture, so the golden files cover each mapping.
    let kinds: std::collections::BTreeSet<String> = decoded_messages(&bytes)
        .iter()
        .filter_map(|m| m["type"].as_str().map(str::to_string))
        .collect();
    for k in ["S", "R", "A", "F", "E", "C", "X", "D", "U", "P"] {
        assert!(
            kinds.contains(k),
            "the fixture has no {k} message: {kinds:?}"
        );
    }
}

#[test]
fn a_one_byte_change_in_the_fixture_is_found_at_the_right_message() {
    let name = format!("synthetic-{FIXTURE_SEED}");
    let mut bytes = std::fs::read(fixtures().join(format!("{name}.itch"))).expect("fixture");
    // Find the 50th message and change the last byte of its body (a price or share field).
    let mut offset = 0usize;
    for _ in 0..50 {
        let len = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
        offset += 2 + len;
    }
    let len = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
    bytes[offset + 2 + len - 1] ^= 0x01;
    let c = compare_records(
        &golden(&format!("{name}.messages.jsonl")),
        &decoded_messages(&bytes),
        &Rules::exact(),
    )
    .expect("compare");
    let d = c.first_difference.expect("a difference");
    assert_eq!(d.index_a, Some(50), "{d:?}");
}
