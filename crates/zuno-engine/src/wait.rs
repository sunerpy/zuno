//! Durable completion facts consumed by the ordinary bounded driver.
//!
//! Publishing a completion and consuming it are separate transactions. The state
//! provider commits consumption with the original tool result and next checkpoint.
//! An HTTP response or transient notification never acknowledges consumption.

use serde::{Deserialize, Serialize};
use zuno_types::identity::CompletionId;
use zuno_types::wait::{WaitContinuation, WaitRef};

use crate::r#loop::{RunTurnRequest, ToolCall, ToolDispatchResult};
use crate::state::TurnStateError;
use crate::state::TurnStateScope;
use zuno_db::event_log::{NewSessionEvent, SessionEvent};
use zuno_db::{Connection, Transaction, event_log, open, session};

/// Completion sources are installed by the host. This port is not available to
/// model tools or arbitrary API callers; the source verifies its producing
/// operation/child/answer before publishing the immutable result.
#[async_trait::async_trait]
pub trait WaitCompletionStore: Send + Sync {
    async fn publish(
        &self,
        scope: &TurnStateScope,
        completion: &WaitCompletion,
    ) -> Result<SessionEvent, crate::r#loop::TurnError>;
}

#[derive(Clone)]
pub struct SqliteWaitCompletionStore {
    pool: std::sync::Arc<zuno_db::Pool>,
}

impl SqliteWaitCompletionStore {
    pub fn new(pool: std::sync::Arc<zuno_db::Pool>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl WaitCompletionStore for SqliteWaitCompletionStore {
    async fn publish(
        &self,
        scope: &TurnStateScope,
        completion: &WaitCompletion,
    ) -> Result<SessionEvent, crate::r#loop::TurnError> {
        let mut connection = self.pool.get()?;
        publish_sqlite_completion(&mut connection, scope, completion)
    }
}

pub const COMPLETION_EVENT: &str = "runtime.wait.completed";
pub const CONSUMED_EVENT: &str = "runtime.wait.consumed";
pub const MAX_COMPLETION_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitCompletion {
    pub id: CompletionId,
    pub reference: WaitRef,
    pub result: ToolDispatchResult,
}

impl WaitCompletion {
    pub fn validate(&self) -> Result<(), TurnStateError> {
        self.reference
            .validate()
            .map_err(|_| TurnStateError::InvalidData)?;
        if serde_json::to_vec(self)
            .map_err(|_| TurnStateError::InvalidData)?
            .len()
            > MAX_COMPLETION_BYTES
            || self.result.uncertain.is_some()
            || self.result.output.continuation == zuno_tool::ToolContinuation::WaitingForHuman
            || (self.result.blocked.is_some()
                && (!self.result.is_error
                    || self.result.uncertain.is_some()
                    || self.result.interruption.is_some()))
            || (self.result.recovery.is_some()
                && (self.result.blocked.is_some() || self.result.uncertain.is_some()))
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(())
    }
}

pub fn completion_event_id(scope: &TurnStateScope, reference: &WaitRef) -> String {
    fact_id("completion", scope, reference)
}

pub fn consumed_event_id(scope: &TurnStateScope, reference: &WaitRef) -> String {
    fact_id("consumed", scope, reference)
}

fn fact_id(kind: &str, scope: &TurnStateScope, reference: &WaitRef) -> String {
    format!(
        "evt_wait_{}",
        zuno_orchestration::sha256_json(&serde_json::json!({
            "kind": kind, "owner": scope.owner, "session": scope.session_id,
            "turn": reference.turn_id, "wait": reference.id,
        }))
    )
}

pub fn completion_event(
    completion: &WaitCompletion,
) -> Result<NewSessionEvent, crate::r#loop::TurnError> {
    completion.validate()?;
    let serde_json::Value::Object(properties) =
        serde_json::to_value(completion).map_err(|_| TurnStateError::InvalidData)?
    else {
        return Err(TurnStateError::InvalidData.into());
    };
    Ok(NewSessionEvent::new(COMPLETION_EVENT, properties)?)
}

pub fn consumed_event(
    completion: &WaitCompletion,
) -> Result<NewSessionEvent, crate::r#loop::TurnError> {
    Ok(NewSessionEvent::new(
        CONSUMED_EVENT,
        serde_json::Map::from_iter([
            ("completionID".to_owned(), serde_json::json!(completion.id)),
            (
                "waitRef".to_owned(),
                serde_json::json!(completion.reference),
            ),
        ]),
    )?)
}

