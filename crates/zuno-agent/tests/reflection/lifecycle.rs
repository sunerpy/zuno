use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use zuno_agent::reflection::{
    CommandOutcome, CompactionMode, ReflectionConfig, ReflectionError, ReflectionFork,
    ReflectionRequest, ReflectionRunner, ReflectionTools, TranscriptEvent, TurnDelivery,
    TurnTranscript,
};

use super::support::{
    CaptureRunner, FailingRunner, MemoryProbe, PanickingRunner, WritingRunner, await_spawned,
    delivered, fork, turn,
};

fn neutral_transcript() -> TurnTranscript {
    TurnTranscript::new(vec![TranscriptEvent::user("Keep going.")])
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct BlockingRunner {
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ReflectionRunner for BlockingRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        _tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        let _drop = DropFlag(Arc::clone(&self.dropped));
        self.entered.notify_one();
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::test]
async fn fork_spawns_only_after_a_delivered_response_when_turn_is_eligible() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = fork(1, Arc::clone(&runner), &memory);

    // When
    let missing_response =
        fork.spawn_after_turn(turn(TurnDelivery::new(false, false), neutral_transcript()));
    let interrupted =
        fork.spawn_after_turn(turn(TurnDelivery::new(true, true), neutral_transcript()));
    let delivered = fork.spawn_after_turn(delivered(neutral_transcript()));
    await_spawned(delivered).await;

    // Then
    assert!(missing_response.is_none());
    assert!(interrupted.is_none());
    assert_eq!(runner.review_count(), 1);
    assert_eq!(memory.call_count(), 1);
}

#[tokio::test]
async fn default_counter_triggers_on_the_tenth_delivered_user_turn() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = ReflectionFork::new(
        ReflectionConfig::default(),
        runner.clone(),
        Arc::new(memory.clone()),
    );

    // When
    for turn_number in 1..10 {
        let task = fork.spawn_after_turn(delivered(neutral_transcript()));
        assert!(task.is_none(), "turn {turn_number} must not trigger early");
    }
    await_spawned(fork.spawn_after_turn(delivered(neutral_transcript()))).await;

    // Then
    assert_eq!(runner.review_count(), 1);
    assert_eq!(memory.call_count(), 1);
}

#[tokio::test]
async fn zero_interval_disables_only_the_periodic_trigger() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = fork(0, Arc::clone(&runner), &memory);

    // When
    let task = fork.spawn_after_turn(delivered(neutral_transcript()));

    // Then
    assert!(task.is_none());
    assert_eq!(runner.review_count(), 0);
    assert_eq!(memory.call_count(), 0);
}

#[tokio::test]
async fn failure_then_success_of_the_same_command_triggers_with_counter_disabled() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = fork(0, Arc::clone(&runner), &memory);
    let transcript = TurnTranscript::new(vec![
        TranscriptEvent::command(
            "cargo test -p zuno-agent",
            CommandOutcome::failed("compile error"),
        ),
        TranscriptEvent::assistant("Corrected the implementation."),
        TranscriptEvent::command(
            "cargo test -p zuno-agent",
            CommandOutcome::succeeded("all tests passed"),
        ),
    ]);

    // When
    await_spawned(fork.spawn_after_turn(delivered(transcript))).await;

    // Then
    assert_eq!(runner.review_count(), 1);
    assert_eq!(memory.call_count(), 1);
}

#[tokio::test]
async fn fork_replays_an_owned_transcript_with_compaction_disabled() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(CaptureRunner::default());
    let fork = fork(1, Arc::clone(&runner), &memory);
    let transcript = TurnTranscript::new(vec![
        TranscriptEvent::user("Use cargo nextest for this repository."),
        TranscriptEvent::assistant("Understood."),
    ]);

    // When
    await_spawned(fork.spawn_after_turn(delivered(transcript.clone()))).await;
    let request = runner.take_request();

    // Then
    assert_eq!(request.transcript, transcript);
    assert_eq!(request.compaction, CompactionMode::Disabled);
    assert_eq!(request.source_session_id, "ses_reflection");
    assert_eq!(request.source_message_id, "msg_reflection");
    assert_eq!(memory.call_count(), 0);
}

#[tokio::test]
async fn panicking_fork_leaves_the_main_turn_result_untouched() {
    // Given
    let memory = MemoryProbe::default();
    let fork = fork(1, Arc::new(PanickingRunner), &memory);
    let main_result = String::from("answer already delivered");

    // When
    await_spawned(fork.spawn_after_turn(delivered(neutral_transcript()))).await;

    // Then
    assert_eq!(main_result, "answer already delivered");
    assert_eq!(memory.call_count(), 0);
}

#[tokio::test]
async fn aborting_the_returned_task_cancels_the_nested_review() {
    let memory = MemoryProbe::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let fork = ReflectionFork::new(
        ReflectionConfig {
            enabled: true,
            turn_interval: 1,
        },
        Arc::new(BlockingRunner {
            entered: Arc::clone(&entered),
            dropped: Arc::clone(&dropped),
        }),
        Arc::new(memory),
    );
    let task = fork
        .spawn_after_turn(delivered(neutral_transcript()))
        .expect("reflection starts");
    entered.notified().await;

    task.abort();
    let _cancelled = task.await;

    tokio::time::timeout(Duration::from_millis(100), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborting the owner must drop the review future");
}

#[tokio::test]
async fn ordinary_reflection_error_is_swallowed_after_delivery() {
    // Given
    let memory = MemoryProbe::default();
    let fork = fork(1, Arc::new(FailingRunner), &memory);

    // When
    await_spawned(fork.spawn_after_turn(delivered(neutral_transcript()))).await;

    // Then
    assert_eq!(memory.call_count(), 0);
}

#[tokio::test]
async fn one_user_correction_produces_exactly_one_memory_write() {
    // Given
    let memory = MemoryProbe::default();
    let runner = Arc::new(WritingRunner::default());
    let fork = fork(1, Arc::clone(&runner), &memory);
    let transcript = TurnTranscript::new(vec![TranscriptEvent::user(
        "Correction: use cargo nextest instead of cargo test in this repository.",
    )]);

    // When
    await_spawned(fork.spawn_after_turn(delivered(transcript))).await;

    // Then
    assert_eq!(runner.review_count(), 1);
    assert_eq!(memory.call_count(), 1);
}
