mod ablation;
mod args;
mod base_run_cache;
mod builds;
pub mod cancel;
mod corefile;
mod detect;
mod envnote;
mod extension;
mod finding;
mod gate;
mod gdb_common;
mod gdbmi;
mod gitx;
mod guidance;
mod helper;
pub mod hooks;
mod impact;
mod lldb_guard;
mod lsp;
mod memsafety;
mod offline;
mod priority;
mod probe;
#[cfg(test)]
mod probe_driver;
mod program_args;
#[cfg(test)]
mod replay_driver;
mod report;
mod resources;
mod results;
mod results_store;
mod run;
mod runtime_extension;
mod sandbox;
mod session_head;
pub mod skip;
mod slowcard;
mod stats;
#[cfg(test)]
mod test_support;
mod testsel;
mod tools;
mod toolver;
mod trust;
mod workflow;

pub use detect::is_cpp_workspace;
pub use extension::install;
pub use extension::tools_gated;
pub use gitx::primary_root as primary_repo_root;
pub use memsafety::critique_patch;
pub use memsafety::root_cause_evidence;
pub use resources::BuildSlot;
pub use resources::build_slot;
pub use resources::jobs as memory_aware_jobs;
/// The warm build for a task whose likely place is known but no test programs are: it picks
/// the tests covering the place and offers one command that builds and runs them
/// (`WarmHandle::check_plan`).
pub use testsel::check::CheckPlan;
pub use testsel::check::place_files as context_place_files;
/// The background warm build: `start_warm_build(workspace, test_programs) -> WarmHandle`.
/// Returns at once; builds the named test programs (CMake target/executable names, program
/// paths, test sources or CTest names; unknown names ignored; none = configure only) in the
/// tree `cpp_finish_check` and `cpp_test_select` build in, under its per-tree lock (taken
/// before returning when free, so a check started later waits and stays incremental), the
/// session build slot, the memory-aware job count and background priority. Failures are logged
/// and ignored. Call it only when [`native_exec_allowed`] holds; dropping the handle does not
/// stop the build (`WarmHandle::cancel` does).
pub use testsel::warm::WarmHandle;
pub use testsel::warm::WarmOutcome;
pub use testsel::warm::start_warm_build;
pub use testsel::warm::start_warm_build_for;
/// Optional switch `BYTEASK_FINISH_REVIEW_OWN_TESTS`: core's agent-command hook.
pub use tools::note_agent_command;
/// The session runs with full permission (approval `never`, no sandbox).
pub use trust::full_access;
pub use trust::native_exec_allowed;

/// Where the finish gate appends one JSON line per run (`{"turn", "round", "problems",
/// "notes", "still_open", "not_checked"}`) for the repository whose top-level directory is
/// `root`. Read by the sub-agents extension (Tier B) to learn the gate's last verdict.
pub fn finish_gate_runs_log(root: &std::path::Path) -> std::path::PathBuf {
    gate::scratch_for(root).join("gate-runs.jsonl")
}
