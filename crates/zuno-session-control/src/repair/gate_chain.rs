//! Bounded proof of repeated user input inheriting the same obsolete gate.
//!
//! Walk only native, consecutive, non-executing admission transitions. Never
//! collapse inputs by text, rewrite history, or treat a previous pause as proof
//! of its own cause. The root still needs the existing complete rejection proof.

use super::*;

const MAX_INHERITED_GATES: usize = 16;

struct Frame {
    state: SessionExecutionState,
    input: SessionInput,
    receipt: InputAdmissionReceipt,
    through: i64,
}

pub(super) fn prove_in(
    connection: &Connection,
    state: &SessionExecutionState,
    input: &SessionInput,
    receipt: &InputAdmissionReceipt,
    through_sequence: i64,
) -> Result<Option<SessionRepairEvidence>> {
    let mut frame = Frame {
        state: state.clone(),
        input: input.clone(),
        receipt: receipt.clone(),
        through: through_sequence,
    };
    let mut inherited = Vec::new();
    loop {
        let Some(previous) = previous_frame(connection, &frame, !inherited.is_empty())? else {
            if inherited.is_empty() {
                return Ok(None);
            }
            return Err(SessionRepairRejection::Unproven.into());
        };
        if inherited.len() == MAX_INHERITED_GATES || inherited.contains(&previous.input.id) {
            return Err(SessionRepairRejection::EvidenceLimit.into());
        }
        inherited.push(previous.input.id.clone());
        frame = previous;
        if let Some(mut evidence) = request_rejection::prove_archived_in(
            connection,
            &frame.state,
            &frame.input,
            &frame.receipt,
            frame.through,
        )? {
            let cycle_id = input
                .cycle_id
                .as_deref()
                .ok_or(SessionRepairRejection::Unproven)?;
            let latest_start = cycle_started(connection, &input.session_id, cycle_id)?;
            evidence.input_cycle_id = cycle_id.to_owned();
            evidence.input_revision = input.revision;
            evidence.input_cycle_sequence = latest_start.sequence;
            evidence.gate_sequence = through_sequence
                .checked_sub(1)
                .ok_or(SessionRepairRejection::Unproven)?;
            evidence.receipt_sequence = through_sequence;
            inherited.reverse();
            evidence.inherited_input_ids = inherited;
            return Ok(Some(evidence));
        }
    }
}

