use serde_json::json;
use zuno_db::assistant_commit::AssistantCommit;
use zuno_db::event_log::NewSessionEvent;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::state::{
    ProviderEventUpdate, SqliteTurnPersistence, ToolPartCommitKind, TurnPersistence, TurnStateScope,
};
use zuno_types::identity::PrincipalScope;

fn fixture() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("database");
    migration::apply(&mut connection).expect("schema");
    connection.execute_batch(
        "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('p','/workspace',1,1,'[]');
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES('session','p','one','/workspace','One','preview',1,1);",
    ).expect("session");
    connection
}

fn scope() -> TurnStateScope {
    TurnStateScope {
        owner: PrincipalScope::local().owner(),
        session_id: "session".to_owned(),
    }
}

fn part(state: serde_json::Value) -> PartRecord {
    PartRecord::from_json(
        json!({
            "id":"tool-part","sessionID":"session","messageID":"assistant","type":"tool",
            "callID":"call-one","tool":"process","state":state,
        }),
        10,
    )
    .expect("tool part")
}

#[tokio::test]
async fn an_admitted_invocation_cannot_change_parameters_reexecute_or_replace_its_receipt() {
    let mut connection = fixture();
    let scope = scope();
    {
        let store = SqliteTurnPersistence::new(&mut connection);
        store.commit_assistant(&scope, &AssistantCommit {
            message: MessageRecord::from_json(json!({
                "id":"assistant","sessionID":"session","role":"assistant","time":{"created":10}
            })).expect("assistant"),
            parts: vec![part(json!({"status":"pending","input":{"command":"inspect"},"dispatchTracked":true}))],
            persisted_at_ms:10,context_limit:None,
        }).await.expect("assistant step");
        let changed =
            part(json!({"status":"pending","input":{"command":"different"},"dispatchedAtMs":20}));
        assert!(
            store
                .commit_tool_parts(&scope, &[changed], ToolPartCommitKind::Dispatched, 20)
                .await
                .is_err()
        );
        let admitted = part(
            json!({"status":"pending","input":{"command":"inspect"},"dispatchTracked":true,"dispatchedAtMs":20}),
        );
        store
            .commit_tool_parts(
                &scope,
                std::slice::from_ref(&admitted),
                ToolPartCommitKind::Dispatched,
                20,
            )
            .await
            .expect("first dispatch");
        assert!(
            store
                .commit_tool_parts(&scope, &[admitted], ToolPartCommitKind::Dispatched, 21)
                .await
                .is_err()
        );
        let receipt = part(
            json!({"status":"completed","input":{"command":"inspect"},"output":"observed result"}),
        );
        store
            .commit_tool_parts(
                &scope,
                std::slice::from_ref(&receipt),
                ToolPartCommitKind::Result,
                30,
            )
            .await
            .expect("receipt");
        store
            .commit_tool_parts(&scope, &[receipt], ToolPartCommitKind::Result, 30)
            .await
            .expect("same receipt");
        let forged = part(
            json!({"status":"completed","input":{"command":"inspect"},"output":"different result"}),
        );
        assert!(
            store
                .commit_tool_parts(&scope, &[forged], ToolPartCommitKind::Result, 31)
                .await
                .is_err()
        );
    }
    assert_eq!(
        MessageStore::new(&connection)
            .part("tool-part")
            .expect("durable part")
            .data["state"]["output"],
        "observed result"
    );
}

#[tokio::test]
async fn an_attempt_event_and_its_retry_state_roll_back_together() {
    let mut connection = fixture();
    let checkpoint = zuno_db::provider_backoff::ProviderBackoffCheckpoint {
        session_id: "session".to_owned(),
        request_id: "request".to_owned(),
        turn_id: "turn".to_owned(),
        failed_attempt: 1,
        next_attempt: 2,
        max_attempts: 3,
        reason: "transient".to_owned(),
        delay_ms: 100,
        retry_at_ms: 101,
        scheduled_at_ms: 1,
    };
    zuno_db::provider_backoff::schedule(&connection, &checkpoint).expect("prior backoff");
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_retry_clear BEFORE DELETE ON provider_retry_backoff
         BEGIN SELECT RAISE(ABORT,'injected retry update failure'); END;",
        )
        .expect("fault injection");
    {
        let store = SqliteTurnPersistence::new(&mut connection);
        let event = NewSessionEvent::new(
            "session.provider.attempt",
            json!({"requestID":"request","status":"started"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .expect("event");
        assert!(
            store
                .append_event(&scope(), event, ProviderEventUpdate::AttemptStarted)
                .await
                .is_err()
        );
    }
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM event WHERE aggregate_id='session' AND type='session.provider.attempt.1'",
        [],|row|row.get(0),
    ).expect("event count");
    assert_eq!(count, 0);
    assert_eq!(
        zuno_db::provider_backoff::get(&connection, "session").expect("retry state"),
        Some(checkpoint)
    );
}

#[tokio::test]
async fn a_failed_parallel_result_batch_preserves_every_previous_part() {
    let mut connection = fixture();
    let first = part(json!({"status":"pending","input":{}}));
    let mut second = first.clone();
    second.id = "second-part".to_owned();
    second.data["callID"] = json!("second-call");
    {
        let store = SqliteTurnPersistence::new(&mut connection);
        store.commit_assistant(&scope(), &AssistantCommit {
            message: MessageRecord::from_json(json!({
                "id":"assistant","sessionID":"session","role":"assistant","time":{"created":10}
            })).unwrap(),
            parts: vec![first.clone(), second.clone()],
            persisted_at_ms:10, context_limit:None,
        }).await.unwrap();
    }
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_second_result BEFORE UPDATE ON part WHEN NEW.id='second-part'
         BEGIN SELECT RAISE(ABORT,'injected second result failure'); END;",
        )
        .unwrap();
    {
        let store = SqliteTurnPersistence::new(&mut connection);
        let mut results = vec![first, second];
        for result in &mut results {
            result.data["state"] = json!({"status":"completed","input":{},"output":"done"});
        }
        assert!(
            store
                .commit_tool_parts(&scope(), &results, ToolPartCommitKind::Result, 20)
                .await
                .is_err()
        );
    }
    for id in ["tool-part", "second-part"] {
        assert_eq!(
            MessageStore::new(&connection).part(id).unwrap().data["state"]["status"],
            "pending"
        );
    }
}
