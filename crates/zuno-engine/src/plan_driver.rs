//! Durable machine phase and Plan reconciliation decisions.
//!
//! User-visible Plan steps describe strategic outcomes. This driver records the
//! machine-owned execution phase separately, then decides from typed durable
//! state whether a host may finish, should continue, or must wait for a human.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_db::Pool;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_error::DbError;

const DRIVER_PHASE_EVENT: &str = "session.driver.phase";
const DEFAULT_RECONCILIATION_LIMIT: u8 = 2;

/// Durable machine-owned phase for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverPhase {
    Idle,
    Executing,
    Reconciling,
    WaitingRetry,
    WaitingHuman,
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
            Self::WaitingHuman => "waiting_human",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanReconciliationInput {
    /// A visible Plan currently exists.
    pub plan_exists: bool,
    /// Every visible Plan step is terminal.
    pub plan_terminal: bool,
    /// A Todo remains pending, in progress, or blocked.
    pub active_todo: bool,
    /// A Job is active, uncertain, or still owns an unconsumed report.
    pub active_job: bool,
    /// A durable Goal remains active and owns continuation.
    pub goal_active: bool,
    /// A read-only Plan Agent completed its planning turn and is handing the
    /// durable Plan/Todos to a later Start Work turn.
    pub planning_handoff: bool,
}

impl PlanReconciliationInput {
    fn settled(self) -> bool {
        let plan_settled = !self.plan_exists || self.plan_terminal;
        plan_settled && !self.active_todo && !self.active_job
    }
}

/// Host action selected from durable Plan/Todo/Job/Goal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReconciliationDecision {
    Finish,
    ContinueGoal,
    ContinueOrdinary { attempt: u8 },
    WaitForHuman { reason: PlanWaitingReason },
}

/// Typed reason an ordinary session cannot be delivered as successful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWaitingReason {
    PlanUnreconciled,
}

impl PlanWaitingReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlanUnreconciled => "plan_unreconciled",
        }
    }
}

/// Latest durable phase projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverPhaseProjection {
    pub phase: DriverPhase,
    pub cycle_id: String,
    pub reconciliation_attempt: u8,
    pub reason: Option<String>,
    pub sequence: i64,
}

/// Durable service that owns machine phase and final Plan reconciliation.
#[derive(Clone)]
pub struct PlanReconciliationDriver {
    events: SessionEventLog,
    reconciliation_limit: u8,
}

