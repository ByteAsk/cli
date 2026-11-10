//! Tier B switches. Sources, highest priority first: environment variables, then the `[byteask]`
//! table of the effective config (`config.toml` or `-c byteask.attempts=off`). Tolerant loader:
//! a missing or malformed value keeps the default, numbers are clamped.
//!
//! | env | config key | default | meaning |
//! |---|---|---|---|
//! | `BYTEASK_ATTEMPTS` | `attempts` | off | `adaptive`, `off`, or a fixed total `N` (2-8) |
//! | `BYTEASK_ATTEMPTS_MAX` | `attempts_max` | 4 | hard cap on attempts, attempt 1 included (1-8) |
//! | `BYTEASK_ATTEMPTS_MAX_TOKENS` | `attempts_max_tokens` | 30000000 | hard cap on tokens of all attempt and judge sub-agents together |
//! | `BYTEASK_ATTEMPTS_CHILD_MAX_STEPS` | `attempts_child_max_steps` | 200 | model requests per attempt |
//! | `BYTEASK_TASK_TIMEOUT_SECS` | `task_timeout_secs` | unset | wall-clock budget of the whole session, from process start |
//! | `BYTEASK_ATTEMPTS_APPLY_MARGIN_SECS` | `attempts_apply_margin_secs` | 120 | lower bound of the margin kept for the final apply (the margin is at least 10% of the budget) |
//! | `BYTEASK_ATTEMPTS_MIN_SECS` | `attempts_min_secs` | 300 | a new attempt starts only with at least this much working time |
//! | `BYTEASK_ATTEMPTS_JUDGE` | `attempts_judge` | on | fresh-context pairwise judge when execution signals tie |
//! | `BYTEASK_ATTEMPTS_WRITER_GRACE_SECS` | `attempts_writer_grace_secs` | 300 | how long to wait for a still-running independent test writer after the root finished |

use serde::Serialize;
use toml::Value;

use crate::settings::number;
use crate::settings::truthy;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AttemptsMode {
    Off,
    /// Attempt 2 only on weak runtime evidence after attempt 1; more only while attempts
    /// disagree.
    Adaptive,
    /// Always run this many attempts in total (budget permitting).
    Fixed(usize),
}

