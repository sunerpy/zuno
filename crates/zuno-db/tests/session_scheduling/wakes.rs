use super::*;
use zuno_db::inbox::{SessionInbox, SessionInput};

fn callback(pool: &Arc<Pool>, id: &str, cycle: Option<&str>) -> SessionInput {
    SessionInbox::new(Arc::clone(pool)).admit(
        NewSessionInput::new(id, SESSION,
            json!({"kind":"backgroundExecutionReport","executionID":"bg-one","status":"completed","text":"done"}),
            InputDelivery::Steer, 30)
            .with_source_key(format!("background:{id}:1"))
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(cycle),
    ).expect("callback admission")
}

#[test]
fn callbacks_are_durable_but_cannot_bypass_no_goal_pause() {
    let pool = initialized(&DbLocation::Memory);
    let before = seeded(&pool);
    pool.transaction(|tx| {
        set_paused_in(
            tx,
            SESSION,
            before.revision,
            SessionPauseReason::NoProgress,
            20,
        )
    })
    .expect("pause");
    let input = callback(&pool, "input", Some("origin-cycle"));
    let inbox = SessionInbox::new(Arc::clone(&pool));
    assert_eq!(
        inbox.wake_admission(&input).expect("gate"),
        WakeAdmission::Reject
    );
    assert_eq!(inbox.pending(SESSION).expect("pending").len(), 1);
    let state = SessionExecutionStore::new(pool)
        .get(SESSION)
        .expect("state")
        .expect("state");
    assert_eq!(state.phase, SessionExecutionPhase::Paused);
    assert_eq!(
        state
            .scheduling
            .expect("scheduling")
            .unchanged_progress_count,
        3
    );
}

#[test]
fn cancelled_or_consumed_input_retires_already_queued_signal() {
    let pool = initialized(&DbLocation::Memory);
    seeded(&pool);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let input = callback(&pool, "cancelled", Some("origin-cycle"));
    assert_eq!(
        inbox.wake_admission(&input).expect("gate"),
        WakeAdmission::Admit
    );
    inbox
        .cancel_pending(SESSION, &input.id, input.revision, 40)
        .expect("cancel");
    assert_eq!(
        inbox.wake_admission(&input).expect("stale signal"),
        WakeAdmission::Reject
    );
    let input = callback(&pool, "consumed", Some("origin-cycle"));
    let promoted = inbox
        .promote_id(SESSION, &input.id)
        .expect("promote")
        .expect("promoted");
    inbox.mark_consumed(SESSION, &promoted.id).expect("consume");
    assert_eq!(
        inbox.wake_admission(&promoted).expect("old consumer"),
        WakeAdmission::Reject
    );
}

#[test]
fn wrong_or_legacy_cycle_cannot_wake_even_when_ready() {
    let pool = initialized(&DbLocation::Memory);
    seeded(&pool);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    for (id, cycle) in [("legacy", None), ("stale", Some("different-cycle"))] {
        let input = callback(&pool, id, cycle);
        assert_eq!(
            inbox.wake_admission(&input).expect("gate"),
            WakeAdmission::Reject
        );
    }
}

#[test]
fn matching_completion_can_resume_only_its_external_wait() {
    let pool = initialized(&DbLocation::Memory);
    let before = seeded(&pool);
    pool.transaction(|tx| {
        set_waiting_in(
            tx,
            SESSION,
            before.revision,
            external_wait("bg-one", "origin-cycle"),
            20,
        )
    })
    .expect("wait");
    let input = callback(&pool, "matching", Some("origin-cycle"));
    let inbox = SessionInbox::new(Arc::clone(&pool));
    assert_eq!(
        inbox.wake_admission(&input).expect("gate"),
        WakeAdmission::Resume
    );
    // A read-only check is not itself the event consumer or a grant.
    assert_eq!(
        SessionExecutionStore::new(pool)
            .get(SESSION)
            .expect("get")
            .expect("state")
            .phase,
        SessionExecutionPhase::Waiting
    );
}
