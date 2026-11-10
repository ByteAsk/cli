//! Choosing among candidate changes. Pure logic over evaluated candidates (deterministic under
//! test); the only side effect is the optional judge.
//!
//! Order:
//! 1. hard filters: non-empty change, builds, no new failures among tests that passed on the
//!    starting code (flake-checked), no edits to existing tests, not disqualified. If nobody
//!    survives, the candidates with the fewest violations (build > regressions > test edits).
//! 2. cross-attempt test matrix: every qualified, valid test runs on every candidate.
//!    Independent tests weigh 1.0, other attempts' tests 0.5, a candidate's own tests 0. A
//!    candidate whose only evidence is its own tests has no matrix score and stays in the top
//!    group (the matrix cannot compare it).
//! 3. tie-breaks: fewer new sanitizer reports, fewer new compiler warnings in the changed files,
//!    minimality (candidates touching more than one file beyond the smallest change lose; line
//!    counts are not used).
//! 4. identical changes collapse to the lowest attempt number.
//! 5. fresh-context pairwise judge on diffs + check results, both presentation orders, king of
//!    the hill from the lowest attempt number: a challenger replaces the incumbent only if both
//!    orders prefer it, so an inconsistent judge leaves attempt 1 in place.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Author {
    /// The independent reproduction-test writer.
    Independent,
    /// Attempt k wrote it.
    Attempt(usize),
}

impl Author {
    pub fn weight_for(&self, candidate: usize) -> f64 {
        match self {
            Author::Independent => 1.0,
            Author::Attempt(k) if *k == candidate => 0.0,
            Author::Attempt(_) => 0.5,
        }
    }

