use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use oc_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
use oc_db::{Connection, migration, open};
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, ResolvedAgent, ResolvedModel,
    RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnError, TurnEvent,
    TurnOutcome, event_channel, hydrate_retained_history, project_history, project_history_owned,
    retained_history, run_turn,
};
use oc_error::ProviderError;
use oc_llm::cache::{DynamicContext, McpToolStatus};
use oc_llm::event::{FinishReason, RequestContentBlock, Role, StreamEvent};
use oc_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use oc_tool::{ToolDefinition, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const SESSION_ID: &str = "ses_loop_test";

#[derive(Debug)]
struct ScriptedResponse {
    events: Vec<Result<StreamEvent, ProviderError>>,
    hang_after: bool,
}

impl ScriptedResponse {
    fn complete(events: Vec<StreamEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            hang_after: false,
        }
    }

    fn failed(events: Vec<StreamEvent>, error: ProviderError) -> Self {
        let mut events = events.into_iter().map(Ok).collect::<Vec<_>>();
        events.push(Err(error));
        Self {
            events,
            hang_after: false,
        }
    }

    fn hanging(events: Vec<StreamEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            hang_after: true,
        }
    }
}

#[derive(Debug)]
struct FakeProvider {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl FakeProvider {
    fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("request lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("one scripted response per provider request");
        let events = stream::iter(response.events);
        if response.hang_after {
            Box::pin(events.chain(stream::pending()))
        } else {
            Box::pin(events)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FakeResolver;

impl AgentModelResolver for FakeResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build")
            .then(|| ResolvedAgent::new("build", "You are a deterministic test agent.", 8))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "fake" && model_id == "fake-model")
            .then(|| ResolvedModel::new(Spec::new("fake"), "fake-model", ApiSurface::Default))
    }
}

#[derive(Debug, Default)]
struct FakeDispatcher {
    calls: Mutex<Vec<DispatchRequest>>,
}

impl FakeDispatcher {
    fn calls(&self) -> Vec<DispatchRequest> {
        self.calls.lock().expect("dispatch lock").clone()
    }
}

#[async_trait]
impl ToolDispatcher for FakeDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(
            vec![ToolDefinition {
                id: "echo".to_owned(),
                description: "Echo text.".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            }],
            McpToolStatus::Ready,
        )
    }

    async fn dispatch(&self, request: DispatchRequest) -> ToolDispatchResult {
        self.calls
            .lock()
            .expect("dispatch lock")
            .push(request.clone());
        let text = request.call.input["text"]
            .as_str()
            .unwrap_or("missing text");
        ToolDispatchResult::success(ToolOutput::text("echo", text))
    }
}

fn seeded() -> Connection {
    let mut connection = open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-loop', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn put_user(connection: &Connection, id: &str, created: i64, text: &str) {
    let message = MessageRecord::from_json(json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid user message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid user part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist user message");
    store
        .put_part_at(&part, created)
        .expect("persist user part");
}

fn put_pending_tool(
    connection: &Connection,
    message_id: &str,
    part_id: &str,
    created: i64,
    call_id: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": "msg_before_repair",
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
        },
        "finish": "tool-calls"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "tool",
            "callID": call_id,
            "tool": "echo",
            "state": {
                "status": "pending",
                "input": { "text": "orphaned" },
                "raw": "{\"text\":\"orphaned\"}"
            }
        }),
        created,
    )
    .expect("valid pending tool part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist pending tool part");
}

fn put_assistant_text(
    connection: &Connection,
    id: &str,
    created: i64,
    parent_id: &str,
    text: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": parent_id,
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "stop"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid assistant text");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist assistant text");
}

fn put_completed_tool_output(
    connection: &Connection,
    message_id: &str,
    part_id: &str,
    created: i64,
    output: &str,
) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1 },
        "parentID": "msg_old_user",
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "tool-calls"
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        json!({
            "id": part_id,
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "tool",
            "callID": "call-large",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/large.txt" },
                "output": output
            }
        }),
        created,
    )
    .expect("valid completed tool part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist completed tool part");
}

