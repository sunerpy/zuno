use std::sync::Arc;

use zuno_agent::reflection::{CommandOutcome, TranscriptEvent, TurnTranscript};

use super::support::{
    DeniedToolRunner, MemoryProbe, WritingRunner, await_spawned, delivered, fork,
};

#[tokio::test]
async fn non_memory_tool_call_is_denied_at_reflection_dispatch() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(DeniedToolRunner::default());
    let fork = fork(1, Arc::clone(&runner), &memory);
    let transcript = TurnTranscript::new(vec![TranscriptEvent::user(
        "Remember that this repository uses cargo nextest.",
    )]);

    // When
    await_spawned(fork.spawn_after_turn(delivered(transcript))).await;

    // Then
    assert_eq!(
        runner.denial(),
        "Background review denied non-whitelisted tool: bash. Only memory proposals are allowed."
    );
    assert_eq!(memory.call_count(), 0);
}

async fn assert_transcript_is_not_learned(transcript: TurnTranscript) {
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = fork(1, Arc::clone(&runner), &memory);

    let task = fork.spawn_after_turn(delivered(transcript));

    assert!(task.is_none());
    assert_eq!(runner.review_count(), 0);
    assert_eq!(memory.call_count(), 0);
}

#[tokio::test]
async fn environment_dependent_failure_produces_no_memory_write() {
    // Given
    let transcript = TurnTranscript::new(vec![TranscriptEvent::command(
        "jq --version",
        CommandOutcome::failed("sh: jq: command not found"),
    )]);

    // When / Then
    assert_transcript_is_not_learned(transcript).await;
}

#[tokio::test]
async fn negative_tool_claim_produces_no_memory_write() {
    // Given
    let transcript = TurnTranscript::new(vec![TranscriptEvent::assistant(
        "The browser tool is broken and does not work.",
    )]);

    // When / Then
    assert_transcript_is_not_learned(transcript).await;
}

#[tokio::test]
async fn transient_error_that_self_resolved_produces_no_memory_write() {
    // Given
    let transcript = TurnTranscript::new(vec![
        TranscriptEvent::command(
            "cargo fetch",
            CommandOutcome::failed("temporary network timeout"),
        ),
        TranscriptEvent::command("cargo fetch", CommandOutcome::succeeded("downloaded")),
    ]);

    // When / Then
    assert_transcript_is_not_learned(transcript).await;
}

#[tokio::test]
async fn one_off_task_narrative_produces_no_memory_write() {
    // Given
    let transcript = TurnTranscript::new(vec![TranscriptEvent::user(
        "Analyze this PR and summarize today's findings.",
    )]);

    // When / Then
    assert_transcript_is_not_learned(transcript).await;
}

#[tokio::test]
async fn unresolved_failure_produces_no_memory_write() {
    // Given
    let transcript = TurnTranscript::new(vec![
        TranscriptEvent::command(
            "cargo test -p zuno-agent",
            CommandOutcome::failed("assertion failed"),
        ),
        TranscriptEvent::assistant(
            "I could not find a working method; please check this manually.",
        ),
    ]);

    // When / Then
    assert_transcript_is_not_learned(transcript).await;
}
