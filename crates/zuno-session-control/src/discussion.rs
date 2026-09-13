//! Text-only user requests may be answered without resuming uncertain work.
//! A discussion grant is never Work/Goal authority and never clears a gate.

use super::*;
use serde::Serialize;
use zuno_db::inbox::{DurableInputKind, SubmissionState};
use zuno_types::admission::InputReceiptState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscussionAdmission {
    pub session_id: String,
    pub input_id: String,
    pub cycle_id: String,
    pub execution_revision: i64,
    pub identity: TurnExecutionIdentity,
}

impl SessionControlService {
    /// Recheck the granted restriction before each logical model request.
    /// A later auth/approval/budget control takes effect
    /// without granting the discussion authority to clear it.
    pub fn discussion_is_current(
        &self,
        admission: &DiscussionAdmission,
        turn_id: &str,
    ) -> Result<bool, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(state) = eligible_state_in(tx, &admission.session_id)? else {
                return Ok(false);
            };
            let Some(cycle) = zuno_db::session_work_cycle::current_in(tx, &admission.session_id)?
            else {
                return Ok(false);
            };
            let receipt =
                zuno_db::input_receipt::get_in(tx, &admission.session_id, &admission.input_id)?;
            Ok(state.revision == admission.execution_revision
                && cycle.cycle_id == admission.cycle_id
                && cycle.stopped.is_none()
                && cycle.goal_id.is_none()
                && cycle.plan_id.is_none()
                && cycle.active_turn_id.as_deref() == Some(turn_id)
                && receipt.is_some_and(|r| {
                    r.turn_id.as_deref() == Some(turn_id) && !r.state.is_terminal()
                }))
        })
    }

    /// A client may deliver a queued genuine user question through an
    /// uncertainty gate. This is not a model-execution grant: promotion and
    /// `claim_discussion` must still revalidate it atomically.
    pub fn may_admit_discussion(&self, input: &SessionInput) -> Result<bool, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let Some(stored) = zuno_db::inbox::read_in(tx, &input.session_id, &input.id)? else {
                return Ok(false);
            };
            Ok(stored.revision == input.revision
                && matches!(
                    stored.state,
                    SubmissionState::Queued | SubmissionState::Steering | SubmissionState::Promoted
                )
                && real_user_input(&stored)
                && eligible_state_in(tx, &stored.session_id)?.is_some())
        })
    }

    /// Read-only candidate lookup shared by ordinary input and restart pumps.
    /// Only the current, never-bound genuine user input may be discussed.
    pub fn pending_discussion(
        &self,
        session_id: &str,
    ) -> Result<Option<DiscussionAdmission>, SessionControlError> {
        self.pool.try_transaction(|tx| candidate_in(tx, session_id))
    }

    /// Caller holds the actual native turn lease. Claim and receipt binding are
    /// atomic; another pump or a process restart cannot execute the same input.
    pub fn claim_discussion(
        &self,
        expected: &DiscussionAdmission,
        turn_id: &str,
        at_ms: i64,
    ) -> Result<bool, SessionControlError> {
        self.pool.try_transaction(|tx| {
            if turn_id.trim().is_empty()
                || candidate_in(tx, &expected.session_id)?.as_ref() != Some(expected)
            {
                return Ok(false);
            }
            let Some(mut cycle) =
                zuno_db::session_work_cycle::current_in(tx, &expected.session_id)?
            else {
                return Ok(false);
            };
            cycle.active_turn_id = Some(turn_id.to_owned());
            zuno_db::session_work_cycle::save_in(tx, &cycle, at_ms)?;
            zuno_db::input_receipt::bind_turn_in(
                tx,
                &expected.session_id,
                std::slice::from_ref(&expected.input_id),
                turn_id,
                at_ms,
            )?;
            zuno_db::event_log::append_in(
                tx,
                &expected.session_id,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.discussion.started",
                    json!({"admission":expected,"turnId":turn_id,"tools":"disabled","time":at_ms})
                        .as_object()
                        .expect("object")
                        .clone(),
                )?,
            )?;
            Ok(true)
        })
    }
}

