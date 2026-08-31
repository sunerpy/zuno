//! Zuno-native SQLite storage for sessions, events, inboxes, and projections.
//!
//! # Storage contract
//!
//! The selected Zuno database is the durable source of truth for session state.
//! Every pooled connection receives the same explicit pragma sequence, and the
//! location comes from [`zuno_paths::db_path`] rather than being re-derived by
//! storage callers. Existing files must carry the current schema-format marker;
//! other pre-release or cross-product formats are rejected without mutation.
//!
//! # The pragma that is load-bearing
//!
//! `PRAGMA foreign_keys` is per-connection state, and *what it defaults to
//! depends on which SQLite you are linked against*. Upstream SQLite and every
//! distro `libsqlite3` default it off — measured at 0 on system SQLite 3.53.4 —
//! while the amalgamation `libsqlite3-sys` bundles may be compiled with a
//! different default. Tests pin the observed behavior. The pragma is always
//! issued explicitly, because a connection that inherits *off* leaves every
//! `ON DELETE CASCADE` inert without reporting it.
//!
//! That is also why [`Pool`] owns connection creation outright — there is no way
//! to put an unconfigured connection into it — and why
//! [`open::apply_pragmas`] reads every pragma back instead of trusting that the
//! statement it sent was honoured.
//!
//! # Scope
//!
//! Opening, pooling and transactions. Current-schema creation and format validation
//! live in this crate too, but are not this module's business.
//!
//! ```
//! use zuno_db::Pool;
//! use zuno_paths::DbLocation;
//!
//! let pool = Pool::open(&DbLocation::Memory)?;
//! pool.transaction(|tx| {
//!     tx.execute_batch("CREATE TABLE demo (id integer primary key)")
//!         .map_err(zuno_db::open::map_error)
//! })?;
//! # Ok::<(), zuno_error::DbError>(())
//! ```

pub mod artifact_gc;
pub mod evaluation;
pub mod event_log;
pub mod experience;
pub mod feedback;
pub mod fts;
pub mod human_request;
pub mod inbox;
pub mod job;
pub mod learning_job;
pub mod learning_pattern;
pub mod memory_candidate;
pub mod memory_reflection;
pub mod message;
pub mod migration;
pub mod open;
pub mod pool;
pub mod provider_backoff;
pub mod prune;
pub mod retention;
pub mod schema;
pub mod session;
pub mod session_export;
pub mod session_list;
pub mod session_prune;
pub mod skill_candidate;
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
