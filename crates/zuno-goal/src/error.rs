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
use crate::projection::clip_to;
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
    ///
    /// A reason that renders as nothing earns the same refusal. `"\u{200b}"` is not
    /// whitespace, so trimming keeps it, and it would land in the ledger as a waiver
    /// the human reviewing the goal cannot see — the display-versus-decision
    /// divergence this whole audit surface exists to remove.
    #[error(
        "goal criterion `{criterion_id}` cannot be waived without a reason that renders as \
         visible text"
    )]
    EmptyWaiverReason {
        /// The criterion the caller tried to waive.
        criterion_id: String,
    },

    /// A waiver reason was longer than the audit trail can usefully hold.
    ///
    /// The sibling of [`GoalError::SuccessCriterionTooLong`], and bounded for the same
    /// reasons on the same surface: the reason is model-written, lands in a durable
    /// column with no spill path, and is rendered straight back into the `goal_update`
    /// tool result and into the human-readable goal document. Unbounded, a single waiver
    /// of 2 000 000 characters made one tool result 2 000 434 bytes and re-inflated it on
    /// every later read. The cap matches the criterion statement it excuses, because a
    /// waiver that cannot be stated as briefly as the check it replaces is a decision
    /// that belongs in the objective or in a plan step.
    #[error(
        "the waiver reason for goal criterion `{criterion_id}` is {actual} characters, which \
         exceeds the {max}-character cap; state in one sentence why the check will not be \
         verified"
    )]
    WaiverReasonTooLong {
        /// The criterion the caller tried to waive.
        criterion_id: String,
        /// How long the reason came out, in characters, after trimming.
        actual: usize,
        /// The cap it had to fit inside.
        max: usize,
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

    /// A cited receipt was recorded before the goal it is offered to close existed.
    ///
    /// The mutation-mark rule in [`GoalError::EvidenceStale`] only refuses evidence
    /// older than the last *reported* write, and a mutation mark is cleared whenever the
    /// goal is replaced. So without this rule the checklist proves citation rather than
    /// verification: a receipt recorded before the goal was proposed describes a
    /// workspace the goal had not touched yet, and a receipt that the previous goal was
    /// refused for becomes usable again the moment a new goal resets the mark. Both
    /// timestamps are named so the model can see that the check ran before the promise
    /// it is offered as proof of.
    #[error(
        "goal criterion `{criterion_id}` cites receipt `{receipt_id}` recorded at \
         {receipt_at_ms}, which is before this goal was created at {goal_created_at_ms}; a check \
         that ran before the goal existed cannot prove the goal's own work — run the check again \
         and cite the new receipt"
    )]
    EvidencePredatesGoal {
        /// The criterion the caller tried to close.
        criterion_id: String,
        /// The receipt that belongs to an earlier goal, or to no goal at all.
        receipt_id: String,
        /// When the current goal instance was created, in Unix milliseconds.
        goal_created_at_ms: i64,
        /// When the receipt was recorded, in Unix milliseconds.
        receipt_at_ms: i64,
    },

    /// Completion was requested for a change goal whose criteria are unproven.
    ///
    /// An empty `unsatisfied` means the goal has no criteria at all, which no longer
    /// happens to a goal the model proposed: [`GoalStore::create_goal_as_model`]
    /// refuses a proposal that names no check, so this shape is a goal the *user*
    /// created with `/goal create` that a tool-reported write then escalated. The
    /// message therefore names the remedy — criteria are proposed with `goal_propose`
    /// at creation, not asserted at completion, and an unfinished goal cannot be
    /// re-proposed — so the model does not spend a turn discovering that no citation
    /// can help.
    ///
    /// The named-id form carries the remedy too, because both audiences reach it. A
    /// human running `/goal complete` on a goal the model proposed is refused by the
    /// same audit and, until the CLI grows a criterion verb, the only way forward is
    /// the run closing each criterion or the user cancelling the goal; a refusal that
    /// named the ids without saying that leaves them with a list and no next step.
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
    ///
    /// A half that renders as nothing is blank. `"\u{200b}"` survives trimming, so it
    /// used to be recorded and then named nothing in the
    /// [`GoalError::CapabilityUnverified`] refusal that blocks completion. Same predicate
    /// as the criterion statements, the waiver reasons and the objective: see
    /// [`crate::store::has_visible_character`].
    #[error("capability claim {field} must contain visible text")]
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

    /// The cited probe receipt was recorded before the current goal existed.
    ///
    /// The sibling of [`GoalError::EvidencePredatesGoal`], and it exists for the same
    /// reason: goal replacement clears the mutation mark, so a probe the previous goal
    /// was refused for would be accepted again under the next one. A claim recorded
    /// *before* any goal is untouched — the completion audit already ignores claims
    /// older than the goal — so this only refuses re-recording an old probe under a new
    /// goal, which is exactly the laundering step.
    #[error(
        "capability `{capability}` of `{subject}` cites probe receipt `{receipt_id}` recorded at \
         {receipt_at_ms}, which is before this goal was created at {goal_created_at_ms}; probe \
         again under the current goal and record the claim again"
    )]
    CapabilityProbePredatesGoal {
        /// The capability that was claimed.
        capability: String,
        /// What it was claimed about.
        subject: String,
        /// The receipt that belongs to an earlier goal, or to no goal at all.
        receipt_id: String,
        /// When the current goal instance was created, in Unix milliseconds.
        goal_created_at_ms: i64,
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

    /// An objective held no visible character.
    ///
    /// Ports the check at `codex-rs/tui/src/goal_files.rs:39-41`. A goal with no
    /// objective is a north star pointing nowhere; storing one would let the
    /// continuation loop run forever against nothing.
    ///
    /// An objective that renders as nothing is the same thing. `"\u{200b}\u{feff}"`
    /// survives trimming, so it used to be stored and then printed as a blank objective
    /// line in the goal document on every later turn. Same predicate as the criterion
    /// statements and the waiver reasons: see [`crate::store::has_visible_character`].
    #[error("goal objective must contain visible text")]
    EmptyObjective,

    /// A model-proposed goal named no check that could ever prove it done.
    ///
    /// The evidence audit reads the checklist, so a proposed goal with no criteria is
    /// one the audit cannot read at all: completion becomes an assertion, and no
    /// citation offered later can help, because criteria are immutable after
    /// creation. Refused at creation for that reason — it is the only moment the
    /// requirement can still be met, and it costs one corrected call instead of
    /// stranding a run that has already done the work.
    ///
    /// Entries that are blank after trimming are not criteria, so `["  "]` earns this
    /// refusal too rather than silently becoming an empty checklist.
    #[error(
        "a proposed goal must record at least one non-blank success criterion in \
         success_criteria, because a goal with no checklist cannot be proven complete and \
         criteria cannot be added afterwards; name the checks that decide whether the \
         objective is met, then close each one at completion by citing the receipt id \
         printed by the tool result that verified it or by stating why it will not be \
         verified"
    )]
    MissingSuccessCriteria,

    /// A model-proposed goal named more checks than one checklist can usefully hold.
    ///
    /// The list is model-supplied, mandatory, stored twice — as `goal_criterion` rows
    /// and as the `goal.success_criteria` JSON projection — and interpolated into the
    /// [`GoalError::EvidenceMissing`] refusal that goes back into the model's own
    /// context on every completion attempt. Unbounded, one bad proposal turns each
    /// later refusal into a multi-kilobyte prompt section nobody chose. The cap is a
    /// crate constant rather than a knob because it bounds a durable column and a
    /// model-visible message, not a deployment cost; nothing an operator could
    /// previously set was tighter, since there was no bound at all.
    ///
    /// Both counts are named because they are not the same number and only one of them
    /// was compared against the cap. Blank entries are dropped before the comparison, so
    /// a proposal of 40 entries with 5 blank ones is refused for the 35 that would
    /// record; a refusal that printed only one of the two numbers would leave the model
    /// deleting entries it never needed to touch.
    #[error(
        "a proposed goal may record at most {max} success criteria; this one sent {submitted} \
         entries, {recorded} of which record as criteria (blank entries are dropped); name the \
         checks that decide whether the objective is met, and track finer work with `plan_update`"
    )]
    TooManySuccessCriteria {
        /// How many entries the caller sent, blank ones included.
        submitted: usize,
        /// How many of them would record as criteria — the number compared against `max`.
        recorded: usize,
        /// The cap they had to fit inside.
        max: usize,
    },

    /// One success criterion was longer than the column contract allows.
    ///
    /// A criterion is a sentence a receipt can close, not a document. The same
    /// reasoning as [`GoalError::TooManySuccessCriteria`]: the statement lands in a
    /// durable column with no spill path and in a model-visible refusal.
    ///
    /// The ordinal counts the list the caller sent, blank entries included, because that
    /// is the list the model can edit. Counting the filtered list instead sent the model
    /// to the wrong element whenever an earlier entry was blank or invisible.
    #[error(
        "success criterion {ordinal} of the {submitted} entries sent is {actual} characters, \
         which exceeds the {max}-character cap for one criterion; state the check in one \
         sentence a single command can close"
    )]
    SuccessCriterionTooLong {
        /// Which criterion, counted from one over the entries the caller sent.
        ordinal: usize,
        /// How many entries the caller sent, so the ordinal cannot be read off the
        /// stored checklist by mistake.
        submitted: usize,
        /// How long it came out, in characters.
        actual: usize,
        /// The cap it had to fit inside.
        max: usize,
    },

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

