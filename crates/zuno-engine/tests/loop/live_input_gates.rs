use super::*;
use zuno_types::admission::InputReceiptState;
use zuno_types::execution::{
    InputTriggerKind, SessionPauseReason, SessionReadiness, SessionScheduling,
};

const DEFERRED_ID: &str = "msg_protected_steer";
const DEFERRED_TEXT: &str = "Deferred input must not appear in the active provider request.";

fn fixture() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection.execute_batch(&format!(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
             VALUES('project-gate','/fixture',1,1,'[]');
             INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
             VALUES('{SESSION_ID}','project-gate','gate','/fixture','gate','1',1,1);"
        )).expect("fixture session");
        put_user(
            &connection,
            "msg_original",
            10,
            "Finish only the original input.",
        );
    }
    bind_loop_cycle(&pool);
    pool
}

fn set_gate(pool: &Arc<Pool>, readiness: SessionReadiness) {
    let store = zuno_db::session_execution::SessionExecutionStore::new(pool.clone());
    let state = store
        .get(SESSION_ID)
        .expect("execution")
        .expect("seeded state");
    store
        .set_scheduling(
            SESSION_ID,
            state.revision,
            SessionScheduling {
                readiness,
                ..Default::default()
            },
            20,
        )
        .expect("install gate");
}

fn stop_scope(pool: &Arc<Pool>) {
    pool.transaction(|tx| {
        let state = zuno_db::session_execution::read_in(tx, SESSION_ID)?.expect("state");
        let cycle = zuno_db::session_work_cycle::SessionWorkCycle {
            session_id: SESSION_ID.to_owned(),
            cycle_id: state.cycle_id.expect("bound cycle"),
            anchor_message_id: Some("msg_original".to_owned()),
            goal_id: None,
            plan_id: None,
            active_turn_id: Some("turn-gated".to_owned()),
            todo_ids: Default::default(),
            resumed_goal_cycles: Default::default(),
            stopped: Some(zuno_db::session_work_cycle::CycleStop {
                turn_id: Some("turn-gated".to_owned()),
                input_id: Some("msg_original".to_owned()),
                user_cancelled: true,
                at_ms: 20,
            }),
            scheduling: None,
        };
        zuno_db::session_work_cycle::save_in(tx, &cycle, 20)
    })
    .expect("stop scope while leaving scheduling Ready");
}

fn steer(input: &zuno_db::inbox::SessionInput) -> SoftInterruptMessage {
    SoftInterruptMessage {
        input_id: Some(input.id.clone()),
        revision: Some(input.revision),
        content: DEFERRED_TEXT.to_owned(),
        images: Vec::new(),
        attachments: Vec::new(),
        urgent: false,
        source: SoftInterruptSource::User,
    }
}

async fn drive(
    pool: &Arc<Pool>,
    inbox: &SessionInbox,
    input: &zuno_db::inbox::SessionInput,
    turn_id: &str,
) -> (Vec<TurnEvent>, Vec<CompletionRequest>) {
    drive_message(pool, inbox, steer(input), turn_id).await
}

async fn drive_message(
    pool: &Arc<Pool>,
    inbox: &SessionInbox,
    message: SoftInterruptMessage,
    turn_id: &str,
) -> (Vec<TurnEvent>, Vec<CompletionRequest>) {
    let runs = SessionRunRegistry::new();
    let guard = runs.begin_turn(SESSION_ID).expect("live lease");
    runs.queue_soft_interrupt(SESSION_ID, message)
        .expect("steer");
    let provider = Arc::new(FakeProvider::new(vec![ScriptedResponse::complete(vec![
        StreamEvent::TextDelta("Original work finished.".to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]));
    let providers = registry(&provider);
    let resolver = FakeResolver;
    let dispatcher = FakeDispatcher::default();
    let mut connection = pool.get().expect("turn connection");
    let (sender, receiver) = event_channel();
    let turn = run_turn(
        request(turn_id),
        TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, inbox),
        sender,
    );
    let (outcome, events) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(turn, collect_events(receiver))
    })
    .await
    .expect("gated steer must not spin or hang");
    assert!(matches!(
        outcome.expect("turn"),
        TurnOutcome::Completed { .. }
    ));
    (events, provider.requests())
}

