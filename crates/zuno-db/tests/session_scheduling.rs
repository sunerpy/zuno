use std::sync::Arc;

use serde_json::json;
use zuno_db::completion_delivery::CompletionDeliveryStore;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_db::session_execution::{
    SessionExecutionStore, admit_wake_in, clear_matching_wait_in, set_completed_in, set_paused_in,
    set_scheduling_in, set_waiting_in,
};
use zuno_db::{Pool, migration};
use zuno_error::DbError;
use zuno_paths::DbLocation;
use zuno_types::execution::{
    CollaborationMode, CompletionEnvelope, CompletionSource, ContinuationToken,
    DraftReviewRiskAcceptance, InputTriggerKind, SessionExecutionPhase, SessionExecutionState,
    SessionPauseReason, SessionReadiness, SessionScheduling, SessionWaitReference,
    SessionWakeSignal, TurnExecutionIdentity, WakeAdmission,
};

const SESSION: &str = "ordinary-session";

#[path = "session_scheduling/wakes.rs"]
mod wakes;

fn initialized(location: &DbLocation) -> Arc<Pool> {
    let pool = Arc::new(Pool::open(location).expect("pool"));
    let mut connection = pool.get().expect("connection");
    migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes)
             VALUES ('project','/workspace',1,1,'[]');
             INSERT INTO session
               (id,project_id,slug,directory,title,version,time_created,time_updated)
             VALUES ('ordinary-session','project','ordinary','/workspace','Ordinary','test',1,1);
             INSERT INTO work_plan
               (session_id,id,goal_id,revision,title,steps,time_created,time_updated)
             VALUES ('ordinary-session','unfinished-plan',NULL,4,'Await a decision',
               '[{\"id\":\"remaining\",\"title\":\"Authorized work\",\"status\":\"in_progress\"}]',
               1,1);",
        )
        .expect("ordinary session with an unfinished Plan and no Goal/request");
    drop(connection);
    pool
}

fn seeded(pool: &Arc<Pool>) -> SessionExecutionState {
    let store = SessionExecutionStore::new(Arc::clone(pool));
    let identity =
        TurnExecutionIdentity::new("build", "provider", "model").with_reasoning(Some("high"));
    let mut state = store
        .seed(SESSION, CollaborationMode::Work, Some(identity.clone()), 10)
        .expect("seed");
    state.authorized_plan_id = Some("unfinished-plan".to_owned());
    state.authorized_plan_revision = Some(4);
    state.handoff_plan_id = Some("unfinished-plan".to_owned());
    state.handoff_plan_revision = Some(4);
    state.draft_review_risk = Some(DraftReviewRiskAcceptance {
        review_id: "review-1".to_owned(),
        review_revision: 2,
        reason: "explicit acceptance".to_owned(),
        time_accepted: 11,
    });
    state.cycle_id = Some("origin-cycle".to_owned());
    state.phase = SessionExecutionPhase::Running;
    state.continuation = Some(ContinuationToken {
        cycle_id: "origin-cycle".to_owned(),
        identity,
        mode: CollaborationMode::Work,
        plan_id: Some("unfinished-plan".to_owned()),
        plan_revision: Some(4),
        context_epoch: 7,
        anchor_message_id: Some("anchor-message".to_owned()),
    });
    state.scheduling = Some(SessionScheduling {
        progress_fingerprint: Some("sha256:unchanged".to_owned()),
        unchanged_progress_count: 3,
        ..SessionScheduling::default()
    });
    state.time_updated = 12;
    store
        .update(state.revision, state)
        .expect("running session")
}

fn assert_authority_unchanged(before: &SessionExecutionState, after: &SessionExecutionState) {
    let mut expected = before.clone();
    expected.revision = after.revision;
    expected.phase = after.phase;
    expected.scheduling = after.scheduling.clone();
    expected.time_updated = after.time_updated;
    assert_eq!(&expected, after);
    assert_eq!(
        after
            .scheduling
            .as_ref()
            .expect("scheduling")
            .progress_fingerprint,
        Some("sha256:unchanged".to_owned())
    );
    assert_eq!(
        after
            .scheduling
            .as_ref()
            .expect("scheduling")
            .unchanged_progress_count,
        3
    );
}

fn external_wait(source: &str, cycle: &str) -> SessionWaitReference {
    SessionWaitReference::External {
        source_id: source.to_owned(),
        origin_cycle_id: cycle.to_owned(),
    }
}

