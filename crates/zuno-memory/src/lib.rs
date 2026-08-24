//! Resident memory: what the agent carries from one session into the next.
//!
//! A coding agent that has to be told the same thing every session is not learning
//! anything. This crate stores a small, curated, capped set of notes and freezes
//! their rendered blocks into each session's system prompt. Harness writes enter
//! [`MemoryService`] as durable candidates and reach the resident files only after
//! the configured review or promotion policy. A session keeps its frozen snapshot;
//! approved changes appear in later prompt assembly.
//!
//! # Shape
//!
//! Two [`Scope`]s, each a plain UTF-8 file of entries separated by
//! [`ENTRY_DELIMITER`] (`"\n§\n"`):
//!
//! | Scope | Location | Cap |
//! |---|---|---|
//! | [`Scope::Global`] | `$CONFIG/memory/MEMORY.md` | 2200 chars |
//! | [`Scope::Project`] | `<worktree>/.zuno/RULES.md` | 3000 chars |
//!
//! Habits that travel with the user go global; rules that belong to one repository
//! stay in it. See [`scope`] for why that is the split, and why the unit is
//! characters rather than tokens.
//!
//! # Using it
//!
//! ```no_run
//! use zuno_memory::{MemoryStore, Operation, Scope};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), zuno_memory::MemoryError> {
//! let mut rules = MemoryStore::discover(Scope::Project, Path::new("/srv/repo"))?;
//!
//! // One batch: retire two stale notes and record what supersedes them. This is
//! // accepted even when the store is exactly full, because only the *result* is
//! // measured against the cap.
//! let usage = rules.apply_batch(&[
//!     Operation::remove("uses yarn"),
//!     Operation::remove("node 18"),
//!     Operation::add("package manager is bun; the CI gate is `bun test`"),
//! ])?;
//! println!("{usage}"); // e.g. `41% — 1,240/3,000 chars`
//!
//! // The block that goes into the system prompt. Empty when the store is empty.
//! let block = rules.render_block();
//! # Ok(())
//! # }
//! ```
//!
//! # Ported from
//!
//! `hermes-agent` and its
//! `tools/threat_patterns.py`, with three deliberate divergences, each recorded at
//! its own site:
//!
//! * **The two scopes are different scopes.** The reference splits by *who the note
//!   is about*; this splits by *where the note applies*. See [`scope`].
//! * **Drift refuses instead of merging, on three signals not two.** The reference
//!   adopts a sister session's writes; this refuses them, because the block is
//!   frozen into a prompt and a silently adopted change would make the returned
//!   [`Usage`] describe content the caller never saw. See
//!   [`MemoryStore::apply_batch`] and [`error::DriftReason`].
//! * **Compatibility folding replaces NFKC.** The documented attack is covered
//!   without a Unicode-table dependency; the documented gap is unchanged. See
//!   [`threat`].
//!
//! What is carried unchanged, because the reference paid for it in production: the
//! delimiter bytes, the character cap and its rationale, the bounded filler in the
//! injection patterns, the invisible-codepoint check running *before* folding, and
//! the rule that a **successful** write returns usage and nothing else while a
//! **failed** one returns the entries. That last one is the anti-thrash rule and it
//! is the easiest to undo by accident — see [`error`].

pub mod error;
pub mod render;
pub mod scope;
pub mod service;
pub mod snapshot;
pub mod store;
pub mod threat;

pub use crate::error::{DriftReason, MemoryError};
pub use crate::render::{
    Usage, parse, render_block, render_block_with_limit, serialize, usage_of, usage_of_with_limit,
};
pub use crate::scope::{
    ENTRY_DELIMITER, GLOBAL_FILE, MEMORY_DIRECTORY, PROJECT_FILE, Scope, ScopeLimits, char_count,
};
pub use crate::service::{
    MemoryObserver, MemoryProposal, MemoryService, MemoryServiceError, PromotionPolicy, ScopePaths,
};
pub use crate::snapshot::{
    CacheConsistency, EXTERNAL_MEMORY_NOTE, ScopeEnablement, SessionMemory, assemble_system_prompt,
    fence_external_context,
};
pub use crate::store::{MemoryStore, Operation};
pub use crate::threat::{Threat, first_threat, scan_for_threats};
