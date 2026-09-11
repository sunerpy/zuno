//! Explicit Goal consent shares a transaction with execution and input routing.

use rusqlite::Transaction;
use serde_json::json;
use uuid::Uuid;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInput};
use zuno_goal::{Goal, GoalPauseReason, GoalStatus, GoalStore};
use zuno_types::execution::{
    CollaborationMode, ContinuationToken, InputTriggerKind, SessionExecutionPhase,
    SessionExecutionState, SessionPauseReason, SessionReadiness, SessionScheduling,
};
use zuno_types::goal_resume::GoalResumeRequest;

use crate::{SessionControlError, SessionControlService};

#[derive(Debug, Clone)]
pub struct GoalResumeOutcome {
    pub goal: Goal,
    pub state: SessionExecutionState,
    pub input: Option<SessionInput>,
}

impl SessionControlService {
    /// Explicit native command recovery may supply the already resolved host
    /// identity for a legacy session. A wire reply cannot select this identity.
    pub fn resume_goal_with_host_identity(
        &self,
        request: &GoalResumeRequest,
        mode: CollaborationMode,
        identity: zuno_types::execution::TurnExecutionIdentity,
        at_ms: i64,
    ) -> Result<GoalResumeOutcome, SessionControlError> {
        self.pool.try_transaction(|tx| {
            request
                .validate()
                .map_err(|error| rejected(&request.session_id, &error.to_string()))?;
            let goal = GoalStore::goal_in(tx, &request.session_id)?
                .ok_or_else(|| rejected(&request.session_id, "the Goal no longer exists"))?;
            if goal.goal_id != request.goal_id || goal.revision != request.expected_revision {
                return Err(rejected(
                    &request.session_id,
                    "the Goal changed before explicit resume",
                ));
            }
            if mode != CollaborationMode::Work {
                return Err(rejected(
                    &request.session_id,
                    "use Start Work to authorize Plan mode",
                ));
            }
            let mut state = zuno_db::session_execution::seed_in(
                tx,
                &request.session_id,
                mode,
                Some(identity.clone()),
                at_ms,
            )?;
            if state.mode == CollaborationMode::Work
                && state.work_identity.as_ref() != Some(&identity)
            {
                state.work_identity = Some(identity.clone());
                state.time_updated = state.time_updated.max(at_ms);
                zuno_db::session_execution::update_in(tx, state.revision, state)?;
            }
            Self::resume_goal_in(tx, request, at_ms)
        })
    }

    pub fn resume_goal(
        &self,
        request: &GoalResumeRequest,
        at_ms: i64,
    ) -> Result<GoalResumeOutcome, SessionControlError> {
        self.pool
            .try_transaction(|tx| Self::resume_goal_in(tx, request, at_ms))
    }

    pub(crate) fn validate_goal_resume_in(
        tx: &Transaction<'_>,
        request: &GoalResumeRequest,
    ) -> Result<(Goal, SessionExecutionState), SessionControlError> {
        let reject = |detail: &str| rejected(&request.session_id, detail);
        request
            .validate()
            .map_err(|error| reject(&error.to_string()))?;
        let goal = GoalStore::goal_in(tx, &request.session_id)?
            .ok_or_else(|| reject("the Goal no longer exists"))?;
        if goal.goal_id != request.goal_id || goal.revision != request.expected_revision {
            return Err(reject(
                "the Goal changed; request fresh explicit resume consent",
            ));
        }
        if !matches!(
            goal.status,
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::Active
        ) {
            return Err(reject(
                "a completed or budget-limited Goal cannot be resumed",
            ));
        }
        let state = zuno_db::session_execution::read_in(tx, &request.session_id)?
            .ok_or_else(|| reject("the session has no saved execution identity"))?;
        if state.mode != CollaborationMode::Work || state.work_identity.is_none() {
            return Err(reject(
                "use Start Work to authorize the Plan before resuming its Goal",
            ));
        }
        if let Some(pause) = GoalStore::pause_state_in(tx, &request.session_id)? {
            if pause.goal_id != goal.goal_id {
                return Err(reject("the pause belongs to a replaced Goal"));
            }
            match pause.reason {
                GoalPauseReason::UserInterruption | GoalPauseReason::NoProgress => {}
                GoalPauseReason::HumanInput => {
                    let answered = pause
                        .human_request_id
                        .as_deref()
                        .map(|id| zuno_db::human_request::get_from(tx, id))
                        .transpose()?
                        .flatten()
                        .is_some_and(|human| {
                            human.goal_id.as_deref() == Some(goal.goal_id.as_str())
                                && human.state
                                    == zuno_db::human_request::HumanRequestState::Answered
                        });
                    if !answered {
                        return Err(reject("the required human request is still unanswered"));
                    }
                }
                GoalPauseReason::PlanMode => {
                    return Err(reject("use Start Work to settle Plan authorization"));
                }
                GoalPauseReason::Permission
                | GoalPauseReason::Authentication
                | GoalPauseReason::UncertainSideEffect
                | GoalPauseReason::TurnBudget => {
                    return Err(reject(
                        "resolve the permission, authentication, uncertainty or budget barrier through its owning control before resuming",
                    ));
                }
            }
        }
        if matches!(
            state
                .scheduling
                .as_ref()
                .map(|scheduling| &scheduling.readiness),
            Some(SessionReadiness::Paused {
                reason: SessionPauseReason::Authentication
                    | SessionPauseReason::TurnBudget
                    | SessionPauseReason::Blocked
            })
        ) {
            return Err(reject("the session still has a protected recovery barrier"));
        }
        if !zuno_db::message::MessageStore::new(tx)
            .pending_uncertain_tool_calls(&request.session_id, 0)?
            .is_empty()
        {
            return Err(reject(
                "inspect and settle uncertain tool outcomes before resuming",
            ));
        }
        if let Some(input_id) = &request.input_id {
            let input = zuno_db::inbox::read_in(tx, &request.session_id, input_id)?
                .ok_or_else(|| reject("the input associated with this choice no longer exists"))?;
            if matches!(
                input.state,
                zuno_db::inbox::SubmissionState::Cancelled
                    | zuno_db::inbox::SubmissionState::Failed
            ) {
                return Err(reject(
                    "the input associated with this choice was withdrawn or failed",
                ));
            }
        }
        Ok((goal, state))
    }

