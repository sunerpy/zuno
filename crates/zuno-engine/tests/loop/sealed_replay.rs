use super::*;
use zuno_engine::hooks::HookMessageWithParts;
use zuno_engine::r#loop::project_history_owned_with_system_messages;

const ORIGINAL_ARGUMENTS: &str = "{ \"z\": 1, \"text\": \"中文\\nvalue\", \"a\": 2 }";
const ORIGINAL_CAPSULE: &str = "kr1_fixture_do_not_log_or_rewrite";

fn seed_sealed_history(connection: &Connection) -> Vec<zuno_llm::registry::RequestMessage> {
    put_user(connection, "msg_replay_user", 10, "read the fixture");
    put_assistant_text(
        connection,
        "msg_replay_prior",
        20,
        "msg_replay_user",
        "Original assistant output.",
    );
    let store = MessageStore::new(connection);
    for (id, created, payload) in [
        (
            "prt_replay_reasoning",
            19,
            json!({
                "type": "reasoning",
                "metadata": {"providerReasoning": {
                    "id": "rs_fixture", "encryptedContent": ORIGINAL_CAPSULE,
                    "summary": ["Original summary."], "status": "completed"
                }}
            }),
        ),
        (
            "prt_replay_tool",
            21,
            json!({
                "type": "tool", "callID": "call_original", "tool": "echo",
                "toolSchemaIdentity": FakeDispatcher::default().available_tools()
                    .definitions[0].schema_identity(),
                "state": {
                    "status": "completed",
                    "input": serde_json::from_str::<Value>(ORIGINAL_ARGUMENTS).unwrap(),
                    "raw": ORIGINAL_ARGUMENTS,
                    "output": "Original result."
                }
            }),
        ),
    ] {
        let mut payload = payload;
        payload["id"] = json!(id);
        payload["sessionID"] = json!(SESSION_ID);
        payload["messageID"] = json!("msg_replay_prior");
        store
            .put_part_at(&PartRecord::from_json(payload, created).unwrap(), created)
            .unwrap();
    }
    put_user(
        connection,
        "msg_replay_next",
        30,
        "continue without using tools",
    );
    project_history_owned_with_system_messages(&[], store.hydrate_session(SESSION_ID).unwrap())
}

fn done_response() -> ScriptedResponse {
    ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("done".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])
}

#[derive(Debug)]
struct RemoveCurrentTools;

#[async_trait]
impl TurnHooks for RemoveCurrentTools {
    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        request.tools.clear();
        Ok(())
    }
}

async fn assert_preserved_replay(
    definitions: Vec<ToolDefinition>,
    hooks: Arc<dyn TurnHooks>,
    resolver: &dyn AgentModelResolver,
) {
    let mut connection = seeded();
    let mut expected = seed_sealed_history(&connection);
    let model = resolver.resolve_model("fake", "fake-model").unwrap();
    if !zuno_llm::registry::ReasoningReplayPolicy::from_spec(&model.provider)
        .unwrap()
        .requests_encrypted()
    {
        for message in &mut expected {
            message.content.retain(|block| {
                !matches!(
                    block,
                    RequestContentBlock::ProviderEncryptedReasoning { .. }
                )
            });
        }
    }
    let original = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap();
    let provider = Arc::new(FakeProvider::new(vec![done_response()]));
    let providers = registry(&provider);
    let dispatcher = SnapshotDispatcher { definitions };
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("replay-resume"),
        TurnContext::new(
            &mut connection,
            &providers,
            resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(hooks),
        sender,
    );
    let (outcome, _) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("the resumed request completes");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let actual = requests[0]
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "current declarations changed sealed history"
    );
    let persisted = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap();
    assert_eq!(&persisted[..original.len()], original.as_slice());
}

#[tokio::test]
async fn responses_replay_preserves_history_when_tool_removed() {
    assert_preserved_replay(
        Vec::new(),
        Arc::new(RemoveCurrentTools),
        &EncryptedReplayResolver,
    )
    .await;
}

#[tokio::test]
async fn responses_replay_preserves_history_when_hook_removes_declaration() {
    assert_preserved_replay(
        FakeDispatcher::default().available_tools().definitions,
        Arc::new(RemoveCurrentTools),
        &EncryptedReplayResolver,
    )
    .await;
}

#[tokio::test]
async fn responses_replay_preserves_history_when_state_tool_schema_changes() {
    let mut definition = ProgressiveDispatcher::definition("echo", "New schema.");
    definition.parameters = json!({
        "type": "object", "properties": {"replacement": {"type": "boolean"}},
        "required": ["replacement"]
    });
    definition.history_policy = zuno_tool::HistoryPolicy::AuthoritativeState;
    assert_preserved_replay(
        vec![definition],
        Arc::new(RemoveCurrentTools),
        &EncryptedReplayResolver,
    )
    .await;
}

