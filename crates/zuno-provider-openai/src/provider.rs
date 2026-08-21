//! OpenAI provider configuration, authentication, and HTTP transport.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::{TryStreamExt as _, stream};
use serde_json::Value;
use zuno_auth::{AuthStore, Credential, Secret};
use zuno_error::ProviderError;
use zuno_llm::event::StreamEvent;
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, FactoryOutcome, Provider,
    ProviderStream, Spec, Unavailable, generation,
};
use zuno_llm::sse::StreamIdleTimeout;

use crate::error::map_http_error;
use crate::request::{Sampling, build_request_body, resolve_surface};
use crate::stream::OpenAiDecoder;

const DEFAULT_PROVIDER: &str = "openai";
const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Immutable options applied to OpenAI requests.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    provider: String,
    base_url: String,
    surface: ApiSurface,
    max_tokens: Option<u64>,
    sampling: Sampling,
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    store: Option<bool>,
    include: Option<Vec<Value>>,
    reasoning: Option<Value>,
    text: Option<Value>,
    headers: BTreeMap<String, String>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_PROVIDER.to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            surface: ApiSurface::Default,
            max_tokens: None,
            sampling: Sampling::default(),
            tools: Vec::new(),
            tool_choice: None,
            store: None,
            include: None,
            reasoning: None,
            text: None,
            headers: BTreeMap::new(),
        }
    }
}

impl OpenAiConfig {
    /// Convert a registry spec into OpenAI options.
    #[must_use]
    pub fn from_spec(spec: Spec) -> Self {
        let mut config = Self {
            provider: if spec.provider.is_empty() {
                DEFAULT_PROVIDER.to_owned()
            } else {
                spec.provider
            },
            base_url: spec.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
            surface: spec.surface,
            headers: spec.headers,
            ..Self::default()
        };
        config.max_tokens =
            option(&spec.options, generation::MAX_TOKENS_KEYS).and_then(Value::as_u64);
        config.sampling = Sampling {
            temperature: option(&spec.options, generation::TEMPERATURE_KEYS)
                .and_then(Value::as_f64),
            top_p: option(&spec.options, generation::TOP_P_KEYS).and_then(Value::as_f64),
            frequency_penalty: option(&spec.options, &["frequencyPenalty", "frequency_penalty"])
                .and_then(Value::as_f64),
            presence_penalty: option(&spec.options, &["presencePenalty", "presence_penalty"])
                .and_then(Value::as_f64),
        };
        config.tools = spec
            .options
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        config.tool_choice = option(&spec.options, generation::TOOL_CHOICE_KEYS).cloned();
        config.store = spec.options.get("store").and_then(Value::as_bool);
        config.include = spec
            .options
            .get("include")
            .and_then(Value::as_array)
            .cloned();
        config.reasoning = spec.options.get("reasoning").cloned();
        config.text = spec.options.get("text").cloned();
        config
    }

    /// Configuration used for stateless reasoning replay.
    #[must_use]
    pub fn stateless_reasoning() -> Self {
        Self::default()
            .with_store(false)
            .with_include(vec![Value::String(
                "reasoning.encrypted_content".to_owned(),
            )])
            .with_reasoning(serde_json::json!({ "effort": "medium", "summary": "auto" }))
    }

    /// Set the output-token ceiling.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set all sampling controls.
    #[must_use]
    pub const fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }

    /// Set tool definitions.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Value>) -> Self {
        self.tools = tools;
        self
    }

    /// Set the native tool-choice value.
    #[must_use]
    pub fn with_tool_choice(mut self, tool_choice: Value) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Set the Responses storage flag.
    #[must_use]
    pub const fn with_store(mut self, store: bool) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the Responses include list.
    #[must_use]
    pub fn with_include(mut self, include: Vec<Value>) -> Self {
        self.include = Some(include);
        self
    }

    /// Set native reasoning controls.
    #[must_use]
    pub fn with_reasoning(mut self, reasoning: Value) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Set native text controls.
    #[must_use]
    pub fn with_text(mut self, text: Value) -> Self {
        self.text = Some(text);
        self
    }

    /// Provider registry identity.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
    /// Base URL before endpoint normalization.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    /// Configured default surface.
    #[must_use]
    pub const fn surface(&self) -> ApiSurface {
        self.surface
    }
    /// Output-token ceiling.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }
    /// Sampling controls.
    #[must_use]
    pub const fn sampling(&self) -> Sampling {
        self.sampling
    }
    /// Tool definitions.
    #[must_use]
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }
    /// Native tool-choice value.
    #[must_use]
    pub const fn tool_choice(&self) -> Option<&Value> {
        self.tool_choice.as_ref()
    }
    /// Responses storage flag.
    #[must_use]
    pub const fn store(&self) -> Option<bool> {
        self.store
    }
    /// Responses include list.
    #[must_use]
    pub fn include(&self) -> Option<&[Value]> {
        self.include.as_deref()
    }
    /// Native reasoning controls.
    #[must_use]
    pub const fn reasoning_options(&self) -> Option<&Value> {
        self.reasoning.as_ref()
    }
    /// Native text controls.
    #[must_use]
    pub const fn text(&self) -> Option<&Value> {
        self.text.as_ref()
    }
}