    pub(crate) fn resume_goal_in(
        tx: &Transaction<'_>,
        request: &GoalResumeRequest,
        at_ms: i64,
    ) -> Result<GoalResumeOutcome, SessionControlError> {
        let (goal, mut state) = Self::validate_goal_resume_in(tx, request)?;
        if goal.status == GoalStatus::Active {
            return Ok(GoalResumeOutcome {
                goal,
                state,
                input: None,
            });
        }
        let goal = GoalStore::resume_explicit_in(
            tx,
            &request.session_id,
            request.expected_revision,
            at_ms,
        )?
        .ok_or_else(|| rejected(&request.session_id, "the Goal changed during resume"))?;
        if goal.status != GoalStatus::Active {
            return Err(rejected(
                &request.session_id,
                "the Goal budget must be raised before resuming",
            ));
        }
        let waiting = state.scheduling.as_ref().is_some_and(|scheduling| {
            matches!(
                scheduling.readiness,
                SessionReadiness::WaitingHuman { .. } | SessionReadiness::WaitingExternal { .. }
            )
        });
        let plan = zuno_tools::WorkStateStore::plan_in(tx, &request.session_id)?;
        if state.authorized_plan_id.is_some()
            && plan.as_ref().is_none_or(|plan| {
                state.authorized_plan_id.as_deref() != Some(plan.id.as_str())
                    || state.authorized_plan_revision != Some(plan.revision)
            })
        {
            return Err(rejected(&request.session_id, "the authorized Plan changed"));
        }
        let original_input = request
            .input_id
            .as_deref()
            .map(|id| zuno_db::inbox::read_in(tx, &request.session_id, id))
            .transpose()?
            .flatten();
        let still_processing = request
            .input_id
            .as_deref()
            .map(|id| zuno_db::input_receipt::get_in(tx, &request.session_id, id))
            .transpose()?
            .flatten()
            .is_some_and(|receipt| {
                receipt.state == zuno_types::admission::InputReceiptState::Applied
            });
        let input = if waiting {
            // Resuming a Goal is not a human answer or an external completion.
            None
        } else {
            let continuation = ContinuationToken {
                cycle_id: state
                    .cycle_id
                    .clone()
                    .unwrap_or_else(|| format!("cycle_{}", Uuid::now_v7().simple())),
                identity: state.work_identity.clone().expect("validated identity"),
                mode: CollaborationMode::Work,
                plan_id: plan.as_ref().map(|plan| plan.id.clone()),
                plan_revision: plan.as_ref().map(|plan| plan.revision),
                context_epoch: state
                    .continuation
                    .as_ref()
                    .map_or(0, |token| token.context_epoch),
                anchor_message_id: request.input_id.clone().or_else(|| {
                    state
                        .continuation
                        .as_ref()
                        .and_then(|token| token.anchor_message_id.clone())
                }),
            };
            let input = match original_input.filter(|input| input.state.is_pending() || still_processing) {
                Some(input) => input,
                None => zuno_db::inbox::admit_in(tx, NewSessionInput::new(
                    format!("ctl_{}", Uuid::now_v7().simple()),
                    &request.session_id,
                    json!({"kind":"sessionControl","control":"resume_work","continuation":continuation}),
                    InputDelivery::Queue, at_ms,
                ).with_source_key(format!("user-control:resume-goal:{}:{}",request.goal_id,request.expected_revision))
                 .with_trigger_kind(InputTriggerKind::UserControl)
                 .with_cycle_id(Some(continuation.cycle_id.clone())))?,
            };
            state.scheduling = Some(SessionScheduling::default());
            if state.phase != SessionExecutionPhase::Running {
                state.phase = SessionExecutionPhase::Authorized;
            }
            state.cycle_id = Some(continuation.cycle_id.clone());
            state.continuation = Some(continuation);
            state.time_updated = at_ms.max(state.time_updated);
            state = zuno_db::session_execution::update_in(tx, state.revision, state)?;
            Some(input)
        };
        zuno_db::event_log::append_in(
            tx,
            &request.session_id,
            zuno_db::event_log::NewSessionEvent::new(
                "session.goal.resumed",
                json!({"goalId":goal.goal_id,"revision":goal.revision,
                       "previousRevision":request.expected_revision,
                       "inputId":input.as_ref().map(|input| &input.id),
                       "waiting":waiting})
                .as_object()
                .expect("event object")
                .clone(),
            )?,
        )?;
        Ok(GoalResumeOutcome { goal, state, input })
    }
}

fn rejected(session_id: &str, detail: &str) -> SessionControlError {
    SessionControlError::ResumeRejected {
        session_id: session_id.to_owned(),
        detail: detail.to_owned(),
    }
}
