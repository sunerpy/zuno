//! Why catalog resolution fails, in the vocabulary a caller can act on.
//!
//! The failure this file exists for is the one QA asks about: `OPENCODE_MODELS_FETCH`
//! disabled with no cache on disk. The naive implementations of that are both
//! wrong — hanging on a fetch that policy forbids, or returning an empty catalog
//! and letting the user discover it as "no models found" three screens later.
//! [`CatalogError::FetchDisabled`] is the third option: fail immediately, and
//! name the cache path that was missing, the source that would have filled it,
//! and the variable to unset.
//!
//! Variants are recovery classes, following the rule `oc-error` sets and
//! `registry.rs` already follows: a caller decides by matching, never by reading
//! rendered text. The `Display` strings are for the human at the terminal.

use std::path::PathBuf;

use oc_error::{ProviderError, Recoverable, Recovery};

/// Why the model catalog could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Fetching is disabled and there is nothing on disk to fall back to.
    ///
    /// Every part of the fix is in the message because the person who sees it is
    /// the person who has to choose between unsetting the flag, warming the
    /// cache, and pointing at a file.
    #[error(
        "the model catalog is unavailable: OPENCODE_DISABLE_MODELS_FETCH is set, \
         so no fetch from `{origin}` was attempted, and no cached catalog exists \
         at `{cache}`. Unset OPENCODE_DISABLE_MODELS_FETCH to fetch it, or set \
         OPENCODE_MODELS_PATH to a catalog file on disk"
    )]
    FetchDisabled {
        /// The source a fetch would have gone to.
        ///
        /// Named `origin` rather than `source` because `thiserror` reserves a
        /// field called `source` for the error cause, and this is a URL.
        origin: String,
        /// The cache file that was looked for and not found.
        cache: PathBuf,
    },

    /// `OPENCODE_MODELS_PATH` points at something that cannot be read.
    ///
    /// Distinct from a missing cache: an explicit path is an instruction, so
    /// failing to honour it is an error rather than a reason to look elsewhere.
    #[error("the model catalog at `{path}` (OPENCODE_MODELS_PATH) could not be read")]
    ExplicitPathUnreadable {
        /// The path the variable named.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// A catalog file was read but is not a catalog.
    #[error("the model catalog at `{path}` is not valid models.dev JSON")]
    Malformed {
        /// The file that failed to parse.
        path: PathBuf,
        /// The parse failure, with its line and column.
        #[source]
        source: serde_json::Error,
    },

    /// The fetch was attempted and did not produce a catalog.
    #[error("fetching the model catalog from `{origin}` failed")]
    Fetch {
        /// The configured source, so a mirror is distinguishable from upstream.
        origin: String,
        /// The transport or status failure.
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The catalog was fetched but could not be cached.
    ///
    /// Kept separate from [`CatalogError::Fetch`] because the catalog is in hand:
    /// a caller may reasonably continue with it and only warn.
    #[error("the fetched model catalog could not be written to `{path}`")]
    CacheWrite {
        /// The cache file the write targeted.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
}

impl CatalogError {
    /// True when the catalog is absent because policy forbade fetching it.
    ///
    /// Lets a startup path tell "you told me not to fetch" apart from "the
    /// fetch failed" without matching every variant at the call site.
    #[must_use]
    pub const fn is_policy(&self) -> bool {
        matches!(self, Self::FetchDisabled { .. })
    }
}

impl Recoverable for CatalogError {
    /// Only a transport failure is worth retrying.
    ///
    /// A disabled fetch, an unreadable explicit path and malformed JSON all
    /// reproduce exactly on a second attempt; retrying them burns time and tells
    /// the user nothing new.
    fn recovery(&self) -> Recovery {
        match self {
            Self::Fetch { .. } => Recovery::Retry { after: None },
            Self::FetchDisabled { .. }
            | Self::ExplicitPathUnreadable { .. }
            | Self::Malformed { .. }
            | Self::CacheWrite { .. } => Recovery::Fail,
        }
    }
}

impl From<CatalogError> for ProviderError {
    /// Fold into the provider taxonomy for callers that speak only `oc-error`.
    ///
    /// A transport failure keeps its retryability by landing on
    /// [`ProviderError::Transient`]; everything else is [`ProviderError::Fatal`],
    /// because no amount of retrying conjures a catalog the user has forbidden
    /// fetching.
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Fetch { cause, .. } => Self::Transient {
                status: None,
                source: Some(cause),
            },
            other => Self::Fatal {
                status: None,
                source: Some(Box::new(other)),
            },
        }
    }
}
