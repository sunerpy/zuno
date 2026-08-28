use std::collections::BTreeSet;
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
    ResolvedModel, RunTurnRequest, ToolDispatcher, TurnContext, TurnEvent, TurnOutcome,
    event_channel, run_turn,
};
use zuno_engine::status::{
    AbortDisposition, SessionRunRegistry, SessionStatus, SoftInterruptAction,
};
use zuno_error::ProviderError;
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::StreamEvent;
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_tool::ToolUiIntent;

const SESSION_ID: &str = "ses_status_test";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConcurrentAttempt {
    Running,
    Busy,
}

#[test]
fn status_rejects_two_concurrent_prompts_for_one_session() {
    let registry = SessionRunRegistry::new();
    let start = Arc::new(Barrier::new(3));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (result_sender, result_receiver) = mpsc::channel();
    let mut workers = Vec::new();

    for _ in 0..2 {
        let registry = registry.clone();
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        let result_sender = result_sender.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            match registry.begin_turn(SESSION_ID) {
                Ok(turn) => {
                    result_sender
                        .send(ConcurrentAttempt::Running)
                        .expect("send running attempt");
                    let (released, wake) = &*release;
                    let released = released.lock().expect("release lock");
                    drop(
                        wake.wait_while(released, |released| !*released)
                            .expect("release wait"),
                    );
                    drop(turn);
                }
                Err(error) => {
                    assert_eq!(error.session_id(), SESSION_ID);
                    result_sender
                        .send(ConcurrentAttempt::Busy)
                        .expect("send busy attempt");
                }
            }
        }));
    }

    start.wait();
    let attempts = [
        result_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("first concurrent result"),
        result_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("second concurrent result"),
    ];

    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| **attempt == ConcurrentAttempt::Running)
            .count(),
        1
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| **attempt == ConcurrentAttempt::Busy)
            .count(),
        1
    );
    assert_eq!(registry.status(SESSION_ID), SessionStatus::Busy);
    assert_eq!(
        registry.active_sessions(),
        BTreeSet::from([SESSION_ID.to_owned()])
    );

    let (released, wake) = &*release;
    *released.lock().expect("release lock") = true;
    wake.notify_all();
    for worker in workers {
        worker.join().expect("concurrent worker");
    }

    assert_eq!(registry.status(SESSION_ID), SessionStatus::Idle);
    assert!(registry.active_sessions().is_empty());
}

#[test]
fn status_abort_during_an_idle_handoff_interrupts_the_next_accepted_turn() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);

    assert_eq!(
        control.abort(),
        AbortDisposition::ArmedNext,
        "a cancellation accepted between turn guards must be armed, not dropped",
    );
    let next = registry
        .begin_turn(SESSION_ID)
        .expect("the accepted follow-up starts its guard");
    assert!(
        next.interrupt_signal().is_set(),
        "the handoff cancellation did not reach the next turn"
    );
}

#[test]
fn status_abort_active_does_not_poison_the_next_idle_turn() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);

    assert!(!control.abort_active(), "idle teardown must be a no-op");
    let next = registry
        .begin_turn(SESSION_ID)
        .expect("the next turn acquires its guard");
    assert!(
        !next.interrupt_signal().is_set(),
        "idle teardown armed a cancellation for the next turn"
    );
}

#[test]
fn status_teardown_can_clear_an_abort_armed_during_prompt_handoff() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);

    assert_eq!(control.abort(), AbortDisposition::ArmedNext);
    assert!(
        control.clear_pending_abort(),
        "teardown must remove the handoff cancellation"
    );
    assert!(
        !control.clear_pending_abort(),
        "clearing an already-settled teardown must be idempotent"
    );
    let next = registry
        .begin_turn(SESSION_ID)
        .expect("a later independent mount acquires its guard");
    assert!(
        !next.interrupt_signal().is_set(),
        "teardown cancellation leaked into a later session mount"
    );
}

