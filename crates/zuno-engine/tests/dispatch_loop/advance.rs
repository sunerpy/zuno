use super::*;
use zuno_engine::advance::{AdvanceError, AdvanceOutcome, AdvanceRequest};
use zuno_engine::budget::{
    BudgetDecision, BudgetPolicyError, NoopBudgetPolicy, TurnBudgetPolicy, TurnUsageSnapshot,
};
use zuno_engine::driver::{AgentDriver, DefaultAgentDriver};
use zuno_engine::r#loop::ToolDispatcher;

#[path = "waits.rs"]
mod waits;

fn request() -> AdvanceRequest {
    AdvanceRequest::new(
        RunTurnRequest::new(
            SESSION_ID,
            "turn-bounded",
            DynamicContext::new("Investigate the task"),
        ),
        "a".repeat(64),
        NonZeroU32::MIN,
    )
    .expect("valid immutable configuration identity")
}

fn dispatcher(tools: Vec<Arc<dyn Tool>>) -> ToolRegistryDispatcher {
    ToolRegistryDispatcher::new(
        tools,
        vec![Rule {
            source: None,
            permission: "*".to_owned(),
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
        Arc::new(AllowAll),
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
}

struct AliasedResolver;
impl AgentModelResolver for AliasedResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("canonical-build", "Alias fixture"))
    }
    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        Resolver.resolve_model(provider, model)
    }
}

struct CanonicalAgentProbe;
#[async_trait]
impl Tool for CanonicalAgentProbe {
    fn id(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Check canonical dispatch attribution."
    }
    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
    }
    async fn execute(&self, _args: Value, context: ToolContext) -> Result<ToolOutput, ToolError> {
        assert_eq!(context.agent, "canonical-build");
        Ok(ToolOutput::text("Agent", "Canonical identity retained"))
    }
}

#[tokio::test]
async fn tool_phase_uses_the_resolved_agent_identity_instead_of_its_requested_alias() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[(
        "alias", "inspect",
    )])));
    let providers = registry(provider);
    let dispatcher = dispatcher(vec![Arc::new(CanonicalAgentProbe)]);
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        DefaultAgentDriver.advance(
            request(),
            TurnContext::new(
                &mut connection,
                &providers,
                &AliasedResolver,
                &dispatcher,
                &interrupt
            ),
            sender,
        ),
        collect_events(receiver),
    );
    assert!(matches!(
        outcome.unwrap(),
        AdvanceOutcome::Progressed { .. }
    ));
}

async fn advance(
    connection: &mut Connection,
    provider: Arc<ScriptedProvider>,
    dispatcher: &dyn ToolDispatcher,
    request: AdvanceRequest,
    budget: Arc<dyn TurnBudgetPolicy>,
) -> (Result<AdvanceOutcome, AdvanceError>, Vec<TurnEvent>) {
    let providers = registry(provider);
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    tokio::join!(
        DefaultAgentDriver.advance(
            request,
            TurnContext::new(connection, &providers, &Resolver, dispatcher, &interrupt)
                .with_budget_policy(budget),
            sender,
        ),
        collect_events(receiver),
    )
}

#[derive(Default)]
struct BudgetProbe(Mutex<Vec<(u32, u64, u32, u64)>>);

#[async_trait]
impl TurnBudgetPolicy for BudgetProbe {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        self.0.lock().unwrap().push((
            snapshot.step,
            snapshot.turn_usage.total(),
            snapshot.tool_calls_dispatched,
            snapshot.elapsed_seconds,
        ));
        Ok(BudgetDecision::Continue)
    }
}

struct ClockProbe(Arc<AtomicUsize>);

#[async_trait]
impl Tool for ClockProbe {
    fn id(&self) -> &str {
        "clock_probe"
    }
    fn description(&self) -> &str {
        "Measure a bounded operation."
    }
    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
    }
    async fn execute(&self, _args: Value, _context: ToolContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        Ok(ToolOutput::text("clock", "operation has completed"))
    }
}

fn usage() -> StreamEvent {
    StreamEvent::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(7),
        reasoning_tokens: None,
        cache_read_input_tokens: Some(20),
        cache_write_input_tokens: Some(0),
        accounting: zuno_llm::event::PromptAccounting::CacheBesideInput,
    }
}

