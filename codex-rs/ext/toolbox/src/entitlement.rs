//! Fetching the plan and the gate switch from the managed gateway, and keeping
//! the 7-day plan cache in `policy.toml`.
//!
//! Two public gateway reads, both short and both optional:
//!
//! * `GET <gateway>/v1/subscription-flags` (public): the served switch is the
//!   `toolbox_plan_gate` key. Absent = OFF, so until the gateway serves it the
//!   gate does nothing.
//! * `GET <gateway>/v1/usage` (signed in): `plan`.
//!
//! Every failure keeps the last known answer; nothing here can make the
//! toolbox fail to start or hang (5 s per request).

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use byteask_policy::PlanCache;
use codex_login::default_client::create_client;
use sha2::Digest as _;
use sha2::Sha256;

use crate::gate::GateState;
use crate::gate::Identity;

/// The managed gateway, used for the switch when nobody is signed in.
pub const DEFAULT_GATEWAY_BASE: &str = "https://code.byteask.ai/byteask/v1";
/// Key of the served switch in `/v1/subscription-flags`.
pub const GATE_FLAG: &str = "toolbox_plan_gate";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A stable, private fingerprint of the signed-in account: a hash of the JWT's
/// `sub` claim (the account), so a renewed token keeps its cached plan and the
/// address never lands on disk. A token that is not a readable JWT is hashed
/// whole.
pub fn account_fingerprint(token: &str) -> String {
    let subject = token
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|claims| {
            claims
                .get("sub")
                .and_then(|sub| sub.as_str())
                .map(str::to_string)
        });
    let mut hasher = Sha256::new();
    match subject {
        Some(subject) => hasher.update(format!("sub:{subject}")),
        None => hasher.update(format!("token:{token}")),
    }
    let digest = hasher.finalize();
    digest
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `.../byteask/v1` and `.../byteask` both become `.../byteask` (the same rule
/// the TUI's `/usage` and `cpp_intrinsic` use).
fn gateway_origin(base: &str) -> &str {
    let base = base.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base)
}

/// The served switch. `None` = the gateway could not be asked.
pub async fn fetch_switch(base: &str) -> Option<bool> {
    let url = format!("{}/v1/subscription-flags", gateway_origin(base));
    let response = create_client()
        .get(url.as_str())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let flags: serde_json::Value = response.json().await.ok()?;
    // A reachable gateway that does not serve the key means OFF.
    Some(
        flags
            .get(GATE_FLAG)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    )
}

/// What the gateway said about the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanAnswer {
    Plan(String),
    /// 401/403: the token is expired or revoked.
    Rejected,
    /// Offline, timed out, or an unexpected answer.
    Unavailable,
}

pub async fn fetch_plan(base: &str, token: &str) -> PlanAnswer {
    let url = format!("{}/v1/usage", gateway_origin(base));
    let response = match create_client()
        .get(url.as_str())
        .header("Authorization", format!("Bearer {token}"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return PlanAnswer::Unavailable,
    };
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return PlanAnswer::Rejected;
    }
    if !(200..300).contains(&status) {
        return PlanAnswer::Unavailable;
    }
    match response.json::<serde_json::Value>().await {
        Ok(body) => match body.get("plan").and_then(serde_json::Value::as_str) {
            Some(plan) if !plan.trim().is_empty() => PlanAnswer::Plan(plan.trim().to_string()),
            _ => PlanAnswer::Unavailable,
        },
        Err(_) => PlanAnswer::Unavailable,
    }
}

/// The gate state from the cache alone, before (or without) any network call.
pub fn cached_state(home: &Path, token: Option<&str>) -> GateState {
    let cache = byteask_policy::load(home, &[]).policy.plan;
    GateState {
        switch_on: cache.as_ref().is_some_and(|cache| cache.gate),
        identity: identity_for(token),
        cache,
    }
}

fn identity_for(token: Option<&str>) -> Identity {
    match token {
        Some(token) => Identity::SignedIn {
            account: account_fingerprint(token),
        },
        None => Identity::SignedOut,
    }
}

