//! One durable question service, independent of presentation and tool waiting.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::Transaction;
use tokio::sync::broadcast;
use uuid::Uuid;
use zuno_db::Pool;
use zuno_db::inbox::{SessionInbox, read_in};
use zuno_db::question::{self, QuestionStore};
use zuno_engine::admission::{SessionInputAdmission, SteeringContent, TurnLease};
use zuno_engine::status::SessionRunRegistry;
use zuno_goal::{GoalError, GoalStatus, GoalStore};
use zuno_review::ReviewStore;
use zuno_tool::InterruptHandle;
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_tools::WorkStateStore;
use zuno_types::execution::{
    CollaborationMode, SessionPauseReason, SessionReadiness, SessionWaitReference,
};
use zuno_types::goal_resume::{GoalResumeRequest, KEEP_GOAL_PAUSED_CHOICE, RESUME_GOAL_CHOICE};
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionBinding, PlanQuestionDecision, QuestionAction,
    QuestionCommand, QuestionMode, QuestionOption, QuestionOrigin, QuestionPurpose,
    QuestionReceipt, QuestionRequest, QuestionSpec, QuestionState, QuestionView,
};

use crate::{SessionControlError, SessionControlService};

/// Session-owned notifications follow the authoritative database commit.
///
/// The same instance is shared by the tool registry and its client adapters.
/// Disconnected consumers can reconstruct pending state without reviving a tool.
#[derive(Clone)]
pub struct QuestionService {
    pool: Arc<Pool>,
    store: QuestionStore,
    changes: broadcast::Sender<QuestionReceipt>,
    runs: Option<SessionRunRegistry>,
}

