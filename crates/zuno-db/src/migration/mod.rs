//! Atomic current-schema creation and guarded format upgrades.
//!
//! Format 5 is the first historical layout Zuno upgrades in place. The learning
//! flywheel, the durable Plan stack, and the tool-verification receipt ledger add
//! only tables, indices, and nullable/defaulted columns, so every upgrade can
//! preserve every existing session, message, Plan, and resident-memory row. Other
//! older, newer, or unmarked layouts are still rejected without mutation.

use crate::{open, schema};
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use zuno_error::DbError;

/// Current database format.
///
/// Bump this whenever [`crate::schema`] changes incompatibly.
pub const CURRENT_FORMAT: u32 = 8;
const LEARNING_UPGRADE_FROM: u32 = 5;
const PLAN_STACK_UPGRADE_FROM: u32 = 6;
const VERIFICATION_UPGRADE_FROM: u32 = 7;

const FORMAT_TABLE: &str = "zuno_schema";
const FORMAT_SQL: &str = "
CREATE TABLE zuno_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL
)";

/// Ensure that `connection` uses the current all-at-once schema.
///
/// Empty databases are initialized. Every supported older format advances to
/// [`CURRENT_FORMAT`] in one `BEGIN IMMEDIATE` transaction that applies each
/// remaining additive step in order, with the marker changed only after every
/// new object exists. Other existing formats are only validated or rejected.
///
/// # Errors
///
/// [`DbError::SchemaMismatch`] for another unsupported format,
/// [`DbError::Schema`] for invalid DDL or marker storage, and [`DbError::Busy`]
/// while another writer owns SQLite's write lock.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    let tables = table_names(connection)?;
    if tables.is_empty() {
        return create_current(connection);
    }
    match observed_format(connection, &tables)? {
        Some(CURRENT_FORMAT) => validate_current(connection, &tables),
        Some(LEARNING_UPGRADE_FROM) => migrate_learning(connection),
        Some(PLAN_STACK_UPGRADE_FROM) => migrate_plan_stack(connection),
        Some(VERIFICATION_UPGRADE_FROM) => migrate_verification(connection),
        observed => Err(DbError::SchemaMismatch {
            expected: CURRENT_FORMAT,
            observed,
        }),
    }
}

fn validate_current(connection: &Connection, tables: &[String]) -> Result<(), DbError> {
    let required = [
        "session",
        "message_feedback",
        "experience_record",
        "work_plan_archive",
        "verification_receipt",
    ];
    let missing = required
        .into_iter()
        .find(|required| !tables.iter().any(|table| table == required));
    if let Some(missing) = missing {
        return Err(failure(std::io::Error::other(format!(
            "current schema marker exists without required table `{missing}`"
        ))));
    }
    let work_plan_columns = column_names(connection, "work_plan")?;
    for required in ["parent_plan_id", "stack_depth"] {
        if !work_plan_columns.iter().any(|column| column == required) {
            return Err(failure(std::io::Error::other(format!(
                "current schema marker exists without required work_plan column `{required}`"
            ))));
        }
    }
    Ok(())
}

fn observed_format(connection: &Connection, tables: &[String]) -> Result<Option<u32>, DbError> {
    if !tables.iter().any(|table| table == FORMAT_TABLE) {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT format FROM zuno_schema WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_error)
}

/// Create the current schema and format marker atomically.
fn create_current(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    if !tables.is_empty() {
        return Err(failure(std::io::Error::other(format!(
            "refusing to create the current schema over an existing database: {}",
            tables.join(", ")
        ))));
    }
    schema::up(&transaction)?;
    transaction.execute(FORMAT_SQL, []).map_err(map_error)?;
    transaction
        .execute(
            "INSERT INTO zuno_schema (singleton, format) VALUES (1, ?1)",
            params![CURRENT_FORMAT],
        )
        .map_err(map_error)?;
    transaction.commit().map_err(map_error)
}

/// Add every post-format-5 additive object without rewriting any historical row.
fn migrate_learning(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(LEARNING_UPGRADE_FROM) {
        return Err(DbError::SchemaMismatch {
            expected: LEARNING_UPGRADE_FROM,
            observed,
        });
    }
    if !tables.iter().any(|table| table == "session") {
        return Err(failure(std::io::Error::other(
            "format-5 marker exists without the required session table",
        )));
    }
    schema::up_learning(&transaction)?;
    schema::up_plan_stack(&transaction)?;
    schema::up_verification(&transaction)?;
    let changed = transaction
        .execute(
            "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1 AND format = ?2",
            params![CURRENT_FORMAT, LEARNING_UPGRADE_FROM],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(failure(std::io::Error::other(
            "format-5 marker changed while the learning migration was running",
        )));
    }
    transaction.commit().map_err(map_error)
}

