//! Format-15 companion ledgers. No old Goal table is required or altered, and no
//! cycle identity, observation, failure streak, or recovery authority is inferred.

use super::{validate_sql_objects, validate_table_columns};
use rusqlite::Connection;
use zuno_error::DbError;

pub(super) fn validate_shape(connection: &Connection) -> Result<(), DbError> {
    validate_table_columns(
        connection,
        "session_work_cycle",
        &[
            ("session_id", "TEXT", true, 1),
            ("cycle_id", "TEXT", true, 2),
            ("anchor_message_id", "TEXT", false, 0),
            ("data", "TEXT", true, 0),
            ("time_created", "INTEGER", true, 0),
            ("time_updated", "INTEGER", true, 0),
        ],
    )?;
    validate_table_columns(
        connection,
        "goal_turn_observation",
        &[
            ("session_id", "TEXT", true, 1),
            ("goal_id", "TEXT", true, 2),
            ("cycle_id", "TEXT", true, 3),
            ("turn_id", "TEXT", true, 4),
            ("signal", "TEXT", true, 0),
            ("time_created", "INTEGER", true, 0),
        ],
    )?;
    validate_table_columns(
        connection,
        "goal_turn_audit",
        &[
            ("session_id", "TEXT", true, 1),
            ("goal_id", "TEXT", true, 2),
            ("cycle_id", "TEXT", true, 3),
            ("turn_id", "TEXT", true, 4),
            ("audit", "TEXT", true, 0),
            ("time_recorded", "INTEGER", true, 0),
        ],
    )?;
    validate_table_columns(
        connection,
        "goal_cycle_failure",
        &[
            ("session_id", "TEXT", true, 1),
            ("goal_id", "TEXT", true, 0),
            ("cycle_id", "TEXT", true, 0),
            ("active_turn_id", "TEXT", true, 0),
            ("signal", "TEXT", false, 0),
            ("consecutive_turns", "INTEGER", true, 0),
        ],
    )?;
    validate_sql_objects(
        connection,
        &[
            "session_work_cycle",
            "session_work_cycle_updated_idx",
            "goal_turn_observation",
            "goal_turn_audit",
            "goal_cycle_failure",
        ],
    )
}