fn put_successful_compaction(
    connection: &Connection,
    marker_id: &str,
    summary_id: &str,
    tail_start_id: &str,
    created: i64,
) {
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "compaction",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid compaction marker");
    let marker_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{marker_id}"),
            "sessionID": SESSION_ID,
            "messageID": marker_id,
            "type": "compaction",
            "auto": true,
            "overflow": true,
            "tail_start_id": tail_start_id
        }),
        created,
    )
    .expect("valid compaction part");
    let summary = MessageRecord::from_json(json!({
        "id": summary_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "parentID": marker_id,
        "time": { "created": created + 1, "completed": created + 2 },
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": "stop"
    }))
    .expect("valid compaction summary");
    let summary_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{summary_id}"),
            "sessionID": SESSION_ID,
            "messageID": summary_id,
            "type": "text",
            "text": "Earlier work was summarized without changing the retained tail."
        }),
        created + 1,
    )
    .expect("valid summary text");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&marker, created)
        .expect("persist compaction marker");
    store
        .put_part_at(&marker_part, created)
        .expect("persist compaction part");
    store
        .put_message_at(&summary, created + 2)
        .expect("persist compaction summary");
    store
        .put_part_at(&summary_part, created + 2)
        .expect("persist summary text");
}

fn put_incomplete_compaction(
    connection: &Connection,
    marker_id: &str,
    summary_id: &str,
    tail_start_id: &str,
    created: i64,
    summary_text: Option<&str>,
    failed: bool,
) {
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "compaction",
        "model": { "providerID": "fake", "modelID": "fake-model" }
    }))
    .expect("valid compaction marker");
    let marker_part = PartRecord::from_json(
        json!({
            "id": format!("prt_{marker_id}"),
            "sessionID": SESSION_ID,
            "messageID": marker_id,
            "type": "compaction",
            "auto": true,
            "overflow": true,
            "tail_start_id": tail_start_id
        }),
        created,
    )
    .expect("valid compaction part");
    let mut summary_data = json!({
        "id": summary_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "parentID": marker_id,
        "time": { "created": created + 1, "completed": created + 2 },
        "modelID": "fake-model",
        "providerID": "fake",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "cost": 0.0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        },
        "finish": if failed { "error" } else { "stop" }
    });
    if failed {
        summary_data["error"] = json!({
            "name": "CompactionError",
            "data": { "message": "summary failed", "isRetryable": false }
        });
    }
    let summary = MessageRecord::from_json(summary_data).expect("valid compaction summary");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&marker, created)
        .expect("persist compaction marker");
    store
        .put_part_at(&marker_part, created)
        .expect("persist compaction part");
    store
        .put_message_at(&summary, created + 2)
        .expect("persist compaction summary");
    if let Some(text) = summary_text {
        let part = PartRecord::from_json(
            json!({
                "id": format!("prt_{summary_id}"),
                "sessionID": SESSION_ID,
                "messageID": summary_id,
                "type": "text",
                "text": text
            }),
            created + 1,
        )
        .expect("valid summary text");
        store
            .put_part_at(&part, created + 2)
            .expect("persist summary text");
    }
}

