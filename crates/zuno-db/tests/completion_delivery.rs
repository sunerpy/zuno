use serde_json::json;
use std::sync::Arc;
use zuno_db::completion_delivery::{CompletionDeliveryStore, CompletionOwner};
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_types::execution::{CompletionEnvelope, CompletionSource, InputTriggerKind};

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
    pool
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
fn inline_and_callback_consumers_compete_for_one_durable_owner() {
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

    store
        .publish(envelope("background:bg_callback:2"), 30)
        .expect("publish callback");
    let (delivery, input) = store
        .claim_callback(
            "background:bg_callback:2",
            callback_input("msg_callback", "background:bg_callback:2"),
            31,
        )
        .expect("callback claim")
        .expect("callback owner");
    assert_eq!(delivery.owner, Some(CompletionOwner::Callback));
    assert_eq!(delivery.input_id.as_deref(), Some(input.id.as_str()));
    assert!(
        store
            .claim_inline("background:bg_callback:2", 32)
            .expect("inline loses")
            .is_none()
    );
    assert_eq!(
        SessionInbox::new(pool)
            .pending(SESSION)
            .expect("pending callback"),
        [input]
    );
}
