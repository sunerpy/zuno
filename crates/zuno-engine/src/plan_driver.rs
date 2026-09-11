//! Durable machine phase and Plan reconciliation decisions.
//!
//! User-visible Plan steps describe strategic outcomes. This driver records the
//! machine-owned execution phase separately, then decides from typed durable
//! state whether a host may finish, should recover, must wait for background
//! completion, or should pause after durable evidence of no progress. An
//! unfinished Plan or blocked Todo is not executable work. The host supplies
//! runnable-work evidence; session scheduling gates precede both ordinary and
//! Goal continuation, without interpreting assistant prose.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zuno_db::event_log::{self, NewSessionEvent};
use zuno_db::session_execution;
use zuno_db::{Connection, Pool, Transaction};
use zuno_error::DbError;
use zuno_types::execution::{
    CollaborationMode, SessionExecutionPhase, SessionExecutionState, SessionReadiness,
    SessionWaitReference, SessionWakeSignal, WakeAdmission,
};

pub use zuno_types::execution::SessionPauseReason as PlanPauseReason;

const DRIVER_PHASE_EVENT: &str = "session.driver.phase";
const NO_PROGRESS_STREAK_LIMIT: u32 = 3;
const PROGRESS_FINGERPRINT_DOMAIN: &[u8] = b"zuno.plan.progress.v2\0";

/// Durable machine-owned phase for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverPhase {
    Idle,
    Executing,
    Reconciling,
    WaitingRetry,
    WaitingBackground,
    WaitingHuman,
    Paused,
    Terminal,
}

impl DriverPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Executing => "executing",
            Self::Reconciling => "reconciling",
            Self::WaitingRetry => "waiting_retry",
            Self::WaitingBackground => "waiting_background",
            Self::WaitingHuman => "waiting_human",
            Self::Paused => "paused",
            Self::Terminal => "terminal",
        }
    }
}

/// Typed durable facts used for final reconciliation.
///
/// # Why the host's planning classification is not among them
///
/// [`crate::planning::PlanningPolicy`] classifies a request from its text before the
/// model has seen it, and that verdict used to count here: a request classified
/// `Required` with no Plan row was treated as unreconciled work. Nothing the model can
/// do settles that except creating a Plan, so a misclassified request — a plain question
/// with no question mark, say — spent the entire continuation budget on turns whose
/// instruction told the model that durable state "is not terminal" when no Plan, Todo, or
/// Job existed at all. A model asked to finish work it had already finished invents some.
///
/// So reconciliation reads only state something durably recorded. A prediction about a
/// request is not evidence about a session; the classification's place is the runtime
/// instruction in the turn that acts on it, where the model can still weigh it against
/// the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReconciliationInput {
    /// A visible Plan currently exists.
    pub plan_exists: bool,
    /// Every visible Plan step is terminal.
    pub plan_terminal: bool,
    /// A Todo remains pending, in progress, or blocked.
    /// This describes unfinished work, not whether it can run.
    pub active_todo: bool,
    /// Runnable Todo/dependency work or authorized queued work, verified by the
    /// host. An unfinished Plan, blocked Todo or callback alone is insufficient.
    pub executable_work: bool,
    /// A Job is active, uncertain, or still owns an unconsumed report.
    pub active_job: bool,
    /// An external operation whose completion the host must await. Supply its
    /// stable execution/job ID and origin cycle, not a completion receipt key
    /// containing a terminal timestamp. Only an `External` reference is valid.
    pub background_wait: Option<SessionWaitReference>,
    /// A durable Goal remains active and owns continuation.
    pub goal_active: bool,
    /// A read-only Plan Agent completed its planning turn and is handing the
    /// durable Plan/Todos to a later Start Work turn.
    pub planning_handoff: bool,
    /// The host verified that this exact waiting request authorizes the Plan
    /// being handed off. Required-input waits leave this unset. Matching the
    /// request ID prevents a stale host snapshot from bypassing a new wait.
    pub plan_authorization_wait: Option<String>,
}

impl PlanReconciliationInput {
    fn settled(&self) -> bool {
        let plan_settled = !self.plan_exists || self.plan_terminal;
        plan_settled && !self.active_todo && !self.active_job
    }
}

/// Host action selected from durable Plan/Todo/Job/Goal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReconciliationDecision {
    Finish,
    ContinueGoal,
    /// Recover authorized ordinary work. `attempt` is the current consecutive
    /// unchanged-progress observation and resets when the fingerprint changes.
    ContinueOrdinary {
        attempt: u8,
    },
}

/// Detailed reconciliation outcome used by the execution-state controller.
///
/// A pause and an existing wait are distinct from completion. Neither creates
/// a new question or a synthetic assistant response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReconciliationOutcome {
    Decision(PlanReconciliationDecision),
    Paused {
        reason: PlanPauseReason,
    },
    /// An authoritative wait already exists. This does not create a request or
    /// synthesize a model-visible reply.
    Waiting {
        wait: SessionWaitReference,
    },
}