fn assert_loader_projection_matches_full_reference(connection: &Connection) {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let full = MessageStore::new(connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    let expected = project_history(SYSTEM, retained_history(&full))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let optimized = hydrate_retained_history(connection, SESSION_ID).expect("optimized history");
    let actual = project_history_owned(SYSTEM, optimized);

    assert_eq!(
        serde_json::to_vec(&actual).expect("serialize optimized projection"),
        serde_json::to_vec(&expected).expect("serialize reference projection"),
        "optimized loader changed provider-visible history bytes"
    );
}

fn assert_dropping_first_history_message_changes_projection(connection: &Connection) {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let full = MessageStore::new(connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    assert!(
        full.len() > 1,
        "the sensitivity fixture needs a non-empty history prefix"
    );
    let complete = project_history(SYSTEM, &full)
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let without_first = project_history(SYSTEM, &full[1..])
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();

    assert_ne!(
        serde_json::to_vec(&without_first).expect("serialize truncated projection"),
        serde_json::to_vec(&complete).expect("serialize full projection"),
        "fixture is vacuous: dropping its first history message did not change provider-visible bytes"
    );
}

fn registry(provider: &Arc<FakeProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let provider = Arc::clone(provider);
    registry.register("fake", move |_spec| provider.clone());
    registry
}

fn request(turn_id: &str) -> RunTurnRequest {
    RunTurnRequest::new(SESSION_ID, turn_id, DynamicContext::default())
}

async fn collect_events(mut receiver: mpsc::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

fn full_turn_responses() -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("I will use echo.".to_owned()),
            StreamEvent::ToolUseStart {
                id: "call-1".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta(r#"{"text":"hello"}"#.to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("echo returned hello".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]
}

async fn run_full_turn_once() -> (Vec<TurnEvent>, Vec<CompletionRequest>, Vec<DispatchRequest>) {
    let mut connection = seeded();
    put_user(&connection, "msg_user", 10, "echo hello");
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-full"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("full turn succeeds"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-full_0002".to_owned(),
            steps: 2,
        }
    );

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate completed turn");
    let assistants: Vec<_> = hydrated
        .iter()
        .filter(|message| message.info.role.as_str() == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].parts.len(), 2);
    assert_eq!(assistants[1].parts.len(), 1);

    (events, provider.requests(), dispatcher.calls())
}

fn expected_full_turn_events() -> Vec<TurnEvent> {
    vec![
        TurnEvent::TurnStarted {
            session_id: SESSION_ID.to_owned(),
        },
        TurnEvent::AgentResolved {
            step: 1,
            agent: "build".to_owned(),
        },
        TurnEvent::ModelResolved {
            step: 1,
            provider_id: "fake".to_owned(),
            model_id: "fake-model".to_owned(),
        },
        TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: "msg_turn-full_0001".to_owned(),
        },
        TurnEvent::ToolSnapshotLocked {
            step: 1,
            tool_ids: vec!["echo".to_owned()],
            rebuilt_for_late_mcp: false,
        },
        TurnEvent::ProviderRequestStarted {
            step: 1,
            message_count: 2,
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::TextDelta("I will use echo.".to_owned()),
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseStart {
                id: "call-1".to_owned(),
                name: "echo".to_owned(),
            },
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolInputDelta(r#"{"text":"hello"}"#.to_owned()),
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::ToolUseEnd,
        },
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        },
        TurnEvent::AssistantCheckpointed {
            step: 1,
            message_id: "msg_turn-full_0001".to_owned(),
            interrupted: false,
        },
        TurnEvent::ToolDispatchStarted {
            step: 1,
            call_id: "call-1".to_owned(),
            name: "echo".to_owned(),
        },
        TurnEvent::ToolDispatchCompleted {
            step: 1,
            call_id: "call-1".to_owned(),
            name: "echo".to_owned(),
            title: "echo".to_owned(),
            output: "hello".to_owned(),
            is_error: false,
        },
        TurnEvent::ToolResultAppended {
            step: 1,
            call_id: "call-1".to_owned(),
            is_error: false,
        },
        TurnEvent::StepCompleted {
            step: 1,
            finish_reason: Some(FinishReason::ToolCalls),
        },
        TurnEvent::AgentResolved {
            step: 2,
            agent: "build".to_owned(),
        },
        TurnEvent::ModelResolved {
            step: 2,
            provider_id: "fake".to_owned(),
            model_id: "fake-model".to_owned(),
        },
        TurnEvent::AssistantMessageCreated {
            step: 2,
            message_id: "msg_turn-full_0002".to_owned(),
        },
        TurnEvent::ToolSnapshotLocked {
            step: 2,
            tool_ids: vec!["echo".to_owned()],
            rebuilt_for_late_mcp: false,
        },
        TurnEvent::ProviderRequestStarted {
            step: 2,
            message_count: 4,
        },
        TurnEvent::Provider {
            step: 2,
            event: StreamEvent::TextDelta("echo returned hello".to_owned()),
        },
        TurnEvent::Provider {
            step: 2,
            event: StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        },
        TurnEvent::AssistantCheckpointed {
            step: 2,
            message_id: "msg_turn-full_0002".to_owned(),
            interrupted: false,
        },
        TurnEvent::StepCompleted {
            step: 2,
            finish_reason: Some(FinishReason::Stop),
        },
        TurnEvent::TurnCompleted {
            assistant_message_id: "msg_turn-full_0002".to_owned(),
            steps: 2,
        },
    ]
}

