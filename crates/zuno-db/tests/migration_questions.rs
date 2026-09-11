//! Format-13 DDL boundaries, independent of the question store's validation.

use rusqlite::{Connection, params};
use zuno_db::{migration, open};
use zuno_paths::DbLocation;

fn current() -> Connection {
    let mut connection = open::open(&DbLocation::Memory).expect("open database");
    migration::apply(&mut connection).expect("create format 13");
    connection
        .execute_batch(
            "INSERT INTO human_request
               (id,session_id,kind,state,payload,revision,time_created,time_updated)
             VALUES ('request-1','session-1','input','pending','{}',1,1,1),
                    ('request-2','session-1','input','pending','{}',1,2,2);",
        )
        .expect("seed parent requests");
    connection
}

fn insert_question(connection: &Connection, request_id: &str) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO question_interaction(request_id,purpose,mode,definition)
         VALUES (?1,'clarification','blocking',?2)",
        params![
            request_id,
            r#"{"origin":{"sessionId":"session-1"},"questions":[{"id":"q1","question":"Any notes?","header":"Notes","options":[]}],"plan":null}"#
        ],
    )
}

#[test]
fn format_thirteen_question_columns_keys_and_lookup_index_match_the_contract() {
    let connection = current();
    type Column<'a> = (&'a str, &'a str, bool, i64);
    let tables: &[(&str, &[Column<'_>])] = &[
        (
            "question_interaction",
            &[
                ("request_id", "TEXT", false, 1),
                ("purpose", "TEXT", true, 0),
                ("mode", "TEXT", true, 0),
                ("definition", "TEXT", true, 0),
                ("decision", "TEXT", false, 0),
                ("authorization", "TEXT", false, 0),
                ("risk_reason", "TEXT", false, 0),
                ("authorization_input_id", "TEXT", false, 0),
            ],
        ),
        (
            "question_action_receipt",
            &[
                ("request_id", "TEXT", true, 1),
                ("command_id", "TEXT", true, 2),
                ("command_json", "TEXT", true, 0),
                ("receipt", "TEXT", true, 0),
                ("time_created", "INTEGER", true, 0),
            ],
        ),
    ];
    for (table, expected) in tables {
        let actual = connection
            .prepare(
                "SELECT name,upper(type),\"notnull\",pk,dflt_value
                 FROM pragma_table_info(?1) ORDER BY cid",
            )
            .expect("column inventory")
            .query_map([table], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .expect("query columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("read columns");
        let expected: Vec<_> = expected
            .iter()
            .map(|(name, kind, required, key)| {
                (
                    (*name).to_owned(),
                    (*kind).to_owned(),
                    *required,
                    *key,
                    None,
                )
            })
            .collect();
        assert_eq!(actual, expected, "{table}");
        let foreign_keys = connection
            .prepare("SELECT \"table\",\"from\",\"to\",on_delete FROM pragma_foreign_key_list(?1)")
            .expect("foreign key inventory")
            .query_map([table], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("query foreign keys")
            .collect::<Result<Vec<_>, _>>()
            .expect("read foreign keys");
        assert_eq!(
            foreign_keys,
            vec![(
                "human_request".to_owned(),
                "request_id".to_owned(),
                "id".to_owned(),
                "CASCADE".to_owned()
            )],
            "{table}"
        );
    }
    let (unique, partial): (bool, bool) = connection
        .query_row(
            "SELECT \"unique\",partial FROM pragma_index_list('question_interaction')
             WHERE name='question_interaction_purpose_authorization_idx'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("lookup index");
    assert_eq!((unique, partial), (false, false));
    let indexed = connection
        .prepare(
            "SELECT name FROM pragma_index_info('question_interaction_purpose_authorization_idx')
             ORDER BY seqno",
        )
        .expect("index columns")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query index columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("read index columns");
    assert_eq!(indexed, ["purpose", "authorization", "request_id"]);
}

#[test]
fn format_thirteen_enforces_closed_question_domains_and_object_definitions() {
    let connection = current();
    insert_question(&connection, "request-1").expect("insert valid companion");
    for (column, allowed, nullable) in [
        (
            "purpose",
            &["clarification", "required_input", "plan_authorization"][..],
            false,
        ),
        ("mode", &["blocking", "deferred"][..], false),
        ("decision", &["approve", "decline"][..], true),
        (
            "authorization",
            &["waiting_for_handoff", "applied", "invalidated"][..],
            true,
        ),
    ] {
        let update =
            format!("UPDATE question_interaction SET {column}=?1 WHERE request_id='request-1'");
        for value in allowed {
            connection
                .execute(&update, [value])
                .expect("every declared enum value is valid");
        }
        for invalid in [
            "".to_owned(),
            "unknown".to_owned(),
            allowed[0].to_uppercase(),
            format!("{} ", allowed[0]),
        ] {
            assert!(
                connection.execute(&update, [&invalid]).is_err(),
                "{column} accepted {invalid:?}"
            );
        }
        let null = connection.execute(&update, [rusqlite::types::Value::Null]);
        assert_eq!(null.is_ok(), nullable, "{column} nullability");
    }
    for definition in ["[]", "null", "true", "1", "\"text\"", "{", ""] {
        assert!(
            connection
                .execute(
                    "UPDATE question_interaction SET definition=?1",
                    [definition]
                )
                .is_err(),
            "definition accepted {definition:?}"
        );
    }
    assert!(
        connection
            .execute("UPDATE question_interaction SET definition=NULL", [])
            .is_err()
    );
    connection
        .execute("UPDATE question_interaction SET definition='{}'", [])
        .expect("the DDL requires an object; the store owns its typed fields");
    connection
        .execute(
            "UPDATE question_interaction SET risk_reason=?1, authorization_input_id=?2",
            ["Reviewed explicitly — 已审阅", "input-held-by-service"],
        )
        .expect("nullable service metadata carries no additional foreign key");
}

#[test]
fn format_thirteen_receipts_require_json_and_non_null_command_fields() {
    let connection = current();
    connection.execute(
        "INSERT INTO question_action_receipt(request_id,command_id,command_json,receipt,time_created)
         VALUES ('request-1','command-1','{}','{}',3)", []
    ).expect("insert a command receipt");
    for column in ["command_json", "receipt"] {
        for valid_json in ["{}", "[]", "null", "1", "true", "\"receipt\""] {
            connection
                .execute(
                    &format!("UPDATE question_action_receipt SET {column}=?1"),
                    [valid_json],
                )
                .expect("all JSON roots are valid in the receipt DDL");
        }
        for invalid_json in ["", "{", "not-json"] {
            assert!(
                connection
                    .execute(
                        &format!("UPDATE question_action_receipt SET {column}=?1"),
                        [invalid_json],
                    )
                    .is_err(),
                "{column} accepted invalid JSON"
            );
        }
    }
    for column in [
        "request_id",
        "command_id",
        "command_json",
        "receipt",
        "time_created",
    ] {
        assert!(
            connection
                .execute(
                    &format!("UPDATE question_action_receipt SET {column}=NULL"),
                    []
                )
                .is_err(),
            "{column} accepted NULL"
        );
    }
}

#[test]
fn format_thirteen_receipt_identity_and_both_cascades_are_request_scoped() {
    let connection = current();
    insert_question(&connection, "request-1").expect("insert companion");
    assert!(
        insert_question(&connection, "request-1").is_err(),
        "one companion per request"
    );
    assert!(
        insert_question(&connection, "missing-request").is_err(),
        "a companion needs its parent"
    );
    let insert_receipt = |request_id: &str| {
        connection.execute(
            "INSERT INTO question_action_receipt(request_id,command_id,command_json,receipt,time_created)
             VALUES (?1,'same-command','{}','{}',3)",
            [request_id],
        )
    };
    insert_receipt("request-1").expect("first command");
    assert!(
        insert_receipt("request-1").is_err(),
        "request plus command is unique"
    );
    insert_receipt("request-2")
        .expect("a different request may use the same command ID without a companion");
    assert!(
        insert_receipt("missing-request").is_err(),
        "a receipt needs its human request"
    );
    connection
        .execute("DELETE FROM human_request WHERE id='request-1'", [])
        .expect("delete parent");
    let companions: i64 = connection
        .query_row("SELECT count(*) FROM question_interaction", [], |row| {
            row.get(0)
        })
        .expect("count surviving companions");
    let receipts: Vec<String> = connection
        .prepare("SELECT request_id FROM question_action_receipt")
        .expect("receipt query")
        .query_map([], |row| row.get(0))
        .expect("surviving receipts")
        .collect::<Result<_, _>>()
        .expect("read receipts");
    assert_eq!(companions, 0);
    assert_eq!(receipts, ["request-2"]);
}