async fn rejected_live_input(readiness: Option<SessionReadiness>, gate_at_promotion: bool) {
    let pool = fixture();
    if let Some(readiness) = readiness {
        set_gate(&pool, readiness);
    } else if !gate_at_promotion {
        stop_scope(&pool);
    }
    if gate_at_promotion {
        pool.get().unwrap().execute_batch(&format!(
            "CREATE TRIGGER close_gate_on_promotion AFTER UPDATE OF state ON session_input
             WHEN NEW.id='{DEFERRED_ID}' AND NEW.state='promoted'
             BEGIN
               UPDATE session_execution_state SET phase='paused',
                 scheduling='{{\"readiness\":{{\"kind\":\"paused\",\"reason\":\"authentication\"}}}}',
                 revision=revision+1 WHERE session_id=NEW.session_id;
             END;"
        )).expect("race fixture closes the gate after promotion");
    }
    assert_deferred_then_usable(pool, gate_at_promotion).await;
}

async fn assert_deferred_then_usable(pool: Arc<Pool>, gate_at_promotion: bool) {
    let inbox = SessionInbox::new(pool.clone());
    let input = inbox
        .admit(
            NewSessionInput::new(
                DEFERRED_ID,
                SESSION_ID,
                json!({"kind":"tuiPrompt","text":DEFERRED_TEXT}),
                InputDelivery::Steer,
                21,
            )
            .with_trigger_kind(InputTriggerKind::User),
        )
        .expect("delivery remains admitted");
    assert_eq!(
        inbox.wake_admission(&input).unwrap(),
        zuno_types::execution::WakeAdmission::Admit
    );

    let (events, requests) = drive(&pool, &inbox, &input, "turn-gated").await;
    assert!(
        !events.iter().any(|event| matches!(event, TurnEvent::InputConsumed { input_id, .. } if input_id == DEFERRED_ID)),
        "a protected live input emitted InputConsumed"
    );
    assert!(
        requests
            .iter()
            .all(|request| !format!("{request:?}").contains(DEFERRED_TEXT)),
        "the protected input reached a provider request"
    );
    let retained = inbox.get(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
    assert!(
        retained.state.is_pending(),
        "refused input must remain available: {retained:?}"
    );
    assert_eq!(retained.prompt, input.prompt);
    assert_eq!(retained.admitted_sequence, input.admitted_sequence);
    assert_eq!(
        retained.cycle_id, input.cycle_id,
        "a refusal must not bind execution"
    );
    assert_eq!(retained.trigger_kind, input.trigger_kind);
    let receipt = zuno_db::input_receipt::InputReceiptStore::new(pool.clone())
        .get(SESSION_ID, DEFERRED_ID)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, InputReceiptState::Admitted);
    assert!(receipt.turn_id.is_none());
    assert!(receipt.applied_at.is_none());
    assert!(
        MessageStore::new(&pool.get().unwrap())
            .message(DEFERRED_ID)
            .is_err()
    );

    // Resolve the fixture's gate and let a future native delivery use the same
    // durable input. No user re-admission or new message identity is involved.
    if gate_at_promotion {
        pool.get()
            .unwrap()
            .execute_batch("DROP TRIGGER close_gate_on_promotion;")
            .unwrap();
    }
    set_gate(&pool, SessionReadiness::Ready);
    pool.transaction(|tx| {
        let mut state = zuno_db::session_execution::read_in(tx, SESSION_ID)?.unwrap();
        state.cycle_id = Some("future-native-activation".to_owned());
        zuno_db::session_execution::update_in(tx, state.revision, state).map(|_| ())
    })
    .unwrap();
    let (events, requests) = drive(&pool, &inbox, &retained, "turn-future").await;
    assert_eq!(events.iter().filter(|event| matches!(event, TurnEvent::InputConsumed { input_id, .. } if input_id == DEFERRED_ID)).count(), 1);
    assert!(
        requests
            .iter()
            .any(|request| format!("{request:?}").contains(DEFERRED_TEXT))
    );
    assert_eq!(
        inbox.get(SESSION_ID, DEFERRED_ID).unwrap().unwrap().state,
        SubmissionState::Consumed
    );
}