#[tokio::test]
async fn loop_full_turn_emits_the_exact_sequence_deterministically() {
    let expected = expected_full_turn_events();
    let mut rendered_runs = Vec::new();

    for run_index in 0..3 {
        let (events, requests, calls) = run_full_turn_once().await;
        assert_eq!(events, expected, "event sequence changed");
        assert_eq!(requests.len(), 2);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.name, "echo");
        assert_eq!(calls[0].call.input, json!({ "text": "hello" }));

        let second = &requests[1];
        let blocks: Vec<&RequestContentBlock> = second
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            RequestContentBlock::ToolUse { id, .. } if id == "call-1"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            RequestContentBlock::ToolResult { tool_use_id, content, is_error }
                if tool_use_id == "call-1" && content == "hello" && *is_error == Some(false)
        )));

        if run_index == 0 {
            eprintln!(
                "HAPPY_QA event_count={} transcript={events:#?}",
                events.len()
            );
        }
        rendered_runs.push(format!("{events:#?}").into_bytes());
    }

    assert!(
        rendered_runs.windows(2).all(|pair| pair[0] == pair[1]),
        "the three event transcripts must be byte-identical"
    );
}

#[tokio::test]
async fn loop_rejects_a_completed_assistant_message_with_zero_parts() {
    let mut connection = seeded();
    put_user(
        &connection,
        "msg_empty_user",
        10,
        "do not accept an empty answer",
    );
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-empty-assistant"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    let error = outcome.expect_err("a zero-part assistant message must not complete the turn");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("fake"), "{diagnostic}");
    assert!(diagnostic.contains("empty"), "{diagnostic}");
    assert!(matches!(
        error,
        TurnError::EmptyAssistantMessage {
            provider_id,
            step: 1,
        } if provider_id == "fake"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
    );

    let assistant = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate rejected empty turn")
        .into_iter()
        .find(|message| message.info.id == "msg_turn-empty-assistant_0001")
        .expect("empty assistant checkpoint remains inspectable");
    assert!(assistant.parts.is_empty());
}

#[tokio::test]
async fn loop_accepts_a_tool_only_assistant_step_as_non_empty() {
    let mut connection = seeded();
    put_user(&connection, "msg_tool_only_user", 10, "use the echo tool");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::complete(vec![
            StreamEvent::ToolUseStart {
                id: "call-tool-only".to_owned(),
                name: "echo".to_owned(),
            },
            StreamEvent::ToolInputDelta(r#"{"text":"hello"}"#.to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("done".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-tool-only"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));
    let assistants = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate tool-only turn")
        .into_iter()
        .filter(|message| message.info.role.as_str() == "assistant")
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].parts.len(), 1, "tool part counts as output");
    assert_eq!(assistants[0].parts[0].kind, PartKind::Tool);
}

#[tokio::test]
async fn loop_provider_retry_replays_transient_503_and_rolls_back_partial_output() {
    let mut connection = seeded();
    put_user(&connection, "msg_retry_user", 10, "retry once");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            vec![StreamEvent::TextDelta("discarded attempt".to_owned())],
            ProviderError::Transient {
                status: Some(503),
                source: None,
            },
        ),
        ScriptedResponse::complete(vec![
            StreamEvent::TextDelta("completed replay".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-503"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    assert_eq!(provider.requests().len(), 2);
    assert!(events.iter().any(|event| matches!(
        event,
        TurnEvent::Provider {
            step: 1,
            event: StreamEvent::RetryRollback { attempt: 2, max: 3 },
        }
    )));

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate retried turn");
    let assistant = hydrated
        .iter()
        .find(|message| message.info.id == "msg_turn-retry-503_0001")
        .expect("retried assistant was persisted");
    let text = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Text)
        .expect("successful replay text was persisted");
    assert_eq!(text.data["text"], "completed replay");
}

#[tokio::test]
async fn loop_provider_retry_is_bounded_and_surfaces_the_final_transient_failure() {
    let mut connection = seeded();
    put_user(&connection, "msg_retry_limit_user", 10, "keep failing");
    let failures = (0..3)
        .map(|_| {
            ScriptedResponse::failed(
                Vec::new(),
                ProviderError::Transient {
                    status: Some(503),
                    source: None,
                },
            )
        })
        .collect();
    let provider = Arc::new(FakeProvider::new(failures));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-retry-limit"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Err(TurnError::Provider(ProviderError::Transient {
            status: Some(503),
            ..
        }))
    ));
    assert_eq!(provider.requests().len(), 3);
    let rollbacks = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnEvent::Provider {
                    event: StreamEvent::RetryRollback { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(rollbacks, 2);
}

#[tokio::test]
async fn loop_provider_retry_never_replays_a_permanent_failure() {
    let mut connection = seeded();
    put_user(&connection, "msg_permanent_user", 10, "do not retry");
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            Vec::new(),
            ProviderError::Fatal {
                status: Some(400),
                source: None,
            },
        ),
        ScriptedResponse::complete(vec![StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        }]),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-permanent"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(
        outcome,
        Err(TurnError::Provider(ProviderError::Fatal {
            status: Some(400),
            ..
        }))
    ));
    assert_eq!(provider.requests().len(), 1);
    assert!(!events.iter().any(|event| matches!(
        event,
        TurnEvent::Provider {
            event: StreamEvent::RetryRollback { .. },
            ..
        }
    )));
}