/// Latest durable phase projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverPhaseProjection {
    pub phase: DriverPhase,
    pub cycle_id: String,
    /// Bounded attempt number for displays; the full session streak is below.
    pub reconciliation_attempt: u8,
    pub reason: Option<String>,
    pub progress_fingerprint: Option<String>,
    pub unchanged_progress_count: u32,
    pub pause_reason: Option<PlanPauseReason>,
    pub sequence: i64,
}

/// Durable service that owns machine phase and final Plan reconciliation.
#[derive(Clone)]
pub struct PlanReconciliationDriver {
    pool: Arc<Pool>,
}

impl PlanReconciliationDriver {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Convenience entry for explicit user input only. A query can run while
    /// automatic work stays paused/waiting. Runtime automatic, recovery,
    /// callback and resume paths must use [`Self::begin_with_wake`].
    pub fn begin(&self, session_id: &str, cycle_id: &str) -> Result<String, DbError> {
        self.begin_with_wake(
            session_id,
            cycle_id,
            &SessionWakeSignal::UserQuery,
            zuno_db::message::now_millis(),
        )?
        .ok_or_else(|| invalid_state("explicit user query was not admitted"))
    }

    /// Admit a host-verified wake and begin its driver cycle atomically.
    ///
    /// `None` suppresses execution without changing state or appending an event.
    /// A user query behind a gate also leaves the gate/projection intact.
    /// Recovery/callbacks use the authoritative execution cycle before consulting
    /// an older driver projection. An explicit control keeps its committed
    /// continuation cycle; a ready/new-after-completion user input binds its
    /// proposed new cycle.
    /// The state and existing continuation are bound before the driver event in
    /// the same transaction, without resetting progress. Only the wake policy
    /// can clear a wait/pause. The host must seed execution state and enforce
    /// Plan/Goal authority before calling this entry point.
    pub fn begin_with_wake(
        &self,
        session_id: &str,
        proposed_cycle_id: &str,
        wake: &SessionWakeSignal,
        at_ms: i64,
    ) -> Result<Option<String>, DbError> {
        validate_cycle(proposed_cycle_id)?;
        self.pool.transaction(|transaction| {
            let state = execution_in(transaction, session_id)?;
            let admission = state.wake_admission(wake);
            if admission == WakeAdmission::Reject {
                return Ok(None);
            }
            let previous = projection_in(transaction, session_id)?;
            let gated = scheduling_outcome(&state).is_some();
            let cycle_id = match wake {
                SessionWakeSignal::UserQuery | SessionWakeSignal::UserAnswer { .. }
                    if !gated || state.phase == SessionExecutionPhase::Completed =>
                {
                    proposed_cycle_id
                }
                // StartWork/resume services commit this authority before the
                // host drives the control. Never replace it with a Plan event.
                SessionWakeSignal::ExplicitResume => {
                    execution_cycle(&state).unwrap_or(proposed_cycle_id)
                }
                _ => existing_cycle(&state, previous.as_ref(), proposed_cycle_id),
            };
            validate_cycle(cycle_id)?;
            let cycle_id = cycle_id.to_owned();
            if gated && admission == WakeAdmission::Admit {
                return Ok(Some(cycle_id));
            }
            session_execution::admit_wake_in(transaction, session_id, wake, at_ms)?;
            let state = bind_cycle_in(
                transaction,
                execution_in(transaction, session_id)?,
                &cycle_id,
                at_ms,
            )?;
            let progress = progress_in(transaction, &state)?;
            if state.scheduling.is_none() {
                persist_progress_in(
                    transaction,
                    &state,
                    progress.as_ref(),
                    SessionReadiness::Ready,
                    at_ms,
                )?;
            }
            record_in(
                transaction,
                session_id,
                &cycle_id,
                DriverPhase::Executing,
                None,
                progress.as_ref(),
                None,
            )?;
            Ok(Some(cycle_id))
        })
    }

    /// Record a recoverable failure whose durable Goal backoff owns the next turn.
    pub fn waiting_retry(
        &self,
        session_id: &str,
        cycle_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), DbError> {
        let reason = reason.into();
        self.pool.transaction(|transaction| {
            let state = execution_in(transaction, session_id)?;
            if state.wake_admission(&SessionWakeSignal::Recovery) == WakeAdmission::Reject {
                return Ok(());
            }
            let previous = projection_in(transaction, session_id)?;
            let cycle_id = existing_cycle(&state, previous.as_ref(), cycle_id);
            let progress = progress_in(transaction, &state)?;
            record_in(
                transaction,
                session_id,
                cycle_id,
                DriverPhase::WaitingRetry,
                Some(&reason),
                progress.as_ref(),
                None,
            )
        })
    }

