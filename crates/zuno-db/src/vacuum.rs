//! Explicit `VACUUM`, integrity checking, and size statistics.
//!
//! # Why any of this exists
//!
//! Nothing upstream ever reclaims space. `packages/opencode/src/cli/cmd/db.ts:8-62`
//! is the whole `db` surface — a query runner and a path printer — and a search of
//! the TypeScript tree finds **no `VACUUM` call at all**. Deleting a session there
//! therefore returns its pages to SQLite's freelist and nothing to the filesystem,
//! so `opencode.db` only ever grows. Todo 82's prune has exactly the same
//! property, by design.
//!
//! # Why it is a separate command and never a side effect
//!
//! `VACUUM` rewrites the entire database file. That has three consequences a
//! prune must not inherit:
//!
//! * it needs free space of roughly the database's own size, so on a full disk it
//!   fails where a delete would have succeeded;
//! * it cannot run inside a transaction — SQLite rejects it outright — while
//!   todo 82's delete is one `IMMEDIATE` transaction by design, so that its
//!   ten-table deletion either lands whole or not at all;
//! * it takes time proportional to the surviving data, not to the rows removed,
//!   which turns a bounded delete into an unbounded rewrite.
//!
//! So [`vacuum`] takes `&mut Connection` rather than `&Connection`. That is not a
//! stylistic choice: a live `rusqlite::Transaction` holds the connection's
//! mutable borrow for its whole lifetime, so **this function cannot be called
//! while a transaction is open on the same connection**. The prohibition is
//! enforced by the borrow checker instead of by a comment. `tests/vacuum.rs`
//! additionally scans this crate's sources so no other module can reach for
//! `VACUUM` behind a caller's back.
//!
//! # Why the WAL is checkpointed on both sides of the measurement
//!
//! [`crate::open::PRAGMA_SEQUENCE`] puts every connection in WAL mode, a verbatim
//! port of `packages/core/src/database/database.ts:22-33`. In WAL mode a delete's
//! pages land in `opencode.db-wal`, and the main file does not change size until a
//! checkpoint folds them in — so a naive "size before, size after" reads zero for
//! work that did happen, and reads a *negative* reclaim for work that only moved
//! bytes into the sidecar. [`vacuum`] therefore checkpoints with `TRUNCATE`
//! before it measures and again after the rewrite, and every size is re-`stat`ed
//! from the filesystem rather than cached, so the reported number is the change in
//! the database's real on-disk footprint.
//!
//! # The disk guard is injected
//!
//! Free space is a property of the host, not of the database, so it arrives
//! through [`DiskSpace`] — the same seam shape todo 81 used for
//! [`crate::retention::LivenessProbe`] and todo 82 for
//! [`crate::prune::RemoteUnshare`]. A test can therefore assert the refusal
//! without filling a disk.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;
use zuno_error::DbError;

use crate::open;

/// The single row `PRAGMA integrity_check` returns for a healthy database.
pub const INTEGRITY_OK: &str = "ok";

/// How many sessions [`stats`] reports by default.
pub const DEFAULT_LARGEST_SESSIONS: usize = 10;

/// The FTS5 table whose presence means a rewrite invalidated the search index.
///
/// `VACUUM` may renumber the implicit `message.rowid` values the external-content
/// indexes use as document ids, so [`crate::fts::rebuild`] has to run afterwards
/// — stated at `fts.rs:240-244`. Detected rather than acted on: rebuilding inside
/// [`vacuum`] would be exactly the kind of hidden side effect this module exists
/// to refuse. The indexes are opt-in and absent from `migration::apply`, so an
/// unconditional rebuild would also fail on the ordinary database.
const FTS_TABLE: &str = "message_fts";

/// The logical byte measure applied to `part`, spelled exactly as
/// [`crate::prune`] spells it.
///
/// Shared as a constant because the two commands are read together: a session
/// that `db stats` calls the largest is the one an operator then prunes, and two
/// different definitions of "bytes" would make the second number look like a bug
/// in the first. `tests/vacuum.rs` asserts the two agree on a real database.
const PART_BYTES: &str = "COALESCE(length(CAST(part.id AS BLOB)), 0) \
     + COALESCE(length(CAST(part.message_id AS BLOB)), 0) \
     + COALESCE(length(CAST(part.session_id AS BLOB)), 0) \
     + COALESCE(length(CAST(part.time_created AS BLOB)), 0) \
     + COALESCE(length(CAST(part.time_updated AS BLOB)), 0) \
     + COALESCE(length(CAST(part.data AS BLOB)), 0)";

