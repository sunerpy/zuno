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
use zuno_engine::r#loop::{
    ReasoningReplayScope, project_history, project_history_owned, withhold_unreplayable_capsules,
};
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

/// One assistant turn whose reasoning the provider sealed, stamped at `created`.
fn sealed_history(created: i64) -> Vec<MessageWithParts> {
    let id = "msg_replay_scope";
    vec![MessageWithParts {
        info: assistant_info(id, created, "stop"),
        parts: vec![
            native_reasoning_part(
                "prt_scope_capsule",
                id,
                "rs_scope_0001",
                &["Scoping the replay"],
                "SEALED-ENVELOPE",
            ),
            text_part("prt_scope_text", id, "Answer."),
        ],
    }]
}

const HOUR_MS: i64 = 60 * 60 * 1000;

#[test]
fn the_model_that_sealed_a_capsule_replays_it() {
    let mut history = sealed_history(10);
    let withheld = withhold_unreplayable_capsules(
        &mut history,
        ReasoningReplayScope::Model {
            provider_id: "fake",
            model_id: "fake-model",
            now: 10 + HOUR_MS,
            max_age: None,
        },
    );

    assert_eq!(withheld, 0, "the sealing model must keep its own envelope");
    assert_eq!(
        replayable_reasoning(&projected(&history)).len(),
        1,
        "scoping withheld a capsule from the model that minted it"
    );
}

#[test]
fn a_capsule_another_model_sealed_is_withheld() {
    // The envelope is bound to a model. Echoing it to a different one fails the
    // whole request, so it has to leave the projection before the request is built —
    // switching model mid-session is a normal thing to do.
    for (provider_id, model_id) in [("fake", "other-model"), ("other", "fake-model")] {
        let mut history = sealed_history(10);
        let withheld = withhold_unreplayable_capsules(
            &mut history,
            ReasoningReplayScope::Model {
                provider_id,
                model_id,
                now: 10,
                max_age: None,
            },
        );

        assert_eq!(
            withheld, 1,
            "a capsule sealed by fake/fake-model must not travel to {provider_id}/{model_id}"
        );
        let messages = projected(&history);
        assert!(
            replayable_reasoning(&messages).is_empty(),
            "the withheld capsule still reached {provider_id}/{model_id}: {messages:#?}"
        );
        let assistant = messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .expect("an assistant message is projected");
        assert_eq!(
            assistant.content,
            vec![RequestContentBlock::Text {
                text: "Answer.".to_owned()
            }],
            "withholding a capsule must not disturb the rest of the turn"
        );
    }
}

#[test]
fn an_expired_capsule_is_withheld_and_a_fresh_one_is_not() {
    let mut expired = sealed_history(10);
    assert_eq!(
        withhold_unreplayable_capsules(
            &mut expired,
            ReasoningReplayScope::Model {
                provider_id: "fake",
                model_id: "fake-model",
                now: 10 + 2 * HOUR_MS,
                max_age: Some(std::time::Duration::from_millis(HOUR_MS as u64)),
            },
        ),
        1,
        "an envelope older than the configured age must leave the request"
    );
    assert!(replayable_reasoning(&projected(&expired)).is_empty());

    let mut fresh = sealed_history(10);
    assert_eq!(
        withhold_unreplayable_capsules(
            &mut fresh,
            ReasoningReplayScope::Model {
                provider_id: "fake",
                model_id: "fake-model",
                now: 10 + 2 * HOUR_MS,
                max_age: Some(std::time::Duration::from_millis(3 * HOUR_MS as u64)),
            },
        ),
        0,
        "an envelope inside the configured age must still be replayed"
    );
    assert_eq!(replayable_reasoning(&projected(&fresh)).len(), 1);
}

