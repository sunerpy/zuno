//! Message and part persistence against the real `data` column contract.
//!
//! Every payload here is shaped after `packages/schema/src/v1/session.ts` and,
//! where a variant appears in a real `opencode.db`, after the field set a real
//! row actually carries - which is not always the field set the schema declares.
//! The `file` payload below keeps `synthetic`, a key present on real `file` parts
//! and absent from `FilePart` at schema line 171; a strict typed decoder would
//! have dropped it and broken the round trip for every attachment a user has.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use zuno_db::message::{
    HYDRATION_CHUNK, MessageRecord, MessageRole, MessageStore, MessageWithParts, PART_KIND_COUNT,
    PartKind, PartRecord, created_after,
};
use zuno_db::{Connection, migration, open};
use zuno_error::DbError;

const SESSION_ID: &str = "ses_test000000000000000000000000";
const MESSAGE_ID: &str = "msg_test000000000000000000000000";

/// A database with the schema applied and one project/session row to hang
/// messages off, because `message.session_id` is a real foreign key.
fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-1', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);"
        ))
        .expect("seed a project and a session");
    connection
}

/// A user message payload, complete per `User` at schema line 332.
fn user_message(id: &str, created: i64) -> Value {
    json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
    })
}

/// An assistant message payload, complete per `Assistant` at schema line 453.
fn assistant_message(id: &str, created: i64) -> Value {
    json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created + 1_200 },
        "parentID": MESSAGE_ID,
        "modelID": "claude-sonnet-4-5",
        "providerID": "anthropic",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.014_25,
        "tokens": {
            "input": 1_024.0,
            "output": 256.0,
            "reasoning": 64.0,
            "cache": { "read": 4_096.0, "write": 512.0 },
        },
        "variant": "thinking",
        "finish": "stop",
    })
}

