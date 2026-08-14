//! Why catalog resolution fails, in the vocabulary a caller can act on.
//!
//! # The case [`CatalogError::FetchDisabled`] actually covers
//!
//! This file used to argue that `ZUNO_DISABLE_MODELS_FETCH` with no cache on
//! disk must fail immediately, because the two alternatives — hanging on a fetch
//! policy forbids, or returning an empty catalog the user meets as "no models
//! found" three screens later — are both worse. That argument is sound, and it was
//! applied one step too early.
//!
//! It is right when the user names a model **nobody defines**. It is wrong when the
//! config already specifies the provider, the model, its cost and its limits: there
//! is nothing to look up, upstream runs that config with an empty catalog
//! (`models-dev.ts:222`, verified against 1.18.12), and refusing to start it meant
//! an air-gapped user with a private gateway could not launch the binary at all.
//!
//! So the two cases are now separated rather than the error deleted.
//! [`crate::catalog::source::CatalogSource::load`] succeeds with an empty document,
//! and this variant is raised by
//! [`crate::catalog::source::LoadedCatalog::unresolved_model`] only once a model has
//! been requested and the *resolved* catalog — config merged in — does not contain
//! it. It still fails immediately, and it still names every way out: the model, the
//! flag, the source that was not contacted, the cache path that was missing, and
//! the config block that would have defined it.
//!
//! [`CatalogError::RefreshDisabled`] keeps the unconditional form, because
//! `models --refresh` *is* a request to go to the network; there is no config to
//! fall back on and silently doing nothing would be the worst possible answer.
//!
//! Variants are recovery classes, following the rule `oc-error` sets and
//! `registry.rs` already follows: a caller decides by matching, never by reading
//! rendered text. The `Display` strings are for the human at the terminal.

use std::path::PathBuf;

use oc_error::{ProviderError, Recoverable, Recovery};

/// Why the model catalog could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// A model was requested that neither the config nor any reachable catalog
    /// defines, because fetching is disabled and there was nothing on disk.
    ///
    /// Every part of the fix is in the message because the person who sees it is
    /// the person who has to choose between defining the model in config, unsetting
    /// the flag, warming the cache, and pointing at a file. The requested id leads
    /// because a message that omits it cannot be told apart from a general failure
    /// to start.
    #[error(
        "model `{requested}` is not available: no `provider` block in your \
         configuration defines it, ZUNO_DISABLE_MODELS_FETCH is set so no fetch \
         from `{origin}` was attempted, and no cached catalog exists at `{cache}`. \
         Define the provider and model under `provider` in your config, or unset \
         ZUNO_DISABLE_MODELS_FETCH to fetch the catalog, or set \
         ZUNO_MODELS_PATH to a catalog file on disk"
    )]
    FetchDisabled {
        /// The `provider/model` the user asked for.
        requested: String,
        /// The source a fetch would have gone to.
        ///
        /// Named `origin` rather than `source` because `thiserror` reserves a
        /// field called `source` for the error cause, and this is a URL.
        origin: String,
        /// The cache file that was looked for and not found.
        cache: PathBuf,
    },

    /// A refresh was asked for and policy forbids the network.
    ///
    /// Unconditional, unlike [`Self::FetchDisabled`]: refreshing the cache *is* the
    /// request, so there is no config that could satisfy it and nothing to fall
    /// back on.
    #[error(
        "the model catalog cannot be refreshed: ZUNO_DISABLE_MODELS_FETCH is \
         set, so no fetch from `{origin}` was attempted. Unset \
         ZUNO_DISABLE_MODELS_FETCH to allow it"
    )]
    RefreshDisabled {
        /// The source a fetch would have gone to.
        origin: String,
    },

    /// `ZUNO_MODELS_PATH` points at something that cannot be read.
    ///
    /// Distinct from a missing cache: an explicit path is an instruction, so
    /// failing to honour it is an error rather than a reason to look elsewhere.
    #[error("the model catalog at `{path}` (ZUNO_MODELS_PATH) could not be read")]
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
        matches!(
            self,
            Self::FetchDisabled { .. } | Self::RefreshDisabled { .. }
        )
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
            | Self::RefreshDisabled { .. }
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
