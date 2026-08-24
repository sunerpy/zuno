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
//! * **The guarded replace.** [`GoalStore::create_goal`] is one upsert whose
//!   `DO UPDATE` carries a terminal-status guard, exactly as
//!   `insert_thread_goal` does (`goals.rs:245`). A read-then-write would let two
//!   concurrent `create_goal` calls both observe `complete` and both replace.
//!   The refusal is the statement returning no row.

use crate::error::GoalError;
use crate::retry::{GoalRetryPolicy, GoalRetryReason, GoalRetryState};
use crate::spill;
use crate::status::{GoalStatus, ModelStatus, SystemStatus};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_db::Pool;
use zuno_error::DbError;
use zuno_paths::DbLocation;

/// The table this module owns.
pub const TABLE: &str = "goal";

/// The directory under [`zuno_paths::data`] that oversized objectives spill into.
pub const OBJECTIVE_SPILL_DIRECTORY: &str = "goal-objective";

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
pub const AUXILIARY_SCHEMA: &str = "\
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
        'tool_transient',
        'tool_uncertain'
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
                .map_err(zuno_db::map_error)
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
    }

    /// Create a guarded model goal with explicit immutable success criteria.
    pub fn create_goal_with_criteria(
        &self,
        session_id: &str,
        objective: &str,
        success_criteria: &[String],
        token_budget: Option<i64>,
    ) -> Result<Goal, GoalError> {
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
                Some(goal) => Ok(Ok(goal)),
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
    /// Returns `None` when the session has no goal.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] on a statement failure.
    pub fn update_status_as_model(
        &self,
        session_id: &str,
        status: ModelStatus,
    ) -> Result<Option<Goal>, GoalError> {
        self.write_status(
            SET_STATUS_AS_MODEL,
            session_id,
            status.as_str(),
            false,
            None,
        )
    }

    /// Update model-owned status only if `expected_revision` is still current.
    pub fn update_status_as_model_checked(
        &self,
        session_id: &str,
        status: ModelStatus,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        self.write_status(
            SET_STATUS_AS_MODEL,
            session_id,
            status.as_str(),
            false,
            Some(expected_revision),
        )
    }

    /// Complete a goal only when all durable plan steps, work items, and jobs are terminal.
    ///
    /// The completion audit and status update share one `IMMEDIATE` transaction, so a
    /// concurrent writer cannot add unfinished work between the check and the update.
    pub fn complete_checked(
        &self,
        session_id: &str,
        expected_revision: i64,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        self.pool.try_transaction(|tx| {
            let actual = tx
                .query_row(
                    "SELECT revision FROM goal WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(zuno_db::map_error)?;
            let Some(actual) = actual else {
                return Ok(None);
            };
            if actual != expected_revision {
                return Err(GoalError::RevisionConflict {
                    session_id: session_id.to_owned(),
                    expected: expected_revision,
                    actual,
                });
            }

            let (plan_steps, work_items, jobs) = completion_blockers(tx, session_id)?;
            if plan_steps != 0 || work_items != 0 || jobs != 0 {
                return Err(GoalError::CompletionBlocked {
                    plan_steps,
                    work_items,
                    jobs,
                });
            }

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
            }
            Ok(goal)
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
        let goal = self.pool.transaction(|tx| {
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
            }
            Ok(Ok(goal))
        })?
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
) -> Result<(usize, usize, usize), DbError> {
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
        tx.query_row(
            "SELECT COUNT(*) FROM agent_job              WHERE parent_session_id = ?1 AND status NOT IN ('completed','cancelled')",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(zuno_db::map_error)
        .map(|count| usize::try_from(count).unwrap_or(usize::MAX))?
    } else {
        0
    };
    Ok((plan_steps, work_items, jobs))
}

fn table_exists(tx: &Transaction<'_>, table: &str) -> Result<bool, DbError> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get::<_, bool>(0),
    )
    .map_err(zuno_db::map_error)
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
fn now_ms() -> Result<i64, GoalError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