    /// Move the currently executing or reconciling cycle into durable retry wait.
    ///
    /// Failures that occur before a driver cycle starts do not manufacture one,
    /// and an already-terminal/waiting projection is never rewritten.
    pub fn waiting_retry_for_active_cycle(
        &self,
        session_id: &str,
        reason: impl Into<String>,
    ) -> Result<bool, DbError> {
        let reason = reason.into();
        self.pool.transaction(|transaction| {
            let Some(projection) = projection_in(transaction, session_id)? else {
                return Ok(false);
            };
            let state = execution_in(transaction, session_id)?;
            if scheduling_outcome(&state).is_some() {
                return Ok(false);
            }
            if !matches!(
                projection.phase,
                DriverPhase::Executing | DriverPhase::Reconciling
            ) {
                return Ok(false);
            }
            let progress = progress_in(transaction, &state)?;
            let cycle_id = existing_cycle(&state, Some(&projection), &projection.cycle_id);
            record_in(
                transaction,
                session_id,
                cycle_id,
                DriverPhase::WaitingRetry,
                Some(&reason),
                progress.as_ref(),
                None,
            )?;
            Ok(true)
        })
    }

    /// Reconcile durable state with explicit execution authorization and a
    /// stable fingerprint of runnable Todo/dependency/queued-work progress.
    ///
    /// `progress_fingerprint` should be derived from authoritative durable
    /// revisions or content digests. The driver hashes it with the typed input,
    /// persists only the bounded digest, and pauses after three consecutive
    /// identical observations. The streak belongs to the session, not a cycle.
    /// Wait/pause eligibility, progress and the driver event are handled in one
    /// transaction. An unfinished Plan or blocked Todo can only cause a
    /// `NoExecutableWork` pause, never ordinary recovery.
    pub fn reconcile_with_progress(
        &self,
        session_id: &str,
        cycle_id: &str,
        input: &PlanReconciliationInput,
        work_authorized: bool,
        progress_fingerprint: &str,
        at_ms: i64,
    ) -> Result<PlanReconciliationOutcome, DbError> {
        validate_cycle(cycle_id)?;
        self.pool.transaction(|transaction| {
            let state = execution_in(transaction, session_id)?;
            let previous = projection_in(transaction, session_id)?;
            let cycle_id = existing_cycle(&state, previous.as_ref(), cycle_id);
            let prior_progress = progress_in(transaction, &state)?;
            let gate = scheduling_outcome(&state);
            if input.planning_handoff
                && (gate.is_none() || matching_plan_authorization_wait(&state, input))
            {
                // The host records the actual Plan handoff after Finish. Keep
                // its pending approval eligible so early approval cannot deadlock.
                if gate.is_none() {
                    persist_progress_in(
                        transaction,
                        &state,
                        prior_progress.as_ref(),
                        SessionReadiness::Completed,
                        at_ms,
                    )?;
                }
                record_in(
                    transaction,
                    session_id,
                    cycle_id,
                    DriverPhase::Terminal,
                    Some("planning_handoff_ready"),
                    prior_progress.as_ref(),
                    None,
                )?;
                return Ok(PlanReconciliationOutcome::Decision(
                    PlanReconciliationDecision::Finish,
                ));
            }
            if let Some(outcome) = gate {
                record_gate_in(
                    transaction,
                    &state,
                    cycle_id,
                    previous.as_ref(),
                    prior_progress.as_ref(),
                )?;
                return Ok(outcome);
            }
            if let Some(wait) = &input.background_wait {
                if !matches!(wait, SessionWaitReference::External { .. }) {
                    return Err(invalid_state(
                        "background_wait requires an external source and origin cycle",
                    ));
                }
                persist_progress_in(
                    transaction,
                    &state,
                    prior_progress.as_ref(),
                    wait.clone().into(),
                    at_ms,
                )?;
                record_in(
                    transaction,
                    session_id,
                    cycle_id,
                    DriverPhase::WaitingBackground,
                    Some("waiting_external"),
                    prior_progress.as_ref(),
                    None,
                )?;
                return Ok(PlanReconciliationOutcome::Waiting { wait: wait.clone() });
            }
            if !input.goal_active {
                let finished_reason = if !input.executable_work && input.settled() {
                    Some("durable_work_settled")
                } else if !work_authorized || state.mode != CollaborationMode::Work {
                    Some("work_not_authorized")
                } else {
                    None
                };
                if let Some(reason) = finished_reason {
                    persist_progress_in(
                        transaction,
                        &state,
                        prior_progress.as_ref(),
                        SessionReadiness::Completed,
                        at_ms,
                    )?;
                    record_in(
                        transaction,
                        session_id,
                        cycle_id,
                        DriverPhase::Terminal,
                        Some(reason),
                        prior_progress.as_ref(),
                        None,
                    )?;
                    return Ok(PlanReconciliationOutcome::Decision(
                        PlanReconciliationDecision::Finish,
                    ));
                }
            }
            let fingerprint = stable_progress_fingerprint(input, progress_fingerprint);
            let unchanged_count = prior_progress.as_ref().map_or(1, |previous| {
                if previous.fingerprint == fingerprint {
                    previous.unchanged_count.saturating_add(1)
                } else {
                    1
                }
            });
            let progress = DurableProgress {
                fingerprint,
                unchanged_count,
            };
            let pause_reason = if !input.goal_active && !input.executable_work {
                Some(PlanPauseReason::NoExecutableWork)
            } else if unchanged_count >= NO_PROGRESS_STREAK_LIMIT {
                Some(PlanPauseReason::NoProgress)
            } else {
                None
            };
            let readiness = pause_reason.map_or(SessionReadiness::Ready, |reason| {
                SessionReadiness::Paused { reason }
            });
            persist_progress_in(transaction, &state, Some(&progress), readiness, at_ms)?;
            if let Some(reason) = pause_reason {
                record_in(
                    transaction,
                    session_id,
                    cycle_id,
                    DriverPhase::Paused,
                    Some(pause_reason_name(reason)),
                    Some(&progress),
                    Some(reason),
                )?;
                return Ok(PlanReconciliationOutcome::Paused { reason });
            }
            let (phase, reason, decision) = if input.goal_active {
                (
                    DriverPhase::Executing,
                    "active_goal_owns_continuation",
                    PlanReconciliationDecision::ContinueGoal,
                )
            } else {
                (
                    DriverPhase::Reconciling,
                    "authorized_work_recovery",
                    PlanReconciliationDecision::ContinueOrdinary {
                        attempt: bounded_attempt(unchanged_count),
                    },
                )
            };
            record_in(
                transaction,
                session_id,
                cycle_id,
                phase,
                Some(reason),
                Some(&progress),
                None,
            )?;
            Ok(PlanReconciliationOutcome::Decision(decision))
        })
    }

