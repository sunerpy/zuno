//! Scheduling preservation through format 15, using published formats 5-14.
//! The pre-13 step alone repairs documented stale-running/no-progress rows;
//! upgrading published format 13 must not replay that repair or resume a Goal.

use rusqlite::{Connection, params, types::Value};
use serde_json::json;
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::{migration, open, session_execution};
use zuno_paths::DbLocation;
use zuno_types::execution::{
    SessionExecutionPhase, SessionPauseReason, SessionReadiness, SessionWakeSignal, WakeAdmission,
};

const SESSION: &str = "ses_fixture_0001";

fn legacy(format: u32) -> Connection {
    assert!((5..=14).contains(&format), "supported fixture format");
    let connection = open::open(&DbLocation::Memory).expect("database");
    connection
        .execute_batch(match format {
            5 => include_str!("fixtures/format-5.sql"),
            6 => include_str!("fixtures/format-6.sql"),
            _ => include_str!("fixtures/format-7.sql"),
        })
        .expect("released base schema");
    for (next, sql) in [
        (8, include_str!("fixtures/format-8.sql")),
        (9, include_str!("fixtures/format-9.sql")),
        (10, include_str!("fixtures/format-10.sql")),
        (11, include_str!("fixtures/format-11.sql")),
        (12, include_str!("fixtures/format-12.sql")),
        (13, include_str!("fixtures/format-13.sql")),
        (14, include_str!("fixtures/format-14.sql")),
    ] {
        if format >= next {
            connection
                .execute_batch(sql)
                .expect("released schema delta");
        }
    }
    assert_eq!(marker(&connection), format);
    // Opening a legacy fixture must not mutate it before migration::apply.
    assert_eq!(has_scheduling(&connection), format >= 13);
    connection
}

fn marker(connection: &Connection) -> u32 {
    connection
        .query_row("SELECT format FROM zuno_schema", [], |row| row.get(0))
        .expect("marker")
}

fn has_scheduling(connection: &Connection) -> bool {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_execution_state') WHERE name='scheduling')",
        [], |row| row.get(0),
    ).expect("column inventory")
}

fn rows(connection: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = connection.prepare(sql).expect("snapshot query");
    let count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..count)
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<Vec<Value>>>()
        })
        .expect("snapshot rows")
        .collect::<Result<_, _>>()
        .expect("snapshot values")
}

fn preserved_rows(connection: &Connection) -> Vec<(String, Vec<Vec<Value>>)> {
    [
        "session",
        "message",
        "memory_candidate",
        "human_request",
        "event",
        "work_plan",
    ]
    .into_iter()
    .filter(|table| {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .expect("table inventory")
    })
    .map(|table| {
        let columns = connection
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .expect("columns")
            .query_map([table], |row| row.get::<_, String>(0))
            .expect("column names")
            .collect::<Result<Vec<_>, _>>()
            .expect("read columns");
        let columns = columns
            .into_iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("SELECT {columns} FROM {table} ORDER BY rowid");
        let snapshot = rows(connection, &query);
        (query, snapshot)
    })
    .collect()
}

fn assert_preserved(connection: &Connection, before: &[(String, Vec<Vec<Value>>)]) {
    for (query, snapshot) in before {
        assert_eq!(&rows(connection, query), snapshot, "{query}");
    }
}

fn phase_event(connection: &mut Connection, properties: serde_json::Value) -> String {
    let transaction = connection.transaction().expect("event transaction");
    let event = append_in(
        &transaction,
        SESSION,
        NewSessionEvent::new(
            "session.driver.phase",
            properties.as_object().expect("properties").clone(),
        )
        .expect("event"),
    )
    .expect("append");
    transaction.commit().expect("event commit");
    event.id
}

fn paused_event() -> serde_json::Value {
    json!({
        "cycleId": "driver-origin-cycle",
        "phase": "paused",
        "pauseReason": "no_progress",
        "reason": "no_progress",
        "progressFingerprint": "sha256:unchanged",
        "unchangedProgressCount": 3,
        "reconciliationAttempt": 3
    })
}

fn stale_running(connection: &Connection) {
    connection.execute(
        "UPDATE session_execution_state SET revision=15, phase='running', cycle_id='callback-cycle',
         work_identity=?1, authorized_plan_id='plan-preserved', authorized_plan_revision=4,
         handoff_plan_id='plan-preserved', handoff_plan_revision=4, continuation=?2,
         draft_review_risk=?3 WHERE session_id=?4",
        params![
            "{ \"agent\" : \"build\", \"providerId\" : \"provider\", \"modelId\" : \"model\", \"reasoning\" : \"high\" }",
            r#"{ "cycleId":"callback-cycle", "identity":{"agent":"build","providerId":"provider","modelId":"model","reasoning":"high"}, "mode":"work", "planId":"plan-preserved", "planRevision":4, "contextEpoch":7, "anchorMessageId":"message-preserved" }"#,
            r#"{ "reviewId":"review-preserved", "reviewRevision":2, "reason":"explicit risk acceptance", "timeAccepted":2 }"#,
            SESSION
        ],
    ).expect("stale execution state");
}

