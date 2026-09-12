//! Bounded advancement of the ordinary agent loop.
//!
//! A checkpoint records a completed step or an exact durable tool wait.
//! Interrupted advances without such a boundary require inspection; a checkpoint
//! never authorizes replay of an external effect or grants an environment lease.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zuno_db::event_log::{NewSessionEvent, SessionEvent, append_in, latest_of_type_in};
use zuno_db::{Connection, TransactionBehavior, session};
use zuno_error::DbError;
use zuno_llm::cache::DynamicContext;
use zuno_tool::ToolDynamicContextRefresh;
use zuno_types::identity::PrincipalKey;

use crate::budget::{BudgetStop, ProviderRequestUsage};
use crate::r#loop::{
    RequestedTurn, RunTurnRequest, ToolFailureRecovery, TurnError, TurnOutcome, TurnRecovery,
};
use crate::prompt::PromptTraceSet;

pub use crate::r#loop::tool_step::ToolStepCheckpoint;

const EVENT_TYPE: &str = "runtime.driver.advance";
const SCHEMA_VERSION: u32 = 4;
pub const DRIVER_CHECKPOINT_VERSION: u32 = SCHEMA_VERSION;
pub const fn supports_checkpoint_schema(version: u32) -> bool {
    matches!(version, 3 | 4)
}
const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;

/// A durable reference, not a caller-supplied replacement checkpoint body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointRef {
    session_id: String,
    turn_id: String,
    event_id: String,
    sequence: i64,
}

impl CheckpointRef {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }
}

/// The host resolves the immutable configuration snapshot identified by this
/// digest before entering the driver. Current authorization is checked separately.
#[derive(Debug, Clone)]
pub struct AdvanceRequest {
    pub run: RunTurnRequest,
    pub checkpoint: Option<CheckpointRef>,
    pub max_steps: NonZeroU32,
    configuration_digest: String,
}

impl AdvanceRequest {
    pub fn configuration_digest(&self) -> &str {
        &self.configuration_digest
    }

    pub fn new(
        run: RunTurnRequest,
        configuration_digest: String,
        max_steps: NonZeroU32,
    ) -> Result<Self, AdvanceError> {
        if configuration_digest.len() != 64
            || !configuration_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AdvanceError::InvalidCheckpoint(
                "configuration identity must be a lowercase SHA-256 digest".to_owned(),
            ));
        }
        Ok(Self {
            run,
            checkpoint: None,
            max_steps,
            configuration_digest,
        })
    }

    #[must_use]
    pub fn resume(mut self, checkpoint: CheckpointRef) -> Self {
        self.checkpoint = Some(checkpoint);
        self
    }
}

/// A scheduling boundary is distinct from an agent turn's terminal result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdvanceOutcome {
    Progressed {
        checkpoint: CheckpointRef,
    },
    Waiting {
        checkpoint: CheckpointRef,
        waits: Vec<zuno_types::wait::WaitRef>,
    },
    Completed {
        assistant_message_id: String,
        steps: u32,
        unresolved_tool_failures: Vec<ToolFailureRecovery>,
    },
    Paused {
        assistant_message_id: Option<String>,
        steps: u32,
    },
    Suspended {
        assistant_message_id: String,
        steps: u32,
        request_id: String,
    },
}

