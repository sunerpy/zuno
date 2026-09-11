use super::*;

#[tokio::test]
async fn native_context_usage_preserves_input_when_final_frame_reports_only_output() {
    let mut connection = seeded();
    put_user(
        &connection,
        "context-user",
        10,
        "Use the supplied synthetic context.",
    );
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("Synthetic answer.".to_owned()),
        StreamEvent::TokenUsage {
            input_tokens: Some(149_501),
            output_tokens: None,
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
        StreamEvent::TokenUsage {
            input_tokens: None,
            output_tokens: Some(9),
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-partial").with_context_limit(200_000),
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
    outcome.expect("native provider turn completes");
    let assistant = MessageStore::new(&connection)
        .message("msg_context-partial_0001")
        .unwrap();
    assert_eq!(assistant.data["tokens"]["input"], 149_501);
    assert_eq!(assistant.data["tokens"]["output"], 9);
    let tracker = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .expect("the native request must persist canonical context");
    assert_eq!(tracker.snapshot().used_tokens, Some(149_510));
    assert!(tracker.snapshot().cumulative_known);
    let live = events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ContextUsageUpdated { snapshot } => Some(snapshot.as_ref()),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(live, tracker.snapshot());
}

fn measured(input: u64, output: u64, accounting: PromptAccounting) -> StreamEvent {
    StreamEvent::TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        reasoning_tokens: None,
        cache_read_input_tokens: Some(0),
        cache_write_input_tokens: Some(0),
        accounting,
    }
}

fn text_response(text: &str, input: u64, output: u64) -> ScriptedResponse {
    ScriptedResponse::complete(vec![
        StreamEvent::TextDelta(text.to_owned()),
        measured(input, output, PromptAccounting::CacheInsideInput),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])
}

fn tool_response() -> ScriptedResponse {
    ScriptedResponse::complete(vec![
        StreamEvent::ToolUseStart {
            id: "context-call".to_owned(),
            name: "echo".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: "context-call".to_owned(),
            delta: r#"{"text":"small input"}"#.to_owned(),
        },
        StreamEvent::ToolUseEnd {
            id: "context-call".to_owned(),
        },
        measured(125_350, 40, PromptAccounting::CacheInsideInput),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ])
}

struct LargeContextResult;

#[async_trait]
impl ToolDispatcher for LargeContextResult {
    fn available_tools(&self) -> AvailableTools {
        FakeDispatcher::default().available_tools()
    }

    async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
        PreparedToolDispatch::ready(ToolDispatchResult::success(ToolOutput::text(
            "Returned bounded contents",
            "x".repeat(8_000),
        )))
    }
}

#[tokio::test]
async fn native_context_usage_uses_confirmation_plus_actual_next_request_tail() {
    let mut connection = seeded();
    put_user(&connection, "context-user", 10, "Read a bounded result.");
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(),
        text_response("done", 149_501, 9),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = LargeContextResult;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-tail").with_context_limit(200_000),
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
    outcome.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let estimate = zuno_engine::context_usage::estimate_request_context(&requests[1]);
    let tail = estimate.tail_tokens.unwrap();
    assert!(tail > 2_000 && tail < 2_300);
    assert!(estimate.prompt_tokens < 125_390);
    let start = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ProviderRequestStarted { step: 2, .. }))
        .unwrap();
    let snapshot = events[..start]
        .iter()
        .rev()
        .find_map(|event| match event {
            TurnEvent::ContextUsageUpdated { snapshot } => Some(snapshot.as_ref()),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.used_tokens, Some(125_390 + tail));
    assert_eq!(snapshot.estimated_tail_tokens, Some(tail));
    assert_eq!(
        snapshot.request_estimate_tokens,
        Some(estimate.prompt_tokens)
    );
    assert_eq!(snapshot.cumulative_usage.total(), 125_390);
    let persisted = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.snapshot().used_tokens, Some(149_510));
    assert_eq!(persisted.snapshot().cumulative_usage.total(), 274_900);
}