/// Ask the gateway, store a fresh answer in the plan cache, and return the
/// state the gate should use from now on. `base` is the signed-in account's
/// gateway, or [`DEFAULT_GATEWAY_BASE`].
pub async fn refresh(home: &Path, base: &str, token: Option<&str>, now: u64) -> GateState {
    let mut state = cached_state(home, token);
    let switch = fetch_switch(base).await;
    if let Some(on) = switch {
        state.switch_on = on;
    }
    let Some(token) = token else {
        return state;
    };
    match fetch_plan(base, token).await {
        PlanAnswer::Plan(plan) => {
            let cache = PlanCache {
                plan,
                checked_at: now,
                account: account_fingerprint(token),
                gate: state.switch_on,
            };
            if let Err(err) = byteask_policy::write_plan_cache(home, &cache) {
                tracing::warn!("could not store the plan cache: {err}");
            }
            state.cache = Some(cache);
        }
        PlanAnswer::Rejected => state.identity = Identity::Expired,
        PlanAnswer::Unavailable => {
            // Keep the cached plan, but remember a newly served switch value so
            // turning the gate off reaches an offline-later user too.
            if let (Some(on), Some(cache)) = (switch, state.cache.as_mut())
                && cache.gate != on
            {
                cache.gate = on;
                if let Err(err) = byteask_policy::write_plan_cache(home, cache) {
                    tracing::warn!("could not store the plan cache: {err}");
                }
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::io::Write as _;

    use pretty_assertions::assert_eq;

    use super::*;

    /// A one-route-per-path HTTP/1.1 mock on loopback: `(path, status, body)`.
    fn serve(routes: Vec<(&'static str, u16, String)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = routes
                    .iter()
                    .find(|(route, _, _)| path == *route)
                    .map(|(_, status, body)| (*status, body.clone()))
                    .unwrap_or((404, String::new()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        format!("http://{addr}/byteask/v1")
    }

    fn closed() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        format!("http://{addr}/byteask/v1")
    }

    fn jwt(sub: &str) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{{\"sub\":\"{sub}\",\"exp\":1}}"));
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig")
    }

    #[test]
    fn fingerprint_follows_the_account_not_the_token() {
        let first = account_fingerprint(&jwt("a@example.com"));
        let renewed = account_fingerprint(&format!("{}x", jwt("a@example.com")));
        assert_eq!(first, renewed, "a renewed token must keep its cached plan");
        assert_ne!(first, account_fingerprint(&jwt("b@example.com")));
        assert!(
            !first.contains("example"),
            "the address never lands on disk"
        );
        assert_eq!(first.len(), 24);
    }

    #[tokio::test]
    async fn a_fresh_plan_is_stored_with_the_served_switch() {
        let home = tempfile::tempdir().unwrap();
        let base = serve(vec![
            (
                "/byteask/v1/subscription-flags",
                200,
                r#"{"chatgpt":true,"toolbox_plan_gate":true}"#.to_string(),
            ),
            (
                "/byteask/v1/usage",
                200,
                r#"{"plan":"pro","used_5h_pct":3}"#.to_string(),
            ),
        ]);
        let token = jwt("a@example.com");
        let state = refresh(home.path(), &base, Some(&token), 1_000).await;
        assert!(state.switch_on);
        assert_eq!(state.cache.as_ref().map(|c| c.plan.as_str()), Some("pro"));
        let stored = byteask_policy::load(home.path(), &[])
            .policy
            .plan
            .expect("cached");
        assert_eq!(stored.checked_at, 1_000);
        assert!(stored.gate);
        assert_eq!(stored.account, account_fingerprint(&token));
    }

    #[tokio::test]
    async fn a_gateway_without_the_key_means_the_gate_is_off() {
        let home = tempfile::tempdir().unwrap();
        let base = serve(vec![
            (
                "/byteask/v1/subscription-flags",
                200,
                r#"{"chatgpt":true,"claude":false}"#.to_string(),
            ),
            ("/byteask/v1/usage", 200, r#"{"plan":"free"}"#.to_string()),
        ]);
        let state = refresh(home.path(), &base, Some(&jwt("a@x")), 5).await;
        assert!(!state.switch_on);
    }

    #[tokio::test]
    async fn offline_keeps_the_cached_answer() {
        let home = tempfile::tempdir().unwrap();
        let token = jwt("a@example.com");
        byteask_policy::write_plan_cache(
            home.path(),
            &PlanCache {
                plan: "max".to_string(),
                checked_at: 10,
                account: account_fingerprint(&token),
                gate: true,
            },
        )
        .unwrap();
        let state = refresh(home.path(), &closed(), Some(&token), 99).await;
        assert!(state.switch_on, "the last served switch value stands");
        assert_eq!(
            state
                .cache
                .as_ref()
                .map(|c| (c.plan.as_str(), c.checked_at)),
            Some(("max", 10))
        );
    }

    #[tokio::test]
    async fn turning_the_switch_off_is_remembered_even_when_the_plan_read_fails() {
        let home = tempfile::tempdir().unwrap();
        let token = jwt("a@example.com");
        byteask_policy::write_plan_cache(
            home.path(),
            &PlanCache {
                plan: "free".to_string(),
                checked_at: 10,
                account: account_fingerprint(&token),
                gate: true,
            },
        )
        .unwrap();
        let base = serve(vec![
            (
                "/byteask/v1/subscription-flags",
                200,
                r#"{"toolbox_plan_gate":false}"#.to_string(),
            ),
            ("/byteask/v1/usage", 500, String::new()),
        ]);
        let state = refresh(home.path(), &base, Some(&token), 99).await;
        assert!(!state.switch_on);
        assert!(
            !cached_state(home.path(), Some(&token)).switch_on,
            "persisted for offline use"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_reads_as_expired() {
        let home = tempfile::tempdir().unwrap();
        let base = serve(vec![
            (
                "/byteask/v1/subscription-flags",
                200,
                r#"{"toolbox_plan_gate":true}"#.to_string(),
            ),
            ("/byteask/v1/usage", 401, r#"{"error":{}}"#.to_string()),
        ]);
        let state = refresh(home.path(), &base, Some(&jwt("a@x")), 5).await;
        assert_eq!(state.identity, Identity::Expired);
    }

    #[tokio::test]
    async fn signed_out_reads_only_the_switch() {
        let home = tempfile::tempdir().unwrap();
        let base = serve(vec![(
            "/byteask/v1/subscription-flags",
            200,
            r#"{"toolbox_plan_gate":true}"#.to_string(),
        )]);
        let state = refresh(home.path(), &base, None, 5).await;
        assert!(state.switch_on);
        assert_eq!(state.identity, Identity::SignedOut);
        assert_eq!(byteask_policy::load(home.path(), &[]).policy.plan, None);
    }
}
