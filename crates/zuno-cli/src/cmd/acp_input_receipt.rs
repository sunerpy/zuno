//! ACP admission receipts and session-owned native input delivery.
//!
//! RPC requests observe durable processing outcomes. The session retains the
//! existing TurnHost drive future, so dropping an observer cannot drop shared
//! execution or infer a stop reason from an idle lease.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use zuno_acp::{ClientConnection, RequestId, RpcError};
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInput, SubmissionState};
use zuno_engine::admission::{InputAdmission, TurnLease};
use zuno_engine::status::{SessionRunGuard, SessionStatus};
use zuno_types::admission::{InputAdmissionReceipt, InputReceiptState, InputStopReason};

use super::{
    AcpPrompt, AcpSession, DurableInputScope, SessionDurableHandles, WithdrawablePrompt,
    acp_prompt_payload, durable_questions, steering_content,
};

const RECEIPT_RECONCILE_INTERVAL: Duration = Duration::from_millis(100);
const EXECUTION_OBSERVATION_UNAVAILABLE_CODE: i64 = -32004;
const EXECUTION_GATED_CODE: i64 = -32005;

pub(super) fn message_id(params: &Value) -> Result<Option<String>, RpcError> {
    let Some(meta) = params.get("_meta").filter(|meta| !meta.is_null()) else {
        return Ok(None);
    };
    let meta = meta
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("_meta must be an object"))?;
    let Some(zuno) = meta.get("zuno").filter(|zuno| !zuno.is_null()) else {
        return Ok(None);
    };
    let zuno = zuno
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("_meta.zuno must be an object"))?;
    let Some(value) = zuno.get("messageId") else {
        return Ok(None);
    };
    let id = value
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 256)
        .ok_or_else(|| {
            RpcError::invalid_params(
                "_meta.zuno.messageId must be a non-empty string of at most 256 bytes",
            )
        })?;
    Ok(Some(id.to_owned()))
}

pub(super) enum TurnOwner {
    Request(RequestId),
    Durable,
}

struct DriverClaim {
    session: Arc<AcpSession>,
}

