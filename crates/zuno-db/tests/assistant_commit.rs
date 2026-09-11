use serde_json::json;
use zuno_db::assistant_commit::{AssistantCommit, commit_assistant};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_paths::DbLocation;
use zuno_types::identity::{PrincipalId, PrincipalKey, PrincipalScope, TenantId};

fn fixture() -> Connection {
    let mut connection = open::open(&DbLocation::Memory).unwrap();
    migration::apply(&mut connection).unwrap();
    connection.execute_batch(
        "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('p','/workspace',1,1,'[]');
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
           VALUES('session','p','one','/workspace','One','preview',1,1);
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
           VALUES('other','p','two','/workspace','Other','preview',1,1);",
    ).unwrap();
    connection
}
fn commit(message: &str, part: &str) -> AssistantCommit {
    AssistantCommit {
        message:MessageRecord::from_json(json!({
            "id":message,"sessionID":"session","role":"assistant","time":{"created":10,"completed":11},
            "cost":0.25,"tokens":{"input":10,"output":3,"reasoning":2,"cache":{"read":5,"write":0},"accounting":"cache-beside-input"}
        })).unwrap(),
        parts:vec![PartRecord::from_json(json!({
            "id":part,"sessionID":"session","messageID":message,"type":"text","text":"Result"
        }),10).unwrap()],
        persisted_at_ms:11,context_limit:Some(100_000),
    }
}
fn usage(connection: &Connection) -> (f64, i64, i64, i64, i64) {
    connection.query_row("SELECT cost,tokens_input,tokens_output,tokens_reasoning,tokens_cache_read FROM session WHERE id='session'",
        [],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap()
}

#[test]
fn repeating_a_step_preserves_parts_and_does_not_charge_usage_twice() {
    let connection = fixture();
    let step = commit("assistant", "part");
    let owner = PrincipalScope::local().owner();
    commit_assistant(&connection, &owner, &step).unwrap();
    let first = usage(&connection);
    assert!(first.0 > 0.0);
    commit_assistant(&connection, &owner, &step).unwrap();
    assert_eq!(usage(&connection), first);
    assert_eq!(
        MessageStore::new(&connection).part("part").unwrap().data["text"],
        "Result"
    );
    let mut replacement = step;
    replacement.message.data["cost"] = json!(99);
    assert!(commit_assistant(&connection, &owner, &replacement).is_err());
    assert_eq!(usage(&connection), first);
}

#[test]
fn a_part_failure_rolls_back_the_message_and_usage() {
    let connection = fixture();
    let before = usage(&connection);
    connection
        .execute_batch(
            "CREATE TRIGGER refuse_part BEFORE INSERT ON part WHEN NEW.id='failure'
         BEGIN SELECT RAISE(ABORT,'injected part failure'); END;",
        )
        .unwrap();
    assert!(
        commit_assistant(
            &connection,
            &PrincipalScope::local().owner(),
            &commit("failed", "failure")
        )
        .is_err()
    );
    assert!(
        MessageStore::new(&connection)
            .find_message("failed")
            .unwrap()
            .is_none()
    );
    assert_eq!(usage(&connection), before);
}

#[test]
fn another_sessions_part_identity_and_another_owner_are_not_writable() {
    let connection = fixture();
    let store = MessageStore::new(&connection);
    store
        .put_message(
            &MessageRecord::from_json(json!({
                "id":"other-message","sessionID":"other","role":"user","time":{"created":1}
            }))
            .unwrap(),
        )
        .unwrap();
    store.put_part(&PartRecord::from_json(json!({
        "id":"shared-id","sessionID":"other","messageID":"other-message","type":"text","text":"Private"
    }),1).unwrap()).unwrap();
    assert!(
        commit_assistant(
            &connection,
            &PrincipalScope::local().owner(),
            &commit("attempt", "shared-id")
        )
        .is_err()
    );
    assert_eq!(store.part("shared-id").unwrap().data["text"], "Private");
    assert!(store.find_message("attempt").unwrap().is_none());
    let other = PrincipalKey {
        tenant_id: TenantId::new("foreign").unwrap(),
        principal_id: PrincipalId::new("other").unwrap(),
    };
    assert!(commit_assistant(&connection, &other, &commit("denied", "new-part")).is_err());
    assert!(store.find_message("denied").unwrap().is_none());
}
