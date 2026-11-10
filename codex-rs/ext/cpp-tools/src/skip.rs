//! A person's skip.
//!
//! `byteask checks skip` (a command, never a tool) records that a person chose not to run a
//! check on the work tree exactly as it is now, with their reason. The finish gate then does
//! not build or test that version and ends the turn with NOT RUN (skipped: reason, who), never
//! PASS, and never loops. The skip is bound to the tree's freshness fingerprint: any
//! later edit is a new version, and the check runs again. The merge check never reads it:
//! at merge a skip is just a NOT RUN the CI re-runs.

use std::path::Path;
use std::time::Duration;

use check_result::CheckResult;
use check_result::NotRunReason;

/// The results-store tier a skip is kept under.
pub const TIER: &str = "skip";
/// Who records skips.
pub const SOURCE: &str = "person";

const BUDGET: Duration = Duration::from_secs(5);

/// The NOT RUN a skip stands for.
pub fn skip_result(check: &str, reason: &str, who: &str) -> CheckResult {
    let reason = reason.lines().next().unwrap_or_default().trim();
    CheckResult::not_run(
        check,
        NotRunReason::Skipped,
        &format!("Skipped by {who}: {reason}"),
        "any edit ends the skip; finish again and the check runs.",
    )
}

/// Record a skip of `check` for the work tree at `cwd` as it is now. Returns the result.
pub async fn record(
    home: &Path,
    cwd: &Path,
    check: &str,
    reason: &str,
    who: &str,
) -> Result<CheckResult, String> {
    if reason.trim().is_empty() {
        return Err("a skip needs a reason (--reason)".to_string());
    }
    let root = crate::gitx::toplevel(cwd)
        .await
        .ok_or_else(|| format!("{} is not inside a git repository", cwd.display()))?;
    let fresh = crate::results_store::fingerprint(&root, BUDGET)
        .await
        .ok_or_else(|| "git did not answer in time; nothing was skipped".to_string())?;
    let result = skip_result(check, reason, who).with_content_key(&fresh);
    crate::results_store::record(
        home,
        crate::results_store::Entry {
            result: result.clone(),
            root,
            at: crate::results_store::now_unix(),
            fresh,
            tier: TIER.to_string(),
            source: SOURCE.to_string(),
        },
    );
    Ok(result)
}

/// The skip of `check` that applies to the work tree at `root` as it is now, if a person
/// recorded one for exactly this version.
pub(crate) async fn active(home: &Path, root: &Path, check: &str) -> Option<CheckResult> {
    let skips: Vec<crate::results_store::Entry> = crate::results_store::all(home, root)
        .into_iter()
        .filter(|entry| entry.tier == TIER && entry.source == SOURCE && entry.result.check == check)
        .filter(|entry| entry.result.reason == Some(NotRunReason::Skipped))
        .collect();
    if skips.is_empty() {
        return None;
    }
    let now = crate::results_store::fingerprint(root, BUDGET).await?;
    skips
        .into_iter()
        .find(|entry| !entry.fresh.is_empty() && entry.fresh == now)
        .map(|entry| entry.result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A skip is NOT RUN (skipped: reason, who), only for the exact version it was made on.
    #[tokio::test]
    async fn a_skip_applies_to_this_version_only_and_is_never_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("a.cpp"), "int a;\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "b"]);
        std::fs::write(root.join("a.cpp"), "int a = 1;\n").unwrap();
        let home = dir.path().join("home");
        assert!(
            record(&home, &root, crate::gate::CHECK, "  ", "alice")
                .await
                .is_err()
        );
        let result = record(
            &home,
            &root,
            crate::gate::CHECK,
            "flaky rig; checked by hand",
            "alice",
        )
        .await
        .unwrap();
        assert!(!result.is_pass());
        assert_eq!(result.reason, Some(NotRunReason::Skipped));
        assert_eq!(
            result.summary,
            "Skipped by alice: flaky rig; checked by hand"
        );
        result.validate().unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(
            active(&home, &canonical, crate::gate::CHECK).await,
            Some(result)
        );
        assert_eq!(
            active(&home, &canonical, "cpp_sanitize").await,
            None,
            "another check"
        );
        // Any edit is a new version: the skip no longer applies.
        std::fs::write(root.join("a.cpp"), "int a = 2;\n").unwrap();
        assert_eq!(active(&home, &canonical, crate::gate::CHECK).await, None);
    }
}
