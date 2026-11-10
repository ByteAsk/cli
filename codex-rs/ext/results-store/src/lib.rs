//! The local results store: every ByteAsk check result, recorded by change,
//! and the local value report built from it.
//!
//! # What a change is
//!
//! One session's work on one branch of one repository: `(repo, branch,
//! session)`, from the AI's first edit until the change is pushed. A session
//! is one agent thread (one TUI conversation, one `byteask exec`, one
//! toolbox process serving Claude Code). Within a change, the **version** of
//! the code a result belongs to is a fingerprint of the workspace taken when
//! the check started ([`workspace::Probe::version`]): the commit, plus the
//! content of every changed and new file.
//!
//! # On disk (`$BYTEASK_HOME/results/`, mode 0700)
//!
//! ```text
//! changes/<key>-<n>.jsonl        the evidence log of one change, append-only, one JSON record per line
//! changes/locks/<xx>.lock        striped writer locks (see [`changes`])
//! payloads/<key>-<n>/<name>.txt  the full text of a tool's answer (disposable)
//! <project-key>/index.json       the newest finish-gate verdicts of one project, with their
//!                                freshness fingerprints (read by check_status
//!                                and the prompt hook; see [`index`])
//! <project-key>/sessions/        the agent hooks' per-session state (cpp-tools' hooks own it)
//! gc.lock, gc.stamp              clean-up lock and last clean-up time
//! ```
//!
//! Evidence records ([`store::Record`]) are small: `open` (repo, branch,
//! session, starting commit), `base` (the code before the AI changed it),
//! `edit` (the AI changed files), `result` (one
//! [`check_result::CheckResult`] and the version it checked), `close` (pushed,
//! session ended, or idle 30 days), `payload_evicted`, `capped`. Writers of one
//! session (the engine or the toolbox, and the agent's hooks) serialise on the
//! change's lock; the clean-up only appends `close` / `payload_evicted` records.
//!
//! # Keeping
//!
//! * Evidence of a change is kept while the change is open. A change closes
//!   when it is pushed, when its session ends, or after 30 days idle.
//! * A closed change is deleted [`policy`'s `max_days`](byteask_policy::ResultsStoreLimits)
//!   (default 90) after its last activity; at most [`store::MAX_CLOSED_CHANGES`]
//!   closed changes are kept (oldest go first).
//! * Payloads stay under `max_bytes` (default 2 GB): closed changes lose theirs
//!   first, oldest first; if open changes' payloads alone are over the cap,
//!   the oldest open change loses its payloads and its evidence stays, marked
//!   `payload_evicted`.
//! * Nothing an open change refers to is ever deleted except those payloads.
//!
//! # The report
//!
//! [`report::ValueReport`] reads only this store, counts only what was
//! recorded, and labels every count local (CI counts are
//! never added to these). It carries no source text: no paths, no branch
//! names, no summaries or payloads, only check names, verdicts, reasons,
//! counts, durations and measured ratios.

pub mod changes;
pub mod index;
pub mod recorder;
pub mod report;
pub mod store;
pub mod workspace;

#[cfg(test)]
mod test_support;

// The index interface (check_status, the prompt hook, the merge check),
// unchanged names and formats.
pub use index::Entry;
pub use index::Freshness;
pub use index::all;
pub use index::describe;
pub use index::fingerprint;
pub use index::freshness;
pub use index::latest;
pub use index::project_key;
pub use index::record;
pub use index::record_for_session;
pub use recorder::Recorded;
pub use recorder::Settings;
pub use recorder::ThreadRecorder;
pub use recorder::install;
pub use report::ValueReport;

/// The checks whose verdict judges the code of a change. Their results decide
/// "problems caught" and "review-ready"; every other ByteAsk tool's results
/// are counted but cannot make a version pass or fail.
///
/// `cpp_nav` (lookups as well as clangd diagnostics), `cpp_compile_db` (finds
/// the build setup), `cpp_godbolt` / `cpp_insights` (snippets), measurements
/// and debuggers are not checks of the change.
pub const CODE_CHECKS: [&str; 5] = [
    "cpp_finish_gate",
    "cpp_check",
    "cpp_tidy",
    "cpp_sanitize",
    "cpp_format",
];

/// An agent session (Claude Code, Codex, through their hooks) started in `cwd`: the code as
/// it is now is the starting point of its change. A resumed session keeps the one it
/// started with. Best effort.
pub fn begin_agent_session(home: &std::path::Path, session: &str, cwd: &std::path::Path) {
    let top = workspace::toplevel(cwd, workspace::PROBE_BUDGET);
    let probe = workspace::probe(cwd, top.as_deref(), workspace::PROBE_BUDGET);
    if probe.git {
        let _ = changes::begin_session(home, session, &probe, now_unix());
    }
}

/// An agent changed files in `cwd`, at the latest by `at` (its hooks see the change only when
/// the turn ends, so they pass the time the turn started). Best effort.
pub fn record_agent_edit(home: &std::path::Path, session: &str, cwd: &std::path::Path, at: u64) {
    let top = workspace::toplevel(cwd, workspace::PROBE_BUDGET);
    let probe = workspace::probe(cwd, top.as_deref(), workspace::PROBE_BUDGET);
    if probe.git {
        let _ = changes::append(home, session, &probe, at, changes::Event::Edit);
    }
}

/// Unix seconds now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