#[tokio::test]
async fn status_abort_through_stale_handle_interrupts_the_live_turn() {
    let registry = SessionRunRegistry::new();
    let stale_handle = registry.control(SESSION_ID);

    let previous_turn = registry.begin_turn(SESSION_ID).expect("previous turn");
    drop(previous_turn);
    let live_turn = registry.begin_turn(SESSION_ID).expect("live turn");

    let started = Arc::new(Barrier::new(2));
    let provider = Arc::new(HangingProvider {
        started: Arc::clone(&started),
    });
    let providers = provider_registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = NoopDispatcher;
    let mut connection = seeded();
    put_user(&connection);
    let (events, mut event_receiver) = event_channel();

    let aborter = std::thread::spawn(move || {
        started.wait();
        stale_handle.abort()
    });
    let context = TurnContext::new(
        &mut connection,
        &providers,
        &resolver,
        &dispatcher,
        live_turn.interrupt_signal(),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        run_turn(
            RunTurnRequest::new(SESSION_ID, "turn-status-abort", DynamicContext::default()),
            context,
            events,
        ),
    )
    .await
    .expect("abort must wake the hanging provider stream")
    .expect("turn result");

    assert_eq!(
        aborter.join().expect("abort worker"),
        AbortDisposition::Active
    );
    assert!(matches!(outcome, TurnOutcome::Interrupted { .. }));
    assert!(live_turn.interrupt_signal().is_set());
    assert!(
        std::iter::from_fn(|| event_receiver.try_recv().ok())
            .any(|event| matches!(event, TurnEvent::TurnInterrupted { .. }))
    );
    assert_eq!(registry.status(SESSION_ID), SessionStatus::Busy);

    drop(live_turn);
    assert_eq!(registry.status(SESSION_ID), SessionStatus::Idle);
}

#[test]
fn status_soft_interrupt_injects_at_safe_point_without_cancelling() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);
    let turn = registry.begin_turn(SESSION_ID).expect("active turn");
    let message = SoftInterruptMessage {
        input_id: None,
        content: "Please include the latest benchmark.".to_owned(),
        images: vec![("image/png".to_owned(), "aW1hZ2U=".to_owned())],
        urgent: false,
        source: SoftInterruptSource::User,
    };

    control
        .queue_soft_interrupt(message.clone())
        .expect("queue soft interrupt");
    assert!(
        turn.soft_interrupt_signal().is_set(),
        "queuing a steer must wake a provider wait"
    );
    assert!(
        !turn.interrupt_signal().is_set(),
        "a soft interrupt must not fire the abort signal"
    );

    let delivery = turn.take_soft_interrupts_at_safe_point();
    assert_eq!(delivery.messages, vec![message]);
    assert_eq!(delivery.action, SoftInterruptAction::Continue);
    assert!(
        !turn.soft_interrupt_signal().is_set(),
        "draining the steer must clear its wake signal"
    );
    assert!(
        turn.take_soft_interrupts_at_safe_point()
            .messages
            .is_empty()
    );
    assert!(
        !turn.interrupt_signal().is_set(),
        "injecting the message must let the turn continue"
    );
}

#[test]
fn status_cancel_soft_interrupt_removes_only_the_named_durable_input() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);
    let turn = registry.begin_turn(SESSION_ID).expect("active turn");
    for (id, content) in [("msg_drop", "drop"), ("msg_keep", "keep")] {
        control
            .queue_soft_interrupt(SoftInterruptMessage {
                input_id: Some(id.to_owned()),
                content: content.to_owned(),
                images: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
            })
            .expect("queue soft interrupt");
    }

    assert_eq!(control.cancel_soft_interrupt("msg_drop"), Ok(true));
    assert_eq!(control.cancel_soft_interrupt("msg_drop"), Ok(false));
    assert!(
        turn.soft_interrupt_signal().is_set(),
        "the remaining steer still owes a safe-boundary wake"
    );
    let delivery = turn.take_soft_interrupts_at_safe_point();
    assert_eq!(delivery.messages.len(), 1);
    assert_eq!(delivery.messages[0].input_id.as_deref(), Some("msg_keep"));
    assert_eq!(delivery.messages[0].content, "keep");
    assert!(!turn.soft_interrupt_signal().is_set());
}

