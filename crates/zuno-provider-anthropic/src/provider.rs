//! Anthropic provider construction, authentication, and HTTP transport.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::{TryStreamExt as _, stream};
use serde_json::Value;
use zuno_auth::{AuthStore, Credential, Secret};
use zuno_error::ProviderError;
use zuno_llm::event::StreamEvent;
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, FactoryOutcome, Provider,
    ProviderStream, Spec, Unavailable,
};
use zuno_llm::sse::StreamIdleTimeout;

use crate::error::map_http_error;
use crate::request::build_request_body;
use crate::stream::AnthropicDecoder;

const DEFAULT_PROVIDER: &str = "anthropic";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8_192;
const OAUTH_BETA_HEADERS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,prompt-caching-scope-2026-01-05,effort-2025-11-24";
const API_KEY_BETA_HEADERS: &str = "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.80 (external, cli)";

/// Credential mode used by an Anthropic transport.
#[derive(Clone, Debug)]
pub enum AnthropicAuth {
    /// Direct Anthropic API key sent through `x-api-key`.
    ApiKey(Secret),
    /// Anthropic OAuth access token sent as a bearer token.
    OAuth(Secret),
}

impl AnthropicAuth {
    /// Whether this credential uses OAuth request conventions.
    #[must_use]
    pub const fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }
}

/// Immutable Anthropic transport and request options.
#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    provider: String,
    base_url: String,
    api_version: String,
    max_tokens: u64,
    temperature: Option<f64>,
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    thinking: Option<Value>,
    prompt_cache: bool,
    headers: BTreeMap<String, String>,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_PROVIDER.to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_version: DEFAULT_API_VERSION.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: None,
            tools: Vec::new(),
            tool_choice: None,
            thinking: None,
            prompt_cache: true,
            headers: BTreeMap::new(),
        }
    }
}

impl AnthropicConfig {
    /// Translate a generic provider spec into Anthropic options.
    #[must_use]
    pub fn from_spec(spec: Spec) -> Self {
        let mut config = Self {
            provider: if spec.provider.is_empty() {
                DEFAULT_PROVIDER.to_owned()
            } else {
                spec.provider
            },
            base_url: spec.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
            api_version: spec
                .api_version
                .unwrap_or_else(|| DEFAULT_API_VERSION.to_owned()),
            headers: spec.headers,
            ..Self::default()
        };

        if let Some(value) =
            option(&spec.options, &["maxTokens", "max_tokens"]).and_then(Value::as_u64)
        {
            config.max_tokens = value;
        }
        config.temperature = spec.options.get("temperature").and_then(Value::as_f64);
        if let Some(tools) = spec.options.get("tools").and_then(Value::as_array) {
            config.tools = tools.clone();
        }
        config.tool_choice = option(&spec.options, &["toolChoice", "tool_choice"]).cloned();
        config.thinking = spec.options.get("thinking").cloned();
        if let Some(enabled) =
            option(&spec.options, &["promptCache", "prompt_cache"]).and_then(Value::as_bool)
        {
            config.prompt_cache = enabled;
        }
        config
    }

    /// Set the request output-token ceiling.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set Anthropic sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Freeze the tool definitions sent with every request from this instance.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Value>) -> Self {
        self.tools = tools;
        self
    }

    /// Set Anthropic's native tool-choice object.
    #[must_use]
    pub fn with_tool_choice(mut self, tool_choice: Value) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Set Anthropic's native thinking configuration.
    #[must_use]
    pub fn with_thinking(mut self, thinking: Value) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Enable or disable explicit prompt-cache breakpoints.
    #[must_use]
    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Provider registry identity.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Messages API base URL before endpoint normalization.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `anthropic-version` request header.
    #[must_use]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Request output-token ceiling.
    #[must_use]
    pub const fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    /// Optional sampling temperature.
    #[must_use]
    pub const fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Frozen tool definitions.
    #[must_use]
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Native tool-choice object.
    #[must_use]
    pub const fn tool_choice(&self) -> Option<&Value> {
        self.tool_choice.as_ref()
    }

    /// Native thinking configuration.
    #[must_use]
    pub const fn thinking(&self) -> Option<&Value> {
        self.thinking.as_ref()
    }

    /// Whether request construction places cache breakpoints.
    #[must_use]
    pub const fn prompt_cache(&self) -> bool {
        self.prompt_cache
    }
}

/// Anthropic Messages implementation of the shared provider trait.
#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: AnthropicAuth,
    config: AnthropicConfig,
}

impl AnthropicProvider {
    /// Construct from an already loaded credential.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] for a well-known credential because that
    /// shape does not identify whether its key or token is Anthropic bearer data.
    pub fn from_credential(
        credential: Credential,
        mut config: AnthropicConfig,
    ) -> Result<Self, ProviderError> {
        let auth = match credential {
            Credential::Api { key, .. } => AnthropicAuth::ApiKey(key),
            Credential::Oauth {
                access,
                enterprise_url,
                ..
            } => {
                if config.base_url == DEFAULT_BASE_URL
                    && let Some(enterprise_url) = enterprise_url
                {
                    config.base_url = enterprise_url;
                }
                AnthropicAuth::OAuth(access)
            }
            Credential::WellKnown { .. } => {
                return Err(ProviderError::Auth {
                    provider: config.provider.clone(),
                    source: None,
                });
            }
        };
        Ok(Self {
            client: reqwest::Client::new(),
            auth,
            config,
        })
    }

