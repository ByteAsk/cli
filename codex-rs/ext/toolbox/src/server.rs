//! The stdio MCP front door: every tool from the shared registry,
//! with byte-identical descriptions, served to Claude Code, Codex or any MCP
//! client.
//!
//! ⚠️ stdout carries only JSON-RPC. One stray byte corrupts the stream, and a
//! child process (a compiler, gdb, a profiler) that inherits stdin would read
//! the agent's requests. [`take_stdio`] therefore moves the MCP channel to
//! private descriptors and points fds 0 and 1 at /dev/null and stderr before
//! any tool can run.
#![deny(clippy::print_stdout)]

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_extension_api::ToolSpec;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;

use crate::gate;
use crate::gate::GateState;
use crate::registry::ToolboxThread;

/// The name agents show for this server.
pub const SERVER_NAME: &str = "byteask";

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// An MCP tool from a registry spec: same name, same description bytes, the
/// parameters schema as the input schema. `None` for a spec MCP cannot carry
/// (no ByteAsk tool has one; the parity test asserts it).
pub fn mcp_tool(spec: &ToolSpec) -> Option<Tool> {
    let ToolSpec::Function(function) = spec else {
        return None;
    };
    let schema = serde_json::to_value(&function.parameters).ok()?;
    let serde_json::Value::Object(schema) = schema else {
        return None;
    };
    Some(Tool::new(
        Cow::Owned(function.name.clone()),
        Cow::Owned(function.description.clone()),
        Arc::new(schema),
    ))
}

/// The toolbox MCP server for one workspace.
#[derive(Clone)]
pub struct ToolboxServer {
    thread: Arc<ToolboxThread>,
    tools: Arc<Vec<Tool>>,
    gate: Arc<RwLock<GateState>>,
    instructions: Arc<String>,
}

impl ToolboxServer {
    pub fn new(thread: ToolboxThread, gate: GateState) -> Self {
        let tools = thread.specs().iter().filter_map(mcp_tool).collect();
        let instructions = thread.guidance().join("\n\n");
        Self {
            thread: Arc::new(thread),
            tools: Arc::new(tools),
            gate: Arc::new(RwLock::new(gate)),
            instructions: Arc::new(instructions),
        }
    }

    /// The handle a background refresh uses to publish a new gate state.
    pub fn gate_handle(&self) -> Arc<RwLock<GateState>> {
        Arc::clone(&self.gate)
    }

    /// The tools this server lists.
    pub fn listed_tools(&self) -> &[Tool] {
        &self.tools
    }

    fn gate_state(&self) -> GateState {
        match self.gate.read() {
            Ok(state) => state.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Handle one `tools/call`. Separate from the trait method so tests and the
    /// trait share one path.
    pub async fn call(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        let name = request.name.to_string();
        if !self.thread.has_tool(&name) {
            return Err(McpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        }
        let decision = gate::decide(&name, &self.gate_state(), now_unix());
        if decision != gate::Decision::Allow {
            let result = gate::refusal_result(&name, &decision);
            let mut refused = CallToolResult::error(vec![ContentBlock::text(
                result.append_to(&gate::refusal_message(&name, &decision)),
            )]);
            refused.structured_content = Some(result.to_json());
            return Ok(refused);
        }
        let arguments =
            serde_json::Value::Object(request.arguments.unwrap_or_default()).to_string();
        let Some(outcome) = self.thread.call(&name, arguments).await else {
            return Err(McpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        };
        // The shared result (the text's last line) also travels as
        // structured content. A tool error and a NOT RUN are errors; a FAIL,
        // NO CLAIM or APPROXIMATE is a working tool reporting what it found.
        let result = check_result::CheckResult::from_tool_text(&name, &outcome.text);
        let not_run = result
            .as_ref()
            .is_some_and(|result| result.verdict == check_result::Verdict::NotRun);
        let content = vec![ContentBlock::text(outcome.text)];
        let mut answer = if outcome.is_error || not_run {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        answer.structured_content = result.map(|result| result.to_json());
        Ok(answer)
    }
}

impl ServerHandler for ToolboxServer {
    fn get_info(&self) -> ServerInfo {
        let info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")));
        if self.instructions.is_empty() {
            info
        } else {
            info.with_instructions(self.instructions.as_str())
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items((*self.tools).clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.call(request).await.map(CallToolResponse::from)
    }
}

/// The MCP channel, moved off fds 0/1 so nothing else can write to it or read
/// from it.
#[cfg(unix)]
pub fn take_stdio() -> std::io::Result<(tokio::fs::File, tokio::fs::File)> {
    use std::os::fd::FromRawFd as _;

    fn private_copy(fd: libc::c_int) -> std::io::Result<libc::c_int> {
        // SAFETY: plain descriptor duplication; the result is checked.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(copy)
        }
    }

    let input = private_copy(0)?;
    let output = private_copy(1)?;
    // SAFETY: the path is a valid NUL-terminated string; every result is checked.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(null, 0) < 0 || libc::dup2(2, 1) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::close(null);
    }
    // SAFETY: both descriptors were just created by us and are owned here.
    let (input, output) = unsafe {
        (
            std::fs::File::from_raw_fd(input),
            std::fs::File::from_raw_fd(output),
        )
    };
    Ok((
        tokio::fs::File::from_std(input),
        tokio::fs::File::from_std(output),
    ))
}

#[cfg(test)]
mod tests;
