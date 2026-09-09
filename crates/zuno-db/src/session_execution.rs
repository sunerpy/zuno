//! Durable, revisioned collaboration and continuation state.

use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::execution::{
    CollaborationMode, DraftReviewRiskAcceptance, SessionExecutionPhase, SessionExecutionState,
    TurnExecutionIdentity,
};

#[derive(Clone)]
pub struct SessionExecutionStore {
    pool: Arc<Pool>,
}

impl SessionExecutionStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(&self, session_id: &str) -> Result<Option<SessionExecutionState>, DbError> {
        let connection = self.pool.get()?;
        read_in(&connection, session_id)
    }

    pub fn seed(
        &self,
        session_id: &str,
        mode: CollaborationMode,
        work_identity: Option<TurnExecutionIdentity>,
        at_ms: i64,
    ) -> Result<SessionExecutionState, DbError> {
        self.pool
            .transaction(|transaction| seed_in(transaction, session_id, mode, work_identity, at_ms))
    }

    pub fn update(
        &self,
        expected_revision: i64,
        state: SessionExecutionState,
    ) -> Result<SessionExecutionState, DbError> {
        self.pool
            .transaction(|transaction| update_in(transaction, expected_revision, state))
    }
}

pub fn read_in(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<SessionExecutionState>, DbError> {
    connection
        .query_row(
            "SELECT session_id, revision, mode, work_identity, authorized_plan_id, \
                    authorized_plan_revision, handoff_plan_id, handoff_plan_revision, \
                    draft_review_risk, cycle_id, phase, continuation, time_created, time_updated \
             FROM session_execution_state WHERE session_id = ?1",
            [session_id],
            decode_stored,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode)
        .transpose()
}

pub fn seed_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    mode: CollaborationMode,
    work_identity: Option<TurnExecutionIdentity>,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    if let Some(existing) = read_in(transaction, session_id)? {
        return Ok(existing);
    }
    let state = SessionExecutionState {
        session_id: session_id.to_owned(),
        revision: 1,
        mode,
        work_identity,
        authorized_plan_id: None,
        authorized_plan_revision: None,
        handoff_plan_id: None,
        handoff_plan_revision: None,
        draft_review_risk: None,
        cycle_id: None,
        phase: match mode {
            CollaborationMode::Plan => SessionExecutionPhase::Planning,
            CollaborationMode::Work => SessionExecutionPhase::Idle,
        },
        continuation: None,
        time_created: at_ms,
        time_updated: at_ms,
    };
    insert_in(transaction, &state)?;
    Ok(state)
}