#[test]
fn every_released_format_five_through_fourteen_reaches_current_and_preserves_scheduling() {
    for format in 5..=14 {
        let mut connection = legacy(format);
        let before = preserved_rows(&connection);
        let execution_query = if format >= 13 {
            "SELECT * FROM session_execution_state ORDER BY session_id"
        } else {
            "SELECT session_id,revision,mode,work_identity,authorized_plan_id,
                    authorized_plan_revision,handoff_plan_id,handoff_plan_revision,
                    draft_review_risk,cycle_id,phase,continuation,time_created,time_updated
             FROM session_execution_state ORDER BY session_id"
        };
        let execution = if format >= 10 {
            Some(rows(&connection, execution_query))
        } else {
            None
        };
        migration::apply(&mut connection).unwrap_or_else(|error| {
            panic!("format {format}: {}", zuno_error::source::describe(&error))
        });
        assert_eq!(marker(&connection), migration::CURRENT_FORMAT);
        assert!(has_scheduling(&connection));
        assert_preserved(&connection, &before);
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM session_execution_state WHERE scheduling IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("legacy metadata"),
            i64::from(format == 14),
            "format {format}"
        );
        if let Some(execution) = execution {
            let after = rows(&connection, execution_query);
            assert_eq!(after, execution, "format {format}");
        }
        migration::apply(&mut connection).expect("validate again");
        assert_preserved(&connection, &before);
        for invalid in ["[]", "null", "true", "\"text\"", "1", "{"] {
            // No row exists before format 10, so test the CHECK on a fixture row
            // only where execution state was already durable.
            if format >= 10 {
                assert!(
                    connection
                        .execute(
                            "UPDATE session_execution_state SET scheduling=?1",
                            [invalid],
                        )
                        .is_err(),
                    "format {format} accepted {invalid}"
                );
            }
        }
    }
}

#[test]
fn legacy_no_progress_repair_preserves_authority_cycle_and_progress_without_a_goal() {
    for format in 10..=12 {
        let mut connection = legacy(format);
        stale_running(&connection);
        phase_event(&mut connection, paused_event());
        // The callback reset execution's cycle, while its latest driver phase
        // still carries the durable no-progress pause from the prior cycle.
        let before = preserved_rows(&connection);
        let control_query = "SELECT mode,work_identity,authorized_plan_id,authorized_plan_revision,
                    handoff_plan_id,handoff_plan_revision,draft_review_risk,cycle_id,
                    continuation,time_created,time_updated
             FROM session_execution_state";
        let control = rows(&connection, control_query);
        migration::apply(&mut connection).expect("repair and upgrade");
        assert_preserved(&connection, &before);
        assert_eq!(
            rows(&connection, control_query),
            control,
            "original JSON bytes"
        );
        let repaired = session_execution::read_in(&connection, SESSION)
            .expect("read")
            .expect("state");
        assert_eq!(repaired.revision, 16);
        assert_eq!(repaired.phase, SessionExecutionPhase::Paused);
        assert_eq!(repaired.cycle_id.as_deref(), Some("callback-cycle"));
        let scheduling = repaired.scheduling.as_ref().expect("repaired scheduling");
        assert_eq!(
            scheduling.readiness,
            SessionReadiness::Paused {
                reason: SessionPauseReason::NoProgress
            }
        );
        assert_eq!(
            scheduling.progress_fingerprint.as_deref(),
            Some("sha256:unchanged")
        );
        assert_eq!(scheduling.unchanged_progress_count, 3);
        for signal in [
            SessionWakeSignal::Automatic,
            SessionWakeSignal::Recovery,
            SessionWakeSignal::Callback,
        ] {
            assert_eq!(repaired.wake_admission(&signal), WakeAdmission::Reject);
        }
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM human_request", [], |row| row
                    .get::<_, i64>(0))
                .expect("requests"),
            0
        );
        assert!(
            !connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='goal')",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .expect("Goal table")
        );
        migration::apply(&mut connection).expect("reopen current format");
        assert_eq!(
            session_execution::read_in(&connection, SESSION).expect("read"),
            Some(repaired)
        );
    }
}