impl AttemptsMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "1" | "false" | "no" | "none" => Some(Self::Off),
            "adaptive" | "auto" | "on" | "true" | "yes" => Some(Self::Adaptive),
            other => other
                .parse::<usize>()
                .ok()
                .filter(|n| (2..=8).contains(n))
                .map(Self::Fixed),
        }
    }

    pub fn as_str(self) -> String {
        match self {
            Self::Off => "off".to_string(),
            Self::Adaptive => "adaptive".to_string(),
            Self::Fixed(n) => n.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AttemptsSettings {
    /// `None`: not set anywhere (see [`Self::effective_mode`]).
    pub mode: Option<AttemptsMode>,
    pub max_attempts: usize,
    pub max_tokens: u64,
    pub child_max_steps: usize,
    pub task_timeout_secs: Option<u64>,
    pub apply_margin_secs: u64,
    pub min_attempt_secs: u64,
    pub judge: bool,
    pub writer_grace_secs: u64,
}

impl Default for AttemptsSettings {
    fn default() -> Self {
        Self {
            mode: None,
            max_attempts: 4,
            max_tokens: 30_000_000,
            child_max_steps: 200,
            task_timeout_secs: None,
            apply_margin_secs: 120,
            min_attempt_secs: 300,
            judge: true,
            writer_grace_secs: 300,
        }
    }
}

const KEYS: &[(&str, &str)] = &[
    ("BYTEASK_ATTEMPTS", "attempts"),
    ("BYTEASK_ATTEMPTS_MAX", "attempts_max"),
    ("BYTEASK_ATTEMPTS_MAX_TOKENS", "attempts_max_tokens"),
    (
        "BYTEASK_ATTEMPTS_CHILD_MAX_STEPS",
        "attempts_child_max_steps",
    ),
    ("BYTEASK_TASK_TIMEOUT_SECS", "task_timeout_secs"),
    (
        "BYTEASK_ATTEMPTS_APPLY_MARGIN_SECS",
        "attempts_apply_margin_secs",
    ),
    ("BYTEASK_ATTEMPTS_MIN_SECS", "attempts_min_secs"),
    ("BYTEASK_ATTEMPTS_JUDGE", "attempts_judge"),
    (
        "BYTEASK_ATTEMPTS_WRITER_GRACE_SECS",
        "attempts_writer_grace_secs",
    ),
];

fn toml_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

impl AttemptsSettings {
    /// Resolve from a lookup over `(env var, config key)`.
    pub fn resolve(lookup: impl Fn(&str, &str) -> Option<String>) -> Self {
        let mut s = Self::default();
        for (env, key) in KEYS {
            let Some(raw) = lookup(env, key) else {
                continue;
            };
            match *key {
                "attempts" => {
                    if let Some(mode) = AttemptsMode::parse(&raw) {
                        s.mode = Some(mode);
                    }
                }
                "attempts_max" => {
                    if let Some(n) = number(&raw, 1, 8) {
                        s.max_attempts = n as usize;
                    }
                }
                "attempts_max_tokens" => {
                    if let Some(n) = number(&raw, 100_000, 1_000_000_000) {
                        s.max_tokens = n;
                    }
                }
                "attempts_child_max_steps" => {
                    if let Some(n) = number(&raw, 5, 10_000) {
                        s.child_max_steps = n as usize;
                    }
                }
                "task_timeout_secs" => {
                    if let Some(n) = number(&raw, 60, 7 * 86_400) {
                        s.task_timeout_secs = Some(n);
                    }
                }
                "attempts_apply_margin_secs" => {
                    if let Some(n) = number(&raw, 30, 3_600) {
                        s.apply_margin_secs = n;
                    }
                }
                "attempts_min_secs" => {
                    if let Some(n) = number(&raw, 30, 7_200) {
                        s.min_attempt_secs = n;
                    }
                }
                "attempts_judge" => {
                    if let Some(b) = truthy(&raw) {
                        s.judge = b;
                    }
                }
                "attempts_writer_grace_secs" => {
                    if let Some(n) = number(&raw, 0, 3_600) {
                        s.writer_grace_secs = n;
                    }
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

    /// Off unless set, in every kind of session. The attempts run after the agent's final
    /// message and print nothing while they run, so an `exec` caller saw the process sit idle
    /// for many minutes after the finish gate.
    pub fn effective_mode(&self) -> AttemptsMode {
        self.mode.unwrap_or(AttemptsMode::Off)
    }

    /// Total attempts allowed in `mode`, attempt 1 included.
    pub fn cap(&self, mode: AttemptsMode) -> usize {
        match mode {
            AttemptsMode::Off => 1,
            AttemptsMode::Adaptive => self.max_attempts,
            AttemptsMode::Fixed(n) => n.min(self.max_attempts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn from(pairs: &[(&str, &str)]) -> AttemptsSettings {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        AttemptsSettings::resolve(|_env, key| map.get(key).cloned())
    }

    #[test]
    fn modes_parse_and_attempts_are_off_unless_set() {
        assert_eq!(
            AttemptsMode::parse("adaptive"),
            Some(AttemptsMode::Adaptive)
        );
        assert_eq!(AttemptsMode::parse(" OFF "), Some(AttemptsMode::Off));
        assert_eq!(AttemptsMode::parse("1"), Some(AttemptsMode::Off));
        assert_eq!(AttemptsMode::parse("3"), Some(AttemptsMode::Fixed(3)));
        assert_eq!(AttemptsMode::parse("9"), None);
        assert_eq!(AttemptsMode::parse("lots"), None);
        assert_eq!(
            AttemptsSettings::default().effective_mode(),
            AttemptsMode::Off
        );
        let off = from(&[("attempts", "off")]);
        assert_eq!(off.effective_mode(), AttemptsMode::Off);
        let on = from(&[("attempts", "adaptive")]);
        assert_eq!(on.effective_mode(), AttemptsMode::Adaptive);
        // A malformed value keeps the default.
        assert_eq!(
            from(&[("attempts", "lots")]).effective_mode(),
            AttemptsMode::Off
        );
    }

    #[test]
    fn caps_and_clamps() {
        let s = from(&[
            ("attempts", "6"),
            ("attempts_max", "3"),
            ("attempts_max_tokens", "5"),
            ("task_timeout_secs", "5400"),
            ("attempts_judge", "no"),
            ("attempts_child_max_steps", "x"),
        ]);
        assert_eq!(
            s.cap(AttemptsMode::Fixed(6)),
            3,
            "the hard cap wins over a fixed count"
        );
        assert_eq!(s.cap(AttemptsMode::Adaptive), 3);
        assert_eq!(s.cap(AttemptsMode::Off), 1);
        assert_eq!(s.max_tokens, 100_000);
        assert_eq!(s.task_timeout_secs, Some(5400));
        assert!(!s.judge);
        assert_eq!(
            s.child_max_steps,
            AttemptsSettings::default().child_max_steps
        );
    }

    #[test]
    fn env_wins_over_config() {
        let table: Value = toml::from_str("attempts = \"off\"\nattempts_max = 2\n")
            .unwrap_or(Value::Boolean(false));
        let s = AttemptsSettings::resolve(|env, key| {
            if env == "BYTEASK_ATTEMPTS" {
                return Some("adaptive".to_string());
            }
            table.get(key).and_then(toml_to_string)
        });
        assert_eq!(s.mode, Some(AttemptsMode::Adaptive));
        assert_eq!(s.max_attempts, 2);
    }
}