#[tokio::test]
async fn bounded_resume_reopens_the_database_and_keeps_results_usage_steps_and_time() {
    let directory = tempfile::tempdir().expect("isolated database");
    let path = directory.path().join("preview.db");
    let mut connection = seed_connection(open::open_at(&path).expect("open"));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut script = named_provider_events("clock_probe", &[("call-once", "measure")]);
    script[0].insert(0, usage());
    let second_response = script.pop().unwrap();
    let first_provider = Arc::new(ScriptedProvider::new(script));
    let first_dispatcher = dispatcher(vec![Arc::new(ClockProbe(calls.clone()))]);
    let (result, first_events) = advance(
        &mut connection,
        first_provider.clone(),
        &first_dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.expect("first safe boundary") else {
        panic!("a tool step must not finish its turn");
    };
    assert_eq!(first_provider.requests().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !first_events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
    );
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap();
    let tool = history
        .iter()
        .flat_map(|message| &message.parts)
        .find(|part| part.kind == PartKind::Tool)
        .expect("durable result");
    assert_eq!(tool.data["state"]["output"], "operation has completed");
    assert_eq!(tool.data["state"]["status"], "completed");
    // The reference survives a transport round trip; its body is loaded from storage.
    let checkpoint = serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
    drop(connection);
    drop(first_dispatcher);
    drop(first_provider);

    let mut connection = open::open_at(&path).expect("a different worker connection");
    migration::apply(&mut connection).expect("validate existing database");
    let second_provider = Arc::new(ScriptedProvider::new(vec![second_response]));
    let fresh_dispatcher = dispatcher(vec![Arc::new(ClockProbe(calls.clone()))]);
    let fresh_budget = Arc::new(BudgetProbe::default());
    let (result, second_events) = advance(
        &mut connection,
        second_provider.clone(),
        &fresh_dispatcher,
        request().resume(checkpoint),
        fresh_budget.clone(),
    )
    .await;
    assert!(matches!(
        result.unwrap(),
        AdvanceOutcome::Completed { steps: 2, .. }
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "completed operations are never replayed"
    );
    assert_eq!(second_provider.requests().len(), 1);
    assert!(
        !second_events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnStarted { .. }))
    );
    let observations = fresh_budget.0.lock().unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0, 2);
    assert_eq!(
        observations[0].1, 127,
        "the new worker receives already-spent tokens"
    );
    assert_eq!(observations[0].2, 1, "tool allowance is cumulative");
    assert!(
        observations[0].3 >= 1,
        "elapsed execution time cannot restart at zero"
    );
}

#[tokio::test]
async fn changed_configuration_and_consumed_checkpoint_cannot_start_another_request() {
    let mut connection = seeded();
    let order = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = dispatcher(vec![Arc::new(SequentialTool {
        active: AtomicUsize::new(0),
        order: order.clone(),
    })]);
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[("one", "once")])));
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
        panic!("checkpoint")
    };
    let (retried, events) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert_eq!(
        retried.unwrap(),
        AdvanceOutcome::Progressed {
            checkpoint: checkpoint.clone()
        }
    );
    assert!(
        events.is_empty(),
        "a lost response is recovered without replaying live events"
    );
    let changed = AdvanceRequest::new(request().run, "b".repeat(64), NonZeroU32::MIN)
        .unwrap()
        .resume(checkpoint.clone());
    let (result, events) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        changed,
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(result, Err(AdvanceError::Conflict)));
    assert!(events.is_empty());
    assert_eq!(provider.requests().len(), 1);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint.clone()),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let terminal = result.unwrap();
    assert!(matches!(
        terminal,
        AdvanceOutcome::Completed { steps: 2, .. }
    ));
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert_eq!(
        result.unwrap(),
        terminal,
        "a lost completion response reads the original outcome"
    );
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(*order.lock().unwrap(), ["once"]);
}

