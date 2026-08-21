//! Native provider configuration and deterministic search-backend selection.

use crate::webfetch::bounds::WebError;
use std::time::Duration;
use zuno_config::{WebSearchBackend, WebSearchConfig};

/// Explicit backend selection.
pub const ENV_PROVIDER: &str = "ZUNO_WEB_SEARCH_PROVIDER";

/// Enables the Exa MCP backend without requiring a key.
pub const ENV_ENABLE_EXA: &str = "ZUNO_WEB_SEARCH_ENABLE_EXA";

/// Enables the Parallel MCP backend without requiring a key.
pub const ENV_ENABLE_PARALLEL: &str = "ZUNO_WEB_SEARCH_ENABLE_PARALLEL";

/// Exa API key.
pub const ENV_EXA_API_KEY: &str = "EXA_API_KEY";

/// Parallel API key.
pub const ENV_PARALLEL_API_KEY: &str = "PARALLEL_API_KEY";

/// Overrides the profile query-count limit.
pub const ENV_MAX_QUERIES: &str = "ZUNO_WEB_SEARCH_MAX_QUERIES";

/// Overrides the profile combined-result limit.
pub const ENV_MAX_RESULTS: &str = "ZUNO_WEB_SEARCH_MAX_RESULTS";

/// Overrides the profile per-query timeout.
pub const ENV_TIMEOUT_MS: &str = "ZUNO_WEB_SEARCH_TIMEOUT_MS";

/// Hosted search backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Exa MCP search.
    Exa,
    /// Parallel MCP search.
    Parallel,
}

impl Provider {
    /// Stable configuration and metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
        }
    }

    /// Human-readable provider name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exa => "Exa",
            Self::Parallel => "Parallel",
        }
    }

    /// Parse one explicit provider name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "exa" => Some(Self::Exa),
            "parallel" => Some(Self::Parallel),
            _ => None,
        }
    }
}

/// Search-provider configuration resolved once at profile mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchConfig {
    /// Explicit backend, when configured.
    pub provider: Option<Provider>,
    /// Whether Exa may be selected.
    pub enable_exa: bool,
    /// Whether Parallel may be selected.
    pub enable_parallel: bool,
    /// Exa credential.
    pub exa_api_key: Option<String>,
    /// Parallel credential.
    pub parallel_api_key: Option<String>,
    /// Maximum submitted queries before duplicate removal.
    pub max_queries: usize,
    /// Maximum sources after batch merge.
    pub max_results: usize,
    /// Time budget for one provider call.
    pub timeout: Duration,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            enable_exa: false,
            enable_parallel: false,
            exa_api_key: None,
            parallel_api_key: None,
            max_queries: super::DEFAULT_MAX_QUERIES,
            max_results: super::DEFAULT_MAX_RESULTS,
            timeout: super::mcp::TIMEOUT,
        }
    }
}

impl SearchConfig {
    /// Read native Zuno environment configuration.
    #[must_use]
    pub fn from_env() -> Self {
        let env = zuno_paths::Env::from_process();
        Self::from_lookup(|key| env.value(key).map(str::to_owned))
    }

    /// Resolve through a caller-provided lookup.
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let exa_api_key = lookup(ENV_EXA_API_KEY).filter(|value| !value.trim().is_empty());
        let parallel_api_key =
            lookup(ENV_PARALLEL_API_KEY).filter(|value| !value.trim().is_empty());
        Self {
            provider: lookup(ENV_PROVIDER).as_deref().and_then(Provider::parse),
            enable_exa: lookup(ENV_ENABLE_EXA).is_some_and(|value| is_truthy(&value))
                || exa_api_key.is_some(),
            enable_parallel: lookup(ENV_ENABLE_PARALLEL).is_some_and(|value| is_truthy(&value))
                || parallel_api_key.is_some(),
            exa_api_key,
            parallel_api_key,
            max_queries: positive_usize(lookup(ENV_MAX_QUERIES))
                .unwrap_or(super::DEFAULT_MAX_QUERIES),
            max_results: positive_usize(lookup(ENV_MAX_RESULTS))
                .unwrap_or(super::DEFAULT_MAX_RESULTS),
            timeout: positive_u64(lookup(ENV_TIMEOUT_MS))
                .map(Duration::from_millis)
                .unwrap_or(super::mcp::TIMEOUT),
        }
    }

    /// Resolve profile settings, with environment values taking precedence.
    #[must_use]
    pub fn from_profile(
        lookup: impl Fn(&str) -> Option<String>,
        profile: Option<&WebSearchConfig>,
    ) -> Self {
        let env_provider = lookup(ENV_PROVIDER);
        let env_max_queries = lookup(ENV_MAX_QUERIES);
        let env_max_results = lookup(ENV_MAX_RESULTS);
        let env_timeout = lookup(ENV_TIMEOUT_MS);
        let mut resolved = Self::from_lookup(&lookup);

        if env_provider.is_none()
            && let Some(provider) = profile.and_then(|config| config.provider)
        {
            resolved.provider = Some(match provider {
                WebSearchBackend::Exa => Provider::Exa,
                WebSearchBackend::Parallel => Provider::Parallel,
            });
        }
        if env_max_queries.is_none()
            && let Some(value) = profile.and_then(|config| config.max_queries)
        {
            resolved.max_queries = usize::try_from(value.get()).unwrap_or(usize::MAX);
        }
        if env_max_results.is_none()
            && let Some(value) = profile.and_then(|config| config.max_results)
        {
            resolved.max_results = usize::try_from(value.get()).unwrap_or(usize::MAX);
        }
        if env_timeout.is_none()
            && let Some(value) = profile.and_then(|config| config.timeout_ms)
        {
            resolved.timeout = Duration::from_millis(value.get());
        }
        resolved
    }

    /// Credential for the selected provider.
    #[must_use]
    pub fn api_key(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Exa => self.exa_api_key.as_deref(),
            Provider::Parallel => self.parallel_api_key.as_deref(),
        }
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn positive_usize(value: Option<String>) -> Option<usize> {
    value?.trim().parse().ok().filter(|value| *value > 0)
}

