use base64::Engine;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn compose_success_url_uses_local_page_by_default() {
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 1455,
        DEFAULT_ISSUER,
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        /*codex_streamlined_login*/ false,
        &LoginSuccessPage::default(),
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.path(), "/success");
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "codex_streamlined_login"),
        None
    );
}

#[test]
fn compose_success_url_uses_streamlined_local_page_when_requested() {
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 1455,
        DEFAULT_ISSUER,
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        /*codex_streamlined_login*/ true,
        &LoginSuccessPage::default(),
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "codex_streamlined_login")
            .map(|(_, value)| value.into_owned()),
        Some("true".to_string())
    );
}

#[test]
fn compose_success_url_uses_hosted_page_when_requested() {
    assert_eq!(
        compose_success_url(
            /*port*/ 1455,
            DEFAULT_ISSUER,
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            /*codex_streamlined_login*/ false,
            &LoginSuccessPage::Hosted {
                url: Url::parse(CODEX_OPEN_APP_URL).expect("open app URL should parse"),
                app_brand: LoginSuccessPageBrand::Chatgpt,
            },
        ),
        LoginSuccessRedirect::Hosted(
            "https://chatgpt.com/codex/open-app?source=login&app_brand=chatgpt".to_string()
        )
    );
}

#[test]
fn compose_success_url_keeps_setup_on_local_page() {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let payload = encode(
        serde_json::to_string(&json!({
            "https://api.openai.com/auth": {
                "completed_platform_onboarding": false,
                "is_org_owner": true,
                "organization_id": "org_123",
                "project_id": "proj_123",
            }
        }))
        .expect("payload should serialize")
        .as_bytes(),
    );
    let access_payload = encode(
        serde_json::to_string(&json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "team",
            }
        }))
        .expect("payload should serialize")
        .as_bytes(),
    );
    let id_token = format!("e30.{payload}.sig");
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 1455,
        DEFAULT_ISSUER,
        &id_token,
        &format!("e30.{access_payload}.sig"),
        /*codex_streamlined_login*/ true,
        &LoginSuccessPage::Hosted {
            url: Url::parse(CODEX_OPEN_APP_URL).expect("open app URL should parse"),
            app_brand: LoginSuccessPageBrand::Codex,
        },
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.path(), "/success");
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "needs_setup")
            .map(|(_, value)| value.into_owned()),
        Some("true".to_string())
    );
}

/// The pages the BROWSER shows at the end of a ChatGPT sign-in are a user surface
/// (tab title and page body), and the rebranding script never touches them: it sweeps `*.rs`
/// only. That is a second blind spot alongside multi-line string literals, so the
/// guard has to be a test.
///
/// `success.html` is deliberately NOT checked: it is only served when the auth URL
/// carries `codex_streamlined_login=true`, which ByteAsk never sets (pinned below),
/// and its one branded string is a button that deep-links `codex://threads/new` —
/// a desktop app we do not ship. Renaming that label without the app would leave a
/// button that claims to open ByteAsk and opens nothing.
#[test]
fn browser_sign_in_pages_show_no_other_brand() {
    let pages = [
        (
            "success_legacy.html",
            include_str!("assets/success_legacy.html"),
        ),
        ("error.html", include_str!("assets/error.html")),
    ];

    for (name, html) in pages {
        // Visible text only: what sits between a `>` and the next `<`. CSS class
        // names, element ids and URLs are functional and must keep their spelling.
        for chunk in html.split('>').skip(1) {
            let text = chunk.split('<').next().unwrap_or("").trim();
            assert!(
                !text.to_ascii_lowercase().contains("codex"),
                "{name} shows {text:?} to the user"
            );
        }
        assert!(html.contains("ByteAsk"), "{name} names no brand at all");
    }
}

/// If upstream ever flips this default, ByteAsk starts serving `success.html` —
/// whose title and button still say Codex. Failing here is the signal to rebrand
/// that page (or keep the flag off) rather than discovering it from a user.
#[test]
fn byteask_never_serves_the_streamlined_success_page() {
    let opts = crate::ServerOptions::new(
        std::path::PathBuf::from("/tmp"),
        "client".to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        codex_config::types::AuthCredentialsStoreMode::default(),
        crate::auth::AuthKeyringBackendKind::default(),
        crate::test_support::transport_default_auth_route_config(),
    );

    assert!(
        !opts.codex_streamlined_login,
        "ByteAsk must keep serving success_legacy.html"
    );
}
