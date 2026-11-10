#![cfg(target_os = "linux")]

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn tool(native_exec: bool) -> KdbTool {
    KdbTool {
        cwd: std::env::temp_dir(),
        sessions: KernelRegistry::new(),
        native_exec,
    }
}

#[tokio::test]
async fn rejects_unknown_irrelevant_and_control_arguments() {
    let tool = tool(true);
    for args in [
        json!({"action":"list","command":"summary"}),
        json!({"action":"inspect","session":1,"command":"go"}),
        json!({"action":"inspect","session":1,"command":"summary\nreboot"}),
        json!({"action":"inspect","session":1,"command":"summary","approved":true}),
        json!({"action":"continue","session":1}),
        json!({"action":"connect","transport":{"kind":"tcp","host":"localhost","port":1,"device":"/dev/null"}}),
        json!({"action":"connect","transport":{"kind":"tcp","host":"localhost","port":1},"timeout_secs":0}),
        json!({"action":"connect","transport":{"kind":"tcp","host":"localhost","port":1},"timeout_secs":61}),
        json!({"action":"inspect","session":1,"command":"summary","limit":0}),
        json!({"action":"inspect","session":1,"command":"summary","limit":201}),
        json!({"action":"inspect","session":-1,"command":"summary"}),
    ] {
        assert!(
            crate::test_support::call(&tool, args.clone())
                .await
                .is_err(),
            "{args}"
        );
    }
}

#[test]
fn validates_tcp_endpoints_and_refuses_pipe_transports() {
    for host in [
        "|touch /tmp/marker",
        "localhost\nmonitor go",
        "a;b",
        "a b",
        "-x",
        "a..b",
        "é",
        "127.0.0.1:123",
        "[::1]",
        "a/../../x",
    ] {
        assert!(
            Transport::Tcp {
                host: host.to_string(),
                port: 123
            }
            .endpoint()
            .is_err(),
            "{host}"
        );
    }
    for (host, expected) in [
        ("localhost", "tcp:localhost:123"),
        ("10.0.0.1", "tcp:10.0.0.1:123"),
        ("::1", "tcp:[::1]:123"),
        ("target-board.example", "tcp:target-board.example:123"),
    ] {
        assert_eq!(
            Transport::Tcp {
                host: host.to_string(),
                port: 123
            }
            .endpoint()
            .expect("valid"),
            (expected.to_string(), None)
        );
    }
    assert!(
        Transport::Tcp {
            host: "localhost".to_string(),
            port: 0
        }
        .endpoint()
        .is_err()
    );
}

#[test]
fn validates_serial_devices_without_opening_a_fifo() {
    for (device, baud) in [
        ("ttyUSB0", 115200),
        ("/tmp/tty", 115200),
        ("/dev/tty\nmonitor go", 115200),
        ("/dev/tty USB", 115200),
        ("/dev/tty\"USB", 115200),
        ("/dev/tty\\USB", 115200),
        ("/dev/null", 0),
        ("/dev/no-such-kdb-device", 115200),
    ] {
        assert!(
            Transport::Serial {
                device: device.to_string(),
                baud
            }
            .endpoint()
            .is_err()
        );
    }
    assert_eq!(
        Transport::Serial {
            device: "/dev/null".to_string(),
            baud: 115200
        }
        .endpoint()
        .expect("char device"),
        ("/dev/null".to_string(), Some(115200))
    );
}

#[tokio::test]
async fn trust_gate_blocks_connect_and_inspect_but_allows_cleanup_queries() {
    let tool = tool(false);
    for args in [
        json!({"action":"connect","transport":{"kind":"tcp","host":"localhost","port":1}}),
        json!({"action":"inspect","session":1,"command":"summary"}),
    ] {
        let error = crate::test_support::call(&tool, args)
            .await
            .expect_err("untrusted");
        assert!(format!("{error:?}").contains("BYTEASK_ALLOW_NATIVE_TOOLS"));
    }
    assert!(
        crate::test_support::call(&tool, json!({"action":"list"}))
            .await
            .expect("list")
            .contains("No open kernel")
    );
    for action in ["status", "disconnect"] {
        let error = crate::test_support::call(&tool, json!({"action":action,"session":99}))
            .await
            .expect_err("missing");
        assert!(format!("{error:?}").contains("No kernel session"));
        assert!(!format!("{error:?}").contains("trust"));
    }
}

#[tokio::test]
async fn missing_symbols_and_connection_refusal_do_not_end_a_turn() {
    let tool = tool(true);
    let error = crate::test_support::call(&tool, json!({"action":"connect","transport":{"kind":"tcp","host":"localhost","port":1},"vmlinux":"/no/such/vmlinux"})).await.expect_err("missing symbols");
    assert!(matches!(error, FunctionCallError::RespondToModel(_)));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let error = crate::test_support::call(&tool, json!({"action":"connect","transport":{"kind":"tcp","host":"127.0.0.1","port":port},"timeout_secs":1})).await.expect_err("refused");
    assert!(matches!(error, FunctionCallError::RespondToModel(_)));
    assert!(
        crate::test_support::call(&tool, json!({"action":"list"}))
            .await
            .expect("list after error")
            .contains("No open kernel")
    );
}
