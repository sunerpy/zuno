//! Admission, model application and completion are separate durable facts.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use zuno_error::DbError;
use zuno_types::admission::{
    InputAdmissionReceipt, InputExecutionGate, InputReceiptDelivery, InputReceiptState,
    InputStopReason,
};

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::inbox::{NewSessionInput, SessionInput, SubmissionState};
use crate::{Pool, inbox, map_error};

pub const CLIENT_MESSAGE_PREFIX: &str = "client-message:";

#[derive(Debug)]
pub struct ReceiptedInput {
    pub input: SessionInput,
    pub receipt: InputAdmissionReceipt,
    pub duplicate: bool,
}

#[derive(Clone)]
pub struct InputReceiptStore {
    pool: Arc<Pool>,
}

impl InputReceiptStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<Option<InputAdmissionReceipt>, DbError> {
        let connection = self.pool.get()?;
        get_in(&connection, session_id, input_id)
    }

    pub fn admit(&self, input: NewSessionInput) -> Result<ReceiptedInput, DbError> {
        self.pool.transaction(|tx| {
            let previous = match input.source_key.as_deref() {
                Some(key) => inbox::read_by_source_key_in(tx, &input.session_id, key)?,
                None => inbox::read_in(tx, &input.session_id, &input.id)?,
            };
            let duplicate = previous.is_some();
            let input = match previous {
                Some(previous) if input.source_key.is_none() => {
                    if previous.prompt != input.prompt
                        || previous.trigger_kind != input.trigger_kind
                        || previous.source_key != input.source_key
                    {
                        return Err(conflict(
                            &previous.id,
                            "the input ID already identifies different input",
                        ));
                    }
                    previous
                }
                _ => inbox::admit_in(tx, input)?,
            };
            ensure_in(tx, &input)?;
            let receipt = get_in(tx, &input.session_id, &input.id)?
                .ok_or_else(|| conflict(&input.id, "admission committed without its receipt"))?;
            Ok(ReceiptedInput {
                input,
                receipt,
                duplicate,
            })
        })
    }

    pub fn bind_turn(
        &self,
        session_id: &str,
        input_ids: &[String],
        turn_id: &str,
        at_ms: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|tx| bind_turn_in(tx, session_id, input_ids, turn_id, at_ms))
    }

    /// Start or explicitly recover one logical owner. Transfer and first-input
    /// binding must commit together; a failed bind cannot strand prior steers.
    pub fn begin_turn(
        &self,
        session_id: &str,
        previous: Option<&str>,
        input_ids: &[String],
        turn_id: &str,
        at_ms: i64,
    ) -> Result<(), DbError> {
        validate_turn(turn_id)?;
        self.pool.transaction(|tx| {
            if let Some(previous) = previous {
                validate_turn(previous)?;
                tx.execute(
                    "UPDATE session_input_receipt SET turn_id=?3,time_updated=MAX(time_updated,?4)
                     WHERE turn_id=?2 AND state IN ('admitted','recorded','applied')
                       AND input_id IN (SELECT id FROM session_input WHERE session_id=?1)",
                    params![session_id, previous, turn_id, at_ms],
                )
                .map_err(map_error)?;
            }
            bind_turn_in(tx, session_id, input_ids, turn_id, at_ms)
        })
    }

    pub fn mark_applied(
        &self,
        session_id: &str,
        input_ids: &[String],
        turn_id: &str,
        at_ms: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|tx| mark_applied_in(tx, session_id, input_ids, turn_id, at_ms))
    }

    pub fn finish_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        stop_reason: Option<InputStopReason>,
        error: Option<&str>,
        at_ms: i64,
    ) -> Result<(), DbError> {
        self.pool
            .transaction(|tx| finish_turn_in(tx, session_id, turn_id, stop_reason, error, at_ms))
    }

    /// Settle an input whose owning host stopped before assigning an engine turn.
    /// The caller must own that input's execution, not merely observe its receipt.
    pub fn fail_input(
        &self,
        session_id: &str,
        input_id: &str,
        error: &str,
        cancelled: bool,
        at_ms: i64,
    ) -> Result<(), DbError> {
        self.pool.transaction(|tx| {
            let Some(input) = inbox::read_in(tx, session_id, input_id)? else {
                return Ok(());
            };
            ensure_in(tx, &input)?;
            let state = if cancelled { "cancelled" } else { "failed" };
            let error = &error[..error.floor_char_boundary(4_096)];
            let changed = tx
                .execute(
                    "UPDATE session_input_receipt SET state=?2,error=?3,completed_at=?4,
                 stop_reason=?5,time_updated=MAX(time_updated,?4)
                 WHERE input_id=?1 AND state IN ('admitted','recorded','applied')",
                    params![
                        input_id,
                        state,
                        error,
                        at_ms,
                        cancelled.then_some("cancelled")
                    ],
                )
                .map_err(map_error)?;
            if changed != 0 {
                publish_in(tx, session_id, input_id)?;
            }
            Ok(())
        })
    }

    /// An old drive may finish after native recovery has taken over. Only the
    /// still-current original cycle may fail an unbound, never-applied input.
    /// Eligibility and mutation share one transaction, including gate projection.
    pub fn fail_unapplied_input(
        &self,
        session_id: &str,
        input_id: &str,
        expected_cycle: &str,
        error: &str,
        cancelled: bool,
        at_ms: i64,
    ) -> Result<bool, DbError> {
        self.fail_unapplied_in_scope(
            session_id,
            input_id,
            Some(expected_cycle),
            error,
            cancelled,
            at_ms,
        )
    }

    /// A failure before history persistence may only retire an input that is
    /// still unconsumed and unbound. An absent locally captured cycle is not
    /// evidence that another owner has not already promoted the input.
    pub fn fail_unrecorded_input(
        &self,
        session_id: &str,
        input_id: &str,
        error: &str,
        cancelled: bool,
        at_ms: i64,
    ) -> Result<bool, DbError> {
        self.fail_unapplied_in_scope(session_id, input_id, None, error, cancelled, at_ms)
    }

    fn fail_unapplied_in_scope(
        &self,
        session_id: &str,
        input_id: &str,
        expected_cycle: Option<&str>,
        error: &str,
        cancelled: bool,
        at_ms: i64,
    ) -> Result<bool, DbError> {
        self.pool.transaction(|tx| {
            let Some(receipt) = get_in(tx, session_id, input_id)? else {
                return Ok(false);
            };
            if receipt.state.is_terminal() || receipt.execution_gate.is_some()
                || receipt.turn_id.is_some() || receipt.applied_at.is_some()
            {
                return Ok(false);
            }
            let error = &error[..error.floor_char_boundary(4_096)];
            let changed = tx.execute(
                "UPDATE session_input_receipt
                 SET state=?4,error=?5,completed_at=?6,stop_reason=?7,time_updated=MAX(time_updated,?6)
                 WHERE input_id=?1 AND turn_id IS NULL AND applied_at IS NULL
                   AND state IN ('admitted','recorded')
                   AND EXISTS(SELECT 1 FROM session_input i WHERE i.id=?1 AND i.session_id=?2
                       AND ((?3 IS NULL AND i.cycle_id IS NULL
                             AND i.state IN ('queued','steering','promoted'))
                            OR (?3 IS NOT NULL AND i.cycle_id=?3 AND EXISTS(
                                SELECT 1 FROM session_execution_state s
                                WHERE s.session_id=i.session_id AND s.cycle_id=?3))))",
                params![input_id, session_id, expected_cycle,
                    if cancelled { "cancelled" } else { "failed" },
                    error, at_ms, cancelled.then_some("cancelled")],
            ).map_err(map_error)?;
            if changed != 0 {
                publish_in(tx, session_id, input_id)?;
            }
            Ok(changed != 0)
        })
    }

    /// Transfer pending receipts only for an explicitly recovered logical turn.
    pub fn handoff_turn(
        &self,
        session_id: &str,
        previous_turn_id: &str,
        turn_id: &str,
        at_ms: i64,
    ) -> Result<(), DbError> {
        validate_turn(turn_id)?;
        self.pool.transaction(|tx| {
            tx.execute(
                "UPDATE session_input_receipt SET turn_id=?3,time_updated=MAX(time_updated,?4)
                 WHERE turn_id=?2 AND state IN ('admitted','recorded','applied')
                   AND input_id IN (SELECT id FROM session_input WHERE session_id=?1)",
                params![session_id, previous_turn_id, turn_id, at_ms],
            )
            .map_err(map_error)?;
            Ok(())
        })
    }
}

