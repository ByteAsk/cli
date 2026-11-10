//! The platform adapters against local fake servers (no real GitHub or GitLab is contacted).

use std::sync::Arc;
use std::sync::Mutex;

use check_result::CheckResult;
use check_result::NotRunReason;
use check_result::Verdict;
use pretty_assertions::assert_eq;

use super::*;
use crate::binding::Binding;

/// One request the fake saw: method, URL, body, headers (lower-case names).
pub(crate) type Seen = (String, String, String, Vec<(String, String)>);
/// A POST the fake saw: URL, JSON body, headers.
pub(crate) type Post = (String, serde_json::Value, Vec<(String, String)>);

/// A fake API: answers GETs from a table, records every request.
pub(crate) struct FakeApi {
    pub(crate) base: String,
    pub(crate) seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeApi {
    pub(crate) fn start(gets: Vec<(&'static str, serde_json::Value)>) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", server.server_addr().to_ip().expect("ip"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let method = request.method().to_string();
                let url = request.url().to_string();
                let headers = request
                    .headers()
                    .iter()
                    .map(|h| (h.field.to_string().to_ascii_lowercase(), h.value.to_string()))
                    .collect();
                log.lock().expect("lock").push((method.clone(), url.clone(), body, headers));
                let answer = if method == "GET" {
                    gets.iter()
                        .find(|(path, _)| url.starts_with(path))
                        .map(|(_, value)| value.to_string())
                } else {
                    Some("{\"id\": 1}".to_string())
                };
                let response = match answer {
                    Some(text) => tiny_http::Response::from_string(text).with_status_code(200),
                    None => tiny_http::Response::from_string("{}").with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        Self { base, seen }
    }

    pub(crate) fn posts(&self) -> Vec<Post> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .filter(|(method, ..)| method == "POST")
            .map(|(_, url, body, headers)| {
                (url.clone(), serde_json::from_str(body).unwrap_or_default(), headers.clone())
            })
            .collect()
    }
}

fn decision(allowed: bool, verdict: Verdict, checks: Vec<CheckResult>) -> Decision {
    Decision {
        schema: crate::gate::DECISION_SCHEMA.to_string(),
        allowed,
        verdict,
        summary: format!("{} summary", verdict.word()),
        candidate_key: "k".repeat(64),
        content_key: "c".repeat(64),
        binding: Binding {
            base_commit: "b".repeat(40),
            head_commit: "h".repeat(40),
            merge_tree: "t".repeat(40),
            profile_rev: "p".into(),
            manifest_rev: "none".into(),
            baseline_rev: "x".into(),
            env: String::new(),
        },
        checks,
        approved_by: None,
        ignored: Vec::new(),
        perf_card: None,
    }
}

/// Only an all-PASS decision is success on any platform.
#[test]
fn only_pass_is_success_on_every_platform() {
    let pass = decision(true, Verdict::Pass, vec![]);
    let fail = decision(false, Verdict::Fail, vec![]);
    let blocked = decision(false, Verdict::NotRun, vec![]);
    let approved = decision(true, Verdict::NotRun, vec![]);
    assert_eq!(
        [&pass, &fail, &blocked, &approved].map(github_conclusion),
        ["success", "failure", "action_required", "neutral"]
    );
    assert_eq!(
        [&pass, &fail, &blocked, &approved].map(gitlab_state),
        ["success", "failed", "failed", "skipped"]
    );
}

#[tokio::test]
async fn github_gets_a_completed_check_run_on_the_head() {
    let api = FakeApi::start(vec![]);
    let github = GitHub {
        api: api.base.clone(),
        repo: "acme/book".into(),
        token: "secret-token".into(),
        pull: None,
    };
    let d = decision(false, Verdict::NotRun, vec![]);
    github.post(&d, "# report").await.expect("posted");
    let posts = api.posts();
    assert_eq!(posts.len(), 1);
    let (url, body, headers) = &posts[0];
    assert_eq!(url, "/repos/acme/book/check-runs");
    assert_eq!(body["head_sha"], "h".repeat(40));
    assert_eq!(body["conclusion"], "action_required");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["external_id"], "k".repeat(64));
    assert_eq!(body["output"]["summary"], "# report");
    assert!(headers.contains(&("authorization".into(), "Bearer secret-token".into())));
}

/// GitHub reviews carry the commit they were given on; bots and the author never count.
#[tokio::test]
async fn github_reviews_become_approvals_with_their_commit() {
    let api = FakeApi::start(vec![
        (
            "/repos/acme/book/pulls/7/reviews",
            serde_json::json!([
                {"user": {"login": "alice", "type": "User"}, "state": "APPROVED",
                 "commit_id": "h1", "submitted_at": "2026-10-19T10:00:00Z"},
                {"user": {"login": "bob", "type": "User"}, "state": "CHANGES_REQUESTED",
                 "commit_id": "h1", "submitted_at": "2026-10-19T10:01:00Z"},
                {"user": {"login": "helper[bot]", "type": "Bot"}, "state": "APPROVED",
                 "commit_id": "h1", "submitted_at": "2026-10-19T10:02:00Z"},
                {"user": {"login": "carol", "type": "User"}, "state": "APPROVED",
                 "commit_id": "h1", "submitted_at": "2026-10-19T10:03:00+02:00"}
            ]),
        ),
        ("/repos/acme/book/pulls/7", serde_json::json!({"user": {"login": "carol"}})),
    ]);
    let github = GitHub {
        api: api.base.clone(),
        repo: "acme/book".into(),
        token: "t".into(),
        pull: Some(7),
    };
    let approvals = github.approvals().await.expect("approvals");
    let summary: Vec<(&str, bool, bool, u64)> = approvals
        .iter()
        .map(|a| (a.reviewer.as_str(), a.is_bot, a.is_author, a.at_ms))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("alice", false, false, 1_792_404_000_000),
            ("helper[bot]", true, false, 1_792_404_120_000),
            ("carol", false, true, 1_792_396_980_000),
        ]
    );
    assert!(approvals.iter().all(|a| a.head_commit == "h1"));
}

