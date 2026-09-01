//! The `export` envelope, its ordering, its redaction pass and the import that
//! reads it back.
//!
//! Every test here is named `session_export_*` so that
//! `cargo test -p zuno-db session_export` selects them rather than reporting a
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

use base64::Engine as _;
use rusqlite::Connection;
use serde_json::{Value, json};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::session_export::{self, ImportTarget};
use zuno_db::{migration, open};

const SESSION: &str = "ses_export00000000000000000000ab";
const USER: &str = "msg_export00000000000000000000us";
const ASSISTANT: &str = "msg_export00000000000000000000as";
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn database() -> Connection {
    let mut connection =
        open::open(&zuno_paths::DbLocation::Memory).expect("open a private in-memory database");
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

fn seeded_with_notes() -> Connection {
    let connection = seeded();
    zuno_db::continuity::ensure_schema(&connection).expect("create continuity tables");
    let content = "CI evidence: run 41744 passed.";
    let content_sha256 = zuno_orchestration::sha256_text(content);
    connection
        .execute(
            "INSERT INTO session_note
               (session_id, agent, name, revision, content, content_sha256,
                time_created, time_updated)
             VALUES (?1, 'build', 'release/evidence.md', 2, ?2, ?3, 1300, 1400)",
            rusqlite::params![SESSION, content, content_sha256],
        )
        .expect("seed note");
    connection
        .execute_batch(
            "INSERT INTO session_note_operation
               (session_id, agent, call_id, request_sha256, action, name,
                result_revision, result_content_sha256, time_created)
             VALUES
               ('ses_export00000000000000000000ab', 'build', 'call-note-1',
                'request-one', 'write', 'release/evidence.md', 1, 'first-sha', 1300),
               ('ses_export00000000000000000000ab', 'build', 'call-note-2',
                'request-two', 'append', 'release/evidence.md', 2,
                'd59ce4192cf3666eb92f6d7d7f16a244a448347853415c292eae377802c509e1', 1400);",
        )
        .expect("seed note operations");
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
fn session_export_reinlines_durable_images_and_leaves_legacy_inline_rows_readable() {
    let connection = seeded();
    let root = tempfile::tempdir().expect("attachment root");
    let attachments = zuno_attachment::AttachmentStore::new(
        root.path(),
        "export",
        zuno_attachment::ImageAdmissionPolicy::default(),
    )
    .expect("attachment store");
    let reference = attachments
        .admit_base64(PNG_BASE64, Some("pixel.png".to_owned()))
        .expect("admit image");
    write_part(
        &connection,
        "prt_export0000000000000000000010",
        USER,
        json!({
            "type": "file",
            "filename": reference.filename,
            "mime": reference.media_type,
            "attachment": reference.clone()
        }),
        1_110,
    );
    write_part(
        &connection,
        "prt_export0000000000000000000011",
        USER,
        json!({
            "type": "file",
            "filename": "legacy.png",
            "mime": "image/png",
            "data": "legacy-inline",
            "url": "data:image/png;base64,legacy-inline"
        }),
        1_111,
    );

    let document = session_export::export_with_attachments(&connection, SESSION, &attachments)
        .expect("portable export");
    let parts = document
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .collect::<Vec<_>>();
    let exported = parts
        .iter()
        .find(|part| part["id"] == "prt_export0000000000000000000010")
        .expect("durable image part");
    assert!(exported.get("attachment").is_none());
    assert_eq!(exported["mime"], reference.media_type);
    let data = exported["data"].as_str().expect("inline base64");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("decode exported image"),
        attachments.read(&reference).expect("canonical object")
    );
    assert_eq!(
        exported["url"],
        format!("data:{};base64,{data}", reference.media_type)
    );

    let legacy = parts
        .iter()
        .find(|part| part["id"] == "prt_export0000000000000000000011")
        .expect("legacy inline part");
    assert_eq!(legacy["data"], "legacy-inline");
    assert_eq!(legacy["url"], "data:image/png;base64,legacy-inline");
}

#[test]
fn session_export_import_admits_portable_inline_images_before_persistence() {
    let source = seeded();
    let source_root = tempfile::tempdir().expect("source attachment root");
    let source_store = zuno_attachment::AttachmentStore::new(
        source_root.path(),
        "source",
        zuno_attachment::ImageAdmissionPolicy::default(),
    )
    .expect("source attachment store");
    let reference = source_store
        .admit_base64(PNG_BASE64, Some("pixel.png".to_owned()))
        .expect("source image");
    write_part(
        &source,
        "prt_export0000000000000000000012",
        USER,
        json!({
            "type": "file",
            "filename": reference.filename,
            "mime": reference.media_type,
            "attachment": reference
        }),
        1_112,
    );
    let document = session_export::export_with_attachments(&source, SESSION, &source_store)
        .expect("portable export")
        .to_json()
        .expect("encode export");

    let mut target = database();
    let target_root = tempfile::tempdir().expect("target attachment root");
    let target_store = zuno_attachment::AttachmentStore::new(
        target_root.path(),
        "target",
        zuno_attachment::ImageAdmissionPolicy::default(),
    )
    .expect("target attachment store");
    let transaction = target.transaction().expect("begin import");
    session_export::import_with_attachments(
        &transaction,
        &document,
        &ImportTarget {
            project_id: "prj_export".to_owned(),
            directory: "/srv/imported".to_owned(),
            path: String::new(),
        },
        &target_store,
    )
    .expect("import portable image");
    transaction.commit().expect("commit import");

    let persisted = MessageStore::new(&target)
        .hydrate_session(SESSION)
        .expect("imported transcript")
        .into_iter()
        .flat_map(|message| message.parts)
        .find(|part| part.id == "prt_export0000000000000000000012")
        .expect("imported image part");
    assert!(persisted.data.get("data").is_none());
    assert!(persisted.data.get("url").is_none());
    let reference = serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(
        persisted.data["attachment"].clone(),
    )
    .expect("durable imported reference");
    target_store
        .read(&reference)
        .expect("imported canonical object");
}

#[test]
fn session_export_reports_an_unknown_session_as_not_found() {
    let connection = database();
    let error = session_export::export(&connection, "ses_missing000000000000000000ab")
        .expect_err("an unknown session cannot be exported");
    assert!(
        matches!(&error, zuno_error::DbError::NotFound { table, .. } if table == "session"),
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
    let source = seeded_with_notes();
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
    assert_eq!(imported.notes, 1);
    assert_eq!(imported.note_operations, 2);

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
fn session_export_sanitize_redacts_notes_and_removes_the_idempotency_ledger() {
    let connection = seeded_with_notes();
    let document = session_export::export(&connection, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");

    let sanitized = session_export::sanitize(document);

    assert_eq!(sanitized["notes"][0]["agent"], json!("redacted-agent"));
    assert_eq!(sanitized["notes"][0]["name"], json!("redacted-note-0.md"));
    assert_eq!(
        sanitized["notes"][0]["content"],
        json!("[redacted:note-content:0]")
    );
    assert_eq!(
        sanitized["notes"][0]["contentSha256"],
        json!(zuno_orchestration::sha256_text("[redacted:note-content:0]"))
    );
    assert_eq!(sanitized["noteOperations"], json!([]));
    assert!(
        !sanitized.to_string().contains("CI evidence"),
        "sanitized export leaked note content"
    );
}

#[test]
fn session_export_sanitize_preserves_note_scope_quotas_across_agents() {
    let source = seeded();
    let mut document = session_export::export(&source, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");
    let mut notes = (0..zuno_db::continuity::MAX_NOTE_DOCUMENTS)
        .map(|index| {
            json!({
                "agent": "build",
                "name": format!("build/{index:03}.md"),
                "revision": 1,
                "content": "build secret",
                "contentSha256": "ignored",
                "timeCreated": 1,
                "timeUpdated": 1,
            })
        })
        .collect::<Vec<_>>();
    notes.push(json!({
        "agent": "plan",
        "name": "plan/summary.md",
        "revision": 1,
        "content": "plan secret",
        "contentSha256": "ignored",
        "timeCreated": 1,
        "timeUpdated": 1,
    }));
    document["notes"] = Value::Array(notes);
    document["noteOperations"] = json!([]);

    let sanitized = session_export::sanitize(document);
    let agents = sanitized["notes"]
        .as_array()
        .expect("sanitized notes")
        .iter()
        .filter_map(|note| note["agent"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(agents.len(), 2, "distinct Agent scopes must stay distinct");

    let mut target = database();
    let transaction = target.transaction().expect("begin import");
    let imported = session_export::import(
        &transaction,
        &sanitized,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/sanitized"),
            path: String::new(),
        },
    )
    .expect("sanitized multi-Agent notes remain importable");
    transaction.commit().expect("commit import");
    assert_eq!(imported.notes, 101);
}

#[test]
fn session_export_import_rejects_invalid_note_names_and_quota_bypass() {
    let source = seeded();
    let mut document = session_export::export(&source, SESSION)
        .expect("export")
        .to_json()
        .expect("encode");
    document["notes"] = json!("not-an-array");
    let mut target = database();
    let transaction = target.transaction().expect("begin");
    let error = session_export::import(
        &transaction,
        &document,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/export"),
            path: String::new(),
        },
    )
    .expect_err("a malformed Notes envelope cannot be silently dropped");
    assert!(matches!(error, zuno_error::DbError::Decode { .. }));
    transaction.rollback().expect("rollback malformed envelope");

    document["notes"] = json!([{
        "agent": "build",
        "name": "../host-path",
        "revision": 1,
        "content": "bad",
        "contentSha256": "ignored",
        "timeCreated": 1,
        "timeUpdated": 1
    }]);
    let transaction = target.transaction().expect("begin");
    let error = session_export::import(
        &transaction,
        &document,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/export"),
            path: String::new(),
        },
    )
    .expect_err("host paths cannot enter the note store through import");
    assert!(matches!(error, zuno_error::DbError::Decode { .. }));
    transaction.rollback().expect("rollback invalid name");

    document["notes"][0]["name"] = json!("valid.md");
    document["notes"][0]["content"] =
        json!("x".repeat((zuno_db::continuity::MAX_NOTE_DOCUMENT_BYTES + 1) as usize));
    let transaction = target.transaction().expect("begin");
    let error = session_export::import(
        &transaction,
        &document,
        &ImportTarget {
            project_id: String::from("prj_export"),
            directory: String::from("/srv/export"),
            path: String::new(),
        },
    )
    .expect_err("imports cannot bypass the document quota");
    assert!(matches!(error, zuno_error::DbError::Decode { .. }));
    transaction.rollback().expect("rollback oversized note");
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
            matches!(&error, zuno_error::DbError::Decode { .. }),
            "{malformed}: {error:?}"
        );
        transaction.rollback().expect("rollback");
    }
}
