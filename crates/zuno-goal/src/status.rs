//! The six goal statuses, and the split that decides who may write which.
//!
//! Ports `codex-rs/state/src/model/thread_goal.rs:14-42` for the variants, the
//! wire strings and the `is_active` / `is_terminal` predicates. The ownership
//! split below is *not* in codex — codex lets any caller name any status and
//! then repairs the outcome in SQL — and is the substance of this crate.
//!
//! # Why ownership is carried by types rather than by a doc comment
//!
//! A goal is the agent's north star: the reason a long run keeps going after the
//! conversation that started it has been compacted away. Three of the six
//! statuses stop that run — `paused`, `usage_limited`, `budget_limited` — and if
//! the model could write them it could stop itself the moment the work got hard,
//! or clear a budget limit it had just hit and keep spending. So the two scopes
//! do not share a status type at all:
//!
//! * [`ModelStatus`] has two variants, `blocked` and `complete`. There is no
//!   `paused` to pass. A future caller cannot smuggle one through the model path
//!   because the type cannot represent it.
//! * [`SystemStatus`] has the other four.
//!
//! [`GoalStatus`] is the union, and exists because that is what the column
//! stores. Nothing accepts a bare `GoalStatus` as a write.
//!
//! # Why `active` is system-owned
//!
//! It is the one non-obvious assignment. The model already obtains `active` by
//! creating a goal, so making it writable buys nothing — and it would be a hole:
//! a model sitting on a `budget_limited` goal could set `active` and carry on.
//! codex closes the same hole one layer lower, re-deriving the budget limit in
//! the `UPDATE` (`codex-rs/state/src/runtime/goals.rs:329`); this crate closes
//! it in the type system *and* keeps codex's SQL guard, because the SQL guard
//! also covers the system's own reactivations.

use crate::error::GoalError;
use serde::{Deserialize, Serialize};

/// Which scope is allowed to write a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusOwner {
    /// The model may write it through the goal tool.
    Model,
    /// Only the runtime or the user may write it.
    System,
}

impl StatusOwner {
    /// Whether the model may write statuses in this scope.
    #[must_use]
    pub fn is_model(self) -> bool {
        matches!(self, Self::Model)
    }
}

/// A goal status as the `goal.status` column stores it.
///
/// The wire strings are the `CHECK` constraint's members, verbatim, so a value
/// that round-trips through serde is also a value the column accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The agent should keep working towards the objective.
    Active,
    /// Stopped by the user, and only the user resumes it.
    Paused,
    /// The model reported it cannot proceed without help.
    Blocked,
    /// Stopped because the account ran out of provider usage.
    UsageLimited,
    /// Stopped because `tokens_used` reached `token_budget`.
    BudgetLimited,
    /// The model reported the objective met.
    Complete,
    /// The user explicitly abandoned this goal instance.
    Cancelled,
}

impl GoalStatus {
    /// Every status, in the order the `CHECK` constraint lists them.
    ///
    /// Exported so a test can walk the whole matrix rather than the handful of
    /// transitions whoever wrote it happened to think of.
    pub const ALL: [Self; 7] = [
        Self::Active,
        Self::Paused,
        Self::Blocked,
        Self::UsageLimited,
        Self::BudgetLimited,
        Self::Complete,
        Self::Cancelled,
    ];

    /// The stored string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a status back out of the column.
    ///
    /// A failure here is corruption or a schema/code skew, never model input —
    /// the `CHECK` constraint makes every other value unstorable — so it does
    /// not use the model-facing refusal.
    ///
    /// # Errors
    ///
    /// [`GoalError::UnknownStatus`] when the column holds a value outside
    /// [`GoalStatus::ALL`].
    pub fn parse(value: &str) -> Result<Self, GoalError> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| GoalError::UnknownStatus {
                value: value.to_owned(),
                expected: Self::rendered_values(),
            })
    }

    /// Which scope may write this status.
    #[must_use]
    pub fn owner(self) -> StatusOwner {
        match self {
            Self::Blocked | Self::Complete => StatusOwner::Model,
            Self::Active
            | Self::Paused
            | Self::UsageLimited
            | Self::BudgetLimited
            | Self::Cancelled => StatusOwner::System,
        }
    }

    /// Whether the agent should keep working (`thread_goal.rs:35-37`).
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether the goal has stopped for good (`thread_goal.rs:39-41`).
    ///
    /// `paused` and `usage_limited` are absent deliberately: both are expected
    /// to resume, so a continuation board must not retire them.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::BudgetLimited | Self::Complete | Self::Cancelled)
    }

    /// Every status string, for an error message.
    #[must_use]
    pub fn rendered_values() -> String {
        render(Self::ALL.iter().map(|status| status.as_str()))
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A status the model itself is allowed to write.
///
/// Both variants are the model reporting on its own work: it finished, or it is
/// stuck. Neither can stop a run for a reason the model gets to invent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    /// Cannot proceed without help.
    Blocked,
    /// The objective is met.
    Complete,
}