impl QuestionService {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        let (changes, _) = broadcast::channel(128);
        Self {
            store: QuestionStore::new(Arc::clone(&pool)),
            pool,
            changes,
            runs: None,
        }
    }

    #[must_use]
    pub fn with_runs(mut self, runs: SessionRunRegistry) -> Self {
        self.runs = Some(runs);
        self
    }

    pub fn subscribe(&self) -> broadcast::Receiver<QuestionReceipt> {
        self.changes.subscribe()
    }

    /// Offer consent without changing Goal, execution, or input state.
    ///
    /// Like Codex's paused-Goal menu this is a host-owned choice, not an
    /// inference from the user's next text. Zuno additionally freezes the Goal
    /// revision and original durable input because replies may arrive later.
    pub async fn offer_goal_resume(
        &self,
        session_id: &str,
        input_id: Option<&str>,
    ) -> QuestionResult<Option<QuestionReceipt>> {
        let pool = Arc::clone(&self.pool);
        let session_id = session_id.to_owned();
        let input_id = input_id.map(str::to_owned);
        let receipt = blocking(move || {
            pool.try_transaction(|tx| {
                let Some(goal) = GoalStore::goal_in(tx, &session_id)
                    .map_err(goal_error)?
                    .filter(|goal| matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked))
                else {
                    return Ok(None);
                };
                let binding = GoalResumeRequest {
                    session_id: session_id.clone(),
                    goal_id: goal.goal_id.clone(),
                    expected_revision: goal.revision,
                    input_id: input_id.clone(),
                };
                match SessionControlService::validate_goal_resume_in(tx, &binding) {
                    Ok(_) => {}
                    Err(SessionControlError::ResumeRejected { .. }) => return Ok(None),
                    Err(error) => return Err(control_error(error)),
                }
                if let Some(previous) = previous_goal_resume_in(tx, &binding)? {
                    return Ok((!previous.state.is_terminal()).then_some(QuestionReceipt {
                        question: previous,
                        input_id: None,
                        duplicate: true,
                    }));
                }
                let spec = QuestionSpec {
                    origin: QuestionOrigin {
                        session_id: session_id.clone(),
                        message_id: input_id.clone(),
                        call_id: None,
                        turn_id: None,
                        goal_id: Some(goal.goal_id),
                    },
                    mode: QuestionMode::Deferred,
                    purpose: QuestionPurpose::GoalResume,
                    questions: vec![QuestionRequest::closed(
                        format!(
                            "Resume paused Goal \"{}\"? Skipping keeps it paused.",
                            goal.objective
                        ),
                        "Resume Goal",
                        vec![
                            QuestionOption::new(
                                RESUME_GOAL_CHOICE,
                                "Resume this Goal when its execution gates allow",
                            ),
                            QuestionOption::new(
                                KEEP_GOAL_PAUSED_CHOICE,
                                "Keep it paused; /goal resume remains available",
                            ),
                        ],
                    )],
                    expected_goal_revision: Some(goal.revision),
                    plan: None,
                };
                question::create_in(
                    tx,
                    &format!("que_{}", Uuid::now_v7().simple()),
                    &spec,
                    zuno_db::message::now_millis(),
                )
                .map(Some)
            })
        })
        .await?;
        if let Some(receipt) = &receipt {
            self.after_commit(receipt.clone());
        }
        Ok(receipt)
    }

    /// The host calls this only after the source Plan turn completed normally.
    /// An earlier approval is applied inside the same handoff transaction.
    pub async fn complete_plan_turn(&self, session_id: &str, turn_id: &str) -> QuestionResult<()> {
        let pool = Arc::clone(&self.pool);
        let session_id = session_id.to_owned();
        let turn_id = turn_id.to_owned();
        let receipts = blocking(move || {
            pool.try_transaction(|tx| {
                let now = zuno_db::message::now_millis();
                if zuno_db::session_execution::read_in(tx, &session_id)?
                    .is_some_and(|state| state.mode == CollaborationMode::Plan)
                {
                    SessionControlService::mark_plan_handoff_in(tx, &session_id, now)
                        .map_err(control_error)?;
                }
                let ids = question::mark_handoff_in(tx, &session_id, &turn_id)?;
                let mut receipts = Vec::new();
                for id in ids {
                    let view = question::get_in(tx, &session_id, &id)?;
                    if view.decision != Some(PlanQuestionDecision::Approve)
                        || view.authorization != Some(PlanAuthorizationState::WaitingForHandoff)
                    {
                        continue;
                    }
                    let receipt =
                        match SessionControlService::apply_plan_question_in(tx, &view, now) {
                            Ok(receipt) => receipt,
                            Err(
                                QuestionError::Rejected { .. } | QuestionError::Conflict { .. },
                            ) => QuestionReceipt {
                                question: question::set_authorization_in(
                                    tx,
                                    &session_id,
                                    &id,
                                    PlanAuthorizationState::Invalidated,
                                    None,
                                )?,
                                input_id: None,
                                duplicate: false,
                            },
                            Err(error) => return Err(error),
                        };
                    receipts.push(receipt);
                }
                Ok(receipts)
            })
        })
        .await?;
        for receipt in receipts {
            self.after_commit(receipt);
        }
        Ok(())
    }

    /// Abort or failure can revoke an unapplied approval, never create authority.
    pub async fn interrupt_plan_turn(&self, session_id: &str, turn_id: &str) -> QuestionResult<()> {
        let pool = Arc::clone(&self.pool);
        let session_id = session_id.to_owned();
        let turn_id = turn_id.to_owned();
        let changed = blocking(move || {
            pool.try_transaction(|tx| {
                let cycle_id = zuno_db::session_execution::read_in(tx, &session_id)?
                    .and_then(|state| state.cycle_id);
                let mut statement = tx
                    .prepare(
                        "SELECT h.id FROM human_request h \
                     JOIN question_interaction q ON q.request_id=h.id \
                     WHERE h.session_id=?1 AND q.purpose='plan_authorization' \
                     AND (json_extract(q.definition,'$.origin.turnId')=?2 \
                          OR (?3 IS NOT NULL AND json_extract(q.definition,'$.plan.sourceCycleId')=?3 \
                              AND COALESCE(json_extract(q.definition,'$.handoffCompleted'),0)=0)) \
                     AND (q.authorization IS NULL OR q.authorization='waiting_for_handoff')",
                    )
                    .map_err(zuno_db::map_error)?;
                let ids = statement
                    .query_map(rusqlite::params![session_id, turn_id, cycle_id], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(zuno_db::map_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(zuno_db::map_error)?;
                drop(statement);
                let mut changed = Vec::new();
                for id in ids {
                    zuno_db::human_request::resolve_in(
                        tx,
                        &id,
                        zuno_db::human_request::HumanRequestState::Cancelled,
                        Some(&serde_json::json!({"outcome":"source_turn_interrupted"})),
                        zuno_db::message::now_millis(),
                    )?;
                    let view = question::set_authorization_in(
                        tx,
                        &session_id,
                        &id,
                        PlanAuthorizationState::Invalidated,
                        None,
                    )?;
                    settle_wait_in(tx, &view, zuno_db::message::now_millis())?;
                    changed.push(view);
                }
                Ok(changed)
            })
        })
        .await?;
        for question in changed {
            let _ = self.changes.send(QuestionReceipt {
                question,
                input_id: None,
                duplicate: false,
            });
        }
        Ok(())
    }

    fn after_commit(&self, receipt: QuestionReceipt) {
        // Notification is repeatable; durable input IDs/revisions fence consumption.
        // A missed notification must not turn a successful transaction into an error.
        let _ = self.changes.send(receipt.clone());
        if !(receipt.question.purpose == QuestionPurpose::PlanAuthorization
            && receipt.question.decision == Some(PlanQuestionDecision::Approve))
            && let (Some(runs), Some(input_id)) = (&self.runs, receipt.input_id.as_deref())
        {
            let routed = (|| -> QuestionResult<()> {
                let connection = self.pool.get()?;
                if let Some(input) =
                    read_in(&connection, &receipt.question.origin.session_id, input_id)?
                {
                    let content = input
                        .prompt
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    drop(connection);
                    if let Some(content) = content {
                        let admission = SessionInputAdmission::new(
                            SessionInbox::new(Arc::clone(&self.pool)),
                            runs.clone(),
                        );
                        let _ = admission.route_admitted(
                            input,
                            TurnLease::Deferred,
                            Some(SteeringContent::user(content)),
                        );
                    }
                }
                Ok(())
            })();
            if let Err(error) = routed {
                tracing::warn!(%error, request_id = %receipt.question.id,
                    "question committed; live delivery will recover from the durable inbox");
            }
        }
    }
}

#[async_trait]
impl QuestionPort for QuestionService {
    async fn open(&self, mut spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        let pool = Arc::clone(&self.pool);
        let receipt = blocking(move || {
            pool.try_transaction(|tx| {
                if spec.purpose == QuestionPurpose::PlanAuthorization {
                    prepare_plan_spec(tx, &mut spec)?;
                    for question in
                        question::active_plan_authorizations_in(tx, &spec.origin.session_id)?
                    {
                        if question.plan == spec.plan {
                            return Ok(QuestionReceipt {
                                question,
                                input_id: None,
                                duplicate: true,
                            });
                        }
                        supersede_plan_question_in(tx, &question)?;
                    }
                }
                if spec.purpose == QuestionPurpose::RequiredInput {
                    if let Some(goal) =
                        GoalStore::goal_in(tx, &spec.origin.session_id).map_err(goal_error)?
                    {
                        if spec.expected_goal_revision.is_none() {
                            return Err(QuestionError::Rejected {
                                code: "goal_revision_required",
                                detail:
                                    "Goal-owned required input must name the observed Goal revision"
                                        .to_owned(),
                            });
                        }
                        if spec
                            .origin
                            .goal_id
                            .as_ref()
                            .is_some_and(|id| *id != goal.goal_id)
                        {
                            return Err(QuestionError::Rejected {
                                code: "stale_goal",
                                detail: "the question belongs to a replaced Goal".to_owned(),
                            });
                        }
                        spec.origin.goal_id = Some(goal.goal_id);
                    } else if spec.origin.goal_id.is_some() || spec.expected_goal_revision.is_some()
                    {
                        return Err(QuestionError::Rejected {
                            code: "missing_goal",
                            detail: "the referenced Goal no longer exists".to_owned(),
                        });
                    }
                }
                spec.validate()?;
                if spec.purpose == QuestionPurpose::GoalResume {
                    let binding = GoalResumeRequest {
                        session_id: spec.origin.session_id.clone(),
                        goal_id: spec.origin.goal_id.clone().expect("validated Goal"),
                        expected_revision: spec.expected_goal_revision.expect("validated revision"),
                        input_id: spec.origin.message_id.clone(),
                    };
                    SessionControlService::validate_goal_resume_in(tx, &binding)
                        .map_err(control_error)?;
                    if let Some(question) = previous_goal_resume_in(tx, &binding)? {
                        return Ok(QuestionReceipt {
                            question,
                            input_id: None,
                            duplicate: true,
                        });
                    }
                }
                let now = zuno_db::message::now_millis();
                let receipt = question::create_in(
                    tx,
                    &format!("que_{}", Uuid::now_v7().simple()),
                    &spec,
                    now,
                )?;
                if !receipt.duplicate
                    && spec.purpose == QuestionPurpose::RequiredInput
                    && let Some(revision) = spec.expected_goal_revision
                {
                    GoalStore::pause_for_question_in(
                        tx,
                        &spec.origin.session_id,
                        &receipt.question.id,
                        revision,
                        now,
                    )
                    .map_err(goal_error)?;
                }
                if !receipt.duplicate
                    && matches!(
                        spec.purpose,
                        QuestionPurpose::RequiredInput | QuestionPurpose::PlanAuthorization
                    )
                {
                    register_wait_in(tx, &receipt.question, now)?;
                }
                Ok(receipt)
            })
        })
        .await?;
        self.after_commit(receipt.clone());
        Ok(receipt)
    }

    async fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: QuestionCommand,
    ) -> QuestionResult<QuestionReceipt> {
        let pool = Arc::clone(&self.pool);
        let session_id = session_id.to_owned();
        let request_id = request_id.to_owned();
        let receipt = blocking(move || {
            pool.try_transaction(|tx| {
                let current = question::get_in(tx, &session_id, &request_id)?;
                if let Some(receipt) = question::receipt_in(tx, &request_id, &command)? {
                    return Ok(receipt);
                }
                let goal_resume = if current.purpose == QuestionPurpose::GoalResume
                    && let QuestionAction::Answer { answers } = &command.action
                    && answers
                        .values()
                        .any(|values| values.iter().any(|value| value == RESUME_GOAL_CHOICE))
                {
                    current.validate_answers(answers)?;
                    let binding = question::goal_resume_request_in(tx, &session_id, &request_id)?;
                    SessionControlService::validate_goal_resume_in(tx, &binding)
                        .map_err(control_error)?;
                    Some(binding)
                } else {
                    None
                };
                if let QuestionAction::PlanDecision {
                    decision: PlanQuestionDecision::Approve,
                    risk_reason,
                } = &command.action
                {
                    SessionControlService::validate_plan_question_in(tx, &current)?;
                    if current.plan.as_ref().is_some_and(|plan| {
                        plan.review_gate
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            == Some("draft")
                    }) && risk_reason
                        .as_deref()
                        .is_none_or(|reason| reason.trim().is_empty())
                    {
                        return Err(QuestionError::Rejected {
                            code: "draft_review",
                            detail: "explicit risk acceptance is required for a Draft review"
                                .to_owned(),
                        });
                    }
                }
                let now = zuno_db::message::now_millis();
                let mut receipt = question::apply_in(tx, &session_id, &request_id, &command, now)?;
                if let Some(binding) = goal_resume {
                    let resumed = SessionControlService::resume_goal_in(tx, &binding, now)
                        .map_err(control_error)?;
                    receipt.input_id = resumed.input.map(|input| input.id);
                    question::record_receipt_in(tx, &command.command_id, &receipt)?;
                }
                if !matches!(command.action, QuestionAction::Defer { .. }) {
                    if receipt.question.purpose == QuestionPurpose::RequiredInput {
                        GoalStore::settle_question_pause_in(tx, &session_id, &request_id, now)
                            .map_err(goal_error)?;
                    }
                    settle_wait_in(tx, &receipt.question, now)?;
                }
                if receipt.question.decision == Some(PlanQuestionDecision::Approve)
                    && question::handoff_completed_in(tx, &request_id)?
                {
                    receipt =
                        SessionControlService::apply_plan_question_in(tx, &receipt.question, now)?;
                    question::record_receipt_in(tx, &command.command_id, &receipt)?;
                }
                Ok(receipt)
            })
        })
        .await?;
        self.after_commit(receipt.clone());
        Ok(receipt)
    }

    async fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView> {
        let store = self.store.clone();
        let session_id = session_id.to_owned();
        let request_id = request_id.to_owned();
        blocking(move || store.get(&session_id, &request_id)).await
    }

    async fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        let store = self.store.clone();
        let session_id = session_id.to_owned();
        blocking(move || store.pending(&session_id)).await
    }

    async fn wait_for_change(
        &self,
        session_id: &str,
        request_id: &str,
        after_revision: i64,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        let mut changed = self.subscribe();
        // Local commits notify immediately. The bounded host-side reconciliation
        // also observes a reply committed by another process using the same DB.
        let mut reconcile = tokio::time::interval(Duration::from_millis(500));
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if interrupt.is_set() {
                return Err(QuestionError::Interrupted);
            }
            let view = self.get(session_id, request_id).await?;
            if view.revision != after_revision
                || view.state.is_terminal()
                || view.mode == QuestionMode::Deferred
            {
                return Ok(view);
            }
            tokio::select! {
                biased;
                () = interrupt.notified() => return Err(QuestionError::Interrupted),
                _ = changed.recv() => {},
                _ = reconcile.tick() => {},
            }
        }
    }
}

