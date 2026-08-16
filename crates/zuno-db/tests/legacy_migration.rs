//! Opening a database that predates the `migration` table.
//!
//! # The defect these tests pin
//!
//! `apply` used to implement two paths: empty → create the current schema, has
//! `session` → read the journal. A real install from before the journal existed
//! takes the second path and has nothing to read, so the first statement issued
//! against the user's own history was `SELECT id FROM migration` on a table that
//! does not exist. Measured on the machine this was written on:
//! `~/.local/share/opencode/opencode.db.bak.20260408` is 2.6 GB, has 14 tables
//! including `session` with **2,345 sessions**, has no `migration` table, and has a
//! `__drizzle_migrations` journal with 10 rows. The released TypeScript binary
//! opens it. This one refused, with `DbError::Migration { version: 38 }`.
//!
//! # Why the fixture is built rather than trimmed
//!
//! Copying 2.6 GB per test is not viable, so the fixture is reconstructed:
//! [`LEGACY_SCHEMA_SQL`] is the real backup's own `.schema` output, committed
//! verbatim, plus synthetic rows and the real backup's ten `__drizzle_migrations`
//! names in the order that table holds them. Nothing is pre-migrated by the
//! TypeScript binary — that would hide the defect by handing the Rust code a
//! database it never had to move forward.
//!
//! The real backup is still exercised, once, by
//! [`legacy_the_users_real_pre_migration_backup_opens_and_keeps_its_sessions`],
//! which is opt-in because it copies 2.6 GB and prints why it skipped otherwise.

use std::path::{Path, PathBuf};
use zuno_db::{Connection, migration, open};

/// The real backup's schema, captured with `sqlite3 -readonly … .schema`.
///
/// Embedded with `include_str!` rather than read at run time: a baked-in
/// `CARGO_MANIFEST_DIR` path breaks when a shared `CARGO_TARGET_DIR` outlives the
/// worktree it was compiled in, and embedding the bytes has no such failure mode.
const LEGACY_SCHEMA_SQL: &str = include_str!("fixtures/legacy_pre_migration.sql");

/// The ten names in the real backup's `__drizzle_migrations`, in `rowid` order.
///
/// Note that this is **not** chronological, and it is not
/// `migration::MIGRATION_IDS[..10]` either: rows 6 and 7 hold
/// `add_workspace_fields` before `blue_harpoon`, the reverse of the generated
/// order. Seeding therefore has to copy what the old journal says rather than
/// assume it agrees with the current chain's prefix.
const DRIZZLE_JOURNAL_NAMES: [&str; 10] = [
    "20260127222353_familiar_lady_ursula",
    "20260211171708_add_project_commands",
    "20260213144116_wakeful_the_professor",
    "20260225215848_workspace",
    "20260227213759_add_session_workspace_id",
    "20260303231226_add_workspace_fields",
    "20260228203230_blue_harpoon",
    "20260309230000_move_org_to_state",
    "20260312043431_session_message_cursor",
    "20260323234822_events",
];

/// Where the real pre-`migration` backup lives on the machine this was measured on.
const REAL_LEGACY_BACKUP: &str = "/config/.local/share/opencode/opencode.db.bak.20260408";

/// Opt-in switch for the real-backup test, which copies a multi-gigabyte file.
const REAL_LEGACY_ENV: &str = "OPENCODE_LEGACY_DB";

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

