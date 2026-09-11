//! Bounded advancement of the ordinary agent loop.
//!
//! A checkpoint is only issued after every tool result in its step is durable.
//! Interrupted in-flight advances require inspection; a checkpoint is never a
//! license to replay a side effect. This local adapter does not grant a lease
//! over an external environment.

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

const EVENT_TYPE: &str = "runtime.driver.advance";
const SCHEMA_VERSION: u32 = 2;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    Progressed {
        checkpoint: CheckpointRef,
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
pub(crate) struct LoopCheckpoint {
    pub steps: u32,
    pub tool_calls_dispatched: u32,
    pub last_assistant_id: Option<String>,
    pub requested_turn: Option<RequestedTurn>,
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

pub(crate) enum LoopOutcome {
    Completed(TurnOutcome),
    Progressed(Box<LoopCheckpoint>),
}

impl From<TurnOutcome> for LoopOutcome {
    fn from(value: TurnOutcome) -> Self {
        Self::Completed(value)
    }
}

/// The durable admission token of one local advance.
pub(crate) struct AdvanceAdmission {
    pub checkpoint: Option<LoopCheckpoint>,
    event_id: String,
    owner: PrincipalKey,
    request_digest: String,
    previous: Option<CheckpointRef>,
}

pub(crate) enum BeginAdvance {
    Admitted(Box<AdvanceAdmission>),
    AlreadyCommitted(AdvanceOutcome),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum AdvanceState {
    Started,
    Checkpointed {
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
    if event.version != 1 || record.schema_version != SCHEMA_VERSION {
        return Err(AdvanceError::InvalidCheckpoint(
            "unsupported checkpoint schema".to_owned(),
        ));
    }
    Ok(record)
}

fn encode(record: AdvanceRecord) -> Result<NewSessionEvent, AdvanceError> {
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
pub(crate) fn begin(
    connection: &mut Connection,
    request: &AdvanceRequest,
    owner: PrincipalKey,
) -> Result<BeginAdvance, AdvanceError> {
    let request_digest = digest(request);
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(zuno_db::open::map_error)?;
    session::get_owned(&transaction, &request.run.session_id, &owner)?;
    if !zuno_db::message::MessageStore::new(&transaction)
        .unfinished_tool_parts_for_session(&request.run.session_id)?
        .is_empty()
    {
        return Err(AdvanceError::NeedsInspection);
    }
    if !zuno_db::message::MessageStore::new(&transaction)
        .pending_uncertain_tool_calls(&request.run.session_id, i64::MIN)?
        .is_empty()
    {
        return Err(AdvanceError::NeedsInspection);
    }
    let latest = latest_of_type_in(&transaction, &request.run.session_id, EVENT_TYPE)?;
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
                        return Ok(BeginAdvance::AlreadyCommitted(AdvanceOutcome::Progressed {
                            checkpoint: checkpoint_ref(&event, &record.turn_id),
                        }));
                    }
                    AdvanceState::Completed { outcome } => {
                        return Ok(BeginAdvance::AlreadyCommitted(outcome.clone().into()));
                    }
                    AdvanceState::Started | AdvanceState::Failed { .. } => {}
                }
            }
            match (record.state, previous) {
                (AdvanceState::Completed { .. }, None) if record.turn_id != request.run.turn_id => {
                    None
                }
                (AdvanceState::Checkpointed { checkpoint }, Some(previous))
                    if previous.session_id == request.run.session_id
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
    let event = append_in(
        &transaction,
        &request.run.session_id,
        encode(AdvanceRecord {
            schema_version: SCHEMA_VERSION,
            turn_id: request.run.turn_id.clone(),
            owner: owner.clone(),
            request_digest: request_digest.clone(),
            previous: request.checkpoint.clone(),
            state: AdvanceState::Started,
        })?,
    )?;
    transaction.commit().map_err(zuno_db::open::map_error)?;
    Ok(BeginAdvance::Admitted(Box::new(AdvanceAdmission {
        checkpoint,
        event_id: event.id,
        owner,
        request_digest,
        previous: request.checkpoint.clone(),
    })))
}

pub(crate) fn finish(
    connection: &mut Connection,
    request: &RunTurnRequest,
    admission: &AdvanceAdmission,
    outcome: Result<LoopOutcome, TurnError>,
) -> Result<AdvanceOutcome, AdvanceError> {
    let state = match &outcome {
        Ok(LoopOutcome::Progressed(checkpoint)) => AdvanceState::Checkpointed {
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
    };
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(zuno_db::open::map_error)?;
    session::get_owned(&transaction, &request.session_id, &admission.owner)?;
    let latest = latest_of_type_in(&transaction, &request.session_id, EVENT_TYPE)?
        .ok_or(AdvanceError::Conflict)?;
    if latest.id != admission.event_id {
        return Err(AdvanceError::Conflict);
    }
    let event = append_in(
        &transaction,
        &request.session_id,
        encode(AdvanceRecord {
            schema_version: SCHEMA_VERSION,
            turn_id: request.turn_id.clone(),
            owner: admission.owner.clone(),
            request_digest: admission.request_digest.clone(),
            previous: admission.previous.clone(),
            state,
        })?,
    )?;
    transaction.commit().map_err(zuno_db::open::map_error)?;
    match outcome? {
        LoopOutcome::Completed(outcome) => Ok(outcome.into()),
        LoopOutcome::Progressed(_) => Ok(AdvanceOutcome::Progressed {
            checkpoint: checkpoint_ref(&event, &request.turn_id),
        }),
    }
}

fn checkpoint_ref(event: &SessionEvent, turn_id: &str) -> CheckpointRef {
    CheckpointRef {
        session_id: event.session_id.clone(),
        turn_id: turn_id.to_owned(),
        event_id: event.id.clone(),
        sequence: event.sequence,
    }
}
