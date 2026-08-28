use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zuno_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::dispatch::ToolRegistryDispatcher;
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel, RunTurnRequest, ToolBlockKind,
    ToolConcurrencyLimit, ToolFailureRecovery, TurnContext, TurnEvent, TurnOutcome, event_channel,
    run_turn,
};
use zuno_error::{ProviderError, ToolError};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_permission::{PermissionAction, Rule};
use zuno_tool::{
    AllowAll, Tool, ToolConcurrencyPolicy, ToolContext, ToolContinuation, ToolOutput,
    ToolReplayPolicy,
};

const SESSION_ID: &str = "ses_dispatch_loop";

#[derive(Debug)]
struct ScriptedProvider {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "dispatch-fake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        let events = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("scripted provider response");
        Box::pin(stream::iter(events.into_iter().map(Ok::<_, ProviderError>)))
    }
}

struct Resolver;

impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("build", "dispatch test"))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "dispatch-fake" && model_id == "dispatch-model").then(|| {
            ResolvedModel::new(
                Spec::new("dispatch-fake"),
                "dispatch-model",
                ApiSurface::Default,
            )
        })
    }
}

struct SequentialTool {
    active: AtomicUsize,
    order: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for SequentialTool {
    fn id(&self) -> &str {
        "shell"
    }

    fn display_name(&self) -> &str {
        "zsh"
    }

    fn description(&self) -> &str {
        "Record dispatch order."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        assert_eq!(
            self.active.fetch_add(1, Ordering::SeqCst),
            0,
            "ordinary calls must never overlap"
        );
        let command = args["command"].as_str().expect("command string");
        self.order
            .lock()
            .expect("order lock")
            .push(command.to_owned());
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text("shell", format!("ran {command}")))
    }
}

#[derive(Default)]
struct ParallelState {
    active: AtomicUsize,
    max_active: AtomicUsize,
    completed: Mutex<Vec<String>>,
}

struct ParallelTool {
    state: Arc<ParallelState>,
}

#[async_trait]
impl Tool for ParallelTool {
    fn id(&self) -> &str {
        "parallel"
    }

    fn description(&self) -> &str {
        "A read-only operation that may overlap with peers."
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.max_active.fetch_max(active, Ordering::SeqCst);
        let command = args["command"].as_str().expect("command string");
        let delay = match command {
            "first" => 30,
            "second" => 20,
            _ => 10,
        };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        self.state
            .completed
            .lock()
            .expect("completed lock")
            .push(command.to_owned());
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text("parallel", format!("completed {command}")))
    }
}

struct YieldingTaskTool;

#[async_trait]
impl Tool for YieldingTaskTool {
    fn id(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Start background work whose durable report will resume the parent."
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(
            "background task",
            "The durable nextStep report will resume this session.",
        )
        .with_continuation(ToolContinuation::YieldUntilInput))
    }
}

#[derive(Default)]
struct PolicyState {
    active: AtomicUsize,
    max_active: AtomicUsize,
    exclusive: AtomicBool,
    events: Mutex<Vec<String>>,
}

struct PolicyTool {
    id: &'static str,
    policy: ToolConcurrencyPolicy,
    state: Arc<PolicyState>,
}

#[async_trait]
impl Tool for PolicyTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "Record mixed tool concurrency barriers."
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        self.policy
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.max_active.fetch_max(active, Ordering::SeqCst);
        if self.policy == ToolConcurrencyPolicy::Exclusive {
            assert_eq!(active, 1, "an exclusive call overlapped an earlier call");
            assert!(
                !self.state.exclusive.swap(true, Ordering::SeqCst),
                "two exclusive calls overlapped"
            );
        } else {
            assert!(
                !self.state.exclusive.load(Ordering::SeqCst),
                "a non-exclusive call started while the barrier was active"
            );
        }
        let command = args["command"].as_str().expect("command string");
        self.state
            .events
            .lock()
            .expect("policy events lock")
            .push(format!("start:{command}"));
        let delay = if command.contains("parallel") { 30 } else { 20 };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        self.state
            .events
            .lock()
            .expect("policy events lock")
            .push(format!("end:{command}"));
        if self.policy == ToolConcurrencyPolicy::Exclusive {
            self.state.exclusive.store(false, Ordering::SeqCst);
        }
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text(self.id, format!("completed {command}")))
    }
}