#[test]
fn published_thirteen_preserves_scheduling_bytes_and_does_not_repeat_the_legacy_repair() {
    for scheduling in [
        None,
        Some(
            r#"{ "readiness" : {"kind":"paused","reason":"user"}, "progressFingerprint":"sha256:published-13", "unchangedProgressCount":4 }"#,
        ),
    ] {
        let mut connection = legacy(13);
        stale_running(&connection);
        phase_event(&mut connection, paused_event());
        connection
            .execute(
                "UPDATE session_execution_state SET scheduling=?1,phase=?2",
                params![
                    scheduling,
                    if scheduling.is_some() {
                        "paused"
                    } else {
                        "running"
                    }
                ],
            )
            .expect("published nullable or explicit scheduling metadata");
        let before = preserved_rows(&connection);
        let execution = rows(&connection, "SELECT * FROM session_execution_state");
        migration::apply(&mut connection).expect("upgrade published format 13");
        assert_eq!(marker(&connection), migration::CURRENT_FORMAT);
        assert_preserved(&connection, &before);
        assert_eq!(
            rows(&connection, "SELECT * FROM session_execution_state"),
            execution,
            "format 14 must not replay the pre-13 repair or rewrite scheduling"
        );
        let state = session_execution::read_in(&connection, SESSION)
            .expect("read typed execution")
            .expect("preserved state");
        assert_eq!(state.revision, 15);
        assert_eq!(
            state.phase,
            if scheduling.is_some() {
                SessionExecutionPhase::Paused
            } else {
                SessionExecutionPhase::Running
            }
        );
        if scheduling.is_some() {
            let metadata = state.scheduling.as_ref().expect("published scheduling");
            assert_eq!(
                metadata.readiness,
                SessionReadiness::Paused {
                    reason: SessionPauseReason::User
                }
            );
            for signal in [
                SessionWakeSignal::Automatic,
                SessionWakeSignal::Recovery,
                SessionWakeSignal::Callback,
            ] {
                assert_eq!(state.wake_admission(&signal), WakeAdmission::Reject);
            }
        } else {
            assert_eq!(state.scheduling, None);
        }
        migration::apply(&mut connection).expect("reopen current format");
        assert_eq!(
            rows(&connection, "SELECT * FROM session_execution_state"),
            execution
        );
        assert_preserved(&connection, &before);
    }
}

#[test]
fn legacy_repair_uses_only_the_latest_supported_structured_driver_phase() {
    for case in [
        "prose_only",
        "executing",
        "reconciling",
        "user_pause",
        "future_phase",
        "already_paused",
    ] {
        let mut connection = legacy(12);
        stale_running(&connection);
        connection.execute(
            "UPDATE message SET data=?1 WHERE id='msg_fixture_0001'",
            [r#"{"role":"assistant","text":"无可执行工作。等待用户批准。No executable work; waiting for your approval."}"#],
        ).expect("assistant prose is not scheduling evidence");
        if case != "prose_only" {
            phase_event(&mut connection, paused_event());
        }
        match case {
            "executing" | "reconciling" => {
                phase_event(
                    &mut connection,
                    json!({
                        "cycleId":"callback-cycle", "phase":case, "reconciliationAttempt":0
                    }),
                );
            }
            "user_pause" => {
                phase_event(
                    &mut connection,
                    json!({
                        "cycleId":"callback-cycle", "phase":"paused", "pauseReason":"user"
                    }),
                );
            }
            "future_phase" => {
                let id = phase_event(&mut connection, paused_event());
                connection
                    .execute(
                        "UPDATE event SET type='session.driver.phase.2' WHERE id=?1",
                        [id],
                    )
                    .expect("unknown future event schema");
            }
            "already_paused" => {
                connection
                    .execute("UPDATE session_execution_state SET phase='paused'", [])
                    .expect("no stale-running discrepancy");
            }
            _ => {}
        }
        let old_state = rows(
            &connection,
            "SELECT session_id,revision,phase,cycle_id,continuation FROM session_execution_state",
        );
        let before = preserved_rows(&connection);
        migration::apply(&mut connection).expect("conservative upgrade");
        assert_preserved(&connection, &before);
        assert_eq!(
            rows(
                &connection,
                "SELECT session_id,revision,phase,cycle_id,continuation FROM session_execution_state"
            ),
            old_state,
            "{case}"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT scheduling FROM session_execution_state",
                    [],
                    |row| row.get::<_, Option<String>>(0)
                )
                .expect("metadata"),
            None,
            "{case}"
        );
    }
}