/// A full part payload for `kind`, identity keys included.
///
/// One arm per variant, matched exhaustively: a thirteenth variant added upstream
/// fails to compile here rather than going untested.
fn part_payload(kind: PartKind, part_id: &str) -> Value {
    let identity = |mut value: Value| {
        let object = value.as_object_mut().expect("payload is an object");
        object.insert("id".to_owned(), json!(part_id));
        object.insert("sessionID".to_owned(), json!(SESSION_ID));
        object.insert("messageID".to_owned(), json!(MESSAGE_ID));
        value
    };
    identity(match kind {
        PartKind::Text => json!({
            "type": "text",
            "text": "the quick brown fox\nwith a \"quoted\" tail and a \\ backslash",
            "synthetic": false,
            "ignored": false,
            "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_400_i64 },
            "metadata": { "providerMetadata": { "anthropic": { "cacheCreation": 12 } } },
        }),
        PartKind::Subtask => json!({
            "type": "subtask",
            "prompt": "audit the parser",
            "description": "parser audit",
            "agent": "explore",
            "model": { "providerID": "anthropic", "modelID": "claude-haiku-4-5" },
            "command": "/audit",
        }),
        PartKind::Reasoning => json!({
            "type": "reasoning",
            "text": "considering the index order",
            "metadata": { "anthropic": { "signature": "abc==" } },
            "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_999_i64 },
        }),
        // `synthetic` is not in `FilePart`; real `file` rows carry it.
        PartKind::File => json!({
            "type": "file",
            "mime": "image/png",
            "filename": "diagram.png",
            "url": "data:image/png;base64,iVBORw0KGgo=",
            "synthetic": true,
            "source": {
                "type": "symbol",
                "path": "/workspace/src/lib.rs",
                "name": "hydrate",
                "kind": 12,
                "range": {
                    "start": { "line": 98, "character": 0 },
                    "end": { "line": 123, "character": 1 },
                },
                "text": { "value": "fn hydrate", "start": 4_210.0, "end": 4_220.0 },
            },
        }),
        PartKind::Tool => json!({
            "type": "tool",
            "callID": "toolu_01ABCDEF",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/workspace/src/lib.rs", "limit": 200 },
                "output": "1: //! docs\n2: pub mod message;",
                "title": "src/lib.rs",
                "metadata": { "lines": 2, "truncated": false },
                "time": {
                    "start": 1_780_034_795_239_i64,
                    "end": 1_780_034_795_512_i64,
                    "compacted": 1_780_034_800_000_i64,
                },
                "attachments": [{
                    "id": "prt_attach00000000000000000000000",
                    "sessionID": SESSION_ID,
                    "messageID": MESSAGE_ID,
                    "type": "file",
                    "mime": "text/plain",
                    "url": "data:text/plain;base64,aGk=",
                }],
            },
            "metadata": { "providerExecuted": false },
        }),
        PartKind::StepStart => json!({
            "type": "step-start",
            "snapshot": "9f2c1ab0d3e4f5061728394a5b6c7d8e9f001122",
        }),
        PartKind::StepFinish => json!({
            "type": "step-finish",
            "reason": "tool-calls",
            "snapshot": "9f2c1ab0d3e4f5061728394a5b6c7d8e9f001122",
            "cost": 0.003_75,
            "tokens": {
                "total": 5_952.0,
                "input": 1_024.0,
                "output": 256.0,
                "reasoning": 64.0,
                "cache": { "read": 4_096.0, "write": 512.0 },
            },
        }),
        PartKind::Snapshot => json!({
            "type": "snapshot",
            "snapshot": "9f2c1ab0d3e4f5061728394a5b6c7d8e9f001122",
        }),
        PartKind::Patch => json!({
            "type": "patch",
            "hash": "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
            "files": ["crates/zuno-db/src/message.rs", "crates/zuno-db/tests/message.rs"],
        }),
        PartKind::Agent => json!({
            "type": "agent",
            "name": "explore",
            "source": { "value": "@explore", "start": 0, "end": 8 },
        }),
        PartKind::Retry => json!({
            "type": "retry",
            "attempt": 2,
            "error": {
                "name": "APIError",
                "data": {
                    "message": "overloaded",
                    "statusCode": 529,
                    "isRetryable": true,
                    "responseHeaders": { "retry-after": "3" },
                    "responseBody": "{\"type\":\"overloaded_error\"}",
                    "metadata": { "requestID": "req_01" },
                },
            },
            "time": { "created": 1_780_034_795_239_i64 },
        }),
        PartKind::Compaction => json!({
            "type": "compaction",
            "auto": true,
            "overflow": false,
            "tail_start_id": MESSAGE_ID,
        }),
    })
}