#[tokio::test(start_paused = true)]
async fn native_context_usage_keeps_failed_attempt_reports_as_an_observed_lower_bound() {
    let mut connection = seeded();
    put_user(
        &connection,
        "context-user",
        10,
        "Retry the synthetic response.",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        ScriptedResponse::failed(
            vec![
                StreamEvent::TextDelta("discarded".to_owned()),
                measured(100, 5, PromptAccounting::CacheInsideInput),
            ],
            ProviderError::Stream {
                code: ProviderStreamFailure::MalformedUpstreamToolArguments,
                source: None,
            },
        ),
        text_response("kept", 200, 10),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-retry").with_context_limit(200_000),
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
    outcome.unwrap();
    let rollback = events
        .iter()
        .find_map(|event| match event {
            TurnEvent::ContextUsageUpdated { snapshot }
                if snapshot
                    .request
                    .as_ref()
                    .is_some_and(|request| request.attempt == 2)
                    && snapshot.last_confirmed.is_none() =>
            {
                Some(snapshot)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(rollback.cumulative_usage.total(), 105);
    assert!(!rollback.cumulative_known);
    let persisted = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.snapshot().used_tokens, Some(210));
    assert_eq!(persisted.snapshot().cumulative_usage.total(), 315);
    assert!(!persisted.snapshot().cumulative_known);
    assert_eq!(provider.requests().len(), 2);
}

struct ForegroundGate {
    waiting: Arc<Semaphore>,
    release: Arc<Semaphore>,
    refreshed: AtomicBool,
}

#[async_trait]
impl TurnHooks for ForegroundGate {
    async fn before_provider_request(
        &self,
        _session: &str,
        _turn: &str,
        step: u32,
    ) -> Result<(), String> {
        if step == 2 {
            self.waiting.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.refreshed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        if self.refreshed.load(Ordering::SeqCst) {
            request
                .developer_context
                .push("Fresh memory after foreground completion.".to_owned());
        }
        Ok(())
    }
}

#[tokio::test]
async fn native_context_usage_waits_before_preparing_the_next_provider_request() {
    let mut connection = seeded();
    put_user(
        &connection,
        "context-user",
        10,
        "Wait for foreground completion.",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(),
        text_response("done", 149_501, 9),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let gate = Arc::new(ForegroundGate {
        waiting: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
        refreshed: AtomicBool::new(false),
    });
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-foreground").with_context_limit(200_000),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(gate.clone()),
        sender,
    );
    let release = async {
        tokio::time::timeout(Duration::from_secs(2), gate.waiting.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(
            provider.requests().len(),
            1,
            "the model must not poll while foreground work is pending"
        );
        assert!(!gate.refreshed.load(Ordering::SeqCst));
        gate.release.add_permits(1);
    };
    let (outcome, _events, ()) = tokio::join!(turn, collect_events(receiver), release);
    outcome.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .developer_context
            .iter()
            .any(|item| item == "Fresh memory after foreground completion.")
    );
}

fn record_input(connection: &mut Connection, id: &str, created: i64, text: &str) {
    let transaction = connection.transaction().unwrap();
    zuno_db::inbox::admit_and_promote_in(
        &transaction,
        NewSessionInput::new(
            id,
            SESSION_ID,
            json!({"kind":"session-message","text":text}),
            InputDelivery::Queue,
            created,
        ),
    )
    .unwrap();
    zuno_db::inbox::mark_consumed_in(&transaction, SESSION_ID, id)
        .unwrap()
        .unwrap();
    transaction.commit().unwrap();
    put_user(connection, id, created, text);
}

struct RemoveOneUserInput;

#[async_trait]
impl TurnHooks for RemoveOneUserInput {
    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        request.messages.retain(|message| {
            !message.content.iter().any(|block| {
                matches!(
                    block, RequestContentBlock::Text { text } if text == "remove-only"
                )
            })
        });
        Ok(())
    }
}

#[tokio::test]
async fn native_context_usage_applies_only_durable_inputs_in_the_post_hook_request() {
    let mut connection = seeded();
    record_input(&mut connection, "old-input", 10, "old-completed");
    {
        let tx = connection.transaction().unwrap();
        let ids = vec!["old-input".to_owned()];
        zuno_db::input_receipt::mark_applied_in(&tx, SESSION_ID, &ids, "old-turn", 11).unwrap();
        zuno_db::input_receipt::finish_turn_in(
            &tx,
            SESSION_ID,
            "old-turn",
            Some(zuno_types::admission::InputStopReason::EndTurn),
            None,
            12,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    record_input(&mut connection, "keep-input", 20, "keep-only");
    record_input(&mut connection, "remove-input", 30, "remove-only");
    let provider = Arc::new(FakeProvider::new(vec![text_response("done", 100, 9)]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("receipt-context").with_context_limit(200_000),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(RemoveOneUserInput)),
        sender,
    );
    let (outcome, _events) = tokio::join!(turn, collect_events(receiver));
    outcome.unwrap();
    use zuno_types::admission::InputReceiptState;
    let get = |id| {
        zuno_db::input_receipt::get_in(&connection, SESSION_ID, id)
            .unwrap()
            .unwrap()
    };
    assert_eq!(get("old-input").state, InputReceiptState::Completed);
    assert_eq!(get("keep-input").state, InputReceiptState::Applied);
    assert_eq!(
        get("keep-input").turn_id.as_deref(),
        Some("receipt-context")
    );
    assert_eq!(get("remove-input").state, InputReceiptState::Recorded);
    let ids: String = connection
        .query_row(
            "SELECT json_extract(data, '$.inputIDs') FROM event \
         WHERE aggregate_id=?1 AND type='session.provider.request.1' \
           AND json_extract(data, '$.status')='started' ORDER BY seq DESC LIMIT 1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ids).unwrap(),
        json!(["keep-input"])
    );
}

struct RunningHandleResult;

#[async_trait]
impl ToolDispatcher for RunningHandleResult {
    fn available_tools(&self) -> AvailableTools {
        FakeDispatcher::default().available_tools()
    }

    async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
        PreparedToolDispatch::ready(ToolDispatchResult::success(ToolOutput::text(
            "Execution handle",
            "foreground execution synthetic-job is still running",
        )))
    }
}

async fn context_turn(
    connection: &mut Connection,
    turn_id: &str,
    provider: &Arc<FakeProvider>,
    dispatcher: &dyn ToolDispatcher,
) -> Vec<TurnEvent> {
    let providers = registry(provider);
    let resolver = FakeResolver;
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request(turn_id).with_context_limit(200_000),
        TurnContext::new(connection, &providers, &resolver, dispatcher, &interrupt),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    outcome.unwrap();
    events
}

#[tokio::test]
async fn native_context_usage_detects_terminal_growth_before_the_last_confirmed_assistant() {
    let mut connection = seeded();
    put_user(&connection, "start-input", 10, "Start foreground work.");
    let initial = Arc::new(FakeProvider::new(vec![
        tool_response(),
        text_response("waiting for work", 130_000, 10),
    ]));
    context_turn(
        &mut connection,
        "context-running",
        &initial,
        &RunningHandleResult,
    )
    .await;

    // A steered input is represented while the original tool row still contains
    // its running handle. The next response confirms that exact input prefix.
    put_user(
        &connection,
        "steer-input",
        now_millis_for_test(),
        "A recorded steer while work is pending.",
    );
    let steered = Arc::new(FakeProvider::new(vec![text_response(
        "steer accepted",
        149_501,
        9,
    )]));
    context_turn(
        &mut connection,
        "context-steered",
        &steered,
        &RunningHandleResult,
    )
    .await;
    let before = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(before.snapshot().used_tokens, Some(149_510));

    // Foreground publication changes the ORIGINAL tool part in place. That row
    // is now before the last confirmed model-generated item, not in its suffix.
    let mut tool = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap()
        .into_iter()
        .find(|message| message.info.id == "msg_context-running_0001")
        .unwrap()
        .parts
        .into_iter()
        .find(|part| part.kind == PartKind::Tool)
        .unwrap();
    tool.data["state"]["output"] = json!("terminal-output ".repeat(8_000));
    MessageStore::new(&connection).put_part(&tool).unwrap();
    put_user(
        &connection,
        "continue-input",
        now_millis_for_test() + 1,
        "Use the terminal output.",
    );
    let continued = Arc::new(FakeProvider::new(vec![text_response("done", 170_000, 10)]));
    let events = context_turn(
        &mut connection,
        "context-expanded",
        &continued,
        &RunningHandleResult,
    )
    .await;
    let started = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ProviderRequestStarted { .. }))
        .unwrap();
    let projected = events[..started]
        .iter()
        .rev()
        .find_map(|event| match event {
            TurnEvent::ContextUsageUpdated { snapshot } => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(projected.used_tokens, None);
    assert_eq!(projected.estimated_tail_tokens, None);
    assert_eq!(
        projected.freshness,
        zuno_types::context_usage::ContextUsageFreshness::Unknown
    );
    assert_eq!(
        projected
            .last_confirmed
            .as_ref()
            .unwrap()
            .usage
            .context_tokens(),
        Some(149_510)
    );
    let after = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(after.snapshot().used_tokens, Some(170_010));
}

fn now_millis_for_test() -> i64 {
    zuno_db::message::now_millis()
}

struct FreshBeforeRequest {
    gate: Arc<ForegroundGate>,
    calls: std::sync::atomic::AtomicUsize,
}

impl zuno_engine::r#loop::DynamicContextRefresher for FreshBeforeRequest {
    fn before_request(
        &self,
        _connection: &Connection,
        _session: &str,
    ) -> Result<Option<DynamicContext>, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 2 {
            assert!(
                self.gate.refreshed.load(Ordering::SeqCst),
                "refresh must follow foreground completion"
            );
        }
        Ok(Some(
            DynamicContext::new(format!("state-{call}")).with_memory(format!("memory-{call}")),
        ))
    }

    fn refresh(
        &self,
        _connection: &Connection,
        _session: &str,
        _refresh: zuno_tool::ToolDynamicContextRefresh,
    ) -> Result<DynamicContext, String> {
        panic!("this fixture emits no tool refresh marker")
    }
}

#[tokio::test]
async fn native_context_usage_refreshes_memory_each_step_after_foreground_wait() {
    let mut connection = seeded();
    put_user(
        &connection,
        "context-user",
        10,
        "Refresh memory without tool markers.",
    );
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(),
        text_response("done", 149_501, 9),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let gate = Arc::new(ForegroundGate {
        waiting: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
        refreshed: AtomicBool::new(false),
    });
    let refresher = FreshBeforeRequest {
        gate: gate.clone(),
        calls: Default::default(),
    };
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-memory").with_context_limit(200_000),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(gate.clone())
        .with_dynamic_context_refresher(&refresher),
        sender,
    );
    let release = async {
        tokio::time::timeout(Duration::from_secs(2), gate.waiting.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.requests().len(), 1);
        gate.release.add_permits(1);
    };
    let (outcome, _events, ()) = tokio::join!(turn, collect_events(receiver), release);
    outcome.unwrap();
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 2);
    let requests = provider.requests();
    assert!(
        requests[0]
            .developer_context
            .iter()
            .any(|item| item.contains("memory-1"))
    );
    assert!(
        requests[1]
            .developer_context
            .iter()
            .any(|item| item.contains("memory-2"))
    );
    assert!(
        requests[1]
            .developer_context
            .iter()
            .all(|item| !item.contains("memory-1"))
    );
}