impl ModelStatus {
    /// Every status the model may write.
    pub const ALL: [Self; 2] = [Self::Blocked, Self::Complete];

    /// The stored string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into_status().as_str()
    }

    /// Widen to the stored type.
    #[must_use]
    pub fn into_status(self) -> GoalStatus {
        match self {
            Self::Blocked => GoalStatus::Blocked,
            Self::Complete => GoalStatus::Complete,
        }
    }

    /// Accept a status string the model supplied, or refuse it by name.
    ///
    /// This is the boundary the goal tool calls, so the refusal is a
    /// model-visible artifact: it names the two values that would have worked,
    /// because a refusal that only says "no" costs a whole extra turn.
    ///
    /// # Errors
    ///
    /// [`GoalError::StatusNotModelOwned`] when `value` is a real status the
    /// system owns, and [`GoalError::UnknownStatus`] when it is not a status at
    /// all. Both name the allowed values.
    pub fn parse(value: &str) -> Result<Self, GoalError> {
        match GoalStatus::parse(value) {
            Ok(status) => Self::from_status(status),
            Err(GoalError::UnknownStatus { value, .. }) => Err(GoalError::UnknownStatus {
                value,
                expected: Self::rendered_values(),
            }),
            Err(other) => Err(other),
        }
    }

    /// Narrow a stored status to the model's scope, or refuse it.
    ///
    /// The path for a caller that already deserialized a [`GoalStatus`] enum
    /// rather than a string. It funnels into the same refusal so there is one
    /// message to keep correct.
    ///
    /// # Errors
    ///
    /// [`GoalError::StatusNotModelOwned`] when `status` is system-owned.
    pub fn from_status(status: GoalStatus) -> Result<Self, GoalError> {
        match status {
            GoalStatus::Blocked => Ok(Self::Blocked),
            GoalStatus::Complete => Ok(Self::Complete),
            GoalStatus::Active
            | GoalStatus::Paused
            | GoalStatus::UsageLimited
            | GoalStatus::BudgetLimited
            | GoalStatus::Cancelled => Err(GoalError::StatusNotModelOwned {
                requested: status,
                allowed: Self::rendered_values(),
            }),
        }
    }

    /// The model-writable status strings, for a refusal message.
    #[must_use]
    pub fn rendered_values() -> String {
        render(Self::ALL.iter().map(|status| status.as_str()))
    }
}

impl From<ModelStatus> for GoalStatus {
    fn from(status: ModelStatus) -> Self {
        status.into_status()
    }
}

impl std::fmt::Display for ModelStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A status only the runtime or the user may write.
///
/// Every variant is a decision about whether the run continues, made on
/// evidence the model does not hold: a user's intent, the account's provider
/// quota, or the budget the user set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemStatus {
    /// Resume work.
    Active,
    /// The user stopped it.
    Paused,
    /// Provider usage ran out.
    UsageLimited,
    /// The token budget is spent.
    BudgetLimited,
    /// The user permanently abandoned this goal instance.
    Cancelled,
}

impl SystemStatus {
    /// Every status the system may write.
    pub const ALL: [Self; 5] = [
        Self::Active,
        Self::Paused,
        Self::UsageLimited,
        Self::BudgetLimited,
        Self::Cancelled,
    ];

    /// The stored string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into_status().as_str()
    }

    /// Widen to the stored type.
    #[must_use]
    pub fn into_status(self) -> GoalStatus {
        match self {
            Self::Active => GoalStatus::Active,
            Self::Paused => GoalStatus::Paused,
            Self::UsageLimited => GoalStatus::UsageLimited,
            Self::BudgetLimited => GoalStatus::BudgetLimited,
            Self::Cancelled => GoalStatus::Cancelled,
        }
    }

    /// Narrow a stored status to the system's scope.
    ///
    /// Returns `None` for the model-owned statuses. Used by the transition
    /// matrix to enumerate what each scope can reach.
    #[must_use]
    pub fn from_status(status: GoalStatus) -> Option<Self> {
        match status {
            GoalStatus::Active => Some(Self::Active),
            GoalStatus::Paused => Some(Self::Paused),
            GoalStatus::UsageLimited => Some(Self::UsageLimited),
            GoalStatus::BudgetLimited => Some(Self::BudgetLimited),
            GoalStatus::Cancelled => Some(Self::Cancelled),
            GoalStatus::Blocked | GoalStatus::Complete => None,
        }
    }
}