    fn attempt(&self) -> Option<usize> {
        match self {
            Author::Attempt(k) => Some(*k),
            Author::Independent => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PooledTest {
    pub id: String,
    pub author: Author,
    /// Fails (or fails to build) on the starting code.
    pub qualified: bool,
    /// Passes with its author's own change (always true for independent tests). A test its
    /// own author's change fails says nothing about the others.
    pub valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Cell {
    Pass,
    Fail,
    /// The test patch does not apply on top of this candidate.
    Conflict,
    /// Not run (deadline, or skipped).
    NotRun,
}

impl Cell {
    fn known(self) -> bool {
        !matches!(self, Cell::NotRun)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CandidateEval {
    pub attempt: usize,
    pub diff_bytes: usize,
    pub changed_files: usize,
    pub changed_lines: usize,
    #[serde(skip)]
    pub normalized: String,
    /// Existing test files modified or deleted by the change.
    pub test_file_edits: Vec<String>,
    /// `None` when no build recipe was available.
    pub build_ok: Option<bool>,
    /// Tests that passed on the starting code (twice) but fail here (twice). `None` = unknown.
    pub regressions: Option<Vec<String>>,
    pub cells: BTreeMap<String, Cell>,
    pub new_sanitizer_reports: usize,
    pub new_warnings: usize,
    /// The attempt's session finished normally (not interrupted or failed).
    pub finished: bool,
    /// Excluded whatever its checks say (e.g. it changed the real repository).
    pub disqualified: Option<String>,
}

impl CandidateEval {
    fn passes_hard_filters(&self) -> bool {
        self.diff_bytes > 0
            && self.disqualified.is_none()
            && self.build_ok != Some(false)
            && self.regressions.as_ref().is_none_or(Vec::is_empty)
            && self.test_file_edits.is_empty()
    }

    /// Violations ordered build > regressions > test edits (smaller is better).
    fn violation_key(&self) -> (u8, usize, usize) {
        (
            u8::from(self.build_ok == Some(false)),
            self.regressions.as_ref().map_or(0, Vec::len),
            self.test_file_edits.len(),
        )
    }

    fn passes(&self, test: &str) -> Option<bool> {
        match self.cells.get(test) {
            Some(Cell::Pass) => Some(true),
            Some(Cell::Fail | Cell::Conflict) => Some(false),
            Some(Cell::NotRun) | None => None,
        }
    }
}

/// Which of two presented patches the judge preferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Pick {
    First,
    Second,
}

pub type JudgeFuture<'a> = Pin<Box<dyn Future<Output = Option<Pick>> + Send + 'a>>;

/// A fresh-context pairwise judge. `None` means no usable verdict.
pub trait Judge: Send + Sync {
    fn compare<'a>(&'a self, first: &'a CandidateEval, second: &'a CandidateEval) -> JudgeFuture<'a>;
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Selection {
    pub winner: Option<usize>,
    pub reason: String,
    pub survivors: Vec<usize>,
    pub relaxed: bool,
    /// Matrix score per candidate; absent = no comparable evidence.
    pub scores: BTreeMap<usize, f64>,
    pub judged: bool,
    pub trace: Vec<String>,
}

/// Qualified, valid tests that every candidate in `cands` (except the test's author) ran.
pub fn usable_tests<'a>(cands: &[&CandidateEval], tests: &'a [PooledTest]) -> Vec<&'a PooledTest> {
    tests
        .iter()
        .filter(|t| t.qualified && t.valid)
        .filter(|t| {
            cands
                .iter()
                .filter(|c| t.author.attempt() != Some(c.attempt))
                .all(|c| c.cells.get(&t.id).is_some_and(|cell| cell.known()))
        })
        .collect()
}

fn matrix_score(c: &CandidateEval, tests: &[&PooledTest]) -> Option<f64> {
    let mut num = 0.0;
    let mut den = 0.0;
    for t in tests {
        let w = t.author.weight_for(c.attempt);
        if w == 0.0 {
            continue;
        }
        den += w;
        if c.passes(&t.id) == Some(true) {
            num += w;
        }
    }
    (den > 0.0).then(|| num / den)
}

fn violations(c: &CandidateEval) -> Vec<String> {
    let mut why = Vec::new();
    if c.diff_bytes == 0 {
        why.push("empty change".to_string());
    }
    if let Some(d) = &c.disqualified {
        why.push(format!("disqualified: {d}"));
    }
    if c.build_ok == Some(false) {
        why.push("build fails".to_string());
    }
    if let Some(r) = &c.regressions
        && !r.is_empty()
    {
        why.push(format!("{} new test failure(s): {}", r.len(), r.join(", ")));
    }
    if !c.test_file_edits.is_empty() {
        why.push(format!("edits existing tests: {}", c.test_file_edits.join(", ")));
    }
    why
}

/// Candidates that pass the hard filters (or the least-bad ones if none do).
pub fn filter<'a>(cands: &'a [CandidateEval], trace: &mut Vec<String>) -> (Vec<&'a CandidateEval>, bool) {
    for c in cands {
        let why = violations(c);
        if !why.is_empty() {
            trace.push(format!("attempt {}: {}", c.attempt, why.join("; ")));
        }
    }
    let eligible: Vec<&CandidateEval> = cands
        .iter()
        .filter(|c| c.diff_bytes > 0 && c.disqualified.is_none())
        .collect();
    let survivors: Vec<&CandidateEval> = eligible
        .iter()
        .copied()
        .filter(|c| c.passes_hard_filters())
        .collect();
    if !survivors.is_empty() || eligible.is_empty() {
        return (survivors, false);
    }
    let best = eligible.iter().map(|c| c.violation_key()).min().unwrap_or_default();
    let relaxed: Vec<&CandidateEval> = eligible
        .into_iter()
        .filter(|c| c.violation_key() == best)
        .collect();
    trace.push(format!(
        "no candidate passes every hard filter; relaxed to the fewest violations: {:?}",
        ids(&relaxed)
    ));
    (relaxed, true)
}

fn ids(group: &[&CandidateEval]) -> Vec<usize> {
    group.iter().map(|c| c.attempt).collect()
}

fn keep_min(group: Vec<&CandidateEval>, key: impl Fn(&CandidateEval) -> usize) -> Vec<&CandidateEval> {
    let best = group.iter().map(|c| key(c)).min().unwrap_or(0);
    group.into_iter().filter(|c| key(c) == best).collect()
}

/// A tie-break stage: narrow `top`, record it, and say whether a single winner remains.
fn narrow<'a>(
    sel: &mut Selection,
    top: Vec<&'a CandidateEval>,
    label: &str,
    keep: impl Fn(Vec<&'a CandidateEval>) -> Vec<&'a CandidateEval>,
) -> Vec<&'a CandidateEval> {
    let before = top.len();
    let after = keep(top);
    if after.len() < before {
        sel.trace.push(format!("tie-break {label}: {:?}", ids(&after)));
        if after.len() == 1 {
            sel.winner = Some(after[0].attempt);
            sel.reason = format!("tie-break: {label}");
        }
    }
    after
}

pub async fn select(cands: &[CandidateEval], tests: &[PooledTest], judge: Option<&dyn Judge>) -> Selection {
    let mut sel = Selection::default();
    let (survivors, relaxed) = filter(cands, &mut sel.trace);
    sel.relaxed = relaxed;
    sel.survivors = ids(&survivors);
    if survivors.is_empty() {
        sel.reason = "no usable candidate".to_string();
        return sel;
    }
    if survivors.len() == 1 {
        sel.winner = Some(survivors[0].attempt);
        sel.reason = if relaxed {
            "only candidate with the fewest hard-filter violations".to_string()
        } else {
            "only candidate that passes the hard filters".to_string()
        };
        return sel;
    }

    // Cross-attempt test matrix.
    let usable = usable_tests(&survivors, tests);
    for c in &survivors {
        if let Some(score) = matrix_score(c, &usable) {
            sel.scores.insert(c.attempt, score);
        }
    }
    let best = sel.scores.values().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut top: Vec<&CandidateEval> = survivors
        .iter()
        .copied()
        .filter(|c| sel.scores.get(&c.attempt).is_none_or(|s| *s >= best - 1e-9))
        .collect();
    sel.trace.push(format!(
        "test matrix over {} usable test(s): scores {:?}, top {:?}",
        usable.len(),
        sel.scores,
        ids(&top)
    ));
    if top.len() == 1 {
        sel.winner = Some(top[0].attempt);
        sel.reason = "highest cross-attempt test score".to_string();
        return sel;
    }

    // Tie-breaks.
    top = narrow(&mut sel, top, "fewest new sanitizer reports", |g| keep_min(g, |c| c.new_sanitizer_reports));
    if sel.winner.is_some() {
        return sel;
    }
    top = narrow(&mut sel, top, "fewest new compiler warnings", |g| keep_min(g, |c| c.new_warnings));
    if sel.winner.is_some() {
        return sel;
    }
    top = narrow(&mut sel, top, "minimality (files touched)", |g| {
        let fewest = g.iter().map(|c| c.changed_files).min().unwrap_or(0);
        g.into_iter().filter(|c| c.changed_files <= fewest + 1).collect()
    });
    if sel.winner.is_some() {
        return sel;
    }

    // Identical changes collapse to the lowest attempt number.
    top.sort_by_key(|c| c.attempt);
    let mut distinct: Vec<&CandidateEval> = Vec::new();
    for c in top {
        if !distinct.iter().any(|d| d.normalized == c.normalized) {
            distinct.push(c);
        }
    }
    if distinct.len() == 1 {
        sel.winner = Some(distinct[0].attempt);
        sel.reason = "the remaining candidates made the same change".to_string();
        return sel;
    }

    // Pairwise judge, king of the hill from the lowest attempt number.
    let mut incumbent = distinct[0];
    let Some(judge) = judge else {
        sel.winner = Some(incumbent.attempt);
        sel.reason = "execution signals tie and no judge; lowest attempt number kept".to_string();
        return sel;
    };
    sel.judged = true;
    for challenger in distinct.iter().skip(1).copied() {
        let forward = judge.compare(incumbent, challenger).await;
        let backward = judge.compare(challenger, incumbent).await;
        let before = incumbent.attempt;
        let verdict = match (forward, backward) {
            (Some(Pick::Second), Some(Pick::First)) => {
                incumbent = challenger;
                "challenger preferred in both orders"
            }
            (Some(Pick::First), Some(Pick::Second)) => "incumbent preferred in both orders",
            _ => "inconsistent or no verdict; incumbent kept",
        };
        sel.trace.push(format!(
            "judge attempt {before} vs {}: {forward:?}/{backward:?} -> {verdict}",
            challenger.attempt
        ));
    }
    sel.winner = Some(incumbent.attempt);
    sel.reason = "pairwise judge (both presentation orders)".to_string();
    sel
}

/// Do two candidates behave alike? Comparable on some test and never different there; with no
/// comparable test, only identical changes count as alike.
fn agree(a: &CandidateEval, b: &CandidateEval, tests: &[&PooledTest]) -> bool {
    let mut comparable = false;
    for t in tests {
        if let (Some(pa), Some(pb)) = (a.passes(&t.id), b.passes(&t.id)) {
            if pa != pb {
                return false;
            }
            comparable = true;
        }
    }
    comparable || a.normalized == b.normalized
}

/// Should another attempt run? Yes when fewer than two candidates pass the hard filters, or
/// when no pair of agreeing candidates includes a top-scoring one.
pub fn disagreement(cands: &[CandidateEval], tests: &[PooledTest]) -> (bool, String) {
    let mut scratch = Vec::new();
    let (survivors, relaxed) = filter(cands, &mut scratch);
    if relaxed || survivors.len() < 2 {
        let n = if relaxed { 0 } else { survivors.len() };
        return (true, format!("{n} candidate(s) pass the hard filters"));
    }
    let usable: Vec<&PooledTest> = tests.iter().filter(|t| t.qualified && t.valid).collect();
    let scores: BTreeMap<usize, Option<f64>> = survivors
        .iter()
        .map(|c| (c.attempt, matrix_score(c, &usable_tests(&survivors, tests))))
        .collect();
    let best = scores.values().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
    let is_top = |c: &CandidateEval| scores.get(&c.attempt).copied().flatten().is_none_or(|s| s >= best - 1e-9);
    for (i, a) in survivors.iter().enumerate() {
        for b in survivors.iter().skip(i + 1) {
            if (is_top(a) || is_top(b)) && agree(a, b, &usable) {
                return (
                    false,
                    format!("attempts {} and {} agree", a.attempt, b.attempt),
                );
            }
        }
    }
    let why = if usable.is_empty() {
        "no discriminating test and the changes differ".to_string()
    } else {
        "candidates pass different sets of qualified tests (or cannot be compared)".to_string()
    };
    (true, why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;

    fn cand(attempt: usize) -> CandidateEval {
        CandidateEval {
            attempt,
            diff_bytes: 100,
            changed_files: 1,
            changed_lines: 2,
            normalized: format!("change-{attempt}"),
            build_ok: Some(true),
            regressions: Some(Vec::new()),
            finished: true,
            ..CandidateEval::default()
        }
    }

    fn with_cells(mut c: CandidateEval, cells: &[(&str, Cell)]) -> CandidateEval {
        for (id, cell) in cells {
            c.cells.insert((*id).to_string(), *cell);
        }
        c
    }

    fn test(id: &str, author: Author) -> PooledTest {
        PooledTest {
            id: id.to_string(),
            author,
            qualified: true,
            valid: true,
        }
    }

    struct ScriptedJudge {
        answers: Mutex<Vec<Option<Pick>>>,
        calls: Mutex<Vec<(usize, usize)>>,
    }

    impl ScriptedJudge {
        fn new(answers: Vec<Option<Pick>>) -> Self {
            Self {
                answers: Mutex::new(answers),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Judge for ScriptedJudge {
        fn compare<'a>(&'a self, first: &'a CandidateEval, second: &'a CandidateEval) -> JudgeFuture<'a> {
            self.calls.lock().unwrap().push((first.attempt, second.attempt));
            let answer = self.answers.lock().unwrap().remove(0);
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn hard_filters_drop_empty_broken_regressing_test_editing_and_disqualified() {
        let mut empty = cand(1);
        empty.diff_bytes = 0;
        let mut broken = cand(2);
        broken.build_ok = Some(false);
        let mut regress = cand(3);
        regress.regressions = Some(vec!["ctest:a".to_string()]);
        let mut edits = cand(4);
        edits.test_file_edits = vec!["tests/a.cpp".to_string()];
        let mut touched = cand(5);
        touched.disqualified = Some("changed the real repository".to_string());
        let good = cand(6);
        let sel = select(&[empty, broken, regress, edits, touched, good], &[], None).await;
        assert_eq!(sel.winner, Some(6));
        assert_eq!(sel.survivors, vec![6]);
        assert!(!sel.relaxed);
        assert_eq!(sel.trace.len(), 5);
    }

    #[tokio::test]
    async fn relaxes_to_fewest_violations_in_order_but_never_to_disqualified() {
        let mut broken = cand(1);
        broken.build_ok = Some(false);
        let mut regress2 = cand(2);
        regress2.regressions = Some(vec!["a".into(), "b".into()]);
        let mut regress1 = cand(3);
        regress1.regressions = Some(vec!["a".into()]);
        let mut touched = cand(4);
        touched.disqualified = Some("x".into());
        let sel = select(&[broken, regress2, regress1, touched], &[], None).await;
        assert!(sel.relaxed);
        assert_eq!(sel.winner, Some(3));
    }

    #[tokio::test]
    async fn all_empty_means_no_winner() {
        let mut a = cand(1);
        a.diff_bytes = 0;
        let mut b = cand(2);
        b.diff_bytes = 0;
        let sel = select(&[a, b], &[], None).await;
        assert_eq!(sel.winner, None);
    }

    #[tokio::test]
    async fn matrix_prefers_independent_evidence_and_ignores_own_tests() {
        let tests = vec![test("repro", Author::Independent), test("t2", Author::Attempt(2)), test("t3", Author::Attempt(3))];
        let c1 = with_cells(cand(1), &[("repro", Cell::Fail), ("t2", Cell::Pass), ("t3", Cell::Pass)]);
        let c2 = with_cells(cand(2), &[("repro", Cell::Pass), ("t2", Cell::Pass), ("t3", Cell::Fail)]);
        let c3 = with_cells(cand(3), &[("repro", Cell::Pass), ("t2", Cell::Pass), ("t3", Cell::Pass)]);
        let sel = select(&[c1, c2, c3], &tests, None).await;
        // c1: (0 + .5 + .5)/2 = .5; c2: (1 + 0)/1.5 = .667; c3: (1 + .5)/1.5 = 1.
        assert_eq!(sel.winner, Some(3));
        assert!((sel.scores[&1] - 0.5).abs() < 1e-9);
        assert!((sel.scores[&2] - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(sel.reason, "highest cross-attempt test score");
    }

    #[tokio::test]
    async fn a_candidate_judged_only_by_its_own_tests_stays_in_the_top_group() {
        // Attempt 2 wrote the only test; attempt 1 fails it. The matrix cannot compare them
        // (attempt 2's own test does not count for it), so the judge decides.
        let tests = vec![test("t2", Author::Attempt(2))];
        let c1 = with_cells(cand(1), &[("t2", Cell::Fail)]);
        let c2 = with_cells(cand(2), &[("t2", Cell::Pass)]);
        let judge = ScriptedJudge::new(vec![Some(Pick::Second), Some(Pick::First)]);
        let sel = select(&[c1.clone(), c2.clone()], &tests, Some(&judge)).await;
        assert_eq!(sel.scores.get(&2), None);
        assert_eq!(sel.winner, Some(2));
        assert!(sel.judged);
        // Without a judge the lowest attempt number stays.
        let sel = select(&[c1, c2], &tests, None).await;
        assert_eq!(sel.winner, Some(1));
    }

    #[tokio::test]
    async fn invalid_unqualified_and_unrun_tests_are_ignored() {
        let mut unq = test("passes-on-base", Author::Independent);
        unq.qualified = false;
        let mut invalid = test("t2", Author::Attempt(2));
        invalid.valid = false;
        let tests = vec![test("repro", Author::Independent), unq, invalid, test("late", Author::Independent)];
        let c1 = with_cells(cand(1), &[("repro", Cell::Conflict), ("passes-on-base", Cell::Pass), ("t2", Cell::Pass), ("late", Cell::Pass)]);
        let c2 = with_cells(cand(2), &[("repro", Cell::Pass), ("passes-on-base", Cell::Fail), ("t2", Cell::Fail), ("late", Cell::NotRun)]);
        let usable = usable_tests(&[&c1, &c2], &tests);
        assert_eq!(usable.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["repro"]);
        let sel = select(&[c1, c2], &tests, None).await;
        assert_eq!(sel.winner, Some(2));
    }

    #[tokio::test]
    async fn tie_breaks_sanitizer_then_warnings_then_minimality() {
        let mut a = cand(1);
        a.new_sanitizer_reports = 1;
        let sel = select(&[a, cand(2)], &[], None).await;
        assert_eq!((sel.winner, sel.reason.as_str()), (Some(2), "tie-break: fewest new sanitizer reports"));

        let mut a = cand(1);
        a.new_warnings = 1;
        let mut b = cand(2);
        b.new_warnings = 3;
        let sel = select(&[a, b], &[], None).await;
        assert_eq!((sel.winner, sel.reason.as_str()), (Some(1), "tie-break: fewest new compiler warnings"));

        // Minimality is coarse: one extra file is tolerated, more is not; lines never count.
        let mut a = cand(1);
        a.changed_files = 4;
        let mut b = cand(2);
        b.changed_files = 1;
        b.changed_lines = 400;
        let sel = select(&[a, b], &[], None).await;
        assert_eq!((sel.winner, sel.reason.as_str()), (Some(2), "tie-break: minimality (files touched)"));
        let mut a = cand(1);
        a.changed_files = 2;
        let sel = select(&[a, cand(2)], &[], None).await;
        assert_eq!(sel.reason, "execution signals tie and no judge; lowest attempt number kept");
    }

    #[tokio::test]
    async fn identical_changes_collapse_before_the_judge() {
        let mut b = cand(2);
        b.normalized = "same".into();
        let mut c = cand(3);
        c.normalized = "same".into();
        let mut a = cand(1);
        a.diff_bytes = 0;
        let sel = select(&[a, b.clone(), c.clone()], &[], None).await;
        assert_eq!((sel.winner, sel.reason.as_str()), (Some(2), "the remaining candidates made the same change"));
        // With attempt 1 different, the judge sees 1 vs 2 only (3 = 2).
        let judge = ScriptedJudge::new(vec![Some(Pick::First), Some(Pick::Second)]);
        let sel = select(&[cand(1), b, c], &[], Some(&judge)).await;
        assert_eq!(sel.winner, Some(1));
        assert_eq!(*judge.calls.lock().unwrap(), vec![(1, 2), (2, 1)]);
    }

    #[tokio::test]
    async fn judge_needs_both_orders_to_replace_attempt_one() {
        let judge = ScriptedJudge::new(vec![Some(Pick::Second), Some(Pick::First)]);
        let sel = select(&[cand(1), cand(2)], &[], Some(&judge)).await;
        assert_eq!(sel.winner, Some(2));
        assert!(sel.judged);
        // Position bias (always the first shown) is inconsistent: attempt 1 stays.
        let judge = ScriptedJudge::new(vec![Some(Pick::First), Some(Pick::First)]);
        let sel = select(&[cand(1), cand(2)], &[], Some(&judge)).await;
        assert_eq!(sel.winner, Some(1));
        // No verdict at all: attempt 1 stays.
        let judge = ScriptedJudge::new(vec![None, None]);
        let sel = select(&[cand(1), cand(2)], &[], Some(&judge)).await;
        assert_eq!(sel.winner, Some(1));
        // King of the hill: 2 beats 1, then 3 is inconsistent against 2.
        let judge = ScriptedJudge::new(vec![Some(Pick::Second), Some(Pick::First), Some(Pick::First), Some(Pick::First)]);
        let sel = select(&[cand(1), cand(2), cand(3)], &[], Some(&judge)).await;
        assert_eq!(sel.winner, Some(2));
        assert_eq!(*judge.calls.lock().unwrap(), vec![(1, 2), (2, 1), (2, 3), (3, 2)]);
    }

    #[test]
    fn disagreement_rules() {
        let tests = vec![test("repro", Author::Independent)];
        let a = with_cells(cand(1), &[("repro", Cell::Pass)]);
        let b = with_cells(cand(2), &[("repro", Cell::Pass)]);
        assert!(!disagreement(&[a.clone(), b], &tests).0, "agree on a qualified test");
        let c = with_cells(cand(2), &[("repro", Cell::Fail)]);
        assert!(disagreement(&[a.clone(), c], &tests).0, "different test outcomes");
        let mut broken = cand(2);
        broken.build_ok = Some(false);
        assert!(disagreement(&[a, broken], &tests).0, "one survivor");
        // No tests: identical changes agree, different ones do not.
        let mut x = cand(1);
        x.normalized = "same".into();
        let mut y = cand(2);
        y.normalized = "same".into();
        assert!(!disagreement(&[x.clone(), y], &[]).0);
        let mut z = cand(2);
        z.normalized = "other".into();
        assert!(disagreement(&[x, z], &[]).0);
        // Three candidates: 2 and 3 agree and are top-scoring -> stop.
        let one = with_cells(cand(1), &[("repro", Cell::Fail)]);
        let two = with_cells(cand(2), &[("repro", Cell::Pass)]);
        let three = with_cells(cand(3), &[("repro", Cell::Pass)]);
        let (dis, why) = disagreement(&[one, two, three], &tests);
        assert!(!dis, "{why}");
        assert_eq!(why, "attempts 2 and 3 agree");
        // An agreeing pair of losers does not stop escalation.
        let tests2 = vec![test("r1", Author::Independent), test("r2", Author::Independent)];
        let w = with_cells(cand(1), &[("r1", Cell::Pass), ("r2", Cell::Pass)]);
        let l1 = with_cells(cand(2), &[("r1", Cell::Fail), ("r2", Cell::Pass)]);
        let l2 = with_cells(cand(3), &[("r1", Cell::Fail), ("r2", Cell::Pass)]);
        assert!(disagreement(&[w, l1, l2], &tests2).0);
    }
}
