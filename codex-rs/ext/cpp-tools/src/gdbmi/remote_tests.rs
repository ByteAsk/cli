#![cfg(target_os = "linux")]

use std::time::Instant;

use pretty_assertions::assert_eq;
use tokio::io::AsyncWriteExt;

use super::*;

fn fake_gdb(mode: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let script = format!(
        r#"#!/usr/bin/python3
import json, os, sys, time
mode = {mode:?}
root = os.path.dirname(__file__)
summaries = 0
help_text = 'Command Usage Description\nhelp Display Help Message\nsummary Summarize the system\nps Display active task list\ndmesg Display system messages\nlsmod List loaded modules\nbt Backtrace\nrd Registers\n'
summary = 'sysname    Linux\nrelease    6.1-fixture\nversion    test\nmachine    x86_64\nnodename   guest\nMemTotal: 65536 kB\n'
for line in sys.stdin:
    with open(root + '/commands', 'a') as f: f.write(line)
    token = line.split('-', 1)[0]
    if '-target-select remote "' in line:
        print(token + '^error,msg="quoted device name is literal in GDB target-select"', flush=True)
        continue
    if mode == 'eof': sys.exit(0)
    if mode == 'wrong_token': token = str(int(token) + 1)
    if mode == 'error':
        print(token + '^error,msg="fixture failure"', flush=True)
        continue
    if mode == 'unexpected_result':
        print(token + '^running', flush=True)
        continue
    text = ''
    if 'monitor help' in line:
        text = help_text if mode != 'unsupported' else 'help QEMU monitor\n'
    if 'monitor summary' in line:
        summaries += 1
        if summaries > 1 and mode == 'dropped': sys.exit(0)
        if summaries > 1 and mode == 'resumed':
            print('*running,thread-id="all"', flush=True)
            continue
        if summaries > 1 and mode == 'slow':
            time.sleep(2)
            open(root + '/late', 'w').write('ran')
        if summaries > 1 and mode == 'oversized':
            sys.stdout.write('~"' + 'x' * 100000)
            sys.stdout.flush()
            time.sleep(2)
        if summaries > 1 and mode == 'flood':
            for _ in range(200): print('~' + json.dumps('中文' * 100 + '\n', ensure_ascii=False), flush=True)
        text = summary if mode != 'not_linux' else 'sysname OtherOS\nrelease fixture\nMemTotal: 65536 kB\n'
    if 'monitor ps A' in line: text = 'Task Addr Pid Parent CPU State Command\n0xff 1 0 0 R init\n'
    if 'monitor dmesg' in line: text = 'guest log line 1\nguest log line 2\n'
    if text: print('~' + json.dumps(text), flush=True)
    print(token + ('^connected' if '-target-select' in line else '^done'), flush=True)
    print('(gdb)', flush=True)
"#
    );
    let path = dir.path().join("gdb");
    std::fs::write(&path, script).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

async fn connect(dir: &tempfile::TempDir, budget: Duration) -> Result<Arc<KernelSession>, String> {
    let registry = KernelRegistry::new();
    let (id, permit) = registry.reserve()?;
    KernelSession::connect_with(
        MiClient::spawn(dir.path().join("gdb").to_str().expect("path"), dir.path())?,
        id,
        permit,
        "tcp:localhost:2012".to_string(),
        /*baud*/ None,
        /*symbols*/ None,
        budget,
    )
    .await
}

#[tokio::test]
async fn connects_inspects_and_disconnects_without_run_control() {
    let dir = fake_gdb("ok");
    let session = connect(&dir, Duration::from_secs(2))
        .await
        .expect("connect");
    assert_eq!(
        session.capabilities,
        INSPECTIONS
            .iter()
            .map(|(n, _)| n.to_string())
            .collect::<Vec<_>>()
    );
    let text = session
        .command(wire::monitor("ps A"))
        .await
        .expect("inspect");
    assert!(text.contains("init"));
    session
        .command("-target-disconnect".to_string())
        .await
        .expect("disconnect");
    assert!(!session.connected.load(Ordering::SeqCst));
    assert!(session.command(wire::monitor("summary")).await.is_err());
    let log = std::fs::read_to_string(dir.path().join("commands")).expect("log");
    assert!(log.contains("-target-disconnect"));
    for forbidden in [
        "-target-detach",
        "-gdb-exit",
        "-exec-continue",
        "-exec-run",
        "monitor go",
        "monitor reboot",
    ] {
        assert!(!log.contains(forbidden), "{log}");
    }
}

#[tokio::test]
async fn rejects_wrong_targets_and_protocol_failures() {
    for (mode, expected) in [
        ("unsupported", "does not expose"),
        ("not_linux", "Linux KDB summary"),
        ("eof", "closed"),
        ("wrong_token", "token"),
        ("error", "fixture failure"),
        ("unexpected_result", "Unexpected GDB result"),
    ] {
        let dir = fake_gdb(mode);
        let error = connect(&dir, Duration::from_secs(2))
            .await
            .err()
            .expect("refusal");
        assert!(error.contains(expected), "{mode}: {error}");
    }
}

#[tokio::test]
async fn dropped_transport_or_unexpected_resume_retires_without_run_control() {
    for (mode, expected) in [("dropped", "closed"), ("resumed", "resumed unexpectedly")] {
        let dir = fake_gdb(mode);
        let session = connect(&dir, Duration::from_secs(2))
            .await
            .expect("connect");
        let error = session
            .command(wire::monitor("summary"))
            .await
            .expect_err("retired");
        assert!(error.contains(expected), "{error}");
        assert!(!session.connected.load(Ordering::SeqCst));
        assert!(session.command(wire::monitor("ps A")).await.is_err());
        let log = std::fs::read_to_string(dir.path().join("commands")).expect("log");
        for forbidden in [
            "-target-disconnect",
            "-target-detach",
            "-gdb-exit",
            "-exec-continue",
            "monitor ps",
        ] {
            assert!(!log.contains(forbidden), "{log}");
        }
    }
}

#[tokio::test]
async fn timeout_retires_and_reaps_the_debugger_without_late_execution() {
    let dir = fake_gdb("slow");
    let session = connect(&dir, Duration::from_millis(250))
        .await
        .expect("connect");
    let start = Instant::now();
    assert!(
        session
            .command(wire::monitor("summary"))
            .await
            .expect_err("timeout")
            .contains("timed out")
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!session.connected.load(Ordering::SeqCst));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!dir.path().join("late").exists());
    let log = std::fs::read_to_string(dir.path().join("commands")).expect("log");
    assert!(
        !log.contains("-target-disconnect"),
        "unsynchronized connection must be killed only"
    );
}

