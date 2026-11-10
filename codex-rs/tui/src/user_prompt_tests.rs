use super::extract_prompt_request_with_offset;
use pretty_assertions::assert_eq;

#[test]
fn extract_prompt_request_returns_text_after_last_delimiter() {
    let message =
        "# Context\n## My request for ByteAsk:\nFirst\n## My request for ByteAsk:\n  Second\n";

    assert_eq!(
        extract_prompt_request_with_offset(message),
        ("Second", message.find("Second").expect("request offset"))
    );
}
