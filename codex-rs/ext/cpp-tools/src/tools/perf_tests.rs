//! Fixture tests for `cpp_perf`. Parsers are pure functions over captured
//! output (a real callgrind annotate from the prod VM, plus representative
//! perf-report / perf-stat / cachegrind blocks) so a format drift is caught here
//! before it reaches a user. Invocation-level behavior (the trace-children
//! regression, real perf/valgrind runs) is covered by the `#[ignore]` live tests.

use pretty_assertions::assert_eq;

use super::*;

// ---------------- time mode: order, render, notes ----------------

#[test]
fn rounds_alternate_ab_then_ba() {
    // ABBA: over any two rounds each command runs once first and once second.
    let orders: Vec<&[usize]> = (0..4).map(|round| round_order(round, 2)).collect();
    assert_eq!(orders, vec![&[0, 1][..], &[1, 0], &[0, 1], &[1, 0]]);
    assert_eq!(round_order(0, 1), &[0]);
    assert_eq!(round_order(7, 1), &[0]);
}

#[test]
fn render_time_ab_names_winner() {
    let a = Measurement::from_summary(2.0, Some(0.001), 5, 0);
    let b = Measurement::from_summary(1.0, Some(0.001), 5, 0);
    let out = render_time(5, &a, Some(&b), &[]);
    assert!(
        out.starts_with(
            "Method: wall-clock timing, A and B interleaved (repeat=5, warmup=1 untimed each)\n"
        ),
        "{out}"
    );
    assert!(out.contains("B is 2.00x faster than A"));
}

#[test]
fn render_time_ab_failure_invalidates() {
    // A crashing B (2/5 non-zero) must not be reported as "faster".
    let a = Measurement::from_summary(2.0, Some(0.001), 5, 0);
    let b = Measurement::from_summary(0.001, Some(0.0), 5, 2);
    let out = render_time(5, &a, Some(&b), &[]);
    assert!(out.contains("INVALID COMPARISON: B exited non-zero on 2/5 run(s)"));
    assert!(!out.contains("faster than"));
}

#[test]
fn render_time_under_the_floor_says_more_runs_will_not_help() {
    // 2% apart with a tiny spread: statistically clean, but under the 3% floor.
    let a = Measurement::from_summary(1.00, Some(0.0001), 30, 0);
    let b = Measurement::from_summary(1.02, Some(0.0001), 30, 0);
    let out = render_time(30, &a, Some(&b), &[]);
    assert!(out.contains("INSIDE the measurement noise (gate 3.0%)"), "{out}");
    assert!(out.contains("benchmark the code in-process with cpp_bench"), "{out}");
    assert!(!out.contains("increase repeat"), "{out}");
}

#[test]
fn floor_hint_states_the_floor_in_force() {
    let pct = format!("under {}%", TIME_FLOOR * 100.0);
    assert!(TIME_FLOOR_HINT.contains(&pct), "{TIME_FLOOR_HINT} vs {pct}");
}

#[test]
fn render_time_single_with_noise_has_no_trailing_newline() {
    let a = Measurement::from_summary(0.0123, Some(0.0001), 5, 0);
    let out = render_time(5, &a, None, &[]);
    assert!(out.starts_with("Method: wall-clock timing (repeat=5, warmup=1 untimed)\n"));
    assert!(out.ends_with("(±0.0001s stderr)"));
}

#[test]
fn render_time_single_run_notes_no_estimate() {
    let a = Measurement::from_summary(0.5, None, 1, 0);
    let out = render_time(1, &a, None, &[]);
    assert!(out.contains("single run — no noise estimate"));
    // repeat=1 → no warmup note in the header.
    assert!(out.starts_with("Method: wall-clock timing (repeat=1)\n"));
}

#[test]
fn render_time_appends_notes_last() {
    let a = Measurement::from_summary(0.5, Some(0.01), 5, 0);
    let out = render_time(5, &a, None, &["note: one".to_string(), "note: two".to_string()]);
    assert!(out.ends_with("stderr)\nnote: one\nnote: two"), "{out}");
}

#[test]
fn shell_share_note_only_when_startup_distorts_the_numbers() {
    let fast = Measurement::from_summary(0.004, Some(0.0001), 5, 0);
    let slow = Measurement::from_summary(0.5, Some(0.001), 5, 0);
    // 2 ms of a 4 ms mean is half of it.
    let note = shell_share_note(Some(0.002), &fast, Some(&slow)).expect("note");
    assert!(note.contains("~2.0 ms of every run is shell and process startup"), "{note}");
    assert!(note.contains("50% of the fastest mean"), "{note}");
    // 2 ms of a 0.5 s run is noise-level: say nothing.
    assert_eq!(shell_share_note(Some(0.002), &slow, None), None);
    // Unmeasured startup: say nothing rather than guess.
    assert_eq!(shell_share_note(None, &fast, Some(&slow)), None);
}

// ---------------- profile mode: perf report ----------------

const PERF_REPORT: &str = "\
# Samples: 10K of event 'cycles'
#
# Children      Self  Command  Shared Object      Symbol
# ........  ........  .......  .................  .....................
#
    99.96%    41.20%  hot      hot                [.] hot_loop
    53.90%    12.00%  hot      hot                [.] push_back
     8.00%     8.00%  hot      [unknown]          [.] 0x0000000000401234
";

