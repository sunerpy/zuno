//! A connection pool over one `zuno.db`, and the transaction helper the
//! session store runs every write through.

use crate::open;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use zuno_error::DbError;
use zuno_paths::DbLocation;

/// How many idle connections a pool keeps before it closes returned ones.
pub const DEFAULT_MAX_IDLE: usize = 4;

/// A pool of connections to one database, each carrying Zuno's pragmas.
///
/// # Why a pool needs to own connection creation
///
/// `journal_mode` is the only configured pragma that lives in the
/// database file. `synchronous`, `busy_timeout`, `cache_size` and `foreign_keys`
/// are per-connection settings that reset on every new connection — a reopened
/// database reports `wal` and a `busy_timeout` of *zero*, so an unconfigured
/// connection fails instantly on a contended write instead of waiting. And
/// whether it enforces foreign keys depends on the linked SQLite's compile-time
/// default rather than on anything visible in this code. A pool that handed out
/// connections it had not configured would therefore hand out ones that behave
/// differently from its siblings, with no error anywhere. So every connection this
/// pool produces goes through [`open`], and there is no constructor that takes a
/// caller's own [`Connection`].
///
/// # Concurrency
///
/// [`Connection`] is `Send` but not `Sync`, so a connection is checked out
/// exclusively and returned on drop. The pool serializes writers in-process before
/// opening or checking out their connections. This matches SQLite's single-writer
/// model and avoids a shared-memory database returning `SQLITE_LOCKED` when one
/// thread opens a connection and applies its pragmas while another holds a write
/// transaction. SQLite still serializes writers from other processes; WAL keeps
/// readers concurrent with either kind of writer.
pub struct Pool {
    location: DbLocation,
    target: String,
    max_idle: usize,
    writer: Mutex<()>,
    state: Mutex<State>,
}

struct State {
    idle: Vec<Connection>,
    anchor: Option<Connection>,
}

impl Pool {
    /// Open a pool on the database the running binary would use.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when the database cannot be opened or configured.
    pub fn open_default() -> Result<Self, DbError> {
        let location = zuno_paths::db_path();
        Self::open(&location)
    }

    /// Open a pool on `location`.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when the database cannot be opened or a pragma did not
    /// take effect.
    pub fn open(location: &DbLocation) -> Result<Self, DbError> {
        Self::open_with_max_idle(location, DEFAULT_MAX_IDLE)
    }

    /// Open a pool on `location`, keeping at most `max_idle` idle connections.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when the database cannot be opened or a pragma did not
    /// take effect.
    pub fn open_with_max_idle(location: &DbLocation, max_idle: usize) -> Result<Self, DbError> {
        let target = match location {
            DbLocation::Memory => open::shared_memory_uri(&unique_memory_name()),
            DbLocation::File(path) => {
                open::ensure_parent(path)?;
                path.to_string_lossy().into_owned()
            }
        };
        let anchor = match location {
            DbLocation::Memory => Some(open::open_target(&target, location)?),
            DbLocation::File(_) => None,
        };
        let first = open::open_target(&target, location)?;
        Ok(Self {
            location: location.clone(),
            target,
            max_idle: max_idle.max(1),
            writer: Mutex::new(()),
            state: Mutex::new(State {
                idle: vec![first],
                anchor,
            }),
        })
    }

    /// Where this pool's database lives.
    #[must_use]
    pub fn location(&self) -> &DbLocation {
        &self.location
    }

