use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;

const SESSION_ID: &str = "ses_inbox";

fn initialized(location: &DbLocation) -> Arc<Pool> {
    let pool = Arc::new(Pool::open(location).expect("open database"));
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
                "inbox",
                "project",
                "/workspace",
                "/workspace",
                "Inbox test",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    pool
}

fn input(id: &str, text: &str, delivery: InputDelivery, time: i64) -> NewSessionInput {
    NewSessionInput::new(id, SESSION_ID, json!({"text": text}), delivery, time)
}

#[test]
fn admission_commits_the_event_and_pending_input_together() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);

    let admitted = inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admit input");

    assert_eq!(admitted.state, SubmissionState::Queued);
    assert_eq!(admitted.revision, 1);
    assert_eq!(admitted.admitted_sequence, 0);
    assert_eq!(admitted.promoted_sequence, None);
    assert_eq!(admitted.time_updated, admitted.time_created);
    assert_eq!(inbox.pending(SESSION_ID).expect("pending"), [admitted]);
    let events = log.read_after(SESSION_ID, None).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "session.input.admitted");
    assert_eq!(events[0].properties["inputID"], "input-1");
}

#[test]
fn promotion_is_fifo_and_each_input_is_claimed_once() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("first admission");
    inbox
        .admit(input("input-2", "second", InputDelivery::Queue, 11))
        .expect("second admission");

    let first = inbox
        .promote_next(SESSION_ID, None)
        .expect("first promotion")
        .expect("first pending input");
    let second = inbox
        .promote_next(SESSION_ID, None)
        .expect("second promotion")
        .expect("second pending input");

    assert_eq!(first.id, "input-1");
    assert_eq!(second.id, "input-2");
    assert!(first.promoted_sequence.is_some());
    assert!(second.promoted_sequence > first.promoted_sequence);
    assert_eq!(
        inbox
            .promote_next(SESSION_ID, None)
            .expect("empty promotion"),
        None
    );
}

#[test]
fn delivery_filter_promotes_only_the_requested_class() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("steer", "urgent", InputDelivery::Steer, 10))
        .expect("steer admission");
    inbox
        .admit(input("next", "later", InputDelivery::Queue, 11))
        .expect("next-step admission");

    let promoted = inbox
        .promote_next(SESSION_ID, Some(InputDelivery::Queue))
        .expect("promotion")
        .expect("next-step input");

    assert_eq!(promoted.id, "next");
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["steer"]
    );
}

#[test]
fn promotion_by_id_claims_only_the_live_injected_input() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("first", "first", InputDelivery::Queue, 10))
        .expect("first admission");
    inbox
        .admit(input("steered", "now", InputDelivery::Steer, 11))
        .expect("steer admission");

    let promoted = inbox
        .promote_id(SESSION_ID, "steered")
        .expect("promotion")
        .expect("pending steer");

    assert_eq!(promoted.id, "steered");
    assert_eq!(
        inbox
            .pending(SESSION_ID)
            .expect("pending")
            .into_iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert_eq!(
        inbox
            .promote_id(SESSION_ID, "steered")
            .expect("repeat promotion"),
        None
    );
}

#[test]
fn two_concurrent_promoters_cannot_claim_the_same_input() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "only", InputDelivery::Queue, 10))
        .expect("admission");

    let left = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_next(SESSION_ID, None)
                .expect("left promotion")
        })
    };
    let right = {
        let inbox = inbox.clone();
        std::thread::spawn(move || {
            inbox
                .promote_next(SESSION_ID, None)
                .expect("right promotion")
        })
    };
    let claimed = [
        left.join().expect("left thread"),
        right.join().expect("right thread"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, "input-1");
}

#[test]
fn pending_inputs_survive_pool_and_process_reconstruction() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let location = DbLocation::File(directory.path().join("zuno.db"));
    {
        let pool = initialized(&location);
        SessionInbox::new(pool)
            .admit(input("input-1", "recover me", InputDelivery::Queue, 10))
            .expect("admission");
    }

    let recovered = SessionInbox::new(initialized(&location))
        .pending(SESSION_ID)
        .expect("recovered pending inputs");

    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].prompt["text"], "recover me");
}

