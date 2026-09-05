//! Atomic current-schema creation and guarded format upgrades.
//!
//! Format 5 is the first historical layout Zuno upgrades in place. The learning
//! flywheel, the durable Plan stack, the tool-verification receipt ledger, and
//! per-session memory policy add only tables, indices, and nullable/defaulted columns,
//! so every upgrade can preserve every existing session, message, Plan, and
//! resident-memory row. Other older, newer, or unmarked layouts are still rejected
//! without mutation.

use crate::{open, schema};
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use zuno_error::DbError;

/// Current database format.
///
/// Bump this whenever [`crate::schema`] changes incompatibly.
pub const CURRENT_FORMAT: u32 = 9;
const LEARNING_UPGRADE_FROM: u32 = 5;
const PLAN_STACK_UPGRADE_FROM: u32 = 6;
const VERIFICATION_UPGRADE_FROM: u32 = 7;
const MEMORY_POLICY_UPGRADE_FROM: u32 = 8;

const FORMAT_TABLE: &str = "zuno_schema";
const FORMAT_SQL: &str = "
CREATE TABLE zuno_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL
)";

/// How many times one opener re-reads the format after losing a write-lock race
/// before it reports the database as unsettled instead of looping.
const MAX_DISPATCH_ATTEMPTS: u32 = 4;

/// Outcome of one dispatch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// The database is at [`CURRENT_FORMAT`] and structurally valid.
    Settled,
    /// The write lock arrived after another opener changed the format, so the
    /// decision taken from the pre-lock inventory is stale and must be made
    /// again. `observed` is the marker seen inside the write transaction, `None`
    /// when the database has none.
    Moved { observed: Option<u32> },
}

/// Ensure that `connection` uses the current all-at-once schema.
///
/// Empty databases are initialized. Every supported older format advances to
/// [`CURRENT_FORMAT`] in one `BEGIN IMMEDIATE` transaction that applies each
/// remaining additive step in order, with the marker changed only after every
/// new object exists. Other existing formats are only validated or rejected.
///
/// The format is inventoried before the write lock is taken, so two processes
/// opening or upgrading the same file at once both decide from the same old
/// state and one of them finds a different format once it holds the lock. That
/// opener does not fail: it re-reads and dispatches again, at most
/// [`MAX_DISPATCH_ATTEMPTS`] times, and settles on validating the schema the
/// winner committed. Only a format no step can handle is a mismatch.
///
/// # Errors
///
/// [`DbError::SchemaMismatch`] for another unsupported format,
/// [`DbError::Schema`] for invalid DDL or marker storage, [`DbError::Busy`]
/// while another writer owns SQLite's write lock past the busy timeout, and
/// [`DbError::Conflict`] on the `zuno_schema` marker when the format kept
/// changing under every dispatch attempt.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    apply_with(connection, dispatch_once)
}

/// Run `dispatch` until it settles, giving up after [`MAX_DISPATCH_ATTEMPTS`].
fn apply_with(
    connection: &mut Connection,
    mut dispatch: impl FnMut(&mut Connection) -> Result<Dispatch, DbError>,
) -> Result<(), DbError> {
    let mut last_observed = None;
    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        match dispatch(connection)? {
            Dispatch::Settled => return Ok(()),
            Dispatch::Moved { observed } => last_observed = observed,
        }
    }
    let last_observed = last_observed.map_or_else(|| "no marker".to_owned(), |f| f.to_string());
    Err(DbError::Conflict {
        table: FORMAT_TABLE.to_owned(),
        id: "1".to_owned(),
        detail: format!(
            "the format marker changed under {MAX_DISPATCH_ATTEMPTS} consecutive opener \
             dispatches; last observed {last_observed} while expecting {CURRENT_FORMAT}"
        ),
    })
}

