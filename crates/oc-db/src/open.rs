//! Opening a SQLite connection the way the TypeScript `opencode` binary does.
//!
//! The pragma sequence is a verbatim port of `database.ts:27-32`, in order, and
//! the connection target comes from [`oc_paths::db_path`] — this module never
//! re-derives a path or re-parses `OPENCODE_DB`.

use oc_error::DbError;
use oc_paths::DbLocation;
use rusqlite::{Connection, ErrorCode};
use std::path::{Path, PathBuf};

/// The pragma sequence from `database.ts:27-32`, in the oracle's order.
///
/// `wal_checkpoint(PASSIVE)` is part of the sequence, not an afterthought: the
/// oracle runs it on every connection it opens, and a passive checkpoint never
/// blocks a reader or a writer, so issuing it per connection is both faithful
/// and free.
pub const PRAGMA_SEQUENCE: &str = "\
PRAGMA journal_mode = WAL;\n\
PRAGMA synchronous = NORMAL;\n\
PRAGMA busy_timeout = 5000;\n\
PRAGMA cache_size = -64000;\n\
PRAGMA foreign_keys = ON;\n\
PRAGMA wal_checkpoint(PASSIVE);\n";

/// What `PRAGMA journal_mode` reports on a file database once WAL is set.
pub const JOURNAL_MODE_WAL: &str = "wal";

/// What `PRAGMA journal_mode` reports on an in-memory database.
///
/// SQLite does not support WAL for a `:memory:` database and does not fail the
/// request either — it keeps `memory` journalling and returns that instead. A
/// caller asserting `wal` unconditionally would therefore be asserting
/// something SQLite never promised.
pub const JOURNAL_MODE_MEMORY: &str = "memory";

/// What `PRAGMA synchronous` reports for `NORMAL`.
pub const SYNCHRONOUS_NORMAL: i64 = 1;

/// What `PRAGMA busy_timeout` reports, in milliseconds.
pub const BUSY_TIMEOUT_MS: i64 = 5000;

/// What `PRAGMA cache_size` reports; negative means kibibytes, not pages.
pub const CACHE_SIZE_KIB: i64 = -64_000;

/// What `PRAGMA foreign_keys` reports for `ON`.
pub const FOREIGN_KEYS_ON: i64 = 1;

/// The suffixes SQLite adds beside a WAL database file.
///
/// Recorded as a constant because pruning (todo 82) and vacuuming (todo 84)
/// both have to move or delete the whole set: deleting `opencode.db` while
/// leaving `opencode.db-wal` behind loses committed transactions.
pub const WAL_SIDECAR_SUFFIXES: [&str; 2] = ["-wal", "-shm"];

/// Open the database the running binary would use.
///
/// # Errors
///
/// [`DbError::Open`] when the file cannot be created or opened, or when a
/// pragma did not take effect.
pub fn open_default() -> Result<Connection, DbError> {
    open(&oc_paths::db_path())
}

/// Open `location`, applying the oracle's pragmas.
///
/// [`DbLocation::Memory`] yields a *private* in-memory database, matching what
/// the oracle gets from `OPENCODE_DB=:memory:`. Use
/// [`open_shared_memory`] when more than one connection has to see the same
/// in-memory data.
///
/// # Errors
///
/// [`DbError::Open`] when the file cannot be created or opened, or when a
/// pragma did not take effect.
pub fn open(location: &DbLocation) -> Result<Connection, DbError> {
    match location {
        DbLocation::Memory => open_target(oc_paths::MEMORY_SENTINEL, location),
        DbLocation::File(path) => open_at(path),
    }
}

/// Open a file database, creating its parent directory if it is missing.
///
/// # Errors
///
/// [`DbError::Open`] when the parent directory cannot be created, the file
/// cannot be opened, or a pragma did not take effect.
pub fn open_at(path: &Path) -> Result<Connection, DbError> {
    ensure_parent(path)?;
    let location = DbLocation::File(path.to_path_buf());
    open_target(&path.to_string_lossy(), &location)
}