/// The on-disk footprint of a database: the main file plus its WAL sidecars.
///
/// The sidecars are counted because they are part of the database, not a cache:
/// `-wal` holds committed transactions the main file has not absorbed yet, so a
/// figure that ignored it would under-report a freshly written database and
/// over-report the effect of a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct DatabaseSize {
    /// Length of the database file itself.
    pub main_bytes: u64,
    /// Length of `-wal`; zero when no connection is open or after a truncating
    /// checkpoint.
    pub wal_bytes: u64,
    /// Length of `-shm`, the WAL index shared-memory file.
    pub shm_bytes: u64,
}

impl DatabaseSize {
    /// Every byte the database occupies on disk.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.main_bytes
            .saturating_add(self.wal_bytes)
            .saturating_add(self.shm_bytes)
    }

    /// Bytes returned to the filesystem going from `self` to `after`.
    ///
    /// Saturating rather than signed: an operation that *grew* the footprint
    /// reclaimed nothing, and reporting a negative reclaim invites a caller to
    /// print `-4096 bytes reclaimed`.
    #[must_use]
    pub const fn reclaimed_since(&self, after: Self) -> u64 {
        self.total_bytes().saturating_sub(after.total_bytes())
    }
}

/// What the host could establish about free space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Availability {
    /// The filesystem reported this many bytes usable by this process.
    Known {
        /// Bytes available to an unprivileged writer.
        bytes: u64,
    },
    /// Free space could not be established, so the guard cannot be evaluated.
    Unknown {
        /// Why, in terms a user can act on.
        reason: String,
    },
}

impl Availability {
    /// The byte count, when there is one.
    #[must_use]
    pub const fn bytes(&self) -> Option<u64> {
        match self {
            Self::Known { bytes } => Some(*bytes),
            Self::Unknown { .. } => None,
        }
    }
}

/// Injectable free-space boundary.
///
/// The database cannot answer how much room its filesystem has left, and a test
/// must not have to fill a disk to prove the refusal fires. Implementations live
/// at the process edge; [`SystemDiskSpace`] is the real one.
pub trait DiskSpace {
    /// Bytes available on the filesystem that holds `path`.
    fn available_bytes(&self, path: &Path) -> Availability;
}

/// The real free-space query.
///
/// On unix this is one `statvfs` call through `rustix`, which adds **no package**
/// to `Cargo.lock` — it is already there for `tempfile` — keeps the syscall's
/// `unsafe` inside a dependency so this workspace's `unsafe_code = "forbid"`
/// still holds, and avoids parsing a localized `df` table out of a spawned
/// process. Elsewhere it reports [`Availability::Unknown`]; see [`vacuum`] for
/// what that means for the guard.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDiskSpace;

impl DiskSpace for SystemDiskSpace {
    fn available_bytes(&self, path: &Path) -> Availability {
        available_on_filesystem(path)
    }
}

/// A refusal is data: a caller must be able to tell "there is not enough room"
/// from "this connection has no file to rewrite" and from a SQLite failure.
#[derive(Debug)]
pub enum VacuumError {
    /// SQLite could not checkpoint, read a pragma, or complete the rewrite.
    Database(DbError),
    /// The connection is in-memory or temporary, so there is no file to compact.
    NotAFileDatabase,
    /// The filesystem has less room than the rewrite needs.
    InsufficientDiskSpace {
        /// Database that was not rewritten.
        path: PathBuf,
        /// Bytes the rewrite needs, which is the current size of the main file.
        required_bytes: u64,
        /// Bytes the filesystem reported available.
        available_bytes: u64,
    },
}

impl fmt::Display for VacuumError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::NotAFileDatabase => formatter.write_str(
                "this connection has no database file to compact; VACUUM only \
                 reclaims space from a file database",
            ),
            Self::InsufficientDiskSpace {
                path,
                required_bytes,
                available_bytes,
            } => write!(
                formatter,
                "refusing to VACUUM {}: rewriting the whole file needs {} ({required_bytes} bytes) \
                 free on its filesystem, but only {} ({available_bytes} bytes) is available; \
                 free at least {} ({} bytes) there, or point ZUNO_DB at a filesystem with \
                 more room",
                path.display(),
                format_bytes(*required_bytes),
                format_bytes(*available_bytes),
                format_bytes(required_bytes.saturating_sub(*available_bytes)),
                required_bytes.saturating_sub(*available_bytes),
            ),
        }
    }
}