/// Inventory the database once and run the step that state calls for.
fn dispatch_once(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let tables = table_names(connection)?;
    if tables.is_empty() {
        return create_current(connection);
    }
    match observed_format(connection, &tables)? {
        Some(CURRENT_FORMAT) => {
            validate_current(connection, &tables)?;
            Ok(Dispatch::Settled)
        }
        Some(LEARNING_UPGRADE_FROM) => migrate_learning(connection),
        Some(PLAN_STACK_UPGRADE_FROM) => migrate_plan_stack(connection),
        Some(VERIFICATION_UPGRADE_FROM) => migrate_verification(connection),
        Some(MEMORY_POLICY_UPGRADE_FROM) => migrate_memory_policy(connection),
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
        "session_memory_policy",
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
    let memory_policy_columns = column_names(connection, "session_memory_policy")?;
    for required in [
        "session_id",
        "use_memories",
        "generation",
        "reason",
        "source",
        "revision",
        "time_created",
        "time_updated",
    ] {
        if !memory_policy_columns
            .iter()
            .any(|column| column == required)
        {
            return Err(failure(std::io::Error::other(format!(
                "current schema marker exists without required session_memory_policy column \
                 `{required}`"
            ))));
        }
    }
    validate_memory_policy_shape(connection)?;
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
///
/// A database that is no longer empty once the write lock is held was populated
/// by another opener in the meantime. Nothing is ever created over it; the
/// decision goes back to [`apply`], which re-reads the marker and validates,
/// upgrades, or rejects what is actually there.
fn create_current(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    if !tables.is_empty() {
        return Ok(Dispatch::Moved {
            observed: observed_format(&transaction, &tables)?,
        });
    }
    schema::up(&transaction)?;
    transaction.execute(FORMAT_SQL, []).map_err(map_error)?;
    transaction
        .execute(
            "INSERT INTO zuno_schema (singleton, format) VALUES (1, ?1)",
            params![CURRENT_FORMAT],
        )
        .map_err(map_error)?;
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

/// Add every post-format-5 additive object without rewriting any historical row.
fn migrate_learning(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(LEARNING_UPGRADE_FROM) {
        return Ok(Dispatch::Moved { observed });
    }
    if !tables.iter().any(|table| table == "session") {
        return Err(failure(std::io::Error::other(
            "format-5 marker exists without the required session table",
        )));
    }
    schema::up_learning(&transaction)?;
    schema::up_plan_stack(&transaction)?;
    schema::up_verification(&transaction)?;
    schema::up_memory_policy(&transaction)?;
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
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

/// Add durable Plan frames without rewriting any format-6 row.
fn migrate_plan_stack(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(PLAN_STACK_UPGRADE_FROM) {
        return Ok(Dispatch::Moved { observed });
    }
    if !tables.iter().any(|table| table == "work_plan") {
        return Err(failure(std::io::Error::other(
            "format-6 marker exists without the required work_plan table",
        )));
    }
    schema::up_plan_stack(&transaction)?;
    schema::up_verification(&transaction)?;
    schema::up_memory_policy(&transaction)?;
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
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

/// Add the tool-verification receipt ledger without rewriting any format-7 row.
fn migrate_verification(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(VERIFICATION_UPGRADE_FROM) {
        return Ok(Dispatch::Moved { observed });
    }
    if !tables.iter().any(|table| table == "session") {
        return Err(failure(std::io::Error::other(
            "format-7 marker exists without the required session table",
        )));
    }
    schema::up_verification(&transaction)?;
    schema::up_memory_policy(&transaction)?;
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
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

/// Add revisioned per-session memory policy without rewriting any format-8 row.
fn migrate_memory_policy(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(MEMORY_POLICY_UPGRADE_FROM) {
        return Ok(Dispatch::Moved { observed });
    }
    for required in ["session", "learning_job"] {
        if !tables.iter().any(|table| table == required) {
            return Err(failure(std::io::Error::other(format!(
                "format-8 marker exists without the required {required} table"
            ))));
        }
    }
    schema::up_memory_policy(&transaction)?;
    let changed = transaction
        .execute(
            "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1 AND format = ?2",
            params![CURRENT_FORMAT, MEMORY_POLICY_UPGRADE_FROM],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(failure(std::io::Error::other(
            "format-8 marker changed while the memory-policy migration was running",
        )));
    }
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
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

fn validate_memory_policy_shape(connection: &Connection) -> Result<(), DbError> {
    let primary_key: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('session_memory_policy')
             WHERE name = 'session_id' AND lower(type) = 'text' AND pk = 1",
            [],
            |row| row.get(0),
        )
        .map_err(map_error)?;
    if primary_key != 1 {
        return Err(failure(std::io::Error::other(
            "current session_memory_policy.session_id is not the TEXT primary key",
        )));
    }
    let required_not_null: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('session_memory_policy')
             WHERE name IN (
               'use_memories','generation','reason','source','revision',
               'time_created','time_updated'
             ) AND \"notnull\" = 1",
            [],
            |row| row.get(0),
        )
        .map_err(map_error)?;
    if required_not_null != 7 {
        return Err(failure(std::io::Error::other(
            "current session_memory_policy is missing required NOT NULL constraints",
        )));
    }
    let foreign_key: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_foreign_key_list('session_memory_policy')
             WHERE \"table\" = 'session' AND \"from\" = 'session_id' AND \"to\" = 'id'
               AND upper(on_delete) = 'CASCADE'",
            [],
            |row| row.get(0),
        )
        .map_err(map_error)?;
    if foreign_key != 1 {
        return Err(failure(std::io::Error::other(
            "current session_memory_policy is missing its cascading session foreign key",
        )));
    }
    let mut index_statement = connection
        .prepare(
            "SELECT name FROM pragma_index_info(
               'session_memory_policy_generation_updated_idx'
             ) ORDER BY seqno",
        )
        .map_err(map_error)?;
    let index_columns = index_statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)?;
    if index_columns != ["generation", "time_updated", "session_id"] {
        return Err(failure(std::io::Error::other(
            "current session_memory_policy generation index is missing or malformed",
        )));
    }
    let table_sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'session_memory_policy'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(map_error)?;
    let normalized = table_sql
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '`')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    for required in [
        "check(use_memoriesin(0,1))",
        "check(generationin('enabled','disabled','excluded'))",
        "check(length(trim(reason))>0)",
        "check(length(trim(source))>0)",
        "check(revision>=1)",
        "check(time_created>=0)",
        "check(time_updated>=time_created)",
    ] {
        if !normalized.contains(required) {
            return Err(failure(std::io::Error::other(format!(
                "current session_memory_policy is missing constraint `{required}`"
            ))));
        }
    }
    Ok(())
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

    // The helpers below synthesize an older shape by removing pieces of the current
    // schema. They exercise each additive step in isolation; the exact databases the
    // v0.0.3, v0.2.2, v0.6.7, and v0.10.5 releases wrote live in
    // `tests/fixtures/format-*.sql` and are upgraded end to end by
    // `tests/migration_fixtures.rs`.
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

    fn remove_memory_policy_schema(connection: &Connection) {
        connection
            .execute_batch("DROP TABLE session_memory_policy;")
            .expect("construct pre-memory-policy schema");
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
        remove_memory_policy_schema(&connection);
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
        remove_memory_policy_schema(&connection);
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

    /// Every table a v0.6.7 database (format 7) contains, in `sqlite_master` order.
    const FORMAT_SEVEN_TABLES: [&str; 38] = [
        "account",
        "account_state",
        "agent_job",
        "control_account",
        "credential",
        "data_migration",
        "evaluation_case",
        "evaluation_result",
        "evaluation_run",
        "evaluation_suite",
        "event",
        "event_sequence",
        "experience_evidence",
        "experience_record",
        "human_request",
        "learning_job",
        "learning_pattern",
        "memory_candidate",
        "memory_reflection_delivery",
        "memory_reflection_job",
        "message",
        "message_feedback",
        "part",
        "permission",
        "project",
        "project_directory",
        "provider_retry_backoff",
        "session",
        "session_context_epoch",
        "session_input",
        "session_message",
        "session_share",
        "skill_candidate",
        "work_item",
        "work_plan",
        "work_plan_archive",
        "workspace",
        "zuno_schema",
    ];

    #[test]
    fn format_seven_adds_the_verification_ledger_without_rewriting_history() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_memory_policy_schema(&connection);
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
                 INSERT INTO work_plan_archive
                   (id, session_id, parent_plan_id, stack_depth, goal_id, revision, title, steps,
                    state, time_created, time_updated, time_archived)
                 VALUES (
                   'plan-0', 'session-1', NULL, 0, 'goal-1', 3, 'parent plan',
                   '[{\"id\":\"deliver\",\"title\":\"Deliver\",\"status\":\"in_progress\"}]',
                   'suspended', 1, 2, 2
                 );
                 INSERT INTO work_plan
                   (session_id, id, parent_plan_id, stack_depth, goal_id, revision, title, steps,
                    time_created, time_updated)
                 VALUES (
                   'session-1', 'plan-1', 'plan-0', 1, 'goal-1', 7, 'durable plan',
                   '[{\"id\":\"ship\",\"title\":\"Ship it\",\"status\":\"in_progress\"}]', 2, 3
                 );
                 UPDATE zuno_schema SET format = 7 WHERE singleton = 1;",
            )
            .expect("construct format-seven schema");
        // The fixture is derived from the current schema minus the ledger, so pin what that
        // produced against the tables v0.6.7 actually shipped: if `create_current` grows
        // another table later, this stops being a format-7 database and the test says so.
        let tables: Vec<String> = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("list tables")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query tables")
            .collect::<Result<_, _>>()
            .expect("collect tables");
        assert_eq!(
            tables, FORMAT_SEVEN_TABLES,
            "fixture is not the format-7 table set"
        );
        let session_before = connection
            .query_row(
                "SELECT id, project_id, slug, directory, title, version, time_created, \
                        time_updated \
                 FROM session WHERE id = 'session-1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .expect("read session before upgrade");
        let archive_before = connection
            .query_row(
                "SELECT id, parent_plan_id, stack_depth, goal_id, revision, title, steps, state, \
                        time_created, time_updated, time_archived \
                 FROM work_plan_archive WHERE session_id = 'session-1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .expect("read suspended parent before upgrade");
        let message_before = connection
            .query_row(
                "SELECT data FROM message WHERE id = 'message-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read message before upgrade");
        let memory_before = connection
            .query_row(
                "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read memory candidate before upgrade");
        let plan_before = connection
            .query_row(
                "SELECT id, parent_plan_id, stack_depth, goal_id, revision, title, steps, \
                        time_created, time_updated \
                 FROM work_plan WHERE session_id = 'session-1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .expect("read plan before upgrade");

        apply(&mut connection).expect("upgrade format seven");

        assert_eq!(
            connection
                .query_row(
                    "SELECT id, project_id, slug, directory, title, version, time_created, \
                            time_updated \
                     FROM session WHERE id = 'session-1'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                        ))
                    },
                )
                .expect("session survives the upgrade"),
            session_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, parent_plan_id, stack_depth, goal_id, revision, title, steps, \
                            state, time_created, time_updated, time_archived \
                     FROM work_plan_archive WHERE session_id = 'session-1'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, i64>(9)?,
                            row.get::<_, i64>(10)?,
                        ))
                    },
                )
                .expect("suspended parent survives the upgrade unchanged"),
            archive_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT data FROM message WHERE id = 'message-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("message survives the upgrade"),
            message_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("memory candidate survives the upgrade"),
            memory_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, parent_plan_id, stack_depth, goal_id, revision, title, steps, \
                            time_created, time_updated \
                     FROM work_plan WHERE session_id = 'session-1'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, i64>(8)?,
                        ))
                    },
                )
                .expect("plan survives the upgrade unchanged"),
            plan_before
        );
        for (index, unique) in [
            ("verification_receipt_call_idx", true),
            ("verification_receipt_session_time_idx", false),
        ] {
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM sqlite_master \
                         WHERE type = 'index' AND name = ?1 AND tbl_name = 'verification_receipt'",
                        [index],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("query index"),
                1,
                "the upgrade installs {index} on verification_receipt"
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT \"unique\" FROM pragma_index_list('verification_receipt') \
                         WHERE name = ?1",
                        [index],
                        |row| row.get::<_, bool>(0),
                    )
                    .expect("query index uniqueness"),
                unique,
                "{index} uniqueness is part of the at-most-once receipt contract"
            );
        }

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
    fn format_eight_adds_memory_policy_without_rewriting_history_or_learning_jobs() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_memory_policy_schema(&connection);
        connection
            .execute_batch(
                "INSERT INTO project
                   (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project-1', '/workspace', 1, 1, '[]');
                 INSERT INTO session
                   (id, project_id, slug, directory, title, version, time_created, time_updated)
                 VALUES ('session-1', 'project-1', 'slug', '/workspace', 'history', '1', 1, 1);
                 INSERT INTO message
                   (id, session_id, time_created, time_updated, data)
                 VALUES ('message-1', 'session-1', 1, 1, '{\"role\":\"assistant\"}');
                 INSERT INTO memory_candidate (
                   id, target, target_path, action, content, reason, confidence, source_kind,
                   source_session_id, source_message_id, status, time_created, time_updated
                 ) VALUES (
                   'memory-1', 'project', '/workspace/MEMORY.md', 'add', 'keep history',
                   'fixture', 9000, 'user', 'session-1', 'message-1', 'pending', 1, 1
                 );
                 INSERT INTO learning_job (
                   id, project_id, session_id, source_message_id, kind, extractor_version,
                   idempotency_key, status, attempt, scheduled_at, payload, time_created,
                   time_updated
                 ) VALUES (
                   'learning-1', 'project-1', 'session-1', 'message-1', 'extraction',
                   'extractor-v1', 'extraction:session-1:message-1:extractor-v1', 'queued', 0, 2,
                   '{\"transcript\":\"durable\"}', 2, 2
                 );
                 INSERT INTO verification_receipt (
                   id, session_id, tool_call_id, tool_id, summary, exit_authority, outcome,
                   time_created
                 ) VALUES (
                   'receipt-1', 'session-1', 'call-1', 'shell', 'cargo test passed',
                   'authoritative', 'passed', 3
                 );
                 UPDATE zuno_schema SET format = 8 WHERE singleton = 1;",
            )
            .expect("construct format-eight schema");
        assert_eq!(
            table_names(&connection)
                .expect("format-eight inventory")
                .len(),
            schema::TABLE_COUNT,
            "format 8 has every current table except session_memory_policy"
        );
        let session_before: (String, String) = connection
            .query_row(
                "SELECT id, title FROM session WHERE id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read session");
        let memory_before: String = connection
            .query_row(
                "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                [],
                |row| row.get(0),
            )
            .expect("read memory");
        let job_before: (String, String, i64, String) = connection
            .query_row(
                "SELECT id, status, attempt, payload FROM learning_job WHERE id = 'learning-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read learning job");
        let receipt_before: String = connection
            .query_row(
                "SELECT summary FROM verification_receipt WHERE id = 'receipt-1'",
                [],
                |row| row.get(0),
            )
            .expect("read verification receipt");

        apply(&mut connection).expect("upgrade format eight");

        assert_eq!(marker(&connection), CURRENT_FORMAT);
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, title FROM session WHERE id = 'session-1'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read migrated session"),
            session_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT content FROM memory_candidate WHERE id = 'memory-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read migrated memory"),
            memory_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT id, status, attempt, payload FROM learning_job WHERE id = 'learning-1'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .expect("read migrated learning job"),
            job_before
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT summary FROM verification_receipt WHERE id = 'receipt-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read migrated verification receipt"),
            receipt_before
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM session_memory_policy", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("query new policy table"),
            0
        );
        apply(&mut connection).expect("reopening the upgraded database validates");
    }

    #[test]
    fn corrupt_format_eight_without_learning_jobs_fails_closed() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_memory_policy_schema(&connection);
        connection
            .execute_batch(
                "INSERT INTO project
                   (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project-1', '/workspace', 1, 1, '[]');
                 INSERT INTO session
                   (id, project_id, slug, directory, title, version, time_created, time_updated)
                 VALUES ('session-1', 'project-1', 'slug', '/workspace', 'keep me', '1', 1, 1);
                 ALTER TABLE learning_job RENAME TO missing_learning_job;
                 UPDATE zuno_schema SET format = 8 WHERE singleton = 1;",
            )
            .expect("construct corrupt format-eight schema");

        let error = apply(&mut connection).expect_err("corrupt format eight must fail");
        assert!(matches!(error, DbError::Schema { .. }), "{error:?}");
        assert_eq!(marker(&connection), MEMORY_POLICY_UPGRADE_FROM);
        assert_eq!(
            connection
                .query_row(
                    "SELECT title FROM session WHERE id = 'session-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("history survives rejection"),
            "keep me"
        );
        assert!(
            !table_names(&connection)
                .expect("inventory")
                .iter()
                .any(|table| table == "session_memory_policy")
        );
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
    fn current_marker_without_memory_policy_is_rejected_without_mutation() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        remove_memory_policy_schema(&connection);

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
            !table_names(&connection)
                .expect("inventory")
                .iter()
                .any(|table| table == "session_memory_policy")
        );
    }

    #[test]
    fn current_marker_without_memory_policy_index_is_rejected_without_mutation() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        connection
            .execute_batch("DROP INDEX session_memory_policy_generation_updated_idx;")
            .expect("remove required index");

        let error = apply(&mut connection).expect_err("corrupt current index must fail");
        assert!(matches!(error, DbError::Schema { .. }));
        assert_eq!(marker(&connection), CURRENT_FORMAT);
        assert!(
            table_names(&connection)
                .expect("inventory")
                .iter()
                .any(|table| table == "session_memory_policy")
        );
    }

    #[test]
    fn current_marker_with_weakened_memory_policy_constraints_is_rejected() {
        let mut connection = memory();
        create_current(&mut connection).expect("create current schema");
        connection
            .execute_batch(
                "ALTER TABLE session_memory_policy RENAME TO weak_session_memory_policy;
                 DROP TABLE weak_session_memory_policy;
                 CREATE TABLE session_memory_policy (
                   session_id text PRIMARY KEY,
                   use_memories integer NOT NULL,
                   generation text NOT NULL,
                   reason text NOT NULL,
                   source text NOT NULL,
                   revision integer NOT NULL,
                   time_created integer NOT NULL,
                   time_updated integer NOT NULL
                 );
                 CREATE INDEX session_memory_policy_generation_updated_idx
                   ON session_memory_policy (generation,time_updated,session_id);",
            )
            .expect("weaken policy constraints");

        let error = apply(&mut connection).expect_err("weakened current schema must fail");
        assert!(matches!(error, DbError::Schema { .. }));
        assert_eq!(marker(&connection), CURRENT_FORMAT);
    }

    /// Two connections to one file, so the second opener contends for SQLite's
    /// write lock exactly the way a second process would.
    fn file_pair() -> (tempfile::TempDir, Connection, Connection) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("zuno.db");
        let winner = open::open_at(&path).expect("open the winning connection");
        let loser = open::open_at(&path).expect("open the losing connection");
        (dir, winner, loser)
    }

    /// Run `apply` on a connection that first inventories the database, then
    /// blocks on `BEGIN IMMEDIATE` behind `held`, which commits after a pause.
    fn apply_behind(
        mut loser: Connection,
        held: Transaction<'_>,
    ) -> (Result<(), DbError>, Connection) {
        let loser_thread = std::thread::spawn(move || {
            let outcome = apply(&mut loser);
            (outcome, loser)
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        held.commit().expect("the winner commits");
        loser_thread.join().expect("the losing opener thread")
    }

    fn marker(connection: &Connection) -> u32 {
        connection
            .query_row(
                "SELECT format FROM zuno_schema WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read the format marker")
    }

    #[test]
    fn a_first_open_that_loses_the_write_lock_validates_the_winner_instead_of_refusing() {
        let (_dir, mut winner, loser) = file_pair();
        let held = winner
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("the winner reserves the writer");
        schema::up(&held).expect("the winner creates the schema");
        held.execute(FORMAT_SQL, [])
            .expect("the winner creates the marker table");
        held.execute(
            "INSERT INTO zuno_schema (singleton, format) VALUES (1, ?1)",
            params![CURRENT_FORMAT],
        )
        .expect("the winner writes the marker");

        let (outcome, loser) = apply_behind(loser, held);

        outcome.expect("the loser must validate the schema the winner created");
        assert_eq!(marker(&loser), CURRENT_FORMAT);
        let rows: i64 = loser
            .query_row("SELECT count(*) FROM zuno_schema", [], |row| row.get(0))
            .expect("count marker rows");
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_concurrent_upgrade_that_loses_the_write_lock_validates_instead_of_mismatching() {
        let (_dir, mut winner, loser) = file_pair();
        apply(&mut winner).expect("create the current schema");
        remove_memory_policy_schema(&winner);
        winner
            .execute(
                "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1",
                params![MEMORY_POLICY_UPGRADE_FROM],
            )
            .expect("rewind the marker to format 8");

        let held = winner
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("the winner reserves the writer");
        schema::up_memory_policy(&held).expect("the winner adds session memory policy");
        held.execute(
            "UPDATE zuno_schema SET format = ?1 WHERE singleton = 1 AND format = ?2",
            params![CURRENT_FORMAT, MEMORY_POLICY_UPGRADE_FROM],
        )
        .expect("the winner advances the marker");

        let (outcome, loser) = apply_behind(loser, held);

        outcome.expect("the loser must accept the upgrade the winner committed");
        assert_eq!(marker(&loser), CURRENT_FORMAT);
        assert!(
            table_names(&loser)
                .expect("inventory")
                .iter()
                .any(|table| table == "session_memory_policy")
        );
    }

    #[test]
    fn create_current_never_writes_over_a_non_empty_database() {
        let mut connection = memory();
        connection
            .execute_batch("CREATE TABLE something_else (id text PRIMARY KEY)")
            .expect("create unrelated table");

        assert_eq!(
            create_current(&mut connection).expect("hands the decision back"),
            Dispatch::Moved { observed: None }
        );
        assert_eq!(
            table_names(&connection).expect("inventory"),
            ["something_else"]
        );

        // Through the entry point the same file is a foreign, unmarked layout:
        // the redispatch classifies it instead of looping or creating over it.
        let error = apply(&mut connection).expect_err("foreign layout must be refused");
        assert!(
            matches!(
                error,
                DbError::SchemaMismatch {
                    expected: CURRENT_FORMAT,
                    observed: None
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            table_names(&connection).expect("inventory"),
            ["something_else"]
        );
    }

    #[test]
    fn dispatch_is_bounded_and_exhaustion_is_a_conflict_not_a_mismatch() {
        let mut connection = memory();
        let mut attempts = 0;
        let error = apply_with(&mut connection, |_| {
            attempts += 1;
            Ok(Dispatch::Moved {
                observed: Some(CURRENT_FORMAT),
            })
        })
        .expect_err("a format that keeps moving must not loop forever");
        assert_eq!(attempts, MAX_DISPATCH_ATTEMPTS);
        assert!(
            matches!(
                error,
                DbError::Conflict { ref table, ref id, .. } if table == FORMAT_TABLE && id == "1"
            ),
            "{error:?}"
        );

        let mut attempts = 0;
        apply_with(&mut connection, |_| {
            attempts += 1;
            Ok(if attempts == 1 {
                Dispatch::Moved { observed: None }
            } else {
                Dispatch::Settled
            })
        })
        .expect("one lost race followed by a settled read succeeds");
        assert_eq!(attempts, 2);
    }
}
