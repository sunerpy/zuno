use serde_json::json;
use std::sync::{Arc, Barrier};
use zuno_db::completion_delivery::{CompletionDeliveryStore, CompletionOwner};
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput, SubmissionState};
use zuno_db::session_execution::SessionExecutionStore;
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_types::execution::{
    CollaborationMode, CompletionEnvelope, CompletionSource, ContinuationToken, InputTriggerKind,
    SessionExecutionPhase, SessionScheduling, TurnExecutionIdentity, WakeAdmission,
};

const SESSION: &str = "ses_completion";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                SESSION,
                "completion",
                "project",
                "/workspace",
                "/workspace",
                "Completion test",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("session");
    let execution = SessionExecutionStore::new(Arc::clone(&pool));
    let identity = TurnExecutionIdentity::new("build", "provider", "model");
    let mut state = execution
        .seed(SESSION, CollaborationMode::Work, Some(identity.clone()), 10)
        .expect("work execution");
    state.cycle_id = Some("cycle_1".to_owned());
    state.phase = SessionExecutionPhase::Running;
    state.scheduling = Some(SessionScheduling::default());
    state.continuation = Some(ContinuationToken {
        cycle_id: "cycle_1".to_owned(),
        identity,
        mode: CollaborationMode::Work,
        plan_id: None,
        plan_revision: None,
        context_epoch: 0,
        anchor_message_id: None,
    });
    state.time_updated = 11;
    execution
        .update(state.revision, state)
        .expect("ready originating cycle");
    pool
}

/// Frozen pre-gate inbox history. These rows already existed before promotion
/// required an originating cycle; the current gate must never create them.
fn restored_legacy_callback(pool: &Arc<Pool>, state: SubmissionState) -> SessionInput {
    let (revision, promoted_sequence, updated_at) = match state {
        SubmissionState::Steering => (1_i64, None, 20_i64),
        SubmissionState::Promoted => (2, Some(1_i64), 21),
        SubmissionState::Consumed => (3, Some(1), 22),
        _ => panic!("unsupported legacy fixture state"),
    };
    pool.transaction(|tx| {
        tx.execute(
            "INSERT INTO event_sequence (aggregate_id,seq,owner_id) \
             VALUES ('ses_completion',?1,NULL)",
            [revision - 1],
        )
        .expect("legacy event sequence");
        tx.execute_batch(
            r#"INSERT INTO event (id,aggregate_id,seq,type,data) VALUES (
                'evt_legacy_admitted','ses_completion',0,'session.input.admitted.1',
                '{"inputID":"msg_legacy","sessionID":"ses_completion","prompt":{"kind":"backgroundExecutionReport","text":"done"},"delivery":"steer","state":"steering","revision":1,"triggerKind":"legacy","sourceKey":"background:bg_legacy_inbox:2","timeCreated":20}'
            );"#,
        )
        .expect("legacy admission event");
        if promoted_sequence.is_some() {
            tx.execute_batch(
                r#"INSERT INTO event (id,aggregate_id,seq,type,data) VALUES (
                    'evt_legacy_promoted','ses_completion',1,'session.input.promoted.1',
                    '{"inputID":"msg_legacy","sessionID":"ses_completion","delivery":"steer","state":"promoted","revision":2,"triggerKind":"legacy","sourceKey":"background:bg_legacy_inbox:2","timeUpdated":21}'
                );"#,
            )
            .expect("legacy promotion event");
        }
        if state == SubmissionState::Consumed {
            tx.execute_batch(
                r#"INSERT INTO event (id,aggregate_id,seq,type,data) VALUES (
                    'evt_legacy_consumed','ses_completion',2,'session.input.consumed.1',
                    '{"inputID":"msg_legacy","sessionID":"ses_completion","delivery":"steer","state":"consumed","revision":3,"triggerKind":"legacy","sourceKey":"background:bg_legacy_inbox:2","timeUpdated":22}'
                );"#,
            )
            .expect("legacy consumption event");
        }
        tx.execute(
            r#"INSERT INTO session_input (
                id,session_id,prompt,delivery,state,revision,admitted_seq,promoted_seq,
                error,source_key,trigger_kind,cycle_id,time_created,time_updated
            ) VALUES (
                'msg_legacy','ses_completion','{"kind":"backgroundExecutionReport","text":"done"}',
                'steer',?1,?2,0,?3,NULL,'background:bg_legacy_inbox:2','legacy',NULL,20,?4
            )"#,
            rusqlite::params![state.as_str(), revision, promoted_sequence, updated_at],
        )
        .expect("exact legacy inbox row");
        Ok(())
    })
    .expect("restore legacy history");
    SessionInbox::new(Arc::clone(pool))
        .get(SESSION, "msg_legacy")
        .expect("read legacy callback")
        .expect("restored callback")
}

