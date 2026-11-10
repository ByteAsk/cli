//! The merge check.
//!
//! Where a team's standard is enforced is the merge button, not the AI's own hook. Three CI
//! steps, three commands:
//!
//! 1. `byteask checks run` (the job that runs the branch's code, with no signing key):
//!    reads the profile and the expected-test manifest from the protected base branch,
//!    checks out head merged into base, runs the golden-path build and tests and the
//!    slow checks for marked paths, and writes a report bound to that exact candidate.
//! 2. `byteask checks sign` (a job that runs nothing from the branch and holds the runner key):
//!    recomputes the candidate from git objects, refuses a report for anything else, signs it
//!    with the runner's enrolled Ed25519 key and records it with the enterprise server so
//!    a later retirement keeps it valid. Bench machines sign their perf results the same
//!    way.
//! 3. `byteask checks gate`: verifies every signed result with the enterprise server, ignores
//!    anything unsigned, edited, from an unenrolled, revoked or wrong-kind key, or for another
//!    version, decides (PASS / FAIL / NOT RUN, human approval of the reasons the profile
//!    allows), and posts it to GitHub, GitLab or Jenkins with the perf card.
//!
//! It never reads the developer's local results: those only speed up the local loop.

pub mod binding;
pub mod cli;
pub mod gate;
mod git;
pub mod human;
pub mod junit;
pub mod perf;
pub mod platform;
pub mod run;
pub mod sign;
pub mod step;
pub mod verify;

pub use binding::Binding;
pub use binding::Candidate;
pub use gate::Decision;

#[cfg(test)]
mod testkit;