#[tokio::test]
async fn live_input_authentication_gate_defers_application() {
    rejected_live_input(
        Some(SessionReadiness::Paused {
            reason: SessionPauseReason::Authentication,
        }),
        false,
    )
    .await;
}

#[tokio::test]
async fn live_input_turn_budget_gate_defers_application() {
    rejected_live_input(
        Some(SessionReadiness::Paused {
            reason: SessionPauseReason::TurnBudget,
        }),
        false,
    )
    .await;
}

#[tokio::test]
async fn live_input_blocked_gate_defers_application() {
    rejected_live_input(
        Some(SessionReadiness::Paused {
            reason: SessionPauseReason::Blocked,
        }),
        false,
    )
    .await;
}

#[tokio::test]
async fn live_input_human_wait_defers_application() {
    rejected_live_input(
        Some(SessionReadiness::WaitingHuman {
            request_id: "human-fixture".to_owned(),
        }),
        false,
    )
    .await;
}

#[tokio::test]
async fn live_input_stopped_scope_defers_application_even_when_ready() {
    rejected_live_input(None, false).await;
}

#[tokio::test]
async fn live_input_rechecks_gate_after_promotion_before_consumption() {
    rejected_live_input(None, true).await;
}

#[test]
fn stale_live_claim_cannot_release_a_newer_promotion() {
    let pool = fixture();
    set_gate(
        &pool,
        SessionReadiness::Paused {
            reason: SessionPauseReason::Authentication,
        },
    );
    let inbox = SessionInbox::new(pool.clone());
    inbox
        .admit(NewSessionInput::new(
            DEFERRED_ID,
            SESSION_ID,
            json!({"kind":"tuiPrompt","text":DEFERRED_TEXT}),
            InputDelivery::Steer,
            21,
        ))
        .unwrap();
    let old = inbox.promote_id(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
    inbox.recover_promoted(SESSION_ID, DEFERRED_ID).unwrap();
    let newer = inbox.promote_id(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
    pool.transaction(|tx| {
        assert_eq!(
            zuno_db::session_wake::model_application_admission_in(tx, SESSION_ID, Some(&old))?,
            zuno_types::execution::WakeAdmission::Reject,
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(inbox.get(SESSION_ID, DEFERRED_ID).unwrap(), Some(newer));
}

#[test]
fn refused_live_claim_release_is_in_the_application_transaction() {
    let pool = fixture();
    set_gate(
        &pool,
        SessionReadiness::Paused {
            reason: SessionPauseReason::TurnBudget,
        },
    );
    let inbox = SessionInbox::new(pool.clone());
    inbox
        .admit(NewSessionInput::new(
            DEFERRED_ID,
            SESSION_ID,
            json!({"kind":"tuiPrompt","text":DEFERRED_TEXT}),
            InputDelivery::Steer,
            21,
        ))
        .unwrap();
    let claim = inbox.promote_id(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
    {
        let connection = pool.get().unwrap();
        let tx = open::immediate_transaction(&connection).unwrap();
        assert_eq!(
            zuno_db::session_wake::model_application_admission_in(&tx, SESSION_ID, Some(&claim))
                .unwrap(),
            zuno_types::execution::WakeAdmission::Reject,
        );
        assert!(
            zuno_db::inbox::read_in(&tx, SESSION_ID, DEFERRED_ID)
                .unwrap()
                .unwrap()
                .state
                .is_pending()
        );
        // Abandon the caller transaction: the helper must not have committed a
        // release on another connection independently of model application.
    }
    assert_eq!(inbox.get(SESSION_ID, DEFERRED_ID).unwrap(), Some(claim));
}

#[tokio::test]
async fn untracked_live_user_input_is_preserved_for_future_native_delivery() {
    let pool = fixture();
    set_gate(
        &pool,
        SessionReadiness::Paused {
            reason: SessionPauseReason::Authentication,
        },
    );
    let inbox = SessionInbox::new(pool.clone());
    let message = SoftInterruptMessage {
        input_id: None,
        revision: None,
        content: DEFERRED_TEXT.to_owned(),
        images: Vec::new(),
        attachments: Vec::new(),
        urgent: false,
        source: SoftInterruptSource::User,
    };
    let (events, requests) = drive_message(&pool, &inbox, message, "turn-untracked").await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::InputConsumed { .. }))
    );
    assert!(
        requests
            .iter()
            .all(|request| !format!("{request:?}").contains(DEFERRED_TEXT))
    );
    let pending = inbox.pending(SESSION_ID).unwrap();
    assert_eq!(pending.len(), 1);
    let retained = &pending[0];
    assert_eq!(retained.prompt["parts"][0]["text"], DEFERRED_TEXT);
    assert_eq!(
        zuno_db::inbox::DurableInputKind::classify(&retained.prompt),
        Some(zuno_db::inbox::DurableInputKind::HostMessage),
    );
    assert!(
        MessageStore::new(&pool.get().unwrap())
            .message(&retained.id)
            .is_err()
    );
    assert_eq!(
        zuno_db::input_receipt::InputReceiptStore::new(pool.clone())
            .get(SESSION_ID, &retained.id)
            .unwrap()
            .unwrap()
            .state,
        InputReceiptState::Admitted,
    );
    set_gate(&pool, SessionReadiness::Ready);
    let (events, requests) = drive(&pool, &inbox, retained, "turn-delayed-untracked").await;
    assert_eq!(events.iter().filter(|event| matches!(event, TurnEvent::InputConsumed { input_id, .. } if input_id == &retained.id)).count(), 1);
    assert!(
        requests
            .iter()
            .any(|request| format!("{request:?}").contains(DEFERRED_TEXT))
    );
    assert!(inbox.pending(SESSION_ID).unwrap().is_empty());
}

#[test]
fn live_report_authority_is_rechecked_after_its_promotion() {
    let pool = fixture();
    let inbox = SessionInbox::new(pool.clone());
    inbox.admit(NewSessionInput::new(
        DEFERRED_ID, SESSION_ID,
        json!({"kind":"backgroundExecutionReport","executionID":"old-execution","text":DEFERRED_TEXT}),
        InputDelivery::Steer, 21,
    ).with_trigger_kind(InputTriggerKind::Automatic).with_cycle_id(Some("loop-cycle"))).unwrap();
    let claim = inbox.promote_id(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
    pool.transaction(|tx| {
        let mut state = zuno_db::session_execution::read_in(tx, SESSION_ID)?.unwrap();
        state.cycle_id = Some("independent-user-cycle".to_owned());
        zuno_db::session_execution::update_in(tx, state.revision, state)?;
        assert_eq!(
            zuno_db::session_wake::model_application_admission_in(tx, SESSION_ID, Some(&claim))?,
            zuno_types::execution::WakeAdmission::Reject,
        );
        Ok(())
    })
    .unwrap();
    assert!(
        inbox
            .get(SESSION_ID, DEFERRED_ID)
            .unwrap()
            .unwrap()
            .state
            .is_pending()
    );
}

fn goal_scope(pool: &Arc<Pool>, status: &str) {
    // Engine fixtures exercise the published Goal key/status columns; Goal
    // lifecycle transitions themselves are covered by the Goal crate.
    pool.get()
        .unwrap()
        .execute_batch(
            "CREATE TABLE goal(
           session_id TEXT PRIMARY KEY NOT NULL,
           goal_id TEXT NOT NULL,
           status TEXT NOT NULL CHECK(status IN
             ('active','paused','blocked','usage_limited','budget_limited','complete','cancelled'))
         );",
        )
        .unwrap();
    pool.transaction(|tx| {
        tx.execute(
            "INSERT INTO goal(session_id,goal_id,status) VALUES(?1,'goal-scope',?2)",
            (SESSION_ID, status),
        )
        .map_err(open::map_error)?;
        let state = zuno_db::session_execution::read_in(tx, SESSION_ID)?.unwrap();
        let scope = zuno_db::session_work_cycle::SessionWorkCycle {
            session_id: SESSION_ID.to_owned(),
            cycle_id: state.cycle_id.unwrap(),
            anchor_message_id: Some("msg_original".to_owned()),
            goal_id: Some("goal-scope".to_owned()),
            plan_id: None,
            active_turn_id: Some("turn-gated".to_owned()),
            todo_ids: Default::default(),
            resumed_goal_cycles: ["old-report-cycle".to_owned()].into(),
            stopped: None,
            scheduling: None,
        };
        zuno_db::session_work_cycle::save_in(tx, &scope, 20)?;
        let original = zuno_db::session_work_cycle::SessionWorkCycle {
            cycle_id: "old-report-cycle".to_owned(),
            resumed_goal_cycles: Default::default(),
            ..scope
        };
        zuno_db::session_work_cycle::save_in(tx, &original, 19)
    })
    .unwrap();
}

async fn inactive_goal(status: &str) {
    let pool = fixture();
    goal_scope(&pool, status);
    assert_deferred_then_usable(pool.clone(), false).await;
    let stored: String = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT status FROM goal WHERE session_id=?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, status,
        "future independent input must not resume the old Goal"
    );
}

#[tokio::test]
async fn live_goal_complete_defers_input_before_host_stop() {
    inactive_goal("complete").await;
}

#[tokio::test]
async fn live_goal_blocked_defers_input_before_host_stop() {
    inactive_goal("blocked").await;
}

#[tokio::test]
async fn live_goal_paused_defers_input_before_host_stop() {
    inactive_goal("paused").await;
}

#[tokio::test]
async fn live_goal_changed_after_promotion_is_rechecked() {
    let pool = fixture();
    goal_scope(&pool, "active");
    pool.get()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER close_gate_on_promotion AFTER UPDATE OF state ON session_input
         WHEN NEW.id='{DEFERRED_ID}' AND NEW.state='promoted'
         BEGIN UPDATE goal SET status='complete' WHERE session_id=NEW.session_id; END;"
        ))
        .unwrap();
    assert_deferred_then_usable(pool, true).await;
}

