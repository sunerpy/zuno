//! Independently versioned preview runtime overlay on the stable SQLite schema.

use super::*;

pub const CURRENT_PREVIEW_FORMAT: u32 = 1;

const MARKER: &str = "zuno_preview_schema";
const RUNTIME_TABLES: &[&str] = &[
    "runtime_session",
    "runtime_job",
    "runtime_attempt",
    "runtime_owner_schedule",
];

fn present(tables: &[String], name: &str) -> bool {
    tables.iter().any(|table| table == name)
}

fn has_runtime(tables: &[String]) -> bool {
    RUNTIME_TABLES.iter().any(|table| present(tables, table))
}

fn has_overlay(tables: &[String]) -> bool {
    present(tables, "session_ownership") || has_runtime(tables)
}

/// Dispatch historical preview layouts before interpreting the stable core
/// format. Main and the unpublished preview used different layouts for 13/14.
pub(super) fn dispatch(
    connection: &mut Connection,
    tables: &[String],
    observed: Option<u32>,
) -> Result<Option<Dispatch>, DbError> {
    if present(tables, MARKER) {
        validate(connection)?;
        return Ok(None);
    }
    if has_overlay(tables) {
        return match observed {
            Some(13 | 14) => migrate_legacy(connection, observed.unwrap()).map(Some),
            _ => Err(failure(std::io::Error::other(
                "unmarked preview objects do not match a supported legacy lineage",
            ))),
        };
    }
    if observed == Some(CURRENT_FORMAT) {
        return install_current(connection).map(Some);
    }
    Ok(None)
}

pub(super) fn validate(connection: &Connection) -> Result<(), DbError> {
    validate_sql_objects(connection, &[MARKER])?;
    let marker: Option<(u32, String)> = connection
        .query_row(
            "SELECT format,channel FROM zuno_preview_schema WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(map_error)?;
    if marker != Some((CURRENT_PREVIEW_FORMAT, "enterprise-preview".to_owned())) {
        return Err(failure(std::io::Error::other(
            "unsupported or missing preview runtime schema marker",
        )));
    }
    validate_session_ownership_shape(connection)?;
    validate_runtime_jobs_shape(connection)
}

/// Called only within a transaction whose old core/lineage has been validated.
pub(super) fn install(transaction: &Transaction<'_>) -> Result<(), DbError> {
    let mut tables = transaction_table_names(transaction)?;
    if present(&tables, "session_ownership") {
        validate_session_ownership_shape(transaction)?;
    } else {
        schema::up_session_ownership(transaction)?;
    }
    if has_runtime(&tables) {
        validate_runtime_jobs_shape(transaction)?;
    } else {
        schema::up_runtime_jobs(transaction)?;
    }
    tables = transaction_table_names(transaction)?;
    if !present(&tables, MARKER) {
        transaction
            .execute_batch(schema::PREVIEW_SCHEMA_SQL)
            .map_err(map_error)?;
    }
    validate_sql_objects(transaction, &[MARKER])?;
    let existing: Option<u32> = transaction
        .query_row(
            "SELECT format FROM zuno_preview_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_error)?;
    if existing.is_none() {
        transaction.execute(
            "INSERT INTO zuno_preview_schema(singleton,format,channel) VALUES(1,?1,'enterprise-preview')",
            [CURRENT_PREVIEW_FORMAT],
        ).map_err(map_error)?;
    }
    validate(transaction)
}

fn install_current(connection: &mut Connection) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(CURRENT_FORMAT) || present(&tables, MARKER) {
        return Ok(Dispatch::Moved { observed });
    }
    if has_overlay(&tables) {
        return Err(failure(std::io::Error::other(
            "preview lineage changed during migration",
        )));
    }
    validate_base_current(&transaction, &tables)?;
    install(&transaction)?;
    validate_current(&transaction, &transaction_table_names(&transaction)?)?;
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

fn validate_legacy(connection: &Connection, tables: &[String], format: u32) -> Result<(), DbError> {
    validate_format_twelve(connection, tables)?;
    validate_session_ownership_shape(connection)?;
    if [
        "question_interaction",
        "question_action_receipt",
        "session_input_receipt",
        "session_context_usage",
    ]
    .iter()
    .any(|table| present(tables, table))
        || column_names(connection, "session_execution_state")?
            .iter()
            .any(|column| column == "scheduling")
    {
        return Err(failure(std::io::Error::other(
            "unmarked database mixes stable and preview schema lineages",
        )));
    }
    match format {
        13 if !has_runtime(tables) => Ok(()),
        14 => validate_runtime_jobs_shape(connection),
        _ => Err(failure(std::io::Error::other(
            "unsupported legacy preview shape",
        ))),
    }
}

fn migrate_legacy(connection: &mut Connection, expected: u32) -> Result<Dispatch, DbError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let tables = transaction_table_names(&transaction)?;
    let observed = observed_format(&transaction, &tables)?;
    if observed != Some(expected) || present(&tables, MARKER) {
        return Ok(Dispatch::Moved { observed });
    }
    validate_legacy(&transaction, &tables, expected)?;
    // The old preview was based on core 12. Apply the real core 13/14 steps;
    // existing preview ownership and Job rows remain in their own overlay.
    add_questions(&transaction)?;
    validate_current(&transaction, &transaction_table_names(&transaction)?)?;
    let changed = transaction
        .execute(
            "UPDATE zuno_schema SET format=?1 WHERE singleton=1 AND format=?2",
            params![CURRENT_FORMAT, expected],
        )
        .map_err(map_error)?;
    if changed != 1 {
        return Err(failure(std::io::Error::other(
            "legacy preview marker changed during migration",
        )));
    }
    transaction.commit().map_err(map_error)?;
    Ok(Dispatch::Settled)
}

fn validate_session_ownership_shape(connection: &Connection) -> Result<(), DbError> {
    validate_sql_objects(
        connection,
        &[
            "session_ownership",
            "session_ownership_insert",
            "session_ownership_principal_idx",
        ],
    )
}

fn validate_runtime_jobs_shape(connection: &Connection) -> Result<(), DbError> {
    validate_sql_objects(
        connection,
        &[
            "agent_job",
            "runtime_session",
            "runtime_job",
            "runtime_attempt",
            "runtime_owner_schedule",
            "runtime_session_insert",
            "runtime_input_insert",
            "runtime_input_update",
            "runtime_session_lease_deadline_idx",
            "runtime_job_ready_idx",
            "runtime_attempt_worker_state_idx",
        ],
    )
}