/// The `data` column of a `part` row, verbatim.
fn raw_part_data(connection: &Connection, id: &str) -> String {
    connection
        .query_row("SELECT data FROM part WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .expect("read the stored part blob")
}

/// The `data` column of a `message` row, verbatim.
fn raw_message_data(connection: &Connection, id: &str) -> String {
    connection
        .query_row("SELECT data FROM message WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .expect("read the stored message blob")
}

/// Write the anchor message every part in these tests points at.
fn write_anchor_message(store: &MessageStore<'_>) {
    let record = MessageRecord::from_json(assistant_message(MESSAGE_ID, 1_780_034_795_000))
        .expect("split the anchor message");
    store
        .put_message_at(&record, 1_780_034_795_000)
        .expect("write the anchor message");
}

/// Write one part of `kind` and read it back, asserting the round trip is exact.
///
/// `serde_json::Map` is a `BTreeMap` here - `preserve_order` is off - so
/// `to_string` emits keys in one canonical order on both sides of the
/// comparison. The assertion is therefore a byte comparison of two independently
/// serialised trees, not a structural one that could hide a re-ordering.
fn assert_variant_round_trips(kind: PartKind) {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    write_anchor_message(&store);

    let part_id = format!("prt_{:0<28}", kind.as_str().replace('-', "_"));
    let original = part_payload(kind, &part_id);
    let created = 1_780_034_795_239_i64;

    let record = PartRecord::from_json(original.clone(), created)
        .unwrap_or_else(|error| panic!("split the {kind} payload: {error}"));
    assert_eq!(
        record.kind, kind,
        "{kind}: discriminator survived the split"
    );
    assert_eq!(record.id, part_id);
    assert_eq!(record.message_id, MESSAGE_ID);
    assert_eq!(record.session_id, SESSION_ID);

    store
        .put_part_at(&record, created)
        .unwrap_or_else(|error| panic!("write the {kind} part: {error}"));
    let read = store
        .part(&part_id)
        .unwrap_or_else(|error| panic!("read the {kind} part back: {error}"));

    assert_eq!(read.kind, kind, "{kind}: discriminator survived the trip");
    assert_eq!(read.time_created, created);
    assert_eq!(read.time_updated, created);
    assert_eq!(
        serde_json::to_string(&read.to_json()).expect("serialise what was read"),
        serde_json::to_string(&original).expect("serialise what was written"),
        "{kind}: the reassembled part is not byte-identical to the original"
    );

    let stored = raw_part_data(&connection, &part_id);
    let stored_value: Value = serde_json::from_str(&stored).expect("the blob is JSON");
    let stored_object = stored_value.as_object().expect("the blob is an object");
    for key in ["id", "sessionID", "messageID"] {
        assert!(
            !stored_object.contains_key(key),
            "{kind}: `{key}` lives in a column and was duplicated into part.data: {stored}"
        );
    }
    assert_eq!(
        stored_object.get("type").and_then(Value::as_str),
        Some(kind.as_str()),
        "{kind}: the discriminator must stay inside the blob"
    );

    let mut expected = original.clone();
    let expected_object = expected.as_object_mut().expect("payload is an object");
    expected_object.remove("id");
    expected_object.remove("sessionID");
    expected_object.remove("messageID");
    assert_eq!(
        stored,
        serde_json::to_string(&expected).expect("serialise the expected blob"),
        "{kind}: part.data is not the payload minus exactly the three identity keys"
    );
}

#[test]
fn message_part_text_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Text);
}

#[test]
fn message_part_reasoning_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Reasoning);
}

#[test]
fn message_part_tool_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Tool);
}

#[test]
fn message_part_step_start_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::StepStart);
}

#[test]
fn message_part_step_finish_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::StepFinish);
}

#[test]
fn message_part_patch_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Patch);
}

#[test]
fn message_part_file_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::File);
}

#[test]
fn message_part_compaction_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Compaction);
}

#[test]
fn message_part_subtask_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Subtask);
}

#[test]
fn message_part_snapshot_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Snapshot);
}

#[test]
fn message_part_agent_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Agent);
}

#[test]
fn message_part_retry_round_trips_byte_identically() {
    assert_variant_round_trips(PartKind::Retry);
}

#[test]
fn message_every_declared_variant_has_a_round_trip_test() {
    // The twelve tests above, by name. A variant added to the union without a
    // test here fails this assertion instead of quietly going uncovered.
    let covered = [
        PartKind::Text,
        PartKind::Reasoning,
        PartKind::Tool,
        PartKind::StepStart,
        PartKind::StepFinish,
        PartKind::Patch,
        PartKind::File,
        PartKind::Compaction,
        PartKind::Subtask,
        PartKind::Snapshot,
        PartKind::Agent,
        PartKind::Retry,
    ];
    assert_eq!(covered.len(), PART_KIND_COUNT);
    for kind in PartKind::ALL {
        assert!(covered.contains(&kind), "{kind} has no round-trip test");
    }
}

