//! Runner keys and signed results.
//!
//! A CI runner or a bench machine signs what it produced with an Ed25519 key the company
//! enrolled in its admin console (`POST /enterprise/v1/admin/runner-keys`). The signed
//! message is the enterprise server's, byte for byte, so that server can verify it:
//!
//! ```text
//! b"byteask-runner-result-v1\n" + sha256_hex(payload) + b"\n" + signed_ms
//! ```
//!
//! The payload travels inside the envelope as the exact text that was signed; whoever checks a
//! signature recomputes the digest from that text and never trusts a digest it is handed.

use std::path::Path;

use base64::Engine as _;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;

/// The signature domain, the same bytes the enterprise server uses.
pub const DOMAIN: &[u8] = b"byteask-runner-result-v1\n";
pub const ENVELOPE_SCHEMA: &str = "byteask.signed/1";

/// Which kind of runner signed (the enterprise server's `RUNNER_KINDS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Ci,
    Bench,
}

impl RunnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunnerKind::Ci => "ci",
            RunnerKind::Bench => "bench",
        }
    }
}

/// A signed payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub schema: String,
    /// What the signer says it is; the merge check believes it only if the enterprise server
    /// agrees the key is enrolled as this kind.
    pub kind: RunnerKind,
    pub key_id: String,
    pub signed_ms: u64,
    /// Base64 Ed25519 signature over [`message`].
    pub signature: String,
    /// The exact signed text (a JSON document).
    pub payload: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The enterprise server's signed message for a payload digest.
pub fn message(digest_hex: &str, signed_ms: u64) -> Vec<u8> {
    let mut out = DOMAIN.to_vec();
    out.extend_from_slice(digest_hex.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(signed_ms.to_string().as_bytes());
    out
}

impl Envelope {
    /// The digest the merge check computes itself from the payload it holds.
    pub fn digest(&self) -> String {
        sha256_hex(self.payload.as_bytes())
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let envelope: Envelope =
            serde_json::from_str(text).map_err(|err| format!("not a signed result: {err}"))?;
        if envelope.schema != ENVELOPE_SCHEMA {
            return Err(format!("unknown envelope schema {:?}", envelope.schema));
        }
        Ok(envelope)
    }
}

/// A runner's signing key.
pub struct RunnerKey {
    key: SigningKey,
}

impl std::fmt::Debug for RunnerKey {
    /// The key id only: a signing key never reaches a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunnerKey({})", self.key_id())
    }
}

