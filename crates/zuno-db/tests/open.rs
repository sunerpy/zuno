//! Opening `zuno.db`: the pragmas, the `ZUNO_DB` forms, WAL behaviour
//! under concurrent writers, and proof that `foreign_keys = ON` is in force.

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use zuno_db::open;
use zuno_db::{Pool, TransactionBehavior};
use zuno_paths::env::{HOME, XDG_DATA_HOME, ZUNO_DB};
use zuno_paths::{DbLocation, Env, Layout};

fn layout(pairs: &[(&str, &str)]) -> Layout {
    Layout::resolve_with(&Env::from_pairs(pairs.iter().copied()), None)
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create a temporary directory")
}

#[test]
fn default_database_open_uses_only_the_zuno_path() {
    let dir = temp_dir();
    let unrelated = dir.path().join("opencode.db");
    let zuno = dir.path().join("zuno.db");
    std::fs::write(&unrelated, b"not a sqlite database").expect("create unrelated file");

    drop(
        open::open_default_location(&DbLocation::File(zuno.clone()))
            .expect("the computed Zuno path opens independently"),
    );

    assert!(zuno.is_file());
    assert_eq!(
        std::fs::read(&unrelated).expect("read unrelated file"),
        b"not a sqlite database"
    );
}

// ---------------------------------------------------------------------------
// The four pragmas, read back from the connection.
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_file_database_reports_all_four_pragmas_active() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let connection = open::open_at(&path).expect("open a fresh database");

    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let synchronous: i64 = connection
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous");
    let busy_timeout: i64 = connection
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .expect("read busy_timeout");
    let cache_size: i64 = connection
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .expect("read cache_size");
    let foreign_keys: i64 = connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("read foreign_keys");

    assert_eq!(journal_mode, zuno_db::JOURNAL_MODE_WAL);
    assert_eq!(synchronous, zuno_db::SYNCHRONOUS_NORMAL);
    assert_eq!(busy_timeout, zuno_db::BUSY_TIMEOUT_MS);
    assert_eq!(cache_size, zuno_db::CACHE_SIZE_KIB);
    assert_eq!(foreign_keys, zuno_db::FOREIGN_KEYS_ON);
}

/// `journal_mode` is recorded in the database file and survives a close;
/// `synchronous` and `cache_size` are per-connection and reset to SQLite's
/// defaults, so a connection nobody configured is durable-but-slower and caches
/// 2 MiB instead of 64. This is why [`Pool`] owns connection creation.
#[test]
fn wal_survives_a_close_but_the_per_connection_pragmas_do_not() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    drop(open::open_at(&path).expect("open a fresh database"));

    let raw = zuno_db::Connection::open(&path).expect("reopen without pragmas");
    let journal_mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let synchronous: i64 = raw
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous");
    let cache_size: i64 = raw
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .expect("read cache_size");
    assert_eq!(journal_mode, zuno_db::JOURNAL_MODE_WAL);
    assert_ne!(
        synchronous,
        zuno_db::SYNCHRONOUS_NORMAL,
        "synchronous must not survive a close"
    );
    assert_ne!(
        cache_size,
        zuno_db::CACHE_SIZE_KIB,
        "cache_size must not survive a close"
    );
    drop(raw);

    let configured = open::open_at(&path).expect("reopen through this crate");
    open::verify_pragmas(&configured, &DbLocation::File(path))
        .expect("a connection opened through this crate carries every pragma");
}

