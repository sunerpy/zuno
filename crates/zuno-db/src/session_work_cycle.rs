//! Host-owned work scope. A stopped cycle remains stopped after a new user input.
//!
//! Plans are context until a native control or a model's typed Plan mutation adopts
//! them. Reading a Plan, receiving a callback, and accepting a new input do not
//! grant that ownership. Rows are never reconstructed from assistant prose.

use crate::{Connection, Transaction, map_error};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use zuno_error::DbError;
use zuno_types::execution::SessionScheduling;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleStop {
    pub turn_id: Option<String>,
    pub input_id: Option<String>,
    /// Typed host provenance, not a model claim.
    pub user_cancelled: bool,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWorkCycle {
    pub session_id: String,
    pub cycle_id: String,
    pub anchor_message_id: Option<String>,
    pub goal_id: Option<String>,
    pub plan_id: Option<String>,
    /// Host-updated fence for the actual engine turn. Not report-delivery authority.
    #[serde(default)]
    pub active_turn_id: Option<String>,
    #[serde(default)]
    pub todo_ids: BTreeSet<String>,
    /// Original report cycles transferred only by revision-bound Goal resume.
    /// Their completion receipts retain the original cycle and consumption key.
    #[serde(default)]
    pub resumed_goal_cycles: BTreeSet<String>,
    pub stopped: Option<CycleStop>,
    /// A superseded cycle keeps its wait/pause for inspection, never as a wake
    /// permission for the current cycle.
    pub scheduling: Option<SessionScheduling>,
}

impl SessionWorkCycle {
    #[must_use]
    pub fn accepts_origin(&self, origin: &str) -> bool {
        self.stopped.is_none()
            && (self.cycle_id == origin || self.resumed_goal_cycles.contains(origin))
    }
}

/// Resolve delivery authority without rewriting the original completion receipt.
/// Only explicit native Goal resume can transfer a previous Goal cycle.
pub fn completion_cycle_in(
    connection: &Connection,
    session_id: &str,
    origin_cycle: &str,
) -> Result<Option<String>, DbError> {
    let Some(current) = current_in(connection, session_id)? else {
        return Ok(crate::session_execution::read_in(connection, session_id)?
            .and_then(|state| state.cycle_id)
            .filter(|cycle| cycle == origin_cycle));
    };
    if !current.accepts_origin(origin_cycle) {
        return Ok(None);
    }
    if let Some(goal_id) = &current.goal_id {
        let active = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM goal WHERE session_id=?1 AND goal_id=?2 AND status='active')",
            params![session_id,goal_id], |row| row.get::<_, bool>(0),
        ).map_err(map_error)?;
        if !active {
            return Ok(None);
        }
    }
    if current.cycle_id != origin_cycle {
        let original = read_in(connection, session_id, origin_cycle)?;
        if current.goal_id.is_none()
            || original
                .as_ref()
                .is_none_or(|origin| origin.goal_id != current.goal_id)
        {
            return Ok(None);
        }
    }
    Ok(Some(current.cycle_id))
}

pub fn read_in(
    connection: &Connection,
    session_id: &str,
    cycle_id: &str,
) -> Result<Option<SessionWorkCycle>, DbError> {
    let data: Option<String> = connection
        .query_row(
            "SELECT data FROM session_work_cycle WHERE session_id=?1 AND cycle_id=?2",
            params![session_id, cycle_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_error)?;
    data.map(|data| {
        let cycle: SessionWorkCycle =
            serde_json::from_str(&data).map_err(|error| invalid(error.to_string()))?;
        if cycle.session_id != session_id || cycle.cycle_id != cycle_id {
            return Err(invalid("cycle payload disagrees with its durable key"));
        }
        Ok(cycle)
    })
    .transpose()
}

pub fn current_in(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<SessionWorkCycle>, DbError> {
    let Some(cycle_id) =
        crate::session_execution::read_in(connection, session_id)?.and_then(|state| state.cycle_id)
    else {
        return Ok(None);
    };
    read_in(connection, session_id, &cycle_id)
}

pub fn save_in(
    transaction: &Transaction<'_>,
    cycle: &SessionWorkCycle,
    at_ms: i64,
) -> Result<(), DbError> {
    for id in [&cycle.session_id, &cycle.cycle_id] {
        if id.trim().is_empty() || id.len() > 512 {
            return Err(invalid("cycle identifiers must contain 1..512 bytes"));
        }
    }
    let data = serde_json::to_string(cycle).map_err(|error| invalid(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO session_work_cycle \
             (session_id,cycle_id,anchor_message_id,data,time_created,time_updated) \
             VALUES (?1,?2,?3,?4,?5,?5) ON CONFLICT(session_id,cycle_id) DO UPDATE SET \
             data=excluded.data,time_updated=excluded.time_updated",
            params![
                cycle.session_id,
                cycle.cycle_id,
                cycle.anchor_message_id,
                data,
                at_ms
            ],
        )
        .map_err(map_error)?;
    Ok(())
}

/// Called only with the immutable Attempt's cycle by a typed mutation provider.
/// A late tool result cannot attach old work to a newer user request.
pub fn adopt_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_cycle_id: &str,
    plan_id: Option<&str>,
    todo_ids: impl IntoIterator<Item = String>,
    at_ms: i64,
) -> Result<bool, DbError> {
    let Some(mut cycle) = current_in(transaction, session_id)? else {
        // Administrative and legacy callers may have no execution cycle.
        return Ok(false);
    };
    if cycle.cycle_id != expected_cycle_id || cycle.stopped.is_some() {
        return Ok(false);
    }
    if let Some(plan_id) = plan_id {
        cycle.plan_id = Some(plan_id.to_owned());
    }
    cycle.todo_ids.extend(todo_ids);
    save_in(transaction, &cycle, at_ms)?;
    Ok(true)
}

pub fn is_stopped_in(
    connection: &Connection,
    session_id: &str,
    cycle_id: &str,
) -> Result<bool, DbError> {
    Ok(read_in(connection, session_id, cycle_id)?.is_some_and(|cycle| cycle.stopped.is_some()))
}

fn invalid(detail: impl Into<String>) -> DbError {
    DbError::Conflict {
        table: "session_work_cycle".to_owned(),
        id: "cycle".to_owned(),
        detail: detail.into(),
    }
}