    /// Rebuild the latest machine phase from the existing session event log.
    ///
    /// The newest phase event is read through the `(aggregate_id, type, seq)`
    /// index rather than by scanning the whole session log, and every stored
    /// version of the type counts, so a projection rebuilt after an event-schema
    /// bump still sees the phases an older release wrote.
    pub fn projection(&self, session_id: &str) -> Result<Option<DriverPhaseProjection>, DbError> {
        let connection = self.pool.get()?;
        projection_in(&connection, session_id)
    }
}

struct DurableProgress {
    fingerprint: String,
    unchanged_count: u32,
}

fn execution_in(
    connection: &Connection,
    session_id: &str,
) -> Result<SessionExecutionState, DbError> {
    session_execution::read_in(connection, session_id)?.ok_or_else(|| DbError::NotFound {
        table: "session_execution_state".to_owned(),
        id: session_id.to_owned(),
    })
}

fn existing_cycle<'a>(
    state: &'a SessionExecutionState,
    previous: Option<&'a DriverPhaseProjection>,
    proposed: &'a str,
) -> &'a str {
    execution_cycle(state)
        .or_else(|| previous.map(|projection| projection.cycle_id.as_str()))
        .unwrap_or(proposed)
}

fn execution_cycle(state: &SessionExecutionState) -> Option<&str> {
    state.cycle_id.as_deref().or_else(|| {
        state
            .continuation
            .as_ref()
            .map(|token| token.cycle_id.as_str())
    })
}

/// Bind only cycle coordinates. Identity, mode, Plan/Goal authorization,
/// context epoch, anchor and scheduling evidence keep their current values.
fn bind_cycle_in(
    transaction: &Transaction<'_>,
    mut state: SessionExecutionState,
    cycle_id: &str,
    at_ms: i64,
) -> Result<SessionExecutionState, DbError> {
    if state.cycle_id.as_deref() == Some(cycle_id)
        && state
            .continuation
            .as_ref()
            .is_none_or(|token| token.cycle_id == cycle_id)
    {
        return Ok(state);
    }
    let expected_revision = state.revision;
    state.cycle_id = Some(cycle_id.to_owned());
    if let Some(continuation) = &mut state.continuation {
        continuation.cycle_id = cycle_id.to_owned();
    }
    state.time_updated = state.time_updated.max(at_ms);
    session_execution::update_in(transaction, expected_revision, state)
}