/// Two measured properties of the stack below this crate, neither of which this
/// crate relies on, both pinned so a silent change is visible here rather than as
/// a cascade that stopped firing or a write that stopped waiting.
///
/// `libsqlite3-sys` compiles the amalgamation with
/// `SQLITE_DEFAULT_FOREIGN_KEYS=1` — while upstream SQLite and every distro
/// `libsqlite3` default it *off*, measured at 0 on system SQLite 3.53.4. And
/// `rusqlite` calls `sqlite3_busy_timeout(db, 5000)` on every connection it
/// opens, which happens to be exactly the value `database.ts:29` asks for.
///
/// Together these mean two required pragmas would *appear* correct on
/// this driver even if the crate never issued them. The pragmas are issued
/// explicitly anyway, because behaviour must not depend on which SQLite is linked
/// or on a driver's undocumented default.
#[test]
fn the_stack_below_this_crate_already_defaults_two_required_pragmas() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let raw = zuno_db::Connection::open(&path).expect("open without pragmas");

    let foreign_keys: i64 = raw
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("read foreign_keys");
    assert_eq!(
        foreign_keys,
        zuno_db::FOREIGN_KEYS_ON,
        "the bundled amalgamation no longer defaults foreign keys on; \
         the explicit pragma is now the only thing enforcing cascades"
    );

    let busy_timeout: i64 = raw
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .expect("read busy_timeout");
    assert_eq!(
        busy_timeout,
        zuno_db::BUSY_TIMEOUT_MS,
        "rusqlite no longer defaults busy_timeout to Zuno's 5000ms; \
         the explicit pragma is now the only thing making a writer wait"
    );
    drop(raw);

    let configured = open::open_at(&path).expect("open through this crate");
    open::verify_pragmas(&configured, &DbLocation::File(path))
        .expect("a connection opened through this crate carries every pragma");
}

#[test]
fn every_pooled_connection_carries_the_pragmas() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let pool = Pool::open(&DbLocation::File(path)).expect("open pool");

    let first = pool.get().expect("first connection");
    let second = pool.get().expect("second connection");
    for connection in [&first, &second] {
        let foreign_keys: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("read foreign_keys");
        let busy_timeout: i64 = connection
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("read busy_timeout");
        assert_eq!(foreign_keys, zuno_db::FOREIGN_KEYS_ON);
        assert_eq!(busy_timeout, zuno_db::BUSY_TIMEOUT_MS);
    }
}

/// SQLite refuses WAL for an in-memory database and reports `memory` instead of
/// failing, so the verifier has to expect that rather than `wal`.
#[test]
fn an_in_memory_database_reports_memory_journalling_and_still_enforces_foreign_keys() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    let connection = pool.get().expect("checkout");

    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let foreign_keys: i64 = connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("read foreign_keys");
    assert_eq!(journal_mode, zuno_db::JOURNAL_MODE_MEMORY);
    assert_eq!(foreign_keys, zuno_db::FOREIGN_KEYS_ON);
    assert!(pool.holds_memory_anchor());
}

#[test]
fn verify_pragmas_rejects_a_connection_whose_foreign_keys_were_turned_off() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let location = DbLocation::File(path.clone());
    let connection = open::open_at(&path).expect("open a fresh database");
    open::verify_pragmas(&connection, &location).expect("a configured connection verifies");

    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("turn foreign keys off");
    let error = open::verify_pragmas(&connection, &location)
        .expect_err("a connection with foreign keys off must not verify");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("could not be opened"),
        "unexpected error: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// Failure QA: a dangling foreign key is rejected.
// ---------------------------------------------------------------------------

fn create_parent_and_child(connection: &zuno_db::Connection) {
    connection
        .execute_batch(
            "CREATE TABLE parent (id text primary key);\n\
             CREATE TABLE child (\n\
               id text primary key,\n\
               parent_id text not null references parent(id) on delete cascade\n\
             );\n\
             INSERT INTO parent (id) VALUES ('p1');",
        )
        .expect("create the parent and child tables");
}

#[test]
fn a_child_row_with_a_dangling_foreign_key_is_rejected() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let connection = open::open_at(&path).expect("open a fresh database");
    create_parent_and_child(&connection);

    let error = connection
        .execute(
            "INSERT INTO child (id, parent_id) VALUES ('c1', 'does-not-exist')",
            [],
        )
        .expect_err("a dangling foreign key must be rejected");

    assert!(
        open::is_constraint_violation(&error),
        "not a constraint violation: {error}"
    );
    assert!(
        error.to_string().contains("FOREIGN KEY constraint failed"),
        "unexpected message: {error}"
    );
    assert!(!open::map_error(error).is_retryable());
}

