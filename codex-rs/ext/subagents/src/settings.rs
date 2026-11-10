//! Switches for the independent reproduction-test writer. OFF by default: it adds
//! noticeably to the cost of a session, a provider can refuse the writer's brief
//! (mostly on security fixes), and on tasks whose instructions forbid
//! paths outside the repository the main agent never runs its tests. Enable it with
//! `BYTEASK_REPRO_TESTS=1` (or `-c byteask.repro_tests=true`); `BYTEASK_REPRO_TESTS=0` stays the
//! kill switch. When enabled it still starts only in a git work tree with native execution
//! allowed, in a C/C++ workspace, for an issue-style request, and only in root threads.
//!
//! Sources, highest priority first:
//! 1. environment variables (`BYTEASK_REPRO_TESTS`, ...);
//! 2. the `[byteask]` table of the effective config (config.toml or `-c byteask.repro_tests=true`).
//!
//! The loader is tolerant: a missing or malformed value falls back to the default for that
//! field, and every number is clamped to a safe range.

use std::path::PathBuf;

use toml::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Run the independent reproduction-test writer. Default off (opt in).
    pub repro_tests: bool,
    /// Wall-clock cap for the reproduction-test writer.
    pub repro_deadline_secs: u64,
    /// Model-request cap for the reproduction-test writer.
    pub repro_max_steps: usize,
    /// Budget for all sub-agent work of one session, from the start of the root's first turn.
    pub deadline_secs: u64,
    /// Engine-run builds/test runs plus sub-agent build/test commands allowed at once.
    pub max_parallel_runs: usize,
    pub build_timeout_secs: u64,
    pub test_timeout_secs: u64,
    /// Explicit build command (run from a worktree root) when a sub-agent reports none.
    pub build_cmd: Option<String>,
    /// Parent directory for worktrees and scratch files (default: the system temp dir).
    pub scratch_root: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            repro_tests: false,
            repro_deadline_secs: 900,
            repro_max_steps: 60,
            deadline_secs: 3600,
            max_parallel_runs: 2,
            build_timeout_secs: 1200,
            test_timeout_secs: 900,
            build_cmd: None,
            scratch_root: None,
        }
    }
}

/// `(env var, config key)` for every knob.
const KEYS: &[(&str, &str)] = &[
    ("BYTEASK_REPRO_TESTS", "repro_tests"),
    ("BYTEASK_REPRO_DEADLINE_SECS", "repro_deadline_secs"),
    ("BYTEASK_REPRO_MAX_STEPS", "repro_max_steps"),
    ("BYTEASK_SUBAGENT_DEADLINE_SECS", "subagent_deadline_secs"),
    ("BYTEASK_SUBAGENT_MAX_PARALLEL", "subagent_max_parallel"),
    ("BYTEASK_SUBAGENT_BUILD_TIMEOUT_SECS", "subagent_build_timeout_secs"),
    ("BYTEASK_SUBAGENT_TEST_TIMEOUT_SECS", "subagent_test_timeout_secs"),
    ("BYTEASK_SUBAGENT_BUILD_CMD", "subagent_build_cmd"),
    ("BYTEASK_SUBAGENT_SCRATCH", "subagent_scratch"),
];

