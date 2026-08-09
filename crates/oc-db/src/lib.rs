//! SQLite storage layer with schema parity against the TypeScript `opencode.db`.
//!
//! # What this crate promises
//!
//! A user must be able to switch between the TypeScript `opencode` binary and
//! this one and back, keeping the same sessions. That promise is decided by one
//! file on disk, so this crate opens it exactly the way `database.ts:22-33` does
//! — the same five pragmas, in the same order, followed by the same passive WAL
//! checkpoint — and takes its location from [`oc_paths::db_path`] rather than
//! deriving one. Pinned to `opencode` **1.18.13**.
//!
//! # The pragma that is load-bearing
//!
//! `PRAGMA foreign_keys` is per-connection state, and *what it defaults to
//! depends on which SQLite you are linked against*. Upstream SQLite and every
//! distro `libsqlite3` default it off — measured at 0 on system SQLite 3.53.4 —
//! while the amalgamation `libsqlite3-sys` bundles is compiled with
//! `SQLITE_DEFAULT_FOREIGN_KEYS=1` and defaults it on. Both facts are pinned by
//! tests. So the pragma is always issued explicitly, because a connection that
//! inherits *off* leaves every `ON DELETE CASCADE` the session schema declares
//! inert with nothing reporting it: the delete succeeds and orphans rows the
//! TypeScript binary will later trip over.
//!
//! That is also why [`Pool`] owns connection creation outright — there is no way
//! to put an unconfigured connection into it — and why
//! [`open::apply_pragmas`] reads every pragma back instead of trusting that the
//! statement it sent was honoured.
//!
//! # Scope
//!
//! Opening, pooling and transactions. Table creation and the migration chain live
//! in this crate too, but are not this module's business.
//!
//! ```
//! use oc_db::Pool;
//! use oc_paths::DbLocation;
//!
//! let pool = Pool::open(&DbLocation::Memory)?;
//! pool.transaction(|tx| {
//!     tx.execute_batch("CREATE TABLE demo (id integer primary key)")
//!         .map_err(oc_db::open::map_error)
//! })?;
//! # Ok::<(), oc_error::DbError>(())
//! ```

pub mod artifact_gc;
pub mod fts;
pub mod message;
pub mod migration;
pub mod open;
pub mod pool;
pub mod prune;
pub mod retention;
pub mod schema;
pub mod session;
pub mod session_export;
pub mod session_list;
pub mod session_prune;
pub mod vacuum;

pub use crate::open::{
    BUSY_TIMEOUT_MS, CACHE_SIZE_KIB, FOREIGN_KEYS_ON, JOURNAL_MODE_MEMORY, JOURNAL_MODE_WAL,
    PRAGMA_SEQUENCE, SYNCHRONOUS_NORMAL, WAL_SIDECAR_SUFFIXES, apply_pragmas, is_busy,
    is_constraint_violation, map_error, open_at, open_default, open_shared_memory, sidecar_files,
    verify_pragmas,
};
pub use crate::pool::{DEFAULT_MAX_IDLE, Pool, PooledConnection};
pub use crate::vacuum::{
    Availability, DEFAULT_LARGEST_SESSIONS, DatabaseSize, DatabaseStats, DiskSpace, INTEGRITY_OK,
    IntegrityReport, SystemDiskSpace, VacuumError, VacuumReport, database_size, integrity_check,
    stats, vacuum,
};
pub use rusqlite::{Connection, Transaction, TransactionBehavior};
