//! Typed failures from a search.
//!
//! Every variant names the thing that went wrong precisely enough that the caller
//! can decide whether the *model* can fix it (a bad regex, a path that is a file)
//! or whether it cannot (a missing root, a cancelled walk). `oc-tools` maps these
//! onto [`oc_error::ToolError`] on that basis, which is why there is no
//! `Other(String)` here to launder an unclassified failure through.

use std::path::PathBuf;

/// A failure from executing a search.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The directory to search does not exist.
    #[error("search root {root} does not exist")]
    RootMissing {
        /// The path that was resolved and then not found.
        root: PathBuf,
    },

    /// The path to search is not a directory.
    #[error("search root {root} is not a directory")]
    RootNotDirectory {
        /// The path that resolved to something other than a directory.
        root: PathBuf,
    },

    /// A glob pattern could not be compiled.
    ///
    /// Model-correctable: the pattern came from the tool call.
    #[error("invalid glob pattern {pattern}: {message}")]
    InvalidGlob {
        /// The pattern as given.
        pattern: String,
        /// The compiler's complaint.
        message: String,
    },

    /// A regex pattern could not be compiled.
    ///
    /// Model-correctable, and deliberately distinct from [`SearchError::InvalidGlob`]
    /// so a caller can say which of a call's two patterns was wrong. Mirrors the
    /// oracle's `Ripgrep.InvalidPatternError` (`packages/core/src/ripgrep.ts:46-49`).
    #[error("invalid regex pattern {pattern}: {message}")]
    InvalidPattern {
        /// The pattern as given.
        pattern: String,
        /// The regex compiler's complaint.
        message: String,
    },

    /// The interrupt fired while walking or searching.
    ///
    /// Raised rather than returning the partial results collected so far: a
    /// truncated result set that does not say it is truncated would be read by the
    /// model as "that is all there is".
    #[error("search was cancelled")]
    Cancelled,

    /// Reading a file's contents failed in a way the walk could not skip past.
    #[error("reading {path} failed")]
    Read {
        /// The file being read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// A system `rg` binary was selected but could not be started.
    #[error("spawning the ripgrep binary {program} failed")]
    Spawn {
        /// The binary that could not be started.
        program: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// A system `rg` binary ran and failed for a reason other than a bad pattern.
    #[error("the ripgrep backend failed: {message}")]
    Ripgrep {
        /// `rg`'s own diagnostics, trimmed.
        message: String,
    },
}

impl SearchError {
    /// Whether the model can fix this by issuing a different call.
    ///
    /// The two pattern failures are the model's to correct. A missing root, a
    /// cancellation, or a backend failure are not: nothing the model can write in
    /// the next call changes them.
    #[must_use]
    pub fn is_model_correctable(&self) -> bool {
        matches!(
            self,
            Self::InvalidGlob { .. } | Self::InvalidPattern { .. } | Self::RootNotDirectory { .. }
        )
    }
}