struct PublishOriginalTerminal {
    calls: std::sync::atomic::AtomicUsize,
}

impl zuno_engine::r#loop::DynamicContextRefresher for PublishOriginalTerminal {
    fn before_request(
        &self,
        connection: &Connection,
        _session: &str,
    ) -> Result<Option<DynamicContext>, String> {
        match self.calls.fetch_add(1, Ordering::SeqCst) + 1 {
            2 => put_user(
                connection,
                "in-run-steer",
                now_millis_for_test(),
                "Steer while the original execution is pending.",
            ),
            3 => {
                let mut tool = MessageStore::new(connection)
                    .hydrate_session(SESSION_ID)
                    .unwrap()
                    .into_iter()
                    .find(|message| message.info.id == "msg_context-inplace_0001")
                    .unwrap()
                    .parts
                    .into_iter()
                    .find(|part| part.kind == PartKind::Tool)
                    .unwrap();
                tool.data["state"]["output"] = json!("foreground terminal output ".repeat(4_000));
                MessageStore::new(connection).put_part(&tool).unwrap();
            }
            _ => {}
        }
        Ok(None)
    }

    fn refresh(
        &self,
        _connection: &Connection,
        _session: &str,
        _refresh: zuno_tool::ToolDynamicContextRefresh,
    ) -> Result<DynamicContext, String> {
        panic!("no tool refresh marker")
    }
}

