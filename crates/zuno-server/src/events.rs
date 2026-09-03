//! Durable event storage and per-session bounded live delivery.

mod route;
mod store;
mod types;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use zuno_db::Pool;
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::event::{ConnectionPhase, StreamEvent as ProviderEvent};

use crate::{Delivery, EventFanout, EventSubscription};
use store::{Page, Snapshot, Store};

pub use route::events_router;
pub use types::{EventCursor, EventStreamError, NewEvent, StreamEvent};

#[derive(Debug)]
pub struct EventPage {
    pub events: Vec<StreamEvent>,
    pub has_more: bool,
}

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Durable event storage plus per-session bounded live fan-out.
#[derive(Clone)]
pub struct EventService {
    store: Arc<Store>,
    fanouts: Arc<Mutex<HashMap<String, EventFanout<StreamEvent>>>>,
    global: EventFanout<StreamEvent>,
    heartbeat_interval: Duration,
}

impl EventService {
    /// Creates a service over an initialized Zuno session database pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>, subscriber_capacity: usize) -> Self {
        Self {
            store: Arc::new(Store::new(pool, subscriber_capacity.max(1))),
            fanouts: Arc::new(Mutex::new(HashMap::new())),
            global: EventFanout::with_capacity(subscriber_capacity),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }

    /// Overrides the ten-second SSE keepalive cadence.
    #[must_use]
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Commits one event before offering it to live subscribers.
    pub async fn publish(
        &self,
        session_id: &str,
        event: NewEvent,
    ) -> Result<StreamEvent, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let store = Arc::clone(&self.store);
        let stored = tokio::task::spawn_blocking({
            let session_id = session_id.clone();
            move || store.append(&session_id, event)
        })
        .await
        .map_err(|source| EventStreamError::Worker { source })??;
        if let Some(fanout) = self.live_fanout(&session_id) {
            fanout.publish(stored.clone());
        }
        self.global.publish(stored.clone());
        Ok(stored)
    }

    /// Commits one event in the same transaction as the state that event asserts,
    /// then offers it to live subscribers.
    ///
    /// A route that publishes first and mutates second can commit a durable claim
    /// its own state never backs: the SSE stream and any replay report the change,
    /// while the row the change was about is unchanged. `mutate` runs inside the
    /// event's transaction, so either both commit or neither does, and the event
    /// reaches subscribers only after that commit.
    pub async fn publish_with<F>(
        &self,
        session_id: &str,
        event: NewEvent,
        mutate: F,
    ) -> Result<StreamEvent, EventStreamError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<(), zuno_error::DbError> + Send + 'static,
    {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let store = Arc::clone(&self.store);
        let stored = tokio::task::spawn_blocking({
            let session_id = session_id.clone();
            move || store.append_with(&session_id, event, mutate)
        })
        .await
        .map_err(|source| EventStreamError::Worker { source })??;
        if let Some(fanout) = self.live_fanout(&session_id) {
            fanout.publish(stored.clone());
        }
        self.global.publish(stored.clone());
        Ok(stored)
    }

    /// Reads committed events strictly after an optional cursor.
    pub async fn replay(
        &self,
        session_id: &str,
        cursor: Option<&EventCursor>,
    ) -> Result<Vec<StreamEvent>, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let after = types::checked_sequence(&session_id, cursor)?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.replay(&session_id, after))
            .await
            .map_err(|source| EventStreamError::Worker { source })?
    }

    pub async fn history_page(
        &self,
        session_id: &str,
        after: Option<i64>,
        limit: usize,
    ) -> Result<EventPage, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let store = Arc::clone(&self.store);
        let Page { events, has_more } =
            tokio::task::spawn_blocking(move || store.page(&session_id, after, limit))
                .await
                .map_err(|source| EventStreamError::Worker { source })??;
        Ok(EventPage { events, has_more })
    }

    /// Projects one engine channel onto both the process-local fan-out and
    /// the durable HTTP event stream. The durable write happens before live HTTP
    /// delivery, so `/history` can always replay an event observed over SSE.
    pub async fn forward_engine_events(
        &self,
        session_id: &str,
        local: &EventFanout<TurnEvent>,
        mut events: mpsc::Receiver<TurnEvent>,
    ) {
        while let Some(event) = events.recv().await {
            self.forward_engine_event(session_id, local, event).await;
        }
    }

    /// Project one engine event from a detached continuation turn.
    ///
    /// Detached turns have no request-owned channel to drain. Reusing this seam keeps
    /// their durable HTTP history and live fan-out identical to ordinary prompt turns.
    pub async fn forward_engine_event(
        &self,
        session_id: &str,
        local: &EventFanout<TurnEvent>,
        event: TurnEvent,
    ) {
        local.publish(event.clone());
        let projected = turn_event(&event);
        if let Err(error) = self.publish(session_id, projected).await {
            eprintln!("failed to publish HTTP turn event for `{session_id}`: {error}");
        }
    }

    async fn subscribe(
        &self,
        session_id: &str,
        cursor: Option<&EventCursor>,
    ) -> Result<SessionSubscription, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let after = types::checked_sequence(&session_id, cursor)?;
        let store = Arc::clone(&self.store);
        let checked_session = session_id.clone();
        let exists = tokio::task::spawn_blocking(move || store.session_exists(&checked_session))
            .await
            .map_err(|source| EventStreamError::Worker { source })??;
        if !exists {
            return Err(EventStreamError::SessionNotFound { session_id });
        }
        let live = self.subscribe_live(&session_id);
        let store = Arc::clone(&self.store);
        let snapshot_session = session_id.clone();
        let Snapshot { events, boundary } =
            tokio::task::spawn_blocking(move || store.snapshot(&snapshot_session, after))
                .await
                .map_err(|source| EventStreamError::Worker { source })??;
        Ok(SessionSubscription {
            session_id,
            events,
            boundary,
            live,
            cursor: cursor.cloned(),
        })
    }

    /// The fan-out for a session somebody is currently observing, if any.
    ///
    /// Publishing never creates one: an event for an unobserved session goes to the
    /// durable store and the global stream, and a later subscriber replays it from
    /// there.
    fn live_fanout(&self, session_id: &str) -> Option<EventFanout<StreamEvent>> {
        self.lock_fanouts().get(session_id).cloned()
    }

    /// Registers a live subscriber, creating the session's fan-out on first use.
    ///
    /// Creation and subscription happen under the map lock, and the matching removal
    /// in [`LiveSessionSubscription::drop`] runs under the same lock, so a subscriber
    /// can never attach to a fan-out that a concurrent last-unsubscribe is discarding.
    fn subscribe_live(&self, session_id: &str) -> LiveSessionSubscription {
        let live = self
            .lock_fanouts()
            .entry(session_id.to_owned())
            .or_insert_with(|| EventFanout::with_capacity(self.store.subscriber_capacity()))
            .subscribe();
        LiveSessionSubscription {
            session_id: session_id.to_owned(),
            live: Some(live),
            fanouts: Arc::clone(&self.fanouts),
        }
    }

    fn subscribe_global(&self) -> EventSubscription<StreamEvent> {
        self.global.subscribe()
    }

    fn lock_fanouts(&self) -> MutexGuard<'_, HashMap<String, EventFanout<StreamEvent>>> {
        self.fanouts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn turn_event(event: &TurnEvent) -> NewEvent {
    let (event_type, properties) = match event {
        TurnEvent::SessionMaterialized { session_id, title } => (
            "session.materialized",
            object(json!({"sessionID": session_id, "title": title})),
        ),
        TurnEvent::SessionTitleUpdated { title } => {
            ("session.title.updated", object(json!({"title": title})))
        }
        TurnEvent::SessionCommandStarted { command } => (
            "session.command.started",
            object(json!({"command": command.name()})),
        ),
        TurnEvent::SessionCommandOutput { command, content } => (
            "session.command.output",
            object(json!({"command": command.name(), "content": content})),
        ),
        TurnEvent::SessionCommandCompleted { command } => (
            "session.command.completed",
            object(json!({"command": command.name()})),
        ),
        TurnEvent::SessionCommandFailed { command, message } => (
            "session.command.failed",
            object(json!({"command": command.name(), "message": message})),
        ),
        TurnEvent::SkillLoaded { name, source } => (
            "skill.loaded",
            object(json!({"name": name, "source": source})),
        ),
        TurnEvent::Notice {
            severity,
            code,
            detail,
        } => (
            "notice",
            object(json!({
                "severity": severity.as_str(),
                "code": code,
                "detail": detail,
            })),
        ),
        TurnEvent::TurnStarted { session_id } => {
            ("turn.started", object(json!({"sessionID": session_id})))
        }
        TurnEvent::HistoryRepaired {
            repaired_tool_results,
        } => (
            "history.repaired",
            object(json!({"repairedToolResults": repaired_tool_results})),
        ),
        TurnEvent::AgentResolved { step, agent } => (
            "agent.resolved",
            object(json!({"step": step, "agent": agent})),
        ),
        TurnEvent::ModelResolved {
            step,
            provider_id,
            model_id,
        } => (
            "model.resolved",
            object(json!({"step": step, "providerID": provider_id, "modelID": model_id})),
        ),
        TurnEvent::AssistantMessageCreated { step, message_id } => (
            "assistant.message.created",
            object(json!({"step": step, "messageID": message_id})),
        ),
        TurnEvent::ToolSnapshotLocked {
            step,
            tool_ids,
            rebuilt_for_late_mcp,
        } => (
            "tool.snapshot.locked",
            object(json!({
                "step": step,
                "toolIDs": tool_ids,
                "rebuiltForLateMcp": rebuilt_for_late_mcp,
            })),
        ),
        TurnEvent::ProviderRequestStarted {
            step,
            message_count,
            estimated_prompt_tokens,
        } => (
            "provider.request.started",
            object(json!({
                "step": step,
                "messageCount": message_count,
                "estimatedPromptTokens": estimated_prompt_tokens,
            })),
        ),
        TurnEvent::Provider { step, event } => (
            "provider",
            object(json!({"step": step, "event": provider_event(event)})),
        ),
        TurnEvent::ToolCallStarted {
            step,
            call_id,
            display_name,
            name,
            ui_intent,
        } => (
            "tool.call.started",
            object(json!({
                "step": step,
                "callID": call_id,
                "name": name,
                "displayName": display_name,
                "uiIntent": ui_intent,
            })),
        ),
        TurnEvent::AssistantCheckpointed {
            step,
            message_id,
            interrupted,
        } => (
            "assistant.checkpointed",
            object(json!({
                "step": step,
                "messageID": message_id,
                "interrupted": interrupted,
            })),
        ),
        TurnEvent::ToolDispatchStarted {
            step,
            call_id,
            display_name,
            name,
            ..
        } => (
            "tool.dispatch.started",
            object(json!({
                "step": step,
                "callID": call_id,
                "name": name,
                "displayName": display_name,
            })),
        ),
        TurnEvent::ToolDispatchBlocked {
            step,
            call_id,
            kind,
        } => (
            "tool.dispatch.blocked",
            object(json!({
                "step": step,
                "callID": call_id,
                "kind": kind.as_str(),
            })),
        ),
        TurnEvent::ToolDispatchInterrupted {
            step,
            call_id,
            display_name,
            name,
            title,
            output,
            interruption,
            uncertain,
        } => (
            "tool.dispatch.interrupted",
            object(json!({
                "step": step,
                "callID": call_id,
                "name": name,
                "displayName": display_name,
                "title": title,
                "output": output,
                "mode": interruption.as_str(),
                // `forced` is the mode and nothing else: the grace window expired. It
                // rode on the certainty helper, which is now a per-call verdict a
                // cooperative return can also carry, so pin it to the mode explicitly.
                "forced": interruption.is_forced(),
                // The verdict the dispatcher resolved and stored, so a live SSE
                // consumer reads the same certainty as the durable tool result.
                "uncertain": uncertain,
            })),
        ),
        TurnEvent::ToolResultPresented {
            step,
            call_id,
            presentation,
        } => (
            "tool.result.presented",
            object(json!({
                "step": step,
                "callID": call_id,
                "presentation": presentation,
            })),
        ),
        TurnEvent::ToolDispatchCompleted {
            step,
            call_id,
            display_name,
            name,
            title,
            output,
            diff,
            written_paths,
            is_error,
        } => {
            let unified = diff.as_ref().and_then(|diff| diff.unified());
            let files = diff.as_ref().map_or(&[][..], |diff| diff.files());
            (
                "tool.dispatch.completed",
                object(json!({
                    "step": step,
                    "callID": call_id,
                    "name": name,
                    "displayName": display_name,
                    "title": title,
                    "output": output,
                    "diff": unified,
                    "fileDiffs": files,
                    "writtenPaths": written_paths,
                    "isError": is_error,
                })),
            )
        }
        TurnEvent::ToolResultAppended {
            step,
            call_id,
            is_error,
        } => (
            "tool.result.appended",
            object(json!({"step": step, "callID": call_id, "isError": is_error})),
        ),
        TurnEvent::StepCompleted {
            step,
            finish_reason,
        } => (
            "step.completed",
            object(json!({"step": step, "finishReason": finish_reason})),
        ),
        TurnEvent::TurnCompleted {
            assistant_message_id,
            steps,
        } => (
            "turn.completed",
            object(json!({"assistantMessageID": assistant_message_id, "steps": steps})),
        ),
        TurnEvent::TurnWaitingForHuman {
            assistant_message_id,
            steps,
            request_id,
        } => (
            "turn.waiting_for_human",
            object(json!({
                "assistantMessageID": assistant_message_id,
                "steps": steps,
                "requestID": request_id,
            })),
        ),
        TurnEvent::TurnInterrupted {
            assistant_message_id,
            steps,
            request,
        } => (
            "turn.interrupted",
            object(json!({
                "assistantMessageID": assistant_message_id,
                "steps": steps,
                "source": request.map(|request| request.source),
                "reason": request.map(|request| request.reason),
            })),
        ),
        TurnEvent::TurnFailed {
            assistant_message_id,
            steps,
            message,
        } => (
            "turn.failed",
            object(json!({
                "assistantMessageID": assistant_message_id,
                "steps": steps,
                "message": message
            })),
        ),
    };
    NewEvent::new(event_type, properties).expect("fixed turn event types are valid")
}