/// The positive control for the test above: with `foreign_keys = OFF` the very
/// same insert succeeds. Without this, a test asserting rejection could be
/// passing for some unrelated reason and would not notice the pragma going
/// missing.
#[test]
fn the_same_insert_succeeds_once_foreign_keys_are_turned_off() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let connection = open::open_at(&path).expect("open a fresh database");
    create_parent_and_child(&connection);
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("turn foreign keys off");

    let inserted = connection
        .execute(
            "INSERT INTO child (id, parent_id) VALUES ('c1', 'does-not-exist')",
            [],
        )
        .expect("with foreign keys off the dangling row is accepted");
    assert_eq!(inserted, 1);
}

#[test]
fn a_cascade_declared_on_the_child_actually_fires() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    {
        let connection = pool.get().expect("checkout");
        create_parent_and_child(&connection);
        connection
            .execute("INSERT INTO child (id, parent_id) VALUES ('c1', 'p1')", [])
            .expect("insert a valid child");
    }

    pool.transaction(|tx| {
        tx.execute("DELETE FROM parent WHERE id = 'p1'", [])
            .map_err(open::map_error)?;
        Ok(())
    })
    .expect("delete the parent");

    let connection = pool.get().expect("checkout");
    let children: i64 = connection
        .query_row("SELECT count(*) FROM child", [], |row| row.get(0))
        .expect("count children");
    assert_eq!(children, 0);
}

// ---------------------------------------------------------------------------
// Happy QA: two concurrent writers under WAL both succeed.
// ---------------------------------------------------------------------------

#[test]
fn two_concurrent_writers_both_succeed_within_the_busy_timeout() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let pool = Arc::new(Pool::open(&DbLocation::File(path)).expect("open pool"));
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE writes (id integer primary key, who text not null)")
            .map_err(open::map_error)
    })
    .expect("create the table");

    let hold = Duration::from_millis(300);
    let barrier = Arc::new(Barrier::new(2));
    let started = Instant::now();

    let slow_pool = Arc::clone(&pool);
    let slow_barrier = Arc::clone(&barrier);
    let slow = std::thread::spawn(move || {
        slow_pool.transaction(|tx| {
            tx.execute("INSERT INTO writes (who) VALUES ('slow')", [])
                .map_err(open::map_error)?;
            slow_barrier.wait();
            std::thread::sleep(hold);
            Ok(())
        })
    });

    let fast_pool = Arc::clone(&pool);
    let fast_barrier = Arc::clone(&barrier);
    let fast = std::thread::spawn(move || {
        fast_barrier.wait();
        let waited_from = Instant::now();
        let outcome = fast_pool.transaction(|tx| {
            tx.execute("INSERT INTO writes (who) VALUES ('fast')", [])
                .map_err(open::map_error)
        });
        (outcome, waited_from.elapsed())
    });

    slow.join()
        .expect("the slow writer thread")
        .expect("the slow writer commits");
    let (fast_outcome, fast_waited) = fast.join().expect("the fast writer thread");
    fast_outcome.expect("the fast writer commits after waiting out the lock");
    let total = started.elapsed();

    let connection = pool.get().expect("checkout");
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM writes", [], |row| row.get(0))
        .expect("count writes");
    assert_eq!(rows, 2, "both writers must have committed");

    assert!(
        fast_waited >= Duration::from_millis(200),
        "the second writer did not actually contend for the lock: waited {fast_waited:?}"
    );
    let busy_timeout =
        Duration::from_millis(u64::try_from(zuno_db::BUSY_TIMEOUT_MS).expect("a positive timeout"));
    assert!(
        total < busy_timeout,
        "the writers took longer than the busy timeout: {total:?}"
    );
}