#[tokio::test]
async fn native_context_usage_handles_steer_and_original_tool_rewrite_within_one_run() {
    let mut connection = seeded();
    put_user(&connection, "context-user", 10, "Run foreground work.");
    let second = ScriptedResponse::complete(vec![
        StreamEvent::ToolUseStart {
            id: "after-steer".to_owned(),
            name: "echo".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: "after-steer".to_owned(),
            delta: r#"{"text":"acknowledge"}"#.to_owned(),
        },
        StreamEvent::ToolUseEnd {
            id: "after-steer".to_owned(),
        },
        measured(149_501, 40, PromptAccounting::CacheInsideInput),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]);
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(),
        second,
        text_response("done", 170_000, 10),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = RunningHandleResult;
    let interrupt = InterruptSignal::new();
    let refresher = PublishOriginalTerminal {
        calls: Default::default(),
    };
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-inplace").with_context_limit(200_000),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_dynamic_context_refresher(&refresher),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    outcome.expect("intentional terminal publication must not fail the request cache");
    assert_eq!(provider.requests().len(), 3);
    let start = events
        .iter()
        .position(|event| matches!(event, TurnEvent::ProviderRequestStarted { step: 3, .. }))
        .unwrap();
    let snapshot = events[..start]
        .iter()
        .rev()
        .find_map(|event| match event {
            TurnEvent::ContextUsageUpdated { snapshot } => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.used_tokens, None);
    assert_eq!(
        snapshot
            .last_confirmed
            .as_ref()
            .unwrap()
            .usage
            .context_tokens(),
        Some(149_541)
    );
}

struct FailPreparedSteeringRequest;

#[async_trait]
impl TurnHooks for FailPreparedSteeringRequest {
    async fn prepare_request(
        &self,
        _input: zuno_engine::hooks::RequestHookInput<'_>,
        _request: &mut CompletionRequest,
    ) -> Result<(), String> {
        Err("synthetic pre-dispatch failure".to_owned())
    }
}

#[tokio::test]
async fn native_context_usage_consumed_steer_has_an_owner_before_dispatch_can_fail() {
    use zuno_types::admission::InputReceiptState;

    let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).unwrap());
    {
        let mut connection = pool.get().unwrap();
        migration::apply(&mut connection).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project-loop', '/workspace', 1, 1, '[]');
                 INSERT INTO session
                   (id, project_id, slug, directory, title, version, time_created, time_updated)
                 VALUES ('{SESSION_ID}', 'project-loop', 'loop', '/workspace', 'loop', '1', 1, 1);"
            ))
            .unwrap();
        put_user(&connection, "context-user", 10, "Original input.");
    }
    let inbox = SessionInbox::new(pool.clone());
    inbox
        .admit(NewSessionInput::new(
            "context-consumed-steer",
            SESSION_ID,
            json!({"kind": "user", "prompt": {"text": "Steer before dispatch."}}),
            InputDelivery::Steer,
            11,
        ))
        .unwrap();
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION_ID).unwrap();
    runs.queue_soft_interrupt(
        SESSION_ID,
        SoftInterruptMessage {
            revision: None,
            input_id: Some("context-consumed-steer".to_owned()),
            content: "Steer before dispatch.".to_owned(),
            images: Vec::new(),
            attachments: Vec::new(),
            urgent: false,
            source: SoftInterruptSource::User,
        },
    )
    .unwrap();
    let provider = Arc::new(FakeProvider::new(Vec::new()));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let mut connection = pool.get().unwrap();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-steer-owner"),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox)
        .with_hooks(Arc::new(FailPreparedSteeringRequest)),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    assert!(matches!(outcome, Err(TurnError::Hook(message))
        if message == "synthetic pre-dispatch failure"));
    assert!(provider.requests().is_empty());
    assert!(events.iter().any(|event| matches!(
        event,
        TurnEvent::InputConsumed { input_id, .. } if input_id == "context-consumed-steer"
    )));
    let receipt = zuno_db::input_receipt::get_in(&connection, SESSION_ID, "context-consumed-steer")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, InputReceiptState::Recorded);
    assert_eq!(receipt.turn_id.as_deref(), Some("context-steer-owner"));
    assert_eq!(receipt.applied_at, None, "no model dispatch occurred");
    pool.transaction(|transaction| {
        zuno_db::input_receipt::finish_turn_in(
            transaction,
            SESSION_ID,
            "context-steer-owner",
            None,
            Some("synthetic pre-dispatch failure"),
            zuno_db::message::now_millis(),
        )
    })
    .unwrap();
    let settled = zuno_db::input_receipt::get_in(&connection, SESSION_ID, "context-consumed-steer")
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, InputReceiptState::Failed);
    assert_eq!(settled.applied_at, None);
}

