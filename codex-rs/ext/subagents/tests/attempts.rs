//! Tier B (adaptive multi-attempt) end to end with a stub child host (no model): scripted
//! attempts edit their worktrees; the real evidence collection, sequential spawning, evaluation
//! bench, selector, apply step and cleanup run against a temp git repository.
// Test helpers outside `#[test]` bodies (here and in `common`) may panic on setup failures.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_subagents::attempts::AttemptsMode;
use codex_subagents::attempts::AttemptsSettings;
use codex_subagents::host::BoxFut;
use codex_subagents::host::ChildHandle;
use codex_subagents::host::ChildHost;
use codex_subagents::host::ChildSpec;
use codex_subagents::host::ChildState;
use codex_subagents::host::Role;
use codex_subagents::orchestrator::Orchestration;
use codex_subagents::orchestrator::Params;
use codex_subagents::orchestrator::StepCounters;
use codex_subagents::settings::Settings;
use common::*;
use pretty_assertions::assert_eq;

const REPORT: &str =
    "BYTEASK_REPORT {\"build\": \"sh build.sh\", \"test\": \"sh tests/run.sh\", \"run\": \"\"}";
const REPORT_WITH_TEST: &str = "BYTEASK_REPORT {\"build\": \"sh build.sh\", \"test\": \"sh tests/run.sh\", \"run\": \"sh tests/add_case.sh\"}";
const ADD_CASE: &str = "#!/bin/sh\nr=$(sh out/calc.sh add 2 3)\n[ \"$r\" = 5 ] && echo 'ok 1 - add' || { echo 'not ok 1 - add'; exit 1; }\n";
const BREAK_MUL: &str = "echo $(( $2 * $3 + 1 ))";

/// Child ids unique across the tests of this binary (the token meter is process-wide).
struct Renamed {
    inner: Arc<dyn ChildHandle>,
    id: String,
}

impl ChildHandle for Renamed {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn state(&self) -> BoxFut<'_, ChildState> {
        self.inner.state()
    }
    fn interrupt(&self) -> BoxFut<'_, ()> {
        self.inner.interrupt()
    }
    fn inject(&self, text: String) -> BoxFut<'_, bool> {
        self.inner.inject(text)
    }
    fn shutdown(&self) -> BoxFut<'_, ()> {
        self.inner.shutdown()
    }
}

/// The stub host with per-test child ids, optionally charging tokens per spawned child.
struct Host {
    fake: FakeHost,
    prefix: String,
    tokens_per_child: u64,
}

impl ChildHost for Host {
    fn spawn(&self, spec: ChildSpec) -> BoxFut<'_, Result<Arc<dyn ChildHandle>, String>> {
        Box::pin(async move {
            let inner = self.fake.spawn(spec).await?;
            let id = format!("{}-{}", self.prefix, inner.id());
            if self.tokens_per_child > 0 {
                codex_subagents::attempts::meter::track(&id);
                codex_subagents::attempts::meter::record(&id, self.tokens_per_child);
            }
            Ok(Arc::new(Renamed { inner, id }) as Arc<dyn ChildHandle>)
        })
    }

    fn notify_root(&self, text: String) -> BoxFut<'_, bool> {
        self.fake.notify_root(text)
    }
}

fn host(prefix: &str, behave: Behave) -> Arc<Host> {
    Arc::new(Host {
        fake: FakeHost::new(behave),
        prefix: prefix.to_string(),
        tokens_per_child: 0,
    })
}

fn tierb(mode: AttemptsMode, max: usize, judge: bool) -> AttemptsSettings {
    AttemptsSettings {
        mode: Some(mode),
        max_attempts: max,
        judge,
        min_attempt_secs: 60,
        writer_grace_secs: 120,
        ..AttemptsSettings::default()
    }
}

fn writer_settings(scratch: &Path, repro: bool) -> Settings {
    Settings {
        repro_tests: repro,
        scratch_root: Some(scratch.to_path_buf()),
        build_timeout_secs: 60,
        test_timeout_secs: 60,
        ..Settings::default()
    }
}

struct Run {
    repo: PathBuf,
    home: PathBuf,
    scratch: PathBuf,
    _tmp: tempfile::TempDir,
}

