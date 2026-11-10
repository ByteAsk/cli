//! The evaluation bench: ONE worktree at the snapshot in which candidates and test patches are
//! checked one at a time (apply, build, run, restore). One build tree per build command is reused
//! incrementally across candidates and shares the session's compiler cache, so nothing is built
//! twice in parallel and the bench holds at most one build at a time.
//!
//! Every command goes through [`EvalContext::step`] (the build/test semaphore, the attempts
//! deadline, cancellation) with its parallelism capped ([`patches::cap_parallelism`]). Test
//! patches are applied only here, never in the real tree.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;

use crate::attempts::patches;
use crate::attempts::selector::Author;
use crate::attempts::selector::CandidateEval;
use crate::attempts::selector::Cell;
use crate::attempts::selector::PooledTest;
use crate::attempts::testparse;
use crate::evaluate::EvalContext;
use crate::evaluate::Recipe;
use crate::evaluate::StepLog;
use crate::gitops;
use crate::gitops::ChangeKind;
use crate::gitops::Snapshot;
use crate::runner::RunOutput;
use crate::runner::spawn_capture;

/// A test patch in the pool: tests an attempt (or the independent writer) wrote, with the
/// author's own build and run commands (their build tree layout).
#[derive(Clone, Debug, Serialize)]
pub struct TestPatch {
    pub id: String,
    pub author: Author,
    #[serde(skip)]
    pub patch: Vec<u8>,
    pub build: String,
    pub run: String,
    /// Already shown to fail on the starting code (the independent writer's frozen tests).
    pub prequalified: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BaseRecord {
    pub build_ok: bool,
    /// Tests that pass in every base run. `None` = no usable test-suite record.
    pub passing: Option<BTreeSet<String>>,
    pub suite_runs: usize,
    pub sanitizer_reports: usize,
    /// test patch id -> fails (or fails to build) on the starting code.
    pub qualified: BTreeMap<String, bool>,
}

/// What the bench needs to know about one candidate.
pub struct CandidateInput<'a> {
    pub attempt: usize,
    pub diff: &'a [u8],
    pub finished: bool,
    pub disqualified: Option<String>,
}

pub struct Bench {
    pub dir: PathBuf,
    /// The independent writer's worktree (already built at the snapshot) was reused.
    pub reused: bool,
    snapshot: Snapshot,
    pub ctx: EvalContext,
    pub jobs: usize,
    pub recipe: Option<Recipe>,
    tried: Vec<String>,
    pub base: BaseRecord,
    pub log: Vec<StepLog>,
    /// Wall time of the last full candidate evaluation.
    pub eval_time: Option<Duration>,
}

/// Existing test files the change modifies or deletes.
pub fn test_file_edits(diff: &[u8]) -> Vec<String> {
    gitops::changed_paths(diff)
        .into_iter()
        .filter(|(path, kind)| *kind != ChangeKind::Added && testparse::is_test_path(path))
        .map(|(path, _)| path)
        .collect()
}

impl Bench {
    /// Open the bench: reuse `reuse` (reset to the snapshot) when given and usable, else add a
    /// fresh detached worktree `<scratch>/wt-eval`.
    pub async fn open(
        repo: &Path,
        snapshot: &Snapshot,
        scratch: &Path,
        reuse: Option<PathBuf>,
        ctx: EvalContext,
        jobs: usize,
    ) -> Result<Self, String> {
        let tmp = scratch.join("tmp");
        let mut chosen: Option<(PathBuf, bool)> = None;
        if let Some(dir) = reuse.filter(|d| d.is_dir()) {
            let current = gitops::candidate_diff(&dir, snapshot, &tmp).await.unwrap_or_default();
            if current.is_empty() || gitops::apply(&dir, &current, true, false).await.is_ok() {
                chosen = Some((dir, true));
            }
        }
        let (dir, reused) = match chosen {
            Some(c) => c,
            None => {
                let dir = scratch.join("wt-eval");
                gitops::worktree_add(repo, &dir, &snapshot.commit).await?;
                (dir, false)
            }
        };
        Ok(Self {
            dir,
            reused,
            snapshot: snapshot.clone(),
            ctx,
            jobs,
            recipe: None,
            tried: Vec::new(),
            base: BaseRecord::default(),
            log: Vec::new(),
            eval_time: None,
        })
    }