#[tokio::test]
async fn loop_repairs_a_missing_tool_result_before_the_provider_sees_history() {
    let mut connection = seeded();
    put_user(&connection, "msg_before_repair", 10, "start tool");
    put_pending_tool(
        &connection,
        "msg_orphaned_assistant",
        "prt_orphaned_tool",
        20,
        "call-orphaned",
    );
    put_user(&connection, "msg_after_repair", 30, "continue safely");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("continued".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-repair"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    assert_eq!(
        events[1],
        TurnEvent::HistoryRepaired {
            repaired_tool_results: 1,
        }
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let mut saw_use = false;
    let mut saw_result = false;
    for message in &request.messages {
        for block in &message.content {
            match block {
                RequestContentBlock::ToolUse { id, .. } if id == "call-orphaned" => {
                    saw_use = true;
                }
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == "call-orphaned" => {
                    saw_result = true;
                    assert_eq!(content, "[Tool execution was interrupted]");
                    assert_eq!(*is_error, Some(true));
                }
                RequestContentBlock::Text { .. }
                | RequestContentBlock::SignedThinking { .. }
                | RequestContentBlock::ProviderEncryptedReasoning { .. }
                | RequestContentBlock::ToolUse { .. }
                | RequestContentBlock::ToolResult { .. }
                | RequestContentBlock::Image { .. } => {}
            }
        }
    }
    assert!(
        saw_use && saw_result,
        "provider must see a repaired tool pair"
    );

    let repaired = MessageStore::new(&connection)
        .part("prt_orphaned_tool")
        .expect("read repaired tool part");
    assert_eq!(repaired.data["state"]["status"], "error");
    assert_eq!(
        repaired.data["state"]["error"],
        "[Tool execution was interrupted]"
    );
    eprintln!(
        "REPAIR_QA provider_request={request:#?} db_part={:#?}",
        repaired.to_json()
    );
}

/// Todo 113's two-phase loader is allowed to avoid decoding only the history that
/// [`retained_history`] would discard. The provider-visible bytes are the invariant:
/// changing their order loses the append-only prompt-cache hit, while omitting any
/// retained block changes the model's prompt. The pending tool before the boundary
/// separately proves repair still scans beyond the retained suffix.
#[tokio::test]
async fn loop_compacted_prefix_is_byte_identical_without_decoding_the_discarded_head() {
    const SYSTEM: &str = "You are a deterministic test agent.";
    let mut connection = seeded();
    put_user(&connection, "msg_old_user", 10, "old request");
    put_pending_tool(
        &connection,
        "msg_old_pending",
        "prt_old_pending",
        15,
        "call-old-pending",
    );
    let large_output = "x".repeat(2 * 1024 * 1024);
    put_completed_tool_output(
        &connection,
        "msg_old_large",
        "prt_old_large",
        20,
        &large_output,
    );
    put_user(&connection, "msg_tail_user", 30, "retained request");
    put_assistant_text(
        &connection,
        "msg_tail_assistant",
        40,
        "msg_tail_user",
        "retained answer",
    );
    put_successful_compaction(
        &connection,
        "msg_compaction_marker",
        "msg_compaction_summary",
        "msg_tail_user",
        50,
    );
    put_user(&connection, "msg_current_user", 60, "current request");

    // Reference path: the pre-113 implementation decoded every part and trimmed
    // only afterwards. Its serialized projection is the byte-level oracle.
    let full = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("full reference history");
    let expected_messages = project_history(SYSTEM, retained_history(&full))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let expected_prefix = serde_json::to_vec(&expected_messages).expect("serialize reference");
    let full_part_bytes = full
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| {
            serde_json::to_vec(&part.data)
                .expect("serialize part")
                .len()
        })
        .sum::<usize>();

    // Optimized path: all retained prompt messages must remain, in byte-identical
    // order, while the multi-megabyte pre-compaction tool blob is never resident.
    let optimized = hydrate_retained_history(&connection, SESSION_ID).expect("retained history");
    let optimized_messages = project_history(SYSTEM, retained_history(&optimized))
        .into_iter()
        .map(|projected| projected.message)
        .collect::<Vec<_>>();
    let owned_messages = project_history_owned(SYSTEM, optimized.clone());
    let optimized_prefix =
        serde_json::to_vec(&optimized_messages).expect("serialize optimized prefix");
    let optimized_part_bytes = optimized
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| {
            serde_json::to_vec(&part.data)
                .expect("serialize part")
                .len()
        })
        .sum::<usize>();

    assert_eq!(
        optimized_prefix, expected_prefix,
        "the request prefix changed byte-for-byte"
    );
    assert_eq!(
        optimized_messages, expected_messages,
        "a prompt-bearing message was dropped or reordered"
    );
    assert_eq!(
        serde_json::to_vec(&owned_messages).expect("serialize consuming projection"),
        expected_prefix,
        "the consuming projection changed the provider-visible bytes"
    );
    assert!(
        optimized_part_bytes * 100 < full_part_bytes,
        "the discarded head was still decoded: full={full_part_bytes}, optimized={optimized_part_bytes}"
    );
    assert!(
        optimized
            .iter()
            .all(|message| message.info.id != "msg_old_large"),
        "the pre-compaction large tool message remains resident"
    );

    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("turn-compacted-prefix"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 1, .. })
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::to_vec(&requests[0].messages).expect("serialize provider request"),
        expected_prefix,
        "run_turn did not send the reference prefix"
    );

    let repaired = MessageStore::new(&connection)
        .part("prt_old_pending")
        .expect("read pre-boundary pending tool");
    assert_eq!(
        repaired.data["state"]["status"], "error",
        "tool repair stopped at the compaction boundary"
    );
}

