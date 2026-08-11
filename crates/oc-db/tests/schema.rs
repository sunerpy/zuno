use oc_db::{Connection, migration, open};
use oc_testkit::pinned_oracle_or_skip;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, PartialEq, Eq)]
struct SchemaSnapshot {
    objects: Vec<(String, String, String, String)>,
    columns: Vec<Column>,
    foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, PartialEq, Eq)]
struct Column {
    table: String,
    position: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct ForeignKey {
    table: String,
    id: i64,
    position: i64,
    referenced_table: String,
    from: String,
    to: String,
    on_update: String,
    on_delete: String,
    match_rule: String,
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

fn run_oracle(binary: &Path, root: &Path, query: &str) -> Output {
    let home = root.join("home");
    let data = root.join("data");
    let config = root.join("config");
    let cache = root.join("cache");
    let state = root.join("state");
    std::fs::create_dir_all(&home).expect("create isolated oracle home");

    Command::new(binary)
        .args(["db", "--pure", "--format", "json", query])
        .current_dir(root)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run the real opencode binary")
}

fn oracle_database(root: &Path) -> PathBuf {
    root.join("data").join("opencode").join("opencode.db")
}

fn create_rust_database(path: &Path) {
    let mut connection = open::open_at(path).expect("open Rust database");
    migration::apply(&mut connection).expect("apply Rust schema");
}

fn assert_process_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "opencode exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn user_tables(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table inventory");
    statement
        .query_map([], |row| row.get(0))
        .expect("query table inventory")
        .collect::<Result<Vec<_>, _>>()
        .expect("read table inventory")
}

fn schema_snapshot(connection: &Connection) -> SchemaSnapshot {
    let tables = user_tables(connection);
    let mut objects = {
        let mut statement = connection
            .prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_master \
                 WHERE type IN ('table', 'index') \
                   AND name NOT LIKE 'sqlite_%' \
                   AND sql IS NOT NULL \
                 ORDER BY type, name",
            )
            .expect("prepare schema object inventory");
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    normalize_sql(&row.get::<_, String>(3)?),
                ))
            })
            .expect("query schema objects")
            .collect::<Result<Vec<_>, _>>()
            .expect("read schema objects")
    };
    objects.sort();

    let mut columns = Vec::new();
    let mut foreign_keys = Vec::new();
    for table in tables {
        let mut column_statement = connection
            .prepare(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk \
                 FROM pragma_table_info(?1) ORDER BY cid",
            )
            .expect("prepare column inventory");
        columns.extend(
            column_statement
                .query_map([&table], |row| {
                    Ok(Column {
                        table: table.clone(),
                        position: row.get(0)?,
                        name: row.get(1)?,
                        declared_type: row.get::<_, String>(2)?.to_ascii_lowercase(),
                        not_null: row.get::<_, i64>(3)? != 0,
                        default_value: row
                            .get::<_, Option<String>>(4)?
                            .map(|value| normalize_sql(&value)),
                        primary_key_position: row.get(5)?,
                    })
                })
                .expect("query columns")
                .collect::<Result<Vec<_>, _>>()
                .expect("read columns"),
        );

        let mut foreign_key_statement = connection
            .prepare(
                "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete, \"match\" \
                 FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
            )
            .expect("prepare foreign key inventory");
        foreign_keys.extend(
            foreign_key_statement
                .query_map([&table], |row| {
                    Ok(ForeignKey {
                        table: table.clone(),
                        id: row.get(0)?,
                        position: row.get(1)?,
                        referenced_table: row.get(2)?,
                        from: row.get(3)?,
                        to: row.get(4)?,
                        on_update: row.get::<_, String>(5)?.to_ascii_uppercase(),
                        on_delete: row.get::<_, String>(6)?.to_ascii_uppercase(),
                        match_rule: row.get::<_, String>(7)?.to_ascii_uppercase(),
                    })
                })
                .expect("query foreign keys")
                .collect::<Result<Vec<_>, _>>()
                .expect("read foreign keys"),
        );
    }

    SchemaSnapshot {
        objects,
        columns,
        foreign_keys,
    }
}

fn normalize_sql(sql: &str) -> String {
    sql.replace(['`', '"'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .to_ascii_lowercase()
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
fn schema_matches_a_database_created_by_the_real_opencode_binary() {
    let Some(binary) = pinned_oracle_or_skip(
        "schema_matches_a_database_created_by_the_real_opencode_binary",
        "the schema was NOT compared against a real release",
    ) else {
        return;
    };
    let dir = temp_dir();
    let rust_path = dir.path().join("rust").join("opencode.db");
    create_rust_database(&rust_path);

    let oracle_root = dir.path().join("oracle");
    let output = run_oracle(binary, &oracle_root, "SELECT 1 AS opened");
    assert_process_succeeded(&output);
    let oracle_path = oracle_database(&oracle_root);

    let rust = Connection::open(&rust_path).expect("open Rust database for inspection");
    let oracle = Connection::open(&oracle_path).expect("open oracle database for inspection");
    let rust_snapshot = schema_snapshot(&rust);
    let oracle_snapshot = schema_snapshot(&oracle);

    assert_eq!(
        user_tables(&rust).len(),
        20,
        "19 schema tables plus migration"
    );
    assert_eq!(rust_snapshot, oracle_snapshot);
    eprintln!(
        "normalized schema diff: empty; tables=20 objects={} columns={} foreign_keys={}",
        rust_snapshot.objects.len(),
        rust_snapshot.columns.len(),
        rust_snapshot.foreign_keys.len(),
    );
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
    assert_eq!(ids.len(), 38);
    assert_eq!(
        ids.first().map(String::as_str),
        Some("20260127222353_familiar_lady_ursula")
    );
    assert_eq!(
        ids.last().map(String::as_str),
        Some("20260622202450_simplify_session_input")
    );
}

#[test]
fn schema_journal_round_trip_through_the_real_binary_does_not_replay_migrations() {
    let Some(binary) = pinned_oracle_or_skip(
        "schema_journal_round_trip_through_the_real_binary_does_not_replay_migrations",
        "the migration journal was NOT round-tripped through a real release",
    ) else {
        return;
    };
    let root = temp_dir();
    let path = oracle_database(root.path());
    create_rust_database(&path);

    let before_connection = Connection::open(&path).expect("open Rust-created database");
    let before = journal_ids(&before_connection);
    drop(before_connection);

    let output = run_oracle(
        binary,
        root.path(),
        "SELECT count(*) AS migration_count FROM migration",
    );
    eprintln!(
        "real opencode status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    );
    assert_process_succeeded(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("38"),
        "unexpected opencode output: {stdout}"
    );

    let after_connection = Connection::open(&path).expect("reopen after opencode");
    let after = journal_ids(&after_connection);
    eprintln!(
        "migration journal count before={} after={}",
        before.len(),
        after.len()
    );
    assert_eq!(after.len(), 38);
    assert_eq!(
        after, before,
        "opencode changed the completed migration set"
    );
}

#[test]
fn schema_session_delete_cascades_through_every_declared_dependent_table() {
    let mut connection = open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(
            "INSERT INTO project \
               (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-1', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);
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
             VALUES ('session-1', 'share-1', 'secret', 'https://example.invalid', 1, 1);",
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