#[test]
fn parses_perf_report_rows_and_strips_marker() {
    let rows = parse_perf_report(PERF_REPORT);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].symbol, "hot_loop");
    assert!((rows[0].self_pct - 41.20).abs() < 1e-9);
    assert!((rows[0].children - 99.96).abs() < 1e-9);
    // Unresolved: [unknown] dso + a hex symbol.
    assert!(rows[2].is_unresolved());
    assert!(!rows[0].is_unresolved());
}

#[test]
fn symbol_quality_ok_when_mostly_symbolized() {
    let rows = parse_perf_report(PERF_REPORT);
    // unresolved self% = 8 of total 61.2 ≈ 13% < 20% → OK.
    let line = symbol_quality_line(&rows);
    assert!(line.starts_with("symbol quality: OK"), "{line}");
}

#[test]
fn symbol_quality_poor_when_mostly_unresolved() {
    let stdout = "\
# Children      Self  Command  Shared Object   Symbol
    90.00%    90.00%  x        [unknown]       [.] 0x00001111
    10.00%    10.00%  x        x               [.] real_fn
";
    let rows = parse_perf_report(stdout);
    let line = symbol_quality_line(&rows);
    assert!(line.starts_with("symbol quality: POOR"), "{line}");
    assert!(line.contains("Rebuild with -g"));
}

#[test]
fn render_perf_profile_sorts_and_labels() {
    let rows = parse_perf_report(PERF_REPORT);
    let out = render_perf_profile(&rows);
    assert!(out.starts_with("Method: perf profile"));
    // Hottest by self% is hot_loop (41.2), appears before push_back.
    let hot = out.find("hot_loop").unwrap();
    let push = out.find("push_back").unwrap();
    assert!(hot < push);
    assert!(out.contains("symbol quality:"));
}

// ---------------- profile mode: callgrind (real VM annotate) ----------------

const CALLGRIND_ANNOTATE: &str = "\
--------------------------------------------------------------------------------
Profile data file 'cg.out.3508822' (creator: callgrind-3.19.0)
--------------------------------------------------------------------------------
Events recorded:  Ir
--------------------------------------------------------------------------------
Ir
--------------------------------------------------------------------------------
320,136,758 (100.0%)  PROGRAM TOTALS
--------------------------------------------------------------------------------
Ir                    file:function
--------------------------------------------------------------------------------
320,000,007 (99.96%)  /tmp/x/hot.c:hot_loop [/tmp/x/hot]
            100  (0.00%)  /tmp/x/hot.c:main [/tmp/x/hot]
--------------------------------------------------------------------------------
";

#[test]
fn parses_callgrind_program_total() {
    assert_eq!(parse_callgrind_total(CALLGRIND_ANNOTATE), 320_136_758);
}

#[test]
fn parses_callgrind_rows_ignoring_header_junk() {
    // The regression: header/totals lines must NOT be surfaced as functions.
    let rows = parse_callgrind_rows(CALLGRIND_ANNOTATE);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].function, "hot_loop");
    assert_eq!(rows[0].ir, 320_000_007);
    assert!((rows[0].pct - 99.96).abs() < 1e-9);
    assert_eq!(rows[1].function, "main");
    // PROGRAM TOTALS was never parsed as a function row.
    assert!(!rows.iter().any(|r| r.function.contains("TOTALS")));
}

/// Real `callgrind_annotate` rows (valgrind 3.19, the prod VM) from a C++
/// program: padded percentages, and names full of `::`, `()`, `<>` and spaces.
const CALLGRIND_ANNOTATE_CPP: &str = "\
--------------------------------------------------------------------------------
215,554,713 (100.0%)  PROGRAM TOTALS

--------------------------------------------------------------------------------
Ir                   file:function
--------------------------------------------------------------------------------
77,244,808 (35.84%)  hotfn.cpp:engine::Book::hash(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&) [/work/hotfn]
18,311,200 ( 8.49%)  /usr/include/c++/12/bits/stl_iterator.h:__gnu_cxx::__normal_iterator<char const*, std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > >::base() const [/work/hotfn]
 2,183,010 ( 1.01%)  ???:std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> >::basic_string(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> >&&) [/usr/lib/x86_64-linux-gnu/libstdc++.so.6.0.30]
 1,436,034 ( 0.67%)  ./string/../sysdeps/x86_64/multiarch/memmove-vec-unaligned-erms.S:__memcpy_avx_unaligned_erms [/usr/lib/x86_64-linux-gnu/libc.so.6]
--------------------------------------------------------------------------------
";

#[test]
fn parses_callgrind_cpp_names_and_padded_percentages() {
    // The regression: these rows came out as "allocator<char> > const&" with
    // 0.00% for every row under 10%.
    let rows = parse_callgrind_rows(CALLGRIND_ANNOTATE_CPP);
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0].function,
        "engine::Book::hash(std::__cxx11::basic_string<char, std::char_traits<char>, \
         std::allocator<char> > const&)"
    );
    assert_eq!(rows[0].ir, 77_244_808);
    assert!((rows[0].pct - 35.84).abs() < 1e-9);
    assert!((rows[1].pct - 8.49).abs() < 1e-9, "padded percentage: {}", rows[1].pct);
    assert!(rows[1].function.starts_with("__gnu_cxx::__normal_iterator<char const*, "));
    assert!(rows[1].function.ends_with(">::base() const"), "{}", rows[1].function);
    // Unknown file: the name still starts after the `???:`.
    assert!(rows[2].function.starts_with("std::__cxx11::basic_string<char, "));
    assert_eq!(rows[3].function, "__memcpy_avx_unaligned_erms");
    assert!((rows[3].pct - 0.67).abs() < 1e-9);

    let out = render_callgrind_profile(&rows, 1, "3329582");
    assert!(out.contains("  8.49  "), "{out}");
    assert!(out.contains("engine::Book::hash(std::__cxx11::basic_string<cha…"), "{out}");
}