/// How many items one model-visible refusal spells out before eliding.
///
/// Shared by [`GoalError::EvidenceMissing`] and [`GoalError::CapabilityUnverified`],
/// because they are the same surface: both are model-visible, both are durable-logged,
/// and both are re-rendered on every completion attempt, so neither one's size may be a
/// function of how many criteria a proposal happened to name or how many claims a
/// session happened to record. Ten is enough to work from — the run has to close them
/// one at a time anyway — and the count that follows keeps the message honest about the
/// rest. A goal stored by an earlier release with more criteria than
/// [`crate::store::MAX_SUCCESS_CRITERIA`], or a ledger with more claims than a run can
/// read, still reads and still completes; only the rendering is bounded.
const MAX_LISTED_ITEMS: usize = 10;

/// How much of one claim clause [`GoalError::CapabilityUnverified`] renders before
/// clipping.
///
/// [`MAX_LISTED_ITEMS`] alone does not bound this message the way it bounds
/// [`GoalError::EvidenceMissing`]: a criterion id is minted by this crate and is a
/// handful of characters, while a claim clause interpolates the model-written capability
/// and subject and, for a probe, the receipt id the model cited — none of which the
/// ledger bounds. Long enough that every reason this crate generates renders whole
/// beside a capability and subject of ordinary length, and short enough that ten of them
/// cannot exceed a few kilobytes.
const MAX_LISTED_CLAIM_CHARS: usize = 400;

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
            "these criteria are neither satisfied nor waived: {}; close each one with \
             `goal_update` — cite the receipt id printed by the tool result that verified it, \
             or give the reason it will not be verified — because completion cannot skip a \
             criterion the goal recorded",
            listed_ids(unsatisfied)
        )
    }
}

