use serde_json::json;
use std::sync::Arc;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::input_receipt::InputReceiptStore;
use zuno_db::{Pool, session};
use zuno_paths::DbLocation;
use zuno_types::admission::{InputReceiptState, InputStopReason};

fn fixture() -> (Arc<Pool>, SessionInbox, InputReceiptStore) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
    }
    pool.transaction(|tx| {
        tx.execute(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
             VALUES ('project','/workspace',1,1,'[]')",
            [],
        )
        .map_err(zuno_db::map_error)?;
        for id in ["session", "other"] {
            session::create(
                tx,
                &session::SessionCreate::new(
                    id,
                    id,
                    "project",
                    "/workspace",
                    "/workspace",
                    "Receipt test",
                    "zuno",
                )
                .at(1),
            )?;
        }
        Ok(())
    })
    .expect("sessions");
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let receipts = InputReceiptStore::new(Arc::clone(&pool));
    (pool, inbox, receipts)
}

fn input(id: &str, text: &str) -> NewSessionInput {
    NewSessionInput::new(
        id,
        "session",
        json!({"kind":"acpPrompt","text":text}),
        InputDelivery::Steer,
        10,
    )
}

fn recorded(inbox: &SessionInbox, id: &str) {
    inbox
        .promote_id("session", id)
        .expect("promote")
        .expect("input");
    inbox
        .mark_consumed("session", id)
        .expect("record")
        .expect("input");
}

#[test]
fn receipt_tracks_recording_application_and_real_completion_separately() {
    let (_, inbox, receipts) = fixture();
    let admitted = receipts.admit(input("input", "work")).expect("admit");
    assert_eq!(admitted.receipt.state, InputReceiptState::Admitted);
    recorded(&inbox, "input");
    receipts
        .bind_turn("session", &["input".into()], "turn", 20)
        .expect("bind");
    assert_eq!(
        receipts.get("session", "input").unwrap().unwrap().state,
        InputReceiptState::Recorded
    );
    receipts
        .finish_turn("session", "turn", Some(InputStopReason::EndTurn), None, 30)
        .expect("a deferred report is not completed");
    assert_eq!(
        receipts.get("session", "input").unwrap().unwrap().state,
        InputReceiptState::Recorded
    );
    receipts
        .mark_applied("session", &["input".into()], "turn", 40)
        .expect("apply");
    receipts
        .finish_turn("session", "turn", Some(InputStopReason::EndTurn), None, 50)
        .expect("complete");
    let complete = receipts.get("session", "input").unwrap().unwrap();
    assert_eq!(complete.state, InputReceiptState::Completed);
    assert_eq!(complete.applied_at, Some(40));
    assert_eq!(complete.stop_reason, Some(InputStopReason::EndTurn));
    receipts
        .finish_turn("session", "turn", Some(InputStopReason::EndTurn), None, 60)
        .expect("idempotent finish");
    assert_eq!(receipts.get("session", "input").unwrap().unwrap(), complete);
}

#[test]
fn client_id_is_session_scoped_and_conflicting_reuse_does_not_admit() {
    let (_, inbox, receipts) = fixture();
    let first = receipts
        .admit(input("first", "same").with_source_key("client-message:client"))
        .expect("first");
    let duplicate = receipts
        .admit(input("second", "same").with_source_key("client-message:client"))
        .expect("retry");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.input.id, first.input.id);
    assert_eq!(
        duplicate.receipt.client_message_id.as_deref(),
        Some("client")
    );
    assert!(
        receipts
            .admit(input("bad", "different").with_source_key("client-message:client"))
            .is_err()
    );
    assert_eq!(inbox.pending("session").unwrap().len(), 1);
    let other = NewSessionInput::new(
        "other-input",
        "other",
        json!({"text":"same"}),
        InputDelivery::Queue,
        10,
    )
    .with_source_key("client-message:client");
    assert!(
        !receipts
            .admit(other)
            .expect("independent session")
            .duplicate
    );
    assert!(receipts.get("other", "first").unwrap().is_none());
}

#[test]
fn exact_native_input_id_retry_reuses_the_receipt_but_not_different_content() {
    let (_, inbox, receipts) = fixture();
    let first = receipts.admit(input("same-id", "same")).expect("first");
    let replay = receipts
        .admit(input("same-id", "same"))
        .expect("exact retry");
    assert!(replay.duplicate);
    assert_eq!(replay.receipt, first.receipt);
    assert!(receipts.admit(input("same-id", "changed")).is_err());
    assert_eq!(inbox.pending("session").expect("pending").len(), 1);
}

