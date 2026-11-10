//! What answers a context tool: the installed context engine's query server.
//!
//! [`Backend`] is the seam the tools are tested through; [`EngineBackend`]
//! is the real one. Before the first query it asks the server for its tool
//! list and compares it with the tools this build of ByteAsk offers. A
//! different list means the installed context engine and this ByteAsk are
//! from different releases, and every call answers NOT RUN naming the fix,
//! rather than sending arguments the server may read differently.

use std::time::Duration;

use codex_extension_api::ToolSpec;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::client::Client;
use crate::client::ClientError;
use crate::client::Launch;
use crate::engine::Workspace;

/// How a call ended, before it is turned into tool text.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The server's `result`: `content`, `structuredContent`, `isError`.
    Answer(serde_json::Value),
    /// The call never reached the server or got no answer.
    Failed(ClientError),
    /// The model's arguments were refused before any query.
    BadArguments(String),
    /// The installed context engine does not match this ByteAsk build.
    Mismatch(String),
}

pub trait Backend: Send + Sync {
    /// The worktree passed when the model names none.
    fn worktree(&self) -> Option<&str>;

    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send + 'a>>;
}

/// The real backend: one query server per thread.
pub struct EngineBackend {
    client: Client,
    user: String,
    worktree: String,
    /// The server's tool list matched ours (checked once per server life).
    verified: AtomicBool,
}

impl EngineBackend {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            client: Client::new(launch(workspace)),
            user: workspace.engine.user.clone(),
            worktree: workspace.project.root.to_string_lossy().into_owned(),
            verified: AtomicBool::new(false),
        }
    }

    /// `None` = verified; otherwise the outcome to answer with.
    async fn check_version(&self, timeout: Duration) -> Option<Outcome> {
        if self.verified.load(Ordering::Acquire) {
            return None;
        }
        match self
            .client
            .request("tools/list", serde_json::json!({}), timeout)
            .await
        {
            Ok(listed) => match tool_list_matches(&listed) {
                Ok(()) => {
                    self.verified.store(true, Ordering::Release);
                    None
                }
                Err(problem) => Some(Outcome::Mismatch(problem)),
            },
            Err(err) => Some(Outcome::Failed(err)),
        }
    }
}

/// How the query server is started: the system Python, isolated (`-I`: no
/// user site-packages, no PYTHON* variables, no script directory on the
/// path), a minimal environment, scratch files under the index data.
pub fn launch(workspace: &Workspace) -> Launch {
    let engine = &workspace.engine;
    let mut env = vec![
        (
            "BCTX_TMP".to_string(),
            engine.data.join("tmp").to_string_lossy().into_owned(),
        ),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ];
    // git answers "how many commits behind"; it is found on the user's PATH.
    if let Some(path) = std::env::var_os("PATH") {
        env.push(("PATH".to_string(), path.to_string_lossy().into_owned()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    Launch {
        program: engine.python.clone(),
        args: vec![
            "-I".to_string(),
            workspace.serve_script().to_string_lossy().into_owned(),
            "--data".to_string(),
            engine.data.to_string_lossy().into_owned(),
            "serve".to_string(),
        ],
        env,
        cwd: engine.install.clone(),
    }
}

/// Does the server offer exactly the tools (names and descriptions) this
/// build offers?
pub fn tool_list_matches(listed: &serde_json::Value) -> Result<(), String> {
    let theirs: Vec<(String, String)> = listed
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    (
                        tool["name"].as_str().unwrap_or_default().to_string(),
                        tool["description"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let ours: Vec<(String, String)> = crate::tools::specs()
        .iter()
        .filter_map(|spec| match spec {
            ToolSpec::Function(function) => {
                Some((function.name.clone(), function.description.clone()))
            }
            _ => None,
        })
        .collect();
    if theirs == ours {
        Ok(())
    } else {
        Err(
            "the installed context engine offers different tools than this ByteAsk build"
                .to_string(),
        )
    }
}

impl Backend for EngineBackend {
    fn worktree(&self) -> Option<&str> {
        Some(&self.worktree)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            if let Some(outcome) = self.check_version(timeout).await {
                return outcome;
            }
            let params = serde_json::json!({
                "name": tool,
                "arguments": arguments,
                "identity": {"user": self.user},
            });
            match self.client.request("tools/call", params, timeout).await {
                Ok(result) => Outcome::Answer(result),
                Err(err) => Outcome::Failed(err),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn listed(tools: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "tools": tools
                .iter()
                .map(|(name, description)| serde_json::json!({"name": name, "description": description}))
                .collect::<Vec<_>>()
        })
    }

    /// The context engine's own pinned list (its `tests/snapshots/tools.json`)
    /// is exactly what this build offers.
    #[test]
    fn the_context_engines_pinned_list_matches() {
        let pinned: serde_json::Value =
            serde_json::from_str(include_str!("snapshots/bctx_tools.json")).expect("json");
        assert_eq!(
            tool_list_matches(&serde_json::json!({ "tools": pinned })),
            Ok(())
        );
    }

    #[test]
    fn a_different_engine_release_is_a_mismatch() {
        let pinned: serde_json::Value =
            serde_json::from_str(include_str!("snapshots/bctx_tools.json")).expect("json");
        let mut changed = pinned;
        changed[1]["description"] = serde_json::json!("Definitions of a symbol.");
        assert!(tool_list_matches(&serde_json::json!({ "tools": changed })).is_err());
        let fewer = listed(&[("context_search", "x")]);
        assert!(tool_list_matches(&fewer).is_err());
        assert!(tool_list_matches(&serde_json::json!({})).is_err());
    }
}