impl std::error::Error for VacuumError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::NotAFileDatabase | Self::InsufficientDiskSpace { .. } => None,
        }
    }
}

impl From<DbError> for VacuumError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

/// What one rewrite cost and returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VacuumReport {
    /// The database that was rewritten.
    pub path: PathBuf,
    /// Footprint measured after a truncating checkpoint and before the rewrite.
    pub before: DatabaseSize,
    /// Footprint measured after the rewrite and a second truncating checkpoint.
    pub after: DatabaseSize,
    /// Bytes returned to the filesystem, never negative.
    pub reclaimed_bytes: u64,
    /// Freelist pages the rewrite absorbed; a large value before is the signal
    /// that a vacuum is worth running at all.
    pub freelist_pages_before: u64,
    /// Freelist pages left afterwards, normally zero.
    pub freelist_pages_after: u64,
    /// What the disk guard saw, recorded so a report can say the guard was
    /// evaluated rather than merely passed.
    pub available_bytes: Availability,
    /// Whether the opt-in FTS5 indexes are installed and therefore now stale.
    ///
    /// True means the caller must run [`crate::fts::rebuild`]; see [`FTS_TABLE`]
    /// for why this is reported instead of done here.
    pub fts_rebuild_required: bool,
}

/// Rewrite the database file, reclaiming freelist pages to the filesystem.
///
/// Runs, in order: a truncating WAL checkpoint, a footprint measurement, the disk
/// guard, `VACUUM`, a second truncating checkpoint, and a second measurement.
///
/// The guard refuses when the filesystem reports **less free space than the main
/// file's current size**, because the rewrite materializes a second copy before
/// replacing the original. It is a necessary rather than a sufficient condition:
/// SQLite may place its intermediate copy under `SQLITE_TMPDIR` on a different
/// filesystem, which this check cannot see. When free space cannot be established
/// at all — [`Availability::Unknown`], which is every non-unix host — the rewrite
/// proceeds and the report records why the guard was not evaluated. Refusing
/// instead would make the command unusable on those hosts to prevent a failure
/// SQLite already handles safely: an out-of-space `VACUUM` aborts and rolls back,
/// leaving the original database intact.
///
/// `&mut Connection` is load-bearing. It makes the call unrepresentable while a
/// transaction is open on the same connection, which is the one thing a prune
/// must never do.
///
/// # Errors
///
/// [`VacuumError::NotAFileDatabase`] for an in-memory or temporary connection,
/// [`VacuumError::InsufficientDiskSpace`] when the guard refuses, or
/// [`VacuumError::Database`] when a checkpoint, pragma, or the rewrite fails.
pub fn vacuum(
    connection: &mut Connection,
    disk: &dyn DiskSpace,
) -> Result<VacuumReport, VacuumError> {
    let path = database_path(connection).ok_or(VacuumError::NotAFileDatabase)?;

    checkpoint(connection)?;
    let before = database_size(&path);
    let freelist_pages_before = page_counter(connection, "freelist_count")?;

    let available_bytes = disk.available_bytes(&path);
    if let Availability::Known { bytes } = available_bytes
        && bytes < before.main_bytes
    {
        return Err(VacuumError::InsufficientDiskSpace {
            path,
            required_bytes: before.main_bytes,
            available_bytes: bytes,
        });
    }

    connection
        .execute_batch("VACUUM")
        .map_err(open::map_error)?;

    checkpoint(connection)?;
    let after = database_size(&path);
    let freelist_pages_after = page_counter(connection, "freelist_count")?;

    Ok(VacuumReport {
        path,
        before,
        after,
        reclaimed_bytes: before.reclaimed_since(after),
        freelist_pages_before,
        freelist_pages_after,
        available_bytes,
        fts_rebuild_required: has_table(connection, FTS_TABLE)?,
    })
}

/// One row of `PRAGMA foreign_key_check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForeignKeyViolation {
    /// Child table holding the dangling reference.
    pub table: String,
    /// `rowid` of the offending row; absent for a `WITHOUT ROWID` table.
    pub rowid: Option<i64>,
    /// Table the reference points at.
    pub parent: String,
    /// Which foreign key of `table` failed, in `PRAGMA foreign_key_list` order.
    pub foreign_key_index: i64,
}

/// Whether the file is internally consistent, and whether its references resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrityReport {
    /// Every row `PRAGMA integrity_check` returned; exactly `["ok"]` when healthy.
    pub integrity: Vec<String>,
    /// Dangling references found by `PRAGMA foreign_key_check`.
    pub foreign_key_violations: Vec<ForeignKeyViolation>,
}