#[test]
fn user_and_assistant_messages_round_trip_byte_identically() {
    let connection = seeded();
    let store = MessageStore::new(&connection);

    for (id, original, role) in [
        (
            "msg_user0000000000000000000000000",
            user_message("msg_user0000000000000000000000000", 1_780_034_795_239),
            MessageRole::User,
        ),
        (
            "msg_asst0000000000000000000000000",
            assistant_message("msg_asst0000000000000000000000000", 1_780_034_795_279),
            MessageRole::Assistant,
        ),
    ] {
        let record = MessageRecord::from_json(original.clone()).expect("split the message");
        assert_eq!(record.role, role);
        assert_eq!(record.id, id);
        assert_eq!(record.session_id, SESSION_ID);
        assert_eq!(
            record.time_created,
            original["time"]["created"].as_i64().expect("time.created"),
            "time_created comes from time.created, as projector.ts:264 does it"
        );

        store
            .put_message_at(&record, record.time_created)
            .expect("write the message");
        let read = store.message(id).expect("read the message back");

        assert_eq!(read.role, role);
        assert_eq!(
            serde_json::to_string(&read.to_json()).expect("serialise what was read"),
            serde_json::to_string(&original).expect("serialise what was written"),
            "{role}: the reassembled message is not byte-identical to the original"
        );

        let stored = raw_message_data(&connection, id);
        let stored_value: Value = serde_json::from_str(&stored).expect("the blob is JSON");
        let stored_object = stored_value.as_object().expect("the blob is an object");
        for key in ["id", "sessionID"] {
            assert!(
                !stored_object.contains_key(key),
                "{role}: `{key}` lives in a column and was duplicated into message.data: {stored}"
            );
        }
        assert_eq!(
            stored_object.get("role").and_then(Value::as_str),
            Some(role.as_str()),
            "{role}: the discriminator must stay inside the blob"
        );

        let mut expected = original.clone();
        let expected_object = expected.as_object_mut().expect("payload is an object");
        expected_object.remove("id");
        expected_object.remove("sessionID");
        assert_eq!(
            stored,
            serde_json::to_string(&expected).expect("serialise the expected blob"),
            "{role}: message.data is not the payload minus exactly id and sessionID"
        );
    }
}

#[test]
fn message_an_unknown_part_variant_is_a_typed_error_when_split() {
    let payload = json!({
        "id": "prt_unknown000000000000000000000",
        "sessionID": SESSION_ID,
        "messageID": MESSAGE_ID,
        "type": "hologram",
        "text": "from a newer opencode",
    });
    let error =
        PartRecord::from_json(payload, 1).expect_err("an unknown variant must not be split");
    match &error {
        DbError::Decode { table, source } => {
            assert_eq!(table, "part");
            let message = source.to_string();
            assert!(
                message.contains("hologram"),
                "the error must name the tag it rejected: {message}"
            );
            assert!(
                message.contains("step-finish"),
                "the error must list what it expected: {message}"
            );
        }
        other => panic!("expected DbError::Decode, got {other:?}"),
    }
    assert!(
        !error.is_retryable(),
        "an unknown variant is a code/schema mismatch, never transient"
    );
}

#[test]
fn message_an_unknown_part_variant_already_on_disk_surfaces_and_is_not_dropped() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    write_anchor_message(&store);

    // A row written by a newer binary than this one, bypassing every check here.
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, 1, 1, ?4)",
            rusqlite_params(),
        )
        .expect("plant a part from the future");

    let by_id = store
        .part("prt_future00000000000000000000000")
        .expect_err("reading an unknown variant must fail");
    assert!(matches!(by_id, DbError::Decode { .. }), "{by_id:?}");

    let hydrated = store
        .hydrate_session(SESSION_ID)
        .expect_err("hydration must fail rather than drop the part");
    match hydrated {
        DbError::Decode { table, source } => {
            assert_eq!(table, "part");
            assert!(
                source.to_string().contains("telepathy"),
                "hydration must name the variant it could not read: {source}"
            );
        }
        other => panic!("expected DbError::Decode, got {other:?}"),
    }

    // And the row is still there: nothing swallowed it.
    let surviving: i64 = connection
        .query_row("SELECT count(*) FROM part", [], |row| row.get(0))
        .expect("count parts");
    assert_eq!(
        surviving, 1,
        "the unreadable part was deleted instead of reported"
    );
}

