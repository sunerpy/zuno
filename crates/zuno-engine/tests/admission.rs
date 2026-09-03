//! Durable-first admission: the inbox row is committed before any lease contention.

use std::sync::Arc;

use serde_json::json;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::{Pool, migration, session};
use zuno_engine::admission::{InputAdmission, SessionInputAdmission, SteeringContent, TurnLease};
use zuno_engine::status::SessionRunRegistry;
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