#[test]
fn ordinary_session_pause_survives_reopen_new_callback_and_status_query() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let location = DbLocation::File(directory.path().join("scheduling.db"));
    let paused = {
        let pool = initialized(&location);
        let state = seeded(&pool);
        pool.transaction(|tx| {
            set_paused_in(
                tx,
                SESSION,
                state.revision,
                SessionPauseReason::NoProgress,
                20,
            )
        })
        .expect("pause without a Goal")
    };
    let pool = Arc::new(Pool::open(&location).expect("reopen"));
    migration::apply(&mut pool.get().expect("connection")).expect("validate reopened database");
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    assert_eq!(store.get(SESSION).expect("reopened"), Some(paused.clone()));

    let completions = CompletionDeliveryStore::new(Arc::clone(&pool));
    completions
        .publish(
            CompletionEnvelope {
                source_key: "background:new-callback:1".to_owned(),
                source: CompletionSource::BackgroundExecution,
                terminal_revision: 1,
                parent_session_id: SESSION.to_owned(),
                cycle_id: None,
                payload: json!({"text":"terminal result"}),
            },
            30,
        )
        .expect("durably receive a new completion");
    completions
        .claim_callback(
            "background:new-callback:1",
            NewSessionInput::new(
                "new-callback-input",
                SESSION,
                json!({"text":"terminal result"}),
                InputDelivery::Queue,
                31,
            )
            .with_source_key("background:new-callback:1")
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(Some("new-callback-cycle")),
            31,
        )
        .expect("persist callback input")
        .expect("callback owner");
    for signal in [
        SessionWakeSignal::Automatic,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Callback,
        SessionWakeSignal::ExternalCompletion {
            source_id: "new-callback".to_owned(),
            origin_cycle_id: "new-callback-cycle".to_owned(),
        },
    ] {
        assert_eq!(
            store.admit_wake(SESSION, &signal, 40).expect("admit wake"),
            WakeAdmission::Reject
        );
        assert_eq!(store.get(SESSION).expect("read"), Some(paused.clone()));
    }
    assert_eq!(
        store
            .admit_wake(SESSION, &SessionWakeSignal::UserQuery, 41)
            .expect("status query"),
        WakeAdmission::Admit
    );
    assert_eq!(
        store.get(SESSION).expect("query keeps gate"),
        Some(paused.clone())
    );
    let connection = pool.get().expect("connection");
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM human_request", [], |row| row
                .get::<_, i64>(0))
            .expect("requests"),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM work_plan WHERE goal_id IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("goal bindings"),
        0
    );
    drop(connection);
    assert_eq!(
        store
            .admit_wake(SESSION, &SessionWakeSignal::ExplicitResume, 42)
            .expect("explicit resume"),
        WakeAdmission::Resume
    );
    let resumed = store.get(SESSION).expect("read").expect("state");
    assert_authority_unchanged(&paused, &resumed);
    assert_eq!(resumed.phase, SessionExecutionPhase::Idle);
    assert_eq!(
        resumed.scheduling.expect("scheduling").readiness,
        SessionReadiness::Ready
    );
}

#[test]
fn exact_human_answer_is_required_and_cannot_lift_a_replacement_request() {
    let pool = initialized(&DbLocation::Memory);
    let state = seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    let wait = SessionWaitReference::Human {
        request_id: "request-1".to_owned(),
    };
    let waiting = pool
        .transaction(|tx| set_waiting_in(tx, SESSION, state.revision, wait.clone(), 20))
        .expect("wait");
    assert_eq!(waiting.phase, SessionExecutionPhase::Waiting);
    assert_authority_unchanged(&state, &waiting);
    for signal in [
        SessionWakeSignal::Automatic,
        SessionWakeSignal::Recovery,
        SessionWakeSignal::Callback,
        SessionWakeSignal::ExplicitResume,
        SessionWakeSignal::UserAnswer {
            request_id: "other-request".to_owned(),
        },
    ] {
        assert_eq!(
            store.admit_wake(SESSION, &signal, 21).expect("wake"),
            WakeAdmission::Reject
        );
        assert_eq!(store.get(SESSION).expect("read"), Some(waiting.clone()));
    }
    assert_eq!(
        store
            .admit_wake(SESSION, &SessionWakeSignal::UserQuery, 22)
            .expect("query"),
        WakeAdmission::Admit
    );
    let ready = pool
        .transaction(|tx| clear_matching_wait_in(tx, SESSION, &wait, 23))
        .expect("matching answer")
        .expect("cleared");
    assert_authority_unchanged(&waiting, &ready);
    assert_eq!(ready.revision, waiting.revision + 1);
    assert!(
        pool.transaction(|tx| clear_matching_wait_in(tx, SESSION, &wait, 24))
            .expect("duplicate answer")
            .is_none()
    );
    let replacement = pool
        .transaction(|tx| {
            set_waiting_in(
                tx,
                SESSION,
                ready.revision,
                SessionWaitReference::Human {
                    request_id: "request-2".to_owned(),
                },
                25,
            )
        })
        .expect("replacement wait");
    assert!(
        pool.transaction(|tx| clear_matching_wait_in(tx, SESSION, &wait, 26))
            .expect("old answer")
            .is_none()
    );
    assert_eq!(store.get(SESSION).expect("read"), Some(replacement));
}