fn scheduling_outcome(state: &SessionExecutionState) -> Option<PlanReconciliationOutcome> {
    match state
        .scheduling
        .as_ref()
        .map(|scheduling| &scheduling.readiness)
    {
        Some(SessionReadiness::Ready) => None,
        Some(SessionReadiness::WaitingHuman { request_id }) => {
            Some(PlanReconciliationOutcome::Waiting {
                wait: SessionWaitReference::Human {
                    request_id: request_id.clone(),
                },
            })
        }
        Some(SessionReadiness::WaitingExternal {
            source_id,
            origin_cycle_id,
        }) => Some(PlanReconciliationOutcome::Waiting {
            wait: SessionWaitReference::External {
                source_id: source_id.clone(),
                origin_cycle_id: origin_cycle_id.clone(),
            },
        }),
        Some(SessionReadiness::Paused { reason }) => {
            Some(PlanReconciliationOutcome::Paused { reason: *reason })
        }
        Some(SessionReadiness::Completed) => Some(PlanReconciliationOutcome::Decision(
            PlanReconciliationDecision::Finish,
        )),
        None => match state.phase {
            SessionExecutionPhase::Paused | SessionExecutionPhase::Blocked => {
                Some(PlanReconciliationOutcome::Paused {
                    reason: PlanPauseReason::Blocked,
                })
            }
            // An untyped legacy wait cannot name an awaited request. Close this
            // turn while preserving that wait; never manufacture a question.
            SessionExecutionPhase::Waiting | SessionExecutionPhase::Completed => Some(
                PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish),
            ),
            _ => None,
        },
    }
}

fn matching_plan_authorization_wait(
    state: &SessionExecutionState,
    input: &PlanReconciliationInput,
) -> bool {
    state.mode == CollaborationMode::Plan
        && matches!(
            state.scheduling.as_ref().map(|scheduling| &scheduling.readiness),
            Some(SessionReadiness::WaitingHuman { request_id })
                if input.plan_authorization_wait.as_deref() == Some(request_id.as_str())
        )
}

fn progress_in(
    connection: &Connection,
    state: &SessionExecutionState,
) -> Result<Option<DurableProgress>, DbError> {
    if let Some(scheduling) = &state.scheduling {
        return Ok(scheduling
            .progress_fingerprint
            .as_ref()
            .map(|fingerprint| DurableProgress {
                fingerprint: fingerprint.clone(),
                unchanged_count: scheduling.unchanged_progress_count,
            }));
    }
    // Only legacy NULL scheduling needs an event fallback. Search across cycles
    // so a callback-generated ID cannot erase the last durable observation.
    Ok(
        event_log::read_of_type_after_in(connection, &state.session_id, DRIVER_PHASE_EVENT, None)?
            .into_iter()
            .rev()
            .find_map(|event| {
                Some(DurableProgress {
                    fingerprint: event
                        .properties
                        .get("progressFingerprint")?
                        .as_str()?
                        .to_owned(),
                    unchanged_count: event
                        .properties
                        .get("unchangedProgressCount")?
                        .as_u64()
                        .and_then(|count| u32::try_from(count).ok())?,
                })
            }),
    )
}

fn persist_progress_in(
    transaction: &Transaction<'_>,
    state: &SessionExecutionState,
    progress: Option<&DurableProgress>,
    readiness: SessionReadiness,
    at_ms: i64,
) -> Result<(), DbError> {
    let mut scheduling = state.scheduling.clone().unwrap_or_default();
    if let Some(progress) = progress {
        scheduling.progress_fingerprint = Some(progress.fingerprint.clone());
        scheduling.unchanged_progress_count = progress.unchanged_count;
    }
    scheduling.readiness = readiness;
    session_execution::set_scheduling_in(
        transaction,
        &state.session_id,
        state.revision,
        scheduling,
        at_ms,
    )
    .map(|_| ())
}

fn record_gate_in(
    transaction: &Transaction<'_>,
    state: &SessionExecutionState,
    cycle_id: &str,
    previous: Option<&DriverPhaseProjection>,
    progress: Option<&DurableProgress>,
) -> Result<(), DbError> {
    let (phase, reason, pause) = match scheduling_outcome(state) {
        Some(PlanReconciliationOutcome::Paused { reason }) => {
            (DriverPhase::Paused, pause_reason_name(reason), Some(reason))
        }
        Some(PlanReconciliationOutcome::Waiting {
            wait: SessionWaitReference::Human { .. },
        }) => (DriverPhase::WaitingHuman, "waiting_human", None),
        Some(PlanReconciliationOutcome::Waiting {
            wait: SessionWaitReference::External { .. },
        }) => (DriverPhase::WaitingBackground, "waiting_external", None),
        Some(PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish))
            if state.phase == SessionExecutionPhase::Completed =>
        {
            (DriverPhase::Terminal, "durable_work_completed", None)
        }
        _ => return Ok(()),
    };
    if previous.is_some_and(|previous| previous.phase == phase && previous.pause_reason == pause) {
        return Ok(());
    }
    record_in(
        transaction,
        &state.session_id,
        cycle_id,
        phase,
        Some(reason),
        progress,
        pause,
    )
}

