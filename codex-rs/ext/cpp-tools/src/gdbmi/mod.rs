//! GDB/MI3 plumbing for the interactive `debug_session` tool: a line parser,
//! a stop-record renderer, and a persistent session driving `gdb -i=mi3`;
//! plus its lldb backend (`lldb.rs`), for the Macs gdb does not run on.

#[cfg(target_os = "linux")]
mod inferior_tty;
mod lldb;
mod parser;
pub(crate) mod remote;
mod render;
mod session;

pub(crate) use lldb::find_on_path;
pub use lldb::start_lldb;
pub use session::GdbRegistry;
pub use session::GdbSession;
