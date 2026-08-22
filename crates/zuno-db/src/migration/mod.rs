//! Atomic current-schema creation and pre-release format validation.
//!
//! Zuno is unreleased, so this module deliberately has no incremental migration
//! chain. An empty database receives the current schema in one transaction. A
//! non-empty database must carry exactly the current format marker; every older,
//! newer, or unmarked layout is rejected without mutation and can be rebuilt.

use crate::{open, schema};
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use zuno_error::DbError;

/// Current unreleased database format.
///
/// Bump this whenever [`crate::schema`] changes incompatibly. There is no upgrade
/// path between values before Zuno's first release.
pub const CURRENT_FORMAT: u32 = 2;

const FORMAT_TABLE: &str = "zuno_schema";
const FORMAT_SQL: &str = "
CREATE TABLE zuno_schema (
  singleton integer PRIMARY KEY CHECK (singleton = 1),
  format integer NOT NULL
)";

/// Ensure that `connection` uses the current all-at-once schema.
///
/// Empty databases are initialized. Existing databases are only validated; this
/// function never alters or backfills them.
///
/// # Errors
///
/// [`DbError::SchemaMismatch`] for another pre-release format,
/// [`DbError::Schema`] for invalid DDL or marker storage, and [`DbError::Busy`]
/// while another writer owns SQLite's write lock.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    let tables = table_names(connection)?;
    if tables.is_empty() {
        return create_current(connection);
    }
    validate_current(connection, &tables)
}

fn validate_current(connection: &Connection, tables: &[String]) -> Result<(), DbError> {
    let observed = observed_format(connection, tables)?;
    if observed != Some(CURRENT_FORMAT) {
        return Err(DbError::SchemaMismatch {
            expected: CURRENT_FORMAT,
            observed,
        });
    }
    if !tables.iter().any(|table| table == "session") {
        return Err(failure(std::io::Error::other(
            "current schema marker exists without the required session table",
        )));
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