#[test]
fn external_wake_requires_both_source_and_origin_cycle_and_keeps_current_cycle() {
    let pool = initialized(&DbLocation::Memory);
    let state = seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    let wait = external_wait("background-1", "origin-cycle");
    let waiting = pool
        .transaction(|tx| set_waiting_in(tx, SESSION, state.revision, wait.clone(), 20))
        .expect("wait");
    for unrelated in [
        external_wait("background-2", "origin-cycle"),
        external_wait("background-1", "old-cycle"),
        SessionWaitReference::Human {
            request_id: "request-1".to_owned(),
        },
    ] {
        assert!(
            pool.transaction(|tx| clear_matching_wait_in(tx, SESSION, &unrelated, 21))
                .expect("unrelated result")
                .is_none()
        );
        assert_eq!(store.get(SESSION).expect("read"), Some(waiting.clone()));
    }
    assert_eq!(
        store
            .admit_wake(
                SESSION,
                &SessionWakeSignal::ExternalCompletion {
                    source_id: "background-1".to_owned(),
                    origin_cycle_id: "origin-cycle".to_owned(),
                },
                22
            )
            .expect("matching completion"),
        WakeAdmission::Resume
    );
    let ready = store.get(SESSION).expect("read").expect("state");
    assert_authority_unchanged(&waiting, &ready);
    assert_eq!(ready.phase, SessionExecutionPhase::Idle);
    assert!(
        pool.transaction(|tx| clear_matching_wait_in(tx, SESSION, &wait, 23))
            .expect("repeated completion")
            .is_none()
    );
    assert_eq!(store.get(SESSION).expect("read"), Some(ready));
}

#[test]
fn scheduling_and_matching_wake_roll_back_with_the_callers_transaction() {
    let pool = initialized(&DbLocation::Memory);
    let state = seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    let rollback = pool.transaction::<(), _>(|tx| {
        set_paused_in(
            tx,
            SESSION,
            state.revision,
            SessionPauseReason::NoExecutableWork,
            20,
        )?;
        Err(DbError::NotFound {
            table: "rollback-probe".to_owned(),
            id: "probe".to_owned(),
        })
    });
    assert!(rollback.is_err());
    assert_eq!(store.get(SESSION).expect("read"), Some(state.clone()));
    let paused = pool
        .transaction(|tx| {
            set_paused_in(
                tx,
                SESSION,
                state.revision,
                SessionPauseReason::NoExecutableWork,
                21,
            )
        })
        .expect("pause");
    let rollback = pool.transaction::<(), _>(|tx| {
        assert_eq!(
            admit_wake_in(tx, SESSION, &SessionWakeSignal::ExplicitResume, 22)?,
            WakeAdmission::Resume
        );
        Err(DbError::NotFound {
            table: "inbox-admission".to_owned(),
            id: "probe".to_owned(),
        })
    });
    assert!(rollback.is_err());
    assert_eq!(store.get(SESSION).expect("read"), Some(paused));
}

