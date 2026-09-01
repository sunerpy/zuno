//! Typed error taxonomy shared by every crate; recovery decisions read data, never rendered messages.
//!
//! # The rule
//!
//! **A recovery decision is answered by an error's type and fields. Never by its
//! text.** If a caller has to call `to_string()` to work out what to do next, the
//! taxonomy has failed and the fix belongs here, not at the call site.
//!
//! # Why the rule exists
//!
//! Two Rust agents studied while planning this workspace both lost the ability to
//! recover correctly because their errors carried no data, and both ended up
//! parsing rendered text to make control-flow decisions:
//!
//! - `try_auto_compact_after_context_limit(&e.to_string())` — deciding whether to
//!   compact a conversation by searching a formatted message.
//! - `is_retryable_error(&message.to_lowercase())` — a chain of
//!   `contains("503 service unavailable")`, `contains("rate limit")`,
//!   `contains("overloaded")` checks. Because the message was all that survived,
//!   the `Retry-After` value the provider sent was gone, and the retry loop could
//!   not honour the delay it had been given.
//! - `RuntimeError { message: String }` — a single opaque error for an entire
//!   runtime, which forced the layer above it to re-derive structure from prose:
//!   classify by message prefix, then recover a tool name, a path, and a CLI flag
//!   by scraping the same string the error had been built from.
//!
//! Every one of those is a data-loss bug wearing an error's clothes. The
//! information existed at the point of failure and was discarded on the way up.
//!
//! # What that means concretely
//!
//! - **Variants are recovery classes.** [`ProviderError::RateLimited`] exists
//!   because "wait and retry" is a distinct action;
//!   [`ProviderError::ContextLimit`] exists because "make the request smaller" is
//!   a different one. Two failures that call for the same action can share a
//!   variant; two that do not, cannot.
//! - **Anything a decision needs is a field.** `retry_after`, `used_tokens`,
//!   `elapsed`, `status` — carried as typed data, propagated from the wire.
//! - **There is no catch-all variant.** No `Other(String)`, no
//!   `Unknown { message }`. A catch-all lets an author report a failure without
//!   classifying it, and once it exists everyone reaches for it. If a failure does
//!   not fit, add a variant; the exhaustive matches will point at every site that
//!   has to be reconsidered.
//! - **The enums are not `#[non_exhaustive]`.** Every consumer is in this
//!   workspace, so exhaustive matching is a feature: a new variant breaks every
//!   recovery site until someone decides what it means. `#[non_exhaustive]` would
//!   force a `_ =>` arm and route new failures to whatever the wildcard happened
//!   to do.
//! - **`String` fields are provenance or payload, never signal.** A `String` is
//!   allowed when it is an identifier the wire supplied (`provider`, `tool`,
//!   `table`, a path) or text a peer supplied verbatim for display (the
//!   `provider_text` on [`ProviderError::Refused`]). It is never a description
//!   this workspace composed for another layer to parse.
//! - **Causes chain, they do not classify.** [`BoxSource`] appears only in
//!   `#[source]` position on a variant that has already classified the failure.
//!   See [`source`] for why that is not the catch-all this crate forbids.
//! - **Library crates do not use the `anyhow` crate.** A dynamically typed error
//!   erases exactly the structure this taxonomy exists to preserve. Only the two
//!   crates at the edges are exempt, where a failure is about to be printed and
//!   the process is about to exit: `zuno-cli` and `zuno-testkit`. A test enforces
//!   this; see `tests/no_anyhow_in_libraries.rs`.
//!
//! # Using it
//!
//! Each domain has its own error type, and [`Error`] aggregates them for code that
//! spans domains. [`Recoverable`] is the generic entry point; the concrete types
//! also expose the same methods inherently, so a caller holding a
//! [`ProviderError`] needs no import:
//!
//! ```
//! use zuno_error::{ProviderError, Recovery};
//! use std::time::Duration;
//!
//! let err = ProviderError::RateLimited {
//!     retry_after: Some(Duration::from_secs(30)),
//! };
//!
//! // The delay the provider asked for, read from a field.
//! assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
//!
//! // The full decision, checked by the compiler.
//! match err.recovery() {
//!     Recovery::Retry { after } => assert_eq!(after, Some(Duration::from_secs(30))),
//!     Recovery::Compact | Recovery::Reauthenticate | Recovery::Fail => unreachable!(),
//! }
//! ```

mod config;
mod db;
mod learning;
mod lsp;
mod mcp;
mod plugin;
mod provider;
mod recovery;
pub mod source;
mod tool;

pub use crate::config::{ConfigError, ConfigIssue};
pub use crate::db::DbError;
pub use crate::learning::LearningError;
pub use crate::lsp::LspError;
pub use crate::mcp::McpError;
pub use crate::plugin::PluginError;
pub use crate::provider::{ProviderError, ProviderProtocolFailure, ProviderStreamFailure};
pub use crate::recovery::{Recoverable, Recovery};
pub use crate::source::BoxSource;
pub use crate::tool::ToolError;

use std::time::Duration;