impl IntegrityReport {
    /// Whether both checks passed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.integrity.as_slice(), [only] if only == INTEGRITY_OK)
            && self.foreign_key_violations.is_empty()
    }
}

/// Check the file's structure and its foreign-key closure.
///
/// `PRAGMA foreign_key_check` is run alongside `integrity_check` because a
/// structurally perfect file can still be wrong here in a way that matters: this
/// crate's whole point is that `PRAGMA foreign_keys` must be ON
/// (see the crate docs), and a delete performed on a connection that inherited it
/// *off* leaves orphans that `integrity_check` happily calls `ok`. That is exactly
/// the failure todo 82's explicit ten-table delete order and its global `part`
/// orphan sweep exist to prevent, so this is the check that proves they worked.
///
/// # Errors
///
/// [`DbError::Query`] when either pragma cannot be read.
pub fn integrity_check(connection: &Connection) -> Result<IntegrityReport, DbError> {
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(open::map_error)?;
    let integrity = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    drop(statement);

    let mut statement = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(open::map_error)?;
    let foreign_key_violations = statement
        .query_map([], |row| {
            Ok(ForeignKeyViolation {
                table: row.get(0)?,
                rowid: row.get(1)?,
                parent: row.get(2)?,
                foreign_key_index: row.get(3)?,
            })
        })
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;

    Ok(IntegrityReport {
        integrity,
        foreign_key_violations,
    })
}

/// Row count for one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableRows {
    /// Table name as `sqlite_master` spells it.
    pub table: String,
    /// `COUNT(*)`.
    pub rows: u64,
}

/// One session's `part` weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionBytes {
    /// `session.id`.
    pub session_id: String,
    /// `session.title`, so a report is readable without a second query.
    pub title: String,
    /// Rows in `part` attributed to this session.
    pub part_rows: u64,
    /// Logical payload bytes of those rows, measured the way
    /// [`crate::prune::preview`] measures them.
    pub part_bytes: u64,
}

/// Everything `db stats` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseStats {
    /// The database file, or `None` for an in-memory connection.
    pub path: Option<PathBuf>,
    /// On-disk footprint, including the WAL sidecars.
    pub size: DatabaseSize,
    /// `PRAGMA page_size`.
    pub page_size: u64,
    /// `PRAGMA page_count`.
    pub page_count: u64,
    /// `PRAGMA freelist_count`; pages a [`vacuum`] would return to the
    /// filesystem.
    pub freelist_pages: u64,
    /// One entry per user table, read from `sqlite_master` rather than from a
    /// hard-coded list, in name order.
    pub tables: Vec<TableRows>,
    /// Sum of every table's row count.
    pub total_rows: u64,
    /// Heaviest sessions by `part` bytes, descending, id-tiebroken.
    pub largest_sessions: Vec<SessionBytes>,
}

impl DatabaseStats {
    /// Find one table's count without callers depending on vector position.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableRows> {
        self.tables.iter().find(|entry| entry.table == name)
    }
}

/// Summarize the database without modifying it.
///
/// The table inventory comes from `sqlite_master` at runtime. A hard-coded list
/// would silently stop counting a table added by a later migration, and the count
/// is the number an operator uses to decide whether a prune is worth running.
///
/// # Errors
///
/// [`DbError::Query`] when a pragma, the table inventory, a row count, or the
/// session aggregate cannot be read.
pub fn stats(connection: &Connection, largest_sessions: usize) -> Result<DatabaseStats, DbError> {
    let path = database_path(connection);
    let size = path.as_deref().map(database_size).unwrap_or_default();
    let tables = table_rows(connection)?;
    let total_rows = tables.iter().map(|entry| entry.rows).sum();

    Ok(DatabaseStats {
        path,
        size,
        page_size: page_counter(connection, "page_size")?,
        page_count: page_counter(connection, "page_count")?,
        freelist_pages: page_counter(connection, "freelist_count")?,
        tables,
        total_rows,
        largest_sessions: heaviest_sessions(connection, largest_sessions)?,
    })
}

