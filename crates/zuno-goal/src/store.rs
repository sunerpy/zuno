//! The `goal` table: one goal per session, and every write that touches it.
//!
//! # Shared durable ownership
//!
//! Runtime goals share the main `zuno.db` pool with sessions, todos and jobs so
//! all work-state projections have one process lifecycle and one backup boundary.
//! There is deliberately no cascading foreign key: deleting conversation history
//! must not erase an explicit long-running goal before the user reviews it.
//!
//! # Split ownership, and where it is enforced
//!
//! Two write paths, and they do not share a status type — see
//! [`crate::status`]. [`GoalStore::update_status_as_model`] takes a
//! [`ModelStatus`], which has no `paused` variant to pass;
//! [`GoalStore::set_status_as_system`] takes a [`SystemStatus`]. On top of that,
//! both statements re-derive the budget limit in SQL, so even a *system*
//! reactivation of an over-budget goal lands back on `budget_limited`. The
//! type system stops the model from asking; the SQL stops anyone from
//! succeeding.
//!
//! # What is deliberately in SQL and not in Rust
//!
//! Two invariants, both because a Rust-side check would be a race:
//!
//! * **The budget flip.** Every statement that can move `tokens_used` or
//!   `token_budget` carries the `CASE` that flips `active` to `budget_limited`,
//!   so the flip happens in the same statement as the increment and there is no
//!   window where the counters are over budget and the status is not. Ports
//!   `status_after_budget_limit` (`codex-rs/state/src/runtime/goals.rs:618-630`)
//!   and the accounting `CASE` (`goals.rs:546-566`).
//! * **The evidence gate.** A criterion is satisfied only by a statement that
//!   reads the receipt, the mutation mark and the goal revision in the same
//!   transaction that flips the row, so a receipt cannot be recorded, or the
//!   workspace changed, between the check and the write. See
//!   [`GoalStore::satisfy_criterion`].
//! * **The guarded replace.** [`GoalStore::create_goal`] is one upsert whose
//!   `DO UPDATE` carries a terminal-status guard, exactly as
//!   `insert_thread_goal` does (`goals.rs:245`). A read-then-write would let two
//!   concurrent `create_goal` calls both observe `complete` and both replace.
//!   The refusal is the statement returning no row.
//!
//! # Why "done" needs a receipt
//!
//! A model can always *say* the work is verified. Before the criterion tables
//! existed, [`GoalStore::complete_checked`] audited durable work — plan steps,
//! work items, jobs, human requests — and then took the model's word for the rest,
//! so a session that never ran the tests, or ran them before its last edit,
//! completed exactly as one that did.
//!
//! Three rules close that gap, and each of them is a rule about *time* rather than
//! about narration. A criterion is satisfied only by a
//! [`zuno_db::verification::VerificationReceipt`] that
//! [`zuno_db::verification::VerificationReceipt::proves_success`] accepts, so a
//! failed run and an inferred exit status count for nothing. A receipt older than
//! the last [`GoalStore::mark_mutation`] is refused, and a criterion already
//! satisfied by one is reopened, so editing files after a green test run undoes the
//! evidence rather than the other way round. And a goal that
//! [`GoalStore::escalate_to_change`] marked as changing the workspace cannot
//! complete while any criterion is still open — including the case of having no
//! criteria at all, which is assertion with extra steps.
//!
//! A [`GoalKind::Question`] goal is untouched by all three. Answering a question
//! leaves nothing behind to verify, and demanding a receipt for it would only teach
//! the model to manufacture one.
//!
//! Two consequences of that split are decisions, not gaps. A goal proposed with no
//! criteria at all is accepted, because a question needs none, and it stays a
//! question until the first write escalates it; from then on it can never complete,
//! and the refusal says to propose criteria with `goal_propose` rather than
//! inventing them from the objective — a checklist the store guessed would be a
//! checklist nobody committed to. And the plan a session can see must belong to the
//! goal being completed: `work_plan` is keyed by session, so a plan bound to an
//! earlier goal survives replacement with every step already `completed`, and
//! without an ownership check a new goal would complete against the previous
//! goal's finished work. See [`GoalStore::complete_checked`].
//!
//! Two further rules keep the gate a gate. The audit re-reads every cited receipt
//! when completion is requested, rather than trusting the criterion row, because
//! the receipt ledger can be rewritten by a replayed call or emptied by pruning;
//! and `complete` reaches the audit from every entry point, including
//! [`GoalStore::update_status_as_model`], so no caller can finish a change goal by
//! choosing the unguarded writer. A satisfied criterion also cannot be waived: a
//! waiver may excuse a check that was never made, never replace one that was.
//!
//! A fourth rule is about reliance rather than verification. A session that enables
//! a provider feature because a *related* model is documented to have it has made a
//! claim it never observed, and the configuration it wrote looks the same as one
//! written on evidence. [`crate::capability`] records every such claim with its
//! provenance, and a change goal cannot complete while a claim recorded under it is
//! `inferred` or `unknown`, or rests on a probe that a later write retired.

use crate::error::GoalError;
use crate::pause::{GoalPauseReason, GoalPauseState};
use crate::retry::{GoalBlockReason, GoalRetryPolicy, GoalRetryReason, GoalRetryState};
use crate::spill;
use crate::status::{GoalStatus, ModelStatus, SystemStatus};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_db::Pool;
use zuno_db::human_request::{
    HumanRequest, HumanRequestKind, HumanRequestState, NewHumanRequest,
    create_in as create_request_in,
};
use zuno_db::verification::{ReceiptOutcome, VerificationReceipt};
use zuno_error::DbError;
use zuno_paths::DbLocation;

/// The table this module owns.
pub const TABLE: &str = "goal";

/// The directory under [`zuno_paths::data`] that oversized objectives spill into.
pub const OBJECTIVE_SPILL_DIRECTORY: &str = "goal-objective";

/// Durable coordinates of the tool call that requested human input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalHumanRequestOrigin {
    pub message_id: Option<String>,
    pub call_id: Option<String>,
}

/// The table, verbatim.
///
/// A port of `codex-rs/state/goals_migrations/0001_thread_goals.sql` with
/// `thread_goals` renamed to `goal` and `thread_id` to `session_id`; every other
/// column name, the `CHECK` members, the defaults and the integer-Unix-
/// milliseconds convention are unchanged.
///
/// The table is initialized inside the application-owned `zuno.db` pool. Goal,
/// plan, todo, job, and session state therefore share one transaction boundary.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS goal (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    objective TEXT NOT NULL,
    success_criteria TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete',
        'cancelled'
    )),
    blocked_reason TEXT,
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    usage_known INTEGER NOT NULL DEFAULT 1 CHECK(usage_known IN (0, 1)),
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
)";

/// Runtime-only state kept beside the stable [`TABLE`] schema in the same pool.
///
/// Neither the goal nor its auxiliary tables cascade on session deletion. Goal
/// replacement clears only transient continuation state; history remains durable.
///
/// # Why the evidence tables are here and not in `zuno-db`
///
/// `goal_criterion`, `goal_kind`, `goal_mutation_mark`, `goal_request_usage` and
/// `goal_capability_claim` are additive `CREATE TABLE IF NOT EXISTS` statements in
/// this batch, outside the database format marker `zuno_db::migration` maintains.
/// That is deliberate: they carry *goal* policy rather than application data, they
/// are created by whichever process attaches a [`GoalStore`], and a database that
/// predates them is not stale — it simply has no criteria, and no claims, yet.
/// Putting them behind the format marker would make a goal-policy change a
/// whole-database migration.
///
/// `goal_capability_claim` is session-scoped rather than goal-scoped: it is
/// provenance for what a session relied on, and [`crate::capability`] explains why
/// goal replacement leaves it alone.
pub const AUXILIARY_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS goal_criterion (
    session_id TEXT NOT NULL,
    criterion_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    statement TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('open', 'satisfied', 'waived')),
    waiver_reason TEXT,
    receipt_id TEXT,
    satisfied_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(session_id, criterion_id)
);
CREATE TABLE IF NOT EXISTS goal_kind (
    session_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('question', 'change')),
    reason TEXT,
    escalated_at_ms INTEGER
);
CREATE TABLE IF NOT EXISTS goal_mutation_mark (
    session_id TEXT PRIMARY KEY NOT NULL,
    marked_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_request_usage (
    session_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    tokens INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY(session_id, request_id)
);
CREATE TABLE IF NOT EXISTS goal_capability_claim (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    subject TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('documented', 'probed', 'inferred', 'unknown')),
    sources TEXT NOT NULL,
    probe_receipt_id TEXT,
    time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL,
    UNIQUE(session_id, capability, subject)
);
CREATE TABLE IF NOT EXISTS goal_continuation_deferral (
    session_id TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_pending_failure_signal (
    session_id TEXT PRIMARY KEY NOT NULL,
    signal TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_failure_streak (
    session_id TEXT PRIMARY KEY NOT NULL,
    signal TEXT NOT NULL,
    consecutive_turns INTEGER NOT NULL CHECK(consecutive_turns BETWEEN 1 AND 3)
);
CREATE TABLE IF NOT EXISTS goal_pause (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN (
        'user_interruption',
        'plan_mode',
        'human_input',
        'permission',
        'authentication',
        'uncertain_side_effect',
        'turn_budget'
    )),
    human_request_id TEXT,
    paused_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_retry (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK(attempt >= 1),
    reason TEXT NOT NULL CHECK(reason IN (
        'rate_limited',
        'provider_transient',
        'provider_stream',
        'provider_retry_deadline',
        'database_busy',
        'step_limit',
        'empty_assistant_message',
        'context_limit',
        'context_compacted',
        'tool_transient'
    )),
    delay_ms INTEGER NOT NULL CHECK(delay_ms >= 0),
    retry_at_ms INTEGER NOT NULL,
    scheduled_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_history (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    objective TEXT NOT NULL,
    success_criteria TEXT NOT NULL,
    status TEXT NOT NULL,
    blocked_reason TEXT,
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL,
    usage_known INTEGER NOT NULL CHECK(usage_known IN (0, 1)),
    time_used_seconds INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(goal_id, revision)
);
CREATE INDEX IF NOT EXISTS goal_history_session_sequence
    ON goal_history(session_id, sequence);
CREATE TRIGGER IF NOT EXISTS goal_history_after_insert
AFTER INSERT ON goal
BEGIN
    INSERT INTO goal_history (
        session_id, goal_id, revision, objective, success_criteria, status, blocked_reason,
        token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, updated_at_ms
    ) VALUES (
        NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
        NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
        NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds, NEW.created_at_ms,
        NEW.updated_at_ms
    );
END;
CREATE TRIGGER IF NOT EXISTS goal_history_after_update
AFTER UPDATE ON goal
BEGIN
    INSERT INTO goal_history (
        session_id, goal_id, revision, objective, success_criteria, status, blocked_reason,
        token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, updated_at_ms
    ) VALUES (
        NEW.session_id, NEW.goal_id, NEW.revision, NEW.objective,
        NEW.success_criteria, NEW.status, NEW.blocked_reason, NEW.token_budget,
        NEW.tokens_used, NEW.usage_known, NEW.time_used_seconds, NEW.created_at_ms,
        NEW.updated_at_ms
    );
END;";

const COLUMNS: &str = "session_id, goal_id, revision, objective, success_criteria, status, \
     blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
     updated_at_ms";

/// One row of the `goal` table.
///
/// Field order follows the table so the two can be checked against each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// The session this goal belongs to, and the table's primary key.
    ///
    /// A plain string with no foreign key: a goal is keyed by a session id, not
    /// owned by a session row. See the module docs.
    pub session_id: String,
    /// Identifies this *instance* of the goal.
    ///
    /// Re-minted on every replacement, so a caller holding an id from before a
    /// replacement can tell that the goal it was working on is gone. codex uses
    /// it for exactly that (`goals.rs:583-585`).
    pub goal_id: String,
    /// Optimistic-concurrency revision for this goal instance.
    pub revision: i64,
    /// What the agent is working towards, or the pointer sentence that names the
    /// file holding it. Never longer than [`spill::MAX_OBJECTIVE_CHARS`].
    pub objective: String,
    /// Concrete checks that define when the objective is complete.
    pub success_criteria: Vec<String>,
    /// Whether, and why, the agent should keep going.
    pub status: GoalStatus,
    /// Stable explanation for a blocked goal.
    pub blocked_reason: Option<String>,
    /// The token ceiling, or `None` for unlimited.
    pub token_budget: Option<i64>,
    /// Tokens spent against this goal instance. Reset by a replacement.
    pub tokens_used: i64,
    /// Whether every token included in this goal's accounting is authoritative.
    ///
    /// `tokens_used` remains a confirmed lower bound when this is false.
    pub usage_known: bool,
    /// Wall-clock seconds spent against this goal instance.
    pub time_used_seconds: i64,
    /// When this goal instance was created, in Unix milliseconds.
    pub created_at_ms: i64,
    /// When it last changed, in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// One immutable snapshot from the durable goal revision log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalHistoryEntry {
    /// Monotonic order across all goal instances in this session.
    pub sequence: i64,
    /// Monotonic revision within one goal instance.
    pub revision: i64,
    /// The goal state recorded at this revision.
    pub goal: Goal,
}

/// The blocking condition observed at the end of consecutive goal turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureStreak {
    /// Stable, caller-supplied description used to recognize the same blocker.
    pub signal: String,
    /// Number of consecutive turns, saturated at the three-turn threshold.
    pub consecutive_turns: u32,
}

/// Whether a criterion is still open, proven, or explicitly excused.
///
/// Three states and not a boolean: a waiver is not a satisfaction, and collapsing
/// the two would hide the one case where a goal completed without evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCriterionStatus {
    /// No evidence has been cited for it yet.
    Open,
    /// A receipt that proves success has been cited for it.
    Satisfied,
    /// It was excused on the record, with a reason, instead of being proven.
    Waived,
}

