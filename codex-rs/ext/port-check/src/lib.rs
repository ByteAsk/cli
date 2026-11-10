//! `port_check`: is this C++ port of a Python trading strategy the same strategy?
//!
//! The answer is earned by replay, never by reading code: one recording of
//! market data goes through the Python original and the C++ port under ONE deterministic
//! harness ([`sim`]), each version answers every event over a small line protocol
//! ([`protocol`], adapters in `adapters/`), and the two transcripts are compared with the shared
//! replay library (`byteask-replay`). The Python version must reproduce itself
//! exactly before anything is claimed; orders and decisions must then match exactly; only
//! diagnostic numbers may use a tolerance.
//!
//! Entry points: [`check::port_check`] (library), [`tool::run_json`] (JSON in/out for the
//! toolbox), and the `byteask-port-check` CLI.

pub mod book;
pub mod check;
pub mod model;
pub mod protocol;
pub mod session;
pub mod sim;
pub mod tool;

pub use check::PortCheckRequest;
pub use check::PortCheckResult;
pub use check::Status;
pub use check::port_check;

use std::path::Path;
use std::path::PathBuf;

/// The Python adapter, shipped inside the tool so a user never needs another download.
pub const PYTHON_ADAPTER: &str = include_str!("../adapters/port_harness.py");
/// The C++ adapter (header-only, C++17).
pub const CPP_ADAPTER: &str = include_str!("../adapters/port_harness.hpp");

/// Write both adapters into `dir` (created if needed). Returns the two paths.
pub fn write_adapters(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for (name, text) in [("port_harness.py", PYTHON_ADAPTER), ("port_harness.hpp", CPP_ADAPTER)] {
        let p = dir.join(name);
        std::fs::write(&p, text)?;
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
#[cfg(unix)]
mod check_tests;
