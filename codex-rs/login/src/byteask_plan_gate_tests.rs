use super::*;
use base64::Engine as _;

fn id_token_with(plan: Option<&str>) -> String {
    let auth = match plan {
        Some(p) => format!(r#"{{"chatgpt_plan_type":"{p}"}}"#),
        None => "{}".to_string(),
    };
    let payload = format!(r#"{{"https://api.openai.com/auth":{auth}}}"#);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    format!("header.{b64}.signature")
}

#[test]
fn a_plan_that_serves_nothing_is_refused() {
    for plan in ["go", "free", "GO", "Free"] {
        let err = ensure_plan_supported(&id_token_with(Some(plan)))
            .expect_err("{plan} serves no model and must be refused");
        assert!(err.contains("Plus or higher"), "{plan}: {err}");
    }
}

#[test]
fn a_supported_plan_passes() {
    for plan in ["plus", "pro", "business", "enterprise"] {
        assert!(
            ensure_plan_supported(&id_token_with(Some(plan))).is_ok(),
            "{plan} must be allowed"
        );
    }
}

/// DENYLIST, not allowlist: a plan string we failed to predict must degrade to
/// "let them try", never to a lockout. Same for a token with no claim at all,
/// and for a token we cannot parse.
#[test]
fn an_unknown_or_missing_plan_is_allowed_through() {
    assert!(ensure_plan_supported(&id_token_with(Some("team-next"))).is_ok());
    assert!(ensure_plan_supported(&id_token_with(None)).is_ok());
    assert!(ensure_plan_supported("not-a-jwt").is_ok());
}