#[tokio::test]
async fn live_goal_report_alias_does_not_authorize_user_input() {
    let pool = fixture();
    goal_scope(&pool, "active");
    let inbox = SessionInbox::new(pool.clone());
    let input = inbox
        .admit(
            NewSessionInput::new(
                DEFERRED_ID,
                SESSION_ID,
                json!({"kind":"tuiPrompt","text":DEFERRED_TEXT}),
                InputDelivery::Steer,
                21,
            )
            .with_trigger_kind(InputTriggerKind::User)
            .with_cycle_id(Some("old-report-cycle")),
        )
        .unwrap();
    let (events, requests) = drive(&pool, &inbox, &input, "turn-alias-user").await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::InputConsumed { .. }))
    );
    assert!(
        requests
            .iter()
            .all(|request| !format!("{request:?}").contains(DEFERRED_TEXT))
    );
    assert!(
        inbox
            .get(SESSION_ID, DEFERRED_ID)
            .unwrap()
            .unwrap()
            .state
            .is_pending()
    );
}

#[tokio::test]
async fn live_goal_report_alias_still_delivers_an_authorized_report() {
    let pool = fixture();
    goal_scope(&pool, "active");
    let inbox = SessionInbox::new(pool.clone());
    let input = inbox.admit(NewSessionInput::new(
        DEFERRED_ID, SESSION_ID,
        json!({"kind":"backgroundExecutionReport","executionID":"resumed-job","text":DEFERRED_TEXT}),
        InputDelivery::Steer, 21,
    ).with_trigger_kind(InputTriggerKind::Automatic).with_cycle_id(Some("old-report-cycle"))).unwrap();
    let mut message = steer(&input);
    message.source = SoftInterruptSource::BackgroundTask;
    let (events, requests) = drive_message(&pool, &inbox, message, "turn-report-alias").await;
    assert_eq!(events.iter().filter(|event| matches!(event, TurnEvent::InputConsumed { input_id, .. } if input_id == DEFERRED_ID)).count(), 1);
    assert!(
        requests
            .iter()
            .any(|request| format!("{request:?}").contains(DEFERRED_TEXT))
    );
    assert_eq!(
        inbox
            .get(SESSION_ID, DEFERRED_ID)
            .unwrap()
            .unwrap()
            .cycle_id,
        input.cycle_id
    );
}