/// Real `callgrind_annotate` output (valgrind 3.19) WITH its default
/// auto-annotation: after the function table come the annotated source lines
/// and an "events annotated" footer, all in the same `<count> (<pct>%)` shape.
const CALLGRIND_ANNOTATE_AUTO: &str = "\
--------------------------------------------------------------------------------
150,138,455 (100.0%)  PROGRAM TOTALS

--------------------------------------------------------------------------------
Ir                    file:function
--------------------------------------------------------------------------------
150,000,018 (99.91%)  /work/addloop.c:main [/work/addloop]

--------------------------------------------------------------------------------
-- Auto-annotated source: /work/addloop.c
--------------------------------------------------------------------------------
Ir

          .           #include <stdio.h>
          1 ( 0.00%)  int main(void) {
          2 ( 0.00%)      unsigned long a = 1, b = 2, c = 3, d = 4;
150,000,001 (99.91%)      for (unsigned long i = 0; i < 30000000UL; ++i) { a += i; b ^= i; c += 3; d ^= 5; }
         11 ( 0.00%)      printf(\"%lu\\n\", a + b + c + d);
      2,841 ( 0.00%)  => ./stdio-common/./stdio-common/printf.c:printf (1x)
          3 ( 0.00%)  }

--------------------------------------------------------------------------------
Ir
--------------------------------------------------------------------------------
150,000,018 (99.91%)  events annotated
";

#[test]
fn callgrind_table_ends_before_annotated_source() {
    // The regression: source lines and the footer came out
    // as hot "functions": `for (unsigned long i = 0; ...` and `events
    // annotated` at 99.91%, `int main(void` at 0.00%.
    let rows = parse_callgrind_rows(CALLGRIND_ANNOTATE_AUTO);
    let names: Vec<&str> = rows.iter().map(|r| r.function.as_str()).collect();
    assert_eq!(names, vec!["main"]);
}

#[tokio::test]
async fn callgrind_profile_lists_functions_not_source_lines() {
    if !crate::program_args::fixture::have("valgrind") {
        eprintln!("skip: valgrind not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    if build_hot(dir.path()).await.is_none() {
        eprintln!("skip: no C compiler");
        return;
    }
    let out = match time_tool(dir.path())
        .callgrind_report("./hot", Duration::from_secs(120))
        .await
        .expect("callgrind")
    {
        ProfileOutcome::Report(text) => text,
        _ => panic!("expected a callgrind report"),
    };
    assert!(out.contains("hot_loop"), "{out}");
    // hot.c is built with -g, so auto-annotation would print its source.
    assert!(!out.contains("s+=i%7"), "source line reported as a function:\n{out}");
    assert!(!out.contains("events annotated"), "{out}");
}

#[test]
fn callgrind_profile_reports_symbol_quality() {
    // The guidance promises a symbol-quality check for mode=profile, but only
    // the perf backend had one. A stripped binary profiles as `???:0x...`
    // (real rows, valgrind 3.19).
    let stripped = "\
Ir                    file:function
--------------------------------------------------------------------------------
320,000,007 (99.96%)  ???:0x0000000000001139 [/work/hot_stripped]
         97 ( 0.00%)  ???:_dl_relocate_object [/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2]

";
    let out = render_callgrind_profile(&parse_callgrind_rows(stripped), 1, "1");
    assert!(
        out.contains("symbol quality: POOR (~100% of top-table Ir mass unsymbolized"),
        "{out}"
    );
    assert!(out.contains("do not strip the binary"), "{out}");
    let out = render_callgrind_profile(&parse_callgrind_rows(CALLGRIND_ANNOTATE_CPP), 1, "1");
    assert!(out.contains("symbol quality: OK (~100% of top-table Ir mass symbolized"), "{out}");
}

#[tokio::test]
async fn callgrind_profile_of_a_stripped_binary_says_symbols_are_poor() {
    if !crate::program_args::fixture::have("valgrind") || !crate::program_args::fixture::have("strip")
    {
        eprintln!("skip: valgrind or strip not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(hot) = build_hot(dir.path()).await else {
        eprintln!("skip: no C compiler");
        return;
    };
    let stripped = dir.path().join("hot_stripped");
    let status = std::process::Command::new("strip")
        .arg("-o")
        .arg(&stripped)
        .arg(&hot)
        .status()
        .expect("strip");
    assert!(status.success());
    let out = match time_tool(dir.path())
        .callgrind_report("./hot_stripped", Duration::from_secs(120))
        .await
        .expect("callgrind")
    {
        ProfileOutcome::Report(text) => text,
        _ => panic!("expected a callgrind report"),
    };
    assert!(out.contains("symbol quality: POOR"), "{out}");
}

#[test]
fn render_callgrind_profile_tags_simulated_and_names_pid() {
    let rows = parse_callgrind_rows(CALLGRIND_ANNOTATE);
    let out = render_callgrind_profile(&rows, 2, "3508822");
    assert!(out.contains("[simulated: instruction counts, not wall time]"));
    assert!(out.contains("profiled 2 process(es); showing the hottest (PID 3508822)"));
    assert!(out.contains("hot_loop"));
}

#[test]
fn callgrind_parts_and_pid_extraction() {
    let dir = std::env::temp_dir().join(format!("byteask-cgtest-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("cg.out.111"), "x").unwrap();
    std::fs::write(dir.join("cg.out.222"), "x").unwrap();
    std::fs::write(dir.join("unrelated.txt"), "x").unwrap();
    let parts = callgrind_parts(&dir);
    assert_eq!(parts.len(), 2);
    assert_eq!(part_pid(&dir.join("cg.out.222")), "222");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------- topdown: perf --topdown (real TMA) ----------------

const PERF_TOPDOWN: &str = "\
 Performance counter stats for 'bash -c ./hot':

             retiring  bad speculation  frontend bound  backend bound
S0-C0            12.3%             2.1%            5.0%          80.6%

       1.234 seconds time elapsed
";

#[test]
fn parses_perf_topdown_picks_max_bucket() {
    let (winner, table) = parse_perf_topdown(PERF_TOPDOWN).expect("parsed");
    assert_eq!(winner, "backend-bound");
    assert_eq!(table.len(), 4);
}

#[test]
fn perf_topdown_none_when_no_table() {
    assert!(parse_perf_topdown("no topdown here").is_none());
}

#[test]
fn render_tma_is_plain_classification() {
    let (winner, table) = parse_perf_topdown(PERF_TOPDOWN).expect("parsed");
    let out = render_topdown_tma(winner, &table);
    assert!(out.contains("real Top-down analysis"));
    assert!(out.contains("bottleneck: backend-bound"));
    // No "[heuristic]"/"[simulated]" hedge for real TMA.
    assert!(!out.contains("[heuristic]"));
    assert!(!out.contains("[simulated]"));
}

// ---------------- topdown: generic counters (heuristic) ----------------

fn counters_stderr(body: &str) -> String {
    format!(" Performance counter stats for 'bash -c ./x':\n\n{body}\n       0.5 seconds time elapsed\n")
}

#[test]
fn classify_bad_speculation() {
    let s = counters_stderr(
        "     1,000,000,000      cycles\n\
             2,000,000,000      instructions              #    2.00  insn per cycle\n\
               100,000,000      branches\n\
                 6,000,000      branch-misses             #    6.00% of all branches",
    );
    let c = parse_perf_counters(&s);
    assert_eq!(classify_counters(&c), Some(Bottleneck::BadSpeculation));
}

#[test]
fn classify_memory_bound() {
    let s = counters_stderr(
        "     1,000,000,000      cycles\n\
             1,000,000,000      instructions              #    1.00  insn per cycle\n\
               100,000,000      branches\n\
                 1,000,000      branch-misses             #    1.00% of all branches\n\
               100,000,000      cache-references\n\
                20,000,000      cache-misses              #   20.00 % of all cache refs\n\
               500,000,000      stalled-cycles-backend",
    );
    let c = parse_perf_counters(&s);
    assert_eq!(classify_counters(&c), Some(Bottleneck::MemoryBound));
}

#[test]
fn classify_retiring() {
    let s = counters_stderr(
        "     1,000,000,000      cycles\n\
             3,000,000,000      instructions              #    3.00  insn per cycle\n\
               100,000,000      branches\n\
                   100,000      branch-misses             #    0.10% of all branches",
    );
    let c = parse_perf_counters(&s);
    assert_eq!(classify_counters(&c), Some(Bottleneck::Retiring));
}

#[test]
fn classify_insufficient_when_events_missing() {
    // Only cycles + instructions counted (no branches) → cannot classify.
    let s = counters_stderr(
        "     1,000,000,000      cycles\n\
             2,000,000,000      instructions\n\
       <not supported>      branches\n\
       <not counted>      branch-misses",
    );
    let c = parse_perf_counters(&s);
    assert_eq!(classify_counters(&c), None);
    let rendered = render_counters_topdown(&c);
    assert!(rendered.contains("insufficient counters to classify"));
    assert!(rendered.contains("[heuristic]"));
}

#[test]
fn render_counters_is_hedged_with_blind_spots() {
    let s = counters_stderr(
        "     1,000,000,000      cycles\n\
             2,000,000,000      instructions\n\
               100,000,000      branches\n\
                 6,000,000      branch-misses",
    );
    let c = parse_perf_counters(&s);
    let out = render_counters_topdown(&c);
    assert!(out.contains("[heuristic]"));
    assert!(out.contains("suggests: bad-speculation"));
    assert!(out.contains("blind spots:"));
    assert!(out.contains("fix:"));
}

// ---------------- topdown: cachegrind (simulated) ----------------

const CACHEGRIND: &str = "\
==12345== D   refs:      1,000,000
==12345== D1  misses:       50,000
==12345== LLd misses:       10,000
==12345== D1  miss rate:      5.0%
==12345== LLd miss rate:      1.5%
==12345== Branches:        200,000
==12345== Mispredicts:      20,000
==12345== Mispred rate:      10.0%
";

#[test]
fn parses_cachegrind_rates() {
    let r = parse_cachegrind(CACHEGRIND).expect("rates");
    assert_eq!(r.d1_miss, Some(5.0));
    assert_eq!(r.ll_miss, Some(1.5));
    assert_eq!(r.mispredict, Some(10.0));
}

#[test]
fn cachegrind_none_when_no_rates() {
    assert!(parse_cachegrind("no cachegrind output").is_none());
}

#[test]
fn render_cachegrind_tags_simulated_and_classifies() {
    let r = parse_cachegrind(CACHEGRIND).expect("rates");
    let out = render_cachegrind_topdown(&r);
    // High mispredict (10% > 5%) → bad-speculation.
    assert!(out.contains("suggests: bad-speculation"));
    assert!(out.contains("[simulated:"));
    assert!(out.contains("branch-mispredict 10.0%"));
    assert!(out.contains("blind spots:"));
}

#[test]
fn render_cachegrind_memory_bound_when_ll_high() {
    let s = "\
==1== D1  miss rate:      3.0%
==1== LLd miss rate:      4.0%
==1== Mispred rate:       0.5%
";
    let r = parse_cachegrind(s).expect("rates");
    let out = render_cachegrind_topdown(&r);
    assert!(out.contains("suggests: memory-bound"));
}

// ---------------- misc pure helpers ----------------

#[test]
fn shell_quote_escapes_single_quotes() {
    assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    assert_eq!(shell_quote("./bench --fast"), "'./bench --fast'");
}

#[test]
fn clamp_repeat_applies_default_floor_and_cap() {
    assert_eq!(clamp_repeat(None), DEFAULT_REPEAT);
    assert_eq!(clamp_repeat(Some(0)), 1);
    assert_eq!(clamp_repeat(Some(12)), 12);
    assert_eq!(clamp_repeat(Some(u32::MAX)), MAX_REPEAT);
}

#[test]
fn unknown_mode_error_lists_four_modes() {
    let msg = unknown_mode_error("bogus");
    assert!(msg.contains("\"time\""));
    assert!(msg.contains("\"profile\""));
    assert!(msg.contains("\"topdown\""));
    assert!(msg.contains("\"callgrind\""));
}

#[test]
fn trunc_appends_ellipsis_when_cut() {
    assert_eq!(trunc("short", 10), "short");
    assert_eq!(trunc("abcdefghij", 5), "abcd…");
}

#[test]
fn unique_tmp_differs_each_call() {
    let a = unique_tmp("x");
    let b = unique_tmp("x");
    assert_ne!(a, b);
}

#[test]
fn timeout_messages_are_actionable() {
    let budget = std::time::Duration::from_secs(300);
    assert!(time_budget_message('A', budget).contains("raise timeout_sec"));
    let round = std::time::Duration::from_secs(4);
    assert!(no_round_fits_message(budget, round).contains("raise timeout_sec"));
    assert!(profile_timeout_message(300).contains("20-100x slower"));
}

#[test]
fn note_if_failed_prepends_only_on_failure() {
    assert_eq!(note_if_failed(true, "body".to_string()), "body");
    let noted = note_if_failed(false, "body".to_string());
    assert!(noted.starts_with("note: the profiled command exited non-zero"));
    assert!(noted.ends_with("\nbody"));
}

#[test]
fn profile_unavailable_points_at_alternatives() {
    let perf = Probe::NotInstalled {
        tool: "perf".to_string(),
        install_hint: "install perf".to_string(),
    };
    let vg = Probe::NotInstalled {
        tool: "valgrind".to_string(),
        install_hint: "install valgrind".to_string(),
    };
    let msg = profile_unavailable_message(&perf, &vg);
    assert!(msg.contains("cpp_bench local mode"));
    assert!(msg.contains("perf not installed"));
    assert!(msg.contains("Instruments"));
}

#[test]
fn topdown_unavailable_mentions_vendor_tools() {
    let perf = Probe::NotInstalled {
        tool: "perf".to_string(),
        install_hint: "install perf".to_string(),
    };
    let vg = Probe::NotInstalled {
        tool: "valgrind".to_string(),
        install_hint: "install valgrind".to_string(),
    };
    let msg = topdown_unavailable_message(&perf, &vg);
    assert!(msg.contains("VTune"));
    assert!(msg.contains("cpp_perf mode=time"));
}

// ---------------- the call boundary: argument checks ----------------

/// Drive the tool the way the engine does: JSON arguments in, the
/// model-facing text (Ok) or error (Err) out.
async fn call(tool: &PerfTool, arguments: serde_json::Value) -> Result<String, String> {
    let payload = codex_extension_api::ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let call = ToolCall {
        turn_id: "turn".to_string(),
        call_id: "call".to_string(),
        tool_name: ToolName::plain(TOOL_NAME),
        model: "test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: codex_utils_output_truncation::TruncationPolicy::Bytes(1 << 20),
        source: codex_extension_api::ToolCallSource::Direct,
        conversation_history: codex_extension_api::ConversationHistory::default(),
        turn_item_emitter: std::sync::Arc::new(codex_extension_api::NoopTurnItemEmitter),
        environments: Vec::new(),
        payload: payload.clone(),
    };
    match tool.handle(call).await {
        Ok(output) => Ok(serde_json::to_string(&output.to_response_item("call", &payload))
            .expect("serialize")),
        Err(err) => Err(err.to_string()),
    }
}

#[tokio::test]
async fn unknown_argument_names_are_refused() {
    // `repetitions` for `repeat` and `command` for `command_b` were dropped
    // silently: A alone, at the default 5 repeats, reported as a success.
    let dir = tempfile::tempdir().expect("tempdir");
    let err = call(
        &time_tool(dir.path()),
        serde_json::json!({"command_a": "true", "command": "true", "repetitions": 3}),
    )
    .await
    .expect_err("an unknown field must be refused");
    assert!(err.contains("unknown field `command`"), "{err}");
    assert!(err.contains("`command_b`"), "the error should list the real names: {err}");
}

#[tokio::test]
async fn single_command_modes_refuse_command_b_instead_of_dropping_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    for mode in ["profile", "callgrind", "topdown"] {
        let err = call(
            &time_tool(dir.path()),
            serde_json::json!({"command_a": "true", "command_b": "false", "mode": mode}),
        )
        .await
        .expect_err(mode);
        assert!(err.contains("command_b is only used by mode=\"time\""), "{mode}: {err}");
        assert!(err.contains(&format!("mode={mode:?}")), "{mode}: {err}");
    }
}

// ---------------- time mode: real runs (need bash; skip without) ----------------

fn time_tool(dir: &std::path::Path) -> PerfTool {
    PerfTool {
        cwd: dir.to_path_buf(),
        native_exec: true,
    }
}

fn time_args(a: &str, b: Option<&str>, repeat: u32, timeout_sec: u64) -> PerfArgs {
    PerfArgs {
        command_a: a.to_string(),
        command_b: b.map(str::to_string),
        mode: Some("time".to_string()),
        repeat: Some(repeat),
        timeout_sec: Some(timeout_sec),
    }
}

/// Run time mode the way `handle_call` does: clamped repeat, resolved timeout.
async fn time_mode(dir: &std::path::Path, args: &PerfArgs) -> String {
    time_tool(dir)
        .run_time_mode(args, clamp_repeat(args.repeat), timeout_from(args.timeout_sec))
        .await
        .map(|(text, _)| text)
        .expect("time mode")
}

fn have_bash() -> bool {
    crate::program_args::fixture::have("bash")
}

#[tokio::test]
async fn time_mode_interleaves_so_drift_is_not_a_difference() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // Every run of this command takes 3 ms longer than the one before it, like
    // a machine that keeps heating up. A and B are the SAME command, so the
    // true answer is "no difference". Timing all of A's runs before all of B's
    // turned the drift into "A is 1.6x faster (significant)".
    let dir = tempfile::tempdir().expect("tempdir");
    let cmd = "n=$(cat n 2>/dev/null || echo 0); echo $((n+1)) > n; \
               sleep 0.$(printf %03d $((20+3*n)))";
    let out = time_mode(dir.path(), &time_args(cmd, Some(cmd), 5, 60)).await;
    assert!(!out.contains("faster than"), "{out}");
    assert!(out.contains("INSIDE the measurement noise"), "{out}");
}

#[tokio::test]
async fn time_mode_still_finds_a_real_2x_difference() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // Guards against an over-strict gate. The sides differ by 100 ms per run,
    // far above scheduler jitter even when the rest of the suite saturates the
    // CPUs, and a miss is retried: one stalled run among five is rightly
    // "inside the noise". (A 20 ms vs 40 ms version failed once in ~20 full
    // suite runs and never in 40 runs under load; jitter is the likely cause.)
    // An over-strict gate fails every attempt; a wrong winner fails at once.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut last = String::new();
    for attempt in 1..=3 {
        let out = time_mode(dir.path(), &time_args("sleep 0.1", Some("sleep 0.2"), 5, 60)).await;
        assert!(!out.contains("B is "), "{out}");
        if out.contains("A is ") && out.contains("x faster than B") && out.contains("; significant).")
        {
            return;
        }
        eprintln!("attempt {attempt} found no difference:\n{out}");
        last = out;
    }
    panic!("a real 2x difference was never found:\n{last}");
}

#[tokio::test]
async fn time_mode_budget_bounds_the_whole_call() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // repeat 150 of a 0.1 s command under timeout_sec 1 used to run for 16 s:
    // the cap applied to each run separately and nothing bounded their sum.
    let dir = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    let out = time_mode(dir.path(), &time_args("sleep 0.1", None, 150, 1)).await;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}:\n{out}");
    // Either budget note is right: a planned stop before a round that would
    // not fit, or (when a run overran its prediction) a run cut at the budget.
    assert!(
        out.contains("note: stopped after ") || out.contains("timeout_sec budget ran out"),
        "{out}"
    );
    assert!(out.contains("A: mean"), "{out}");
}

#[tokio::test]
async fn time_mode_caps_repeat_instead_of_allocating_it() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // repeat 4294967295 once aborted the whole engine on a 34 GB
    // Vec::with_capacity (or, where that allocation succeeded, never ended).
    let dir = tempfile::tempdir().expect("tempdir");
    let args = time_args("true", None, u32::MAX, 60);
    let out = tokio::time::timeout(Duration::from_secs(60), time_mode(dir.path(), &args))
        .await
        .expect("time mode finished");
    assert!(
        out.contains("note: repeat=4294967295 is above the maximum of 100; ran 100."),
        "{out}"
    );
    assert!(out.contains("over 100 run(s)"), "{out}");
}