fn provider_event(event: &ProviderEvent) -> Value {
    match event {
        ProviderEvent::TextDelta(text) => json!({"type": "text.delta", "text": text}),
        ProviderEvent::ToolUseStart { id, name } => {
            json!({"type": "tool.use.start", "id": id, "name": name})
        }
        ProviderEvent::ToolInputDelta { id, delta } => {
            json!({"type": "tool.input.delta", "id": id, "delta": delta})
        }
        ProviderEvent::ToolUseEnd { id } => json!({"type": "tool.use.end", "id": id}),
        ProviderEvent::ToolUseSignature { id, signature } => {
            json!({"type": "tool.use.signature", "id": id, "signature": signature})
        }
        ProviderEvent::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool.result",
            "toolUseID": tool_use_id,
            "content": content,
            "isError": is_error,
        }),
        ProviderEvent::GeneratedImage {
            id,
            path,
            metadata_path,
            output_format,
            revised_prompt,
        } => json!({
            "type": "generated.image",
            "id": id,
            "path": path,
            "metadataPath": metadata_path,
            "outputFormat": output_format,
            "revisedPrompt": revised_prompt,
        }),
        ProviderEvent::ReasoningStart => json!({"type": "reasoning.start"}),
        ProviderEvent::ReasoningDelta(text) => {
            json!({"type": "reasoning.delta", "text": text})
        }
        ProviderEvent::ReasoningSignatureDelta(signature) => {
            json!({"type": "reasoning.signature.delta", "signature": signature})
        }
        ProviderEvent::ProviderReasoningItem {
            id,
            summary,
            encrypted_content,
            status,
        } => json!({
            "type": "provider.reasoning.item",
            "id": id,
            "summary": summary,
            "encryptedContent": encrypted_content,
            "status": status,
        }),
        ProviderEvent::ReasoningEnd => json!({"type": "reasoning.end"}),
        ProviderEvent::ReasoningDone { duration_secs } => {
            json!({"type": "reasoning.done", "durationSecs": duration_secs})
        }
        ProviderEvent::MessageEnd { stop_reason } => {
            json!({"type": "message.end", "stopReason": stop_reason})
        }
        ProviderEvent::RetryRollback { attempt, max } => {
            json!({"type": "retry.rollback", "attempt": attempt, "max": max})
        }
        // `promptAccounting` travels with the four numbers because a client has the same
        // ambiguity the TUI had: whether the cache figures are inside the prompt figure or
        // beside it decides both the session total and the context percentage, and it
        // cannot be told from the values.
        ProviderEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            accounting,
        } => json!({
            "type": "token.usage",
            "inputTokens": input_tokens,
            "outputTokens": output_tokens,
            "cacheReadInputTokens": cache_read_input_tokens,
            "cacheWriteInputTokens": cache_write_input_tokens,
            "promptAccounting": accounting.as_str(),
        }),
        ProviderEvent::ConnectionType { connection } => {
            json!({"type": "connection.type", "connection": connection})
        }
        ProviderEvent::ConnectionPhase { phase } => connection_phase(*phase),
        ProviderEvent::StatusDetail { detail } => {
            json!({"type": "status.detail", "detail": detail})
        }
        ProviderEvent::Error {
            message,
            retry_after,
        } => json!({
            "type": "error",
            "message": message,
            "retryAfterMs": retry_after.map(|value| value.as_millis()),
        }),
        ProviderEvent::SessionId(id) => json!({"type": "session.id", "id": id}),
        ProviderEvent::Compaction {
            trigger,
            pre_tokens,
            openai_encrypted_content,
        } => json!({
            "type": "compaction",
            "trigger": trigger,
            "preTokens": pre_tokens,
            "openaiEncryptedContent": openai_encrypted_content,
        }),
        ProviderEvent::UpstreamProvider { provider } => {
            json!({"type": "upstream.provider", "provider": provider})
        }
        ProviderEvent::NativeToolCall {
            request_id,
            tool_name,
            input,
        } => json!({
            "type": "native.tool.call",
            "requestID": request_id,
            "toolName": tool_name,
            "input": input,
        }),
    }
}