impl From<SystemStatus> for GoalStatus {
    fn from(status: SystemStatus) -> Self {
        status.into_status()
    }
}

impl std::fmt::Display for SystemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn render<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let quoted: Vec<String> = values.map(|value| format!("`{value}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_scopes_partition_the_six_statuses_with_no_overlap_and_no_gap() {
        let model: Vec<GoalStatus> = ModelStatus::ALL.map(ModelStatus::into_status).into();
        let system: Vec<GoalStatus> = SystemStatus::ALL.map(SystemStatus::into_status).into();
        let mut union = model.clone();
        union.extend(system.iter().copied());
        for status in GoalStatus::ALL {
            assert!(
                union.contains(&status),
                "{status} belongs to neither scope, so nothing can ever write it"
            );
            let owned_by_model = model.contains(&status);
            assert_ne!(
                owned_by_model,
                system.contains(&status),
                "{status} is claimed by both scopes or by neither"
            );
            assert_eq!(status.owner().is_model(), owned_by_model);
        }
        assert_eq!(union.len(), GoalStatus::ALL.len());
    }

    #[test]
    fn the_model_scope_is_exactly_blocked_and_complete() {
        assert_eq!(
            ModelStatus::ALL.map(ModelStatus::as_str),
            ["blocked", "complete"]
        );
    }

    #[test]
    fn every_status_string_round_trips_through_parse_and_serde() {
        for status in GoalStatus::ALL {
            assert_eq!(
                GoalStatus::parse(status.as_str()).expect("a status parses itself"),
                status
            );
            let json = serde_json::to_string(&status).expect("serialize status");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
    }

    #[test]
    fn a_status_outside_the_check_constraint_is_reported_as_unknown() {
        let error = GoalStatus::parse("finished").expect_err("`finished` is not a status");
        assert!(
            matches!(&error, GoalError::UnknownStatus { value, .. } if value == "finished"),
            "{error:?}"
        );
        assert!(error.to_string().contains("`budget_limited`"), "{error}");
    }

    #[test]
    fn the_terminal_set_excludes_the_two_statuses_that_are_expected_to_resume() {
        let terminal: Vec<&str> = GoalStatus::ALL
            .into_iter()
            .filter(|status| status.is_terminal())
            .map(GoalStatus::as_str)
            .collect();
        assert_eq!(terminal, ["budget_limited", "complete", "cancelled"]);
        let active: Vec<&str> = GoalStatus::ALL
            .into_iter()
            .filter(|status| status.is_active())
            .map(GoalStatus::as_str)
            .collect();
        assert_eq!(active, ["active"]);
    }

    #[test]
    fn the_refusal_for_a_system_owned_status_names_the_two_the_model_may_set() {
        for status in SystemStatus::ALL {
            let error = ModelStatus::parse(status.as_str())
                .expect_err("the model must not reach a system-owned status");
            let message = error.to_string();
            assert!(
                matches!(&error, GoalError::StatusNotModelOwned { requested, .. }
                    if *requested == status.into_status()),
                "{error:?}"
            );
            assert!(message.contains("`blocked` or `complete`"), "{message}");
            assert!(message.contains(status.as_str()), "{message}");
        }
    }

    #[test]
    fn an_unknown_status_from_the_model_is_refused_with_the_model_allowed_values() {
        let error = ModelStatus::parse("halted").expect_err("`halted` is not a status");
        assert!(
            matches!(&error, GoalError::UnknownStatus { value, .. } if value == "halted"),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "unknown goal status `halted`; expected `blocked` or `complete`"
        );
    }

    #[test]
    fn rendering_one_two_and_many_values_reads_as_a_sentence() {
        assert_eq!(render(["a"].into_iter()), "`a`");
        assert_eq!(render(["a", "b"].into_iter()), "`a` or `b`");
        assert_eq!(render(["a", "b", "c"].into_iter()), "`a`, `b` or `c`");
        assert_eq!(render(std::iter::empty()), "");
    }
}
