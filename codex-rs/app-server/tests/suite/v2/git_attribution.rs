//! ByteAsk: commit attribution follows two switches, the served catalog's
//! `byteask_attribution.enabled` (off unless literally `true`) and the user's
//! `[features] commit_attribution`, and never the ChatGPT backend's workspace setting.
//! These replace upstream's tests of that setting, which ByteAsk no longer reads.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::to_response;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BYTEASK_TRAILER: &str = "Co-authored-by: ByteAsk <noreply@byteask.ai>";
const BYTEASK_PR_LINE: &str = "Generated with [ByteAsk](https://byteask.ai/).";
const ATTRIBUTION_DISABLED: &str = "ByteAsk commit and pull request attribution is disabled";
// Upstream's own text, as a history written by an upstream build would carry it.
const UPSTREAM_TRAILER: &str = "Co-authored-by: Codex <noreply@openai.com>";
const UPSTREAM_PR_LINE: &str = "Generated with [Codex](https://openai.com/codex/).";
const LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS: &str = "\
When you write or edit a git commit message, ensure the message ends with this trailer exactly once:
Co-authored-by: Codex <noreply@openai.com>

Rules:
- Keep existing trailers and append this trailer at the end if missing.
- Do not duplicate this trailer if it already exists.
- Keep one blank line between the commit body and trailer block.";

#[derive(Clone, Copy)]
enum LegacyAttribution {
    /// Upstream's older commit-only instruction.
    CommitOnly,
    /// Upstream's enabled world-state fragment.
    UpstreamEnabled,
}

/// A one-model catalog from the bundled models.json, carrying `switch` as its
/// `byteask_attribution` object (or none).
fn write_catalog(path: &Path, switch: Option<&Value>) -> Result<String> {
    let bundled = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../models-manager/models.json"
    ))?;
    let mut catalog: Value = serde_json::from_str(&bundled)?;
    let first = catalog["models"]
        .as_array()
        .and_then(|models| models.first())
        .cloned()
        .context("bundled models.json has no models")?;
    let slug = first["slug"].as_str().context("model without a slug")?.to_string();
    catalog["models"] = json!([first]);
    let object = catalog.as_object_mut().context("catalog is not an object")?;
    object.remove("byteask_attribution");
    if let Some(switch) = switch {
        object.insert("byteask_attribution".to_string(), switch.clone());
    }
    std::fs::write(path, catalog.to_string())?;
    Ok(slug)
}

async fn mock_server_that_must_not_be_asked_for_settings() -> MockServer {
    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/config/bundle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    // ByteAsk never asks the ChatGPT backend whether to attribute.
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/settings/user"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"commit_attribution_enabled": true})),
        )
        .expect(0)
        .mount(&server)
        .await;
    server
}

fn write_config(
    codex_home: &Path,
    server: &MockServer,
    catalog: &Path,
    model: &str,
    opted_out: bool,
) -> Result<()> {
    let mut config = MockResponsesConfig::new(&server.uri())
        .with_model(model)
        .with_root_config(&format!("model_catalog_json = \"{}\"", catalog.display()))
        .with_root_config(&format!(
            "chatgpt_base_url = \"{}/backend-api\"",
            server.uri()
        ));
    if opted_out {
        config = config.disable_feature(Feature::CommitAttribution);
    }
    config.write(codex_home)?;
    Ok(())
}

async fn start_app_server(codex_home: &Path) -> Result<TestAppServer> {
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home)
        .with_env_overrides(&[("OPENAI_API_KEY", None), ("CODEX_ACCESS_TOKEN", None)])
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, app_server.initialize()).await??;
    Ok(app_server)
}

fn counts(developer_text: &str) -> (usize, usize, usize) {
    (
        developer_text.matches(BYTEASK_TRAILER).count(),
        developer_text.matches(BYTEASK_PR_LINE).count(),
        developer_text.matches(ATTRIBUTION_DISABLED).count(),
    )
}

#[test_case(Some(json!({"enabled": true})), false, 1; "switch_on")]
#[test_case(Some(json!({"enabled": false})), false, 0; "switch_off")]
#[test_case(None, false, 0; "no_switch_in_the_catalog")]
#[test_case(Some(json!({"enabled": "yes"})), false, 0; "malformed_switch")]
#[test_case(Some(json!({"enabled": true})), true, 0; "user_opted_out")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attribution_follows_the_served_switch_and_the_user_setting(
    switch: Option<Value>,
    opted_out: bool,
    expected: usize,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = mock_server_that_must_not_be_asked_for_settings().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        ["First", "Second"]
            .into_iter()
            .map(create_final_assistant_message_sse_response)
            .collect::<Result<Vec<_>>>()?,
    )
    .await;
    let codex_home = TempDir::new()?;
    let catalog = codex_home.path().join("catalog.json");
    let model = write_catalog(&catalog, switch.as_ref())?;
    write_config(codex_home.path(), &server, &catalog, &model, opted_out)?;

    let mut app_server = start_app_server(codex_home.path()).await?;
    let request_id = app_server
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = read_response(&mut app_server, request_id).await?;
    run_turn(&mut app_server, &thread.id, "First turn").await?;
    run_turn(&mut app_server, &thread.id, "Second turn").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        // Sent once, then carried in history: exactly `expected` copies on every turn.
        let developer_text = request.message_input_texts("developer").join("\n");
        assert_eq!(counts(&developer_text), (expected, expected, 0));
    }
    server.verify().await;
    Ok(())
}

