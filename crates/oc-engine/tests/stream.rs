use std::collections::VecDeque;
use std::time::Duration;

use oc_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
use oc_db::{Connection, migration, open};
use oc_engine::stream::{
    DELTA_BATCH_BYTES, ProjectionContext, ProjectionEffects, SnapshotPatch, StepUsage,
    StreamProjector,
};
use oc_llm::event::{ConnectionPhase, FinishReason, StreamEvent, ThoughtSignature};
use serde_json::json;

const SESSION_ID: &str = "ses_stream_test";
const MESSAGE_ID: &str = "msg_stream_test";

fn seeded() -> Connection {
    let mut connection = open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-stream', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-stream', 'stream', '/workspace', 'stream', '1', 1, 1);"
        ))
        .expect("seed project and session");

    let message = MessageRecord::from_json(json!({
        "id": MESSAGE_ID,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": 10 },
        "parentID": "msg_user",
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        }
    }))
    .expect("valid assistant message");
    MessageStore::new(&connection)
        .put_message_at(&message, 10)
        .expect("persist assistant message");
    connection
}

fn context() -> ProjectionContext {
    ProjectionContext::new(SESSION_ID, MESSAGE_ID, 1, 10, "build").with_cost(0.125)
}

fn message_parts(connection: &Connection) -> Vec<PartRecord> {
    MessageStore::new(connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate projected message")
        .into_iter()
        .find(|message| message.info.id == MESSAGE_ID)
        .expect("assistant message exists")
        .parts
}

#[derive(Debug, Default)]
struct RecordingEffects {
    snapshots: VecDeque<Option<String>>,
    patch: Option<SnapshotPatch>,
    summaries: Vec<(String, String)>,
    overflow_checks: Vec<StepUsage>,
    overflow: bool,
}

impl ProjectionEffects for RecordingEffects {
    fn track_snapshot(&mut self) -> Option<String> {
        self.snapshots.pop_front().flatten()
    }

    fn patch(&mut self, _snapshot: &str) -> Option<SnapshotPatch> {
        self.patch.take()
    }

    fn trigger_summary(&mut self, session_id: &str, message_id: &str) {
        self.summaries
            .push((session_id.to_owned(), message_id.to_owned()));
    }

    fn is_overflow(&mut self, usage: &StepUsage) -> bool {
        self.overflow_checks.push(usage.clone());
        self.overflow
    }
}

#[test]
fn stream_five_thousand_text_deltas_are_batched_below_the_documented_write_bound() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    for _ in 0..5_000 {
        projector
            .apply(StreamEvent::TextDelta("x".to_owned()))
            .expect("project text delta");
    }
    projector
        .apply(StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        })
        .expect("finish stream projection");

    // One size-window flush at 4096 bytes plus one terminal flush. This bound
    // proves the projector did not perform one SQLite upsert per provider token.
    const MAX_DELTA_WRITES: u64 = 2;
    let measured = projector.stats().delta_writes;
    assert!(
        measured <= MAX_DELTA_WRITES,
        "5000 one-byte deltas caused {measured} writes; bound is {MAX_DELTA_WRITES}"
    );
    assert_eq!(DELTA_BATCH_BYTES, 4 * 1024);
    drop(projector);

    let text_parts: Vec<_> = message_parts(&connection)
        .into_iter()
        .filter(|part| part.kind == PartKind::Text)
        .collect();
    assert_eq!(text_parts.len(), 1);
    assert_eq!(text_parts[0].data["text"], "x".repeat(5_000));
    eprintln!(
        "BATCH_QA deltas=5000 delta_writes={measured} bound={MAX_DELTA_WRITES} final_bytes={}",
        text_parts[0].data["text"]
            .as_str()
            .expect("text string")
            .len()
    );
}

#[test]
fn stream_fifty_kibibyte_response_is_one_complete_text_part() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");
    let fragment = "z".repeat(1024);

    for _ in 0..50 {
        projector
            .apply(StreamEvent::TextDelta(fragment.clone()))
            .expect("project 1 KiB fragment");
    }
    projector
        .apply(StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        })
        .expect("finish 50 KiB response");
    let writes = projector.stats().delta_writes;
    drop(projector);

    let text_parts: Vec<_> = message_parts(&connection)
        .into_iter()
        .filter(|part| part.kind == PartKind::Text)
        .collect();
    assert_eq!(text_parts.len(), 1);
    let text = text_parts[0].data["text"].as_str().expect("text string");
    assert_eq!(text.len(), 50 * 1024);
    assert!(text.bytes().all(|byte| byte == b'z'));
    eprintln!(
        "HAPPY_QA response_bytes={} text_parts={} delta_writes={writes}",
        text.len(),
        text_parts.len()
    );
}