pub(crate) fn truthy(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

pub(crate) fn number(value: &str, min: u64, max: u64) -> Option<u64> {
    value.trim().parse::<u64>().ok().map(|n| n.clamp(min, max))
}

fn toml_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

impl Settings {
    /// Resolve from a lookup function over `(env var, config key)`.
    pub fn resolve(lookup: impl Fn(&str, &str) -> Option<String>) -> Self {
        let mut s = Self::default();
        for (env, key) in KEYS {
            let Some(raw) = lookup(env, key) else {
                continue;
            };
            match *key {
                "repro_tests" => {
                    if let Some(b) = truthy(&raw) {
                        s.repro_tests = b;
                    }
                }
                "repro_deadline_secs" => {
                    if let Some(n) = number(&raw, 30, 7_200) {
                        s.repro_deadline_secs = n;
                    }
                }
                "repro_max_steps" => {
                    if let Some(n) = number(&raw, 2, 1_000) {
                        s.repro_max_steps = n as usize;
                    }
                }
                "subagent_deadline_secs" => {
                    if let Some(n) = number(&raw, 120, 86_400) {
                        s.deadline_secs = n;
                    }
                }
                "subagent_max_parallel" => {
                    if let Some(n) = number(&raw, 1, 8) {
                        s.max_parallel_runs = n as usize;
                    }
                }
                "subagent_build_timeout_secs" => {
                    if let Some(n) = number(&raw, 10, 14_400) {
                        s.build_timeout_secs = n;
                    }
                }
                "subagent_test_timeout_secs" => {
                    if let Some(n) = number(&raw, 10, 14_400) {
                        s.test_timeout_secs = n;
                    }
                }
                "subagent_build_cmd" => {
                    s.build_cmd = Some(raw).filter(|v| !v.trim().is_empty());
                }
                "subagent_scratch" => {
                    s.scratch_root = Some(PathBuf::from(raw)).filter(|p| p.is_absolute());
                }
                _ => {}
            }
        }
        s
    }

    /// Environment first, then the `[byteask]` table of the effective config.
    pub fn from_env_and_config(byteask_table: Option<&Value>) -> Self {
        Self::resolve(|env, key| {
            std::env::var(env).ok().or_else(|| {
                byteask_table
                    .and_then(|table| table.get(key))
                    .and_then(toml_to_string)
            })
        })
    }

    pub fn any_enabled(&self) -> bool {
        self.repro_tests
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn from(pairs: &[(&str, &str)]) -> Settings {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Settings::resolve(|_env, key| map.get(key).cloned())
    }

    #[test]
    fn default_is_off_config_enables_it_and_the_kill_switch_turns_it_off() {
        let s = Settings::resolve(|_, _| None);
        assert_eq!(s, Settings::default());
        assert!(!s.repro_tests && !s.any_enabled());
        for on in ["1", "true", "on", "yes"] {
            let s = from(&[("repro_tests", on)]);
            assert!(s.repro_tests && s.any_enabled(), "{on}");
        }
        for off in ["0", "false", "off", "no"] {
            let s = from(&[("repro_tests", off)]);
            assert!(!s.repro_tests && !s.any_enabled(), "{off}");
        }
        let env_on = Settings::resolve(|env, _| (env == "BYTEASK_REPRO_TESTS").then(|| "1".to_string()));
        assert!(env_on.any_enabled());
        let env_off = Settings::resolve(|env, _| (env == "BYTEASK_REPRO_TESTS").then(|| "0".to_string()));
        assert!(!env_off.any_enabled());
    }

    #[test]
    fn numbers_are_clamped_and_bad_values_ignored() {
        let s = from(&[
            ("subagent_max_parallel", "99"),
            ("repro_deadline_secs", "5"),
            ("subagent_test_timeout_secs", "abc"),
            ("repro_tests", "yes"),
            ("subagent_build_cmd", "  "),
            ("subagent_scratch", "relative/dir"),
        ]);
        assert_eq!(s.max_parallel_runs, 8);
        assert_eq!(s.repro_deadline_secs, 30);
        assert_eq!(s.test_timeout_secs, Settings::default().test_timeout_secs);
        assert!(s.repro_tests);
        assert!(s.any_enabled());
        assert_eq!(s.build_cmd, None);
        assert_eq!(s.scratch_root, None);
    }

    #[test]
    fn config_table_is_read() {
        let table: Value =
            toml::from_str("repro_tests = true\nrepro_max_steps = 30\nsubagent_deadline_secs = 900\n")
                .unwrap_or(Value::Boolean(false));
        let s = Settings::resolve(|_env, key| table.get(key).and_then(toml_to_string));
        assert!(s.repro_tests);
        assert_eq!(s.repro_max_steps, 30);
        assert_eq!(s.deadline_secs, 900);
    }
}