    /// The exact string handed to SQLite.
    ///
    /// For a file database this is the path. For [`DbLocation::Memory`] it is the
    /// shared-cache URI that lets several pooled connections see one in-memory
    /// database.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The on-disk database file, or `None` for an in-memory pool.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.location.as_path()
    }

    /// The `-wal` and `-shm` files beside this pool's database, if it is a file.
    #[must_use]
    pub fn sidecar_files(&self) -> Vec<PathBuf> {
        self.location
            .as_path()
            .map(open::sidecar_files)
            .unwrap_or_default()
    }

    /// How many connections are idle right now.
    #[must_use]
    pub fn idle_count(&self) -> usize {
        self.lock().idle.len()
    }

    /// Whether this pool holds the connection that keeps a shared in-memory
    /// database alive.
    ///
    /// A shared in-memory database exists only while at least one connection to
    /// it is open, so an in-memory pool permanently retains one that is never
    /// handed out. Dropping the last connection would discard the data, which is
    /// why the anchor is not merely an idle connection. A file pool needs none.
    #[must_use]
    pub fn holds_memory_anchor(&self) -> bool {
        self.lock().anchor.is_some()
    }

    /// Open an owned connection to this pool's database.
    ///
    /// Unlike [`Self::get`], the connection is not returned to the idle set when
    /// dropped. It still uses the pool's configured target, so an in-memory
    /// connection shares the database kept alive by this pool's anchor. The pool
    /// must therefore outlive an owned connection to an in-memory database.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when the connection cannot be opened or configured.
    pub fn open_connection(&self) -> Result<Connection, DbError> {
        open::open_target(&self.target, &self.location)
    }

    /// Check out a connection, opening a new one if none is idle.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when a new connection cannot be opened or a pragma did
    /// not take effect.
    pub fn get(&self) -> Result<PooledConnection<'_>, DbError> {
        let existing = self.lock().idle.pop();
        let connection = match existing {
            Some(connection) => connection,
            None => open::open_target(&self.target, &self.location)?,
        };
        Ok(PooledConnection {
            pool: self,
            connection: Some(connection),
        })
    }

    /// Run `work` inside a write transaction, committing when it succeeds.
    ///
    /// The transaction is `IMMEDIATE`, not SQLite's default `DEFERRED`, and that
    /// choice is what makes `busy_timeout` do its job. A deferred transaction
    /// takes a read lock first and asks to upgrade on its first write; if another
    /// writer committed in between, SQLite fails the upgrade with
    /// `SQLITE_BUSY_SNAPSHOT`, which the busy handler is explicitly *not* allowed
    /// to retry because the reader's snapshot is already stale. `IMMEDIATE` takes
    /// the write lock up front, so a second writer waits out the timeout and then
    /// proceeds instead of failing.
    ///
    /// `work` returning `Err` drops the transaction unfinished, which rolls it
    /// back.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, [`DbError::Busy`] when the write lock could not
    /// be taken within the busy timeout, or [`DbError::Query`] when the
    /// transaction could not be started or committed.
    pub fn transaction<T, F>(&self, work: F) -> Result<T, DbError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    {
        self.transaction_with_behavior(TransactionBehavior::Immediate, work)
    }

    /// Run `work` inside a transaction with an explicit locking behaviour.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, [`DbError::Busy`] when the lock could not be
    /// taken within the busy timeout, or [`DbError::Query`] when the transaction
    /// could not be started or committed.
    pub fn transaction_with_behavior<T, F>(
        &self,
        behavior: TransactionBehavior,
        work: F,
    ) -> Result<T, DbError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    {
        let _writer = self.lock_writer();
        let mut connection = self.get()?;
        let transaction = connection
            .transaction_with_behavior(behavior)
            .map_err(open::map_error)?;
        let value = work(&transaction)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(value)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_writer(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn put_back(&self, connection: Connection) {
        let mut state = self.lock();
        if state.idle.len() < self.max_idle {
            state.idle.push(connection);
        }
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("location", &self.location)
            .field("target", &self.target)
            .field("max_idle", &self.max_idle)
            .field("idle", &self.idle_count())
            .field("memory_anchor", &self.holds_memory_anchor())
            .finish()
    }
}

/// A connection checked out of a [`Pool`], returned to it on drop.
pub struct PooledConnection<'pool> {
    pool: &'pool Pool,
    connection: Option<Connection>,
}

impl PooledConnection<'_> {
    /// Run `work` inside an `IMMEDIATE` transaction on this connection.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, [`DbError::Busy`] when the write lock could not
    /// be taken within the busy timeout, or [`DbError::Query`] when the
    /// transaction could not be started or committed.
    pub fn transaction<T, F>(&mut self, work: F) -> Result<T, DbError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    {
        let pool = self.pool;
        let _writer = pool.lock_writer();
        let transaction = self
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(open::map_error)?;
        let value = work(&transaction)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(value)
    }
}

impl Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("a checked-out connection is present until it is dropped")
    }
}

impl DerefMut for PooledConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("a checked-out connection is present until it is dropped")
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.put_back(connection);
        }
    }
}

impl std::fmt::Debug for PooledConnection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledConnection")
            .field("target", &self.pool.target)
            .finish()
    }
}

fn unique_memory_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ordinal = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("zuno-db-{}-{ordinal}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_names_are_unique_per_pool() {
        let first = unique_memory_name();
        let second = unique_memory_name();
        assert_ne!(first, second);
        assert!(first.starts_with("zuno-db-"));
    }

    #[test]
    fn an_in_memory_pool_shares_one_database_across_connections() {
        let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
        {
            let first = pool.get().expect("first connection");
            first
                .execute_batch("CREATE TABLE t (id integer primary key)")
                .expect("create table");
            first
                .execute("INSERT INTO t (id) VALUES (1)", [])
                .expect("insert");
        }
        let second = pool.get().expect("second connection");
        let count: i64 = second
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(count, 1);
    }

    #[test]
    fn an_owned_connection_shares_the_pools_database() {
        let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
        let owned = pool
            .open_connection()
            .expect("open an owned connection from the pool");
        owned
            .execute_batch("CREATE TABLE t (id integer primary key); INSERT INTO t VALUES (1)")
            .expect("seed through the owned connection");

        let pooled = pool.get().expect("check out a pooled connection");
        pooled
            .execute("INSERT INTO t VALUES (2)", [])
            .expect("write through the pooled connection");
        let count: i64 = owned
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .expect("read both writes through the owned connection");

        assert_eq!(count, 2);
    }

    #[test]
    fn a_connection_returns_to_the_pool_on_drop() {
        let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
        assert_eq!(pool.idle_count(), 1);
        {
            let _connection = pool.get().expect("checkout");
            assert_eq!(pool.idle_count(), 0);
        }
        assert_eq!(pool.idle_count(), 1);
    }

    #[test]
    fn a_pool_over_max_idle_closes_returned_connections() {
        let pool = Pool::open_with_max_idle(&DbLocation::Memory, 1).expect("open pool");
        let first = pool.get().expect("first");
        let second = pool.get().expect("second");
        drop(first);
        drop(second);
        assert_eq!(pool.idle_count(), 1);
    }
}