fn register_wait_in(tx: &Transaction<'_>, question: &QuestionView, now: i64) -> QuestionResult<()> {
    let session_id = &question.origin.session_id;
    let state =
        zuno_db::session_execution::seed_in(tx, session_id, CollaborationMode::Work, None, now)?;
    if state.scheduling.as_ref().is_some_and(|scheduling| {
        !matches!(
            scheduling.readiness,
            SessionReadiness::Ready | SessionReadiness::Completed
        ) && !(question.purpose == QuestionPurpose::PlanAuthorization
            && state.mode == CollaborationMode::Plan
            && matches!(scheduling.readiness, SessionReadiness::Paused { .. }))
    }) {
        return Err(QuestionError::Rejected {
            code: "existing_wait",
            detail: "resolve the existing session wait or explicitly resume before requesting new required input".to_owned(),
        });
    }
    zuno_db::session_execution::set_waiting_in(
        tx,
        session_id,
        state.revision,
        SessionWaitReference::Human {
            request_id: question.id.clone(),
        },
        now,
    )?;
    Ok(())
}

fn supersede_plan_question_in(tx: &Transaction<'_>, question: &QuestionView) -> QuestionResult<()> {
    let now = zuno_db::message::now_millis();
    zuno_db::human_request::resolve_in(
        tx,
        &question.id,
        zuno_db::human_request::HumanRequestState::Cancelled,
        Some(
            &serde_json::json!({"outcome":"superseded","source":"host","reason":"plan_binding_changed"}),
        ),
        now,
    )?;
    question::set_authorization_in(
        tx,
        &question.origin.session_id,
        &question.id,
        PlanAuthorizationState::Invalidated,
        None,
    )?;
    if let Some(state) = zuno_db::session_execution::read_in(tx, &question.origin.session_id)?
        && state.mode == CollaborationMode::Plan
        && state.scheduling.as_ref().is_some_and(|scheduling| {
            scheduling.readiness
                == SessionReadiness::WaitingHuman {
                    request_id: question.id.clone(),
                }
        })
    {
        let mut scheduling = state.scheduling.clone().expect("matched exact wait");
        scheduling.readiness = SessionReadiness::Ready;
        zuno_db::session_execution::set_scheduling_in(
            tx,
            &question.origin.session_id,
            state.revision,
            scheduling,
            now,
        )?;
    }
    Ok(())
}