/// Create the directory a database file will live in.
///
/// [`oc_paths`] deliberately keeps every path getter pure, and the oracle relies
/// on `global.ts` having already created `data()` at import. Doing it here is a
/// documented superset: it also covers an `OPENCODE_DB` pointing at a nested
/// directory, which the oracle would fail to open.
///
/// # Errors
///
/// [`DbError::Open`] naming the database file when the directory cannot be
/// created.
pub fn ensure_parent(path: &Path) -> Result<(), DbError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| DbError::Open {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    }
    Ok(())
}

/// Open a *named*, shared in-memory database.
///
/// SQLite gives every plain `:memory:` connection its own private database, so a
/// pool of them would silently hand out unrelated databases. The URI form with
/// `cache=shared` is the only way several connections can share one in-memory
/// database, and the database lives exactly as long as the first connection to
/// it — which is why [`crate::Pool`] holds an anchor connection open.
///
/// # Errors
///
/// [`DbError::Open`] when the database cannot be opened or a pragma did not take
/// effect.
pub fn open_shared_memory(name: &str) -> Result<Connection, DbError> {
    let uri = shared_memory_uri(name);
    open_target(&uri, &DbLocation::Memory)
}

/// The URI that names a shared in-memory database.
#[must_use]
pub fn shared_memory_uri(name: &str) -> String {
    format!("file:{name}?mode=memory&cache=shared")
}

/// Open `target` verbatim — a path or a SQLite URI — and apply the pragmas.
///
/// `location` decides only how failures are reported and whether `journal_mode`
/// is expected to be `wal` or `memory`; it is never re-derived into a path.
///
/// # Errors
///
/// [`DbError::Open`] when the database cannot be opened or a pragma did not take
/// effect.
pub fn open_target(target: &str, location: &DbLocation) -> Result<Connection, DbError> {
    let connection = Connection::open(target).map_err(|source| DbError::Open {
        path: reported_path(location),
        source: Box::new(source),
    })?;
    apply_pragmas(&connection, location)?;
    Ok(connection)
}

/// Apply [`PRAGMA_SEQUENCE`] and prove each setting actually took effect.
///
/// The read-back half is the point. Every pragma except `journal_mode` is
/// per-connection state that defaults to something else — `foreign_keys`
/// defaults to *off* — so a connection handed out without them looks healthy and
/// silently declines to enforce the cascades the session schema declares. A
/// pragma that does not stick is therefore an open failure, not a warning.
///
/// # Errors
///
/// [`DbError::Open`] naming the database when a pragma reports a value other
/// than the one the oracle asked for.
pub fn apply_pragmas(connection: &Connection, location: &DbLocation) -> Result<(), DbError> {
    connection
        .execute_batch(PRAGMA_SEQUENCE)
        .map_err(|source| DbError::Open {
            path: reported_path(location),
            source: Box::new(source),
        })?;
    verify_pragmas(connection, location)
}

/// Read every pragma back and compare it against what the oracle asked for.
///
/// # Errors
///
/// [`DbError::Open`] naming the pragma, the expected value and the observed one.
pub fn verify_pragmas(connection: &Connection, location: &DbLocation) -> Result<(), DbError> {
    let expected_journal_mode = if location.is_memory() {
        JOURNAL_MODE_MEMORY
    } else {
        JOURNAL_MODE_WAL
    };
    let journal_mode = query_text(connection, "journal_mode", location)?;
    if !journal_mode.eq_ignore_ascii_case(expected_journal_mode) {
        return Err(pragma_mismatch(
            location,
            "journal_mode",
            expected_journal_mode,
            &journal_mode,
        ));
    }
    for (name, expected) in [
        ("synchronous", SYNCHRONOUS_NORMAL),
        ("busy_timeout", BUSY_TIMEOUT_MS),
        ("cache_size", CACHE_SIZE_KIB),
        ("foreign_keys", FOREIGN_KEYS_ON),
    ] {
        let observed = query_int(connection, name, location)?;
        if observed != expected {
            return Err(pragma_mismatch(
                location,
                name,
                &expected.to_string(),
                &observed.to_string(),
            ));
        }
    }
    Ok(())
}

