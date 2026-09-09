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

#[test]
fn compaction_markers_do_not_replace_the_user_anchor_or_recall_query() {
    let mut connection = Connection::open_in_memory().expect("database");
    migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            r#"
        INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
        VALUES ('p', '/workspace', 1, 1, '[]');
        INSERT INTO session
          (id, project_id, slug, directory, title, version, time_created, time_updated)
        VALUES ('s', 'p', 'session', '/workspace', 'compaction anchor', 'test', 1, 1);
        INSERT INTO message (id, session_id, time_created, time_updated, data)
        VALUES ('user', 's', 1, 1, '{"role":"user"}'),
               ('current-marker', 's', 2, 2, '{"role":"user","mode":"compaction"}'),
               ('released-marker', 's', 3, 3, '{"role":"user"}'),
               ('completed', 's', 4, 4, '{"role":"assistant","finish":"stop"}');
        INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
        VALUES ('user-text', 'user', 's', 1, 1,
                '{"type":"text","text":"initial input"}'),
               ('current-part', 'current-marker', 's', 2, 2,
                '{"type":"compaction","auto":true}'),
               ('released-part', 'released-marker', 's', 3, 3,
                '{"type":"compaction","auto":true}');
        "#,
        )
        .expect("real input followed by compaction bookkeeping");
    for input in [
        "增加 JSON 导出功能，复用现有接口。",
        "定位断连原因，先复现，不要直接修改代码。",
        "比较两种缓存设计，只做调研并列出证据。",
        "制定迁移计划，暂时不要执行迁移。",
    ] {
        connection
            .execute(
                "UPDATE part SET data = json_set(data, '$.text', ?1) WHERE id = 'user-text'",
                [input],
            )
            .expect("the actual user request defines the task");
        let store = MessageStore::new(&connection);
        assert_eq!(
            store
                .latest_user_message_id("s")
                .expect("input anchor")
                .as_deref(),
            Some("user")
        );
        assert_eq!(store.latest_user_text("s").expect("recall query"), input);
        let (start, _) = store.completed_turn_bounds("s", "completed").unwrap();
        assert_eq!(
            start.1, "user",
            "learning must retain the original task after compaction"
        );
        assert_eq!(
            store
                .latest_user_message_id("other-session")
                .expect("isolated session"),
            None
        );
    }
}
