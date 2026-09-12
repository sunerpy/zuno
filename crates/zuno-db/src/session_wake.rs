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
    if signal == SessionWakeSignal::UserMessage {
        // Delivery is not execution authority. A real user input must reach
        // native promotion so it can acquire its own scope instead of inheriting
        // an old no-progress/stop gate. That transaction preserves protected
        // barriers, and the driver independently rejects model execution there.
        return Ok(WakeAdmission::Admit);
    }
    let state = crate::session_execution::read_in(connection, &input.session_id)?;
    if let SessionWakeSignal::ExternalCompletion {
        origin_cycle_id, ..
    } = &signal
    {
        // Legacy unbound results are retained as evidence, not upgraded into a
        // fresh work cycle. Nor may an older cycle resume newer work.
        if crate::session_work_cycle::completion_cycle_in(
            connection,
            &input.session_id,
            origin_cycle_id,
        )?
        .is_none()
        {
            return Ok(WakeAdmission::Reject);
        }
    }
    Ok(state.map_or(WakeAdmission::Admit, |state| state.wake_admission(&signal)))
}

/// Recheck authority at live model application, under the caller's writer
/// transaction. Delivery admission alone must never authorize this boundary.
///
/// `claimed` is the exact promotion owned by this caller, not an observer's
/// receipt. A refused claim is returned to its admitted lane in this transaction;
/// a stale claim cannot release or consume another owner's input. The caller
/// commits a rejection without writing model history, or commits an admission
/// together with history, consumption and turn binding.
/// A bound Goal must still be active. Report aliases authorize only report
/// delivery; they cannot transfer a user/control input's execution ownership.
/// An admitted unbound non-report input acquires the current execution cycle
/// with the later history/consumption commit. Its original trigger is immutable.
pub fn model_application_admission_in(
    transaction: &crate::Transaction<'_>,
    session_id: &str,
    claimed: Option<&SessionInput>,
) -> Result<WakeAdmission, DbError> {
    let (delivery, input) = if let Some(claimed) = claimed {
        if claimed.session_id != session_id {
            return Ok(WakeAdmission::Reject);
        }
        let Some(current) = crate::inbox::read_in(transaction, session_id, &claimed.id)? else {
            return Ok(WakeAdmission::Reject);
        };
        if current.state != SubmissionState::Promoted || current.revision != claimed.revision {
            return Ok(WakeAdmission::Reject);
        }
        (admission_in(transaction, &current)?, Some(current))
    } else {
        (WakeAdmission::Admit, None)
    };
    let scope = crate::session_work_cycle::current_in(transaction, session_id)?;
    let stopped = scope.as_ref().is_some_and(|cycle| cycle.stopped.is_some());
    let state = crate::session_execution::read_in(transaction, session_id)?;
    let goal_active = match scope.as_ref().and_then(|cycle| cycle.goal_id.as_deref()) {
        Some(goal_id) => transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM goal \
                 WHERE session_id=?1 AND goal_id=?2 AND status='active')",
                (session_id, goal_id),
                |row| row.get::<_, bool>(0),
            )
            .map_err(open::map_error)?,
        None => true,
    };
    let input_scope_matches = input.as_ref().is_none_or(|input| {
        DurableInputKind::classify(&input.prompt)
            .is_some_and(DurableInputKind::is_asynchronous_report)
            || input.cycle_id.as_deref().is_none_or(|cycle_id| {
                state.as_ref().and_then(|state| state.cycle_id.as_deref()) == Some(cycle_id)
            })
    });
    // A completed cycle needs native activation too. Live injection must neither
    // clear a gate nor impersonate a trusted UserQuery/ExplicitResume control.
    let executable = delivery != WakeAdmission::Reject
        && !stopped
        && goal_active
        && input_scope_matches
        && state.as_ref().is_none_or(|state| {
            state.wake_admission(&SessionWakeSignal::UserMessage) == WakeAdmission::Admit
        });
    if executable {
        if let (Some(input), Some(cycle_id)) = (
            input.as_ref(),
            state.as_ref().and_then(|state| state.cycle_id.as_deref()),
        ) && input.cycle_id.is_none()
            && !DurableInputKind::classify(&input.prompt)
                .is_some_and(DurableInputKind::is_asynchronous_report)
        {
            let changed = transaction
                .execute(
                    "UPDATE session_input SET cycle_id=?1 \
                     WHERE session_id=?2 AND id=?3 AND revision=?4 \
                       AND state='promoted' AND cycle_id IS NULL",
                    (cycle_id, session_id, input.id.as_str(), input.revision),
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(DbError::Conflict {
                    table: "session_input".to_owned(),
                    id: input.id.clone(),
                    detail: "live application lost its input before cycle binding".to_owned(),
                });
            }
        }
        return Ok(WakeAdmission::Admit);
    }
    if let Some(claimed) = claimed {
        crate::inbox::recover_promoted_in(transaction, session_id, &claimed.id)?;
    }
    Ok(WakeAdmission::Reject)
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
        InputTriggerKind::User => SessionWakeSignal::UserMessage,
        InputTriggerKind::Legacy
            if matches!(
                DurableInputKind::classify(&input.prompt),
                Some(
                    DurableInputKind::User
                        | DurableInputKind::TuiPrompt
                        | DurableInputKind::AcpPrompt
                        | DurableInputKind::HostMessage
                )
            ) =>
        {
            SessionWakeSignal::UserMessage
        }
        _ => SessionWakeSignal::UserQuery,
    }))
}
