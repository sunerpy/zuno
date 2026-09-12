//! Deferred input diagnostics are native, durable decisions, not client guesses.

use super::*;
use zuno_types::admission::{InputExecutionGate, InputGateReason, InputGateRecovery};

impl SessionControlService {
    /// Called by the native input owner after a drive returned without sampling.
    /// Read eligibility and record the receipt under one writer snapshot; a stale
    /// owner cannot attach another input's gate or alter an applied receipt.
    pub fn defer_input_at_execution_gate(
        &self,
        session_id: &str,
        input_id: &str,
        at_ms: i64,
    ) -> Result<bool, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(state) = read_in(tx, session_id)? else {
                return Ok(false);
            };
            defer_input_in(tx, &state, input_id, at_ms)
        })
    }
}

/// Also used by explicit recovery before it changes the old execution state.
/// This closes the resume-before-diagnostic window without guessing a failure
/// or exposing an unbound input between two independent transactions.
pub(crate) fn defer_input_in(
    tx: &zuno_db::Transaction<'_>,
    state: &SessionExecutionState,
    input_id: &str,
    at_ms: i64,
) -> Result<bool, SessionControlError> {
    let session_id = &state.session_id;
    if zuno_db::input_receipt::get_in(tx, session_id, input_id)?
        .is_some_and(|receipt| receipt.execution_gate.is_some())
    {
        return Ok(true);
    }
    let Some(input) = zuno_db::inbox::read_in(tx, session_id, input_id)? else {
        return Ok(false);
    };
    if input.cycle_id.is_none() || input.cycle_id != state.cycle_id {
        return Ok(false);
    }
    let Some(mut gate) = execution_gate(state) else {
        return Ok(false);
    };
    if gate.recovery == InputGateRecovery::ResumeWork && state.mode == CollaborationMode::Plan {
        gate.recovery = InputGateRecovery::StartWork;
    } else if gate.recovery == InputGateRecovery::ResumeWork
        && let Some(goal) = GoalStore::goal_in(tx, session_id)?
    {
        use zuno_goal::GoalStatus;
        gate.recovery = match goal.status {
            GoalStatus::Active => InputGateRecovery::ResumeWork,
            GoalStatus::Paused | GoalStatus::Blocked => InputGateRecovery::ResumeGoal,
            GoalStatus::UsageLimited
            | GoalStatus::BudgetLimited
            | GoalStatus::Complete
            | GoalStatus::Cancelled => InputGateRecovery::InspectSession,
        };
    }
    zuno_db::input_receipt::record_execution_gate_in(tx, session_id, input_id, &gate, at_ms)
        .map_err(Into::into)
}

fn execution_gate(state: &SessionExecutionState) -> Option<InputExecutionGate> {
    use InputGateReason as Reason;
    use InputGateRecovery as Recovery;
    let mut request_id = None;
    let mut source_id = None;
    let (reason, recovery) = match state.scheduling.as_ref().map(|s| &s.readiness) {
        Some(SessionReadiness::Ready | SessionReadiness::Completed) => return None,
        Some(SessionReadiness::WaitingHuman { request_id: id }) => {
            request_id = Some(id.clone());
            (Reason::WaitingHuman, Recovery::ResolveHumanRequest)
        }
        Some(SessionReadiness::WaitingExternal { source_id: id, .. }) => {
            source_id = Some(id.clone());
            (Reason::WaitingExternal, Recovery::WaitForEvent)
        }
        Some(SessionReadiness::Paused { reason }) => match reason {
            SessionPauseReason::User => (Reason::User, Recovery::ResumeWork),
            SessionPauseReason::Authentication => {
                (Reason::Authentication, Recovery::Reauthenticate)
            }
            SessionPauseReason::TurnBudget => (Reason::TurnBudget, Recovery::ReviewBudget),
            SessionPauseReason::UncertainSideEffect => {
                (Reason::UncertainSideEffect, Recovery::InspectOutcome)
            }
            SessionPauseReason::Blocked => (Reason::Blocked, Recovery::InspectSession),
            SessionPauseReason::NoProgress => (Reason::NoProgress, Recovery::ResumeWork),
            SessionPauseReason::NoExecutableWork => {
                (Reason::NoExecutableWork, Recovery::ResumeWork)
            }
        },
        None if matches!(
            state.phase,
            SessionExecutionPhase::Paused
                | SessionExecutionPhase::Waiting
                | SessionExecutionPhase::Blocked
        ) =>
        {
            (Reason::ExecutionUnavailable, Recovery::InspectSession)
        }
        None => return None,
    };
    Some(InputExecutionGate {
        reason,
        recovery,
        execution_revision: state.revision,
        cycle_id: state.cycle_id.clone()?,
        request_id,
        source_id,
    })
}
