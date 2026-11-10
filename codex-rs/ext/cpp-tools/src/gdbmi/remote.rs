//! Inspection-only KGDB sessions. Unlike local debugger sessions, closing one
//! disconnects without detaching/resuming the kernel or signaling an inferior.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::parser::MiRecord;
use super::parser::StreamKind;
use super::parser::parse_mi_line;

pub(crate) mod wire;

pub(crate) const INSPECTIONS: &[(&str, &str)] = &[
    ("help", "help"),
    ("summary", "summary"),
    ("processes", "ps"),
    ("modules", "lsmod"),
    ("dmesg", "dmesg"),
    ("backtrace", "bt"),
    ("registers", "rd"),
];

pub(crate) struct KernelRegistry {
    sessions: Mutex<BTreeMap<u64, Arc<KernelSession>>>,
    next: AtomicU64,
    slots: Arc<Semaphore>,
}

impl KernelRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            slots: Arc::new(Semaphore::new(4)),
        })
    }

    pub(crate) fn reserve(&self) -> Result<(u64, OwnedSemaphorePermit), String> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            "Four kernel connections are already open or connecting; disconnect one first."
                .to_string()
        })?;
        Ok((self.next.fetch_add(1, Ordering::Relaxed), permit))
    }

    pub(crate) fn insert(&self, session: Arc<KernelSession>) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, s| s.connected.load(Ordering::SeqCst));
        sessions.insert(session.id, session);
    }

    pub(crate) fn get(&self, id: u64) -> Result<Arc<KernelSession>, String> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("No kernel session #{id}. Use action=list or connect."))
    }

    pub(crate) fn remove(&self, id: u64) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    pub(crate) fn list(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|s| s.describe())
            .collect()
    }
}

struct Request {
    command: String,
    reply: oneshot::Sender<Result<String, String>>,
}

pub(crate) struct KernelSession {
    pub(crate) id: u64,
    pub(crate) capabilities: Vec<String>,
    pub(crate) identity: String,
    endpoint: String,
    symbols: String,
    connected: Arc<AtomicBool>,
    requests: mpsc::Sender<Request>,
}

impl KernelSession {
    pub(crate) async fn connect(
        id: u64,
        permit: OwnedSemaphorePermit,
        endpoint: String,
        baud: Option<u32>,
        symbols: Option<&Path>,
        cwd: &Path,
        budget: Duration,
    ) -> Result<Arc<Self>, String> {
        let client = MiClient::spawn("gdb", cwd)?;
        Self::connect_with(client, id, permit, endpoint, baud, symbols, budget).await
    }

