//! Failures this crate reports, and why each one is its own variant.
//!
//! Two of these are model-visible: [`GoalError::StatusNotModelOwned`] and
//! [`GoalError::UnknownStatus`] are rendered straight into a tool result, so
//! their wording is a tested artifact rather than prose. Both name the values
//! that *would* have worked, because a refusal the model has to guess at costs a
//! whole extra turn.
//!
//! The evidence refusals are model-visible for the same reason:
//! [`GoalError::EvidenceMissing`], [`GoalError::EvidenceUnproven`] and
//! [`GoalError::EvidenceStale`] are the sentences that tell a model *which*
//! criterion is still unproven and *why* the receipt it cited does not count. A
//! completion refused without naming the criterion ids would leave the model
//! guessing, and guessing at "done" is the defect these variants exist to stop.
//! [`GoalError::PlanBelongsToAnotherGoal`] belongs to the same family: it names
//! both goal ids so a model can see that the plan it is looking at was written for
//! a goal that no longer exists, rather than being told only that completion was
//! refused.
//!
//! The capability refusals — [`GoalError::CapabilityUndocumented`],
//! [`GoalError::CapabilityProbeUncited`], [`GoalError::CapabilityProbeUnproven`],
//! [`GoalError::CapabilityProbeStale`] and [`GoalError::CapabilityUnverified`] — are
//! model-visible for the same reason again: each names the capability and subject
//! it is about and the state that would have been accepted, because the whole point
//! of the ledger is that a guess is told apart from an observation *before* it is
//! written into configuration, not discovered afterwards.
//!
//! `zuno-error` deliberately gains nothing here. Four sibling crates depend on its
//! shape, and none of these failures is a database failure — a refused status is
//! a policy decision, not a broken statement. Database failures pass through
//! unchanged as [`GoalError::Db`].

