//! Failures this crate reports, and why each one is its own variant.
//!
//! Two of these are model-visible: [`GoalError::StatusNotModelOwned`] and
//! [`GoalError::UnknownStatus`] are rendered straight into a tool result, so
//! their wording is a tested artifact rather than prose. Both name the values
//! that *would* have worked, because a refusal the model has to guess at costs a
//! whole extra turn.
//!
//! `oc-error` deliberately gains nothing here. Four sibling crates depend on its
//! shape, and none of these failures is a database failure — a refused status is
//! a policy decision, not a broken statement. Database failures pass through
//! unchanged as [`GoalError::Db`].

use crate::status::GoalStatus;
use oc_error::DbError;
use std::path::PathBuf;

/// A goal store failure.
#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    /// The underlying SQLite operation failed. Retryability is
    /// [`DbError::is_retryable`]'s call, unchanged.
    #[error(transparent)]
    Db(#[from] DbError),

    /// The model asked for a status the system owns.
    ///
    /// The one refusal the split-ownership design exists to produce.
    #[error(
        "goal status `{requested}` is the system's to set, not the model's; \
         the model may set only {allowed}"
    )]
    StatusNotModelOwned {
        /// The status that was asked for.
        requested: GoalStatus,
        /// The statuses that would have been accepted.
        allowed: String,
    },

    /// A status string matched nothing.
    ///
    /// From the model path `expected` lists the model-writable statuses; from a
    /// row read-back it lists all six, and then this is corruption or a
    /// schema/code skew rather than input.
    #[error("unknown goal status `{value}`; expected {expected}")]
    UnknownStatus {
        /// The string that was supplied.
        value: String,
        /// The statuses that would have been accepted.
        expected: String,
    },

    /// `create_goal` found a goal that is not finished yet.
    ///
    /// Distinct from a plain conflict because the remedy is specific: finish the
    /// current goal, or have the user replace it.
    #[error(
        "session {session_id} already has a goal with status `{status}`; \
         create_goal may replace a goal only once it is `complete`"
    )]
    GoalNotReplaceable {
        /// The session whose goal blocked the replacement.
        session_id: String,
        /// The status that blocked it, read in the same transaction that refused.
        status: GoalStatus,
    },

    /// An objective was empty or only whitespace.
    ///
    /// Ports the check at `codex-rs/tui/src/goal_files.rs:39-41`. A goal with no
    /// objective is a north star pointing nowhere; storing one would let the
    /// continuation loop run forever against nothing.
    #[error("goal objective must not be empty")]
    EmptyObjective,

    /// An oversized objective could not be written to its spill file.
    #[error("goal objective could not be written to {path}")]
    Spill {
        /// The file that could not be written.
        path: PathBuf,
        /// The I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The pointer sentence for a spilled objective would itself exceed the cap.
    ///
    /// Ports `codex-rs/tui/src/goal_files.rs:174-183`. Only reachable with an
    /// absurdly deep spill directory, and failing is the honest answer: the
    /// alternative is storing an objective longer than the column's contract.
    #[error(
        "the pointer to the spilled goal objective at {path} is {actual} characters, \
         which exceeds the {max}-character objective cap"
    )]
    PointerTooLong {
        /// The spill file the pointer would have named.
        path: PathBuf,
        /// How long the pointer sentence came out.
        actual: usize,
        /// The cap it has to fit inside.
        max: usize,
    },

    /// The system clock is before the Unix epoch, so no timestamp can be taken.
    #[error("the system clock is before the Unix epoch")]
    Clock(#[from] std::time::SystemTimeError),
}

impl GoalError {
    /// Whether this failure is the model's to fix by asking again differently.
    ///
    /// The goal tool uses this to decide between returning a refusal the model
    /// can act on and surfacing an internal failure.
    #[must_use]
    pub fn is_model_refusal(&self) -> bool {
        match self {
            Self::StatusNotModelOwned { .. }
            | Self::UnknownStatus { .. }
            | Self::GoalNotReplaceable { .. }
            | Self::EmptyObjective => true,
            Self::Db(_) | Self::Spill { .. } | Self::PointerTooLong { .. } | Self::Clock(_) => {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_replacement_names_the_blocking_status_and_the_only_status_that_would_work() {
        let error = GoalError::GoalNotReplaceable {
            session_id: "ses_abc".to_owned(),
            status: GoalStatus::Active,
        };
        assert_eq!(
            error.to_string(),
            "session ses_abc already has a goal with status `active`; \
             create_goal may replace a goal only once it is `complete`"
        );
        assert!(error.is_model_refusal());
    }

    #[test]
    fn a_database_failure_is_not_presented_to_the_model_as_a_refusal() {
        let error = GoalError::Db(DbError::Busy { retry_after: None });
        assert!(!error.is_model_refusal());
        assert_eq!(
            error.to_string(),
            DbError::Busy { retry_after: None }.to_string(),
            "the transparent variant must not add a prefix of its own"
        );
    }
}