#[test]
fn cancelling_pending_input_has_a_terminal_receipt_without_model_application() {
    let (_, inbox, receipts) = fixture();
    let admitted = receipts.admit(input("input", "work")).unwrap();
    inbox
        .cancel_pending("session", "input", admitted.input.revision, 20)
        .unwrap();
    let receipt = receipts.get("session", "input").unwrap().unwrap();
    assert_eq!(receipt.state, InputReceiptState::Cancelled);
    assert_eq!(receipt.applied_at, None);
    assert_eq!(receipt.turn_id, None);
}

#[test]
fn recovered_turn_handoff_does_not_complete_from_the_abandoned_turn() {
    let (_, inbox, receipts) = fixture();
    receipts.admit(input("input", "work")).unwrap();
    recorded(&inbox, "input");
    receipts
        .mark_applied("session", &["input".into()], "old", 20)
        .unwrap();
    receipts.handoff_turn("session", "old", "new", 30).unwrap();
    receipts
        .finish_turn("session", "old", Some(InputStopReason::EndTurn), None, 40)
        .unwrap();
    assert!(
        !receipts
            .get("session", "input")
            .unwrap()
            .unwrap()
            .state
            .is_terminal()
    );
    receipts
        .finish_turn("session", "new", Some(InputStopReason::EndTurn), None, 50)
        .unwrap();
    assert_eq!(
        receipts
            .get("session", "input")
            .unwrap()
            .unwrap()
            .turn_id
            .as_deref(),
        Some("new")
    );
}

#[test]
fn receipt_failure_rolls_back_input_and_admission_event() {
    let (pool, inbox, receipts) = fixture();
    pool.get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_receipt BEFORE INSERT ON session_input_receipt
         BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;",
        )
        .unwrap();
    assert!(receipts.admit(input("input", "work")).is_err());
    assert!(inbox.pending("session").unwrap().is_empty());
    let count: i64 = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM event WHERE aggregate_id='session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn owning_host_can_settle_preflight_failure_without_inventing_model_application() {
    let (_, inbox, receipts) = fixture();
    receipts.admit(input("before-model", "work")).unwrap();
    recorded(&inbox, "before-model");
    receipts
        .fail_input("session", "before-model", "preflight failed", false, 30)
        .unwrap();
    let receipt = receipts.get("session", "before-model").unwrap().unwrap();
    assert_eq!(receipt.state, InputReceiptState::Failed);
    assert_eq!(receipt.turn_id, None);
    assert_eq!(receipt.applied_at, None);
    assert_eq!(receipt.completed_at, Some(30));
    receipts
        .fail_input("session", "before-model", "late cancellation", true, 40)
        .unwrap();
    assert_eq!(
        receipts.get("session", "before-model").unwrap().unwrap(),
        receipt
    );
    assert_eq!(
        inbox.get("session", "before-model").unwrap().unwrap().state,
        zuno_db::inbox::SubmissionState::Consumed
    );
}

#[test]
fn recovery_transfer_rolls_back_when_the_new_input_cannot_bind() {
    let (pool, inbox, receipts) = fixture();
    receipts.admit(input("previous", "previous input")).unwrap();
    receipts.admit(input("next", "new input")).unwrap();
    recorded(&inbox, "previous");
    recorded(&inbox, "next");
    receipts
        .mark_applied("session", &["previous".to_owned()], "old-turn", 20)
        .unwrap();
    pool.get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_new_bind BEFORE UPDATE OF turn_id ON session_input_receipt
         WHEN OLD.input_id='next' BEGIN SELECT RAISE(ABORT,'injected bind failure'); END;",
        )
        .unwrap();
    assert!(
        receipts
            .begin_turn(
                "session",
                Some("old-turn"),
                &["next".to_owned()],
                "new-turn",
                30
            )
            .is_err()
    );
    assert_eq!(
        receipts
            .get("session", "previous")
            .unwrap()
            .unwrap()
            .turn_id
            .as_deref(),
        Some("old-turn")
    );
    assert_eq!(
        receipts.get("session", "next").unwrap().unwrap().turn_id,
        None
    );
    receipts
        .finish_turn("session", "old-turn", None, Some("recovery failed"), 40)
        .unwrap();
    assert_eq!(
        receipts.get("session", "previous").unwrap().unwrap().state,
        InputReceiptState::Failed
    );
}