impl RunnerKey {
    /// A new random key.
    pub fn generate() -> Result<Self, String> {
        use rand::TryRngCore as _;
        let mut seed = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut seed)
            .map_err(|err| format!("no secure random source: {err}"))?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`), or base64 of the 32-byte seed.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.starts_with("-----BEGIN PRIVATE KEY-----") {
            let body: String = text
                .lines()
                .filter(|line| !line.starts_with("-----"))
                .collect();
            let der = base64::engine::general_purpose::STANDARD
                .decode(body.trim())
                .map_err(|_| "the PEM key is not valid base64".to_string())?;
            let key = SigningKey::from_pkcs8_der(&der)
                .map_err(|_| "the PEM key is not an Ed25519 PKCS#8 private key".to_string())?;
            return Ok(Self { key });
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| "a runner key is PKCS#8 PEM or base64 of 32 bytes".to_string())?;
        let seed: [u8; 32] = raw
            .try_into()
            .map_err(|_| "a raw Ed25519 key is 32 bytes".to_string())?;
        Ok(Self::from_seed(seed))
    }

    /// Read a key file. On Unix the file must not be readable by group or others: a signing
    /// key anyone on the runner can read signs for anyone.
    pub fn load(path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let meta = std::fs::metadata(path)
                .map_err(|err| format!("could not read {}: {err}", path.display()))?;
            if meta.mode() & 0o077 != 0 {
                return Err(format!(
                    "{} is readable by other users; chmod 600 it first",
                    path.display()
                ));
            }
        }
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read {}: {err}", path.display()))?;
        Self::parse(&text)
    }

    /// PKCS#8 PEM, for a key file.
    pub fn to_pem(&self) -> Result<String, String> {
        let der = self
            .key
            .to_pkcs8_der()
            .map_err(|err| format!("could not encode the key: {err}"))?;
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());
        let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for chunk in body.as_bytes().chunks(64) {
            pem.push_str(&String::from_utf8_lossy(chunk));
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        Ok(pem)
    }

    /// Base64 of the 32 raw public key bytes: what the admin console enrols.
    pub fn public_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().as_bytes())
    }

    /// The enterprise server's key id: `rk_` + the first 16 hex digits of SHA-256(public key).
    pub fn key_id(&self) -> String {
        format!(
            "rk_{}",
            &sha256_hex(self.key.verifying_key().as_bytes())[..16]
        )
    }

    pub fn sign(&self, kind: RunnerKind, payload: String, signed_ms: u64) -> Envelope {
        let digest = sha256_hex(payload.as_bytes());
        let signature = self.key.sign(&message(&digest, signed_ms));
        Envelope {
            schema: ENVELOPE_SCHEMA.to_string(),
            kind,
            key_id: self.key_id(),
            signed_ms,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            payload,
        }
    }
}

/// Write a new key file (0600, never overwriting) and return the key.
pub fn write_new_key(path: &Path) -> Result<RunnerKey, String> {
    let key = RunnerKey::generate()?;
    let pem = key.to_pem()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| format!("could not create {}: {err}", path.display()))?;
    std::io::Write::write_all(&mut file, pem.as_bytes())
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The enterprise server's verifier checks this exact message and key id; a test
    /// vector pins both so the two sides cannot drift.
    #[test]
    fn message_and_key_id_match_the_server() {
        let key = RunnerKey::from_seed([7u8; 32]);
        assert_eq!(
            message("ab".repeat(32).as_str(), 1_700_000_000_123),
            format!(
                "byteask-runner-result-v1\n{}\n1700000000123",
                "ab".repeat(32)
            )
            .into_bytes()
        );
        let id = key.key_id();
        assert!(id.starts_with("rk_") && id.len() == 19, "{id}");
        assert_eq!(
            id,
            format!(
                "rk_{}",
                &sha256_hex(
                    &base64::engine::general_purpose::STANDARD
                        .decode(key.public_b64())
                        .unwrap()
                )[..16]
            )
        );
    }

    #[test]
    fn signatures_verify_and_bind_the_payload() {
        let key = RunnerKey::from_seed([9u8; 32]);
        let envelope = key.sign(RunnerKind::Ci, "{\"a\":1}".to_string(), 42);
        let sig = base64::engine::general_purpose::STANDARD
            .decode(&envelope.signature)
            .unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig).unwrap();
        let public = key.key.verifying_key();
        assert!(
            public
                .verify_strict(&message(&envelope.digest(), 42), &sig)
                .is_ok()
        );
        let mut edited = envelope;
        edited.payload = "{\"a\":2}".to_string();
        assert!(
            public
                .verify_strict(&message(&edited.digest(), 42), &sig)
                .is_err()
        );
    }

    #[test]
    fn keys_round_trip_through_pem_and_raw() {
        let key = RunnerKey::generate().unwrap();
        let again = RunnerKey::parse(&key.to_pem().unwrap()).unwrap();
        assert_eq!(again.key_id(), key.key_id());
        let raw = base64::engine::general_purpose::STANDARD.encode([3u8; 32]);
        assert_eq!(
            RunnerKey::parse(&raw).unwrap().key_id(),
            RunnerKey::from_seed([3u8; 32]).key_id()
        );
        assert!(RunnerKey::parse("not a key").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner.pem");
        write_new_key(&path).unwrap();
        assert!(RunnerKey::load(&path).is_ok());
        assert!(write_new_key(&path).is_err(), "never overwrites a key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(RunnerKey::load(&path).unwrap_err().contains("chmod 600"));
    }
}