fn positive_u64(value: Option<String>) -> Option<u64> {
    value?.trim().parse().ok().filter(|value| *value > 0)
}

/// Whether the profile has at least one usable search provider.
#[must_use]
pub fn web_search_enabled(config: &SearchConfig) -> bool {
    config.provider.is_some() || config.enable_exa || config.enable_parallel
}

/// Select the configured provider deterministically.
///
/// An explicit provider wins. Otherwise Exa wins when enabled, then Parallel.
/// Calling this without any configured provider is a composition error.
#[must_use]
pub fn select_provider(_session_id: &str, config: &SearchConfig) -> Provider {
    if let Some(provider) = config.provider {
        return provider;
    }
    if config.enable_exa {
        return Provider::Exa;
    }
    if config.enable_parallel {
        return Provider::Parallel;
    }
    Provider::Exa
}

/// Validate that provider selection is resolvable at profile mount.
pub fn require_provider(config: &SearchConfig) -> Result<(), WebError> {
    web_search_enabled(config)
        .then_some(())
        .ok_or(WebError::NoSearchProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> SearchConfig {
        SearchConfig::from_lookup(|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn only_native_zuno_keys_control_provider_selection() {
        let settings = config(&[
            ("ZUNO_WEBSEARCH_PROVIDER", "parallel"),
            (ENV_ENABLE_EXA, "true"),
        ]);
        assert_eq!(settings.provider, None);
        assert_eq!(select_provider("ses", &settings), Provider::Exa);
    }

    #[test]
    fn credentials_enable_their_provider_without_a_second_switch() {
        let exa = config(&[(ENV_EXA_API_KEY, "secret")]);
        assert!(web_search_enabled(&exa));
        assert_eq!(select_provider("ses", &exa), Provider::Exa);

        let parallel = config(&[(ENV_PARALLEL_API_KEY, "secret")]);
        assert!(web_search_enabled(&parallel));
        assert_eq!(select_provider("ses", &parallel), Provider::Parallel);
    }

    #[test]
    fn explicit_provider_wins_when_both_are_available() {
        let settings = config(&[
            (ENV_PROVIDER, "parallel"),
            (ENV_ENABLE_EXA, "true"),
            (ENV_ENABLE_PARALLEL, "true"),
        ]);
        assert_eq!(select_provider("ses", &settings), Provider::Parallel);
    }

    #[test]
    fn profile_limits_are_used_and_environment_values_override_them() {
        let profile = WebSearchConfig {
            provider: Some(WebSearchBackend::Parallel),
            max_queries: std::num::NonZeroU32::new(3),
            max_results: std::num::NonZeroU32::new(6),
            timeout_ms: std::num::NonZeroU64::new(700),
        };
        let settings = SearchConfig::from_profile(
            |key| match key {
                ENV_MAX_RESULTS => Some("2".to_owned()),
                _ => None,
            },
            Some(&profile),
        );
        assert_eq!(settings.provider, Some(Provider::Parallel));
        assert_eq!(settings.max_queries, 3);
        assert_eq!(settings.max_results, 2);
        assert_eq!(settings.timeout, Duration::from_millis(700));
    }

    #[test]
    fn no_provider_is_not_exposed() {
        let settings = SearchConfig::default();
        assert!(!web_search_enabled(&settings));
        assert!(matches!(
            require_provider(&settings),
            Err(WebError::NoSearchProvider)
        ));
    }
}