#[test]
fn status_urgent_soft_interrupt_skips_remaining_tools_in_event_sequence() {
    let registry = SessionRunRegistry::new();
    let control = registry.control(SESSION_ID);
    let turn = registry.begin_turn(SESSION_ID).expect("active turn");
    let mut emitted = Vec::new();

    emit_tool_events(&mut emitted, "call-1", "first");
    control
        .queue_soft_interrupt(SoftInterruptMessage {
            input_id: None,
            content: "Stop the remaining tools and use this correction.".to_owned(),
            images: Vec::new(),
            urgent: true,
            source: SoftInterruptSource::System,
        })
        .expect("queue urgent soft interrupt");

    let delivery = turn.take_soft_interrupts_at_safe_point();
    assert_eq!(delivery.action, SoftInterruptAction::SkipRemainingTools);
    assert_eq!(delivery.messages.len(), 1);
    assert!(
        !turn.interrupt_signal().is_set(),
        "urgent soft interrupt still must not abort the provider stream"
    );
    if delivery.action == SoftInterruptAction::Continue {
        emit_tool_events(&mut emitted, "call-2", "second");
        emit_tool_events(&mut emitted, "call-3", "third");
    }
    emitted.push(TurnEvent::StepCompleted {
        step: 1,
        finish_reason: None,
    });

    assert_eq!(
        emitted,
        vec![
            TurnEvent::ToolDispatchStarted {
                step: 1,
                call_id: "call-1".to_owned(),
                display_name: "first".to_owned(),
                name: "first".to_owned(),
                ui_intent: ToolUiIntent::Generic,
            },
            TurnEvent::ToolDispatchCompleted {
                step: 1,
                call_id: "call-1".to_owned(),
                display_name: "first".to_owned(),
                name: "first".to_owned(),
                title: "first complete".to_owned(),
                output: "first output".to_owned(),
                diff: None,
                written_paths: Vec::new(),
                is_error: false,
            },
            TurnEvent::ToolResultAppended {
                step: 1,
                call_id: "call-1".to_owned(),
                is_error: false,
            },
            TurnEvent::StepCompleted {
                step: 1,
                finish_reason: None,
            },
        ]
    );
}

fn emit_tool_events(events: &mut Vec<TurnEvent>, call_id: &str, name: &str) {
    events.push(TurnEvent::ToolDispatchStarted {
        step: 1,
        call_id: call_id.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        ui_intent: ToolUiIntent::Generic,
    });
    events.push(TurnEvent::ToolDispatchCompleted {
        step: 1,
        call_id: call_id.to_owned(),
        display_name: name.to_owned(),
        name: name.to_owned(),
        title: format!("{name} complete"),
        output: format!("{name} output"),
        diff: None,
        written_paths: Vec::new(),
        is_error: false,
    });
    events.push(TurnEvent::ToolResultAppended {
        step: 1,
        call_id: call_id.to_owned(),
        is_error: false,
    });
}

#[derive(Debug)]
struct HangingProvider {
    started: Arc<Barrier>,
}

impl Provider for HangingProvider {
    fn id(&self) -> &str {
        "status-hanging"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::text_only()
    }

    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        self.started.wait();
        Box::pin(stream::pending::<Result<StreamEvent, ProviderError>>())
    }
}

#[derive(Debug, Clone, Copy)]
struct FakeResolver;

impl AgentModelResolver for FakeResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("build", "status test"))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "status-hanging" && model_id == "status-model").then(|| {
            ResolvedModel::new(
                Spec::new("status-hanging"),
                "status-model",
                ApiSurface::Default,
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct NoopDispatcher;

#[async_trait]
impl ToolDispatcher for NoopDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(Vec::new(), McpToolStatus::Ready)
    }

    async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
        panic!("the hanging provider never dispatches a tool")
    }
}

fn provider_registry(provider: &Arc<HangingProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let provider = Arc::clone(provider);
    registry.register("status-hanging", move |_spec| provider.clone());
    registry
}

fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-status', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-status', 'status', '/workspace', 'status', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn put_user(connection: &Connection) {
    let message = MessageRecord::from_json(serde_json::json!({
        "id": "msg_status_user",
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": 2 },
        "agent": "build",
        "model": { "providerID": "status-hanging", "modelID": "status-model" }
    }))
    .expect("valid user message");
    let part = PartRecord::from_json(
        serde_json::json!({
            "id": "prt_status_user",
            "sessionID": SESSION_ID,
            "messageID": "msg_status_user",
            "type": "text",
            "text": "wait until interrupted"
        }),
        2,
    )
    .expect("valid user part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, 2)
        .expect("persist user message");
    store.put_part_at(&part, 2).expect("persist user part");
}
