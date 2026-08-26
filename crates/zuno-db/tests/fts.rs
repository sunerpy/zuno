use serde_json::json;
use zuno_db::fts::{SearchFlavor, ensure, search, search_with};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};

const SESSION_ID: &str = "ses_fts_root";
const MESSAGE_ID: &str = "msg_fts_anchor";

fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-fts', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('ses_fts_root', 'project-fts', 'root', '/workspace', \
                     'FTS root', '1', 1, 1);",
        )
        .expect("seed project and session");
    connection
}

fn write_message_named(connection: &Connection, message_id: &str) {
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": 10 },
        "agent": "build",
        "model": { "providerID": "test", "modelID": "test" },
    }))
    .expect("valid message");
    MessageStore::new(connection)
        .put_message_at(&message, 10)
        .expect("write message");
}

fn write_message(connection: &Connection) {
    write_message_named(connection, MESSAGE_ID);
}

fn write_part(connection: &Connection, id: &str, value: serde_json::Value) {
    let mut object = value.as_object().expect("part object").clone();
    object.insert("id".to_owned(), json!(id));
    object.insert("sessionID".to_owned(), json!(SESSION_ID));
    object.insert("messageID".to_owned(), json!(MESSAGE_ID));
    let part = PartRecord::from_json(serde_json::Value::Object(object), 11).expect("valid part");
    MessageStore::new(connection)
        .put_part_at(&part, 11)
        .expect("write part");
}

fn has_object(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )
        .expect("inspect sqlite schema")
}

#[test]
fn fts_is_opt_in_and_ensure_is_idempotent() {
    let mut connection = seeded();

    assert!(!has_object(&connection, "message_fts"));
    assert!(!has_object(&connection, "message_fts_trigram"));

    ensure(&mut connection).expect("create FTS objects");
    ensure(&mut connection).expect("creating FTS objects twice is safe");

    assert!(has_object(&connection, "message_fts"));
    assert!(has_object(&connection, "message_fts_trigram"));
}

#[test]
fn fts_finds_english_and_cjk_phrases_through_the_expected_tokenizers() {
    let mut connection = seeded();
    write_message(&connection);
    write_part(
        &connection,
        "prt_text",
        json!({
            "type": "text",
            "text": "retry the socket after the database handshake; 数据库连接超时后重试",
        }),
    );
    ensure(&mut connection).expect("build FTS indexes");

    let english = search(&connection, "database handshake", 10).expect("search English");
    let chinese = search(&connection, "数据库连接超时", 10).expect("search Chinese");

    assert_eq!(english.len(), 1);
    assert_eq!(english[0].message_id, MESSAGE_ID);
    assert_eq!(english[0].flavor, SearchFlavor::Unicode);
    assert_eq!(chinese.len(), 1);
    assert_eq!(chinese[0].message_id, MESSAGE_ID);
    assert_eq!(chinese[0].flavor, SearchFlavor::Trigram);
}

#[test]
fn fts_keeps_tool_output_in_main_but_out_of_trigram() {
    let mut connection = seeded();
    write_message(&connection);
    write_part(
        &connection,
        "prt_tool",
        json!({
            "type": "tool",
            "callID": "call_1",
            "tool": "shell",
            "state": {
                "status": "completed",
                "input": { "command": "diagnose" },
                "output": "diagnostic avalanche 数据库连接超时",
                "title": "diagnose",
                "metadata": {},
                "time": { "start": 11, "end": 12 }
            }
        }),
    );
    ensure(&mut connection).expect("build FTS indexes");

    let main = search_with(
        &connection,
        "diagnostic avalanche",
        SearchFlavor::Unicode,
        10,
    )
    .expect("search main FTS");
    let trigram = search_with(&connection, "数据库连接超时", SearchFlavor::Trigram, 10)
        .expect("search trigram FTS");

    assert_eq!(main.len(), 1, "tool output remains searchable in main FTS");
    assert!(
        trigram.is_empty(),
        "tool output must not consume the expensive trigram index"
    );
}

#[test]
fn fts_triggers_follow_part_updates_and_deletes() {
    let mut connection = seeded();
    write_message(&connection);
    write_part(
        &connection,
        "prt_mutable",
        json!({ "type": "text", "text": "obsolete semaphore wording" }),
    );
    ensure(&mut connection).expect("build FTS indexes");

    connection
        .execute(
            "UPDATE part SET data = json_set(data, '$.text', 'replacement semaphore wording') \
             WHERE id = 'prt_mutable'",
            [],
        )
        .expect("update indexed part");
    assert!(
        search(&connection, "obsolete", 10)
            .expect("search old value")
            .is_empty()
    );
    assert_eq!(
        search(&connection, "replacement", 10)
            .expect("search replacement")
            .len(),
        1
    );

    write_message_named(&connection, "msg_fts_destination");
    connection
        .execute(
            "UPDATE part SET message_id = 'msg_fts_destination' WHERE id = 'prt_mutable'",
            [],
        )
        .expect("move indexed part to another message");
    let moved = search(&connection, "replacement", 10).expect("search moved value");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].message_id, "msg_fts_destination");

    connection
        .execute("DELETE FROM part WHERE id = 'prt_mutable'", [])
        .expect("delete indexed part");
    assert!(
        search(&connection, "replacement", 10)
            .expect("search deleted value")
            .is_empty()
    );
}

#[test]
fn fts_survives_message_cascade_deletion() {
    let mut connection = seeded();
    write_message(&connection);
    write_part(
        &connection,
        "prt_cascade",
        json!({ "type": "text", "text": "cascade-only phrase" }),
    );
    ensure(&mut connection).expect("build FTS indexes");

    connection
        .execute("DELETE FROM message WHERE id = ?1", [MESSAGE_ID])
        .expect("cascade message deletion through its parts");

    assert!(
        search(&connection, "cascade", 10)
            .expect("search after cascade")
            .is_empty()
    );
}

#[test]
fn fts_keeps_pre_compaction_text_searchable() {
    let mut connection = seeded();
    write_message(&connection);
    write_part(
        &connection,
        "prt_history",
        json!({ "type": "text", "text": "historical quasar decision" }),
    );
    write_part(
        &connection,
        "prt_compaction",
        json!({
            "type": "compaction",
            "auto": true,
            "overflow": false,
            "tail_start_id": MESSAGE_ID,
        }),
    );
    ensure(&mut connection).expect("build FTS indexes");

    let hits = search(&connection, "historical quasar", 10).expect("search old text");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, MESSAGE_ID);
}