    async fn run(&mut self, what: &str, cmd: &str, timeout: Duration) -> Option<RunOutput> {
        let cmd = patches::cap_parallelism(cmd, self.jobs);
        let out = self.ctx.step(&cmd, &self.dir, timeout).await?;
        self.log.push(StepLog::from_run(what, &cmd, &out));
        Some(out)
    }

    /// Put a patch on the bench (which must be at the snapshot for a candidate change).
    async fn put(&self, patch: &[u8]) -> Result<(), String> {
        gitops::apply(&self.dir, patch, false, false).await
    }

    /// Put a candidate change on the bench. When it does not apply because a build in the bench
    /// created or rewrote files it touches (in-source builds), those paths are reset to the
    /// snapshot first and the change is applied again.
    async fn put_change(&mut self, diff: &[u8]) -> Result<(), String> {
        if self.put(diff).await.is_ok() {
            return Ok(());
        }
        self.reset_paths(diff, "apply failed").await;
        self.put(diff).await
    }

    /// Take a patch off again. When the reverse apply fails (a build rewrote a file it touches),
    /// the touched paths are restored from the snapshot.
    async fn take(&mut self, patch: &[u8]) {
        if patch.is_empty() || gitops::apply(&self.dir, patch, true, false).await.is_ok() {
            return;
        }
        self.reset_paths(patch, "reverse apply failed").await;
    }

    /// Bring the paths `patch` touches back to the snapshot: files it adds are removed, the
    /// others checked out from the snapshot commit.
    async fn reset_paths(&mut self, patch: &[u8], why: &str) {
        for (path, kind) in gitops::changed_paths(patch) {
            if kind == ChangeKind::Added {
                let _ = std::fs::remove_file(self.dir.join(&path));
                continue;
            }
            let commit = self.snapshot.commit.clone();
            let _ = spawn_capture(
                "git",
                &["-c", "core.hooksPath=/dev/null", "checkout", &commit, "--", &path],
                &self.dir,
                &[("GIT_OPTIONAL_LOCKS", "0")],
                None,
                Duration::from_secs(120),
            )
            .await;
        }
        self.log.push(StepLog {
            what: "restore".to_string(),
            cmd: "git checkout <snapshot> -- <paths>".to_string(),
            code: Some(0),
            timed_out: false,
            elapsed_ms: 0,
            tail: format!("{why}; touched paths reset to the snapshot"),
        });
    }

    /// Pick the first recipe (in trust order) whose build works at the snapshot; at most three
    /// tries over the bench's life. True when a recipe is set.
    pub async fn ensure_recipe(&mut self, recipes: &[Recipe]) -> bool {
        if self.recipe.is_some() {
            return true;
        }
        for r in recipes {
            if self.tried.len() >= 3 || self.ctx.remaining().is_zero() {
                break;
            }
            if self.tried.contains(&r.build) {
                continue;
            }
            self.tried.push(r.build.clone());
            let what = format!("base build ({})", r.source);
            let timeout = self.ctx.build_timeout;
            let Some(out) = self.run(&what, &r.build, timeout).await else {
                break;
            };
            if out.ok() {
                self.base.build_ok = true;
                self.recipe = Some(r.clone());
                return true;
            }
        }
        false
    }

    /// The base test-suite record: two runs (one when the first used much of the time left);
    /// a test counts as passing on base only if it passed in every run.
    pub async fn base_suite(&mut self) {
        if self.base.suite_runs > 0 {
            return;
        }
        let Some(suite) = self.recipe.as_ref().and_then(|r| r.test.clone()) else {
            return;
        };
        let mut runs: Vec<testparse::TestRun> = Vec::new();
        for i in 0..2 {
            let started = Instant::now();
            let timeout = self.ctx.test_timeout.min(self.ctx.remaining() / 3);
            let Some(out) = self.run(&format!("base tests run {}", i + 1), &suite, timeout).await else {
                break;
            };
            self.base.sanitizer_reports = self
                .base
                .sanitizer_reports
                .max(testparse::sanitizer_reports(&out.output));
            if out.timed_out {
                break;
            }
            runs.push(testparse::parse_test_output(&out.output, out.ok(), out.timed_out));
            if started.elapsed() * 4 > self.ctx.remaining() {
                break;
            }
        }
        self.base.suite_runs = runs.len();
        if let Some(first) = runs.first() {
            let passing: BTreeSet<String> = first
                .outcomes
                .iter()
                .filter(|(name, ok)| **ok && runs.iter().all(|r| r.outcomes.get(*name) == Some(&true)))
                .map(|(name, _)| name.clone())
                .collect();
            self.base.passing = Some(passing);
        }
    }