fn conflict(input_id: &str, detail: &str) -> DbError {
    DbError::Conflict {
        table: "session_input_receipt".to_owned(),
        id: input_id.to_owned(),
        detail: detail.to_owned(),
    }
}

fn validate_turn(turn_id: &str) -> Result<(), DbError> {
    if turn_id.trim().is_empty() || turn_id.len() > 256 {
        return Err(query_error(std::io::Error::other(
            "turn ID must contain 1–256 bytes",
        )));
    }
    Ok(())
}

pub fn client_message_id(source_key: Option<&str>) -> Option<&str> {
    source_key.and_then(|key| key.strip_prefix(CLIENT_MESSAGE_PREFIX))
}

pub(crate) fn validate_client_key(input: &NewSessionInput) -> Result<(), DbError> {
    if let Some(id) = client_message_id(input.source_key.as_deref())
        && (id.trim().is_empty() || id.len() > 256)
    {
        return Err(query_error(std::io::Error::other(
            "client message ID must contain 1–256 bytes",
        )));
    }
    Ok(())
}

pub(crate) fn validate_client_replay(
    existing: &SessionInput,
    incoming: &NewSessionInput,
) -> Result<(), DbError> {
    if client_message_id(incoming.source_key.as_deref()).is_some()
        && (existing.prompt != incoming.prompt || existing.trigger_kind != incoming.trigger_kind)
    {
        return Err(conflict(
            &existing.id,
            "the client message ID already identifies different input",
        ));
    }
    Ok(())
}