use crate::capability::UnverifiedCapability;
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

    /// A criterion status in the auxiliary table is outside the closed runtime set.
    #[error("unknown goal criterion status `{value}`")]
    UnknownCriterionStatus {
        /// Corrupt stored discriminator.
        value: String,
    },

    /// A goal kind in the auxiliary table is outside the closed runtime set.
    #[error("unknown goal kind `{value}`")]
    UnknownGoalKind {
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

    /// The session's visible plan was written for a different goal.
    ///
    /// `work_plan` is keyed by session, not by goal, so the plan of a finished goal
    /// stays visible after `goal_propose` replaces that goal — and `plan_update`
    /// inherits the old `goal_id` when a caller re-creates the plan without naming
    /// one. Every step of such a plan may already be `completed`, so the step count
    /// blocks nothing, yet it describes the previous goal's work. Completing over it
    /// would let a goal finish against a checklist nobody wrote for it. Both ids are
    /// named so the model can see the plan is stale rather than be told to try again.
    #[error(
        "the visible plan for session {session_id} belongs to goal {plan_goal_id}, not to goal \
         {goal_id} being completed; recreate the plan for this goal with `plan_update` \
         (action `create`, goal_id `{goal_id}`) before completing"
    )]
    PlanBelongsToAnotherGoal {
        /// The session whose plan was consulted.
        session_id: String,
        /// The goal the plan says it belongs to.
        plan_goal_id: String,
        /// The goal that was asked to complete.
        goal_id: String,
    },

    /// A criterion id was cited that this goal never assigned.
    ///
    /// Names the ids that *do* exist, because the ids are minted by the store and
    /// echoed once in the `goal_propose` result; a model that lost them has no
    /// other way back to them.
    #[error("goal criterion `{criterion_id}` does not exist for session {session_id}; {known}")]
    UnknownCriterion {
        /// The session whose criteria were consulted.
        session_id: String,
        /// The id that matched nothing.
        criterion_id: String,
        /// The ids that would have been accepted.
        known: String,
    },

    /// A waiver arrived without a reason.
    ///
    /// A waiver is the one way to close a criterion without evidence, so the
    /// reason is the entire audit trail. An empty one would make the escape hatch
    /// indistinguishable from the failure it exists to record.
    #[error("goal criterion `{criterion_id}` cannot be waived without a reason")]
    EmptyWaiverReason {
        /// The criterion the caller tried to waive.
        criterion_id: String,
    },

    /// A waiver was aimed at a criterion that already has evidence.
    ///
    /// A satisfied criterion needs no excuse, and a waiver landing on it would swap a
    /// recorded, re-checkable receipt for a judgement call nothing can re-check. The
    /// receipt is named so the model can see that the criterion is closed, not merely
    /// that its call was refused.
    #[error(
        "goal criterion `{criterion_id}` is already satisfied by receipt `{receipt_id}` and does \
         not need waiving; a waiver may only close a criterion that is open or already waived"
    )]
    CriterionAlreadySatisfied {
        /// The criterion the caller tried to waive.
        criterion_id: String,
        /// The receipt that already proves it.
        receipt_id: String,
    },

    /// A cited receipt does not prove the criterion.
    ///
    /// `reason` names which rule refused: no such receipt in this session, a
    /// failed or undecidable outcome, an exit status that was inferred rather than
    /// observed, or a criterion already settled by a waiver. At completion time it
    /// also names a receipt that proved the criterion once but has since been
    /// rewritten by a replayed call or removed by pruning — the audit re-reads every
    /// citation rather than trusting the row. Asserting success in prose is exactly
    /// what this refusal exists to stop.
    #[error("goal criterion `{criterion_id}` is not proven by receipt `{receipt_id}`: {reason}")]
    EvidenceUnproven {
        /// The criterion the caller tried to satisfy.
        criterion_id: String,
        /// The receipt that was cited.
        receipt_id: String,
        /// Which rule refused, in the words the model needs to act on.
        reason: String,
    },

    /// A cited receipt predates the last recorded change to the workspace.
    ///
    /// "The tests passed, then I edited three more files, so it is done" is the
    /// failure this variant refuses. Both timestamps are named so the model can
    /// see that the evidence is older than its own last edit rather than merely
    /// being told to try again.
    #[error(
        "goal criterion `{criterion_id}` cites receipt `{receipt_id}` recorded at \
         {receipt_at_ms}, which predates the workspace change recorded at {marked_at_ms}; \
         verify again after the last change and cite the new receipt"
    )]
    EvidenceStale {
        /// The criterion whose evidence went stale.
        criterion_id: String,
        /// The receipt that is now too old to count.
        receipt_id: String,
        /// When the workspace last changed, in Unix milliseconds.
        marked_at_ms: i64,
        /// When the receipt was recorded, in Unix milliseconds.
        receipt_at_ms: i64,
    },

    /// Completion was requested for a change goal whose criteria are unproven.
    ///
    /// An empty `unsatisfied` means the goal has no criteria at all. That is a
    /// deliberate outcome rather than an oversight: a goal proposed without criteria
    /// is accepted as a question, because questions need none, and the first write
    /// to the workspace makes it a change goal with nothing for evidence to attach
    /// to. The message therefore names the remedy — criteria are proposed with
    /// `goal_propose`, not asserted at completion — so the model does not spend a
    /// turn discovering that no citation can help.
    #[error(
        "goal cannot complete without recorded verification evidence: {}",
        unsatisfied_detail(unsatisfied)
    )]
    EvidenceMissing {
        /// The criterion ids that are neither satisfied nor waived.
        unsatisfied: Vec<String>,
    },

    /// A capability claim state in the ledger is outside the closed runtime set.
    #[error("unknown capability claim state `{value}`")]
    UnknownCapabilityClaimState {
        /// Corrupt stored discriminator.
        value: String,
    },

    /// A capability claim named no capability, or nothing to claim it about.
    ///
    /// A claim is the sentence "`subject` has `capability`"; with either half blank
    /// there is nothing for a state to be the provenance of, and an anonymous row
    /// would satisfy the letter of "record the claim" while recording nothing.
    #[error("capability claim {field} must not be empty")]
    EmptyCapabilityClaimField {
        /// Which half was blank: `capability` or `subject`.
        field: &'static str,
    },

    /// A claim was called `documented` without citing anything.
    ///
    /// A claim with no citation is not documentation. This is the refusal that stops
    /// "the docs say so" from being recorded as if a document had been read.
    #[error(
        "capability `{capability}` of `{subject}` cannot be recorded as documented without at \
         least one source naming the document (a URL, a title or a file path); with nothing \
         to cite, record it as `inferred`"
    )]
    CapabilityUndocumented {
        /// The capability that was claimed.
        capability: String,
        /// What it was claimed about.
        subject: String,
    },

    /// A claim was called `probed` without citing the probe's receipt.
    ///
    /// A probe whose response nobody can point at was not observed; it is a guess
    /// with a request attached, and the honest state for that is `inferred`.
    #[error(
        "capability `{capability}` of `{subject}` cannot be recorded as probed without citing \
         the receipt id printed by the tool result whose request exercised the capability; \
         without one, record it as `inferred`"
    )]
    CapabilityProbeUncited {
        /// The capability that was claimed.
        capability: String,
        /// What it was claimed about.
        subject: String,
    },

    /// The cited probe receipt does not prove the probe was observed to succeed.
    ///
    /// `reason` names which rule refused, in the same vocabulary as
    /// [`GoalError::EvidenceUnproven`]: no such receipt in this session, a failed or
    /// undecidable outcome, or an exit status that was inferred rather than observed.
    #[error(
        "capability `{capability}` of `{subject}` is not proven by probe receipt `{receipt_id}`: \
         {reason}"
    )]
    CapabilityProbeUnproven {
        /// The capability that was claimed.
        capability: String,
        /// What it was claimed about.
        subject: String,
        /// The receipt that was cited.
        receipt_id: String,
        /// Which rule refused, in the words the model needs to act on.
        reason: String,
    },

    /// The cited probe receipt predates the last recorded change to the workspace.
    ///
    /// The same rule as [`GoalError::EvidenceStale`], for the same reason: a probe
    /// made before the configuration was written says nothing about the configuration
    /// that exists now. Both timestamps are named so the model can see the order.
    #[error(
        "capability `{capability}` of `{subject}` cites probe receipt `{receipt_id}` recorded at \
         {receipt_at_ms}, which predates the workspace change recorded at {marked_at_ms}; probe \
         again after the last change and record the claim again"
    )]
    CapabilityProbeStale {
        /// The capability that was claimed.
        capability: String,
        /// What it was claimed about.
        subject: String,
        /// The receipt that is now too old to count.
        receipt_id: String,
        /// When the workspace last changed, in Unix milliseconds.
        marked_at_ms: i64,
        /// When the receipt was recorded, in Unix milliseconds.
        receipt_at_ms: i64,
    },

    /// Completion was requested while the goal rests on capability claims that were
    /// never verified.
    ///
    /// Every claim is listed with the capability, the subject and why it does not
    /// count, because the remedy is per claim: cite a document for *this* subject, or
    /// probe *this* subject again after the last change. A refusal that said only
    /// "unverified capability" would send the model back to guessing which one.
    #[error(
        "goal cannot complete while it relies on capability claims that were never verified: {}; \
         only `documented` or `probed` claims may be relied on — cite a vendor document for this \
         exact subject or record an observed probe with `capability_claim`",
        unverified_detail(claims)
    )]
    CapabilityUnverified {
        /// The claims that block, in the order they were recorded.
        claims: Vec<UnverifiedCapability>,
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

/// How [`GoalError::EvidenceMissing`] describes what is still unproven.
///
/// Two sentences rather than one because the two cases have different remedies:
/// named ids mean "verify these and cite the receipts", while an empty list means
/// "this goal has no criteria to verify at all".
fn unsatisfied_detail(unsatisfied: &[String]) -> String {
    if unsatisfied.is_empty() {
        "a goal that changes the workspace cannot complete without success criteria; propose \
         success criteria with `goal_propose` before completing (an unfinished goal cannot be \
         re-proposed, so this one has to be cancelled by the user first)"
            .to_owned()
    } else {
        format!(
            "these criteria are neither satisfied nor waived: {}",
            unsatisfied.join(", ")
        )
    }
}

/// How [`GoalError::CapabilityUnverified`] lists what blocks.
///
/// One clause per claim, each reading "`capability` of `subject` is recorded as …",
/// so the model can match every clause to a `capability_claim` call it has to make.
fn unverified_detail(claims: &[UnverifiedCapability]) -> String {
    claims
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
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
            | Self::PlanBelongsToAnotherGoal { .. }
            | Self::UnknownCriterion { .. }
            | Self::EmptyWaiverReason { .. }
            | Self::CriterionAlreadySatisfied { .. }
            | Self::EvidenceUnproven { .. }
            | Self::EvidenceStale { .. }
            | Self::EvidenceMissing { .. }
            | Self::EmptyCapabilityClaimField { .. }
            | Self::CapabilityUndocumented { .. }
            | Self::CapabilityProbeUncited { .. }
            | Self::CapabilityProbeUnproven { .. }
            | Self::CapabilityProbeStale { .. }
            | Self::CapabilityUnverified { .. }
            | Self::EmptyObjective => true,
            Self::Db(_)
            | Self::UnknownRetryReason { .. }
            | Self::UnknownPauseReason { .. }
            | Self::UnknownCriterionStatus { .. }
            | Self::UnknownGoalKind { .. }
            | Self::UnknownCapabilityClaimState { .. }
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

    #[test]
    fn a_stale_plan_refusal_names_both_goals_and_the_tool_that_rebinds_the_plan() {
        let error = GoalError::PlanBelongsToAnotherGoal {
            session_id: "ses_abc".to_owned(),
            plan_goal_id: "goal_old".to_owned(),
            goal_id: "goal_new".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "the visible plan for session ses_abc belongs to goal goal_old, not to goal goal_new \
             being completed; recreate the plan for this goal with `plan_update` (action \
             `create`, goal_id `goal_new`) before completing"
        );
        assert!(error.is_model_refusal());
    }

    #[test]
    fn an_unverified_capability_refusal_names_every_claim_and_the_two_states_that_count() {
        let error = GoalError::CapabilityUnverified {
            claims: vec![
                UnverifiedCapability {
                    capability: "bedrock:converse:structured_output".to_owned(),
                    subject: "vendor.model-a-v1:0".to_owned(),
                    state: crate::CapabilityClaimState::Inferred,
                    reason: "is recorded as `inferred`".to_owned(),
                },
                UnverifiedCapability {
                    capability: "bedrock:converse:tool_use".to_owned(),
                    subject: "vendor.model-a-v1:0".to_owned(),
                    state: crate::CapabilityClaimState::Unknown,
                    reason: "is recorded as `unknown`".to_owned(),
                },
            ],
        };
        assert_eq!(
            error.to_string(),
            "goal cannot complete while it relies on capability claims that were never \
             verified: `bedrock:converse:structured_output` of `vendor.model-a-v1:0` is recorded \
             as `inferred`; `bedrock:converse:tool_use` of `vendor.model-a-v1:0` is recorded as \
             `unknown`; only `documented` or `probed` claims may be relied on — cite a vendor \
             document for this exact subject or record an observed probe with `capability_claim`"
        );
        assert!(error.is_model_refusal());
    }

    #[test]
    fn waiving_a_satisfied_criterion_is_refused_by_naming_the_receipt_that_already_proves_it() {
        let error = GoalError::CriterionAlreadySatisfied {
            criterion_id: "c1".to_owned(),
            receipt_id: "rec_pass".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "goal criterion `c1` is already satisfied by receipt `rec_pass` and does not need \
             waiving; a waiver may only close a criterion that is open or already waived"
        );
        assert!(error.is_model_refusal());
    }

    #[test]
    fn a_change_goal_with_no_criteria_is_told_to_propose_them_rather_than_which_id_is_open() {
        let error = GoalError::EvidenceMissing {
            unsatisfied: Vec::new(),
        };
        let message = error.to_string();
        assert!(
            message.contains("propose success criteria with `goal_propose` before completing"),
            "{message}"
        );
        assert!(
            !message.contains("neither satisfied nor waived"),
            "an empty checklist has no ids to list: {message}"
        );
        assert!(error.is_model_refusal());
    }
}
