use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use zuno_engine::status::SessionRunRegistry;
use zuno_session_control::repair::{
    SessionRepairAction, SessionRepairRequest, SessionRepairService,
};

use crate::command::SessionRepairArgs;

pub(super) fn run(args: &SessionRepairArgs) -> Result<(), String> {
    if args.apply && (args.dry_run || args.expected_revision.is_none_or(|revision| revision < 1))
        || !args.apply && args.expected_revision.is_some()
    {
        return Err("apply requires --expected-revision N and conflicts with --dry-run".to_owned());
    }
    let location = zuno_paths::db_path();
    let path = location
        .as_path()
        .ok_or("session repair requires an existing file database")?;
    let mut connection = open_repair_connection(path, args.apply)?;
    let runs = SessionRunRegistry::new();
    let mut request = SessionRepairRequest::new(&args.session_id, &args.input);
    if args.apply {
        request.action = SessionRepairAction::Apply {
            expected_revision: args.expected_revision.ok_or("missing execution revision")?,
        };
    }
    let report = SessionRepairService::new(&mut connection, &runs)
        .repair(request)
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn open_repair_connection(path: &Path, apply: bool) -> Result<Connection, String> {
    // No CREATE, migration, journal-mode change, checkpoint, or Pool opener.
    let flags = if apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(path, flags)
        .map_err(|_| "cannot open the existing database for session repair".to_owned())?;
    connection
        .busy_timeout(Duration::ZERO)
        .map_err(|_| "cannot configure bounded repair locking".to_owned())?;
    if apply {
        // Set BEFORE first database access. In WAL this requires sole database
        // ownership, including idle connections held by another ACP/TUI host.
        // A fresh registry alone could not prove those hosts inactive.
        connection
            .pragma_update(None, "locking_mode", "EXCLUSIVE")
            .map_err(|_| "cannot acquire exclusive repair access".to_owned())?;
    } else {
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|_| "cannot configure read-only repair inspection".to_owned())?;
    }
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|_| "cannot configure repair foreign-key validation".to_owned())?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_inspection_does_not_create_a_missing_database() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("absent.db");
        assert!(open_repair_connection(&path, false).is_err());
        assert!(open_repair_connection(&path, true).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn repair_inspection_opens_read_only_and_cannot_write() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("fixture.db");
        let original = Connection::open(&path).expect("create fixture");
        original
            .execute_batch("CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES (1);")
            .expect("fixture");
        drop(original);
        let before = std::fs::read(&path).expect("database bytes");
        let connection = open_repair_connection(&path, false).expect("inspect connection");
        assert_eq!(
            connection
                .query_row("SELECT value FROM sentinel", [], |row| row.get::<_, i64>(0))
                .expect("read"),
            1
        );
        assert!(
            connection
                .execute("UPDATE sentinel SET value=2", [])
                .is_err()
        );
        drop(connection);
        assert_eq!(std::fs::read(&path).expect("database bytes"), before);
    }

    #[test]
    fn repair_apply_rejects_an_idle_wal_host_and_a_writer() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("fixture.db");
        let host = Connection::open(&path).expect("host");
        host.pragma_update(None, "journal_mode", "WAL")
            .expect("WAL fixture");
        host.execute_batch("CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES (1);")
            .expect("fixture");
        for writer in [false, true] {
            if writer {
                host.execute_batch("BEGIN IMMEDIATE").expect("writer");
            }
            let mut repair =
                open_repair_connection(&path, true).expect("open without touching database");
            let error = repair
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .expect_err("an idle live WAL connection also excludes standalone repair");
            assert!(zuno_db::open::is_busy(&error));
            if writer {
                host.execute_batch("ROLLBACK").expect("release writer");
            }
        }
        drop(host);
        let mut repair = open_repair_connection(&path, true).expect("offline repair");
        let tx = repair
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("sole owner can acquire repair writer");
        tx.rollback().expect("no fixture mutation");
    }
}
