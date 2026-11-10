//! Simulator tests: each pins one rule of the deterministic harness.

use super::*;
use crate::model::parse_md;
use pretty_assertions::assert_eq;
use serde_json::json;

fn recs(values: &[Value]) -> Vec<MdRecord> {
    values
        .iter()
        .map(|v| parse_md(v).expect("record"))
        .collect()
}

fn symbols_of(records: &[MdRecord]) -> Vec<String> {
    let mut s = BTreeSet::new();
    for r in records {
        match &r.kind {
            MdKind::Add { sym, .. } | MdKind::Trade { sym, .. } => {
                s.insert(sym.clone());
            }
            _ => {}
        }
    }
    s.into_iter().collect()
}

/// Run a scripted strategy over the records; return the event lines it saw, each followed by
/// what it answered, and the simulator.
fn drive(
    cfg: HarnessConfig,
    initial: &[MdRecord],
    records: &[MdRecord],
    mut strategy: impl FnMut(&Event) -> Vec<Action>,
) -> (Vec<String>, Sim) {
    let mut all = initial.to_vec();
    all.extend_from_slice(records);
    let mut sim = Sim::new(cfg, StepLimits::default(), symbols_of(&all), initial).expect("sim");
    let mut it = records.iter().cloned().map(Ok);
    let mut seen = Vec::new();
    while let Some(ev) = sim.next_event(&mut it).expect("event") {
        let actions = strategy(&ev);
        seen.push(ev.line());
        sim.respond(&Response {
            actions,
            ..Response::default()
        });
    }
    (seen, sim)
}

fn new(id: &str, sym: &str, side: Side, px: i64, qty: i64, tif: Tif) -> Action {
    Action::New {
        id: id.into(),
        sym: sym.into(),
        side,
        px,
        qty,
        tif,
    }
}

fn base_book() -> Vec<MdRecord> {
    recs(&[
        json!({"ts":0,"type":"add","sym":"X","id":1,"side":"B","px":99,"qty":5}),
        json!({"ts":0,"type":"add","sym":"X","id":2,"side":"S","px":101,"qty":3}),
        json!({"ts":0,"type":"add","sym":"X","id":3,"side":"S","px":102,"qty":4}),
    ])
}

#[test]
fn a_session_is_init_then_snapshots_then_the_recording_then_end() {
    let records = recs(&[
        json!({"ts":10,"type":"add","sym":"Y","id":10,"side":"B","px":50,"qty":1}),
        json!({"ts":12,"type":"trade","sym":"X","side":"S","px":100,"qty":2}),
    ]);
    let mut cfg = HarnessConfig::standard();
    cfg.seed = 7;
    cfg.params.insert("k".into(), "v".into());
    let (seen, _) = drive(cfg, &base_book(), &records, |_| Vec::new());
    assert_eq!(
        seen,
        vec![
            "init step=0 ts=10 seed=7 syms=X,Y depth=1 p.k=v",
            "md step=1 ts=10 sym=X kind=snapshot bp1=99 bq1=5 ap1=101 aq1=3",
            "md step=2 ts=10 sym=Y kind=snapshot",
            "md step=3 ts=10 rec=0 sym=Y kind=add id=10 side=B px=50 qty=1 bp1=50 bq1=1",
            "md step=4 ts=12 rec=1 sym=X kind=trade side=S px=100 qty=2 bp1=99 bq1=5 ap1=101 aq1=3",
            "end step=5 ts=12",
        ]
    );
}

