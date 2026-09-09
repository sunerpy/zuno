//! Durable machine phase and Plan reconciliation decisions.
//!
//! User-visible Plan steps describe strategic outcomes. This driver records the
//! machine-owned execution phase separately, then decides from typed durable
//! state whether a host may finish, should recover, must wait for background
//! completion, or should pause after durable evidence of no progress.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zuno_db::Pool;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_error::DbError;

const DRIVER_PHASE_EVENT: &str = "session.driver.phase";
const NO_PROGRESS_STREAK_LIMIT: u8 = 3;
const PROGRESS_FINGERPRINT_DOMAIN: &[u8] = b"zuno.plan.progress.v1\0";

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
    /// A background command is still observing remote work and will durably wake
    /// the session when it settles.
    pub remote_observer_running: bool,
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
    /// Recover authorized ordinary work. `attempt` is the current consecutive
    /// unchanged-progress observation and resets when the fingerprint changes.
    ContinueOrdinary {
        attempt: u8,
    },
    WaitForBackground,
    /// Compatibility-only decision retained for older hosts.
    ///
    /// The driver no longer emits this decision for unreconciled work. Lack of
    /// progress is a typed pause, not a request for new user input.
    WaitForHuman {
        reason: PlanWaitingReason,
    },
}

/// Compatibility-only reason retained for older host APIs.
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

/// Typed reason that automatic recovery paused without asking the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPauseReason {
    NoProgress,
}

impl PlanPauseReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoProgress => "no_progress",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "no_progress" => Some(Self::NoProgress),
            _ => None,
        }
    }
}

/// Detailed reconciliation outcome used by the execution-state controller.
///
/// Older hosts can continue calling [`PlanReconciliationDriver::reconcile`].
/// New hosts should call [`PlanReconciliationDriver::reconcile_with_progress`]
/// so a typed pause cannot be mistaken for a human-input request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReconciliationOutcome {
    Decision(PlanReconciliationDecision),
    Paused { reason: PlanPauseReason },
}

/// Latest durable phase projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverPhaseProjection {
    pub phase: DriverPhase,
    pub cycle_id: String,
    /// Compatibility projection of the unchanged-progress count.
    pub reconciliation_attempt: u8,
    pub reason: Option<String>,
    pub progress_fingerprint: Option<String>,
    pub unchanged_progress_count: u8,
    pub pause_reason: Option<PlanPauseReason>,
    pub sequence: i64,
}

/// Durable service that owns machine phase and final Plan reconciliation.
#[derive(Clone)]
pub struct PlanReconciliationDriver {
    events: SessionEventLog,
}

