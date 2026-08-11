//! Atomic current-schema creation, legacy migration, and TypeScript
//! migration-journal parity.
//!
//! # Three databases, three paths
//!
//! [`apply`] is a port of `packages/core/src/database/migration.ts:18-79`, and the
//! shape of that function is the whole design. A database arrives in exactly one
//! of three states and each needs different handling:
//!
//! * **has `session`** — someone's real history. Never recreate it; run only the
//!   migrations its journal does not already record ([`apply_only`]).
//! * **empty** — a fresh install. Create the current schema in one statement batch
//!   and pre-seed all 38 journal ids, so no migration ever replays.
//! * **non-empty without `session`** — unrecognised. Fail; touching it would be
//!   guesswork over someone's data.
//!
//! The order those are tested in is load-bearing, so it matches upstream's:
//! `session` first, then non-empty, then empty. `create_current` cannot be reached
//! by any database that has tables, and it re-checks that *inside* its own write
//! transaction rather than trusting the caller — see there for why.

mod steps;

use crate::{open, schema};
use oc_error::DbError;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// The journal Drizzle wrote before upstream replaced it with `migration`.
///
/// Only its `name` column is read, and only once — see [`seed_from_drizzle`].
pub const DRIZZLE_JOURNAL_TABLE: &str = "__drizzle_migrations";

/// What [`apply_only`] did, so a caller can tell seeding from execution.
///
/// The distinction is the whole safety property of a legacy migration: an id that
/// was *seeded* is a claim the old journal made about SQL that already ran, and an
/// id that was *executed* is SQL this call ran itself. "Nothing was replayed" is
/// only checkable if the two are reported separately — inferring it from a
/// successful return would be inferring it from the fact that replayed DDL happens
/// to error, which is a coincidence, not a guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    /// Ids copied out of Drizzle's journal, in the order that journal held them.
    pub seeded: Vec<String>,
    /// Ids whose SQL this call ran, in [`MIGRATION_IDS`] order.
    pub executed: Vec<String>,
}

const JOURNAL_SQL: &str =
    "CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)";

const JOURNAL_IF_ABSENT_SQL: &str = "CREATE TABLE IF NOT EXISTS \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)";

/// Bring a database to the current schema, whatever state it arrives in.
///
/// # Errors
///
/// [`DbError::Migration`] for a non-empty database with no `session` table, for a
/// migration SQLite rejects, for time conversion failures, or for SQLite DDL/DML
/// failures. [`DbError::Busy`] if another writer holds the database lock.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    let tables = table_names(connection)?;
    if tables.iter().any(|table| table == "session") {
        return apply_only(connection).map(|_| ());
    }
    if !tables.is_empty() {
        return Err(failure(std::io::Error::other(
            "database is not empty and has no session table",
        )));
    }
    create_current(connection)
}

/// Run every migration an existing database has not recorded.
///
/// A port of `migration.ts:43-79`. Three things it does that a journal check
/// alone does not:
///
/// 1. **Creates the journal if it is absent.** A real install predating the
///    `migration` table has a `session` table and no journal at all, so reading
///    the journal first is how this function used to fail on the very databases it
///    exists to serve.
/// 2. **Seeds the journal from Drizzle's, once.** See [`seed_from_drizzle`].
/// 3. **Skips anything already recorded**, so old SQL never replays over data.
/// 4. **Refuses a journal newer than this migration set**, before running SQL.
///
/// Each migration commits in its own transaction, as upstream does: a chain that
/// fails halfway leaves the completed prefix recorded, and the next launch resumes
/// rather than starting over.
///
/// # Errors
///
/// [`DbError::Migration`] if the journal cannot be created or read, or if a
/// migration's SQL fails. [`DbError::MigrationTooNew`] if the journal contains an
/// id above this binary's known ceiling. [`DbError::Busy`] if another writer holds
/// the lock.
pub fn apply_only(connection: &mut Connection) -> Result<Applied, DbError> {
    connection
        .execute_batch(JOURNAL_IF_ABSENT_SQL)
        .map_err(map_error)?;

    let mut applied = Applied::default();
    let mut completed = journal_ids(connection)?;
    if completed.is_empty() && has_table(connection, DRIZZLE_JOURNAL_TABLE)? {
        seed_from_drizzle(connection)?;
        applied.seeded = drizzle_names(connection)?;
        completed = journal_ids(connection)?;
    }
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

/// Copy Drizzle's completed migration names into the `migration` journal.
///
/// Upstream's reason, verbatim from `migration.ts:52-54`: *"Existing installs used
/// Drizzle's migration journal. Seed the new journal once so TypeScript migrations
/// don't replay old SQL."*
///
/// Without this step every migration looks outstanding on a legacy install, and
/// migration 1 would try to `CREATE TABLE session` over 2,345 live sessions. The
/// `INSERT OR IGNORE` and the empty-journal precondition together make it a
/// once-only operation, and the names are taken as-is: this function's job is to
/// record what the old journal claims, not to audit it against
/// [`MIGRATION_IDS`].
fn seed_from_drizzle(connection: &Connection) -> Result<(), DbError> {
    let time_completed = unix_milliseconds()?;
    connection
        .execute(
            "INSERT OR IGNORE INTO migration (id, time_completed) \
             SELECT name, ?1 FROM __drizzle_migrations WHERE name IS NOT NULL",
            params![time_completed],
        )
        .map_err(map_error)?;
    Ok(())
}

fn drizzle_names(connection: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM __drizzle_migrations \
             WHERE name IS NOT NULL ORDER BY rowid",
        )
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_ids_are_derived_from_the_statements_that_actually_run() {
        assert_eq!(MIGRATION_IDS.len(), 38);
        assert_eq!(CURRENT_VERSION, 38);
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(MIGRATION_IDS[index], migration.id);
        }
        assert_eq!(MIGRATION_IDS[0], "20260127222353_familiar_lady_ursula");
        assert_eq!(MIGRATION_IDS[37], "20260622202450_simplify_session_input");
    }

    #[test]
    fn migration_ids_are_unique() {
        let unique: HashSet<&str> = MIGRATION_IDS.iter().copied().collect();
        assert_eq!(unique.len(), MIGRATION_IDS.len());
    }

    fn memory() -> Connection {
        open::open(&oc_paths::DbLocation::Memory).expect("open memory database")
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
    fn apply_tests_for_a_session_table_before_it_tests_for_emptiness() {
        let mut connection = memory();
        connection
            .execute_batch(
                "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
                 INSERT INTO session VALUES ('session-1', 'kept');",
            )
            .expect("stand in for a live database");

        let error = apply(&mut connection).expect_err("the chain cannot migrate this shape");
        let cause = format!("{:?}", std::error::Error::source(&error));
        assert!(
            !cause.contains("refusing to create the current schema"),
            "apply must route a session database to the migration path, not to creation: {cause}"
        );
        let title: String = connection
            .query_row("SELECT title FROM session", [], |row| row.get(0))
            .expect("the session survived");
        assert_eq!(title, "kept");
    }
}