    /// Read this provider's OAuth or API-key credential through [`zuno_auth`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] when storage cannot be read, no credential
    /// exists, or the stored shape is not usable for Anthropic.
    pub fn from_auth_store(store: &AuthStore, spec: Spec) -> Result<Self, ProviderError> {
        let config = AnthropicConfig::from_spec(spec);
        let credential = store
            .get(config.provider())
            .map_err(|source| ProviderError::Auth {
                provider: config.provider.clone(),
                source: Some(Box::new(source)),
            })?
            .ok_or_else(|| ProviderError::Auth {
                provider: config.provider.clone(),
                source: None,
            })?;
        Self::from_credential(credential, config)
    }

    /// Authentication mode, with the secret redacted by its wrapper.
    #[must_use]
    pub const fn auth(&self) -> &AnthropicAuth {
        &self.auth
    }

    /// Immutable request options.
    #[must_use]
    pub const fn config(&self) -> &AnthropicConfig {
        &self.config
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        self.config.provider()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            reasoning: true,
            tool_calls: true,
            prompt_cache: true,
            attachments: true,
            sampling_params: true,
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let client = self.client.clone();
        let auth = self.auth.clone();
        let config = self.config.clone();
        Box::pin(
            stream::once(async move { start_stream(client, auth, config, request).await })
                .try_flatten(),
        )
    }
}

/// Build the registry factory for Anthropic Messages providers.
pub fn factory<C>(credentials: C) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<Credential> + Send + Sync + 'static,
{
    move |spec| {
        let credential = credentials(&spec.provider)
            .ok_or(Declined::Unavailable(Unavailable::MissingCredential))?;
        let config = AnthropicConfig::from_spec(spec);
        let provider =
            AnthropicProvider::from_credential(credential, config).map_err(Declined::Failed)?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

async fn start_stream(
    client: reqwest::Client,
    auth: AnthropicAuth,
    config: AnthropicConfig,
    request: CompletionRequest,
) -> Result<ProviderStream<'static>, ProviderError> {
    let mut body = build_request_body(&request, &config)?;
    // This crate posts to `/v1/messages` unconditionally, whatever the request's
    // surface hint says, so the surface it lowers against is fixed.
    request.apply_parameters(&mut body, ApiSurface::Messages);
    let model = request.model_id;
    let provider = config.provider.clone();
    let mut endpoint = messages_endpoint(&config.base_url);
    if auth.is_oauth() {
        endpoint.push_str(if endpoint.contains('?') {
            "&beta=true"
        } else {
            "?beta=true"
        });
    }

    let mut outgoing = client
        .post(endpoint)
        .header("anthropic-version", &config.api_version)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    for (name, value) in &config.headers {
        outgoing = outgoing.header(name, value);
    }
    for (name, value) in &request.headers {
        outgoing = outgoing.header(name, value);
    }
    outgoing = match &auth {
        AnthropicAuth::ApiKey(key) => outgoing
            .header("x-api-key", key.expose())
            .header("anthropic-beta", API_KEY_BETA_HEADERS),
        AnthropicAuth::OAuth(access) => outgoing
            .bearer_auth(access.expose())
            .header("user-agent", CLAUDE_CODE_USER_AGENT)
            .header("anthropic-beta", OAUTH_BETA_HEADERS),
    };

    let response = outgoing
        .json(&body)
        .send()
        .await
        .map_err(ProviderError::transient)?;
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(ProviderError::transient)?;
        return Err(map_http_error(&provider, status.as_u16(), &headers, &bytes));
    }

    let state = ResponseState {
        response,
        decoder: AnthropicDecoder::new(provider.clone(), model.clone()),
        pending: VecDeque::new(),
        provider,
        model,
        ended: false,
    };
    let output = stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return item.map(|event| Some((event, state)));
            }
            if state.ended {
                return Ok(None);
            }

            let chunk = StreamIdleTimeout::default()
                .wait(&state.provider, &state.model, state.response.chunk())
                .await?
                .map_err(ProviderError::transient)?;
            match chunk {
                Some(bytes) => state.pending.extend(state.decoder.push(&bytes)),
                None => {
                    state.pending.extend(state.decoder.finish());
                    state.ended = true;
                }
            }
        }
    });
    Ok(Box::pin(output))
}

#[derive(Debug)]
struct ResponseState {
    response: reqwest::Response,
    decoder: AnthropicDecoder,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    provider: String,
    model: String,
    ended: bool,
}

fn option<'a>(options: &'a BTreeMap<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| options.get(*name))
}

fn messages_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        base.to_owned()
    } else {
        format!("{base}/v1/messages")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_and_oauth_credentials_select_distinct_modes() {
        let api = AnthropicProvider::from_credential(
            Credential::Api {
                key: Secret::new("sk-ant-test"),
                metadata: None,
            },
            AnthropicConfig::default(),
        )
        .expect("api");
        let oauth = AnthropicProvider::from_credential(
            Credential::Oauth {
                refresh: Secret::new("refresh"),
                access: Secret::new("access"),
                expires: 1,
                account_id: None,
                enterprise_url: Some("https://enterprise.example.test".to_owned()),
            },
            AnthropicConfig::default(),
        )
        .expect("oauth");

        assert!(matches!(api.auth(), AnthropicAuth::ApiKey(_)));
        assert!(matches!(oauth.auth(), AnthropicAuth::OAuth(_)));
        assert_eq!(oauth.config().base_url(), "https://enterprise.example.test");
        let debug = format!("{api:?} {oauth:?}");
        assert!(!debug.contains("sk-ant-test"));
        assert!(!debug.contains("access"));
    }

    #[test]
    fn messages_endpoint_accepts_root_or_full_endpoint() {
        assert_eq!(
            messages_endpoint("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_endpoint("https://proxy.example/v1/messages/"),
            "https://proxy.example/v1/messages"
        );
    }
}