fn envelope(source_key: &str) -> CompletionEnvelope {
    CompletionEnvelope {
        source_key: source_key.to_owned(),
        source: CompletionSource::BackgroundExecution,
        terminal_revision: 2,
        parent_session_id: SESSION.to_owned(),
        cycle_id: Some("cycle_1".to_owned()),
        payload: json!({"kind":"backgroundExecutionReport","text":"done"}),
    }
}

fn callback_input(id: &str, source_key: &str) -> NewSessionInput {
    NewSessionInput::new(
        id,
        SESSION,
        json!({"kind":"backgroundExecutionReport","text":"done"}),
        InputDelivery::Queue,
        20,
    )
    .with_source_key(source_key)
    .with_trigger_kind(InputTriggerKind::Automatic)
    .with_cycle_id(Some("cycle_1"))
}

#[test]
fn publication_is_idempotent_but_reusing_a_source_key_for_other_payload_is_refused() {
    let store = CompletionDeliveryStore::new(initialized());
    let first = store
        .publish(envelope("background:bg_1:2"), 10)
        .expect("publish");
    assert_eq!(
        store
            .publish(envelope("background:bg_1:2"), 11)
            .expect("idempotent publish"),
        first
    );
    let mut changed = envelope("background:bg_1:2");
    changed.payload = json!({"kind":"backgroundExecutionReport","text":"changed"});
    assert!(store.publish(changed, 12).is_err());
}

#[test]
fn an_inline_read_prevents_later_callback_admission_and_repeat_claims() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(Arc::clone(&pool));
    store
        .publish(envelope("background:bg_inline:2"), 10)
        .expect("publish inline");
    let inline = store
        .claim_inline("background:bg_inline:2", 20)
        .expect("inline claim")
        .expect("inline owner");
    assert_eq!(inline.owner, Some(CompletionOwner::Inline));
    assert!(
        store
            .claim_callback(
                "background:bg_inline:2",
                callback_input("msg_inline", "background:bg_inline:2"),
                21,
            )
            .expect("callback loses")
            .is_none()
    );
    assert!(
        SessionInbox::new(Arc::clone(&pool))
            .pending(SESSION)
            .expect("pending")
            .is_empty()
    );
    assert!(
        store
            .claim_inline("background:bg_inline:2", 22)
            .expect("repeat inline claim")
            .is_none()
    );
}

