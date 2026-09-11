//! Durable collaboration mode, Plan handoff, and Work authorization.
//!
//! This crate is the single transaction boundary for the controls that used to
//! be spread across client-specific mode switches. It reads the exact Plan and
//! bound review revision, updates Goal state, freezes the execution identity,
//! and admits the control input before any of those facts become visible.

mod goal_resume;
mod question;
pub use goal_resume::GoalResumeOutcome;
pub use question::QuestionService;

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
    SessionExecutionPhase, SessionExecutionState, SessionReadiness, SessionScheduling,
    TurnExecutionIdentity,
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

/// Explicit resume of already-authorized Work, never a Plan authorization.
#[derive(Debug, Clone)]
pub struct ResumeWorkOutcome {
    pub state: SessionExecutionState,
    pub input: SessionInput,
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
    #[error("session `{session_id}` cannot resume: {detail}")]
    ResumeRejected { session_id: String, detail: String },
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
            state.scheduling = Some(SessionScheduling::default());
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

    /// Record an explicit selector's resolved identity without granting execution.
    /// Ephemeral panels remain unmaterialized; all existing wait/authorization
    /// and Goal state remains intact in either collaboration mode.
    pub fn record_work_selection(
        &self,
        session_id: &str,
        identity: TurnExecutionIdentity,
        at_ms: i64,
    ) -> Result<Option<SessionExecutionState>, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",
                    [session_id],
                    |row| row.get(0),
                )
                .map_err(zuno_db::map_error)?;
            if !exists {
                return Ok(None);
            }
            let mut state = seed_in(
                tx,
                session_id,
                CollaborationMode::Work,
                Some(identity.clone()),
                at_ms,
            )?;
            if state.work_identity.as_ref() != Some(&identity) {
                state.work_identity = Some(identity);
                state.time_updated = state.time_updated.max(at_ms);
                state = update_in(tx, state.revision, state)?;
            }
            Ok(Some(state))
        })
    }

    /// Mark the exact current Plan revision as ready for an explicit handoff.
    pub fn mark_plan_handoff(
        &self,
        session_id: &str,
        at_ms: i64,
    ) -> Result<SessionExecutionState, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let state = Self::mark_plan_handoff_in(&transaction, session_id, at_ms)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(state)
    }

    pub(crate) fn mark_plan_handoff_in(
        transaction: &rusqlite::Transaction<'_>,
        session_id: &str,
        at_ms: i64,
    ) -> Result<SessionExecutionState, SessionControlError> {
        let plan = WorkStateStore::plan_in(transaction, session_id)?.ok_or_else(|| {
            SessionControlError::MissingPlan {
                session_id: session_id.to_owned(),
            }
        })?;
        let mut state = read_in(transaction, session_id)?.ok_or_else(|| {
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
            || (state.phase != SessionExecutionPhase::Idle
                && state
                    .scheduling
                    .as_ref()
                    .is_none_or(|scheduling| scheduling.readiness == SessionReadiness::Ready))
        {
            let expected = state.revision;
            state.handoff_plan_id = Some(plan.id);
            state.handoff_plan_revision = Some(plan.revision);
            if state
                .scheduling
                .as_ref()
                .is_none_or(|scheduling| scheduling.readiness == SessionReadiness::Ready)
            {
                state.phase = SessionExecutionPhase::Idle;
            }
            state.time_updated = at_ms;
            state = update_in(transaction, expected, state)?;
        }
        Ok(state)
    }

    /// Atomically authorize Work for the exact handoff-ready Plan revision.
    pub fn start_work(
        &self,
        request: StartWorkRequest<'_>,
    ) -> Result<StartWorkOutcome, SessionControlError> {
        let connection = self.pool.get()?;
        let transaction = open::immediate_transaction(&connection)?;
        let outcome = Self::start_work_in(&transaction, request)?;
        transaction.commit().map_err(open::map_error)?;
        Ok(outcome)
    }

    pub(crate) fn start_work_in(
        transaction: &rusqlite::Transaction<'_>,
        request: StartWorkRequest<'_>,
    ) -> Result<StartWorkOutcome, SessionControlError> {
        let plan = WorkStateStore::plan_in(transaction, request.session_id)?.ok_or_else(|| {
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
        let mut state = read_in(transaction, request.session_id)?.ok_or_else(|| {
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
            transaction,
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
            transaction,
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
            GoalStore::goal_in(transaction, request.session_id)?
        } else {
            let expected = state.revision;
            state.mode = CollaborationMode::Work;
            state.authorized_plan_id = Some(plan.id.clone());
            state.authorized_plan_revision = Some(plan.revision);
            state.cycle_id = Some(continuation.cycle_id.clone());
            state.phase = SessionExecutionPhase::Authorized;
            // Only this explicit, revision-bound user control starts fresh work.
            state.scheduling = Some(SessionScheduling::default());
            state.continuation = Some(continuation);
            state.draft_review_risk = draft_review_risk;
            state.time_updated = request.at_ms;
            state = update_in(transaction, expected, state)?;
            GoalStore::resume_for_work_in(transaction, request.session_id, request.at_ms)?
        };
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

    /// Validate what the human actually saw, not merely the current mode label.
    pub(crate) fn validate_plan_question_in(
        transaction: &rusqlite::Transaction<'_>,
        question: &zuno_types::question::QuestionView,
    ) -> zuno_tool::question::QuestionResult<SessionExecutionState> {
        use zuno_tool::question::QuestionError;
        use zuno_types::question::QuestionPurpose;

        let binding = question
            .plan
            .as_ref()
            .filter(|_| question.purpose == QuestionPurpose::PlanAuthorization)
            .ok_or_else(|| {
                QuestionError::Invalid("question has no Plan authorization".to_owned())
            })?;
        let session_id = &question.origin.session_id;
        let state = read_in(transaction, session_id)?.ok_or_else(|| QuestionError::Rejected {
            code: "missing_execution_state",
            detail: "the session has no Plan execution state".to_owned(),
        })?;
        if state.mode != CollaborationMode::Plan
            && !(state.mode == CollaborationMode::Work
                && state.authorized_plan_id.as_deref() == Some(binding.plan_id.as_str())
                && state.authorized_plan_revision == Some(binding.plan_revision))
        {
            return Err(QuestionError::Rejected {
                code: "not_in_plan_mode",
                detail: "the session is no longer awaiting this Plan authorization".to_owned(),
            });
        }
        let plan = WorkStateStore::plan_in(transaction, session_id)
            .map_err(|error| question::control_error(SessionControlError::WorkState(error)))?
            .ok_or_else(|| QuestionError::Rejected {
                code: "missing_plan",
                detail: "the Plan no longer exists".to_owned(),
            })?;
        if plan.id != binding.plan_id || plan.revision != binding.plan_revision {
            return Err(QuestionError::Rejected {
                code: "stale_plan",
                detail:
                    "the Plan changed after this question was published; request fresh approval"
                        .to_owned(),
            });
        }
        if state.work_identity.as_ref() != Some(&binding.work_identity) {
            return Err(QuestionError::Rejected {
                code: "stale_work_identity",
                detail: "the Work Agent or model changed; request fresh approval".to_owned(),
            });
        }
        let review = ReviewStore::plan_review_gate_in(
            transaction,
            session_id,
            &binding.plan_id,
            binding.plan_revision,
        )
        .map_err(|error| question::control_error(SessionControlError::Review(error)))?;
        let review = serde_json::to_value(review)
            .map_err(|error| QuestionError::Invalid(error.to_string()))?;
        if review != binding.review_gate {
            return Err(QuestionError::Rejected {
                code: "stale_review",
                detail: "the bound review changed; request fresh approval".to_owned(),
            });
        }
        Ok(state)
    }

    /// Apply one explicitly approved, handoff-ready Plan question exactly once.
    pub(crate) fn apply_plan_question_in(
        transaction: &rusqlite::Transaction<'_>,
        question: &zuno_types::question::QuestionView,
        at_ms: i64,
    ) -> zuno_tool::question::QuestionResult<zuno_types::question::QuestionReceipt> {
        use zuno_tool::question::QuestionError;
        use zuno_types::question::{PlanAuthorizationState, PlanQuestionDecision, QuestionReceipt};

        if question.decision != Some(PlanQuestionDecision::Approve)
            || question.authorization != Some(PlanAuthorizationState::WaitingForHandoff)
            || !zuno_db::question::handoff_completed_in(transaction, &question.id)?
        {
            return Err(QuestionError::Rejected {
                code: "approval_not_ready",
                detail: "an explicit approval and successful source-turn handoff are required"
                    .to_owned(),
            });
        }
        let state = Self::validate_plan_question_in(transaction, question)?;
        let binding = question.plan.as_ref().expect("validated Plan binding");
        let risk_reason = zuno_db::question::risk_reason_in(transaction, &question.id)?;
        let outcome = Self::start_work_in(
            transaction,
            StartWorkRequest {
                session_id: &question.origin.session_id,
                // Handoff bookkeeping may legitimately advance this revision. The
                // exact Work identity and Plan/review were just checked under this lock.
                expected_execution_revision: Some(state.revision),
                expected_plan_revision: Some(binding.plan_revision),
                anchor_message_id: question.origin.message_id.clone(),
                draft_review_risk_reason: risk_reason,
                // This service queues control; only a host holding the run lease
                // may subsequently claim that execution has actually started.
                session_busy: true,
                at_ms,
            },
        )
        .map_err(question::control_error)?;
        let question = zuno_db::question::set_authorization_in(
            transaction,
            &question.origin.session_id,
            &question.id,
            PlanAuthorizationState::Applied,
            Some(&outcome.input.id),
        )?;
        Ok(QuestionReceipt {
            question,
            input_id: Some(outcome.input.id),
            duplicate: false,
        })
    }

    pub fn state(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionState>, SessionControlError> {
        let connection = self.pool.get()?;
        Ok(read_in(&connection, session_id)?)
    }

    /// An explicit Goal command resumes the Goal driver, not an ordinary queued
    /// Work control. Its current host supplies the Agent/model identity.
    pub fn resume_goal_execution(
        &self,
        session_id: &str,
        at_ms: i64,
    ) -> Result<(), SessionControlError> {
        self.pool.try_transaction(|tx| {
            if !GoalStore::goal_in(tx, session_id)?
                .is_some_and(|goal| goal.status == zuno_goal::GoalStatus::Active)
            {
                return Ok(());
            }
            let Some(state) = read_in(tx, session_id)? else {
                return Ok(());
            };
            if state.mode == CollaborationMode::Work
                && matches!(
                    state.scheduling.as_ref().map(|s| &s.readiness),
                    Some(SessionReadiness::Paused { .. } | SessionReadiness::Completed)
                )
            {
                zuno_db::session_execution::set_scheduling_in(
                    tx,
                    session_id,
                    state.revision,
                    SessionScheduling::default(),
                    at_ms,
                )?;
            }
            Ok(())
        })
    }

    /// Resume one exact paused Work revision and durably queue its control.
    /// Waiting for a specific human/external event is not waived by this action.
    pub fn resume_session(
        &self,
        session_id: &str,
        expected_revision: i64,
        at_ms: i64,
    ) -> Result<ResumeWorkOutcome, SessionControlError> {
        self.pool.try_transaction(|tx| {
            let rejected = |detail: &str| SessionControlError::ResumeRejected {
                session_id: session_id.to_owned(), detail: detail.to_owned(),
            };
            let mut state = read_in(tx, session_id)?.ok_or_else(|| rejected("no execution state exists"))?;
            let source_key = format!("user-control:resume-work:{expected_revision}");
            if let Some(input) = zuno_db::inbox::read_by_source_key_in(tx, session_id, &source_key)? {
                return Ok(ResumeWorkOutcome { state, input });
            }
            if state.revision != expected_revision {
                return Err(SessionControlError::ExecutionRevisionConflict {
                    session_id: session_id.to_owned(), expected: expected_revision, actual: state.revision,
                });
            }
            if state.mode != CollaborationMode::Work {
                return Err(rejected("use Start Work to authorize a Plan"));
            }
            if !matches!(state.scheduling.as_ref().map(|s| &s.readiness),
                Some(SessionReadiness::Paused { .. } | SessionReadiness::Completed))
            {
                return Err(rejected("work is not paused, or an exact human/external wait is still pending"));
            }
            if GoalStore::goal_in(tx, session_id)?.is_some_and(|goal| goal.status != zuno_goal::GoalStatus::Active) {
                return Err(rejected("resume the Goal explicitly first"));
            }
            let plan = WorkStateStore::plan_in(tx, session_id)?;
            if state.authorized_plan_id.is_some()
                && plan.as_ref().is_none_or(|plan| state.authorized_plan_id.as_deref() != Some(plan.id.as_str())
                    || state.authorized_plan_revision != Some(plan.revision))
            {
                return Err(rejected("the authorized Plan changed; obtain fresh Plan authorization"));
            }
            let continuation = ContinuationToken {
                cycle_id: state.cycle_id.clone().unwrap_or_else(|| format!("cycle_{}", Uuid::now_v7().simple())),
                identity: state.work_identity.clone().ok_or_else(|| rejected("the Work identity is missing"))?,
                mode: CollaborationMode::Work,
                plan_id: plan.as_ref().map(|plan| plan.id.clone()),
                plan_revision: plan.as_ref().map(|plan| plan.revision),
                context_epoch: state.continuation.as_ref().map_or(0, |token| token.context_epoch),
                anchor_message_id: state.continuation.as_ref().and_then(|token| token.anchor_message_id.clone()),
            };
            // Like explicit Goal resume, this user control acknowledges the
            // named uncertainty barrier. It never mechanically replays a tool.
            let messages = zuno_db::message::MessageStore::new(tx);
            let pending = messages.pending_uncertain_tool_calls(session_id, 0)?;
            let part_ids = pending.into_iter().map(|call| call.part_id).collect::<Vec<_>>();
            messages.reconcile_uncertain_tool_calls(&part_ids, at_ms)?;
            let input = admit_in(tx, NewSessionInput::new(
                format!("ctl_{}", Uuid::now_v7().simple()), session_id,
                json!({"kind":"sessionControl","control":"resume_work","continuation":continuation}),
                InputDelivery::Queue, at_ms,
            ).with_source_key(source_key).with_trigger_kind(InputTriggerKind::UserControl)
                .with_cycle_id(Some(continuation.cycle_id.clone())))?;
            state.scheduling = Some(SessionScheduling::default());
            state.phase = SessionExecutionPhase::Authorized;
            state.cycle_id = Some(continuation.cycle_id.clone());
            state.continuation = Some(continuation);
            state.time_updated = at_ms;
            let state = update_in(tx, expected_revision, state)?;
            Ok(ResumeWorkOutcome { state, input })
        })
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
        if state
            .scheduling
            .as_ref()
            .is_none_or(|scheduling| scheduling.readiness == SessionReadiness::Ready)
        {
            state.phase = SessionExecutionPhase::Running;
        }
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
