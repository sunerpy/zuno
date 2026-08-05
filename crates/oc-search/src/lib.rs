//! Content and path search over a project, honouring ignore semantics.
//!
//! # What this replaces
//!
//! The oracle downloads a ripgrep binary at first use — a GitHub release archive,
//! extracted with `tar` on Unix or `powershell.exe` on Windows, then `chmod`ed
//! (`packages/core/src/ripgrep/binary.ts:88-121`) — and shells out to it for every
//! `glob` and `grep`. This crate does the same work in process, with the crates
//! ripgrep itself is built from, so a fresh install can search offline and a search
//! is a function call rather than a process spawn.
//!
//! The [`Backend`] enum keeps a system `rg` reachable for divergence
//! investigations; it is opt-in and never chosen implicitly.
//!
//! # Ordering is part of the contract
//!
//! The oracle passes no `--sort`, so its walk is parallel and its output order is
//! whatever the threads produce. Five consecutive runs of `opencode debug rg files`
//! over one unchanged ten-file tree gave five different orders:
//!
//! ```text
//! .hidden_dir/e.ts | ignored.ts | README.md | .gitignore | ...
//! ignored.ts | README.md | .hidden_dir/e.ts | .gitignore | ...
//! .hidden_dir/e.ts | nested/deep/d.ts | ignored.ts | README.md | ...
//! .hidden_dir/e.ts | README.md | ignored.ts | .gitignore | ...
//! .hidden_dir/e.ts | .gitignore | .hidden_file.ts | nested/deep/d.ts | ...
//! ```
//!
//! There is therefore no oracle order to preserve, and "identical ordering" in the
//! acceptance criterion can only mean identical after sorting both sides. Both
//! engines here emit **path-sorted** results, which is exactly `rg --sort=path`, and
//! the differential test compares sorted-to-sorted with a set comparison that would
//! also catch a missing or extra path.
//!
//! Sorting is not a cosmetic choice. It makes truncation deterministic — "the first
//! 100 of a stable order" rather than "100 arbitrary results" — and it makes `grep`'s
//! grouped output correct, because a path whose matches are scattered through an
//! unsorted stream would head two separate groups in the rendered output.
//!
//! # Cancellation
//!
//! [`Cancellation`] is polled inside the walk, not merely before it. A search over a
//! large tree that cannot be interrupted is the hang this port exists to remove, and
//! the poll is synchronous so it works in blocking code with no runtime in scope.
//!
//! # Layout
//!
//! - [`types`] — the result and request shapes, mirrored from the oracle's schema.
//! - [`embedded`] — the in-process engine, and the flag-by-flag mapping from `rg`.
//! - [`ripgrep`] — the opt-in system-binary backend.
//! - [`backend`] — the selection between them.
//! - [`cancel`] — the interrupt a walk polls.
//! - [`error`] — typed failures.

pub mod backend;
pub mod cancel;
pub mod embedded;
pub mod error;
pub mod ripgrep;
pub mod types;

pub use crate::backend::{BACKEND_ENV, Backend};
pub use crate::cancel::{AlreadyCancelled, Cancellation, NeverCancelled};
pub use crate::embedded::{EmbeddedEngine, GIT_EXCLUDE_GLOB};
pub use crate::error::SearchError;
pub use crate::ripgrep::{RipgrepEngine, locate_ripgrep};
pub use crate::types::{
    Entry, EntryKind, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match,
    SearchResults, Submatch, normalize_relative, relative_to, truncate_utf16,
};
