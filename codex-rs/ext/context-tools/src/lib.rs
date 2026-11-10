//! The context engine's tools (`context_*`) in ByteAsk's tool registry,
//! so the ByteAsk CLI, Claude Code and Codex (through the toolbox) see them
//! beside the C/C++ tools.
//!
//! The tools are offered only in a workspace the context engine has indexed
//! ([`engine::workspace_for`]); elsewhere they cost the model nothing. Each
//! call goes to the installed context engine's query server over JSON-RPC
//! ([`client`]), and its answer comes back in the shared result format
//! with the build setting and commit first ([`render`]).

mod backend;
mod client;
pub mod engine;
mod extension;
mod guidance;
pub mod harness;
mod render;
mod tools;

#[cfg(test)]
mod description_snapshot_tests;

pub use extension::install;
pub use extension::tools_gated;
pub use guidance::CONTEXT_GUIDANCE;
pub use tools::TOOL_NAMES;