pub fn update_in(
    transaction: &Transaction<'_>,
    expected_revision: i64,
    mut state: SessionExecutionState,
) -> Result<SessionExecutionState, DbError> {
    validate(&state)?;
    let next_revision = expected_revision
        .checked_add(1)
        .ok_or_else(revision_exhausted)?;
    state.revision = next_revision;
    state.time_updated = state.time_updated.max(state.time_created);
    let work_identity = encode_optional(&state.work_identity)?;
    let continuation = encode_optional(&state.continuation)?;
    let draft_review_risk = encode_optional(&state.draft_review_risk)?;
    let changed = transaction
        .execute(
            "UPDATE session_execution_state SET \
               revision = ?1, mode = ?2, work_identity = ?3, authorized_plan_id = ?4, \
               authorized_plan_revision = ?5, handoff_plan_id = ?6, handoff_plan_revision = ?7, \
               draft_review_risk = ?8, cycle_id = ?9, phase = ?10, continuation = ?11, \
               time_updated = ?12 \
             WHERE session_id = ?13 AND revision = ?14",
            params![
                state.revision,
                state.mode.as_str(),
                work_identity,
                state.authorized_plan_id,
                state.authorized_plan_revision,
                state.handoff_plan_id,
                state.handoff_plan_revision,
                draft_review_risk,
                state.cycle_id,
                state.phase.as_str(),
                continuation,
                state.time_updated,
                state.session_id,
                expected_revision,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        let actual = read_in(transaction, &state.session_id)?
            .map(|current| current.revision)
            .unwrap_or_default();
        return Err(DbError::Conflict {
            table: "session_execution_state".to_owned(),
            id: state.session_id.clone(),
            detail: format!("revision conflict: expected {expected_revision}, current {actual}"),
        });
    }
    Ok(state)
}

fn insert_in(transaction: &Transaction<'_>, state: &SessionExecutionState) -> Result<(), DbError> {
    validate(state)?;
    let work_identity = encode_optional(&state.work_identity)?;
    let continuation = encode_optional(&state.continuation)?;
    let draft_review_risk = encode_optional(&state.draft_review_risk)?;
    transaction
        .execute(
            "INSERT INTO session_execution_state \
             (session_id, revision, mode, work_identity, authorized_plan_id, \
              authorized_plan_revision, handoff_plan_id, handoff_plan_revision, draft_review_risk, \
              cycle_id, phase, continuation, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                state.session_id,
                state.revision,
                state.mode.as_str(),
                work_identity,
                state.authorized_plan_id,
                state.authorized_plan_revision,
                state.handoff_plan_id,
                state.handoff_plan_revision,
                draft_review_risk,
                state.cycle_id,
                state.phase.as_str(),
                continuation,
                state.time_created,
                state.time_updated,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

struct StoredExecution {
    session_id: String,
    revision: i64,
    mode: String,
    work_identity: Option<String>,
    authorized_plan_id: Option<String>,
    authorized_plan_revision: Option<i64>,
    handoff_plan_id: Option<String>,
    handoff_plan_revision: Option<i64>,
    draft_review_risk: Option<String>,
    cycle_id: Option<String>,
    phase: String,
    continuation: Option<String>,
    time_created: i64,
    time_updated: i64,
}

fn decode_stored(row: &Row<'_>) -> rusqlite::Result<StoredExecution> {
    Ok(StoredExecution {
        session_id: row.get(0)?,
        revision: row.get(1)?,
        mode: row.get(2)?,
        work_identity: row.get(3)?,
        authorized_plan_id: row.get(4)?,
        authorized_plan_revision: row.get(5)?,
        handoff_plan_id: row.get(6)?,
        handoff_plan_revision: row.get(7)?,
        draft_review_risk: row.get(8)?,
        cycle_id: row.get(9)?,
        phase: row.get(10)?,
        continuation: row.get(11)?,
        time_created: row.get(12)?,
        time_updated: row.get(13)?,
    })
}

fn decode(stored: StoredExecution) -> Result<SessionExecutionState, DbError> {
    Ok(SessionExecutionState {
        session_id: stored.session_id,
        revision: stored.revision,
        mode: CollaborationMode::parse(&stored.mode)
            .ok_or_else(|| query_error(format!("unknown collaboration mode `{}`", stored.mode)))?,
        work_identity: decode_optional(stored.work_identity)?,
        authorized_plan_id: stored.authorized_plan_id,
        authorized_plan_revision: stored.authorized_plan_revision,
        handoff_plan_id: stored.handoff_plan_id,
        handoff_plan_revision: stored.handoff_plan_revision,
        draft_review_risk: decode_optional::<DraftReviewRiskAcceptance>(stored.draft_review_risk)?,
        cycle_id: stored.cycle_id,
        phase: SessionExecutionPhase::parse(&stored.phase)
            .ok_or_else(|| query_error(format!("unknown execution phase `{}`", stored.phase)))?,
        continuation: decode_optional(stored.continuation)?,
        time_created: stored.time_created,
        time_updated: stored.time_updated,
    })
}

fn validate(state: &SessionExecutionState) -> Result<(), DbError> {
    if state.session_id.trim().is_empty() {
        return Err(query_error("session execution state requires a session id"));
    }
    if state.revision < 1 {
        return Err(query_error("session execution revision must be positive"));
    }
    if state.authorized_plan_id.is_some() != state.authorized_plan_revision.is_some() {
        return Err(query_error(
            "authorized plan id and revision must be supplied together",
        ));
    }
    if state.handoff_plan_id.is_some() != state.handoff_plan_revision.is_some() {
        return Err(query_error(
            "handoff plan id and revision must be supplied together",
        ));
    }
    if state
        .authorized_plan_revision
        .is_some_and(|revision| revision < 1)
    {
        return Err(query_error("authorized plan revision must be positive"));
    }
    if state
        .handoff_plan_revision
        .is_some_and(|revision| revision < 1)
    {
        return Err(query_error("handoff plan revision must be positive"));
    }
    if let Some(risk) = &state.draft_review_risk
        && (risk.review_id.trim().is_empty()
            || risk.review_revision < 1
            || risk.reason.trim().is_empty()
            || risk.time_accepted < 0)
    {
        return Err(query_error(
            "draft review risk requires a review id, positive revision, visible reason, and timestamp",
        ));
    }
    Ok(())
}

fn encode_optional<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>, DbError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| query_error(error.to_string()))
}

fn decode_optional<T: serde::de::DeserializeOwned>(
    value: Option<String>,
) -> Result<Option<T>, DbError> {
    value
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|error| query_error(error.to_string()))
}

fn query_error(detail: impl Into<String>) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(detail.into())),
    }
}

fn revision_exhausted() -> DbError {
    query_error("session execution revision exhausted")
}