/// Parameters for the planted future part, kept out of the test body so the
/// literal tag appears exactly once.
fn rusqlite_params() -> [&'static str; 4] {
    [
        "prt_future00000000000000000000000",
        MESSAGE_ID,
        SESSION_ID,
        r#"{"type":"telepathy","thought":"unreadable by this build"}"#,
    ]
}

#[test]
fn message_a_blob_carrying_a_stripped_key_is_rejected_on_read() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    write_anchor_message(&store);
    connection
        .execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES ('prt_dupe000000000000000000000000', ?1, ?2, 1, 1, ?3)",
            (
                MESSAGE_ID,
                SESSION_ID,
                format!(r#"{{"type":"text","text":"hi","messageID":"{MESSAGE_ID}"}}"#),
            ),
        )
        .expect("plant a part with a duplicated identity key");

    let error = store
        .part("prt_dupe000000000000000000000000")
        .expect_err("a duplicated identity key must be reported");
    match error {
        DbError::Decode { table, source } => {
            assert_eq!(table, "part");
            assert!(
                source.to_string().contains("messageID"),
                "the error must name the duplicated key: {source}"
            );
        }
        other => panic!("expected DbError::Decode, got {other:?}"),
    }
}

#[test]
fn a_message_with_an_unknown_role_is_a_typed_error() {
    let payload = json!({
        "id": "msg_weird000000000000000000000000",
        "sessionID": SESSION_ID,
        "role": "oracle",
        "time": { "created": 1 },
    });
    let error = MessageRecord::from_json(payload).expect_err("an unknown role must not be split");
    match error {
        DbError::Decode { table, source } => {
            assert_eq!(table, "message");
            assert!(source.to_string().contains("oracle"), "{source}");
        }
        other => panic!("expected DbError::Decode, got {other:?}"),
    }
}

/// SQLite's own count of statements executed, filled by the trace hook below.
static TRACED_STATEMENTS: AtomicUsize = AtomicUsize::new(0);

/// `SQLITE_TRACE_STMT` fires once per statement as it begins running. The hook is
/// a bare `fn` pointer - it cannot capture - so the tally lives in a static, and
/// only this test ever installs it.
fn count_traced_statement(event: rusqlite::trace::TraceEvent<'_>) {
    if matches!(event, rusqlite::trace::TraceEvent::Stmt(_, _)) {
        TRACED_STATEMENTS.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn hydrating_500_messages_costs_two_statements_not_501() {
    const MESSAGES: usize = 500;
    const PARTS_PER_MESSAGE: usize = 3;

    let connection = seeded();
    let store = MessageStore::new(&connection);

    for index in 0..MESSAGES {
        let id = format!("msg_{index:028}");
        let created = 1_780_034_795_000_i64 + i64::try_from(index).expect("index fits in i64");
        let record =
            MessageRecord::from_json(assistant_message(&id, created)).expect("split the message");
        store
            .put_message_at(&record, created)
            .expect("write the message");
        for part_index in 0..PARTS_PER_MESSAGE {
            let part_id = format!("prt_{index:022}_{part_index:03}");
            let mut payload = part_payload(PartKind::Text, &part_id);
            let object = payload.as_object_mut().expect("payload is an object");
            object.insert("messageID".to_owned(), json!(id));
            let part = PartRecord::from_json(payload, created).expect("split the part");
            store.put_part_at(&part, created).expect("write the part");
        }
    }

    // Only the hydration is measured; the writes above are not.
    store.reset_query_count();
    TRACED_STATEMENTS.store(0, Ordering::SeqCst);
    connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(count_traced_statement),
    );
    let hydrated = store
        .hydrate_session(SESSION_ID)
        .expect("hydrate the session");
    connection.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);

    assert_eq!(hydrated.len(), MESSAGES);
    assert_eq!(
        store.query_count(),
        2,
        "hydration must be one statement for messages and one for parts"
    );
    assert_eq!(
        TRACED_STATEMENTS.load(Ordering::SeqCst),
        2,
        "SQLite disagrees with the store's own count, which means a statement escaped it"
    );
    const {
        assert!(
            MESSAGES > HYDRATION_CHUNK / 2,
            "the fixture must be large enough that an N+1 would be visible"
        );
    }

    let total_parts: usize = hydrated.iter().map(|entry| entry.parts.len()).sum();
    assert_eq!(total_parts, MESSAGES * PARTS_PER_MESSAGE);
    for entry in &hydrated {
        assert_eq!(entry.parts.len(), PARTS_PER_MESSAGE);
        for part in &entry.parts {
            assert_eq!(
                part.message_id, entry.info.id,
                "a part was grouped under the wrong message"
            );
        }
    }
}