struct UnfinishedTool(Arc<tokio::sync::Notify>);
#[async_trait]
impl Tool for UnfinishedTool {
    fn id(&self) -> &str {
        "unfinished"
    }
    fn description(&self) -> &str {
        "Simulate a worker dying after handoff."
    }
    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
    }
    async fn execute(&self, _args: Value, _context: ToolContext) -> Result<ToolOutput, ToolError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_lost_worker_without_a_completed_checkpoint_requires_inspection() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(named_provider_events(
        "unfinished",
        &[("inflight", "unobserved effect")],
    )));
    let started = Arc::new(tokio::sync::Notify::new());
    let dispatcher = dispatcher(vec![Arc::new(UnfinishedTool(started.clone()))]);
    {
        let running = advance(
            &mut connection,
            provider.clone(),
            &dispatcher,
            request(),
            Arc::new(NoopBudgetPolicy),
        );
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => panic!("the operation must remain in flight: {result:?}"),
            started = tokio::time::timeout(Duration::from_secs(10), started.notified()) => {
                started.expect("reach the actual tool handoff before simulating worker loss");
            }
        }
    }
    assert_eq!(provider.requests().len(), 1);
    let (result, events) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(result, Err(AdvanceError::NeedsInspection)));
    assert!(events.is_empty());
    assert_eq!(
        provider.requests().len(),
        1,
        "no automatic side-effect replay"
    );
    // The same in-flight transcript from an older driver has no advance ledger.
    connection
        .execute(
            "DELETE FROM event WHERE aggregate_id=?1 AND type LIKE 'runtime.driver.advance%'",
            [SESSION_ID],
        )
        .unwrap();
    let before = MessageStore::new(&connection)
        .unfinished_tool_parts_for_session(SESSION_ID)
        .unwrap();
    assert_eq!(before.len(), 1);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(result, Err(AdvanceError::NeedsInspection)));
    assert_eq!(
        MessageStore::new(&connection)
            .unfinished_tool_parts_for_session(SESSION_ID)
            .unwrap(),
        before
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "legacy unfinished effects are not auto-replayed"
    );
}

#[tokio::test]
async fn checkpoint_resume_preserves_the_stagnant_tool_loop_guard() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(repeated_plan_events(3, false)));
    let dispatcher = dispatcher(vec![Arc::new(PlanProbeTool {
        calls: AtomicUsize::new(0),
        stable: true,
    })]);
    let mut next = request();
    for _ in 0..2 {
        let (result, _) = advance(
            &mut connection,
            provider.clone(),
            &dispatcher,
            next,
            Arc::new(NoopBudgetPolicy),
        )
        .await;
        let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
            panic!("checkpoint")
        };
        next = request().resume(checkpoint);
    }
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        next,
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(
        result,
        Err(AdvanceError::Turn(TurnError::StagnantToolLoop {
            count: 3,
            ..
        }))
    ));
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn another_principal_cannot_advance_a_local_sessions_checkpoint() {
    use std::num::NonZeroU64;
    use zuno_types::identity::{PrincipalId, PrincipalKind, PrincipalScope, TenantId};
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let providers = registry(provider.clone());
    let dispatcher = dispatcher(Vec::new());
    let interrupt = InterruptSignal::new();
    let (sender, receiver) = event_channel();
    let (result, events) = tokio::join!(
        DefaultAgentDriver.advance(
            request(),
            TurnContext::new(
                &mut connection,
                &providers,
                &Resolver,
                &dispatcher,
                &interrupt,
            )
            .with_principal_scope(PrincipalScope::new(
                TenantId::new("organization").unwrap(),
                PrincipalId::new("alice").unwrap(),
                PrincipalKind::User,
                None,
                NonZeroU64::MIN,
            )),
            sender
        ),
        collect_events(receiver),
    );
    assert!(matches!(
        result,
        Err(AdvanceError::Database(zuno_error::DbError::NotFound { .. }))
    ));
    assert!(events.is_empty());
    assert!(provider.requests().is_empty());
}

struct SpentBudget;
#[async_trait]
impl TurnBudgetPolicy for SpentBudget {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(if snapshot.turn_usage.total() >= 127 {
            BudgetDecision::stop_tokens("the turn allowance is already spent")
        } else {
            BudgetDecision::Continue
        })
    }
}

