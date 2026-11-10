//! Tests for the comparator. A defect here hits the verify gate, port_check and the perf lab at
//! once, so every rule has a test that fails if the rule regresses, plus seeded randomized checks
//! of the invariants the rules promise.

use super::*;
use crate::record::RecordError;
use pretty_assertions::assert_eq;
use serde_json::json;

fn rules(text: &str) -> Rules {
    Rules::from_json(text).expect("rules")
}

fn cmp(a: &[Value], b: &[Value], r: &Rules) -> Comparison {
    compare_records(a, b, r).expect("comparison runs")
}

fn first(c: &Comparison) -> &Difference {
    c.first_difference.as_ref().expect("a difference")
}

// ---- exact order ----------------------------------------------------------------------------

#[test]
fn identical_streams_are_the_same_and_fully_read() {
    let a = vec![json!({"seq":1,"px":100}), json!({"seq":2,"px":101})];
    let c = cmp(&a, &a, &Rules::exact());
    assert!(c.same);
    assert!(c.read_to_end);
    assert_eq!((c.records_a, c.records_b), (2, 2));
    assert_eq!(c.first_difference, None);
    assert_eq!(c.rules, "exact order; all values exact");
}

#[test]
fn first_difference_is_reported_at_the_right_record_and_field() {
    let a: Vec<Value> = (0..10).map(|i| json!({"seq": i, "qty": 10})).collect();
    let mut b = a.clone();
    b[6] = json!({"seq": 6, "qty": 11});
    b[8] = json!({"seq": 8, "qty": 12}); // a later difference must not be the one reported
    let c = cmp(&a, &b, &rules(r#"{"key":"/seq"}"#));
    let d = first(&c);
    assert!(!c.same);
    assert_eq!(d.kind, DiffKind::ValueDiffers);
    assert_eq!((d.index_a, d.index_b), (Some(6), Some(6)));
    assert_eq!(d.path, "/qty");
    assert_eq!(
        (d.a.clone(), d.b.clone()),
        (Some(json!(10)), Some(json!(11)))
    );
    assert_eq!(d.rule, "exact");
    assert_eq!(d.key, Some(json!(6)));
    assert_eq!(d.record_a, Some(json!({"seq":6,"qty":10})));
    assert_eq!(d.record_b, Some(json!({"seq":6,"qty":11})));
    assert_eq!(
        d.context_a,
        vec![
            json!({"seq":3,"qty":10}),
            json!({"seq":4,"qty":10}),
            json!({"seq":5,"qty":10})
        ],
        "the three equal records before the difference"
    );
    assert!(!c.read_to_end, "stops at the first difference");
    assert_eq!((c.records_a, c.records_b), (7, 7));
}

#[test]
fn a_reordering_is_a_difference_under_exact_order() {
    let a = vec![json!(1), json!(2)];
    let b = vec![json!(2), json!(1)];
    let d = first(&cmp(&a, &b, &Rules::exact())).clone();
    assert_eq!((d.index_a, d.path.as_str()), (Some(0), ""));
}

#[test]
fn a_shorter_stream_is_reported_as_missing_or_extra_records() {
    let a = vec![json!(1), json!(2), json!(3)];
    let b = vec![json!(1), json!(2)];
    let c = cmp(&a, &b, &Rules::exact());
    let d = first(&c);
    assert_eq!(d.kind, DiffKind::RecordMissingInB);
    assert_eq!((d.index_a, d.index_b), (Some(2), None));
    assert_eq!(d.record_a, Some(json!(3)));
    let c = cmp(&b, &a, &Rules::exact());
    let d = first(&c);
    assert_eq!(d.kind, DiffKind::RecordExtraInB);
    assert_eq!((d.index_a, d.index_b), (None, Some(2)));
    // Empty against non-empty, both ways.
    assert_eq!(first(&cmp(&[], &a, &Rules::exact())).index_b, Some(0));
    assert!(cmp(&[], &[], &Rules::exact()).same);
}

#[test]
fn inside_a_record_keys_are_visited_in_sorted_order() {
    // Both /a and /b differ; the report must deterministically name /a, whatever order the
    // producer wrote the keys in.
    let a = vec![json!({"b": 1, "a": 1})];
    let b = vec![json!({"a": 2, "b": 2})];
    assert_eq!(first(&cmp(&a, &b, &Rules::exact())).path, "/a");
}

#[test]
fn nested_paths_missing_and_extra_fields_and_type_changes() {
    let r = Rules::exact();
    let d = diff_values(&json!({"o":{"x":[1,2]}}), &json!({"o":{"x":[1,3]}}), &r).expect("diff");
    assert_eq!(
        (d.kind, d.path.as_str()),
        (DiffKind::ValueDiffers, "/o/x/1")
    );
    let d = diff_values(&json!({"o":{"x":[1,2]}}), &json!({"o":{"x":[1]}}), &r).expect("diff");
    assert_eq!((d.kind, d.path.as_str()), (DiffKind::MissingInB, "/o/x/1"));
    let d = diff_values(&json!({"a":1}), &json!({"a":1,"z":null}), &r).expect("diff");
    assert_eq!(
        (d.kind, d.path.as_str(), d.b),
        (DiffKind::ExtraInB, "/z", Some(json!(null)))
    );
    let d = diff_values(&json!({"a":"1"}), &json!({"a":1}), &r).expect("diff");
    assert_eq!(d.kind, DiffKind::TypeDiffers);
    let d = diff_values(&json!(true), &json!(false), &r).expect("diff");
    assert_eq!(d.kind, DiffKind::ValueDiffers);
    // Keys that need escaping are rendered as valid JSON Pointers.
    let d = diff_values(&json!({"a/b":1}), &json!({"a/b":2}), &r).expect("diff");
    assert_eq!(d.path, "/a~1b");
}

// ---- numbers --------------------------------------------------------------------------------

#[test]
fn exact_numbers_compare_by_value_and_integers_never_round_through_a_double() {
    let r = Rules::exact();
    let same = |a: Value, b: Value| diff_values(&a, &b, &r).is_none();
    assert!(
        same(json!(1), json!(1.0)),
        "Python prints 1.0 where C++ prints 1"
    );
    assert!(same(json!(-0.0), json!(0)));
    assert!(same(json!(0.1), json!(0.1)));
    assert!(
        !same(json!(0.1), json!(f64::from_bits(0.1f64.to_bits() + 1))),
        "one ulp is a difference"
    );
    assert!(same(json!(u64::MAX), json!(u64::MAX)));
    assert!(!same(json!(u64::MAX), json!(u64::MAX - 1)));
    assert!(!same(json!(i64::MIN), json!(i64::MAX)));
    // 2^53 + 1 is not a double; comparing through f64 would call these equal.
    assert!(!same(
        json!(9_007_199_254_740_993_u64),
        json!(9_007_199_254_740_992_u64)
    ));
    let parsed: Value = serde_json::from_str("9007199254740993.0").expect("json");
    assert!(
        !same(json!(9_007_199_254_740_993_u64), parsed),
        "the float literal is really 2^53, so it must not equal the integer 2^53+1"
    );
    assert!(!same(json!(1), json!(1.5)));
    assert!(!same(json!(3), json!(1e300)));
}

#[test]
fn tolerances_accept_inside_the_bound_and_refuse_just_outside_it() {
    let abs = rules(r#"{"numbers":[{"path":"/x","abs":0.5}]}"#);
    assert!(
        diff_values(&json!({"x":1.0}), &json!({"x":1.5}), &abs).is_none(),
        "exactly at abs"
    );
    let d = diff_values(&json!({"x":1.0}), &json!({"x":1.5000001}), &abs).expect("outside");
    assert_eq!(d.rule, "abs<=5e-1");
    assert!(diff_values(&json!({"x":1}), &json!({"x":2}), &abs).is_some());

    let rel = rules(r#"{"numbers":[{"path":"/x","rel":1e-3}]}"#);
    assert!(diff_values(&json!({"x":1000.0}), &json!({"x":1000.9}), &rel).is_none());
    assert!(diff_values(&json!({"x":1000.0}), &json!({"x":1001.1}), &rel).is_some());
    assert!(
        diff_values(&json!({"x":0.0}), &json!({"x":1e-300}), &rel).is_some(),
        "rel near 0"
    );

    let ulps = rules(r#"{"numbers":[{"path":"/x","ulps":2}]}"#);
    let one_up = f64::from_bits(1.0f64.to_bits() + 2);
    let three_up = f64::from_bits(1.0f64.to_bits() + 3);
    assert!(diff_values(&json!({"x":1.0}), &json!({"x":one_up}), &ulps).is_none());
    assert!(diff_values(&json!({"x":1.0}), &json!({"x":three_up}), &ulps).is_some());
    // Across zero: the smallest positive and negative subnormals are 2 representable values apart.
    let tiny = f64::from_bits(1);
    assert_eq!(ulps_apart(tiny, -tiny), 2);
    assert_eq!(ulps_apart(-0.0, 0.0), 0);
    assert_eq!(ulps_apart(f64::MAX, -f64::MAX), 2 * f64::MAX.to_bits());
}

#[test]
fn a_tolerance_applies_only_to_its_path_and_never_between_a_number_and_a_string() {
    let r = rules(r#"{"numbers":[{"path":"/diag/*","abs":1}]}"#);
    assert!(diff_values(&json!({"diag":{"pnl":1}}), &json!({"diag":{"pnl":1.9}}), &r).is_none());
    let d = diff_values(&json!({"px":1}), &json!({"px":1.9}), &r).expect("px is exact");
    assert_eq!((d.path.as_str(), d.rule.as_str()), ("/px", "exact"));
    let d =
        diff_values(&json!({"diag":{"pnl":1}}), &json!({"diag":{"pnl":"1"}}), &r).expect("type");
    assert_eq!(d.kind, DiffKind::TypeDiffers);
    // Nested deeper than the wildcard: not covered.
    assert!(
        diff_values(
            &json!({"diag":{"a":{"b":1}}}),
            &json!({"diag":{"a":{"b":1.5}}}),
            &r
        )
        .is_some()
    );
}

// ---- ignore ---------------------------------------------------------------------------------

#[test]
fn an_ignored_field_never_counts_present_absent_or_different() {
    let r = rules(r#"{"ignore":["/wall_ns","/legs/*/venue_ts"]}"#);
    let base = json!({"px":1,"wall_ns":5,"legs":[{"q":1,"venue_ts":9}]});
    for other in [
        json!({"px":1,"wall_ns":6,"legs":[{"q":1,"venue_ts":10}]}),
        json!({"px":1,"legs":[{"q":1}]}),
        json!({"px":1,"wall_ns":"x","legs":[{"q":1,"venue_ts":null}]}),
    ] {
        assert_eq!(diff_values(&base, &other, &r), None, "{other}");
        assert_eq!(diff_values(&other, &base, &r), None, "{other}");
    }
    assert!(diff_values(&base, &json!({"px":2,"wall_ns":5,"legs":[{"q":1}]}), &r).is_some());
    let everything = rules(r#"{"ignore":[""]}"#);
    assert!(diff_values(&json!(1), &json!("x"), &everything).is_none());
}

// ---- any order ------------------------------------------------------------------------------

#[test]
fn any_order_accepts_a_permutation_but_counts_duplicates() {
    let r = rules(r#"{"ordering":"unordered"}"#);
    let a = vec![
        json!({"id":1}),
        json!({"id":2}),
        json!({"id":2}),
        json!({"id":3}),
    ];
    let b = vec![
        json!({"id":3}),
        json!({"id":2}),
        json!({"id":1}),
        json!({"id":2}),
    ];
    assert!(cmp(&a, &b, &r).same);
    // Same set, different multiplicities: a multiset comparison must catch it.
    let b2 = vec![
        json!({"id":3}),
        json!({"id":2}),
        json!({"id":1}),
        json!({"id":1}),
    ];
    let c = cmp(&a, &b2, &r);
    assert!(!c.same);
    assert_eq!((c.unmatched_a, c.unmatched_b), (1, 1));
    let d = first(&c);
    // A's second id-2 (index 2) has no partner, nor does B's second id-1 (index 3); the lower
    // index is reported, with the other as the closest candidate.
    assert_eq!(d.kind, DiffKind::RecordMissingInB);
    assert_eq!(d.index_a, Some(2));
    assert_eq!(d.record_a, Some(json!({"id":2})));
    let hint = d.closest.clone().expect("closest");
    assert_eq!(
        (hint.path.as_str(), hint.a, hint.b),
        ("/id", Some(json!(2)), Some(json!(1)))
    );
}

#[test]
fn any_order_with_tolerance_finds_the_pairing_a_greedy_match_would_miss() {
    // A0 is within 0.1 of both B0 and B1, A1 only of B0. Greedy pairs A0-B0 and strands A1; the
    // maximum matching pairs A0-B1 and A1-B0. Reporting a difference here would be a false FAIL.
    let r = rules(r#"{"ordering":"unordered","numbers":[{"path":"/v","abs":0.1}]}"#);
    let a = vec![json!({"v":1.05}), json!({"v":0.95})];
    let b = vec![json!({"v":1.0}), json!({"v":1.1})];
    let c = cmp(&a, &b, &r);
    assert!(c.same, "{:?}", c.first_difference);
}

#[test]
fn any_order_refuses_rather_than_guesses_above_the_pair_limit() {
    let mut r = rules(r#"{"ordering":"unordered","numbers":[{"path":"/v","abs":0.1}]}"#);
    r.pair_limit = 3;
    let a = vec![json!({"v":1.0}), json!({"v":1.0})];
    let b = vec![json!({"v":1.01}), json!({"v":1.02})];
    let err = compare_records(&a, &b, &r).expect_err("4 pairs > 3");
    assert_eq!(err, CompareError::TooManyLookAlikes { pairs: 4, limit: 3 });
    assert!(err.to_string().contains("grouped_by"), "{err}");
    // Exactly-equal look-alikes never need pairwise checks, whatever their number.
    let exact = rules(r#"{"ordering":"unordered"}"#);
    let many = vec![json!({"v":1}); 5000];
    assert!(cmp(&many, &many, &exact).same);
}

// ---- grouped --------------------------------------------------------------------------------

#[test]
fn grouped_allows_reordering_inside_a_group_only() {
    let r = rules(r#"{"ordering":{"grouped_by":"/seq"}}"#);
    let a = vec![
        json!({"seq":1,"op":"new","id":1}),
        json!({"seq":1,"op":"new","id":2}),
        json!({"seq":2,"op":"cancel","id":1}),
    ];
    let b = vec![
        json!({"seq":1,"op":"new","id":2}),
        json!({"seq":1,"op":"new","id":1}),
        json!({"seq":2,"op":"cancel","id":1}),
    ];
    assert!(cmp(&a, &b, &r).same);
    // Moving the cancel into the first group changes which input caused it: a difference.
    let b2 = vec![
        json!({"seq":1,"op":"new","id":2}),
        json!({"seq":1,"op":"cancel","id":1}),
        json!({"seq":1,"op":"new","id":1}),
        json!({"seq":2,"op":"new","id":3}),
    ];
    let c = cmp(&a, &b2, &r);
    let d = first(&c);
    assert_eq!(d.kind, DiffKind::RecordExtraInB, "{d:?}");
    assert_eq!(d.index_b, Some(1));
}

#[test]
fn grouped_reports_absolute_indices_and_key_mismatches() {
    let r = rules(r#"{"ordering":{"grouped_by":"/seq"},"key":"/seq"}"#);
    let a = vec![
        json!({"seq":1,"x":1}),
        json!({"seq":2,"x":1}),
        json!({"seq":2,"x":2}),
    ];
    let b = vec![
        json!({"seq":1,"x":1}),
        json!({"seq":2,"x":2}),
        json!({"seq":2,"x":3}),
    ];
    let c = cmp(&a, &b, &r);
    let d = first(&c);
    assert_eq!(d.kind, DiffKind::RecordMissingInB);
    assert_eq!(
        d.index_a,
        Some(1),
        "absolute index in A, not the index inside the group"
    );
    assert_eq!(d.key, Some(json!(2)));
    assert_eq!(d.context_a, vec![json!({"seq":1,"x":1})]);
    // Different group keys at the same position.
    let b3 = vec![
        json!({"seq":1,"x":1}),
        json!({"seq":3,"x":1}),
        json!({"seq":3,"x":2}),
    ];
    let d = first(&cmp(&a, &b3, &r)).clone();
    assert_eq!(d.kind, DiffKind::GroupKeyDiffers);
    assert_eq!(
        (d.index_a, d.index_b, d.a, d.b),
        (Some(1), Some(1), Some(json!(2)), Some(json!(3)))
    );
    // A whole trailing group missing in B.
    let d = first(&cmp(&a, &a[..1], &r)).clone();
    assert_eq!((d.kind, d.index_a), (DiffKind::RecordMissingInB, Some(1)));
}

#[test]
fn grouped_treats_a_key_that_comes_back_later_as_a_new_group() {
    let r = rules(r#"{"ordering":{"grouped_by":"/k"}}"#);
    let a = vec![
        json!({"k":1,"v":"a"}),
        json!({"k":2,"v":"b"}),
        json!({"k":1,"v":"c"}),
    ];
    let b = vec![
        json!({"k":1,"v":"c"}),
        json!({"k":2,"v":"b"}),
        json!({"k":1,"v":"a"}),
    ];
    assert!(
        !cmp(&a, &b, &r).same,
        "groups are consecutive runs, not global buckets"
    );
}

// ---- unreadable input -----------------------------------------------------------------------

#[test]
fn an_unreadable_record_is_an_error_never_a_verdict() {
    let bad = RecordError {
        index: 1,
        location: "line 2".to_string(),
        message: "not valid JSON".to_string(),
    };
    for ordering in [
        r#"{}"#,
        r#"{"ordering":"unordered"}"#,
        r#"{"ordering":{"grouped_by":"/k"}}"#,
    ] {
        let r = rules(ordering);
        let a = vec![Ok(json!({"k":1})), Err(bad.clone())];
        let b = vec![Ok(json!({"k":1})), Ok(json!({"k":2}))];
        let err = compare_streams(a.clone(), b.clone(), &r).expect_err(ordering);
        assert_eq!(
            err,
            CompareError::BadRecord {
                side: Side::A,
                error: bad.clone()
            },
            "{ordering}"
        );
        let err = compare_streams(b, a, &r).expect_err(ordering);
        assert!(
            matches!(err, CompareError::BadRecord { side: Side::B, .. }),
            "{ordering}"
        );
    }
}

// ---- seeded randomized invariants -----------------------------------------------------------

/// SplitMix64: tiny, deterministic, and good enough to explore record shapes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_record(rng: &mut Rng, seq: u64) -> Value {
    let mut legs = Vec::new();
    for _ in 0..rng.below(3) {
        legs.push(json!({"px": rng.below(5) as i64 - 2, "qty": rng.below(4)}));
    }
    json!({
        "seq": seq,
        "side": if rng.below(2) == 0 { "B" } else { "S" },
        "legs": legs,
        "diag": {"edge": (rng.below(1000) as f64) / 7.0},
        "wall_ns": rng.next(),
    })
}

#[test]
fn randomized_self_comparison_is_always_the_same_under_every_ordering() {
    let mut rng = Rng(7);
    for round in 0..200 {
        let n = rng.below(12);
        let a: Vec<Value> = (0..n).map(|i| random_record(&mut rng, i / 2)).collect();
        for text in [
            r#"{}"#,
            r#"{"ordering":"unordered"}"#,
            r#"{"ordering":{"grouped_by":"/seq"}}"#,
        ] {
            assert!(cmp(&a, &a, &rules(text)).same, "round {round} {text}");
        }
    }
}

#[test]
fn randomized_single_mutation_is_found_at_exactly_that_record() {
    let mut rng = Rng(11);
    for round in 0..300 {
        let n = 1 + rng.below(20);
        let a: Vec<Value> = (0..n).map(|i| random_record(&mut rng, i)).collect();
        let k = rng.below(n) as usize;
        let mut b = a.clone();
        let (path, new) = match rng.below(3) {
            0 => ("/side", json!(if a[k]["side"] == "B" { "S" } else { "B" })),
            1 => ("/seq", json!(a[k]["seq"].as_u64().unwrap_or(0) + 1000)),
            _ => (
                "/diag/edge",
                json!(a[k]["diag"]["edge"].as_f64().unwrap_or(0.0) + 1.0),
            ),
        };
        *b[k].pointer_mut(path).expect("path exists") = new;
        let strict = rules(r#"{"ignore":["/wall_ns"]}"#);
        let c = cmp(&a, &b, &strict);
        let d = first(&c);
        assert_eq!(
            (d.index_a, d.path.as_str()),
            (Some(k as u64), path),
            "round {round}"
        );
        // A tolerance wide enough for the diag change hides it, and only it.
        let loose = rules(r#"{"ignore":["/wall_ns"],"numbers":[{"path":"/diag/edge","abs":2}]}"#);
        assert_eq!(
            cmp(&a, &b, &loose).same,
            path == "/diag/edge",
            "round {round}"
        );
        // Shuffling B does not hide it from any-order comparison.
        let mut shuffled = b.clone();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let unordered = rules(r#"{"ordering":"unordered","ignore":["/wall_ns"]}"#);
        assert!(!cmp(&a, &shuffled, &unordered).same, "round {round}");
        let mut a_shuffled = a.clone();
        for i in (1..a_shuffled.len()).rev() {
            a_shuffled.swap(i, rng.below(i as u64 + 1) as usize);
        }
        assert!(cmp(&a, &a_shuffled, &unordered).same, "round {round}");
    }
}

#[test]
fn randomized_equal_under_rules_implies_same_signature() {
    // Any-order matching only pairs records that share a signature. If two records are equal
    // under the rules but get different signatures, a real pair would be reported as a
    // difference. Check that invariant on perturbations that the rules must accept.
    let r = rules(
        r#"{"ignore":["/wall_ns","/legs/2"],"numbers":[{"path":"/diag/edge","abs":0.5},{"path":"/legs/*/px","ulps":1}]}"#,
    );
    let mut rng = Rng(23);
    for round in 0..500 {
        let seq = rng.below(5);
        let a = random_record(&mut rng, seq);
        let mut b = a.clone();
        match rng.below(5) {
            0 => {
                b["wall_ns"] = json!(rng.next());
            }
            1 => {
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("wall_ns");
                }
            }
            2 => {
                let e = b["diag"]["edge"].as_f64().unwrap_or(0.0);
                b["diag"]["edge"] = json!(e + 0.25);
            }
            3 => {
                if let Some(legs) = b["legs"].as_array_mut() {
                    // Index 2 is ignored: adding or dropping it must not matter.
                    if legs.len() == 2 {
                        legs.push(json!({"anything": true}));
                    } else if legs.len() == 3 {
                        legs.pop();
                    }
                }
            }
            _ => {
                b["seq"] = json!(b["seq"].as_u64().unwrap_or(0) as f64); // 3 vs 3.0
            }
        }
        if diff_values(&a, &b, &r).is_none() {
            assert_eq!(
                signature(&a, &r),
                signature(&b, &r),
                "round {round}: {a} vs {b}"
            );
        }
        // And with no tolerance leaves, the same signature means equal.
        let exact = rules(r#"{"ignore":["/wall_ns","/legs/2"]}"#);
        if signature(&a, &exact) == signature(&b, &exact) {
            assert_eq!(
                diff_values(&a, &b, &exact),
                None,
                "round {round}: {a} vs {b}"
            );
        }
    }
}
