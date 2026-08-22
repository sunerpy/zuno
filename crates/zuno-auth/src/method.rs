//! Provider login methods and the registry that advertises them.
//!
//! A credential shape says what is stored. A login method says how that credential
//! is obtained. Keeping those separate prevents an `oauth` JSON variant from being
//! mistaken for a working OAuth implementation.

use std::collections::BTreeMap;

/// Stable identifier for the generic API-key login method.
pub const API_KEY_METHOD: &str = "api-key";
/// Stable identifier for OpenAI's browser-based ChatGPT OAuth flow.
pub const CHATGPT_BROWSER_METHOD: &str = "chatgpt-browser";
/// Stable identifier for OpenAI's device-code ChatGPT OAuth flow.
pub const CHATGPT_DEVICE_METHOD: &str = "chatgpt-device";

/// What work a login method performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMethodKind {
    /// Read an API key from standard input.
    ApiKey,
    /// Open a browser and receive an OAuth callback on loopback.
    OAuthBrowser,
    /// Print a device code and poll until authorization completes.
    OAuthDevice,
}

/// One user-selectable way to authenticate a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginMethod {
    id: String,
    label: String,
    aliases: Vec<String>,
    kind: LoginMethodKind,
}

impl LoginMethod {
    /// Define a login method.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: LoginMethodKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            aliases: Vec::new(),
            kind,
        }
    }

    /// Add accepted command-line aliases.
    #[must_use]
    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    /// Stable command-line identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-facing label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Work performed by this method.
    #[must_use]
    pub const fn kind(&self) -> LoginMethodKind {
        self.kind
    }

    fn matches(&self, requested: &str) -> bool {
        self.id.eq_ignore_ascii_case(requested)
            || self.label.eq_ignore_ascii_case(requested)
            || self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(requested))
    }
}

/// Typed registry of provider-specific authentication methods.
///
/// Every method is an explicit registration. A credential shape is not a login
/// implementation, and an arbitrary provider id must not become login-capable
/// merely because the CLI knows how to read an API key.
#[derive(Clone, Debug, Default)]
pub struct LoginMethodRegistry {
    methods: BTreeMap<String, Vec<LoginMethod>>,
}

impl LoginMethodRegistry {
    /// The methods implemented by the shipped native components.
    #[must_use]
    pub fn native() -> Self {
        let mut registry = Self::default();
        registry.register(
            "openai",
            LoginMethod::new(
                CHATGPT_BROWSER_METHOD,
                "ChatGPT Plus/Pro (browser)",
                LoginMethodKind::OAuthBrowser,
            )
            .with_aliases(["chatgpt", "browser", "oauth"]),
        );
        registry.register(
            "openai",
            LoginMethod::new(
                CHATGPT_DEVICE_METHOD,
                "ChatGPT Plus/Pro (device code)",
                LoginMethodKind::OAuthDevice,
            )
            .with_aliases(["device", "headless"]),
        );
        registry.register_api_key("openai");
        registry
    }

    /// Register the standard hidden-input API-key flow for one provider instance.
    pub fn register_api_key(&mut self, provider: impl Into<String>) {
        self.register(
            provider,
            LoginMethod::new(
                API_KEY_METHOD,
                "Manually enter API key",
                LoginMethodKind::ApiKey,
            )
            .with_aliases(["api", "api key", "key"]),
        );
    }

    /// Register one provider-specific method.
    ///
    /// Re-registering the same method id replaces that exact registration. This is
    /// the transactional profile-replacement behavior a future auth component needs:
    /// one provider/method identity has one active implementation.
    pub fn register(&mut self, provider: impl Into<String>, method: LoginMethod) {
        let methods = self.methods.entry(provider.into()).or_default();
        match methods
            .iter_mut()
            .find(|candidate| candidate.id == method.id)
        {
            Some(existing) => *existing = method,
            None => methods.push(method),
        }
    }