fn settle_wait_in(tx: &Transaction<'_>, question: &QuestionView, now: i64) -> QuestionResult<()> {
    if matches!(
        question.purpose,
        QuestionPurpose::Clarification | QuestionPurpose::GoalResume
    ) {
        return Ok(());
    }
    let session_id = &question.origin.session_id;
    let wait = SessionWaitReference::Human {
        request_id: question.id.clone(),
    };
    if question.purpose == QuestionPurpose::RequiredInput
        && question.state == QuestionState::Answered
    {
        zuno_db::session_execution::clear_matching_wait_in(tx, session_id, &wait, now)?;
    } else if question.state.is_terminal()
        && !(question.decision == Some(PlanQuestionDecision::Approve)
            && question.authorization != Some(PlanAuthorizationState::Invalidated))
        && let Some(state) = zuno_db::session_execution::read_in(tx, session_id)?
        && state.scheduling.as_ref().is_some_and(|scheduling| {
            scheduling.readiness
                == SessionReadiness::WaitingHuman {
                    request_id: question.id.clone(),
                }
        })
    {
        zuno_db::session_execution::set_paused_in(
            tx,
            session_id,
            state.revision,
            SessionPauseReason::User,
            now,
        )?;
    }
    Ok(())
}

fn previous_goal_resume_in(
    tx: &Transaction<'_>,
    binding: &GoalResumeRequest,
) -> QuestionResult<Option<QuestionView>> {
    use rusqlite::OptionalExtension;
    loop {
        let id: Option<String> = tx.query_row(
            "SELECT q.request_id FROM question_interaction q JOIN human_request h ON h.id=q.request_id \
             WHERE h.session_id=?1 AND q.purpose='goal_resume' \
             AND json_extract(q.definition,'$.goalResume.goalId')=?2 \
             AND json_extract(q.definition,'$.goalResume.expectedRevision')=?3 \
             AND COALESCE(json_extract(h.response,'$.resumeSuperseded'),0)=0 \
             ORDER BY h.time_created,h.id LIMIT 1",
            rusqlite::params![binding.session_id, binding.goal_id, binding.expected_revision],
            |row| row.get(0),
        ).optional().map_err(zuno_db::map_error)?;
        let Some(id) = id else {
            return Ok(None);
        };
        if !question::supersede_unusable_goal_resume_in(
            tx,
            &binding.session_id,
            &id,
            zuno_db::message::now_millis(),
        )? {
            return question::get_in(tx, &binding.session_id, &id).map(Some);
        }
    }
}

