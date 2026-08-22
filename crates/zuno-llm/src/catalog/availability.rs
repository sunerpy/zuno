//! Why a provider is available — and the three independent reasons it can be.
//!
//! # Three sources, not one predicate
//!
//! The oracle establishes availability in three separate passes, and they are not
//! interchangeable:
//!
//! | source | oracle | what it means |
//! |---|---|---|
//! | [`AvailabilitySource::EnvVar`] | `provider.ts:1523-1533` | one of the provider's declared env vars is set in the process |
//! | [`AvailabilitySource::StoredApiKey`] | `provider.ts:1536-1546` | `auth.json` holds a **`type: "api"`** credential |
//! | [`AvailabilitySource::NativeOauth`] | Zuno auth registry | a native login method and request consumer both exist |
//! | [`AvailabilitySource::ConfigBlock`] | `provider.ts:1588-1595` | the user wrote a `provider.<id>` block at all |
//!
//! Collapsing these into one boolean loses information a user needs. "Set
//! `ANTHROPIC_API_KEY` or run `opencode auth login`" is a different sentence from
//! "this provider only exists because your config declares it", and a caller that
//! cannot tell them apart cannot write either.
//!
//! # Each of the three, verified in isolation against the 1.18.12 binary
//!
//! With a pinned catalog (`ZUNO_MODELS_PATH`), an isolated `HOME`, and nothing
//! else set, `opencode models` printed **nothing**. Adding exactly one thing at a
//! time, each on its own, made exactly one provider appear:
//!
//! ```text
//! DEEPSEEK_API_KEY=sk-x                          → deepseek/*     (EnvVar)
//! auth.json {"deepseek":{"type":"api",...}}      → deepseek/*     (StoredApiKey)
//! config {"provider":{"groq":{}}}                → groq/*         (ConfigBlock)
//! ```
//!
//! `tests/catalog_resolution.rs` asserts all three separately — one test per
//! source, each supplying exactly one — so a later refactor that merges them into
//! a single `is_available()` predicate breaks one of them. Each is also covered
//! against the real binary in `tests/catalog_differential.rs`.
//!
//! # The one that surprises people: a stored OAuth credential is not enough
//!
//! `provider.ts:1540` gates the auth pass on `provider.type === "api"`. A stored
//! **`oauth`** credential does *not* make a provider available through this path.
//! Verified: `auth.json` holding `{"mistral":{"type":"oauth",…}}` with a pinned
//! catalog produced an empty model list, where the same file with `type: "api"`
//! produced mistral's models.
//!
//! Zuno follows the same ownership boundary without importing a JavaScript
//! `custom()` loader. [`AvailabilitySource::StoredOauth`] remains insufficient,
//! while [`AvailabilitySource::NativeOauth`] is recorded only when the supplied
//! [`zuno_auth::LoginMethodRegistry`] says a native OAuth implementation is
//! mounted for that provider. A custom provider therefore cannot become
//! selectable merely because its credential happens to use the OAuth JSON shape.
//!
//! # How this maps onto the registry's two diagnostics
//!
//! `registry.rs` distinguishes "unavailable" (a user-facing state) from "not
//! registered" (a wiring bug). This module produces only the former:
//! [`Availability::unavailable_reason`] returns
//! [`zuno_llm::registry::Unavailable`](crate::registry::Unavailable), never a
//! `NotRegistered`. A provider absent from the *registry* is a fact about this
//! build's composition root and is unknowable from a catalog, config file and
//! `auth.json`. Keeping the two apart is what stops a user with no API key from
//! being told the program is miswired.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zuno_auth::{Credential, LoginMethodRegistry};

use crate::registry::Unavailable;

