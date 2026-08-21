use std::path::Path;
use zuno_db::{Connection, migration, open};

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

fn create_rust_database(path: &Path) {
    let mut connection = open::open_at(path).expect("open Rust database");
    migration::apply(&mut connection).expect("apply Rust schema");
}

fn journal_ids(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT id FROM migration ORDER BY rowid")
        .expect("prepare migration journal query");
    statement
        .query_map([], |row| row.get(0))
        .expect("query migration journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("read migration journal")
}

fn row_count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

#[test]
fn schema_part_session_id_is_indexed_but_is_not_a_foreign_key() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    create_rust_database(&path);
    let connection = Connection::open(path).expect("open schema database");

    let session_foreign_keys: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_foreign_key_list('part') WHERE \"from\" = 'session_id'",
            [],
            |row| row.get(0),
        )
        .expect("count part.session_id foreign keys");
    assert_eq!(session_foreign_keys, 0);

    let index_columns: String = connection
        .query_row(
            "SELECT group_concat(name, ',') FROM pragma_index_info('part_session_idx') ORDER BY seqno",
            [],
            |row| row.get(0),
        )
        .expect("read part_session_idx columns");
    assert_eq!(index_columns, "session_id");
}

#[test]
fn schema_prefills_every_current_migration_id_in_generated_order() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    create_rust_database(&path);
    let connection = Connection::open(path).expect("open schema database");

    let ids = journal_ids(&connection);
    assert_eq!(ids, migration::MIGRATION_IDS);
    assert_eq!(ids.len(), 39);
    assert_eq!(
        ids.first().map(String::as_str),
        Some("20260127222353_familiar_lady_ursula")
    );
    assert_eq!(
        ids.last().map(String::as_str),
        Some("20260821160000_agent_job")
    );
}

#[test]
fn schema_session_delete_cascades_through_every_declared_dependent_table() {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project \
               (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-1', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);
             INSERT INTO session \
               (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('session-child', 'project-1', 'session-1', 'child', '/workspace', 'child', '1', 1, 1);
             INSERT INTO message \
               (id, session_id, time_created, time_updated, data) \
             VALUES ('message-1', 'session-1', 1, 1, '{}');
             INSERT INTO part \
               (id, message_id, session_id, time_created, time_updated, data) \
             VALUES ('part-1', 'message-1', 'session-1', 1, 1, '{}');
             INSERT INTO todo \
               (session_id, content, status, priority, position, time_created, time_updated) \
             VALUES ('session-1', 'item', 'pending', 'high', 0, 1, 1);
             INSERT INTO session_message \
               (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES ('session-message-1', 'session-1', 'user', 1, 1, 1, '{}');
             INSERT INTO session_input \
               (id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created) \
             VALUES ('session-input-1', 'session-1', '{}', 'inbox', 1, 1, 1);
             INSERT INTO session_context_epoch \
               (session_id, baseline, snapshot, baseline_seq) \
             VALUES ('session-1', 'base', '{}', 1);
             INSERT INTO session_share \
               (session_id, id, secret, url, time_created, time_updated) \
             VALUES ('session-1', 'share-1', 'secret', 'https://example.invalid', 1, 1);
             INSERT INTO agent_job \
               (id, parent_session_id, child_session_id, status, report_delivery, created_seq, time_created, time_updated) \
             VALUES ('job-1', 'session-1', 'session-child', 'running', 'next-step', 1, 1, 1);",
        )
        .expect("seed a complete session graph");

    let dependent_tables = [
        "message",
        "part",
        "todo",
        "session_message",
        "session_input",
        "session_context_epoch",
        "session_share",
        "agent_job",
    ];
    let before: Vec<_> = dependent_tables
        .iter()
        .map(|table| (*table, row_count(&connection, table)))
        .collect();
    eprintln!("cascade counts before: {before:?}");
    assert!(
        before.iter().all(|(_, count)| *count == 1),
        "before: {before:?}"
    );

    connection
        .execute("DELETE FROM session WHERE id = 'session-1'", [])
        .expect("delete session");
    let after: Vec<_> = dependent_tables
        .iter()
        .map(|table| (*table, row_count(&connection, table)))
        .collect();
    eprintln!("cascade counts after: {after:?}");
    assert!(
        after.iter().all(|(_, count)| *count == 0),
        "after: {after:?}"
    );
    assert_eq!(row_count(&connection, "project"), 1);
}