#[test]
fn failed_admission_rolls_back_its_event_sequence() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);
    inbox
        .admit(input("duplicate", "first", InputDelivery::Queue, 10))
        .expect("first admission");

    inbox
        .admit(input("duplicate", "second", InputDelivery::Queue, 11))
        .expect_err("duplicate id must fail");
    log.append(
        SESSION_ID,
        NewSessionEvent::new("test.after.failure", serde_json::Map::new()).expect("valid event"),
    )
    .expect("append after failed admission");

    let events = log.read_after(SESSION_ID, None).expect("events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [0, 1],
        "the rolled-back admission must not consume sequence 1"
    );
    assert_eq!(events[1].event_type, "test.after.failure");
}

#[test]
fn pending_inputs_can_be_edited_and_cancelled_with_revisions() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admission");

    let edited = inbox
        .edit_pending(SESSION_ID, "input-1", 1, json!({"text": "revised"}), 20)
        .expect("edit pending input");
    assert_eq!(edited.state, SubmissionState::Queued);
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.prompt["text"], "revised");
    assert_eq!(edited.time_updated, 20);

    let conflict = inbox
        .edit_pending(SESSION_ID, "input-1", 1, json!({"text": "stale"}), 21)
        .expect_err("stale edit must fail");
    assert!(conflict.to_string().contains("revision conflict"));

    let cancelled = inbox
        .cancel_pending(SESSION_ID, "input-1", 2, 30)
        .expect("cancel pending input");
    assert_eq!(cancelled.state, SubmissionState::Cancelled);
    assert_eq!(cancelled.revision, 3);
    assert!(inbox.pending(SESSION_ID).expect("pending").is_empty());
    assert_eq!(
        inbox
            .get(SESSION_ID, "input-1")
            .expect("stored input")
            .expect("input row")
            .state,
        SubmissionState::Cancelled
    );
    assert_eq!(
        log.read_after(SESSION_ID, None)
            .expect("events")
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        [
            "session.input.admitted",
            "session.input.edited",
            "session.input.cancelled",
        ],
        "the rejected stale edit must not consume an event sequence"
    );
}

#[test]
fn promoted_inputs_settle_consumed_once() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    inbox
        .admit(input("input-1", "first", InputDelivery::Queue, 10))
        .expect("admission");

    let promoted = inbox
        .promote_next(SESSION_ID, None)
        .expect("promotion")
        .expect("promoted input");
    assert_eq!(promoted.state, SubmissionState::Promoted);
    assert_eq!(promoted.revision, 2);

    let consumed = inbox
        .mark_consumed(SESSION_ID, "input-1")
        .expect("consume input")
        .expect("consumed input");
    assert_eq!(consumed.state, SubmissionState::Consumed);
    assert_eq!(consumed.revision, 3);
    assert_eq!(
        inbox
            .mark_consumed(SESSION_ID, "input-1")
            .expect("repeat consume"),
        None,
        "a consumed input cannot be settled twice"
    );
}

#[test]
fn failed_inputs_leave_the_pending_queue_with_a_diagnostic() {
    let pool = initialized(&DbLocation::Memory);
    let inbox = SessionInbox::new(pool);
    let admitted = inbox
        .admit(input("input-1", "urgent", InputDelivery::Steer, 10))
        .expect("admission");
    assert_eq!(admitted.state, SubmissionState::Steering);

    let failed = inbox
        .mark_failed(SESSION_ID, "input-1", "invalid persisted prompt")
        .expect("mark failed")
        .expect("failed input");
    assert_eq!(failed.state, SubmissionState::Failed);
    assert_eq!(failed.error.as_deref(), Some("invalid persisted prompt"));
    assert!(inbox.pending(SESSION_ID).expect("pending").is_empty());
    assert!(
        inbox
            .edit_pending(
                SESSION_ID,
                "input-1",
                failed.revision,
                json!({"text": "too late"}),
                20,
            )
            .expect_err("failed input is immutable")
            .to_string()
            .contains("already failed")
    );
}