#[tokio::test]
async fn time_mode_counts_failures_over_timed_runs_only() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // A broken command used to read "exited non-zero on 6/5 run(s)": its
    // warmup was added to the failures but not to the runs.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = time_mode(dir.path(), &time_args("false", Some("true"), 5, 60)).await;
    assert!(
        out.contains("INVALID COMPARISON: A failed its untimed warmup run (exit status 1)"),
        "{out}"
    );
    assert!(!out.contains("6/5"), "{out}");
    assert!(!out.contains("faster than"), "{out}");

    // A command that passes its warmup and fails ONE timed run: 1 of 5.
    let flaky = "n=$(cat n 2>/dev/null || echo 0); echo $((n+1)) > n; [ $n -ne 2 ]";
    let out = time_mode(dir.path(), &time_args(flaky, Some("true"), 5, 60)).await;
    assert!(out.contains("INVALID COMPARISON: A exited non-zero on 1/5 run(s)"), "{out}");
}

#[tokio::test]
async fn time_mode_runs_commands_without_a_login_shell() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // `bash -lc` sourced the profile files on every timed run (~4.5 ms per
    // sample here, vs ~2 ms), inflating every mean and pulling every A/B ratio
    // toward 1: 2 ms vs 4 ms of work read as 1.27x.
    let dir = tempfile::tempdir().expect("tempdir");
    let cmd = "if shopt -q login_shell; then echo login > shell; else echo plain > shell; fi";
    time_mode(dir.path(), &time_args(cmd, None, 1, 60)).await;
    let shell = std::fs::read_to_string(dir.path().join("shell")).expect("command ran");
    assert_eq!(shell.trim(), "plain");
}

