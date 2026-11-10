//! The context engine's query server (`serve.py` = `bctx serve`), driven over
//! its stdio: JSON-RPC 2.0, one JSON object per line, in MCP's shape
//! (`tools/list`, `tools/call` with `params.identity.user`).
//!
//! One server process per thread, started on the first call and kept for the
//! thread's life (a query is then a few milliseconds, not a Python start).
//! Calls are serialised: the server answers one request at a time. A call that
//! does not answer in time, a server that exits or writes something that is
//! not a response is killed and started again on the next call, so one bad
//! query cannot wedge the tools for the rest of the session.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::Mutex;

/// The longest response line accepted (the server caps answers near 24,000
/// characters; this leaves room for JSON escaping and the envelope).
pub const MAX_LINE_BYTES: usize = 4 << 20;
/// How much of the server's error output is kept for a failure report.
const STDERR_TAIL_BYTES: usize = 4096;

/// How to start the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The complete environment (the server gets nothing else).
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Why a call produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The server could not be started at all.
    Start(String),
    /// No answer within the time limit; the server was stopped.
    Timeout(Duration),
    /// The server exited, or wrote something that is not an answer.
    Broken { what: String, stderr: String },
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<StdMutex<VecDeque<u8>>>,
    /// Collects `stderr`; ends at end-of-file (the server exited).
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    fn stderr_tail(&self) -> String {
        let tail = match self.stderr.lock() {
            Ok(tail) => tail.iter().copied().collect::<Vec<u8>>(),
            Err(poisoned) => poisoned.into_inner().iter().copied().collect(),
        };
        String::from_utf8_lossy(&tail).trim().to_string()
    }
}

/// The query server of one thread.
pub struct Client {
    launch: Launch,
    running: Mutex<Option<Running>>,
    next_id: AtomicU64,
}

impl Client {
    pub fn new(launch: Launch) -> Self {
        Self {
            launch,
            running: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    fn start(&self) -> Result<Running, ClientError> {
        let mut command = tokio::process::Command::new(&self.launch.program);
        command
            .args(&self.launch.args)
            .env_clear()
            .envs(
                self.launch
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            )
            .current_dir(&self.launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|err| {
            ClientError::Start(format!(
                "could not start {}: {err}",
                self.launch.program.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Start("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Start("no stdout".into()))?;
        let stderr_pipe = child.stderr.take();
        let stderr = Arc::new(StdMutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
        let stderr_reader = stderr_pipe.map(|mut pipe| {
            let tail = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut tail = match tail.lock() {
                        Ok(tail) => tail,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    tail.extend(&buf[..n]);
                    while tail.len() > STDERR_TAIL_BYTES {
                        tail.pop_front();
                    }
                }
            })
        });
        Ok(Running {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
            stderr_reader,
        })
    }

    /// Send one request and return its `result` object (or the JSON-RPC
    /// `error` object as an `Err`-free value under `"error"`).
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the query server answers one request at a time: the lock is held for exactly one request-response exchange"
    )]
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, ClientError> {
        let mut slot = self.running.lock().await;
        if slot.is_none() {
            *slot = Some(self.start()?);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        line.push('\n');
        let outcome = match slot.as_mut() {
            Some(running) => {
                tokio::time::timeout(timeout, exchange(running, line.as_bytes(), id)).await
            }
            None => return Err(ClientError::Start("the server did not start".into())),
        };
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(what)) => {
                let stderr = match slot.take() {
                    Some(mut running) => {
                        let _ = running.child.start_kill();
                        let _ = tokio::time::timeout(Duration::from_secs(1), running.child.wait())
                            .await;
                        // The server has exited, so its stderr ends: the reader
                        // finishes once it holds every last line.
                        if let Some(reader) = running.stderr_reader.take() {
                            let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;
                        }
                        running.stderr_tail()
                    }
                    None => String::new(),
                };
                Err(ClientError::Broken { what, stderr })
            }
            Err(_elapsed) => {
                if let Some(mut running) = slot.take() {
                    let _ = running.child.start_kill();
                }
                Err(ClientError::Timeout(timeout))
            }
        }
    }
}

/// Write the request, then read lines until the response with this id.
async fn exchange(
    running: &mut Running,
    request: &[u8],
    id: u64,
) -> Result<serde_json::Value, String> {
    running
        .stdin
        .write_all(request)
        .await
        .map_err(|err| format!("could not send the request: {err}"))?;
    running
        .stdin
        .flush()
        .await
        .map_err(|err| format!("could not send the request: {err}"))?;
    loop {
        let mut buf = Vec::new();
        let n = (&mut running.stdout)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|err| format!("could not read the answer: {err}"))?;
        if n == 0 {
            return Err("the context engine stopped before answering".to_string());
        }
        if buf.len() > MAX_LINE_BYTES {
            return Err(format!("the answer was longer than {MAX_LINE_BYTES} bytes"));
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| "the context engine wrote something that is not JSON".to_string())?;
        if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
            // An answer to an earlier request that timed out on our side
            // cannot arrive (the server is restarted then), so this is a
            // notification or noise: skip it.
            continue;
        }
        if let Some(result) = value.get("result") {
            return Ok(result.clone());
        }
        if let Some(error) = value.get("error") {
            return Ok(serde_json::json!({ "error": error }));
        }
        return Err("the answer had neither a result nor an error".to_string());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A fake server in sh: answers every request with a fixed result.
    fn fake(script: &str) -> Client {
        Client::new(Launch {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
            cwd: std::env::temp_dir(),
        })
    }

    const ECHO_ID: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"n":%s}}\n' "$id" "$id"
done"#;