/// Add durable Plan frames without rewriting any format-6 row.
fn migrate_plan_stack(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(PLAN_STACK_UPGRADE_FROM) {
        return Err(DbError::SchemaMismatch {
            expected: PLAN_STACK_UPGRADE_FROM,
            observed,
        });
    }
    if !tables.iter().any(|table| table == "work_plan") {
        return Err(failure(std::io::Error::other(
            "format-6 marker exists without the required work_plan table",
        )));
    }
    schema::up_plan_stack(&transaction)?;
    schema::up_verification(&transaction)?;
    let changed = transaction
        .execute(
            "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1 AND format = ?2",
            params![CURRENT_FORMAT, PLAN_STACK_UPGRADE_FROM],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(failure(std::io::Error::other(
            "format-6 marker changed while the Plan-stack migration was running",
        )));
    }
    transaction.commit().map_err(map_error)
}

/// Add the tool-verification receipt ledger without rewriting any format-7 row.
fn migrate_verification(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(VERIFICATION_UPGRADE_FROM) {
        return Err(DbError::SchemaMismatch {
            expected: VERIFICATION_UPGRADE_FROM,
            observed,
        });
    }
    if !tables.iter().any(|table| table == "session") {
        return Err(failure(std::io::Error::other(
            "format-7 marker exists without the required session table",
        )));
    }
    schema::up_verification(&transaction)?;
    let changed = transaction
        .execute(
            "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1 AND format = ?2",
            params![CURRENT_FORMAT, VERIFICATION_UPGRADE_FROM],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(failure(std::io::Error::other(
            "format-7 marker changed while the verification migration was running",
        )));
    }
    transaction.commit().map_err(map_error)
}

fn table_names(connection: &Connection) -> Result<Vec<String>, DbError> {
    query_table_names(connection)
}

fn transaction_table_names(transaction: &Transaction<'_>) -> Result<Vec<String>, DbError> {
    query_table_names(transaction)
}

fn query_table_names(connection: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
}

fn column_names(connection: &Connection, table: &str) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
}

pub(crate) fn map_error(error: rusqlite::Error) -> DbError {
    if open::is_busy(&error) {
        return open::map_error(error);
    }
    failure(error)
}

