//! Typed, durable reasons that suspend an otherwise resumable Goal.

use crate::GoalError;
use serde::{Deserialize, Serialize};

/// Why automatic Goal execution is currently suspended.
///
/// The reason is persisted separately from [`crate::GoalStatus::Paused`] so a
/// restart can distinguish an intentional mode switch from a condition that
/// still requires human action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReason {
    /// The user interrupted or manually paused execution.
    UserInterruption,
    /// The session entered read-only planning mode.
    PlanMode,
    /// The Goal explicitly requested missing human input.
    HumanInput,
    /// A permission decision is still owned by a human.
    Permission,
    /// Provider credentials must be repaired before work can continue.
    Authentication,
    /// A side effect may have completed even though its response was lost.
    UncertainSideEffect,
    /// One turn spent the allowance the budget policy gave it.
    ///
    /// Distinct from [`crate::GoalStatus::BudgetLimited`], which is the whole goal's
    /// token budget running out. This is a single turn stopped mid-flight, so the goal
    /// still has budget left and the work is still resumable — but not automatically,
    /// because the next turn would spend the same allowance the same way.
    TurnBudget,
}

impl GoalPauseReason {
    /// Closed set persisted in SQLite and exposed on status surfaces.
    pub const ALL: [Self; 7] = [
        Self::UserInterruption,
        Self::PlanMode,
        Self::HumanInput,
        Self::Permission,
        Self::Authentication,
        Self::UncertainSideEffect,
        Self::TurnBudget,
    ];

    /// Stable storage and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserInterruption => "user_interruption",
            Self::PlanMode => "plan_mode",
            Self::HumanInput => "human_input",
            Self::Permission => "permission",
            Self::Authentication => "authentication",
            Self::UncertainSideEffect => "uncertain_side_effect",
            Self::TurnBudget => "turn_budget",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, GoalError> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
            .ok_or_else(|| GoalError::UnknownPauseReason {
                value: value.to_owned(),
            })
    }

    /// Whether a settled durable human request can make this pause resumable.
    #[must_use]
    pub const fn waits_for_human_request(self) -> bool {
        matches!(self, Self::HumanInput | Self::Permission)
    }
}

impl std::fmt::Display for GoalPauseReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One durable pause attached to an exact Goal instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalPauseState {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "goalID")]
    pub goal_id: String,
    pub reason: GoalPauseReason,
    #[serde(rename = "humanRequestID", skip_serializing_if = "Option::is_none")]
    pub human_request_id: Option<String>,
    pub paused_at_ms: i64,
}

/// Human-interaction capabilities exposed to one agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionPolicy {
    /// A Plan turn may ask ordinary clarifying questions.
    PlanClarification,
    /// Non-Goal work proceeds autonomously and asks a direct turn-boundary
    /// question only when no safe in-scope default exists.
    WorkAutonomous,
    /// Active Goal work may only create a durable Goal request and yield.
    GoalAutonomous,
    /// A child agent reports blockers to its parent instead of contacting a user.
    SubagentReportOnly,
}

impl InteractionPolicy {
    /// Whether the ordinary synchronous `question` tool may be registered.
    #[must_use]
    pub const fn allows_question(self) -> bool {
        matches!(self, Self::PlanClarification)
    }

    /// Whether `goal_request_input` may be registered.
    #[must_use]
    pub const fn allows_goal_request_input(self) -> bool {
        matches!(self, Self::GoalAutonomous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_reasons_round_trip_through_their_stable_spelling() {
        for reason in GoalPauseReason::ALL {
            assert_eq!(
                GoalPauseReason::parse(reason.as_str()).expect("parse"),
                reason
            );
        }
    }

    #[test]
    fn only_autonomous_goals_get_the_durable_request_tool() {
        assert!(InteractionPolicy::GoalAutonomous.allows_goal_request_input());
        assert!(!InteractionPolicy::GoalAutonomous.allows_question());
        assert!(InteractionPolicy::PlanClarification.allows_question());
        assert!(!InteractionPolicy::WorkAutonomous.allows_question());
        assert!(!InteractionPolicy::SubagentReportOnly.allows_question());
    }
}