fn setup() -> Run {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    Run {
        home: tmp.path().join("home"),
        scratch: tmp.path().join("scratch"),
        repo,
        _tmp: tmp,
    }
}

async fn start(
    r: &Run,
    host: Arc<Host>,
    settings: Settings,
    tierb: AttemptsSettings,
    id: &str,
) -> Arc<Orchestration> {
    let mode = tierb.mode.unwrap_or(AttemptsMode::Off);
    let params = Params {
        run_repro: settings.repro_tests,
        settings,
        repo: r.repo.clone(),
        task: TASK.to_string(),
        root_id: id.to_string(),
        codex_home: r.home.clone(),
    };
    let orch = Orchestration::start(params, host, Arc::new(StepCounters::default()))
        .await
        .expect("start");
    orch.attempts.configure(tierb, mode);
    orch
}

fn report_json(home: &Path, id: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(home.join("byteask-subagents").join(format!("{id}.json")))
        .expect("report written");
    serde_json::from_str(&text).expect("report is json")
}

fn assert_clean(r: &Run, id: &str) {
    assert_eq!(
        extra_worktrees(&r.repo),
        Vec::<String>::new(),
        "worktrees left behind"
    );
    assert_eq!(byteask_refs(&r.repo), "", "snapshot ref left behind");
    assert!(
        !r.scratch.join("byteask-subagents").join(id).exists(),
        "scratch dir left behind"
    );
}

fn attempt_roles(h: &Host) -> Vec<Role> {
    h.fake
        .roles()
        .into_iter()
        .filter(|r| matches!(r, Role::Attempt(_)))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weak_attempt_one_spawns_attempt_two_which_is_selected_and_applied() {
    let r = setup();
    let h = host(
        "weak",
        Box::new(|spec: &ChildSpec| match spec.role {
            Role::Repro => repro_writer(spec),
            Role::Attempt(_) => {
                // Fresh context: only the task and the brief; the held-out tests are not shown.
                assert!(
                    spec.prompt
                        .starts_with(codex_subagents::attempts::prompts::ATTEMPT_MARK)
                );
                assert!(spec.prompt.contains(TASK));
                assert!(!spec.prompt.contains("independent reproduction tests"));
                fix(&spec.cwd, FIX_GOOD);
                Some(format!("Fixed.\n{REPORT}"))
            }
            Role::Judge => Some("WINNER: A".to_string()),
        }),
    );
    let orch = start(
        &r,
        h.clone(),
        writer_settings(&r.scratch, true),
        tierb(AttemptsMode::Adaptive, 2, true),
        "w1",
    )
    .await;
    // Attempt 1 (the root) gets it wrong and says it could not verify.
    fix(&r.repo, FIX_WRONG);
    orch.attempts
        .observe_turn_end("turn-1", Some("Changed add. I could not verify it."));
    orch.finalize().await;

    let rep = report_json(&r.home, "w1");
    let a = &rep["attempts"];
    let triggers = a["triggers"].as_array().expect("triggers").clone();
    assert!(
        triggers.contains(&serde_json::json!("NoPassingTest")),
        "{triggers:?}"
    );
    assert!(
        triggers.contains(&serde_json::json!("UncertaintyMarkers")),
        "{triggers:?}"
    );
    assert_eq!(
        a["evidence"]["tests"]["Failed"],
        serde_json::json!(["independent"])
    );
    assert_eq!(attempt_roles(&h), vec![Role::Attempt(2)]);
    assert!(
        h.fake
            .spawned
            .lock()
            .unwrap()
            .iter()
            .any(|(role, _, high)| *role == Role::Attempt(2) && *high)
    );
    assert_eq!(a["selection"]["winner"], 2);
    assert_eq!(a["selection"]["reason"], "highest cross-attempt test score");
    assert_eq!(a["applied"], 2);
    assert_eq!(a["stopped"], "attempt cap (2) reached");
    assert_eq!(a["bench"]["reused_writer_worktree"], true);
    assert_eq!(a["candidates"][0]["cells"]["independent"], "Fail");
    assert_eq!(a["candidates"][1]["cells"]["independent"], "Pass");
    assert!(
        a["summary"]
            .as_str()
            .unwrap_or("")
            .starts_with("tier-b mode adaptive")
    );
    assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_GOOD));
    assert_eq!(git(&r.repo, &["status", "--porcelain"]).trim(), "M calc.sh");
    assert_clean(&r, "w1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strong_evidence_stops_at_attempt_one() {
    let r = setup();
    let h = host(
        "strong",
        Box::new(|spec: &ChildSpec| match spec.role {
            Role::Repro => repro_writer(spec),
            _ => panic!("no attempt or judge may start on strong evidence"),
        }),
    );
    let orch = start(
        &r,
        h.clone(),
        writer_settings(&r.scratch, true),
        tierb(AttemptsMode::Adaptive, 4, true),
        "s1",
    )
    .await;
    fix(&r.repo, FIX_GOOD);
    // The finish gate passed with builds and tests for this turn.
    let log = codex_cpp_tools::finish_gate_runs_log(&r.repo);
    std::fs::create_dir_all(log.parent().expect("dir")).expect("mkdir");
    std::fs::write(&log, "{\"turn\":\"turn-1\",\"round\":0,\"problems\":[],\"notes\":[\"build: ok\",\"tests: 1 passed\"]}\n")
        .expect("gate log");
    orch.attempts
        .observe_turn_end("turn-1", Some("Fixed add; the tests pass."));
    orch.finalize().await;
    let _ = std::fs::remove_file(&log);

    let a = report_json(&r.home, "s1")["attempts"].clone();
    assert_eq!(a["triggers"], serde_json::json!([]));
    assert_eq!(a["evidence"]["gate"], "Passed");
    assert_eq!(
        a["evidence"]["tests"]["Passed"],
        serde_json::json!(["independent"])
    );
    assert_eq!(
        a["stopped"],
        "strong evidence after attempt 1 (no trigger fired)"
    );
    assert_eq!(a["applied"], 1);
    assert_eq!(h.fake.roles(), vec![Role::Repro]);
    assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_GOOD));
    assert_clean(&r, "s1");
}

