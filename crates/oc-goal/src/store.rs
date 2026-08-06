//! The `goal` table: one goal per session, and every write that touches it.
//!
//! # Where this table lives, and why not in `opencode.db`
//!
//! In its own database file, `goal_1.db`, beside `opencode.db` but not inside
//! it. Two independent reasons, either of which would be sufficient.
//!
//! **`opencode.db` is not ours to extend.** `oc-db` reproduces the TypeScript
//! `opencode.db` schema byte-for-byte — 19 application tables plus the migration
//! journal — and proves it with a differential test against a real database
//! (`oc-db/src/schema.rs`, `oc-db/tests/schema.rs`). Adding a 20th table would
//! break that test, and papering over it would break the promise the whole crate
//! exists for: that a user can switch between the two binaries and keep their
//! sessions. A goal is a feature the TypeScript binary does not have, so it has
//! no place in a file that binary also writes.
//!
//! **A goal must outlive session churn.** The point of a goal is to survive the
//! compaction that throws away the conversation which set it. That argues
//! against sharing a file with state that gets pruned, vacuumed and cascaded,
//! and it is why there is deliberately **no** `FOREIGN KEY (session_id)
//! REFERENCES session(id) ON DELETE CASCADE` here: a goal is keyed *by* a
//! session id, not owned by a session row.
//!
//! codex reached the same place by the same route. Goals live in their own
//! `goals_1.sqlite` (`codex-rs/state/src/sqlite.rs:30`), and there is a
//! `0034_drop_thread_goals.sql` in the *main* migration set recording the move
//! out of the shared state database.
//!
//! The `_1` in the filename is codex's convention and is load-bearing: an
//! incompatible change to this schema should rev the filename rather than
//! migrate in place, because a goal is cheap to lose and expensive to corrupt.
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
//!   `DO UPDATE` carries `WHERE goal.status = 'complete'`, exactly as
//!   `insert_thread_goal` does (`goals.rs:245`). A read-then-write would let two
//!   concurrent `create_goal` calls both observe `complete` and both replace.
//!   The refusal is the statement returning no row.

use crate::error::GoalError;
use crate::spill;
use crate::status::{GoalStatus, ModelStatus, SystemStatus};
use oc_db::Pool;
use oc_error::DbError;
use oc_paths::DbLocation;
use rusqlite::{Row, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The table this module owns.
pub const TABLE: &str = "goal";

/// The goal database's filename under [`oc_paths::data`].
///
/// Suffixed the way codex suffixes `goals_1.sqlite`
/// (`codex-rs/state/src/sqlite.rs:30`): an incompatible schema change revs the
/// number instead of migrating.
pub const GOAL_DB_FILE: &str = "goal_1.db";

/// The directory under [`oc_paths::data`] that oversized objectives spill into.
pub const OBJECTIVE_SPILL_DIRECTORY: &str = "goal-objective";

/// The table, verbatim.
///
/// A port of `codex-rs/state/goals_migrations/0001_thread_goals.sql` with
/// `thread_goals` renamed to `goal` and `thread_id` to `session_id`; every other
/// column name, the `CHECK` members, the defaults and the integer-Unix-
/// milliseconds convention are unchanged.
///
/// `IF NOT EXISTS` rather than a migration chain: this file has exactly one
/// table and one version, and reopening it must be a no-op — that is what makes
/// a goal survive a restart.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS goal (
    session_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete'
    )),
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
)";

const COLUMNS: &str = "session_id, goal_id, objective, status, token_budget, tokens_used, \
     time_used_seconds, created_at_ms, updated_at_ms";

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
    /// What the agent is working towards, or the pointer sentence that names the
    /// file holding it. Never longer than [`spill::MAX_OBJECTIVE_CHARS`].
    pub objective: String,
    /// Whether, and why, the agent should keep going.
    pub status: GoalStatus,
    /// The token ceiling, or `None` for unlimited.
    pub token_budget: Option<i64>,
    /// Tokens spent against this goal instance. Reset by a replacement.
    pub tokens_used: i64,
    /// Wall-clock seconds spent against this goal instance.
    pub time_used_seconds: i64,
    /// When this goal instance was created, in Unix milliseconds.
    pub created_at_ms: i64,
    /// When it last changed, in Unix milliseconds.
    pub updated_at_ms: i64,
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

/// The `goal` table, over its own SQLite database.
///
/// Cheap to clone-by-reference and safe to share: the pool serializes
/// connections and every write runs in one `IMMEDIATE` transaction.
#[derive(Debug)]
pub struct GoalStore {
    pool: Pool,
    spill_dir: PathBuf,
}

