use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;

fn args(json: serde_json::Value) -> ReverseArgs {
    serde_json::from_value(json).expect("args parse")
}

fn request(json: serde_json::Value) -> Result<Request, String> {
    check_request(&args(json))
}

// --------------------------------- validation ---------------------------------

#[test]
fn each_query_names_what_it_is_missing() {
    let err = request(serde_json::json!({"query": "who_wrote"})).unwrap_err();
    assert!(err.contains("needs `expr`"), "{err}");
    let err = request(serde_json::json!({"query": "last_reached"})).unwrap_err();
    assert!(err.contains("needs `location`"), "{err}");
    assert!(request(serde_json::json!({"query": "rewind"})).unwrap_err().contains("unknown query"));
    assert!(request(serde_json::json!({"query": "stop_point"})).is_ok());
    assert!(request(serde_json::json!({"query": "last_lines"})).is_ok());
}

#[test]
fn a_misnamed_or_misplaced_field_is_refused_not_ignored() {
    assert!(serde_json::from_value::<ReverseArgs>(serde_json::json!({"query": "stop_point", "trace_dir": "x"})).is_err());
    let err = request(serde_json::json!({"query": "last_lines", "expr": "x"})).unwrap_err();
    assert!(err.contains("only used by query \"who_wrote\""), "{err}");
    let err = request(serde_json::json!({"query": "stop_point", "stop_at": "a.c:3", "stop_event": 9})).unwrap_err();
    assert!(err.contains("not both"), "{err}");
    let err = request(serde_json::json!({"query": "stop_point", "stop_hit": 2})).unwrap_err();
    assert!(err.contains("give `stop_at` too"), "{err}");
}

#[test]
fn counts_default_per_query_and_are_capped() {
    assert_eq!(request(serde_json::json!({"query": "who_wrote", "expr": "x"})).unwrap().count, 3);
    assert_eq!(request(serde_json::json!({"query": "last_lines"})).unwrap().count, 20);
    assert_eq!(request(serde_json::json!({"query": "last_lines", "count": 9999})).unwrap().count, MAX_COUNT);
    assert_eq!(request(serde_json::json!({"query": "last_lines", "count": 0})).unwrap().count, 1);
    assert_eq!(request(serde_json::json!({"query": "last_lines", "count": 4.0})).unwrap().count, 4);
}