#[test]
fn completed_eligibility_does_not_change_plan_mode_or_authorization() {
    let pool = initialized(&DbLocation::Memory);
    let mut state = seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    state.mode = CollaborationMode::Plan;
    state.phase = SessionExecutionPhase::Planning;
    state.continuation.as_mut().expect("continuation").mode = CollaborationMode::Plan;
    let state = store.update(state.revision, state).expect("Plan mode");
    let completed = pool
        .transaction(|tx| set_completed_in(tx, SESSION, state.revision, 20))
        .expect("complete scheduling");
    assert_authority_unchanged(&state, &completed);
    assert_eq!(completed.phase, SessionExecutionPhase::Completed);
    assert_eq!(
        store
            .admit_wake(SESSION, &SessionWakeSignal::Recovery, 21)
            .expect("recovery"),
        WakeAdmission::Reject
    );
    assert_eq!(
        store
            .admit_wake(SESSION, &SessionWakeSignal::UserQuery, 22)
            .expect("query"),
        WakeAdmission::Resume
    );
    let resumed = store.get(SESSION).expect("read").expect("state");
    assert_authority_unchanged(&completed, &resumed);
    assert_eq!(resumed.phase, SessionExecutionPhase::Planning);
    let completed_again = pool
        .transaction(|tx| set_completed_in(tx, SESSION, resumed.revision, 23))
        .expect("complete the new turn");
    assert_eq!(
        store
            .admit_wake(
                SESSION,
                &SessionWakeSignal::UserAnswer {
                    request_id: "validated-deferred-answer".to_owned(),
                },
                24
            )
            .expect("late answer starts a new turn"),
        WakeAdmission::Resume
    );
    let resumed = store.get(SESSION).expect("read").expect("state");
    assert_authority_unchanged(&completed_again, &resumed);
    assert_eq!(resumed.phase, SessionExecutionPhase::Planning);
}

#[test]
fn stale_writes_invalid_waits_and_inconsistent_phase_fail_without_mutation() {
    let pool = initialized(&DbLocation::Memory);
    let state = seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    let paused = pool
        .transaction(|tx| set_paused_in(tx, SESSION, state.revision, SessionPauseReason::User, 20))
        .expect("pause");
    assert!(matches!(
        pool.transaction(|tx| set_completed_in(tx, SESSION, state.revision, 21)),
        Err(DbError::Conflict { .. })
    ));
    for wait in [
        SessionWaitReference::Human {
            request_id: " ".to_owned(),
        },
        external_wait("", "origin-cycle"),
        external_wait("background-1", ""),
    ] {
        assert!(
            pool.transaction(|tx| set_waiting_in(tx, SESSION, paused.revision, wait, 22))
                .is_err()
        );
    }
    let mut inconsistent = paused.clone();
    inconsistent.phase = SessionExecutionPhase::Running;
    assert!(store.update(paused.revision, inconsistent).is_err());
    assert_eq!(store.get(SESSION).expect("unchanged"), Some(paused));
}

#[test]
fn scheduling_helpers_preserve_original_control_json_bytes_and_monotonic_time() {
    let pool = initialized(&DbLocation::Memory);
    let state = seeded(&pool);
    let raw = "{ \"agent\" : \"build\", \"providerId\" : \"provider\", \"modelId\" : \"model\", \"reasoning\" : \"high\" }";
    pool.get()
        .expect("connection")
        .execute(
            "UPDATE session_execution_state SET work_identity = ?1 WHERE session_id = ?2",
            [raw, SESSION],
        )
        .expect("historical JSON formatting");
    let paused = pool
        .transaction(|tx| {
            set_scheduling_in(
                tx,
                SESSION,
                state.revision,
                SessionScheduling {
                    readiness: SessionReadiness::Paused {
                        reason: SessionPauseReason::NoProgress,
                    },
                    ..state.scheduling.clone().expect("scheduling")
                },
                5,
            )
        })
        .expect("pause with an older clock");
    assert_eq!(paused.time_updated, state.time_updated);
    assert_eq!(
        pool.get()
            .expect("connection")
            .query_row(
                "SELECT work_identity FROM session_execution_state WHERE session_id = ?1",
                [SESSION],
                |row| row.get::<_, String>(0),
            )
            .expect("raw identity"),
        raw
    );
    assert_authority_unchanged(&state, &paused);
}

#[test]
fn invalid_persisted_scheduling_is_rejected_at_the_read_boundary() {
    let pool = initialized(&DbLocation::Memory);
    seeded(&pool);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    for invalid in [
        r#"{"readiness":{"kind":"unknown"}}"#,
        r#"{"readiness":{"kind":"waiting_human","requestId":""}}"#,
        r#"{"readiness":{"kind":"paused","reason":"user"}}"#,
        r#"{"readiness":{"kind":"ready"},"unchangedProgressCount":-1}"#,
    ] {
        pool.get()
            .expect("connection")
            .execute(
                "UPDATE session_execution_state SET scheduling = ?1 WHERE session_id = ?2",
                [invalid, SESSION],
            )
            .expect("valid JSON at SQL boundary");
        assert!(store.get(SESSION).is_err(), "accepted {invalid}");
        assert!(
            store
                .admit_wake(SESSION, &SessionWakeSignal::Callback, 20)
                .is_err()
        );
    }
}
