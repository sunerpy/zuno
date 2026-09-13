//! Reconstruct safe-point delivery from the durable inbox; notifications only
//! accelerate it. Controls authorize execution but never overtake saved answers.

use std::collections::BTreeMap;
use zuno_db::inbox::{DurableInputKind, SubmissionState};
use zuno_error::DbError;
use zuno_types::admission::{InputDeliveryBatch, InputDeliveryReference};
use zuno_types::execution::WakeAdmission;

use crate::interrupt::{SoftInterruptMessage, SoftInterruptSource};

pub(crate) struct PreparedInputBatch {
    pub receipt: InputDeliveryBatch,
    pub messages: Vec<SoftInterruptMessage>,
}

pub(crate) fn prepare(
    connection: &zuno_db::Connection,
    session_id: &str,
    turn_id: &str,
    notifications: Vec<SoftInterruptMessage>,
) -> Result<PreparedInputBatch, DbError> {
    let scope = zuno_db::session_work_cycle::current_in(connection, session_id)?;
    let mut receipt = InputDeliveryBatch {
        session_id: session_id.to_owned(),
        turn_id: turn_id.to_owned(),
        cycle_id: scope.as_ref().map(|scope| scope.cycle_id.clone()),
        inputs: Vec::new(),
    };
    if scope.as_ref().is_some_and(|scope| {
        scope.stopped.is_some()
            || scope
                .active_turn_id
                .as_deref()
                .is_some_and(|active| active != turn_id)
    }) {
        return Ok(PreparedInputBatch {
            receipt,
            messages: Vec::new(),
        });
    }
    let mut hints = BTreeMap::new();
    let mut legacy = Vec::new();
    for hint in notifications {
        if let Some(id) = &hint.input_id {
            hints.insert(id.clone(), hint);
        } else {
            legacy.push(hint);
        }
    }
    let mut messages = Vec::new();
    // pending_in is admitted_seq ordered. Explicitly queued ordinary prompts
    // remain queued; answers and settled reports are safe-point inputs.
    for input in zuno_db::inbox::pending_in(connection, session_id)? {
        let Some(kind) = DurableInputKind::classify(&input.prompt) else {
            continue;
        };
        let hinted = hints.remove(&input.id);
        if hinted
            .as_ref()
            .is_some_and(|hint| hint.revision.is_some_and(|r| r != input.revision))
        {
            continue;
        }
        if hinted.is_none()
            && input.state != SubmissionState::Steering
            && kind != DurableInputKind::HumanRequestAnswer
            && !kind.is_asynchronous_report()
        {
            continue;
        }
        if zuno_db::session_wake::pending_admission_in(connection, &input)? == WakeAdmission::Reject
        {
            continue;
        }
        let message = if let Some(hint) = hinted {
            hint.with_revision(input.revision)
        } else {
            // Complex client content requires its native projection. Do not
            // silently discard image blocks or Agent/model overrides.
            if kind
                .content_blocks(&input.prompt)
                .is_some_and(|blocks| !blocks.is_empty())
            {
                continue;
            }
            let Some(text) = kind.plain_text(&input.prompt) else {
                continue;
            };
            SoftInterruptMessage {
                input_id: Some(input.id.clone()),
                revision: Some(input.revision),
                content: text.to_owned(),
                images: Vec::new(),
                attachments: Vec::new(),
                urgent: false,
                source: if kind.is_asynchronous_report() {
                    SoftInterruptSource::BackgroundTask
                } else if kind == DurableInputKind::SessionMessage {
                    SoftInterruptSource::PeerSession
                } else {
                    SoftInterruptSource::User
                },
            }
        };
        receipt.inputs.push(InputDeliveryReference {
            input_id: input.id,
            admitted_sequence: input.admitted_sequence,
            expected_revision: input.revision,
        });
        messages.push(message);
    }
    messages.extend(legacy);
    Ok(PreparedInputBatch { receipt, messages })
}