/// Backfill only known history facts, never invent provider application.
pub fn ensure_in(tx: &Transaction<'_>, input: &SessionInput) -> Result<(), DbError> {
    let state = match input.state {
        SubmissionState::Consumed => InputReceiptState::Recorded,
        SubmissionState::Cancelled => InputReceiptState::Cancelled,
        SubmissionState::Failed => InputReceiptState::Failed,
        _ => InputReceiptState::Admitted,
    };
    tx.execute(
        "INSERT OR IGNORE INTO session_input_receipt
           (input_id,state,delivery,completed_at,error,time_updated)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            input.id,
            state.as_str(),
            input.delivery.as_str(),
            state.is_terminal().then_some(input.time_updated),
            input.error,
            input.time_updated
        ],
    )
    .map_err(map_error)?;
    Ok(())
}

struct RawReceipt {
    session_id: String,
    input_id: String,
    source_key: Option<String>,
    admitted_sequence: i64,
    delivery: String,
    state: String,
    turn_id: Option<String>,
    applied_at: Option<i64>,
    completed_at: Option<i64>,
    stop_reason: Option<String>,
    error: Option<String>,
    execution_gate: Option<String>,
    time_updated: i64,
}

pub fn get_in(
    connection: &Connection,
    session_id: &str,
    input_id: &str,
) -> Result<Option<InputAdmissionReceipt>, DbError> {
    let raw = connection
        .query_row(
            "SELECT i.session_id,i.id,i.source_key,i.admitted_seq,r.delivery,r.state,
                r.turn_id,r.applied_at,r.completed_at,r.stop_reason,r.error,r.time_updated,
                CASE WHEN r.state='recorded' AND r.turn_id IS NULL AND r.applied_at IS NULL
                THEN (SELECT json_extract(e.data,'$.gate') FROM event e
                      WHERE e.aggregate_id=i.session_id AND e.seq>=i.admitted_seq
                        AND e.type='session.input.execution_gate.1'
                        AND json_extract(e.data,'$.inputId')=i.id
                      ORDER BY e.seq DESC LIMIT 1) END
         FROM session_input i JOIN session_input_receipt r ON r.input_id=i.id
         WHERE i.session_id=?1 AND i.id=?2",
            params![session_id, input_id],
            |row| {
                Ok(RawReceipt {
                    session_id: row.get(0)?,
                    input_id: row.get(1)?,
                    source_key: row.get(2)?,
                    admitted_sequence: row.get(3)?,
                    delivery: row.get(4)?,
                    state: row.get(5)?,
                    turn_id: row.get(6)?,
                    applied_at: row.get(7)?,
                    completed_at: row.get(8)?,
                    stop_reason: row.get(9)?,
                    error: row.get(10)?,
                    time_updated: row.get(11)?,
                    execution_gate: row.get(12)?,
                })
            },
        )
        .optional()
        .map_err(map_error)?;
    raw.map(|raw| {
        let state = InputReceiptState::parse(&raw.state)
            .ok_or_else(|| conflict(input_id, "unknown receipt state"))?;
        let delivery = InputReceiptDelivery::parse(&raw.delivery)
            .ok_or_else(|| conflict(input_id, "unknown receipt delivery"))?;
        let stop_reason = raw
            .stop_reason
            .as_deref()
            .map(|value| {
                InputStopReason::parse(value)
                    .ok_or_else(|| conflict(input_id, "unknown receipt stop reason"))
            })
            .transpose()?;
        Ok(InputAdmissionReceipt {
            session_id: raw.session_id,
            input_id: raw.input_id,
            client_message_id: client_message_id(raw.source_key.as_deref()).map(str::to_owned),
            admitted_sequence: raw.admitted_sequence,
            delivery,
            state,
            turn_id: raw.turn_id,
            applied_at: raw.applied_at,
            completed_at: raw.completed_at,
            stop_reason,
            error: raw.error,
            execution_gate: raw
                .execution_gate
                .map(|value| serde_json::from_str(&value).map_err(query_error))
                .transpose()?,
            time_updated: raw.time_updated,
        })
    })
    .transpose()
}