#[test]
fn a_watch_expression_may_read_but_never_change_the_replay() {
    for ok in [
        "book.ask",
        "o->next",
        "*(long*)0x7ffd1234",
        "((struct order*)0x57b7bfa212c0)->next",
        "levels[3].qty",
        "sizeof(x) == 8",
        "a == b",
        "p != 0",
        "x <= 3 && y >= 4",
        "static_cast<int>(side)",
        "book->bids._M_impl",
    ] {
        assert!(check_expr(ok).is_ok(), "{ok} should be allowed: {:?}", check_expr(ok));
    }
    for bad in [
        "x = 5",
        "book.ask=99",
        "x += 1",
        "++seq",
        "seq--",
        "reset()",
        "book.top()",
        "(*fp)(1)",
        "handlers[2](x)",
        "a\nshell rm -rf /",
        "a\u{1b}[2J",
    ] {
        assert!(check_expr(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn a_location_is_a_plain_linespec_without_a_condition() {
    for ok in ["book.cpp:42", "OrderBook::apply", "*0x401136", "feed", "ns::Book<int>::top"] {
        assert!(check_location("location", ok).is_ok(), "{ok}");
    }
    for bad in ["foo if x=1", "book.cpp:9 thread 2", "-qualified foo", "a\nshell id"] {
        assert!(check_location("location", bad).is_err(), "{bad:?}");
    }
}

// ------------------------------- argv and plumbing -------------------------------

#[test]
fn the_stop_point_follows_the_request_then_how_the_program_ended() {
    let r = |j| request(j).unwrap();
    assert_eq!(start_for(&r(serde_json::json!({"query": "stop_point", "stop_event": 274})), Some(-6)), Start::Event(274));
    assert_eq!(start_for(&r(serde_json::json!({"query": "stop_point", "stop_at": "a.c:3"})), Some(0)), Start::Beginning);
    // killed by a signal: run forward to its delivery
    assert_eq!(start_for(&r(serde_json::json!({"query": "stop_point"})), Some(-11)), Start::Beginning);
    // a normal exit, or a recording cut short: start at its end
    assert_eq!(start_for(&r(serde_json::json!({"query": "stop_point"})), Some(3)), Start::Exit);
    assert_eq!(start_for(&r(serde_json::json!({"query": "stop_point"})), None), Start::Exit);
}

#[test]
fn replay_argv_puts_rr_options_before_the_trace_and_gdb_options_after_dashdash() {
    let trace = Path::new("/rr/book-0");
    let hook = Path::new("/tmp/s/hook.gdb");
    let tail: Vec<String> = [
        "/rr/book-0",
        "--",
        "-q",
        "-nx",
        "-batch",
        "-iex",
        "set auto-load off",
        "-iex",
        "set may-call-functions off",
        "-x",
        "/tmp/s/hook.gdb",
    ]
    .map(str::to_string)
    .to_vec();
    let with = |head: &[&str]| -> Vec<String> {
        head.iter().map(|s| s.to_string()).chain(tail.iter().cloned()).collect()
    };
    assert_eq!(build_replay_argv(trace, Start::Beginning, false, hook), with(&["replay", "-q"]));
    assert_eq!(build_replay_argv(trace, Start::Exit, false, hook), with(&["replay", "-e", "-q"]));
    assert_eq!(build_replay_argv(trace, Start::Event(42), true, hook), with(&["replay", "-u", "-g", "42", "-q"]));
}

#[test]
fn the_driver_runs_from_a_hook_on_rrs_own_connect_command() {
    let hook = hook_script(Path::new("/tmp/s/byteask_rr_driver.py"));
    assert_eq!(hook, "define target hookpost-extended-remote\nsource /tmp/s/byteask_rr_driver.py\nend\n");
}

#[test]
fn the_fatal_signal_decides_the_stop_only_when_nothing_else_does() {
    let roots = vec![PathBuf::from("/work")];
    let plain = request(serde_json::json!({"query": "who_wrote", "expr": "x"})).unwrap();
    let v: serde_json::Value = serde_json::from_str(&query_json(&plain, Some("SIGABRT"), &roots)).unwrap();
    assert_eq!(v["fatal_signal"], "SIGABRT");
    assert_eq!(v["user_roots"][0], "/work");
    assert_eq!(v["expr"], "x");
    let at = request(serde_json::json!({"query": "who_wrote", "expr": "x", "stop_at": "a.c:3", "stop_hit": 2})).unwrap();
    let v: serde_json::Value = serde_json::from_str(&query_json(&at, Some("SIGABRT"), &roots)).unwrap();
    assert!(v["fatal_signal"].is_null());
    assert_eq!(v["stop_hit"], 2);
}

#[test]
fn a_cpu_bind_failure_is_recognised_for_the_unbound_retry() {
    // Measured on a machine with a PMU after CPU isolation moved user processes off core 7.
    assert!(cpu_bind_failed(
        "[FATAL src/Session.cc:794:do_bind_cpu() errno: EINVAL] Can't bind to requested CPU 7, and CPUID faulting not available"
    ));
    assert!(!cpu_bind_failed("rr: Trace file `/x/incomplete' found."));
}

#[test]
fn the_driver_never_takes_a_debugger_command_from_the_model() {
    // Every gdb.execute goes through run()/move(), and every string they get
    // is a literal or one of the checked fields. A new gdb.execute elsewhere
    // would bypass that; this pins it.
    assert_eq!(DRIVER.matches("gdb.execute(").count(), 2, "gdb.execute outside run()/move()");
    for line in DRIVER.lines().filter(|l| l.contains("run(\"") || l.contains("move(\"")) {
        assert!(!line.contains("QUERY"), "a query field reaches a command directly: {line}");
    }
}

/// Real rr parses its options before it opens the trace, so a machine with no
/// PMU can still prove rr accepts every replay command line we
/// build: an unknown option says "Invalid option", a non-trace directory says
/// something else. A control with a bogus option proves the check can fail.
#[tokio::test]
async fn real_rr_accepts_every_replay_argv_we_build() {
    let dir = tempfile::tempdir().unwrap();
    let rejected = |out: &str| {
        let lower = out.to_ascii_lowercase();
        lower.contains("invalid option") || lower.contains("unknown option")
    };
    let run = |argv: Vec<String>| {
        let cwd = dir.path().to_path_buf();
        async move {
            match crate::run::run_capped("rr", &argv, &cwd, crate::run::timeout_from(Some(30))).await {
                Ok(crate::run::RunOutcome::Finished { stdout, stderr, .. }) => Some(format!("{stdout}\n{stderr}")),
                _ => None,
            }
        }
    };
    let Some(control) = run(vec!["replay".into(), "--byteask-not-a-flag".into(), dir.path().to_string_lossy().into_owned()]).await else {
        eprintln!("skip: rr not installed");
        return;
    };
    assert!(rejected(&control), "the control was not rejected, so this test proves nothing:\n{control}");
    let not_a_trace = dir.path().join("empty");
    std::fs::create_dir(&not_a_trace).unwrap();
    let hook = dir.path().join("hook.gdb");
    for (label, start, unbound) in [
        ("from the beginning", Start::Beginning, false),
        ("at the exit", Start::Exit, false),
        ("at an event, unbound", Start::Event(274), true),
    ] {
        let argv = build_replay_argv(&not_a_trace, start, unbound, &hook);
        let out = run(argv.clone()).await.unwrap_or_default();
        assert!(!rejected(&out), "real rr REJECTED the replay argv {label} ({argv:?}):\n{out}");
    }
}

// --------------------------------- rendering ---------------------------------
//
// Fixtures are the driver's real output on a machine with a PMU (rr 5.10, gdb 15.1),
// trimmed to the fields the renderer reads.

const BOOK_STOP: &str = r#"{"how": "signal", "event": 293, "frames": [{"level": 0, "pc": "0x761b7a49ec0c", "function": "__pthread_kill_implementation", "file": "./nptl/./nptl/pthread_kill.c", "line": 44, "library": "/lib/x86_64-linux-gnu/libc.so.6", "user": false}, {"level": 4, "pc": "0x761b7a4288ff", "function": "__GI_abort", "file": "./stdlib/./stdlib/abort.c", "line": 79, "library": "/lib/x86_64-linux-gnu/libc.so.6", "user": false}, {"level": 5, "pc": "0x59db08164478", "function": "main", "file": "/lab/exp/book.cpp", "line": 19, "user": true}], "signal": "SIGABRT", "thread": {"num": 1, "tid": 4219, "name": ""}, "kind": "stop"}"#;

fn records(lines: &[&str]) -> Vec<Record> {
    lines.iter().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"))).collect()
}

fn who_wrote_book() -> Vec<Record> {
    records(&[
        BOOK_STOP,
        r#"{"expr": "book.ask", "address": "0x59db08167018", "size": 8, "type": "long", "frame_level": 5, "value": "99", "kind": "target"}"#,
        r#"{"event": 274, "thread": {"num": 2, "tid": 4221, "name": ""}, "frames": [{"level": 0, "pc": "0x59db081643c9", "function": "risk_reset", "file": "/lab/exp/book.cpp", "line": 14, "user": true}, {"level": 1, "function": "std::__invoke_impl<void, void (*)()>", "file": "/usr/include/c++/13/bits/invoke.h", "line": 61, "user": false}, {"level": 7, "function": "start_thread", "file": "./nptl/./nptl/pthread_create.c", "line": 447, "library": "/lib/x86_64-linux-gnu/libc.so.6", "user": false}], "before": "102", "after": "99", "kind": "write"}"#,
        r#"{"event": 242, "thread": {"num": 3, "tid": 4220, "name": ""}, "frames": [{"level": 0, "pc": "0x59db081642ba", "function": "feed", "file": "/lab/exp/book.cpp", "line": 9, "user": true}], "before": "101", "after": "102", "kind": "write"}"#,
        r#"{"event": 242, "thread": {"num": 3, "tid": 4220, "name": ""}, "frames": [{"level": 0, "pc": "0x59db081642ba", "function": "feed", "file": "/lab/exp/book.cpp", "line": 9, "user": true}], "before": "103", "after": "101", "kind": "write"}"#,
        r#"{"kind": "done"}"#,
    ])
}

fn render_ok(req: serde_json::Value, recs: &[Record]) -> String {
    render(&request(req).unwrap(), "recording: /rr/book-0\n", recs, None, "", Path::new("/lab/exp")).expect("rendered")
}

#[test]
fn who_wrote_names_the_other_threads_write_with_the_value_it_changed() {
    let text = render_ok(serde_json::json!({"query": "who_wrote", "expr": "book.ask"}), &who_wrote_book());
    assert!(text.contains("asked from: SIGABRT at __pthread_kill_implementation (libc.so.6), called from main() at book.cpp:19 (event 293) in thread 4219 (main)"), "{text}");
    assert!(text.contains("watching `book.ask` — long, 8 bytes at 0x59db08167018; its value at the stop: 99"), "{text}");
    assert!(text.contains(" 1. event 274 · thread 4221 (risk_reset) — 102 -> 99\n    risk_reset() at book.cpp:14"), "{text}");
    assert!(text.contains("written by a DIFFERENT thread (4221) than the one that stopped (4219)"), "{text}");
    assert!(text.contains(" 2. event 242 · thread 4220 (feed) — 101 -> 102"), "{text}");
    // three found, three asked for: there may be more, and it says so
    assert!(text.contains("earlier writes exist: raise `count`"), "{text}");
    assert!(!text.contains("INCOMPLETE"), "{text}");
}

#[test]
fn a_write_inside_a_library_is_shown_with_the_users_caller_and_the_start_is_named() {
    let recs = records(&[
        r#"{"how": "end", "event": 144, "frames": [{"level": 0, "pc": "0x70000002", "function": "syscall_traced", "user": false}], "thread": {"num": 1, "tid": 4205, "name": ""}, "kind": "stop"}"#,
        r#"{"expr": "g.max_qty", "address": "0x621b38ce7018", "size": 4, "type": "int", "frame_level": null, "value": "4934217", "kind": "target"}"#,
        r#"{"event": 122, "thread": {"num": 1, "tid": 4205, "name": ""}, "frames": [{"level": 0, "function": "__strcpy_evex", "file": "./string/../sysdeps/x86_64/multiarch/strcpy-evex.S", "line": 224, "library": "/lib/x86_64-linux-gnu/libc.so.6", "user": false}, {"level": 1, "function": "set_name", "file": "/lab/exp/overflow.c", "line": 5, "user": true}, {"level": 2, "function": "main", "file": "/lab/exp/overflow.c", "line": 7, "user": true}], "before": "500", "after": "4934217", "kind": "write"}"#,
        r#"{"value": "500", "kind": "start"}"#,
        r#"{"kind": "done"}"#,
    ]);
    let text = render_ok(serde_json::json!({"query": "who_wrote", "expr": "g.max_qty"}), &recs);
    assert!(text.contains("asked from: the end of the recording (after the program exited, event 144)"), "{text}");
    assert!(text.contains("500 -> 4934217\n    __strcpy_evex (libc.so.6), called from set_name() at overflow.c:5"), "{text}");
    assert!(text.contains("no earlier write in the recording: 500 was already there"), "{text}");
    assert!(!text.contains("raise `count`"), "the history is complete: {text}");
}

#[test]
fn memory_that_did_not_exist_yet_is_called_an_allocation_not_a_value() {
    let recs = records(&[
        r#"{"how": "signal", "event": 129, "frames": [{"level": 0, "function": "total", "file": "/lab/exp/segv.c", "line": 3, "user": true}], "signal": "SIGSEGV", "thread": {"num": 1, "tid": 4241, "name": ""}, "kind": "stop"}"#,
        r#"{"event": 128, "thread": {"num": 1, "tid": 4241, "name": ""}, "frames": [{"level": 0, "function": "main", "file": "/lab/exp/segv.c", "line": 7, "user": true}], "before": "0x0", "after": "0x10", "kind": "write"}"#,
        r#"{"event": 126, "thread": {"num": 1, "tid": 4241, "name": ""}, "frames": [{"level": 0, "function": "__brk_call", "library": "/lib/x86_64-linux-gnu/libc.so.6", "user": false}, {"level": 3, "function": "main", "file": "/lab/exp/segv.c", "line": 5, "user": true}], "before": "<unreadable: Cannot access memory at address 0x57b7bfa212d0>", "after": "0x0", "kind": "write"}"#,
        r#"{"kind": "done"}"#,
    ]);
    let text = render_ok(serde_json::json!({"query": "who_wrote", "expr": "b->next", "count": 2}), &recs);
    assert!(text.contains("0x0 -> 0x10\n    main() at segv.c:7"), "{text}");
    assert!(text.contains("did not exist before here (it was just allocated or mapped)"), "{text}");
}

#[test]
fn last_lines_are_shown_oldest_first_and_end_at_the_stop() {
    let recs = records(&[
        BOOK_STOP,
        r#"{"function": "main", "file": "/lab/exp/book.cpp", "line": 19, "user": true, "event": 282, "kind": "line"}"#,
        r#"{"function": "main", "file": "/lab/exp/book.cpp", "line": 18, "user": true, "event": 224, "kind": "line"}"#,
        r#"{"function": "main", "file": "/lab/exp/book.cpp", "line": 17, "user": true, "event": 188, "kind": "line"}"#,
        r#"{"function": "__libc_start_call_main", "file": "./csu/../sysdeps/nptl/libc_start_call_main.h", "line": 58, "user": false, "event": 188, "kind": "line"}"#,
        r#"{"kind": "done"}"#,
    ]);
    let text = render_ok(serde_json::json!({"query": "last_lines", "count": 4}), &recs);
    let list = &text[text.find("oldest first:").expect("list")..];
    let a = list.find("book.cpp:17").expect("17");
    let b = list.find("book.cpp:18").expect("18");
    let c = list.find("book.cpp:19").expect("19");
    assert!(a < b && b < c, "oldest first: {text}");
    assert!(list.lines().find(|l| l.contains("book.cpp:19")).unwrap().ends_with("<- the stop"), "{text}");
    assert!(text.contains("the last 3 line(s) thread 4219 ran"), "{text}");
    assert!(text.contains("system code"), "{text}");
}

#[test]
fn last_reached_shows_arguments_and_says_when_that_was_every_time() {
    let recs = records(&[
        r#"{"how": "signal", "event": 129, "frames": [{"level": 0, "function": "total", "file": "/lab/exp/segv.c", "line": 3, "user": true}], "signal": "SIGSEGV", "thread": {"num": 1, "tid": 4241, "name": ""}, "kind": "stop"}"#,
        r#"{"event": 128, "thread": {"num": 1, "tid": 4241, "name": ""}, "frames": [{"level": 0, "function": "total", "file": "/lab/exp/segv.c", "line": 3, "user": true}, {"level": 1, "function": "main", "file": "/lab/exp/segv.c", "line": 8, "user": true}], "args": [["o", "0x57b7bfa212a0"]], "kind": "hit"}"#,
        r#"{"kind": "start"}"#,
        r#"{"kind": "done"}"#,
    ]);
    let text = render_ok(serde_json::json!({"query": "last_reached", "location": "total"}), &recs);
    assert!(text.contains(" 1. event 128 · thread 4241 (main) — total() at segv.c:3\n    args: o=0x57b7bfa212a0\n    called from main() at segv.c:8"), "{text}");
    assert!(text.contains("that is every time before the stop (1)"), "{text}");
}

#[test]
fn a_bad_expression_goes_back_to_the_model_with_the_stop_it_was_evaluated_at() {
    let recs = records(&[BOOK_STOP, r#"{"where": "expr", "message": "No symbol \"bokk\" in current context.", "kind": "error"}"#]);
    let err = render(
        &request(serde_json::json!({"query": "who_wrote", "expr": "bokk.ask"})).unwrap(),
        "",
        &recs,
        None,
        "",
        Path::new("/lab/exp"),
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("Nothing answered"), "{text}");
    assert!(text.contains("No symbol"), "{text}");
    assert!(text.contains("SIGABRT"), "{text}");
}

#[test]
fn a_replay_that_never_started_is_not_run_with_the_reason() {
    let req = request(serde_json::json!({"query": "stop_point"})).unwrap();
    let cwd = Path::new("/w");
    let perf = render(&req, "", &[], None, "rr needs /proc/sys/kernel/perf_event_paranoid <= 1, but it is 3.", cwd).unwrap();
    assert!(perf.contains("RESULT: NOT RUN — rr cannot replay here"), "{perf}");
    let py = render(&req, "", &[], None, "Python scripting is not supported in this copy of GDB.", cwd).unwrap();
    assert!(py.contains("RESULT: NOT RUN") && py.contains("without Python") && py.contains("FIX:"), "{py}");
    let slow = render(&req, "", &[], Some(300), "", cwd).unwrap();
    assert!(slow.contains("RESULT: NOT RUN — the replay did not reach the stop point within 300s"), "{slow}");
    let other = render(&req, "", &[], None, "[FATAL src/TraceStream.cc:100] Trace version 77 incompatible", cwd).unwrap();
    assert!(other.contains("RESULT: NOT RUN") && other.contains("Trace version 77"), "{other}");
}

#[test]
fn a_timed_out_walk_shows_what_it_found_and_says_it_is_incomplete() {
    let mut recs = who_wrote_book();
    recs.retain(|r| !matches!(r, Record::Done));
    recs.truncate(3); // stop, target, one write
    let text = render(
        &request(serde_json::json!({"query": "who_wrote", "expr": "book.ask", "count": 5})).unwrap(),
        "",
        &recs,
        Some(120),
        "",
        Path::new("/lab/exp"),
    )
    .unwrap();
    assert!(text.contains("102 -> 99"), "{text}");
    assert!(text.contains("TIMED OUT after 120s: the answer above is INCOMPLETE (1 of the 5 requested"), "{text}");
}

#[test]
fn a_driver_that_died_mid_answer_is_never_read_as_complete() {
    let mut recs = who_wrote_book();
    recs.retain(|r| !matches!(r, Record::Done));
    let text = render(
        &request(serde_json::json!({"query": "who_wrote", "expr": "book.ask"})).unwrap(),
        "",
        &recs,
        None,
        "gdb: internal-error",
        Path::new("/lab/exp"),
    )
    .unwrap();
    assert!(text.contains("INCOMPLETE: the replay ended before the question was finished"), "{text}");
}

#[test]
fn stop_point_marks_the_users_frames_and_lists_every_thread() {
    let recs = records(&[
        BOOK_STOP,
        r#"{"threads": [{"num": 1, "tid": 4219, "name": "", "frames": [{"function": "main", "file": "/lab/exp/book.cpp", "line": 19, "user": true}]}, {"num": 2, "tid": 4221, "name": "", "frames": [{"function": "risk_reset", "file": "/lab/exp/book.cpp", "line": 13, "user": true}]}], "kind": "threads"}"#,
        r#"{"kind": "done"}"#,
    ]);
    let text = render_ok(serde_json::json!({"query": "stop_point"}), &recs);
    assert!(text.contains("  *#2 main() at book.cpp:19"), "{text}");
    assert!(text.contains("(1 system frame(s) not shown)"), "{text}");
    assert!(text.contains("threads at the stop (2):\n  * thread 4219 (main) — main() at book.cpp:19\n    thread 4221 (risk_reset) — risk_reset() at book.cpp:13"), "{text}");
}

// ------------------------- live tests (need rr and a PMU) -------------------------
//
//   BYTEASK_RR_LIVE_REQUIRED=1 <test-binary> --include-ignored reverse::tests::live
//
// Each records a real program with rr, then asks the REAL tool, through its
// entry point, questions about that recording. With BYTEASK_RR_LIVE_REQUIRED
// set, a host that cannot record fails instead of skipping.

use crate::tools::record::tests::rr_live_skip;

struct Recorded {
    dir: tempfile::TempDir,
    trace: PathBuf,
}

async fn record_cxx(name: &str, source: &str, flags: &[&str]) -> Option<Recorded> {
    let dir = tempfile::tempdir().ok()?;
    let src = dir.path().join(format!("{name}.cpp"));
    std::fs::write(&src, source).ok()?;
    let bin = dir.path().join(name);
    let built = tokio::process::Command::new("c++")
        .args(["-g", "-O0", "-pthread"])
        .args(flags)
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status()
        .await
        .ok()?
        .success();
    if !built {
        rr_live_skip("no C++ compiler");
        return None;
    }
    let trace = dir.path().join("trace");
    let out = tokio::process::Command::new("rr")
        .arg("record")
        .arg("-o")
        .arg(&trace)
        .arg(&bin)
        .current_dir(dir.path())
        .output()
        .await;
    let Ok(out) = out else {
        rr_live_skip("rr is not installed");
        return None;
    };
    if !crate::tools::runtime_util::is_rr_trace(&trace) {
        rr_live_skip(&format!("rr could not record here: {}", String::from_utf8_lossy(&out.stderr)));
        return None;
    }
    Some(Recorded { dir, trace })
}

async fn ask(rec: &Recorded, json: serde_json::Value) -> String {
    let mut json = json;
    json["trace"] = serde_json::Value::String(rec.trace.to_string_lossy().into_owned());
    let tool = ReverseTool {
        cwd: rec.dir.path().to_path_buf(),
        native_exec: true,
    };
    match crate::test_support::call(&tool, json).await {
        Ok(text) => text,
        Err(err) => format!("ERROR: {err:?}"),
    }
}

/// A two-thread order book: the feed keeps bid/ask consistent; a reset path on
/// ANOTHER thread writes a stale ask without the feed's discipline, and main
/// aborts on the crossed book. The question a forward debugger cannot answer:
/// who wrote that ask?
const BOOK: &str = r#"#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <thread>
struct Book { long bid; long ask; };
Book book{100, 101};
std::atomic<int> phase{0};
static void feed() {
  for (int i = 0; i < 20000; i++) { book.bid = 100 + (i % 3); book.ask = book.bid + 1; }
  phase.store(1);
}
static void risk_reset() {
  while (phase.load() != 1) {}
  book.ask = 99;
}
int main() {
  std::thread a(feed), b(risk_reset);
  a.join(); b.join();
  if (book.ask <= book.bid) { std::fprintf(stderr, "crossed book bid=%ld ask=%ld\n", book.bid, book.ask); std::abort(); }
  return 0;
}
"#;

#[tokio::test]
#[ignore]
async fn live_who_wrote_finds_the_write_made_by_another_thread() {
    let Some(rec) = record_cxx("book", BOOK, &[]).await else { return };
    let text = ask(&rec, serde_json::json!({"query": "who_wrote", "expr": "book.ask", "count": 2})).await;
    assert!(text.contains("`./book`") || text.contains("/book` ended by SIGABRT"), "{text}");
    assert!(text.contains("asked from: SIGABRT"), "{text}");
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(first.contains("(risk_reset)") && first.ends_with("-> 99"), "{text}");
    assert!(text.contains("risk_reset() at book.cpp:14"), "{text}");
    assert!(text.contains("written by a DIFFERENT thread"), "{text}");
    let second = text.lines().find(|l| l.starts_with(" 2. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(second.contains("(feed)"), "{text}");
}

#[tokio::test]
#[ignore]
async fn live_stop_point_lists_every_thread_and_last_lines_walk_back_from_abort() {
    let Some(rec) = record_cxx("book", BOOK, &[]).await else { return };
    let text = ask(&rec, serde_json::json!({"query": "stop_point"})).await;
    assert!(text.contains("*#") && text.contains("main() at book.cpp:19"), "{text}");
    assert!(text.contains("threads at the stop (1)"), "only main is left at the abort: {text}");
    let text = ask(&rec, serde_json::json!({"query": "last_lines", "count": 3})).await;
    let list = &text[text.find("oldest first:").unwrap_or_else(|| panic!("{text}"))..];
    let a = list.find("book.cpp:17").unwrap_or_else(|| panic!("{text}"));
    let c = list.find("book.cpp:19").unwrap_or_else(|| panic!("{text}"));
    assert!(a < c, "{text}");
}

/// A list walk that dereferences a corrupted `next`: chain two questions from
/// the crash back to the line that wrote the bad link.
const LIST: &str = r#"#include <cstdlib>
struct order { long id; long qty; order *next; };
__attribute__((noinline)) long total(order *o) {
  long t = 0;
  while (o) {
    t += o->qty;
    o = o->next;
  }
  return t;
}
int main() {
  order *a = (order *)std::malloc(sizeof *a), *b = (order *)std::malloc(sizeof *b);
  a->id = 1; a->qty = 10; a->next = b;
  b->id = 2; b->qty = 20; b->next = (order *)0x10;
  return (int)total(a);
}
"#;

#[tokio::test]
#[ignore]
async fn live_a_crash_is_traced_back_to_the_line_that_corrupted_the_pointer() {
    let Some(rec) = record_cxx("list", LIST, &[]).await else { return };
    let text = ask(&rec, serde_json::json!({"query": "who_wrote", "expr": "o", "count": 1})).await;
    assert!(text.contains("asked from: SIGSEGV"), "{text}");
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(first.ends_with("-> 0x10"), "{text}");
    assert!(text.contains("total() at list.cpp:7"), "o = o->next wrote the bad pointer: {text}");
    // The value `o` had before: the node whose `next` was loaded.
    let node = first.rsplit(" — ").next().unwrap().split(" -> ").next().unwrap().trim().to_string();
    assert!(node.starts_with("0x"), "{node}");
    let text = ask(&rec, serde_json::json!({"query": "who_wrote", "expr": format!("((order*){node})->next"), "count": 1})).await;
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(first.ends_with("-> 0x10"), "{text}");
    assert!(text.contains("main() at list.cpp:14"), "b->next = 0x10 is the corruption: {text}");

    let text = ask(&rec, serde_json::json!({"query": "last_reached", "location": "total"})).await;
    assert!(text.contains("args: o=0x"), "{text}");
    assert!(text.contains("called from main() at list.cpp:15"), "{text}");
}

#[tokio::test]
#[ignore]
async fn live_stop_at_moves_the_question_earlier_in_time() {
    let Some(rec) = record_cxx("book", BOOK, &[]).await else { return };
    // Asked from main's check, the ask was written by risk_reset; asked from
    // the moment risk_reset is about to write, the last writer is the feed.
    let text = ask(
        &rec,
        serde_json::json!({"query": "who_wrote", "expr": "book.ask", "count": 1, "stop_at": "book.cpp:14"}),
    )
    .await;
    assert!(text.contains("asked from: arrival 1 at `book.cpp:14`"), "{text}");
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(first.contains("(feed)"), "{text}");
    let text = ask(&rec, serde_json::json!({"query": "stop_point", "stop_at": "no_such_function_xyz"})).await;
    assert!(text.starts_with("ERROR") && text.contains("Nothing answered"), "{text}");
}

#[tokio::test]
#[ignore]
async fn live_a_clean_exit_is_asked_from_the_end_and_a_static_value_reaches_the_start() {
    let Some(rec) = record_cxx(
        "cfg",
        "#include <cstring>\nstruct cfg { char name[8]; int max_qty; };\ncfg g = { \"\", 500 };\n\
         __attribute__((noinline)) void set_name(const char *s) { std::strcpy(g.name, s); }\n\
         int main() { set_name(\"ABCDEFGHIJK\"); return g.max_qty == 500 ? 0 : 3; }\n",
        &["-fno-builtin"],
    )
    .await
    else {
        return;
    };
    let text = ask(&rec, serde_json::json!({"query": "who_wrote", "expr": "g.max_qty", "count": 3})).await;
    assert!(text.contains("exited with status 3"), "{text}");
    assert!(text.contains("asked from: the end of the recording"), "{text}");
    assert!(text.contains("500 -> "), "{text}");
    assert!(text.contains("called from set_name() at cfg.cpp:4"), "the overflow inside strcpy: {text}");
    assert!(text.contains("no earlier write in the recording: 500 was already there"), "{text}");
}

/// Manual check on a trace recorded on a CPU this process may not use:
///   BYTEASK_RR_BOUND_TRACE=<trace> <test-binary> --include-ignored reverse::tests::live_a_trace_from
#[tokio::test]
#[ignore]
async fn live_a_trace_from_an_isolated_cpu_is_replayed_unbound() {
    let Some(trace) = std::env::var_os("BYTEASK_RR_BOUND_TRACE").map(PathBuf::from) else {
        eprintln!("skip: set BYTEASK_RR_BOUND_TRACE");
        return;
    };
    let rec = Recorded {
        dir: tempfile::tempdir().unwrap(),
        trace,
    };
    let text = ask(&rec, serde_json::json!({"query": "stop_point"})).await;
    assert!(text.contains("asked from:"), "{text}");
    if !text.contains("replayed unbound (`rr replay -u`)") {
        // Measured: whether rr can bind to the recorded CPU depends on the
        // cpuset at the moment, so the retry cannot always be provoked.
        eprintln!("skip: rr could bind to the recorded CPU this time; the unbound retry was not needed");
    }
}

#[tokio::test]
#[ignore]
async fn live_stop_event_asks_from_before_an_earlier_answer() {
    let Some(rec) = record_cxx("book", BOOK, &[]).await else { return };
    let text = ask(&rec, serde_json::json!({"query": "who_wrote", "expr": "book.ask", "count": 1})).await;
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    let event: u64 = first
        .trim_start_matches(" 1. event ")
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no event in {first}"));
    // From just before that write, the last writer of the ask is the feed.
    let text = ask(
        &rec,
        serde_json::json!({"query": "who_wrote", "expr": "book.ask", "count": 1, "stop_event": event - 1}),
    )
    .await;
    assert!(text.contains(&format!("asked from: event {}", event - 1)) || text.contains("rr cannot stop exactly"), "{text}");
    let first = text.lines().find(|l| l.starts_with(" 1. ")).unwrap_or_else(|| panic!("{text}"));
    assert!(first.contains("(feed)"), "{text}");
}