fn failure(source: impl std::error::Error + Send + Sync + 'static) -> DbError {
    DbError::Schema {
        format: CURRENT_FORMAT,
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        open::open(&zuno_paths::DbLocation::Memory).expect("open memory database")
    }

    fn remove_plan_stack_schema(connection: &Connection) {
        connection
            .execute_batch(
                "DROP TABLE work_plan_archive;
                 ALTER TABLE work_plan DROP COLUMN parent_plan_id;
                 ALTER TABLE work_plan DROP COLUMN stack_depth;",
            )
            .expect("construct pre-Plan-stack schema");
    }

    fn remove_verification_schema(connection: &Connection) {
        connection
            .execute_batch("DROP TABLE verification_receipt;")
            .expect("construct pre-verification schema");
    }

    #[test]
    fn empty_database_gets_one_current_format_marker_and_reopens_unchanged() {
        let mut connection = memory();
        apply(&mut connection).expect("create current schema");
        apply(&mut connection).expect("validate current schema");

        let format: u32 = connection
            .query_row(
                "SELECT format FROM zuno_schema WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read current marker");
        assert_eq!(format, CURRENT_FORMAT);
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM zuno_schema", [], |row| row.get(0))
            .expect("count format rows");
        assert_eq!(rows, 1);
    }

    #[test]
    fn unmarked_pre_release_database_is_rejected_without_mutation() {
        let mut connection = memory();
        connection
            .execute_batch(
                "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
                 INSERT INTO session VALUES ('session-1', 'real history');
                 CREATE TABLE migration (id text PRIMARY KEY);",
            )
            .expect("stand in for the retired layout");

        let error = apply(&mut connection).expect_err("old format must be refused");
        assert!(matches!(
            error,
            DbError::SchemaMismatch {
                expected: CURRENT_FORMAT,
                observed: None
            }
        ));
        let title: String = connection
            .query_row("SELECT title FROM session", [], |row| row.get(0))
            .expect("history survives rejection");
        assert_eq!(title, "real history");
        assert!(
            !table_names(&connection)
                .expect("inventory")
                .iter()
                .any(|table| table == FORMAT_TABLE)
        );
    }

    #[test]
    fn another_format_is_rejected_before_application_queries_run() {
        let mut connection = memory();
        connection
            .execute_batch(
                "CREATE TABLE session (id text PRIMARY KEY);
                 CREATE TABLE zuno_schema (
                   singleton integer PRIMARY KEY CHECK (singleton = 1),
                   format integer NOT NULL
                 );
                 INSERT INTO zuno_schema VALUES (1, 99);",
            )
            .expect("create another format");

        let error = apply(&mut connection).expect_err("wrong format must be refused");
        assert!(matches!(
            error,
            DbError::SchemaMismatch {
                expected: CURRENT_FORMAT,
                observed: Some(99)
            }
        ));
    }

    #[test]
    fn current_marker_without_plan_stack_columns_is_rejected_without_mutation() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        connection
            .execute_batch(
                "DROP TABLE work_plan_archive;
                 ALTER TABLE work_plan DROP COLUMN parent_plan_id;
                 ALTER TABLE work_plan DROP COLUMN stack_depth;
                 CREATE TABLE work_plan_archive (id text PRIMARY KEY);",
            )
            .expect("construct corrupt current shape");

        let error = apply(&mut connection).expect_err("corrupt current shape must fail");
        assert!(matches!(error, DbError::Schema { .. }));
        assert_eq!(
            connection
                .query_row(
                    "SELECT format FROM zuno_schema WHERE singleton=1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("read unchanged marker"),
            CURRENT_FORMAT
        );
        assert!(
            !column_names(&connection, "work_plan")
                .expect("read unchanged columns")
                .iter()
                .any(|column| column == "stack_depth")
        );
    }

    #[test]
    fn format_five_upgrades_without_rewriting_history() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_plan_stack_schema(&connection);
        remove_verification_schema(&connection);
        connection
            .execute_batch(
                "INSERT INTO project \
                   (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-1', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('session-1', 'project-1', 'slug', '/workspace', 'history', '1', 1, 1);
                 INSERT INTO message \
                   (id, session_id, time_created, time_updated, data) \
                 VALUES ('message-1', 'session-1', 1, 1, '{\"role\":\"user\"}');
                 INSERT INTO memory_candidate (
                   id, target, target_path, action, content, reason, confidence, source_kind,
                   source_session_id, source_message_id, status, time_created, time_updated
                 ) VALUES (
                   'memory-1', 'project', '/workspace/MEMORY.md', 'add', 'keep history',
                   'fixture', 9000, 'user', 'session-1', 'message-1', 'pending', 1, 1
                 );
                 INSERT INTO work_plan
                   (session_id, id, goal_id, revision, title, steps, time_created, time_updated)
                 VALUES (
                   'session-1', 'plan-1', NULL, 4, 'preserve the plan',
                   '[{\"id\":\"inspect\",\"title\":\"Inspect history\",\"status\":\"in_progress\"}]',
                   1, 2
                 );
                 DROP TABLE skill_candidate;
                 DROP TABLE evaluation_result;
                 DROP TABLE evaluation_run;
                 DROP TABLE evaluation_case;
                 DROP TABLE evaluation_suite;
                 DROP TABLE learning_pattern;
                 DROP TABLE experience_evidence;
                 DROP TABLE experience_record;
                 DROP TABLE learning_job;
                 DROP TABLE message_feedback;
                 UPDATE zuno_schema SET format = 5 WHERE singleton = 1;",
            )
            .expect("construct exact additive format-five shape");

        let session_before: (String, String) = connection
            .query_row(
                "SELECT id, title FROM session WHERE id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read session before migration");
        let message_before: String = connection
            .query_row(
                "SELECT data FROM message WHERE id = 'message-1'",
                [],
                |row| row.get(0),
            )
            .expect("read message before migration");
        let memory_before: String = connection
            .query_row(
                "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                [],
                |row| row.get(0),
            )
            .expect("read memory before migration");
        let plan_before: (String, i64, String) = connection
            .query_row(
                "SELECT id, revision, steps FROM work_plan WHERE session_id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read plan before migration");

        apply(&mut connection).expect("upgrade format five");

        let format: u32 = connection
            .query_row(
                "SELECT format FROM zuno_schema WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read upgraded format");
        assert_eq!(format, CURRENT_FORMAT);
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, title FROM session WHERE id = 'session-1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read session after migration"),
            session_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT data FROM message WHERE id = 'message-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read message after migration"),
            message_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read memory after migration"),
            memory_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, revision, steps FROM work_plan WHERE session_id = 'session-1'",
                    [],
                    |row| Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?
                    )),
                )
                .expect("read plan after migration"),
            plan_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT parent_plan_id, stack_depth FROM work_plan \
                     WHERE session_id = 'session-1'",
                    [],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read migrated Plan stack fields"),
            (None, 0)
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM experience_record", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("query newly installed table"),
            0
        );
    }

    #[test]
    fn format_six_adds_plan_stack_without_rewriting_the_active_plan() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_plan_stack_schema(&connection);
        remove_verification_schema(&connection);
        connection
            .execute_batch(
                "INSERT INTO project \
                   (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-1', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('session-1', 'project-1', 'slug', '/workspace', 'history', '1', 1, 1);
                 INSERT INTO work_plan
                   (session_id, id, goal_id, revision, title, steps, time_created, time_updated)
                 VALUES (
                   'session-1', 'plan-1', 'goal-1', 7, 'durable plan',
                   '[{\"id\":\"verify\",\"title\":\"Verify release\",\"status\":\"in_progress\"}]',
                   2, 3
                 );
                 UPDATE zuno_schema SET format = 6 WHERE singleton = 1;",
            )
            .expect("construct format-six schema");
        let before: (String, Option<String>, i64, String, String, i64, i64) = connection
            .query_row(
                "SELECT id, goal_id, revision, title, steps, time_created, time_updated \
                 FROM work_plan WHERE session_id='session-1'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("read format-six plan");

        apply(&mut connection).expect("upgrade format six");

        let after: (String, Option<String>, i64, String, String, i64, i64) = connection
            .query_row(
                "SELECT id, goal_id, revision, title, steps, time_created, time_updated \
                 FROM work_plan WHERE session_id='session-1'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("read migrated plan");
        assert_eq!(after, before);
        assert_eq!(
            connection
                .query_row(
                    "SELECT parent_plan_id, stack_depth FROM work_plan \
                     WHERE session_id='session-1'",
                    [],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read Plan stack defaults"),
            (None, 0)
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM work_plan_archive", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("query archive"),
            0
        );
    }

    #[test]
    fn format_seven_adds_the_verification_ledger_without_rewriting_history() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_verification_schema(&connection);
        connection
            .execute_batch(
                "INSERT INTO project \
                   (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-1', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('session-1', 'project-1', 'slug', '/workspace', 'history', '1', 1, 1);
                 INSERT INTO work_plan
                   (session_id, id, goal_id, revision, title, steps, time_created, time_updated,
                    stack_depth)
                 VALUES (
                   'session-1', 'plan-1', 'goal-1', 7, 'durable plan', '[]', 2, 3, 0
                 );
                 UPDATE zuno_schema SET format = 7 WHERE singleton = 1;",
            )
            .expect("construct format-seven schema");

        apply(&mut connection).expect("upgrade format seven");

        assert_eq!(
            connection
                .query_row(
                    "SELECT format FROM zuno_schema WHERE singleton = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("read migrated marker"),
            CURRENT_FORMAT
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT title FROM work_plan WHERE session_id = 'session-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("plan survives the upgrade"),
            "durable plan"
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM verification_receipt", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .expect("query newly installed ledger"),
            0
        );
        apply(&mut connection).expect("reopening the upgraded database validates");
    }

    #[test]
    fn current_marker_without_the_verification_ledger_is_rejected_without_mutation() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_verification_schema(&connection);

        let error = apply(&mut connection).expect_err("corrupt current shape must fail");
        assert!(matches!(error, DbError::Schema { .. }));
        assert_eq!(
            connection
                .query_row(
                    "SELECT format FROM zuno_schema WHERE singleton=1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("read unchanged marker"),
            CURRENT_FORMAT
        );
    }

    #[test]
    fn create_current_refuses_any_non_empty_database() {
        let mut connection = memory();
        connection
            .execute_batch("CREATE TABLE something_else (id text PRIMARY KEY)")
            .expect("create unrelated table");

        let error = create_current(&mut connection).expect_err("must refuse");
        let source = std::error::Error::source(&error)
            .expect("schema error carries source")
            .to_string();
        assert!(source.contains("something_else"), "{source}");
        assert_eq!(
            table_names(&connection).expect("inventory"),
            ["something_else"]
        );
    }
}