struct TimeoutTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for TimeoutTool {
    fn id(&self) -> &str {
        "fragile"
    }

    fn description(&self) -> &str {
        "A side-effecting operation whose response may time out."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ToolError::Timeout {
            tool: self.id().to_owned(),
            elapsed: Duration::from_secs(30),
        })
    }
}

struct RetryableThenTerminalTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for RetryableThenTerminalTool {
    fn id(&self) -> &str {
        "fragile"
    }

    fn description(&self) -> &str {
        "A tool whose latest failure determines whether goal recovery remains pending."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(ToolError::Timeout {
                tool: self.id().to_owned(),
                elapsed: Duration::from_secs(30),
            }),
            1 => Err(ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::other("terminal rejection")),
            }),
            call => panic!("unexpected fragile invocation {call}"),
        }
    }
}

fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-dispatch', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-dispatch', 'dispatch', '/workspace', 'dispatch', '1', 1, 1);"
        ))
        .expect("seed project and session");
    let message = MessageRecord::from_json(json!({
        "id": "msg_user_dispatch",
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": 10 },
        "agent": "build",
        "model": { "providerID": "dispatch-fake", "modelID": "dispatch-model" }
    }))
    .expect("valid user message");
    let part = PartRecord::from_json(
        json!({
            "id": "prt_user_dispatch",
            "sessionID": SESSION_ID,
            "messageID": "msg_user_dispatch",
            "type": "text",
            "text": "run tools"
        }),
        10,
    )
    .expect("valid user part");
    let store = MessageStore::new(&connection);
    store.put_message_at(&message, 10).expect("persist user");
    store.put_part_at(&part, 10).expect("persist user part");
    connection
}

fn provider_events(calls: &[(&str, &str)]) -> Vec<Vec<StreamEvent>> {
    named_provider_events("shell", calls)
}