impl Drop for DriverClaim {
    fn drop(&mut self) {
        *self
            .session
            .turn_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl AcpSession {
    pub(super) async fn admit_and_drive_content(
        self: &Arc<Self>,
        mut prompt: AcpPrompt,
        message_id: Option<String>,
        handles: &SessionDurableHandles,
        withdrawable: &WithdrawablePrompt<'_>,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        prompt.admit_images(handles.attachments.as_ref())?;
        let mut row = NewSessionInput::new(
            format!("msg_{}", uuid::Uuid::new_v4().simple()),
            self.id.clone(),
            acp_prompt_payload(&prompt)?,
            InputDelivery::Steer,
            zuno_db::message::now_millis(),
        );
        if let Some(message_id) = &message_id {
            row = row.with_source_key(format!("client-message:{message_id}"));
        }
        let first_input = if handles.identity.is_materialized() {
            None
        } else {
            let mut resources = self.resources.lock().await;
            let resources = resources.as_mut().ok_or_else(|| self.closed_error())?;
            if handles.identity.is_materialized() {
                None
            } else {
                resources
                    .host
                    .materialize_session_with_input(row.clone())
                    .map_err(RpcError::internal)?
            }
        };
        let (input, duplicate) = match first_input {
            Some(input) => {
                // The shared first-admission transaction creates both records.
                self.receipt_for_input(&input)?;
                (input, false)
            }
            None => {
                let admitted = self
                    .receipts
                    .admit(row)
                    .map_err(|error| admission_error(error, message_id.as_deref()))?;
                (admitted.input, admitted.duplicate)
            }
        };
        if withdrawable.publish(&input.id, !duplicate) {
            if !duplicate {
                self.retire_pending_input(&input.id);
            }
            return Err(withdrawn(&self.receipt_for_input(&input)?));
        }
        if !duplicate {
            let receipt = self.receipt_for_input(&input)?;
            self.questions
                .offer_goal_resume(&self.id, Some(&input.id))
                .await
                .map_err(|error| accepted_error(&receipt, RpcError::internal(error.to_string())))?;
            if withdrawable.withdrawn() {
                self.retire_pending_input(&input.id);
                return Err(withdrawn(&self.receipt_for_input(&input)?));
            }
            let claim = self.claim_prompt_driver();
            let lease = if claim.is_some() {
                TurnLease::Acquire
            } else {
                TurnLease::Deferred
            };
            let admitted = handles.admission.route_admitted(
                input.clone(),
                lease,
                Some(steering_content(&prompt)),
            );
            if let InputAdmission::Drive { guard, .. } = admitted {
                self.start_prompt_driver(
                    &input.id,
                    guard,
                    claim.expect("a drive admission requires the caller's driver claim"),
                    client,
                );
            }
            // A steer into an autonomous turn owns no driver claim. Releasing
            // it here lets the native owner and queued-input recovery cooperate.
        }
        self.wait_for_prompt_receipt(&input, handles, withdrawable, client)
            .await
    }

    pub(super) fn receipt_for_input(
        &self,
        input: &SessionInput,
    ) -> Result<InputAdmissionReceipt, RpcError> {
        self.receipts
            .get(&self.id, &input.id)
            .map_err(|error| admitted_error(input, RpcError::internal(error.to_string())))?
            .ok_or_else(|| {
                admitted_error(
                    input,
                    RpcError::internal("accepted input is missing its durable receipt"),
                )
            })
    }

    fn claim_prompt_driver(self: &Arc<Self>) -> Option<DriverClaim> {
        let mut owner = self
            .turn_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.is_some() {
            return None;
        }
        *owner = Some(TurnOwner::Durable);
        Some(DriverClaim {
            session: Arc::clone(self),
        })
    }

    /// Retain one existing native FIFO drive independently of RPC observers.
    fn start_prompt_driver(
        self: &Arc<Self>,
        admitted_input_id: &str,
        guard: SessionRunGuard,
        claim: DriverClaim,
        client: &ClientConnection,
    ) {
        let session = Arc::clone(self);
        let client = client.session_scoped();
        let admitted_input_id = admitted_input_id.to_owned();
        let task = tokio::spawn(async move {
            let _claim = claim;
            let outcome = async {
                if session.closed.load(Ordering::Acquire) {
                    return Ok(());
                }
                session.recover_pending_permissions(&client, &guard).await?;
                let next = session
                    .drive_next_durable_input(&client, &guard, DurableInputScope::Prompts)
                    .await?;
                drop(guard);
                if let Some((driven, projected)) = next {
                    session
                        .settle_turn(driven, projected, false, &client)
                        .await?;
                }
                Ok::<(), RpcError>(())
            }
            .await;
            if let Err(error) = outcome {
                tracing::warn!(
                    session_id = %session.id,
                    %admitted_input_id,
                    %error,
                    "ACP native input drive stopped; observers retain their durable receipts"
                );
            }
        });
        *self
            .prompt_driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }

    /// Recover a pending FIFO handoff through the existing native driver.
    ///
    /// Duplicate observers may wake delivery; they never re-admit or re-steer the
    /// input and never own this session task's lifetime.
    fn wake_prompt_driver(
        self: &Arc<Self>,
        handles: &SessionDurableHandles,
        client: &ClientConnection,
    ) -> Result<(), RpcError> {
        if self.closed.load(Ordering::Acquire) || self.control.status() == SessionStatus::Busy {
            return Ok(());
        }
        let Some((input, _)) = durable_questions::next_input(
            handles.admission.inbox(),
            &self.id,
            DurableInputScope::Prompts,
        )?
        else {
            return Ok(());
        };
        let Some(claim) = self.claim_prompt_driver() else {
            return Ok(());
        };
        let Ok(guard) = self.runs.begin_turn(self.id.clone()) else {
            return Ok(());
        };
        self.start_prompt_driver(&input.id, guard, claim, client);
        Ok(())
    }

    async fn wait_for_prompt_receipt(
        self: &Arc<Self>,
        input: &SessionInput,
        handles: &SessionDurableHandles,
        withdrawable: &WithdrawablePrompt<'_>,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        // Subscribe before the first read. Reconciliation also observes commits
        // made by another connection and changes during runtime replacement.
        let mut changes = handles.work_changes.clone();
        let mut watching = true;
        let mut reconcile = tokio::time::interval(RECEIPT_RECONCILE_INTERVAL);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let receipt = self.receipt_for_input(input)?;
            if withdrawable.withdrawn() {
                return Err(withdrawn(&receipt));
            }
            if let Some(response) = terminal_response(&receipt) {
                self.publications
                    .wait_for(&receipt.input_id, receipt.turn_id.as_deref())
                    .await;
                return response;
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(accepted_error(
                    &receipt,
                    RpcError::cancelled("session closed while this accepted input was pending"),
                ));
            }
            if !self.can_observe_input(&receipt)
                && handles
                    .admission
                    .inbox()
                    .get(&self.id, &input.id)
                    .map_err(|error| {
                        accepted_error(&receipt, RpcError::internal(error.to_string()))
                    })?
                    .is_some_and(|current| current.state == SubmissionState::Consumed)
            {
                // Native completion may have committed between our first read
                // and the ownership observation. Never replace it with a guessed
                // execution result or a stale observer-unavailable response.
                let latest = self.receipt_for_input(input)?;
                if let Some(response) = terminal_response(&latest) {
                    self.publications
                        .wait_for(&latest.input_id, latest.turn_id.as_deref())
                        .await;
                    return response;
                }
                if !self.can_observe_input(&latest) {
                    return Err(observation_unavailable(&latest));
                }
            }
            self.wake_prompt_driver(handles, client)
                .map_err(|error| accepted_error(&receipt, error))?;
            let busy = self.control.status() == SessionStatus::Busy;
            tokio::select! {
                changed = changes.changed(), if watching => watching = changed.is_ok(),
                () = self.control.wait_until_idle(), if busy => {}
                _ = reconcile.tick() => {}
            }
        }
    }

    /// Positive local evidence that this runtime can still observe this input.
    ///
    /// Absence only means observation is unavailable here. It cannot prove that
    /// another process stopped executing the durable turn.
    fn can_observe_input(&self, receipt: &InputAdmissionReceipt) -> bool {
        if receipt
            .turn_id
            .as_deref()
            .is_some_and(|turn_id| self.control.active_turn_id().as_deref() == Some(turn_id))
            || self.control.active_input_id().as_deref() == Some(receipt.input_id.as_str())
        {
            return true;
        }
        self.prompt_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|request| {
                request.owns_input && request.input_id.as_deref() == Some(receipt.input_id.as_str())
            })
    }
}