#[test]
fn at_equal_times_market_data_comes_before_an_order_arrives() {
    // The order is sent while answering the first event at ts=20; at latency 0 it takes effect
    // at ts=20, but only after the second recorded event that is also stamped 20.
    let records = recs(&[
        json!({"ts":20,"type":"add","sym":"X","id":4,"side":"S","px":101,"qty":1}),
        json!({"ts":20,"type":"add","sym":"X","id":5,"side":"S","px":101,"qty":1}),
        json!({"ts":21,"type":"delete","id":5}),
    ]);
    let mut sent = false;
    let (seen, _) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        if ev.line().contains("rec=0") && !sent {
            sent = true;
            vec![new("o1", "X", Side::Buy, 101, 10, Tif::Ioc)]
        } else {
            Vec::new()
        }
    });
    let kinds: Vec<&str> = seen
        .iter()
        .map(|l| l.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "init",
            "md",
            "md",
            "md",
            "ack",
            "fill",
            "cancelled",
            "md",
            "end"
        ],
        "{seen:#?}"
    );
    // It bought every share displayed at 101 at that moment: 3 + 1 + 1.
    assert!(
        seen[5].contains("px=101 qty=5 leaves=5 liq=taker"),
        "{}",
        seen[5]
    );
    assert!(seen[6].contains("qty=5 reason=ioc"), "{}", seen[6]);
}