#[test_case(LegacyAttribution::CommitOnly, false; "commit_only_resumed_off")]
#[test_case(LegacyAttribution::UpstreamEnabled, false; "upstream_enabled_resumed_off")]
#[test_case(LegacyAttribution::CommitOnly, true; "commit_only_resumed_on")]
#[test_case(LegacyAttribution::UpstreamEnabled, true; "upstream_enabled_resumed_on")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_replaces_an_upstream_attribution_fragment(
    legacy_attribution: LegacyAttribution,
    resumed_on: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = mock_server_that_must_not_be_asked_for_settings().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        ["Initial", "Resumed", "Resumed again"]
            .into_iter()
            .map(create_final_assistant_message_sse_response)
            .collect::<Result<Vec<_>>>()?,
    )
    .await;
    let codex_home = TempDir::new()?;
    let catalog = codex_home.path().join("catalog.json");
    let model = write_catalog(&catalog, Some(&json!({"enabled": true})))?;
    write_config(codex_home.path(), &server, &catalog, &model, false)?;

    let mut app_server = start_app_server(codex_home.path()).await?;
    let request_id = app_server
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = read_response(&mut app_server, request_id).await?;
    run_turn(&mut app_server, &thread.id, "persist enabled attribution").await?;
    let rollout_path: PathBuf = thread
        .path
        .context("initial thread should have a rollout path")?;
    let status = timeout(DEFAULT_READ_TIMEOUT, app_server.shutdown_gracefully()).await??;
    anyhow::ensure!(status.success(), "initial app-server did not exit successfully");
    replace_attribution_fragment_with_legacy(&rollout_path, legacy_attribution)?;
    // The switch is read once per process, so the resumed process sees the new value.
    write_catalog(&catalog, Some(&json!({"enabled": resumed_on})))?;

    let mut app_server = start_app_server(codex_home.path()).await?;
    let request_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse { thread, .. } = read_response(&mut app_server, request_id).await?;
    run_turn(&mut app_server, &thread.id, "resumed").await?;
    run_turn(&mut app_server, &thread.id, "resumed again").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let initial = requests[0].message_input_texts("developer").join("\n");
    assert_eq!(counts(&initial), (1, 1, 0));
    for request in &requests[1..] {
        let developer_text = request.message_input_texts("developer").join("\n");
        // The upstream instruction stays in history either way; what matters is that
        // exactly one ByteAsk notice follows it, and it is not repeated on the next turn.
        assert_eq!(developer_text.matches(UPSTREAM_TRAILER).count(), 1);
        if resumed_on {
            assert_eq!(counts(&developer_text), (1, 1, 0));
        } else {
            assert_eq!(counts(&developer_text), (0, 0, 1));
        }
    }
    server.verify().await;
    Ok(())
}

fn replace_attribution_fragment_with_legacy(
    rollout_path: &Path,
    legacy_attribution: LegacyAttribution,
) -> Result<()> {
    let rollout = std::fs::read_to_string(rollout_path)?;
    let mut replaced = false;
    let mut removed_saved_attribution = false;
    let lines = rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut line = serde_json::from_str::<RolloutLine>(line)?;
            if let RolloutItem::ResponseItem(response_item) = &mut line.item
                && let ResponseItem::Message { role, content, .. } = &mut response_item.item
                && role == "developer"
            {
                for item in content {
                    if let ContentItem::InputText { text } = item
                        && text.contains("<git_attribution>")
                    {
                        *text = match legacy_attribution {
                            LegacyAttribution::CommitOnly => {
                                LEGACY_COMMIT_ATTRIBUTION_INSTRUCTIONS.to_string()
                            }
                            LegacyAttribution::UpstreamEnabled => text
                                .replace(BYTEASK_TRAILER, UPSTREAM_TRAILER)
                                .replace(BYTEASK_PR_LINE, UPSTREAM_PR_LINE),
                        };
                        replaced = true;
                    }
                }
            }
            if let RolloutItem::WorldState(world_state) = &mut line.item
                && world_state.state.remove("git_attribution").is_some()
            {
                removed_saved_attribution = true;
            }
            serde_json::to_string(&line)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    anyhow::ensure!(replaced, "rollout did not contain git attribution context");
    anyhow::ensure!(
        removed_saved_attribution,
        "rollout did not contain saved git attribution state"
    );
    std::fs::write(rollout_path, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

async fn read_response<T: DeserializeOwned>(
    app_server: &mut TestAppServer,
    request_id: i64,
) -> Result<T> {
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response(response)
}

async fn run_turn(app_server: &mut TestAppServer, thread_id: &str, text: &str) -> Result<()> {
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    Ok(())
}
