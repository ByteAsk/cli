//! `byteask toolbox report`: the command reads only the local results
//! store, makes no network calls, and the toolbox's tools write into that
//! store.

use std::path::Path;
use std::path::PathBuf;

use check_result::CheckResult;
use clap::Parser as _;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::LoaderOverrides;
use pretty_assertions::assert_eq;
use results_store::store;
use results_store::store::Record;

use super::*;
use crate::registry::ToolboxThread;

const NOW: u64 = 1_800_000_000;

/// A store with one change: an AI edit, a sanitizer FAIL, then a version
/// where it passes (1 problem caught, review-ready in 30 s).
fn small_store(home: &Path) {
    let root = store::root(home);
    let records = [
        Record::Open {
            at: NOW - 100,
            repo: "/work/book".to_string(),
            branch: "main".to_string(),
            session: "s".to_string(),
            head: Some("c0".to_string()),
        },
        Record::Edit {
            at: NOW - 90,
            version: Some("v1".to_string()),
        },
        Record::Result {
            at: NOW - 80,
            version: Some("v1".to_string()),
            result: CheckResult::fail("cpp_sanitize", "heap-buffer-overflow"),
            payload: None,
        },
        Record::Edit {
            at: NOW - 70,
            version: Some("v2".to_string()),
        },
        Record::Result {
            at: NOW - 60,
            version: Some("v2".to_string()),
            result: CheckResult::pass("cpp_sanitize", "clean"),
            payload: None,
        },
    ];
    for record in &records {
        store::append(&root, "c1", record).unwrap();
    }
}

#[test]
fn the_command_line_parses() {
    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: crate::cli::ToolboxCommand,
    }
    let cli = Cli::try_parse_from(["toolbox", "report", "--json", "--days", "30"]).unwrap();
    let crate::cli::ToolboxCommand::Report(args) = cli.command else {
        panic!("not the report command");
    };
    assert!(args.json);
    assert_eq!(args.days, Some(30));
    assert!(Cli::try_parse_from(["toolbox", "report", "--days", "0"]).is_err());
}

#[test]
fn json_is_the_export_and_days_limit_the_period() {
    let home = tempfile::tempdir().unwrap();
    small_store(home.path());
    let json: serde_json::Value = serde_json::from_str(&render(
        home.path(),
        &ReportArgs {
            json: true,
            days: None,
        },
        NOW,
    ))
    .unwrap();
    assert_eq!(json["source"], "local");
    assert_eq!(json["local"]["problems_caught"]["changes"], 1);
    assert_eq!(json["local"]["review_ready"]["median_secs"], 30);

    // A period that starts after everything: nothing counted, still complete.
    let later: serde_json::Value = serde_json::from_str(&render(
        home.path(),
        &ReportArgs {
            json: true,
            days: Some(1),
        },
        NOW + 2 * 86_400,
    ))
    .unwrap();
    assert_eq!(later["local"]["changes"]["total"], 0);
    assert_eq!(later["complete"], true);

    let text = render(home.path(), &ReportArgs::default(), NOW);
    assert!(text.contains("Problems caught (local)"), "{text}");
}

/// The limits come from the policy file.
#[test]
fn the_text_names_the_limits_from_the_policy_file() {
    let home = tempfile::tempdir().unwrap();
    let policy = home.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[results_store]\nmax_days = 30\nmax_bytes = 5242880\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let text = render(home.path(), &ReportArgs::default(), NOW);
    assert!(
        text.contains("keeps results for 30 days and up to 5 MB of tool output"),
        "{text}"
    );
}

