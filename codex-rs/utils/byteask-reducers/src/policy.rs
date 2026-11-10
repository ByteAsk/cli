//! The server-driven switchboard.
//!
//! Lives as a top-level `byteask_efficiency` object in the model catalog JSON. It is read
//! here by a tolerant, field-by-field loader, deliberately NOT as part of the engine's
//! typed catalog structs: a malformed policy must never be able to fail the catalog parse
//! (that would stop the engine from starting). Missing or wrong -> the built-in default for THAT field.
//! Every number is clamped to a safe range.
//!
//! Defaults are conservative: only the two terminal-equivalent transforms are on.
//! A lower output budget is OFF until the policy turns it on, and it is only ever applied
//! when the original could be spilled.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use serde_json::Value;

pub const POLICY_KEY: &str = "byteask_efficiency";
const MIN_BUDGET_TOKENS: u64 = 1_000;
const MAX_BUDGET_TOKENS: u64 = 10_000;

/// Class L beyond `ansi`/`cr`: keep every fact, fire only on a positive family match. ON.
pub const LOSSLESS_REDUCERS: &[&str] = &["rle", "ws", "search-group", "path-fold"];

/// Class N and G: shipped dark, switched on one by one from the policy object.
pub const DARK_REDUCERS: &[&str] = &[
    "json-min", "progress", "pkg", "cmake-conf", "tests", "warn-group", "incl-chain",
    "cxx-templates", "cxx-candidates", "link-group", "stack-fold", "json-array", "gen-diff",
    "vendor-ls", "html-text", "blob-line", "table", "log-prefix",
];

/// Budget keys the policy may set (tokens). `exec` is the fallback for every family.
pub const BUDGET_KEYS: &[&str] = &["exec", "read", "search", "listing", "diff", "build_test", "other"];

/// Behavioural switches (class B), all OFF unless the policy turns them on.
pub const FLAG_KEYS: &[&str] = &[
    "compact_header", "loop_nudge", "patch_echo", "reread_stub", "native_read",
    "native_search", "compact_with_tools", "compact_working_set", "cold_slim",
];

/// Numeric knobs with their (min, max) clamp.
pub const NUMBER_KEYS: &[(&str, u64, u64)] = &[
    ("first_yield_ms", 10_000, 300_000),
    // "Wait instead of poll": floors for commands started without a TTY (see
    // `core/src/unified_exec/mod.rs`). The low ends equal upstream's own floors.
    ("empty_poll_min_ms", 5_000, 300_000),
    ("first_yield_min_ms", 250, 30_000),
    ("compaction_headroom_tokens", 0, 64_000),
    ("cold_after_secs", 300, 172_800),
    ("cold_protect_tokens", 8_000, 200_000),
    ("compact_keep_pairs", 0, 6),
    ("compact_keep_bytes", 2_000, 48_000),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub version: String,
    /// Strip ANSI escapes from command output.
    pub ansi: bool,
    /// Collapse bare carriage-return overwrites.
    pub cr: bool,
    /// Token budget for one command output, or `None` for the engine's own (upstream) budget.
    /// Only honoured together with a successful spill of the original.
    pub exec_budget_tokens: Option<usize>,
    /// Emergency brake: everything off except what is byte-preserving.
    pub brake: bool,
    /// Names of the enabled reducers beyond `ansi`/`cr`.
    pub enabled: BTreeSet<String>,
    /// Per-family token budgets, already clamped.
    pub budgets: BTreeMap<String, usize>,
    /// Behavioural switches that are ON.
    pub flags: BTreeSet<String>,
    /// Numeric knobs, already clamped.
    pub numbers: BTreeMap<String, u64>,
    /// `(model, reasoning effort)` for the built-in read-only `explorer` subagent, or
    /// `None` to leave that role exactly as upstream ships it (same model as the parent).
    pub explorer: Option<(String, String)>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: "builtin".to_string(),
            ansi: true,
            cr: true,
            exec_budget_tokens: None,
            brake: false,
            enabled: LOSSLESS_REDUCERS.iter().map(|s| (*s).to_string()).collect(),
            budgets: BTreeMap::new(),
            flags: BTreeSet::new(),
            numbers: BTreeMap::new(),
            explorer: None,
        }
    }
}

/// A model slug is copied into a TOML document, so it is restricted to slug characters.
fn is_model_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