fn previous_frame(connection: &Connection, frame: &Frame, archived: bool) -> Result<Option<Frame>> {
    let session = &frame.input.session_id;
    let cycle_id = frame
        .input
        .cycle_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let started = cycle_started(connection, session, cycle_id)?;
    let previous_id = started.data["previousCycleId"]
        .as_str()
        .ok_or(SessionRepairRejection::Unproven)?;
    let previous_cycle =
        bounded_cycle(connection, session, previous_id)?.ok_or(SessionRepairRejection::Unproven)?;
    // A real failed turn is a root candidate, not another gate-only link.
    if previous_cycle.active_turn_id.is_some() {
        return Ok(None);
    }
    let previous_input_id = previous_cycle
        .anchor_message_id
        .as_deref()
        .ok_or(SessionRepairRejection::Unproven)?;
    let previous_input = bounded_input(connection, session, previous_input_id)?;
    let previous_receipt = input_receipt::get_in(connection, session, previous_input_id)?
        .ok_or(SessionRepairRejection::Unproven)?;
    require_unapplied(&frame.input, &frame.receipt)?;
    require_unapplied(&previous_input, &previous_receipt)?;
    require_blocked(&frame.state, &frame.input, &frame.receipt)?;
    for id in [&frame.input.id, &previous_input.id] {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM message WHERE session_id=?1 AND id=?2
             AND json_extract(data,'$.role')='user')",
                params![session, id],
                |row| row.get(0),
            )
            .map_err(db)?;
        if !present {
            return Err(SessionRepairRejection::Unproven.into());
        }
    }
    let previous_started = cycle_started(connection, session, previous_id)?;
    let previous_initial = independent_cycle(
        &previous_started.data["cycle"],
        session,
        previous_id,
        previous_input_id,
    )?;
    let initial = independent_cycle(&started.data["cycle"], session, cycle_id, &frame.input.id)?;
    let mut expected = initial.clone();
    if archived {
        expected.scheduling = frame.state.scheduling.clone();
    }
    let mut expected_previous = previous_initial.clone();
    expected_previous.scheduling = frame.state.scheduling.clone();
    let current_cycle =
        bounded_cycle(connection, session, cycle_id)?.ok_or(SessionRepairRejection::Unproven)?;
    let previous_recorded = recorded_gate(connection, session, previous_input_id)?;
    let gate = previous_receipt
        .execution_gate
        .as_ref()
        .ok_or(SessionRepairRejection::Unproven)?;
    if cycle_id == previous_id
        || current_cycle != expected
        || initial.active_turn_id.is_some()
        || initial.scheduling.is_some()
        || previous_initial.active_turn_id.is_some()
        || previous_initial.scheduling.is_some()
        || previous_cycle != expected_previous
        || previous_input.cycle_id.as_deref() != Some(previous_id)
        || gate.cycle_id != previous_id
        || previous_recorded.data != json!(previous_receipt)
        || gate.execution_revision.checked_add(1) != Some(frame.state.revision)
        || started.data["inputId"] != frame.input.id
        || started.data["protectedGateRetained"] != true
        || started.data["legacyInterruption"] != false
        || started.data["legacyInterruptionProof"] != Value::Null
        || started.data["previousScheduling"] != json!(frame.state.scheduling)
        || started.data["time"] != frame.state.time_updated
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let events = window(
        connection,
        session,
        previous_recorded
            .sequence
            .checked_add(1)
            .ok_or(SessionRepairRejection::Unproven)?,
        frame.through,
    )?;
    let kinds = [
        "session.input.admitted.1",
        "session.input.promoted.1",
        "session.work_cycle.started.1",
        "session.input.consumed.1",
        "session.input.execution_gate.1",
        "session.input.receipt.1",
    ];
    if events.len() != kinds.len()
        || !events
            .iter()
            .zip(kinds)
            .all(|(event, kind)| event.kind == kind)
    {
        return Err(SessionRepairRejection::LateEvent.into());
    }
    request_rejection::require_lifecycle(&events, &frame.input)?;
    if events[2].sequence != started.sequence
        || events[4].data["inputId"] != frame.input.id
        || events[4].data["gate"] != json!(frame.receipt.execution_gate)
        || events[5].data != json!(frame.receipt)
        || events[5].sequence != frame.through
        || previous_started.sequence >= previous_recorded.sequence
    {
        return Err(SessionRepairRejection::Unproven.into());
    }
    let mut previous_state = frame.state.clone();
    previous_state.revision = gate.execution_revision;
    previous_state.cycle_id = Some(previous_id.to_owned());
    previous_state.time_updated = previous_started.data["time"]
        .as_i64()
        .ok_or(SessionRepairRejection::Unproven)?;
    let token = previous_state
        .continuation
        .as_mut()
        .ok_or(SessionRepairRejection::Unproven)?;
    token.cycle_id = previous_id.to_owned();
    token.anchor_message_id = Some(previous_input_id.to_owned());
    require_blocked(&previous_state, &previous_input, &previous_receipt)?;
    Ok(Some(Frame {
        state: previous_state,
        input: previous_input,
        receipt: previous_receipt,
        through: previous_recorded.sequence,
    }))
}

fn recorded_gate(connection: &Connection, session: &str, input: &str) -> Result<Event> {
    let mut statement = connection.prepare(
        "SELECT seq,type,CASE WHEN length(CAST(data AS BLOB))<=?3 THEN data END FROM event
         WHERE aggregate_id=?1 AND type='session.input.receipt.1'
           AND json_extract(data,'$.inputId')=?2 AND json_extract(data,'$.executionGate') IS NOT NULL
         ORDER BY seq LIMIT 2",
    ).map_err(db)?;
    let mut rows = statement
        .query(params![session, input, sql_limit(MAX_EVENT_BYTES)?])
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