    /// Run one test patch on whatever change is on the bench now. Returns the cell and the
    /// sanitizer reports seen.
    async fn cell(&mut self, t: &TestPatch, label: &str) -> (Cell, usize) {
        if self.ctx.remaining().is_zero() {
            return (Cell::NotRun, 0);
        }
        if self.put(&t.patch).await.is_err() {
            self.log.push(StepLog {
                what: format!("{label}: apply {}", t.id),
                cmd: "git apply".to_string(),
                code: Some(1),
                timed_out: false,
                elapsed_ms: 0,
                tail: "test patch does not apply".to_string(),
            });
            return (Cell::Conflict, 0);
        }
        let mut sanitizer = 0;
        let build_timeout = self.ctx.build_timeout;
        let test_timeout = self.ctx.test_timeout;
        let cell = match self.run(&format!("{label}: build with {}", t.id), &t.build, build_timeout).await {
            None => Cell::NotRun,
            Some(out) if out.timed_out => Cell::NotRun,
            Some(out) if !out.ok() => Cell::Fail,
            Some(_) => match self.run(&format!("{label}: run {}", t.id), &t.run, test_timeout).await {
                None => Cell::NotRun,
                Some(out) if out.timed_out => Cell::NotRun,
                Some(out) => {
                    sanitizer = testparse::sanitizer_reports(&out.output);
                    if out.ok() { Cell::Pass } else { Cell::Fail }
                }
            },
        };
        self.take(&t.patch).await;
        (cell, sanitizer)
    }

    /// Does `t` fail (or fail to build) on the starting code? A patch that does not even apply
    /// there, or whose run was cut off, is not qualified.
    pub async fn qualify(&mut self, t: &TestPatch) -> bool {
        let qualified = if t.prequalified {
            true
        } else {
            matches!(self.cell(t, "base").await.0, Cell::Fail)
        };
        self.base.qualified.insert(t.id.clone(), qualified);
        qualified
    }

    /// Run `tests` on the change `diff` (put on the bench and taken off again).
    pub async fn cells_on(&mut self, attempt: usize, diff: &[u8], tests: &[TestPatch]) -> BTreeMap<String, (Cell, usize)> {
        let mut out = BTreeMap::new();
        if diff.is_empty() || self.put_change(diff).await.is_err() {
            for t in tests {
                out.insert(t.id.clone(), (Cell::NotRun, 0));
            }
            return out;
        }
        for t in tests {
            let r = self.cell(t, &format!("attempt {attempt}")).await;
            out.insert(t.id.clone(), r);
        }
        self.take(diff).await;
        out
    }