impl PlanReconciliationDriver {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            events: SessionEventLog::new(pool),
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
            self.latest_attempt_for_cycle(session_id, cycle_id)?,
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
        let legacy_fingerprint = legacy_progress_fingerprint(input);
        match self.reconcile_with_progress(
            session_id,
            cycle_id,
            input,
            true,
            &legacy_fingerprint,
        )? {
            PlanReconciliationOutcome::Decision(decision) => Ok(decision),
            PlanReconciliationOutcome::Paused { .. } => {
                // Compatibility hosts only understand terminal, continuation,
                // background wait, and human wait. Preserve the durable typed
                // pause while deliberately avoiding the human-request path.
                Ok(PlanReconciliationDecision::Finish)
            }
        }
    }

    /// Reconcile durable state with explicit execution authorization and a
    /// stable fingerprint of executable Plan/Todo/Job progress.
    ///
    /// `progress_fingerprint` should be derived from authoritative durable
    /// revisions or content digests. The driver hashes it with the typed input,
    /// persists only the bounded digest, and pauses after three consecutive
    /// identical observations. Process restarts therefore neither reset nor
    /// fabricate progress.
    pub fn reconcile_with_progress(
        &self,
        session_id: &str,
        cycle_id: &str,
        input: PlanReconciliationInput,
        work_authorized: bool,
        progress_fingerprint: &str,
    ) -> Result<PlanReconciliationOutcome, DbError> {
        let attempt = self.latest_attempt_for_cycle(session_id, cycle_id)?;
        if input.remote_observer_running {
            self.record(
                session_id,
                cycle_id,
                DriverPhase::WaitingBackground,
                attempt,
                Some("remote_observer_running".to_owned()),
            )?;
            return Ok(PlanReconciliationOutcome::Decision(
                PlanReconciliationDecision::WaitForBackground,
            ));
        }
        if input.planning_handoff {
            self.record(
                session_id,
                cycle_id,
                DriverPhase::Terminal,
                attempt,
                Some("planning_handoff_ready".to_owned()),
            )?;
            return Ok(PlanReconciliationOutcome::Decision(
                PlanReconciliationDecision::Finish,
            ));
        }
        if !input.goal_active {
            if input.settled() {
                self.record(
                    session_id,
                    cycle_id,
                    DriverPhase::Terminal,
                    attempt,
                    Some("durable_work_settled".to_owned()),
                )?;
                return Ok(PlanReconciliationOutcome::Decision(
                    PlanReconciliationDecision::Finish,
                ));
            }
            if !work_authorized {
                self.record(
                    session_id,
                    cycle_id,
                    DriverPhase::Terminal,
                    attempt,
                    Some("work_not_authorized".to_owned()),
                )?;
                return Ok(PlanReconciliationOutcome::Decision(
                    PlanReconciliationDecision::Finish,
                ));
            }
        }

        let fingerprint = stable_progress_fingerprint(input, progress_fingerprint);
        let unchanged_progress_count =
            self.progress_for_cycle(session_id, cycle_id)?
                .map_or(1, |previous| {
                    if previous.fingerprint == fingerprint {
                        previous.unchanged_count.saturating_add(1)
                    } else {
                        1
                    }
                });
        if unchanged_progress_count >= NO_PROGRESS_STREAK_LIMIT {
            self.record_progress(
                session_id,
                cycle_id,
                DriverPhase::Paused,
                unchanged_progress_count,
                Some(PlanPauseReason::NoProgress.as_str().to_owned()),
                &fingerprint,
                unchanged_progress_count,
                Some(PlanPauseReason::NoProgress),
            )?;
            return Ok(PlanReconciliationOutcome::Paused {
                reason: PlanPauseReason::NoProgress,
            });
        }

        if input.goal_active {
            self.record_progress(
                session_id,
                cycle_id,
                DriverPhase::Executing,
                unchanged_progress_count,
                Some("active_goal_owns_continuation".to_owned()),
                &fingerprint,
                unchanged_progress_count,
                None,
            )?;
            return Ok(PlanReconciliationOutcome::Decision(
                PlanReconciliationDecision::ContinueGoal,
            ));
        }

        self.record_progress(
            session_id,
            cycle_id,
            DriverPhase::Reconciling,
            unchanged_progress_count,
            Some("authorized_work_recovery".to_owned()),
            &fingerprint,
            unchanged_progress_count,
            None,
        )?;
        Ok(PlanReconciliationOutcome::Decision(
            PlanReconciliationDecision::ContinueOrdinary {
                attempt: unchanged_progress_count,
            },
        ))
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
                let progress_fingerprint = event
                    .properties
                    .get("progressFingerprint")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let unchanged_progress_count = event
                    .properties
                    .get("unchangedProgressCount")
                    .and_then(Value::as_u64)
                    .and_then(|value| u8::try_from(value).ok())
                    .unwrap_or(0);
                let pause_reason = event
                    .properties
                    .get("pauseReason")
                    .and_then(Value::as_str)
                    .and_then(PlanPauseReason::parse);
                Some(DriverPhaseProjection {
                    phase,
                    cycle_id,
                    reconciliation_attempt,
                    reason,
                    progress_fingerprint,
                    unchanged_progress_count,
                    pause_reason,
                    sequence: event.sequence,
                })
            }))
    }

    fn latest_attempt_for_cycle(&self, session_id: &str, cycle_id: &str) -> Result<u8, DbError> {
        let attempt = self
            .events
            .read_of_type_after(session_id, DRIVER_PHASE_EVENT, None)?
            .into_iter()
            .rev()
            .filter(|event| {
                event.properties.get("cycleId").and_then(Value::as_str) == Some(cycle_id)
            })
            .find_map(|event| {
                event
                    .properties
                    .get("reconciliationAttempt")
                    .and_then(Value::as_u64)
                    .and_then(|value| u8::try_from(value).ok())
            })
            .unwrap_or(0);
        Ok(attempt)
    }

    fn progress_for_cycle(
        &self,
        session_id: &str,
        cycle_id: &str,
    ) -> Result<Option<DurableProgress>, DbError> {
        Ok(self
            .events
            .read_of_type_after(session_id, DRIVER_PHASE_EVENT, None)?
            .into_iter()
            .rev()
            .filter(|event| {
                event.properties.get("cycleId").and_then(Value::as_str) == Some(cycle_id)
            })
            .find_map(|event| {
                let fingerprint = event
                    .properties
                    .get("progressFingerprint")?
                    .as_str()?
                    .to_owned();
                let unchanged_count = event
                    .properties
                    .get("unchangedProgressCount")?
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())?;
                Some(DurableProgress {
                    fingerprint,
                    unchanged_count,
                })
            }))
    }

    fn record(
        &self,
        session_id: &str,
        cycle_id: &str,
        phase: DriverPhase,
        reconciliation_attempt: u8,
        reason: Option<String>,
    ) -> Result<(), DbError> {
        self.record_state(
            session_id,
            cycle_id,
            phase,
            reconciliation_attempt,
            reason,
            None,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the durable progress event records every typed reconciliation dimension explicitly"
    )]
    fn record_progress(
        &self,
        session_id: &str,
        cycle_id: &str,
        phase: DriverPhase,
        reconciliation_attempt: u8,
        reason: Option<String>,
        progress_fingerprint: &str,
        unchanged_progress_count: u8,
        pause_reason: Option<PlanPauseReason>,
    ) -> Result<(), DbError> {
        self.record_state(
            session_id,
            cycle_id,
            phase,
            reconciliation_attempt,
            reason,
            Some(ProgressRecord {
                fingerprint: progress_fingerprint,
                unchanged_count: unchanged_progress_count,
                pause_reason,
            }),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the shared event writer keeps phase and optional progress evidence in one append"
    )]
    fn record_state(
        &self,
        session_id: &str,
        cycle_id: &str,
        phase: DriverPhase,
        reconciliation_attempt: u8,
        reason: Option<String>,
        progress: Option<ProgressRecord<'_>>,
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
        if let Some(progress) = progress {
            properties.insert(
                "progressFingerprint".to_owned(),
                Value::String(progress.fingerprint.to_owned()),
            );
            properties.insert(
                "unchangedProgressCount".to_owned(),
                Value::from(progress.unchanged_count),
            );
            if let Some(pause_reason) = progress.pause_reason {
                properties.insert(
                    "pauseReason".to_owned(),
                    Value::String(pause_reason.as_str().to_owned()),
                );
            }
        }
        self.events
            .append(
                session_id,
                NewSessionEvent::new(DRIVER_PHASE_EVENT, properties)?,
            )
            .map(|_| ())
    }
}

