//! Why resolving a provider from the registry failed.
//!
//! The three variants exist because they call for three different actions from
//! three different people, and a single "could not get a provider" answer would
//! send all three to the wrong place:
//!
//! - [`RegistryError::NotRegistered`] is a **bug in this workspace**. A key was
//!   asked for and the composition root never wired it. Nothing a user can
//!   configure will fix it.
//! - [`RegistryError::Unavailable`] is a **user-facing state**. The provider is
//!   wired correctly and declined to construct — no credential, wrong platform,
//!   half-filled configuration. The fix is a login or a config edit.
//! - [`RegistryError::Construction`] is a **runtime failure inside the
//!   provider**. It is wired, it wanted to construct, and something broke; the
//!   cause chain carries what.
//!
//! The reference implementation this registry is modelled on collapses the first
//! two: `.get(key).cloned()?` and a factory returning `None` both surface as a
//! bare `Option::None`, and the wrapper that notices logs the same
//! "composition root must call register_external_provider()" warning either way
//! (`jcode`). So a
//! user with no GitHub token is told the *program* is miswired. Keeping the two
//! apart is the point of having two registration forms at all.

use crate::registry::provider::Provider;
use std::sync::Arc;
use zuno_error::{ProviderError, Recoverable, Recovery};

/// A failure to obtain a provider instance for a registry key.
///
/// Variants are recovery classes, per the rule in `zuno-error`: a caller decides
/// what to do by matching, never by inspecting rendered text. The `Display`
/// strings are for the human reading a terminal, and the
/// [`NotRegistered`](RegistryError::NotRegistered) one deliberately names both
/// the key and the function that must be called, because the person who sees it
/// is the person who has to fix the wiring.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No factory was registered for this key.
    ///
    /// A wiring bug, not a configuration problem. The message names the missing
    /// key and the composition root that must register it, so the report is
    /// actionable without a debugger.
    #[error(
        "provider `{provider}` is not registered; \
         the composition root must call ProviderRegistry::register() or \
         ProviderRegistry::register_fallible() for `{provider}` at startup"
    )]
    NotRegistered { provider: String },

    /// A factory is registered and declined to construct.
    ///
    /// The provider is wired correctly; it cannot run in this environment yet.
    #[error("provider `{provider}` is unavailable: {reason}")]
    Unavailable {
        provider: String,
        reason: Unavailable,
    },

    /// A registered factory ran and failed.
    #[error("provider `{provider}` failed to construct")]
    Construction {
        provider: String,
        #[source]
        source: ProviderError,
    },
}

impl RegistryError {
    /// The provider key this failure is about.
    ///
    /// Every variant carries it, so a reporter never has to scrape the rendered
    /// message to find out which of several configured providers failed.
    #[must_use]
    pub fn provider(&self) -> &str {
        match self {
            Self::NotRegistered { provider }
            | Self::Unavailable { provider, .. }
            | Self::Construction { provider, .. } => provider,
        }
    }

    /// True when this failure means the workspace forgot to wire the provider.
    ///
    /// Lets a startup audit separate "tell the developer" from "tell the user"
    /// without matching on the variant at every call site.
    #[must_use]
    pub const fn is_wiring_bug(&self) -> bool {
        match self {
            Self::NotRegistered { .. } => true,
            Self::Unavailable { .. } | Self::Construction { .. } => false,
        }
    }
}

impl Recoverable for RegistryError {
    /// Mapped so the agent loop can treat a registry failure like any other:
    ///
    /// - a missing credential asks for re-authentication, which is exactly what
    ///   the user must do;
    /// - a wiring bug, an unsupported platform and an incomplete configuration
    ///   all fail, because retrying re-runs the same absent registration or the
    ///   same missing setting;
    /// - a construction failure defers to the provider's own classification, so
    ///   a `503` during construction is still retryable.
    fn recovery(&self) -> Recovery {
        match self {
            Self::NotRegistered { .. } => Recovery::Fail,
            Self::Unavailable { reason, .. } => match reason {
                Unavailable::MissingCredential => Recovery::Reauthenticate,
                Unavailable::UnsupportedPlatform | Unavailable::IncompleteConfiguration => {
                    Recovery::Fail
                }
            },
            Self::Construction { source, .. } => Recoverable::recovery(source),
        }
    }
}