    /// Full evaluation of one candidate: build, warnings in the changed files, regressions
    /// against the base record, and every qualified test.
    pub async fn evaluate(&mut self, c: CandidateInput<'_>, tests: &[TestPatch]) -> CandidateEval {
        let started = Instant::now();
        let changed: Vec<String> = gitops::changed_paths(c.diff)
            .into_iter()
            .filter(|(_, kind)| *kind != ChangeKind::Deleted)
            .map(|(p, _)| p)
            .collect();
        let mut eval = CandidateEval {
            attempt: c.attempt,
            diff_bytes: c.diff.len(),
            changed_files: patches::changed_files(c.diff),
            changed_lines: patches::changed_lines(c.diff),
            normalized: gitops::normalized_change(c.diff),
            test_file_edits: test_file_edits(c.diff),
            finished: c.finished,
            disqualified: c.disqualified.clone(),
            ..CandidateEval::default()
        };
        if c.diff.is_empty() || c.disqualified.is_some() {
            return eval;
        }
        let label = format!("attempt {}", c.attempt);
        if let Err(e) = self.put_change(c.diff).await {
            self.log.push(StepLog {
                what: format!("{label}: apply"),
                cmd: "git apply".to_string(),
                code: Some(1),
                timed_out: false,
                elapsed_ms: 0,
                tail: e,
            });
            eval.build_ok = Some(false);
            return eval;
        }
        let mut sanitizer = 0usize;
        let mut cand_warnings = BTreeMap::new();
        if let Some(recipe) = self.recipe.clone() {
            let build_timeout = self.ctx.build_timeout;
            match self.run(&format!("{label}: build"), &recipe.build, build_timeout).await {
                Some(out) if !out.timed_out => {
                    eval.build_ok = Some(out.ok());
                    cand_warnings = testparse::warnings_in(
                        &testparse::warnings_per_file(&out.output, &self.dir.to_string_lossy()),
                        &changed,
                    );
                }
                _ => {}
            }
            if eval.build_ok == Some(true)
                && let (Some(suite), Some(passing)) = (recipe.test.clone(), self.base.passing.clone())
            {
                let mut failing: Option<BTreeSet<String>> = None;
                for i in 0..2 {
                    let timeout = self.ctx.test_timeout.min(self.ctx.remaining() / 2);
                    let Some(out) = self.run(&format!("{label}: tests run {}", i + 1), &suite, timeout).await else {
                        break;
                    };
                    if i == 0 {
                        sanitizer += testparse::sanitizer_reports(&out.output)
                            .saturating_sub(self.base.sanitizer_reports);
                    }
                    let run = testparse::parse_test_output(&out.output, out.ok(), out.timed_out);
                    // A test missing from the output (its binary no longer builds) counts as failing.
                    let now_failing: BTreeSet<String> = passing
                        .iter()
                        .filter(|name| run.outcomes.get(*name) != Some(&true))
                        .cloned()
                        .collect();
                    failing = Some(match failing {
                        None => now_failing,
                        Some(prev) => prev.intersection(&now_failing).cloned().collect(),
                    });
                    // Re-run only to rule out flakes.
                    if failing.as_ref().is_some_and(BTreeSet::is_empty) {
                        break;
                    }
                }
                eval.regressions = failing.map(|f| f.into_iter().collect());
            }
        }
        for t in tests {
            if !self.base.qualified.get(&t.id).copied().unwrap_or(false) {
                continue;
            }
            let (cell, san) = self.cell(t, &label).await;
            sanitizer += san;
            eval.cells.insert(t.id.clone(), cell);
        }
        self.take(c.diff).await;
        // Rebuild at the snapshot: brings the build tree back to base and, through the compiler
        // cache, gives the base warnings of exactly the files this change touched.
        if eval.build_ok == Some(true)
            && let Some(recipe) = self.recipe.clone()
        {
            let build_timeout = self.ctx.build_timeout;
            if let Some(out) = self.run(&format!("{label}: base rebuild"), &recipe.build, build_timeout).await {
                let base_w = testparse::warnings_in(
                    &testparse::warnings_per_file(&out.output, &self.dir.to_string_lossy()),
                    &changed,
                );
                eval.new_warnings = cand_warnings
                    .iter()
                    .map(|(f, n)| n.saturating_sub(*base_w.get(f).unwrap_or(&0)))
                    .sum();
            }
        }
        eval.new_sanitizer_reports = sanitizer;
        self.eval_time = Some(started.elapsed());
        eval
    }

    /// The pooled tests as the selector sees them. A test is valid unless its author's own
    /// change was evaluated and failed it.
    pub fn pooled(&self, tests: &[TestPatch], evals: &[CandidateEval]) -> Vec<PooledTest> {
        tests
            .iter()
            .map(|t| {
                let own = match t.author {
                    Author::Attempt(k) => evals.iter().find(|e| e.attempt == k),
                    Author::Independent => None,
                };
                let valid = own.is_none_or(|e| !matches!(e.cells.get(&t.id), Some(Cell::Fail | Cell::Conflict)));
                PooledTest {
                    id: t.id.clone(),
                    author: t.author.clone(),
                    qualified: self.base.qualified.get(&t.id).copied().unwrap_or(false),
                    valid,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_edits_only_count_existing_test_files() {
        let diff = b"diff --git a/tests/a.cpp b/tests/a.cpp\n--- a/tests/a.cpp\n+++ b/tests/a.cpp\n\
diff --git a/tests/new_case.cpp b/tests/new_case.cpp\nnew file mode 100644\n\
diff --git a/src/lib.cpp b/src/lib.cpp\n--- a/src/lib.cpp\n+++ b/src/lib.cpp\n";
        assert_eq!(test_file_edits(diff), vec!["tests/a.cpp".to_string()]);
    }
}