/// Genuine OpenAI implementation of the shared provider trait.
#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    credential: Secret,
    config: OpenAiConfig,
}

impl OpenAiProvider {
    /// Construct from a loaded OpenAI credential.
    ///
    /// # Errors
    ///
    /// Returns an authentication error when the credential shape has no usable
    /// bearer value.
    pub fn from_credential(
        credential: Credential,
        mut config: OpenAiConfig,
    ) -> Result<Self, ProviderError> {
        let credential = match credential {
            Credential::Api { key, .. } => key,
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
                access
            }
            Credential::WellKnown { token, .. } => token,
        };
        Ok(Self {
            client: reqwest::Client::new(),
            credential,
            config,
        })
    }

    /// Load OpenAI authentication through `zuno-auth`.
    ///
    /// # Errors
    ///
    /// Returns an authentication error when storage cannot be read or no OpenAI
    /// credential exists.
    pub fn from_auth_store(store: &AuthStore, spec: Spec) -> Result<Self, ProviderError> {
        let config = OpenAiConfig::from_spec(spec);
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

    /// Immutable request options.
    #[must_use]
    pub const fn config(&self) -> &OpenAiConfig {
        &self.config
    }
}

impl Provider for OpenAiProvider {
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

    fn stream(&self, mut request: CompletionRequest) -> ProviderStream<'_> {
        if request.surface == ApiSurface::Default && self.config.surface != ApiSurface::Default {
            request.surface = self.config.surface;
        }
        let client = self.client.clone();
        let credential = self.credential.clone();
        let config = self.config.clone();
        Box::pin(
            stream::once(async move { start_stream(client, credential, config, request).await })
                .try_flatten(),
        )
    }
}

/// Build the registry factory for OpenAI Chat Completions and Responses.
pub fn factory<C>(credentials: C) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<Credential> + Send + Sync + 'static,
{
    move |spec| {
        let credential = credentials(&spec.provider)
            .ok_or(Declined::Unavailable(Unavailable::MissingCredential))?;
        let config = OpenAiConfig::from_spec(spec);
        let provider =
            OpenAiProvider::from_credential(credential, config).map_err(Declined::Failed)?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

async fn start_stream(
    client: reqwest::Client,
    credential: Secret,
    config: OpenAiConfig,
    request: CompletionRequest,
) -> Result<ProviderStream<'static>, ProviderError> {
    let mut body = build_request_body(&request, &config)?;
    let surface = resolve_surface(request.surface);
    request.apply_parameters(&mut body, surface);
    let endpoint = endpoint(&config.base_url, surface);
    let provider = config.provider.clone();
    let model = request.model_id;
    let mut outgoing = client
        .post(endpoint)
        .bearer_auth(credential.expose())
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    for (name, value) in &config.headers {
        outgoing = outgoing.header(name, value);
    }
    for (name, value) in &request.headers {
        outgoing = outgoing.header(name, value);
    }
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
        decoder: OpenAiDecoder::new(provider.clone(), model.clone(), surface),
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
    decoder: OpenAiDecoder,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    provider: String,
    model: String,
    ended: bool,
}

fn endpoint(base_url: &str, surface: ApiSurface) -> String {
    let base = base_url.trim_end_matches('/');
    let suffix = match resolve_surface(surface) {
        ApiSurface::Chat => "/v1/chat/completions",
        ApiSurface::Responses | ApiSurface::Default => "/v1/responses",
        ApiSurface::Messages => "/v1/messages",
    };
    if base.ends_with(suffix) {
        base.to_owned()
    } else {
        format!("{base}{suffix}")
    }
}

fn option<'a>(options: &'a BTreeMap<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| options.get(*name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_surface_targets_responses() {
        assert_eq!(
            endpoint(DEFAULT_BASE_URL, ApiSurface::Default),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint(DEFAULT_BASE_URL, ApiSurface::Chat),
            "https://api.openai.com/v1/chat/completions"
        );
    }
}