/// A writer that must sort last cannot get there from the clock alone.
///
/// `hydration_orders_messages_by_time_then_id_and_parts_by_id` below pins the tie
/// rule: equal `time_created` orders by id. That is faithful to upstream, whose ids
/// are time-ordered — but this port's ids are random uuids, so a caller writing two
/// messages inside one millisecond gets a coin flip instead of the order it wrote
/// them in. [`created_after`] is how a caller declines the flip, and pairing it with
/// [`MessageStore::latest_time_created`] is the whole mechanism.
#[test]
fn a_stamp_clamped_past_the_latest_message_cannot_tie_it() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    assert_eq!(
        store
            .latest_time_created(SESSION_ID)
            .expect("empty session"),
        None,
        "a session with no messages has no latest stamp to sort after"
    );
    assert_eq!(
        created_after(500, None),
        500,
        "with nothing to sort after, the clock stands"
    );

    for (id, created) in [
        ("msg_e00000000000000000000000000000", 100),
        ("msg_f00000000000000000000000000000", 400),
    ] {
        let record = MessageRecord::from_json(user_message(id, created)).expect("split");
        store.put_message_at(&record, created).expect("write");
    }
    let latest = store
        .latest_time_created(SESSION_ID)
        .expect("read the latest stamp");
    assert_eq!(latest, Some(400), "the newest stamp, not the last written");

    assert_eq!(
        created_after(400, latest),
        401,
        "a clock that has not moved past the latest message must still sort after it"
    );
    assert_eq!(
        created_after(399, latest),
        401,
        "a clock that went backwards must not file a new message before an old one"
    );
    assert_eq!(
        created_after(900, latest),
        900,
        "a clock already past the latest message is left alone"
    );
    assert_eq!(
        created_after(0, Some(i64::MAX)),
        i64::MAX,
        "saturating rather than wrapping, because a wrapped stamp sorts first"
    );
}