fn prepare_plan_spec(tx: &Transaction<'_>, spec: &mut QuestionSpec) -> QuestionResult<()> {
    let session_id = &spec.origin.session_id;
    if spec.origin.turn_id.as_deref().is_none_or(str::is_empty) {
        return Err(QuestionError::Invalid(
            "Plan authorization needs the originating live turn identity".to_owned(),
        ));
    }
    let state = zuno_db::session_execution::read_in(tx, session_id)?
        .filter(|state| state.mode == CollaborationMode::Plan)
        .ok_or_else(|| QuestionError::Rejected {
            code: "not_in_plan_mode",
            detail: "enter Plan mode before requesting Start Work".to_owned(),
        })?;
    let plan = WorkStateStore::plan_in(tx, session_id)
        .map_err(|error| QuestionError::Rejected {
            code: "plan_state",
            detail: error.to_string(),
        })?
        .ok_or_else(|| QuestionError::Rejected {
            code: "missing_plan",
            detail: "create a durable Plan before requesting Start Work".to_owned(),
        })?;
    let identity = state.work_identity.ok_or_else(|| QuestionError::Rejected {
        code: "missing_work_identity",
        detail: "Plan mode has no saved Work identity".to_owned(),
    })?;
    let review_gate = ReviewStore::plan_review_gate_in(tx, session_id, &plan.id, plan.revision)
        .map_err(|error| QuestionError::Rejected {
            code: "review_state",
            detail: error.to_string(),
        })?;
    let completed_steps = plan
        .steps
        .iter()
        .filter(|step| step.status.is_terminal())
        .count();
    let total_steps = plan.steps.len();
    spec.questions = vec![QuestionRequest::closed(
        format!(
            "Start Work on Plan \"{}\" revision {} ({completed_steps}/{total_steps} completed steps) with Agent {}?",
            plan.title, plan.revision, identity.agent,
        ),
        "Start Work",
        vec![
            QuestionOption::new("approve", "Authorize this exact Plan and Work identity"),
            QuestionOption::new("decline", "Stay in Plan mode"),
        ],
    )];
    spec.plan = Some(PlanQuestionBinding {
        plan_id: plan.id,
        plan_revision: plan.revision,
        source_cycle_id: state.cycle_id,
        title: plan.title,
        completed_steps,
        total_steps,
        work_identity: identity,
        review_gate: serde_json::to_value(review_gate)
            .map_err(|error| QuestionError::Invalid(error.to_string()))?,
    });
    spec.mode = QuestionMode::Deferred;
    Ok(())
}