fn record_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    cycle_id: &str,
    phase: DriverPhase,
    reason: Option<&str>,
    progress: Option<&DurableProgress>,
    pause_reason: Option<PlanPauseReason>,
) -> Result<(), DbError> {
    let mut properties = Map::new();
    properties.insert("phase".to_owned(), Value::from(phase.as_str()));
    properties.insert("cycleId".to_owned(), Value::from(cycle_id));
    properties.insert(
        "reconciliationAttempt".to_owned(),
        Value::from(progress.map_or(0, |progress| bounded_attempt(progress.unchanged_count))),
    );
    if let Some(reason) = reason {
        properties.insert("reason".to_owned(), Value::from(reason));
    }
    if let Some(progress) = progress {
        properties.insert(
            "progressFingerprint".to_owned(),
            Value::from(progress.fingerprint.clone()),
        );
        properties.insert(
            "unchangedProgressCount".to_owned(),
            Value::from(progress.unchanged_count),
        );
    }
    if let Some(reason) = pause_reason {
        properties.insert(
            "pauseReason".to_owned(),
            Value::from(pause_reason_name(reason)),
        );
    }
    event_log::append_in(
        transaction,
        session_id,
        NewSessionEvent::new(DRIVER_PHASE_EVENT, properties)?,
    )
    .map(|_| ())
}

fn projection_in(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<DriverPhaseProjection>, DbError> {
    let Some(event) = event_log::latest_of_type_in(connection, session_id, DRIVER_PHASE_EVENT)?
    else {
        return Ok(None);
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StoredPhase {
        phase: DriverPhase,
        cycle_id: String,
        reconciliation_attempt: u8,
        reason: Option<String>,
        progress_fingerprint: Option<String>,
        #[serde(default)]
        unchanged_progress_count: u32,
        pause_reason: Option<PlanPauseReason>,
    }
    let stored: StoredPhase = serde_json::from_value(event.properties.into())
        .map_err(|error| invalid_state(error.to_string()))?;
    validate_cycle(&stored.cycle_id)?;
    Ok(Some(DriverPhaseProjection {
        phase: stored.phase,
        cycle_id: stored.cycle_id,
        reconciliation_attempt: stored.reconciliation_attempt,
        reason: stored.reason,
        progress_fingerprint: stored.progress_fingerprint,
        unchanged_progress_count: stored.unchanged_progress_count,
        pause_reason: stored.pause_reason,
        sequence: event.sequence,
    }))
}

fn bounded_attempt(count: u32) -> u8 {
    count.min(u32::from(u8::MAX)) as u8
}

fn pause_reason_name(reason: PlanPauseReason) -> &'static str {
    match reason {
        PlanPauseReason::NoProgress => "no_progress",
        PlanPauseReason::NoExecutableWork => "no_executable_work",
        PlanPauseReason::User => "user",
        PlanPauseReason::Authentication => "authentication",
        PlanPauseReason::TurnBudget => "turn_budget",
        PlanPauseReason::Blocked => "blocked",
    }
}

fn validate_cycle(cycle_id: &str) -> Result<(), DbError> {
    if cycle_id.trim().is_empty() {
        return Err(invalid_state("driver cycle id must not be empty"));
    }
    Ok(())
}

fn invalid_state(detail: impl Into<String>) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(detail.into())),
    }
}

fn stable_progress_fingerprint(input: &PlanReconciliationInput, source: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(PROGRESS_FINGERPRINT_DOMAIN);
    digest.update([
        u8::from(input.plan_exists),
        u8::from(input.plan_terminal),
        u8::from(input.active_todo),
        u8::from(input.executable_work),
        u8::from(input.active_job),
        u8::from(input.goal_active),
    ]);
    digest.update((source.len() as u64).to_be_bytes());
    digest.update(source.as_bytes());
    format!("sha256:{}", hex::encode(digest.finalize()))
}