    /// Every explicitly registered method a provider can use.
    #[must_use]
    pub fn methods_for(&self, provider: &str) -> Vec<LoginMethod> {
        self.methods.get(provider).cloned().unwrap_or_default()
    }

    /// Whether this provider has a native OAuth implementation.
    ///
    /// The generic API-key fallback does not count. Catalog availability uses
    /// this signal so an OAuth credential enables only a provider whose login
    /// and request consumer are both present.
    #[must_use]
    pub fn supports_oauth(&self, provider: &str) -> bool {
        self.methods.get(provider).is_some_and(|methods| {
            methods.iter().any(|method| {
                matches!(
                    method.kind,
                    LoginMethodKind::OAuthBrowser | LoginMethodKind::OAuthDevice
                )
            })
        })
    }

    /// Resolve a method id, label, or alias.
    ///
    /// With no requested spelling, the first method is the provider's default.
    pub fn resolve(
        &self,
        provider: &str,
        requested: Option<&str>,
    ) -> Result<LoginMethod, LoginMethodError> {
        let methods = self.methods_for(provider);
        match requested {
            None => methods
                .into_iter()
                .next()
                .ok_or_else(|| LoginMethodError::Unavailable {
                    provider: provider.to_owned(),
                }),
            Some(requested) => methods
                .iter()
                .find(|method| method.matches(requested))
                .cloned()
                .ok_or_else(|| LoginMethodError::Unknown {
                    provider: provider.to_owned(),
                    requested: requested.to_owned(),
                    available: methods.iter().map(|method| method.id.clone()).collect(),
                }),
        }
    }
}

/// A login method could not be selected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LoginMethodError {
    /// No implementation exists for this provider.
    #[error("provider {provider:?} has no login methods")]
    Unavailable {
        /// Provider id.
        provider: String,
    },
    /// The requested spelling did not match one of the provider's methods.
    #[error(
        "unknown login method {requested:?} for {provider}; available methods: {}",
        available.join(", ")
    )]
    Unknown {
        /// Provider id.
        provider: String,
        /// User-supplied spelling.
        requested: String,
        /// Stable method ids.
        available: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicitly_registered_providers_receive_login_methods() {
        let registry = LoginMethodRegistry::native();
        assert_eq!(
            registry
                .methods_for("openai")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![
                CHATGPT_BROWSER_METHOD,
                CHATGPT_DEVICE_METHOD,
                API_KEY_METHOD
            ]
        );
        assert_eq!(
            registry
                .methods_for("myopenai")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
        assert!(registry.supports_oauth("openai"));
        assert!(!registry.supports_oauth("myopenai"));
    }

    #[test]
    fn an_api_key_method_is_registered_per_provider_instance() {
        let mut registry = LoginMethodRegistry::native();
        registry.register_api_key("myopenai");
        assert_eq!(
            registry
                .methods_for("myopenai")
                .iter()
                .map(LoginMethod::id)
                .collect::<Vec<_>>(),
            vec![API_KEY_METHOD]
        );
        assert!(!registry.supports_oauth("myopenai"));
    }

    #[test]
    fn ids_labels_and_aliases_resolve_case_insensitively() {
        let registry = LoginMethodRegistry::native();
        for requested in [
            "chatgpt-device",
            "ChatGPT Plus/Pro (device code)",
            "HEADLESS",
        ] {
            assert_eq!(
                registry
                    .resolve("openai", Some(requested))
                    .expect("method")
                    .kind(),
                LoginMethodKind::OAuthDevice
            );
        }
    }

    #[test]
    fn an_unknown_method_names_the_stable_choices() {
        let error = LoginMethodRegistry::native()
            .resolve("openai", Some("magic"))
            .expect_err("unknown");
        assert_eq!(
            error,
            LoginMethodError::Unknown {
                provider: "openai".to_owned(),
                requested: "magic".to_owned(),
                available: vec![
                    CHATGPT_BROWSER_METHOD.to_owned(),
                    CHATGPT_DEVICE_METHOD.to_owned(),
                    API_KEY_METHOD.to_owned(),
                ],
            }
        );
    }
}