#[tokio::test]
async fn time_mode_reports_shell_startup_when_it_dominates() {
    if !have_bash() {
        eprintln!("skip: bash not installed");
        return;
    }
    // `true` is nothing BUT shell startup: the report must say so rather than
    // present the shell's cost as the command's.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = time_mode(dir.path(), &time_args("true", Some("true"), 5, 60)).await;
    assert!(out.contains("of every run is shell and process startup"), "{out}");
}

#[tokio::test]
async fn cachegrind_topdown_leaves_nothing_in_the_working_directory() {
    if !crate::program_args::fixture::have("valgrind") {
        eprintln!("skip: valgrind not installed");
        return;
    }
    // Every topdown call used to drop a cachegrind.out.<pid> into the cwd.
    let dir = tempfile::tempdir().expect("tempdir");
    time_tool(dir.path())
        .cachegrind_topdown("true", Duration::from_secs(120))
        .await
        .expect("cachegrind");
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

// ---------------- live tests (#[ignore]; run with --include-ignored) ----------------
//
// These exercise the real invocation path (the layer fixtures cannot see — it is
// where the trace-children regression lived) against real valgrind. They skip
// gracefully where the toolchain is missing; run explicitly:
//   cargo test -p codex-cpp-tools -- --include-ignored perf::live

/// Compile a tiny hot-loop C program into `dir`, returning its path, or `None`
/// if no C compiler is available.
async fn build_hot(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let src = dir.join("hot.c");
    std::fs::write(
        &src,
        "#include <stdio.h>\n\
         __attribute__((noinline)) long hot_loop(long n){ volatile long s=0; \
         for(long i=0;i<n;i++) s+=i%7; return s; }\n\
         int main(){ printf(\"%ld\\n\", hot_loop(20000000)); return 0; }\n",
    )
    .ok()?;
    let out = dir.join("hot");
    for cc in ["cc", "gcc", "clang"] {
        let ok = tokio::process::Command::new(cc)
            .args(["-O1", "-g", "-o"])
            .arg(&out)
            .arg(&src)
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(out);
        }
    }
    None
}