fn named_provider_events(tool: &str, calls: &[(&str, &str)]) -> Vec<Vec<StreamEvent>> {
    let mut first = Vec::new();
    for (id, command) in calls {
        first.push(StreamEvent::ToolUseStart {
            id: (*id).to_owned(),
            name: tool.to_owned(),
        });
        first.push(StreamEvent::ToolInputDelta {
            id: (*id).to_owned(),
            delta: json!({ "command": command, "intent": "qa" }).to_string(),
        });
        first.push(StreamEvent::ToolUseEnd {
            id: (*id).to_owned(),
        });
    }
    first.push(StreamEvent::MessageEnd {
        stop_reason: Some(FinishReason::ToolCalls),
    });
    vec![
        first,
        vec![
            StreamEvent::TextDelta("tools completed".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ],
    ]
}

fn mixed_provider_events(calls: &[(&str, &str, &str)]) -> Vec<Vec<StreamEvent>> {
    let mut first = Vec::new();
    for (id, tool, command) in calls {
        first.push(StreamEvent::ToolUseStart {
            id: (*id).to_owned(),
            name: (*tool).to_owned(),
        });
        first.push(StreamEvent::ToolInputDelta {
            id: (*id).to_owned(),
            delta: json!({ "command": command, "intent": "qa" }).to_string(),
        });
        first.push(StreamEvent::ToolUseEnd {
            id: (*id).to_owned(),
        });
    }
    first.push(StreamEvent::MessageEnd {
        stop_reason: Some(FinishReason::ToolCalls),
    });
    vec![
        first,
        vec![
            StreamEvent::TextDelta("tools completed".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ],
    ]
}

fn registry(provider: Arc<ScriptedProvider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register("dispatch-fake", move |_spec| provider.clone());
    registry
}

async fn collect_events(mut receiver: mpsc::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

fn lifecycle(events: &[TurnEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolDispatchStarted { call_id, .. } => Some(format!("{call_id}:running")),
            TurnEvent::ToolDispatchBlocked { call_id, kind, .. } => Some(format!(
                "{call_id}:blocked:{}",
                match kind {
                    ToolBlockKind::Denied => "denied",
                    ToolBlockKind::InvalidArguments => "invalid-arguments",
                    ToolBlockKind::Unavailable => "unavailable",
                }
            )),
            TurnEvent::ToolDispatchCompleted {
                call_id, is_error, ..
            } => Some(format!(
                "{call_id}:{}",
                if *is_error { "error" } else { "completed" }
            )),
            TurnEvent::ToolResultAppended {
                call_id, is_error, ..
            } => Some(format!(
                "{call_id}:result:{}",
                if *is_error { "error" } else { "ok" }
            )),
            _ => None,
        })
        .collect()
}

async fn run_scenario(
    turn_id: &str,
    calls: &[(&str, &str)],
    rules: Vec<Rule>,
) -> (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<(String, String, String)>,
) {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(provider_events(calls)));
    let providers = registry(provider);
    let order = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(SequentialTool {
            active: AtomicUsize::new(0),
            order: Arc::clone(&order),
        })],
        rules,
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, turn_id, DynamicContext::default()),
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
    let TurnOutcome::Completed {
        steps,
        unresolved_tool_failures,
        ..
    } = outcome.expect("turn completes")
    else {
        panic!("turn was interrupted");
    };
    assert_eq!(steps, 2);
    assert!(unresolved_tool_failures.is_empty());

    let statuses = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate turn")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter(|part| part.kind == PartKind::Tool)
        .map(|part| {
            part.data["state"]["status"]
                .as_str()
                .expect("tool status")
                .to_owned()
        })
        .collect::<Vec<_>>();
    let outcomes = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate turn outcomes")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter(|part| part.kind == PartKind::Tool)
        .map(|part| {
            part.data["state"]["outcome"]
                .as_str()
                .or_else(|| part.data["state"]["status"].as_str())
                .expect("tool outcome or status")
                .to_owned()
        })
        .collect();
    let execution_order = order.lock().expect("order lock").clone();
    let pending = events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                name,
                ..
            } => Some((call_id.clone(), name.clone(), display_name.clone())),
            _ => None,
        })
        .collect();
    (
        lifecycle(&events),
        statuses,
        outcomes,
        execution_order,
        pending,
    )
}

#[tokio::test]
async fn a_next_step_tool_yields_without_spending_a_second_provider_request() {
    let mut connection = seeded();
    let mut responses = named_provider_events("task", &[("call-task", "inspect")]);
    responses.truncate(1);
    let provider = Arc::new(ScriptedProvider::new(responses));
    let providers = registry(provider);
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(YieldingTaskTool)],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(
            SESSION_ID,
            "turn-next-step-yield",
            DynamicContext::default(),
        ),
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

    let TurnOutcome::Completed { steps, .. } = outcome.expect("turn yields successfully") else {
        panic!("turn was interrupted");
    };
    assert_eq!(steps, 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TurnEvent::ProviderRequestStarted { .. }))
            .count(),
        1,
        "the host must wait for the durable report instead of asking the model to say it is waiting"
    );
}

#[tokio::test]
async fn dispatch_loop_runs_three_calls_sequentially_with_complete_transitions() {
    let calls = [
        ("call-one", "first"),
        ("call-two", "second"),
        ("call-three", "third"),
    ];
    let (transcript, statuses, outcomes, order, pending) = run_scenario(
        "turn-dispatch-happy",
        &calls,
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
    )
    .await;

    assert_eq!(
        transcript,
        [
            "call-one:running",
            "call-one:completed",
            "call-one:result:ok",
            "call-two:running",
            "call-two:completed",
            "call-two:result:ok",
            "call-three:running",
            "call-three:completed",
            "call-three:result:ok",
        ]
    );
    assert_eq!(statuses, ["completed", "completed", "completed"]);
    assert_eq!(outcomes, ["completed", "completed", "completed"]);
    assert_eq!(order, ["first", "second", "third"]);
    assert_eq!(
        pending,
        [
            ("call-one".to_owned(), "shell".to_owned(), "zsh".to_owned()),
            ("call-two".to_owned(), "shell".to_owned(), "zsh".to_owned()),
            (
                "call-three".to_owned(),
                "shell".to_owned(),
                "zsh".to_owned()
            ),
        ],
        "the pending event must resolve the display identity before dispatch"
    );
    eprintln!("HAPPY_QA transcript={transcript:?} persisted={statuses:?} order={order:?}");
}