#[test]
fn an_immediate_transaction_from_a_shared_reference_reserves_the_writer() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let first_connection = open::open_at(&path).expect("open first writer");
    first_connection
        .execute_batch("CREATE TABLE writes (id integer primary key, who text not null)")
        .expect("create the table");
    let second_connection = open::open_at(&path).expect("open second writer");
    second_connection
        .busy_timeout(Duration::ZERO)
        .expect("disable waiting for the deterministic contention probe");

    let first = open::immediate_transaction(&first_connection)
        .expect("begin first immediate transaction through a shared reference");
    first
        .query_row("SELECT count(*) FROM writes", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read after reserving the writer");

    let busy = match open::immediate_transaction(&second_connection) {
        Ok(_) => panic!("a second writer started while the first reservation was active"),
        Err(error) => error,
    };
    assert!(
        matches!(busy, zuno_error::DbError::Busy { .. }),
        "a contending writer returned the wrong error: {busy}"
    );

    first
        .execute("INSERT INTO writes (who) VALUES ('first')", [])
        .expect("write after reading");
    first.commit().expect("commit first writer");

    let second = open::immediate_transaction(&second_connection)
        .expect("second writer starts after the reservation is released");
    let visible_rows: i64 = second
        .query_row("SELECT count(*) FROM writes", [], |row| row.get(0))
        .expect("read the first committed row");
    assert_eq!(visible_rows, 1);
    second
        .execute("INSERT INTO writes (who) VALUES ('second')", [])
        .expect("write from the second transaction");
    second.commit().expect("commit second writer");
}

/// A reader must not be blocked by an open writer. That is the whole reason the
/// Zuno uses WAL rather than the default rollback journal.
#[test]
fn a_reader_is_not_blocked_by_an_open_writer_under_wal() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let pool = Pool::open(&DbLocation::File(path)).expect("open pool");
    pool.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE rows_ (id integer primary key);\n\
             INSERT INTO rows_ (id) VALUES (1);",
        )
        .map_err(open::map_error)
    })
    .expect("seed the table");

    let mut writer = pool.get().expect("writer connection");
    let transaction = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin an immediate write transaction");
    transaction
        .execute("INSERT INTO rows_ (id) VALUES (2)", [])
        .expect("write inside the open transaction");

    let reader = pool.get().expect("reader connection");
    let visible: i64 = reader
        .query_row("SELECT count(*) FROM rows_", [], |row| row.get(0))
        .expect("a reader is served while a writer holds the lock");
    assert_eq!(
        visible, 1,
        "the reader must see the pre-transaction snapshot"
    );

    transaction.commit().expect("commit");
    let visible: i64 = reader
        .query_row("SELECT count(*) FROM rows_", [], |row| row.get(0))
        .expect("read after commit");
    assert_eq!(visible, 2);
}

// ---------------------------------------------------------------------------
// The transaction helper.
// ---------------------------------------------------------------------------

#[test]
fn a_transaction_that_fails_leaves_nothing_behind() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE t (id integer primary key)")
            .map_err(open::map_error)
    })
    .expect("create the table");

    let outcome: Result<(), zuno_error::DbError> = pool.transaction(|tx| {
        tx.execute("INSERT INTO t (id) VALUES (1)", [])
            .map_err(open::map_error)?;
        Err(zuno_error::DbError::NotFound {
            table: "t".to_owned(),
            id: "1".to_owned(),
        })
    });
    assert!(outcome.is_err());

    let connection = pool.get().expect("checkout");
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .expect("count rows");
    assert_eq!(rows, 0, "the failed transaction must have rolled back");
}

#[test]
fn a_transaction_that_succeeds_commits() {
    let pool = Pool::open(&DbLocation::Memory).expect("open in-memory pool");
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE t (id integer primary key)")
            .map_err(open::map_error)
    })
    .expect("create the table");
    let inserted = pool
        .transaction(|tx| {
            tx.execute("INSERT INTO t (id) VALUES (1)", [])
                .map_err(open::map_error)
        })
        .expect("insert");
    assert_eq!(inserted, 1);

    let connection = pool.get().expect("checkout");
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .expect("count rows");
    assert_eq!(rows, 1);
}

// ---------------------------------------------------------------------------
// The three `ZUNO_DB` forms, resolved by `zuno-paths` and opened here.
// ---------------------------------------------------------------------------

#[test]
fn zuno_db_memory_yields_an_in_memory_database_and_writes_no_file() {
    let dir = temp_dir();
    let resolved = layout(&[
        (HOME, &dir.path().to_string_lossy()),
        (XDG_DATA_HOME, &dir.path().to_string_lossy()),
        (ZUNO_DB, ":memory:"),
    ]);
    let location = resolved.db_path();
    assert_eq!(location, DbLocation::Memory);

    let pool = Pool::open(&location).expect("open the resolved location");
    assert!(pool.location().is_memory());
    assert!(pool.path().is_none());
    assert!(pool.sidecar_files().is_empty());
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE t (id integer primary key)")
            .map_err(open::map_error)
    })
    .expect("create a table in memory");

    let files = entries(dir.path());
    assert!(
        files.iter().all(|name| !name.ends_with(".db")),
        "an in-memory database must not touch the filesystem, found {files:?}"
    );
}

