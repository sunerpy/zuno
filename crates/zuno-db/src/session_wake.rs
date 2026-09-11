//! Read-only scheduling checks shared by live steering and idle input drivers.

use crate::inbox::{DurableInputKind, SessionInput, SubmissionState};
use crate::{Connection, open};
use rusqlite::OptionalExtension;
use zuno_error::DbError;
use zuno_types::execution::{InputTriggerKind, SessionWakeSignal, WakeAdmission};

/// A queued signal is only a hint. Re-read its exact revision and state before
/// offering it to a model; inline consumption or cancellation can retire it.
pub fn pending_admission_in(
    connection: &Connection,
    input: &SessionInput,
) -> Result<WakeAdmission, DbError> {
    let Some(current) = crate::inbox::read_in(connection, &input.session_id, &input.id)? else {
        return Ok(WakeAdmission::Reject);
    };
    if current.revision != input.revision
        || !matches!(
            current.state,
            SubmissionState::Queued | SubmissionState::Steering | SubmissionState::Promoted
        )
    {
        return Ok(WakeAdmission::Reject);
    }
    admission_in(connection, &current)
}

/// Evaluate the durable trigger, without consuming input or clearing its gate.
pub fn admission_in(
    connection: &Connection,
    input: &SessionInput,
) -> Result<WakeAdmission, DbError> {
    let Some(signal) = signal_in(connection, input)? else {
        return Ok(WakeAdmission::Reject);
    };
    let state = crate::session_execution::read_in(connection, &input.session_id)?;
    if let SessionWakeSignal::ExternalCompletion {
        origin_cycle_id, ..
    } = &signal
    {
        // Legacy unbound results are retained as evidence, not upgraded into a
        // fresh work cycle. Nor may an older cycle resume newer work.
        if state.as_ref().and_then(|state| state.cycle_id.as_deref()) != Some(origin_cycle_id) {
            return Ok(WakeAdmission::Reject);
        }
    }
    Ok(state.map_or(WakeAdmission::Admit, |state| state.wake_admission(&signal)))
}

/// Construct wake facts from trusted durable input, not assistant/user prose.
pub fn signal_in(
    connection: &Connection,
    input: &SessionInput,
) -> Result<Option<SessionWakeSignal>, DbError> {
    if DurableInputKind::classify(&input.prompt) == Some(DurableInputKind::HumanRequestAnswer) {
        let Some(request_id) = input
            .prompt
            .get("requestID")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(None);
        };
        let state = connection
            .query_row(
                "SELECT h.state, q.purpose FROM human_request h \
             LEFT JOIN question_interaction q ON q.request_id=h.id \
             WHERE h.id=?1 AND h.session_id=?2",
                rusqlite::params![request_id, input.session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(open::map_error)?;
        let Some((state, purpose)) = state else {
            return Ok(None);
        };
        if purpose.as_deref() == Some("plan_authorization") {
            // Approve never uses an answer input: only its exact Work control
            // grants authority. Decline/cancel/defer are user notifications and
            // may be acknowledged while the existing pause/wait remains intact.
            return Ok(matches!(
                input
                    .prompt
                    .get("outcome")
                    .and_then(serde_json::Value::as_str),
                Some("declined" | "cancelled" | "deferred")
            )
            .then_some(SessionWakeSignal::UserQuery));
        }
        if state == "cancelled" {
            return Ok(Some(SessionWakeSignal::UserQuery));
        }
        if purpose.as_deref() == Some("required_input") && state != "answered" {
            return Ok(None);
        }
        return Ok(Some(SessionWakeSignal::UserAnswer {
            request_id: request_id.to_owned(),
        }));
    }
    if input.trigger_kind == InputTriggerKind::Automatic
        || DurableInputKind::classify(&input.prompt)
            .is_some_and(DurableInputKind::is_asynchronous_report)
    {
        let Some(origin_cycle_id) = input
            .cycle_id
            .as_ref()
            .filter(|cycle| !cycle.trim().is_empty())
        else {
            return Ok(None);
        };
        if !DurableInputKind::classify(&input.prompt)
            .is_some_and(DurableInputKind::is_asynchronous_report)
        {
            return Ok(None);
        }
        let source_id = ["executionID", "jobID", "workflowID", "productAgentID"]
            .into_iter()
            .find_map(|key| input.prompt.get(key).and_then(serde_json::Value::as_str))
            .or(input.source_key.as_deref());
        return Ok(source_id
            .filter(|id| !id.trim().is_empty())
            .map(|source_id| SessionWakeSignal::ExternalCompletion {
                source_id: source_id.to_owned(),
                origin_cycle_id: origin_cycle_id.clone(),
            }));
    }
    Ok(Some(match input.trigger_kind {
        InputTriggerKind::UserControl => SessionWakeSignal::ExplicitResume,
        InputTriggerKind::Recovery => SessionWakeSignal::Recovery,
        _ => SessionWakeSignal::UserQuery,
    }))
}
