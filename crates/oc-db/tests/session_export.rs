//! The `export` envelope, its ordering, its redaction pass and the import that
//! reads it back.
//!
//! Every test here is named `session_export_*` so that
//! `cargo test -p oc-db session_export` selects them rather than reporting a
//! vacuous `0 passed; N filtered out`.
//!
//! Three of these exist to fail under a specific mutation rather than to
//! describe behaviour:
//!
//! * [`session_export_orders_messages_oldest_first_and_parts_by_id`] writes the
//!   two messages in **reverse** chronological order, so an export that returns
//!   rows in insertion order rather than by `time_created` fails.
//! * [`session_export_sanitize_leaves_a_blank_string_and_an_empty_object_alone`]
//!   pins the two rules in `cli/cmd/export.ts:11-18` that look like oversights,
//!   so "redact everything" is not a passing simplification.
//! * [`session_export_import_does_not_overwrite_an_existing_message`] proves the
//!   `ON CONFLICT DO NOTHING` writer is the one in use; swapping it for the
//!   upserting `put_message_at` makes import a silent editor of live history and
//!   fails here.

use oc_db::message::{MessageRecord, MessageStore, PartRecord};
use oc_db::session_export::{self, ImportTarget};
use oc_db::{migration, open};
use rusqlite::Connection;
use serde_json::{Value, json};

const SESSION: &str = "ses_export00000000000000000000ab";
const USER: &str = "msg_export00000000000000000000us";
const ASSISTANT: &str = "msg_export00000000000000000000as";

fn database() -> Connection {
    let mut connection =
        open::open(&oc_paths::DbLocation::Memory).expect("open a private in-memory database");
    migration::apply(&mut connection).expect("apply the schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_export', '/srv/export', 1, 1, '[]');",
        )
        .expect("seed a project");
    connection
}

fn seed_session(connection: &Connection, id: &str) {
    connection
        .execute(
            "INSERT INTO session \
             (id, project_id, slug, directory, path, title, version, cost, tokens_input, \
              tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, agent, \
              model, metadata, time_created, time_updated) \
             VALUES (?1, 'prj_export', 'exported', '/srv/export', '', 'An exported session', \
                     '1.18.13', 1.5, 10, 20, 0, 1, 2, 'build', \
                     '{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\"}', \
                     '{\"ticket\":\"OC-1\"}', 1000, 2000)",
            rusqlite::params![id],
        )
        .expect("seed a session");
}

fn write_message(connection: &Connection, id: &str, role: &str, created: i64) {
    let payload = if role == "user" {
        json!({
            "id": id,
            "sessionID": SESSION,
            "role": "user",
            "time": { "created": created },
            "system": "you are a careful engineer",
        })
    } else {
        json!({
            "id": id,
            "sessionID": SESSION,
            "role": "assistant",
            "time": { "created": created, "completed": created + 500 },
            "modelID": "claude-sonnet-4-5",
            "providerID": "anthropic",
            "path": { "cwd": "/srv/export", "root": "/srv/export" },
            "cost": 0.001,
            "tokens": { "input": 1.0, "output": 2.0, "reasoning": 0.0,
                        "cache": { "read": 0.0, "write": 0.0 } },
        })
    };
    let record = MessageRecord::from_json(payload).expect("split the message");
    MessageStore::new(connection)
        .put_message_at(&record, created)
        .expect("write the message");
}

fn write_part(connection: &Connection, id: &str, message_id: &str, payload: Value, created: i64) {
    let mut payload = payload;
    let object = payload.as_object_mut().expect("part payload is an object");
    object.insert("id".to_owned(), json!(id));
    object.insert("sessionID".to_owned(), json!(SESSION));
    object.insert("messageID".to_owned(), json!(message_id));
    let record = PartRecord::from_json(payload, created).expect("split the part");
    MessageStore::new(connection)
        .put_part_at(&record, created)
        .expect("write the part");
}

fn seeded() -> Connection {
    let connection = database();
    seed_session(&connection, SESSION);
    write_message(&connection, USER, "user", 1100);
    write_part(
        &connection,
        "prt_export0000000000000000000001",
        USER,
        json!({ "type": "text", "text": "audit the parser" }),
        1101,
    );
    write_message(&connection, ASSISTANT, "assistant", 1200);
    write_part(
        &connection,
        "prt_export0000000000000000000002",
        ASSISTANT,
        json!({ "type": "text", "text": "reading src/lib.rs" }),
        1201,
    );
    connection
}