impl GoalCriterionStatus {
    /// The stored representation, matching the column's `CHECK` members.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Satisfied => "satisfied",
            Self::Waived => "waived",
        }
    }

    /// Whether this status lets a change goal complete.
    ///
    /// Both settled states do: the point of the gate is that nothing is left
    /// undecided, not that nothing was ever excused.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Satisfied | Self::Waived)
    }

    /// Read a stored discriminator back.
    ///
    /// # Errors
    ///
    /// [`GoalError::UnknownCriterionStatus`] when the column holds a value outside
    /// the `CHECK` constraint, which is corruption or a schema/code skew rather
    /// than input.
    pub fn parse(value: &str) -> Result<Self, GoalError> {
        match value {
            "open" => Ok(Self::Open),
            "satisfied" => Ok(Self::Satisfied),
            "waived" => Ok(Self::Waived),
            other => Err(GoalError::UnknownCriterionStatus {
                value: other.to_owned(),
            }),
        }
    }
}

/// One success criterion with an identity, so it can be cited and audited.
///
/// The `goal.success_criteria` JSON column is still written exactly as before as a
/// compatibility projection; this row is the authority for identity and status.
/// Without an id there is nothing for a receipt to attach to, which is how an
/// opaque list of sentences ends up "verified" by assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCriterion {
    /// Stable short id assigned at creation: `c1`, `c2`, and so on.
    pub criterion_id: String,
    /// Position in the list the goal was created with, from one.
    pub ordinal: i64,
    /// The check itself, as the caller stated it.
    pub statement: String,
    /// Whether it is open, proven, or excused.
    pub status: GoalCriterionStatus,
    /// Why it was excused, when it was waived rather than proven.
    pub waiver_reason: Option<String>,
    /// The receipt cited as proof, when it is satisfied.
    pub receipt_id: Option<String>,
    /// When it became satisfied, in Unix milliseconds.
    pub satisfied_at_ms: Option<i64>,
}

/// Whether a goal only answers a question or also changes the workspace.
///
/// The distinction decides whether completion needs evidence at all. Absent an
/// explicit escalation a goal is a [`GoalKind::Question`], because the store
/// cannot know that a run touched the workspace until something tells it — see
/// [`GoalStore::escalate_to_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalKind {
    /// Nothing durable was changed, so there is nothing to verify.
    Question,
    /// The workspace was changed, so completion requires recorded evidence.
    Change,
}

impl GoalKind {
    /// The stored representation, matching the column's `CHECK` members.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Change => "change",
        }
    }

    /// Whether completion of this kind of goal requires recorded evidence.
    #[must_use]
    pub const fn requires_evidence(self) -> bool {
        matches!(self, Self::Change)
    }

    /// Read a stored discriminator back.
    ///
    /// # Errors
    ///
    /// [`GoalError::UnknownGoalKind`] when the column holds a value outside the
    /// `CHECK` constraint, which is corruption rather than input.
    pub fn parse(value: &str) -> Result<Self, GoalError> {
        match value {
            "question" => Ok(Self::Question),
            "change" => Ok(Self::Change),
            other => Err(GoalError::UnknownGoalKind {
                value: other.to_owned(),
            }),
        }
    }
}

/// A goal and the criterion ids its creation assigned.
///
/// Returned together because the ids are minted inside the creating transaction
/// and are the only handle a later citation has. A caller that had to read them
/// back separately could observe a replacement in between and echo ids that no
/// longer exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCreation {
    /// The goal as stored.
    pub goal: Goal,
    /// Its criteria, in creation order.
    pub criteria: Vec<GoalCriterion>,
}

/// What one criterion write left behind.
///
/// Carries the goal as well as the criterion because both changed: a criterion
/// write bumps the goal revision, so a caller that goes on to complete the goal in
/// the same tool call needs the revision this write produced rather than the one it
/// arrived with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionOutcome {
    /// The goal at the revision this write produced.
    pub goal: Goal,
    /// The criterion as it now stands.
    pub criterion: GoalCriterion,
}

/// What [`GoalStore::record_request_usage`] did with one request's tokens.
///
/// The flag is the point: a retried or replayed request must be recognisable as
/// already accounted, because charging it twice would end a turn that still had
/// allowance left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecorded {
    /// Whether this call was the first to account for the request id.
    pub accounted: bool,
    /// The goal after the write, or `None` when the session has no goal.
    pub goal: Option<Goal>,
}

impl Goal {
    /// Whether the budget is spent.
    ///
    /// Reads the counters rather than the status, so a caller can tell a genuine
    /// overrun from a `budget_limited` that a later budget raise has made stale.
    #[must_use]
    pub fn is_over_budget(&self) -> bool {
        self.token_budget
            .is_some_and(|budget| self.tokens_used >= budget)
    }

    /// Tokens left before the budget flips the status, if there is a budget.
    #[must_use]
    pub fn tokens_remaining(&self) -> Option<i64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.tokens_used).max(0))
    }
}

/// The `goal` table over an application-owned SQLite pool.
///
/// Cheap to clone-by-reference and safe to share: the pool serializes
/// connections and every write runs in one `IMMEDIATE` transaction.
#[derive(Debug)]
pub struct GoalStore {
    pool: Arc<Pool>,
    spill_dir: PathBuf,
}

impl GoalStore {
    /// Open the goal database at `path`, spilling objectives into `spill_dir`.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the database cannot be opened, a pragma did not
    /// take effect, or the table cannot be created.
    pub fn open_at(path: &Path, spill_dir: PathBuf) -> Result<Self, GoalError> {
        Self::open(&DbLocation::File(path.to_path_buf()), spill_dir)
    }

    /// Open a private in-memory goal database.
    ///
    /// For tests and for a run that deliberately keeps no goal across restarts.
    /// `zuno-db`'s pool gives this a named shared-cache database with a retained
    /// anchor connection, so several pooled connections see one database.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the database cannot be opened or the table cannot
    /// be created.
    pub fn open_memory(spill_dir: PathBuf) -> Result<Self, GoalError> {
        Self::open(&DbLocation::Memory, spill_dir)
    }

    fn open(location: &DbLocation, spill_dir: PathBuf) -> Result<Self, GoalError> {
        let pool = Arc::new(Pool::open(location)?);
        {
            let mut connection = pool.get()?;
            zuno_db::migration::apply(&mut connection)?;
        }
        Self::from_pool(pool, spill_dir)
    }

    /// Attach goal state to an existing application database pool.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the goal tables cannot be initialized.
    pub fn from_pool(pool: Arc<Pool>, spill_dir: PathBuf) -> Result<Self, GoalError> {
        pool.transaction(|tx| {
            tx.execute_batch(SCHEMA).map_err(zuno_db::map_error)?;
            tx.execute_batch(AUXILIARY_SCHEMA)
                .map_err(zuno_db::map_error)?;
            widen_pause_reasons(tx)
        })?;
        Ok(Self { pool, spill_dir })
    }