#[test]
fn latency_delays_the_effect_of_an_order_in_market_time() {
    let records = recs(&[
        json!({"ts":10,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":12,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":20,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
    ]);
    let mut cfg = HarnessConfig::standard();
    cfg.latency_ns = 5;
    let (seen, _) = drive(cfg, &base_book(), &records, |ev| {
        if ev.line().contains("rec=0") {
            vec![new("o1", "X", Side::Buy, 101, 1, Tif::Ioc)]
        } else {
            Vec::new()
        }
    });
    let ack = seen.iter().position(|l| l.starts_with("ack")).expect("ack");
    assert!(seen[ack].contains("ts=15"), "{}", seen[ack]);
    assert!(seen[ack - 1].contains("rec=1"), "after the event at 12");
    assert!(seen[ack + 2].contains("rec=2"), "before the event at 20");
}

#[test]
fn the_same_displayed_shares_never_fill_twice_and_a_day_order_rests_then_fills_as_maker() {
    let records = recs(&[
        json!({"ts":1,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":2,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        // A new seller joins at 100: it crosses the strategy's resting bid at 100.
        json!({"ts":3,"type":"add","sym":"X","id":9,"side":"S","px":100,"qty":2}),
    ]);
    let (seen, sim) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        let l = ev.line();
        if l.contains("rec=0") {
            vec![
                new("a", "X", Side::Buy, 101, 2, Tif::Ioc),
                new("b", "X", Side::Buy, 101, 2, Tif::Ioc),
            ]
        } else if l.contains("rec=1") {
            vec![new("c", "X", Side::Buy, 100, 5, Tif::Day)]
        } else {
            Vec::new()
        }
    });
    let fills: Vec<&String> = seen.iter().filter(|l| l.starts_with("fill")).collect();
    assert!(
        fills[0].contains("id=a px=101 qty=2 leaves=0 liq=taker"),
        "{fills:?}"
    );
    assert!(
        fills[1].contains("id=b px=101 qty=1 leaves=1 liq=taker"),
        "only 1 share was left: {fills:?}"
    );
    assert!(
        fills[2].contains("id=c px=100 qty=2 leaves=3 liq=maker"),
        "{fills:?}"
    );
    assert_eq!(fills.len(), 3);
    let summary = sim.summary();
    assert_eq!(summary["positions"]["X"]["position"], json!(5));
    assert_eq!(
        summary["positions"]["X"]["cash"],
        json!(-(101 * 3 + 100 * 2))
    );
    // Long 5, marked at the best bid 99: -503 + 5 * 99.
    assert_eq!(
        summary["positions"]["X"]["pnl_at_exit_price"],
        json!(-503 + 495)
    );
    assert_eq!(summary["open_orders"], json!(1));
}

#[test]
fn every_reject_reason_is_reachable_and_deterministic() {
    let records = recs(&[json!({"ts":1,"type":"trade","sym":"X","side":"B","px":101,"qty":1})]);
    let mut cfg = HarnessConfig::standard();
    cfg.tick = 1;
    cfg.max_order_qty = Some(100);
    cfg.price_band = Some(10);
    cfg.max_position = Some(50);
    cfg.reject_every = 9;
    let orders = vec![
        new("d", "X", Side::Buy, 95, 1, Tif::Day), // 1 accepted (rests)
        new("d", "X", Side::Buy, 95, 1, Tif::Day), // 2 duplicate_id
        new("e", "Q", Side::Buy, 95, 1, Tif::Day), // 3 unknown_symbol
        new("f", "X", Side::Buy, 95, 0, Tif::Day), // 4 bad_qty
        new("g", "X", Side::Buy, 95, 101, Tif::Day), // 5 max_qty
        new("h", "X", Side::Buy, 200, 1, Tif::Day), // 6 price_band (mid 100)
        new("i", "X", Side::Buy, 95, 50, Tif::Day), // 7 position_limit (1 open + 50 > 50)
        new("j", "X", Side::Sell, 105, 1, Tif::Day), // 8 accepted
        new("k", "X", Side::Sell, 105, 1, Tif::Day), // 9 injected (every 9th)
    ];
    let (seen, _) = drive(cfg, &base_book(), &records, |ev| {
        if ev.line().contains("rec=0") {
            orders.clone()
        } else {
            Vec::new()
        }
    });
    let outcomes: Vec<String> = seen
        .iter()
        .filter(|l| l.starts_with("ack") || l.starts_with("reject"))
        .map(|l| {
            l.split(' ')
                .filter(|t| t.starts_with("id=") || t.starts_with("reason="))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            "id=d",
            "id=d reason=duplicate_id",
            "id=e reason=unknown_symbol",
            "id=f reason=bad_qty",
            "id=g reason=max_qty",
            "id=h reason=price_band",
            "id=i reason=position_limit",
            "id=j",
            "id=k reason=injected",
        ]
    );
    let mut ticky = HarnessConfig::standard();
    ticky.tick = 5;
    let (seen, _) = drive(ticky, &base_book(), &records, |ev| {
        if ev.line().contains("rec=0") {
            vec![new("t", "X", Side::Buy, 96, 1, Tif::Day)]
        } else {
            Vec::new()
        }
    });
    assert!(
        seen.iter().any(|l| l.contains("reason=bad_tick")),
        "{seen:?}"
    );
}

#[test]
fn seeded_rejects_repeat_for_one_seed_and_move_with_another() {
    let records: Vec<MdRecord> = (0..40)
        .map(|i| {
            parse_md(&json!({"ts":i,"type":"trade","sym":"X","side":"B","px":101,"qty":1}))
                .expect("rec")
        })
        .collect();
    let run = |seed: u64| {
        let mut cfg = HarnessConfig::standard();
        cfg.seed = seed;
        cfg.reject_ppm = 300_000;
        let mut n = 0;
        let (seen, _) = drive(cfg, &base_book(), &records, |ev| {
            if ev.line().starts_with("md") && ev.line().contains("kind=trade") {
                n += 1;
                vec![new(&format!("o{n}"), "X", Side::Buy, 90, 1, Tif::Ioc)]
            } else {
                Vec::new()
            }
        });
        seen.into_iter()
            .filter(|l| l.contains("reason=injected"))
            .collect::<Vec<_>>()
    };
    let a = run(1);
    assert_eq!(a, run(1), "same seed, same rejects");
    assert!(
        !a.is_empty() && a.len() < 40,
        "about 30% of 40: {}",
        a.len()
    );
    assert_ne!(a, run(2), "a different seed rejects different orders");
}

#[test]
fn cancels_answer_cancelled_or_a_reason() {
    let records = recs(&[
        json!({"ts":1,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":2,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
    ]);
    let (seen, _) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        let l = ev.line();
        if l.contains("rec=0") {
            vec![
                new("r", "X", Side::Buy, 95, 3, Tif::Day),
                new("t", "X", Side::Buy, 101, 1, Tif::Ioc),
            ]
        } else if l.contains("rec=1") {
            vec![
                Action::Cancel { id: "r".into() },
                Action::Cancel { id: "t".into() },
                Action::Cancel { id: "zz".into() },
            ]
        } else {
            Vec::new()
        }
    });
    let tail: Vec<&String> = seen.iter().filter(|l| l.starts_with("cancel")).collect();
    assert!(
        tail[0].contains("cancelled") && tail[0].contains("id=r qty=3 reason=user"),
        "{tail:?}"
    );
    assert!(
        tail[1].contains("cancel_reject") && tail[1].contains("id=t reason=not_open"),
        "{tail:?}"
    );
    assert!(tail[2].contains("id=zz reason=unknown_order"), "{tail:?}");
}

#[test]
fn timers_fire_in_market_time_after_market_data_at_the_same_time() {
    let records = recs(&[
        json!({"ts":10,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":30,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":30,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
    ]);
    let (seen, _) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        if ev.line().contains("rec=0") {
            vec![
                Action::Timer {
                    at: 30,
                    tag: "late".into(),
                },
                Action::Timer {
                    at: 20,
                    tag: "early".into(),
                },
                Action::Timer {
                    at: 5,
                    tag: "past".into(),
                },
            ]
        } else {
            Vec::new()
        }
    });
    let order: Vec<String> = seen
        .iter()
        .skip(2)
        .map(|l| {
            let ts = l.split(' ').find(|t| t.starts_with("ts=")).unwrap_or("");
            let what = l
                .split(' ')
                .find(|t| t.starts_with("tag=") || t.starts_with("rec="))
                .unwrap_or("end");
            format!("{ts} {what}")
        })
        .collect();
    assert_eq!(
        order,
        vec![
            "ts=10 rec=0",
            "ts=10 tag=past",
            "ts=20 tag=early",
            "ts=30 rec=1",
            "ts=30 rec=2",
            "ts=30 tag=late",
            "ts=30 end"
        ]
    );
}

#[test]
fn after_the_end_of_data_orders_are_rejected_and_the_session_stops() {
    let records = recs(&[json!({"ts":1,"type":"trade","sym":"X","side":"B","px":101,"qty":1})]);
    let (seen, _) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        if ev.line().starts_with("end") {
            vec![
                new("late", "X", Side::Buy, 101, 1, Tif::Ioc),
                Action::Cancel { id: "x".into() },
                Action::Timer {
                    at: 99,
                    tag: "t".into(),
                },
            ]
        } else {
            Vec::new()
        }
    });
    let n = seen.len();
    assert!(seen[n - 2].contains("id=late reason=closed"), "{seen:?}");
    assert!(
        seen[n - 1].contains("cancel_reject") && seen[n - 1].contains("reason=closed"),
        "{seen:?}"
    );
}

