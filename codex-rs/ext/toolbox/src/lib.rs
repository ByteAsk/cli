//! The ByteAsk toolbox: every ByteAsk tool, from the same registry the TUI
//! uses, served to Claude Code, Codex or any MCP client over stdio.

pub mod checks;
pub mod cli;
mod entitlement;
pub mod gate;
mod install;
mod registry;
mod report;
mod server;
pub mod trial;

pub use registry::install_byteask_tools;
