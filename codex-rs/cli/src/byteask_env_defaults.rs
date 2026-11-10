//! ByteAsk harness defaults read from the environment (0.2.4).
//!
//! The C/C++ tools, the finish check and the Context calls read these switches from the
//! environment. Each one that is not set when the engine starts gets the value below, so every
//! session runs with these defaults unless the user sets the variable (any value, `0` included) to override
//! it. They are set once at process start, before any thread exists, and the agent's commands
//! inherit them like the rest of the environment.

/// `(variable, default)`. Not here on purpose: `BYTEASK_FINISH_REVIEW_LOG` (a diagnostics log),
/// `BYTEASK_PROBE_JOBS` and `BYTEASK_BACKGROUND_JOBS` (fixed job counts for one machine size; the
/// environment probe derives the count from the CPUs and the memory limit instead).
pub(crate) const DEFAULTS: &[(&str, &str)] = &[
    // The agent's commands start at oom_score_adj 500 and the engine's background builds at 1000:
    // under memory pressure the background build dies first, then the command, never the engine.
    ("BYTEASK_CHILD_OOM_SCORE_ADJ", "500"),
    ("BYTEASK_BACKGROUND_OOM_SCORE_ADJ", "1000"),
    // Background builds run in the SCHED_IDLE class: they use only CPU the agent leaves idle.
    ("BYTEASK_BACKGROUND_IDLE", "1"),
    // The environment probe states the memory limit and derives the job count from it.
    ("BYTEASK_PROBE_MEMORY", "1"),
    // cpp_finish_check: its own CMake configure keeps the project's test options on.
    ("BYTEASK_FINISH_CHECK_CFG2", "1"),
    // cpp_finish_check: build and run the nearest tests' programs, name the others.
    ("BYTEASK_FINISH_CHECK_NEAR_ONLY", "1"),
    // A check stops a warm build still running in its tree instead of waiting for all of it.
    ("BYTEASK_FINISH_CHECK_WARM_YIELD", "1"),
    // A change no compiled file reaches is "not checked", never PASS.
    ("BYTEASK_FINISH_CHECK_UNCOVERED", "1"),
    ("BYTEASK_FINISH_CHECK_SECS", "120"),
    // The automatic review of an unchecked final change: its own time limit, and the agent's own
    // passing test run on that change counts.
    ("BYTEASK_FINISH_REVIEW_SECS", "90"),
    ("BYTEASK_FINISH_REVIEW_OWN_TESTS", "1"),
    // cpp_test_select blocks at most 120 s by default (300 s when asked); the run goes on in the
    // background. It stops a warm build still running in its tree.
    ("BYTEASK_TEST_SELECT_WAIT_S", "120"),
    ("BYTEASK_TEST_SELECT_MAX_WAIT_S", "300"),
    ("BYTEASK_TEST_SELECT_WARM_YIELD", "1"),
    // Context's related places: wait up to 40 s, plus 5 s for an answer that is ready but not yet
    // processed; the check's synchronous work runs off the async workers meanwhile.
    ("BYTEASK_RELATED_TIMEOUT_SECS", "40"),
    ("BYTEASK_RELATED_GRACE_SECS", "5"),
    ("BYTEASK_CHECK_OFFLOAD", "1"),
    // The check's answer puts the related places right after the verdict, at most 12 notes.
    ("BYTEASK_CHECK_RELATED_FIRST", "1"),
];

/// The defaults the environment `get` does not set, in [`DEFAULTS`] order.
pub(crate) fn missing(
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(&'static str, &'static str)> {
    DEFAULTS
        .iter()
        .filter(|(name, _)| get(name).is_none())
        .copied()
        .collect()
}

/// Sets every unset variable of [`DEFAULTS`].
///
/// Must run at process start, before any thread is spawned.
pub(crate) fn apply() {
    for (name, value) in missing(|name| std::env::var_os(name)) {
        // SAFETY: called synchronously at the top of `main`, before any thread exists.
        unsafe { std::env::set_var(name, value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    #[test]
    fn unset_variables_get_the_default_and_set_ones_are_kept() {
        let env: HashMap<&str, OsString> = HashMap::from([
            ("BYTEASK_CHECK_OFFLOAD", OsString::from("0")),
            ("BYTEASK_FINISH_CHECK_SECS", OsString::from("")),
        ]);
        let missing = missing(|name| env.get(name).cloned());
        assert_eq!(missing.len(), DEFAULTS.len() - 2);
        assert!(missing.iter().all(|(name, _)| !env.contains_key(name)));
        assert!(missing.contains(&("BYTEASK_CHECK_RELATED_FIRST", "1")));
    }

    #[test]
    fn every_default_is_a_byteask_variable_named_once() {
        let mut names: Vec<&str> = DEFAULTS.iter().map(|(name, _)| *name).collect();
        assert!(names.iter().all(|name| name.starts_with("BYTEASK_")));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DEFAULTS.len());
        assert!(!names.contains(&"BYTEASK_PROBE_JOBS"));
        assert!(!names.contains(&"BYTEASK_FINISH_REVIEW_LOG"));
    }
}
