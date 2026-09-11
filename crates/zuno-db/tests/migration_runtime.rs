//! Published format-13 rows survive format-14 runtime-receipt migration.
//! Inbox consumption proves recording, not application to a model request.

use rusqlite::{Connection, types::Value};
use zuno_db::{migration, open};
use zuno_paths::DbLocation;

fn format_thirteen() -> Connection {
    let connection = open::open(&DbLocation::Memory).expect("open fixture");
    connection
        .execute_batch(concat!(
            include_str!("fixtures/format-7.sql"),
            include_str!("fixtures/format-8.sql"),
            include_str!("fixtures/format-9.sql"),
            include_str!("fixtures/format-10.sql"),
            include_str!("fixtures/format-11.sql"),
            include_str!("fixtures/format-12.sql"),
            include_str!("fixtures/format-13.sql"),
        ))
        .expect("load released format 13");
    connection
        .execute_batch(
            "INSERT INTO session_input
               (id,session_id,prompt,delivery,state,revision,admitted_seq,promoted_seq,
                time_created,time_updated,source_key,trigger_kind,cycle_id)
             VALUES ('inp_format13_consumed','ses_fixture_0001','Keep historical input — 保留输入',
               'steer','consumed',3,100,101,1735690000000,1735690001000,
               'published-consumed-input','user','published-cycle');
             INSERT INTO question_action_receipt
               (request_id,command_id,command_json,receipt,time_created)
             VALUES ('req_format13','published-command','{ \"kind\" : \"draft\" }',
               '{ \"preserve\" : \"回执 bytes\" }',1735690001000);
             UPDATE session_execution_state SET phase='paused',scheduling=
               '{ \"readiness\" : {\"kind\":\"paused\",\"reason\":\"user\"},
                  \"progressFingerprint\":\"sha256:published\",\"unchangedProgressCount\":2 }';",
        )
        .expect("seed nonempty published-layout receipts, scheduling, and consumed input");
    connection
}

fn rows(connection: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .expect("snapshot query");
    let count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..count)
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<Vec<Value>>>()
        })
        .expect("snapshot")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("rows")
}

#[test]
fn published_format_thirteen_preserves_rows_and_adds_runtime_receipts() {
    let mut connection = format_thirteen();
    let tables = [
        "session",
        "message",
        "session_message",
        "session_input",
        "session_execution_state",
        "completion_delivery",
        "event",
        "work_plan",
        "memory_candidate",
        "resident_memory_document",
        "resident_memory_revision",
        "human_request",
        "question_interaction",
        "question_action_receipt",
    ];
    let before: Vec<_> = tables
        .iter()
        .map(|table| rows(&connection, table))
        .collect();
    migration::apply(&mut connection).expect("upgrade published database");
    for (table, before) in tables.iter().zip(&before) {
        assert_eq!(&rows(&connection, table), before, "{table} changed");
    }
    for table in ["session_input_receipt", "session_context_usage"] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .expect("schema inventory");
        assert!(exists, "upgrading format 13 must add {table}");
    }
    let version: u32 = connection
        .query_row("SELECT format FROM zuno_schema", [], |r| r.get(0))
        .expect("format");
    assert_eq!(version, 14);
    let receipt = rows(&connection, "session_input_receipt");
    assert_eq!(
        receipt,
        vec![vec![
            Value::Text("inp_format13_consumed".to_owned()),
            Value::Text("recorded".to_owned()),
            Value::Text("steer".to_owned()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(1735690001000),
        ]],
        "legacy consumption cannot invent a turn, model application, or completion"
    );
    assert!(rows(&connection, "session_context_usage").is_empty());
    migration::apply(&mut connection).expect("reopening format 14 is validation only");
    assert_eq!(rows(&connection, "session_input_receipt"), receipt);
    for (table, before) in tables.iter().zip(&before) {
        assert_eq!(
            &rows(&connection, table),
            before,
            "{table} changed on reopen"
        );
    }
}