#[test]
fn pending_callbacks_are_superseded_atomically_with_a_retained_revisioned_audit() {
    for lane in [InputDelivery::Queue, InputDelivery::Steer] {
        let pool = initialized();
        let store = CompletionDeliveryStore::new(Arc::clone(&pool));
        let inbox = SessionInbox::new(Arc::clone(&pool));
        let log = SessionEventLog::new(pool);
        let source_key = "background:bg_callback:2";
        store
            .publish(envelope(source_key), 30)
            .expect("publish callback");
        let mut request = callback_input("msg_callback", source_key);
        request.delivery = lane;
        let (_, input) = store
            .claim_callback(source_key, request, 31)
            .expect("callback claim")
            .expect("callback owner");
        let unseen = inbox
            .admit(callback_input("msg_unseen", "background:bg_other:2"))
            .expect("another result remains unseen");

        let inline = store
            .claim_inline(source_key, 32)
            .expect("inline supersedes pending callback")
            .expect("inline owner");
        assert_eq!(inline.owner, Some(CompletionOwner::Inline));
        assert_eq!(inline.input_id, None);
        let superseded = inbox
            .get(SESSION, &input.id)
            .expect("retained input")
            .expect("audit row");
        assert_eq!(superseded.state, SubmissionState::Cancelled);
        assert_eq!(superseded.revision, input.revision + 1);
        assert_eq!(superseded.prompt, input.prompt);
        assert_eq!(superseded.source_key, input.source_key);
        assert_eq!(superseded.cycle_id, input.cycle_id);
        assert_eq!(superseded.promoted_sequence, None);
        assert_eq!(
            superseded.error.as_deref(),
            Some("completion consumed inline before callback promotion")
        );
        assert_eq!(
            inbox
                .get_by_source_key(SESSION, source_key)
                .expect("source audit"),
            Some(superseded.clone())
        );
        assert_eq!(inbox.pending(SESSION).expect("pending"), [unseen]);
        assert!(
            inbox
                .promote_revision(SESSION, &input.id, input.revision)
                .expect("stale steer is refused")
                .is_none()
        );
        assert!(
            inbox
                .promote_id(SESSION, &input.id)
                .expect("cancelled callback cannot be promoted")
                .is_none()
        );
        assert!(
            inbox
                .mark_consumed(SESSION, &input.id)
                .expect("cancelled callback cannot be consumed")
                .is_none()
        );
        assert!(
            store
                .claim_inline(source_key, 33)
                .expect("repeat inline claim")
                .is_none()
        );
        assert!(
            store
                .claim_callback(source_key, callback_input("msg_retry", source_key), 34)
                .expect("late callback cannot reclaim completion")
                .is_none()
        );
        let events = log.read_after(SESSION, None).expect("audit events");
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].event_type, "session.input.superseded");
        assert_eq!(events[2].properties["inputID"], input.id);
        assert_eq!(events[2].properties["revision"], superseded.revision);
        assert_eq!(events[2].properties["sourceKey"], source_key);
        assert_eq!(events[2].properties["cycleID"], "cycle_1");
        assert_eq!(events[2].properties["state"], "cancelled");
    }
}

#[test]
fn promoted_and_consumed_callbacks_keep_their_owner_and_input() {
    for consumed in [false, true] {
        let pool = initialized();
        let store = CompletionDeliveryStore::new(Arc::clone(&pool));
        let inbox = SessionInbox::new(Arc::clone(&pool));
        let log = SessionEventLog::new(pool);
        let source_key = "background:bg_callback:2";
        store.publish(envelope(source_key), 30).expect("publish");
        let (delivery, input) = store
            .claim_callback(source_key, callback_input("msg_callback", source_key), 31)
            .expect("callback claim")
            .expect("callback owner");
        assert_eq!(
            inbox.wake_admission(&input).expect("modern callback gate"),
            WakeAdmission::Admit
        );
        let mut claimed = inbox
            .promote_revision(SESSION, &input.id, input.revision)
            .expect("promote callback")
            .expect("claimed input");
        if consumed {
            claimed = inbox
                .mark_consumed(SESSION, &claimed.id)
                .expect("consume callback")
                .expect("model-visible input");
        }
        let before = log.read_after(SESSION, None).expect("events before read");
        assert!(
            store
                .claim_inline(source_key, 32)
                .expect("callback consumer keeps its claim")
                .is_none()
        );
        assert_eq!(store.get(source_key).expect("delivery"), Some(delivery));
        assert_eq!(
            inbox.get(SESSION, &input.id).expect("retained input"),
            Some(claimed)
        );
        assert_eq!(log.read_after(SESSION, None).expect("events after"), before);
        if !consumed {
            assert!(
                inbox
                    .mark_consumed(SESSION, &input.id)
                    .expect("the reserved result is still consumable")
                    .is_some()
            );
        }
        assert!(
            inbox
                .mark_consumed(SESSION, &input.id)
                .expect("callback is only consumed once")
                .is_none()
        );
    }
}