#[test]
fn hydration_orders_messages_by_time_then_id_and_parts_by_id() {
    let connection = seeded();
    let store = MessageStore::new(&connection);

    // Two messages share a timestamp so the id tie-break is exercised.
    for (id, created) in [
        ("msg_c00000000000000000000000000000", 300),
        ("msg_a00000000000000000000000000000", 100),
        ("msg_b00000000000000000000000000000", 100),
    ] {
        let record = MessageRecord::from_json(user_message(id, created)).expect("split");
        store.put_message_at(&record, created).expect("write");
    }
    for (part_id, message_id) in [
        (
            "prt_z00000000000000000000000000000",
            "msg_a00000000000000000000000000000",
        ),
        (
            "prt_m00000000000000000000000000000",
            "msg_a00000000000000000000000000000",
        ),
        (
            "prt_a00000000000000000000000000000",
            "msg_a00000000000000000000000000000",
        ),
    ] {
        let mut payload = part_payload(PartKind::Text, part_id);
        let object = payload.as_object_mut().expect("payload is an object");
        object.insert("messageID".to_owned(), json!(message_id));
        let record = PartRecord::from_json(payload, 1).expect("split");
        store.put_part_at(&record, 1).expect("write");
    }

    let hydrated = store.hydrate_session(SESSION_ID).expect("hydrate");
    let ids: Vec<&str> = hydrated
        .iter()
        .map(|entry| entry.info.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "msg_a00000000000000000000000000000",
            "msg_b00000000000000000000000000000",
            "msg_c00000000000000000000000000000",
        ],
        "messages come back oldest first, id breaking ties"
    );

    let part_ids: Vec<&str> = hydrated[0]
        .parts
        .iter()
        .map(|part| part.id.as_str())
        .collect();
    assert_eq!(
        part_ids,
        [
            "prt_a00000000000000000000000000000",
            "prt_m00000000000000000000000000000",
            "prt_z00000000000000000000000000000",
        ],
        "parts come back in id order, as message-v2.ts:107 asks for"
    );
    assert!(
        hydrated[1].parts.is_empty(),
        "a message with no parts hydrates to an empty list, not to a missing entry"
    );
}

#[test]
fn message_a_second_write_replaces_data_and_bumps_time_updated() {
    let connection = seeded();
    let store = MessageStore::new(&connection);

    let id = "msg_upsert00000000000000000000000";
    let first = MessageRecord::from_json(assistant_message(id, 1_000)).expect("split");
    store.put_message_at(&first, 1_000).expect("first write");

    let mut second_payload = assistant_message(id, 1_000);
    second_payload["finish"] = json!("length");
    second_payload["cost"] = json!(0.9);
    let second = MessageRecord::from_json(second_payload.clone()).expect("split");
    store.put_message_at(&second, 5_000).expect("second write");

    let read = store.message(id).expect("read back");
    assert_eq!(
        read.time_created, 1_000,
        "time_created is not rewritten by a later upsert"
    );
    assert_eq!(
        read.time_updated, 5_000,
        "time_updated tracks the last write"
    );
    assert_eq!(
        serde_json::to_string(&read.to_json()).expect("serialise"),
        serde_json::to_string(&second_payload).expect("serialise"),
        "the second write replaced the blob wholesale"
    );

    let rows: i64 = connection
        .query_row("SELECT count(*) FROM message", [], |row| row.get(0))
        .expect("count messages");
    assert_eq!(
        rows, 1,
        "the upsert inserted a second row instead of updating"
    );
}

#[test]
fn a_part_whose_message_does_not_exist_is_rejected_by_the_foreign_key() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    let payload = part_payload(PartKind::Text, "prt_orphan00000000000000000000000");
    let record = PartRecord::from_json(payload, 1).expect("split");
    let error = store
        .put_part_at(&record, 1)
        .expect_err("part.message_id is a foreign key and the message was never written");
    assert!(matches!(error, DbError::Query { .. }), "{error:?}");
}

