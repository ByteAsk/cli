//! Asking the company's enterprise server whether a signed result counts.
//!
//! The server holds the enrolment log: which keys are enrolled,
//! as which kind, when a key was retired (re-imaged runner: results it recorded before stay
//! valid) and when one was revoked as compromised (every result it ever signed is rejected).
//! The merge check sends it the digest it computed itself from the payload it holds.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use serde_json::json;

use crate::sign::Envelope;
use crate::sign::RunnerKind;

/// The server's answer for one envelope.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Answer {
    pub valid: bool,
    pub reason: String,
    /// The kind the key is enrolled as.
    #[serde(default)]
    pub kind: Option<RunnerKind>,
}

pub type VerifyFuture<'a> = Pin<Box<dyn Future<Output = Result<Answer, String>> + Send + 'a>>;

/// Who decides whether a signature counts.
pub trait Verifier: Send + Sync {
    /// `Err`: the question could not be answered (the result then does not count).
    fn verify<'a>(&'a self, envelope: &'a Envelope) -> VerifyFuture<'a>;
}

/// The enterprise server's `POST /enterprise/v1/runner-keys/verify` (service token).
pub struct EnterpriseVerifier {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl EnterpriseVerifier {
    pub fn new(base: &str, token: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|err| format!("could not make an HTTP client: {err}"))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            client,
        })
    }
}

impl Verifier for EnterpriseVerifier {
    fn verify<'a>(&'a self, envelope: &'a Envelope) -> VerifyFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .post(format!("{}/enterprise/v1/runner-keys/verify", self.base))
                .bearer_auth(&self.token)
                .json(&json!({
                    "key_id": envelope.key_id,
                    "digest": envelope.digest(),
                    "signed_ms": envelope.signed_ms,
                    "signature": envelope.signature,
                }))
                .send()
                .await
                .map_err(|err| format!("the enterprise server did not answer: {err}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("the enterprise server answered HTTP {status}"));
            }
            response
                .json::<Answer>()
                .await
                .map_err(|err| format!("the enterprise server's answer did not parse: {err}"))
        })
    }
}

/// The runner records what it just signed (`POST /enterprise/v1/runner-keys/attest`), so the
/// server's own clock says it existed before any later retirement.
pub async fn attest(base: &str, envelope: &Envelope) -> Result<u64, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|err| format!("could not make an HTTP client: {err}"))?;
    let response = client
        .post(format!(
            "{}/enterprise/v1/runner-keys/attest",
            base.trim_end_matches('/')
        ))
        .json(&json!({
            "key_id": envelope.key_id,
            "digest": envelope.digest(),
            "signed_ms": envelope.signed_ms,
            "signature": envelope.signature,
        }))
        .send()
        .await
        .map_err(|err| format!("the enterprise server did not answer: {err}"))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if status.is_success() && body["ok"].as_bool() == Some(true) {
        Ok(body["attested_ms"].as_u64().unwrap_or_default())
    } else {
        Err(format!(
            "the enterprise server refused to record it: {}",
            body["reason"].as_str().unwrap_or("no reason given")
        ))
    }
}

/// Why an envelope does or does not count, after the server answered.
pub(crate) fn accept(envelope: &Envelope, answer: &Answer, want: RunnerKind) -> Result<(), String> {
    if !answer.valid {
        return Err(answer.reason.clone());
    }
    match answer.kind {
        // A bench key cannot vouch for CI test results, nor a CI key for a perf card.
        Some(kind) if kind != want => Err(format!(
            "signed by a {} runner key, but this needs a {} runner",
            kind.as_str(),
            want.as_str()
        )),
        Some(_) if envelope.kind != want => Err(format!(
            "the result says it is a {} result, but this needs a {} result",
            envelope.kind.as_str(),
            want.as_str()
        )),
        Some(_) => Ok(()),
        None => Err(
            "the enterprise server did not say which kind of runner the key belongs to; \
             update the server"
                .to_string(),
        ),
    }
}