impl From<TurnOutcome> for AdvanceOutcome {
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed {
                assistant_message_id,
                steps,
                unresolved_tool_failures,
            } => Self::Completed {
                assistant_message_id,
                steps,
                unresolved_tool_failures,
            },
            TurnOutcome::Interrupted {
                assistant_message_id,
                steps,
            } => Self::Paused {
                assistant_message_id,
                steps,
            },
            TurnOutcome::WaitingForHuman {
                assistant_message_id,
                steps,
                request_id,
            } => Self::Suspended {
                assistant_message_id,
                steps,
                request_id,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdvanceError {
    #[error("driver {0} does not implement bounded checkpoints")]
    UnsupportedDriver(String),
    #[error("invalid driver checkpoint: {0}")]
    InvalidCheckpoint(String),
    #[error("the checkpoint or execution identity changed")]
    Conflict,
    #[error("an advance has no completed checkpoint; inspect its durable results before resuming")]
    NeedsInspection,
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Turn(#[from] TurnError),
}

/// Reconstructable loop state. Caches of provider objects and open files are
/// deliberately rebuilt; counters and recovery obligations are never reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoopCheckpoint {
    pub tool_step: Option<ToolStepCheckpoint>,
    pub steps: u32,
    pub tool_calls_dispatched: u32,
    pub last_assistant_id: Option<String>,
    pub(crate) requested_turn: Option<RequestedTurn>,
    pub prompt_traces: PromptTraceSet,
    pub unresolved_tool_failures: BTreeMap<String, ToolFailureRecovery>,
    pub consecutive_invalid_tool_calls: u8,
    pub stagnant_work_state_read: Option<(String, String, u8)>,
    pub dynamic_context: DynamicContext,
    pub dynamic_context_refresh: Option<ToolDynamicContextRefresh>,
    pub step_limit_finalization_attempted: bool,
    pub turn_usage: ProviderRequestUsage,
    pub last_request: ProviderRequestUsage,
    pub last_context_tokens: Option<u64>,
    pub reported_historical_tool_repair: bool,
    pub elapsed_millis: u64,
    pub started_at_ms: i64,
}

impl LoopCheckpoint {
    pub fn waits(&self) -> Vec<zuno_types::wait::WaitRef> {
        self.tool_step
            .as_ref()
            .map(|step| {
                step.pending
                    .iter()
                    .map(|pending| pending.reference.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub(crate) enum LoopOutcome {
    Completed(TurnOutcome),
    Progressed(Box<LoopCheckpoint>),
    Waiting(Box<LoopCheckpoint>),
}

impl From<TurnOutcome> for LoopOutcome {
    fn from(value: TurnOutcome) -> Self {
        Self::Completed(value)
    }
}

/// The durable admission token of one local advance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdvanceAdmission {
    pub checkpoint: Option<LoopCheckpoint>,
    event_id: String,
    pub(crate) owner: PrincipalKey,
    request_digest: String,
    previous: Option<CheckpointRef>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BeginAdvance {
    Admitted(Box<AdvanceAdmission>),
    AlreadyCommitted(AdvanceOutcome),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum AdvanceState {
    Started,
    Checkpointed {
        checkpoint: Box<LoopCheckpoint>,
    },
    Waiting {
        checkpoint: Box<LoopCheckpoint>,
    },
    Completed {
        outcome: TurnOutcome,
    },
    Failed {
        code: String,
        recovery: TurnRecovery,
        budget_stop: Option<BudgetStop>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdvanceRecord {
    schema_version: u32,
    turn_id: String,
    owner: PrincipalKey,
    request_digest: String,
    previous: Option<CheckpointRef>,
    state: AdvanceState,
}

fn digest(request: &AdvanceRequest) -> String {
    zuno_orchestration::sha256_json(&json!({
        "configuration": request.configuration_digest,
        "run": request.run,
    }))
}

fn decode(event: &SessionEvent) -> Result<AdvanceRecord, AdvanceError> {
    let record: AdvanceRecord = serde_json::from_value(Value::Object(event.properties.clone()))
        .map_err(|error| AdvanceError::InvalidCheckpoint(error.to_string()))?;
    if event.version != 1 || !supports_checkpoint_schema(record.schema_version) {
        return Err(AdvanceError::InvalidCheckpoint(
            "unsupported checkpoint schema".to_owned(),
        ));
    }
    // Older checkpoints can describe only waits before external handoff. Never
    // reinterpret an injected new execution proof under an old schema version.
    if record.schema_version == 3 {
        let checkpoint = match &record.state {
            AdvanceState::Checkpointed { checkpoint } | AdvanceState::Waiting { checkpoint } => {
                Some(checkpoint)
            }
            _ => None,
        };
        if checkpoint
            .and_then(|checkpoint| checkpoint.tool_step.as_ref())
            .is_some_and(|phase| {
                phase.pending.iter().any(|pending| {
                    pending.dispatch != crate::r#loop::tool_step::PendingDispatch::NotStarted
                })
            })
        {
            return Err(AdvanceError::InvalidCheckpoint(
                "legacy checkpoint cannot prove external handoff".to_owned(),
            ));
        }
    }
    Ok(record)
}

/// A valid tool-phase checkpoint protects only its exact unfinished rows.
pub fn protects_unfinished(
    event: &SessionEvent,
    parts: &[zuno_db::message::PartRecord],
) -> Result<bool, AdvanceError> {
    let record = decode(event)?;
    let checkpoint = match record.state {
        AdvanceState::Checkpointed { checkpoint } | AdvanceState::Waiting { checkpoint } => {
            checkpoint
        }
        _ => return Ok(false),
    };
    let Some(phase) = &checkpoint.tool_step else {
        return Ok(false);
    };
    Ok(phase.covers_unfinished(&event.session_id, &record.turn_id, parts, true)?)
}

pub fn pending_waits(event: &SessionEvent) -> Result<Vec<zuno_types::wait::WaitRef>, AdvanceError> {
    let record = decode(event)?;
    Ok(match record.state {
        AdvanceState::Waiting { checkpoint } => checkpoint.waits(),
        _ => Vec::new(),
    })
}

/// Reclaim only an unchanged, explicit boundary. A newer started advance or an
/// unexplained unfinished invocation keeps the expired execution uncertain.
pub fn reclaimable_checkpoint(
    event: &SessionEvent,
    owner: &PrincipalKey,
    reference: &Value,
    unfinished: &[zuno_db::message::PartRecord],
) -> Result<bool, AdvanceError> {
    let record = decode(event)?;
    if &record.owner != owner
        || event.event_type != EVENT_TYPE
        || json!(checkpoint_ref(event, &record.turn_id)) != *reference
    {
        return Ok(false);
    }
    match record.state {
        AdvanceState::Checkpointed { checkpoint } | AdvanceState::Waiting { checkpoint } => {
            match checkpoint.tool_step {
                Some(phase) => Ok(phase.covers_unfinished(
                    &event.session_id,
                    &record.turn_id,
                    unfinished,
                    true,
                )?),
                None => Ok(unfinished.is_empty()),
            }
        }
        _ => Ok(false),
    }
}

/// Recover the authoritative checkpoint referenced by a request. Caller-supplied
/// admission/checkpoint bodies never substitute for this durable event.
pub fn restore_checkpoint(
    event: &SessionEvent,
    request: &AdvanceRequest,
    owner: &PrincipalKey,
) -> Result<LoopCheckpoint, AdvanceError> {
    let record = decode(event)?;
    if event.session_id != request.run.session_id
        || event.event_type != EVENT_TYPE
        || record.turn_id != request.run.turn_id
        || &record.owner != owner
        || record.request_digest != digest(request)
        || request.checkpoint.as_ref() != Some(&checkpoint_ref(event, &record.turn_id))
    {
        return Err(AdvanceError::Conflict);
    }
    match record.state {
        AdvanceState::Checkpointed { checkpoint } | AdvanceState::Waiting { checkpoint } => {
            Ok(*checkpoint)
        }
        _ => Err(AdvanceError::Conflict),
    }
}

fn encode(record: &AdvanceRecord) -> Result<NewSessionEvent, AdvanceError> {
    let encoded = serde_json::to_vec(&record)
        .map_err(|error| AdvanceError::InvalidCheckpoint(error.to_string()))?;
    if encoded.len() > MAX_CHECKPOINT_BYTES {
        return Err(AdvanceError::InvalidCheckpoint(
            "checkpoint exceeds the 8 MiB limit".to_owned(),
        ));
    }
    let Value::Object(properties) = serde_json::from_slice(&encoded)
        .map_err(|error| AdvanceError::InvalidCheckpoint(error.to_string()))?
    else {
        return Err(AdvanceError::InvalidCheckpoint(
            "checkpoint must be an object".to_owned(),
        ));
    };
    Ok(NewSessionEvent::new(EVENT_TYPE, properties)?)
}

/// Compare and admit before any provider/tool effect. A lost in-flight marker is
/// never silently replaced by another executor.
/// The storage provider makes this decision while holding its session write lock.
pub enum PreparedBegin {
    Admit(Box<PreparedAdmission>),
    Consume(Box<PreparedConsumption>),
    AlreadyCommitted(AdvanceOutcome),
}

/// A dependency-only transition has no provider/tool effects and needs no
/// separately committed "started" marker. Its entire consumption is one CAS.
pub struct PreparedConsumption {
    pub checkpoint: LoopCheckpoint,
    turn_id: String,
    owner: PrincipalKey,
    request_digest: String,
    previous: Option<CheckpointRef>,
}

impl PreparedConsumption {
    pub fn event(self) -> Result<NewSessionEvent, AdvanceError> {
        if !self.checkpoint.waits().is_empty() {
            return Err(AdvanceError::Conflict);
        }
        encode(&AdvanceRecord {
            schema_version: SCHEMA_VERSION,
            turn_id: self.turn_id,
            owner: self.owner,
            request_digest: self.request_digest,
            previous: self.previous,
            state: AdvanceState::Checkpointed {
                checkpoint: Box::new(self.checkpoint),
            },
        })
    }
}

pub struct PreparedAdmission {
    session_id: String,
    checkpoint: Option<LoopCheckpoint>,
    record: AdvanceRecord,
}

impl PreparedAdmission {
    pub fn event(&self) -> Result<NewSessionEvent, AdvanceError> {
        encode(&self.record)
    }

    pub fn committed(self, event: &SessionEvent) -> Result<AdvanceAdmission, AdvanceError> {
        let record = decode(event)?;
        if event.session_id != self.session_id
            || event.event_type != EVENT_TYPE
            || event.id.is_empty()
            || record.turn_id != self.record.turn_id
            || record.owner != self.record.owner
            || record.request_digest != self.record.request_digest
            || record.previous != self.record.previous
            || !matches!(record.state, AdvanceState::Started)
        {
            return Err(AdvanceError::Conflict);
        }
        Ok(AdvanceAdmission {
            checkpoint: self.checkpoint,
            event_id: event.id.clone(),
            owner: record.owner,
            request_digest: record.request_digest,
            previous: record.previous,
        })
    }
}

pub fn prepare_begin(
    request: &AdvanceRequest,
    owner: PrincipalKey,
    latest: Option<SessionEvent>,
    unfinished: bool,
    uncertain: bool,
    waits_ready: bool,
) -> Result<PreparedBegin, AdvanceError> {
    if unfinished || uncertain {
        return Err(AdvanceError::NeedsInspection);
    }
    let request_digest = digest(request);
    let checkpoint = match (latest, &request.checkpoint) {
        (None, None) => None,
        (Some(event), previous) => {
            let record = decode(&event)?;
            // A repeated request whose response was lost reads its original
            // committed result. It does not spend another provider attempt.
            if record.turn_id == request.run.turn_id
                && record.owner == owner
                && record.request_digest == request_digest
                && record.previous.as_ref() == previous.as_ref()
            {
                match &record.state {
                    AdvanceState::Checkpointed { .. } => {
                        return Ok(PreparedBegin::AlreadyCommitted(
                            AdvanceOutcome::Progressed {
                                checkpoint: checkpoint_ref(&event, &record.turn_id),
                            },
                        ));
                    }
                    AdvanceState::Waiting { checkpoint } => {
                        return Ok(PreparedBegin::AlreadyCommitted(AdvanceOutcome::Waiting {
                            checkpoint: checkpoint_ref(&event, &record.turn_id),
                            waits: checkpoint.waits(),
                        }));
                    }
                    AdvanceState::Completed { outcome } => {
                        return Ok(PreparedBegin::AlreadyCommitted(outcome.clone().into()));
                    }
                    AdvanceState::Started | AdvanceState::Failed { .. } => {}
                }
            }
            match (record.state, previous) {
                (AdvanceState::Completed { .. }, None) if record.turn_id != request.run.turn_id => {
                    None
                }
                (
                    AdvanceState::Checkpointed { checkpoint }
                    | AdvanceState::Waiting { checkpoint },
                    Some(previous),
                ) if previous.session_id == request.run.session_id
                    && previous.turn_id == request.run.turn_id
                    && previous.sequence == event.sequence
                    && previous.event_id == event.id
                    && record.turn_id == request.run.turn_id
                    && record.owner == owner
                    && record.request_digest == request_digest =>
                {
                    if checkpoint.steps == 0
                        || checkpoint.steps == u32::MAX
                        || checkpoint.last_assistant_id.is_none()
                        || checkpoint.requested_turn.is_none()
                    {
                        return Err(AdvanceError::InvalidCheckpoint(
                            "a ready checkpoint requires a completed assistant step".to_owned(),
                        ));
                    }
                    if !checkpoint.waits().is_empty() && !waits_ready {
                        return Ok(PreparedBegin::AlreadyCommitted(AdvanceOutcome::Waiting {
                            checkpoint: previous.clone(),
                            waits: checkpoint.waits(),
                        }));
                    }
                    Some(*checkpoint)
                }
                (AdvanceState::Started | AdvanceState::Failed { .. }, _) => {
                    return Err(AdvanceError::NeedsInspection);
                }
                _ => return Err(AdvanceError::Conflict),
            }
        }
        (None, Some(_)) => return Err(AdvanceError::Conflict),
    };
    if let Some(checkpoint) = &checkpoint
        && !checkpoint.waits().is_empty()
    {
        return Ok(PreparedBegin::Consume(Box::new(PreparedConsumption {
            checkpoint: checkpoint.clone(),
            turn_id: request.run.turn_id.clone(),
            owner,
            request_digest,
            previous: request.checkpoint.clone(),
        })));
    }
    Ok(PreparedBegin::Admit(Box::new(PreparedAdmission {
        session_id: request.run.session_id.clone(),
        checkpoint,
        record: AdvanceRecord {
            schema_version: SCHEMA_VERSION,
            turn_id: request.run.turn_id.clone(),
            owner,
            request_digest,
            previous: request.checkpoint.clone(),
            state: AdvanceState::Started,
        },
    })))
}

pub(crate) fn begin(
    connection: &mut Connection,
    request: &AdvanceRequest,
    owner: PrincipalKey,
) -> Result<BeginAdvance, AdvanceError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(zuno_db::open::map_error)?;
    session::get_owned(&transaction, &request.run.session_id, &owner)?;
    let messages = zuno_db::message::MessageStore::new(&transaction);
    let unfinished_parts = messages.unfinished_tool_parts_for_session(&request.run.session_id)?;
    let uncertain = !messages
        .pending_uncertain_tool_calls(&request.run.session_id, i64::MIN)?
        .is_empty();
    let latest = latest_of_type_in(&transaction, &request.run.session_id, EVENT_TYPE)?;
    let unfinished = !unfinished_parts.is_empty()
        && !latest
            .as_ref()
            .map(|event| protects_unfinished(event, &unfinished_parts))
            .transpose()?
            .unwrap_or(false);
    let waits = latest
        .as_ref()
        .map(pending_waits)
        .transpose()?
        .unwrap_or_default();
    let scope = crate::state::TurnStateScope {
        owner: owner.clone(),
        session_id: request.run.session_id.clone(),
    };
    let completions = crate::wait::sqlite_completions(&transaction, &scope, &waits)?;
    let waits_ready = completions.is_some();
    let outcome = match prepare_begin(request, owner, latest, unfinished, uncertain, waits_ready)? {
        PreparedBegin::AlreadyCommitted(outcome) => BeginAdvance::AlreadyCommitted(outcome),
        PreparedBegin::Consume(mut prepared) => {
            let completions = completions.ok_or(AdvanceError::Conflict)?;
            let parts = crate::r#loop::tool_step::consume(
                &request.run,
                &mut prepared.checkpoint,
                &completions,
                &unfinished_parts,
            )?;
            for part in parts {
                messages.put_part(&part)?;
            }
            for completion in &completions {
                zuno_db::event_log::append_identified_in(
                    &transaction,
                    &scope.session_id,
                    &crate::wait::consumed_event_id(&scope, &completion.reference),
                    crate::wait::consumed_event(completion)?,
                )?;
            }
            let event = append_in(&transaction, &scope.session_id, prepared.event()?)?;
            BeginAdvance::AlreadyCommitted(AdvanceOutcome::Progressed {
                checkpoint: checkpoint_ref(&event, &request.run.turn_id),
            })
        }
        PreparedBegin::Admit(prepared) => {
            let event = append_in(&transaction, &request.run.session_id, prepared.event()?)?;
            let admission = prepared.committed(&event)?;
            BeginAdvance::Admitted(Box::new(admission))
        }
    };
    transaction.commit().map_err(zuno_db::open::map_error)?;
    Ok(outcome)
}

pub(crate) fn completion_state(outcome: &Result<LoopOutcome, TurnError>) -> AdvanceState {
    match outcome {
        Ok(LoopOutcome::Progressed(checkpoint)) => AdvanceState::Checkpointed {
            checkpoint: checkpoint.clone(),
        },
        Ok(LoopOutcome::Waiting(checkpoint)) => AdvanceState::Waiting {
            checkpoint: checkpoint.clone(),
        },
        Ok(LoopOutcome::Completed(outcome)) => AdvanceState::Completed {
            outcome: outcome.clone(),
        },
        Err(error) => AdvanceState::Failed {
            code: error.kind().to_owned(),
            recovery: error.recovery(),
            budget_stop: match error {
                TurnError::BudgetLimited { kind, detail } => Some(BudgetStop {
                    kind: *kind,
                    detail: detail.clone(),
                }),
                _ => None,
            },
        },
    }
}

pub fn prepare_commit(
    request: &AdvanceRequest,
    admission: &AdvanceAdmission,
    latest: &SessionEvent,
    state: AdvanceState,
) -> Result<NewSessionEvent, AdvanceError> {
    let record = decode(latest)?;
    if latest.id != admission.event_id
        || latest.session_id != request.run.session_id
        || latest.event_type != EVENT_TYPE
        || record.turn_id != request.run.turn_id
        || record.owner != admission.owner
        || record.request_digest != admission.request_digest
        || record.request_digest != digest(request)
        || record.previous != admission.previous
        || !matches!(record.state, AdvanceState::Started)
        || matches!(state, AdvanceState::Started)
    {
        return Err(AdvanceError::Conflict);
    }
    encode(&AdvanceRecord {
        schema_version: SCHEMA_VERSION,
        turn_id: request.run.turn_id.clone(),
        owner: admission.owner.clone(),
        request_digest: admission.request_digest.clone(),
        previous: admission.previous.clone(),
        state,
    })
}

pub(crate) fn commit(
    connection: &mut Connection,
    request: &AdvanceRequest,
    admission: &AdvanceAdmission,
    state: AdvanceState,
) -> Result<CheckpointRef, AdvanceError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(zuno_db::open::map_error)?;
    session::get_owned(&transaction, &request.run.session_id, &admission.owner)?;
    let latest = latest_of_type_in(&transaction, &request.run.session_id, EVENT_TYPE)?
        .ok_or(AdvanceError::Conflict)?;
    if let AdvanceState::Waiting { checkpoint } = &state {
        let phase = checkpoint
            .tool_step
            .as_ref()
            .ok_or(AdvanceError::Conflict)?;
        let messages = zuno_db::message::MessageStore::new(&transaction);
        let parts = messages.unfinished_tool_parts_for_session(&request.run.session_id)?;
        if phase.pending.is_empty()
            || !phase.covers_unfinished(
                &request.run.session_id,
                &request.run.turn_id,
                &parts,
                false,
            )?
        {
            return Err(AdvanceError::NeedsInspection);
        }
        for part in phase.waiting_parts(&parts)? {
            messages.put_part(&part)?;
        }
    }
    let event = append_in(
        &transaction,
        &request.run.session_id,
        prepare_commit(request, admission, &latest, state)?,
    )?;
    transaction.commit().map_err(zuno_db::open::map_error)?;
    Ok(checkpoint_ref(&event, &request.run.turn_id))
}

pub fn checkpoint_reference(
    event: &SessionEvent,
    turn_id: &str,
) -> Result<CheckpointRef, AdvanceError> {
    let record = decode(event)?;
    if record.turn_id != turn_id || event.event_type != EVENT_TYPE || event.id.is_empty() {
        return Err(AdvanceError::Conflict);
    }
    Ok(checkpoint_ref(event, turn_id))
}

fn checkpoint_ref(event: &SessionEvent, turn_id: &str) -> CheckpointRef {
    CheckpointRef {
        session_id: event.session_id.clone(),
        turn_id: turn_id.to_owned(),
        event_id: event.id.clone(),
        sequence: event.sequence,
    }
}