#[tokio::test]
async fn live_goal_missing_does_not_grant_old_scope_execution() {
    let pool = fixture();
    goal_scope(&pool, "active");
    pool.get()
        .unwrap()
        .execute("DELETE FROM goal WHERE session_id=?1", [SESSION_ID])
        .unwrap();
    assert_deferred_then_usable(pool, false).await;
}

#[tokio::test]
async fn live_goal_replaced_by_an_active_goal_does_not_grant_old_scope_execution() {
    let pool = fixture();
    goal_scope(&pool, "active");
    pool.get()
        .unwrap()
        .execute(
            "UPDATE goal SET goal_id='replacement-goal' WHERE session_id=?1",
            [SESSION_ID],
        )
        .unwrap();
    assert_deferred_then_usable(pool, false).await;
}

#[tokio::test]
async fn live_user_application_binds_current_cycle_without_changing_admitted_trigger() {
    for trigger in [InputTriggerKind::Legacy, InputTriggerKind::User] {
        let pool = fixture();
        let inbox = SessionInbox::new(pool.clone());
        let input = inbox
            .admit(
                NewSessionInput::new(
                    DEFERRED_ID,
                    SESSION_ID,
                    json!({"kind":"tuiPrompt","text":DEFERRED_TEXT}),
                    InputDelivery::Steer,
                    21,
                )
                .with_trigger_kind(trigger),
            )
            .unwrap();
        assert!(input.cycle_id.is_none());
        drive(&pool, &inbox, &input, "turn-cycle-binding").await;
        let consumed = inbox.get(SESSION_ID, DEFERRED_ID).unwrap().unwrap();
        assert_eq!(consumed.state, SubmissionState::Consumed);
        assert_eq!(consumed.cycle_id.as_deref(), Some("loop-cycle"));
        assert_eq!(consumed.trigger_kind, trigger);
        assert_eq!(
            consumed.revision,
            input.revision + 2,
            "only promotion and consumption advance revision"
        );
        let receipt = zuno_db::input_receipt::InputReceiptStore::new(pool.clone())
            .get(SESSION_ID, DEFERRED_ID)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.turn_id.as_deref(), Some("turn-cycle-binding"));
    }
}

