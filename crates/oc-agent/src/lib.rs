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

pub mod builtin;
pub mod plan_file;

pub use plan_file::{
    PLANS_DIRECTORY, PROJECT_DIRECTORY, PlanKey, PlanLocation, plan_path, read_plan, write_plan,
};
