//! The one guidance paragraph the model gets when this workspace has an index
//! (model-facing text, pinned by `description_snapshot_tests`).
//! Static on purpose: no commit or setting in it, so it never changes the
//! cached prompt prefix between commits.

pub const CONTEXT_GUIDANCE: &str = "\
BUILD-AWARE INDEX: this project has a ByteAsk index built from its real compile commands, one per \
build setting. To find where a C/C++ symbol is defined or used, which template versions a build \
really compiles, whether an #if branch is live, or how a file is compiled, call the context_* \
tools before grepping or reading headers by hand: they answer per build setting and mark answers \
that are only approximate. For a bug report, issue or change request, call context_bundle first with \
the full task text (example code, errors and stack traces included): it says where the change most \
likely belongs and why, with the code, what it calls, what calls it and the tests that exercise it \
(context_pack is the older, smaller version). context_find, context_hierarchy and context_calls answer \
exact definition, inheritance and call questions, overloads and virtual calls included; \
context_example shows what a piece of example code really executes in this project; after a fix \
the tests confirm, record the lesson with context_remember. Every answer names its build setting \
and commit; when it says the index is behind your worktree, read the edited files themselves before \
relying on it.";