#[test]
fn legacy_inbox_callbacks_are_arbitrated_before_a_completion_owner_is_recorded() {
    for state in [
        SubmissionState::Steering,
        SubmissionState::Promoted,
        SubmissionState::Consumed,
    ] {
        let pool = initialized();
        let store = CompletionDeliveryStore::new(Arc::clone(&pool));
        let inbox = SessionInbox::new(Arc::clone(&pool));
        let source_key = "background:bg_legacy_inbox:2";
        let input = restored_legacy_callback(&pool, state);
        let before = input.clone();
        let log = SessionEventLog::new(pool);
        let historical_events = log.read_after(SESSION, None).expect("legacy history");
        assert_eq!(input.state, state);
        assert_eq!(input.trigger_kind, InputTriggerKind::Legacy);
        assert_eq!(input.cycle_id, None);
        assert_eq!(
            inbox.wake_admission(&input).expect("unbound legacy gate"),
            WakeAdmission::Reject
        );
        if state.is_pending() {
            assert!(
                inbox
                    .promote_revision(SESSION, &input.id, input.revision)
                    .expect("legacy cannot be newly promoted")
                    .is_none()
            );
            assert!(
                inbox
                    .promote_next(SESSION, None)
                    .expect("FIFO cannot bypass the legacy gate")
                    .is_none()
            );
        }
        let mut terminal = envelope(source_key);
        terminal.cycle_id = None;
        store.publish(terminal, 30).expect("publish on recovery");
        let inline = store.claim_inline(source_key, 31).expect("explicit read");
        assert_eq!(inline.is_some(), state.is_pending());
        if state.is_pending() {
            let audit = inbox
                .get_by_source_key(SESSION, source_key)
                .expect("legacy audit")
                .expect("retained callback");
            assert_eq!(audit.state, SubmissionState::Cancelled);
            assert_eq!(audit.revision, input.revision + 1);
            assert_eq!(audit.cycle_id, None);
            assert!(
                inbox
                    .promote_revision(SESSION, &input.id, input.revision)
                    .expect("stale legacy steer")
                    .is_none()
            );
            assert!(
                store
                    .claim_callback(source_key, callback_input("msg_retry", source_key), 32)
                    .expect("recovery cannot readmit a duplicate")
                    .is_none()
            );
        } else {
            assert_eq!(
                inbox.get(SESSION, &input.id).expect("input"),
                Some(before.clone())
            );
            let (_, recovered) = store
                .claim_callback(source_key, callback_input("msg_retry", source_key), 32)
                .expect("record existing callback owner")
                .expect("callback owner");
            assert_eq!(recovered, before);
            assert_eq!(
                log.read_after(SESSION, None)
                    .expect("retained legacy history"),
                historical_events
            );
        }
    }
}

#[test]
fn inline_claim_does_not_discard_work_added_to_a_pending_callback() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(Arc::clone(&pool));
    let inbox = SessionInbox::new(pool);
    let source_key = "background:bg_edited:2";
    store.publish(envelope(source_key), 10).expect("publish");
    let (delivery, input) = store
        .claim_callback(source_key, callback_input("msg_edited", source_key), 20)
        .expect("callback")
        .expect("pending callback");
    let edited = inbox
        .edit_pending(
            SESSION,
            &input.id,
            input.revision,
            json!({"kind":"backgroundExecutionReport","text":"done; also inspect another result"}),
            21,
        )
        .expect("edit pending work");
    assert!(
        store
            .claim_inline(source_key, 22)
            .expect("reading the old terminal result")
            .is_none()
    );
    assert_eq!(store.get(source_key).expect("delivery"), Some(delivery));
    assert_eq!(inbox.pending(SESSION).expect("unseen work"), [edited]);
}

#[test]
fn a_failed_owner_update_rolls_back_callback_cancellation_and_its_event() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(Arc::clone(&pool));
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let log = SessionEventLog::new(Arc::clone(&pool));
    let source_key = "background:bg_rollback:2";
    store.publish(envelope(source_key), 10).expect("publish");
    let (delivery, input) = store
        .claim_callback(source_key, callback_input("msg_rollback", source_key), 20)
        .expect("callback")
        .expect("pending callback");
    let before = log.read_after(SESSION, None).expect("events");
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER refuse_inline BEFORE UPDATE OF owner ON completion_delivery \
             WHEN NEW.owner = 'inline' BEGIN SELECT RAISE(ABORT, 'injected owner failure'); END;",
        )
        .expect("inject failure");

    assert!(store.claim_inline(source_key, 21).is_err());
    assert_eq!(store.get(source_key).expect("delivery"), Some(delivery));
    assert_eq!(
        inbox.pending(SESSION).expect("callback remains pending"),
        [input]
    );
    assert_eq!(log.read_after(SESSION, None).expect("events"), before);
}