#[test]
fn session_export_emits_the_upstream_envelope() {
    let connection = seeded();
    let document = session_export::export(&connection, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");

    let info = document.get("info").expect("info");
    assert_eq!(info.get("id").and_then(Value::as_str), Some(SESSION));
    assert_eq!(
        info.get("projectID").and_then(Value::as_str),
        Some("prj_export")
    );
    assert_eq!(
        info.get("title").and_then(Value::as_str),
        Some("An exported session")
    );
    assert_eq!(info.get("cost"), Some(&json!(1.5)));
    assert_eq!(
        info.get("tokens").and_then(|tokens| tokens.get("cache")),
        Some(&json!({ "read": 1, "write": 2 }))
    );
    // `model` and `metadata` are opaque JSON text in the column and parsed values
    // on the wire, so a string here would mean the export leaked the storage form.
    assert_eq!(
        info.get("model"),
        Some(&json!({ "id": "claude-sonnet-4-5", "providerID": "anthropic" }))
    );
    assert_eq!(info.get("metadata"), Some(&json!({ "ticket": "OC-1" })));
    // A listing carries `project`; a bare `Info` does not (`export.ts:284`).
    assert!(info.get("project").is_none(), "{info}");

    let messages = document
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    assert_eq!(messages.len(), 2);
    for message in messages {
        let info = message.get("info").expect("message info");
        assert_eq!(info.get("sessionID").and_then(Value::as_str), Some(SESSION));
        assert!(info.get("id").and_then(Value::as_str).is_some());
        let parts = message
            .get("parts")
            .and_then(Value::as_array)
            .expect("parts array");
        for part in parts {
            assert_eq!(part.get("sessionID").and_then(Value::as_str), Some(SESSION));
            assert_eq!(
                part.get("messageID").and_then(Value::as_str),
                info.get("id").and_then(Value::as_str)
            );
        }
    }
}

#[test]
fn session_export_orders_messages_oldest_first_and_parts_by_id() {
    let connection = database();
    seed_session(&connection, SESSION);
    // Written newest-first so insertion order and chronological order disagree.
    write_message(&connection, ASSISTANT, "assistant", 1200);
    write_message(&connection, USER, "user", 1100);
    for (index, id) in [
        "prt_export0000000000000000000003",
        "prt_export0000000000000000000001",
        "prt_export0000000000000000000002",
    ]
    .into_iter()
    .enumerate()
    {
        write_part(
            &connection,
            id,
            USER,
            json!({ "type": "text", "text": "a part" }),
            1_500 - i64::try_from(index).expect("index fits"),
        );
    }

    let document = session_export::export(&connection, SESSION).expect("export");
    let ids: Vec<&str> = document
        .messages
        .iter()
        .map(|message| {
            message
                .info
                .get("id")
                .and_then(Value::as_str)
                .expect("message id")
        })
        .collect();
    assert_eq!(ids, vec![USER, ASSISTANT]);

    let part_ids: Vec<&str> = document.messages[0]
        .parts
        .iter()
        .map(|part| part.get("id").and_then(Value::as_str).expect("part id"))
        .collect();
    assert_eq!(
        part_ids,
        vec![
            "prt_export0000000000000000000001",
            "prt_export0000000000000000000002",
            "prt_export0000000000000000000003",
        ]
    );
}

#[test]
fn session_export_reports_an_unknown_session_as_not_found() {
    let connection = database();
    let error = session_export::export(&connection, "ses_missing000000000000000000ab")
        .expect_err("an unknown session cannot be exported");
    assert!(
        matches!(&error, oc_error::DbError::NotFound { table, .. } if table == "session"),
        "{error:?}"
    );
}

#[test]
fn session_export_sanitize_redacts_every_transcript_and_path_string() {
    let connection = seeded();
    let document = session_export::export(&connection, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");
    let sanitized = session_export::sanitize(document);
    let rendered = serde_json::to_string(&sanitized).expect("encode the redacted document");

    for leaked in [
        "An exported session",
        "/srv/export",
        "audit the parser",
        "reading src/lib.rs",
        "you are a careful engineer",
    ] {
        assert!(
            !rendered.contains(leaked),
            "sanitize leaked {leaked:?}:\n{rendered}"
        );
    }
    assert_eq!(
        sanitized
            .get("info")
            .and_then(|info| info.get("title"))
            .and_then(Value::as_str),
        Some(format!("[redacted:session-title:{SESSION}]").as_str())
    );
    // The identifiers a bug report is filed against must survive redaction.
    assert_eq!(
        sanitized
            .get("info")
            .and_then(|info| info.get("id"))
            .and_then(Value::as_str),
        Some(SESSION)
    );
}

#[test]
fn session_export_sanitize_leaves_a_blank_string_and_an_empty_object_alone() {
    let document = json!({
        "info": {
            "id": SESSION,
            "title": "   ",
            "directory": "",
        },
        "messages": [{
            "info": { "id": USER, "role": "user", "system": "" },
            "parts": [
                { "id": "prt_a", "type": "text", "text": "  ", "metadata": {} },
                { "id": "prt_b", "type": "text", "text": "secret", "metadata": { "k": 1 } },
            ],
        }],
    });
    let sanitized = session_export::sanitize(document);
    let info = sanitized.get("info").expect("info");
    assert_eq!(info.get("title").and_then(Value::as_str), Some("   "));
    assert_eq!(info.get("directory").and_then(Value::as_str), Some(""));

    let parts = sanitized["messages"][0]["parts"]
        .as_array()
        .expect("parts array");
    assert_eq!(parts[0].get("text").and_then(Value::as_str), Some("  "));
    assert_eq!(parts[0].get("metadata"), Some(&json!({})));
    assert_eq!(
        parts[1].get("text").and_then(Value::as_str),
        Some("[redacted:text:prt_b]")
    );
    assert_eq!(
        parts[1].get("metadata"),
        Some(&json!({ "redacted": "text-metadata:prt_b" }))
    );
}

#[test]
fn session_export_sanitize_covers_every_part_variant_it_names() {
    let document = json!({
        "info": { "id": SESSION },
        "messages": [{
            "info": { "id": ASSISTANT, "role": "assistant",
                      "path": { "cwd": "/srv/secret", "root": "/srv/secret" } },
            "parts": [
                { "id": "p1", "type": "reasoning", "text": "thinking about /etc/passwd" },
                { "id": "p2", "type": "file", "url": "data:text/plain;base64,aGk=",
                  "filename": "note.txt",
                  "source": { "type": "file", "path": "/srv/secret/note.txt",
                              "text": { "value": "hi", "start": 0, "end": 2 } } },
                { "id": "p3", "type": "subtask", "prompt": "audit", "description": "d",
                  "command": "run" },
                { "id": "p4", "type": "tool", "callID": "c", "tool": "read",
                  "state": { "status": "completed", "input": { "filePath": "/srv/secret" },
                             "output": "file body", "title": "note.txt" } },
                { "id": "p5", "type": "patch", "hash": "deadbeef",
                  "files": ["/srv/secret/a.rs"] },
                { "id": "p6", "type": "snapshot", "snapshot": "abc123" },
                { "id": "p7", "type": "agent", "name": "explore",
                  "source": { "type": "command", "value": "/audit" } },
            ],
        }],
    });
    let rendered = serde_json::to_string(&session_export::sanitize(document))
        .expect("encode the redacted document");
    for leaked in [
        "/etc/passwd",
        "aGk=",
        "note.txt",
        "/srv/secret",
        "file body",
        "deadbeef",
        "abc123",
        "/audit",
    ] {
        assert!(
            !rendered.contains(leaked),
            "sanitize leaked {leaked:?}:\n{rendered}"
        );
    }
}

#[test]
fn session_export_round_trips_through_import() {
    let source = seeded();
    let document = session_export::export(&source, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");

    let mut target = database();
    let transaction = target.transaction().expect("begin");
    let imported = session_export::import(
        &transaction,
        &document,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/elsewhere"),
            path: String::from("nested"),
        },
    )
    .expect("import");
    transaction.commit().expect("commit");

    assert_eq!(imported.session_id, SESSION);
    assert_eq!(imported.messages, 2);
    assert_eq!(imported.parts, 2);

    let reexported = session_export::export(&target, SESSION)
        .expect("re-export")
        .to_json()
        .expect("encode");
    // Everything but the three re-homed fields must survive the round trip.
    let mut expected = document.clone();
    let info = expected
        .get_mut("info")
        .and_then(Value::as_object_mut)
        .expect("info");
    info.insert("directory".to_owned(), json!("/srv/elsewhere"));
    info.insert("path".to_owned(), json!("nested"));
    assert_eq!(reexported, expected);
}

#[test]
fn session_export_import_does_not_overwrite_an_existing_message() {
    let source = seeded();
    let document = session_export::export(&source, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");

    let mut target = database();
    seed_session(&target, SESSION);
    write_message(&target, USER, "user", 9999);
    let live = MessageStore::new(&target)
        .message(USER)
        .expect("the live message");

    let transaction = target.transaction().expect("begin");
    session_export::import(
        &transaction,
        &document,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/export"),
            path: String::new(),
        },
    )
    .expect("import");
    transaction.commit().expect("commit");

    let after = MessageStore::new(&target)
        .message(USER)
        .expect("the message after import");
    assert_eq!(
        after, live,
        "import overwrote a message that already existed"
    );
}

#[test]
fn session_export_import_rejects_a_document_that_is_not_an_envelope() {
    let mut target = database();
    for malformed in [
        json!("a string"),
        json!({ "messages": [] }),
        json!({ "info": { "id": SESSION } }),
        json!({ "info": {}, "messages": [] }),
    ] {
        let transaction = target.transaction().expect("begin");
        let error = session_export::import(
            &transaction,
            &malformed,
            &ImportTarget {
                project_id: String::from("prj_export"),
                directory: String::from("/srv/export"),
                path: String::new(),
            },
        )
        .expect_err("a malformed document cannot be imported");
        assert!(
            matches!(&error, oc_error::DbError::Decode { .. }),
            "{malformed}: {error:?}"
        );
        transaction.rollback().expect("rollback");
    }
}