#[test]
fn an_auxiliary_request_replays_no_capsule_at_all() {
    // Title, summary and compaction run on their own model. None of them sealed
    // anything, so every envelope is withheld rather than gambled on.
    let mut history = sealed_history(10);
    assert_eq!(
        withhold_unreplayable_capsules(&mut history, ReasoningReplayScope::None),
        1
    );
    assert!(replayable_reasoning(&projected(&history)).is_empty());
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
    use serde_json::{Value, json};
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
    const OTHER_MODEL: &str = "other-model";

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

    /// The resolver, carrying the replay options the provider spec declares.
    ///
    /// The options are a field rather than a constant because the engine reads them
    /// from the resolved model's spec on every step: that read is the wiring between
    /// configuration and the request, and a suite that hard-codes one value cannot
    /// tell whether the wiring exists.
    #[derive(Debug)]
    struct Resolver {
        /// The `reasoningReplay` value, or `None` for a spec that omits the key.
        replay: Option<Value>,
        /// The `reasoningReplayMaxAge` value in milliseconds, if declared.
        max_age: Option<u64>,
    }

    impl Resolver {
        /// A declared sealing endpoint with no configured envelope age.
        fn encrypted() -> Self {
            Self {
                replay: Some(json!("encrypted")),
                max_age: None,
            }
        }

        /// A provider that has switched replay back off.
        fn off() -> Self {
            Self {
                replay: Some(json!("off")),
                max_age: None,
            }
        }

        /// A sealing endpoint whose envelopes expire after `millis`.
        fn expiring(millis: u64) -> Self {
            Self {
                replay: Some(json!("encrypted")),
                max_age: Some(millis),
            }
        }
    }

    impl AgentModelResolver for Resolver {
        fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
            (requested == "build").then(|| {
                ResolvedAgent::new("build", SYSTEM)
                    .with_max_steps(std::num::NonZeroU32::new(4).expect("test limit is non-zero"))
            })
        }

        fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
            // Two models on one sealing endpoint, which is what makes a switch
            // mid-session expressible: the envelope one of them minted is worthless
            // to the other.
            (provider_id == "scripted" && (model_id == "scripted-model" || model_id == OTHER_MODEL))
                .then(|| {
                    let mut spec = Spec::new("scripted");
                    if let Some(replay) = self.replay.clone() {
                        spec = spec.with_option("reasoningReplay", replay);
                    }
                    if let Some(max_age) = self.max_age {
                        spec = spec.with_option("reasoningReplayMaxAge", json!(max_age));
                    }
                    ResolvedModel::new(spec, model_id, ApiSurface::Responses)
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

    /// One more user message, so the next turn has something to answer.
    fn seed_user(connection: &Connection, message_id: &str, model_id: &str, created: i64) {
        let store = MessageStore::new(connection);
        let user = MessageRecord::from_json(json!({
            "id": message_id,
            "sessionID": SESSION_ID,
            "role": "user",
            "time": { "created": created },
            "agent": "build",
            "model": { "providerID": "scripted", "modelID": model_id }
        }))
        .expect("valid user message");
        store
            .put_message_at(&user, created)
            .expect("store user message");
        let part = PartRecord::from_json(
            json!({
                "id": format!("prt_{message_id}"),
                "sessionID": SESSION_ID,
                "messageID": message_id,
                "type": "text",
                "text": "Carry on."
            }),
            created,
        )
        .expect("valid text part");
        store.put_part_at(&part, created).expect("store user part");
    }

    /// The `started` provider-request events of one session, oldest first.
    ///
    /// One request appends two events under the same type, a `started` and a terminal
    /// one; the replay evidence lives on the first.
    fn provider_requests(connection: &Connection) -> Vec<serde_json::Value> {
        let mut statement = connection
            .prepare(
                "SELECT data FROM event \
                 WHERE aggregate_id = ?1 AND type = 'session.provider.request.1' ORDER BY seq",
            )
            .expect("prepare provider request events");
        statement
            .query_map([SESSION_ID], |row| row.get::<_, String>(0))
            .expect("query provider request events")
            .map(|row| {
                serde_json::from_str::<serde_json::Value>(&row.expect("provider request event"))
                    .expect("provider request JSON")
            })
            .filter(|event: &serde_json::Value| event["status"] == "started")
            .collect()
    }

    /// Every part of one message, in the order the hydrator yields it.
    fn part_ids(connection: &Connection, message_id: &str) -> Vec<String> {
        MessageStore::new(connection)
            .hydrate_session(SESSION_ID)
            .expect("hydrate session")
            .into_iter()
            .filter(|message| message.info.id == message_id)
            .flat_map(|message| message.parts)
            .map(|part| part.id)
            .collect()
    }

    /// The sealed envelope stored on one part, if it still has one.
    fn stored_envelope(connection: &Connection, part_id: &str) -> Option<String> {
        MessageStore::new(connection)
            .hydrate_session(SESSION_ID)
            .expect("hydrate session")
            .into_iter()
            .flat_map(|message| message.parts)
            .find(|part| part.id == part_id)
            .and_then(|part| {
                part.data
                    .get("metadata")?
                    .get("providerReasoning")?
                    .get("encryptedContent")?
                    .as_str()
                    .map(str::to_owned)
            })
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
        let resolver = Resolver::encrypted();
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

    /// Wrap scripted events as one provider response.
    fn ok(events: Vec<StreamEvent>) -> Vec<Result<StreamEvent, ProviderError>> {
        events.into_iter().map(Ok).collect()
    }

    /// One sealed reasoning item, as a sealing endpoint streams it.
    fn sealed(id: &str, envelope: &str) -> StreamEvent {
        StreamEvent::ProviderReasoningItem {
            id: id.to_owned(),
            summary: Vec::new(),
            encrypted_content: Some(envelope.to_owned()),
            status: Some("completed".to_owned()),
        }
    }

    /// One complete tool call: start, arguments, end.
    fn call(id: &str, path: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseStart {
                id: id.to_owned(),
                name: "write".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: id.to_owned(),
                delta: format!("{{\"filePath\":\"{path}\"}}"),
            },
            StreamEvent::ToolUseEnd { id: id.to_owned() },
        ]
    }

    /// A step that seals one capsule and answers with prose.
    fn sealing_step(id: &str, envelope: &str) -> Vec<Result<StreamEvent, ProviderError>> {
        ok(vec![
            sealed(id, envelope),
            StreamEvent::TextDelta("Answering.".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ])
    }

    /// A step that answers with prose alone.
    fn plain_step() -> Vec<Result<StreamEvent, ProviderError>> {
        ok(vec![
            StreamEvent::TextDelta("Carrying on.".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ])
    }

    /// Run one real turn against a declared sealing endpoint.
    async fn run_scripted(
        connection: &mut Connection,
        turn_id: &str,
        responses: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    ) {
        run_scripted_as(connection, turn_id, responses, &Resolver::encrypted()).await;
    }

    /// Run one real turn, with the provider's stream and its replay options scripted.
    async fn run_scripted_as(
        connection: &mut Connection,
        turn_id: &str,
        responses: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        resolver: &Resolver,
    ) {
        let provider = Arc::new(ScriptedProvider(Mutex::new(VecDeque::from(responses))));
        let mut providers = ProviderRegistry::new();
        providers.register("scripted", move |_spec| provider.clone());
        let dispatcher = NoTools;
        let interrupt = InterruptSignal::new();
        let (sender, receiver) = event_channel();
        let turn = run_turn(
            RunTurnRequest::new(SESSION_ID, turn_id, DynamicContext::default()),
            TurnContext::new(connection, &providers, resolver, &dispatcher, &interrupt),
            sender,
        );
        let (outcome, ()) = tokio::join!(turn, drain(receiver));
        outcome.expect("the turn completes");
    }

    /// Run one turn whose provider response is expected to fail the turn.
    ///
    /// A step that streams a sealed reasoning item and then nothing else has no
    /// assistant content, which the engine reports rather than persisting an empty
    /// message. The checkpoint still runs, so the capsule reaches the database — the
    /// durable shape this module's unpaired-capsule test needs.
    async fn run_scripted_failing(
        connection: &mut Connection,
        turn_id: &str,
        responses: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    ) {
        let provider = Arc::new(ScriptedProvider(Mutex::new(VecDeque::from(responses))));
        let mut providers = ProviderRegistry::new();
        providers.register("scripted", move |_spec| provider.clone());
        let dispatcher = NoTools;
        let resolver = Resolver::encrypted();
        let interrupt = InterruptSignal::new();
        let (sender, receiver) = event_channel();
        let turn = run_turn(
            RunTurnRequest::new(SESSION_ID, turn_id, DynamicContext::default()),
            TurnContext::new(connection, &providers, &resolver, &dispatcher, &interrupt),
            sender,
        );
        let (outcome, ()) = tokio::join!(turn, drain(receiver));
        outcome.expect_err("a step with no assistant content cannot complete");
    }

    /// The projected shape of the first assistant message, block kind by block kind.
    fn assistant_shape(connection: &Connection) -> Vec<String> {
        let history = MessageStore::new(connection)
            .hydrate_session(SESSION_ID)
            .expect("hydrate session");
        let messages = project_history_owned(SYSTEM, history);
        let assistant = messages
            .into_iter()
            .find(|message| message.role == Role::Assistant)
            .expect("the turn persisted an assistant message");
        assistant
            .content
            .iter()
            .map(|block| match block {
                RequestContentBlock::ProviderEncryptedReasoning { id, .. } => {
                    format!("capsule:{id}")
                }
                RequestContentBlock::Text { text } => format!("text:{text}"),
                RequestContentBlock::ToolUse { id, .. } => format!("call:{id}"),
                other => format!("unexpected:{other:?}"),
            })
            .collect()
    }

    /// A step that reasons twice and calls a tool after each reasoning item.
    ///
    /// The sealing endpoints reject a replayed conversation whose reasoning items are
    /// not in the order they were produced, each immediately before the output it
    /// explains. A step is therefore persisted as a ledger of positioned parts, not as
    /// one text blob plus a trailing pile of tool calls.
    #[tokio::test]
    async fn a_multi_item_step_is_persisted_and_replayed_in_stream_order() {
        let mut connection = seeded();
        let mut first = vec![
            sealed("rs_first", "SEALED-ONE"),
            StreamEvent::TextDelta("Writing the first file.".to_owned()),
        ];
        first.extend(call("call_one", "a.txt"));
        first.push(sealed("rs_second", "SEALED-TWO"));
        first.push(StreamEvent::TextDelta(
            "Writing the second file.".to_owned(),
        ));
        first.extend(call("call_two", "b.txt"));
        first.push(StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        });
        run_scripted(&mut connection, "turn-order", vec![ok(first), plain_step()]).await;

        let ids = part_ids(&connection, "msg_turn-order_0001");
        assert_eq!(
            ids.iter().map(String::as_str).collect::<Vec<&str>>(),
            vec![
                "prt_turn-order_0001_0000_reasoning_capsule",
                "prt_turn-order_0001_0001_text",
                "prt_turn-order_0001_0002_tool",
                "prt_turn-order_0001_0003_reasoning_capsule",
                "prt_turn-order_0001_0004_text",
                "prt_turn-order_0001_0005_tool",
            ],
            "the step's parts must be stored in the order the provider streamed them"
        );
        assert_eq!(
            assistant_shape(&connection),
            vec![
                "capsule:rs_first",
                "text:Writing the first file.",
                "call:call_one",
                "capsule:rs_second",
                "text:Writing the second file.",
                "call:call_two",
            ],
            "each capsule must be replayed immediately before the output it explains"
        );
    }

    /// A capsule the current model did not seal leaves the request, not the database.
    #[tokio::test]
    async fn switching_model_withholds_the_capsule_but_keeps_it_stored() {
        let mut connection = seeded();
        run_scripted(
            &mut connection,
            "turn-seal",
            vec![sealing_step(CAPSULE_ID, CIPHERTEXT)],
        )
        .await;
        seed_user(&connection, "msg_capsule_user_2", OTHER_MODEL, 20);
        run_scripted(&mut connection, "turn-switch", vec![plain_step()]).await;

        let events = provider_requests(&connection);
        assert_eq!(events.len(), 2, "one started event per foreground request");
        assert_eq!(events[0]["reasoningReplay"], json!("encrypted"));
        assert_eq!(events[0]["replayedReasoningCapsules"], json!(0));
        assert_eq!(
            events[1]["withheldReasoningCapsules"],
            json!(1),
            "the other model's envelope is worthless to this one and must be withheld"
        );
        assert_eq!(events[1]["replayedReasoningCapsules"], json!(0));
        assert_eq!(
            stored_envelope(&connection, "prt_turn-seal_0001_0000_reasoning_capsule"),
            Some(CIPHERTEXT.to_owned()),
            "withholding is a request-time decision; the durable row is never rewritten"
        );
    }

    /// The model that sealed a capsule replays it on its next request.
    #[tokio::test]
    async fn the_sealing_model_replays_its_own_capsule_on_the_next_request() {
        let mut connection = seeded();
        run_scripted(
            &mut connection,
            "turn-seal",
            vec![sealing_step(CAPSULE_ID, CIPHERTEXT)],
        )
        .await;
        seed_user(&connection, "msg_capsule_user_2", "scripted-model", 20);
        run_scripted(&mut connection, "turn-again", vec![plain_step()]).await;

        let events = provider_requests(&connection);
        assert_eq!(events.len(), 2, "one started event per foreground request");
        assert_eq!(
            events[1]["replayedReasoningCapsules"],
            json!(1),
            "the sealing model must get its own envelope back"
        );
        assert_eq!(events[1]["withheldReasoningCapsules"], json!(0));
    }

    /// An envelope with nothing after it is counted as withheld, not as replayed.
    ///
    /// A Responses endpoint validates the pairing positionally, so both adapters drop
    /// a sealed item that no output follows rather than earn a permanent HTTP 400.
    /// Durable history reaches that shape honestly: a step that streams its reasoning
    /// item and then ends leaves an assistant message whose only part is the capsule.
    /// The engine counts with the adapters' own rule, because a count that ignored it
    /// reported `replayedReasoningCapsules: 1` for the rest of the session while every
    /// request carried no reasoning item at all — and that count is what the operator
    /// documentation says to trust.
    #[tokio::test]
    async fn an_envelope_no_output_follows_is_counted_as_withheld() {
        let mut connection = seeded();
        run_scripted_failing(
            &mut connection,
            "turn-lonely",
            vec![ok(vec![
                sealed(CAPSULE_ID, CIPHERTEXT),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ])],
        )
        .await;
        assert_eq!(
            part_ids(&connection, "msg_turn-lonely_0001"),
            vec!["prt_turn-lonely_0001_0000_reasoning_capsule".to_owned()],
            "the capsule is the whole assistant message"
        );

        seed_user(&connection, "msg_capsule_user_2", "scripted-model", 20);
        run_scripted(&mut connection, "turn-after", vec![plain_step()]).await;

        let events = provider_requests(&connection);
        let last = events.last().expect("a second foreground request");
        assert_eq!(
            last["replayedReasoningCapsules"],
            json!(0),
            "the adapter cannot send an unpaired item, so the count must not claim it did"
        );
        assert_eq!(
            last["withheldReasoningCapsules"],
            json!(1),
            "the same envelope is reported once, as withheld"
        );
        assert_eq!(
            stored_envelope(&connection, "prt_turn-lonely_0001_0000_reasoning_capsule"),
            Some(CIPHERTEXT.to_owned()),
            "the durable row keeps the envelope either way"
        );
    }

    /// Switching replay off withholds the envelopes an earlier session sealed.
    ///
    /// `off` is not "stop asking for new envelopes", it is "no sealed item on the
    /// wire". A provider that once sealed has envelopes in durable history, and an
    /// endpoint that is no longer sent `include: ["reasoning.encrypted_content"]`
    /// must not be sent a `reasoning` item with ciphertext either: the item is
    /// unrequested, and the request event would otherwise claim
    /// `reasoningReplay: "off"` beside a non-zero replay count.
    #[tokio::test]
    async fn switching_replay_off_withholds_a_capsule_the_same_model_sealed() {
        let mut connection = seeded();
        run_scripted(
            &mut connection,
            "turn-seal",
            vec![sealing_step(CAPSULE_ID, CIPHERTEXT)],
        )
        .await;
        seed_user(&connection, "msg_capsule_user_2", "scripted-model", 20);
        run_scripted_as(
            &mut connection,
            "turn-off",
            vec![plain_step()],
            &Resolver::off(),
        )
        .await;

        let events = provider_requests(&connection);
        assert_eq!(events.len(), 2, "one started event per foreground request");
        assert_eq!(events[1]["reasoningReplay"], json!("off"));
        assert_eq!(
            events[1]["replayedReasoningCapsules"],
            json!(0),
            "an endpoint that is not asked for sealed reasoning must not receive it"
        );
        assert_eq!(events[1]["withheldReasoningCapsules"], json!(1));
        assert_eq!(
            stored_envelope(&connection, "prt_turn-seal_0001_0000_reasoning_capsule"),
            Some(CIPHERTEXT.to_owned()),
            "turning replay off is a request-time decision, not a deletion"
        );
    }

    /// The configured envelope age reaches the request, not just the scope helper.
    ///
    /// `reasoningReplayMaxAge` exists to drop an envelope Zuno believes has expired
    /// upstream before the endpoint rejects the whole request. Nothing else in the
    /// suite reads it out of a `Spec`, so without this test the option could be wired
    /// to nothing and every check would stay green.
    #[tokio::test]
    async fn the_configured_max_age_withholds_an_aged_capsule() {
        let mut connection = seeded();
        run_scripted_as(
            &mut connection,
            "turn-seal",
            vec![sealing_step(CAPSULE_ID, CIPHERTEXT)],
            &Resolver::expiring(1),
        )
        .await;
        // The stamp compared against the age is the assistant message's own, so the
        // envelope has to be measurably older than the one millisecond it is allowed.
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        seed_user(&connection, "msg_capsule_user_2", "scripted-model", 20);
        run_scripted_as(
            &mut connection,
            "turn-aged",
            vec![plain_step()],
            &Resolver::expiring(1),
        )
        .await;

        let events = provider_requests(&connection);
        assert_eq!(events.len(), 2, "one started event per foreground request");
        assert_eq!(events[1]["reasoningReplay"], json!("encrypted"));
        assert_eq!(
            events[1]["withheldReasoningCapsules"],
            json!(1),
            "an envelope past the configured age must leave the request"
        );
        assert_eq!(events[1]["replayedReasoningCapsules"], json!(0));
    }

    /// The same history, inside a generous configured age, is still replayed.
    ///
    /// Paired with the test above so a max age that withheld everything — or one read
    /// as zero — would fail here instead of reading as correct expiry.
    #[tokio::test]
    async fn a_capsule_inside_the_configured_max_age_is_replayed() {
        let mut connection = seeded();
        run_scripted_as(
            &mut connection,
            "turn-seal",
            vec![sealing_step(CAPSULE_ID, CIPHERTEXT)],
            &Resolver::expiring(86_400_000),
        )
        .await;
        seed_user(&connection, "msg_capsule_user_2", "scripted-model", 20);
        run_scripted_as(
            &mut connection,
            "turn-fresh",
            vec![plain_step()],
            &Resolver::expiring(86_400_000),
        )
        .await;

        let events = provider_requests(&connection);
        assert_eq!(events.len(), 2, "one started event per foreground request");
        assert_eq!(
            events[1]["replayedReasoningCapsules"],
            json!(1),
            "a day-old ceiling must not withhold an envelope sealed moments ago"
        );
        assert_eq!(events[1]["withheldReasoningCapsules"], json!(0));
    }

    /// The provider's own argument bytes survive the database, key order and all.
    ///
    /// A sealing endpoint fingerprints the turn it sealed, and the tool call's
    /// `arguments` string is part of that fingerprint. Re-serializing the parsed value
    /// is a different string — this workspace sorts object keys and drops the
    /// provider's spacing — so the envelope would no longer match what the endpoint
    /// sealed and the request would be refused. The executed value stays the parsed
    /// one; only the replayed text is the provider's.
    #[tokio::test]
    async fn a_tool_calls_provider_bytes_survive_the_database_verbatim() {
        const ARGUMENTS: &str = r#"{"filePath": "a.txt", "content": "hi"}"#;

        let mut connection = seeded();
        let step = ok(vec![
            sealed(CAPSULE_ID, CIPHERTEXT),
            StreamEvent::ToolUseStart {
                id: "call_raw".to_owned(),
                name: "write".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call_raw".to_owned(),
                delta: ARGUMENTS.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call_raw".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]);
        run_scripted(&mut connection, "turn-raw", vec![step, plain_step()]).await;

        let history = MessageStore::new(&connection)
            .hydrate_session(SESSION_ID)
            .expect("hydrate session");
        let calls: Vec<RequestContentBlock> = project_history_owned(SYSTEM, history)
            .into_iter()
            .flat_map(|message| message.content)
            .filter(|block| matches!(block, RequestContentBlock::ToolUse { .. }))
            .collect();
        let RequestContentBlock::ToolUse {
            input,
            raw_arguments,
            ..
        } = calls.first().expect("the call is projected back")
        else {
            unreachable!("filtered to tool calls")
        };
        assert_eq!(
            raw_arguments.as_deref(),
            Some(ARGUMENTS),
            "the bytes the provider streamed must come back out of the database"
        );
        assert_eq!(
            zuno_llm::event::tool_arguments_text(input, raw_arguments.as_deref()),
            ARGUMENTS,
            "the replayed arguments must be the provider's text, not a re-serialization"
        );
        assert_ne!(
            input.to_string(),
            ARGUMENTS,
            "this fixture is only evidence while re-serializing the value differs"
        );
    }
}
