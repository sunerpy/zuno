//! Agent definitions, presets, and the sub-agent task boundary.
//!
//! # The plan document
//!
//! [`plan_file`] owns where a session's plan lives: `<created>-<slug>.md`, under
//! `<worktree>/.opencode/plans` when the project is a repository and under the
//! global data directory when it is not (`session/session.ts:331-335`). The naming
//! is the contract — the `plan` agent is told the path and writes the file with the
//! ordinary file tools — so it is stated once, here, and asserted rather than
//! reconstructed at each call site.
//!
//! # Models
//!
//! [`model_policy`] decides which model an agent runs on, and it decides it the way
//! `.omo/refs/omo-slim/src/config/constants.ts:31-41` does: nothing at all by
//! default, so every agent inherits the session model. A user selects a named
//! *preset* — a flat `{agent → {model, variant}}` map — and may override any single
//! agent on top of it. No model id appears anywhere in this crate, and a test walks
//! the sources to keep that true; the reason is the table that inversion replaces,
//! `oh-my-openagent/dist/index.js:24467` and `:24652`, whose per-agent and
//! per-category fallback chains name concrete models and the providers entitled to
//! serve them, and therefore go stale on every model release.
//!
//! # Continuing a child session
//!
//! [`continuation`] owns the state that makes `task_id` mean something. Two ids, not
//! one: a session id names a conversation and a job id names one dispatch into it, so a
//! lane can be continued repeatedly without its handles colliding — upstream reuses the
//! session id as the job id and the ambiguity is visible on a single code path
//! (`packages/opencode/src/tool/task.ts:262` against `:294`). The board it renders is
//! injected into the coordinator's context each turn, and its load-bearing rule is that
//! an `Active` lane is **not addressable**: a re-dispatch into a running lane is
//! refused, naming the lane, rather than silently amending work already in flight.
//! "Active" is derived from the engine's run registry rather than stored a second time,
//! which is why the answer is process-local and the module says so.

pub mod builtin;
pub mod continuation;
pub mod model_policy;
pub mod plan_file;
pub mod reflection;

pub use plan_file::{
    PLANS_DIRECTORY, PROJECT_DIRECTORY, PlanKey, PlanLocation, plan_path, read_plan, write_plan,
};