#[tokio::test]
async fn a_new_worker_cannot_spend_an_allowance_that_the_checkpoint_already_exhausted() {
    let mut connection = seeded();
    let mut script = provider_events(&[("once", "finish the operation")]);
    script[0].insert(0, usage());
    // There must be no request after the checkpoint, even though another response exists.
    let provider = Arc::new(ScriptedProvider::new(script));
    let dispatcher = dispatcher(vec![Arc::new(SequentialTool {
        active: AtomicUsize::new(0),
        order: Arc::new(Mutex::new(Vec::new())),
    })]);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
        panic!("checkpoint")
    };
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(SpentBudget),
    )
    .await;
    assert!(
        matches!(result, Err(AdvanceError::Turn(TurnError::BudgetLimited { kind, .. }))
        if kind == zuno_engine::budget::BudgetStopKind::TokenBudget)
    );
    assert_eq!(provider.requests().len(), 1);
    let record =
        zuno_db::event_log::latest_of_type_in(&connection, SESSION_ID, "runtime.driver.advance")
            .unwrap()
            .unwrap();
    assert_eq!(record.properties["state"]["recovery"]["kind"], "pause");
    assert_eq!(
        record.properties["state"]["budget_stop"]["kind"],
        "token_budget"
    );
}

#[tokio::test]
async fn an_unknown_checkpoint_schema_fails_without_mutation_or_a_provider_request() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[(
        "once", "complete",
    )])));
    let dispatcher = dispatcher(vec![Arc::new(SequentialTool {
        active: AtomicUsize::new(0),
        order: Arc::new(Mutex::new(Vec::new())),
    })]);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
        panic!("checkpoint")
    };
    connection.execute(
        "UPDATE event SET data=json_set(data,'$.schemaVersion',999) WHERE aggregate_id=?1 AND seq=?2",
        (SESSION_ID, checkpoint.sequence()),
    ).unwrap();
    let before =
        zuno_db::event_log::latest_of_type_in(&connection, SESSION_ID, "runtime.driver.advance")
            .unwrap()
            .unwrap();
    let (result, events) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(result, Err(AdvanceError::InvalidCheckpoint(_))));
    assert!(events.is_empty());
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        zuno_db::event_log::latest_of_type_in(&connection, SESSION_ID, "runtime.driver.advance",)
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn resumed_execution_keeps_its_admitted_agent_and_model_identity() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[(
        "once", "complete",
    )])));
    let dispatcher = dispatcher(vec![Arc::new(SequentialTool {
        active: AtomicUsize::new(0),
        order: Arc::new(Mutex::new(Vec::new())),
    })]);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
        panic!("checkpoint")
    };
    // A later message must not silently replace the identity of the running turn.
    let new_user = MessageRecord::from_json(json!({
        "id":"msg_new_settings",
        "sessionID":SESSION_ID,
        "role":"user",
        "time":{"created": zuno_db::message::now_millis() + 1_000},
        "agent":"different-agent",
        "model":{"providerID":"different-provider","modelID":"different-model"}
    }))
    .unwrap();
    MessageStore::new(&connection)
        .put_message(&new_user)
        .unwrap();
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(
        result.unwrap(),
        AdvanceOutcome::Completed { steps: 2, .. }
    ));
    assert_eq!(provider.requests().len(), 2);
}

struct WallClockLimit;
#[async_trait]
impl TurnBudgetPolicy for WallClockLimit {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(if snapshot.elapsed_seconds >= 30 {
            BudgetDecision::stop_time("the turn wall-clock allowance expired while queued")
        } else {
            BudgetDecision::Continue
        })
    }
}

#[tokio::test]
async fn time_between_checkpoints_counts_toward_the_same_turn_allowance() {
    let mut connection = seeded();
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[(
        "once", "complete",
    )])));
    let dispatcher = dispatcher(vec![Arc::new(SequentialTool {
        active: AtomicUsize::new(0),
        order: Arc::new(Mutex::new(Vec::new())),
    })]);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = result.unwrap() else {
        panic!("checkpoint")
    };
    // Advance the persisted wall-clock age without a slow or timing-sensitive test.
    connection
        .execute(
            "UPDATE event SET data=json_set(data,'$.state.checkpoint.startedAtMs',
         CAST(unixepoch('subsec')*1000 AS INTEGER)-60000)
         WHERE aggregate_id=?1 AND seq=?2",
            (SESSION_ID, checkpoint.sequence()),
        )
        .unwrap();
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(WallClockLimit),
    )
    .await;
    assert!(matches!(
        result,
        Err(AdvanceError::Turn(TurnError::BudgetLimited {
            kind: zuno_engine::budget::BudgetStopKind::TimeBudget,
            ..
        }))
    ));
    assert_eq!(
        provider.requests().len(),
        1,
        "waiting cannot buy another provider request"
    );
}