impl PlanReconciliationDriver {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            events: SessionEventLog::new(pool),
            reconciliation_limit: DEFAULT_RECONCILIATION_LIMIT,
        }
    }

    #[cfg(test)]
    fn with_limit(pool: Arc<Pool>, reconciliation_limit: u8) -> Self {
        Self {
            events: SessionEventLog::new(pool),
            reconciliation_limit,
        }
    }

    /// Start one user/continuation execution cycle, or resume an interrupted
    /// reconciliation cycle from the durable event log.
    pub fn begin(&self, session_id: &str, cycle_id: &str) -> Result<String, DbError> {
        if let Some(previous) = self.projection(session_id)?
            && previous.phase == DriverPhase::Reconciling
        {
            return Ok(previous.cycle_id);
        }
        self.record(session_id, cycle_id, DriverPhase::Executing, 0, None)?;
        Ok(cycle_id.to_owned())
    }

    /// Record a recoverable failure whose durable Goal backoff owns the next turn.
    pub fn waiting_retry(
        &self,
        session_id: &str,
        cycle_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), DbError> {
        self.record(
            session_id,
            cycle_id,
            DriverPhase::WaitingRetry,
            self.attempts_for_cycle(session_id, cycle_id)?,
            Some(reason.into()),
        )
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
        let Some(projection) = self.projection(session_id)? else {
            return Ok(false);
        };
        if !matches!(
            projection.phase,
            DriverPhase::Executing | DriverPhase::Reconciling
        ) {
            return Ok(false);
        }
        self.waiting_retry(session_id, &projection.cycle_id, reason)?;
        Ok(true)
    }

    /// Reconcile typed durable state before a successful terminal event is emitted.
    pub fn reconcile(
        &self,
        session_id: &str,
        cycle_id: &str,
        input: PlanReconciliationInput,
    ) -> Result<PlanReconciliationDecision, DbError> {
        let attempt = self.attempts_for_cycle(session_id, cycle_id)?;
        if input.planning_handoff && !input.active_job {
            self.record(
                session_id,
                cycle_id,
                DriverPhase::Terminal,
                attempt,
                Some("planning_handoff_ready".to_owned()),
            )?;
            return Ok(PlanReconciliationDecision::Finish);
        }
        if input.settled() {
            self.record(
                session_id,
                cycle_id,
                DriverPhase::Terminal,
                attempt,
                Some("durable_work_settled".to_owned()),
            )?;
            return Ok(PlanReconciliationDecision::Finish);
        }
        if input.goal_active {
            self.record(
                session_id,
                cycle_id,
                DriverPhase::Executing,
                attempt,
                Some("active_goal_owns_continuation".to_owned()),
            )?;
            return Ok(PlanReconciliationDecision::ContinueGoal);
        }
        if attempt < self.reconciliation_limit {
            let next = attempt.saturating_add(1);
            self.record(
                session_id,
                cycle_id,
                DriverPhase::Reconciling,
                next,
                Some("durable_work_unreconciled".to_owned()),
            )?;
            return Ok(PlanReconciliationDecision::ContinueOrdinary { attempt: next });
        }
        self.record(
            session_id,
            cycle_id,
            DriverPhase::WaitingHuman,
            attempt,
            Some("plan_unreconciled".to_owned()),
        )?;
        Ok(PlanReconciliationDecision::WaitForHuman {
            reason: PlanWaitingReason::PlanUnreconciled,
        })
    }

    /// Rebuild the latest machine phase from the existing session event log.
    ///
    /// The newest phase event is read through the `(aggregate_id, type, seq)`
    /// index rather than by scanning the whole session log, and every stored
    /// version of the type counts, so a projection rebuilt after an event-schema
    /// bump still sees the phases an older release wrote.
    pub fn projection(&self, session_id: &str) -> Result<Option<DriverPhaseProjection>, DbError> {
        Ok(self
            .events
            .latest_of_type(session_id, DRIVER_PHASE_EVENT)?
            .and_then(|event| {
                let phase =
                    serde_json::from_value::<DriverPhase>(event.properties.get("phase")?.clone())
                        .ok()?;
                let cycle_id = event.properties.get("cycleId")?.as_str()?.to_owned();
                let reconciliation_attempt = event
                    .properties
                    .get("reconciliationAttempt")?
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())?;
                let reason = event
                    .properties
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Some(DriverPhaseProjection {
                    phase,
                    cycle_id,
                    reconciliation_attempt,
                    reason,
                    sequence: event.sequence,
                })
            }))
    }

    fn attempts_for_cycle(&self, session_id: &str, cycle_id: &str) -> Result<u8, DbError> {
        let attempts = self
            .events
            .read_of_type_after(session_id, DRIVER_PHASE_EVENT, None)?
            .into_iter()
            .filter(|event| {
                event.properties.get("cycleId").and_then(Value::as_str) == Some(cycle_id)
            })
            .filter_map(|event| {
                event
                    .properties
                    .get("reconciliationAttempt")
                    .and_then(Value::as_u64)
                    .and_then(|value| u8::try_from(value).ok())
            })
            .max()
            .unwrap_or(0);
        Ok(attempts)
    }

    fn record(
        &self,
        session_id: &str,
        cycle_id: &str,
        phase: DriverPhase,
        reconciliation_attempt: u8,
        reason: Option<String>,
    ) -> Result<(), DbError> {
        let mut properties = Map::new();
        properties.insert("phase".to_owned(), Value::String(phase.as_str().to_owned()));
        properties.insert("cycleId".to_owned(), Value::String(cycle_id.to_owned()));
        properties.insert(
            "reconciliationAttempt".to_owned(),
            Value::from(reconciliation_attempt),
        );
        if let Some(reason) = reason {
            properties.insert("reason".to_owned(), Value::String(reason));
        }
        self.events
            .append(
                session_id,
                NewSessionEvent::new(DRIVER_PHASE_EVENT, properties)?,
            )
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> Arc<Pool> {
        let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.get().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        drop(connection);
        pool
    }

    fn unfinished() -> PlanReconciliationInput {
        PlanReconciliationInput {
            plan_exists: true,
            plan_terminal: false,
            active_todo: false,
            active_job: false,
            goal_active: false,
            planning_handoff: false,
        }
    }

    #[test]
    fn ordinary_reconciliation_survives_a_driver_restart_and_then_waits() {
        let pool = pool();
        let first = PlanReconciliationDriver::with_limit(Arc::clone(&pool), 2);
        assert_eq!(first.begin("ses", "cycle").expect("begin"), "cycle");
        assert_eq!(
            first
                .reconcile("ses", "cycle", unfinished())
                .expect("first"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 }
        );

        let restarted = PlanReconciliationDriver::with_limit(pool, 2);
        assert_eq!(
            restarted.begin("ses", "replacement").expect("resume"),
            "cycle",
            "a restarted host must retain the durable reconciliation budget"
        );
        assert_eq!(
            restarted
                .reconcile("ses", "cycle", unfinished())
                .expect("second"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 2 }
        );
        assert_eq!(
            restarted
                .reconcile("ses", "cycle", unfinished())
                .expect("wait"),
            PlanReconciliationDecision::WaitForHuman {
                reason: PlanWaitingReason::PlanUnreconciled
            }
        );
        assert_eq!(
            restarted
                .projection("ses")
                .expect("projection")
                .unwrap()
                .phase,
            DriverPhase::WaitingHuman
        );
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
            active_job: false,
            goal_active: false,
            planning_handoff: false,
        };

        assert_eq!(
            driver
                .reconcile("ses", "cycle", nothing_recorded)
                .expect("decision"),
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
            driver.reconcile("ses", "cycle", handoff).expect("decision"),
            PlanReconciliationDecision::Finish
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Terminal);
        assert_eq!(projection.reason.as_deref(), Some("planning_handoff_ready"));

        let mut active_job = handoff;
        active_job.active_job = true;
        assert_eq!(
            driver
                .reconcile("ses_job", "cycle", active_job)
                .expect("active jobs still reconcile"),
            PlanReconciliationDecision::ContinueGoal
        );

        active_job.goal_active = false;
        assert_eq!(
            driver
                .reconcile("ses_job_without_goal", "cycle", active_job)
                .expect("ordinary active jobs still reconcile"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 }
        );
    }

    #[test]
    fn a_plan_left_with_live_steps_still_spends_the_continuation_budget() {
        // The other half of the same edge: what makes a session unreconciled is a durable
        // row that is not terminal, and that must still be driven rather than delivered.
        let driver = PlanReconciliationDriver::new(pool());
        let mut only_a_todo = unfinished();
        only_a_todo.plan_exists = false;
        only_a_todo.active_todo = true;

        assert_eq!(
            driver
                .reconcile("ses", "cycle", only_a_todo)
                .expect("decision"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 },
            "an open Todo is recorded work, not a prediction about the request"
        );
    }

    #[test]
    fn an_active_goal_owns_continuation_without_spending_ordinary_attempts() {
        let driver = PlanReconciliationDriver::new(pool());
        let mut input = unfinished();
        input.goal_active = true;
        assert_eq!(
            driver.reconcile("ses", "cycle", input).expect("decision"),
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