/// Record a native gate without inventing failure, cancellation or model usage.
/// Caller holds the execution-state transaction and owns this consumed input.
pub fn record_execution_gate_in(
    tx: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    gate: &InputExecutionGate,
    at_ms: i64,
) -> Result<bool, DbError> {
    let Some(receipt) = get_in(tx, session_id, input_id)? else {
        return Ok(false);
    };
    if receipt.state != InputReceiptState::Recorded
        || receipt.turn_id.is_some()
        || receipt.applied_at.is_some()
    {
        return Ok(false);
    }
    if gate.execution_revision < 1 || gate.cycle_id.trim().is_empty() {
        return Err(conflict(input_id, "invalid native execution gate"));
    }
    if receipt.execution_gate.as_ref() == Some(gate) {
        return Ok(true);
    }
    let changed = tx
        .execute(
            "UPDATE session_input_receipt SET time_updated=MAX(time_updated,?3)
         WHERE input_id=?1 AND state='recorded' AND turn_id IS NULL AND applied_at IS NULL
         AND EXISTS(SELECT 1 FROM session_input i WHERE i.id=?1 AND i.session_id=?2
                    AND i.state='consumed' AND i.cycle_id=?4)",
            params![input_id, session_id, at_ms, gate.cycle_id],
        )
        .map_err(map_error)?;
    if changed == 0 {
        return Ok(false);
    }
    append_in(
        tx,
        session_id,
        NewSessionEvent::new(
            "session.input.execution_gate",
            serde_json::json!({"inputId":input_id,"gate":gate,"time":at_ms})
                .as_object()
                .expect("object")
                .clone(),
        )?,
    )?;
    publish_in(tx, session_id, input_id)?;
    Ok(true)
}

