use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
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
        .admit(input("input-1", "first", InputDelivery::NextStep, 10))
        .expect("admit input");

    assert_eq!(admitted.admitted_sequence, 0);
    assert_eq!(admitted.promoted_sequence, None);
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
        .admit(input("input-1", "first", InputDelivery::NextStep, 10))
        .expect("first admission");
    inbox
        .admit(input("input-2", "second", InputDelivery::NextStep, 11))
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
        .admit(input("next", "later", InputDelivery::NextStep, 11))
        .expect("next-step admission");

    let promoted = inbox
        .promote_next(SESSION_ID, Some(InputDelivery::NextStep))
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
        .admit(input("first", "first", InputDelivery::NextStep, 10))
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
        .admit(input("input-1", "only", InputDelivery::NextStep, 10))
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
            .admit(input("input-1", "recover me", InputDelivery::NextStep, 10))
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
        .admit(input("duplicate", "first", InputDelivery::NextStep, 10))
        .expect("first admission");

    inbox
        .admit(input("duplicate", "second", InputDelivery::NextStep, 11))
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