fn admission_error(error: zuno_error::DbError, message_id: Option<&str>) -> RpcError {
    if matches!(error, zuno_error::DbError::Conflict { .. }) {
        return RpcError::invalid_params(error.to_string()).with_data(json!({
            "admission": "rejected",
            "reason": "clientMessageConflict",
            "messageId": message_id,
        }));
    }
    RpcError::internal(error.to_string())
}

fn admitted_error(input: &SessionInput, error: RpcError) -> RpcError {
    error.with_data(json!({
        "admission": "accepted",
        "sessionId": input.session_id,
        "inputId": input.id,
        "admittedSequence": input.admitted_sequence,
    }))
}

fn accepted_error(receipt: &InputAdmissionReceipt, error: RpcError) -> RpcError {
    error.with_data(json!({
        "admission": "accepted",
        "sessionId": receipt.session_id,
        "inputId": receipt.input_id,
        "admittedSequence": receipt.admitted_sequence,
        "receipt": receipt,
    }))
}

fn observation_unavailable(receipt: &InputAdmissionReceipt) -> RpcError {
    let mut error = accepted_error(
        receipt,
        RpcError {
            code: EXECUTION_OBSERVATION_UNAVAILABLE_CODE,
            message: "input was accepted, but this runtime cannot observe its existing execution; recovery or the owning runtime is required".to_owned(),
            data: None,
        },
    );
    if let Some(data) = error.data.as_mut() {
        data["reason"] = json!("executionObservationUnavailable");
        data["recoveryRequired"] = json!(true);
    }
    error
}

fn withdrawn(receipt: &InputAdmissionReceipt) -> RpcError {
    let mut error = accepted_error(receipt, RpcError::cancelled("prompt request withdrawn"));
    if receipt.state == InputReceiptState::Cancelled
        && let Some(data) = error.data.as_mut()
    {
        data["admission"] = json!("withdrawn");
    }
    error
}

fn terminal_response(receipt: &InputAdmissionReceipt) -> Option<Result<Value, RpcError>> {
    if let Some(gate) = &receipt.execution_gate {
        let mut error = accepted_error(
            receipt,
            RpcError {
                code: EXECUTION_GATED_CODE,
                message: gate.message().to_owned(),
                data: None,
            },
        );
        if let Some(data) = error.data.as_mut() {
            data["reason"] = json!("executionGated");
            data["recoveryRequired"] = json!(true);
        }
        return Some(Err(error));
    }
    if !receipt.state.is_terminal() {
        return None;
    }
    if receipt.state == InputReceiptState::Failed {
        return Some(Err(accepted_error(
            receipt,
            RpcError::internal(
                receipt
                    .error
                    .as_deref()
                    .unwrap_or("accepted input failed without a stored diagnostic"),
            ),
        )));
    }
    let reason = if receipt.state == InputReceiptState::Cancelled {
        InputStopReason::Cancelled
    } else if let Some(reason) = receipt.stop_reason {
        reason
    } else {
        return Some(Err(accepted_error(
            receipt,
            RpcError::internal("completed input receipt is missing its stop reason"),
        )));
    };
    Some(Ok(json!({
        "stopReason": reason.as_str(),
        "_meta": {"zuno": {"receipt": receipt}},
    })))
}
