//! Linux KDB inspection through a prepared remote KGDB connection.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::json;

use crate::gdbmi::remote::INSPECTIONS;
use crate::gdbmi::remote::KernelRegistry;
use crate::gdbmi::remote::KernelSession;
use crate::gdbmi::remote::wire::monitor;
use crate::report::report;

const TOOL: &str = "cpp_kdb";

pub struct KdbTool {
    pub cwd: PathBuf,
    pub(crate) sessions: Arc<KernelRegistry>,
    pub native_exec: bool,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Args {
    Connect {
        transport: Transport,
        #[serde(default)]
        vmlinux: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    Inspect {
        session: u64,
        command: Inspection,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Status {
        session: u64,
    },
    List {},
    Disconnect {
        session: u64,
    },
}

fn default_timeout() -> u64 {
    15
}
fn default_limit() -> usize {
    80
}
fn default_baud() -> u32 {
    115_200
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Transport {
    Tcp {
        host: String,
        port: u16,
    },
    Serial {
        device: String,
        #[serde(default = "default_baud")]
        baud: u32,
    },
}

impl Transport {
    fn endpoint(&self) -> Result<(String, Option<u32>), String> {
        match self {
            Self::Tcp { host, port } => {
                if *port == 0 || host.is_empty() || host.len() > 253 {
                    return Err("TCP requires a host and a port in 1..65535.".to_string());
                }
                let host = match host.parse::<IpAddr>() {
                    Ok(IpAddr::V6(addr)) => format!("[{addr}]"),
                    Ok(IpAddr::V4(addr)) => addr.to_string(),
                    Err(_) => {
                        if !host.split('.').all(|label| {
                            !label.is_empty()
                                && label.len() <= 63
                                && label.starts_with(|c: char| c.is_ascii_alphanumeric())
                                && label.ends_with(|c: char| c.is_ascii_alphanumeric())
                                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                        }) {
                            return Err("TCP host must be an IP address or a DNS hostname; debugger commands and pipe transports are not accepted.".to_string());
                        }
                        host.clone()
                    }
                };
                Ok((format!("tcp:{host}:{port}"), None))
            }
            Self::Serial { device, baud } => {
                if !device.starts_with("/dev/")
                    || device.len() > 1024
                    || device
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\\'))
                    || !(1200..=4_000_000).contains(baud)
                {
                    return Err(
                        "Serial requires a single-token absolute /dev/ device path without quotes or escapes and baud in 1200..4000000."
                            .to_string(),
                    );
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if !std::fs::metadata(device)
                        .map_err(|e| format!("Serial device {device}: {e}"))?
                        .file_type()
                        .is_char_device()
                    {
                        return Err(
                            "Serial transport must name a character device, not a file or FIFO."
                                .to_string(),
                        );
                    }
                }
                Ok((device.clone(), Some(*baud)))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Inspection {
    Help,
    Summary,
    Processes,
    Modules,
    Dmesg,
    Backtrace,
    Registers,
}

impl Inspection {
    fn name(&self) -> &str {
        match self {
            Self::Help => "help",
            Self::Summary => "summary",
            Self::Processes => "processes",
            Self::Modules => "modules",
            Self::Dmesg => "dmesg",
            Self::Backtrace => "backtrace",
            Self::Registers => "registers",
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for KdbTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL)
    }

    fn spec(&self) -> ToolSpec {
        let properties = BTreeMap::from([
            ("action".to_string(), JsonSchema::string_enum(vec![json!("connect"), json!("inspect"), json!("status"), json!("list"), json!("disconnect")], Some("connect opens a prepared, already stopped remote kernel; inspect reads it; status/list show this thread's sessions; disconnect closes without resuming.".to_string()))),
            ("transport".to_string(), JsonSchema::object(BTreeMap::from([
                ("kind".to_string(), JsonSchema::string_enum(vec![json!("tcp"), json!("serial")], None)),
                ("host".to_string(), JsonSchema::string(Some("tcp: hostname or IP of a proxy to the KGDB serial transport, not a QEMU GDB stub.".to_string()))),
                ("port".to_string(), JsonSchema::integer(Some("tcp: port, 1..65535.".to_string()))),
                ("device".to_string(), JsonSchema::string(Some("serial: absolute /dev/ character device, e.g. /dev/ttyUSB0.".to_string()))),
                ("baud".to_string(), JsonSchema::integer(Some("serial: baud rate, default 115200.".to_string()))),
            ]), Some(vec!["kind".to_string()]), Some(AdditionalProperties::from(false)))),
            ("vmlinux".to_string(), JsonSchema::string(Some("connect: optional matching uncompressed kernel symbol image; omit for monitor-only inspection. Matching build and KASLR relocation are user-supplied.".to_string()))),
            ("timeout_secs".to_string(), JsonSchema::integer(Some("connect: connection and per-command wall-clock budget, 1..60 seconds, default 15.".to_string()))),
            ("session".to_string(), JsonSchema::integer(Some("inspect/status/disconnect: id returned by connect.".to_string()))),
            ("command".to_string(), JsonSchema::string_enum(INSPECTIONS.iter().map(|(name, _)| json!(name)).collect(), Some("inspect: typed read-only KDB inspection. Availability is discovered on the target. Processes shows all tasks; dmesg returns the most recent lines.".to_string()))),
            ("limit".to_string(), JsonSchema::integer(Some("inspect: maximum output lines, 1..200, default 80; output is also byte capped.".to_string()))),
        ]);
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL.to_string(),
            description: "Inspect a Linux kernel using KDB through a persistent remote KGDB connection: kernel summary, tasks, modules, dmesg, backtrace and registers when supported. Use for kernel/driver bugs, not user-process debugging. Linux debugging hosts only. The user must configure CONFIG_KGDB_KDB and the KGDB transport and already stop the target; ByteAsk must run outside that target (a separate host or outside a guest VM). Does not trigger SysRq, run/continue/reboot, set breakpoints or write memory. connect returns a session id; inspect reads it; disconnect closes the connection and may leave the target stopped. Native execution requires project trust or BYTEASK_ALLOW_NATIVE_TOOLS=1. Kernel output is sent to the model and follows normal local tool logging.".to_string(),
            strict: false,
            parameters: JsonSchema::object(properties, Some(vec!["action".to_string()]), Some(AdditionalProperties::from(false))),
            output_schema: None, defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }
    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl KdbTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: Args = serde_json::from_str(call.function_arguments()?)
            .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
        if matches!(args, Args::Connect { .. } | Args::Inspect { .. }) {
            crate::trust::require_native_exec(self.native_exec, TOOL, &self.cwd)?;
            if !cfg!(target_os = "linux") {
                return Err(FunctionCallError::RespondToModel("cpp_kdb currently supports Linux debugging hosts. Run ByteAsk on a Linux host outside the target.".to_string()));
            }
        }
        let result = match args {
            Args::Connect {
                transport,
                vmlinux,
                timeout_secs,
            } => {
                if !(1..=60).contains(&timeout_secs) {
                    return Err(FunctionCallError::RespondToModel(
                        "timeout_secs must be in 1..60.".to_string(),
                    ));
                }
                let (endpoint, baud) = transport
                    .endpoint()
                    .map_err(FunctionCallError::RespondToModel)?;
                let symbols = vmlinux.map(|p| crate::gdb_common::resolve_path(&self.cwd, &p));
                if let Some(path) = &symbols
                    && (!path.is_file()
                        || path
                            .as_os_str()
                            .to_string_lossy()
                            .chars()
                            .any(char::is_control))
                {
                    return Err(FunctionCallError::RespondToModel(
                        "vmlinux must be a regular kernel image file with a valid path."
                            .to_string(),
                    ));
                }
                let (id, permit) = self
                    .sessions
                    .reserve()
                    .map_err(FunctionCallError::RespondToModel)?;
                match KernelSession::connect(
                    id,
                    permit,
                    endpoint,
                    baud,
                    symbols.as_deref(),
                    &self.cwd,
                    Duration::from_secs(timeout_secs),
                )
                .await
                {
                    Ok(session) => {
                        let description = session.describe();
                        self.sessions.insert(session);
                        Ok((format!("#{id} connect"), description))
                    }
                    Err(error) => Err(format!(
                        "{error}\nConnection closed; target may remain stopped. Reconnect from the debugging host to recover."
                    )),
                }
            }
            Args::Inspect {
                session,
                command,
                limit,
            } => {
                if !(1..=200).contains(&limit) {
                    return Err(FunctionCallError::RespondToModel(
                        "limit must be in 1..200.".to_string(),
                    ));
                }
                let s = self
                    .sessions
                    .get(session)
                    .map_err(FunctionCallError::RespondToModel)?;
                let name = command.name();
                if !s.capabilities.iter().any(|cap| cap == name) {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Target does not support {name}. Available: {}",
                        s.capabilities.join(", ")
                    )));
                }
                let raw = INSPECTIONS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, raw)| *raw)
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(format!(
                            "No KDB command mapping for {name}."
                        ))
                    })?;
                // KDB accepts a typed line count for dmesg. Limit at the target
                // to preserve the tail before the host-side byte capture cap.
                let raw = match command {
                    Inspection::Processes => "ps A".to_string(),
                    Inspection::Dmesg => format!("dmesg {limit}"),
                    _ => raw.to_string(),
                };
                s.command(monitor(&raw)).await.map(|output| {
                    let mut lines = output.lines();
                    let mut body = lines.by_ref().take(limit).collect::<Vec<_>>().join("\n");
                    if lines.next().is_some() { body.push_str("\n[output truncated to line limit]"); }
                    if body.is_empty() { body = "Command completed with no output; this is not proof of absence of a kernel problem.".to_string(); }
                    (format!("#{session} {name}"), body)
                })
            }
            Args::Status { session } => self
                .sessions
                .get(session)
                .map(|s| (format!("#{session}"), s.describe())),
            Args::List {} => {
                let rows = self.sessions.list();
                Ok((
                    String::new(),
                    if rows.is_empty() {
                        "No open kernel sessions.".to_string()
                    } else {
                        rows.join("\n\n")
                    },
                ))
            }
            Args::Disconnect { session } => {
                let s = self
                    .sessions
                    .get(session)
                    .map_err(FunctionCallError::RespondToModel)?;
                let result = s.command("-target-disconnect".to_string()).await;
                self.sessions.remove(session);
                result.map(|_| (format!("#{session} disconnect"), "Disconnected without a resume command. Last observed target state: stopped; verify remotely before assuming it resumed.".to_string()))
            }
        };
        let (target, body) = result
            .map_err(|e| FunctionCallError::RespondToModel(crate::gdb_common::truncate(e, 4000)))?;
        Ok(report(
            TOOL,
            &target,
            body,
            check_result::CheckResult::pass(TOOL, "The kernel debugger answered."),
        ))
    }
}

#[cfg(test)]
#[path = "kdb_tests.rs"]
mod tests;