/// Every failure this workspace produces, aggregated for code that spans domains.
///
/// Prefer the specific type in a function signature when a function can only fail
/// one way — `Result<Config, ConfigError>` tells a caller more than
/// `Result<Config, Error>` and lets the compiler prove the caller handled every
/// case. This aggregate is for the layers that genuinely span domains: the agent
/// loop, the server, the CLI boundary.
///
/// There is deliberately no `Io` variant and no catch-all. An I/O failure always
/// happens *while doing something*, and that something is the domain that owns it
/// — [`ConfigError::Io`] names the file it could not read, which a bare
/// `Error::Io` could not.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error(transparent)]
    Mcp(#[from] McpError),
    #[error(transparent)]
    Lsp(#[from] LspError),
    #[error(transparent)]
    Learning(#[from] LearningError),
}

/// The workspace result type, defaulting to the aggregate [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The action this failure calls for, delegated to the domain that produced it.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when the identical operation may succeed on another attempt.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// The delay the peer asked for, if any layer named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        Recoverable::recovery(self).retry_after()
    }

    /// The provider failure inside this error, if that is what it is.
    ///
    /// Lets the agent loop reach provider-specific data — token counts, the
    /// refusing provider's own wording — without a downcast and without
    /// re-classifying.
    #[must_use]
    pub fn as_provider(&self) -> Option<&ProviderError> {
        match self {
            Self::Provider(e) => Some(e),
            Self::Config(_)
            | Self::Tool(_)
            | Self::Db(_)
            | Self::Plugin(_)
            | Self::Mcp(_)
            | Self::Lsp(_)
            | Self::Learning(_) => None,
        }
    }
}

impl Recoverable for Error {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Config(e) => Recoverable::recovery(e),
            Self::Provider(e) => Recoverable::recovery(e),
            Self::Tool(e) => Recoverable::recovery(e),
            Self::Db(e) => Recoverable::recovery(e),
            Self::Plugin(e) => Recoverable::recovery(e),
            Self::Mcp(e) => Recoverable::recovery(e),
            Self::Lsp(e) => Recoverable::recovery(e),
            Self::Learning(e) => Recoverable::recovery(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn aggregation_preserves_the_recovery_decision() {
        let provider = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        let expected = provider.recovery();
        let aggregated = Error::from(provider);

        assert_eq!(aggregated.recovery(), expected);
        assert_eq!(aggregated.retry_after(), Some(Duration::from_secs(30)));
        assert!(aggregated.is_retryable());
    }

    #[test]
    fn aggregation_is_transparent_in_display_and_source() {
        let inner = ToolError::Failed {
            tool: "shell".to_owned(),
            source: Box::new(std::io::Error::other("exit status 1")),
        };
        let inner_text = inner.to_string();
        let aggregated = Error::from(inner);

        assert_eq!(aggregated.to_string(), inner_text);
        assert_eq!(
            aggregated.source().map(ToString::to_string).as_deref(),
            Some("exit status 1")
        );
    }

    #[test]
    fn as_provider_reaches_provider_data_without_downcasting() {
        let aggregated = Error::from(ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        });

        let Some(ProviderError::ContextLimit {
            limit_tokens,
            used_tokens,
        }) = aggregated.as_provider()
        else {
            panic!("aggregated a ContextLimit, got something else back");
        };
        assert_eq!(*limit_tokens, Some(200_000));
        assert_eq!(*used_tokens, Some(214_311));

        let other = Error::from(DbError::Busy { retry_after: None });
        assert!(other.as_provider().is_none());
    }

    #[test]
    fn every_domain_error_converts_into_the_aggregate() {
        let errors: Vec<Error> = vec![
            ConfigError::RemoteAuth {
                url: "https://example.invalid/c.json".to_owned(),
                remote: "origin".to_owned(),
            }
            .into(),
            ProviderError::RateLimited { retry_after: None }.into(),
            ToolError::NotFound {
                tool: "shell".to_owned(),
            }
            .into(),
            DbError::Busy { retry_after: None }.into(),
            PluginError::IncompatibleApi {
                plugin: "p".to_owned(),
                required: "2".to_owned(),
                provided: "1".to_owned(),
            }
            .into(),
            McpError::Timeout {
                server: "s".to_owned(),
                elapsed: Duration::from_secs(1),
            }
            .into(),
            LspError::Exited {
                server: "s".to_owned(),
                code: None,
            }
            .into(),
        ];
        assert_eq!(errors.len(), 7, "one conversion per domain error type");
        for e in &errors {
            assert!(!e.to_string().is_empty(), "every error renders something");
        }
    }

    /// A fat error variant makes every `Result` in the workspace fat, since these
    /// types are returned from nearly every function. Clippy's `result_large_err`
    /// fires at 128 bytes; this asserts the same budget up front so the crate that
    /// grows past it finds out here rather than as a warning in an unrelated build.
    #[test]
    fn the_aggregate_error_stays_small_enough_to_return_by_value() {
        let size = size_of::<Error>();
        assert!(
            size <= 128,
            "Error grew to {size} bytes; box the offending variant's payload"
        );
    }
}