/// The ids as a refusal spells them, eliding everything past [`MAX_LISTED_ITEMS`].
///
/// Shared with [`GoalError::UnknownCriterion`]'s `known` list, which
/// `store::known_criteria` builds: it is the third criterion-id list on this
/// model-visible surface, and the bound that landed on two of the three would
/// otherwise have left the same 34 KB refusal reachable through the goal an earlier
/// release stored with an unbounded checklist. One function, so a fourth list cannot
/// be added unbounded either.
pub(crate) fn listed_ids(ids: &[String]) -> String {
    listed(ids, ids.len(), ", ")
}

/// `rendered` joined with `separator`, naming how many of `total` were left out.
///
/// One function for both refusals so the bound cannot be added to one list and left off
/// its sibling, which is exactly how [`GoalError::CapabilityUnverified`] came to render
/// half a megabyte while [`GoalError::EvidenceMissing`] next door was bounded.
///
/// `total` is separate from `rendered.len()` because a caller may only render the prefix
/// it will print: a claim clause interpolates model-written text of any length, so
/// building 2 000 of them to discard 1 990 is work proportional to the ledger inside the
/// one message whose whole purpose is to be bounded. The `take` stays here as well, so a
/// caller that hands over a longer slice still cannot widen the bound.
fn listed(rendered: &[String], total: usize, separator: &str) -> String {
    let listed = rendered
        .iter()
        .take(MAX_LISTED_ITEMS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(separator);
    match total.saturating_sub(MAX_LISTED_ITEMS) {
        0 => listed,
        elided => format!("{listed} and {elided} more"),
    }
}

/// How [`GoalError::CapabilityUnverified`] lists what blocks.
///
/// One clause per claim, each reading "`capability` of `subject` is recorded as …",
/// so the model can match every clause to a `capability_claim` call it has to make.
///
/// Bounded in both dimensions, because the ledger bounds neither: at most
/// [`MAX_LISTED_ITEMS`] clauses, each clipped to [`MAX_LISTED_CLAIM_CHARS`]. A session
/// that recorded 2 000 unverified claims used to render a 550 019-byte refusal into the
/// next model request; a single claim with a 2 000 000-character capability name would
/// have done it on its own.
fn unverified_detail(claims: &[UnverifiedCapability]) -> String {
    let clauses = claims
        .iter()
        .take(MAX_LISTED_ITEMS)
        .map(|claim| clip_to(&claim.to_string(), MAX_LISTED_CLAIM_CHARS))
        .collect::<Vec<_>>();
    listed(&clauses, claims.len(), "; ")
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
            | Self::WaiverReasonTooLong { .. }
            | Self::CriterionAlreadySatisfied { .. }
            | Self::EvidenceUnproven { .. }
            | Self::EvidenceStale { .. }
            | Self::EvidencePredatesGoal { .. }
            | Self::EvidenceMissing { .. }
            | Self::EmptyCapabilityClaimField { .. }
            | Self::CapabilityUndocumented { .. }
            | Self::CapabilityProbeUncited { .. }
            | Self::CapabilityProbeUnproven { .. }
            | Self::CapabilityProbeStale { .. }
            | Self::CapabilityProbePredatesGoal { .. }
            | Self::CapabilityUnverified { .. }
            | Self::MissingSuccessCriteria
            | Self::TooManySuccessCriteria { .. }
            | Self::SuccessCriterionTooLong { .. }
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
    fn a_proposal_with_no_checklist_is_told_that_criteria_cannot_be_added_later() {
        let error = GoalError::MissingSuccessCriteria;
        let message = error.to_string();
        assert!(
            message.contains("at least one non-blank success criterion in success_criteria"),
            "the refusal names the field and the shape it needs: {message}"
        );
        assert!(
            message.contains("criteria cannot be added afterwards"),
            "a model that expects to add them at completion would spend the whole run \
             before finding out: {message}"
        );
        assert!(
            error.is_model_refusal(),
            "this is a corrected call, not a harness failure, so it must not be retried \
             mechanically as `Failed`"
        );
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

    /// The refusal is model-visible and durably logged, and a goal an earlier release
    /// stored with an unbounded checklist still has to render. 5000 open criteria used
    /// to produce a 34 213-byte refusal, repeated on every completion attempt.
    #[test]
    fn the_unsatisfied_list_stays_short_however_long_a_stored_checklist_is() {
        let error = GoalError::EvidenceMissing {
            unsatisfied: (1..=5_000).map(|index| format!("c{index}")).collect(),
        };
        let message = error.to_string();
        assert!(
            message.len() < 400,
            "a refusal that is repeated into the model's context is bounded, not 34 KB: \
             {} bytes",
            message.len()
        );
        assert!(message.contains("c1, c2, c3"), "{message}");
        assert!(
            message.contains("c10 and 4990 more"),
            "the elision says how many were left out, so nothing looks like the whole list: \
             {message}"
        );
        assert!(
            !message.contains("c11"),
            "and the eleventh id is genuinely gone: {message}"
        );
        assert!(error.is_model_refusal());
    }

    /// The sibling of the test above, on the list that was left unbounded. A session with
    /// 2 000 unverified claims rendered a 550 019-byte refusal into the next model
    /// request, and one claim with a 2 000 000-character capability name would have done
    /// it alone, because the ledger bounds neither the number of claims nor the length of
    /// a name.
    #[test]
    fn the_unverified_claim_list_stays_short_however_large_a_ledger_is() {
        let error = GoalError::CapabilityUnverified {
            claims: (1..=2_000)
                .map(|index| UnverifiedCapability {
                    capability: format!("cap-{index}-{}", "x".repeat(500)),
                    subject: format!("subject-{index}-{}", "y".repeat(500)),
                    state: crate::CapabilityClaimState::Inferred,
                    reason: "is recorded as `inferred`".to_owned(),
                })
                .collect(),
        };
        let message = error.to_string();
        assert!(
            message.len() < 4_500,
            "a refusal that is repeated into the model's context is bounded, not 550 KB:              {} bytes",
            message.len()
        );
        assert!(message.contains("`cap-1-xxx"), "{message}");
        assert!(
            message.contains("and 1990 more"),
            "the elision says how many claims were left out: {message}"
        );
        assert!(
            !message.contains("cap-11-"),
            "and the eleventh claim is genuinely gone: {message}"
        );
        assert!(
            message.contains('…'),
            "each clause is clipped as well as counted, because a single name is unbounded              too: {message}"
        );
        assert!(
            message.contains("only `documented` or `probed` claims may be relied on"),
            "the remedy survives the clipping: {message}"
        );
        assert!(error.is_model_refusal());
    }

    /// A waiver reason is bounded like the criterion statement it excuses, and the
    /// refusal has to be self-correctable: it names the criterion, the length and the cap.
    #[test]
    fn an_oversized_waiver_reason_is_refused_by_naming_the_criterion_the_length_and_the_cap() {
        let error = GoalError::WaiverReasonTooLong {
            criterion_id: "c2".to_owned(),
            actual: 2_000_000,
            max: crate::store::MAX_WAIVER_REASON_CHARS,
        };
        let message = error.to_string();
        assert!(message.contains("`c2`"), "{message}");
        assert!(message.contains("2000000 characters"), "{message}");
        assert!(
            message.contains("500-character cap"),
            "the cap in force is named, so the model can correct in one call: {message}"
        );
        assert!(
            error.is_model_refusal(),
            "an oversized reason is a corrected call, not a harness failure"
        );
    }

    /// Both counts are the model's to act on, and they are not the same number: the cap
    /// is compared against the entries that would record, while the model edits the list
    /// it sent.
    #[test]
    fn an_oversized_checklist_names_what_was_sent_and_what_would_record() {
        let error = GoalError::TooManySuccessCriteria {
            submitted: 40,
            recorded: 35,
            max: crate::store::MAX_SUCCESS_CRITERIA,
        };
        let message = error.to_string();
        assert!(message.contains("sent 40 entries"), "{message}");
        assert!(
            message.contains("35 of which record as criteria"),
            "the number the cap was compared against is the one it names: {message}"
        );
        assert!(message.contains("at most 32"), "{message}");
        assert!(error.is_model_refusal());
    }

    /// The ordinal is a position in the list the model sent, so the refusal says so
    /// rather than leaving "criterion 2" to be counted off the stored checklist.
    #[test]
    fn an_oversized_criterion_is_refused_by_a_position_in_the_list_that_was_sent() {
        let error = GoalError::SuccessCriterionTooLong {
            ordinal: 4,
            submitted: 4,
            actual: 501,
            max: crate::store::MAX_CRITERION_STATEMENT_CHARS,
        };
        let message = error.to_string();
        assert!(
            message.contains("success criterion 4 of the 4 entries sent"),
            "{message}"
        );
        assert!(message.contains("501 characters"), "{message}");
        assert!(error.is_model_refusal());
    }

    /// A receipt from before the goal is a different mistake from a receipt from before
    /// the last edit, and only one of the two has "run it again" as its remedy.
    #[test]
    fn a_receipt_from_before_the_goal_is_refused_by_naming_both_times_and_the_way_out() {
        let error = GoalError::EvidencePredatesGoal {
            criterion_id: "c1".to_owned(),
            receipt_id: "rec_before".to_owned(),
            goal_created_at_ms: 5_000,
            receipt_at_ms: 2_000,
        };
        let message = error.to_string();
        assert!(message.contains("recorded at 2000"), "{message}");
        assert!(message.contains("created at 5000"), "{message}");
        assert!(
            message.contains("run the check again and cite the new receipt"),
            "{message}"
        );
        assert!(error.is_model_refusal());
    }
}
