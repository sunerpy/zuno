//! Reasoning must survive the transcript round trip, or the next request 400s.
//!
//! A provider that seals its reasoning — Anthropic's signed thinking, or the
//! encrypted capsule an OpenAI-Responses-shaped gateway returns for an
//! Anthropic-family model — requires that reasoning back, unmodified, and first,
//! on every later request in the same assistant turn. Dropping it is not a silent
//! degradation: the provider rejects the turn.
//!
//! These tests live apart from `loop.rs` because they exercise one seam — the
//! stored-part to request-block projection — and need none of that file's
//! scripted-provider machinery.

use serde_json::{Value, json};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_engine::r#loop::{project_history, project_history_owned};
use zuno_llm::event::{Message, RequestContentBlock, Role};

const SESSION_ID: &str = "ses_reasoning_replay";
const SYSTEM: &str = "You are a deterministic test agent.";

fn assistant_info(id: &str, created: i64, finish: &str) -> MessageRecord {
    MessageRecord::from_json(json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": "msg_replay_user",
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 1,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": finish
    }))
    .expect("valid assistant message")
}

/// The reasoning part exactly as `stream.rs::persist_provider_reasoning` writes it.
fn native_reasoning_part(
    part_id: &str,
    message_id: &str,
    capsule_id: &str,
    summary: &[&str],
    encrypted: &str,
) -> PartRecord {
    PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "reasoning",
            "text": summary.join("\n"),
            "metadata": {
                "providerReasoning": {
                    "id": capsule_id,
                    "summary": summary,
                    "encryptedContent": encrypted,
                    "status": "completed",
                }
            },
            "time": { "start": 10, "end": 10 }
        }),
        10,
    )
    .expect("valid native reasoning part")
}

/// The reasoning part exactly as `stream.rs::finish_reasoning` writes it.
fn signed_reasoning_part(
    part_id: &str,
    message_id: &str,
    thinking: &str,
    signature: &str,
) -> PartRecord {
    PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "reasoning",
            "text": thinking,
            "metadata": { "signature": signature },
            "time": { "start": 10, "end": 10 }
        }),
        10,
    )
    .expect("valid signed reasoning part")
}

fn tool_part(part_id: &str, message_id: &str, call_id: &str, state: Value) -> PartRecord {
    PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "tool",
            "callID": call_id,
            "tool": "write",
            "state": state
        }),
        10,
    )
    .expect("valid tool part")
}

fn text_part(part_id: &str, message_id: &str, text: &str) -> PartRecord {
    PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "text",
            "text": text
        }),
        10,
    )
    .expect("valid text part")
}

/// A permission gate refusing the call. The state a denial leaves behind.
fn denied_state() -> Value {
    json!({
        "status": "error",
        "input": { "filePath": "/tmp/zunodemo/main.go", "content": "package main" },
        "error": "denied `write`: permission `external_directory` resolves to ask"
    })
}

/// The tool itself failing. A different error, same stored shape.
fn failed_state() -> Value {
    json!({
        "status": "error",
        "input": { "filePath": "/tmp/zunodemo/go.mod", "content": "module demo" },
        "error": "write error"
    })
}

fn replayable_reasoning(messages: &[Message]) -> Vec<&RequestContentBlock> {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter(|block| {
            matches!(
                block,
                RequestContentBlock::ProviderEncryptedReasoning { .. }
                    | RequestContentBlock::SignedThinking { .. }
            )
        })
        .collect()
}

fn projected(history: &[MessageWithParts]) -> Vec<Message> {
    let borrowed = project_history(SYSTEM, history)
        .into_iter()
        .map(|projection| projection.message)
        .collect::<Vec<_>>();
    let owned = project_history_owned(SYSTEM, history.to_vec());
    assert_eq!(
        borrowed, owned,
        "the borrowed and owned projections disagree, so a request built by the \
         runtime path carries different bytes than the one compaction measures"
    );
    owned
}

