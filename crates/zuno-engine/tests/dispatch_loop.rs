use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    AgentModelResolver, ResolvedAgent, ResolvedModel, RunTurnRequest, TurnContext, TurnEvent,
    TurnOutcome, event_channel, run_turn,
};
use zuno_error::{ProviderError, ToolError};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_permission::{PermissionAction, Rule};
use zuno_tool::{AllowAll, Tool, ToolContext, ToolOutput};

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
        (requested == "build").then(|| ResolvedAgent::new("build", "dispatch test", 4))
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
        "bash"
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
        Ok(ToolOutput::text("bash", format!("ran {command}")))
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
    named_provider_events("bash", calls)
}

fn named_provider_events(tool: &str, calls: &[(&str, &str)]) -> Vec<Vec<StreamEvent>> {
    let mut first = Vec::new();
    for (id, command) in calls {
        first.push(StreamEvent::ToolUseStart {
            id: (*id).to_owned(),
            name: tool.to_owned(),
        });
        first.push(StreamEvent::ToolInputDelta(
            json!({ "command": command, "intent": "qa" }).to_string(),
        ));
        first.push(StreamEvent::ToolUseEnd);
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
) -> (Vec<String>, Vec<String>, Vec<String>) {
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
        InterruptSignal::new(),
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
    assert!(matches!(
        outcome,
        Ok(TurnOutcome::Completed { steps: 2, .. })
    ));

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
        .collect();
    let execution_order = order.lock().expect("order lock").clone();
    (lifecycle(&events), statuses, execution_order)
}

#[tokio::test]
async fn dispatch_loop_runs_three_calls_sequentially_with_complete_transitions() {
    let calls = [
        ("call-one", "first"),
        ("call-two", "second"),
        ("call-three", "third"),
    ];
    let (transcript, statuses, order) = run_scenario(
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
    assert_eq!(order, ["first", "second", "third"]);
    eprintln!("HAPPY_QA transcript={transcript:?} persisted={statuses:?} order={order:?}");
}

#[tokio::test]
async fn dispatch_loop_appends_denial_and_continues_to_the_next_call() {
    let calls = [("call-denied", "rm -rf /"), ("call-safe", "git status")];
    let (transcript, statuses, order) = run_scenario(
        "turn-dispatch-denied",
        &calls,
        vec![
            Rule {
                permission: "*".to_owned(),
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            },
            Rule {
                permission: "bash".to_owned(),
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
            "call-denied:error",
            "call-denied:result:error",
            "call-safe:running",
            "call-safe:completed",
            "call-safe:result:ok",
        ]
    );
    assert_eq!(statuses, ["error", "completed"]);
    assert_eq!(order, ["git status"]);
    eprintln!("DENIAL_QA transcript={transcript:?} persisted={statuses:?} order={order:?}");
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
        InterruptSignal::new(),
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
