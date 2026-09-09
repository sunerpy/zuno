//! Durable-first admission: the inbox row is committed before any lease contention.

use std::sync::Arc;

use serde_json::json;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::{Pool, migration, session};
use zuno_engine::admission::{
    InputAdmission, SessionInputAdmission, SteerAdmissionError, SteeringContent, TurnLease,
};
use zuno_engine::status::{ExpectedTurnError, SessionRunRegistry};
use zuno_paths::DbLocation;

const SESSION_ID: &str = "ses_admission";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT OR IGNORE INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("create project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                SESSION_ID,
                "admission",
                "project",
                "/workspace",
                "/workspace",
                "Admission test",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    pool
}

fn prompt(id: &str, text: &str) -> NewSessionInput {
    NewSessionInput::new(
        id,
        SESSION_ID,
        json!({"text": text}),
        InputDelivery::Steer,
        10,
    )
}

#[test]
fn an_idle_session_hands_the_caller_the_lease_for_the_row_it_just_wrote() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());

    let admitted = admission
        .admit(prompt("input-1", "first"), TurnLease::Acquire, None)
        .expect("admit input");

    let InputAdmission::Drive { input, guard } = admitted else {
        panic!("an idle session must hand the caller the lease");
    };
    assert_eq!(input.id, "input-1");
    assert_eq!(guard.session_id(), SESSION_ID);
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("read pending")
            .iter()
            .map(|pending| pending.id.clone())
            .collect::<Vec<_>>(),
        ["input-1"],
        "the caller drives the row, but the row is durable before the lease exists"
    );
}

#[test]
fn a_prompt_that_loses_the_lease_is_still_durable_and_steers_the_running_turn() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());
    let running = runs.begin_turn(SESSION_ID).expect("own the live turn");

    let admitted = admission
        .admit(
            prompt("input-2", "while busy"),
            TurnLease::Acquire,
            Some(SteeringContent::user("while busy")),
        )
        .expect("admit input");

    assert!(
        admitted.steered(),
        "a busy session must accept the input into the running turn"
    );
    assert_eq!(admitted.input().state, SubmissionState::Steering);
    let delivered = running.take_soft_interrupts_at_safe_point();
    assert_eq!(
        delivered
            .messages
            .iter()
            .map(|message| (message.input_id.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        [(Some("input-2".to_owned()), "while busy".to_owned())],
        "the steered message must carry the durable row's identifier"
    );
}

#[test]
fn a_busy_session_with_nothing_to_steer_leaves_the_row_pending() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());
    let _running = runs.begin_turn(SESSION_ID).expect("own the live turn");

    let admitted = admission
        .admit(prompt("input-3", "queued"), TurnLease::Acquire, None)
        .expect("admit input");

    assert!(
        matches!(admitted, InputAdmission::Pending { .. }),
        "without a steering projection the durable row is the queue"
    );
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("read pending")
            .iter()
            .map(|pending| pending.id.clone())
            .collect::<Vec<_>>(),
        ["input-3"],
        "the next turn promotes the row in FIFO order"
    );
}

#[test]
fn a_deferred_caller_never_takes_the_lease_from_its_own_driver() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());

    let admitted = admission
        .admit(prompt("input-4", "deferred"), TurnLease::Deferred, None)
        .expect("admit input");

    assert!(
        matches!(admitted, InputAdmission::Pending { .. }),
        "a deferred caller must not receive a lease even when the session is idle"
    );
    let guard = runs
        .begin_turn(SESSION_ID)
        .expect("the lease is still available to the session's own driver");
    assert_eq!(guard.session_id(), SESSION_ID);
}

#[test]
fn a_deferred_caller_steers_the_turn_its_driver_is_already_running() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());
    let running = runs.begin_turn(SESSION_ID).expect("own the live turn");

    let admitted = admission
        .admit(
            prompt("input-5", "steer the driver"),
            TurnLease::Deferred,
            Some(SteeringContent::user("steer the driver")),
        )
        .expect("admit input");

    assert!(admitted.steered());
    assert_eq!(
        running.take_soft_interrupts_at_safe_point().messages.len(),
        1
    );
}

