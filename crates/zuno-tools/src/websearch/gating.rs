//! Native provider configuration and deterministic search-backend selection.

use crate::webfetch::bounds::WebError;
use std::time::Duration;
use zuno_config::{WebSearchBackend, WebSearchConfig};

/// Explicit backend selection.
pub const ENV_PROVIDER: &str = "ZUNO_WEB_SEARCH_PROVIDER";

/// Enables the Exa MCP backend without requiring a key.
pub const ENV_ENABLE_EXA: &str = "ZUNO_WEB_SEARCH_ENABLE_EXA";

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
    /// Selected backend after profile and environment precedence.
    pub provider: Provider,
    /// Whether the selected backend is enabled for this profile.
    pub enabled: bool,
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
            provider: Provider::Exa,
            enabled: true,
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
        Self::from_profile(lookup, None)
    }

    /// Resolve profile settings, with environment values taking precedence.
    #[must_use]
    pub fn from_profile(
        lookup: impl Fn(&str) -> Option<String>,
        profile: Option<&WebSearchConfig>,
    ) -> Self {
        let exa_api_key = lookup(ENV_EXA_API_KEY).filter(|value| !value.trim().is_empty());
        let parallel_api_key =
            lookup(ENV_PARALLEL_API_KEY).filter(|value| !value.trim().is_empty());
        let env_provider = lookup(ENV_PROVIDER).as_deref().and_then(Provider::parse);
        let env_enable_exa = lookup(ENV_ENABLE_EXA).as_deref().and_then(parse_bool);
        let profile_enabled = profile.and_then(|config| config.enabled).unwrap_or(true);
        let profile_provider =
            profile
                .and_then(|config| config.provider)
                .map(|provider| match provider {
                    WebSearchBackend::Exa => Provider::Exa,
                    WebSearchBackend::Parallel => Provider::Parallel,
                });
        let provider = env_provider.or(profile_provider).unwrap_or_else(|| {
            if parallel_api_key.is_some() && exa_api_key.is_none() && env_enable_exa != Some(true) {
                Provider::Parallel
            } else {
                Provider::Exa
            }
        });
        let enabled = match provider {
            Provider::Exa => env_enable_exa.unwrap_or(profile_enabled),
            Provider::Parallel => profile_enabled,
        };
        let env_max_queries = lookup(ENV_MAX_QUERIES);
        let env_max_results = lookup(ENV_MAX_RESULTS);
        let env_timeout = lookup(ENV_TIMEOUT_MS);
        let mut resolved = Self {
            provider,
            enabled,
            exa_api_key,
            parallel_api_key,
            ..Self::default()
        };
        if let Some(value) = positive_usize(env_max_queries) {
            resolved.max_queries = value;
        } else if let Some(value) = profile.and_then(|config| config.max_queries) {
            resolved.max_queries = usize::try_from(value.get()).unwrap_or(usize::MAX);
        }
        if let Some(value) = positive_usize(env_max_results) {
            resolved.max_results = value;
        } else if let Some(value) = profile.and_then(|config| config.max_results) {
            resolved.max_results = usize::try_from(value.get()).unwrap_or(usize::MAX);
        }
        if let Some(value) = positive_u64(env_timeout) {
            resolved.timeout = Duration::from_millis(value);
        } else if let Some(value) = profile.and_then(|config| config.timeout_ms) {
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

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn positive_usize(value: Option<String>) -> Option<usize> {
    value?.trim().parse().ok().filter(|value| *value > 0)
}

fn positive_u64(value: Option<String>) -> Option<u64> {
    value?.trim().parse().ok().filter(|value| *value > 0)
}

/// Whether the selected provider is enabled and has every required credential.
#[must_use]
pub fn web_search_usable(config: &SearchConfig) -> bool {
    config.enabled
        && match config.provider {
            Provider::Exa => true,
            Provider::Parallel => config.parallel_api_key.is_some(),
        }
}

/// Select the configured provider deterministically.
#[must_use]
pub fn select_provider(_session_id: &str, config: &SearchConfig) -> Provider {
    config.provider
}

/// Validate that provider selection is resolvable at profile mount.
pub fn require_provider(config: &SearchConfig) -> Result<(), WebError> {
    if !config.enabled {
        return Err(WebError::NoSearchProvider);
    }
    if config.provider == Provider::Parallel && config.parallel_api_key.is_none() {
        return Err(WebError::MissingSearchCredential {
            provider: Provider::Parallel.as_str(),
        });
    }
    Ok(())
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
        assert_eq!(settings.provider, Provider::Exa);
        assert_eq!(select_provider("ses", &settings), Provider::Exa);
    }

    #[test]
    fn credentials_enable_their_provider_without_a_second_switch() {
        let exa = config(&[(ENV_EXA_API_KEY, "secret")]);
        assert!(web_search_usable(&exa));
        assert_eq!(select_provider("ses", &exa), Provider::Exa);

        let parallel = config(&[(ENV_PARALLEL_API_KEY, "secret")]);
        assert!(web_search_usable(&parallel));
        assert_eq!(select_provider("ses", &parallel), Provider::Parallel);
    }

    #[test]
    fn an_explicit_exa_false_is_not_overridden_by_a_key() {
        let settings = config(&[(ENV_ENABLE_EXA, "false"), (ENV_EXA_API_KEY, "secret")]);

        assert!(!web_search_usable(&settings));
    }

    #[test]
    fn explicit_provider_wins_when_both_are_available() {
        let settings = config(&[
            (ENV_PROVIDER, "parallel"),
            (ENV_ENABLE_EXA, "true"),
            (ENV_PARALLEL_API_KEY, "secret"),
        ]);
        assert_eq!(select_provider("ses", &settings), Provider::Parallel);
    }

    #[test]
    fn profile_limits_are_used_and_environment_values_override_them() {
        let profile = WebSearchConfig {
            enabled: None,
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
        assert_eq!(settings.provider, Provider::Parallel);
        assert_eq!(settings.max_queries, 3);
        assert_eq!(settings.max_results, 2);
        assert_eq!(settings.timeout, Duration::from_millis(700));
    }

    #[test]
    fn anonymous_exa_is_the_default_provider() {
        let settings = SearchConfig::default();

        assert!(web_search_usable(&settings));
        assert_eq!(select_provider("ses", &settings), Provider::Exa);
        assert!(require_provider(&settings).is_ok());
    }

    #[test]
    fn explicit_parallel_without_a_key_is_rejected() {
        let settings = config(&[(ENV_PROVIDER, "parallel")]);

        assert!(matches!(
            require_provider(&settings),
            Err(WebError::MissingSearchCredential {
                provider: "parallel"
            })
        ));
    }

    #[test]
    fn profile_false_hides_exa_even_when_a_key_exists() {
        let profile = WebSearchConfig {
            enabled: Some(false),
            ..WebSearchConfig::default()
        };
        let settings = SearchConfig::from_profile(
            |key| (key == ENV_EXA_API_KEY).then(|| "secret".to_owned()),
            Some(&profile),
        );

        assert_eq!(settings.provider, Provider::Exa);
        assert!(!web_search_usable(&settings));
    }
}
