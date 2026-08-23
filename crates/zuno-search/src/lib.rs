//! Project search through the official `rg` executable.
//!
//! Zuno intentionally does not maintain a second ripgrep-compatible walker. One
//! `rg` process answers each request; Zuno adds only typed request construction,
//! cancellation, bounded JSON decoding, stable ordering, and result shaping.

pub mod cancel;
pub mod error;
pub mod ripgrep;
pub mod types;

pub use crate::cancel::{AlreadyCancelled, Cancellation, NeverCancelled};
pub use crate::error::SearchError;
pub use crate::ripgrep::{MINIMUM_RIPGREP_MAJOR, Ripgrep};
pub use crate::types::{
    Entry, EntryKind, GlobRequest, GrepRequest, MAX_MATCH_TEXT, MAX_SUBMATCHES, Match,
    SearchResults, Submatch, normalize_relative, relative_to, truncate_utf16,
};