    /// The pool this store writes through.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        self.pool.as_ref()
    }

    /// Where oversized objectives spill to.
    #[must_use]
    pub fn spill_dir(&self) -> &Path {
        &self.spill_dir
    }

    /// The file a spilled objective lives in, if `objective` is a pointer.
    ///
    /// The reader for [`Goal::objective`] when it turned out to be a pointer
    /// sentence. Validating, not merely parsing — see
    /// [`spill::objective_pointer_path`].
    #[must_use]
    pub fn objective_file(&self, objective: &str) -> Option<PathBuf> {
        spill::objective_pointer_path(&self.spill_dir, objective)
    }

    /// The goal for `session_id`, if it has one.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure and
    /// [`GoalError::UnknownStatus`] when the stored status is outside the
    /// `CHECK` constraint, which is corruption.
    pub fn goal(&self, session_id: &str) -> Result<Option<Goal>, GoalError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM {TABLE} WHERE session_id = ?1"
            ))
            .map_err(zuno_db::map_error)?;
        read_optional(&mut statement, params![session_id])
    }

    /// Durable reason attached to a paused Goal, if one is recorded.
    pub fn pause_state(&self, session_id: &str) -> Result<Option<GoalPauseState>, GoalError> {
        let connection = self.pool.get()?;
        pause_state_from(&connection, session_id)
    }

    /// Shared durable human-request store over the same application pool.
    #[must_use]
    pub fn human_requests(&self) -> zuno_db::human_request::HumanRequestStore {
        zuno_db::human_request::HumanRequestStore::new(Arc::clone(&self.pool))
    }

    /// Pause the current Goal with a typed reason.
    ///
    /// The Goal row and its reason commit in one transaction. A non-paused
    /// terminal state is never relabelled as paused by this helper.
    pub fn pause_with_reason(
        &self,
        session_id: &str,
        reason: GoalPauseReason,
    ) -> Result<Option<Goal>, GoalError> {
        self.pause_with_reason_checked(session_id, reason, None, None)
    }

    /// Pause only when an expected Goal revision is still current.
    pub fn pause_with_reason_at_revision(
        &self,
        session_id: &str,
        reason: GoalPauseReason,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        self.pause_with_reason_checked(session_id, reason, None, Some(expected_revision))
    }

    /// Persist a pending human request and suspend the exact active Goal revision.
    ///
    /// Insertion of the request, the `paused` transition, and the typed pause
    /// record are atomic. The caller may safely return a waiting outcome only
    /// after this method succeeds.
    pub fn request_human_input(
        &self,
        session_id: &str,
        expected_revision: i64,
        request_id: String,
        payload: serde_json::Value,
        message_id: Option<String>,
        call_id: Option<String>,
    ) -> Result<HumanRequest, GoalError> {
        self.request_human_input_at(
            session_id,
            expected_revision,
            request_id,
            payload,
            GoalHumanRequestOrigin {
                message_id,
                call_id,
            },
            now_ms()?,
        )
    }

    /// Deterministic-clock form of [`Self::request_human_input`].
    pub fn request_human_input_at(
        &self,
        session_id: &str,
        expected_revision: i64,
        request_id: String,
        payload: serde_json::Value,
        origin: GoalHumanRequestOrigin,
        paused_at_ms: i64,
    ) -> Result<HumanRequest, GoalError> {
        self.pool.try_transaction(|tx| {
            let goal = goal_from_transaction(tx, session_id)?.ok_or_else(|| GoalError::NoGoal {
                session_id: session_id.to_owned(),
            })?;
            if goal.revision != expected_revision {
                return Err(GoalError::RevisionConflict {
                    session_id: session_id.to_owned(),
                    expected: expected_revision,
                    actual: goal.revision,
                });
            }
            if goal.status != GoalStatus::Active {
                return Err(GoalError::GoalNotActive {
                    session_id: session_id.to_owned(),
                    status: goal.status,
                });
            }

            let request = create_request_in(
                tx,
                &NewHumanRequest {
                    id: request_id,
                    session_id: session_id.to_owned(),
                    goal_id: Some(goal.goal_id.clone()),
                    kind: HumanRequestKind::Input,
                    payload,
                    message_id: origin.message_id,
                    call_id: origin.call_id,
                    time_created: paused_at_ms,
                },
            )?;
            let paused = update_system_status_in(
                tx,
                session_id,
                SystemStatus::Paused,
                Some(expected_revision),
                paused_at_ms,
            )?
            .ok_or_else(|| GoalError::RevisionConflict {
                session_id: session_id.to_owned(),
                expected: expected_revision,
                actual: goal.revision,
            })?;
            upsert_pause_in(
                tx,
                &paused,
                GoalPauseReason::HumanInput,
                Some(&request.id),
                paused_at_ms,
            )?;
            clear_failure_and_retry_state(tx, session_id)?;
            Ok(request)
        })
    }

    /// Persist a permission request and pause an active Goal before the gated
    /// effect can run.
    ///
    /// Returns `None` when the session has no active Goal; callers may then use
    /// the generic human-request store without changing Goal state.
    pub fn request_permission(
        &self,
        session_id: &str,
        request_id: String,
        payload: serde_json::Value,
        message_id: Option<String>,
        call_id: Option<String>,
    ) -> Result<Option<HumanRequest>, GoalError> {
        let paused_at_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let Some(goal) = goal_from_transaction(tx, session_id)? else {
                return Ok(None);
            };
            if goal.status != GoalStatus::Active {
                return Ok(None);
            }
            let request = create_request_in(
                tx,
                &NewHumanRequest {
                    id: request_id,
                    session_id: session_id.to_owned(),
                    goal_id: Some(goal.goal_id.clone()),
                    kind: HumanRequestKind::Permission,
                    payload,
                    message_id,
                    call_id,
                    time_created: paused_at_ms,
                },
            )?;
            let paused = update_system_status_in(
                tx,
                session_id,
                SystemStatus::Paused,
                Some(goal.revision),
                paused_at_ms,
            )?
            .expect("the active Goal revision was read in this transaction");
            upsert_pause_in(
                tx,
                &paused,
                GoalPauseReason::Permission,
                Some(&request.id),
                paused_at_ms,
            )?;
            clear_failure_and_retry_state(tx, session_id)?;
            Ok(Some(request))
        })
    }

    /// Enter Plan mode transactionally.
    ///
    /// An active Goal becomes `paused(plan_mode)`. Reopening or restarting in
    /// Plan mode is idempotent and does not advance the Goal revision again.
    /// A stronger pre-existing pause (human input, auth, uncertain side effect)
    /// is preserved.
    pub fn enter_plan_mode(&self, session_id: &str) -> Result<Option<Goal>, GoalError> {
        let paused_at_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let Some(goal) = goal_from_transaction(tx, session_id)? else {
                return Ok(None);
            };
            if goal.status != GoalStatus::Active {
                return Ok(Some(goal));
            }
            let paused = update_system_status_in(
                tx,
                session_id,
                SystemStatus::Paused,
                Some(goal.revision),
                paused_at_ms,
            )?
            .expect("the active Goal revision was read in this transaction");
            upsert_pause_in(tx, &paused, GoalPauseReason::PlanMode, None, paused_at_ms)?;
            clear_failure_and_retry_state(tx, session_id)?;
            Ok(Some(paused))
        })
    }

    /// Start Work by resuming only pauses that are now authoritatively settled.
    ///
    /// `plan_mode` resumes immediately. Human-input and permission pauses resume
    /// only after their durable request is answered. Authentication, manual
    /// interruption, and uncertain side effects remain paused for an explicit
    /// recovery action. The matching pause row is consumed in the same
    /// transaction, making repeated Start Work requests harmless.
    pub fn resume_for_work(&self, session_id: &str) -> Result<Option<Goal>, GoalError> {
        let resumed_at_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let Some(goal) = goal_from_transaction(tx, session_id)? else {
                return Ok(None);
            };
            if goal.status != GoalStatus::Paused {
                return Ok(Some(goal));
            }
            let Some(pause) = pause_state_from(tx, session_id)? else {
                return Ok(Some(goal));
            };
            if pause.goal_id != goal.goal_id {
                return Ok(Some(goal));
            }
            let resumable = match pause.reason {
                GoalPauseReason::PlanMode => true,
                reason if reason.waits_for_human_request() => {
                    let Some(request_id) = pause.human_request_id.as_deref() else {
                        return Ok(Some(goal));
                    };
                    matches!(
                        zuno_db::human_request::get_from(tx, request_id)?,
                        Some(HumanRequest {
                            state: HumanRequestState::Answered,
                            ..
                        })
                    )
                }
                GoalPauseReason::UserInterruption
                | GoalPauseReason::Authentication
                | GoalPauseReason::UncertainSideEffect
                | GoalPauseReason::TurnBudget => false,
                GoalPauseReason::HumanInput | GoalPauseReason::Permission => {
                    unreachable!("human request reasons handled by the guard")
                }
            };
            if !resumable {
                return Ok(Some(goal));
            }
            let resumed = update_system_status_in(
                tx,
                session_id,
                SystemStatus::Active,
                Some(goal.revision),
                resumed_at_ms,
            )?
            .expect("the paused Goal revision was read in this transaction");
            tx.execute(
                "DELETE FROM goal_pause WHERE session_id = ?1 AND goal_id = ?2",
                params![session_id, goal.goal_id],
            )
            .map_err(zuno_db::map_error)?;
            Ok(Some(resumed))
        })
    }

    /// Immutable revisions for every goal instance in a session, oldest first.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a query failure or [`GoalError::UnknownStatus`] when
    /// a stored snapshot contains an invalid status.
    pub fn history(&self, session_id: &str) -> Result<Vec<GoalHistoryEntry>, GoalError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT sequence, {COLUMNS} FROM goal_history \
                 WHERE session_id = ?1 ORDER BY sequence"
            ))
            .map_err(zuno_db::map_error)?;
        let mut rows = statement
            .query(params![session_id])
            .map_err(zuno_db::map_error)?;
        let mut history = Vec::new();
        while let Some(row) = rows.next().map_err(zuno_db::map_error)? {
            let goal = from_row(row)?;
            history.push(GoalHistoryEntry {
                sequence: row.get("sequence").map_err(zuno_db::map_error)?,
                revision: goal.revision,
                goal,
            });
        }
        Ok(history)
    }

    /// Create the goal for `session_id`, replacing a finished or cancelled one.
    ///
    /// The model-facing entry point. It succeeds when the session has no goal,
    /// or when the goal it has is `complete` or `cancelled`; anything else is
    /// [`GoalError::GoalNotReplaceable`], naming the status that blocked it. A
    /// replacement mints a fresh `goal_id` and resets both counters, matching
    /// `goals.rs:236-244`.
    ///
    /// The status starts `active`, or `budget_limited` when `token_budget` is
    /// already met at zero tokens — the flip is in the statement, so a budget of
    /// `Some(0)` can never be observed as an `active` goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::GoalNotReplaceable`] when an unfinished goal is in the way,
    /// [`GoalError::EmptyObjective`] for a blank objective,
    /// [`GoalError::Spill`] or [`GoalError::PointerTooLong`] when an oversized
    /// objective cannot be spilled, and [`GoalError::Db`] on a statement
    /// failure.
    pub fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<Goal, GoalError> {
        self.create_goal_with_criteria(session_id, objective, &[], token_budget)
            .map(|created| created.goal)
    }

    /// Create a guarded model goal with explicit immutable success criteria.
    ///
    /// Each criterion is stored twice: verbatim in the `goal.success_criteria`
    /// JSON column, which is unchanged and remains the compatibility projection,
    /// and as a `goal_criterion` row carrying the identity a later citation needs.
    /// The ids are assigned in creation order as `c1`, `c2`, … — short because they
    /// are typed back by a model, deterministic so the same list always yields the
    /// same handles, and returned here because they are minted inside this
    /// transaction.
    ///
    /// # Errors
    ///
    /// [`GoalError::GoalNotReplaceable`] when an unfinished goal is in the way,
    /// [`GoalError::EmptyObjective`] for a blank objective,
    /// [`GoalError::Spill`] or [`GoalError::PointerTooLong`] when an oversized
    /// objective cannot be spilled, and [`GoalError::Db`] on a statement failure.
    pub fn create_goal_with_criteria(
        &self,
        session_id: &str,
        objective: &str,
        success_criteria: &[String],
        token_budget: Option<i64>,
    ) -> Result<GoalCreation, GoalError> {
        let objective = spill::store_objective(&self.spill_dir, objective)?;
        let goal_id = new_goal_id();
        let now_ms = now_ms()?;
        let outcome = self.pool.transaction(|tx| {
            let inserted = upsert(
                tx,
                GoalUpsert {
                    tail: UPSERT_IF_COMPLETE,
                    session_id,
                    goal_id: &goal_id,
                    objective: &objective,
                    success_criteria,
                    token_budget,
                    now_ms,
                },
            )?;
            if inserted.is_some() {
                clear_auxiliary_state(tx, session_id)?;
            }
            match inserted {
                Some(goal) => {
                    let criteria = insert_criteria(tx, session_id, success_criteria, now_ms)?;
                    Ok(Ok(GoalCreation { goal, criteria }))
                }
                // Read only to *name* the blocker. The refusal itself already
                // happened, atomically, in the statement's `WHERE`; this read
                // shares that statement's transaction, so the status it reports
                // is the one that blocked and not a later one.
                None => Ok(Err(blocking_status(tx, session_id)?)),
            }
        })?;
        outcome.map_err(|status| GoalError::GoalNotReplaceable {
            session_id: session_id.to_owned(),
            status,
        })
    }

    /// Replace the goal for `session_id` whatever state it is in.
    ///
    /// The user's escape hatch, and system-owned: without it a `blocked` goal
    /// would be permanent, since no system status is `complete` and
    /// [`GoalStore::create_goal`] refuses. codex draws the same line by having
    /// two functions — the unguarded `replace_thread_goal` (`goals.rs:156`) for
    /// its `/goal` command and the guarded `insert_thread_goal` (`goals.rs:213`)
    /// for the model — and this crate keeps that split, moving it from
    /// convention into the API's names.
    ///
    /// # Errors
    ///
    /// [`GoalError::EmptyObjective`] for a blank objective,
    /// [`GoalError::Spill`] or [`GoalError::PointerTooLong`] when an oversized
    /// objective cannot be spilled, and [`GoalError::Db`] on a statement
    /// failure.
    pub fn replace_goal_as_system(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<Goal, GoalError> {
        let objective = spill::store_objective(&self.spill_dir, objective)?;
        let goal_id = new_goal_id();
        let now_ms = now_ms()?;
        let replaced = self.pool.transaction(|tx| {
            let goal = upsert(
                tx,
                GoalUpsert {
                    tail: UPSERT_UNCONDITIONAL,
                    session_id,
                    goal_id: &goal_id,
                    objective: &objective,
                    success_criteria: &[],
                    token_budget,
                    now_ms,
                },
            )?;
            if goal.is_some() {
                clear_auxiliary_state(tx, session_id)?;
            }
            Ok(goal)
        })?;
        replaced.ok_or_else(|| {
            DbError::NotFound {
                table: TABLE.to_owned(),
                id: session_id.to_owned(),
            }
            .into()
        })
    }

    /// Record what the model reported about its own progress.
    ///
    /// `status` is a [`ModelStatus`], so `paused`, `usage_limited` and
    /// `budget_limited` are not expressible here. Use
    /// [`ModelStatus::parse`] to turn a string the model supplied into one, and
    /// the resulting refusal names what it should have said.
    ///
    /// A `budget_limited` goal stays `budget_limited` under a `blocked` report —
    /// the model does not get to relabel a spent budget as being stuck — while
    /// `complete` is still honoured, because finishing the work is true
    /// regardless of what it cost. Ports `goals.rs:328`.
    ///
    /// `complete` is not a plain write. Whichever entry point asks for it, the request
    /// runs the audit [`Self::complete_checked`] runs — plan ownership, unfinished
    /// durable work, criterion evidence, capability claims — because a gate with an
    /// unguarded side door is not a gate. Before this, an embedder calling here with
    /// `Complete` could finish a change goal whose criteria were still open.
    ///
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure, and for `Complete` every refusal
    /// listed on [`Self::complete_checked`] except [`GoalError::RevisionConflict`].
    pub fn update_status_as_model(
        &self,
        session_id: &str,
        status: ModelStatus,
    ) -> Result<Option<Goal>, GoalError> {
        if matches!(status, ModelStatus::Complete) {
            return self.complete_with_revision(session_id, None);
        }
        self.write_status(
            SET_STATUS_AS_MODEL,
            session_id,
            status.as_str(),
            false,
            None,
        )
    }

    /// Update model-owned status only if `expected_revision` is still current.
    ///
    /// `Complete` takes the audited path exactly as [`Self::complete_checked`] does;
    /// the two are the same call.
    pub fn update_status_as_model_checked(
        &self,
        session_id: &str,
        status: ModelStatus,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        if matches!(status, ModelStatus::Complete) {
            return self.complete_with_revision(session_id, Some(expected_revision));
        }
        self.write_status(
            SET_STATUS_AS_MODEL,
            session_id,
            status.as_str(),
            false,
            Some(expected_revision),
        )
    }

    /// Block the current active Goal with a typed permanent-failure reason.
    ///
    /// Status, reason, revision, and transient retry cleanup commit in one
    /// transaction. A concurrent pause, cancellation, budget stop, or completion
    /// wins instead of being overwritten by a late turn failure.
    pub fn block_with_reason(
        &self,
        session_id: &str,
        reason: GoalBlockReason,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        let rendered = reason.rendered();
        Ok(self.pool.transaction(|tx| {
            let goal = {
                let mut statement = tx
                    .prepare(BLOCK_ACTIVE_WITH_REASON)
                    .map_err(zuno_db::map_error)?;
                read_optional(&mut statement, params![rendered, now_ms, session_id])
                    .map_err(into_db_error)?
            };
            if goal.is_some() {
                clear_failure_and_retry_state(tx, session_id)?;
                tx.execute(
                    "DELETE FROM goal_pause WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
            }
            Ok(goal)
        })?)
    }

    /// Complete a goal only when all durable work and recorded evidence agree.
    ///
    /// The completion audit and status update share one `IMMEDIATE` transaction, so a
    /// concurrent writer cannot add unfinished work, record a receipt, or change the
    /// workspace between the check and the update. The same audit runs when
    /// [`Self::update_status_as_model`] is handed `Complete`; this entry point adds
    /// only the revision guard.
    ///
    /// Every pre-existing blocker still applies: unfinished plan steps, work items,
    /// jobs and pending human requests refuse completion exactly as before. Before
    /// those are even counted, the visible plan has to be *this* goal's: a
    /// `work_plan` row whose `goal_id` names another goal is refused as stale
    /// whatever its steps say — see [`audit_plan_ownership`] for why a plan with no
    /// `goal_id`, and an archived plan, are deliberately let through. On top of all
    /// that, a goal that [`Self::escalate_to_change`] marked as changing the
    /// workspace must have every criterion settled and must cite evidence no older
    /// than the last [`Self::mark_mutation`]. A [`GoalKind::Question`] goal is
    /// unaffected by the evidence rules and completes as it always did.
    ///
    /// # Errors
    ///
    /// [`GoalError::RevisionConflict`] when the goal moved on since the caller read
    /// it, [`GoalError::PlanBelongsToAnotherGoal`] when the visible plan is bound to
    /// a different goal, [`GoalError::CompletionBlocked`] when durable work is
    /// unfinished, [`GoalError::EvidenceMissing`] when a change goal has criteria
    /// that are neither satisfied nor waived — or no criteria at all, which is
    /// completion by assertion — [`GoalError::EvidenceUnproven`] when a cited receipt
    /// has since been rewritten or pruned so that it no longer proves success,
    /// [`GoalError::EvidenceStale`] when a cited receipt predates the last recorded
    /// change to the workspace, [`GoalError::CapabilityUnverified`] when a capability
    /// claim recorded under this goal cannot be relied on, and [`GoalError::Db`] on a
    /// statement failure.
    pub fn complete_checked(
        &self,
        session_id: &str,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        self.complete_with_revision(session_id, Some(expected_revision))
    }

    /// The one statement path that sets `complete`, guarded on a revision or not.
    ///
    /// Shared by [`Self::complete_checked`] and by the two model status writers when
    /// the model reports `complete`, so the audit cannot be skipped by choosing a
    /// different entry point. The audit and the status write share one `IMMEDIATE`
    /// transaction, so a concurrent writer cannot add unfinished work, rewrite a
    /// receipt, or change the workspace between the check and the update.
    fn complete_with_revision(
        &self,
        session_id: &str,
        expected_revision: Option<i64>,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let current = tx
                .query_row(
                    "SELECT goal_id, revision FROM goal WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(zuno_db::map_error)?;
            let Some((goal_id, actual)) = current else {
                return Ok(None);
            };
            if let Some(expected) = expected_revision
                && actual != expected
            {
                return Err(GoalError::RevisionConflict {
                    session_id: session_id.to_owned(),
                    expected,
                    actual,
                });
            }

            // Ownership before arithmetic: a plan written for another goal is refused
            // as stale whatever its step count says, because counting its unfinished
            // steps would describe the previous goal's work as this one's.
            audit_plan_ownership(tx, session_id, &goal_id)?;
            let (plan_steps, work_items, jobs, human_requests) =
                completion_blockers(tx, session_id)?;
            if plan_steps != 0 || work_items != 0 || jobs != 0 || human_requests != 0 {
                return Err(GoalError::CompletionBlocked {
                    plan_steps,
                    work_items,
                    jobs,
                    human_requests,
                });
            }
            audit_evidence(tx, session_id)?;

            let goal = {
                let mut statement = tx
                    .prepare(SET_STATUS_AS_MODEL)
                    .map_err(zuno_db::map_error)?;
                read_optional(
                    &mut statement,
                    params![
                        ModelStatus::Complete.as_str(),
                        now_ms,
                        session_id,
                        expected_revision
                    ],
                )
                .map_err(into_db_error)?
            };
            if goal.is_some() {
                tx.execute(
                    "DELETE FROM goal_pending_failure_signal WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
                clear_retry_state(tx, session_id)?;
                // A goal that just completed is no longer paused, so a pause row left
                // behind would describe a resumption that can never happen.
                if goal
                    .as_ref()
                    .is_some_and(|current| current.status != GoalStatus::Paused)
                {
                    tx.execute(
                        "DELETE FROM goal_pause WHERE session_id = ?1",
                        params![session_id],
                    )
                    .map_err(zuno_db::map_error)?;
                }
            }
            Ok(goal)
        })
    }

    /// Every success criterion for a session, in the order the goal listed them.
    ///
    /// The authority for identity and status. [`Goal::success_criteria`] still
    /// carries the same statements as a JSON projection, but it has no ids, so it
    /// cannot say which check a receipt proved.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a query failure and
    /// [`GoalError::UnknownCriterionStatus`] when a stored status is outside the
    /// `CHECK` constraint, which is corruption.
    pub fn criteria(&self, session_id: &str) -> Result<Vec<GoalCriterion>, GoalError> {
        let connection = self.pool.get()?;
        criteria_from(&connection, session_id)
    }

    /// Whether this goal only answers a question or also changes the workspace.
    ///
    /// [`GoalKind::Question`] with no `goal_kind` row, because a goal is presumed
    /// harmless until something reports otherwise; see [`Self::escalate_to_change`].
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a query failure and [`GoalError::UnknownGoalKind`] when
    /// the stored kind is outside the `CHECK` constraint.
    pub fn kind(&self, session_id: &str) -> Result<GoalKind, GoalError> {
        let connection = self.pool.get()?;
        kind_from(&connection, session_id)
    }

    /// Satisfy one criterion by citing a receipt that proves it.
    ///
    /// The whole audit runs in one `IMMEDIATE` transaction: the goal revision, the
    /// criterion's current state, the receipt and the mutation mark are all read
    /// beside the write that flips the row, so no receipt can be recorded and no
    /// file changed between the check and the citation being accepted.
    ///
    /// Four rules, and each one exists because a model can assert its way past the
    /// absence of it. The receipt has to exist *in this session*, so a receipt id
    /// from another run proves nothing here. Its outcome has to be `passed`. Its
    /// exit status has to be authoritative, because a status inferred from the last
    /// stage of a pipeline is a claim about `tee`, not about the tests. And it has
    /// to be newer than the last recorded change to the workspace, because evidence
    /// gathered before an edit says nothing about the code that exists after it.
    ///
    /// # Errors
    ///
    /// [`GoalError::NoGoal`] when the session has no goal,
    /// [`GoalError::RevisionConflict`] when the goal moved on since the caller read
    /// it, [`GoalError::UnknownCriterion`] when the id was never assigned,
    /// [`GoalError::EvidenceUnproven`] when the receipt is missing, failed,
    /// undecidable, carries a derived or absent exit status, or the criterion is
    /// already waived, [`GoalError::EvidenceStale`] when the receipt predates the
    /// last recorded change to the workspace, and [`GoalError::Db`] on a statement
    /// failure.
    pub fn satisfy_criterion(
        &self,
        session_id: &str,
        expected_revision: i64,
        criterion_id: &str,
        receipt_id: &str,
        at_ms: i64,
    ) -> Result<CriterionOutcome, GoalError> {
        self.pool.try_transaction(|tx| {
            let criterion =
                read_criterion_for_write(tx, session_id, expected_revision, criterion_id)?;
            if criterion.status == GoalCriterionStatus::Waived {
                return Err(GoalError::EvidenceUnproven {
                    criterion_id: criterion_id.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    reason: "the criterion is waived, so it is settled by a recorded decision \
                             rather than by evidence"
                        .to_owned(),
                });
            }
            let receipt = receipt_for(tx, session_id, receipt_id)?.ok_or_else(|| {
                GoalError::EvidenceUnproven {
                    criterion_id: criterion_id.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    reason: "no receipt with that id was recorded for this session; cite the \
                             receipt id printed by the tool result that ran the check"
                        .to_owned(),
                }
            })?;
            if !receipt.proves_success() {
                return Err(GoalError::EvidenceUnproven {
                    criterion_id: criterion_id.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    reason: unproven_reason(&receipt),
                });
            }
            if let Some(marked_at_ms) = mutation_mark(tx, session_id)?
                && marked_at_ms > receipt.time_created
            {
                return Err(GoalError::EvidenceStale {
                    criterion_id: criterion_id.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    marked_at_ms,
                    receipt_at_ms: receipt.time_created,
                });
            }
            tx.execute(
                "UPDATE goal_criterion \
                 SET status = 'satisfied', waiver_reason = NULL, receipt_id = ?3, \
                     satisfied_at_ms = ?4, updated_at_ms = ?4 \
                 WHERE session_id = ?1 AND criterion_id = ?2",
                params![session_id, criterion_id, receipt_id, at_ms],
            )
            .map_err(zuno_db::map_error)?;
            let goal = touch_goal(tx, session_id, expected_revision, at_ms)?;
            let criterion = require_criterion(tx, session_id, criterion_id)?;
            Ok(CriterionOutcome { goal, criterion })
        })
    }

    /// Close one criterion on the record without proving it.
    ///
    /// The deliberate escape hatch, and the reason it takes a reason: a criterion
    /// that turned out to be impossible, out of scope, or already true elsewhere is
    /// a real outcome, but it has to be readable afterwards as a decision somebody
    /// made rather than as a check that passed. A waived criterion lets a change
    /// goal complete; it never claims evidence, and [`Self::mark_mutation`] does not
    /// reopen it, because a decision is not invalidated by a later edit the way a
    /// test result is.
    ///
    /// A satisfied criterion cannot be waived. It has evidence, so it needs no excuse,
    /// and a waiver landing on it would swap a recorded, re-checkable receipt for a
    /// judgement call that nothing can re-check — the exact substitution this table
    /// exists to make visible. If the evidence has gone stale, [`Self::mark_mutation`]
    /// has already reopened the criterion, and a waiver is then accepted.
    ///
    /// # Errors
    ///
    /// [`GoalError::NoGoal`] when the session has no goal,
    /// [`GoalError::RevisionConflict`] when the goal moved on since the caller read
    /// it, [`GoalError::UnknownCriterion`] when the id was never assigned,
    /// [`GoalError::EmptyWaiverReason`] when the reason is blank,
    /// [`GoalError::CriterionAlreadySatisfied`] when the criterion already has
    /// evidence, and [`GoalError::Db`] on a statement failure.
    pub fn waive_criterion(
        &self,
        session_id: &str,
        expected_revision: i64,
        criterion_id: &str,
        reason: &str,
        at_ms: i64,
    ) -> Result<CriterionOutcome, GoalError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(GoalError::EmptyWaiverReason {
                criterion_id: criterion_id.to_owned(),
            });
        }
        self.pool.try_transaction(|tx| {
            let criterion =
                read_criterion_for_write(tx, session_id, expected_revision, criterion_id)?;
            if criterion.status == GoalCriterionStatus::Satisfied {
                return Err(GoalError::CriterionAlreadySatisfied {
                    criterion_id: criterion_id.to_owned(),
                    receipt_id: criterion.receipt_id.unwrap_or_default(),
                });
            }
            tx.execute(
                "UPDATE goal_criterion \
                 SET status = 'waived', waiver_reason = ?3, receipt_id = NULL, \
                     satisfied_at_ms = NULL, updated_at_ms = ?4 \
                 WHERE session_id = ?1 AND criterion_id = ?2",
                params![session_id, criterion_id, reason, at_ms],
            )
            .map_err(zuno_db::map_error)?;
            let goal = touch_goal(tx, session_id, expected_revision, at_ms)?;
            let criterion = require_criterion(tx, session_id, criterion_id)?;
            Ok(CriterionOutcome { goal, criterion })
        })
    }

    /// Record that the workspace changed, and reopen evidence that predates it.
    ///
    /// This is the rule that makes "the tests passed, then I edited three more
    /// files, so it is done" impossible. Every criterion satisfied before the mark
    /// goes back to `open` and loses its citation, so the run has to verify again
    /// after its last change rather than pointing at a result that describes code
    /// that no longer exists. Returns how many criteria were reopened, which is what
    /// a caller reports to the model.
    ///
    /// Idempotent, and monotonic: the stored mark keeps the greater of the two
    /// timestamps, so a mutation reported late — out of order, or replayed from a
    /// resumed turn — cannot move the workspace's clock backwards and quietly
    /// re-validate stale evidence.
    ///
    /// Deliberately does not bump the goal revision. A mutation is reported from the
    /// tool hot path, once per edit, and a revision bump there would make every
    /// unrelated concurrent write lose an optimistic-concurrency race it has no way
    /// to anticipate. Completion re-reads the criteria in its own transaction, so
    /// nothing depends on the revision having moved.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure.
    pub fn mark_mutation(&self, session_id: &str, at_ms: i64) -> Result<usize, GoalError> {
        let reopened = self.pool.transaction(|tx| {
            let marked_at_ms = tx
                .query_row(
                    "INSERT INTO goal_mutation_mark (session_id, marked_at_ms) \
                     SELECT session_id, ?2 FROM goal WHERE session_id = ?1 \
                     ON CONFLICT(session_id) DO UPDATE SET \
                         marked_at_ms = max(goal_mutation_mark.marked_at_ms, excluded.marked_at_ms) \
                     RETURNING marked_at_ms",
                    params![session_id, at_ms],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(zuno_db::map_error)?;
            let Some(marked_at_ms) = marked_at_ms else {
                return Ok(0);
            };
            tx.execute(
                "UPDATE goal_criterion \
                 SET status = 'open', receipt_id = NULL, satisfied_at_ms = NULL, \
                     updated_at_ms = ?2 \
                 WHERE session_id = ?1 AND status = 'satisfied' \
                   AND (satisfied_at_ms IS NULL OR satisfied_at_ms < ?3)",
                params![session_id, at_ms, marked_at_ms],
            )
            .map_err(zuno_db::map_error)
        })?;
        Ok(reopened)
    }

    /// Record that this goal changes the workspace, so completion needs evidence.
    ///
    /// Idempotent, and keeps the first reason: the earliest observed change is the
    /// one that explains why the goal is gated, and a later overwrite would replace
    /// the moment the run stopped being a question with whatever it happened to
    /// touch last.
    ///
    /// Returns the kind the goal now has, which is [`GoalKind::Question`] only when
    /// the session has no goal to escalate — there is deliberately no row for a
    /// session without one, so a stale escalation cannot gate a goal created later.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure and [`GoalError::UnknownGoalKind`]
    /// when the stored kind is outside the `CHECK` constraint.
    pub fn escalate_to_change(
        &self,
        session_id: &str,
        reason: &str,
        at_ms: i64,
    ) -> Result<GoalKind, GoalError> {
        let reason = reason.trim();
        self.pool.try_transaction(|tx| {
            tx.execute(
                "INSERT INTO goal_kind (session_id, kind, reason, escalated_at_ms) \
                 SELECT session_id, 'change', ?2, ?3 FROM goal WHERE session_id = ?1 \
                 ON CONFLICT(session_id) DO UPDATE SET \
                     kind = 'change', \
                     reason = COALESCE(goal_kind.reason, excluded.reason), \
                     escalated_at_ms = COALESCE(goal_kind.escalated_at_ms, excluded.escalated_at_ms)",
                params![
                    session_id,
                    (!reason.is_empty()).then_some(reason),
                    at_ms
                ],
            )
            .map_err(zuno_db::map_error)?;
            kind_from(tx, session_id)
        })
    }

    /// Account for one provider request's tokens exactly once.
    ///
    /// The turn loop asks a budget policy about every request, and a turn can be
    /// retried, resumed or replayed from a checkpoint. Keyed by
    /// `(session_id, request_id)` with `ON CONFLICT DO NOTHING`, so a replayed
    /// request is recognised as already accounted instead of being charged twice —
    /// double-charging would end a turn that still had allowance left, which looks
    /// exactly like a budget that cannot be trusted.
    ///
    /// A new request goes through the ordinary usage path, so the budget flip and
    /// the counters behave exactly as they do for turn-boundary accounting; nothing
    /// else about accounting changes.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure and [`GoalError::UnknownStatus`]
    /// when the stored status is outside the `CHECK` constraint.
    pub fn record_request_usage(
        &self,
        session_id: &str,
        request_id: &str,
        tokens: i64,
        at_ms: i64,
    ) -> Result<UsageRecorded, GoalError> {
        let tokens = tokens.max(0);
        self.pool.try_transaction(|tx| {
            let inserted = tx
                .execute(
                    "INSERT INTO goal_request_usage \
                     (session_id, request_id, tokens, recorded_at_ms) \
                     SELECT session_id, ?2, ?3, ?4 FROM goal WHERE session_id = ?1 \
                     ON CONFLICT(session_id, request_id) DO NOTHING",
                    params![session_id, request_id, tokens, at_ms],
                )
                .map_err(zuno_db::map_error)?
                > 0;
            let goal = if inserted {
                record_usage_in(tx, session_id, tokens, 0, true, at_ms)?
            } else {
                goal_from_transaction(tx, session_id)?
            };
            Ok(UsageRecorded {
                accounted: inserted,
                goal,
            })
        })
    }

    /// Set a status only the runtime or the user may choose.
    ///
    /// Unreachable from the model: [`SystemStatus`] is a distinct type and no
    /// model-facing API produces one.
    ///
    /// Two guards, both in the statement. A `budget_limited` goal is not
    /// downgraded to `paused` (`goals.rs:293`), and `active` on a goal already
    /// over budget resolves back to `budget_limited` (`goals.rs:329`) — so even
    /// this path cannot clear a budget limit without the budget being raised
    /// first via [`GoalStore::set_token_budget`].
    ///
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure.
    pub fn set_status_as_system(
        &self,
        session_id: &str,
        status: SystemStatus,
    ) -> Result<Option<Goal>, GoalError> {
        if status == SystemStatus::Paused {
            return self.pause_with_reason_checked(
                session_id,
                GoalPauseReason::UserInterruption,
                None,
                None,
            );
        }
        self.write_status(
            SET_STATUS_AS_SYSTEM,
            session_id,
            status.as_str(),
            matches!(status, SystemStatus::Active),
            None,
        )
    }

    /// Update system-owned status only if `expected_revision` is still current.
    pub fn set_status_as_system_checked(
        &self,
        session_id: &str,
        status: SystemStatus,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        if status == SystemStatus::Paused {
            return self.pause_with_reason_checked(
                session_id,
                GoalPauseReason::UserInterruption,
                None,
                Some(expected_revision),
            );
        }
        self.write_status(
            SET_STATUS_AS_SYSTEM,
            session_id,
            status.as_str(),
            matches!(status, SystemStatus::Active),
            Some(expected_revision),
        )
    }

    /// Suppress exactly the next idle continuation for an active goal.
    ///
    /// Returns `false` when the session has no active goal, so stale resume or
    /// fork notifications cannot leave a deferral that affects a later goal.
    pub fn defer_continuation_once(&self, session_id: &str) -> Result<bool, GoalError> {
        let changed = self.pool.transaction(|tx| {
            tx.execute(
                "INSERT INTO goal_continuation_deferral (session_id) \
                 SELECT session_id FROM goal WHERE session_id = ?1 AND status = 'active' \
                 ON CONFLICT(session_id) DO NOTHING",
                params![session_id],
            )
            .map_err(zuno_db::map_error)
        })?;
        Ok(changed > 0)
    }

    /// Consume a pending one-shot continuation deferral atomically.
    ///
    /// Competing idle callbacks cannot both observe the row: only the callback
    /// whose `DELETE ... RETURNING` removes it receives `true`.
    pub fn consume_continuation_deferral(&self, session_id: &str) -> Result<bool, GoalError> {
        let consumed = self.pool.transaction(|tx| {
            tx.query_row(
                "DELETE FROM goal_continuation_deferral WHERE session_id = ?1 \
                 RETURNING session_id",
                params![session_id],
                |_row| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(zuno_db::map_error)
        })?;
        Ok(consumed)
    }

    /// Persist the next automatic turn for the current active goal.
    ///
    /// The schedule is tied to `goal_id`; replacement clears it in the same
    /// transaction that mints a new goal. Consecutive failures increment the
    /// attempt before computing exponential backoff.
    pub fn schedule_retry(
        &self,
        session_id: &str,
        reason: GoalRetryReason,
        retry_after: Option<std::time::Duration>,
        policy: GoalRetryPolicy,
        scheduled_at_ms: i64,
        entropy: u64,
    ) -> Result<Option<GoalRetryState>, GoalError> {
        let state = self.pool.transaction(|tx| {
            let goal_id = tx
                .query_row(
                    "SELECT goal_id FROM goal WHERE session_id = ?1 AND status = 'active'",
                    params![session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(zuno_db::map_error)?;
            let Some(goal_id) = goal_id else {
                return Ok(None);
            };
            let previous = tx
                .query_row(
                    "SELECT attempt FROM goal_retry \
                     WHERE session_id = ?1 AND goal_id = ?2",
                    params![session_id, goal_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(zuno_db::map_error)?;
            let attempt = previous
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default()
                .saturating_add(1);
            let delay = policy.delay(attempt, retry_after, entropy);
            let delay_ms = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
            let retry_at_ms = scheduled_at_ms.saturating_add(delay_ms);
            tx.execute(
                "INSERT INTO goal_retry (
                    session_id, goal_id, attempt, reason, delay_ms,
                    retry_at_ms, scheduled_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id) DO UPDATE SET
                    goal_id = excluded.goal_id,
                    attempt = excluded.attempt,
                    reason = excluded.reason,
                    delay_ms = excluded.delay_ms,
                    retry_at_ms = excluded.retry_at_ms,
                    scheduled_at_ms = excluded.scheduled_at_ms",
                params![
                    session_id,
                    goal_id,
                    i64::from(attempt),
                    reason.as_str(),
                    delay_ms,
                    retry_at_ms,
                    scheduled_at_ms,
                ],
            )
            .map_err(zuno_db::map_error)?;
            Ok(Some(GoalRetryState {
                session_id: session_id.to_owned(),
                goal_id,
                attempt,
                reason,
                delay_ms,
                retry_at_ms,
                scheduled_at_ms,
            }))
        })?;
        Ok(state)
    }

    /// Read the pending automatic retry for a session.
    pub fn retry_state(&self, session_id: &str) -> Result<Option<GoalRetryState>, GoalError> {
        let connection = self.pool.get()?;
        let row = connection
            .query_row(
                "SELECT session_id, goal_id, attempt, reason, delay_ms,
                        retry_at_ms, scheduled_at_ms
                 FROM goal_retry WHERE session_id = ?1",
                params![session_id],
                retry_row_from_row,
            )
            .optional()
            .map_err(zuno_db::map_error)?;
        row.map(GoalRetryState::try_from).transpose()
    }

    /// Same-request provider backoff checkpoint that survived a process stop.
    pub fn provider_backoff_state(
        &self,
        session_id: &str,
    ) -> Result<Option<zuno_db::provider_backoff::ProviderBackoffCheckpoint>, GoalError> {
        let connection = self.pool.get()?;
        zuno_db::provider_backoff::get(&connection, session_id).map_err(GoalError::from)
    }

    /// Remove any retry schedule for a session.
    pub fn clear_retry(&self, session_id: &str) -> Result<bool, GoalError> {
        self.pool
            .transaction(|tx| clear_retry_state(tx, session_id))
            .map(|changed| changed > 0)
            .map_err(GoalError::from)
    }

    /// Mark a context-limit retry as durably compacted without incrementing backoff.
    ///
    /// A process that stops after compaction but before the provider request can then
    /// resume from the compacted transcript instead of compacting the same history
    /// again.
    pub fn mark_retry_context_compacted(&self, session_id: &str) -> Result<bool, GoalError> {
        self.pool
            .transaction(|tx| {
                tx.execute(
                    "UPDATE goal_retry
                     SET reason = 'context_compacted'
                     WHERE session_id = ?1
                       AND reason = 'context_limit'
                       AND goal_id = (
                         SELECT goal_id FROM goal
                         WHERE session_id = ?1 AND status = 'active'
                       )",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)
            })
            .map(|changed| changed > 0)
            .map_err(GoalError::from)
    }

    /// Stage the blocking condition reported during the current real turn.
    ///
    /// Repeated tool calls in one turn overwrite this row instead of incrementing
    /// the persisted streak. The turn boundary consumes the row exactly once, so
    /// three tool retries cannot impersonate three consecutive turns.
    pub fn stage_failure_signal(&self, session_id: &str, signal: &str) -> Result<bool, GoalError> {
        self.stage_failure_signal_with_revision(session_id, signal, None)
    }

    /// Stage a blocker only when the goal revision observed by the model is current.
    pub fn stage_failure_signal_checked(
        &self,
        session_id: &str,
        signal: &str,
        expected_revision: i64,
    ) -> Result<bool, GoalError> {
        self.stage_failure_signal_with_revision(session_id, signal, Some(expected_revision))
    }

    fn stage_failure_signal_with_revision(
        &self,
        session_id: &str,
        signal: &str,
        expected_revision: Option<i64>,
    ) -> Result<bool, GoalError> {
        let signal = signal.trim();
        if signal.is_empty() {
            return Ok(false);
        }
        self.pool.transaction(|tx| {
            let changed = tx
                .execute(
                    "INSERT INTO goal_pending_failure_signal (session_id, signal) \
                     SELECT session_id, ?2 FROM goal \
                     WHERE session_id = ?1 AND status = 'active' \
                       AND (?3 IS NULL OR revision = ?3) \
                     ON CONFLICT(session_id) DO UPDATE SET signal = excluded.signal",
                    params![session_id, signal, expected_revision],
                )
                .map_err(zuno_db::map_error)?;
            if changed == 0
                && let Some(error) = revision_conflict(tx, session_id, expected_revision)?
            {
                return Ok(Err(error));
            }
            Ok(Ok(changed > 0))
        })?
    }

    /// Consume the blocking condition staged by this turn, if any.
    pub fn consume_staged_failure_signal(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, GoalError> {
        self.pool
            .transaction(|tx| {
                tx.query_row(
                    "DELETE FROM goal_pending_failure_signal WHERE session_id = ?1 \
                     RETURNING signal",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(zuno_db::map_error)
            })
            .map_err(GoalError::from)
    }

    /// Record one turn's blocking signal, or clear the streak after progress.
    ///
    /// Repeating the same non-empty signal increments the count; a different
    /// signal starts again at one. The count saturates at three because that is
    /// the only threshold the continuation policy consumes. A missing or blank
    /// signal means the turn made progress and deletes the persisted streak.
    pub fn record_failure_signal(
        &self,
        session_id: &str,
        signal: Option<&str>,
    ) -> Result<Option<FailureStreak>, GoalError> {
        let signal = signal.map(str::trim).filter(|signal| !signal.is_empty());
        let streak = self.pool.transaction(|tx| {
            clear_retry_state(tx, session_id)?;
            let Some(signal) = signal else {
                tx.execute(
                    "DELETE FROM goal_failure_streak WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
                return Ok(None);
            };

            tx.query_row(
                "INSERT INTO goal_failure_streak (session_id, signal, consecutive_turns) \
                 SELECT session_id, ?2, 1 FROM goal \
                 WHERE session_id = ?1 AND status = 'active' \
                 ON CONFLICT(session_id) DO UPDATE SET \
                     signal = excluded.signal, \
                     consecutive_turns = CASE \
                         WHEN goal_failure_streak.signal = excluded.signal \
                             THEN min(goal_failure_streak.consecutive_turns + 1, 3) \
                         ELSE 1 \
                     END \
                 RETURNING signal, consecutive_turns",
                params![session_id, signal],
                failure_streak_from_row,
            )
            .optional()
            .map_err(zuno_db::map_error)
        })?;
        Ok(streak)
    }

    /// Read the current consecutive blocking signal for a session.
    pub fn failure_streak(&self, session_id: &str) -> Result<Option<FailureStreak>, GoalError> {
        let connection = self.pool.get()?;
        connection
            .query_row(
                "SELECT signal, consecutive_turns FROM goal_failure_streak WHERE session_id = ?1",
                params![session_id],
                failure_streak_from_row,
            )
            .optional()
            .map_err(zuno_db::map_error)
            .map_err(GoalError::from)
    }

    /// Change the token budget, flipping the status if the new one is already
    /// spent.
    ///
    /// System-owned, because a budget is the user's decision. Lowering it below
    /// `tokens_used` stops the goal in the same statement, so there is no window
    /// in which the agent keeps spending against a budget it has already
    /// exceeded. Ports `goals.rs:352-379`.
    ///
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure.
    pub fn set_token_budget(
        &self,
        session_id: &str,
        token_budget: Option<i64>,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        let goal = self.pool.transaction(|tx| {
            let goal = {
                let mut statement = tx.prepare(SET_TOKEN_BUDGET).map_err(zuno_db::map_error)?;
                read_optional(
                    &mut statement,
                    params![token_budget, now_ms, session_id, Option::<i64>::None],
                )
                .map_err(into_db_error)?
            };
            if goal.as_ref().is_some_and(|goal| !goal.status.is_active()) {
                clear_retry_state(tx, session_id)?;
            }
            Ok(goal)
        })?;
        Ok(goal)
    }

    /// Rewrite the objective, keeping the goal instance and its counters.
    ///
    /// Spills exactly as creation does, so a rewrite cannot smuggle an oversized
    /// value past the column's contract.
    ///
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::EmptyObjective`] for a blank objective,
    /// [`GoalError::Spill`] or [`GoalError::PointerTooLong`] when an oversized
    /// objective cannot be spilled, and [`GoalError::Db`] on a statement
    /// failure.
    pub fn update_objective(
        &self,
        session_id: &str,
        objective: &str,
    ) -> Result<Option<Goal>, GoalError> {
        self.update_objective_with_revision(session_id, objective, None)
    }

    /// Rewrite the objective only when `expected_revision` is still current.
    pub fn update_objective_checked(
        &self,
        session_id: &str,
        objective: &str,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        self.update_objective_with_revision(session_id, objective, Some(expected_revision))
    }

    fn update_objective_with_revision(
        &self,
        session_id: &str,
        objective: &str,
        expected_revision: Option<i64>,
    ) -> Result<Option<Goal>, GoalError> {
        let objective = spill::store_objective(&self.spill_dir, objective)?;
        let now_ms = now_ms()?;
        self.pool.transaction(|tx| {
            let goal = {
                let mut statement = tx.prepare(SET_OBJECTIVE).map_err(zuno_db::map_error)?;
                read_optional(
                    &mut statement,
                    params![objective, now_ms, session_id, expected_revision],
                )
                .map_err(into_db_error)?
            };
            if goal.is_none()
                && let Some(error) = revision_conflict(tx, session_id, expected_revision)?
            {
                return Ok(Err(error));
            }
            if goal.is_some() {
                clear_retry_state(tx, session_id)?;
            }
            Ok(Ok(goal))
        })?
    }

    /// Add a turn's spend to the counters, flipping to `budget_limited` in the
    /// same statement if that crosses the budget.
    ///
    /// The counters accumulate whatever the status, so a turn that finished the
    /// goal still has its cost recorded — codex needs a separate accounting mode
    /// for that (`GoalAccountingMode::ActiveOrComplete`, `goals.rs:521`); here it
    /// is simply the default, because a counter that stops counting is a counter
    /// nobody can trust. Only the *flip* is restricted to `active`, matching
    /// `goals.rs:526-531`: a paused or complete goal does not silently become
    /// `budget_limited`.
    ///
    /// Negative deltas are clamped to zero (`goals.rs:507-508`): usage does not
    /// run backwards, and a caller that computed a negative delta has a bug this
    /// store should not persist.
    ///
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure.
    pub fn record_usage(
        &self,
        session_id: &str,
        token_delta: i64,
        time_delta_seconds: i64,
        accounting_known: bool,
    ) -> Result<Option<Goal>, GoalError> {
        let token_delta = token_delta.max(0);
        let time_delta_seconds = time_delta_seconds.max(0);
        let now_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            record_usage_in(
                tx,
                session_id,
                token_delta,
                time_delta_seconds,
                accounting_known,
                now_ms,
            )
        })
    }

    fn write_status(
        &self,
        sql: &str,
        session_id: &str,
        status: &str,
        clear_failure_streak: bool,
        expected_revision: Option<i64>,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        self.pool.transaction(|tx| {
            let goal = {
                let mut statement = tx.prepare(sql).map_err(zuno_db::map_error)?;
                read_optional(
                    &mut statement,
                    params![status, now_ms, session_id, expected_revision],
                )
                .map_err(into_db_error)?
            };
            if goal.is_none()
                && let Some(error) = revision_conflict(tx, session_id, expected_revision)?
            {
                return Ok(Err(error));
            }
            if clear_failure_streak && goal.is_some() {
                tx.execute(
                    "DELETE FROM goal_failure_streak WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
            }
            if goal.is_some() {
                tx.execute(
                    "DELETE FROM goal_pending_failure_signal WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(zuno_db::map_error)?;
                clear_retry_state(tx, session_id)?;
                if goal
                    .as_ref()
                    .is_some_and(|current| current.status != GoalStatus::Paused)
                {
                    tx.execute(
                        "DELETE FROM goal_pause WHERE session_id = ?1",
                        params![session_id],
                    )
                    .map_err(zuno_db::map_error)?;
                }
            }
            Ok(Ok(goal))
        })?
    }

    fn pause_with_reason_checked(
        &self,
        session_id: &str,
        reason: GoalPauseReason,
        human_request_id: Option<&str>,
        expected_revision: Option<i64>,
    ) -> Result<Option<Goal>, GoalError> {
        let paused_at_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let goal = update_system_status_in(
                tx,
                session_id,
                SystemStatus::Paused,
                expected_revision,
                paused_at_ms,
            )?;
            if goal.is_none()
                && let Some(error) = revision_conflict(tx, session_id, expected_revision)?
            {
                return Err(error);
            }
            if let Some(goal) = goal.as_ref() {
                if goal.status == GoalStatus::Paused {
                    upsert_pause_in(tx, goal, reason, human_request_id, paused_at_ms)?;
                } else {
                    tx.execute(
                        "DELETE FROM goal_pause WHERE session_id = ?1",
                        params![session_id],
                    )
                    .map_err(zuno_db::map_error)?;
                }
                clear_failure_and_retry_state(tx, session_id)?;
            }
            Ok(goal)
        })
    }
}

/// The upsert body shared by the guarded and unguarded replacements.
///
/// `?4` is the budget and appears twice: `0 >= ?4` is `tokens_used >= budget`
/// evaluated at the zero counters a new goal starts with, which is
/// `status_after_budget_limit(Active, 0, budget)` (`goals.rs:618-630`) expressed
/// in SQL so no caller can skip it.
const UPSERT_BODY: &str = "\
INSERT INTO goal (
    session_id, goal_id, revision, objective, success_criteria, status, blocked_reason,
    token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, updated_at_ms
) VALUES (
    ?1, ?2, 1, ?3, ?4,
    CASE WHEN ?5 IS NOT NULL AND 0 >= ?5 THEN 'budget_limited' ELSE 'active' END,
    NULL, ?5, 0, 1, 0, ?6, ?6
)
ON CONFLICT(session_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    revision = 1,
    objective = excluded.objective,
    success_criteria = excluded.success_criteria,
    status = excluded.status,
    blocked_reason = NULL,
    token_budget = excluded.token_budget,
    tokens_used = 0,
    usage_known = 1,
    time_used_seconds = 0,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms";

/// The model's replacement: refuses unless the goal in the way is terminal.
///
/// The `WHERE` is what makes the refusal atomic. When it is false SQLite skips
/// the row, so `RETURNING` yields nothing and there is no separate read that a
/// concurrent writer could slip between. Ports `goals.rs:245`.
const UPSERT_IF_COMPLETE: &str = "\
WHERE goal.status IN ('complete', 'cancelled')
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

/// The user's replacement, with no status guard. Ports `goals.rs:179-198`.
const UPSERT_UNCONDITIONAL: &str = "\
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

const SET_STATUS_AS_MODEL: &str = "\
UPDATE goal
SET status = CASE
        WHEN status = 'budget_limited' AND ?1 = 'blocked' THEN status
        WHEN status = 'cancelled' THEN status
        ELSE ?1
    END,
    blocked_reason = CASE
        WHEN status = 'budget_limited' AND ?1 = 'blocked' THEN blocked_reason
        WHEN status = 'cancelled' THEN blocked_reason
        WHEN ?1 = 'blocked' THEN (
            SELECT signal FROM goal_failure_streak WHERE session_id = ?3
        )
        ELSE NULL
    END,
    revision = revision + 1,
    updated_at_ms = ?2
WHERE session_id = ?3 AND (?4 IS NULL OR revision = ?4)
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

const BLOCK_ACTIVE_WITH_REASON: &str = "\
UPDATE goal
SET status = 'blocked',
    blocked_reason = ?1,
    revision = revision + 1,
    updated_at_ms = ?2
WHERE session_id = ?3 AND status = 'active'
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

const SET_STATUS_AS_SYSTEM: &str = "\
UPDATE goal
SET status = CASE
        WHEN status = 'budget_limited' AND ?1 = 'paused' THEN status
        WHEN status = 'cancelled' THEN status
        WHEN ?1 = 'active'
             AND token_budget IS NOT NULL
             AND tokens_used >= token_budget THEN 'budget_limited'
        ELSE ?1
    END,
    blocked_reason = NULL,
    revision = revision + 1,
    updated_at_ms = ?2
WHERE session_id = ?3 AND (?4 IS NULL OR revision = ?4)
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

const SET_TOKEN_BUDGET: &str = "\
UPDATE goal
SET token_budget = ?1,
    status = CASE
        WHEN status = 'active' AND ?1 IS NOT NULL AND tokens_used >= ?1
            THEN 'budget_limited'
        ELSE status
    END,
    revision = revision + 1,
    updated_at_ms = ?2
WHERE session_id = ?3 AND (?4 IS NULL OR revision = ?4)
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

const SET_OBJECTIVE: &str = "\
UPDATE goal
SET objective = ?1,
    revision = revision + 1,
    updated_at_ms = ?2
WHERE session_id = ?3 AND (?4 IS NULL OR revision = ?4)
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

/// `tokens_used + ?1` reads the pre-update value, which is how the flip decides
/// on the post-increment total inside the statement that performs the increment.
const RECORD_USAGE: &str = "\
UPDATE goal
SET tokens_used = tokens_used + ?1,
    time_used_seconds = time_used_seconds + ?2,
    usage_known = usage_known AND ?3,
    revision = revision + 1,
    status = CASE
        WHEN status = 'active'
             AND token_budget IS NOT NULL
             AND tokens_used + ?1 >= token_budget THEN 'budget_limited'
        ELSE status
    END,
    updated_at_ms = ?4
WHERE session_id = ?5
RETURNING session_id, goal_id, revision, objective, success_criteria, status, \
blocked_reason, token_budget, tokens_used, usage_known, time_used_seconds, created_at_ms, \
updated_at_ms";

/// Where oversized objectives spill: `data()/goal-objective`.
#[must_use]
pub fn default_spill_dir() -> PathBuf {
    zuno_paths::data().join(OBJECTIVE_SPILL_DIRECTORY)
}

struct GoalUpsert<'a> {
    tail: &'a str,
    session_id: &'a str,
    goal_id: &'a str,
    objective: &'a str,
    success_criteria: &'a [String],
    token_budget: Option<i64>,
    now_ms: i64,
}

fn upsert(tx: &Transaction<'_>, input: GoalUpsert<'_>) -> Result<Option<Goal>, DbError> {
    let sql = format!("{UPSERT_BODY}\n{}", input.tail);
    let success_criteria =
        serde_json::to_string(input.success_criteria).map_err(|source| DbError::Query {
            source: Box::new(source),
        })?;
    let mut statement = tx.prepare(&sql).map_err(zuno_db::map_error)?;
    read_optional(
        &mut statement,
        params![
            input.session_id,
            input.goal_id,
            input.objective,
            success_criteria,
            input.token_budget,
            input.now_ms
        ],
    )
    .map_err(into_db_error)
}

fn completion_blockers(
    tx: &Transaction<'_>,
    session_id: &str,
) -> Result<(usize, usize, usize, usize), DbError> {
    let plan_steps = if table_exists(tx, "work_plan")? {
        let steps = tx
            .query_row(
                "SELECT steps FROM work_plan WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(zuno_db::map_error)?;
        match steps {
            Some(steps) => serde_json::from_str::<Vec<serde_json::Value>>(&steps)
                .map_err(|error| DbError::Query {
                    source: Box::new(error),
                })?
                .into_iter()
                .filter(|step| {
                    !matches!(
                        step.get("status").and_then(serde_json::Value::as_str),
                        Some("completed" | "cancelled")
                    )
                })
                .count(),
            None => 0,
        }
    } else {
        0
    };
    let work_items = if table_exists(tx, "work_item")? {
        tx.query_row(
            "SELECT COUNT(*) FROM work_item              WHERE session_id = ?1 AND status NOT IN ('completed','cancelled')",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(zuno_db::map_error)
        .map(|count| usize::try_from(count).unwrap_or(usize::MAX))?
    } else {
        0
    };
    let jobs = if table_exists(tx, "agent_job")? {
        // Session ancestry is durable in `session.parent_id`. `UNION` deliberately
        // de-duplicates each reachable id, so even a corrupt parent cycle reaches
        // a fixed point instead of recursing forever.
        tx.query_row(
            "WITH RECURSIVE descendant_session(session_id) AS ( \
               VALUES (?1) \
               UNION \
               SELECT s.id \
               FROM session AS s \
               JOIN descendant_session AS d ON s.parent_id = d.session_id \
             ) \
             SELECT COUNT(*) \
             FROM agent_job AS j \
             JOIN descendant_session AS d ON d.session_id = j.parent_session_id \
             WHERE ( \
                 j.status IN ('queued', 'running', 'uncertain') \
                 OR ( \
                   j.report_delivery = 'next-step' \
                   AND j.status IN ('completed', 'failed', 'cancelled') \
                   AND EXISTS ( \
                     SELECT 1 FROM session_input AS i \
                     WHERE i.id = j.report_input_id \
                       AND i.session_id = j.parent_session_id \
                       AND i.state IN ('queued', 'steering', 'promoted') \
                   ) \
                 ) \
               )",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(zuno_db::map_error)
        .map(|count| usize::try_from(count).unwrap_or(usize::MAX))?
    } else {
        0
    };
    let human_requests = if table_exists(tx, "human_request")? {
        tx.query_row(
            "SELECT COUNT(*) FROM human_request \
             WHERE session_id = ?1 AND state = 'pending' \
               AND goal_id = (SELECT goal_id FROM goal WHERE session_id = ?1)",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(zuno_db::map_error)
        .map(|count| usize::try_from(count).unwrap_or(usize::MAX))?
    } else {
        0
    };
    Ok((plan_steps, work_items, jobs, human_requests))
}

/// Refuse completion when the visible plan was written for a different goal.
///
/// `work_plan` is keyed by session and outlives the goal it was written for. Once
/// every step of the previous goal's plan is `completed`, [`completion_blockers`]
/// counts nothing, so after `goal_propose` replaces that goal the new one could
/// complete against the old checklist — or against a plan `plan_update` re-created
/// for it while inheriting the stale `goal_id`. The check makes the plan the model
/// can see the plan of the goal it is completing.
///
/// A plan with a `NULL` `goal_id` passes. It predates the binding, and refusing it
/// would strand every session whose plan was written before plans knew their goal.
///
/// Archived plans (`work_plan_archive`) are deliberately not consulted. A
/// `completed` or `superseded` row is history, and refusing over it would make any
/// goal that ever replaced its plan uncompletable. A `suspended` row is a parent
/// waiting to be restored; the moment it is restored it is the visible plan and this
/// same check sees it. Ownership is judged where a plan can describe work again, not
/// while it is dormant.
fn audit_plan_ownership(
    tx: &Transaction<'_>,
    session_id: &str,
    goal_id: &str,
) -> Result<(), GoalError> {
    if !table_exists(tx, "work_plan")? {
        return Ok(());
    }
    let plan_goal_id = tx
        .query_row(
            "SELECT goal_id FROM work_plan WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?
        .flatten();
    match plan_goal_id {
        Some(plan_goal_id) if plan_goal_id != goal_id => Err(GoalError::PlanBelongsToAnotherGoal {
            session_id: session_id.to_owned(),
            plan_goal_id,
            goal_id: goal_id.to_owned(),
        }),
        _ => Ok(()),
    }
}

fn table_exists(tx: &Transaction<'_>, table: &str) -> Result<bool, DbError> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get::<_, bool>(0),
    )
    .map_err(zuno_db::map_error)
}

/// The `goal_criterion` columns every read below selects, in one place so a row
/// reader can rely on the names.
const CRITERION_COLUMNS: &str = "criterion_id, ordinal, statement, status, waiver_reason, \
                                 receipt_id, satisfied_at_ms";

/// Mint the criterion rows for a freshly created goal.
///
/// Ids are positional — `c1`, `c2`, … in the order the goal listed its criteria —
/// because they are typed back by a model citing evidence: short enough to
/// transcribe without error, and deterministic, so the same list always produces
/// the same handles and a projection written from either storage agrees.
///
/// The delete is deliberate belt and braces. Creation is the only place ids are
/// minted, and it must never inherit a row from an earlier goal, because a
/// citation recorded against the previous `c1` would otherwise appear to prove the
/// new goal's first criterion.
fn insert_criteria(
    tx: &Transaction<'_>,
    session_id: &str,
    statements: &[String],
    now_ms: i64,
) -> Result<Vec<GoalCriterion>, DbError> {
    tx.execute(
        "DELETE FROM goal_criterion WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    let mut criteria = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let ordinal = i64::try_from(index).unwrap_or(i64::MAX);
        let criterion_id = format!("c{}", ordinal.saturating_add(1));
        tx.execute(
            "INSERT INTO goal_criterion \
             (session_id, criterion_id, ordinal, statement, status, waiver_reason, \
              receipt_id, satisfied_at_ms, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, 'open', NULL, NULL, NULL, ?5, ?5)",
            params![session_id, criterion_id, ordinal, statement, now_ms],
        )
        .map_err(zuno_db::map_error)?;
        criteria.push(GoalCriterion {
            criterion_id,
            ordinal,
            statement: statement.clone(),
            status: GoalCriterionStatus::Open,
            waiver_reason: None,
            receipt_id: None,
            satisfied_at_ms: None,
        });
    }
    Ok(criteria)
}

/// Refuse completion when a change goal's criteria are not settled by evidence
/// that still describes the current workspace.
///
/// Two distinct failures, both invisible in prose. A criterion still `open` means
/// nothing ever verified it, and a change goal with no criteria at all means
/// nothing was ever verifiable — the same refusal, because an empty checklist is
/// not a completed one. Separately, a mutation mark newer than a cited receipt
/// means the workspace moved after that check ran, so the receipt describes code
/// that no longer exists.
///
/// The cited receipts are re-read, not trusted. A criterion row says "satisfied by
/// receipt X", but the receipt ledger is not append-only: it upserts on
/// `(session_id, tool_call_id)`, so a replayed call can rewrite the row X named into
/// a failed run — under a fresh id or the same one — and pruning can delete it
/// outright. Either leaves a citation that resolves to nothing, or to a failure,
/// while the criterion still reads as satisfied. Completion is the moment that
/// matters, so it is the moment each receipt is fetched again and
/// [`VerificationReceipt::proves_success`] is asked again.
///
/// A third failure is a reliance rather than a check: a capability the session
/// enabled on a guess. Once the criteria are settled, the claims recorded under this
/// goal are audited too — see [`crate::capability::audit_capability_claims`] — so a
/// goal cannot complete while it rests on an `inferred` or `unknown` claim, or on a
/// probe that a later write retired.
///
/// A [`GoalKind::Question`] goal passes straight through. Nothing changed, so
/// there is nothing to verify, and requiring a receipt would leave a run that was
/// only ever asked a question with no way to finish.
fn audit_evidence(tx: &Transaction<'_>, session_id: &str) -> Result<(), GoalError> {
    if !kind_from(tx, session_id)?.requires_evidence() {
        return Ok(());
    }
    let criteria = criteria_from(tx, session_id)?;
    // A `satisfied` row with no citation is unproven by construction — the store
    // never writes one — but a row is data, and the audit trusts it no further than
    // the receipt it can fetch.
    let unsatisfied: Vec<String> = criteria
        .iter()
        .filter(|criterion| {
            !criterion.status.is_settled()
                || (criterion.status == GoalCriterionStatus::Satisfied
                    && criterion.receipt_id.is_none())
        })
        .map(|criterion| criterion.criterion_id.clone())
        .collect();
    if criteria.is_empty() || !unsatisfied.is_empty() {
        return Err(GoalError::EvidenceMissing { unsatisfied });
    }
    let marked_at_ms = mutation_mark(tx, session_id)?;
    // Checked per criterion rather than against the newest citation alone: each
    // criterion stands on its own receipt, and the first one that fails in list order
    // is also the first one the run should verify again.
    for criterion in &criteria {
        let Some(receipt_id) = criterion.receipt_id.as_deref() else {
            continue;
        };
        let Some(receipt) = receipt_for(tx, session_id, receipt_id)? else {
            return Err(GoalError::EvidenceUnproven {
                criterion_id: criterion.criterion_id.clone(),
                receipt_id: receipt_id.to_owned(),
                reason: "the cited receipt is no longer recorded for this session — a replayed \
                         call or receipt pruning removed it; run the check again and cite the \
                         new receipt"
                    .to_owned(),
            });
        };
        if !receipt.proves_success() {
            return Err(GoalError::EvidenceUnproven {
                criterion_id: criterion.criterion_id.clone(),
                receipt_id: receipt_id.to_owned(),
                reason: format!(
                    "the cited receipt no longer proves success — {}; run the check again and \
                     cite the new receipt",
                    unproven_reason(&receipt)
                ),
            });
        }
        if let Some(marked_at_ms) = marked_at_ms
            && marked_at_ms > receipt.time_created
        {
            return Err(GoalError::EvidenceStale {
                criterion_id: criterion.criterion_id.clone(),
                receipt_id: receipt_id.to_owned(),
                marked_at_ms,
                receipt_at_ms: receipt.time_created,
            });
        }
    }
    crate::capability::audit_capability_claims(tx, session_id)
}

/// Every criterion for a session in list order, from any connection or
/// transaction.
fn criteria_from(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<GoalCriterion>, GoalError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {CRITERION_COLUMNS} FROM goal_criterion WHERE session_id = ?1 \
             ORDER BY ordinal, criterion_id"
        ))
        .map_err(zuno_db::map_error)?;
    let mut rows = statement
        .query(params![session_id])
        .map_err(zuno_db::map_error)?;
    let mut criteria = Vec::new();
    while let Some(row) = rows.next().map_err(zuno_db::map_error)? {
        criteria.push(criterion_from_row(row)?);
    }
    Ok(criteria)
}

fn criterion_from_row(row: &Row<'_>) -> Result<GoalCriterion, GoalError> {
    let status: String = row.get("status").map_err(zuno_db::map_error)?;
    Ok(GoalCriterion {
        criterion_id: row.get("criterion_id").map_err(zuno_db::map_error)?,
        ordinal: row.get("ordinal").map_err(zuno_db::map_error)?,
        statement: row.get("statement").map_err(zuno_db::map_error)?,
        status: GoalCriterionStatus::parse(&status)?,
        waiver_reason: row.get("waiver_reason").map_err(zuno_db::map_error)?,
        receipt_id: row.get("receipt_id").map_err(zuno_db::map_error)?,
        satisfied_at_ms: row.get("satisfied_at_ms").map_err(zuno_db::map_error)?,
    })
}

fn read_criterion(
    connection: &Connection,
    session_id: &str,
    criterion_id: &str,
) -> Result<Option<GoalCriterion>, GoalError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {CRITERION_COLUMNS} FROM goal_criterion \
             WHERE session_id = ?1 AND criterion_id = ?2"
        ))
        .map_err(zuno_db::map_error)?;
    let mut rows = statement
        .query(params![session_id, criterion_id])
        .map_err(zuno_db::map_error)?;
    let Some(row) = rows.next().map_err(zuno_db::map_error)? else {
        return Ok(None);
    };
    criterion_from_row(row).map(Some)
}

/// Read a criterion, or name the ids that do exist.
///
/// A bare "no such criterion" costs a turn spent guessing, so the refusal carries
/// the list the model should have cited from.
fn require_criterion(
    connection: &Connection,
    session_id: &str,
    criterion_id: &str,
) -> Result<GoalCriterion, GoalError> {
    match read_criterion(connection, session_id, criterion_id)? {
        Some(criterion) => Ok(criterion),
        None => Err(GoalError::UnknownCriterion {
            session_id: session_id.to_owned(),
            criterion_id: criterion_id.to_owned(),
            known: known_criteria(connection, session_id)?,
        }),
    }
}

fn known_criteria(connection: &Connection, session_id: &str) -> Result<String, GoalError> {
    let ids: Vec<String> = criteria_from(connection, session_id)?
        .into_iter()
        .map(|criterion| criterion.criterion_id)
        .collect();
    Ok(if ids.is_empty() {
        "this goal has no success criteria".to_owned()
    } else {
        format!("known criteria: {}", ids.join(", "))
    })
}

/// Check the revision and load the criterion a write is about, in that order.
///
/// Both reads share the caller's transaction, so the state that passed the check
/// is the state the write lands on.
fn read_criterion_for_write(
    tx: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    criterion_id: &str,
) -> Result<GoalCriterion, GoalError> {
    let goal = goal_from_transaction(tx, session_id)?.ok_or_else(|| GoalError::NoGoal {
        session_id: session_id.to_owned(),
    })?;
    if goal.revision != expected_revision {
        return Err(GoalError::RevisionConflict {
            session_id: session_id.to_owned(),
            expected: expected_revision,
            actual: goal.revision,
        });
    }
    require_criterion(tx, session_id, criterion_id)
}

/// Bump the goal's revision so a criterion change is visible to optimistic
/// concurrency.
///
/// Guarded on the expected revision like every other goal write, which also means
/// the history trigger records the change: a citation is part of the goal's story,
/// not a side table nobody replays.
fn touch_goal(
    tx: &Transaction<'_>,
    session_id: &str,
    expected_revision: i64,
    at_ms: i64,
) -> Result<Goal, GoalError> {
    let goal = {
        let mut statement = tx
            .prepare(&format!(
                "UPDATE {TABLE} SET revision = revision + 1, updated_at_ms = ?2 \
                 WHERE session_id = ?1 AND revision = ?3 RETURNING {COLUMNS}"
            ))
            .map_err(zuno_db::map_error)?;
        read_optional(
            &mut statement,
            params![session_id, at_ms, expected_revision],
        )?
    };
    match goal {
        Some(goal) => Ok(goal),
        // Unreachable while callers check the revision first, and re-read rather
        // than assume so a future caller that forgets still gets a true conflict.
        None => Ok(
            revision_conflict(tx, session_id, Some(expected_revision))?.map_or_else(
                || {
                    Err(GoalError::NoGoal {
                        session_id: session_id.to_owned(),
                    })
                },
                Err,
            )?,
        ),
    }
}

/// When the workspace last changed, if anything reported a change.
pub(crate) fn mutation_mark(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<i64>, GoalError> {
    let marked_at_ms = connection
        .query_row(
            "SELECT marked_at_ms FROM goal_mutation_mark WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    Ok(marked_at_ms)
}

/// Whether this goal is gated on evidence, defaulting to
/// [`GoalKind::Question`].
fn kind_from(connection: &Connection, session_id: &str) -> Result<GoalKind, GoalError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT kind FROM goal_kind WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    match stored {
        Some(stored) => GoalKind::parse(&stored),
        None => Ok(GoalKind::Question),
    }
}

/// Find a receipt recorded for this session, tolerating a pool without the table.
///
/// The goal store shares the application pool, and a caller may hand it a
/// database opened without the verification table — an older file, a narrow test
/// fixture. Absent table means no receipt, which is the answer the evidence gate
/// needs anyway, and matches how the completion blockers already degrade.
///
/// Scoped to `session_id` because a receipt is evidence about one run: an id
/// borrowed from another session proves nothing here, and looking it up
/// unscoped would make it look like it did.
pub(crate) fn receipt_for(
    tx: &Transaction<'_>,
    session_id: &str,
    receipt_id: &str,
) -> Result<Option<VerificationReceipt>, GoalError> {
    if !table_exists(tx, "verification_receipt")? {
        return Ok(None);
    }
    Ok(zuno_db::verification::find(tx, session_id, receipt_id)?)
}

/// Say which proof condition a receipt failed, in the receipt's own vocabulary.
///
/// "Failed" and "cannot be trusted" call for different next actions — fix the
/// code, or run the check in a way that surfaces a real exit status — so the
/// refusal distinguishes them instead of saying only that the citation was
/// rejected.
pub(crate) fn unproven_reason(receipt: &VerificationReceipt) -> String {
    match receipt.outcome {
        ReceiptOutcome::Passed => format!(
            "its recorded exit status is {}, not authoritative, so it does not show the \
             command as a whole succeeded",
            receipt.exit_authority.as_str()
        ),
        ReceiptOutcome::Failed => "the recorded outcome is failed".to_owned(),
        ReceiptOutcome::Unknown => {
            "the recorded outcome is unknown, so it decides nothing".to_owned()
        }
    }
}

/// Add a spend to the counters inside a caller's transaction.
///
/// Shared by turn-boundary accounting and per-request accounting so both take the
/// same path through [`RECORD_USAGE`], where the budget flip happens in the
/// increment statement itself.
fn record_usage_in(
    tx: &Transaction<'_>,
    session_id: &str,
    token_delta: i64,
    time_delta_seconds: i64,
    accounting_known: bool,
    now_ms: i64,
) -> Result<Option<Goal>, GoalError> {
    let goal = {
        let mut statement = tx.prepare(RECORD_USAGE).map_err(zuno_db::map_error)?;
        read_optional(
            &mut statement,
            params![
                token_delta,
                time_delta_seconds,
                accounting_known,
                now_ms,
                session_id
            ],
        )?
    };
    if goal.as_ref().is_some_and(|goal| !goal.status.is_active()) {
        clear_retry_state(tx, session_id)?;
    }
    Ok(goal)
}

fn goal_from_transaction(
    tx: &Transaction<'_>,
    session_id: &str,
) -> Result<Option<Goal>, GoalError> {
    let mut statement = tx
        .prepare(&format!(
            "SELECT {COLUMNS} FROM {TABLE} WHERE session_id = ?1"
        ))
        .map_err(zuno_db::map_error)?;
    read_optional(&mut statement, params![session_id])
}

fn update_system_status_in(
    tx: &Transaction<'_>,
    session_id: &str,
    status: SystemStatus,
    expected_revision: Option<i64>,
    now_ms: i64,
) -> Result<Option<Goal>, GoalError> {
    let mut statement = tx
        .prepare(SET_STATUS_AS_SYSTEM)
        .map_err(zuno_db::map_error)?;
    read_optional(
        &mut statement,
        params![status.as_str(), now_ms, session_id, expected_revision],
    )
}

fn upsert_pause_in(
    tx: &Transaction<'_>,
    goal: &Goal,
    reason: GoalPauseReason,
    human_request_id: Option<&str>,
    paused_at_ms: i64,
) -> Result<(), GoalError> {
    tx.execute(
        "INSERT INTO goal_pause \
         (session_id, goal_id, reason, human_request_id, paused_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(session_id) DO UPDATE SET \
           goal_id=excluded.goal_id, reason=excluded.reason, \
           human_request_id=excluded.human_request_id, paused_at_ms=excluded.paused_at_ms",
        params![
            goal.session_id,
            goal.goal_id,
            reason.as_str(),
            human_request_id,
            paused_at_ms
        ],
    )
    .map_err(zuno_db::map_error)?;
    Ok(())
}

fn pause_state_from(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<GoalPauseState>, GoalError> {
    let row = connection
        .query_row(
            "SELECT session_id, goal_id, reason, human_request_id, paused_at_ms \
             FROM goal_pause WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, String>("session_id")?,
                    row.get::<_, String>("goal_id")?,
                    row.get::<_, String>("reason")?,
                    row.get::<_, Option<String>>("human_request_id")?,
                    row.get::<_, i64>("paused_at_ms")?,
                ))
            },
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    row.map(
        |(session_id, goal_id, reason, human_request_id, paused_at_ms)| {
            Ok(GoalPauseState {
                session_id,
                goal_id,
                reason: GoalPauseReason::parse(&reason)?,
                human_request_id,
                paused_at_ms,
            })
        },
    )
    .transpose()
}

fn revision_conflict(
    tx: &Transaction<'_>,
    session_id: &str,
    expected_revision: Option<i64>,
) -> Result<Option<GoalError>, DbError> {
    let Some(expected) = expected_revision else {
        return Ok(None);
    };
    let actual = tx
        .query_row(
            "SELECT revision FROM goal WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    Ok(actual
        .filter(|actual| *actual != expected)
        .map(|actual| GoalError::RevisionConflict {
            session_id: session_id.to_owned(),
            expected,
            actual,
        }))
}

fn blocking_status(tx: &Transaction<'_>, session_id: &str) -> Result<GoalStatus, DbError> {
    let stored: String = tx
        .query_row(
            "SELECT status FROM goal WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(zuno_db::map_error)?;
    GoalStatus::parse(&stored).map_err(into_db_error)
}

fn clear_auxiliary_state(tx: &Transaction<'_>, session_id: &str) -> Result<(), DbError> {
    tx.execute(
        "DELETE FROM goal_continuation_deferral WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_pending_failure_signal WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_failure_streak WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_pause WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    // Evidence belongs to one goal. A citation, an escalation, a mutation mark and
    // an accounting ledger all describe the goal that was in force when they were
    // written, so a replacement starts from nothing: otherwise a receipt recorded
    // for the previous objective would keep proving a criterion of the new one, and
    // a request already charged to a spent budget would be refused a second time
    // against a budget that was just reset.
    tx.execute(
        "DELETE FROM goal_criterion WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_kind WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_mutation_mark WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_request_usage WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    // `goal_capability_claim` is deliberately left alone. A claim is provenance for
    // what the session relied on, not evidence about one goal; deleting it here would
    // make an inferred capability indistinguishable from a checked one as soon as the
    // goal that inferred it was replaced. See `crate::capability`.
    if table_exists(tx, "human_request")? {
        tx.execute(
            "UPDATE human_request \
             SET state='cancelled', revision=revision+1, \
                 time_updated=(SELECT updated_at_ms FROM goal WHERE session_id=?1), \
                 time_resolved=(SELECT updated_at_ms FROM goal WHERE session_id=?1) \
             WHERE session_id=?1 AND state='pending' \
               AND (goal_id IS NULL OR goal_id != (SELECT goal_id FROM goal WHERE session_id=?1))",
            params![session_id],
        )
        .map_err(zuno_db::map_error)?;
    }
    if table_exists(tx, "provider_retry_backoff")? {
        zuno_db::provider_backoff::clear_session(tx, session_id)?;
    }
    clear_retry_state(tx, session_id)?;
    Ok(())
}

fn clear_failure_and_retry_state(tx: &Transaction<'_>, session_id: &str) -> Result<(), DbError> {
    tx.execute(
        "DELETE FROM goal_pending_failure_signal WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    tx.execute(
        "DELETE FROM goal_failure_streak WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)?;
    clear_retry_state(tx, session_id)?;
    Ok(())
}

fn clear_retry_state(tx: &Transaction<'_>, session_id: &str) -> Result<usize, DbError> {
    tx.execute(
        "DELETE FROM goal_retry WHERE session_id = ?1",
        params![session_id],
    )
    .map_err(zuno_db::map_error)
}

fn failure_streak_from_row(row: &Row<'_>) -> rusqlite::Result<FailureStreak> {
    let count: i64 = row.get("consecutive_turns")?;
    Ok(FailureStreak {
        signal: row.get("signal")?,
        consecutive_turns: u32::try_from(count).unwrap_or(3),
    })
}

struct RetryRow {
    session_id: String,
    goal_id: String,
    attempt: i64,
    reason: String,
    delay_ms: i64,
    retry_at_ms: i64,
    scheduled_at_ms: i64,
}

fn retry_row_from_row(row: &Row<'_>) -> rusqlite::Result<RetryRow> {
    Ok(RetryRow {
        session_id: row.get("session_id")?,
        goal_id: row.get("goal_id")?,
        attempt: row.get("attempt")?,
        reason: row.get("reason")?,
        delay_ms: row.get("delay_ms")?,
        retry_at_ms: row.get("retry_at_ms")?,
        scheduled_at_ms: row.get("scheduled_at_ms")?,
    })
}

impl TryFrom<RetryRow> for GoalRetryState {
    type Error = GoalError;

    fn try_from(row: RetryRow) -> Result<Self, Self::Error> {
        let reason = GoalRetryReason::parse(&row.reason)
            .ok_or(GoalError::UnknownRetryReason { value: row.reason })?;
        Ok(Self {
            session_id: row.session_id,
            goal_id: row.goal_id,
            attempt: u32::try_from(row.attempt).unwrap_or(u32::MAX),
            reason,
            delay_ms: row.delay_ms,
            retry_at_ms: row.retry_at_ms,
            scheduled_at_ms: row.scheduled_at_ms,
        })
    }
}

fn read_optional(
    statement: &mut rusqlite::Statement<'_>,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<Option<Goal>, GoalError> {
    let mut rows = statement.query(parameters).map_err(zuno_db::map_error)?;
    let Some(row) = rows.next().map_err(zuno_db::map_error)? else {
        return Ok(None);
    };
    from_row(row).map(Some)
}

fn from_row(row: &Row<'_>) -> Result<Goal, GoalError> {
    let status: String = row.get("status").map_err(zuno_db::map_error)?;
    Ok(Goal {
        session_id: row.get("session_id").map_err(zuno_db::map_error)?,
        goal_id: row.get("goal_id").map_err(zuno_db::map_error)?,
        revision: row.get("revision").map_err(zuno_db::map_error)?,
        objective: row.get("objective").map_err(zuno_db::map_error)?,
        success_criteria: serde_json::from_str(
            &row.get::<_, String>("success_criteria")
                .map_err(zuno_db::map_error)?,
        )
        .map_err(|source| {
            GoalError::Db(DbError::Query {
                source: Box::new(source),
            })
        })?,
        status: GoalStatus::parse(&status)?,
        blocked_reason: row.get("blocked_reason").map_err(zuno_db::map_error)?,
        token_budget: row.get("token_budget").map_err(zuno_db::map_error)?,
        tokens_used: row.get("tokens_used").map_err(zuno_db::map_error)?,
        usage_known: row.get("usage_known").map_err(zuno_db::map_error)?,
        time_used_seconds: row.get("time_used_seconds").map_err(zuno_db::map_error)?,
        created_at_ms: row.get("created_at_ms").map_err(zuno_db::map_error)?,
        updated_at_ms: row.get("updated_at_ms").map_err(zuno_db::map_error)?,
    })
}

/// Collapse a [`GoalError`] back into a [`DbError`] for the transaction helper.
///
/// `Pool::transaction` is typed on `DbError`, and the only non-database failure
/// reachable inside one of these closures is a stored status outside the `CHECK`
/// constraint — corruption, which the taxonomy already has a home for.
fn into_db_error(error: GoalError) -> DbError {
    match error {
        GoalError::Db(error) => error,
        other => DbError::Query {
            source: Box::new(std::io::Error::other(other.to_string())),
        },
    }
}

fn new_goal_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Now, in Unix milliseconds — the convention the columns use.
///
/// Saturating rather than failing on the `i64` conversion: it overflows in the
/// year 292 million, and a goal store that refuses to write then is worse than
/// one that clamps.
pub(crate) fn now_ms() -> Result<i64, GoalError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

/// Let the pause table accept every reason this build knows about.
///
/// [`AUXILIARY_SCHEMA`] creates `goal_pause` with a `CHECK` constraint naming each
/// [`GoalPauseReason`], and `CREATE TABLE IF NOT EXISTS` does nothing to a table that
/// already exists. A database created before a reason was added therefore keeps a
/// constraint that rejects it, and SQLite cannot `ALTER` a `CHECK` constraint. Without
/// this, adding a reason would work on a fresh install and fail on an upgraded one, at
/// the worst moment: the write that records why a run stopped.
///
/// The goal tables sit outside the [`zuno_db::migration`] format marker, which versions
/// only the tables `zuno-db` itself owns, so the repair is expressed here and keyed on
/// the constraint rather than on a version number.
///
/// The rebuild is the standard SQLite sequence — rename, recreate, copy, drop — and runs
/// inside the caller's transaction, so a failure anywhere leaves the original table in
/// place. Recreating by re-running [`AUXILIARY_SCHEMA`] rather than a second copy of the
/// DDL is deliberate: one definition cannot drift from another. `goal_pause` carries no
/// index or trigger, so nothing else has to be reattached.
fn widen_pause_reasons(tx: &Transaction<'_>) -> Result<(), DbError> {
    let declared: Option<String> = tx
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'goal_pause'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    let Some(declared) = declared else {
        return Ok(());
    };
    if GoalPauseReason::ALL
        .into_iter()
        .all(|reason| declared.contains(&format!("'{}'", reason.as_str())))
    {
        return Ok(());
    }
    tx.execute_batch("ALTER TABLE goal_pause RENAME TO goal_pause_superseded")
        .map_err(zuno_db::map_error)?;
    tx.execute_batch(AUXILIARY_SCHEMA)
        .map_err(zuno_db::map_error)?;
    tx.execute_batch(
        "INSERT INTO goal_pause (session_id, goal_id, reason, human_request_id, paused_at_ms)
           SELECT session_id, goal_id, reason, human_request_id, paused_at_ms
           FROM goal_pause_superseded;
         DROP TABLE goal_pause_superseded",
    )
    .map_err(zuno_db::map_error)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