#[test]
fn a_feedback_loop_hits_the_per_time_limit_instead_of_hanging() {
    let records = recs(&[json!({"ts":1,"type":"trade","sym":"X","side":"B","px":101,"qty":1})]);
    let all = {
        let mut a = base_book();
        a.extend(records.clone());
        a
    };
    let limits = StepLimits {
        max_steps: 1_000_000,
        max_steps_per_ts: 500,
    };
    let mut sim = Sim::new(
        HarnessConfig::standard(),
        limits,
        symbols_of(&all),
        &base_book(),
    )
    .expect("sim");
    let mut it = records.into_iter().map(Ok);
    let mut n = 0u64;
    let err = loop {
        match sim.next_event(&mut it) {
            Ok(Some(_)) => {
                n += 1;
                sim.respond(&Response {
                    actions: vec![new(&format!("o{n}"), "X", Side::Buy, 90, 1, Tif::Ioc)],
                    ..Response::default()
                });
            }
            Ok(None) => panic!("the loop must not end on its own"),
            Err(e) => break e,
        }
    };
    assert!(
        matches!(&err, SimError::Limit(m) if m.contains("more than 500 events at time 1")),
        "{err}"
    );
}

#[test]
fn a_strict_book_refuses_unknown_orders_and_a_loose_one_counts_them() {
    let records = recs(&[json!({"ts":1,"type":"delete","id":42})]);
    let mut sim = Sim::new(
        HarnessConfig::standard(),
        StepLimits::default(),
        vec!["X".into()],
        &base_book(),
    )
    .expect("sim");
    let mut it = records.clone().into_iter().map(Ok);
    let mut err = None;
    for _ in 0..10 {
        match sim.next_event(&mut it) {
            Ok(Some(_)) => sim.respond(&Response::default()),
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(err, Some(SimError::Recording(m)) if m.contains("record 0: order id 42 is not in the book"))
    );
    let mut loose = HarnessConfig::standard();
    loose.strict_book = false;
    let (seen, sim) = drive(loose, &base_book(), &records, |_| Vec::new());
    assert_eq!(sim.counts().unknown_order_events, 1);
    assert!(
        !seen.iter().any(|l| l.contains("rec=0")),
        "a skipped record is not shown"
    );
}

#[test]
fn a_recording_that_goes_back_in_time_is_an_error() {
    let records = recs(&[
        json!({"ts":5,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":4,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
    ]);
    let mut sim = Sim::new(
        HarnessConfig::standard(),
        StepLimits::default(),
        vec!["X".into()],
        &base_book(),
    )
    .expect("sim");
    let mut it = records.into_iter().map(Ok);
    let err = loop {
        match sim.next_event(&mut it) {
            Ok(Some(_)) => sim.respond(&Response::default()),
            Ok(None) => panic!("must fail"),
            Err(e) => break e,
        }
    };
    assert!(matches!(err, SimError::Recording(m) if m.contains("goes back in time")));
}

#[test]
fn config_validation_names_the_bad_setting() {
    for (patch, want) in [
        (json!({"latency_ns": -1}), "latency_ns"),
        (json!({"depth": 0}), "depth"),
        (json!({"depth": 21}), "depth"),
        (json!({"tick": 0}), "tick"),
        (json!({"reject_ppm": 1000001}), "reject_ppm"),
        (json!({"params": {"a b": "1"}}), "param"),
        (json!({"max_position": -5}), "max_position"),
    ] {
        let cfg: HarnessConfig = serde_json::from_value(patch.clone()).expect("shape");
        let err = cfg.validate().expect_err(&patch.to_string());
        assert!(err.contains(want), "{patch}: {err}");
    }
    assert!(
        serde_json::from_value::<HarnessConfig>(json!({"latency": 5})).is_err(),
        "typo refused"
    );
    let defaults: HarnessConfig = serde_json::from_value(json!({})).expect("defaults");
    assert_eq!(defaults, HarnessConfig::standard());
}

#[test]
fn a_strategy_that_always_re_arms_its_timer_still_reaches_the_end() {
    let records = recs(&[
        json!({"ts":100,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
        json!({"ts":200,"type":"trade","sym":"X","side":"B","px":101,"qty":1}),
    ]);
    let (seen, _) = drive(HarnessConfig::standard(), &base_book(), &records, |ev| {
        if ev.line().starts_with("init") || ev.line().starts_with("timer") {
            vec![Action::Timer {
                at: ev.ts + 30,
                tag: "hb".into(),
            }]
        } else {
            Vec::new()
        }
    });
    let timers = seen.iter().filter(|l| l.starts_with("timer")).count();
    // 130, 160, 190 fire; 220 is after the last recorded time (200) and never does.
    assert_eq!(timers, 3, "{seen:#?}");
    assert!(
        seen.last()
            .is_some_and(|l| l.starts_with("end") && l.contains("ts=200")),
        "{seen:#?}"
    );
}