struct ExpandFirstAssistant;

#[async_trait]
impl TurnHooks for ExpandFirstAssistant {
    async fn text_complete(
        &self,
        _session_id: &str,
        message_id: &str,
        _part_id: &str,
        text: &mut String,
    ) -> Result<(), String> {
        if message_id.ends_with("_0001") {
            text.push_str(&" post-generation rewrite".repeat(1_000));
        }
        Ok(())
    }
}

#[tokio::test]
async fn native_context_usage_text_complete_rewrite_invalidates_window_not_consumption() {
    let mut connection = seeded();
    put_user(&connection, "context-user", 10, "Run the synthetic tool.");
    let mut first = tool_response();
    first.events.insert(
        0,
        Ok(StreamEvent::TextDelta("Original response.".to_owned())),
    );
    let provider = Arc::new(FakeProvider::new(vec![
        first,
        text_response("done", 149_501, 9),
    ]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request("context-text-rewrite").with_context_limit(200_000),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            &interrupt,
        )
        .with_hooks(Arc::new(ExpandFirstAssistant)),
        sender,
    );
    let (outcome, events) = tokio::join!(turn, collect_events(receiver));
    outcome.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == Role::Assistant
            && message.content.iter().any(
                |block| matches!(block, RequestContentBlock::Text { text } if text.len() > 20_000),
            )
    }));
    for boundary in [
        events
            .iter()
            .position(|event| matches!(event, TurnEvent::AssistantCheckpointed { step: 1, .. }))
            .unwrap(),
        events
            .iter()
            .position(|event| matches!(event, TurnEvent::ProviderRequestStarted { step: 2, .. }))
            .unwrap(),
    ] {
        let snapshot = events[..boundary]
            .iter()
            .rev()
            .find_map(|event| match event {
                TurnEvent::ContextUsageUpdated { snapshot } => Some(snapshot),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            snapshot.used_tokens, None,
            "the assistant changed after measurement"
        );
        assert_eq!(snapshot.estimated_tail_tokens, None);
        assert_eq!(
            snapshot.freshness,
            zuno_types::context_usage::ContextUsageFreshness::Unknown
        );
        assert_eq!(snapshot.cumulative_usage.total(), 125_390);
        assert!(
            snapshot.cumulative_known,
            "rewriting does not undo known consumption"
        );
    }
    let tracker = zuno_db::context_usage::read_in(&connection, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(tracker.snapshot().used_tokens, Some(149_510));
    assert_eq!(tracker.snapshot().cumulative_usage.total(), 274_900);
    assert!(tracker.snapshot().cumulative_known);
}
