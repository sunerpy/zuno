//! Explicit, bounded repair of a proven legacy ordinary-input false block.
//!
//! Only native v1 event provenance is authority here. Rendered errors, a completed
//! Goal, and the absence of an active turn are never repair authorization. The
//! default operation reads a snapshot; apply rechecks the entire proof under the
//! SQLite writer and the caller's shared run registry before admitting control.
//! This service neither migrates storage nor starts a model request.
//!
//! Design reference: Codex 9ba1d9eb5bbbd87ba2fc528d91ad239eea975ee9,
//! `codex-rs/core/src/session/turn_input.rs`: user, automatic and recovery
//! admission are distinct; proposed settings are validated before being applied.
//! Here only an explicit native Apply request can create the resume control.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::inbox::{
    self, DurableInputKind, InputDelivery, NewSessionInput, SessionInput, SubmissionState,
};
use zuno_db::input_receipt;
use zuno_db::session_execution;
use zuno_db::session_work_cycle::{self, SessionWorkCycle};
use zuno_engine::status::SessionRunRegistry;
use zuno_types::admission::{
    InputAdmissionReceipt, InputGateReason, InputGateRecovery, InputReceiptState,
};
use zuno_types::execution::{
    CollaborationMode, ContinuationToken, InputTriggerKind, SessionExecutionPhase,
    SessionExecutionState, SessionPauseReason, SessionReadiness, SessionScheduling,
};

mod gate_chain;
mod request_rejection;

const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 262_144;
const MAX_PROOF_BYTES: usize = 2_097_152;
const APPLIED_EVENT: &str = "session.repair.legacy_false_blocked_applied";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionRepairAction {
    #[default]
    Inspect,
    Apply {
        expected_revision: i64,
    },
}

#[derive(Debug, Clone)]
pub struct SessionRepairRequest<'a> {
    pub session_id: &'a str,
    pub input_id: &'a str,
    pub action: SessionRepairAction,
    pub at_ms: i64,
}