#[test]
fn live_control_cycle_binding_rolls_back_with_the_application_transaction() {
    let pool = fixture();
    let inbox = SessionInbox::new(pool.clone());
    inbox
        .admit(
            NewSessionInput::new(
                "control-cycle-binding",
                SESSION_ID,
                json!({"kind":"sessionControl","control":"resume_work"}),
                InputDelivery::Queue,
                21,
            )
            .with_trigger_kind(InputTriggerKind::UserControl),
        )
        .unwrap();
    let claim = inbox
        .promote_id(SESSION_ID, "control-cycle-binding")
        .unwrap()
        .unwrap();
    {
        let connection = pool.get().unwrap();
        let tx = open::immediate_transaction(&connection).unwrap();
        assert_eq!(
            zuno_db::session_wake::model_application_admission_in(&tx, SESSION_ID, Some(&claim))
                .unwrap(),
            zuno_types::execution::WakeAdmission::Admit,
        );
        let bound = zuno_db::inbox::read_in(&tx, SESSION_ID, &claim.id)
            .unwrap()
            .unwrap();
        assert_eq!(bound.cycle_id.as_deref(), Some("loop-cycle"));
        assert_eq!(bound.revision, claim.revision);
        assert_eq!(bound.trigger_kind, InputTriggerKind::UserControl);
    }
    assert_eq!(inbox.get(SESSION_ID, &claim.id).unwrap(), Some(claim));
}
