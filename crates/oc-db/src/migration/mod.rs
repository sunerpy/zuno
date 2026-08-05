//! Atomic current-schema creation and TypeScript migration-journal parity.

use crate::{open, schema};
use oc_error::DbError;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// The migration ids loaded by `migration.gen.ts`, in its generated order.
pub const MIGRATION_IDS: [&str; 38] = [
    "20260127222353_familiar_lady_ursula",
    "20260211171708_add_project_commands",
    "20260213144116_wakeful_the_professor",
    "20260225215848_workspace",
    "20260227213759_add_session_workspace_id",
    "20260228203230_blue_harpoon",
    "20260303231226_add_workspace_fields",
    "20260309230000_move_org_to_state",
    "20260312043431_session_message_cursor",
    "20260323234822_events",
    "20260410174513_workspace-name",
    "20260413175956_chief_energizer",
    "20260423070820_add_icon_url_override",
    "20260427172553_slow_nightmare",
    "20260428004200_add_session_path",
    "20260501142318_next_venus",
    "20260504145000_add_sync_owner",
    "20260507164347_add_workspace_time",
    "20260510033149_session_usage",
    "20260511000411_data_migration_state",
    "20260511173437_session-metadata",
    "20260601010001_normalize_storage_paths",
    "20260601202201_amazing_prowler",
    "20260602002951_lowly_union_jack",
    "20260602182828_add_project_directories",
    "20260603001617_session_message_projection_indexes",
    "20260603040000_session_message_projection_order",
    "20260603141458_session_input_inbox",
    "20260603160727_jittery_ezekiel_stane",
    "20260604172448_event_sourced_session_input",
    "20260605003541_add_session_context_snapshot",
    "20260605042240_add_context_epoch_agent",
    "20260611035744_credential",
    "20260611192811_lush_chimera",
    "20260612174303_project_dir_strategy",
    "20260622142730_simplify_session_context_epoch",
    "20260622170816_reset_v2_session_state",
    "20260622202450_simplify_session_input",
];

/// Numeric version attached to failures from this generated migration set.
pub const CURRENT_VERSION: u32 = MIGRATION_IDS.len() as u32;

const JOURNAL_SQL: &str =
    "CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)";

/// Create a current database atomically, or verify that an existing session
/// database already records every migration this binary knows.
///
/// Existing databases are never marked current speculatively. If one lacks a
/// known id, this function fails rather than hiding an older schema behind a
/// newly seeded journal.
///
/// # Errors
///
/// [`DbError::Migration`] for an unknown non-empty database, missing journal
/// entries, time conversion failures, or SQLite DDL/DML failures.
/// [`DbError::Busy`] if another writer holds the database lock.
pub fn apply(connection: &mut Connection) -> Result<(), DbError> {
    let tables = table_names(connection)?;
    if tables.is_empty() {
        return create_current(connection);
    }
    if tables.iter().any(|table| table == "session") {
        return verify_journal(connection);
    }
    Err(failure(std::io::Error::other(
        "database is not empty and has no session table",
    )))
}

fn create_current(connection: &mut Connection) -> Result<(), DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    schema::up(&transaction)?;
    create_journal(&transaction)?;
    seed_journal(&transaction)?;
    transaction.commit().map_err(map_error)
}

fn create_journal(transaction: &Transaction<'_>) -> Result<(), DbError> {
    transaction.execute(JOURNAL_SQL, []).map_err(map_error)?;
    Ok(())
}

fn seed_journal(transaction: &Transaction<'_>) -> Result<(), DbError> {
    let time_completed = unix_milliseconds()?;
    let mut statement = transaction
        .prepare("INSERT INTO migration (id, time_completed) VALUES (?1, ?2)")
        .map_err(map_error)?;
    for id in MIGRATION_IDS {
        statement
            .execute(params![id, time_completed])
            .map_err(map_error)?;
    }
    Ok(())
}

fn verify_journal(connection: &Connection) -> Result<(), DbError> {
    let completed = journal_ids(connection)?;
    let missing: Vec<_> = MIGRATION_IDS
        .iter()
        .copied()
        .filter(|id| !completed.contains(*id))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(failure(std::io::Error::other(format!(
        "migration journal is missing: {}",
        missing.join(", ")
    ))))
}

fn table_names(connection: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)
}

fn journal_ids(connection: &Connection) -> Result<HashSet<String>, DbError> {
    let mut statement = connection
        .prepare("SELECT id FROM migration")
        .map_err(map_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(map_error)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(map_error)
}

fn unix_milliseconds() -> Result<i64, DbError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(failure)?;
    i64::try_from(elapsed.as_millis()).map_err(failure)
}

pub(crate) fn map_error(error: rusqlite::Error) -> DbError {
    if open::is_busy(&error) {
        return open::map_error(error);
    }
    failure(error)
}

fn failure(source: impl std::error::Error + Send + Sync + 'static) -> DbError {
    DbError::Migration {
        version: CURRENT_VERSION,
        source: Box::new(source),
    }
}