/// Carry only a positively identified, never-applied input into an explicitly
/// authorized recovery cycle. Keep its gate receipt until a real turn binds it:
/// a duplicate observer in the handoff gap must not infer execution or failure.
pub fn recover_gated_input_in(
    tx: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    cycle_id: &str,
    at_ms: i64,
) -> Result<bool, DbError> {
    let Some(receipt) = get_in(tx, session_id, input_id)? else {
        return Ok(false);
    };
    let Some(gate) = receipt.execution_gate else {
        return Ok(false);
    };
    let changed = tx.execute(
        "UPDATE session_input SET cycle_id=?3,revision=revision+1,time_updated=MAX(time_updated,?4)
         WHERE id=?1 AND session_id=?2 AND state='consumed' AND cycle_id=?5
           AND EXISTS(SELECT 1 FROM session_input_receipt r WHERE r.input_id=?1
                      AND r.state='recorded' AND r.turn_id IS NULL AND r.applied_at IS NULL)",
        params![input_id, session_id, cycle_id, at_ms, gate.cycle_id],
    ).map_err(map_error)?;
    if changed != 0 {
        append_in(
            tx,
            session_id,
            NewSessionEvent::new(
                "session.input.execution_recovered",
                serde_json::json!({"inputId":input_id,"originCycleId":gate.cycle_id,
                "cycleId":cycle_id,"time":at_ms})
                .as_object()
                .expect("object")
                .clone(),
            )?,
        )?;
    }
    Ok(changed != 0)
}

fn publish_in(tx: &Transaction<'_>, session_id: &str, input_id: &str) -> Result<(), DbError> {
    let Some(receipt) = get_in(tx, session_id, input_id)? else {
        return Err(conflict(input_id, "receipt disappeared while publishing"));
    };
    let value = serde_json::to_value(&receipt).map_err(query_error)?;
    append_in(
        tx,
        session_id,
        NewSessionEvent::new(
            "session.input.receipt",
            value.as_object().expect("receipt object").clone(),
        )?,
    )?;
    Ok(())
}

pub fn bind_turn_in(
    tx: &Transaction<'_>,
    session_id: &str,
    input_ids: &[String],
    turn_id: &str,
    at_ms: i64,
) -> Result<(), DbError> {
    validate_turn(turn_id)?;
    for id in input_ids {
        let Some(input) = inbox::read_in(tx, session_id, id)? else {
            continue;
        };
        ensure_in(tx, &input)?;
        let receipt = get_in(tx, session_id, id)?.expect("ensured receipt");
        if receipt.state.is_terminal() || receipt.turn_id.as_deref() == Some(turn_id) {
            continue;
        }
        if receipt.turn_id.is_some() {
            return Err(conflict(id, "another turn already owns the input receipt"));
        }
        tx.execute(
            "UPDATE session_input_receipt SET turn_id=?2,time_updated=MAX(time_updated,?3)
             WHERE input_id=?1 AND turn_id IS NULL
               AND state IN ('admitted','recorded','applied')",
            params![id, turn_id, at_ms],
        )
        .map_err(map_error)?;
        publish_in(tx, session_id, id)?;
    }
    Ok(())
}

/// Called only for input identities present in the committed provider request.
pub fn mark_applied_in(
    tx: &Transaction<'_>,
    session_id: &str,
    input_ids: &[String],
    turn_id: &str,
    at_ms: i64,
) -> Result<(), DbError> {
    bind_turn_in(tx, session_id, input_ids, turn_id, at_ms)?;
    for id in input_ids {
        let changed = tx
            .execute(
                "UPDATE session_input_receipt
             SET state='applied',applied_at=COALESCE(applied_at,?3),
                 time_updated=MAX(time_updated,?3)
             WHERE input_id=?1 AND turn_id=?2 AND state IN ('admitted','recorded')
               AND EXISTS(SELECT 1 FROM session_input i
                          WHERE i.id=?1 AND i.session_id=?4 AND i.state='consumed')",
                params![id, turn_id, at_ms, session_id],
            )
            .map_err(map_error)?;
        if changed != 0 {
            publish_in(tx, session_id, id)?;
        }
    }
    Ok(())
}