/// The writer off (its default), so no qualified test exists: a clean gate record and a
/// confident final message mean ONE attempt — no sub-agent of any kind starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_writer_clean_evidence_keeps_one_attempt() {
    let r = setup();
    let h = host(
        "nowriter",
        Box::new(|_spec: &ChildSpec| -> Option<String> {
            panic!("no sub-agent may start: the test writer is off and the evidence is clean")
        }),
    );
    let orch = start(
        &r,
        h.clone(),
        writer_settings(&r.scratch, false),
        tierb(AttemptsMode::Adaptive, 4, true),
        "n1",
    )
    .await;
    fix(&r.repo, FIX_GOOD);
    let log = codex_cpp_tools::finish_gate_runs_log(&r.repo);
    std::fs::create_dir_all(log.parent().expect("dir")).expect("mkdir");
    std::fs::write(
        &log,
        "{\"turn\":\"turn-1\",\"round\":0,\"problems\":[],\"notes\":[\"build: ok\",\"tests: 1 passed or skipped\"],\"still_open\":[],\"not_checked\":[]}\n",
    )
    .expect("gate log");
    orch.attempts
        .observe_turn_end("turn-1", Some("Fixed add; the tests pass."));
    orch.finalize().await;
    let _ = std::fs::remove_file(&log);

    let a = report_json(&r.home, "n1")["attempts"].clone();
    assert_eq!(a["triggers"], serde_json::json!([]));
    assert_eq!(a["evidence"]["gate"], "Passed");
    assert_eq!(a["evidence"]["tests"], "NoTests");
    assert_eq!(
        a["stopped"],
        "strong evidence after attempt 1 (no trigger fired)"
    );
    assert_eq!(a["applied"], 1);
    assert_eq!(h.fake.roles(), Vec::<Role>::new());
    assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_GOOD));
    assert_clean(&r, "n1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adaptive_escalates_while_attempts_disagree_then_selects() {
    let r = setup();
    let h = host(
        "esc",
        Box::new(|spec: &ChildSpec| match spec.role {
            Role::Attempt(2) => {
                // Fixes add but breaks mul: a regression in the existing suite.
                let text = read(&spec.cwd, "calc.sh")
                    .replace(BUG_LINE, FIX_GOOD)
                    .replace("echo $(( $2 * $3 ))", BREAK_MUL);
                write(&spec.cwd, "calc.sh", &text);
                Some(format!("Fixed.\n{REPORT}"))
            }
            Role::Attempt(3) => {
                // Fixes add and leaves its own test in the worktree (split off automatically).
                fix(&spec.cwd, FIX_GOOD);
                write(&spec.cwd, "tests/add_case.sh", ADD_CASE);
                Some(format!("Fixed, with a test.\n{REPORT_WITH_TEST}"))
            }
            Role::Attempt(_) => {
                fix(&spec.cwd, FIX_GOOD);
                Some(format!("Fixed.\n{REPORT}"))
            }
            Role::Judge => panic!("the execution signals decide here"),
            Role::Repro => None,
        }),
    );
    let orch = start(
        &r,
        h.clone(),
        writer_settings(&r.scratch, false),
        tierb(AttemptsMode::Adaptive, 4, true),
        "e1",
    )
    .await;
    fix(&r.repo, FIX_WRONG);
    // The writer is off, so no test evidence: the agent's own hedging is the weak signal.
    orch.attempts
        .observe_turn_end("turn-1", Some("Done, but I could not verify it."));
    orch.finalize().await;

    let a = report_json(&r.home, "e1")["attempts"].clone();
    assert_eq!(a["triggers"], serde_json::json!(["UncertaintyMarkers"]));
    assert_eq!(
        attempt_roles(&h),
        vec![Role::Attempt(2), Role::Attempt(3), Role::Attempt(4)]
    );
    let esc = a["escalations"].as_array().expect("escalations");
    assert_eq!(esc.len(), 2, "{esc:?}");
    assert!(esc.iter().all(|e| e["disagree"] == true), "{esc:?}");
    let cand = |k: u64| {
        a["candidates"]
            .as_array()
            .expect("candidates")
            .iter()
            .find(|c| c["attempt"] == k)
            .cloned()
            .expect("candidate")
    };
    assert_eq!(cand(2)["regressions"], serde_json::json!(["tap:mul"]));
    assert_eq!(cand(1)["cells"]["attempt-3"], "Fail");
    assert_eq!(
        cand(3)["cells"]["attempt-3"],
        "Pass",
        "the author's own test is valid"
    );
    assert_eq!(cand(4)["cells"]["attempt-3"], "Pass");
    assert_eq!(a["pooled"][0]["valid"], true);
    // 3 and 4 made the same fix (3's test was split off): the lower number wins.
    assert_eq!(a["selection"]["winner"], 3);
    assert_eq!(
        a["selection"]["reason"],
        "the remaining candidates made the same change"
    );
    assert_eq!(a["applied"], 3);
    assert_eq!(a["stopped"], "attempt cap (4) reached");
    assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_GOOD));
    assert_eq!(
        git(&r.repo, &["status", "--porcelain"]).trim(),
        "M calc.sh",
        "no test file in the final tree"
    );
    assert_clean(&r, "e1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consistent_judge_replaces_attempt_one_but_position_bias_does_not() {
    for (bias, expect_winner) in [(false, 2), (true, 1)] {
        let r = setup();
        let h = host(
            &format!("judge-{bias}"),
            Box::new(move |spec: &ChildSpec| match spec.role {
                Role::Attempt(_) => {
                    fix(&spec.cwd, FIX_GOOD);
                    Some(format!("Fixed.\n{REPORT}"))
                }
                Role::Judge => {
                    if bias {
                        return Some("WINNER: A".to_string());
                    }
                    let a = spec.prompt.split("<patch id=\"B\">").next().unwrap_or("");
                    Some(
                        if a.contains("$2 + $3") {
                            "WINNER: A"
                        } else {
                            "WINNER: B"
                        }
                        .to_string(),
                    )
                }
                Role::Repro => None,
            }),
        );
        let id = format!("j-{bias}");
        let orch = start(
            &r,
            h.clone(),
            writer_settings(&r.scratch, false),
            tierb(AttemptsMode::Fixed(2), 4, true),
            &id,
        )
        .await;
        fix(&r.repo, FIX_SWAPPED);
        orch.attempts.observe_turn_end("turn-1", Some("Done."));
        orch.finalize().await;
        let a = report_json(&r.home, &id)["attempts"].clone();
        assert_eq!(a["selection"]["judged"], true, "{}", a["selection"]);
        assert_eq!(
            a["selection"]["winner"], expect_winner,
            "bias={bias}: {}",
            a["selection"]
        );
        let expected = if expect_winner == 2 {
            FIX_GOOD
        } else {
            FIX_SWAPPED
        };
        assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, expected));
        assert_eq!(
            h.fake.roles().iter().filter(|r| **r == Role::Judge).count(),
            2,
            "both orders"
        );
        assert_clean(&r, &id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_little_time_left_starts_no_attempt() {
    let r = setup();
    let h = host(
        "late",
        Box::new(|_spec: &ChildSpec| -> Option<String> { panic!("no child may start") }),
    );
    let mut s = tierb(AttemptsMode::Adaptive, 4, true);
    s.task_timeout_secs = Some(60); // the margin (>= 120 s) leaves nothing
    let orch = start(&r, h.clone(), writer_settings(&r.scratch, false), s, "t1").await;
    fix(&r.repo, FIX_WRONG);
    orch.attempts
        .observe_turn_end("turn-1", Some("I could not verify the fix."));
    orch.finalize().await;
    let a = report_json(&r.home, "t1")["attempts"].clone();
    assert_eq!(a["triggers"], serde_json::json!(["UncertaintyMarkers"]));
    assert!(
        a["stopped"]
            .as_str()
            .unwrap_or("")
            .starts_with("only 0s of working time left"),
        "{}",
        a["stopped"]
    );
    assert_eq!(a["budget"]["source"], "task timeout");
    assert_eq!(a["budget"]["margin_secs"], 120);
    assert_eq!(a["applied"], 1);
    assert!(h.fake.roles().is_empty());
    assert_eq!(read(&r.repo, "calc.sh"), CALC.replace(BUG_LINE, FIX_WRONG));
    assert_clean(&r, "t1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_that_edits_the_real_repo_is_disqualified_and_the_tree_restored() {
    let r = setup();
    let real = r.repo.clone();
    let h = host(
        "guard",
        Box::new(move |spec: &ChildSpec| match spec.role {
            Role::Attempt(_) => {
                write(&real, "calc.sh", &CALC.replace(BUG_LINE, FIX_GOOD)); // the wrong copy
                Some(format!("Fixed.\n{REPORT}"))
            }
            _ => None,
        }),
    );
    let orch = start(
        &r,
        h.clone(),
        writer_settings(&r.scratch, false),
        tierb(AttemptsMode::Fixed(2), 4, true),
        "g1",
    )
    .await;
    fix(&r.repo, FIX_WRONG);
    orch.finalize().await;
    let a = report_json(&r.home, "g1")["attempts"].clone();
    let disq = a["attempts"][0]["disqualified"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        disq.starts_with("changed the real repository (calc.sh); restored: true"),
        "{disq}"
    );
    assert_eq!(a["selection"]["winner"], 1);
    assert_eq!(a["applied"], 1);
    assert_eq!(
        read(&r.repo, "calc.sh"),
        CALC.replace(BUG_LINE, FIX_WRONG),
        "the root's change is back"
    );
    assert_clean(&r, "g1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_cap_stops_further_attempts() {
    let r = setup();
    let h = Arc::new(Host {
        fake: FakeHost::new(Box::new(|spec: &ChildSpec| match spec.role {
            Role::Attempt(_) => {
                fix(&spec.cwd, FIX_GOOD);
                Some(format!("Fixed.\n{REPORT}"))
            }
            _ => None,
        })),
        prefix: "tokens".to_string(),
        tokens_per_child: 250_000,
    });
    let mut s = tierb(AttemptsMode::Fixed(3), 4, false);
    s.max_tokens = 200_000;
    let orch = start(&r, h.clone(), writer_settings(&r.scratch, false), s, "k1").await;
    fix(&r.repo, FIX_WRONG);
    orch.finalize().await;
    let a = report_json(&r.home, "k1")["attempts"].clone();
    assert_eq!(attempt_roles(&h), vec![Role::Attempt(2)]);
    assert_eq!(a["attempts"][0]["stopped"], "token cap");
    assert!(
        a["stopped"]
            .as_str()
            .unwrap_or("")
            .starts_with("token cap reached"),
        "{}",
        a["stopped"]
    );
    assert_eq!(a["tokens_total"], 250_000);
    assert_clean(&r, "k1");
}
