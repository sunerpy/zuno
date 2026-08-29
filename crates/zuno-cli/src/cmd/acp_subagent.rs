//! Negotiated ACP projection for Zuno's durable child sessions.
//!
//! Stable ACP clients still see the ordinary `task` tool call. When a client
//! explicitly advertises the draft `clientCapabilities.subagents` capability,
//! this component additionally routes a foreground child's own transcript to its
//! durable session id. The queue is bounded and terminal transitions are protected
//! from high-volume token/tool updates.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zuno_engine::r#loop::TurnEvent;

use super::child_turn::{ChildSessionOpened, ChildTurnObserver};

const EVENT_QUEUE_CAPACITY: usize = 256;

pub(crate) struct AcpSubagentBridge {
    shared: Arc<Shared>,
    worker: JoinHandle<Result<(), String>>,
}

impl AcpSubagentBridge {
    pub(crate) fn start(
        client: zuno_acp::ClientConnection,
        replay_root: std::path::PathBuf,
        default_context_size: u64,
    ) -> Result<(Arc<dyn ChildTurnObserver>, Self), String> {
        let pool =
            Arc::new(zuno_db::pool::Pool::open_default().map_err(|error| error.to_string())?);
        let shared = Arc::new(Shared::default());
        let observer: Arc<dyn ChildTurnObserver> = Arc::new(AcpSubagentObserver {
            shared: Arc::clone(&shared),
        });
        let worker_shared = Arc::clone(&shared);
        let worker_finished = Arc::clone(&shared);
        let worker = tokio::spawn(async move {
            let result = run_worker(
                worker_shared,
                client,
                pool,
                replay_root,
                default_context_size,
            )
            .await;
            shared_worker_finished(worker_finished.as_ref(), result.as_ref().err());
            result
        });
        Ok((observer, Self { shared, worker }))
    }

    pub(crate) fn flush_handle(&self) -> AcpSubagentFlush {
        AcpSubagentFlush {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) async fn shutdown(self) -> Result<(), String> {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.notify.notify_one();
        self.worker
            .await
            .map_err(|error| format!("ACP subagent projector task failed: {error}"))?
    }
}

#[derive(Clone)]
pub(crate) struct AcpSubagentFlush {
    shared: Arc<Shared>,
}

impl AcpSubagentFlush {
    pub(crate) async fn flush(&self) -> Result<(), String> {
        let target = self.shared.enqueued.load(Ordering::Acquire);
        loop {
            if self.shared.processed.load(Ordering::Acquire) >= target {
                return Ok(());
            }
            let notified = self.shared.drained.notified();
            if self.shared.processed.load(Ordering::Acquire) >= target {
                return Ok(());
            }
            if self.shared.worker_finished.load(Ordering::Acquire) {
                return Err(locked(&self.shared.failure).clone().unwrap_or_else(|| {
                    "ACP subagent projector stopped before queued updates drained".to_owned()
                }));
            }
            notified.await;
        }
    }
}

struct AcpSubagentObserver {
    shared: Arc<Shared>,
}

impl ChildTurnObserver for AcpSubagentObserver {
    fn opened(&self, opened: ChildSessionOpened) {
        self.shared.push(BridgeEvent::Opened(opened));
    }

    fn event(&self, session_id: &str, event: &TurnEvent) {
        self.shared.push(BridgeEvent::Event {
            session_id: session_id.to_owned(),
            event: event.clone(),
        });
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<QueueState>,
    notify: Notify,
    drained: Notify,
    closed: AtomicBool,
    enqueued: AtomicU64,
    processed: AtomicU64,
    worker_finished: AtomicBool,
    failure: Mutex<Option<String>>,
}

#[derive(Default)]
struct QueueState {
    queue: VecDeque<QueuedEvent>,
    omitted: HashMap<String, usize>,
    next_sequence: u64,
}

impl Shared {
    fn push(&self, event: BridgeEvent) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut state = locked(&self.state);
        if state.queue.len() >= EVENT_QUEUE_CAPACITY {
            if event.protected() {
                if let Some(index) = state
                    .queue
                    .iter()
                    .position(|queued| !queued.event.protected())
                {
                    if let Some(dropped) = state.queue.remove(index) {
                        *state
                            .omitted
                            .entry(dropped.event.session_id().to_owned())
                            .or_default() += 1;
                    }
                } else {
                    *state
                        .omitted
                        .entry(event.session_id().to_owned())
                        .or_default() += 1;
                    return;
                }
            } else {
                *state
                    .omitted
                    .entry(event.session_id().to_owned())
                    .or_default() += 1;
                return;
            }
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
        let sequence = state.next_sequence;
        state.queue.push_back(QueuedEvent { sequence, event });
        self.enqueued.store(sequence, Ordering::Release);
        drop(state);
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<(QueuedEvent, usize)> {
        let mut state = locked(&self.state);
        let event = state.queue.pop_front()?;
        let omitted = state
            .omitted
            .remove(event.event.session_id())
            .unwrap_or_default();
        Some((event, omitted))
    }

    fn is_empty(&self) -> bool {
        locked(&self.state).queue.is_empty()
    }
}

struct QueuedEvent {
    sequence: u64,
    event: BridgeEvent,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

enum BridgeEvent {
    Opened(ChildSessionOpened),
    Event {
        session_id: String,
        event: TurnEvent,
    },
}

impl BridgeEvent {
    fn session_id(&self) -> &str {
        match self {
            Self::Opened(opened) => &opened.session_id,
            Self::Event { session_id, .. } => session_id,
        }
    }