fn eligible_state_in(
    tx: &zuno_db::Transaction<'_>,
    session_id: &str,
) -> Result<Option<SessionExecutionState>, SessionControlError> {
    let Some(state) = read_in(tx, session_id)? else {
        return Ok(None);
    };
    if state.mode != CollaborationMode::Work
        || !matches!(
            state.phase,
            SessionExecutionPhase::Paused | SessionExecutionPhase::Blocked
        )
        || state.authorized_plan_id.is_some()
        || state.authorized_plan_revision.is_some()
        || state.handoff_plan_id.is_some()
        || state.handoff_plan_revision.is_some()
        || state.draft_review_risk.is_some()
    {
        return Ok(None);
    }
    let goal = GoalStore::goal_in(tx, session_id)?;
    if goal.as_ref().is_some_and(|goal| {
        matches!(
            goal.status,
            zuno_goal::GoalStatus::Active
                | zuno_goal::GoalStatus::BudgetLimited
                | zuno_goal::GoalStatus::UsageLimited
        ) || goal
            .token_budget
            .is_some_and(|budget| !goal.usage_known || goal.tokens_used >= budget)
    }) {
        return Ok(None);
    }
    let goal_pause = GoalStore::pause_state_in(tx, session_id)?;
    let legacy_uncertainty = goal.as_ref().is_some_and(|goal| {
        goal.status == zuno_goal::GoalStatus::Paused
            && goal_pause.as_ref().is_some_and(|pause| {
                pause.goal_id == goal.goal_id
                    && pause.reason == zuno_goal::GoalPauseReason::UncertainSideEffect
            })
    });
    let permitted = state
        .scheduling
        .as_ref()
        .is_some_and(|s| match s.readiness {
            SessionReadiness::Paused {
                reason: SessionPauseReason::UncertainSideEffect,
            } => true,
            SessionReadiness::Paused {
                reason: SessionPauseReason::Blocked,
            } => legacy_uncertainty,
            _ => false,
        });
    if !permitted {
        return Ok(None);
    }
    // Model access must not accidentally waive a second, separately owned gate.
    if goal_pause.as_ref().is_some_and(|pause| {
        matches!(
            pause.reason,
            zuno_goal::GoalPauseReason::Authentication
                | zuno_goal::GoalPauseReason::Permission
                | zuno_goal::GoalPauseReason::TurnBudget
                | zuno_goal::GoalPauseReason::PlanMode
                | zuno_goal::GoalPauseReason::HumanInput
        )
    }) {
        return Ok(None);
    }
    let human_pending: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM human_request WHERE session_id=?1 AND state='pending')",
            [session_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if human_pending {
        return Ok(None);
    }
    Ok(Some(state))
}

fn real_user_input(input: &SessionInput) -> bool {
    input.error.is_none()
        && matches!(
            input.trigger_kind,
            InputTriggerKind::User | InputTriggerKind::Legacy
        )
        && matches!(
            DurableInputKind::classify(&input.prompt),
            Some(
                DurableInputKind::User
                    | DurableInputKind::AcpPrompt
                    | DurableInputKind::TuiPrompt
                    | DurableInputKind::HostMessage
            )
        )
}

fn candidate_in(
    tx: &zuno_db::Transaction<'_>,
    session_id: &str,
) -> Result<Option<DiscussionAdmission>, SessionControlError> {
    let Some(state) = eligible_state_in(tx, session_id)? else {
        return Ok(None);
    };
    let Some(cycle) = zuno_db::session_work_cycle::current_in(tx, session_id)? else {
        return Ok(None);
    };
    if cycle.active_turn_id.is_some()
        || cycle.stopped.is_some()
        || cycle.goal_id.is_some()
        || cycle.plan_id.is_some()
        || !cycle.todo_ids.is_empty()
        || !cycle.resumed_goal_cycles.is_empty()
    {
        return Ok(None);
    }
    let Some(message_id) = cycle.anchor_message_id.as_deref() else {
        return Ok(None);
    };
    let Some(input) = zuno_db::inbox::input_for_message_in(tx, session_id, message_id)? else {
        return Ok(None);
    };
    let input_id = input.id.as_str();
    if input.state != SubmissionState::Consumed
        || !real_user_input(&input)
        || input.cycle_id.as_deref() != Some(cycle.cycle_id.as_str())
    {
        return Ok(None);
    }
    let Some(receipt) = zuno_db::input_receipt::get_in(tx, session_id, input_id)? else {
        return Ok(None);
    };
    if receipt.state != InputReceiptState::Recorded
        || receipt.turn_id.is_some()
        || receipt.applied_at.is_some()
        || receipt.completed_at.is_some()
        || receipt.error.is_some()
    {
        return Ok(None);
    }
    if zuno_db::message::MessageStore::new(tx)
        .latest_user_message_id(session_id)?
        .as_deref()
        != Some(message_id)
    {
        return Ok(None);
    }
    let Some(token) = state.continuation.as_ref() else {
        return Ok(None);
    };
    if token.cycle_id != cycle.cycle_id
        || token.anchor_message_id.as_deref() != Some(message_id)
        || token.mode != CollaborationMode::Work
        || token.plan_id.is_some()
        || token.plan_revision.is_some()
        || state.work_identity.as_ref() != Some(&token.identity)
    {
        return Ok(None);
    }
    Ok(Some(DiscussionAdmission {
        session_id: session_id.to_owned(),
        input_id: input_id.to_owned(),
        cycle_id: cycle.cycle_id,
        execution_revision: state.revision,
        identity: token.identity.clone(),
    }))
}
