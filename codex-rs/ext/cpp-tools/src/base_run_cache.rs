//! Test results on the unmodified base commit, cached per (repository, base
//! commit, build configuration), so "did my change break this test?" costs one
//! base build per session instead of a revert, rebuild and re-run each time.
//!
//! Shared API: `cpp_test_select compare_base` writes and reads it, and any other
//! engine component that needs "status on base" (a finish gate comparing
//! failures with a base run) can use the same functions and file layout.
//!
//! ```text
//!   let key = BaseRunKey::new(repo_root, h0_sha, config_fingerprint);
//!   let missing = missing(&key, &names);          // names never run on base
//!   ... run `missing` on a base build ...
//!   record(&key, results)?;                       // merged into the cache
//!   let flips = flips(&load(&key).unwrap_or_default(), &now);
//! ```
//!
//! Layout: `$BYTEASK_HOME/cache/base-runs/<repo-hash>/<commit>-<config-hash>.json`
//! (else `~/.byteask/cache/…`, else the system temp dir). One JSON object per key:
//! `{"repo", "commit", "config", "updated", "results": {"<test>": "pass"|…}}`.
//! Test names are whatever the writer uses consistently; `cpp_test_select` uses
//! the CTest name, or `<test>::<case>` for a single case of a test program.
//! Writes are atomic (temp file + rename) and merge with what is already there.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Outcome of one test (or test case).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestStatus {
    Pass,
    Fail,
    /// Failed once, passed on the re-run.
    Flaky,
    Skip,
    Timeout,
    /// The test program did not compile or link.
    NotBuilt,
    /// Selected but never executed (e.g. a filter matched nothing).
    NotRun,
}

impl TestStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            TestStatus::Pass => "pass",
            TestStatus::Fail => "FAIL",
            TestStatus::Flaky => "flaky",
            TestStatus::Skip => "skip",
            TestStatus::Timeout => "TIMEOUT",
            TestStatus::NotBuilt => "NOT BUILT",
            TestStatus::NotRun => "not run",
        }
    }

    /// Counts as passing for a flip (flaky passes on re-run).
    pub(crate) fn passing(self) -> bool {
        matches!(self, TestStatus::Pass | TestStatus::Flaky)
    }

    /// Counts as failing for a flip. A test that does not compile fails.
    pub(crate) fn failing(self) -> bool {
        matches!(
            self,
            TestStatus::Fail | TestStatus::Timeout | TestStatus::NotBuilt
        )
    }
}

/// Which base run a result belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseRunKey {
    pub repo_root: PathBuf,
    /// Full commit id of the base (normally the session-start HEAD).
    pub commit: String,
    /// Anything that changes results besides the source: build system, build
    /// type, compiler, options. Free-form; hashed into the file name.
    pub config: String,
}

impl BaseRunKey {
    pub(crate) fn new(repo_root: &Path, commit: &str, config: &str) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            commit: commit.to_string(),
            config: config.to_string(),
        }
    }

    /// The cache file for this key.
    pub(crate) fn path(&self) -> PathBuf {
        let repo =
            std::fs::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());
        let commit: String = self.commit.chars().take(16).collect();
        cache_root()
            .join(format!("{:016x}", fnv(repo.to_string_lossy().as_bytes())))
            .join(format!(
                "{commit}-{:016x}.json",
                fnv(self.config.as_bytes())
            ))
    }
}

/// Cached base results for one key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BaseRun {
    pub repo: String,
    pub commit: String,
    pub config: String,
    /// Unix seconds of the last write.
    pub updated: u64,
    pub results: BTreeMap<String, TestStatus>,
}

/// One test whose status differs between base and now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Flip {
    pub name: String,
    pub base: TestStatus,
    pub now: TestStatus,
}

impl Flip {
    /// `pass->FAIL` style label.
    pub(crate) fn label(&self) -> String {
        format!("{}->{}", self.base.label(), self.now.label())
    }

    /// A passing test that now fails (a regression, unless flaky).
    pub(crate) fn is_regression(&self) -> bool {
        self.base.passing() && self.now.failing()
    }

    /// A failing test (or one that did not build) that now passes.
    pub(crate) fn is_fix(&self) -> bool {
        self.base.failing() && self.now.passing()
    }
}

/// `$BYTEASK_HOME/cache/base-runs`, else `~/.byteask/cache/base-runs`, else temp.
pub(crate) fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("BYTEASK_BASE_RUN_CACHE").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("BYTEASK_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".byteask")))
        .unwrap_or_else(std::env::temp_dir);
    home.join("cache").join("base-runs")
}