pub fn finish_turn_in(
    tx: &Transaction<'_>,
    session_id: &str,
    turn_id: &str,
    stop_reason: Option<InputStopReason>,
    error: Option<&str>,
    at_ms: i64,
) -> Result<(), DbError> {
    validate_turn(turn_id)?;
    if stop_reason.is_none() && error.is_none() {
        return Err(conflict(turn_id, "a terminal turn must have an outcome"));
    }
    let state = if error.is_some() {
        InputReceiptState::Failed
    } else if stop_reason == Some(InputStopReason::Cancelled) {
        InputReceiptState::Cancelled
    } else {
        InputReceiptState::Completed
    };
    let mut query = tx
        .prepare(
            "SELECT i.id FROM session_input i
         JOIN session_input_receipt r ON r.input_id=i.id
         WHERE i.session_id=?1 AND r.turn_id=?2
           AND r.state IN ('recorded','applied')
           AND (?3 <> 'completed' OR r.state='applied'
                OR json_extract(i.prompt,'$.kind')='sessionControl')",
        )
        .map_err(map_error)?;
    let ids = query
        .query_map(params![session_id, turn_id, state.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_error)?;
    drop(query);
    let error = error.map(|value| {
        let boundary = value.floor_char_boundary(4_096);
        value[..boundary].to_owned()
    });
    for id in ids {
        tx.execute(
            "UPDATE session_input_receipt
             SET state=?2,completed_at=?3,stop_reason=?4,error=?5,
                 time_updated=MAX(time_updated,?3)
             WHERE input_id=?1 AND state IN ('recorded','applied')",
            params![
                id,
                state.as_str(),
                at_ms,
                if error.is_some() {
                    None
                } else {
                    stop_reason.map(InputStopReason::as_str)
                },
                error
            ],
        )
        .map_err(map_error)?;
        publish_in(tx, session_id, &id)?;
    }
    // A queued input that missed a safe point still belongs to the inbox, not
    // to the finishing turn. A persisted-but-unapplied report remains recorded.
    tx.execute(
        "UPDATE session_input_receipt SET turn_id=NULL,time_updated=MAX(time_updated,?3)
         WHERE turn_id=?2 AND state IN ('admitted','recorded')
           AND input_id IN (SELECT id FROM session_input WHERE session_id=?1)",
        params![session_id, turn_id, at_ms],
    )
    .map_err(map_error)?;
    Ok(())
}

/// Follow inbox edits/withdrawals without confusing history persistence with application.
pub(crate) fn sync_input_in(tx: &Transaction<'_>, input: &SessionInput) -> Result<(), DbError> {
    ensure_in(tx, input)?;
    let next = match input.state {
        SubmissionState::Consumed => Some(InputReceiptState::Recorded),
        SubmissionState::Cancelled => Some(InputReceiptState::Cancelled),
        SubmissionState::Failed => Some(InputReceiptState::Failed),
        _ => None,
    };
    if let Some(next) = next {
        tx.execute(
            "UPDATE session_input_receipt
             SET state=?2,error=?3,completed_at=?4,time_updated=MAX(time_updated,?5)
             WHERE input_id=?1 AND
               (state='admitted' OR (?2 <> 'recorded' AND state='recorded'))",
            params![
                input.id,
                next.as_str(),
                input.error,
                next.is_terminal().then_some(input.time_updated),
                input.time_updated
            ],
        )
        .map_err(map_error)?;
    }
    Ok(())
}
