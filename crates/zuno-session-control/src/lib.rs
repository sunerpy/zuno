//! Durable collaboration mode, Plan handoff, and Work authorization.
//!
//! This crate is the single transaction boundary for the controls that used to
//! be spread across client-specific mode switches. It reads the exact Plan and
//! bound review revision, updates Goal state, freezes the execution identity,
//! and admits the control input before any of those facts become visible.

use std::sync::Arc;

use serde_json::json;
use thiserror::Error;
use uuid::Uuid;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInput, admit_in};
use zuno_db::session_execution::{read_in, seed_in, update_in};
use zuno_db::{Pool, open};
use zuno_goal::{Goal, GoalError, GoalStore};
use zuno_review::{PlanReviewGate, ReviewError, ReviewStore};
use zuno_tools::{WorkPlan, WorkStateError, WorkStateStore};
use zuno_types::execution::{
    CollaborationMode, ContinuationToken, DraftReviewRiskAcceptance, InputTriggerKind,
    SessionExecutionPhase, SessionExecutionState, TurnExecutionIdentity,
};

/// One user-visible Start Work disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartWorkDisposition {
    /// The caller owns an idle turn lease and may drive the control immediately.
    Started,
    /// Another turn is active; the durable control remains queued for its safe point.
    Queued,
}

/// Parameters that enter or refresh Plan mode.
#[derive(Debug, Clone)]
pub struct EnterPlanRequest<'a> {
    pub session_id: &'a str,
    /// The exact implementation identity to restore when Work is later authorized.
    pub work_identity: TurnExecutionIdentity,
    pub at_ms: i64,
}

/// Parameters for one Plan-to-Work authorization.
#[derive(Debug, Clone)]
pub struct StartWorkRequest<'a> {
    pub session_id: &'a str,
    /// Freeze the Work identity inspected by a client's execution preflight.
    pub expected_execution_revision: Option<i64>,
    pub expected_plan_revision: Option<i64>,
    pub anchor_message_id: Option<String>,
    pub draft_review_risk_reason: Option<String>,
    pub session_busy: bool,
    pub at_ms: i64,
}

/// Result of an atomic Start Work authorization.
#[derive(Debug, Clone)]
pub struct StartWorkOutcome {
    pub state: SessionExecutionState,
    pub plan: WorkPlan,
    pub goal: Option<Goal>,
    pub review_gate: PlanReviewGate,
    pub input: SessionInput,
    pub disposition: StartWorkDisposition,
}

#[derive(Debug, Error)]
pub enum SessionControlError {
    #[error(transparent)]
    Database(#[from] zuno_error::DbError),
    #[error(transparent)]
    Goal(#[from] GoalError),
    #[error(transparent)]
    Review(#[from] ReviewError),
    #[error(transparent)]
    WorkState(#[from] WorkStateError),
    #[error("session `{session_id}` is not in Plan mode; run /plan or /start-plan first")]
    NotInPlanMode { session_id: String },
    #[error("session `{session_id}` has no durable Plan to authorize")]
    MissingPlan { session_id: String },
    #[error(
        "Plan `{plan_id}` revision {actual} is stale for this request; expected revision {expected}"
    )]
    PlanRevisionConflict {
        plan_id: String,
        expected: i64,
        actual: i64,
    },
    #[error(
        "session `{session_id}` execution state changed during preflight: expected revision {expected}, found {actual}"
    )]
    ExecutionRevisionConflict {
        session_id: String,
        expected: i64,
        actual: i64,
    },
    #[error(
        "Plan `{plan_id}` revision {plan_revision} has no matching handoff-ready record; finish the Plan turn before Start Work"
    )]
    HandoffRequired { plan_id: String, plan_revision: i64 },
    #[error(
        "review `{review_id}` revision {review_revision} is still Draft; make it Ready or explicitly accept its risk"
    )]
    DraftReview {
        review_id: String,
        review_revision: i64,
    },
    #[error("session execution state for `{session_id}` is corrupt: {detail}")]
    CorruptState { session_id: String, detail: String },
}

/// Shared service used by ACP, TUI, and future clients.
#[derive(Clone)]
pub struct SessionControlService {
    pool: Arc<Pool>,
}