fn connection_phase(phase: ConnectionPhase) -> Value {
    match phase {
        ConnectionPhase::Authenticating => {
            json!({"type": "connection.phase", "phase": "authenticating"})
        }
        ConnectionPhase::Connecting => {
            json!({"type": "connection.phase", "phase": "connecting"})
        }
        ConnectionPhase::SendingRequest => {
            json!({"type": "connection.phase", "phase": "sending-request"})
        }
        ConnectionPhase::WaitingForResponse => {
            json!({"type": "connection.phase", "phase": "waiting-for-response"})
        }
        ConnectionPhase::Streaming => {
            json!({"type": "connection.phase", "phase": "streaming"})
        }
        ConnectionPhase::Retrying { attempt, max } => json!({
            "type": "connection.phase",
            "phase": "retrying",
            "attempt": attempt,
            "max": max,
        }),
    }
}

fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("fixed turn event payloads are objects")
        .clone()
}

impl fmt::Debug for EventService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventService")
            .field("sessions", &self.lock_fanouts().len())
            .field("heartbeat_interval", &self.heartbeat_interval)
            .finish_non_exhaustive()
    }
}

struct SessionSubscription {
    session_id: String,
    events: Vec<StreamEvent>,
    boundary: i64,
    live: LiveSessionSubscription,
    cursor: Option<EventCursor>,
}