#[test]
fn reasoning_survives_a_denied_and_a_failed_tool_call_in_the_same_turn() {
    // The live failure this pins, in the order it happened: reasoning, two tool
    // calls that both come back as errors — one refused by the permission gate,
    // one failing on its own — then more assistant prose, then the next request.
    //
    // `persist_provider_reasoning` stores the capsule under
    // `metadata.providerReasoning`. If the projection only knows
    // `metadata.signature`, the capsule vanishes from the replayed turn and an
    // Anthropic-family model behind any gateway answers HTTP 400, because the
    // assistant message it is handed no longer opens with the reasoning it sealed.
    let id = "msg_replay_denied";
    let history = vec![MessageWithParts {
        info: assistant_info(id, 10, "tool-calls"),
        parts: vec![
            native_reasoning_part(
                "prt_replay_reasoning",
                id,
                "rs_capsule_0001",
                &["Deciding where the files belong"],
                "Base64EncryptedCapsulePayload==",
            ),
            tool_part("prt_replay_denied", id, "call_denied", denied_state()),
            tool_part("prt_replay_failed", id, "call_failed", failed_state()),
            text_part(
                "prt_replay_text",
                id,
                "Both writes were denied. Let me check where I actually am.",
            ),
        ],
    }];

    let messages = projected(&history);
    let reasoning = replayable_reasoning(&messages);
    assert_eq!(
        reasoning.len(),
        1,
        "the reasoning capsule was dropped from the replayed turn, so the next \
         request opens an assistant message the provider sealed with reasoning: \
         {messages:#?}"
    );
    match reasoning[0] {
        RequestContentBlock::ProviderEncryptedReasoning {
            id: capsule_id,
            summary,
            encrypted_content,
            status,
        } => {
            assert_eq!(capsule_id, "rs_capsule_0001");
            assert_eq!(summary, &["Deciding where the files belong".to_owned()]);
            assert_eq!(
                encrypted_content.as_deref(),
                Some("Base64EncryptedCapsulePayload=="),
                "the capsule must be replayed byte-for-byte"
            );
            assert_eq!(status.as_deref(), Some("completed"));
        }
        other => panic!("the capsule came back as the wrong shape: {other:?}"),
    }

    // Both failed calls must still be answered, or the provider rejects the turn
    // for a dangling tool call instead — a different 400 with the same effect.
    let results = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            RequestContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => Some((tool_use_id.as_str(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![("call_denied", Some(true)), ("call_failed", Some(true))],
        "a denied or failed call must still be answered to the provider"
    );

    // Anthropic-family models reject a replayed reasoning block that is not the
    // first content of its assistant message.
    let assistant = messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .expect("an assistant message is projected");
    assert!(
        matches!(
            assistant.content.first(),
            Some(RequestContentBlock::ProviderEncryptedReasoning { .. })
        ),
        "reasoning is not the assistant turn's prefix: {:#?}",
        assistant.content
    );
}

#[test]
fn a_reasoning_signature_and_capsule_survive_the_projection_byte_for_byte() {
    // A signature that is truncated, re-encoded, line-folded, or paired with
    // another block's text is exactly what reads as "reasoning prefix does not
    // match" at the provider. Both replayable shapes are asserted on their bytes.
    let signature = "ErUBCkYIBRgCKkDwq5c9+SIGNATURE/bytes==\nsecond line\ttab ";
    let capsule = "gAAAAABn+CAPSULE/bytes==\nwith a newline\tand a tab ";

    let id = "msg_replay_bytes";
    let history = vec![MessageWithParts {
        info: assistant_info(id, 20, "stop"),
        parts: vec![
            signed_reasoning_part(
                "prt_bytes_signed",
                id,
                "thinking text\nwith bytes",
                signature,
            ),
            native_reasoning_part(
                "prt_bytes_capsule",
                id,
                "rs_bytes_0002",
                &["summary one", "summary two"],
                capsule,
            ),
        ],
    }];

    let messages = projected(&history);
    let blocks = replayable_reasoning(&messages);
    assert_eq!(
        blocks.len(),
        2,
        "both replayable reasoning shapes must survive: {messages:#?}"
    );
    match blocks[0] {
        RequestContentBlock::SignedThinking {
            thinking,
            signature: replayed,
        } => {
            assert_eq!(thinking, "thinking text\nwith bytes");
            assert_eq!(
                replayed, signature,
                "the signature did not survive byte-for-byte"
            );
            assert_eq!(
                replayed.len(),
                signature.len(),
                "the signature was truncated, padded, or re-encoded"
            );
        }
        other => panic!("expected signed thinking first, got {other:?}"),
    }
    match blocks[1] {
        RequestContentBlock::ProviderEncryptedReasoning {
            id: capsule_id,
            summary,
            encrypted_content,
            status,
        } => {
            assert_eq!(capsule_id, "rs_bytes_0002");
            assert_eq!(
                summary,
                &["summary one".to_owned(), "summary two".to_owned()],
                "the summary lines must not be re-joined into one or re-split"
            );
            assert_eq!(
                encrypted_content.as_deref(),
                Some(capsule),
                "the encrypted capsule did not survive byte-for-byte"
            );
            assert_eq!(
                encrypted_content.as_deref().map(str::len),
                Some(capsule.len()),
                "the capsule was truncated, padded, or re-encoded"
            );
            assert_eq!(status.as_deref(), Some("completed"));
        }
        other => panic!("expected an encrypted capsule second, got {other:?}"),
    }
}

#[test]
fn a_capsule_without_encrypted_content_is_not_replayed() {
    // A capsule the provider never sealed cannot be replayed as one: the OpenAI
    // Responses request builder rejects a reasoning item with no
    // `encrypted_content`, so projecting one would trade a provider 400 for a
    // fatal request-shape error. Reasoning with nothing to prove is history only.
    let id = "msg_replay_unsealed";
    let history = vec![MessageWithParts {
        info: assistant_info(id, 30, "stop"),
        parts: vec![
            PartRecord::from_json(
                json!({
                    "id": "prt_unsealed",
                    "sessionID": SESSION_ID,
                    "messageID": id,
                    "type": "reasoning",
                    "text": "a summary with no capsule",
                    "metadata": {
                        "providerReasoning": {
                            "id": "rs_unsealed",
                            "summary": ["a summary with no capsule"],
                            "encryptedContent": Value::Null,
                            "status": "completed",
                        }
                    },
                    "time": { "start": 30, "end": 30 }
                }),
                30,
            )
            .expect("valid unsealed reasoning part"),
            text_part("prt_unsealed_text", id, "Here is the answer."),
        ],
    }];

    let messages = projected(&history);
    assert!(
        replayable_reasoning(&messages).is_empty(),
        "an unsealed capsule must not be replayed: {messages:#?}"
    );
    let assistant = messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .expect("an assistant message is projected");
    assert_eq!(
        assistant.content,
        vec![RequestContentBlock::Text {
            text: "Here is the answer.".to_owned()
        }],
        "dropping the unsealed capsule must not disturb the rest of the turn"
    );
}

#[test]
fn plain_reasoning_with_no_signature_or_capsule_stays_history_only() {
    // Unchanged behaviour, pinned so restoring the capsule does not accidentally
    // start replaying unsigned reasoning — which Anthropic rejects outright.
    let id = "msg_replay_plain";
    let history = vec![MessageWithParts {
        info: assistant_info(id, 40, "stop"),
        parts: vec![
            PartRecord::from_json(
                json!({
                    "id": "prt_plain",
                    "sessionID": SESSION_ID,
                    "messageID": id,
                    "type": "reasoning",
                    "text": "unsigned thinking",
                    "time": { "start": 40, "end": 40 }
                }),
                40,
            )
            .expect("valid plain reasoning part"),
            text_part("prt_plain_text", id, "Done."),
        ],
    }];

    let messages = projected(&history);
    assert!(
        replayable_reasoning(&messages).is_empty(),
        "unsigned reasoning must stay out of requests: {messages:#?}"
    );
}

/// A capsule the provider streamed must come back out of the database.
///
/// Every fixture above starts from a hand-written `PartRecord`, which is why they
/// all passed while nothing in production wrote one: the only writer of
/// `metadata.providerReasoning` was `StreamProjector::persist_provider_reasoning`,
/// and it had no production callers, so the accumulated capsule was cleared at the
/// end of each step without ever being persisted. This test starts one step
/// earlier — from the `StreamEvent` a provider actually emits — and runs the real
/// `run_turn` persistence path, so it cannot be satisfied by a fixture.
///
/// It also pins the position: the capsule must be the assistant message's *first*
/// content block, because that is the only arrangement the sealing models accept.
mod production_round_trip {
    use super::{SESSION_ID, SYSTEM};

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::stream;
    use serde_json::json;
    use tokio::sync::mpsc;
    use zuno_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
    use zuno_db::{Connection, migration, open};
    use zuno_engine::interrupt::InterruptSignal;
    use zuno_engine::r#loop::{
        AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
        ResolvedModel, RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnEvent,
        event_channel, project_history_owned, run_turn,
    };
    use zuno_error::ProviderError;
    use zuno_llm::cache::{DynamicContext, McpToolStatus};
    use zuno_llm::event::{FinishReason, RequestContentBlock, Role, StreamEvent};
    use zuno_llm::registry::{
        ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream,
        Spec,
    };

    const CAPSULE_ID: &str = "rs_production_capsule";
    const CIPHERTEXT: &str = "ENCRYPTED-CAPSULE-PRODUCTION-PATH";

    #[derive(Debug)]
    struct ScriptedProvider(Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>);

    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::text_only()
        }

        fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
            let events = self
                .0
                .lock()
                .expect("response lock")
                .pop_front()
                .expect("one scripted response per request");
            Box::pin(stream::iter(events))
        }
    }

    #[derive(Debug)]
    struct Resolver;

    impl AgentModelResolver for Resolver {
        fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
            (requested == "build").then(|| {
                ResolvedAgent::new("build", SYSTEM)
                    .with_max_steps(std::num::NonZeroU32::new(4).expect("test limit is non-zero"))
            })
        }

        fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
            (provider_id == "scripted" && model_id == "scripted-model").then(|| {
                ResolvedModel::new(
                    Spec::new("scripted"),
                    "scripted-model",
                    ApiSurface::Responses,
                )
            })
        }
    }

    #[derive(Debug, Default)]
    struct NoTools;

    #[async_trait]
    impl ToolDispatcher for NoTools {
        fn available_tools(&self) -> AvailableTools {
            AvailableTools::new(Vec::new(), McpToolStatus::Ready)
        }

        async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
            PreparedToolDispatch::ready(ToolDispatchResult::error(zuno_tool::ToolOutput::text(
                "none", "no tools",
            )))
        }
    }

    fn seeded() -> Connection {
        let mut connection =
            open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-capsule', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('{SESSION_ID}', 'project-capsule', 'capsule', '/workspace', 'c', '1', 1, 1);"
            ))
            .expect("seed project and session");
        let store = MessageStore::new(&connection);
        let user = MessageRecord::from_json(json!({
            "id": "msg_capsule_user",
            "sessionID": SESSION_ID,
            "role": "user",
            "time": { "created": 10 },
            "agent": "build",
            "model": { "providerID": "scripted", "modelID": "scripted-model" }
        }))
        .expect("valid user message");
        store.put_message_at(&user, 10).expect("store user message");
        let part = PartRecord::from_json(
            json!({
                "id": "prt_capsule_user",
                "sessionID": SESSION_ID,
                "messageID": "msg_capsule_user",
                "type": "text",
                "text": "Read the fixture and summarise it."
            }),
            10,
        )
        .expect("valid text part");
        store.put_part_at(&part, 10).expect("store user part");
        connection
    }

    async fn drain(mut receiver: mpsc::Receiver<TurnEvent>) {
        while receiver.recv().await.is_some() {}
    }

    #[tokio::test]
    async fn a_streamed_capsule_is_persisted_and_replays_as_the_first_content_block() {
        let mut connection = seeded();
        let provider = Arc::new(ScriptedProvider(Mutex::new(VecDeque::from(vec![vec![
            Ok(StreamEvent::ProviderReasoningItem {
                id: CAPSULE_ID.to_owned(),
                summary: vec!["Deciding to read the fixture.".to_owned()],
                encrypted_content: Some(CIPHERTEXT.to_owned()),
                status: Some("completed".to_owned()),
            }),
            Ok(StreamEvent::TextDelta("Let me check that file.".to_owned())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            }),
        ]]))));
        let mut providers = ProviderRegistry::new();
        {
            let provider = Arc::clone(&provider);
            providers.register("scripted", move |_spec| provider.clone());
        }
        let resolver = Resolver;
        let dispatcher = NoTools;
        let interrupt = InterruptSignal::new();
        let (sender, receiver) = event_channel();

        let turn = run_turn(
            RunTurnRequest::new(SESSION_ID, "turn-capsule", DynamicContext::default()),
            TurnContext::new(
                &mut connection,
                &providers,
                &resolver,
                &dispatcher,
                &interrupt,
            ),
            sender,
        );
        let (outcome, ()) = tokio::join!(turn, drain(receiver));
        outcome.expect("the turn completes");

        let store = MessageStore::new(&connection);
        let history = store.hydrate_session(SESSION_ID).expect("hydrate session");

        let capsules: Vec<&PartRecord> = history
            .iter()
            .flat_map(|message| &message.parts)
            .filter(|part| {
                part.kind == PartKind::Reasoning
                    && part
                        .data
                        .get("metadata")
                        .and_then(|metadata| metadata.get("providerReasoning"))
                        .is_some()
            })
            .collect();
        assert_eq!(
            capsules.len(),
            1,
            "the streamed capsule was never persisted, so no later turn can replay it"
        );
        let stored = capsules[0]
            .data
            .get("metadata")
            .and_then(|metadata| metadata.get("providerReasoning"))
            .expect("the capsule metadata is present");
        assert_eq!(stored["id"], json!(CAPSULE_ID));
        assert_eq!(stored["encryptedContent"], json!(CIPHERTEXT));
        assert_eq!(stored["status"], json!("completed"));
        assert_eq!(
            stored["summary"],
            json!(["Deciding to read the fixture."]),
            "the summary must survive verbatim, it is part of what the provider signed"
        );

        let messages = project_history_owned(SYSTEM, history);
        let assistant = messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .expect("the turn persisted an assistant message");
        match assistant.content.first() {
            Some(RequestContentBlock::ProviderEncryptedReasoning {
                id,
                encrypted_content,
                ..
            }) => {
                assert_eq!(id, CAPSULE_ID);
                assert_eq!(encrypted_content.as_deref(), Some(CIPHERTEXT));
            }
            other => panic!(
                "the capsule must be the assistant message's first content block, found {other:#?} \
                 in {:#?}",
                assistant.content
            ),
        }
    }
}