    #[tokio::test]
    async fn one_server_answers_many_requests() {
        let client = fake(ECHO_ID);
        let first = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("first");
        let second = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("second");
        assert_eq!(first, serde_json::json!({"n": 1}));
        assert_eq!(second, serde_json::json!({"n": 2}));
    }

    #[tokio::test]
    async fn a_silent_server_times_out_and_is_started_again() {
        // First run of the script: hang. Second run (after the restart): answer.
        let marker = tempfile::tempdir().expect("tmp");
        let flag = marker.path().join("started");
        let script = format!(
            "if [ -e '{flag}' ]; then {ECHO_ID}; else : > '{flag}'; sleep 30; fi",
            flag = flag.display()
        );
        let client = fake(&script);
        let err = client
            .request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_millis(300),
            )
            .await
            .expect_err("timeout");
        assert_eq!(err, ClientError::Timeout(Duration::from_millis(300)));
        let again = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("restarted server answers");
        assert!(again.get("n").is_some(), "{again}");
    }

    #[tokio::test]
    async fn a_server_that_dies_reports_its_last_words() {
        let client = fake("read -r line; echo 'Traceback: no module named bctx' >&2; exit 1");
        let err = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect_err("broken");
        match err {
            ClientError::Broken { stderr, .. } => {
                assert!(stderr.contains("no module named bctx"), "{stderr}")
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn noise_on_stdout_is_a_broken_server_not_an_answer() {
        let client = fake("read -r line; echo 'hello from a print statement'; sleep 5");
        let err = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect_err("broken");
        assert!(matches!(err, ClientError::Broken { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_missing_program_is_a_start_error() {
        let client = Client::new(Launch {
            program: PathBuf::from("/nonexistent/python3"),
            args: Vec::new(),
            env: Vec::new(),
            cwd: std::env::temp_dir(),
        });
        let err = client
            .request("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect_err("start");
        assert!(matches!(err, ClientError::Start(_)), "{err:?}");
    }

    /// The server sees only the environment we give it: no PYTHONPATH or
    /// other variables leak in from the agent's environment.
    #[tokio::test]
    async fn the_server_gets_only_the_environment_we_pass() {
        let client = Client::new(Launch {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                r#"read -r line; printf '{"jsonrpc":"2.0","id":1,"result":{"env":"%s"}}\n' "$(env | sort | tr '\n' ' ')""#
                    .to_string(),
            ],
            env: vec![
                ("ONLY_THIS".to_string(), "1".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ],
            cwd: std::env::temp_dir(),
        });
        let result = client
            .request("x", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("answer");
        let env = result["env"].as_str().expect("env");
        assert!(env.contains("ONLY_THIS=1"), "{env}");
        assert!(!env.contains("HOME="), "{env}");
    }
}
