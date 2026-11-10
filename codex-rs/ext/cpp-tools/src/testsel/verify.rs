//! `cpp_finish_check`'s build and test step for a CMake project: build only the test
//! programs the change reaches, in the tools' own tree, and run exactly those tests.
//!
//! ```text
//!   per-tree lock (waits for a background warm build of the same tree)
//!   → the change since the session-start commit (+ `git diff --check`)
//!   → the build model of the tools' own tree, configured once per repository and commit
//!     (File API codemodel-v2 targets, `ctest --show-only=json-v1`, depfiles)
//!   → tier 3 of `cpp_test_select`: every test the build graph says the change reaches
//!   → build exactly their programs (or the smallest stated superset), incrementally
//!   → `ctest -R '^(…)$'`, failures re-run once, then compared with the session-start commit
//! ```
//!
//! Everything within the check's time limit (cancelled at the deadline, never left running).
//! Its findings go into the gate's outcome, so the verdict, the advisory contract (no hygiene
//! rules, nothing deleted) and the rendering stay `cpp_finish_check`'s.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::watch;

use super::Model;
use super::base;
use super::check;
use super::cmake;
use super::context;
use super::exec;
use super::exec::Ctx;
use super::exec::RunItem;
use super::select;
use super::symbols;
use super::tree;
use crate::base_run_cache;
use crate::base_run_cache::BaseRunKey;
use crate::base_run_cache::TestStatus;
use crate::gate::GateOutcome;
use crate::gate::Problem;
use crate::gate::ProblemKind;
use crate::gate::Snapshot;
use crate::gate::SuiteRun;
use crate::session_head::Head;

/// Longest one test may run inside the check.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Below this much time left, tests are not started.
const MIN_TEST_TIME: Duration = Duration::from_secs(5);
/// Targets named in one answer line.
const TARGETS_SHOWN: usize = 30;
/// With more test programs than this to build, the nearest tests' programs are built (and their
/// tests run) first, so a check that runs out of time still verifies the closest tests.
const NEAR_PROGRAMS: usize = 3;
/// Time kept back for running the nearest tests while the other programs are still building.
const NEAR_TEST_RESERVE: Duration = Duration::from_secs(20);

/// Whether the environment switch `name` is on (the switches are off unless set to 1).
fn switch_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
}

/// `BYTEASK_FINISH_CHECK_NEAR_ONLY`: only the nearest tests' programs are built and run.
fn near_only() -> bool {
    switch_on("BYTEASK_FINISH_CHECK_NEAR_ONLY")
}

/// The words of the near-only note; the verdict (gate `result`) reads them to say what ran.
pub(crate) const NEAR_ONLY_NOTE: &str =
    "this check builds and runs only the nearest tests' programs";

/// `BYTEASK_FINISH_CHECK_UNCOVERED`: a change no target compiles is reported as not covered (never PASS).
fn uncovered() -> bool {
    switch_on("BYTEASK_FINISH_CHECK_UNCOVERED")
}

/// `BYTEASK_FINISH_CHECK_WARM_YIELD`: a running background warm build of the tree stops for the check.
pub(crate) fn warm_yield() -> bool {
    switch_on("BYTEASK_FINISH_CHECK_WARM_YIELD")
}

/// The targeted check applies: a CMake project with cmake and ctest installed.
pub(crate) fn applies(root: &Path) -> bool {
    root.join("CMakeLists.txt").is_file()
        && crate::tools::binary_on_path("cmake")
        && crate::tools::binary_on_path("ctest")
}

/// Cancels the check's commands at its deadline; stops the timer when dropped.
struct CancelAt(tokio::task::JoinHandle<()>);