#[tokio::test]
async fn parallel_safe_calls_overlap_but_persist_and_emit_in_model_order() {
    let mut connection = seeded();
    let calls = [
        ("call-one", "first"),
        ("call-two", "second"),
        ("call-three", "third"),
    ];
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "parallel", &calls,
    )));
    let providers = registry(provider);
    let state = Arc::new(ParallelState::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(ParallelTool {
            state: Arc::clone(&state),
        })],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-parallel-safe", DynamicContext::default()),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_tool_concurrency(ToolConcurrencyLimit::new(3).expect("valid limit")),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(outcome, Ok(TurnOutcome::Completed { .. })));
    assert_eq!(state.max_active.load(Ordering::SeqCst), 3);
    assert_eq!(
        *state.completed.lock().expect("completed lock"),
        ["third", "second", "first"],
        "shorter calls should physically settle first"
    );
    assert_eq!(
        lifecycle(&events),
        [
            "call-one:running",
            "call-two:running",
            "call-three:running",
            "call-one:completed",
            "call-one:result:ok",
            "call-two:completed",
            "call-two:result:ok",
            "call-three:completed",
            "call-three:result:ok",
        ],
        "durable and client-visible order must remain the provider's call order"
    );
    let outputs = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate turn")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter(|part| part.kind == PartKind::Tool)
        .map(|part| {
            part.data["state"]["output"]
                .as_str()
                .expect("tool output")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        ["completed first", "completed second", "completed third"]
    );
}

#[tokio::test]
async fn parallel_safe_calls_never_exceed_the_configured_execution_bound() {
    let mut connection = seeded();
    let calls = [
        ("call-one", "first"),
        ("call-two", "second"),
        ("call-three", "third"),
        ("call-four", "fourth"),
        ("call-five", "fifth"),
    ];
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "parallel", &calls,
    )));
    let providers = registry(provider);
    let state = Arc::new(ParallelState::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(ParallelTool {
            state: Arc::clone(&state),
        })],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-parallel-bound", DynamicContext::default()),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_tool_concurrency(ToolConcurrencyLimit::new(2).expect("valid limit")),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(outcome, Ok(TurnOutcome::Completed { .. })));
    assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
    assert_eq!(state.completed.lock().expect("completed lock").len(), 5);
    assert_eq!(
        lifecycle(&events)
            .iter()
            .filter(|event| event.ends_with(":result:ok"))
            .count(),
        5
    );
}