#[test]
fn final_fifteen_marker_failure_rolls_back_scheduling_repair_and_runtime_schema() {
    let mut connection = legacy(12);
    stale_running(&connection);
    phase_event(&mut connection, paused_event());
    connection
        .execute_batch(
            "CREATE TRIGGER scheduling_marker_failure BEFORE UPDATE OF format ON zuno_schema
         WHEN NEW.format=15 BEGIN
           SELECT CASE WHEN (SELECT phase FROM session_execution_state LIMIT 1) <> 'paused'
             THEN RAISE(ABORT,'repair must precede marker') END;
           SELECT CASE WHEN NOT EXISTS(
             SELECT 1 FROM pragma_table_info('session_execution_state') WHERE name='scheduling')
             THEN RAISE(ABORT,'column must precede marker') END;
           SELECT CASE WHEN (SELECT count(*) FROM sqlite_schema WHERE name IN
             ('session_input_receipt','session_context_usage',
              'session_input_receipt_turn_state_idx','session_context_usage_updated_idx',
              'session_work_cycle','session_work_cycle_updated_idx',
              'goal_turn_observation','goal_turn_audit','goal_cycle_failure')) <> 9
             THEN RAISE(ABORT,'runtime schema must precede marker') END;
           SELECT RAISE(ABORT,'final marker refused');
         END;",
        )
        .expect("trap the final marker write");
    let before = preserved_rows(&connection);
    let inventory = rows(
        &connection,
        "SELECT type,name,sql FROM sqlite_schema ORDER BY name",
    );
    let old_execution = rows(&connection, "SELECT * FROM session_execution_state");
    let error = migration::apply(&mut connection).expect_err("final marker trap");
    assert!(
        zuno_error::source::describe(&error).contains("final marker refused"),
        "{error:?}"
    );
    assert_eq!(marker(&connection), 12);
    assert!(!has_scheduling(&connection));
    assert_eq!(
        rows(
            &connection,
            "SELECT type,name,sql FROM sqlite_schema ORDER BY name"
        ),
        inventory
    );
    assert_eq!(
        rows(&connection, "SELECT * FROM session_execution_state"),
        old_execution
    );
    assert_preserved(&connection, &before);
}

#[test]
fn current_marker_with_missing_scheduling_column_fails_without_mutation() {
    let mut connection = legacy(12);
    migration::apply(&mut connection).expect("upgrade");
    connection
        .execute_batch("ALTER TABLE session_execution_state DROP COLUMN scheduling;")
        .expect("simulate incomplete current format");
    let inventory = rows(
        &connection,
        "SELECT type,name,sql FROM sqlite_schema ORDER BY name",
    );
    let before = preserved_rows(&connection);
    assert!(migration::apply(&mut connection).is_err());
    assert_eq!(marker(&connection), migration::CURRENT_FORMAT);
    assert_eq!(
        rows(
            &connection,
            "SELECT type,name,sql FROM sqlite_schema ORDER BY name"
        ),
        inventory
    );
    assert_preserved(&connection, &before);
}

#[test]
fn current_rejects_weakened_scheduling_constraints_without_rewriting_rows() {
    for column in [
        "scheduling text",
        "scheduling integer CHECK(scheduling IS NULL OR (json_valid(scheduling) AND json_type(scheduling)='object'))",
        "scheduling text DEFAULT '{}' CHECK(scheduling IS NULL OR (json_valid(scheduling) AND json_type(scheduling)='object'))",
        "scheduling text NOT NULL DEFAULT '{}' CHECK(json_valid(scheduling) AND json_type(scheduling)='object')",
        "scheduling text CHECK(scheduling IS NULL OR json_valid(scheduling))",
        "scheduling text CHECK(scheduling IS NULL OR (json_valid(scheduling) AND json_type(scheduling) IN ('object','array')))",
    ] {
        let mut connection = legacy(13);
        migration::apply(&mut connection).expect("prepare current schema");
        connection
            .execute_batch(&format!(
                "ALTER TABLE session_execution_state DROP COLUMN scheduling;
                 ALTER TABLE session_execution_state ADD COLUMN {column};"
            ))
            .expect("construct the damaged current scheduling column");
        let before = preserved_rows(&connection);
        let execution = rows(&connection, "SELECT * FROM session_execution_state");
        let inventory = rows(
            &connection,
            "SELECT type,name,sql FROM sqlite_schema ORDER BY name",
        );
        let error = migration::apply(&mut connection).expect_err("corrupt current scheduling");
        assert!(
            matches!(error, zuno_error::DbError::Schema { .. }),
            "{column}: {error:?}"
        );
        assert_eq!(marker(&connection), migration::CURRENT_FORMAT);
        assert_eq!(
            rows(&connection, "SELECT * FROM session_execution_state"),
            execution,
            "{column}"
        );
        assert_eq!(
            rows(
                &connection,
                "SELECT type,name,sql FROM sqlite_schema ORDER BY name"
            ),
            inventory,
            "{column}"
        );
        assert_preserved(&connection, &before);
    }
}
