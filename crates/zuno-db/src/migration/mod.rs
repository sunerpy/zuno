//! Atomic current-schema creation and monotonic Zuno migrations.
//!
//! # Recognized database states
//!
//! A database arrives in exactly one of three states:
//!
//! * **has `session` and `migration`** — a Zuno database. Run only migrations
//!   its journal does not already record ([`apply_only`]).
//! * **empty** — a fresh install. Create the current schema in one statement batch
//!   and pre-seed all 39 journal ids, so no migration ever replays.
//! * **anything else** — unsupported. Fail without manufacturing a journal or
//!   guessing how another schema should be interpreted.
//!
//! Zuno is unreleased and does not migrate pre-Zuno or cross-product database
//! formats. `create_current` cannot be reached by any database that has tables,
//! and it re-checks that inside its own write transaction.

mod steps;

use crate::{open, schema};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_error::DbError;

use steps::MIGRATIONS;

/// The migration ids loaded by `migration.gen.ts`, in its generated order.
///
/// Derived from [`steps::MIGRATIONS`] at compile time rather than written out
/// twice: a hand-maintained copy could disagree with the SQL that actually runs,
/// and a journal that names a migration nobody executed is worse than no journal.
pub const MIGRATION_IDS: [&str; MIGRATIONS.len()] = {
    let mut ids = [""; MIGRATIONS.len()];
    let mut index = 0;
    while index < MIGRATIONS.len() {
        ids[index] = MIGRATIONS[index].id;
        index += 1;
    }
    ids
};

/// Numeric version attached to failures from this generated migration set.
pub const CURRENT_VERSION: u32 = MIGRATION_IDS.len() as u32;

/// What [`apply_only`] executed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    /// Ids whose SQL this call ran, in [`MIGRATION_IDS`] order.
    pub executed: Vec<String>,
}

const JOURNAL_SQL: &str =
    "CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)";

/// Bring a recognized Zuno database to the current schema.
///
/// # Errors
///
/// [`DbError::Migration`] for an unsupported existing database, a migration
/// SQLite rejects, time conversion failures, or SQLite DDL/DML failures.
/// [`DbError::Busy`] if another writer holds the database lock.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    let tables = table_names(connection)?;
    if tables.iter().any(|table| table == "session") {
        if !tables.iter().any(|table| table == "migration") {
            return Err(unsupported_existing_database());
        }
        return apply_only(connection).map(|_| ());
    }
    if !tables.is_empty() {
        return Err(failure(std::io::Error::other(
            "database is not empty and has no session table",
        )));
    }
    create_current(connection)
}

/// Run every migration a Zuno database has not recorded.
///
/// The journal must already exist. Missing journals are unsupported formats,
/// rather than an invitation to infer which historical migrations ran. Recorded
/// migrations are skipped, and a journal newer than this migration set is refused
/// before any SQL runs.
///
/// Each migration commits in its own transaction. A chain that fails halfway
/// leaves the completed prefix recorded, and the next launch resumes.
///
/// # Errors
///
/// [`DbError::Migration`] if the journal is absent or cannot be read, or if a
/// migration's SQL fails. [`DbError::MigrationTooNew`] if the journal contains
/// an id above this binary's known ceiling. [`DbError::Busy`] if another writer
/// holds the lock.
pub fn apply_only(connection: &mut Connection) -> Result<Applied, DbError> {
    if !has_table(connection, "migration")? {
        return Err(unsupported_existing_database());
    }
    let mut applied = Applied::default();
    let completed = journal_ids(connection)?;
    refuse_future_migrations(&completed)?;

    for migration in &MIGRATIONS {
        if completed.contains(migration.id) {
            continue;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_error)?;
        migration.run(&transaction)?;
        record(&transaction, migration.id)?;
        transaction.commit().map_err(map_error)?;
        applied.executed.push(migration.id.to_owned());
    }
    Ok(applied)
}

fn refuse_future_migrations(completed: &HashSet<String>) -> Result<(), DbError> {
    let ceiling = MIGRATION_IDS
        .iter()
        .copied()
        .max()
        .expect("the production migration set is not empty");
    let observed = completed
        .iter()
        .map(String::as_str)
        .filter(|id| *id > ceiling)
        .max();
    if let Some(observed) = observed {
        return Err(DbError::MigrationTooNew {
            ceiling: ceiling.to_owned(),
            observed: observed.to_owned(),
        });
    }
    Ok(())
}

/// Create the current schema and pre-seed the journal, atomically.
///
/// The emptiness check is repeated here, inside the `IMMEDIATE` transaction, and
/// that is not belt-and-braces. [`apply`]'s check runs before any write lock
/// exists, so another process can create the schema in the gap; and this function
/// is reachable from anywhere in the module. `schema::up` over a live `session`
/// table would recreate tables on top of someone's history. Refusing here makes
/// that unreachable no matter how the function is called or who else is writing.
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
    create_journal(&transaction)?;
    seed_journal(&transaction)?;
    transaction.commit().map_err(map_error)
}

fn create_journal(transaction: &Transaction<'_>) -> Result<(), DbError> {
    transaction.execute(JOURNAL_SQL, []).map_err(map_error)?;
    Ok(())
}