#[test]
fn admission_fails_closed_when_the_durable_write_cannot_land() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox, runs.clone());

    let error = admission
        .admit(
            NewSessionInput::new(
                "input-6",
                "ses_missing",
                json!({"text": "orphan"}),
                InputDelivery::Steer,
                10,
            ),
            TurnLease::Acquire,
            Some(SteeringContent::user("orphan")),
        )
        .expect_err("an input for a session that does not exist cannot be admitted");

    assert!(
        !error.to_string().is_empty(),
        "the durable failure is reported instead of a lease"
    );
    assert!(
        runs.begin_turn("ses_missing").is_ok(),
        "a refused admission must not leave a lease behind"
    );
}

#[test]
fn a_precise_steer_is_durable_and_a_duplicate_does_not_retire_the_first_signal() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());
    let running = runs.begin_turn(SESSION_ID).expect("own the live turn");
    let _identity = running.mark_turn_started("turn_current").expect("turn id");

    let input = admission
        .admit_steer(
            prompt("input-precise", "same turn"),
            "turn_current",
            SteeringContent::user("same turn"),
        )
        .expect("admit precise steer");
    assert_eq!(input.state, SubmissionState::Steering);
    assert_eq!(
        inbox.get(SESSION_ID, &input.id).expect("durable row"),
        Some(input)
    );
    let duplicate = admission
        .admit_steer(
            prompt("input-precise", "duplicate"),
            "turn_current",
            SteeringContent::user("duplicate"),
        )
        .expect_err("duplicate input id must fail");
    assert!(matches!(duplicate, SteerAdmissionError::Database(_)));
    let delivered = running.take_soft_interrupts_at_safe_point();
    assert_eq!(delivered.messages.len(), 1);
    assert_eq!(delivered.messages[0].content, "same turn");
    assert_eq!(
        delivered.messages[0].input_id.as_deref(),
        Some("input-precise")
    );
}

#[test]
fn a_precise_steer_losing_its_turn_while_waiting_for_sqlite_admits_nothing() {
    let pool = initialized();
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let runs = SessionRunRegistry::new();
    let admission = SessionInputAdmission::new(inbox.clone(), runs.clone());
    let running = runs.begin_turn(SESSION_ID).expect("own first turn");
    let identity = running
        .mark_turn_started("turn_old")
        .expect("first turn id");
    let events = zuno_db::event_log::SessionEventLog::new(Arc::clone(&pool));
    let before = events.read_after(SESSION_ID, None).expect("initial events");

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker_pool = Arc::clone(&pool);
    let blocker = std::thread::spawn(move || {
        blocker_pool
            .transaction(|_transaction| {
                held_tx.send(()).expect("announce writer lock");
                release_rx.recv().expect("release writer lock");
                Ok(())
            })
            .expect("blocking transaction");
    });
    held_rx.recv().expect("writer is held");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let steering = std::thread::spawn(move || {
        started_tx.send(()).expect("announce admission");
        admission.admit_steer(
            prompt("input-raced", "must not reach the new turn"),
            "turn_old",
            SteeringContent::user("must not reach the new turn"),
        )
    });
    started_rx.recv().expect("admission started");
    drop(identity);
    drop(running);
    let replacement = runs.begin_turn(SESSION_ID).expect("own replacement turn");
    let _replacement_identity = replacement
        .mark_turn_started("turn_new")
        .expect("replacement turn id");
    release_tx
        .send(())
        .expect("let admission reach its turn check");
    blocker.join().expect("writer thread");

    let error = steering
        .join()
        .expect("admission thread")
        .expect_err("stale turn must be rejected");
    assert!(matches!(
        error,
        SteerAdmissionError::Turn(ExpectedTurnError::Mismatch {
            actual_turn_id,
            ..
        }) if actual_turn_id == "turn_new"
    ));
    assert!(
        inbox
            .pending(SESSION_ID)
            .expect("pending inputs")
            .is_empty()
    );
    assert_eq!(
        events
            .read_after(SESSION_ID, None)
            .expect("events after rejection"),
        before,
        "admission event must also roll back"
    );
    assert!(
        replacement
            .take_soft_interrupts_at_safe_point()
            .messages
            .is_empty()
    );
}