/// Re-`stat` the database file and both WAL sidecars.
///
/// Deliberately re-reads the filesystem on every call and never caches: the whole
/// point of the before/after pair in [`VacuumReport`] is that the two numbers were
/// observed at two different times.
#[must_use]
pub fn database_size(path: &Path) -> DatabaseSize {
    let sidecars = open::sidecar_files(path);
    DatabaseSize {
        main_bytes: file_len(path),
        wal_bytes: sidecars.first().map_or(0, |sidecar| file_len(sidecar)),
        shm_bytes: sidecars.get(1).map_or(0, |sidecar| file_len(sidecar)),
    }
}

/// The file this connection is attached to, or `None` when there is not one.
///
/// SQLite reports an empty filename for an in-memory or temporary database,
/// including the `file:name?mode=memory&cache=shared` form
/// [`open::open_shared_memory`] uses, so both collapse to `None` here.
#[must_use]
pub fn database_path(connection: &Connection) -> Option<PathBuf> {
    match connection.path() {
        Some(path) if !path.is_empty() && path != zuno_paths::MEMORY_SENTINEL => {
            Some(PathBuf::from(path))
        }
        _ => None,
    }
}

/// Fold pending WAL frames into the main file and truncate the sidecar.
///
/// `TRUNCATE` rather than the `PASSIVE` form [`open::PRAGMA_SEQUENCE`] issues:
/// passive leaves the `-wal` file at its high-water mark, so a footprint measured
/// right after it would count bytes that hold nothing.
///
/// # Errors
///
/// [`DbError::Query`] when the checkpoint statement fails. A checkpoint blocked by
/// another reader is reported by SQLite in the pragma's result row, not as an
/// error, so a busy database yields a truthful (larger) `wal_bytes` rather than a
/// failure.
pub fn checkpoint(connection: &Connection) -> Result<(), DbError> {
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(open::map_error)
}

/// Render `bytes` for a human, in binary units.
///
/// Integer arithmetic throughout: a size formatter that goes through `f64` starts
/// disagreeing with the exact byte count it is printed next to.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            let tenths = (bytes % scale) * 10 / scale;
            return format!("{whole}.{tenths} {unit}");
        }
    }
    format!("{bytes} B")
}

/// Serialize a report for `--format json`.
///
/// # Errors
///
/// [`DbError::Query`] when the report cannot be encoded.
pub fn to_json<T: Serialize>(report: &T) -> Result<serde_json::Value, DbError> {
    serde_json::to_value(report).map_err(|source| DbError::Query {
        source: Box::new(source),
    })
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn has_table(connection: &Connection, name: &str) -> Result<bool, DbError> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    Ok(count > 0)
}

fn page_counter(connection: &Connection, pragma: &str) -> Result<u64, DbError> {
    let value = connection
        .pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
        .map_err(open::map_error)?;
    u64::try_from(value).map_err(|source| DbError::Query {
        source: Box::new(source),
    })
}

fn table_rows(connection: &Connection) -> Result<Vec<TableRows>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name ASC",
        )
        .map_err(open::map_error)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    drop(statement);

    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        // The name came from `sqlite_master`, but it is still interpolated into
        // SQL, so it is quoted the way SQLite quotes identifiers.
        let quoted = name.replace('"', "\"\"");
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                row.get(0)
            })
            .map_err(open::map_error)?;
        tables.push(TableRows {
            table: name,
            rows: u64::try_from(rows).map_err(|source| DbError::Query {
                source: Box::new(source),
            })?,
        });
    }
    Ok(tables)
}