pub(crate) fn goal_error(error: GoalError) -> QuestionError {
    match error {
        GoalError::Db(error) => QuestionError::Database(error),
        error => QuestionError::Rejected {
            code: "goal_state",
            detail: error.to_string(),
        },
    }
}

pub(crate) fn control_error(error: SessionControlError) -> QuestionError {
    match error {
        SessionControlError::Database(error) => QuestionError::Database(error),
        SessionControlError::Goal(error) => goal_error(error),
        error => QuestionError::Rejected {
            code: "plan_authorization",
            detail: error.to_string(),
        },
    }
}

async fn blocking<T: Send + 'static>(
    mut work: impl FnMut() -> QuestionResult<T> + Send + 'static,
) -> QuestionResult<T> {
    tokio::task::spawn_blocking(move || {
        // These operations are reads or command/call-ID-idempotent transactions.
        // Shared-cache readers can briefly return SQLITE_LOCKED rather than wait
        // for SQLite's busy timeout. Retain the command and retry only typed
        // contention; never replay validation, conflict, I/O or unknown failures.
        for attempt in 0..8_u32 {
            let error = match work() {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if attempt == 7 {
                return Err(error);
            }
            let retry_after = match &error {
                QuestionError::Database(zuno_error::DbError::Busy { retry_after }) => *retry_after,
                QuestionError::Database(zuno_error::DbError::Open { source, .. })
                    if source.downcast_ref::<rusqlite::Error>().is_some_and(|error| matches!(
                        error, rusqlite::Error::SqliteFailure(code, _)
                            if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                    )) =>
                {
                    None
                }
                _ => return Err(error),
            };
            let ceiling = Duration::from_millis(200);
            let local = Duration::from_millis(4_u64 << attempt.min(5));
            let jitter = Duration::from_millis(u64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
                    % 7,
            ));
            std::thread::sleep(
                retry_after
                    .unwrap_or(local + jitter)
                    .min(ceiling)
                    .max(Duration::from_millis(1)),
            );
        }
        unreachable!("the final contention attempt returns its typed error")
    })
    .await
    .map_err(|error| QuestionError::Unavailable(format!("question worker failed: {error}")))?
}
