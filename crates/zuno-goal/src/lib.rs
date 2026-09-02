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
//! Inspired by Codex's goal mechanism and extended with Zuno's durable
//! cross-turn retry controller.
//!
//! # The six decisions this crate makes
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
//! **Goal and work state share the application database.** Goal, plan, todo, job,
//! and session checkpoints can be audited and completed against one SQLite
//! transaction boundary. Test fixtures may still use private pools.
//!
//! **"Done" is a claim about the workspace, so it needs evidence.** A goal that
//! changes files carries success criteria with ids, and each one is closed either
//! by citing a verification receipt the runtime recorded — a real exit status from
//! a real command, in this session — or by an explicit waiver with a reason. Prose
//! asserting that the tests pass is not evidence, and evidence gathered before the
//! last edit is not evidence about the code that exists now, so a recorded change
//! reopens anything it invalidates. See [`store::GoalStore::satisfy_criterion`].
//!
//! **A capability the session relies on is a claim with provenance.** Enabling a
//! provider feature because a *related* model is documented to have it is a guess,
//! and a guess written into configuration and then reported as success is
//! indistinguishable afterwards from a checked fact. The [`capability`] ledger
//! records each claim with how it is known — a cited document, an observed probe, an
//! inference, or nothing — and a goal that changes the workspace cannot complete
//! while it rests on one of the last two. See
//! [`store::GoalStore::record_capability_claim`].
//!
//! **The Markdown document is a projection, and the conflict rule is fixed.** The
//! goal is also rendered to `.zuno/goal/<sessionID>.md` for a human to read
//! and edit. SQL stays authoritative for the status, the budget and the counters;
//! the document is authoritative for the objective text, and nothing else. An
//! edit outside that one region is refused *and reported inside the document*,
//! because a file that could set the status would let an editor left open on a
//! stale copy resurrect a completed goal by saving. See [`projection`].
//!
//! ```
//! use zuno_goal::{GoalStore, ModelStatus};
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

pub mod budget;
pub mod capability;
pub mod continuation;
pub mod error;
pub mod pause;
pub mod projection;
pub mod retry;
pub mod spill;
pub mod status;
pub mod store;
pub mod tools;

pub use crate::budget::{GoalBudgetPolicy, SOFT_RESERVE_DIVISOR};
pub use crate::capability::{
    CAPABILITY_CLAIM_TABLE, CapabilityClaim, CapabilityClaimOutcome, CapabilityClaimState,
    NewCapabilityClaim, UnverifiedCapability,
};
pub use crate::continuation::{
    BLOCKED_TURN_THRESHOLD, BlockedAudit, ContinuationAttempt, ContinuationSuppression,
    GoalContinuation, GoalTurnMode, GoalTurnOutcome, PreparedContinuation, QueuedUserInput,
    render_goal_context,
};
pub use crate::error::GoalError;
pub use crate::pause::{GoalPauseReason, GoalPauseState, InteractionPolicy};
pub use crate::projection::{
    Check, Document, Edited, Field, GITIGNORE_SNIPPET, GOAL_DIRECTORY, GoalProjection,
    IGNORE_PATTERN, Ingest, MAX_CRITERION_CHARS, Notes, OBJECTIVE_BEGIN, OBJECTIVE_END,
    PROJECT_DIRECTORY, Refusal, RejectedEdit, document_path, parse, render,
};
pub use crate::retry::{
    DEFAULT_GOAL_RETRY_INITIAL_DELAY, DEFAULT_GOAL_RETRY_JITTER_PERCENT,
    DEFAULT_GOAL_RETRY_MAX_DELAY, DEFAULT_GOAL_RETRY_POLL_INTERVAL, GoalBlockReason,
    GoalFailureDisposition, GoalRetryPolicy, GoalRetryPolicyError, GoalRetryReason, GoalRetryState,
    GoalTerminalFailure,
};
pub use crate::spill::{
    MAX_OBJECTIVE_CHARS, OBJECTIVE_FILE_NAME, OBJECTIVE_POINTER_PREFIX, OBJECTIVE_POINTER_SUFFIX,
};
pub use crate::status::{GoalStatus, ModelStatus, StatusOwner, SystemStatus};
pub use crate::store::{
    AUXILIARY_SCHEMA, CriterionOutcome, FailureStreak, Goal, GoalCreation, GoalCriterion,
    GoalCriterionStatus, GoalHistoryEntry, GoalHumanRequestOrigin, GoalKind, GoalStore,
    OBJECTIVE_SPILL_DIRECTORY, SCHEMA, TABLE, UsageRecorded, default_spill_dir,
};
pub use crate::tools::{
    CAPABILITY_CLAIM_TOOL_ID, CREATE_GOAL_TOOL_ID, CapabilityClaimParams, CapabilityClaimTool,
    CreateGoalParams, CreateGoalTool, GET_GOAL_TOOL_ID, GetGoalParams, GetGoalTool,
    GoalInputOption, GoalRequestInputParams, GoalRequestInputTool, REQUEST_GOAL_INPUT_TOOL_ID,
    SatisfiedCriterion, UPDATE_GOAL_TOOL_ID, UpdateGoalParams, UpdateGoalStatus, UpdateGoalTool,
    WaivedCriterion, goal_from_metadata, goal_tools,
};

#[cfg(test)]
#[path = "retry_tests.rs"]
mod retry_tests;