impl SessionControlService {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Enter Plan mode without changing the implementation Agent selection.
    ///
    /// Repeating the operation in Plan mode is idempotent unless the caller
    /// intentionally supplies a newer Work identity.
    pub fn enter_plan(
        &self,
        request: EnterPlanRequest<'_>,
    ) -> Result<SessionExecutionState, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut state = seed_in(
            &transaction,
            request.session_id,
            CollaborationMode::Work,
            Some(request.work_identity.clone()),
            request.at_ms,
        )?;
        let transition = state.mode != CollaborationMode::Plan;
        let identity_changed = state.work_identity.as_ref() != Some(&request.work_identity);
        if transition || identity_changed {
            let expected = state.revision;
            state.mode = CollaborationMode::Plan;
            state.work_identity = Some(request.work_identity);
            state.phase = SessionExecutionPhase::Planning;
            state.continuation = None;
            state.cycle_id = None;
            if transition {
                state.authorized_plan_id = None;
                state.authorized_plan_revision = None;
                state.handoff_plan_id = None;
                state.handoff_plan_revision = None;
                state.draft_review_risk = None;
            }
            state.time_updated = request.at_ms;
            state = update_in(&transaction, expected, state)?;
        }
        GoalStore::enter_plan_mode_in(&transaction, request.session_id, request.at_ms)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(state)
    }

    /// Update the implementation identity while preserving Plan mode.
    ///
    /// Agent, provider, model, and reasoning selectors call this instead of
    /// implicitly leaving Plan mode.
    pub fn update_work_identity(
        &self,
        session_id: &str,
        identity: TurnExecutionIdentity,
        at_ms: i64,
    ) -> Result<SessionExecutionState, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut state = read_in(&transaction, session_id)?.ok_or_else(|| {
            SessionControlError::NotInPlanMode {
                session_id: session_id.to_owned(),
            }
        })?;
        if state.mode != CollaborationMode::Plan {
            return Err(SessionControlError::NotInPlanMode {
                session_id: session_id.to_owned(),
            });
        }
        if state.work_identity.as_ref() != Some(&identity) {
            let expected = state.revision;
            state.work_identity = Some(identity);
            state.time_updated = at_ms;
            state = update_in(&transaction, expected, state)?;
        }
        transaction.commit().map_err(open::map_error)?;
        Ok(state)
    }

    /// Mark the exact current Plan revision as ready for an explicit handoff.
    pub fn mark_plan_handoff(
        &self,
        session_id: &str,
        at_ms: i64,
    ) -> Result<SessionExecutionState, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let plan = WorkStateStore::plan_in(&transaction, session_id)?.ok_or_else(|| {
            SessionControlError::MissingPlan {
                session_id: session_id.to_owned(),
            }
        })?;
        let mut state = read_in(&transaction, session_id)?.ok_or_else(|| {
            SessionControlError::NotInPlanMode {
                session_id: session_id.to_owned(),
            }
        })?;
        if state.mode != CollaborationMode::Plan {
            return Err(SessionControlError::NotInPlanMode {
                session_id: session_id.to_owned(),
            });
        }
        if state.handoff_plan_id.as_deref() != Some(plan.id.as_str())
            || state.handoff_plan_revision != Some(plan.revision)
            || state.phase != SessionExecutionPhase::Idle
        {
            let expected = state.revision;
            state.handoff_plan_id = Some(plan.id);
            state.handoff_plan_revision = Some(plan.revision);
            state.phase = SessionExecutionPhase::Idle;
            state.time_updated = at_ms;
            state = update_in(&transaction, expected, state)?;
        }
        transaction.commit().map_err(open::map_error)?;
        Ok(state)
    }

    /// Atomically authorize Work for the exact handoff-ready Plan revision.
    pub fn start_work(
        &self,
        request: StartWorkRequest<'_>,
    ) -> Result<StartWorkOutcome, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let plan = WorkStateStore::plan_in(&transaction, request.session_id)?.ok_or_else(|| {
            SessionControlError::MissingPlan {
                session_id: request.session_id.to_owned(),
            }
        })?;
        if let Some(expected) = request.expected_plan_revision
            && expected != plan.revision
        {
            return Err(SessionControlError::PlanRevisionConflict {
                plan_id: plan.id,
                expected,
                actual: plan.revision,
            });
        }
        let mut state = read_in(&transaction, request.session_id)?.ok_or_else(|| {
            SessionControlError::NotInPlanMode {
                session_id: request.session_id.to_owned(),
            }
        })?;
        if let Some(expected) = request.expected_execution_revision
            && expected != state.revision
        {
            return Err(SessionControlError::ExecutionRevisionConflict {
                session_id: request.session_id.to_owned(),
                expected,
                actual: state.revision,
            });
        }
        let already_authorized = state.mode == CollaborationMode::Work
            && state.authorized_plan_id.as_deref() == Some(plan.id.as_str())
            && state.authorized_plan_revision == Some(plan.revision)
            && state.continuation.is_some();
        if !already_authorized && state.mode != CollaborationMode::Plan {
            return Err(SessionControlError::NotInPlanMode {
                session_id: request.session_id.to_owned(),
            });
        }
        if !already_authorized
            && (state.handoff_plan_id.as_deref() != Some(plan.id.as_str())
                || state.handoff_plan_revision != Some(plan.revision))
        {
            return Err(SessionControlError::HandoffRequired {
                plan_id: plan.id,
                plan_revision: plan.revision,
            });
        }

        let review_gate = ReviewStore::plan_review_gate_in(
            &transaction,
            request.session_id,
            &plan.id,
            plan.revision,
        )?;
        let draft_review_risk = match &review_gate {
            PlanReviewGate::Draft {
                review_id,
                review_revision,
            } => {
                if already_authorized {
                    state.draft_review_risk.clone()
                } else {
                    let reason = request
                        .draft_review_risk_reason
                        .as_deref()
                        .map(str::trim)
                        .filter(|reason| !reason.is_empty())
                        .ok_or_else(|| SessionControlError::DraftReview {
                            review_id: review_id.clone(),
                            review_revision: *review_revision,
                        })?;
                    Some(DraftReviewRiskAcceptance {
                        review_id: review_id.clone(),
                        review_revision: *review_revision,
                        reason: reason.to_owned(),
                        time_accepted: request.at_ms,
                    })
                }
            }
            PlanReviewGate::Unbound | PlanReviewGate::Ready { .. } => None,
        };

        let identity =
            state
                .work_identity
                .clone()
                .ok_or_else(|| SessionControlError::CorruptState {
                    session_id: request.session_id.to_owned(),
                    detail: "Plan mode has no saved Work identity".to_owned(),
                })?;
        let continuation = if already_authorized {
            state
                .continuation
                .clone()
                .expect("already-authorized state checked above")
        } else {
            ContinuationToken {
                cycle_id: format!("cycle_{}", Uuid::now_v7().simple()),
                identity,
                mode: CollaborationMode::Work,
                plan_id: Some(plan.id.clone()),
                plan_revision: Some(plan.revision),
                context_epoch: state
                    .continuation
                    .as_ref()
                    .map_or(0, |token| token.context_epoch),
                anchor_message_id: request.anchor_message_id.clone(),
            }
        };
        let source_key = format!("user-control:start-work:{}:{}", plan.id, plan.revision);
        let prompt = json!({
            "kind": "sessionControl",
            "control": "start_work",
            "planID": plan.id.clone(),
            "planRevision": plan.revision,
            "cycleID": continuation.cycle_id.clone(),
            "continuation": continuation.clone(),
        });
        let input = admit_in(
            &transaction,
            NewSessionInput::new(
                format!("ctl_{}", Uuid::now_v7().simple()),
                request.session_id,
                prompt,
                InputDelivery::Queue,
                request.at_ms,
            )
            .with_source_key(source_key)
            .with_trigger_kind(InputTriggerKind::UserControl)
            .with_cycle_id(Some(continuation.cycle_id.clone())),
        )?;

        let goal = if already_authorized {
            GoalStore::goal_in(&transaction, request.session_id)?
        } else {
            let expected = state.revision;
            state.mode = CollaborationMode::Work;
            state.authorized_plan_id = Some(plan.id.clone());
            state.authorized_plan_revision = Some(plan.revision);
            state.cycle_id = Some(continuation.cycle_id.clone());
            state.phase = SessionExecutionPhase::Authorized;
            state.continuation = Some(continuation);
            state.draft_review_risk = draft_review_risk;
            state.time_updated = request.at_ms;
            state = update_in(&transaction, expected, state)?;
            GoalStore::resume_for_work_in(&transaction, request.session_id, request.at_ms)?
        };
        transaction.commit().map_err(open::map_error)?;
        Ok(StartWorkOutcome {
            state,
            plan,
            goal,
            review_gate,
            input,
            disposition: if request.session_busy {
                StartWorkDisposition::Queued
            } else {
                StartWorkDisposition::Started
            },
        })
    }

    pub fn state(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionState>, SessionControlError> {
        let connection = self.pool.get()?;
        Ok(read_in(&connection, session_id)?)
    }

    /// Persist a same-cycle recovery token after compaction or process recovery.
    ///
    /// This does not authorize Work. It preserves the existing collaboration
    /// boundary and only advances `context_epoch`, so a Plan turn remains
    /// read-only and an unauthorized Work turn cannot manufacture authority.
    #[allow(
        clippy::too_many_arguments,
        reason = "recovery authority freezes every identity and Plan coordinate at the transaction boundary"
    )]
    pub fn record_recovery(
        &self,
        session_id: &str,
        cycle_id: &str,
        identity: TurnExecutionIdentity,
        mode: CollaborationMode,
        plan_id: Option<String>,
        plan_revision: Option<i64>,
        anchor_message_id: Option<String>,
        at_ms: i64,
    ) -> Result<ContinuationToken, SessionControlError> {
        self.persist_continuation(
            session_id,
            cycle_id,
            identity,
            mode,
            plan_id,
            plan_revision,
            anchor_message_id,
            true,
            at_ms,
        )
    }

    /// Persist an ordinary same-cycle continuation without advancing context epoch.
    #[allow(
        clippy::too_many_arguments,
        reason = "ordinary continuation authority shares the complete recovery coordinate set"
    )]
    pub fn record_continuation(
        &self,
        session_id: &str,
        cycle_id: &str,
        identity: TurnExecutionIdentity,
        mode: CollaborationMode,
        plan_id: Option<String>,
        plan_revision: Option<i64>,
        anchor_message_id: Option<String>,
        at_ms: i64,
    ) -> Result<ContinuationToken, SessionControlError> {
        self.persist_continuation(
            session_id,
            cycle_id,
            identity,
            mode,
            plan_id,
            plan_revision,
            anchor_message_id,
            false,
            at_ms,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one private transaction helper persists the complete continuation contract atomically"
    )]
    fn persist_continuation(
        &self,
        session_id: &str,
        cycle_id: &str,
        identity: TurnExecutionIdentity,
        mode: CollaborationMode,
        plan_id: Option<String>,
        plan_revision: Option<i64>,
        anchor_message_id: Option<String>,
        advance_context_epoch: bool,
        at_ms: i64,
    ) -> Result<ContinuationToken, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let mut state = seed_in(
            &transaction,
            session_id,
            mode,
            Some(identity.clone()),
            at_ms,
        )?;
        if state.mode != mode {
            return Err(SessionControlError::CorruptState {
                session_id: session_id.to_owned(),
                detail: format!(
                    "recovery requested mode `{}` while durable mode is `{}`",
                    mode.as_str(),
                    state.mode.as_str()
                ),
            });
        }
        if mode == CollaborationMode::Work
            && plan_id.is_some()
            && state.authorized_plan_id.is_some()
            && (state.authorized_plan_id.as_deref() != plan_id.as_deref()
                || state.authorized_plan_revision != plan_revision)
        {
            return Err(SessionControlError::CorruptState {
                session_id: session_id.to_owned(),
                detail: "recovery Plan does not match durable Work authorization".to_owned(),
            });
        }
        let current_epoch = state
            .continuation
            .as_ref()
            .map_or(0, |token| token.context_epoch);
        let context_epoch = if advance_context_epoch {
            current_epoch.saturating_add(1)
        } else {
            current_epoch
        };
        let token = ContinuationToken {
            cycle_id: cycle_id.to_owned(),
            identity,
            mode,
            plan_id,
            plan_revision,
            context_epoch,
            anchor_message_id,
        };
        let expected = state.revision;
        state.cycle_id = Some(cycle_id.to_owned());
        state.phase = SessionExecutionPhase::Running;
        state.continuation = Some(token.clone());
        state.time_updated = at_ms;
        update_in(&transaction, expected, state)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(token)
    }

    /// Whether the current Work mode authorizes the exact visible Plan revision.
    pub fn work_authorized(
        &self,
        session_id: &str,
        plan: Option<(&str, i64)>,
    ) -> Result<bool, SessionControlError> {
        let Some(state) = self.state(session_id)? else {
            return Ok(false);
        };
        if state.mode != CollaborationMode::Work {
            return Ok(false);
        }
        Ok(match plan {
            Some((id, revision)) => {
                state.authorized_plan_id.as_deref() == Some(id)
                    && state.authorized_plan_revision == Some(revision)
            }
            None => state.authorized_plan_id.is_none(),
        })
    }
}