/// A database shaped like a real install from before the `migration` table.
///
/// `drizzle` decides whether Drizzle's journal is present. Without it the database
/// has `session` and no journal of any kind, which is the second case upstream's
/// `applyOnly` has to cope with.
fn legacy_database(path: &Path, drizzle: bool) {
    let connection = open::open_at(path).expect("open legacy fixture");
    connection
        .execute_batch(LEGACY_SCHEMA_SQL)
        .expect("apply the real backup's captured schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
               VALUES ('project-legacy', '/legacy/worktree', 100, 100, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
               VALUES ('session-legacy-1', 'project-legacy', 'first', '/legacy/worktree', \
                       'a session from before the journal', '1.0.0', 100, 101);
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
               VALUES ('session-legacy-2', 'project-legacy', 'second', '/legacy/worktree', \
                       'another session from before the journal', '1.0.0', 200, 201);
             INSERT INTO message (id, session_id, time_created, time_updated, data) \
               VALUES ('message-legacy-1', 'session-legacy-1', 100, 100, \
                       '{\"role\":\"assistant\",\"cost\":0.5,\"tokens\":{\"input\":11,\"output\":22}}');
             INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
               VALUES ('part-legacy-1', 'message-legacy-1', 'session-legacy-1', 100, 100, '{}');
             INSERT INTO todo \
               (session_id, content, status, priority, position, time_created, time_updated) \
               VALUES ('session-legacy-1', 'an old todo', 'pending', 'high', 0, 100, 100);
             INSERT INTO account (id, email, url, access_token, refresh_token, \
                                  time_created, time_updated) \
               VALUES ('account-legacy', 'user@example.invalid', 'https://example.invalid', \
                       'access', 'refresh', 100, 100);",
        )
        .expect("seed legacy rows");

    if drizzle {
        let mut statement = connection
            .prepare(
                "INSERT INTO __drizzle_migrations (hash, created_at, name, applied_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .expect("prepare Drizzle journal insert");
        for (index, name) in DRIZZLE_JOURNAL_NAMES.iter().enumerate() {
            statement
                .execute(rusqlite::params![
                    format!("hash-{index}"),
                    100 + index as i64,
                    name,
                    "2026-01-01T00:00:00Z"
                ])
                .expect("record a Drizzle migration");
        }
    } else {
        connection
            .execute_batch("DROP TABLE `__drizzle_migrations`;")
            .expect("remove Drizzle's journal");
    }
}

fn journal_ids_in_order(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT id FROM migration ORDER BY rowid")
        .expect("prepare journal query");
    statement
        .query_map([], |row| row.get(0))
        .expect("query journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("read journal")
}

fn table_names(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table inventory");
    statement
        .query_map([], |row| row.get(0))
        .expect("query table inventory")
        .collect::<Result<Vec<_>, _>>()
        .expect("read table inventory")
}

fn count(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

fn has_table(connection: &Connection, name: &str) -> bool {
    count(
        connection,
        &format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{name}'"),
    ) > 0
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

fn sorted_migration_ids() -> Vec<String> {
    sorted(
        migration::MIGRATION_IDS
            .iter()
            .map(|id| (*id).to_owned())
            .collect(),
    )
}

#[test]
fn legacy_database_with_a_drizzle_journal_migrates_to_all_38_ids() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    legacy_database(&path, true);

    let mut connection = open::open_at(&path).expect("reopen legacy fixture");
    let before_tables = table_names(&connection);
    assert!(
        !before_tables.iter().any(|table| table == "migration"),
        "the fixture must start with no journal: {before_tables:?}"
    );
    assert!(before_tables.iter().any(|table| table == "session"));

    let applied = migration::apply_only(&mut connection).expect("migrate the legacy database");

    let ids = journal_ids_in_order(&connection);
    eprintln!(
        "legacy migrate: tables {} -> {}, journal 0 -> {}, seeded {}, executed {}",
        before_tables.len(),
        table_names(&connection).len(),
        ids.len(),
        applied.seeded.len(),
        applied.executed.len(),
    );

    assert_eq!(ids.len(), 38);
    assert_eq!(sorted(ids.clone()), sorted_migration_ids());
    assert_eq!(&ids[..10], &DRIZZLE_JOURNAL_NAMES[..]);

    assert_eq!(applied.executed.len(), 28);
    for recorded in DRIZZLE_JOURNAL_NAMES {
        assert!(
            !applied.executed.iter().any(|id| id == recorded),
            "replayed a migration the seeded journal already recorded: {recorded}"
        );
    }
    assert_eq!(
        applied.executed,
        migration::MIGRATION_IDS[10..]
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>()
    );

    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 2);
    assert_eq!(count(&connection, "SELECT count(*) FROM project"), 1);
    assert_eq!(count(&connection, "SELECT count(*) FROM message"), 1);
    assert_eq!(count(&connection, "SELECT count(*) FROM account"), 1);
    let title: String = connection
        .query_row(
            "SELECT title FROM session WHERE id = 'session-legacy-1'",
            [],
            |row| row.get(0),
        )
        .expect("read a pre-existing session");
    assert_eq!(title, "a session from before the journal");
}

#[test]
fn legacy_migration_seeds_exactly_the_names_drizzle_recorded() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    legacy_database(&path, true);
    let mut connection = open::open_at(&path).expect("reopen legacy fixture");

    let applied = migration::apply_only(&mut connection).expect("migrate the legacy database");

    eprintln!("seeded ids: {:?}", applied.seeded);
    assert_eq!(applied.seeded, DRIZZLE_JOURNAL_NAMES);

    let drizzle: Vec<String> = {
        let mut statement = connection
            .prepare("SELECT name FROM __drizzle_migrations ORDER BY rowid")
            .expect("prepare Drizzle query");
        statement
            .query_map([], |row| row.get(0))
            .expect("query Drizzle journal")
            .collect::<Result<Vec<_>, _>>()
            .expect("read Drizzle journal")
    };
    assert_eq!(applied.seeded, drizzle);
}

#[test]
fn legacy_migration_reaches_the_same_schema_the_current_creator_does() {
    let dir = temp_dir();
    let legacy_path = dir.path().join("legacy").join("opencode.db");
    legacy_database(&legacy_path, true);
    let mut legacy = open::open_at(&legacy_path).expect("reopen legacy fixture");
    migration::apply(&mut legacy).expect("migrate the legacy database");

    let fresh_path = dir.path().join("fresh").join("opencode.db");
    let mut fresh = open::open_at(&fresh_path).expect("open a fresh database");
    migration::apply(&mut fresh).expect("create the current schema");

    // Drizzle's journal is deliberately left behind — the real installed
    // `opencode.db`, which the TypeScript binary migrated itself, still has it.
    // Dropping it would be a change upstream does not make.
    let migrated = table_names(&legacy);
    assert!(
        migrated.iter().any(|table| table == "__drizzle_migrations"),
        "Drizzle's journal must survive, as it does in the real installed database"
    );
    let created = table_names(&fresh);
    eprintln!(
        "migrated tables: {} (including Drizzle's journal); created tables: {}",
        migrated.len(),
        created.len()
    );
    assert_eq!(created.len(), 20, "19 schema tables plus migration");
    let migrated_without_drizzle: Vec<String> = migrated
        .iter()
        .filter(|table| *table != "__drizzle_migrations")
        .cloned()
        .collect();
    assert_eq!(migrated_without_drizzle, created);

    for table in &created {
        let columns = |connection: &Connection| -> Vec<String> {
            let mut statement = connection
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY name")
                .expect("prepare column query");
            statement
                .query_map([table], |row| row.get(0))
                .expect("query columns")
                .collect::<Result<Vec<_>, _>>()
                .expect("read columns")
        };
        assert_eq!(columns(&legacy), columns(&fresh), "columns differ: {table}");
    }

    let indexes = |connection: &Connection| -> Vec<String> {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("prepare index query");
        statement
            .query_map([], |row| row.get(0))
            .expect("query indexes")
            .collect::<Result<Vec<_>, _>>()
            .expect("read indexes")
    };
    assert_eq!(indexes(&legacy), indexes(&fresh));
}

#[test]
fn legacy_migration_is_idempotent_and_the_second_open_runs_nothing() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    legacy_database(&path, true);

    let mut connection = open::open_at(&path).expect("reopen legacy fixture");
    let first = migration::apply_only(&mut connection).expect("first migration");
    let second = migration::apply_only(&mut connection).expect("second migration");
    eprintln!(
        "first run executed {}, second run executed {}",
        first.executed.len(),
        second.executed.len()
    );
    assert_eq!(first.executed.len(), 28);
    assert!(second.executed.is_empty());
    assert!(second.seeded.is_empty());
    assert_eq!(journal_ids_in_order(&connection).len(), 38);
}

#[test]
fn a_session_database_with_neither_journal_creates_one_and_refuses_to_touch_the_data() {
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    legacy_database(&path, false);

    let mut connection = open::open_at(&path).expect("reopen fixture");
    assert!(!has_table(&connection, "migration"));
    assert!(!has_table(&connection, "__drizzle_migrations"));

    let error = migration::apply(&mut connection).expect_err("upstream cannot migrate this either");
    let message = error.to_string();
    let cause = format!("{:?}", std::error::Error::source(&error));
    eprintln!("neither-journal outcome: {message} / cause {cause}");

    assert!(
        !cause.contains("no such table: migration"),
        "still failing on the journal read instead of creating it: {cause}"
    );
    assert!(
        cause.contains("already exists"),
        "expected the first migration's DDL to be what refuses: {cause}"
    );

    assert!(
        has_table(&connection, "migration"),
        "the journal must be created unconditionally, even when the chain then fails"
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM migration"), 0);
    assert_eq!(count(&connection, "SELECT count(*) FROM session"), 2);
    assert_eq!(count(&connection, "SELECT count(*) FROM project"), 1);
    assert_eq!(count(&connection, "SELECT count(*) FROM message"), 1);
    let title: String = connection
        .query_row(
            "SELECT title FROM session WHERE id = 'session-legacy-1'",
            [],
            |row| row.get(0),
        )
        .expect("the session survived");
    assert_eq!(title, "a session from before the journal");
}

fn real_legacy_backup() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(REAL_LEGACY_ENV) {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
        eprintln!(
            "skipping the real pre-migration backup: {REAL_LEGACY_ENV} points at {}, \
             which is not a file",
            path.display()
        );
        return None;
    }
    eprintln!(
        "skipping the real pre-migration backup: set {REAL_LEGACY_ENV} to a copyable \
         pre-`migration` database to run it. The measured one is {REAL_LEGACY_BACKUP} \
         ({}). It is opt-in because the test copies the whole file, which is 2.6 GB \
         on this machine, and the reduced fixture above covers the same code path.",
        if Path::new(REAL_LEGACY_BACKUP).is_file() {
            "present"
        } else {
            "absent"
        }
    );
    None
}

#[test]
fn legacy_the_users_real_pre_migration_backup_opens_and_keeps_its_sessions() {
    let Some(source) = real_legacy_backup() else {
        return;
    };
    let dir = temp_dir();
    let path = dir.path().join("opencode.db");
    let bytes = std::fs::copy(&source, &path).expect("copy the real backup");
    eprintln!(
        "copied {} ({bytes} bytes) to {}",
        source.display(),
        path.display()
    );

    let mut connection = open::open_at(&path).expect("open the real backup copy");
    let before_sessions = count(&connection, "SELECT count(*) FROM session");
    let before_messages = count(&connection, "SELECT count(*) FROM message");
    assert!(!has_table(&connection, "migration"));
    assert!(has_table(&connection, "__drizzle_migrations"));

    let applied = migration::apply_only(&mut connection).expect("migrate the real backup");
    let ids = journal_ids_in_order(&connection);
    eprintln!(
        "real backup: sessions {before_sessions} -> {}, messages {before_messages} -> {}, \
         journal 0 -> {}, seeded {:?}, executed {}",
        count(&connection, "SELECT count(*) FROM session"),
        count(&connection, "SELECT count(*) FROM message"),
        ids.len(),
        applied.seeded,
        applied.executed.len(),
    );

    assert_eq!(ids.len(), 38);
    assert_eq!(sorted(ids), sorted_migration_ids());
    assert_eq!(applied.executed.len(), 28);
    assert_eq!(
        count(&connection, "SELECT count(*) FROM session"),
        before_sessions,
        "migrating the real backup lost sessions"
    );
    assert_eq!(
        count(&connection, "SELECT count(*) FROM message"),
        before_messages,
        "migrating the real backup lost messages"
    );
    assert!(before_sessions > 0, "the backup had no sessions to keep");
}