/// One reason a provider is (or might be) usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AvailabilitySource {
    /// A declared environment variable is set — `provider.ts:1527`.
    EnvVar {
        /// The variable that was found, so a message can name it.
        name: String,
    },
    /// `auth.json` holds a `type: "api"` credential — `provider.ts:1540-1545`.
    StoredApiKey,
    /// `auth.json` holds a `type: "oauth"` credential.
    ///
    /// Recorded but **not** sufficient on its own; see the module docs.
    StoredOauth,
    /// A stored OAuth credential whose provider has a native OAuth implementation.
    NativeOauth,
    /// `auth.json` holds a `type: "wellknown"` credential.
    ///
    /// Recorded but not sufficient on its own, for the same reason as OAuth.
    StoredWellKnown,
    /// The user's config declares a `provider.<id>` block — `provider.ts:1588-1595`.
    ///
    /// Sufficient by itself, and with no credential of any kind: verified with
    /// `{"provider":{"groq":{}}}` and an otherwise empty environment.
    ConfigBlock,
}

impl AvailabilitySource {
    /// True when this source alone makes the provider selectable.
    ///
    /// The two stored-credential shapes that are *not* an API key return `false`
    /// here on purpose. See the module docs.
    #[must_use]
    pub const fn is_sufficient(&self) -> bool {
        match self {
            Self::EnvVar { .. } | Self::StoredApiKey | Self::NativeOauth | Self::ConfigBlock => {
                true
            }
            Self::StoredOauth | Self::StoredWellKnown => false,
        }
    }
}

/// Every reason a provider is available, in the order the oracle establishes them.
///
/// Order is preserved because it is the oracle's `source` precedence: env, then
/// stored auth, then config, with the last writer winning
/// (`provider.ts:1523`, `:1536`, `:1588`). [`Availability::effective_source`]
/// reports that winner without a caller re-deriving the ladder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Availability {
    /// The sources that fired, in precedence order.
    pub sources: Vec<AvailabilitySource>,
}

impl Availability {
    /// An empty availability: no source fired.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Record a source, keeping precedence order and rejecting duplicates.
    pub fn record(&mut self, source: AvailabilitySource) {
        if !self.sources.contains(&source) {
            self.sources.push(source);
        }
    }

    /// True when at least one *sufficient* source fired.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.sources.iter().any(AvailabilitySource::is_sufficient)
    }

    /// The source that wins, which is the last sufficient one recorded.
    ///
    /// Matches the oracle, where each pass overwrites `source` and config runs
    /// last, so a provider with both an env var and a config block reports
    /// `config`.
    #[must_use]
    pub fn effective_source(&self) -> Option<&AvailabilitySource> {
        self.sources
            .iter()
            .rev()
            .find(|source| source.is_sufficient())
    }

    /// True when a stored OAuth credential exists for this provider.
    ///
    /// The signal a provider crate needs: this provider has a login the generic
    /// path cannot act on. Not availability.
    #[must_use]
    pub fn has_oauth_credential(&self) -> bool {
        self.sources.iter().any(|source| {
            matches!(
                source,
                AvailabilitySource::StoredOauth | AvailabilitySource::NativeOauth
            )
        })
    }

    /// Why this provider is not selectable, in the registry's vocabulary.
    ///
    /// `None` when it is. Returns [`Unavailable::MissingCredential`] when nothing
    /// fired, and [`Unavailable::IncompleteConfiguration`] when a credential
    /// exists but is of a shape the generic path cannot use — the distinction
    /// between "log in" and "this needs its provider's own flow".
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<Unavailable> {
        if self.is_available() {
            return None;
        }
        if self.sources.is_empty() {
            Some(Unavailable::MissingCredential)
        } else {
            Some(Unavailable::IncompleteConfiguration)
        }
    }
}