struct DurableProgress {
    fingerprint: String,
    unchanged_count: u8,
}

struct ProgressRecord<'a> {
    fingerprint: &'a str,
    unchanged_count: u8,
    pause_reason: Option<PlanPauseReason>,
}

fn legacy_progress_fingerprint(input: PlanReconciliationInput) -> String {
    stable_progress_fingerprint(input, "legacy-typed-state")
}

fn stable_progress_fingerprint(input: PlanReconciliationInput, source: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(PROGRESS_FINGERPRINT_DOMAIN);
    digest.update([
        u8::from(input.plan_exists),
        u8::from(input.plan_terminal),
        u8::from(input.active_todo),
        u8::from(input.active_job),
        u8::from(input.goal_active),
    ]);
    digest.update((source.len() as u64).to_be_bytes());
    digest.update(source.as_bytes());
    format!("sha256:{}", hex::encode(digest.finalize()))
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
            remote_observer_running: false,
            goal_active: false,
            planning_handoff: false,
        }
    }

    #[test]
    fn changing_progress_survives_a_driver_restart_without_requesting_human_input() {
        let pool = pool();
        let first = PlanReconciliationDriver::new(Arc::clone(&pool));
        assert_eq!(first.begin("ses", "cycle").expect("begin"), "cycle");
        assert_eq!(
            first
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-1")
                .expect("first"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );

        let restarted = PlanReconciliationDriver::new(pool);
        assert_eq!(
            restarted.begin("ses", "replacement").expect("resume"),
            "cycle",
            "a restarted host must retain the durable reconciliation cycle"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-2")
                .expect("second"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            }),
            "authoritative progress resets the no-progress streak"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-3")
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
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-1")
                .expect("first"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 1
            })
        );
        assert_eq!(
            first
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-1")
                .expect("second"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueOrdinary {
                attempt: 2
            })
        );

        let restarted = PlanReconciliationDriver::new(pool);
        assert_eq!(
            restarted.begin("ses", "replacement").expect("resume"),
            "cycle",
            "the persisted reconciliation phase must restore the original cycle"
        );
        assert_eq!(
            restarted
                .reconcile_with_progress("ses", "cycle", unfinished(), true, "plan-revision-1")
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

    #[test]
    fn compatibility_entry_finishes_a_no_progress_pause_without_human_wait() {
        let driver = PlanReconciliationDriver::new(pool());
        assert_eq!(
            driver
                .reconcile("ses", "cycle", unfinished())
                .expect("first"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 1 }
        );
        assert_eq!(
            driver
                .reconcile("ses", "cycle", unfinished())
                .expect("second"),
            PlanReconciliationDecision::ContinueOrdinary { attempt: 2 }
        );
        assert_eq!(
            driver
                .reconcile("ses", "cycle", unfinished())
                .expect("typed pause through compatibility entry"),
            PlanReconciliationDecision::Finish,
            "a no-progress pause must not enter the legacy human-request path"
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Paused);
        assert_eq!(projection.pause_reason, Some(PlanPauseReason::NoProgress));
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
            remote_observer_running: false,
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
                .expect("planning handoff does not execute active jobs"),
            PlanReconciliationDecision::Finish
        );

        active_job.goal_active = false;
        assert_eq!(
            driver
                .reconcile("ses_job_without_goal", "cycle", active_job)
                .expect("planning handoff remains read-only"),
            PlanReconciliationDecision::Finish
        );
    }

    #[test]
    fn ordinary_work_requires_authorization_but_an_active_goal_keeps_continuation() {
        let driver = PlanReconciliationDriver::new(pool());
        assert_eq!(
            driver
                .reconcile_with_progress("ses", "cycle", unfinished(), false, "plan-revision-1")
                .expect("unauthorized"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::Finish)
        );
        let projection = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(projection.phase, DriverPhase::Terminal);
        assert_eq!(projection.reason.as_deref(), Some("work_not_authorized"));

        let mut goal = unfinished();
        goal.plan_exists = false;
        goal.goal_active = true;
        assert_eq!(
            driver
                .reconcile_with_progress("goal", "cycle", goal, false, "goal-revision-1")
                .expect("active goal"),
            PlanReconciliationOutcome::Decision(PlanReconciliationDecision::ContinueGoal),
            "an active Goal is continuation authority even without ordinary executable work"
        );
    }

    #[test]
    fn a_plan_left_with_live_steps_enters_authorized_recovery() {
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
    fn a_running_remote_observer_waits_without_spending_reconciliation_attempts() {
        let driver = PlanReconciliationDriver::new(pool());
        driver.begin("ses", "cycle").expect("begin");
        let mut input = unfinished();
        input.remote_observer_running = true;
        input.goal_active = true;

        assert_eq!(
            driver
                .reconcile("ses", "cycle", input)
                .expect("first background wait"),
            PlanReconciliationDecision::WaitForBackground
        );
        assert_eq!(
            driver
                .reconcile("ses", "cycle", input)
                .expect("repeated background wait"),
            PlanReconciliationDecision::WaitForBackground
        );
        let waiting = driver.projection("ses").expect("projection").unwrap();
        assert_eq!(waiting.phase, DriverPhase::WaitingBackground);
        assert_eq!(waiting.reconciliation_attempt, 0);
        assert_eq!(waiting.reason.as_deref(), Some("remote_observer_running"));
        assert_eq!(waiting.unchanged_progress_count, 0);

        input.remote_observer_running = false;
        input.goal_active = false;
        assert_eq!(
            driver
                .reconcile("ses", "cycle", input)
                .expect("observer settled"),
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