#[test]
fn loop_failed_compaction_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before failed summary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "tail that must not replace history",
    );
    put_incomplete_compaction(
        &connection,
        "msg_failed_marker",
        "msg_failed_summary",
        "msg_tail_user",
        30,
        None,
        true,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_empty_compaction_summary_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before empty summary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "tail that must not replace history",
    );
    put_incomplete_compaction(
        &connection,
        "msg_empty_marker",
        "msg_empty_summary",
        "msg_tail_user",
        30,
        Some(""),
        false,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_successful_compaction_with_missing_tail_falls_back_to_byte_identical_full_history() {
    let connection = seeded();
    put_user(
        &connection,
        "msg_old_user",
        10,
        "history before dangling boundary",
    );
    put_user(
        &connection,
        "msg_tail_user",
        20,
        "real tail remains part of history",
    );
    put_successful_compaction(
        &connection,
        "msg_dangling_marker",
        "msg_dangling_summary",
        "msg_missing_tail",
        30,
    );
    put_user(&connection, "msg_current_user", 40, "current request");

    assert_dropping_first_history_message_changes_projection(&connection);
    assert_loader_projection_matches_full_reference(&connection);
}

#[test]
fn loop_without_compaction_marker_is_byte_identical_to_full_history() {
    let connection = seeded();
    put_user(&connection, "msg_first_user", 10, "first request");
    put_assistant_text(
        &connection,
        "msg_first_assistant",
        20,
        "msg_first_user",
        "first answer",
    );
    put_user(&connection, "msg_current_user", 30, "current request");

    assert_loader_projection_matches_full_reference(&connection);
}

async fn collect_and_interrupt(
    mut receiver: mpsc::Receiver<TurnEvent>,
    interrupt: InterruptSignal,
) -> (Vec<TurnEvent>, Duration) {
    let mut events = Vec::new();
    let mut fired_at = None;
    while let Some(event) = receiver.recv().await {
        if fired_at.is_none()
            && matches!(
                &event,
                TurnEvent::Provider {
                    event: StreamEvent::TextDelta(text),
                    ..
                } if text == "partial checkpoint"
            )
        {
            fired_at = Some(Instant::now());
            interrupt.fire();
        }
        events.push(event);
    }
    let elapsed = fired_at
        .expect("the first text delta fires the interrupt")
        .elapsed();
    (events, elapsed)
}

#[tokio::test]
async fn loop_mid_stream_interrupt_finishes_within_100ms_and_checkpoints_db() {
    let mut connection = seeded();
    put_user(&connection, "msg_interrupt_user", 10, "stream forever");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::hanging(vec![
        StreamEvent::TextDelta("partial checkpoint".to_owned()),
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-interrupt"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let collector = collect_and_interrupt(receiver, interrupt.clone());
    let (outcome, (events, elapsed)) = tokio::join!(turn, collector);
    assert_eq!(
        outcome.expect("interrupt is a normal turn outcome"),
        TurnOutcome::Interrupted {
            assistant_message_id: Some("msg_turn-interrupt_0001".to_owned()),
            steps: 1,
        }
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "interrupt took {elapsed:?}"
    );
    assert!(matches!(
        events.last(),
        Some(TurnEvent::TurnInterrupted {
            assistant_message_id: Some(message_id),
            steps: 1,
        }) if message_id == "msg_turn-interrupt_0001"
    ));

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate interrupted turn");
    let assistant = hydrated
        .iter()
        .find(|message| message.info.id == "msg_turn-interrupt_0001")
        .expect("interrupted assistant was persisted");
    let text = assistant
        .parts
        .iter()
        .find(|part| part.kind == PartKind::Text)
        .expect("partial text was checkpointed");
    assert_eq!(text.data["text"], "partial checkpoint");
    assert_eq!(assistant.info.data["error"]["name"], "AbortError");
    assert!(assistant.info.data["time"]["completed"].is_number());
    eprintln!(
        "INTERRUPT_QA elapsed={elapsed:?} db_message={:#?} db_parts={:#?}",
        assistant.info.to_json(),
        assistant
            .parts
            .iter()
            .map(PartRecord::to_json)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn loop_head_interrupt_starts_no_provider_request() {
    let mut connection = seeded();
    put_user(&connection, "msg_head_user", 10, "do not start");
    let provider = Arc::new(FakeProvider::new(Vec::new()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    interrupt.fire();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-head"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("head interrupt is a normal outcome"),
        TurnOutcome::Interrupted {
            assistant_message_id: None,
            steps: 0,
        }
    );
    assert_eq!(
        events,
        vec![
            TurnEvent::TurnStarted {
                session_id: SESSION_ID.to_owned(),
            },
            TurnEvent::TurnInterrupted {
                assistant_message_id: None,
                steps: 0,
            },
        ]
    );
    assert!(provider.requests().is_empty());
}

/// The reply must sort after the prompt no matter what the clock says.
///
/// Todo 105 regressed exactly this. Moving the prompt's write to immediately before
/// the loop left it in the same millisecond as the first assistant record, ties are
/// broken by the random uuid in the id, and the losing half of those flips filed the
/// reply ahead of the prompt. Step 2 then hydrated a different message at index 1
/// than step 1 had sent and the append-only tracker refused the request.
///
/// A same-millisecond tie only reproduces that a fraction of the time, so this pins
/// the general invariant instead: a prompt stamped ahead of the current clock. An
/// unclamped reply carries `now_millis()`, sorts before that prompt every single
/// time, and this test fails deterministically. Clock skew and an imported session
/// reach the same state in production.
#[tokio::test]
async fn loop_reply_sorts_after_a_prompt_stamped_ahead_of_the_clock() {
    let ahead = oc_db::message::now_millis() + 60_000;
    let mut connection = seeded();
    put_user(&connection, "msg_user", ahead, "echo hello");
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        request("turn-skew"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    assert_eq!(
        outcome.expect("a prompt ahead of the clock must not break the request prefix"),
        TurnOutcome::Completed {
            assistant_message_id: "msg_turn-skew_0002".to_owned(),
            steps: 2,
        }
    );

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate the completed turn");
    let order: Vec<&str> = hydrated
        .iter()
        .map(|message| message.info.role.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["user", "assistant", "assistant"],
        "the persisted order put a reply ahead of the prompt it answers"
    );
    assert!(
        hydrated
            .windows(2)
            .all(|pair| pair[0].info.time_created < pair[1].info.time_created),
        "each record must carry a strictly later stamp than the one before it, so the \
         order does not depend on which random id sorts first"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.messages[1].role,
            Role::User,
            "the prompt must stay at index 1 of every request in the turn"
        );
    }
}

#[test]
fn loop_test_fixture_uses_no_live_network_provider() {
    let provider = FakeProvider::new(Vec::new());
    assert_eq!(provider.id(), "fake");
    assert!(provider.requests().is_empty());
    assert_eq!(Value::Null, Value::Null);
    assert_eq!(Role::User, Role::User);
}
