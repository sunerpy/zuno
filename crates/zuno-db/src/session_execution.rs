//! Durable, revisioned collaboration and continuation state.

use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::execution::{
    CollaborationMode, DraftReviewRiskAcceptance, SessionExecutionPhase, SessionExecutionState,
    SessionPauseReason, SessionReadiness, SessionScheduling, SessionWaitReference,
    SessionWakeSignal, TurnExecutionIdentity, WakeAdmission,
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

    pub fn set_scheduling(
        &self,
        session_id: &str,
        expected_revision: i64,
        scheduling: SessionScheduling,
        at_ms: i64,
    ) -> Result<SessionExecutionState, DbError> {
        self.pool.transaction(|transaction| {
            set_scheduling_in(
                transaction,
                session_id,
                expected_revision,
                scheduling,
                at_ms,
            )
        })
    }

    pub fn admit_wake(
        &self,
        session_id: &str,
        signal: &SessionWakeSignal,
        at_ms: i64,
    ) -> Result<WakeAdmission, DbError> {
        self.pool
            .transaction(|transaction| admit_wake_in(transaction, session_id, signal, at_ms))
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
                    draft_review_risk, cycle_id, phase, continuation, time_created, time_updated, \
                    scheduling \
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
        scheduling: Some(SessionScheduling::default()),
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
    let scheduling = encode_optional(&state.scheduling)?;
    let changed = transaction
        .execute(
            "UPDATE session_execution_state SET \
               revision = ?1, mode = ?2, work_identity = ?3, authorized_plan_id = ?4, \
               authorized_plan_revision = ?5, handoff_plan_id = ?6, handoff_plan_revision = ?7, \
               draft_review_risk = ?8, cycle_id = ?9, phase = ?10, continuation = ?11, \
               time_updated = ?12, scheduling = ?15 \
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
                scheduling,
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

/// Replace scheduling metadata and its execution phase in the caller's
/// transaction. All identity, cycle, continuation, mode and authorization
/// fields are read from the current row and preserved.
pub fn set_scheduling_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    scheduling: SessionScheduling,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    let state = required_in(transaction, session_id)?;
    write_scheduling_in(transaction, expected_revision, state, scheduling, at_ms)
}

/// Enter an exact human/external wait without resetting session progress.
pub fn set_waiting_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    wait: SessionWaitReference,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    set_readiness_in(
        transaction,
        session_id,
        expected_revision,
        wait.into(),
        at_ms,
    )
}

/// Pause automatic execution without changing Plan/Goal authorization.
/// Use `set_scheduling_in` when also recording a new fingerprint/count.
pub fn set_paused_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    reason: SessionPauseReason,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    set_readiness_in(
        transaction,
        session_id,
        expected_revision,
        SessionReadiness::Paused { reason },
        at_ms,
    )
}

/// Close automatic eligibility independently of an unfinished Plan.
pub fn set_completed_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    set_readiness_in(
        transaction,
        session_id,
        expected_revision,
        SessionReadiness::Completed,
        at_ms,
    )
}

/// Clear only this exact wait after the caller has durably accepted its answer
/// or terminal external result. A stale, unrelated or already-cleared wait is
/// a no-op (`None`), including its revision and timestamps.
pub fn clear_matching_wait_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    wait: &SessionWaitReference,
    at_ms: i64,
) -> Result<Option<SessionExecutionState>, DbError> {
    let Some(state) = read_in(transaction, session_id)? else {
        return Ok(None);
    };
    let signal = match wait {
        SessionWaitReference::Human { request_id } => SessionWakeSignal::UserAnswer {
            request_id: request_id.clone(),
        },
        SessionWaitReference::External {
            source_id,
            origin_cycle_id,
        } => SessionWakeSignal::ExternalCompletion {
            source_id: source_id.clone(),
            origin_cycle_id: origin_cycle_id.clone(),
        },
    };
    if state.wake_admission(&signal) != WakeAdmission::Resume {
        return Ok(None);
    }
    resume_in(transaction, state, at_ms).map(Some)
}