#[cfg(test)]
#[path = "plan_driver_scheduling_tests.rs"]
mod scheduling_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn pool() -> Arc<Pool> {
        let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.get().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        connection
            .execute(
                "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes)
             VALUES ('project','/workspace',1,1,'[]')",
                [],
            )
            .expect("project");
        for session_id in ["ses", "goal", "ses_job", "ses_job_without_goal"] {
            connection
                .execute(
                    "INSERT INTO session
                 (id,project_id,slug,directory,title,version,time_created,time_updated)
                 VALUES (?1,'project',?1,'/workspace','Driver test','test',1,1)",
                    [session_id],
                )
                .expect("session");
        }
        drop(connection);
        let store = session_execution::SessionExecutionStore::new(Arc::clone(&pool));
        for session_id in ["ses", "goal", "ses_job", "ses_job_without_goal"] {
            store
                .seed(session_id, CollaborationMode::Work, None, 1)
                .expect("execution state");
        }
        pool
    }

    pub(super) fn unfinished() -> PlanReconciliationInput {
        PlanReconciliationInput {
            plan_exists: true,
            plan_terminal: false,
            active_todo: false,
            executable_work: true,
            active_job: false,
            background_wait: None,
            goal_active: false,
            planning_handoff: false,
            plan_authorization_wait: None,
        }
    }

    #[test]
    fn changing_progress_survives_a_driver_restart_without_requesting_human_input() {
        let pool = pool();
        let first = PlanReconciliationDriver::new(Arc::clone(&pool));
        assert_eq!(first.begin("ses", "cycle").expect("begin"), "cycle");
        assert_eq!(
            first
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-1", 10)
                .expect("first"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );

        let restarted = PlanReconciliationDriver::new(pool);
        assert_eq!(
            restarted
                .begin_with_wake("ses", "replacement", &SessionWakeSignal::Recovery, 20)
                .expect("resume")
                .expect("admitted"),
            "cycle",
            "a restarted host must retain the durable reconciliation cycle"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-2", 30)
                .expect("second"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            }),
            "authoritative progress resets the no-progress streak"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-3", 40)
                .expect("third"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );
        let projection = restarted.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Reconciling);
        assert_eq!(projection.unchanged_progress_count, 1);
        assert_eq!(projection.pause_reason, None);
    }

    #[test]
    fn third_identical_fingerprint_returns_a_durable_typed_no_progress_pause() {
        let pool = pool();
        let first = PlanReconciliationDriver::new(Arc::clone(&pool));
        assert_eq!(
            first
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-1", 10)
                .expect("first"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );
        assert_eq!(
            first
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-1", 20)
                .expect("second"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 2
            })
        );

        let restarted = PlanReconciliationDriver::new(pool);
        assert_eq!(
            restarted
                .begin_with_wake("ses", "replacement", &SessionWakeSignal::Recovery, 25)
                .expect("resume")
                .expect("admitted"),
            "cycle",
            "the persisted reconciliation phase must restore the original cycle"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", &unfinished(), true, "plan-revision-1", 30)
                .expect("third after restart"),
            PlanReconciliationOutcome::Paused {
                reason: PlanPauseReason::NoProgress
            }
        );
        let projection = restarted.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Paused);
        assert_eq!(projection.reason.as_deref(), Some("no_progress"));
        assert_eq!(projection.pause_reason, Some(PlanPauseReason::NoProgress));
        assert_eq!(projection.unchanged_progress_count, 3);
        assert!(
            projection
                .progress_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint.starts_with("sha256:"))
        );
    }

    fn decision(
        driver: &PlanReconciliationDriver,
        session_id: &str,
        cycle_id: &str,
        input: &PlanReconciliationInput,
    ) -> PlanReconciliationDecision {
        match driver
            .reconcile_with_progress(
                session_id,
                cycle_id,
                input,
                true,
                "durable-runnable-state",
                10,
            )
            .expect("reconcile")
        {
            PlanReconciliationOutcome::Decision(decision) => decision,
            other => panic!("expected a decision, got {other:?}"),
        }
    }

    #[test]
    fn a_session_that_recorded_no_durable_work_finishes_instead_of_being_driven_again() {
        // The reported defect. `你现在能看到多少个skill` — "how many skills can you see" —
        // was classified as requiring a Plan because it carries no question mark. The model
        // answered it and created nothing, and the driver then spent both continuations
        // telling the model that Plan, Todo, or Job state was "not terminal" while all
        // three were empty. The second turn duly invented work to do: enumerate every
        // page of the catalog to verify the count it had already reported.
        let driver = PlanReconciliationDriver::new(pool());
        let nothing_recorded = PlanReconciliationInput {
            plan_exists: false,
            plan_terminal: false,
            active_todo: false,
            executable_work: false,
            active_job: false,
            background_wait: None,
            goal_active: false,
            planning_handoff: false,
            plan_authorization_wait: None,
        };

        assert_eq!(
            decision(&driver, "ses", "cycle", &nothing_recorded),
            PlanReconciliationDecision::Finish,
            "a session that recorded no durable work has nothing to reconcile"
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Terminal);
        assert_eq!(projection.reason.as_deref(), Some("durable_work_settled"));
    }

    #[test]
    fn a_read_only_planning_handoff_finishes_without_executing_future_work() {
        let driver = PlanReconciliationDriver::new(pool());
        let mut handoff = unfinished();
        handoff.active_todo = true;
        handoff.goal_active = true;
        handoff.planning_handoff = true;

        assert_eq!(
            decision(&driver, "ses", "cycle", &handoff),
            PlanReconciliationDecision::Finish
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Terminal);
        assert_eq!(projection.reason.as_deref(), Some("planning_handoff_ready"));

        let mut active_job = handoff;
        active_job.active_job = true;
        assert_eq!(
            decision(&driver, "ses_job", "cycle", &active_job),
            PlanReconciliationDecision::Finish
        );

        active_job.goal_active = false;
        assert_eq!(
            decision(&driver, "ses_job_without_goal", "cycle", &active_job),
            PlanReconciliationDecision::Finish
        );
    }

    #[test]
    fn ordinary_work_requires_authorization_but_an_active_goal_keeps_continuation() {
        let driver = PlanReconciliationDriver::new(pool());
        assert_eq!(
            driver
                .reconcile_with_progress(
                    "ses",
                    "cycle",
                    &unfinished(),
                    false,
                    "plan-revision-1",
                    10
                )
                .expect("unauthorized"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Terminal);
        assert_eq!(projection.reason.as_deref(), Some("work_not_authorized"));

        let mut goal = unfinished();
        goal.plan_exists = false;
        goal.executable_work = false;
        goal.goal_active = true;
        assert_eq!(
            driver
                .reconcile_with_progress("goal", "cycle", &goal, false, "goal-revision-1", 20)
                .expect("active goal"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueGoal),
            "an active Goal is continuation authority even without ordinary executable work"
        );
    }

    #[test]
    fn a_runnable_todo_enters_authorized_recovery() {
        // The host separately verifies runnable work; the active-Todo bit by
        // itself would also be true for a blocked Todo.
        let driver = PlanReconciliationDriver::new(pool());
        let mut only_a_todo = unfinished();
        only_a_todo.plan_exists = false;
        only_a_todo.active_todo = true;

        assert_eq!(
            decision(&driver, "ses", "cycle", &only_a_todo),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 },
            "a verified runnable Todo is executable work"
        );
    }

    #[test]
    fn a_running_remote_observer_waits_without_spending_reconciliation_attempts() {
        let driver = PlanReconciliationDriver::new(pool());
        driver.begin("ses", "cycle").expect("begin");
        let mut input = unfinished();
        let wait = SessionWaitReference::External {
            source_id: "bg-observer".to_owned(),
            origin_cycle_id: "cycle".to_owned(),
        };
        input.background_wait = Some(wait.clone());
        input.goal_active = true;

        assert_eq!(
            driver
                .reconcile_with_progress("ses", "cycle", &input, true, "runnable-state", 10)
                .expect("first background wait"),
            PlanReconciliationOutcome::Waiting { wait: wait.clone() }
        );
        assert_eq!(
            driver
                .reconcile_with_progress("ses", "cycle", &input, true, "runnable-state", 11)
                .expect("repeated background wait"),
            PlanReconciliationOutcome::Waiting { wait: wait.clone() }
        );
        let waiting = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(waiting.phase, DriverPhase::WaitingBackground);
        assert_eq!(waiting.reconciliation_attempt, 0);
        assert_eq!(waiting.reason.as_deref(), Some("waiting_external"));
        assert_eq!(waiting.unchanged_progress_count, 0);

        input.background_wait = None;
        input.goal_active = false;
        assert_eq!(
            driver
                .reconcile_with_progress("ses", "cycle", &input, true, "runnable-state", 12)
                .expect("absence of an observer is not completion evidence"),
            PlanReconciliationOutcome::Waiting { wait }
        );
        assert_eq!(
            driver
                .begin_with_wake(
                    "ses",
                    "callback-cycle",
                    &SessionWakeSignal::ExternalCompletion {
                        source_id: "bg-observer".to_owned(),
                        origin_cycle_id: "cycle".to_owned(),
                    },
                    13
                )
                .expect("matching completion"),
            Some("cycle".to_owned())
        );
        assert_eq!(
            decision(&driver, "ses", "cycle", &input),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 },
            "once the observer settles, the ordinary durable-work policy resumes"
        );
    }

    #[test]
    fn an_active_goal_owns_continuation() {
        let driver = PlanReconciliationDriver::new(pool());
        let mut input = unfinished();
        input.goal_active = true;
        assert_eq!(
            decision(&driver, "ses", "cycle", &input),
            PlanReconciliationDecision::ContinueGoal
        );
        assert_eq!(
            driver.projection("ses").expect("projection").unwrap().phase,
            DriverPhase::Executing
        );
    }

    #[test]
    fn a_goal_retry_moves_the_active_cycle_into_durable_wait() {
        let driver = PlanReconciliationDriver::new(pool());
        driver.begin("ses", "cycle").expect("begin");

        assert!(
            driver
                .waiting_retry_for_active_cycle("ses", "provider_transient")
                .expect("waiting retry")
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::WaitingRetry);
        assert_eq!(projection.cycle_id, "cycle");
        assert_eq!(projection.reason.as_deref(), Some("provider_transient"));
        assert!(
            !driver
                .waiting_retry_for_active_cycle("ses", "duplicate")
                .expect("already waiting")
        );
    }
}