#[test]
fn zuno_db_memory_is_transient_between_pools() {
    let first = Pool::open(&DbLocation::Memory).expect("first pool");
    first
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t (id integer primary key)")
                .map_err(open::map_error)
        })
        .expect("create a table");

    let second = Pool::open(&DbLocation::Memory).expect("second pool");
    let connection = second.get().expect("checkout");
    let error = connection
        .query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0))
        .expect_err("a second in-memory pool must not see the first pool's tables");
    assert!(
        error.to_string().contains("no such table"),
        "unexpected error: {error}"
    );
    assert_ne!(first.target(), second.target());
}

#[test]
fn zuno_db_relative_resolves_under_data_and_the_file_lands_there() {
    let dir = temp_dir();
    let data_home = dir.path().join("xdg");
    let resolved = layout(&[
        (HOME, &dir.path().to_string_lossy()),
        (XDG_DATA_HOME, &data_home.to_string_lossy()),
        (ZUNO_DB, "relprobe.db"),
    ]);
    let location = resolved.db_path();
    let expected = data_home.join("zuno").join("relprobe.db");
    assert_eq!(location, DbLocation::File(expected.clone()));
    assert!(
        location
            .as_path()
            .is_some_and(|path| path.starts_with(resolved.data()))
    );

    let pool = Pool::open(&location).expect("open the resolved location");
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE t (id integer primary key)")
            .map_err(open::map_error)
    })
    .expect("write to the resolved file");
    assert!(expected.is_file(), "{} was not created", expected.display());
    assert!(
        !Path::new("relprobe.db").exists(),
        "the file landed in the working directory"
    );
}

#[test]
fn zuno_db_absolute_is_used_verbatim() {
    let dir = temp_dir();
    let absolute = dir.path().join("nested").join("custom.db");
    let resolved = layout(&[
        (HOME, &dir.path().to_string_lossy()),
        (XDG_DATA_HOME, &dir.path().join("xdg").to_string_lossy()),
        (ZUNO_DB, &absolute.to_string_lossy()),
    ]);
    let location = resolved.db_path();
    assert_eq!(location, DbLocation::File(absolute.clone()));

    let pool = Pool::open(&location).expect("open a nested absolute path");
    pool.transaction(|tx| {
        tx.execute_batch("CREATE TABLE t (id integer primary key)")
            .map_err(open::map_error)
    })
    .expect("write to the absolute file");
    assert!(absolute.is_file(), "{} was not created", absolute.display());
}

// ---------------------------------------------------------------------------
// What WAL puts on disk. Todo 82 prunes and todo 84 vacuums; both have to move
// the whole set, not just the main file.
// ---------------------------------------------------------------------------

#[test]
fn wal_creates_a_wal_and_shm_sidecar_beside_the_database() {
    let dir = temp_dir();
    let path = dir.path().join("zuno.db");
    let pool = Pool::open(&DbLocation::File(path.clone())).expect("open pool");
    let connection = pool.get().expect("checkout");
    connection
        .execute_batch("CREATE TABLE t (id integer primary key)")
        .expect("create a table so the write-ahead log is not empty");

    let mut found = entries(dir.path());
    found.sort();
    assert_eq!(
        found,
        ["zuno.db", "zuno.db-shm", "zuno.db-wal"],
        "unexpected on-disk file set"
    );

    let sidecars = pool.sidecar_files();
    assert_eq!(
        sidecars,
        [
            dir.path().join("zuno.db-wal"),
            dir.path().join("zuno.db-shm"),
        ]
    );
    for sidecar in &sidecars {
        assert!(sidecar.exists(), "{} is missing", sidecar.display());
    }
    assert_eq!(open::sidecar_files(&path), sidecars);
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read the directory")
        .map(|entry| {
            entry
                .expect("read a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}
