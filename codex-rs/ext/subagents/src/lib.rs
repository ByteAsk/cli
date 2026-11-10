//! ByteAsk sub-agent features.
//!
//! The independent reproduction-test writer: at session start in a C/C++ workspace with an
//! issue-style task, a fresh-context sub-agent writes 1-3 tests in the project's own framework,
//! in a worktree outside the repository. Tests that fail on the starting code are frozen and
//! delivered to the main agent as a message; they never stay in its diff.
//!
//! Off by default: enable it with `BYTEASK_REPRO_TESTS=1` or the `[byteask]` config key
//! `repro_tests = true`; `BYTEASK_REPRO_TESTS=0` switches it off again (see [`settings`] for why
//! it is opt-in).
//!
//! Tier B, adaptive multi-attempt with execution-based selection ([`attempts`]): when the root
//! session ends with weak runtime evidence, further independent attempts run one after another
//! in worktrees outside the repository, and the selected attempt's change is put into the real
//! tree. `BYTEASK_ATTEMPTS` / `[byteask] attempts = adaptive | off | N`.

pub mod attempts;
pub mod ccache;
pub mod evaluate;
mod extension;
pub mod gitops;
pub mod host;
pub mod orchestrator;
pub mod prompts;
pub mod report;
pub mod runner;
pub mod settings;
pub mod spawner;

pub use extension::install;
pub use extension::is_build_or_test_command;