#[tokio::test]
async fn gitlab_gets_a_commit_status_and_a_note() {
    let api = FakeApi::start(vec![]);
    let gitlab = GitLab {
        api: api.base.clone(),
        project: "42".into(),
        token: "glpat".into(),
        merge_request: Some(5),
    };
    let d = decision(true, Verdict::NotRun, vec![]);
    gitlab.post(&d, "card").await.expect("posted");
    let posts = api.posts();
    assert_eq!(posts[0].0, format!("/projects/42/statuses/{}", "h".repeat(40)));
    assert_eq!(posts[0].1["state"], "skipped", "an approved NOT RUN is never success");
    assert!(posts[0].2.contains(&("private-token".into(), "glpat".into())));
    assert_eq!(posts[1].0, "/projects/42/merge_requests/5/notes");
    assert_eq!(posts[1].1["body"], "card");
}

#[test]
fn jenkins_reports_not_run_as_skipped_never_passed() {
    let checks = vec![
        CheckResult::pass("build", "ok"),
        CheckResult::fail("tests", "1 failed"),
        CheckResult::not_run("manifest", NotRunReason::BuildFailed, "build failed <&>", ""),
    ];
    let (_, xml) = jenkins_files(&decision(false, Verdict::Fail, checks), "md");
    let cases = crate::junit::parse(&xml).expect("valid JUnit");
    let status: Vec<_> = cases.iter().map(|c| (c.name.as_str(), c.status)).collect();
    assert_eq!(
        status,
        vec![
            ("build", crate::junit::CaseStatus::Passed),
            ("tests", crate::junit::CaseStatus::Failed),
            ("manifest", crate::junit::CaseStatus::Skipped),
        ]
    );
}

#[test]
fn rfc3339_times() {
    assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01Z"), Some(1_000));
    assert_eq!(parse_rfc3339_ms("garbage"), None);
}
