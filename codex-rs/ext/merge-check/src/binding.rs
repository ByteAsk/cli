//! What a merge-check result is bound to.
//!
//! A result counts only for the exact merge candidate it checked: the tree `head` merged into
//! the current `base` would produce, plus the revisions of everything the check read from the
//! protected base branch (the profile, the test manifest, the perf baseline) and the build
//! environment. The [`Binding::content_key`] is a SHA-256 over all of it, so any base-branch
//! change after a PASS is a new key: nothing earlier counts and the checks run again.

use std::path::Path;

use checks_profile::Profile;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;

use crate::git;

/// The perf unit-test file that `perf-runner ut` reads, taken from the base branch.
pub const PERF_UT_PATH: &str = ".byteask/perf-ut.toml";

/// Environment that changes what a build produces (the same list as the local gate's).
pub const BUILD_ENV: &[&str] = &[
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CMAKE_GENERATOR",
    "CMAKE_BUILD_TYPE",
];

/// The merge candidate a result is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The base branch commit the candidate merges into.
    pub base_commit: String,
    /// The head commit being merged (information only: the key is the merged tree).
    pub head_commit: String,
    /// The tree of head merged into base (empty when they do not merge cleanly).
    pub merge_tree: String,
    /// Blob id of `.byteask/checks.toml` on the base branch, or `none`.
    pub profile_rev: String,
    /// Blob id of the test manifest on the base branch, or `none`.
    pub manifest_rev: String,
    /// The perf baseline: the base commit and the perf-ut file's blob id on it.
    pub baseline_rev: String,
    /// `NAME=value;` for every [`BUILD_ENV`] variable.
    pub env: String,
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut hasher = sha2::Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Binding {
    /// A candidate that does not merge has no tree: it is keyed by both commits instead.
    fn tree_part(&self) -> String {
        if self.merge_tree.is_empty() {
            format!("conflict:{}:{}", self.base_commit, self.head_commit)
        } else {
            self.merge_tree.clone()
        }
    }

    /// The merge candidate, from git objects alone: the merged tree and the base
    /// branch's profile, manifest and perf-baseline revisions. Every CI job computes the same
    /// value from the same repository, whatever runner image it is on; the CI-side cache and
    /// approvals are keyed by it.
    pub fn candidate_key(&self) -> String {
        sha256_hex(&[
            "byteask-merge-candidate/1\ntree:",
            &self.tree_part(),
            "\nprofile:",
            &self.profile_rev,
            "\nmanifest:",
            &self.manifest_rev,
            "\nbaseline:",
            &self.baseline_rev,
        ])
    }

    /// The content key a result carries: the candidate plus the build environment the
    /// job that produced it ran with.
    pub fn content_key(&self) -> String {
        sha256_hex(&[
            "byteask-merge-check/1\ncandidate:",
            &self.candidate_key(),
            "\nenv:",
            &self.env,
        ])
    }

    /// The same merge candidate (everything but the recording job's build environment).
    pub fn same_candidate(&self, other: &Binding) -> bool {
        self.candidate_key() == other.candidate_key()
            && self.base_commit == other.base_commit
            && self.head_commit == other.head_commit
    }
}

/// The profile as the protected base branch has it: a branch's own edits never apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseProfile {
    /// No `.byteask/checks.toml` on the base branch.
    Missing,
    /// It exists but does not parse; the reason.
    Invalid(String),
    Valid(Box<Profile>),
}

impl BaseProfile {
    pub fn profile(&self) -> Option<&Profile> {
        match self {
            BaseProfile::Valid(profile) => Some(profile),
            _ => None,
        }
    }
}

/// Everything the check knows about the candidate before running anything.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub binding: Binding,
    pub profile: BaseProfile,
    /// The head does not merge cleanly into base: the conflicting paths.
    pub conflict: Option<Vec<String>>,
    /// Paths that differ between base and the merged tree.
    pub changed: Vec<String>,
}

pub(crate) fn build_env(env: &dyn Fn(&str) -> Option<String>) -> String {
    BUILD_ENV
        .iter()
        .map(|key| format!("{key}={};", env(key).unwrap_or_default()))
        .collect()
}

impl Candidate {
    /// Read the candidate from git objects only (nothing from the branch runs).
    pub fn compute(repo: &Path, base: &str, head: &str) -> Result<Self, String> {
        Self::compute_with_env(repo, base, head, &|key| std::env::var(key).ok())
    }

    pub fn compute_with_env(
        repo: &Path,
        base: &str,
        head: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let base_commit = git::rev_parse(repo, &format!("{base}^{{commit}}"))?;
        let head_commit = git::rev_parse(repo, &format!("{head}^{{commit}}"))?;
        let (merge_tree, conflict) = match git::merge_tree(repo, &base_commit, &head_commit) {
            Ok(tree) => (tree, None),
            Err(git::MergeError::Conflict(paths)) => (String::new(), Some(paths)),
            Err(git::MergeError::Git(err)) => return Err(err),
        };
        let profile_rev = git::blob_at(repo, &base_commit, checks_profile::PATH);
        let profile = match &profile_rev {
            None => BaseProfile::Missing,
            Some(_) => match git::show(repo, &base_commit, checks_profile::PATH) {
                Some(text) => match Profile::parse(&text) {
                    Ok(profile) => BaseProfile::Valid(Box::new(profile)),
                    Err(err) => BaseProfile::Invalid(err),
                },
                None => BaseProfile::Invalid(format!("{} could not be read", checks_profile::PATH)),
            },
        };
        let manifest_rev = profile
            .profile()
            .and_then(|profile| profile.test.as_ref())
            .and_then(|test| test.manifest.as_deref())
            .map(|path| git::blob_at(repo, &base_commit, path).unwrap_or_else(|| "missing".into()))
            .unwrap_or_else(|| "none".to_string());
        let perf_rev = git::blob_at(repo, &base_commit, PERF_UT_PATH).unwrap_or_else(|| "none".into());
        let changed = if conflict.is_some() {
            git::changed_paths(repo, &base_commit, &head_commit)?
        } else {
            git::changed_paths(repo, &base_commit, &merge_tree)?
        };
        Ok(Candidate {
            binding: Binding {
                baseline_rev: format!("{base_commit}:{perf_rev}"),
                base_commit,
                head_commit,
                merge_tree,
                profile_rev: profile_rev.unwrap_or_else(|| "none".to_string()),
                manifest_rev,
                env: build_env(env),
            },
            profile,
            conflict,
            changed,
        })
    }
}