impl<'a> SessionRepairRequest<'a> {
    #[must_use]
    pub fn new(session_id: &'a str, input_id: &'a str) -> Self {
        Self {
            session_id,
            input_id,
            action: SessionRepairAction::Inspect,
            at_ms: zuno_db::message::now_millis(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRepairDisposition {
    Eligible,
    /// The native resume control was admitted; the saved input is not applied.
    ControlQueued,
    /// The same still-queued control was found; no write or re-admission occurred.
    AlreadyQueued,
}

/// Coordinates only: no input text, provider error, Goal objective, or secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRepairEvidence {
    pub faulty_cycle_id: String,
    pub faulty_input_id: String,
    pub faulty_turn_id: String,
    pub provider_request_id: String,
    pub input_cycle_id: String,
    pub input_revision: i64,
    pub faulty_cycle_sequence: i64,
    pub provider_failure_sequence: i64,
    pub input_cycle_sequence: i64,
    pub gate_sequence: i64,
    pub receipt_sequence: i64,
    /// Earlier gate-only inputs, oldest first. These remain retained history;
    /// only the explicit request's latest input is authorized for recovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_input_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRepairReport {
    pub session_id: String,
    pub input_id: String,
    pub disposition: SessionRepairDisposition,
    pub expected_revision: i64,
    pub execution_revision: i64,
    pub evidence: SessionRepairEvidence,
    pub recovery_cycle_id: Option<String>,
    pub control_input_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum SessionRepairRejection {
    #[error("repair requires one bounded session/input identity and a positive apply revision")]
    InvalidRequest,
    #[error("repair requires an existing format-15 database; it never migrates storage")]
    UnsupportedFormat,
    #[error("the session has an active turn or recovery lease")]
    ActiveLease,
    #[error("the database is held by another connection or writer")]
    WriterLease,
    #[error("the execution revision, input, cycle, or previously inspected evidence changed")]
    Changed,
    #[error("only a consumed, recorded, never-applied real user input is eligible")]
    IneligibleInput,
    #[error("a real execution, authentication, budget, Plan, Goal, or approval gate is present")]
    ProtectedGate,
    #[error("an unsettled or uncertain tool outcome requires its own native inspection")]
    Uncertainty,
    #[error("the bounded legacy event chain does not prove this false block")]
    Unproven,
    #[error("the legacy proof exceeds the bounded inspection window")]
    EvidenceLimit,
    #[error("a later event invalidates the legacy repair proof")]
    LateEvent,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionRepairError {
    #[error(transparent)]
    Rejected(#[from] SessionRepairRejection),
    // Do not print a decoder's source value: it may contain an input or secret.
    #[error("repair database validation or access failed")]
    Database(#[from] zuno_error::DbError),
}

type Result<T> = std::result::Result<T, SessionRepairError>;

/// Native callers must supply the actual shared registry, never a fresh registry
/// as a substitute for a live host's ownership. Standalone callers additionally
/// need SQLite EXCLUSIVE locking mode before first access to exclude other
/// processes; the CLI establishes that boundary on its dedicated connection.
pub struct SessionRepairService<'a> {
    connection: &'a mut Connection,
    runs: &'a SessionRunRegistry,
}

impl<'a> SessionRepairService<'a> {
    #[must_use]
    pub fn new(connection: &'a mut Connection, runs: &'a SessionRunRegistry) -> Self {
        Self { connection, runs }
    }

    pub fn repair(&mut self, request: SessionRepairRequest<'_>) -> Result<SessionRepairReport> {
        use SessionRepairAction::{Apply, Inspect};
        if [request.session_id, request.input_id]
            .iter()
            .any(|id| id.trim().is_empty() || id.len() > 512)
            || request.at_ms < 0
            || matches!(request.action, Apply { expected_revision } if expected_revision < 1)
        {
            return Err(SessionRepairRejection::InvalidRequest.into());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(match request.action {
                Inspect => TransactionBehavior::Deferred,
                Apply { .. } => TransactionBehavior::Immediate,
            })
            .map_err(db)?;
        // Match admission's writer-before-registry order. Hold this reservation
        // through commit/rollback, including failed validation after mutations.
        let _lease = self
            .runs
            .begin_recovery(request.session_id)
            .map_err(|_| SessionRepairRejection::ActiveLease)?;
        let outcome = repair_in(&transaction, &request);
        match outcome {
            Ok(report) => {
                if matches!(request.action, Apply { .. }) {
                    transaction.commit().map_err(db)?;
                } else {
                    transaction.rollback().map_err(db)?;
                }
                Ok(report)
            }
            Err(error) => {
                transaction.rollback().map_err(db)?;
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalWitness {
    id: String,
    revision: i64,
    status: String,
    tokens_used: i64,
    time_used_seconds: i64,
    usage_known: bool,
    token_budget: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AppliedRepair {
    report: SessionRepairReport,
    original_execution: SessionExecutionState,
    execution: SessionExecutionState,
    cycle: SessionWorkCycle,
    recovered_input_revision: i64,
    goal: Option<GoalWitness>,
}

fn repair_in(
    tx: &Transaction<'_>,
    request: &SessionRepairRequest<'_>,
) -> Result<SessionRepairReport> {
    require_format(tx)?;
    let mut state = session_execution::read_in(tx, request.session_id)?
        .ok_or(SessionRepairRejection::Unproven)?;
    let input = bounded_input(tx, request.session_id, request.input_id)?;
    let receipt = input_receipt::get_in(tx, request.session_id, request.input_id)?
        .ok_or(SessionRepairRejection::IneligibleInput)?;
    require_unapplied(&input, &receipt)?;
    let goal = protect_in(tx, request.session_id)?;
    if let Some(report) = already_applied_in(tx, request, &state, &input, &receipt, &goal)? {
        return Ok(report);
    }
    if let SessionRepairAction::Apply { expected_revision } = request.action
        && state.revision != expected_revision
    {
        return Err(SessionRepairRejection::Changed.into());
    }
    require_empty_inbox(tx, request.session_id, None)?;
    require_blocked(&state, &input, &receipt)?;
    let evidence = prove_in(
        tx,
        &state,
        &input,
        &receipt,
        latest_sequence(tx, request.session_id)?,
    )?;
    let mut report = SessionRepairReport {
        session_id: request.session_id.to_owned(),
        input_id: request.input_id.to_owned(),
        disposition: SessionRepairDisposition::Eligible,
        expected_revision: state.revision,
        execution_revision: state.revision,
        evidence,
        recovery_cycle_id: None,
        control_input_id: None,
    };
    if request.action == SessionRepairAction::Inspect {
        return Ok(report);
    }
    let original_execution = state.clone();
    let old_token = state
        .continuation
        .as_ref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let token = ContinuationToken {
        cycle_id: format!("cycle_{}", uuid::Uuid::now_v7().simple()),
        identity: old_token.identity.clone(),
        mode: CollaborationMode::Work,
        plan_id: None,
        plan_revision: None,
        context_epoch: old_token.context_epoch,
        anchor_message_id: Some(input.id.clone()),
    };
    let cycle = SessionWorkCycle {
        session_id: request.session_id.to_owned(),
        cycle_id: token.cycle_id.clone(),
        anchor_message_id: Some(input.id.clone()),
        goal_id: None,
        plan_id: None,
        todo_ids: Default::default(),
        active_turn_id: None,
        resumed_goal_cycles: Default::default(),
        stopped: None,
        scheduling: None,
    };
    let at_ms = request
        .at_ms
        .max(state.time_updated)
        .max(input.time_updated);
    let control = inbox::admit_in(
        tx,
        NewSessionInput::new(
            format!("ctl_{}", uuid::Uuid::now_v7().simple()),
            request.session_id,
            json!({"kind":"sessionControl","control":"resume_work","continuation":token}),
            InputDelivery::Queue,
            at_ms,
        )
        .with_source_key(source_key(request.input_id, state.revision))
        .with_trigger_kind(InputTriggerKind::UserControl)
        .with_cycle_id(Some(token.cycle_id.clone())),
    )?;
    session_work_cycle::save_in(tx, &cycle, at_ms)?;
    append(
        tx,
        request.session_id,
        "session.work_cycle.authorized",
        json!({"cycle":cycle,"time":at_ms}),
    )?;
    // This deliberately keeps the user's row consumed and its frozen gate
    // visible until a real provider turn binds it. Never admit/requeue the user.
    if !input_receipt::recover_gated_input_in(
        tx,
        request.session_id,
        &input.id,
        &token.cycle_id,
        at_ms,
    )? {
        return Err(SessionRepairRejection::Changed.into());
    }
    state.phase = SessionExecutionPhase::Authorized;
    state.scheduling = Some(SessionScheduling::default());
    state.cycle_id = Some(token.cycle_id.clone());
    state.continuation = Some(token);
    state.time_updated = at_ms;
    let state = session_execution::update_in(tx, report.expected_revision, state)?;
    report.disposition = SessionRepairDisposition::ControlQueued;
    report.execution_revision = state.revision;
    report.recovery_cycle_id = state.cycle_id.clone();
    report.control_input_id = Some(control.id);
    let applied = AppliedRepair {
        report: report.clone(),
        original_execution,
        execution: state,
        cycle,
        recovered_input_revision: input
            .revision
            .checked_add(1)
            .ok_or(SessionRepairRejection::Changed)?,
        goal,
    };
    append(
        tx,
        request.session_id,
        APPLIED_EVENT,
        serde_json::to_value(applied).map_err(|_| SessionRepairRejection::Unproven)?,
    )?;
    Ok(report)
}

fn require_format(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "zuno_schema")? {
        return Err(SessionRepairRejection::UnsupportedFormat.into());
    }
    let format: Option<i64> = connection
        .query_row(
            "SELECT format FROM zuno_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    if format != Some(15) {
        return Err(SessionRepairRejection::UnsupportedFormat.into());
    }
    Ok(())
}

fn bounded_input(connection: &Connection, session: &str, id: &str) -> Result<SessionInput> {
    let size: Option<i64> = connection
        .query_row(
            "SELECT length(CAST(prompt AS BLOB)) FROM session_input WHERE session_id=?1 AND id=?2",
            params![session, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    let size = size
        .map(usize::try_from)
        .transpose()
        .map_err(|_| SessionRepairRejection::EvidenceLimit)?;
    if size.is_some_and(|size| size > MAX_EVENT_BYTES) {
        return Err(SessionRepairRejection::EvidenceLimit.into());
    }
    inbox::read_in(connection, session, id)?.ok_or(SessionRepairRejection::IneligibleInput.into())
}

fn require_unapplied(input: &SessionInput, receipt: &InputAdmissionReceipt) -> Result<()> {
    if !real_user(input)
        || input.state != SubmissionState::Consumed
        || input.error.is_some()
        || receipt.state != InputReceiptState::Recorded
        || receipt.turn_id.is_some()
        || receipt.applied_at.is_some()
        || receipt.completed_at.is_some()
        || receipt.stop_reason.is_some()
        || receipt.error.is_some()
    {
        return Err(SessionRepairRejection::IneligibleInput.into());
    }
    Ok(())
}

fn real_user(input: &SessionInput) -> bool {
    matches!(
        input.trigger_kind,
        InputTriggerKind::User | InputTriggerKind::Legacy
    ) && matches!(
        DurableInputKind::classify(&input.prompt),
        Some(
            DurableInputKind::User
                | DurableInputKind::AcpPrompt
                | DurableInputKind::TuiPrompt
                | DurableInputKind::HostMessage
        )
    )
}

fn protect_in(connection: &Connection, session: &str) -> Result<Option<GoalWitness>> {
    let owned: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM event_sequence WHERE aggregate_id=?1 AND owner_id IS NOT NULL)",
        [session], |row| row.get(0),
    ).map_err(db)?;
    if owned {
        return Err(SessionRepairRejection::ActiveLease.into());
    }
    let archived: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1 AND time_archived IS NOT NULL)",
            [session],
            |row| row.get(0),
        )
        .map_err(db)?;
    if archived {
        return Err(SessionRepairRejection::ProtectedGate.into());
    }
    let pending: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM human_request WHERE session_id=?1 AND state='pending')
             OR EXISTS(SELECT 1 FROM question_interaction q JOIN human_request h ON h.id=q.request_id
               WHERE h.session_id=?1 AND h.state='answered' AND q.purpose='plan_authorization'
               AND (q.authorization IS NULL OR q.authorization='waiting_for_handoff'))",
            [session],
            |row| row.get(0),
        )
        .map_err(db)?;
    if pending {
        return Err(SessionRepairRejection::ProtectedGate.into());
    }
    // Same uncertainty fields as MessageStore::pending_uncertain_tool_calls;
    // EXISTS is bounded in memory and does not render or acknowledge any call.
    let uncertain: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM part WHERE session_id=?1
         AND json_extract(data,'$.type')='tool'
         AND (json_extract(data,'$.state.status') IN ('pending','running')
          OR (json_extract(data,'$.state.outcome')='uncertain'
              AND json_extract(data,'$.state.uncertain.reconciledAtMs') IS NULL)))",
            [session],
            |row| row.get(0),
        )
        .map_err(db)?;
    if uncertain {
        return Err(SessionRepairRejection::Uncertainty.into());
    }
    let goal = if table_exists(connection, "goal")? {
        connection.query_row(
            "SELECT goal_id,revision,status,tokens_used,time_used_seconds,usage_known,token_budget
             FROM goal WHERE session_id=?1", [session],
            |row| Ok(GoalWitness { id: row.get(0)?, revision: row.get(1)?, status: row.get(2)?,
                tokens_used: row.get(3)?, time_used_seconds: row.get(4)?, usage_known: row.get(5)?,
                token_budget: row.get(6)? }),
        ).optional().map_err(db)?
    } else {
        None
    };
    if goal
        .as_ref()
        .is_some_and(|goal| !matches!(goal.status.as_str(), "complete" | "cancelled"))
    {
        return Err(SessionRepairRejection::ProtectedGate.into());
    }
    for table in [
        "goal_pause",
        "goal_retry",
        "goal_pending_failure_signal",
        "goal_continuation_deferral",
    ] {
        if table_exists(connection, table)? {
            let present: bool = connection
                .query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE session_id=?1)"),
                    [session],
                    |row| row.get(0),
                )
                .map_err(db)?;
            if present {
                return Err(SessionRepairRejection::ProtectedGate.into());
            }
        }
    }
    let plan_goal: Option<Option<String>> = connection
        .query_row(
            "SELECT goal_id FROM work_plan WHERE session_id=?1",
            [session],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    if let Some(plan_goal) = plan_goal
        && (plan_goal.is_none()
            || plan_goal.as_deref() != goal.as_ref().map(|goal| goal.id.as_str()))
    {
        return Err(SessionRepairRejection::ProtectedGate.into());
    }
    Ok(goal)
}

fn require_empty_inbox(connection: &Connection, session: &str, except: Option<&str>) -> Result<()> {
    let pending: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_input WHERE session_id=?1
         AND state IN ('queued','steering','promoted') AND (?2 IS NULL OR id<>?2))",
            params![session, except],
            |row| row.get(0),
        )
        .map_err(db)?;
    if pending {
        return Err(SessionRepairRejection::ActiveLease.into());
    }
    Ok(())
}

fn require_blocked(
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
) -> Result<()> {
    let gate = receipt
        .execution_gate
        .as_ref()
        .ok_or(SessionRepairRejection::Unproven)?;
    if state.mode != CollaborationMode::Work
        || !matches!(
            state.phase,
            SessionExecutionPhase::Paused | SessionExecutionPhase::Blocked
        )
        || !blocked(state.scheduling.as_ref())
        || state.authorized_plan_id.is_some()
        || state.authorized_plan_revision.is_some()
        || state.handoff_plan_id.is_some()
        || state.handoff_plan_revision.is_some()
        || state.draft_review_risk.is_some()
        || gate.reason != InputGateReason::Blocked
        || gate.recovery != InputGateRecovery::InspectSession
        || gate.request_id.is_some()
        || gate.source_id.is_some()
    {
        return Err(SessionRepairRejection::ProtectedGate.into());
    }
    if state.revision != gate.execution_revision
        || input.cycle_id.as_deref() != Some(&gate.cycle_id)
        || state.cycle_id != input.cycle_id
    {
        return Err(SessionRepairRejection::Changed.into());
    }
    let token = state
        .continuation
        .as_ref()
        .ok_or(SessionRepairRejection::Unproven)?;
    if token.cycle_id != gate.cycle_id
        || token.anchor_message_id.as_deref() != Some(&input.id)
        || token.mode != CollaborationMode::Work
        || token.plan_id.is_some()
        || token.plan_revision.is_some()
        || state.work_identity.as_ref() != Some(&token.identity)
    {
        return Err(SessionRepairRejection::Changed.into());
    }
    Ok(())
}

fn blocked(scheduling: Option<&SessionScheduling>) -> bool {
    scheduling.is_some_and(|s| {
        s.readiness
            == SessionReadiness::Paused {
                reason: SessionPauseReason::Blocked,
            }
    })
}

struct Event {
    sequence: i64,
    kind: String,
    data: Value,
}

fn cycle_started(connection: &Connection, session: &str, cycle: &str) -> Result<Event> {
    let mut statement = connection
        .prepare(
            "SELECT seq,type,CASE WHEN length(CAST(data AS BLOB))<=?3 THEN data END FROM event
         WHERE aggregate_id=?1 AND type='session.work_cycle.started.1'
         AND json_extract(data,'$.cycle.cycleId')=?2 ORDER BY seq LIMIT 2",
        )
        .map_err(db)?;
    let mut rows = statement
        .query(params![session, cycle, sql_limit(MAX_EVENT_BYTES)?])
        .map_err(db)?;
    let event = read_event(
        rows.next()
            .map_err(db)?
            .ok_or(SessionRepairRejection::Unproven)?,
    )?;
    if rows.next().map_err(db)?.is_some() {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(event)
}

fn read_event(row: &rusqlite::Row<'_>) -> Result<Event> {
    let raw: Option<String> = row.get(2).map_err(db)?;
    let raw = raw.ok_or(SessionRepairRejection::EvidenceLimit)?;
    Ok(Event {
        sequence: row.get(0).map_err(db)?,
        kind: row.get(1).map_err(db)?,
        data: serde_json::from_str(&raw).map_err(|_| SessionRepairRejection::Unproven)?,
    })
}

fn window(connection: &Connection, session: &str, first: i64, last: i64) -> Result<Vec<Event>> {
    let mut statement = connection
        .prepare(
            "SELECT seq,type,CASE
           WHEN type IN ('session.work_cycle.started.1','session.input.admitted.1',
             'session.input.promoted.1','session.input.consumed.1','session.input.receipt.1',
             'session.input.execution_gate.1','session.turn.started.1',
             'session.provider.request.1','session.provider.attempt.1','session.driver.phase.1',
             'session.work_cycle.authorized.1','session.input.execution_recovered.1',
             'session.repair.legacy_false_blocked_applied.1','session.turn.failure_settled.1')
           THEN CASE WHEN length(CAST(data AS BLOB))<=?3 THEN data END ELSE '{}' END
         FROM event WHERE aggregate_id=?1 AND seq>=?2 AND seq<=?5 ORDER BY seq LIMIT ?4",
        )
        .map_err(db)?;
    let mut rows = statement
        .query(params![
            session,
            first,
            sql_limit(MAX_EVENT_BYTES)?,
            sql_limit(MAX_EVENTS + 1)?,
            last,
        ])
        .map_err(db)?;
    let mut result = Vec::new();
    let mut bytes = 0_usize;
    while let Some(row) = rows.next().map_err(db)? {
        let event = read_event(row)?;
        bytes = bytes
            .checked_add(event.data.to_string().len())
            .ok_or(SessionRepairRejection::EvidenceLimit)?;
        if result.len() == MAX_EVENTS || bytes > MAX_PROOF_BYTES {
            return Err(SessionRepairRejection::EvidenceLimit.into());
        }
        if Some(event.sequence) != first.checked_add(sql_limit(result.len())?) {
            return Err(SessionRepairRejection::Unproven.into());
        }
        result.push(event);
    }
    Ok(result)
}

fn one<'a>(
    events: &'a [Event],
    kind: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<&'a Event> {
    let mut matching = events
        .iter()
        .filter(|event| event.kind == kind && predicate(&event.data));
    let found = matching.next().ok_or(SessionRepairRejection::Unproven)?;
    if matching.next().is_some() {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(found)
}

fn independent_cycle(
    value: &Value,
    session: &str,
    id: &str,
    anchor: &str,
) -> Result<SessionWorkCycle> {
    if value.get("goalId") != Some(&Value::Null) || value.get("planId") != Some(&Value::Null) {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let cycle: SessionWorkCycle =
        serde_json::from_value(value.clone()).map_err(|_| SessionRepairRejection::Unproven)?;
    if cycle.session_id != session
        || cycle.cycle_id != id
        || cycle.anchor_message_id.as_deref() != Some(anchor)
        || cycle.stopped.is_some()
        || !cycle.todo_ids.is_empty()
        || !cycle.resumed_goal_cycles.is_empty()
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(cycle)
}

fn prove_in(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
) -> Result<SessionRepairEvidence> {
    if let Some(evidence) =
        gate_chain::prove_in(connection, state, input, receipt, through_sequence)?
    {
        return Ok(evidence);
    }
    if let Some(evidence) =
        request_rejection::prove_in(connection, state, input, receipt, through_sequence)?
    {
        return Ok(evidence);
    }
    prove_legacy_in(connection, state, input, receipt, through_sequence)
}

fn prove_legacy_in(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
) -> Result<SessionRepairEvidence> {
    let session = &input.session_id;
    let cycle_id = input
        .cycle_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let started = cycle_started(connection, session, cycle_id)?;
    let initial = independent_cycle(&started.data["cycle"], session, cycle_id, &input.id)?;
    let current =
        bounded_cycle(connection, session, cycle_id)?.ok_or(SessionRepairRejection::Unproven)?;
    if initial != current
        || current.active_turn_id.is_some()
        || current.scheduling.is_some()
        || started.data["inputId"] != input.id
        || started.data["protectedGateRetained"] != true
        || started.data["legacyInterruption"] != false
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let faulty_id = started.data["previousCycleId"]
        .as_str()
        .ok_or(SessionRepairRejection::Unproven)?;
    if faulty_id == cycle_id {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let faulty =
        bounded_cycle(connection, session, faulty_id)?.ok_or(SessionRepairRejection::Unproven)?;
    let faulty_input_id = faulty
        .anchor_message_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let faulty_start = cycle_started(connection, session, faulty_id)?;
    let original_faulty = independent_cycle(
        &faulty_start.data["cycle"],
        session,
        faulty_id,
        faulty_input_id,
    )?;
    let previous_scheduling: SessionScheduling =
        serde_json::from_value(started.data["previousScheduling"].clone())
            .map_err(|_| SessionRepairRejection::Unproven)?;
    if faulty.goal_id.is_some()
        || faulty.plan_id.is_some()
        || faulty.stopped.is_some()
        || !faulty.todo_ids.is_empty()
        || !faulty.resumed_goal_cycles.is_empty()
        || original_faulty.active_turn_id.is_some()
        || original_faulty.scheduling.is_some()
        || faulty_start.data["protectedGateRetained"] != false
        || faulty_start.data["previousScheduling"]["readiness"]["kind"] != "ready"
        || faulty_start.data["inputId"] != faulty_input_id
        || !blocked(Some(&previous_scheduling))
        || state.scheduling.as_ref() != Some(&previous_scheduling)
        || faulty.scheduling.as_ref() != Some(&previous_scheduling)
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let old_input = bounded_input(connection, session, faulty_input_id)?;
    let old_receipt = input_receipt::get_in(connection, session, faulty_input_id)?
        .ok_or(SessionRepairRejection::Unproven)?;
    let turn = old_receipt
        .turn_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    if !real_user(&old_input)
        || old_input.state != SubmissionState::Consumed
        || old_input.cycle_id.as_deref() != Some(faulty_id)
        || old_receipt.state != InputReceiptState::Failed
        || old_receipt.applied_at.is_none()
        || old_receipt.completed_at.is_none()
        || faulty.active_turn_id.as_deref() != Some(turn)
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    for id in [&input.id, &old_input.id] {
        let user: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM message WHERE id=?1 AND session_id=?2 AND json_extract(data,'$.role')='user')",
            params![id, session], |row| row.get(0),
        ).map_err(db)?;
        if !user {
            return Err(SessionRepairRejection::Unproven.into());
        }
    }
    let events = window(
        connection,
        session,
        old_input.admitted_sequence,
        through_sequence,
    )?;
    for row in [&old_input, input] {
        let admitted = one(&events, "session.input.admitted.1", |d| {
            d["inputID"] == row.id
        })?;
        let consumed = one(&events, "session.input.consumed.1", |d| {
            d["inputID"] == row.id
        })?;
        let promoted = one(&events, "session.input.promoted.1", |d| {
            d["inputID"] == row.id
        })?;
        if admitted.sequence != row.admitted_sequence
            || Some(promoted.sequence) != row.promoted_sequence
            || admitted.data["prompt"] != row.prompt
            || admitted.data["sessionID"] != *session
            || admitted.data["triggerKind"] != row.trigger_kind.as_str()
            || admitted.data["delivery"] != row.delivery.as_str()
            || admitted
                .data
                .get("sourceKey")
                .cloned()
                .unwrap_or(Value::Null)
                != json!(row.source_key)
            || consumed.data["revision"] != row.revision
            || consumed.data["cycleID"] != json!(row.cycle_id)
            || consumed.data["state"] != "consumed"
            || consumed.data["sessionID"] != *session
            || consumed.data["triggerKind"] != row.trigger_kind.as_str()
            || consumed.data["delivery"] != row.delivery.as_str()
            || !(admitted.sequence < promoted.sequence && promoted.sequence < consumed.sequence)
        {
            return Err(SessionRepairRejection::Changed.into());
        }
    }
    if events
        .iter()
        .filter(|e| e.kind == "session.work_cycle.started.1")
        .count()
        != 2
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let turn_started = one(&events, "session.turn.started.1", |_| true)?;
    if turn_started.data["anchorMessageID"] != old_input.id
        || turn_started.data["turnID"] != turn
        || turn_started.data["turnTrigger"] != "user"
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let request_started = one(&events, "session.provider.request.1", |d| {
        d["status"] == "started"
    })?;
    let request_failed = one(&events, "session.provider.request.1", |d| {
        d["status"] == "failed"
    })?;
    let request_id = request_failed.data["requestID"]
        .as_str()
        .ok_or(SessionRepairRejection::Unproven)?;
    if events
        .iter()
        .filter(|e| e.kind == "session.provider.request.1")
        .count()
        != 2
        || events
            .iter()
            .filter(|e| e.kind == "session.input.execution_gate.1")
            .count()
            != 1
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let deadline = one(&events, "session.provider.attempt.1", |d| {
        d["status"] == "failed" && d["turnErrorKind"] == "provider_retry_deadline"
    })?;
    if request_failed.data["errorKind"] != "provider_retry_deadline"
        || request_failed.data["turnID"] != turn
        || request_started.data["requestID"] != request_id
        || request_started.data["turnID"] != turn
        || !request_started.data["inputIDs"]
            .as_array()
            .is_some_and(|ids| {
                ids.contains(&json!(old_input.id)) && !ids.contains(&json!(input.id))
            })
        || deadline.data["requestID"] != request_id
        || deadline.data["turnID"] != turn
        || deadline.data["turnTrigger"] != "user"
        || deadline.data["retryable"] != true
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    for attempt in events
        .iter()
        .filter(|e| e.kind == "session.provider.attempt.1")
    {
        if attempt.data["turnID"] != turn
            || attempt.data["turnTrigger"] != "user"
            || attempt.data["requestID"] != request_id
            || !matches!(attempt.data["status"].as_str(), Some("started" | "failed"))
            || (attempt.data["status"] == "failed"
                && attempt.sequence != deadline.sequence
                && !(attempt.data["turnErrorKind"] == "provider"
                    && matches!(
                        attempt.data["errorKind"].as_str(),
                        Some("transient" | "rate_limited")
                    )
                    && attempt.data["retryable"] == true))
            || attempt.sequence > deadline.sequence
        {
            return Err(SessionRepairRejection::Unproven.into());
        }
    }
    let gate = one(&events, "session.input.execution_gate.1", |d| {
        d["inputId"] == input.id
    })?;
    let recorded = one(&events, "session.input.receipt.1", |d| {
        d["inputId"] == input.id
    })?;
    let failed_receipt = one(&events, "session.input.receipt.1", |d| {
        d["inputId"] == old_input.id && d["state"] == "failed"
    })?;
    let consumed = one(&events, "session.input.consumed.1", |d| {
        d["inputID"] == input.id
    })?;
    let gate_value = serde_json::to_value(&receipt.execution_gate)
        .map_err(|_| SessionRepairRejection::Unproven)?;
    if gate.data["gate"] != gate_value
        || recorded.data["executionGate"] != gate_value
        || recorded.data["state"] != "recorded"
        || recorded.data["sessionId"] != *session
        || recorded.data["admittedSequence"] != input.admitted_sequence
        || recorded.data["delivery"] != input.delivery.as_str()
        || recorded.data.get("turnId").is_some_and(|v| !v.is_null())
        || recorded.data.get("appliedAt").is_some_and(|v| !v.is_null())
        || recorded
            .data
            .get("completedAt")
            .is_some_and(|v| !v.is_null())
        || recorded.data.get("error").is_some_and(|v| !v.is_null())
        || failed_receipt.data["turnId"] != turn
        || failed_receipt.data["appliedAt"] != json!(old_receipt.applied_at)
        || failed_receipt.data["completedAt"] != json!(old_receipt.completed_at)
        || failed_receipt.data["admittedSequence"] != old_input.admitted_sequence
        || !(faulty_start.sequence < turn_started.sequence
            && turn_started.sequence < request_started.sequence
            && request_started.sequence < deadline.sequence
            && deadline.sequence < request_failed.sequence
            && request_failed.sequence < failed_receipt.sequence
            && failed_receipt.sequence < input.admitted_sequence
            && input.admitted_sequence < started.sequence
            && started.sequence < consumed.sequence
            && consumed.sequence < gate.sequence
            && gate.sequence.checked_add(1) == Some(recorded.sequence))
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    // Only the observed native chain and these inert observations are known.
    // Unknown versions, controls, stops and gates cannot be ignored anywhere in
    // the proof, even when they did not increment the execution revision.
    for event in &events {
        let known = match event.kind.as_str() {
            "session.input.admitted.1"
            | "session.input.promoted.1"
            | "session.input.consumed.1" => {
                event.data["inputID"] == old_input.id || event.data["inputID"] == input.id
            }
            "session.input.receipt.1"
            | "session.work_cycle.started.1"
            | "session.input.execution_gate.1" => true,
            "session.turn.started.1"
            | "session.provider.request.1"
            | "session.provider.attempt.1" => event.sequence <= request_failed.sequence,
            "session.driver.phase.1" => {
                event.data["cycleId"] == faulty_id
                    && event.data["phase"] == "executing"
                    && event.sequence < turn_started.sequence
            }
            "session.prompt.assembled.1" | "learning.retrieval.selected.1" => {
                event.sequence < request_started.sequence
            }
            "session.context.usage.1"
            | "learning.consolidation.request.1"
            | "learning.consolidation.outcome.1" => true,
            _ => false,
        };
        if !known {
            return Err(if event.sequence > request_failed.sequence {
                SessionRepairRejection::LateEvent
            } else {
                SessionRepairRejection::Unproven
            }
            .into());
        }
    }
    if through_sequence != recorded.sequence
        || events
            .last()
            .is_none_or(|event| event.sequence != recorded.sequence)
    {
        return Err(SessionRepairRejection::LateEvent.into());
    }
    Ok(SessionRepairEvidence {
        faulty_cycle_id: faulty_id.to_owned(),
        faulty_input_id: old_input.id,
        faulty_turn_id: turn.to_owned(),
        provider_request_id: request_id.to_owned(),
        input_cycle_id: cycle_id.to_owned(),
        input_revision: input.revision,
        faulty_cycle_sequence: faulty_start.sequence,
        provider_failure_sequence: request_failed.sequence,
        input_cycle_sequence: started.sequence,
        gate_sequence: gate.sequence,
        receipt_sequence: recorded.sequence,
        inherited_input_ids: Vec::new(),
    })
}

fn already_applied_in(
    connection: &Connection,
    request: &SessionRepairRequest<'_>,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    goal: &Option<GoalWitness>,
) -> Result<Option<SessionRepairReport>> {
    let original_revision = receipt
        .execution_gate
        .as_ref()
        .ok_or(SessionRepairRejection::Unproven)?
        .execution_revision;
    let key = source_key(request.input_id, original_revision);
    let Some(control) = inbox::read_by_source_key_in(connection, request.session_id, &key)? else {
        return Ok(None);
    };
    let stored: Option<(i64, Option<String>)> = connection
        .query_row(
            "SELECT seq,CASE WHEN length(CAST(data AS BLOB))<=?4 THEN data END FROM event
         WHERE aggregate_id=?1 AND type=?2 AND json_extract(data,'$.report.inputId')=?3
         ORDER BY seq DESC LIMIT 1",
            params![
                request.session_id,
                format!("{APPLIED_EVENT}.1"),
                request.input_id,
                sql_limit(MAX_EVENT_BYTES)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(db)?;
    let (sequence, raw) = stored.ok_or(SessionRepairRejection::Unproven)?;
    let applied: AppliedRepair =
        serde_json::from_str(&raw.ok_or(SessionRepairRejection::EvidenceLimit)?)
            .map_err(|_| SessionRepairRejection::Unproven)?;
    if matches!(request.action, SessionRepairAction::Apply { expected_revision } if expected_revision != original_revision)
        || applied.report.expected_revision != original_revision
        || applied.report.session_id != request.session_id
        || applied.report.input_id != request.input_id
        || applied.execution != *state
        || applied.recovered_input_revision != input.revision
        || input.cycle_id != state.cycle_id
        || applied.goal != *goal
        || applied.report.control_input_id.as_deref() != Some(&control.id)
        || control.state != SubmissionState::Queued
        || control.trigger_kind != InputTriggerKind::UserControl
        || control.cycle_id != state.cycle_id
        || control.prompt
            != json!({"kind":"sessionControl","control":"resume_work","continuation":state.continuation})
        || bounded_cycle(
            connection,
            request.session_id,
            state
                .cycle_id
                .as_deref()
                .ok_or(SessionRepairRejection::Changed)?,
        )?
        .as_ref()
            != Some(&applied.cycle)
    {
        return Err(SessionRepairRejection::Changed.into());
    }
    if latest_sequence(connection, request.session_id)? != sequence {
        return Err(SessionRepairRejection::LateEvent.into());
    }
    require_empty_inbox(connection, request.session_id, Some(&control.id))?;
    // Idempotency is an acknowledgement of this exact repair, not a shortcut
    // around a changed input or edited historical proof.
    let mut original_input = input.clone();
    original_input.revision = applied.report.evidence.input_revision;
    original_input.cycle_id = Some(applied.report.evidence.input_cycle_id.clone());
    require_blocked(&applied.original_execution, &original_input, receipt)?;
    let evidence = prove_in(
        connection,
        &applied.original_execution,
        &original_input,
        receipt,
        applied.report.evidence.receipt_sequence,
    )?;
    if evidence != applied.report.evidence {
        return Err(SessionRepairRejection::Changed.into());
    }
    let mut report = applied.report;
    report.disposition = SessionRepairDisposition::AlreadyQueued;
    Ok(Some(report))
}

fn source_key(input: &str, revision: i64) -> String {
    format!("user-control:repair-legacy-false-blocked:{input}:{revision}")
}

fn latest_sequence(connection: &Connection, session: &str) -> Result<i64> {
    let (allocated, stored): (i64, Option<i64>) = connection
        .query_row(
            "SELECT seq,(SELECT max(seq) FROM event WHERE aggregate_id=?1)
             FROM event_sequence WHERE aggregate_id=?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db)?;
    if stored != Some(allocated) {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(allocated)
}

fn bounded_cycle(
    connection: &Connection,
    session: &str,
    cycle: &str,
) -> Result<Option<SessionWorkCycle>> {
    let size: Option<i64> = connection.query_row(
        "SELECT length(CAST(data AS BLOB)) FROM session_work_cycle WHERE session_id=?1 AND cycle_id=?2",
        params![session,cycle], |row| row.get(0),
    ).optional().map_err(db)?;
    if size.is_some_and(|size| usize::try_from(size).map_or(true, |size| size > MAX_EVENT_BYTES)) {
        return Err(SessionRepairRejection::EvidenceLimit.into());
    }
    session_work_cycle::read_in(connection, session, cycle).map_err(Into::into)
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(db)
}

fn append(tx: &Transaction<'_>, session: &str, kind: &str, value: Value) -> Result<()> {
    append_in(
        tx,
        session,
        NewSessionEvent::new(
            kind,
            value
                .as_object()
                .ok_or(SessionRepairRejection::Unproven)?
                .clone(),
        )?,
    )?;
    Ok(())
}

fn db(error: rusqlite::Error) -> SessionRepairError {
    if zuno_db::open::is_busy(&error) {
        return SessionRepairRejection::WriterLease.into();
    }
    zuno_db::open::map_error(error).into()
}

fn sql_limit(value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| SessionRepairRejection::EvidenceLimit.into())
}