    async fn connect_with(
        mut client: MiClient,
        id: u64,
        permit: OwnedSemaphorePermit,
        endpoint: String,
        baud: Option<u32>,
        symbols: Option<&Path>,
        budget: Duration,
    ) -> Result<Arc<Self>, String> {
        let (capabilities, identity) = timeout(budget, async {
            client.exchange("-gdb-set pagination off").await?;
            client.exchange("-gdb-set may-call-functions off").await?;
            if let Some(baud) = baud {
                client.exchange(&format!("-gdb-set serial baud {baud}")).await?;
            }
            if let Some(path) = symbols {
                let path = path.to_str().ok_or("vmlinux path must be UTF-8")?;
                client.exchange(&format!("-file-exec-and-symbols {}", wire::quote(path))).await?;
            }
            // GDB's MI target-select passes the device string verbatim to the
            // remote backend (MI quotes become part of the device name). This
            // endpoint comes from the tool's validated, single-token transport.
            client.exchange(&format!("-target-select remote {endpoint}")).await?;
            let help = client.exchange(&wire::monitor("help")).await?;
            let available: Vec<&str> = help.lines().filter_map(|line| line.split_whitespace().next()).collect();
            if !available.contains(&"summary") || !available.contains(&"ps") || !available.contains(&"dmesg") {
                return Err("This endpoint does not expose the Linux KDB monitor (summary/ps/dmesg missing). Use the kernel's KGDB transport, not a QEMU GDB stub or user-space gdbserver.".to_string());
            }
            let summary = client.exchange(&wire::monitor("summary")).await?;
            if !summary.lines().any(|line| line.split_whitespace().collect::<Vec<_>>() == ["sysname", "Linux"])
                || !summary.lines().any(|line| line.starts_with("release "))
                || !summary.contains("MemTotal:") {
                return Err("Remote monitor did not return a Linux KDB summary; connection refused.".to_string());
            }
            let capabilities = INSPECTIONS.iter().filter(|(_, raw)| available.contains(raw))
                .map(|(name, _)| name.to_string()).collect();
            Ok((capabilities, crate::gdb_common::truncate(summary, 1500)))
        }).await.map_err(|_| "Kernel connection timed out. The target must already be stopped; ByteAsk does not trigger SysRq. The target may remain stopped.".to_string())??;
        let connected = Arc::new(AtomicBool::new(true));
        let (requests, mut receive) = mpsc::channel::<Request>(8);
        let state = connected.clone();
        tokio::spawn(async move {
            let _permit = permit;
            while let Some(mut request) = receive.recv().await {
                if request.reply.is_closed() {
                    continue;
                }
                let disconnect = request.command == "-target-disconnect";
                let result = tokio::select! {
                    biased;
                    _ = request.reply.closed() => {
                        state.store(false, Ordering::SeqCst);
                        client.terminate().await;
                        break;
                    },
                    result = timeout(budget, client.exchange(&request.command)) => {
                        result.unwrap_or_else(|_| Err("Kernel command timed out; connection retired. The target may remain stopped; reconnect to inspect it.".to_string()))
                    }
                };
                let retire = result.is_err() || disconnect;
                if retire {
                    state.store(false, Ordering::SeqCst);
                    client.terminate().await;
                }
                let _ = request.reply.send(result);
                if retire {
                    break;
                }
            }
            state.store(false, Ordering::SeqCst);
            // The registry/thread was dropped. A live idle connection can be
            // explicitly disconnected; an unsynchronized one is killed only.
            if client.child.id().is_some() {
                let _ = timeout(
                    Duration::from_secs(1),
                    client.exchange("-target-disconnect"),
                )
                .await;
                client.terminate().await;
            }
        });
        Ok(Arc::new(Self {
            id,
            capabilities,
            identity,
            endpoint,
            symbols: symbols
                .map(|p| {
                    format!(
                        "loaded {}; matching build is user-supplied, not verified",
                        p.display()
                    )
                })
                .unwrap_or_else(|| "not supplied; monitor inspection only".to_string()),
            connected,
            requests,
        }))
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "#{} {} [{}]\nsymbols: {}\ncapabilities: {}\n{}",
            self.id,
            self.endpoint,
            if self.connected.load(Ordering::SeqCst) {
                "connected; last observed stopped"
            } else {
                "closed; remote state unknown"
            },
            self.symbols,
            self.capabilities.join(", "),
            self.identity
        )
    }

    pub(crate) async fn command(&self, command: String) -> Result<String, String> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(
                "Kernel session is closed; reconnect. Remote execution state is unknown."
                    .to_string(),
            );
        }
        let (reply, result) = oneshot::channel();
        self.requests
            .send(Request { command, reply })
            .await
            .map_err(|_| "Kernel session ended; target may remain stopped.".to_string())?;
        result.await.map_err(|_| {
            "Kernel command canceled or session ended; target may remain stopped.".to_string()
        })?
    }
}

struct MiClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    token: u64,
}

impl MiClient {
    fn spawn(gdb: &str, cwd: &Path) -> Result<Self, String> {
        // Kernel debugging is not inside the tool sandbox yet; this
        // records the note the output carries.
        crate::sandbox::wrap(gdb, &[], cwd).map_err(crate::sandbox::error_text)?;
        let mut cmd = Command::new(gdb);
        cmd.args([
            "--interpreter=mi3",
            "-q",
            "-nx",
            "-nh",
            "-iex",
            "set auto-load off",
            "-iex",
            "set debuginfod enabled off",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Could not start gdb: {e}. Install GDB on the debugging host."))?;
        let stdin = child.stdin.take().ok_or("gdb stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("gdb stdout unavailable")?);
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut buf = [0; 1024];
                while stderr.read(&mut buf).await.is_ok_and(|n| n > 0) {}
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout,
            token: 0,
        })
    }

    async fn exchange(&mut self, command: &str) -> Result<String, String> {
        self.token += 1;
        wire::exchange(&mut self.stdin, &mut self.stdout, self.token, command).await
    }

    async fn terminate(&mut self) {
        crate::run::kill_process_group(self.child.id());
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for MiClient {
    fn drop(&mut self) {
        // No quit/detach packet: dropping a connect future must not resume the
        // target. kill_on_drop reaps the owned host debugger.
        crate::run::kill_process_group(self.child.id());
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
