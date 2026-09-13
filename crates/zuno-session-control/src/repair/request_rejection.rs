//! A failed authorized repair is not authority to replay its failed input.
//! Prove the prior native authorization and rejected request, then repair only
//! the subsequent recorded user input. This does not broaden the legacy 503
//! validator or treat arbitrary provider 400 responses as repair authorization.

use super::*;

pub(super) fn prove_in(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
) -> Result<Option<SessionRepairEvidence>> {
    prove(connection, state, input, receipt, through_sequence, false)
}

/// Only the gate-chain verifier may use a prior cycle whose scheduling was
/// archived by a fully proven, non-executing user-input transition.
pub(super) fn prove_archived_in(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
) -> Result<Option<SessionRepairEvidence>> {
    prove(connection, state, input, receipt, through_sequence, true)
}

fn prove(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
    archived: bool,
) -> Result<Option<SessionRepairEvidence>> {
    let session = &input.session_id;
    let cycle_id = input
        .cycle_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let started = cycle_started(connection, session, cycle_id)?;
    let faulty_id = started.data["previousCycleId"]
        .as_str()
        .ok_or(SessionRepairRejection::Unproven)?;
    let Some(audit) = authorization(connection, session, faulty_id)? else {
        return Ok(None);
    };
    let applied: AppliedRepair =
        serde_json::from_value(audit.data.clone()).map_err(|_| SessionRepairRejection::Unproven)?;
    let faulty_input_id = &applied.report.input_id;
    let control_id = applied
        .report
        .control_input_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let old_input = bounded_input(connection, session, faulty_input_id)?;
    let control = bounded_input(connection, session, control_id)?;
    let old_receipt = input_receipt::get_in(connection, session, faulty_input_id)?
        .ok_or(SessionRepairRejection::Unproven)?;
    let control_receipt = input_receipt::get_in(connection, session, control_id)?
        .ok_or(SessionRepairRejection::Unproven)?;
    let turn = old_receipt
        .turn_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let initial = independent_cycle(&started.data["cycle"], session, cycle_id, &input.id)?;
    let mut expected_current = initial.clone();
    if archived {
        expected_current.scheduling = state.scheduling.clone();
    }
    let current =
        bounded_cycle(connection, session, cycle_id)?.ok_or(SessionRepairRejection::Unproven)?;
    let faulty =
        bounded_cycle(connection, session, faulty_id)?.ok_or(SessionRepairRejection::Unproven)?;
    let original = independent_cycle(&json!(applied.cycle), session, faulty_id, faulty_input_id)?;
    let mut expected_faulty = original.clone();
    expected_faulty.active_turn_id = Some(turn.to_owned());
    expected_faulty.scheduling = state.scheduling.clone();
    let mut authorized_execution = applied.original_execution.clone();
    let mut authorized_token = authorized_execution
        .continuation
        .clone()
        .ok_or(SessionRepairRejection::Unproven)?;
    authorized_token.cycle_id = faulty_id.to_owned();
    authorized_execution.continuation = Some(authorized_token);
    authorized_execution.cycle_id = Some(faulty_id.to_owned());
    authorized_execution.phase = SessionExecutionPhase::Authorized;
    authorized_execution.scheduling = Some(SessionScheduling::default());
    authorized_execution.revision = authorized_execution
        .revision
        .checked_add(1)
        .ok_or(SessionRepairRejection::Unproven)?;
    authorized_execution.time_updated = control.time_created;
    if cycle_id == faulty_id
        || input.id == *faulty_input_id
        || current != expected_current
        || current.active_turn_id.is_some()
        || initial.scheduling.is_some()
        || original.active_turn_id.is_some()
        || original.scheduling.is_some()
        || faulty != expected_faulty
        || started.data["inputId"] != input.id
        || started.data["protectedGateRetained"] != true
        || started.data["legacyInterruption"] != false
        || started.data["previousScheduling"] != json!(state.scheduling)
        || !blocked(state.scheduling.as_ref())
        || !real_user(&old_input)
        || old_input.state != SubmissionState::Consumed
        || old_input.error.is_some()
        || old_input.cycle_id.as_deref() != Some(faulty_id)
        || old_input.revision != applied.recovered_input_revision
        || old_receipt.state != InputReceiptState::Failed
        || old_receipt.completed_at.is_none()
        || old_receipt.applied_at.is_some()
        || control_receipt.state != InputReceiptState::Failed
        || control_receipt.turn_id.as_deref() != Some(turn)
        || control_receipt.applied_at.is_some()
        || control_receipt.completed_at != old_receipt.completed_at
        || control.state != SubmissionState::Consumed
        || control.error.is_some()
        || control.trigger_kind != InputTriggerKind::UserControl
        || control.delivery != InputDelivery::Queue
        || control.cycle_id.as_deref() != Some(faulty_id)
        || control.source_key.as_deref()
            != Some(source_key(faulty_input_id, applied.report.expected_revision).as_str())
        || control.prompt
            != json!({"kind":"sessionControl","control":"resume_work",
            "continuation":applied.execution.continuation})
        || applied.report.session_id != *session
        || applied.report.disposition != SessionRepairDisposition::ControlQueued
        || applied.report.recovery_cycle_id.as_deref() != Some(faulty_id)
        || applied.report.expected_revision != applied.original_execution.revision
        || Some(applied.report.execution_revision)
            != applied.report.expected_revision.checked_add(1)
        || applied.execution.revision != applied.report.execution_revision
        || applied.execution.session_id != *session
        || applied.execution.cycle_id.as_deref() != Some(faulty_id)
        || applied.execution.mode != CollaborationMode::Work
        || applied.execution.phase != SessionExecutionPhase::Authorized
        || applied.execution.scheduling != Some(SessionScheduling::default())
        || applied.execution != authorized_execution
        || control.time_created < applied.original_execution.time_updated
        || applied.goal != protect_in(connection, session)?
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    // Recheck the original 503 proof using its frozen input/receipt coordinates,
    // not the failed input's current receipt. This is bounded and non-recursive.
    let original_events = window(
        connection,
        session,
        applied.report.evidence.receipt_sequence,
        applied.report.evidence.receipt_sequence,
    )?;
    let recorded = one(&original_events, "session.input.receipt.1", |d| {
        d["inputId"] == *faulty_input_id
    })?;
    let original_receipt: InputAdmissionReceipt = serde_json::from_value(recorded.data.clone())
        .map_err(|_| SessionRepairRejection::Unproven)?;
    let mut original_input = old_input.clone();
    original_input.revision = applied.report.evidence.input_revision;
    original_input.cycle_id = Some(applied.report.evidence.input_cycle_id.clone());
    require_unapplied(&original_input, &original_receipt)?;
    require_blocked(
        &applied.original_execution,
        &original_input,
        &original_receipt,
    )?;
    if original_input.revision.checked_add(1) != Some(applied.recovered_input_revision)
        || recorded.sequence.checked_add(1) != Some(control.admitted_sequence)
        || prove_legacy_in(
            connection,
            &applied.original_execution,
            &original_input,
            &original_receipt,
            recorded.sequence,
        )? != applied.report.evidence
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let events = window(
        connection,
        session,
        control.admitted_sequence,
        through_sequence,
    )?;
    let authorized = one(&events, "session.work_cycle.authorized.1", |_| true)?;
    let recovered = one(&events, "session.input.execution_recovered.1", |_| true)?;
    let audit_event = one(
        &events,
        "session.repair.legacy_false_blocked_applied.1",
        |_| true,
    )?;
    if authorized.data["cycle"] != json!(original)
        || recovered.data["inputId"] != *faulty_input_id
        || recovered.data["originCycleId"] != applied.report.evidence.input_cycle_id
        || recovered.data["cycleId"] != faulty_id
        || audit_event.sequence != audit.sequence
        || !(control.admitted_sequence < authorized.sequence
            && authorized.sequence < recovered.sequence
            && recovered.sequence < audit.sequence)
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    for row in [&control, input] {
        require_lifecycle(&events, row)?;
    }
    for id in [faulty_input_id, &input.id] {
        let user: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM message WHERE id=?1 AND session_id=?2 AND json_extract(data,'$.role')='user')",
            params![id, session], |row| row.get(0)).map_err(db)?;
        if !user {
            return Err(SessionRepairRejection::Unproven.into());
        }
    }
    let turn_started = one(&events, "session.turn.started.1", |_| true)?;
    let request_started = one(&events, "session.provider.request.1", |d| {
        d["status"] == "started"
    })?;
    let request_failed = one(&events, "session.provider.request.1", |d| {
        d["status"] == "failed"
    })?;
    let attempt_started = one(&events, "session.provider.attempt.1", |d| {
        d["status"] == "started"
    })?;
    let attempt_failed = one(&events, "session.provider.attempt.1", |d| {
        d["status"] == "failed"
    })?;
    let settled = one(&events, "session.turn.failure_settled.1", |_| true)?;
    let request = request_failed.data["requestID"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or(SessionRepairRejection::Unproven)?;
    if turn_started.data["turnID"] != turn
        || turn_started.data["cycleID"] != faulty_id
        || turn_started.data["anchorMessageID"] != *faulty_input_id
        || turn_started.data["turnTrigger"] != "user_control"
        || turn_started.data["userControl"] != "resume_work"
        || request_started.data["inputIDs"] != json!([])
        || request_failed.data["errorKind"] != "provider"
        || !recognized_rejection(&request_failed.data["providerDiagnostic"])
        || !recognized_rejection(&attempt_failed.data["providerDiagnostic"])
        || attempt_failed.data["turnErrorKind"] != "provider"
        || attempt_failed.data["errorKind"] != "fatal"
        || attempt_failed.data["retryable"] != false
        || settled.data["scope"] != json!({"cycleId":faulty_id,"turnId":turn,"goalId":null})
        || settled.data["effectiveGoalId"] != Value::Null
        || settled.data.get("effectiveGoalId").is_none()
        || settled.data["retryable"] != false
        || settled.data["ordinaryStopped"] != false
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    for event in [
        request_started,
        request_failed,
        attempt_started,
        attempt_failed,
    ] {
        if event.data["turnID"] != turn
            || event.data["requestID"] != request
            || (event.kind == "session.provider.attempt.1"
                && (event.data["cycleID"] != faulty_id
                    || event.data["turnTrigger"] != "user_control"))
        {
            return Err(SessionRepairRejection::Unproven.into());
        }
    }
    let failed = terminal_receipt(&events, &old_receipt)?;
    let control_failed = terminal_receipt(&events, &control_receipt)?;
    let gate = one(&events, "session.input.execution_gate.1", |d| {
        d["inputId"] == input.id
    })?;
    let new_receipt = one(&events, "session.input.receipt.1", |d| {
        d["inputId"] == input.id
    })?;
    let consumed = one(&events, "session.input.consumed.1", |d| {
        d["inputID"] == input.id
    })?;
    let control_consumed = one(&events, "session.input.consumed.1", |d| {
        d["inputID"] == control.id
    })?;
    if gate.data["gate"] != json!(receipt.execution_gate)
        || new_receipt.data != json!(receipt)
        || !(audit.sequence < control_consumed.sequence
            && control_consumed.sequence < turn_started.sequence
            && turn_started.sequence < request_started.sequence
            && request_started.sequence < attempt_started.sequence
            && attempt_started.sequence < attempt_failed.sequence
            && attempt_failed.sequence < request_failed.sequence
            && request_failed.sequence < failed.sequence
            && request_failed.sequence < control_failed.sequence
            && failed.sequence < settled.sequence
            && control_failed.sequence < settled.sequence
            && settled.sequence < input.admitted_sequence
            && input.admitted_sequence < started.sequence
            && started.sequence < consumed.sequence
            && consumed.sequence < gate.sequence
            && gate.sequence.checked_add(1) == Some(new_receipt.sequence))
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    for event in &events {
        let known = match event.kind.as_str() {
            "session.input.admitted.1"
            | "session.input.promoted.1"
            | "session.input.consumed.1" => {
                event.data["inputID"] == control.id || event.data["inputID"] == input.id
            }
            "session.input.receipt.1" => {
                event.sequence == new_receipt.sequence
                    || event.sequence == failed.sequence
                    || event.sequence == control_failed.sequence
                    || (event.data["state"] == "recorded"
                        && event.data["turnId"] == turn
                        && (event.data["inputId"] == *faulty_input_id
                            || event.data["inputId"] == control.id)
                        && event.sequence > control_consumed.sequence
                        && event.sequence < turn_started.sequence)
            }
            "session.work_cycle.started.1" => event.sequence == started.sequence,
            "session.input.execution_gate.1" => event.sequence == gate.sequence,
            "session.work_cycle.authorized.1" => event.sequence == authorized.sequence,
            "session.input.execution_recovered.1" => event.sequence == recovered.sequence,
            "session.repair.legacy_false_blocked_applied.1" => event.sequence == audit.sequence,
            "session.turn.started.1" => event.sequence == turn_started.sequence,
            "session.turn.failure_settled.1" => event.sequence == settled.sequence,
            "session.provider.request.1" => {
                [request_started.sequence, request_failed.sequence].contains(&event.sequence)
            }
            "session.provider.attempt.1" => {
                [attempt_started.sequence, attempt_failed.sequence].contains(&event.sequence)
            }
            "session.driver.phase.1" => {
                event.data["cycleId"] == faulty_id
                    && event.data["phase"] == "executing"
                    && event.sequence > control_consumed.sequence
                    && event.sequence < turn_started.sequence
            }
            "session.prompt.assembled.1" | "learning.retrieval.selected.1" => {
                event.sequence > control_consumed.sequence
                    && event.sequence < request_started.sequence
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
    if through_sequence != new_receipt.sequence
        || events
            .last()
            .is_none_or(|event| event.sequence != new_receipt.sequence)
    {
        return Err(SessionRepairRejection::LateEvent.into());
    }
    Ok(Some(SessionRepairEvidence {
        faulty_cycle_id: faulty_id.to_owned(),
        faulty_input_id: faulty_input_id.clone(),
        faulty_turn_id: turn.to_owned(),
        provider_request_id: request.to_owned(),
        input_cycle_id: cycle_id.to_owned(),
        input_revision: input.revision,
        faulty_cycle_sequence: authorized.sequence,
        provider_failure_sequence: request_failed.sequence,
        input_cycle_sequence: started.sequence,
        gate_sequence: gate.sequence,
        receipt_sequence: new_receipt.sequence,
        inherited_input_ids: Vec::new(),
    }))
}

fn recognized_rejection(value: &Value) -> bool {
    let Some(status) = value["status"].as_u64().and_then(|v| u16::try_from(v).ok()) else {
        return false;
    };
    value["code"].as_str().is_some_and(|code| {
        zuno_error::ProviderRequestRejection::from_wire(status, code)
            == Some(zuno_error::ProviderRequestRejection::ReasoningReplayContextMismatch)
    })
}

fn authorization(connection: &Connection, session: &str, cycle: &str) -> Result<Option<Event>> {
    let mut query = connection
        .prepare(
            "SELECT seq,type,CASE WHEN length(CAST(data AS BLOB))<=?3 THEN data END FROM event
         WHERE aggregate_id=?1 AND type='session.repair.legacy_false_blocked_applied.1'
         AND json_extract(data,'$.cycle.cycleId')=?2 ORDER BY seq LIMIT 2",
        )
        .map_err(db)?;
    let mut rows = query
        .query(params![session, cycle, sql_limit(MAX_EVENT_BYTES)?])
        .map_err(db)?;
    let Some(row) = rows.next().map_err(db)? else {
        return Ok(None);
    };
    let event = read_event(row)?;
    if rows.next().map_err(db)?.is_some() {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(Some(event))
}

fn terminal_receipt<'a>(events: &'a [Event], receipt: &InputAdmissionReceipt) -> Result<&'a Event> {
    let found = one(events, "session.input.receipt.1", |d| {
        d["inputId"] == receipt.input_id && d["state"] == "failed"
    })?;
    if found.data != json!(receipt) {
        return Err(SessionRepairRejection::Unproven.into());
    }
    Ok(found)
}

pub(super) fn require_lifecycle(events: &[Event], row: &SessionInput) -> Result<()> {
    let admitted = one(events, "session.input.admitted.1", |d| {
        d["inputID"] == row.id
    })?;
    let promoted = one(events, "session.input.promoted.1", |d| {
        d["inputID"] == row.id
    })?;
    let consumed = one(events, "session.input.consumed.1", |d| {
        d["inputID"] == row.id
    })?;
    if admitted.sequence != row.admitted_sequence
        || Some(promoted.sequence) != row.promoted_sequence
        || admitted.data["prompt"] != row.prompt
        || admitted.data["sessionID"] != row.session_id
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
        || consumed.data["sessionID"] != row.session_id
        || consumed.data["triggerKind"] != row.trigger_kind.as_str()
        || consumed.data["delivery"] != row.delivery.as_str()
        || !(admitted.sequence < promoted.sequence && promoted.sequence < consumed.sequence)
    {
        return Err(SessionRepairRejection::Changed.into());
    }
    Ok(())
}
