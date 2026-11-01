use super::extract_prompt_request_with_offset;
use pretty_assertions::assert_eq;

#[test]
fn extract_prompt_request_returns_text_after_last_delimiter() {
    let message = "# Context\n## My request for Zuno:\nFirst\n## My request for Zuno:\n  Second\n";

    assert_eq!(
        extract_prompt_request_with_offset(message),
        ("Second", message.find("Second").expect("request offset"))
    );
}

#[test]
fn extract_prompt_request_still_reads_legacy_codex_heading() {
    let message = "# Context\n## My request for Codex:\n  Stored before the rename\n";

    assert_eq!(
        extract_prompt_request_with_offset(message),
        (
            "Stored before the rename",
            message.find("Stored").expect("request offset")
        )
    );
}
