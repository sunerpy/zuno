//! Failure ownership is a native transaction, not a session-wide Goal lookup.

use super::*;
use serde::{Deserialize, Serialize};
use zuno_goal::{GoalFailureDisposition, GoalRetryPolicy, GoalTerminalFailure};

/// Captured before execution. A late failure cannot acquire a newer work scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnFailureScope {
    pub cycle_id: String,
    pub turn_id: Option<String>,
    pub goal_id: Option<String>,
}

#[derive(Debug)]
pub enum SessionFailureDisposition {
    Goal(GoalFailureDisposition),
    OrdinaryStopped,
    GateRetained,
    Stale,
}

impl SessionControlService {
    pub fn capture_failure_scope(
        &self,
        session_id: &str,
    ) -> Result<Option<TurnFailureScope>, SessionControlError> {
        let connection = self.pool.get()?;
        Ok(
            zuno_db::session_work_cycle::current_in(&connection, session_id)?.map(|scope| {
                TurnFailureScope {
                    cycle_id: scope.cycle_id,
                    turn_id: scope.active_turn_id,
                    goal_id: scope.goal_id,
                }
            }),
        )
    }

    /// Settle only the captured execution. Recoverable ordinary failures exhaust
    /// this request, not permission to receive another independent user message.
    pub fn settle_turn_failure(
        &self,
        session_id: &str,
        scope: &TurnFailureScope,
        failure: GoalTerminalFailure,
        retry_policy: GoalRetryPolicy,
        at_ms: i64,
        entropy: u64,
    ) -> Result<SessionFailureDisposition, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(current) = zuno_db::session_work_cycle::current_in(tx, session_id)?
                .filter(|current| current.cycle_id == scope.cycle_id
                    && current.active_turn_id == scope.turn_id
                    && current.stopped.is_none())
            else {
                return Ok(SessionFailureDisposition::Stale);
            };
            if current.goal_id != scope.goal_id {
                // A model may propose its Goal inside this very engine turn.
                // Only that native binding can extend a previously independent
                // scope. Never inherit an external/replacement Goal.
                let bound = GoalStore::current_goal_turn_in(tx, session_id)?;
                if scope.goal_id.is_some() || scope.turn_id.is_none()
                    || !bound.as_ref().is_some_and(|bound|
                        bound.cycle_id == scope.cycle_id
                        && Some(bound.turn_id.as_str()) == scope.turn_id.as_deref()
                        && Some(bound.goal_id.as_str()) == current.goal_id.as_deref())
                {
                    return Ok(SessionFailureDisposition::Stale);
                }
            }
            let settled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM goal_turn_audit
                 WHERE session_id=?1 AND goal_id=?2 AND cycle_id=?3 AND turn_id=?4)",
                rusqlite::params![session_id, current.goal_id, scope.cycle_id, scope.turn_id],
                |row| row.get(0),
            ).map_err(open::map_error)?;
            if settled {
                return Ok(SessionFailureDisposition::Stale);
            }
            let Some(state) = read_in(tx, session_id)? else {
                return Ok(SessionFailureDisposition::Stale);
            };
            let protected = state.scheduling.as_ref().map_or(
                matches!(state.phase, SessionExecutionPhase::Paused
                    | SessionExecutionPhase::Blocked | SessionExecutionPhase::Waiting),
                |s| !matches!(s.readiness, SessionReadiness::Ready | SessionReadiness::Completed),
            );
            let uncertain = !zuno_db::message::MessageStore::new(tx)
                .pending_uncertain_tool_calls(session_id, 0)?.is_empty();
            let disposition = if protected || uncertain {
                if uncertain && !protected {
                    zuno_db::session_execution::set_paused_in(
                        tx, session_id, state.revision, SessionPauseReason::UncertainSideEffect, at_ms,
                    )?;
                }
                SessionFailureDisposition::GateRetained
            } else if let Some(goal_id) = &current.goal_id {
                let result = GoalStore::settle_failure_in(
                    tx, session_id, goal_id, failure, retry_policy, at_ms, entropy,
                )?;
                if matches!(result, GoalFailureDisposition::NoActiveGoal) {
                    // Completion, cancellation, replacement or a user pause won.
                    return Ok(SessionFailureDisposition::Stale);
                }
                if let GoalFailureDisposition::RetryScheduled(retry) = &result {
                    zuno_engine::plan_driver::PlanReconciliationDriver::waiting_retry_in(
                        tx, session_id, &scope.cycle_id, retry.reason.as_str(),
                    )?;
                }
                if let Some(reason) = pause_reason(failure) {
                    zuno_db::session_execution::set_paused_in(
                        tx, session_id, state.revision, reason, at_ms,
                    )?;
                }
                SessionFailureDisposition::Goal(result)
            } else if let Some(reason) = pause_reason(failure)
                .filter(|reason| *reason != SessionPauseReason::User)
            {
                zuno_db::session_execution::set_paused_in(
                    tx, session_id, state.revision, reason, at_ms,
                )?;
                SessionFailureDisposition::GateRetained
            } else {
                Self::stop_cycle_in(
                    tx, session_id, &scope.cycle_id, scope.turn_id.as_deref(), false, at_ms,
                )?;
                SessionFailureDisposition::OrdinaryStopped
            };
            let category = match failure {
                GoalTerminalFailure::Retry { reason, .. } => reason.as_str().to_owned(),
                GoalTerminalFailure::Pause(reason) => format!("pause:{}", reason.as_str()),
                GoalTerminalFailure::Block(reason) => reason.rendered(),
            };
            zuno_db::event_log::append_in(tx, session_id,
                zuno_db::event_log::NewSessionEvent::new("session.turn.failure_settled",
                    json!({"scope":scope,"effectiveGoalId":current.goal_id,"category":category,
                        "retryable":matches!(failure, GoalTerminalFailure::Retry { .. }),
                        "ordinaryStopped":matches!(disposition, SessionFailureDisposition::OrdinaryStopped),
                        "time":at_ms}).as_object().expect("object").clone())?)?;
            Ok(disposition)
        })
    }
}

fn pause_reason(failure: GoalTerminalFailure) -> Option<SessionPauseReason> {
    use zuno_goal::GoalPauseReason as GoalPause;
    Some(match failure {
        GoalTerminalFailure::Retry { .. } => return None,
        GoalTerminalFailure::Block(_) => SessionPauseReason::Blocked,
        GoalTerminalFailure::Pause(reason) => match reason {
            GoalPause::Authentication => SessionPauseReason::Authentication,
            GoalPause::NoProgress => SessionPauseReason::NoProgress,
            GoalPause::TurnBudget => SessionPauseReason::TurnBudget,
            GoalPause::UserInterruption => SessionPauseReason::User,
            GoalPause::UncertainSideEffect => SessionPauseReason::UncertainSideEffect,
            GoalPause::Permission | GoalPause::PlanMode | GoalPause::HumanInput => {
                SessionPauseReason::Blocked
            }
        },
    })
}
