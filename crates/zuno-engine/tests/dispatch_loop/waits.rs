use super::*;
use zuno_engine::r#loop::{AvailableTools, DispatchRequest, PreparedToolDispatch};
use zuno_engine::state::TurnStateScope;
use zuno_engine::wait::{WaitCompletion, publish_sqlite_completion};
use zuno_types::identity::{
    CompletionId, InvocationId, OperationId, PrincipalScope, TurnId, WaitId,
};
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

struct DeferredDispatcher {
    inner: ToolRegistryDispatcher,
    deferred: Mutex<Vec<WaitRef>>,
}

#[async_trait]
impl ToolDispatcher for DeferredDispatcher {
    fn available_tools(&self) -> AvailableTools {
        self.inner.available_tools()
    }

    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.id == "wait" {
            let reference = WaitRef {
                id: WaitId::new("external-result").unwrap(),
                turn_id: TurnId::new("turn-bounded").unwrap(),
                invocation_id: InvocationId::new(&request.call.id).unwrap(),
                arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
                target: WaitTarget::Operation {
                    operation_id: OperationId::new("once").unwrap(),
                },
                continuation: WaitContinuation::CurrentTurn,
            };
            self.deferred.lock().unwrap().push(reference.clone());
            PreparedToolDispatch::Pending(reference)
        } else {
            self.inner.prepare(request).await
        }
    }
}

fn deferred(order: &Arc<Mutex<Vec<String>>>) -> DeferredDispatcher {
    DeferredDispatcher {
        inner: dispatcher(vec![Arc::new(SequentialTool {
            active: AtomicUsize::new(0),
            order: Arc::clone(order),
        })]),
        deferred: Mutex::new(Vec::new()),
    }
}

fn scope() -> TurnStateScope {
    TurnStateScope {
        owner: PrincipalScope::local().owner(),
        session_id: SESSION_ID.to_owned(),
    }
}

fn completion(reference: WaitRef) -> WaitCompletion {
    WaitCompletion {
        id: CompletionId::new("completed-once").unwrap(),
        reference,
        result: zuno_engine::r#loop::ToolDispatchResult::success(ToolOutput::text(
            "external operation",
            "authoritative external result",
        )),
    }
}