/// Evaluate and apply a host-verified wake in the same transaction as inbox,
/// question, or completion delivery. `Reject` and `Admit` do not mutate the
/// execution row; only `Resume` clears the satisfied gate. In particular, a
/// user query may run while work stays paused/waiting. A new user query or
/// validated answer after completion restores session eligibility; it does not
/// resume a completed Goal or change Plan authorization.
///
/// This changes scheduling eligibility only. The caller still checks mode and
/// Plan/Goal authorization and owns the durable input/event admission.
pub fn admit_wake_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    signal: &SessionWakeSignal,
    at_ms: i64,
) -> Result<WakeAdmission, DbError> {
    let state = required_in(transaction, session_id)?;
    let admission = state.wake_admission(signal);
    if admission == WakeAdmission::Resume {
        resume_in(transaction, state, at_ms)?;
    }
    Ok(admission)
}

fn required_in(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<SessionExecutionState, DbError> {
    read_in(connection, session_id)?.ok_or_else(|| DbError::NotFound {
        table: "session_execution_state".to_owned(),
        id: session_id.to_owned(),
    })
}

fn set_readiness_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    readiness: SessionReadiness,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    let state = required_in(transaction, session_id)?;
    let mut scheduling = state.scheduling.clone().unwrap_or_default();
    scheduling.readiness = readiness;
    write_scheduling_in(transaction, expected_revision, state, scheduling, at_ms)
}

fn resume_in(
    transaction: &Transaction<'_>,
    state: SessionExecutionState,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    let mut scheduling = state.scheduling.clone().unwrap_or_default();
    scheduling.readiness = SessionReadiness::Ready;
    write_scheduling_in(transaction, state.revision, state, scheduling, at_ms)
}