#[tokio::test]
#[ignore = "needs a C compiler + valgrind; run with --include-ignored"]
async fn live_callgrind_finds_hot_function() {
    if !crate::probe::probe("valgrind", &["--version"], &std::env::temp_dir())
        .await
        .is_available()
    {
        eprintln!("skip: valgrind unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(_bin) = build_hot(dir.path()).await else {
        eprintln!("skip: no C compiler");
        return;
    };
    let tool = PerfTool {
            native_exec: true,
        cwd: dir.path().to_path_buf(),
    };
    let out = tool
        .callgrind_report("./hot", std::time::Duration::from_secs(120))
        .await
        .expect("callgrind");
    match out {
        ProfileOutcome::Report(text) => {
            // THE regression: without --trace-children the child is never profiled
            // and hot_loop never appears (the out-file is 0 bytes).
            assert!(text.contains("hot_loop"), "hot_loop missing from:\n{text}");
            assert!(text.contains("[simulated"));
        }
        other => panic!("expected a callgrind Report, got a different outcome: {}", match other {
            ProfileOutcome::TimedOut(s) => format!("TimedOut({s})"),
            ProfileOutcome::Empty => "Empty".to_string(),
            ProfileOutcome::Report(_) => unreachable!(),
        }),
    }
}

#[tokio::test]
#[ignore = "runs real commands; run with --include-ignored"]
async fn live_time_mode_detects_real_difference() {
    let tool = PerfTool {
            native_exec: true,
        cwd: std::env::temp_dir(),
    };
    let args = PerfArgs {
        command_a: "true".to_string(),
        command_b: Some("sleep 0.1".to_string()),
        mode: Some("time".to_string()),
        repeat: Some(5),
        timeout_sec: Some(30),
    };
    let out = tool
        .run_time_mode(&args, 5, std::time::Duration::from_secs(30))
        .await
        .map(|(text, _)| text)
        .expect("time mode");
    // A (true) is clearly faster than B (sleep 0.1); the gate must confirm it.
    assert!(out.contains("A is") && out.contains("faster than B"), "{out}");
    assert!(out.contains("warmup=1 untimed"));
}

#[tokio::test]
#[ignore = "runs real commands; run with --include-ignored"]
async fn live_time_mode_failure_invalidates() {
    let tool = PerfTool {
            native_exec: true,
        cwd: std::env::temp_dir(),
    };
    let args = PerfArgs {
        command_a: "true".to_string(),
        command_b: Some("false".to_string()), // exits non-zero every run
        mode: Some("time".to_string()),
        repeat: Some(3),
        timeout_sec: Some(30),
    };
    let out = tool
        .run_time_mode(&args, 3, std::time::Duration::from_secs(30))
        .await
        .map(|(text, _)| text)
        .expect("time mode");
    assert!(out.contains("INVALID COMPARISON: B"), "{out}");
    assert!(!out.contains("faster than"));
}

#[tokio::test]
#[ignore = "needs valgrind; run with --include-ignored"]
async fn live_callgrind_notes_failed_target() {
    if !crate::probe::probe("valgrind", &["--version"], &std::env::temp_dir())
        .await
        .is_available()
    {
        eprintln!("skip: valgrind unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(_bin) = build_hot(dir.path()).await else {
        eprintln!("skip: no C compiler");
        return;
    };
    let tool = PerfTool {
            native_exec: true,
        cwd: dir.path().to_path_buf(),
    };
    // A command that does real work (so callgrind produces a profile) then fails.
    let out = tool
        .callgrind_report("./hot; exit 1", std::time::Duration::from_secs(120))
        .await
        .expect("callgrind");
    match out {
        ProfileOutcome::Report(text) => {
            assert!(text.contains("hot_loop"), "{text}");
            assert!(text.contains("exited non-zero"), "missing failed-target note:\n{text}");
        }
        _ => panic!("expected a Report"),
    }
}

#[tokio::test]
#[ignore = "needs a C compiler + valgrind; run with --include-ignored"]
async fn live_cachegrind_topdown_classifies() {
    if !crate::probe::probe("valgrind", &["--version"], &std::env::temp_dir())
        .await
        .is_available()
    {
        eprintln!("skip: valgrind unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    if build_hot(dir.path()).await.is_none() {
        eprintln!("skip: no C compiler");
        return;
    }
    let tool = PerfTool {
            native_exec: true,
        cwd: dir.path().to_path_buf(),
    };
    let out = tool
        .cachegrind_topdown("./hot", std::time::Duration::from_secs(120))
        .await
        .expect("cachegrind");
    match out {
        TopdownOutcome::Report(text) => {
            assert!(text.contains("[simulated"));
            assert!(text.contains("suggests:"));
        }
        _ => panic!("expected a cachegrind Report"),
    }
}

// ---- regressions found by probing the tool --------------------------------------

#[test]
fn profiling_runs_never_use_a_login_shell() {
    // `bash -lc` re-sources the profile files and RESETS PATH, so the
    // command profiled would not be the one the engine's PATH (and the probes) saw.
    let argv = shell_argv("perf stat -- ./app".to_string());
    assert_eq!(argv, vec!["-c".to_string(), "perf stat -- ./app".to_string()]);
    assert!(!argv.iter().any(|a| a.starts_with('-') && a.contains('l')));
}

#[test]
fn a_perf_that_measured_nothing_has_no_verdict_to_blame_on_the_command() {
    // Ubuntu's wrapper exits 2 without measuring; every event unsupported
    // is the no-PMU case. Neither may produce a counter verdict (the rung now
    // falls through to cachegrind instead).
    let wrapper = "WARNING: perf not found for kernel 6.1.0-53\n\n  You may need to install \
                   the following packages for this specific kernel:\n    linux-tools-6.1.0-53\n";
    assert_eq!(classify_counters(&parse_perf_counters(wrapper)), None);
    let no_pmu = "   <not supported>      cycles\n   <not supported>      instructions\n\
                  1234      branches\n   <not supported>      branch-misses\n";
    assert_eq!(classify_counters(&parse_perf_counters(no_pmu)), None);
}

#[test]
fn cachegrind_without_a_memory_or_branch_problem_is_inconclusive() {
    // A pure ALU loop (0% misses) must not be called "core/backend-bound".
    let quiet = CachegrindRates {
        d1_miss: Some(0.0),
        ll_miss: Some(0.0),
        mispredict: Some(0.0),
    };
    assert_eq!(cachegrind_bottleneck(&quiet), None);
    let text = render_cachegrind_topdown(&quiet);
    assert!(text.contains("INCONCLUSIVE"), "{text}");
    assert!(!text.contains("suggests: core/backend-bound"), "{text}");
    let branchy = CachegrindRates {
        mispredict: Some(12.0),
        ..quiet
    };
    assert_eq!(cachegrind_bottleneck(&branchy), Some(Bottleneck::BadSpeculation));
    let missy = CachegrindRates {
        ll_miss: Some(4.0),
        mispredict: Some(0.0),
        d1_miss: Some(9.0),
    };
    assert_eq!(cachegrind_bottleneck(&missy), Some(Bottleneck::MemoryBound));
}