struct PlainResponsesResolver(ApiSurface);

impl AgentModelResolver for PlainResponsesResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        FakeResolver.resolve_agent(requested)
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "fake" && model_id == "fake-model").then(|| {
            ResolvedModel::new(
                Spec::new("fake").with_surface(ApiSurface::Responses),
                "fake-model",
                self.0,
            )
        })
    }
}

#[tokio::test]
async fn responses_history_is_preserved_even_when_reasoning_replay_is_off() {
    for surface in [ApiSurface::Default, ApiSurface::Responses] {
        assert_preserved_replay(
            Vec::new(),
            Arc::new(RemoveCurrentTools),
            &PlainResponsesResolver(surface),
        )
        .await;
    }
}

#[derive(Debug, Clone, Copy)]
enum MutationStage {
    Messages,
    Request,
    Capsule,
    Boundary,
    Parameters,
    AdjacentAssistant,
    Model,
    Surface,
    ModelParameter,
    TextComplete,
}

#[derive(Debug)]
struct RewriteSealedText(MutationStage);

#[async_trait]
impl TurnHooks for RewriteSealedText {
    fn enabled(&self) -> bool {
        true
    }

    async fn transform_messages(
        &self,
        _session_id: &str,
        messages: &mut Vec<HookMessageWithParts>,
    ) -> Result<(), String> {
        if matches!(self.0, MutationStage::Messages) {
            for message in messages {
                for part in &mut message.parts {
                    if part.data.get("text").and_then(Value::as_str)
                        == Some("Original assistant output.")
                    {
                        part.data
                            .insert("text".to_owned(), json!("Changed output."));
                    }
                }
            }
        }
        Ok(())
    }

    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        if matches!(self.0, MutationStage::Request) {
            for message in &mut request.messages {
                for block in &mut message.content {
                    if let RequestContentBlock::Text { text } = block
                        && text == "Original assistant output."
                    {
                        *text = "Changed output.".to_owned();
                    }
                }
            }
        }
        if matches!(self.0, MutationStage::Parameters) {
            request.parameters.insert("input".to_owned(), json!([]));
        }
        if matches!(self.0, MutationStage::Model) {
            request.model_id = "different-model".to_owned();
        }
        if matches!(self.0, MutationStage::Surface) {
            request.surface = ApiSurface::Chat;
        }
        if matches!(self.0, MutationStage::ModelParameter) {
            request
                .parameters
                .insert("model".to_owned(), json!("different-model"));
        }
        if matches!(self.0, MutationStage::AdjacentAssistant) {
            let index = request
                .messages
                .iter()
                .position(|message| {
                    message.content.iter().any(|block| {
                        matches!(
                            block,
                            RequestContentBlock::ProviderEncryptedReasoning { .. }
                        )
                    })
                })
                .unwrap();
            request.messages.insert(
                index,
                zuno_llm::registry::RequestMessage::new(zuno_llm::event::Message::new(
                    Role::Assistant,
                    "Injected adjacent assistant output.",
                )),
            );
        }
        for message in &mut request.messages {
            if matches!(self.0, MutationStage::Capsule) {
                for block in &mut message.content {
                    if let RequestContentBlock::ProviderEncryptedReasoning {
                        encrypted_content,
                        ..
                    } = block
                    {
                        *encrypted_content = Some("replacement_capsule".to_owned());
                    }
                }
            }
            if matches!(self.0, MutationStage::Boundary) && message.role == Role::Assistant {
                *message = message.clone().with_preceding_responses_input(
                    zuno_llm::registry::ResponsesInputBoundary::from_developer_context(vec![
                        "replacement boundary".to_owned(),
                    ]),
                );
            }
        }
        Ok(())
    }

    async fn text_complete(
        &self,
        _session_id: &str,
        _message_id: &str,
        _part_id: &str,
        text: &mut String,
    ) -> Result<(), String> {
        if matches!(self.0, MutationStage::TextComplete) {
            *text = "rewritten output".to_owned();
        }
        Ok(())
    }
}