fn write_scheduling_in(
    transaction: &Transaction<'_>,
    expected_revision: i64,
    mut state: SessionExecutionState,
    scheduling: SessionScheduling,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    state.phase = match scheduling.readiness {
        SessionReadiness::Ready => match state.phase {
            SessionExecutionPhase::Waiting
            | SessionExecutionPhase::Paused
            | SessionExecutionPhase::Blocked
            | SessionExecutionPhase::Completed => match state.mode {
                CollaborationMode::Plan => SessionExecutionPhase::Planning,
                CollaborationMode::Work => SessionExecutionPhase::Idle,
            },
            phase => phase,
        },
        SessionReadiness::WaitingHuman { .. } | SessionReadiness::WaitingExternal { .. } => {
            SessionExecutionPhase::Waiting
        }
        SessionReadiness::Paused { .. } => SessionExecutionPhase::Paused,
        SessionReadiness::Completed => SessionExecutionPhase::Completed,
    };
    state.scheduling = Some(scheduling);
    state.time_updated = at_ms.max(state.time_updated);
    validate(&state)?;
    state.revision = expected_revision
        .checked_add(1)
        .ok_or_else(revision_exhausted)?;
    let changed = transaction
        .execute(
            "UPDATE session_execution_state SET revision = ?1, phase = ?2, scheduling = ?3, \
             time_updated = ?4 WHERE session_id = ?5 AND revision = ?6",
            params![
                state.revision,
                state.phase.as_str(),
                encode_optional(&state.scheduling)?,
                state.time_updated,
                state.session_id,
                expected_revision,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        let actual = required_in(transaction, &state.session_id)?.revision;
        return Err(DbError::Conflict {
            table: "session_execution_state".to_owned(),
            id: state.session_id,
            detail: format!("revision conflict: expected {expected_revision}, current {actual}"),
        });
    }
    Ok(state)
}

/// Format-13 conservative repair for the released no-progress regression.
///
/// Only a latest structured v1 driver event with phase `paused` and reason
/// `no_progress` repairs a legacy execution row still marked `running`. The
/// execution revision advances once; only phase and scheduling change. Cycle,
/// identity, continuation, timestamps and Plan/Goal authority stay byte-for-byte
/// intact. All other legacy rows retain NULL scheduling. No message text is read.
pub(crate) fn repair_legacy_scheduling_in(transaction: &Transaction<'_>) -> Result<(), DbError> {
    let sessions = transaction
        .prepare(
            "SELECT session_id FROM session_execution_state \
             WHERE scheduling IS NULL AND phase = 'running'",
        )
        .map_err(open::map_error)?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    for session_id in sessions {
        let Some(event) =
            crate::event_log::latest_of_type_in(transaction, &session_id, "session.driver.phase")?
        else {
            continue;
        };
        if event.version != 1
            || event
                .properties
                .get("phase")
                .and_then(serde_json::Value::as_str)
                != Some("paused")
            || event
                .properties
                .get("pauseReason")
                .or_else(|| event.properties.get("reason"))
                .and_then(serde_json::Value::as_str)
                != Some("no_progress")
        {
            continue;
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Progress {
            progress_fingerprint: Option<String>,
            #[serde(default)]
            unchanged_progress_count: u32,
        }
        let progress: Progress = serde_json::from_value(event.properties.into())
            .map_err(|error| query_error(error.to_string()))?;
        let scheduling = SessionScheduling {
            readiness: SessionReadiness::Paused {
                reason: SessionPauseReason::NoProgress,
            },
            progress_fingerprint: progress.progress_fingerprint,
            unchanged_progress_count: progress.unchanged_progress_count,
        };
        let state = required_in(transaction, &session_id)?;
        // Use a column-scoped update: historical JSON strings may have valid
        // formatting that a deserialize/serialize round trip would normalize.
        let encoded =
            serde_json::to_string(&scheduling).map_err(|error| query_error(error.to_string()))?;
        let next_revision = state
            .revision
            .checked_add(1)
            .ok_or_else(revision_exhausted)?;
        validate_scheduling(&scheduling, SessionExecutionPhase::Paused)?;
        transaction
            .execute(
                "UPDATE session_execution_state SET phase = 'paused', scheduling = ?1, \
                 revision = ?2 WHERE session_id = ?3 AND scheduling IS NULL AND phase = 'running'",
                params![encoded, next_revision, session_id],
            )
            .map_err(open::map_error)?;
    }
    Ok(())
}

fn insert_in(transaction: &Transaction<'_>, state: &SessionExecutionState) -> Result<(), DbError> {
    validate(state)?;
    let work_identity = encode_optional(&state.work_identity)?;
    let continuation = encode_optional(&state.continuation)?;
    let draft_review_risk = encode_optional(&state.draft_review_risk)?;
    let scheduling = encode_optional(&state.scheduling)?;
    transaction
        .execute(
            "INSERT INTO session_execution_state \
             (session_id, revision, mode, work_identity, authorized_plan_id, \
              authorized_plan_revision, handoff_plan_id, handoff_plan_revision, draft_review_risk, \
              cycle_id, phase, continuation, time_created, time_updated, scheduling) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                scheduling,
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
    scheduling: Option<String>,
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
        scheduling: row.get(14)?,
    })
}

fn decode(stored: StoredExecution) -> Result<SessionExecutionState, DbError> {
    let state = SessionExecutionState {
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
        scheduling: decode_optional(stored.scheduling)?,
    };
    validate(&state)?;
    Ok(state)
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
    if let Some(scheduling) = &state.scheduling {
        validate_scheduling(scheduling, state.phase)?;
    }
    Ok(())
}

fn validate_scheduling(
    scheduling: &SessionScheduling,
    phase: SessionExecutionPhase,
) -> Result<(), DbError> {
    if scheduling
        .progress_fingerprint
        .as_ref()
        .is_some_and(|fingerprint| fingerprint.trim().is_empty())
    {
        return Err(query_error("scheduling fingerprint must not be empty"));
    }
    let phase_matches = match &scheduling.readiness {
        SessionReadiness::Ready => matches!(
            phase,
            SessionExecutionPhase::Idle
                | SessionExecutionPhase::Planning
                | SessionExecutionPhase::Authorized
                | SessionExecutionPhase::Running
        ),
        SessionReadiness::WaitingHuman { request_id } => {
            if request_id.trim().is_empty() {
                return Err(query_error("human wait requires a request id"));
            }
            phase == SessionExecutionPhase::Waiting
        }
        SessionReadiness::WaitingExternal {
            source_id,
            origin_cycle_id,
        } => {
            if source_id.trim().is_empty() || origin_cycle_id.trim().is_empty() {
                return Err(query_error(
                    "external wait requires a source id and origin cycle id",
                ));
            }
            phase == SessionExecutionPhase::Waiting
        }
        SessionReadiness::Paused { .. } => matches!(
            phase,
            SessionExecutionPhase::Paused | SessionExecutionPhase::Blocked
        ),
        SessionReadiness::Completed => phase == SessionExecutionPhase::Completed,
    };
    if !phase_matches {
        return Err(query_error(
            "execution phase and scheduling readiness disagree",
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