#[tokio::test]
async fn exclusive_calls_barrier_parallel_safe_and_isolated_background_groups() {
    let mut connection = seeded();
    let calls = [
        ("call-one", "parallel", "before-parallel"),
        ("call-two", "background", "before-background"),
        ("call-three", "exclusive", "barrier"),
        ("call-four", "background", "after-background"),
        ("call-five", "parallel", "after-parallel"),
    ];
    let provider = Arc::new(ScriptedProvider::new(mixed_provider_events(&calls)));
    let providers = registry(provider);
    let state = Arc::new(PolicyState::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![
            Arc::new(PolicyTool {
                id: "parallel",
                policy: ToolConcurrencyPolicy::ParallelSafe,
                state: Arc::clone(&state),
            }),
            Arc::new(PolicyTool {
                id: "background",
                policy: ToolConcurrencyPolicy::IsolatedBackground,
                state: Arc::clone(&state),
            }),
            Arc::new(PolicyTool {
                id: "exclusive",
                policy: ToolConcurrencyPolicy::Exclusive,
                state: Arc::clone(&state),
            }),
        ],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-mixed-barrier", DynamicContext::default()),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_tool_concurrency(ToolConcurrencyLimit::new(4).expect("valid limit")),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));

    assert!(matches!(outcome, Ok(TurnOutcome::Completed { .. })));
    assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
    let physical = state.events.lock().expect("policy events lock");
    let position = |expected: &str| {
        physical
            .iter()
            .position(|event| event == expected)
            .unwrap_or_else(|| panic!("missing event `{expected}` in {physical:?}"))
    };
    assert!(position("end:before-parallel") < position("start:barrier"));
    assert!(position("end:before-background") < position("start:barrier"));
    assert!(position("end:barrier") < position("start:after-background"));
    assert!(position("end:barrier") < position("start:after-parallel"));
    assert!(position("start:before-parallel") < position("end:before-background"));
    assert!(position("start:before-background") < position("end:before-parallel"));
    assert!(position("start:after-parallel") < position("end:after-background"));
    assert!(position("start:after-background") < position("end:after-parallel"));
    drop(physical);
    assert_eq!(
        lifecycle(&events),
        [
            "call-one:running",
            "call-two:running",
            "call-one:completed",
            "call-one:result:ok",
            "call-two:completed",
            "call-two:result:ok",
            "call-three:running",
            "call-three:completed",
            "call-three:result:ok",
            "call-four:running",
            "call-five:running",
            "call-four:completed",
            "call-four:result:ok",
            "call-five:completed",
            "call-five:result:ok",
        ]
    );
}

#[tokio::test]
async fn dispatch_loop_appends_denial_and_continues_to_the_next_call() {
    let calls = [("call-denied", "rm -rf /"), ("call-safe", "git status")];
    let (transcript, statuses, outcomes, order, _pending) = run_scenario(
        "turn-dispatch-denied",
        &calls,
        vec![
            Rule {
                permission: "*".to_owned(),
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            },
            Rule {
                permission: "shell".to_owned(),
                pattern: "rm -rf /".to_owned(),
                action: PermissionAction::Deny,
            },
        ],
    )
    .await;

    assert_eq!(
        transcript,
        [
            "call-denied:running",
            "call-denied:blocked:denied",
            "call-denied:error",
            "call-denied:result:error",
            "call-safe:running",
            "call-safe:completed",
            "call-safe:result:ok",
        ]
    );
    assert_eq!(statuses, ["error", "completed"]);
    assert_eq!(outcomes, ["blocked", "completed"]);
    assert_eq!(order, ["git status"]);
    eprintln!("DENIAL_QA transcript={transcript:?} persisted={statuses:?} order={order:?}");
}

#[tokio::test]
async fn dispatch_loop_rejects_namespaced_or_historical_tool_aliases() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "functions.bash",
        &[("call-alias", "git status")],
    )));
    let providers = registry(provider);
    let order = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(SequentialTool {
            active: AtomicUsize::new(0),
            order: Arc::clone(&order),
        })],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-exact-tool-id", DynamicContext::default()),
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

    let TurnOutcome::Completed {
        unresolved_tool_failures,
        ..
    } = outcome.expect("turn completes")
    else {
        panic!("turn was interrupted");
    };
    assert!(unresolved_tool_failures.is_empty());
    assert!(order.lock().expect("order lock").is_empty());
    assert_eq!(
        lifecycle(&events),
        [
            "call-alias:running",
            "call-alias:blocked:unavailable",
            "call-alias:error",
            "call-alias:result:error",
        ]
    );
    let tool_part = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate turn")
        .into_iter()
        .flat_map(|message| message.parts)
        .find(|part| part.kind == PartKind::Tool)
        .expect("unknown tool result is persisted");
    assert!(
        tool_part.data["state"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("Unknown tool: functions.bash")),
        "{tool_part:?}"
    );
    assert_eq!(tool_part.data["state"]["outcome"], "blocked");
    assert_eq!(tool_part.data["state"]["blockKind"], "unavailable");
}