#[test]
fn message_a_missing_row_is_not_found_rather_than_empty() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    match store.message("msg_absent00000000000000000000000") {
        Err(DbError::NotFound { table, id }) => {
            assert_eq!(table, "message");
            assert_eq!(id, "msg_absent00000000000000000000000");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
    match store.part("prt_absent00000000000000000000000") {
        Err(DbError::NotFound { table, id }) => {
            assert_eq!(table, "part");
            assert_eq!(id, "prt_absent00000000000000000000000");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn message_hydrating_nothing_costs_one_statement_and_no_part_lookup() {
    let connection = seeded();
    let store = MessageStore::new(&connection);
    let hydrated: Vec<MessageWithParts> = store
        .hydrate_session("ses_empty00000000000000000000000")
        .expect("hydrate an empty session");
    assert!(hydrated.is_empty());
    assert_eq!(
        store.query_count(),
        1,
        "with no message ids there is nothing to look parts up by"
    );
}

/// Hydrate rows written by the real TypeScript binary.
///
/// Skipped unless `ZUNO_T22_REAL_ROWS` points at a JSON array of rows extracted
/// from a real `opencode.db` - one entry per distinct `part.type`, each carrying
/// the part's columns and blob plus its parent message's. The fixture is not
/// committed because it contains a user's conversation content; the extraction
/// query lives in the task evidence file.
#[test]
fn message_real_typescript_rows_hydrate_when_a_fixture_is_supplied() {
    let Some(path) = std::env::var_os("ZUNO_T22_REAL_ROWS") else {
        return;
    };
    let raw = std::fs::read_to_string(&path).expect("read the real-row fixture");
    let rows: Vec<Value> = serde_json::from_str(&raw).expect("the fixture is a JSON array");
    assert!(!rows.is_empty(), "the fixture is empty");

    let connection = seeded();
    let store = MessageStore::new(&connection);
    let mut expected_variants = Vec::new();

    for row in &rows {
        let variant = row["variant"]
            .as_str()
            .expect("variant is a string")
            .to_owned();
        let session_id = row["session_id"].as_str().expect("session_id");
        let message_id = row["message_id"].as_str().expect("message_id");
        let part_id = row["part_id"].as_str().expect("part_id");

        // Real rows point at real sessions; re-home them onto the seeded one so
        // the message foreign key holds without importing the session table.
        let mut message = row["msg_data"].clone();
        let message_object = message.as_object_mut().expect("msg_data is an object");
        message_object.insert("id".to_owned(), json!(message_id));
        message_object.insert("sessionID".to_owned(), json!(SESSION_ID));
        let message_record = MessageRecord::from_json(message)
            .unwrap_or_else(|error| panic!("{variant}: split the real message: {error}"));
        store
            .put_message_at(
                &message_record,
                row["msg_time_created"].as_i64().unwrap_or(0),
            )
            .unwrap_or_else(|error| panic!("{variant}: write the real message: {error}"));

        let mut part = row["part_data"].clone();
        let part_object = part.as_object_mut().expect("part_data is an object");
        for key in ["id", "sessionID", "messageID"] {
            assert!(
                !part_object.contains_key(key),
                "{variant}: a real part.data already contains `{key}`, \
                 so the strip contract is not what this crate assumes"
            );
        }
        assert!(!session_id.is_empty(), "{variant}: session_id is blank");
        part_object.insert("id".to_owned(), json!(part_id));
        part_object.insert("sessionID".to_owned(), json!(SESSION_ID));
        part_object.insert("messageID".to_owned(), json!(message_id));
        let original = part.clone();
        let created = row["part_time_created"].as_i64().unwrap_or(0);
        let part_record = PartRecord::from_json(part, created)
            .unwrap_or_else(|error| panic!("{variant}: split the real part: {error}"));
        assert_eq!(
            part_record.kind.as_str(),
            variant,
            "the variant this crate read is not the one the row declares"
        );
        store
            .put_part_at(&part_record, created)
            .unwrap_or_else(|error| panic!("{variant}: write the real part: {error}"));

        let read = store
            .part(part_id)
            .unwrap_or_else(|error| panic!("{variant}: read the real part back: {error}"));
        assert_eq!(
            serde_json::to_string(&read.to_json()).expect("serialise what was read"),
            serde_json::to_string(&original).expect("serialise what was written"),
            "{variant}: a real part did not survive the round trip"
        );
        expected_variants.push(variant);
    }

    // Only the hydration is measured; the writes and per-row reads above are not.
    store.reset_query_count();
    let hydrated = store
        .hydrate_session(SESSION_ID)
        .expect("hydrate real rows");
    let hydrated_parts: usize = hydrated.iter().map(|entry| entry.parts.len()).sum();
    assert_eq!(
        hydrated_parts,
        rows.len(),
        "hydration lost a real part: {expected_variants:?}"
    );
    assert_eq!(
        store.query_count(),
        2,
        "hydrating real rows must still be two statements"
    );
}