impl Drop for CancelAt {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Build the targets the change reaches and run their tests; findings go into `out` and
/// `not_checked` as the gate's own build-and-test step would put them.
pub(crate) async fn finish_check(
    snapshot: &Snapshot,
    deadline: Instant,
    budget: Duration,
    out: &mut GateOutcome,
    not_checked: &mut Vec<String>,
) -> SuiteRun {
    let root = &snapshot.root;
    let scratch = tree::scratch_dir(root);
    let own = tree::own_tree(root);
    let left = || deadline.saturating_duration_since(Instant::now());
    let limit = budget.as_secs();
    let (cancel_tx, cancel) = watch::channel(false);
    let _timer = CancelAt(tokio::spawn(async move {
        tokio::time::sleep_until(deadline.into()).await;
        let _ = cancel_tx.send(true);
    }));

    // 1. The tree: a background warm build (or another session) may be building in it.
    // `BYTEASK_FINISH_CHECK_WARM_YIELD`: a warm build still running here stops (what it built is kept).
    if warm_yield() && super::warm::stop_for_check(&own) {
        out.notes.push(
            "stopped the background warm build of its tree (what it built is kept) to build what this check needs"
                .to_string(),
        );
    }
    let waiting = Instant::now();
    let Ok(_guard) = tree::lock_tree(&own, left()).await else {
        return SuiteRun::NotChecked(format!(
            "another build of the check's own tree {} (the background warm build, or another session) was still running when the time limit ({limit}s) ran out",
            own.display()
        ));
    };
    if waiting.elapsed() >= Duration::from_secs(1) {
        out.notes.push(format!(
            "waited {:.0}s for the background build of its tree to finish",
            waiting.elapsed().as_secs_f64()
        ));
    }

    // 2. The change.
    let short = &snapshot.head[..snapshot.head.len().min(10)];
    let changes = match symbols::changes(
        root,
        &snapshot.head,
        format!("session-start commit {short}"),
        None,
        None,
    )
    .await
    {
        Ok(changes) => changes,
        Err(err) => return SuiteRun::NotChecked(format!("could not read the change ({err})")),
    };
    out.notes.push(tree::diff_check(root, &snapshot.head).await);

    // 3. The build model of the tools' own tree.
    let version = crate::toolver::tool_version("cmake", root).await;
    let cmake_files: Vec<PathBuf> = changes
        .files
        .iter()
        .filter(|f| {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            name == "CMakeLists.txt" || name.ends_with(".cmake")
        })
        .cloned()
        .collect();
    let request = cmake::Request {
        root,
        build_dir: None,
        scratch: &scratch,
        version,
        cmake_files: &cmake_files,
        commit: Some(&snapshot.head),
    };
    let model = match tokio::time::timeout(left(), cmake::model(&request)).await {
        Err(_) => {
            return SuiteRun::NotChecked(format!(
                "configuring its own build tree {} did not finish within the time limit ({limit}s)",
                own.display()
            ));
        }
        Ok(Err(err)) => {
            return SuiteRun::NotChecked(format!(
                "configuring a scratch build failed: {}",
                err.lines().take(4).collect::<Vec<_>>().join(" | ")
            ));
        }
        Ok(Ok(model)) => model,
    };
    for note in model.notes.iter().take(2) {
        out.notes.push(note.clone());
    }

    // 4. The tests the change reaches (tier 3, the safe superset) and their programs.
    // The local build graph's selection always runs: it is the fallback, and its count is the
    // comparison when ByteAsk Context's select_tests chooses instead.
    let sel = select::select(&model, &changes).await;
    let picked =
        match tokio::time::timeout(left(), context::pick(root, &snapshot.head, &model)).await {
            Ok(Some(Ok(pick))) => Some(pick),
            Ok(Some(Err(why))) => {
                out.notes
                    .push(format!("ByteAsk Context select_tests not used: {why}"));
                None
            }
            Ok(None) => None,
            Err(_) => {
                out.notes.push(
                    "ByteAsk Context select_tests not used: no answer within the time limit"
                        .to_string(),
                );
                None
            }
        };
    // The graph is blind for this change when a changed header has no includer it can see (a macro
    // or generated include it may miss) or its include scan was cut short. A changed source file no
    // target compiles is not blindness: no registered test can contain it.
    let graph_blind =
        sel.affected.unmapped.iter().any(|f| super::is_header(f)) || sel.affected.scan_truncated;
    let combined = picked
        .as_ref()
        .map(|pick| context::combine(&pick.tests, &sel.tier3, graph_blind));
    let (mut tests, mut plan, compile_only) = match (&picked, &combined) {
        (Some(pick), Some((tests, _))) => (
            tests.clone(),
            tree::plan_targets(&model, tests),
            pick.targets.clone(),
        ),
        _ => (
            sel.tier3.clone(),
            tree::plan_targets(&model, &sel.tier3),
            sel.affected.direct.iter().cloned().collect(),
        ),
    };
    if tests.is_empty() {
        // No test reaches the change: still compile what it touches.
        plan = tree::TargetPlan {
            targets: compile_only,
            ..tree::TargetPlan::default()
        };
    }
    match &picked {
        Some(pick) => {
            let how = match combined.as_ref().map(|(_, how)| how) {
                Some(context::Combined::Both) => "the tests both name",
                Some(context::Combined::Union) => "both sets (they share no test)",
                _ => "Context's choice",
            };
            out.notes.push(format!(
                "selection: {} of {} registered test(s), {how}: ByteAsk Context select_tests named {} ({} program(s)), the local build graph reaches {} ({})",
                tests.len(),
                sel.enabled.len(),
                pick.tests.len(),
                pick.programs,
                sel.tier3.len(),
                model.mapping_note
            ));
            if !pick.unregistered.is_empty() {
                out.notes.push(format!(
                    "not registered in this build tree, so not run: {}",
                    tree::list(&pick.unregistered, 6)
                ));
            }
        }
        None => out.notes.push(format!(
            "selection: {} of {} registered test(s) reach the change ({})",
            tests.len(),
            sel.enabled.len(),
            model.mapping_note
        )),
    }
    // Warnings about the name-based tiers 1-2 do not concern this check's tier 3.
    for warning in sel.warnings.iter().filter(|w| !w.contains("tier")).take(2) {
        out.notes.push(warning.clone());
    }
    if !sel.affected.unmapped.is_empty() {
        let files: Vec<String> = sel
            .affected
            .unmapped
            .iter()
            .map(|f| model.rel(f).to_string())
            .collect();
        out.notes.push(format!(
            "not compiled by any target, so not checked: {}",
            tree::list(&files, 4)
        ));
        // No changed file reaches any target at all (no target lists it, and no file a target compiles
        // includes it, sources included: unity builds), so the tests that run here may not exercise the change.
        if uncovered()
            && sel.affected.direct.is_empty()
            && !sel.affected.scan_truncated
            && super::graph::included_by_compiled(&model, &sel.affected.unmapped) == Some(false)
        {
            not_checked.push(format!(
                "the change is not covered: no target of this build compiles a changed file or a file that includes one ({}), so the tests run here may not exercise it",
                tree::list(&files, 4)
            ));
        }
    }
    out.notes.extend(plan.notes.iter().cloned());

    // 5. Build exactly those, incrementally.
    let ctx = Ctx {
        log: scratch
            .join("logs")
            .join(format!("finish-{}.log", millis())),
        jobs: exec::default_jobs(),
        test_timeout: TEST_TIMEOUT,
        cancel,
        progress: Arc::new(Mutex::new(String::new())),
    };
    let what = if plan.build_all {
        "everything".to_string()
    } else {
        format!("targets {}", tree::list(&plan.targets, TARGETS_SHOWN))
    };
    // Many programs: the nearest tests' programs first, the rest until NEAR_TEST_RESERVE before
    // the deadline; when the rest does not finish, only the nearest tests run.
    let near = if plan.build_all || plan.targets.len() <= NEAR_PROGRAMS {
        Vec::new()
    } else {
        check::nearest(&model, &sel.affected, &tests, &changes.files, NEAR_PROGRAMS)
    };
    if !near.is_empty() && near.len() < tests.len() {
        let near_plan = tree::plan_targets(&model, &near);
        let near_what = format!(
            "the nearest tests' targets {} (of {} to build)",
            tree::list(&near_plan.targets, TARGETS_SHOWN),
            plan.targets.len()
        );
        let built = exec::build(&ctx, &model, &model.build_dir, &near_plan.targets).await;
        out.notes.extend(built.waited.clone());
        if built.cancelled || built.timed_out {
            return SuiteRun::NotChecked(format!(
                "its build of {near_what} did not finish within the time limit ({limit}s; log: {})",
                ctx.log.display()
            ));
        }
        if !built.ok {
            return build_failed(out, &model, &near_what, &built, &ctx.log);
        }
        let near_secs = built.secs;
        // `BYTEASK_FINISH_CHECK_NEAR_ONLY`: no time for the rest; only the nearest tests run.
        let rest_left = if near_only() {
            Duration::ZERO
        } else {
            left().saturating_sub(NEAR_TEST_RESERVE)
        };
        let (rest_tx, rest_cancel) = watch::channel(false);
        let _rest_timer = CancelAt(tokio::spawn(async move {
            tokio::time::sleep(rest_left).await;
            let _ = rest_tx.send(true);
        }));
        let rest_ctx = Ctx {
            cancel: rest_cancel,
            log: ctx.log.clone(),
            jobs: ctx.jobs,
            test_timeout: ctx.test_timeout,
            progress: ctx.progress.clone(),
        };
        let rest = if rest_left.is_zero() {
            None
        } else {
            Some(exec::build(&rest_ctx, &model, &model.build_dir, &plan.targets).await)
        };
        match rest {
            Some(rest) if rest.ok => out.notes.push(format!(
                "build: ok — built only {what} in its own tree {} ({:.0}s, incremental; the nearest tests' first)",
                model.build_dir.display(),
                near_secs + rest.secs
            )),
            Some(rest) if !rest.cancelled && !rest.timed_out => {
                return build_failed(out, &model, &what, &rest, &ctx.log);
            }
            _ => {
                let skipped: Vec<String> = tests
                    .iter()
                    .filter(|i| !near.contains(i))
                    .map(|&i| model.tests[i].name.clone())
                    .collect();
                out.notes.push(format!(
                    "build: ok — built {near_what} in its own tree {} ({near_secs:.0}s, incremental)",
                    model.build_dir.display()
                ));
                if near_only() {
                    // The tests near-only leaves out are named, not "not checked" (which makes the verdict
                    // NOT RUN although the nearest tests ran and passed).
                    out.notes.push(format!(
                        "{} more test(s) the change reaches were not built or run ({NEAR_ONLY_NOTE}; build and run the others yourself if your change can affect them): {}",
                        skipped.len(),
                        tree::list(&skipped, 8)
                    ));
                } else {
                    not_checked.push(format!(
                        "{} more test(s) the change reaches, whose programs did not finish building within the time limit ({limit}s; the build so far is kept, so checking again continues it): {}",
                        skipped.len(),
                        tree::list(&skipped, 8)
                    ));
                }
                tests = near;
                plan = near_plan;
            }
        }
    } else if plan.build_all || !plan.targets.is_empty() {
        let to_build = if plan.build_all {
            Vec::new()
        } else {
            plan.targets.clone()
        };
        let built = exec::build(&ctx, &model, &model.build_dir, &to_build).await;
        out.notes.extend(built.waited.clone());
        if built.cancelled || built.timed_out {
            return SuiteRun::NotChecked(format!(
                "its build of {what} did not finish within the time limit ({limit}s; log: {})",
                ctx.log.display()
            ));
        }
        if !built.ok {
            return build_failed(out, &model, &what, &built, &ctx.log);
        }
        out.notes.push(format!(
            "build: ok — built only {what} in its own tree {} ({:.0}s, incremental)",
            model.build_dir.display(),
            built.secs
        ));
    }

    // 6. Run exactly those tests.
    if model.tests.is_empty() {
        not_checked.push(format!(
            "no tests ran (the build tree {} registers no CTest tests)",
            model.build_dir.display()
        ));
        return SuiteRun::Ran;
    }
    if tests.is_empty() {
        not_checked.push(format!(
            "no tests ran: none of the {} registered test(s) reaches the changed code",
            sel.enabled.len()
        ));
        return SuiteRun::Ran;
    }
    if left() < MIN_TEST_TIME {
        return SuiteRun::NotChecked(format!(
            "the build used the check's time limit ({limit}s) before the tests could run"
        ));
    }
    let names: Vec<String> = tests.iter().map(|&i| model.tests[i].name.clone()).collect();
    let items = vec![RunItem::Ctest {
        dir: model.build_dir.clone(),
        names,
    }];
    let command = tree::run_lines(&model, &items).join("; ");
    let ran = exec::run(&ctx, &items).await;
    if ran.cancelled {
        return SuiteRun::NotChecked(format!(
            "the test run ({command}) did not finish within the time limit ({limit}s; log: {})",
            ctx.log.display()
        ));
    }
    let count = |f: fn(TestStatus) -> bool| ran.results.values().filter(|s| f(**s)).count();
    let passed = count(|s| s.passing());
    let flaky = count(|s| s == TestStatus::Flaky);
    let failing: Vec<String> = ran
        .results
        .iter()
        .filter(|(_, s)| s.failing())
        .map(|(n, _)| n.clone())
        .collect();
    let not_run: Vec<String> = ran
        .results
        .iter()
        .filter(|(_, s)| **s == TestStatus::NotRun)
        .map(|(n, _)| n.clone())
        .collect();
    out.notes.push(format!(
        "tests: {passed} passed{}{} of {} ({command}, {:.1}s)",
        if failing.is_empty() {
            String::new()
        } else {
            format!(", {} FAILED", failing.len())
        },
        if flaky > 0 {
            format!(", {flaky} flaky (passed on the re-run)")
        } else {
            String::new()
        },
        ran.results.len(),
        ran.secs
    ));
    if !not_run.is_empty() {
        not_checked.push(format!(
            "{} selected test(s) produced no result ({}; log: {})",
            not_run.len(),
            tree::list(&not_run, 6),
            ctx.log.display()
        ));
    }
    if failing.is_empty() {
        return SuiteRun::Ran;
    }

    // 7. Failures: compared with the session-start commit (the shared base-run cache).
    let head = Head {
        sha: snapshot.head.clone(),
        root: root.clone(),
    };
    let base_results: BTreeMap<String, TestStatus> = if left() > MIN_TEST_TIME {
        let compared = base::compare(
            &ctx,
            &base::Compared {
                model: &model,
                head: &head,
                scratch: &scratch,
                targets: &plan.targets,
                items: &items,
                now: &ran.results,
                sel: &sel,
                changes: &changes,
            },
        )
        .await;
        for note in compared.notes.iter().take(2) {
            out.notes.push(note.clone());
        }
        base_run_cache::load(&BaseRunKey::new(&model.root, &head.sha, &model.config))
            .map(|run| run.results)
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let mut regressions = Vec::new();
    let mut preexisting = 0usize;
    let mut not_compared = Vec::new();
    for name in &failing {
        let shown = match ran.details.get(name).and_then(|d| d.first()) {
            Some(first) => format!("{name} ({first})"),
            None => name.clone(),
        };
        match base_results.get(name) {
            Some(status) if status.passing() => regressions.push(shown),
            Some(_) => preexisting += 1,
            None => not_compared.push(shown),
        }
    }
    if !regressions.is_empty() {
        out.problems.push(Problem {
            kind: ProblemKind::Tests,
            text: format!(
                "TESTS THAT PASS ON THE UNMODIFIED TREE NOW FAIL (re-run once): {} — log: {}. Fix the code (do not edit the tests) unless the request changes what they assert.",
                tree::list(&regressions, 8),
                ctx.log.display()
            ),
        });
    }
    if !not_compared.is_empty() {
        not_checked.push(format!(
            "{} failing test(s) could not be compared with the unmodified tree within the time limit, so they may have failed before your change: {} (log: {})",
            not_compared.len(),
            tree::list(&not_compared, 6),
            ctx.log.display()
        ));
    }
    if preexisting > 0 {
        out.notes.push(format!(
            "tests: {preexisting} failure(s) also fail on the unmodified tree (not caused by the change)"
        ));
    }
    SuiteRun::Ran
}

/// A failed build of `what`: the problem, and no tests run.
fn build_failed(
    out: &mut GateOutcome,
    model: &Model,
    what: &str,
    built: &exec::BuildResult,
    log: &Path,
) -> SuiteRun {
    let shown = if built.errors.is_empty() {
        "no recognised compiler error".to_string()
    } else {
        built
            .errors
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ")
    };
    out.problems.push(Problem {
        kind: ProblemKind::Build,
        text: format!(
            "BUILD FAILS ({what} in its own tree {}): {shown} — full log: {}",
            model.build_dir.display(),
            log.display()
        ),
    });
    out.notes
        .push("tests: not run — the build fails".to_string());
    SuiteRun::Ran
}

fn millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
