//! Refuse a ChatGPT plan that cannot run ByteAsk — BEFORE the credential is stored.
//!
//! On `go`, every model the subscription picker can offer is refused by the backend —
//! some with "not supported when using ... with a ChatGPT account" and some with
//! "requires a newer version", which is the same entitlement block wearing a misleading
//! message. None works; `plus` and above do. So a `go`/`free` sign-in cannot produce a
//! usable session, and the honest moment to say so is before anything is written.
//!
//! ⚠️ Why the gate lives HERE, next to `ensure_workspace_allowed`, and not after login:
//! the wrapper used to check the plan once `login` had already replaced `auth.json`, so
//! refusing meant undoing damage — and a user who tried a second account lost the working
//! subscription they already had. Checking before `persist_tokens_async` means a refused
//! account never touches stored credentials at all.
//!
//! ⚠️ DENYLIST, not an allowlist: an unrecognised plan string is allowed
//! through. A plan we failed to predict should degrade to "let them try", never to a
//! lockout.

use base64::Engine as _;

/// Plans measured to serve no model at all.
const UNSUPPORTED_PLANS: &[&str] = &["go", "free"];

/// The `chatgpt_plan_type` claim, or `None` when the token does not carry one.
pub fn plan_type_of(id_token: &str) -> Option<String> {
    // Decode the payload here rather than widening `server::jwt_auth_claims` to pub:
    // this gate only ever reads ONE claim, and the signature is irrelevant to it - the
    // token came straight from the exchange we just performed, and a forged plan claim
    // would only let someone into a session the backend then refuses anyway.
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_plan_type")?
        .as_str()
        .map(str::to_string)
}

/// `Err(message)` when this plan cannot run ByteAsk. The message is shown to the user.
pub fn ensure_plan_supported(id_token: &str) -> Result<(), String> {
    let Some(plan) = plan_type_of(id_token) else {
        return Ok(()); // no claim -> let them try
    };
    if UNSUPPORTED_PLANS.contains(&plan.to_ascii_lowercase().as_str()) {
        return Err(format!(
            "This ChatGPT account is on the \"{plan}\" plan, and OpenAI serves no model \
             to ByteAsk on it — every model is refused, so the session would fail on the \
             first message. ChatGPT Plus or higher is required."
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "byteask_plan_gate_tests.rs"]
mod tests;