impl Policy {
    /// Parse from the catalog's JSON text. Never fails: anything unusable falls back
    /// to the default for that field.
    pub fn from_catalog_json(text: &str) -> Self {
        let mut p = Self::default();
        let Ok(root) = serde_json::from_str::<Value>(text) else {
            return p;
        };
        let Some(obj) = root.get(POLICY_KEY).and_then(Value::as_object) else {
            return p;
        };
        if let Some(v) = obj.get("policy_version").and_then(Value::as_str) {
            p.version = v.chars().take(64).collect();
        }
        if let Some(r) = obj.get("reducers").and_then(Value::as_object) {
            if let Some(b) = r.get("ansi").and_then(Value::as_bool) {
                p.ansi = b;
            }
            if let Some(b) = r.get("cr").and_then(Value::as_bool) {
                p.cr = b;
            }
            for name in LOSSLESS_REDUCERS.iter().chain(DARK_REDUCERS.iter()) {
                match r.get(*name).and_then(Value::as_bool) {
                    Some(true) => {
                        p.enabled.insert((*name).to_string());
                    }
                    Some(false) => {
                        p.enabled.remove(*name);
                    }
                    None => {}
                }
            }
        }
        if let Some(b) = obj.get("budgets").and_then(Value::as_object) {
            for key in BUDGET_KEYS {
                if let Some(n) = b.get(*key).and_then(Value::as_u64) {
                    let clamped = n.clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS);
                    if let Ok(v) = usize::try_from(clamped) {
                        p.budgets.insert((*key).to_string(), v);
                    }
                }
            }
        }
        if let Some(f) = obj.get("flags").and_then(Value::as_object) {
            for key in FLAG_KEYS {
                if f.get(*key).and_then(Value::as_bool) == Some(true) {
                    p.flags.insert((*key).to_string());
                }
            }
        }
        if let Some(n) = obj.get("numbers").and_then(Value::as_object) {
            for (key, lo, hi) in NUMBER_KEYS {
                if let Some(v) = n.get(*key).and_then(Value::as_u64) {
                    p.numbers.insert((*key).to_string(), v.clamp(*lo, *hi));
                }
            }
        }
        if let Some(n) = obj
            .get("budgets")
            .and_then(Value::as_object)
            .and_then(|b| b.get("exec"))
            .and_then(Value::as_u64)
        {
            let clamped = n.clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS);
            p.exec_budget_tokens = usize::try_from(clamped).ok();
        }
        if let Some(e) = obj.get("explorer").and_then(Value::as_object) {
            let model = e.get("model").and_then(Value::as_str).unwrap_or("");
            let effort = e.get("effort").and_then(Value::as_str).unwrap_or("low");
            if is_model_slug(model) && matches!(effort, "minimal" | "low" | "medium") {
                p.explorer = Some((model.to_string(), effort.to_string()));
            }
        }
        if let Some(b) = obj.get("brake").and_then(Value::as_bool) {
            p.brake = b;
        }
        if p.brake {
            p.apply_brake();
        }
        p
    }

    fn apply_brake(&mut self) {
        self.brake = true;
        self.ansi = false;
        self.cr = false;
        self.exec_budget_tokens = None;
        self.enabled.clear();
        self.budgets.clear();
        self.flags.clear();
        self.numbers.clear();
        self.explorer = None;
    }

    pub fn reducer_on(&self, name: &str) -> bool {
        !self.brake && self.enabled.contains(name)
    }

    pub fn flag(&self, name: &str) -> bool {
        !self.brake && self.flags.contains(name)
    }

    pub fn number(&self, name: &str) -> Option<u64> {
        if self.brake { None } else { self.numbers.get(name).copied() }
    }

    /// Token budget for a family: its own key, else `exec`. `None` = upstream's budget.
    pub fn budget_for(&self, family: crate::family::Family) -> Option<usize> {
        if self.brake {
            return None;
        }
        self.budgets
            .get(family.budget_key())
            .or_else(|| self.budgets.get("exec"))
            .copied()
            .or(self.exec_budget_tokens)
    }

    pub fn from_catalog_file(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_catalog_json(&text),
            Err(_) => Self::default(),
        }
    }
}

static ACTIVE: OnceLock<Policy> = OnceLock::new();

/// Install the process-wide policy once, at startup. Later calls are ignored.
pub fn install(policy: Policy) {
    let _ = ACTIVE.set(policy);
}

/// Set by the gateway's emergency-brake response header. Process-wide and one-way for
/// the life of the process: a running session stops reducing at once, with no restart.
static BRAKE: AtomicBool = AtomicBool::new(false);

pub fn pull_brake() {
    BRAKE.store(true, Ordering::Relaxed);
}

pub fn brake_pulled() -> bool {
    BRAKE.load(Ordering::Relaxed)
}

