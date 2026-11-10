//! ByteAsk token efficiency: shrink what the model READS, at the moment a tool output
//! enters the conversation, never afterwards.
//!
//! Everything here is pure, deterministic and idempotent: the same bytes in always give
//! the same bytes out, so a resumed thread replays exactly what was sent and the
//! provider's prompt cache is never broken by us.
//!
//! * [`terminal`] — the two transforms that are safe on ANY command output: strip ANSI
//!   escapes and collapse bare carriage-return overwrites. "Safe" means
//!   terminal-equivalent: the model receives what a person would see on the screen.
//!   That is not byte-identical, so every reduced output is TAGGED and the raw bytes are
//!   spilled.
//! * [`policy`] — the server-driven switchboard, read from the model catalog file by its
//!   own tolerant loader so a bad policy can never fail the catalog parse.
//! * [`spill`] — the full original, kept on disk, so nothing is ever lost and recovery
//!   never means re-running a command.

pub mod family;
pub mod gated;
pub mod lossless;
pub mod nearloss;
pub mod pipeline;
pub mod policy;
pub mod spill;
pub mod terminal;

pub use family::Family;
pub use pipeline::Ctx;
pub use pipeline::Outcome;
pub use pipeline::reduce;
pub use policy::Policy;
pub use terminal::Reduced;
pub use terminal::reduce_terminal;

/// Marker that ends every output this crate touched. The engine's later truncation
/// passes recognise it and leave such an output alone, which is what keeps a resumed
/// thread byte-identical even when the resume-time model has a smaller budget.
pub const SENTINEL_PREFIX: &str = "[byteask:";

/// True when `text` already went through this crate.
pub fn is_reduced(text: &str) -> bool {
    text.trim_end()
        .rsplit('\n')
        .next()
        .is_some_and(|last| last.trim_start().starts_with(SENTINEL_PREFIX))
}