/// Which of a provider's declared env vars is set, if any.
///
/// `provider.ts:1527` takes the **first** declared variable with a value, in the
/// catalog's declared order, not the first one it happens to find in the
/// environment. Order matters when a provider declares two aliases.
#[must_use]
pub fn env_var_source(
    declared: &[String],
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<AvailabilitySource> {
    declared
        .iter()
        .find(|name| lookup(name).is_some_and(|value| !value.is_empty()))
        .map(|name| AvailabilitySource::EnvVar { name: name.clone() })
}

/// The source a stored credential contributes, by shape.
#[must_use]
pub fn credential_source(
    provider: &str,
    credential: &Credential,
    login_methods: Option<&LoginMethodRegistry>,
) -> AvailabilitySource {
    match credential {
        Credential::Api { .. } => AvailabilitySource::StoredApiKey,
        Credential::Oauth { .. }
            if login_methods.is_some_and(|methods| methods.supports_oauth(provider)) =>
        {
            AvailabilitySource::NativeOauth
        }
        Credential::Oauth { .. } => AvailabilitySource::StoredOauth,
        Credential::WellKnown { .. } => AvailabilitySource::StoredWellKnown,
    }
}

/// Sources contributed by stored credentials, keyed by provider id.
#[must_use]
pub fn credential_sources(
    credentials: &BTreeMap<String, Credential>,
    login_methods: Option<&LoginMethodRegistry>,
) -> BTreeMap<String, AvailabilitySource> {
    credentials
        .iter()
        .map(|(provider, credential)| {
            (
                provider.clone(),
                credential_source(provider, credential, login_methods),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_auth::Secret;

    #[test]
    fn an_api_key_credential_is_sufficient_and_an_oauth_one_is_not() {
        // provider.ts:1540 gates on `type === "api"`. Verified against 1.18.12:
        // a stored oauth credential alone yields an empty model list.
        let api = Credential::Api {
            key: Secret::new("sk-x"),
            metadata: None,
        };
        let oauth = Credential::Oauth {
            refresh: Secret::new("r"),
            access: Secret::new("a"),
            expires: 0,
            account_id: None,
            enterprise_url: None,
        };
        assert!(credential_source("openai", &api, None).is_sufficient());
        assert!(!credential_source("openai", &oauth, None).is_sufficient());
        let methods = LoginMethodRegistry::native();
        assert!(credential_source("openai", &oauth, Some(&methods)).is_sufficient());
        assert!(!credential_source("myopenai", &oauth, Some(&methods)).is_sufficient());
    }

    #[test]
    fn an_oauth_credential_alone_reports_incomplete_not_missing() {
        let mut availability = Availability::none();
        availability.record(AvailabilitySource::StoredOauth);
        assert!(!availability.is_available());
        assert!(availability.has_oauth_credential());
        assert_eq!(
            availability.unavailable_reason(),
            Some(Unavailable::IncompleteConfiguration),
            "a credential that exists but needs its provider's flow is not a \
             missing credential"
        );
    }

    #[test]
    fn nothing_at_all_reports_a_missing_credential() {
        assert_eq!(
            Availability::none().unavailable_reason(),
            Some(Unavailable::MissingCredential)
        );
    }

    #[test]
    fn config_wins_over_env_because_it_is_applied_last() {
        let mut availability = Availability::none();
        availability.record(AvailabilitySource::EnvVar {
            name: "DEEPSEEK_API_KEY".to_owned(),
        });
        availability.record(AvailabilitySource::ConfigBlock);
        assert_eq!(
            availability.effective_source(),
            Some(&AvailabilitySource::ConfigBlock)
        );
        assert!(availability.unavailable_reason().is_none());
    }

    #[test]
    fn the_first_declared_env_var_wins_not_the_first_found() {
        let declared = vec!["PRIMARY_KEY".to_owned(), "LEGACY_KEY".to_owned()];
        let both = |name: &str| match name {
            "PRIMARY_KEY" | "LEGACY_KEY" => Some("set".to_owned()),
            _ => None,
        };
        assert_eq!(
            env_var_source(&declared, &both),
            Some(AvailabilitySource::EnvVar {
                name: "PRIMARY_KEY".to_owned()
            })
        );
    }

    #[test]
    fn an_empty_env_var_does_not_count() {
        let declared = vec!["EMPTY".to_owned()];
        let empty = |_: &str| Some(String::new());
        assert_eq!(env_var_source(&declared, &empty), None);
    }

    #[test]
    fn recording_the_same_source_twice_keeps_one() {
        let mut availability = Availability::none();
        availability.record(AvailabilitySource::StoredApiKey);
        availability.record(AvailabilitySource::StoredApiKey);
        assert_eq!(availability.sources.len(), 1);
    }
}
