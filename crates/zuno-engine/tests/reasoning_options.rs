//! The session's chosen reasoning level has to reach the provider's request body.
//!
//! Its own file rather than an addition to `tests/loop.rs` so that the one thing it
//! asserts is not entangled with that file's fixtures.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zuno_db::message::{MessageRecord, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
    ResolvedModel, RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnEvent,
    event_channel, run_turn,
};
use zuno_error::ProviderError;
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::effort::{
    DeclaredVariants, EffortCapabilities, ProviderFamily, ReasoningEffort, resolve_effort,
};
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};

const SESSION_ID: &str = "ses_reasoning_options";

/// A provider that answers once and keeps the request it was handed.
#[derive(Debug)]
struct RecordingProvider {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl RecordingProvider {
    fn answering_once() -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(vec![vec![
                Ok(StreamEvent::TextDelta("done".to_owned())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                }),
            ]])),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::text_only()
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("request lock").push(request);
        let events = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("one scripted response per request");
        Box::pin(stream::iter(events))
    }
}

/// A resolver whose model carries the reasoning controls a chosen effort resolved to.
#[derive(Debug, Clone)]
struct ReasoningResolver(serde_json::Map<String, Value>);

impl AgentModelResolver for ReasoningResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("build", "You are a test agent."))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "recording" && model_id == "recording-model").then(|| {
            ResolvedModel::new(
                Spec::new("recording"),
                "recording-model",
                ApiSurface::Default,
            )
            .with_reasoning_options(self.0.clone())
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
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-reasoning', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-reasoning', 'reasoning', '/workspace', 'r', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn put_user(connection: &Connection, text: &str) {
    let store = zuno_db::message::MessageStore::new(connection);
    let message = MessageRecord::from_json(json!({
        "id": "msg_reasoning_user",
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": 10 },
        "agent": "build",
        "model": { "providerID": "recording", "modelID": "recording-model" }
    }))
    .expect("valid user message");
    store
        .put_message_at(&message, 10)
        .expect("store user message");
    let part = PartRecord::from_json(
        json!({
            "id": "prt_reasoning_user",
            "sessionID": SESSION_ID,
            "messageID": "msg_reasoning_user",
            "type": "text",
            "text": text
        }),
        10,
    )
    .expect("valid text part");
    store.put_part_at(&part, 10).expect("store user part");
}

async fn drain(mut receiver: mpsc::Receiver<TurnEvent>) {
    while receiver.recv().await.is_some() {}
}

/// The chosen level must arrive in the request, and then in the provider's body.
///
/// This is what separates a working reasoning control from a label. Asserting on
/// `ResolvedModel` would not do: the field can be set correctly and still be dropped
/// on the way to the request, which is precisely the defect the delegation path had —
/// `task` resolved an `effort` and `child_turn` never passed it on.
///
/// The second half covers the last hop. `apply_parameters` is the call every provider
/// adapter makes on its outbound body, so a body that comes back carrying the
/// resolved control is the same overlay Anthropic, OpenAI, Bedrock, Google and the
/// OpenAI-compatible factory each perform.
#[tokio::test]
async fn the_sessions_reasoning_level_reaches_the_provider_request() {
    let resolved = resolve_effort(
        ProviderFamily::Anthropic,
        ReasoningEffort::High,
        EffortCapabilities::default(),
        &DeclaredVariants::new(),
    );
    assert!(
        !resolved.options.is_empty(),
        "the fixture is vacuous if the level resolves to no controls"
    );

    let mut connection = seeded();
    put_user(&connection, "think hard about this");
    let provider = Arc::new(RecordingProvider::answering_once());
    let mut providers = ProviderRegistry::new();
    {
        let provider = Arc::clone(&provider);
        providers.register("recording", move |_spec| provider.clone());
    }
    let resolver = ReasoningResolver(resolved.options.clone());
    let dispatcher = NoTools;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-reasoning", DynamicContext::default()),
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

    let requests = provider.requests();
    let sent = requests.first().expect("the provider received a request");
    assert_eq!(
        sent.parameters, resolved.options,
        "the resolved reasoning controls must reach the request, or the key that chose \
         the level only changed a label"
    );

    let mut body = json!({ "model": "recording-model" });
    sent.apply_parameters(&mut body, ApiSurface::Messages);
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 16000}),
        "the overlay must hand Anthropic its documented wire field: {body}"
    );
    assert!(
        !body.to_string().contains("budgetTokens"),
        "the SDK option name reached the body: {body}"
    );
}

/// A body already carrying sampling fields must keep them when the level lands.
///
/// Gemini's level goes to `generationConfig.thinkingConfig`, and a top-level
/// overwrite would evict the `temperature` the provider had already written
/// alongside it. That is why the overlay merges rather than replaces.
#[tokio::test]
async fn a_nested_reasoning_control_does_not_evict_the_sampling_fields_beside_it() {
    let resolved = resolve_effort(
        ProviderFamily::Google,
        ReasoningEffort::High,
        EffortCapabilities::default(),
        &DeclaredVariants::new(),
    );

    let mut connection = seeded();
    put_user(&connection, "think about this");
    let provider = Arc::new(RecordingProvider::answering_once());
    let mut providers = ProviderRegistry::new();
    {
        let provider = Arc::clone(&provider);
        providers.register("recording", move |_spec| provider.clone());
    }
    let resolver = ReasoningResolver(resolved.options.clone());
    let dispatcher = NoTools;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-nested", DynamicContext::default()),
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

    let requests = provider.requests();
    let sent = requests.first().expect("the provider received a request");
    let mut body = json!({
        "generationConfig": { "temperature": 0.7, "maxOutputTokens": 1024 }
    });
    sent.apply_parameters(&mut body, ApiSurface::Messages);

    assert_eq!(
        body["generationConfig"]["temperature"],
        json!(0.7),
        "the overlay replaced generationConfig instead of merging into it: {body}"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(1024));
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts": true, "thinkingLevel": "high"})
    );
    assert!(
        body.get("thinkingConfig").is_none(),
        "Gemini reads thinkingConfig only inside generationConfig: {body}"
    );
}

/// A session that chose no level must send no reasoning control at all.
///
/// `None` is not [`ReasoningEffort::Off`]: `Off` actively disables thinking, while no
/// choice must leave the request exactly as a build without this feature would send it.
#[tokio::test]
async fn no_chosen_level_sends_no_reasoning_control() {
    let mut connection = seeded();
    put_user(&connection, "answer plainly");
    let provider = Arc::new(RecordingProvider::answering_once());
    let mut providers = ProviderRegistry::new();
    {
        let provider = Arc::clone(&provider);
        providers.register("recording", move |_spec| provider.clone());
    }
    let resolver = ReasoningResolver(serde_json::Map::new());
    let dispatcher = NoTools;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();

    let turn = run_turn(
        RunTurnRequest::new(SESSION_ID, "turn-plain", DynamicContext::default()),
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

    let requests = provider.requests();
    let sent = requests.first().expect("the provider received a request");
    assert!(
        sent.parameters.is_empty(),
        "an unchosen level must add nothing to the request: {:?}",
        sent.parameters
    );
}