fn seed_journal(transaction: &Transaction<'_>) -> Result<(), DbError> {
    let time_completed = unix_milliseconds()?;
    let mut statement = transaction
        .prepare("INSERT INTO migration (id, time_completed) VALUES (?1, ?2)")
        .map_err(map_error)?;
    for id in MIGRATION_IDS {
        statement
            .execute(params![id, time_completed])
            .map_err(map_error)?;
    }
    Ok(())
}

fn record(transaction: &Transaction<'_>, id: &str) -> Result<(), DbError> {
    let time_completed = unix_milliseconds()?;
    transaction
        .execute(
            "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
            params![id, time_completed],
        )
        .map_err(map_error)?;
    Ok(())
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
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
}

fn has_table(connection: &Connection, name: &str) -> Result<bool, DbError> {
    connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(map_error)
}

fn journal_ids(connection: &Connection) -> Result<HashSet<String>, DbError> {
    let mut statement = connection
        .prepare("SELECT id FROM migration")
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(map_error)
}

fn unix_milliseconds() -> Result<i64, DbError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(failure)?;
    i64::try_from(elapsed.as_millis()).map_err(failure)
}

pub(crate) fn map_error(error: rusqlite::Error) -> DbError {
    if open::is_busy(&error) {
        return open::map_error(error);
    }
    failure(error)
}

fn failure(source: impl std::error::Error + Send + Sync + 'static) -> DbError {
    DbError::Migration {
        version: CURRENT_VERSION,
        source: Box::new(source),
    }
}

fn unsupported_existing_database() -> DbError {
    failure(std::io::Error::other(
        "database uses an unsupported pre-release format: a Zuno session database must contain the Zuno migration journal",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_ids_are_derived_from_the_statements_that_actually_run() {
        assert_eq!(MIGRATION_IDS.len(), 39);
        assert_eq!(CURRENT_VERSION, 39);
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(MIGRATION_IDS[index], migration.id);
        }
        assert_eq!(MIGRATION_IDS[0], "20260127222353_familiar_lady_ursula");
        assert_eq!(MIGRATION_IDS[38], "20260821160000_agent_job");
    }

    #[test]
    fn migration_ids_are_unique() {
        let unique: HashSet<&str> = MIGRATION_IDS.iter().copied().collect();
        assert_eq!(unique.len(), MIGRATION_IDS.len());
    }

    fn memory() -> Connection {
        open::open(&zuno_paths::DbLocation::Memory).expect("open memory database")
    }

    #[test]
    fn create_current_refuses_a_database_that_already_holds_a_session_table() {
        let mut connection = memory();
        connection
            .execute_batch(
                "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
                 INSERT INTO session VALUES ('session-1', 'someone real history');",
            )
            .expect("stand in for a live database");

        let error = create_current(&mut connection).expect_err("must refuse");
        let cause = format!("{:?}", std::error::Error::source(&error));
        assert!(
            cause.contains("refusing to create the current schema"),
            "{cause}"
        );

        let title: String = connection
            .query_row("SELECT title FROM session", [], |row| row.get(0))
            .expect("the session survived");
        assert_eq!(title, "someone real history");
        let tables = query_table_names(&connection).expect("inventory");
        assert_eq!(tables, ["session"], "no schema was created over the data");
    }

    #[test]
    fn create_current_refuses_any_non_empty_database_even_without_a_session_table() {
        let mut connection = memory();
        connection
            .execute_batch("CREATE TABLE something_else (id text PRIMARY KEY)")
            .expect("create an unrelated table");

        let error = create_current(&mut connection).expect_err("must refuse");
        let cause = format!("{:?}", std::error::Error::source(&error));
        assert!(cause.contains("something_else"), "{cause}");
        assert_eq!(
            query_table_names(&connection).expect("inventory"),
            ["something_else"]
        );
    }

    #[test]
    fn apply_rejects_a_session_database_without_the_zuno_journal_without_mutating_it() {
        let mut connection = memory();
        connection
            .execute_batch(
                "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
                 INSERT INTO session VALUES ('session-1', 'kept');",
            )
            .expect("stand in for a live database");

        let error = apply(&mut connection).expect_err("the format is unsupported");
        let cause = format!("{:?}", std::error::Error::source(&error));
        assert!(
            cause.contains("unsupported pre-release format"),
            "the error must explain the hard format cut: {cause}"
        );
        let title: String = connection
            .query_row("SELECT title FROM session", [], |row| row.get(0))
            .expect("the session survived");
        assert_eq!(title, "kept");
        assert!(
            !has_table(&connection, "migration").expect("inspect tables"),
            "rejection must not manufacture a journal"
        );
    }

    #[test]
    fn apply_only_also_rejects_a_missing_journal_without_mutation() {
        let mut connection = memory();
        connection
            .execute_batch("CREATE TABLE session (id text PRIMARY KEY)")
            .expect("stand in for an unsupported database");

        let error = apply_only(&mut connection).expect_err("the journal is required");
        let cause = format!("{:?}", std::error::Error::source(&error));
        assert!(cause.contains("unsupported pre-release format"), "{cause}");
        assert!(!has_table(&connection, "migration").expect("inspect tables"));
    }
}