/// The toolbox's own tools write into the store: a real C/C++ tool called
/// through the same registry the TUI uses, then the report reads it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_toolbox_tool_call_lands_in_the_store_and_the_report() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.cpp"), "int main() { return 0; }\n").unwrap();
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(workspace.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .expect("config");
    let thread = ToolboxThread::start(&config).await;

    let missing = thread
        .call("cpp_compile_db", "{}".to_string())
        .await
        .expect("the tool exists");
    assert!(missing.text.contains("Result: FAIL"), "{}", missing.text);
    std::fs::write(
        workspace.path().join("compile_commands.json"),
        r#"[{"directory": ".", "command": "c++ -c a.cpp", "file": "a.cpp"}]"#,
    )
    .unwrap();
    let found = thread
        .call("cpp_compile_db", "{}".to_string())
        .await
        .expect("the tool exists");
    assert!(found.text.contains("Result: PASS"), "{}", found.text);

    let loaded = store::load(&store::root(home.path()));
    assert_eq!(loaded.changes.len(), 1, "one session, one directory");
    let results: Vec<(String, check_result::Verdict, Option<String>)> = loaded.changes[0]
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Result {
                result, payload, ..
            } => Some((result.check.clone(), result.verdict, payload.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        results
            .iter()
            .map(|(check, verdict, _)| (check.as_str(), *verdict))
            .collect::<Vec<_>>(),
        vec![
            ("cpp_compile_db", check_result::Verdict::Fail),
            ("cpp_compile_db", check_result::Verdict::Pass),
        ]
    );
    let stored = std::fs::read_to_string(
        store::root(home.path())
            .join("payloads")
            .join(&loaded.changes[0].stem)
            .join(results[1].2.as_deref().expect("a payload")),
    )
    .unwrap();
    assert_eq!(
        stored, found.text,
        "the stored payload is the tool's answer"
    );

    let json: serde_json::Value = serde_json::from_str(&render(
        home.path(),
        &ReportArgs {
            json: true,
            days: None,
        },
        results_store::now_unix(),
    ))
    .unwrap();
    assert_eq!(json["local"]["other_tools"]["cpp_compile_db"]["pass"], 1);
    assert_eq!(json["local"]["other_tools"]["cpp_compile_db"]["fail"], 1);
    assert_eq!(json["local"]["changes"]["total"], 1);
    assert_eq!(json["local"]["review_ready"]["no_ai_edit"], 1);
}

/// The network test: the report runs in a child process where
/// any socket call kills the process (seccomp). The control below proves the
/// trap fires, so a pass here cannot be vacuous.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod network {
    use std::process::Command;

    use pretty_assertions::assert_eq;

    use super::*;

    const CHILD: &str = "BYTEASK_REPORT_TEST_NETWORK_CHILD";
    const HOME: &str = "BYTEASK_REPORT_TEST_NETWORK_HOME";

    #[repr(C)]
    struct SockFilter {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }

    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const SockFilter,
    }

    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;

    /// From here on, any socket syscall kills this process.
    fn deny_sockets() {
        let denied = [
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
        ];
        let stmt = |code, k| SockFilter {
            code,
            jt: 0,
            jf: 0,
            k,
        };
        let mut program = vec![
            stmt(LD_W_ABS, 4), // seccomp_data.arch
            SockFilter {
                code: JEQ_K,
                jt: 1,
                jf: 0,
                k: ARCH,
            },
            stmt(RET_K, RET_KILL_PROCESS),
            stmt(LD_W_ABS, 0), // seccomp_data.nr
        ];
        for nr in denied {
            program.push(SockFilter {
                code: JEQ_K,
                jt: 0,
                jf: 1,
                k: nr as u32,
            });
            program.push(stmt(RET_K, RET_KILL_PROCESS));
        }
        program.push(stmt(RET_K, RET_ALLOW));
        let fprog = SockFprog {
            len: program.len() as u16,
            filter: program.as_ptr(),
        };
        // SAFETY: plain syscalls with a valid, live filter program; results checked.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(
                libc::syscall(libc::SYS_seccomp, 1 /* SET_MODE_FILTER */, 0, &fprog),
                0,
                "seccomp: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Runs only as the child of the tests below.
    #[test]
    fn report_child() {
        let Some(mode) = std::env::var_os(CHILD) else {
            return;
        };
        let home = PathBuf::from(std::env::var_os(HOME).expect("home"));
        deny_sockets();
        if mode == "control" {
            let _ = std::net::TcpStream::connect("127.0.0.1:9");
        }
        let text = render(&home, &ReportArgs::default(), NOW);
        let json = render(
            &home,
            &ReportArgs {
                json: true,
                days: Some(30),
            },
            NOW,
        );
        std::fs::write(home.join("out.txt"), text).unwrap();
        std::fs::write(home.join("out.json"), json).unwrap();
    }

    fn run_child(mode: &str, home: &Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "report::tests::network::report_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .env(HOME, home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("the child runs")
    }

    #[test]
    fn the_report_makes_no_network_calls() {
        let home = tempfile::tempdir().unwrap();
        small_store(home.path());
        let status = run_child("report", home.path());
        assert!(
            status.success(),
            "the report was killed or failed: {status:?}"
        );
        let text = std::fs::read_to_string(home.path().join("out.txt")).unwrap();
        let json = std::fs::read_to_string(home.path().join("out.json")).unwrap();
        // It really read the store, under the trap.
        assert!(text.contains("  1 change: a check failed"), "{text}");
        assert!(json.contains("\"problems_caught\""), "{json}");
    }

    #[test]
    fn control_the_trap_kills_a_process_that_opens_a_socket() {
        use std::os::unix::process::ExitStatusExt as _;
        let home = tempfile::tempdir().unwrap();
        small_store(home.path());
        let status = run_child("control", home.path());
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
        assert!(!home.path().join("out.txt").exists());
    }
}