pub fn decode_completion(
    event: SessionEvent,
    reference: &WaitRef,
) -> Result<WaitCompletion, crate::r#loop::TurnError> {
    if event.version != 1 || event.event_type != COMPLETION_EVENT {
        return Err(TurnStateError::InvalidData.into());
    }
    let completion: WaitCompletion =
        serde_json::from_value(serde_json::Value::Object(event.properties))
            .map_err(|_| TurnStateError::InvalidData)?;
    completion.validate()?;
    if &completion.reference != reference {
        return Err(TurnStateError::Conflict.into());
    }
    Ok(completion)
}

/// A database clock, not a Worker's wall clock, decides whether a timer is due.
pub fn timer_completion(reference: &WaitRef, now_ms: i64) -> Option<WaitCompletion> {
    if !matches!(reference.target, zuno_types::wait::WaitTarget::Timer { deadline_ms } if deadline_ms <= now_ms)
    {
        return None;
    }
    let id = format!(
        "cmp_{}",
        zuno_orchestration::sha256_json(&serde_json::json!(reference))
    );
    Some(WaitCompletion {
        id: CompletionId::new(id).expect("bounded derived completion identity"),
        reference: reference.clone(),
        result: ToolDispatchResult::success(zuno_tool::ToolOutput::text(
            "Timer completed",
            "The requested deadline has been reached.",
        )),
    })
}

/// Called by a trusted local completion source after it has validated the
/// external result. This is not an authorization endpoint for users or models.
pub fn publish_sqlite_completion(
    connection: &mut Connection,
    scope: &TurnStateScope,
    completion: &WaitCompletion,
) -> Result<SessionEvent, crate::r#loop::TurnError> {
    let transaction = open::immediate_transaction(connection)?;
    session::get_owned(&transaction, &scope.session_id, &scope.owner)?;
    let event = event_log::append_identified_in(
        &transaction,
        &scope.session_id,
        &completion_event_id(scope, &completion.reference),
        completion_event(completion)?,
    )?;
    transaction.commit().map_err(open::map_error)?;
    Ok(event)
}

pub(crate) fn sqlite_completions(
    transaction: &Transaction<'_>,
    scope: &TurnStateScope,
    references: &[WaitRef],
) -> Result<Option<Vec<WaitCompletion>>, crate::r#loop::TurnError> {
    let time: i64 = transaction
        .query_row(
            "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    let mut completions = Vec::with_capacity(references.len());
    for reference in references {
        let id = completion_event_id(scope, reference);
        let mut event = event_log::by_id_in(transaction, &scope.session_id, &id)?;
        if event.is_none()
            && let Some(completion) = timer_completion(reference, time)
        {
            event = Some(event_log::append_identified_in(
                transaction,
                &scope.session_id,
                &id,
                completion_event(&completion)?,
            )?);
        }
        let Some(event) = event else { return Ok(None) };
        if event_log::by_id_in(
            transaction,
            &scope.session_id,
            &consumed_event_id(scope, reference),
        )?
        .is_some()
        {
            return Err(TurnStateError::Conflict.into());
        }
        completions.push(decode_completion(event, reference)?);
    }
    Ok(Some(completions))
}

/// A checkpoint pins the original call, never a newly issued replacement.
pub fn validate_binding(
    reference: &WaitRef,
    request: &RunTurnRequest,
    call: &ToolCall,
) -> Result<(), TurnStateError> {
    reference
        .validate()
        .map_err(|_| TurnStateError::InvalidData)?;
    if reference.turn_id.as_str() != request.turn_id
        || reference.invocation_id.as_str() != call.id
        || reference.arguments_sha256 != zuno_orchestration::sha256_json(&call.input)
        || reference.continuation != WaitContinuation::CurrentTurn
        || call.input_error.is_some()
    {
        return Err(TurnStateError::Conflict);
    }
    Ok(())
}

pub fn consume_results(
    request: &RunTurnRequest,
    checkpoint: &mut crate::advance::LoopCheckpoint,
    completions: &[WaitCompletion],
) -> Result<Vec<zuno_db::message::PartRecord>, crate::r#loop::TurnError> {
    crate::r#loop::tool_step::consume(request, checkpoint, completions)
}

pub(crate) mod uncertain_cause {
    use serde::{Deserialize, Deserializer, Serializer};
    use zuno_error::UncertainCause;

    pub fn serialize<S: Serializer>(
        cause: &UncertainCause,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(cause.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<UncertainCause, D::Error> {
        let text = String::deserialize(deserializer)?;
        UncertainCause::parse(&text)
            .ok_or_else(|| serde::de::Error::custom("unknown uncertain outcome cause"))
    }
}