impl From<RegistryError> for ProviderError {
    /// Lifts a registry failure into the workspace taxonomy without losing the
    /// distinction the registry just made.
    ///
    /// A missing credential becomes [`ProviderError::Auth`], which already names
    /// the provider whose credentials to refresh. Everything else becomes
    /// [`ProviderError::Fatal`] with the registry error in `#[source]` position,
    /// so `{:#}` rendering still reaches the "composition root must call…" text
    /// that says how to fix it. A construction failure passes through
    /// unchanged — re-classifying it here would discard the provider's own
    /// judgement.
    fn from(error: RegistryError) -> Self {
        match error {
            RegistryError::Construction { source, .. } => source,
            RegistryError::Unavailable {
                ref provider,
                reason: Unavailable::MissingCredential,
            } => Self::Auth {
                provider: provider.clone(),
                source: Some(Box::new(error)),
            },
            RegistryError::NotRegistered { .. } | RegistryError::Unavailable { .. } => {
                Self::Fatal {
                    status: None,
                    source: Some(Box::new(error)),
                }
            }
        }
    }
}

/// Why a registered factory declined to construct its provider.
///
/// A fallible factory returns one of these instead of a free-form string so the
/// caller can act: only [`MissingCredential`](Unavailable::MissingCredential)
/// warrants pushing the user through a login flow, and only it is worth
/// re-checking after one.
///
/// All three are visible in the oracle. Copilot's loader needs a GitHub token
/// (`MissingCredential`), Azure's needs a resource name assembled from config,
/// env and auth before it can build an endpoint (`IncompleteConfiguration`), and
/// a provider family whose signing or transport stack is absent from a given
/// build cannot run at all (`UnsupportedPlatform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// No credential is stored for the provider.
    #[error("no credential is stored for it")]
    MissingCredential,

    /// The provider cannot run on this platform or in this build.
    #[error("it is not supported on this platform or build")]
    UnsupportedPlatform,

    /// Required configuration is missing or half-filled.
    #[error("its configuration is incomplete")]
    IncompleteConfiguration,
}

/// What a fallible factory returns.
///
/// `Ok(Some(provider))` constructed, `Ok(None)`… is deliberately **not** a case:
/// declining requires naming a reason, because an unexplained decline is what
/// makes "not configured" and "we forgot to wire it" look alike in a log. The
/// registry converts the reason into [`RegistryError::Unavailable`].
pub type FactoryOutcome = Result<Arc<dyn Provider>, Declined>;

/// A factory's decision not to construct.
///
/// Either the provider is unavailable in this environment, which is a state, or
/// building it failed, which is an error. Both are distinct from the key being
/// absent from the registry entirely.
#[derive(Debug)]
pub enum Declined {
    /// The provider is wired but cannot run here.
    Unavailable(Unavailable),
    /// Construction was attempted and failed.
    Failed(ProviderError),
}

impl Declined {
    /// Attach the provider key the registry was resolving.
    ///
    /// The factory does not have to repeat the key it was registered under; the
    /// registry knows it and stamps it on the way out, which is why every
    /// [`RegistryError`] variant can carry one.
    pub(crate) fn into_error(self, provider: &str) -> RegistryError {
        match self {
            Self::Unavailable(reason) => RegistryError::Unavailable {
                provider: provider.to_owned(),
                reason,
            },
            Self::Failed(source) => RegistryError::Construction {
                provider: provider.to_owned(),
                source,
            },
        }
    }
}

impl From<Unavailable> for Declined {
    fn from(reason: Unavailable) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<ProviderError> for Declined {
    fn from(error: ProviderError) -> Self {
        Self::Failed(error)
    }
}