/// The active policy, or the built-in default when none was installed.
pub fn active() -> Policy {
    let mut p = ACTIVE.get().cloned().unwrap_or_default();
    if brake_pulled() {
        p.apply_brake();
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_catalog_without_the_object_gives_the_defaults() {
        assert_eq!(Policy::from_catalog_json(r#"{"models":[]}"#), Policy::default());
        assert_eq!(Policy::from_catalog_json("not json at all"), Policy::default());
        assert_eq!(Policy::from_catalog_json(r#"{"byteask_efficiency": 7}"#), Policy::default());
    }

    #[test]
    fn defaults_are_conservative() {
        let p = Policy::default();
        assert!(p.ansi && p.cr);
        assert_eq!(p.exec_budget_tokens, None);
    }

    #[test]
    fn fields_are_read_one_by_one_and_a_bad_one_does_not_poison_the_rest() {
        let p = Policy::from_catalog_json(
            r#"{"models":[],"byteask_efficiency":{"policy_version":"2026-09-19.1",
                "reducers":{"ansi":"yes","cr":false},"budgets":{"exec":4000}}}"#,
        );
        assert_eq!(p.version, "2026-09-19.1");
        assert!(p.ansi, "a non-boolean leaves the default in place");
        assert!(!p.cr);
        assert_eq!(p.exec_budget_tokens, Some(4000));
    }

    #[test]
    fn budgets_are_clamped() {
        let low = Policy::from_catalog_json(r#"{"byteask_efficiency":{"budgets":{"exec":5}}}"#);
        assert_eq!(low.exec_budget_tokens, Some(1000));
        let high = Policy::from_catalog_json(r#"{"byteask_efficiency":{"budgets":{"exec":9999999}}}"#);
        assert_eq!(high.exec_budget_tokens, Some(10000));
        let junk = Policy::from_catalog_json(r#"{"byteask_efficiency":{"budgets":{"exec":-3}}}"#);
        assert_eq!(junk.exec_budget_tokens, None);
    }

    #[test]
    fn the_brake_switches_everything_off() {
        let p = Policy::from_catalog_json(
            r#"{"byteask_efficiency":{"brake":true,"budgets":{"exec":4000}}}"#,
        );
        assert!(!p.ansi && !p.cr);
        assert_eq!(p.exec_budget_tokens, None);
    }

    #[test]
    fn reducer_switches_budgets_flags_and_numbers_are_read_and_clamped() {
        let p = Policy::from_catalog_json(
            r#"{"byteask_efficiency":{"reducers":{"tests":true,"rle":false,"made-up":true},
                "budgets":{"search":2000,"build_test":50,"nonsense":1},
                "flags":{"loop_nudge":true,"compact_header":"yes","unknown":true},
                "numbers":{"first_yield_ms":9999999,"cold_after_secs":1,
                "empty_poll_min_ms":1,"first_yield_min_ms":999999}}}"#,
        );
        assert!(p.reducer_on("tests"));
        assert!(!p.reducer_on("rle"));
        assert!(p.reducer_on("ws"), "untouched lossless reducers stay on");
        assert!(!p.reducer_on("made-up"));
        assert_eq!(p.budget_for(crate::family::Family::Search), Some(2000));
        assert_eq!(p.budget_for(crate::family::Family::Build), Some(1000), "clamped up");
        assert_eq!(p.budget_for(crate::family::Family::Diff), None);
        assert!(p.flag("loop_nudge"));
        assert!(!p.flag("compact_header"), "a non-boolean is not true");
        assert_eq!(p.number("first_yield_ms"), Some(300_000));
        assert_eq!(p.number("cold_after_secs"), Some(300));
        assert_eq!(p.number("empty_poll_min_ms"), Some(5_000), "clamped up to upstream's floor");
        assert_eq!(p.number("first_yield_min_ms"), Some(30_000), "clamped down to the cap");
    }

    #[test]
    fn the_explorer_model_is_validated_before_it_can_reach_a_toml_document() {
        let ok = Policy::from_catalog_json(r#"{"byteask_efficiency":{"explorer":{"model":"gpt-5.6-luna","effort":"low"}}}"#);
        assert_eq!(ok.explorer, Some(("gpt-5.6-luna".to_string(), "low".to_string())));
        let default_effort = Policy::from_catalog_json(r#"{"byteask_efficiency":{"explorer":{"model":"kimi-k3"}}}"#);
        assert_eq!(default_effort.explorer, Some(("kimi-k3".to_string(), "low".to_string())));
        for bad in [
            r#"{"byteask_efficiency":{"explorer":{"model":"x"
sandbox_mode = "danger-full-access"}}}"#,
            r#"{"byteask_efficiency":{"explorer":{"model":""}}}"#,
            r#"{"byteask_efficiency":{"explorer":{"model":"gpt-5.4","effort":"xhigh"}}}"#,
            r#"{"byteask_efficiency":{"explorer":"gpt-5.4"}}"#,
            r#"{"byteask_efficiency":{"brake":true,"explorer":{"model":"gpt-5.4"}}}"#,
        ] {
            assert_eq!(Policy::from_catalog_json(bad).explorer, None, "{bad}");
        }
    }

    #[test]
    fn the_exec_budget_is_the_fallback_for_every_family() {
        let p = Policy::from_catalog_json(r#"{"byteask_efficiency":{"budgets":{"exec":4000,"search":2000}}}"#);
        assert_eq!(p.budget_for(crate::family::Family::Unknown), Some(4000));
        assert_eq!(p.budget_for(crate::family::Family::Search), Some(2000));
    }

    #[test]
    fn the_brake_clears_the_new_switches_too() {
        let p = Policy::from_catalog_json(
            r#"{"byteask_efficiency":{"brake":true,"reducers":{"tests":true},
                "budgets":{"exec":4000},"flags":{"loop_nudge":true}}}"#,
        );
        assert!(!p.reducer_on("tests") && !p.reducer_on("rle"));
        assert_eq!(p.budget_for(crate::family::Family::Unknown), None);
        assert!(!p.flag("loop_nudge"));
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        assert_eq!(Policy::from_catalog_file(Path::new("/nonexistent/x.json")), Policy::default());
    }
}