#[test]
fn stream_rollback_removes_already_flushed_partial_parts_from_the_database() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    projector
        .apply(StreamEvent::TextDelta("x".repeat(DELTA_BATCH_BYTES)))
        .expect("flush the first attempt");
    assert!(
        message_parts(&connection)
            .iter()
            .any(|part| part.kind == PartKind::Text),
        "precondition: the size window must have reached SQLite"
    );

    projector
        .apply(StreamEvent::RetryRollback { attempt: 2, max: 3 })
        .expect("rollback the first attempt");
    drop(projector);

    let parts = message_parts(&connection);
    assert!(parts.iter().all(|part| !matches!(
        part.kind,
        PartKind::Text | PartKind::Reasoning | PartKind::Tool | PartKind::File
    )));
    let retry = parts
        .iter()
        .find(|part| part.kind == PartKind::Retry)
        .expect("rollback is retained as a retry part");
    assert_eq!(retry.data["attempt"], 2);
    eprintln!(
        "ROLLBACK_QA remaining_parts={:#?}",
        parts.iter().map(PartRecord::to_json).collect::<Vec<_>>()
    );
}

#[test]
fn stream_ending_mid_tool_input_synthesizes_an_error_without_panicking() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    projector
        .apply(StreamEvent::ToolUseStart {
            id: "call-incomplete".to_owned(),
            name: "read".to_owned(),
        })
        .expect("start tool input");
    projector
        .apply(StreamEvent::ToolInputDelta(
            r#"{"filePath":"README.md""#.to_owned(),
        ))
        .expect("accumulate incomplete JSON without parsing it");
    projector
        .finish_incomplete("provider stream ended before ToolUseEnd")
        .expect("synthesize incomplete-tool error");
    drop(projector);

    let parts = message_parts(&connection);
    let tool = parts
        .iter()
        .find(|part| part.kind == PartKind::Tool)
        .expect("incomplete tool is persisted as an error");
    assert_eq!(tool.data["state"]["status"], "error");
    assert_eq!(
        tool.data["state"]["error"],
        "provider stream ended before ToolUseEnd"
    );
    assert_eq!(tool.data["state"]["raw"], r#"{"filePath":"README.md""#);
    eprintln!("FAILURE_QA synthesized_tool_part={:#?}", tool.to_json());
}

#[test]
fn stream_tool_input_is_parsed_once_at_end_with_a_trailing_comma_repair() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    projector
        .apply(StreamEvent::ToolUseStart {
            id: "call-lenient".to_owned(),
            name: "read".to_owned(),
        })
        .expect("start tool input");
    projector
        .apply(StreamEvent::ToolInputDelta(
            r#"{"filePath":"README.md",}"#.to_owned(),
        ))
        .expect("accumulate malformed fragment");
    projector
        .apply(StreamEvent::ToolUseEnd)
        .expect("parse complete accumulated input leniently");
    drop(projector);

    let parts = message_parts(&connection);
    let tool = parts
        .iter()
        .find(|part| part.kind == PartKind::Tool)
        .expect("tool part");
    assert_eq!(tool.data["state"]["status"], "pending");
    assert_eq!(tool.data["state"]["input"]["filePath"], "README.md");
    assert_eq!(tool.data["state"]["raw"], r#"{"filePath":"README.md",}"#);
}

#[test]
fn stream_step_finish_updates_usage_writes_patch_and_triggers_summary_and_overflow() {
    let connection = seeded();
    let mut effects = RecordingEffects {
        snapshots: VecDeque::from([
            Some("snapshot-before".to_owned()),
            Some("snapshot-after".to_owned()),
        ]),
        patch: Some(SnapshotPatch {
            hash: "patch-hash".to_owned(),
            files: vec!["src/lib.rs".to_owned()],
        }),
        overflow: true,
        ..RecordingEffects::default()
    };
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    projector
        .apply(StreamEvent::TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(40),
            cache_read_input_tokens: Some(20),
            cache_write_input_tokens: Some(5),
        })
        .expect("record usage");
    projector
        .apply(StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Length),
        })
        .expect("finish projected step");
    assert!(projector.outcome().needs_compaction);
    drop(projector);

    let parts = message_parts(&connection);
    let start = parts
        .iter()
        .find(|part| part.kind == PartKind::StepStart)
        .expect("step-start part");
    assert_eq!(start.data["snapshot"], "snapshot-before");
    let finish = parts
        .iter()
        .find(|part| part.kind == PartKind::StepFinish)
        .expect("step-finish part");
    assert_eq!(finish.data["reason"], "length");
    assert_eq!(finish.data["snapshot"], "snapshot-after");
    assert_eq!(finish.data["cost"], 0.125);
    assert_eq!(finish.data["tokens"]["input"], 100);
    assert_eq!(finish.data["tokens"]["output"], 40);
    assert_eq!(finish.data["tokens"]["cache"]["read"], 20);
    assert_eq!(finish.data["tokens"]["cache"]["write"], 5);
    let patch = parts
        .iter()
        .find(|part| part.kind == PartKind::Patch)
        .expect("snapshot patch part");
    assert_eq!(patch.data["hash"], "patch-hash");
    assert_eq!(patch.data["files"], json!(["src/lib.rs"]));

    let message = MessageStore::new(&connection)
        .message(MESSAGE_ID)
        .expect("updated assistant message");
    assert_eq!(message.data["finish"], "length");
    assert_eq!(message.data["cost"], 0.125);
    assert_eq!(message.data["tokens"]["input"], 100);
    assert_eq!(
        effects.summaries,
        vec![(SESSION_ID.to_owned(), MESSAGE_ID.to_owned())]
    );
    assert_eq!(effects.overflow_checks.len(), 1);
}