/// One session's live subscription plus the fan-out bookkeeping it owes on exit.
///
/// A session's fan-out entry exists exactly while that session has at least one
/// live subscriber. Without the removal on drop, every session ever watched kept a
/// queue registry alive for the rest of the process.
struct LiveSessionSubscription {
    session_id: String,
    live: Option<EventSubscription<StreamEvent>>,
    fanouts: Arc<Mutex<HashMap<String, EventFanout<StreamEvent>>>>,
}

impl LiveSessionSubscription {
    async fn recv(&mut self) -> Option<Delivery<StreamEvent>> {
        self.live.as_mut()?.recv().await
    }
}

impl Drop for LiveSessionSubscription {
    fn drop(&mut self) {
        let mut fanouts = self.fanouts.lock().unwrap_or_else(PoisonError::into_inner);
        // Unregister this queue while the map is locked, so no subscriber can attach
        // between the count below and the removal.
        drop(self.live.take());
        if fanouts
            .get(&self.session_id)
            .is_some_and(|fanout| fanout.subscriber_count() == 0)
        {
            fanouts.remove(&self.session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::turn_event;
    use zuno_engine::interrupt::{HardInterruptReason, HardInterruptRequest, HardInterruptSource};
    use zuno_engine::r#loop::TurnEvent;
    use zuno_engine::session_command::SessionCommand;

    #[test]
    fn native_session_command_lifecycle_has_stable_durable_event_names() {
        let started = turn_event(&TurnEvent::SessionCommandStarted {
            command: SessionCommand::Compact,
        });
        assert_eq!(started.event_type, "session.command.started");
        assert_eq!(started.properties["command"], "compact");

        let output = turn_event(&TurnEvent::SessionCommandOutput {
            command: SessionCommand::Goal,
            content: "active goal".to_owned(),
        });
        assert_eq!(output.event_type, "session.command.output");
        assert_eq!(output.properties["command"], "goal");
        assert_eq!(output.properties["content"], "active goal");

        let completed = turn_event(&TurnEvent::SessionCommandCompleted {
            command: SessionCommand::Compact,
        });
        assert_eq!(completed.event_type, "session.command.completed");
        assert_eq!(completed.properties["command"], "compact");

        let failed = turn_event(&TurnEvent::SessionCommandFailed {
            command: SessionCommand::Compact,
            message: "provider unavailable".to_owned(),
        });
        assert_eq!(failed.event_type, "session.command.failed");
        assert_eq!(failed.properties["command"], "compact");
        assert_eq!(failed.properties["message"], "provider unavailable");
    }

    #[test]
    fn hard_interrupt_provenance_is_durable_and_client_visible() {
        let interrupted = turn_event(&TurnEvent::TurnInterrupted {
            assistant_message_id: Some("msg_assistant".to_owned()),
            steps: 3,
            request: Some(HardInterruptRequest::new(
                HardInterruptSource::Acp,
                HardInterruptReason::RequestCancelled,
            )),
        });

        assert_eq!(interrupted.event_type, "turn.interrupted");
        assert_eq!(interrupted.properties["source"], "acp");
        assert_eq!(interrupted.properties["reason"], "request_cancelled");
    }

    #[test]
    fn interrupted_tool_is_not_projected_as_an_ordinary_failure() {
        let interrupted = turn_event(&TurnEvent::ToolDispatchInterrupted {
            step: 2,
            call_id: "call_task".to_owned(),
            display_name: "Delegate".to_owned(),
            name: "task".to_owned(),
            title: "Inspect repository".to_owned(),
            output: "child did not stop before the deadline".to_owned(),
            interruption: zuno_engine::r#loop::ToolInterruption::Forced,
            uncertain: true,
        });

        assert_eq!(interrupted.event_type, "tool.dispatch.interrupted");
        assert_eq!(interrupted.properties["mode"], "forced");
        assert_eq!(interrupted.properties["forced"], true);
        assert_eq!(interrupted.properties["uncertain"], true);
    }

    #[test]
    fn a_cooperative_cancellation_publishes_its_resolved_uncertainty_without_forcing() {
        let interrupted = turn_event(&TurnEvent::ToolDispatchInterrupted {
            step: 2,
            call_id: "call_shell".to_owned(),
            display_name: "zsh".to_owned(),
            name: "shell".to_owned(),
            title: "shell cancelled".to_owned(),
            output: "partial output".to_owned(),
            interruption: zuno_engine::r#loop::ToolInterruption::Cooperative,
            uncertain: true,
        });

        assert_eq!(interrupted.event_type, "tool.dispatch.interrupted");
        assert_eq!(interrupted.properties["mode"], "cooperative");
        // The grace window never expired, so nothing was forced; the outcome is still
        // undecided, which is the only fact that asks for state inspection.
        assert_eq!(interrupted.properties["forced"], false);
        assert_eq!(interrupted.properties["uncertain"], true);
    }

    #[test]
    fn typed_tool_result_presentation_has_a_stable_native_event() {
        let presented = turn_event(&TurnEvent::ToolResultPresented {
            step: 2,
            call_id: "call_question".to_owned(),
            presentation: serde_json::from_value(serde_json::json!({
                "question": {
                    "status": "answered",
                    "answers": [["SQLite"]],
                    "questionCount": 1,
                    "elapsedMs": 12,
                }
            }))
            .expect("typed question presentation"),
        });

        assert_eq!(presented.event_type, "tool.result.presented");
        assert_eq!(presented.properties["callID"], "call_question");
        assert_eq!(
            presented.properties["presentation"]["question"]["status"],
            "answered"
        );
        assert_eq!(
            presented.properties["presentation"]["question"]["answers"][0][0],
            "SQLite"
        );
    }
}
