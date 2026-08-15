//! Opening `zuno.db`: the pragmas, the `ZUNO_DB` forms, WAL behaviour
//! under concurrent writers, and proof that `foreign_keys = ON` is in force.

use oc_db::open;
use oc_db::{Pool, TransactionBehavior};
use oc_paths::env::{HOME, XDG_DATA_HOME, ZUNO_DB};
use oc_paths::{DbLocation, Env, Layout};
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn layout(pairs: &[(&str, &str)]) -> Layout {
    Layout::resolve_with(&Env::from_pairs(pairs.iter().copied()), None)
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create a temporary directory")
}

#[test]
fn default_database_filename_hard_cut_covers_old_new_both_and_neither() {
    for (old_exists, new_exists) in [(true, false), (false, true), (true, true), (false, false)] {
        let dir = temp_dir();
        let old_path = dir.path().join("opencode.db");
        let new_path = dir.path().join("zuno.db");
        if old_exists {
            std::fs::write(&old_path, []).expect("create legacy filename");
        }
        if new_exists {
            std::fs::write(&new_path, []).expect("create current filename");
        }

        let location = DbLocation::File(new_path.clone());
        let result = open::open_default_location(&location);
        if old_exists && !new_exists {
            let error = result.expect_err("old-only must require an explicit filename migration");
            let message = error.to_string();
            assert!(
                message.contains(&old_path.display().to_string()),
                "{message}"
            );
            assert!(
                message.contains(&new_path.display().to_string()),
                "{message}"
            );
            assert!(
                old_path.is_file(),
                "the diagnostic must not move the old file"
            );
            assert!(
                !new_path.exists(),
                "the diagnostic must not create the new file"
            );
        } else {
            drop(result.expect("new-only, both, and neither open the Zuno filename"));
            assert!(
                new_path.is_file(),
                "the Zuno filename must be authoritative"
            );
        }
    }

    let dir = temp_dir();
    let explicit_path = dir.path().join("zuno.db");
    let legacy_path = dir.path().join("opencode.db");
    std::fs::write(&legacy_path, []).expect("create unrelated legacy basename");
    drop(open::open_at(&explicit_path).expect("an explicit path remains authoritative"));
    assert!(explicit_path.is_file());
}

// ---------------------------------------------------------------------------
// The four pragmas, read back from the connection.
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_file_database_reports_all_four_pragmas_active() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
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

    assert_eq!(journal_mode, oc_db::JOURNAL_MODE_WAL);
    assert_eq!(synchronous, oc_db::SYNCHRONOUS_NORMAL);
    assert_eq!(busy_timeout, oc_db::BUSY_TIMEOUT_MS);
    assert_eq!(cache_size, oc_db::CACHE_SIZE_KIB);
    assert_eq!(foreign_keys, oc_db::FOREIGN_KEYS_ON);
}

/// `journal_mode` is recorded in the database file and survives a close;
/// `synchronous` and `cache_size` are per-connection and reset to SQLite's
/// defaults, so a connection nobody configured is durable-but-slower and caches
/// 2 MiB instead of 64. This is why [`Pool`] owns connection creation.
#[test]
fn wal_survives_a_close_but_the_per_connection_pragmas_do_not() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    drop(open::open_at(&path).expect("open a fresh database"));

    let raw = oc_db::Connection::open(&path).expect("reopen without pragmas");
    let journal_mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let synchronous: i64 = raw
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous");
    let cache_size: i64 = raw
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .expect("read cache_size");
    assert_eq!(journal_mode, oc_db::JOURNAL_MODE_WAL);
    assert_ne!(
        synchronous,
        oc_db::SYNCHRONOUS_NORMAL,
        "synchronous must not survive a close"
    );
    assert_ne!(
        cache_size,
        oc_db::CACHE_SIZE_KIB,
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
/// Together these mean two of the oracle's four pragmas would *appear* correct on
/// this driver even if the crate never issued them. The pragmas are issued
/// explicitly anyway, because behaviour must not depend on which SQLite is linked
/// or on a driver's undocumented default.
#[test]
fn the_stack_below_this_crate_already_defaults_two_pragmas_to_the_oracle_values() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    let raw = oc_db::Connection::open(&path).expect("open without pragmas");

    let foreign_keys: i64 = raw
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("read foreign_keys");
    assert_eq!(
        foreign_keys,
        oc_db::FOREIGN_KEYS_ON,
        "the bundled amalgamation no longer defaults foreign keys on; \
         the explicit pragma is now the only thing enforcing cascades"
    );

    let busy_timeout: i64 = raw
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .expect("read busy_timeout");
    assert_eq!(
        busy_timeout,
        oc_db::BUSY_TIMEOUT_MS,
        "rusqlite no longer defaults busy_timeout to the oracle's 5000ms; \
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
    let path = dir.path().join("opencode.db");
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
        assert_eq!(foreign_keys, oc_db::FOREIGN_KEYS_ON);
        assert_eq!(busy_timeout, oc_db::BUSY_TIMEOUT_MS);
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
    assert_eq!(journal_mode, oc_db::JOURNAL_MODE_MEMORY);
    assert_eq!(foreign_keys, oc_db::FOREIGN_KEYS_ON);
    assert!(pool.holds_memory_anchor());
}

#[test]
fn verify_pragmas_rejects_a_connection_whose_foreign_keys_were_turned_off() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
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

fn create_parent_and_child(connection: &oc_db::Connection) {
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
    let path = dir.path().join("opencode.db");
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
    let path = dir.path().join("opencode.db");
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
    let path = dir.path().join("opencode.db");
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
        Duration::from_millis(u64::try_from(oc_db::BUSY_TIMEOUT_MS).expect("a positive timeout"));
    assert!(
        total < busy_timeout,
        "the writers took longer than the busy timeout: {total:?}"
    );
}

/// A reader must not be blocked by an open writer. That is the whole reason the
/// oracle asks for WAL rather than the default rollback journal.
#[test]
fn a_reader_is_not_blocked_by_an_open_writer_under_wal() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
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

    let outcome: Result<(), oc_error::DbError> = pool.transaction(|tx| {
        tx.execute("INSERT INTO t (id) VALUES (1)", [])
            .map_err(open::map_error)?;
        Err(oc_error::DbError::NotFound {
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
// The three `ZUNO_DB` forms, resolved by `oc-paths` and opened here.
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
    let path = dir.path().join("opencode.db");
    let pool = Pool::open(&DbLocation::File(path.clone())).expect("open pool");
    let connection = pool.get().expect("checkout");
    connection
        .execute_batch("CREATE TABLE t (id integer primary key)")
        .expect("create a table so the write-ahead log is not empty");

    let mut found = entries(dir.path());
    found.sort();
    assert_eq!(
        found,
        ["opencode.db", "opencode.db-shm", "opencode.db-wal"],
        "unexpected on-disk file set"
    );

    let sidecars = pool.sidecar_files();
    assert_eq!(
        sidecars,
        [
            dir.path().join("opencode.db-wal"),
            dir.path().join("opencode.db-shm"),
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