#[test]
fn racing_callback_admission_and_inline_read_leave_no_pending_duplicate() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(Arc::clone(&pool));
    let inbox = SessionInbox::new(pool);
    for index in 0..8 {
        let source_key = format!("background:bg_race_{index}:2");
        store.publish(envelope(&source_key), 10).expect("publish");
        let barrier = Barrier::new(2);
        let (callback, inline) = std::thread::scope(|scope| {
            let callback = scope.spawn(|| {
                barrier.wait();
                store
                    .claim_callback(
                        &source_key,
                        callback_input(&format!("msg_race_{index}"), &source_key),
                        20,
                    )
                    .expect("callback race")
            });
            barrier.wait();
            let inline = store.claim_inline(&source_key, 21).expect("inline race");
            (callback.join().expect("callback thread"), inline)
        });
        let inline = inline.expect("inline read owns an unpromoted completion");
        assert_eq!(inline.owner, Some(CompletionOwner::Inline));
        assert_eq!(inline.input_id, None);
        assert!(inbox.pending(SESSION).expect("pending").is_empty());
        if let Some((_, queued)) = callback {
            assert_eq!(
                inbox
                    .get_by_source_key(SESSION, &source_key)
                    .expect("source audit")
                    .expect("cancelled callback remains inspectable")
                    .id,
                queued.id
            );
            assert!(
                inbox
                    .promote_revision(SESSION, &queued.id, queued.revision)
                    .expect("stale callback")
                    .is_none()
            );
        }
    }
}

#[test]
fn racing_callback_promotion_and_inline_read_reserve_exactly_one_consumer() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(Arc::clone(&pool));
    let inbox = SessionInbox::new(pool);
    for index in 0..8 {
        let source_key = format!("background:bg_promotion_{index}:2");
        store.publish(envelope(&source_key), 10).expect("publish");
        let (_, queued) = store
            .claim_callback(
                &source_key,
                callback_input(&format!("msg_promotion_{index}"), &source_key),
                20,
            )
            .expect("callback")
            .expect("pending callback");
        assert_eq!(
            inbox
                .wake_admission(&queued)
                .expect("eligible race contender"),
            WakeAdmission::Admit
        );
        let barrier = Barrier::new(2);
        let (promoted, inline) = std::thread::scope(|scope| {
            let promotion = scope.spawn(|| {
                barrier.wait();
                inbox
                    .promote_revision(SESSION, &queued.id, queued.revision)
                    .expect("promotion race")
            });
            barrier.wait();
            let inline = store.claim_inline(&source_key, 21).expect("inline race");
            (promotion.join().expect("promotion thread"), inline)
        });
        assert_ne!(promoted.is_some(), inline.is_some());
        let delivery = store.get(&source_key).expect("delivery").expect("exists");
        if let Some(promoted) = promoted {
            assert_eq!(delivery.owner, Some(CompletionOwner::Callback));
            assert!(
                inbox
                    .mark_consumed(SESSION, &promoted.id)
                    .expect("consume reserved callback")
                    .is_some()
            );
            assert!(
                inbox
                    .mark_consumed(SESSION, &promoted.id)
                    .expect("no repeated consumption")
                    .is_none()
            );
        } else {
            assert_eq!(delivery.owner, Some(CompletionOwner::Inline));
            assert_eq!(
                inbox
                    .get(SESSION, &queued.id)
                    .expect("audit")
                    .expect("retained input")
                    .state,
                SubmissionState::Cancelled
            );
        }
    }
}

#[test]
fn callback_cycle_comes_from_the_published_origin_and_legacy_stays_unbound() {
    let pool = initialized();
    let store = CompletionDeliveryStore::new(pool);
    for (source_key, cycle_id) in [
        ("background:bg_origin:2", Some("completion_original_cycle")),
        ("background:bg_legacy:2", None),
    ] {
        let mut terminal = envelope(source_key);
        terminal.cycle_id = cycle_id.map(str::to_owned);
        store.publish(terminal.clone(), 10).expect("publish");
        let (_, input) = store
            .claim_callback(
                source_key,
                callback_input(source_key, source_key)
                    .with_cycle_id(Some("later_turn_is_not_the_origin_cycle")),
                20,
            )
            .expect("callback")
            .expect("claimed callback");
        assert_eq!(input.cycle_id.as_deref(), cycle_id);
        assert_eq!(
            store
                .get(source_key)
                .expect("delivery")
                .expect("retained")
                .envelope,
            terminal
        );
    }
}