#[tokio::test]
async fn dispatch_loop_reports_a_tool_timeout_without_replaying_the_call() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "fragile",
        &[("call-timeout", "publish once")],
    )));
    let providers = registry(provider);
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(TimeoutTool {
            calls: Arc::clone(&calls),
        })],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-tool-timeout", DynamicContext::default()),
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

    let TurnOutcome::Completed {
        steps,
        unresolved_tool_failures,
        ..
    } = outcome.expect("turn completes")
    else {
        panic!("turn was interrupted");
    };
    assert_eq!(steps, 2);
    assert_eq!(
        unresolved_tool_failures,
        vec![ToolFailureRecovery {
            tool: "fragile".to_owned(),
            replay_policy: ToolReplayPolicy::Never,
            retry_after: None,
        }]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the model may inspect the timeout and decide what to do; the harness must not replay \
         a possibly side-effecting call"
    );
    assert_eq!(
        lifecycle(&events),
        [
            "call-timeout:running",
            "call-timeout:error",
            "call-timeout:result:error",
        ]
    );

    let tool_part = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate turn")
        .into_iter()
        .flat_map(|message| message.parts)
        .find(|part| part.kind == PartKind::Tool)
        .expect("timeout tool result is persisted");
    assert_eq!(tool_part.data["state"]["status"], "error");
    assert!(
        tool_part.data["state"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("timed out")),
        "{tool_part:?}"
    );
}

#[tokio::test]
async fn dispatch_loop_keeps_only_the_latest_failure_recovery_for_each_tool() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "fragile",
        &[
            ("call-timeout", "first attempt"),
            ("call-terminal", "corrected attempt"),
        ],
    )));
    let providers = registry(provider);
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RetryableThenTerminalTool {
            calls: Arc::clone(&calls),
        })],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(
            SESSION_ID,
            "turn-tool-latest-recovery",
            DynamicContext::default(),
        ),
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

    let TurnOutcome::Completed {
        unresolved_tool_failures,
        ..
    } = outcome.expect("turn completes")
    else {
        panic!("turn was interrupted");
    };
    assert!(
        unresolved_tool_failures.is_empty(),
        "the terminal second result resolves the earlier retryable failure"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        lifecycle(&events),
        [
            "call-timeout:running",
            "call-timeout:error",
            "call-timeout:result:error",
            "call-terminal:running",
            "call-terminal:error",
            "call-terminal:result:error",
        ]
    );
}

/// A tool that writes files and reports them the way the real file tools do.
///
/// Named `apply_patch` and reporting two paths on purpose: those are the two properties
/// that made the defect invisible. A host cannot recognise this tool from the shipped
/// `["edit", "write", "patch"]` name list, and it writes more files than any single
/// `filepath` field or prose `title` could carry.
struct PatchingTool;

#[async_trait]
impl Tool for PatchingTool {
    fn id(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a patch to several files."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(
            "Success. Updated the following files:",
            "M first.rs\nM second.rs",
        )
        .with_written_path(std::path::Path::new("first.rs"))
        .with_written_path(std::path::Path::new("second.rs")))
    }
}

#[tokio::test]
async fn dispatch_loop_carries_every_written_path_from_the_tool_onto_the_event() {
    // The link a green suite did not cover. `ToolDispatchCompleted` forwarded only name,
    // title, output and diff, dropping the result's metadata — so even a tool that stated
    // which files it wrote had that answer discarded here, and the host downstream was
    // left inferring paths from a prose `title` and a name list. Both inferences fail on
    // exactly this shape: one call, two files, a title that is a sentence.
    //
    // Driven through `run_turn` with a real `ToolRegistryDispatcher` rather than by
    // constructing the event, because constructing it is what hides a dropped field.
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "apply_patch",
        &[("call-patch", "patch")],
    )));
    let providers = registry(provider);
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(PatchingTool)],
        vec![Rule {
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );
    let resolver = Resolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-written-paths", DynamicContext::default()),
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
    assert!(matches!(outcome, Ok(TurnOutcome::Completed { .. })));

    let completed = events
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolDispatchCompleted {
                name,
                written_paths,
                is_error,
                ..
            } => Some((name.clone(), written_paths.clone(), *is_error)),
            _ => None,
        })
        .expect("the turn dispatched the patching tool");

    assert_eq!(completed.0, "apply_patch");
    assert!(!completed.2, "the call succeeded");
    assert_eq!(
        completed.1,
        vec![String::from("first.rs"), String::from("second.rs")],
        "the event must carry every path the tool reported writing; a host checking \
         diagnostics has no other source for them"
    );
}