/// Cached results for `key`, if any.
pub(crate) fn load(key: &BaseRunKey) -> Option<BaseRun> {
    let text = std::fs::read_to_string(key.path()).ok()?;
    let run: BaseRun = serde_json::from_str(&text).ok()?;
    (run.commit == key.commit && run.config == key.config).then_some(run)
}

/// Merge `results` into the cache for `key` and return the merged run.
pub(crate) fn record(
    key: &BaseRunKey,
    results: impl IntoIterator<Item = (String, TestStatus)>,
) -> std::io::Result<BaseRun> {
    let mut run = load(key).unwrap_or_else(|| BaseRun {
        repo: key.repo_root.display().to_string(),
        commit: key.commit.clone(),
        config: key.config.clone(),
        ..BaseRun::default()
    });
    run.results.extend(results);
    run.updated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = key.path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&run).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(run)
}

/// The names in `names` that have no cached base result for `key`.
pub(crate) fn missing<'a>(key: &BaseRunKey, names: &'a [String]) -> Vec<&'a String> {
    let cached = load(key).map(|run| run.results).unwrap_or_default();
    names
        .iter()
        .filter(|name| !cached.contains_key(*name))
        .collect()
}

/// Tests whose pass/fail class differs between `base` and `now`. Tests with no
/// base result, or not run now, are not flips. Skip↔pass is not a flip.
pub(crate) fn flips(base: &BaseRun, now: &BTreeMap<String, TestStatus>) -> Vec<Flip> {
    now.iter()
        .filter_map(|(name, now)| {
            let base = *base.results.get(name)?;
            let changed = (base.passing() && now.failing()) || (base.failing() && now.passing());
            changed.then(|| Flip {
                name: name.clone(),
                base,
                now: *now,
            })
        })
        .collect()
}

/// FNV-1a: stable across runs and Rust versions.
pub(crate) fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn records_merge_and_flips_are_reported() {
        {
            use_test_cache_dir();
            let repo = tempfile::tempdir().unwrap();
            let key = BaseRunKey::new(repo.path(), "abc123", "cmake Release");
            assert_eq!(load(&key), None);
            record(&key, [("a".to_string(), TestStatus::Pass)]).unwrap();
            record(
                &key,
                [
                    ("b".to_string(), TestStatus::Fail),
                    ("c".to_string(), TestStatus::NotBuilt),
                    ("d".to_string(), TestStatus::Skip),
                ],
            )
            .unwrap();
            let run = load(&key).unwrap();
            assert_eq!(run.results.len(), 4);
            let names = vec!["a".to_string(), "e".to_string()];
            assert_eq!(missing(&key, &names), vec![&"e".to_string()]);

            let now: BTreeMap<String, TestStatus> = [
                ("a".to_string(), TestStatus::Fail),
                ("b".to_string(), TestStatus::Pass),
                ("c".to_string(), TestStatus::Pass),
                ("d".to_string(), TestStatus::Pass),
                ("e".to_string(), TestStatus::Fail),
            ]
            .into_iter()
            .collect();
            let flips = flips(&run, &now);
            let labels: Vec<(String, String)> =
                flips.iter().map(|f| (f.name.clone(), f.label())).collect();
            assert_eq!(
                labels,
                vec![
                    ("a".to_string(), "pass->FAIL".to_string()),
                    ("b".to_string(), "FAIL->pass".to_string()),
                    ("c".to_string(), "NOT BUILT->pass".to_string()),
                ]
            );
            assert!(flips[0].is_regression());
            assert!(flips[2].is_fix());

            // A different configuration is a different base run.
            let other = BaseRunKey::new(repo.path(), "abc123", "cmake Debug");
            assert_ne!(other.path(), key.path());
            assert_eq!(load(&other), None);
        }
    }
}

/// Point the cache at a per-process temp dir for the rest of the test run
/// (never at the real home). Set once, never removed: keys include the
/// (unique) repository path, so tests cannot see each other's entries.
#[cfg(test)]
pub(crate) fn use_test_cache_dir() {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("byteask-base-runs-test-{}", std::process::id()));
        // SAFETY: set once per process, before any reader in these tests.
        unsafe { std::env::set_var("BYTEASK_BASE_RUN_CACHE", &dir) };
        dir
    });
}
