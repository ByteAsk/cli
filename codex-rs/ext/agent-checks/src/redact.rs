//! Log tails leave the sandbox (checks.json goes to the control plane), so known secret shapes
//! are cut out first: the generic patterns in `codex-secrets` plus ByteAsk and GitHub token
//! prefixes.

use std::sync::LazyLock;

use regex::Regex;

static TOKEN_PREFIX_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"\b(?:bcs|bck|bxk|bxt)_[A-Za-z0-9_-]{8,}|\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})")
        .ok()
});

pub(crate) fn redact(text: &str) -> String {
    let redacted = codex_secrets::redact_secrets(text.to_string());
    match TOKEN_PREFIX_REGEX.as_ref() {
        Some(regex) => regex
            .replace_all(&redacted, "[REDACTED_SECRET]")
            .into_owned(),
        None => redacted,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::redact;

    #[test]
    fn redacts_byteask_and_github_tokens() {
        assert_eq!(
            redact("key bxt_FAKE_FIXTURE_abc123 and ghp_abcdefghijklmnopqrstuvwxyz ok"),
            "key [REDACTED_SECRET] and [REDACTED_SECRET] ok"
        );
        assert_eq!(redact("ctest passed"), "ctest passed");
    }
}