#[test]
fn stream_event_families_project_to_their_terminal_part_shapes() {
    let connection = seeded();
    let mut effects = RecordingEffects::default();
    let mut projector = StreamProjector::start(&connection, context(), &mut effects)
        .expect("start stream projection");

    for event in [
        StreamEvent::ReasoningStart,
        StreamEvent::ReasoningDelta("think".to_owned()),
        StreamEvent::ReasoningSignatureDelta("signature".to_owned()),
        StreamEvent::ReasoningEnd,
        StreamEvent::ReasoningStart,
        StreamEvent::ReasoningDelta("timed".to_owned()),
        StreamEvent::ReasoningDone { duration_secs: 1.5 },
        StreamEvent::ToolUseStart {
            id: "provider-call".to_owned(),
            name: "read".to_owned(),
        },
        StreamEvent::ToolInputDelta(r#"{"filePath":"README.md"}"#.to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::ToolUseSignature(ThoughtSignature::new("tool-signature")),
        StreamEvent::ToolResult {
            tool_use_id: "provider-call".to_owned(),
            content: "contents".to_owned(),
            is_error: false,
        },
        StreamEvent::GeneratedImage {
            id: "image-1".to_owned(),
            path: "/workspace/generated.png".to_owned(),
            metadata_path: Some("/workspace/generated.json".to_owned()),
            output_format: "png".to_owned(),
            revised_prompt: Some("a generated image".to_owned()),
        },
        StreamEvent::ProviderReasoningItem {
            id: "reasoning-1".to_owned(),
            summary: vec!["summary".to_owned()],
            encrypted_content: Some("encrypted".to_owned()),
            status: Some("completed".to_owned()),
        },
        StreamEvent::Compaction {
            trigger: "overflow".to_owned(),
            pre_tokens: Some(10_000),
            openai_encrypted_content: Some("compact-state".to_owned()),
        },
        StreamEvent::NativeToolCall {
            request_id: "native-call".to_owned(),
            tool_name: "search".to_owned(),
            input: json!({ "query": "rust" }),
        },
        StreamEvent::ConnectionType {
            connection: "http2".to_owned(),
        },
        StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::Streaming,
        },
        StreamEvent::StatusDetail {
            detail: "streaming".to_owned(),
        },
        StreamEvent::Error {
            message: "transient".to_owned(),
            retry_after: Some(Duration::from_millis(1)),
        },
        StreamEvent::SessionId("provider-session".to_owned()),
        StreamEvent::UpstreamProvider {
            provider: "upstream".to_owned(),
        },
        StreamEvent::TextDelta("answer".to_owned()),
        StreamEvent::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_input_tokens: Some(2),
            cache_write_input_tokens: Some(1),
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ] {
        projector.apply(event).expect("project stream event");
    }
    drop(projector);

    let parts = message_parts(&connection);
    assert!(parts.iter().any(|part| part.kind == PartKind::Text));
    assert_eq!(
        parts
            .iter()
            .filter(|part| part.kind == PartKind::Reasoning)
            .count(),
        3
    );
    assert_eq!(
        parts
            .iter()
            .filter(|part| part.kind == PartKind::Tool)
            .count(),
        2
    );
    assert!(parts.iter().any(|part| part.kind == PartKind::File));
    assert!(parts.iter().any(|part| part.kind == PartKind::Compaction));
    assert!(parts.iter().any(|part| part.kind == PartKind::StepStart));
    assert!(parts.iter().any(|part| part.kind == PartKind::StepFinish));
    let completed_tool = parts
        .iter()
        .find(|part| {
            part.data.get("callID").and_then(serde_json::Value::as_str) == Some("provider-call")
        })
        .expect("provider tool part");
    assert_eq!(completed_tool.data["state"]["status"], "completed");
    assert_eq!(completed_tool.data["state"]["output"], "contents");
    assert_eq!(
        completed_tool.data["metadata"]["thoughtSignature"],
        "tool-signature"
    );
}