#[tokio::test]
async fn a_wait_releases_the_driver_and_consumes_its_original_result_once_before_remaining_calls() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wait.db");
    let mut connection = seed_connection(open::open_at(&path).unwrap());
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_dispatcher = deferred(&order);
    let mut script =
        provider_events(&[("before", "before"), ("wait", "remote"), ("after", "after")]);
    script[0].insert(0, usage());
    let final_response = script.pop().unwrap();
    let provider = Arc::new(ScriptedProvider::new(script));
    let (outcome, events) = advance(
        &mut connection,
        provider.clone(),
        &first_dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Waiting { checkpoint, waits } = outcome.unwrap() else {
        panic!("durable wait")
    };
    assert_eq!(*order.lock().unwrap(), ["before"]);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(first_dispatcher.deferred.lock().unwrap().len(), 1);
    assert!(!events.iter().any(|event| matches!(event,
        TurnEvent::ToolDispatchCompleted { call_id, .. } if call_id == "wait"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::StepCompleted { .. }))
    );
    let original = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap()
        .into_iter()
        .flat_map(|message| message.parts)
        .find(|part| part.data.get("callID").and_then(Value::as_str) == Some("wait"))
        .unwrap();
    assert_eq!(original.data["state"]["status"], "pending");
    assert!(original.data["state"].get("output").is_none());
    assert_eq!(original.data["state"]["waitRef"], json!(waits[0]));
    let (still_waiting, events) = advance(
        &mut connection,
        provider.clone(),
        &first_dispatcher,
        request().resume(checkpoint.clone()),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert_eq!(
        still_waiting.unwrap(),
        AdvanceOutcome::Waiting {
            checkpoint: checkpoint.clone(),
            waits: waits.clone(),
        }
    );
    assert!(events.is_empty());
    assert_eq!(provider.requests().len(), 1);

    let fact = completion(waits[0].clone());
    let receipt = publish_sqlite_completion(&mut connection, &scope(), &fact).unwrap();
    let repeated = publish_sqlite_completion(&mut connection, &scope(), &fact).unwrap();
    assert_eq!(receipt, repeated);
    let mut changed = fact.clone();
    changed.result.output.output = "a different result".to_owned();
    assert!(publish_sqlite_completion(&mut connection, &scope(), &changed).is_err());
    drop(first_dispatcher);
    drop(provider);
    drop(connection);

    let mut connection = open::open_at(&path).unwrap();
    migration::apply(&mut connection).unwrap();
    let second_dispatcher = deferred(&order);
    let provider = Arc::new(ScriptedProvider::new(vec![final_response]));
    let budget = Arc::new(BudgetProbe::default());
    let consume_request = request().resume(checkpoint);
    let (consumed, _) = advance(
        &mut connection,
        provider.clone(),
        &second_dispatcher,
        consume_request.clone(),
        budget.clone(),
    )
    .await;
    let AdvanceOutcome::Progressed {
        checkpoint: consumed_checkpoint,
    } = consumed.unwrap()
    else {
        panic!("consumption is a durable boundary")
    };
    assert!(provider.requests().is_empty());
    assert_eq!(*order.lock().unwrap(), ["before"]);
    // Simulate losing the commit acknowledgement: the same source checkpoint
    // returns the original next checkpoint without rewriting a result.
    let (repeated, _) = advance(
        &mut connection,
        provider.clone(),
        &second_dispatcher,
        consume_request,
        budget.clone(),
    )
    .await;
    assert_eq!(
        repeated.unwrap(),
        AdvanceOutcome::Progressed {
            checkpoint: consumed_checkpoint.clone()
        }
    );
    let (done, _) = advance(
        &mut connection,
        provider.clone(),
        &second_dispatcher,
        request().resume(consumed_checkpoint),
        budget.clone(),
    )
    .await;
    assert!(matches!(
        done.unwrap(),
        AdvanceOutcome::Completed { steps: 2, .. }
    ));
    assert_eq!(*order.lock().unwrap(), ["before", "after"]);
    assert!(second_dispatcher.deferred.lock().unwrap().is_empty());
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(budget.0.lock().unwrap()[0].1, 127);
    assert_eq!(budget.0.lock().unwrap()[0].2, 3);
    let result = MessageStore::new(&connection).part(&original.id).unwrap();
    assert_eq!(result.data["state"]["status"], "completed");
    assert_eq!(
        result.data["state"]["output"],
        "authoritative external result"
    );
    assert_eq!(result.data["completionID"], "completed-once");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM event WHERE aggregate_id=?1 AND type='runtime.wait.consumed.1'",
            [SESSION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn result_and_consumption_roll_back_together_when_checkpoint_commit_fails() {
    let mut connection = seeded();
    let order = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = deferred(&order);
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[(
        "wait", "remote",
    )])));
    let (outcome, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    let AdvanceOutcome::Waiting { checkpoint, waits } = outcome.unwrap() else {
        panic!("wait")
    };
    publish_sqlite_completion(&mut connection, &scope(), &completion(waits[0].clone())).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_consumption_checkpoint BEFORE INSERT ON event
         WHEN NEW.type='runtime.driver.advance.1' AND json_extract(NEW.data,'$.state.phase')='checkpointed'
         BEGIN SELECT RAISE(ABORT,'injected checkpoint failure'); END;",
    ).unwrap();
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint.clone()),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(result.is_err());
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .unwrap();
    let part = history
        .iter()
        .flat_map(|message| &message.parts)
        .find(|part| part.kind == PartKind::Tool)
        .unwrap();
    assert_eq!(part.data["state"]["status"], "pending");
    assert!(part.data["state"].get("output").is_none());
    let consumed: i64 = connection
        .query_row(
            "SELECT count(*) FROM event WHERE type='runtime.wait.consumed.1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(consumed, 0);
    assert_eq!(provider.requests().len(), 1);
    connection
        .execute_batch("DROP TRIGGER reject_consumption_checkpoint")
        .unwrap();
    let (retried, _) = advance(
        &mut connection,
        provider,
        &dispatcher,
        request().resume(checkpoint),
        Arc::new(NoopBudgetPolicy),
    )
    .await;
    assert!(matches!(
        retried.unwrap(),
        AdvanceOutcome::Progressed { .. }
    ));
}

struct OneMinuteBudget;

#[async_trait]
impl TurnBudgetPolicy for OneMinuteBudget {
    async fn after_response(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(if snapshot.elapsed_seconds >= 60 {
            BudgetDecision::stop_time("the original turn allowance has elapsed")
        } else {
            BudgetDecision::Continue
        })
    }
}

#[tokio::test]
async fn waiting_time_does_not_authorize_remaining_tools_after_the_original_budget_expires() {
    let mut connection = seeded();
    let order = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = deferred(&order);
    let provider = Arc::new(ScriptedProvider::new(provider_events(&[
        ("before", "before"),
        ("wait", "remote"),
        ("after", "after"),
    ])));
    let budget: Arc<dyn TurnBudgetPolicy> = Arc::new(OneMinuteBudget);
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request(),
        budget.clone(),
    )
    .await;
    let AdvanceOutcome::Waiting { checkpoint, waits } = result.unwrap() else {
        panic!("wait")
    };
    // Advance the persisted database-clock anchor beyond the allowance without
    // a slow or scheduler-sensitive wall-clock sleep.
    connection
        .execute(
            "UPDATE event SET data=json_set(data,'$.state.checkpoint.startedAtMs',
           CAST(unixepoch('subsec') * 1000 AS INTEGER)-61000)
         WHERE aggregate_id=?1 AND seq=?2 AND type='runtime.driver.advance.1'",
            (SESSION_ID, checkpoint.sequence()),
        )
        .unwrap();
    publish_sqlite_completion(&mut connection, &scope(), &completion(waits[0].clone())).unwrap();
    let (consumed, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        budget.clone(),
    )
    .await;
    let AdvanceOutcome::Progressed { checkpoint } = consumed.unwrap() else {
        panic!("consumed")
    };
    let (result, _) = advance(
        &mut connection,
        provider.clone(),
        &dispatcher,
        request().resume(checkpoint),
        budget,
    )
    .await;
    assert!(matches!(
        result,
        Err(AdvanceError::Turn(TurnError::BudgetLimited { .. }))
    ));
    assert_eq!(*order.lock().unwrap(), ["before"]);
    assert_eq!(provider.requests().len(), 1);
}