fn heaviest_sessions(connection: &Connection, limit: usize) -> Result<Vec<SessionBytes>, DbError> {
    let limit = i64::try_from(limit).map_err(|source| DbError::Query {
        source: Box::new(source),
    })?;
    // `LEFT JOIN`, so a session with no parts still appears once its heavier
    // neighbours are gone; a session whose rows have all been pruned is exactly
    // what an operator wants to see reported at zero.
    let sql = format!(
        "SELECT session.id, session.title, COUNT(part.id), \
                COALESCE(SUM({PART_BYTES}), 0) AS part_bytes \
         FROM session LEFT JOIN part ON part.session_id = session.id \
         GROUP BY session.id, session.title \
         ORDER BY part_bytes DESC, session.id ASC LIMIT ?1"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(open::map_error)?;

    let mut sessions = Vec::new();
    for row in rows {
        let (session_id, title, part_rows, part_bytes) = row.map_err(open::map_error)?;
        sessions.push(SessionBytes {
            session_id,
            title,
            part_rows: u64::try_from(part_rows).map_err(|source| DbError::Query {
                source: Box::new(source),
            })?,
            part_bytes: u64::try_from(part_bytes).map_err(|source| DbError::Query {
                source: Box::new(source),
            })?,
        });
    }
    Ok(sessions)
}

#[cfg(unix)]
fn available_on_filesystem(path: &Path) -> Availability {
    // `statvfs` needs a path that exists. The database file normally does, but
    // asking about a database that has not been created yet is a legitimate
    // question, and its directory is on the same filesystem.
    let target = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    match rustix::fs::statvfs(target) {
        // `f_frsize` is the fundamental block size `f_bavail` counts. Some
        // filesystems report it as 0; `f_bsize` is the documented fallback.
        Ok(stat) => {
            let block = if stat.f_frsize == 0 {
                stat.f_bsize
            } else {
                stat.f_frsize
            };
            Availability::Known {
                bytes: stat.f_bavail.saturating_mul(block),
            }
        }
        Err(errno) => Availability::Unknown {
            reason: format!("statvfs({}) failed: {errno}", target.display()),
        },
    }
}

#[cfg(not(unix))]
fn available_on_filesystem(path: &Path) -> Availability {
    Availability::Unknown {
        reason: format!(
            "free space on the filesystem holding {} cannot be queried on this platform \
             without unsafe FFI, which this workspace forbids",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_size_reads_the_main_file_and_both_wal_sidecars_in_order() {
        // The positional reads in `database_size` depend on this order; pin it
        // here so a change in `open` surfaces as a failure with a reason.
        assert_eq!(open::WAL_SIDECAR_SUFFIXES, ["-wal", "-shm"]);

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("opencode.db");
        std::fs::write(&path, vec![0_u8; 300]).expect("write main");
        std::fs::write(directory.path().join("opencode.db-wal"), vec![0_u8; 20])
            .expect("write wal");
        std::fs::write(directory.path().join("opencode.db-shm"), vec![0_u8; 7]).expect("write shm");

        let size = database_size(&path);
        assert_eq!(size.main_bytes, 300);
        assert_eq!(size.wal_bytes, 20);
        assert_eq!(size.shm_bytes, 7);
        assert_eq!(size.total_bytes(), 327);
    }

    #[test]
    fn a_missing_file_measures_zero_rather_than_failing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let size = database_size(&directory.path().join("absent.db"));
        assert_eq!(size, DatabaseSize::default());
        assert_eq!(size.total_bytes(), 0);
    }

    #[test]
    fn reclaim_saturates_so_a_grown_database_never_reports_negative_bytes() {
        let small = DatabaseSize {
            main_bytes: 10,
            wal_bytes: 0,
            shm_bytes: 0,
        };
        let large = DatabaseSize {
            main_bytes: 100,
            wal_bytes: 0,
            shm_bytes: 0,
        };
        assert_eq!(large.reclaimed_since(small), 90);
        assert_eq!(small.reclaimed_since(large), 0);
    }

    #[test]
    fn format_bytes_stays_exact_at_every_unit_boundary() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(12 * 1024 * 1024), "12.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes(1024_u64.pow(4)), "1.0 TiB");
    }

    #[test]
    fn an_in_memory_connection_has_no_file_to_compact() {
        let connection = open::open_shared_memory("zuno-vacuum-unit").expect("open shared memory");
        assert!(database_path(&connection).is_none());
    }

    #[test]
    fn availability_exposes_its_byte_count_only_when_it_has_one() {
        assert_eq!(Availability::Known { bytes: 7 }.bytes(), Some(7));
        assert_eq!(
            Availability::Unknown {
                reason: "no".to_owned()
            }
            .bytes(),
            None
        );
    }

    #[test]
    fn the_real_probe_answers_for_a_path_that_exists_and_for_one_that_does_not() {
        let directory = tempfile::tempdir().expect("tempdir");
        let present = directory.path().join("present.db");
        std::fs::write(&present, b"x").expect("write");

        let probe = SystemDiskSpace;
        for path in [present.as_path(), &directory.path().join("absent.db")] {
            let answer = probe.available_bytes(path);
            // A path that does not exist yet must still be answerable, because
            // `statvfs` needs an existing target and the guard has to work on a
            // database file the caller has not created.
            #[cfg(unix)]
            match answer {
                Availability::Known { bytes } => assert!(
                    bytes > 0,
                    "a writable temporary directory must report free space"
                ),
                Availability::Unknown { reason } => panic!("unix must answer: {reason}"),
            }
            #[cfg(not(unix))]
            assert!(matches!(answer, Availability::Unknown { .. }));
        }
    }
}