#[tokio::test]
async fn canceled_request_kills_connection_and_queued_command_never_runs() {
    let dir = fake_gdb("slow");
    let session = connect(&dir, Duration::from_secs(4))
        .await
        .expect("connect");
    let first = session.clone();
    let task = tokio::spawn(async move { first.command(wire::monitor("summary")).await });
    for _ in 0..100 {
        if std::fs::read_to_string(dir.path().join("commands"))
            .unwrap_or_default()
            .matches("monitor summary")
            .count()
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = session.clone();
    let queued = tokio::spawn(async move { second.command(wire::monitor("ps A")).await });
    task.abort();
    let _ = task.await;
    assert!(queued.await.expect("join").is_err());
    assert!(
        !std::fs::read_to_string(dir.path().join("commands"))
            .expect("log")
            .contains("monitor ps")
    );
    assert!(!dir.path().join("late").exists());
}

#[tokio::test]
async fn bounds_multibyte_output_and_oversized_records() {
    let dir = fake_gdb("flood");
    let session = connect(&dir, Duration::from_secs(2))
        .await
        .expect("connect");
    let text = session
        .command(wire::monitor("summary"))
        .await
        .expect("bounded");
    assert!(text.len() <= crate::gdb_common::OUTPUT_CAP + 30);
    assert!(text.contains("中文"));
    assert!(text.contains("output truncated"));
    session
        .command("-target-disconnect".to_string())
        .await
        .expect("disconnect");
    let dir = fake_gdb("oversized");
    let session = connect(&dir, Duration::from_secs(2))
        .await
        .expect("connect");
    assert!(
        session
            .command(wire::monitor("summary"))
            .await
            .expect_err("oversized")
            .contains("Oversized")
    );
}

#[tokio::test]
async fn a_newline_free_stream_and_unbounded_stream_are_refused() {
    for data in [
        format!("~\"{}", "x".repeat(70_000)),
        "~\"x\"\n".repeat(9000),
    ] {
        let (mut write, read) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ = write.write_all(data.as_bytes()).await;
        });
        let error = wire::exchange(
            &mut tokio::io::sink(),
            &mut BufReader::new(read),
            1,
            "-test",
        )
        .await
        .expect_err("refused");
        assert!(
            error.contains("Oversized") || error.contains("Too many"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn thread_drop_disconnects_and_registry_has_a_connection_cap() {
    let registry = KernelRegistry::new();
    let permits: Vec<_> = (0..4).map(|_| registry.reserve().expect("slot")).collect();
    assert!(registry.reserve().is_err());
    drop(permits);
    assert!(registry.reserve().is_ok());
    let dir = fake_gdb("ok");
    let session = connect(&dir, Duration::from_secs(2))
        .await
        .expect("connect");
    drop(session);
    for _ in 0..100 {
        let log = std::fs::read_to_string(dir.path().join("commands")).unwrap_or_default();
        if log.contains("-target-disconnect") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("idle thread cleanup did not disconnect");
}

#[tokio::test]
async fn missing_gdb_is_a_recoverable_error() {
    let dir = tempfile::tempdir().expect("dir");
    let error = MiClient::spawn("/no/such/gdb", dir.path())
        .err()
        .expect("missing");
    assert!(error.contains("Install GDB"));
}

#[tokio::test]
#[ignore = "live disposable KGDB guest; BYTEASK_KDB_ENDPOINT must point to its serial proxy"]
async fn live_kdb_guest() {
    let endpoint = std::env::var("BYTEASK_KDB_ENDPOINT").expect("guest endpoint");
    let registry = KernelRegistry::new();
    let (id, permit) = registry.reserve().expect("slot");
    let dir = tempfile::tempdir().expect("dir");
    let s = KernelSession::connect(
        id,
        permit,
        endpoint,
        None,
        None,
        dir.path(),
        Duration::from_secs(10),
    )
    .await
    .expect("real KGDB/KDB");
    for (_, raw) in INSPECTIONS {
        if s.capabilities.iter().any(|cap| {
            INSPECTIONS
                .iter()
                .any(|(name, cmd)| cap == name && cmd == raw)
        }) {
            let output = s
                .command(wire::monitor(raw))
                .await
                .expect("real inspection");
            println!("{raw}: {output}");
        }
    }
    s.command("-target-disconnect".to_string())
        .await
        .expect("disconnect");
    let (id, permit) = registry.reserve().expect("slot");
    let s = KernelSession::connect(
        id,
        permit,
        std::env::var("BYTEASK_KDB_ENDPOINT").expect("endpoint"),
        None,
        None,
        dir.path(),
        Duration::from_secs(10),
    )
    .await
    .expect("reconnect");
    s.command("-target-disconnect".to_string())
        .await
        .expect("disconnect");
}