/// Read one pragma as text.
///
/// # Errors
///
/// [`DbError::Open`] when the pragma cannot be read.
pub fn query_text(
    connection: &Connection,
    name: &str,
    location: &DbLocation,
) -> Result<String, DbError> {
    connection
        .pragma_query_value(None, name, |row| row.get::<_, String>(0))
        .map_err(|source| DbError::Open {
            path: reported_path(location),
            source: Box::new(source),
        })
}

/// Read one pragma as an integer.
///
/// # Errors
///
/// [`DbError::Open`] when the pragma cannot be read.
pub fn query_int(
    connection: &Connection,
    name: &str,
    location: &DbLocation,
) -> Result<i64, DbError> {
    connection
        .pragma_query_value(None, name, |row| row.get::<_, i64>(0))
        .map_err(|source| DbError::Open {
            path: reported_path(location),
            source: Box::new(source),
        })
}

/// Classify a `rusqlite` failure into the workspace taxonomy.
///
/// The one distinction that matters is `SQLITE_BUSY` / `SQLITE_LOCKED`: those
/// clear on their own, and [`DbError::Busy`] is the only variant a caller may
/// retry. Everything else is reported as a statement failure with the original
/// error preserved as the cause.
#[must_use]
pub fn map_error(error: rusqlite::Error) -> DbError {
    if is_busy(&error) {
        return DbError::Busy { retry_after: None };
    }
    DbError::Query {
        source: Box::new(error),
    }
}

/// Whether a failure is the transient "another writer holds the lock".
#[must_use]
pub fn is_busy(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(inner, _) => {
            matches!(
                inner.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            )
        }
        _ => false,
    }
}

/// Whether a failure is a constraint violation, foreign keys included.
#[must_use]
pub fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(inner, _) => {
            matches!(inner.code, ErrorCode::ConstraintViolation)
        }
        _ => false,
    }
}

/// The `-wal` and `-shm` files SQLite keeps beside `path`.
///
/// Both exist only while a connection is open or after a crash; a clean close
/// removes them. They are still part of the database: any code that moves,
/// copies or deletes the main file has to account for them.
#[must_use]
pub fn sidecar_files(path: &Path) -> Vec<PathBuf> {
    let base = path.as_os_str();
    WAL_SIDECAR_SUFFIXES
        .iter()
        .map(|suffix| {
            let mut name = base.to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        })
        .collect()
}

fn pragma_mismatch(location: &DbLocation, name: &str, expected: &str, observed: &str) -> DbError {
    DbError::Open {
        path: reported_path(location),
        source: Box::new(std::io::Error::other(format!(
            "PRAGMA {name} is {observed}, expected {expected}"
        ))),
    }
}

fn reported_path(location: &DbLocation) -> PathBuf {
    PathBuf::from(location.as_oracle_string().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragma_sequence_matches_the_oracle_line_for_line() {
        let statements: Vec<&str> = PRAGMA_SEQUENCE.lines().collect();
        assert_eq!(
            statements,
            [
                "PRAGMA journal_mode = WAL;",
                "PRAGMA synchronous = NORMAL;",
                "PRAGMA busy_timeout = 5000;",
                "PRAGMA cache_size = -64000;",
                "PRAGMA foreign_keys = ON;",
                "PRAGMA wal_checkpoint(PASSIVE);",
            ]
        );
    }

    #[test]
    fn sidecar_files_append_rather_than_replace_the_extension() {
        let files = sidecar_files(Path::new("/data/opencode.db"));
        assert_eq!(
            files,
            [
                PathBuf::from("/data/opencode.db-wal"),
                PathBuf::from("/data/opencode.db-shm"),
            ]
        );
    }

    #[test]
    fn shared_memory_uri_names_a_shared_cache_database() {
        assert_eq!(
            shared_memory_uri("oc-1"),
            "file:oc-1?mode=memory&cache=shared"
        );
    }

    #[test]
    fn busy_is_classified_as_retryable_and_nothing_else_is() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_owned()),
        );
        assert!(is_busy(&busy));
        assert!(map_error(busy).is_retryable());

        let constraint = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some("FOREIGN KEY constraint failed".to_owned()),
        );
        assert!(!is_busy(&constraint));
        assert!(is_constraint_violation(&constraint));
        assert!(!map_error(constraint).is_retryable());
    }
}
