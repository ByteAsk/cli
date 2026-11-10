use super::*;

#[test]
fn device_code_prompt_renders_phishing_warning() {
    let prompt = device_code_prompt("https://example.com/device", "ABCD-EFGH");

    assert!(prompt.contains(
        "Continue only if you started this sign-in in ByteAsk. If a website\n  or another person gave you this code, cancel."
    ));
}

#[test]
fn device_code_prompt_shows_the_link_and_the_code() {
    let prompt = device_code_prompt("https://example.com/device", "ABCD-EFGH");

    assert!(prompt.contains("https://example.com/device"), "{prompt}");
    assert!(prompt.contains("ABCD-EFGH"), "{prompt}");
    assert!(
        prompt.contains("Sign in with your ChatGPT subscription"),
        "{prompt}"
    );
}

/// This screen is where a user decides whether to trust us with their ChatGPT
/// account, so it is the worst possible place to leak the upstream brand
/// (upstream's text names it three times: a "Welcome to Codex [v…]" banner,
/// "OpenAI's command-line coding agent", and a phishing warning naming Codex),
/// and the rebranding script cannot reach any of them, because they live in a
/// MULTI-LINE string literal and its sweep only rewrites quoted spans on one
/// line. A test is the only guard that survives the next upstream merge.
#[test]
fn device_code_prompt_names_no_other_brand() {
    let prompt = device_code_prompt("https://example.com/device", "ABCD-EFGH");

    for leak in ["Codex", "codex", "command-line coding agent"] {
        assert!(
            !prompt.contains(leak),
            "device-code sign-in screen leaks {leak:?}:\n{prompt}"
        );
    }
}
