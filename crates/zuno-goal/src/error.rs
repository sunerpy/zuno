//! Failures this crate reports, and why each one is its own variant.
//!
//! Two of these are model-visible: [`GoalError::StatusNotModelOwned`] and
//! [`GoalError::UnknownStatus`] are rendered straight into a tool result, so
//! their wording is a tested artifact rather than prose. Both name the values
//! that *would* have worked, because a refusal the model has to guess at costs a
//! whole extra turn.
//!
//! `zuno-error` deliberately gains nothing here. Four sibling crates depend on its
//! shape, and none of these failures is a database failure — a refused status is
//! a policy decision, not a broken statement. Database failures pass through
//! unchanged as [`GoalError::Db`].

use crate::status::GoalStatus;
use std::path::PathBuf;
use zuno_error::DbError;

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

    /// A retry reason in the auxiliary table is outside the closed runtime set.
    #[error("unknown goal retry reason `{value}`")]
    UnknownRetryReason {
        /// Corrupt stored discriminator.
        value: String,
    },

    /// A pause reason in the auxiliary table is outside the closed runtime set.
    #[error("unknown goal pause reason `{value}`")]
    UnknownPauseReason {
        /// Corrupt stored discriminator.
        value: String,
    },

    /// `create_goal` found a goal that is not replaceable yet.
    ///
    /// Distinct from a plain conflict because the remedy is specific: finish the
    /// current goal, or have the user replace it.
    #[error(
        "session {session_id} already has a goal with status `{status}`; \
         create_goal may replace a goal only once it is `complete` or `cancelled`"
    )]
    GoalNotReplaceable {
        /// The session whose goal blocked the replacement.
        session_id: String,
        /// The status that blocked it, read in the same transaction that refused.
        status: GoalStatus,
    },

    /// A Goal-only operation was requested for a session without a Goal.
    #[error("session {session_id} has no goal")]
    NoGoal {
        /// Session whose Goal was expected.
        session_id: String,
    },

    /// A Goal-only operation requires the current Goal to be active.
    #[error("session {session_id} goal is `{status}`; the operation requires `active`")]
    GoalNotActive {
        /// Session whose Goal is suspended or terminal.
        session_id: String,
        /// Current durable status.
        status: GoalStatus,
    },

    /// A writer used a stale optimistic-concurrency revision.
    #[error(
        "goal revision conflict for session {session_id}: expected {expected}, current {actual}"
    )]
    RevisionConflict {
        /// Session whose goal changed concurrently.
        session_id: String,
        /// Revision supplied by the writer.
        expected: i64,
        /// Revision stored when the guarded update ran.
        actual: i64,
    },

    /// Completion was requested while durable work still says the goal is unfinished.
    #[error(
        "goal cannot complete while {plan_steps} plan steps, {work_items} work items, {jobs} jobs, and {human_requests} human requests remain unfinished"
    )]
    CompletionBlocked {
        /// Plan steps not completed or cancelled.
        plan_steps: usize,
        /// Work items not completed or cancelled.
        work_items: usize,
        /// Jobs not completed or cancelled.
        jobs: usize,
        /// Human requests that have not reached a terminal response.
        human_requests: usize,
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

    /// The Markdown projection could not be read or written.
    ///
    /// Separate from [`GoalError::Spill`] because the two files answer different
    /// questions: a spill failure means an objective could not be *stored*, so
    /// the write must fail, while a projection failure means the human-readable
    /// copy is stale. SQL is still authoritative either way, which is why this
    /// variant exists to be reported rather than to be recovered from.
    #[error("goal document {operation} failed for {path}")]
    Document {
        /// What was being attempted: `read`, `write`, `rename`, `create directory`
        /// or `back up`.
        operation: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The I/O failure.
        #[source]
        source: std::io::Error,
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
            | Self::NoGoal { .. }
            | Self::GoalNotActive { .. }
            | Self::RevisionConflict { .. }
            | Self::CompletionBlocked { .. }
            | Self::EmptyObjective => true,
            Self::Db(_)
            | Self::UnknownRetryReason { .. }
            | Self::UnknownPauseReason { .. }
            | Self::Spill { .. }
            | Self::PointerTooLong { .. }
            | Self::Document { .. }
            | Self::Clock(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_replacement_names_the_blocking_status_and_the_terminal_statuses_that_work() {
        let error = GoalError::GoalNotReplaceable {
            session_id: "ses_abc".to_owned(),
            status: GoalStatus::Active,
        };
        assert_eq!(
            error.to_string(),
            "session ses_abc already has a goal with status `active`; \
             create_goal may replace a goal only once it is `complete` or `cancelled`"
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
