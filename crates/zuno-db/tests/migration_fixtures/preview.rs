//! Captured unpublished preview layouts are a different lineage from main 13/14.
use super::*;

const PREVIEW_THIRTEEN: Fixture = Fixture {
    format: 13,
    release: "unpublished preview ownership format 13",
    sql: concat!(
        include_str!("../fixtures/format-7.sql"),
        include_str!("../fixtures/format-8.sql"),
        include_str!("../fixtures/format-9.sql"),
        include_str!("../fixtures/format-10.sql"),
        include_str!("../fixtures/format-11.sql"),
        include_str!("../fixtures/preview-legacy-format-12.sql"),
        include_str!("../fixtures/preview-legacy-format-13.sql")
    ),
    table_count: 58,
};

fn preview_fourteen(path: &Path) -> Connection {
    let mut connection = load_fixture(path, &PREVIEW_THIRTEEN);
    let indexes = {
        let mut statement = connection.prepare(
            "SELECT sql FROM sqlite_schema WHERE type='index' AND tbl_name='agent_job' AND sql IS NOT NULL ORDER BY name",
        ).unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let transaction = connection.transaction().unwrap();
    let root = include_str!("../fixtures/preview-legacy-agent_job_root.sql").replace(
        "CREATE TABLE `agent_job`",
        "CREATE TABLE `__captured_preview_job`",
    );
    transaction.execute_batch(&root).unwrap();
    transaction
        .execute_batch(
            "INSERT INTO __captured_preview_job SELECT * FROM agent_job;
         DROP TABLE agent_job;
         ALTER TABLE __captured_preview_job RENAME TO agent_job;",
        )
        .unwrap();
    for sql in indexes {
        transaction.execute_batch(&sql).unwrap();
    }
    transaction
        .execute_batch(include_str!("../fixtures/preview-legacy-runtime_jobs.sql"))
        .unwrap();
    transaction.execute_batch(
        "INSERT INTO agent_job(id,parent_session_id,logical_key,subject_kind,subject_payload,
           evidence_start_rowid,status,report_delivery,created_seq,time_created,time_updated)
         VALUES('preview-root','ses_fixture_0001','preview-root','root-turn',
           '{\"kind\":\"rootTurn\",\"turnID\":\"preview-turn\"}',0,'running','quiet',1,100,102);
         UPDATE runtime_session SET current_job_id='preview-root',lease_epoch=7 WHERE session_id='ses_fixture_0001';",
    ).unwrap();
    let checkpoint = serde_json::json!({
        "jobId":"preview-root","sessionId":"ses_fixture_0001","turnId":"preview-turn",
        "driver":"default","schemaVersion":2,
        "reference":{"spentTokens":1234,"toolCalls":9,"elapsedMs":7500,"eventId":"old-checkpoint"},
    });
    transaction.execute(
        "INSERT INTO runtime_job(job_id,session_id,turn_id,input_id,request_digest,principal,configuration,
          phase,checkpoint,checkpoint_version,input_version,ready_at,time_created,time_updated)
         VALUES('preview-root','ses_fixture_0001','preview-turn','inp_migration_consumed',?1,?2,?3,
           'ready',?4,1,6,102,100,102)",
        rusqlite::params![
            "a".repeat(64),
            serde_json::to_string(&zuno_types::identity::PrincipalScope::local()).unwrap(),
            serde_json::json!({"id":"old-definition","version":1,"sha256":"a".repeat(64)}).to_string(),
            checkpoint.to_string(),
        ],
    ).unwrap();
    transaction.execute_batch(
        "INSERT INTO runtime_attempt(id,job_id,worker_id,lease_epoch,state,started_at,finished_at)
         VALUES('preview-attempt','preview-root','retired-worker',7,'released',100,102);
         UPDATE zuno_schema SET format=14 WHERE singleton=1;",
    ).unwrap();
    transaction.commit().unwrap();
    connection
}

fn assert_overlay(connection: &Connection) {
    let marker: (u32, String) = connection
        .query_row(
            "SELECT format,channel FROM zuno_preview_schema WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        marker,
        (
            migration::CURRENT_PREVIEW_FORMAT,
            "enterprise-preview".to_owned()
        )
    );
    assert_same_structure(
        &structure(connection),
        &structure(&fresh_current()),
        "preview lineage migration",
    );
}

#[test]
fn legacy_preview_thirteen_preserves_owned_sessions_messages_and_memory() {
    let directory = temp_dir();
    let mut connection = load_fixture(&directory.path().join("preview.db"), &PREVIEW_THIRTEEN);
    let rows = snapshot_rows(&connection, &structure(&connection));
    migration::apply(&mut connection).unwrap();
    assert_rows_preserved(&connection, &rows, &["zuno_schema"]);
    assert_overlay(&connection);
}

#[test]
fn legacy_preview_fourteen_preserves_jobs_leases_budgets_and_failed_migration_evidence() {
    let directory = temp_dir();
    let mut connection = preview_fourteen(&directory.path().join("preview.db"));
    let before = structure(&connection);
    let rows = snapshot_rows(&connection, &before);
    connection
        .execute_batch(
            "CREATE TRIGGER fail_preview_marker BEFORE UPDATE ON zuno_schema
         BEGIN SELECT RAISE(ABORT,'injected preview marker failure'); END;",
        )
        .unwrap();
    assert!(migration::apply(&mut connection).is_err());
    connection
        .execute_batch("DROP TRIGGER fail_preview_marker")
        .unwrap();
    assert_same_structure(
        &structure(&connection),
        &before,
        "rolled back preview upgrade",
    );
    assert_rows_preserved(&connection, &rows, &[]);
    migration::apply(&mut connection).unwrap();
    assert_rows_preserved(&connection, &rows, &["zuno_schema"]);
    assert_overlay(&connection);
    let checkpoint: String = connection
        .query_row(
            "SELECT checkpoint FROM runtime_job WHERE job_id='preview-root'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&checkpoint).unwrap()["reference"]["spentTokens"],
        1234
    );
}

#[test]
fn mixed_or_future_preview_lineage_is_rejected_without_mutation() {
    let directory = temp_dir();
    let mut connection = preview_fourteen(&directory.path().join("mixed.db"));
    connection
        .execute_batch(
            "CREATE TABLE question_interaction(request_id TEXT PRIMARY KEY, fabricated TEXT);",
        )
        .unwrap();
    let before = structure(&connection);
    let rows = snapshot_rows(&connection, &before);
    assert!(migration::apply(&mut connection).is_err());
    assert_same_structure(
        &structure(&connection),
        &before,
        "mixed lineage remains unchanged",
    );
    assert_rows_preserved(&connection, &rows, &[]);
    let mut current = fresh_current();
    current
        .execute("UPDATE zuno_preview_schema SET format=999", [])
        .unwrap();
    let before = structure(&current);
    let rows = snapshot_rows(&current, &before);
    assert!(migration::apply(&mut current).is_err());
    assert_same_structure(
        &structure(&current),
        &before,
        "future overlay remains unchanged",
    );
    assert_rows_preserved(&current, &rows, &[]);
}
