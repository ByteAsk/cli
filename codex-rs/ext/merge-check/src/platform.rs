//! Posting the decision where the merge button is: a GitHub check run, a GitLab
//! commit status plus a merge-request note, or files a Jenkins step publishes. And reading who
//! approved the change, where the platform says which version they approved.
//!
//! On every platform, only an all-PASS decision is shown as success. A NOT RUN that a
//! reviewer approved is GitHub `neutral` / GitLab `skipped` / a skipped Jenkins test, with the
//! approver named; a blocked NOT RUN is `action_required` / `failed` / a failing Jenkins step.
//!
//! Tested against local fake servers only: the request shapes follow the platforms' public API
//! documentation (GitHub REST 2022-11-28, GitLab REST v4) but have not been run against the
//! real services.

use check_result::Verdict;
use serde_json::json;

use crate::gate::Approval;
use crate::gate::Decision;

pub const CHECK_NAME: &str = "ByteAsk merge check";
const USER_AGENT: &str = "byteask-merge-check";

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| format!("could not make an HTTP client: {err}"))
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub(20);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n(cut to fit)", &text[..end])
}

/// GitHub's check-run conclusion for a decision.
pub fn github_conclusion(decision: &Decision) -> &'static str {
    match (decision.allowed, decision.verdict) {
        (true, Verdict::Pass) => "success",
        (true, _) => "neutral",
        (false, Verdict::Fail) => "failure",
        (false, _) => "action_required",
    }
}

/// GitLab's commit-status state for a decision.
pub fn gitlab_state(decision: &Decision) -> &'static str {
    match (decision.allowed, decision.verdict) {
        (true, Verdict::Pass) => "success",
        (true, _) => "skipped",
        (false, _) => "failed",
    }
}

pub struct GitHub {
    pub api: String,
    /// `owner/name`.
    pub repo: String,
    pub token: String,
    /// The pull request, for reading reviews.
    pub pull: Option<u64>,
}

impl GitHub {
    fn url(&self, path: &str) -> String {
        format!("{}/repos/{}{path}", self.api.trim_end_matches('/'), self.repo)
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// Post the decision as a completed check run on the head commit.
    pub async fn post(&self, decision: &Decision, markdown: &str) -> Result<(), String> {
        let body = json!({
            "name": CHECK_NAME,
            "head_sha": decision.binding.head_commit,
            "status": "completed",
            "conclusion": github_conclusion(decision),
            "external_id": decision.candidate_key,
            "output": {
                "title": clip(&decision.summary, 255),
                "summary": clip(markdown, 65_000),
            },
        });
        let response = self
            .request(client()?.post(self.url("/check-runs")))
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("GitHub did not answer: {err}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("GitHub refused the check run: HTTP {}", response.status()))
        }
    }

    /// Reviews that approved, with the commit each was given on.
    pub async fn approvals(&self) -> Result<Vec<Approval>, String> {
        let Some(pull) = self.pull else {
            return Ok(Vec::new());
        };
        let client = client()?;
        let pr: serde_json::Value = self
            .request(client.get(self.url(&format!("/pulls/{pull}"))))
            .send()
            .await
            .map_err(|err| format!("GitHub did not answer: {err}"))?
            .json()
            .await
            .map_err(|err| format!("GitHub's pull request did not parse: {err}"))?;
        let author = pr["user"]["login"].as_str().unwrap_or_default().to_string();
        let reviews: serde_json::Value = self
            .request(client.get(self.url(&format!("/pulls/{pull}/reviews?per_page=100"))))
            .send()
            .await
            .map_err(|err| format!("GitHub did not answer: {err}"))?
            .json()
            .await
            .map_err(|err| format!("GitHub's reviews did not parse: {err}"))?;
        Ok(reviews
            .as_array()
            .into_iter()
            .flatten()
            .filter(|review| review["state"].as_str() == Some("APPROVED"))
            .filter_map(|review| {
                let login = review["user"]["login"].as_str()?.to_string();
                let at_ms = parse_rfc3339_ms(review["submitted_at"].as_str()?)?;
                Some(Approval {
                    is_bot: review["user"]["type"].as_str() == Some("Bot") || login.ends_with("[bot]"),
                    is_author: login == author,
                    reviewer: login,
                    head_commit: review["commit_id"].as_str().unwrap_or_default().to_string(),
                    at_ms,
                })
            })
            .collect())
    }
}

pub struct GitLab {
    pub api: String,
    /// The project id or URL-encoded path.
    pub project: String,
    pub token: String,
    pub merge_request: Option<u64>,
}

impl GitLab {
    fn url(&self, path: &str) -> String {
        format!("{}/projects/{}{path}", self.api.trim_end_matches('/'), self.project)
    }

    /// A commit status on the head, and the report (with the perf card) as a note on the
    /// merge request. GitLab approvals carry no version, so they are not read: a NOT RUN
    /// there blocks unless the pipeline passes `--approvals` from its own approval step.
    pub async fn post(&self, decision: &Decision, markdown: &str) -> Result<(), String> {
        let client = client()?;
        let status = json!({
            "state": gitlab_state(decision),
            "name": CHECK_NAME,
            "description": clip(&decision.summary, 255),
        });
        let response = client
            .post(self.url(&format!("/statuses/{}", decision.binding.head_commit)))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&status)
            .send()
            .await
            .map_err(|err| format!("GitLab did not answer: {err}"))?;
        if !response.status().is_success() {
            return Err(format!("GitLab refused the commit status: HTTP {}", response.status()));
        }
        if let Some(iid) = self.merge_request {
            let response = client
                .post(self.url(&format!("/merge_requests/{iid}/notes")))
                .header("PRIVATE-TOKEN", &self.token)
                .json(&json!({ "body": clip(markdown, 1_000_000) }))
                .send()
                .await
                .map_err(|err| format!("GitLab did not answer: {err}"))?;
            if !response.status().is_success() {
                return Err(format!("GitLab refused the note: HTTP {}", response.status()));
            }
        }
        Ok(())
    }
}

/// Jenkins: a Markdown summary and a JUnit report (one test case per check: PASS passed, FAIL
/// failed, NOT RUN skipped, so a NOT RUN never reads as a pass); the step's exit status blocks.
pub fn jenkins_files(decision: &Decision, markdown: &str) -> (String, String) {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let failures = decision.checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    let skipped = decision
        .checks
        .iter()
        .filter(|c| !matches!(c.verdict, Verdict::Pass | Verdict::Fail))
        .count();
    xml.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">\n",
        escape(CHECK_NAME),
        decision.checks.len()
    ));
    for check in &decision.checks {
        xml.push_str(&format!(
            "  <testcase classname=\"byteask.merge\" name=\"{}\">",
            escape(&check.check)
        ));
        let line = escape(&check.render_line());
        match check.verdict {
            Verdict::Pass => {}
            Verdict::Fail => xml.push_str(&format!("<failure message=\"{line}\"/>")),
            _ => xml.push_str(&format!("<skipped message=\"{line}\"/>")),
        }
        xml.push_str("</testcase>\n");
    }
    xml.push_str("</testsuite>\n");
    (markdown.to_string(), xml)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `2026-10-19T10:15:30Z` (or with an offset) to Unix milliseconds.
pub fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let time = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(time.timestamp_millis()).ok()
}

#[cfg(test)]
#[path = "platform_tests.rs"]
pub(crate) mod tests;
