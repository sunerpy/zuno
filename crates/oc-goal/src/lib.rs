//! Goal store and continuation board that survive across sessions.
//!
//! # What a goal is for
//!
//! A long agent run outlives the conversation that started it. Compaction throws
//! away the messages, and with them the reason the run exists — so the agent
//! drifts, or stops, or declares victory on something nobody asked for. A goal is
//! the one piece of state that does not get summarised away: an objective, a
//! status saying whether the run should continue, and the budget it is allowed to
//! spend getting there.
//!
//! Ported from codex's `/goal` mechanism, `codex-rs/state/src/runtime/goals.rs`
//! and `codex-rs/state/src/model/thread_goal.rs`.
//!
//! # The three decisions this crate makes
//!
//! **Statuses are split by who may write them.** The model may report `blocked`
//! or `complete` — facts about its own work. It may not write `paused`,
//! `usage_limited` or `budget_limited`, because those decide whether the run
//! continues on evidence the model does not hold, and a model that could pause
//! itself or clear a budget limit would be governing its own leash. The split is
//! carried by [`ModelStatus`] and [`SystemStatus`] being different types, not by
//! a runtime check that a future caller can forget. See [`status`].
//!
//! **Two invariants live in SQL.** The budget flip and the guarded replace are
//! both in the statements that perform the write, because as Rust-side checks
//! they would be check-then-act races. See [`store`].
//!
//! **The goal has its own database file.** `oc-db` owns a byte-compatible
//! reproduction of the TypeScript `opencode.db`; a goal is a feature that binary
//! does not have, so it does not belong in a file that binary also writes — and a
//! goal that cascaded away with unrelated session state would defeat the whole
//! point. See [`store`] for the full argument.
//!
//! ```
//! use oc_goal::{GoalStore, ModelStatus};
//!
//! let spill = tempfile::tempdir()?;
//! let store = GoalStore::open_memory(spill.path().to_path_buf())?;
//! let goal = store.create_goal("ses_1", "land the port", Some(100_000))?;
//! assert!(goal.status.is_active());
//!
//! // The model cannot even name a status the system owns.
//! let refusal = ModelStatus::parse("paused").expect_err("system-owned");
//! assert!(refusal.to_string().contains("`blocked` or `complete`"));
//!
//! store.update_status_as_model("ses_1", ModelStatus::Complete)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod spill;
pub mod status;
pub mod store;

pub use crate::error::GoalError;
pub use crate::spill::{
    MAX_OBJECTIVE_CHARS, OBJECTIVE_FILE_NAME, OBJECTIVE_POINTER_PREFIX, OBJECTIVE_POINTER_SUFFIX,
};
pub use crate::status::{GoalStatus, ModelStatus, StatusOwner, SystemStatus};
pub use crate::store::{
    GOAL_DB_FILE, Goal, GoalStore, OBJECTIVE_SPILL_DIRECTORY, SCHEMA, TABLE, default_db_path,
    default_spill_dir,
};