#[tokio::test]
async fn sealed_replay_rejects_hook_rewrites_before_provider_dispatch() {
    for stage in [
        MutationStage::Messages,
        MutationStage::Request,
        MutationStage::Capsule,
        MutationStage::Boundary,
        MutationStage::Parameters,
        MutationStage::AdjacentAssistant,
        MutationStage::Model,
        MutationStage::Surface,
        MutationStage::ModelParameter,
    ] {
        let mut connection = seeded();
        seed_sealed_history(&connection);
        let provider = Arc::new(FakeProvider::new(vec![done_response()]));
        let providers = registry(&provider);
        let dispatcher = SnapshotDispatcher {
            definitions: Vec::new(),
        };
        let interrupt = InterruptSignal::new();
        let (sender, receiver) = event_channel();
        let turn = run_turn(
            request("replay-hook"),
            TurnContext::new(
                &mut connection,
                &providers,
                &EncryptedReplayResolver,
                &dispatcher,
                &interrupt,
            )
            .with_hooks(Arc::new(RewriteSealedText(stage))),
            sender,
        );
        let (outcome, _) = tokio::join!(turn, collect_events(receiver));
        assert!(
            matches!(&outcome, Err(TurnError::Hook(detail))
                if detail.contains("sealed") && !detail.contains(ORIGINAL_CAPSULE)),
            "sealed history mutation at {stage:?} was not rejected: {outcome:?}"
        );
        assert!(provider.requests().is_empty());
    }
}

#[tokio::test]
async fn responses_replay_does_not_reauthorize_removed_tools() {
    let mut connection = seeded();
    let expected = seed_sealed_history(&connection);
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let dispatcher = zuno_engine::dispatch::ToolRegistryDispatcher::new(
        Vec::new(),
        Vec::new(),
        Arc::new(zuno_tool::DenyAll),
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("replay-no-authority"),
        TurnContext::new(
            &mut connection,
            &providers,
            &EncryptedReplayResolver,
            &dispatcher,
            &interrupt,
        ),
        sender,
    );
    let (outcome, _) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("the model can receive a denied-call result and finish");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.tools.is_empty()));
    assert_eq!(
        requests[0].messages[1..],
        expected,
        "historical input remains intact without exposing any current tools"
    );
    assert!(requests[1].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, RequestContentBlock::ToolResult {
                tool_use_id, is_error: Some(true), ..
            } if tool_use_id == "call-1")
        })
    }));
}

struct CountingEchoTool(Arc<AtomicBool>);

#[async_trait]
impl zuno_tool::Tool for CountingEchoTool {
    fn id(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Record a fixture invocation."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }

    async fn execute(
        &self,
        _args: Value,
        _context: zuno_tool::ToolContext,
    ) -> Result<ToolOutput, zuno_error::ToolError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(ToolOutput::text("echo", "fixture invocation"))
    }
}

#[tokio::test]
async fn responses_replay_does_not_reauthorize_hook_hidden_tool() {
    let mut connection = seeded();
    seed_sealed_history(&connection);
    let provider = Arc::new(FakeProvider::new(full_turn_responses()));
    let providers = registry(&provider);
    let executed = Arc::new(AtomicBool::new(false));
    let dispatcher = zuno_engine::dispatch::ToolRegistryDispatcher::new(
        vec![Arc::new(CountingEchoTool(Arc::clone(&executed)))],
        Vec::new(),
        Arc::new(zuno_tool::DenyAll),
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("replay-hook-hidden"),
        TurnContext::new(
            &mut connection,
            &providers,
            &EncryptedReplayResolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(RemoveCurrentTools)),
        sender,
    );
    let (outcome, _) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("denied tool result can be consumed");
    assert!(
        provider
            .requests()
            .iter()
            .all(|request| request.tools.is_empty())
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "a registered tool removed from this request was executed"
    );
}

#[tokio::test]
async fn sealed_output_cannot_be_rewritten_by_text_complete_hook() {
    let mut connection = seeded();
    put_user(&connection, "msg_fresh_sealed", 10, "answer");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::ProviderReasoningItem {
            id: "rs_fresh".to_owned(),
            summary: vec!["original summary".to_owned()],
            encrypted_content: Some(ORIGINAL_CAPSULE.to_owned()),
            status: Some("completed".to_owned()),
        },
        StreamEvent::TextDelta("original fresh output".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let dispatcher = SnapshotDispatcher {
        definitions: Vec::new(),
    };
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("fresh-sealed"),
        TurnContext::new(
            &mut connection,
            &providers,
            &EncryptedReplayResolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(RewriteSealedText(MutationStage::TextComplete))),
        sender,
    );
    let (outcome, _) = tokio::join!(turn, collect_events(receiver));
    assert!(
        matches!(&outcome, Err(TurnError::Hook(detail))
            if detail.contains("sealed") && !detail.contains(ORIGINAL_CAPSULE)),
        "text_complete changed provider-sealed output: {outcome:?}"
    );
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap();
    let text = history
        .iter()
        .filter(|message| message.info.role == zuno_db::message::MessageRole::Assistant)
        .flat_map(|message| &message.parts)
        .filter(|part| part.kind == PartKind::Text)
        .filter_map(|part| part.data.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(text, ["original fresh output"]);
    assert_eq!(provider.requests().len(), 1, "no automatic replay");
}
