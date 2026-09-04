//! Typed failures from a search.
//!
//! Every variant names the thing that went wrong precisely enough that the caller
//! can decide whether the *model* can fix it (a bad regex, a path that is a file)
//! or whether it cannot (a missing root, a cancelled process). `zuno-tools` maps these
//! onto [`zuno_error::ToolError`] on that basis, which is why there is no
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

    /// The interrupt fired while `rg` was running.
    ///
    /// Raised rather than returning the partial results collected so far: a
    /// truncated result set that does not say it is truncated would be read by the
    /// model as "that is all there is".
    #[error("search was cancelled")]
    Cancelled,

    /// No supported official `rg` executable is available.
    #[error("{message}")]
    Unavailable { message: String },

    /// The official `rg` binary could not be started.
    #[error("spawning the ripgrep binary {program} failed")]
    Spawn {
        /// The binary that could not be started.
        program: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The official `rg` binary ran and failed for a reason other than a bad pattern.
    #[error("ripgrep failed: {message}")]
    Ripgrep {
        /// `rg`'s own diagnostics, trimmed.
        message: String,
    },

    /// `rg` refused the invocation and therefore searched nothing.
    ///
    /// Model-correctable, and deliberately distinct from [`SearchError::Ripgrep`] so
    /// it is not laundered into the not-correctable bucket: every part of the
    /// invocation Zuno does not fix itself came from the call, so a refusal names the
    /// call's own pattern, include glob, or path, and `message` is the backend's
    /// advice about which. Distinct from the two pattern failures because Zuno cannot
    /// always say *which* input `rg` objected to.
    #[error("ripgrep rejected the search: {message}")]
    Rejected {
        /// `rg`'s own diagnostic, or Zuno's description of what was never searched.
        message: String,
    },

    /// The search produced more output than Zuno will buffer, and was abandoned.
    ///
    /// Model-correctable: a narrower pattern, path, or include filter is the only
    /// thing that changes the outcome, which is what `message` tells the model. Typed
    /// apart from [`SearchError::Ripgrep`] because the backend did not fail — Zuno
    /// stopped it — and because a caller that reported this as unfixable would turn a
    /// one-token correction into a permanent tool failure.
    #[error("{message}")]
    TooBroad {
        /// What the limit was and how to get under it.
        message: String,
    },
}

impl SearchError {
    /// Whether the model can fix this by issuing a different call.
    ///
    /// The two pattern failures are the model's to correct, as are an invocation the
    /// backend refused and a search too broad to buffer: in all four the next call can
    /// carry a different pattern, filter, or path. A missing root, a cancellation, or
    /// a backend failure are not: nothing the model can write in the next call changes
    /// them.
    ///
    /// This predicate is the only thing `zuno-tools` consults when it decides between
    /// `ToolError::InvalidArgs` and the deliberately-non-retryable `ToolError::Failed`,
    /// so a failure whose own message tells the model what to change must be listed
    /// here or that advice is addressed to an actor the taxonomy declares powerless.
    #[must_use]
    pub fn is_model_correctable(&self) -> bool {
        matches!(
            self,
            Self::InvalidGlob { .. }
                | Self::InvalidPattern { .. }
                | Self::RootNotDirectory { .. }
                | Self::Rejected { .. }
                | Self::TooBroad { .. }
        )
    }
}