    fn protected(&self) -> bool {
        match self {
            Self::Opened(_) => true,
            Self::Event { event, .. } => terminal_state(event).is_some(),
        }
    }
}

struct ChildProjection {
    parent_session_id: String,
    projector: zuno_acp::AttemptBufferedTurnEventProjector,
}

async fn run_worker(
    shared: Arc<Shared>,
    client: zuno_acp::ClientConnection,
    pool: Arc<zuno_db::pool::Pool>,
    replay_root: std::path::PathBuf,
    default_context_size: u64,
) -> Result<(), String> {
    let mut children = HashMap::<String, ChildProjection>::new();
    let mut background = HashSet::<String>::new();
    loop {
        let notified = shared.notify.notified();
        if let Some((queued, omitted)) = shared.pop() {
            let sequence = queued.sequence;
            let event = queued.event;
            let session_id = event.session_id().to_owned();
            if omitted > 0 && children.contains_key(&session_id) {
                client
                    .session_update(
                        &session_id,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {
                                "type": "text",
                                "text": format!(
                                    "{omitted} high-frequency child updates were omitted because \
                                     the ACP client was not draining them fast enough."
                                ),
                            },
                            "_meta": {
                                "zuno": {
                                    "kind": "subagent_event_omission",
                                    "omittedUpdates": omitted,
                                },
                            },
                        }),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            match event {
                BridgeEvent::Opened(opened) => {
                    if opened.background {
                        background.insert(opened.session_id);
                    } else {
                        open_child(
                            &client,
                            pool.as_ref(),
                            &replay_root,
                            default_context_size,
                            opened,
                            &mut children,
                        )
                        .await?;
                    }
                }
                BridgeEvent::Event { session_id, event } => {
                    if background.contains(&session_id) {
                        if terminal_state(&event).is_some() {
                            background.remove(&session_id);
                        }
                    } else if let Some(child) = children.get_mut(&session_id) {
                        for update in child.projector.project(&event) {
                            client
                                .session_update(&session_id, update)
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                        if let Some(terminal) = terminal_state(&event) {
                            if let Some(message) = terminal.failure {
                                client
                                    .session_update(
                                        &session_id,
                                        json!({
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": {
                                                "type": "text",
                                                "text": message,
                                            },
                                            "_meta": {
                                                "zuno": {
                                                    "kind": "subagent_failure",
                                                },
                                            },
                                        }),
                                    )
                                    .await
                                    .map_err(|error| error.to_string())?;
                            }
                            let parent_session_id = child.parent_session_id.clone();
                            children.remove(&session_id);
                            let update = json!({
                                "sessionUpdate": "subagent_state_update",
                                "subagentSessionId": session_id,
                                "state": terminal.state,
                                "_meta": {
                                    "zuno": {
                                        "source": terminal.source,
                                        "reason": terminal.reason,
                                    },
                                },
                            });
                            client
                                .session_update(&parent_session_id, update)
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                    }
                }
            }
            shared.processed.store(sequence, Ordering::Release);
            shared.drained.notify_waiters();
            continue;
        }
        if shared.closed.load(Ordering::Acquire) && shared.is_empty() {
            break;
        }
        notified.await;
    }

    let mut disconnected = children.into_iter().collect::<Vec<_>>();
    disconnected.sort_by(|left, right| left.0.cmp(&right.0));
    for (session_id, child) in disconnected {
        client
            .session_update(
                &child.parent_session_id,
                json!({
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": session_id,
                    "state": "disconnected",
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn shared_worker_finished(shared: &Shared, failure: Option<&String>) {
    if let Some(failure) = failure {
        *locked(&shared.failure) = Some(failure.clone());
    }
    shared.worker_finished.store(true, Ordering::Release);
    shared.drained.notify_waiters();
}

async fn open_child(
    client: &zuno_acp::ClientConnection,
    pool: &zuno_db::pool::Pool,
    replay_root: &std::path::Path,
    default_context_size: u64,
    opened: ChildSessionOpened,
    children: &mut HashMap<String, ChildProjection>,
) -> Result<(), String> {
    client
        .session_update(
            &opened.parent_session_id,
            json!({
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": opened.session_id,
                "name": opened.agent,
                "task": opened.prompt,
                "capabilities": {
                    "cancel": false,
                    "close": false,
                },
            }),
        )
        .await
        .map_err(|error| error.to_string())?;

    let connection = pool.open_connection().map_err(|error| error.to_string())?;
    let history = zuno_engine::r#loop::hydrate_retained_history_tail(
        &connection,
        &opened.session_id,
        zuno_acp::REPLAY_MESSAGE_CAP,
        zuno_acp::REPLAY_TRANSCRIPT_BYTE_CAP,
    )
    .map_err(|error| error.to_string())?;
    let replay = zuno_acp::durable_updates(
        &history.messages,
        &zuno_acp::ReplayPolicy::for_workspace(replay_root),
        history.omitted,
    );
    for update in replay.updates {
        client
            .session_update(&opened.session_id, update)
            .await
            .map_err(|error| error.to_string())?;
    }
    let prompt_is_durable = history.messages.iter().any(|message| {
        message.info.role == zuno_db::message::MessageRole::User
            && message.parts.iter().any(|part| {
                part.kind == zuno_db::message::PartKind::Text
                    && part.data.get("text").and_then(serde_json::Value::as_str)
                        == Some(opened.prompt.as_str())
            })
    });
    if !prompt_is_durable {
        client
            .session_update(
                &opened.session_id,
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {
                        "type": "text",
                        "text": opened.prompt,
                    },
                    "_meta": {
                        "zuno": {
                            "kind": "delegated_prompt",
                        },
                    },
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
    }

    let context_size = opened
        .usage
        .and_then(|usage| usage.context_limit)
        .filter(|limit| *limit > 0)
        .unwrap_or(default_context_size);
    if let Some(update) = opened.usage.and_then(|usage| {
        zuno_acp::durable_usage_update(&history.messages, context_size, 0.0).or_else(|| {
            usage.last_prompt_tokens.map(|used| {
                json!({
                    "sessionUpdate": "usage_update",
                    "used": used,
                    "size": context_size,
                })
            })
        })
    }) {
        client
            .session_update(&opened.session_id, update)
            .await
            .map_err(|error| error.to_string())?;
    }
    children.insert(
        opened.session_id,
        ChildProjection {
            parent_session_id: opened.parent_session_id,
            projector: zuno_acp::AttemptBufferedTurnEventProjector::with_context_size(context_size),
        },
    );
    Ok(())
}

struct TerminalState {
    state: &'static str,
    failure: Option<String>,
    source: Option<zuno_engine::interrupt::HardInterruptSource>,
    reason: Option<zuno_engine::interrupt::HardInterruptReason>,
}

fn terminal_state(event: &TurnEvent) -> Option<TerminalState> {
    match event {
        TurnEvent::TurnCompleted { .. } | TurnEvent::SessionCommandCompleted { .. } => {
            Some(TerminalState {
                state: "completed",
                failure: None,
                source: None,
                reason: None,
            })
        }
        TurnEvent::TurnInterrupted { request, .. } => Some(TerminalState {
            state: "cancelled",
            failure: None,
            source: request.map(|request| request.source),
            reason: request.map(|request| request.reason),
        }),
        TurnEvent::TurnFailed { message, .. } | TurnEvent::SessionCommandFailed { message, .. } => {
            Some(TerminalState {
                state: "failed",
                failure: Some(message.clone()),
                source: None,
                reason: None,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_are_protected_when_the_bridge_queue_is_full() {
        let shared = Shared::default();
        for index in 0..EVENT_QUEUE_CAPACITY {
            shared.push(BridgeEvent::Event {
                session_id: "ses-child".to_owned(),
                event: TurnEvent::TurnStarted {
                    session_id: format!("ses-{index}"),
                },
            });
        }
        shared.push(BridgeEvent::Event {
            session_id: "ses-child".to_owned(),
            event: TurnEvent::TurnCompleted {
                assistant_message_id: "msg-final".to_owned(),
                steps: 1,
            },
        });

        let state = locked(&shared.state);
        assert_eq!(state.queue.len(), EVENT_QUEUE_CAPACITY);
        assert!(
            state.queue.iter().any(|queued| queued.event.protected()),
            "the terminal transition must displace a high-frequency update"
        );
        assert_eq!(state.omitted.get("ses-child"), Some(&1));
    }

    #[test]
    fn child_terminal_state_keeps_hard_interrupt_provenance() {
        let terminal = terminal_state(&TurnEvent::TurnInterrupted {
            assistant_message_id: Some("msg-child".to_owned()),
            steps: 2,
            request: Some(zuno_engine::interrupt::HardInterruptRequest::new(
                zuno_engine::interrupt::HardInterruptSource::Acp,
                zuno_engine::interrupt::HardInterruptReason::UserCancel,
            )),
        })
        .expect("interruption is terminal");

        assert_eq!(terminal.state, "cancelled");
        assert_eq!(
            terminal.source,
            Some(zuno_engine::interrupt::HardInterruptSource::Acp)
        );
        assert_eq!(
            terminal.reason,
            Some(zuno_engine::interrupt::HardInterruptReason::UserCancel)
        );
    }
}
