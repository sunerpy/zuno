use zuno_db::message::MessageStore;
use zuno_db::{Connection, migration};

#[test]
fn learning_reads_only_its_completed_turn_and_latest_user_text() {
    let mut connection = Connection::open_in_memory().expect("database");
    migration::apply(&mut connection).expect("schema");
    connection.execute_batch(r#"
        INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
        VALUES ('p', '/workspace', 1, 1, '[]');
        INSERT INTO session
          (id, project_id, slug, directory, title, version, time_created, time_updated)
        VALUES ('s', 'p', 'session', '/workspace', 'window test', 'test', 1, 1);
        INSERT INTO message (id, session_id, time_created, time_updated, data)
        VALUES ('old-user', 's', 1, 1, '{"role":"user"}'),
               ('old-assistant', 's', 2, 2, '{"role":"assistant"}'),
               ('user', 's', 3, 3, '{"role":"user"}'),
               ('assistant', 's', 4, 4, '{"role":"assistant"}'),
               ('later-user', 's', 5, 5, '{"role":"user"}');
        INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
        VALUES ('old-part', 'old-assistant', 's', 2, 2,
                '{"type":"not-a-supported-part","text":"unrelated retired history"}'),
               ('user-part', 'user', 's', 3, 3, '{"type":"text","text":"Verify migrations."}'),
               ('answer-part', 'assistant', 's', 4, 4, '{"type":"text","text":"Verification passed."}'),
               ('later-part', 'later-user', 's', 5, 5, '{"type":"text","text":"现在检查另一个问题。"}');
    "#).expect("history");
    let store = MessageStore::new(&connection);
    assert!(
        store.hydrate_session("s").is_err(),
        "control proves the unrelated row is not decodable"
    );
    let selected = store
        .hydrate_completed_turn("s", "assistant")
        .expect("targeted turn");
    assert_eq!(
        selected
            .iter()
            .map(|message| message.info.id.as_str())
            .collect::<Vec<_>>(),
        ["user", "assistant"],
    );
    assert_eq!(
        store.latest_user_text("s").expect("latest text"),
        "现在检查另一个问题。"
    );
    assert!(
        store
            .hydrate_completed_turn("other-session", "assistant")
            .is_err()
    );
}

#[test]
fn latest_user_text_has_a_bounded_result_even_for_large_input() {
    let mut connection = Connection::open_in_memory().expect("database");
    migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            r#"
        INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
        VALUES ('p', '/workspace', 1, 1, '[]');
        INSERT INTO session
          (id, project_id, slug, directory, title, version, time_created, time_updated)
        VALUES ('s', 'p', 'session', '/workspace', 'window test', 'test', 1, 1);
        INSERT INTO message (id, session_id, time_created, time_updated, data)
        VALUES ('user', 's', 1, 1, '{"role":"user"}');
    "#,
        )
        .expect("session");
    let data = serde_json::json!({"type": "text", "text": "数据库".repeat(10_000)});
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES ('part', 'user', 's', 1, 1, ?1)",
            [data.to_string()],
        )
        .expect("large input");
    let store = MessageStore::new(&connection);
    let text = store.latest_user_text("s").expect("bounded query");
    assert_eq!(text.chars().count(), 8_192);
    assert_eq!(store.query_count(), 1);
}
