//! Input admission is independent from model application and turn completion.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputReceiptDelivery {
    Queue,
    Steer,
}

impl InputReceiptDelivery {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputReceiptState {
    Admitted,
    /// Persisted in conversation history, not proof of a provider request.
    Recorded,
    Applied,
    Completed,
    Failed,
    Cancelled,
}

impl InputReceiptState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Recorded => "recorded",
            Self::Applied => "applied",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admitted" => Some(Self::Admitted),
            "recorded" => Some(Self::Recorded),
            "applied" => Some(Self::Applied),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// The existing public ACP stop vocabulary used by native turn outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputStopReason {
    EndTurn,
    MaxTokens,
    Refusal,
    Cancelled,
}

impl InputStopReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::Refusal => "refusal",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "end_turn" => Some(Self::EndTurn),
            "max_tokens" => Some(Self::MaxTokens),
            "refusal" => Some(Self::Refusal),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Why a recorded input has not been allowed to enter a model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputGateReason {
    User,
    Authentication,
    TurnBudget,
    UncertainSideEffect,
    Blocked,
    WaitingHuman,
    WaitingExternal,
    NoProgress,
    NoExecutableWork,
    ExecutionUnavailable,
}

/// A recovery hint, not authorization to perform the indicated operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputGateRecovery {
    ResumeWork,
    ResumeGoal,
    StartWork,
    ResolveHumanRequest,
    WaitForEvent,
    Reauthenticate,
    InspectOutcome,
    ReviewBudget,
    InspectSession,
}

/// Frozen native execution decision. Stored in the session event ledger and
/// projected with the input receipt, independently of an ACP observer's lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputExecutionGate {
    pub reason: InputGateReason,
    pub recovery: InputGateRecovery,
    pub execution_revision: i64,
    pub cycle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

impl InputExecutionGate {
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self.recovery {
            InputGateRecovery::ResumeWork => {
                "Input saved but not applied: Work is paused. Use /resume to explicitly resume; do not resend this input."
            }
            InputGateRecovery::ResumeGoal => {
                "Input saved but not applied: the Goal requires explicit recovery. Inspect /goal and use its recovery control; do not resend this input."
            }
            InputGateRecovery::StartWork => {
                "Input saved but not applied: use Start Work to authorize the Plan; /resume cannot authorize it."
            }
            InputGateRecovery::ResolveHumanRequest => {
                "Input saved but not applied: resolve the pending human request through its own control."
            }
            InputGateRecovery::WaitForEvent => {
                "Input saved but not applied: the recorded external event is still required."
            }
            InputGateRecovery::Reauthenticate => {
                "Input saved but not applied: authentication must be repaired before recovery; /resume cannot waive this gate."
            }
            InputGateRecovery::InspectOutcome => {
                "Input saved but not applied: inspect the exact uncertain tool outcomes before recovery; /resume is not an inspection."
            }
            InputGateRecovery::ReviewBudget => {
                "Input saved but not applied: the turn budget requires its own validated recovery."
            }
            InputGateRecovery::InspectSession => {
                "Input saved but not applied: inspect the recorded execution gate before recovery."
            }
        }
    }
}

/// An authoritative receipt for one input, shared by every client surface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputAdmissionReceipt {
    pub session_id: String,
    pub input_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    pub admitted_sequence: i64,
    pub delivery: InputReceiptDelivery,
    pub state: InputReceiptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<InputStopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_gate: Option<InputExecutionGate>,
    pub time_updated: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_or_applying_input_does_not_finish_it() {
        for state in [
            InputReceiptState::Admitted,
            InputReceiptState::Recorded,
            InputReceiptState::Applied,
        ] {
            assert!(!state.is_terminal());
        }
        for state in [
            InputReceiptState::Completed,
            InputReceiptState::Failed,
            InputReceiptState::Cancelled,
        ] {
            assert!(state.is_terminal());
        }
    }

    #[test]
    fn receipt_spelling_is_closed_and_round_trips() {
        for state in [
            InputReceiptState::Admitted,
            InputReceiptState::Recorded,
            InputReceiptState::Applied,
            InputReceiptState::Completed,
            InputReceiptState::Failed,
            InputReceiptState::Cancelled,
        ] {
            assert_eq!(InputReceiptState::parse(state.as_str()), Some(state));
        }
        assert!(InputReceiptState::parse("received_as_busy").is_none());
        assert!(InputStopReason::parse("steered").is_none());
    }
}