impl GoalStore {
    /// Open the goal database the running binary would use.
    ///
    /// # Errors
    ///
    /// [`GoalError::Db`] when the database cannot be opened, a pragma did not
    /// take effect, or the table cannot be created.
    pub fn open_default() -> Result<Self, GoalError> {
        Self::open_at(&default_db_path(), default_spill_dir())
    }

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
    /// `oc-db`'s pool gives this a named shared-cache database with a retained
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
        let pool = Pool::open(location)?;
        pool.transaction(|tx| tx.execute_batch(SCHEMA).map_err(oc_db::map_error))?;
        Ok(Self { pool, spill_dir })
    }

    /// The pool this store writes through.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
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
            .map_err(oc_db::map_error)?;
        read_optional(&mut statement, params![session_id])
    }

    /// Create the goal for `session_id`, replacing a finished one.
    ///
    /// The model-facing entry point. It succeeds when the session has no goal,
    /// or when the goal it has is `complete`; anything else is
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
        let objective = spill::store_objective(&self.spill_dir, objective)?;
        let goal_id = new_goal_id();
        let now_ms = now_ms()?;
        let outcome = self.pool.transaction(|tx| {
            let inserted = upsert(
                tx,
                UPSERT_IF_COMPLETE,
                session_id,
                &goal_id,
                &objective,
                token_budget,
                now_ms,
            )?;
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
            upsert(
                tx,
                UPSERT_UNCONDITIONAL,
                session_id,
                &goal_id,
                &objective,
                token_budget,
                now_ms,
            )
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
        self.write_status(SET_STATUS_AS_MODEL, session_id, status.as_str())
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
        self.write_status(SET_STATUS_AS_SYSTEM, session_id, status.as_str())
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
            let mut statement = tx.prepare(SET_TOKEN_BUDGET).map_err(oc_db::map_error)?;
            read_optional(&mut statement, params![token_budget, now_ms, session_id])
                .map_err(into_db_error)
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
        let objective = spill::store_objective(&self.spill_dir, objective)?;
        let now_ms = now_ms()?;
        let goal = self.pool.transaction(|tx| {
            let mut statement = tx.prepare(SET_OBJECTIVE).map_err(oc_db::map_error)?;
            read_optional(&mut statement, params![objective, now_ms, session_id])
                .map_err(into_db_error)
        })?;
        Ok(goal)
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
    ) -> Result<Option<Goal>, GoalError> {
        let token_delta = token_delta.max(0);
        let time_delta_seconds = time_delta_seconds.max(0);
        let now_ms = now_ms()?;
        let goal = self.pool.transaction(|tx| {
            let mut statement = tx.prepare(RECORD_USAGE).map_err(oc_db::map_error)?;
            read_optional(
                &mut statement,
                params![token_delta, time_delta_seconds, now_ms, session_id],
            )
            .map_err(into_db_error)
        })?;
        Ok(goal)
    }

    fn write_status(
        &self,
        sql: &str,
        session_id: &str,
        status: &str,
    ) -> Result<Option<Goal>, GoalError> {
        let now_ms = now_ms()?;
        let goal = self.pool.transaction(|tx| {
            let mut statement = tx.prepare(sql).map_err(oc_db::map_error)?;
            read_optional(&mut statement, params![status, now_ms, session_id])
                .map_err(into_db_error)
        })?;
        Ok(goal)
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
    session_id, goal_id, objective, status,
    token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms
) VALUES (
    ?1, ?2, ?3,
    CASE WHEN ?4 IS NOT NULL AND 0 >= ?4 THEN 'budget_limited' ELSE 'active' END,
    ?4, 0, 0, ?5, ?5
)
ON CONFLICT(session_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    objective = excluded.objective,
    status = excluded.status,
    token_budget = excluded.token_budget,
    tokens_used = 0,
    time_used_seconds = 0,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms";

/// The model's replacement: refuses unless the goal in the way is finished.
///
/// The `WHERE` is what makes the refusal atomic. When it is false SQLite skips
/// the row, so `RETURNING` yields nothing and there is no separate read that a
/// concurrent writer could slip between. Ports `goals.rs:245`.
const UPSERT_IF_COMPLETE: &str = "\
WHERE goal.status = 'complete'
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

/// The user's replacement, with no status guard. Ports `goals.rs:179-198`.
const UPSERT_UNCONDITIONAL: &str = "\
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

const SET_STATUS_AS_MODEL: &str = "\
UPDATE goal
SET status = CASE
        WHEN status = 'budget_limited' AND ?1 = 'blocked' THEN status
        ELSE ?1
    END,
    updated_at_ms = ?2
WHERE session_id = ?3
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

const SET_STATUS_AS_SYSTEM: &str = "\
UPDATE goal
SET status = CASE
        WHEN status = 'budget_limited' AND ?1 = 'paused' THEN status
        WHEN ?1 = 'active'
             AND token_budget IS NOT NULL
             AND tokens_used >= token_budget THEN 'budget_limited'
        ELSE ?1
    END,
    updated_at_ms = ?2
WHERE session_id = ?3
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

const SET_TOKEN_BUDGET: &str = "\
UPDATE goal
SET token_budget = ?1,
    status = CASE
        WHEN status = 'active' AND ?1 IS NOT NULL AND tokens_used >= ?1
            THEN 'budget_limited'
        ELSE status
    END,
    updated_at_ms = ?2
WHERE session_id = ?3
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

const SET_OBJECTIVE: &str = "\
UPDATE goal
SET objective = ?1,
    updated_at_ms = ?2
WHERE session_id = ?3
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

/// `tokens_used + ?1` reads the pre-update value, which is how the flip decides
/// on the post-increment total inside the statement that performs the increment.
const RECORD_USAGE: &str = "\
UPDATE goal
SET tokens_used = tokens_used + ?1,
    time_used_seconds = time_used_seconds + ?2,
    status = CASE
        WHEN status = 'active'
             AND token_budget IS NOT NULL
             AND tokens_used + ?1 >= token_budget THEN 'budget_limited'
        ELSE status
    END,
    updated_at_ms = ?3
WHERE session_id = ?4
RETURNING session_id, goal_id, objective, status, token_budget, tokens_used, \
time_used_seconds, created_at_ms, updated_at_ms";

/// Where the goal database lives: `data()/goal_1.db`.
#[must_use]
pub fn default_db_path() -> PathBuf {
    oc_paths::data().join(GOAL_DB_FILE)
}

/// Where oversized objectives spill: `data()/goal-objective`.
#[must_use]
pub fn default_spill_dir() -> PathBuf {
    oc_paths::data().join(OBJECTIVE_SPILL_DIRECTORY)
}

fn upsert(
    tx: &Transaction<'_>,
    tail: &str,
    session_id: &str,
    goal_id: &str,
    objective: &str,
    token_budget: Option<i64>,
    now_ms: i64,
) -> Result<Option<Goal>, DbError> {
    let sql = format!("{UPSERT_BODY}\n{tail}");
    let mut statement = tx.prepare(&sql).map_err(oc_db::map_error)?;
    read_optional(
        &mut statement,
        params![session_id, goal_id, objective, token_budget, now_ms],
    )
    .map_err(into_db_error)
}

fn blocking_status(tx: &Transaction<'_>, session_id: &str) -> Result<GoalStatus, DbError> {
    let stored: String = tx
        .query_row(
            "SELECT status FROM goal WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(oc_db::map_error)?;
    GoalStatus::parse(&stored).map_err(into_db_error)
}

fn read_optional(
    statement: &mut rusqlite::Statement<'_>,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<Option<Goal>, GoalError> {
    let mut rows = statement.query(parameters).map_err(oc_db::map_error)?;
    let Some(row) = rows.next().map_err(oc_db::map_error)? else {
        return Ok(None);
    };
    from_row(row).map(Some)
}

fn from_row(row: &Row<'_>) -> Result<Goal, GoalError> {
    let status: String = row.get("status").map_err(oc_db::map_error)?;
    Ok(Goal {
        session_id: row.get("session_id").map_err(oc_db::map_error)?,
        goal_id: row.get("goal_id").map_err(oc_db::map_error)?,
        objective: row.get("objective").map_err(oc_db::map_error)?,
        status: GoalStatus::parse(&status)?,
        token_budget: row.get("token_budget").map_err(oc_db::map_error)?,
        tokens_used: row.get("tokens_used").map_err(oc_db::map_error)?,
        time_used_seconds: row.get("time_used_seconds").map_err(oc_db::map_error)?,
        created_at_ms: row.get("created_at_ms").map_err(oc_db::map_error)?,
        updated_at_ms: row.get("updated_at_ms").map_err(oc_db::map_error)?,
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
