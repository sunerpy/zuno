//! OpenAI provider configuration, authentication, and HTTP transport.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::{TryStreamExt as _, stream};
use serde_json::Value;
use tokio::sync::Mutex;
use zuno_auth::{
    AuthStore, CHATGPT_CODEX_BASE_URL, Credential, OpenAiOauthClient, OpenAiOauthError, Secret,
    residency_from_jwt,
};
use zuno_error::ProviderError;
use zuno_llm::event::StreamEvent;
use zuno_llm::http::{HttpTimeouts, RequestDeadlines, read_error_body};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, FactoryOutcome, Provider,
    ProviderStream, ReasoningReplayPolicy, Spec, Unavailable, generation,
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
    reasoning_replay: ReasoningReplayPolicy,
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
            reasoning_replay: ReasoningReplayPolicy::default(),
            headers: BTreeMap::new(),
        }
    }
}

impl OpenAiConfig {
    /// Convert a registry spec into OpenAI options.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Fatal`] when the spec declares a sealed-reasoning replay
    /// option this crate cannot interpret. Reading it as
    /// [`ReasoningReplay::Off`](zuno_llm::registry::ReasoningReplay::Off) instead
    /// would run the whole session without the capability the user asked for, and
    /// nothing on the Zuno side would say so.
    pub fn try_from_spec(spec: Spec) -> Result<Self, ProviderError> {
        let reasoning_replay =
            ReasoningReplayPolicy::from_spec(&spec).map_err(ProviderError::fatal)?;
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
        config.reasoning_replay = reasoning_replay;
        Ok(config)
    }

    /// Declare whether this endpoint seals its reasoning for replay.
    #[must_use]
    pub const fn with_reasoning_replay(mut self, reasoning_replay: ReasoningReplayPolicy) -> Self {
        self.reasoning_replay = reasoning_replay;
        self
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
    /// Whether this endpoint seals its reasoning and takes it back on replay.
    #[must_use]
    pub const fn reasoning_replay(&self) -> ReasoningReplayPolicy {
        self.reasoning_replay
    }
}

/// Genuine OpenAI implementation of the shared provider trait.
#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    auth: OpenAiAuth,
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
        config: OpenAiConfig,
    ) -> Result<Self, ProviderError> {
        Self::from_credential_and_store(credential, config, None)
    }

    fn from_credential_and_store(
        credential: Credential,
        mut config: OpenAiConfig,
        store: Option<AuthStore>,
    ) -> Result<Self, ProviderError> {
        if let Credential::Oauth {
            enterprise_url: Some(enterprise_url),
            ..
        } = &credential
            && config.base_url == DEFAULT_BASE_URL
        {
            config.base_url.clone_from(enterprise_url);
        }
        let chatgpt = config.provider == DEFAULT_PROVIDER;
        let auth = OpenAiAuth::new(credential, store, chatgpt);
        Ok(Self {
            client: zuno_network::client(),
            auth,
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
        let config = OpenAiConfig::try_from_spec(spec)?;
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
        Self::from_credential_and_store(credential, config, Some(store.clone()))
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
        let auth = self.auth.clone();
        let config = self.config.clone();
        Box::pin(
            stream::once(async move { start_stream(client, auth, config, request).await })
                .try_flatten(),
        )
    }
}

/// Build the registry factory for OpenAI Chat Completions and Responses.
pub fn factory<C>(
    credentials: C,
    store: Option<AuthStore>,
) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<Credential> + Send + Sync + 'static,
{
    move |spec| {
        let credential = credentials(&spec.provider)
            .ok_or(Declined::Unavailable(Unavailable::MissingCredential))?;
        let config = OpenAiConfig::try_from_spec(spec).map_err(Declined::Failed)?;
        let provider = OpenAiProvider::from_credential_and_store(credential, config, store.clone())
            .map_err(Declined::Failed)?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

async fn start_stream(
    client: reqwest::Client,
    auth: OpenAiAuth,
    config: OpenAiConfig,
    request: CompletionRequest,
) -> Result<ProviderStream<'static>, ProviderError> {
    let body = build_request_body(&request, &config)?;
    let surface = resolve_surface(request.surface);
    let request_auth = auth.resolve(config.provider()).await?;
    if request_auth.chatgpt && surface != ApiSurface::Responses {
        return Err(ProviderError::fatal(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ChatGPT OAuth supports the OpenAI Responses surface only",
        )));
    }
    let endpoint = endpoint(&config.base_url, surface, request_auth.chatgpt);
    let provider = config.provider.clone();
    let model = request.model_id;
    let mut outgoing = client
        .post(endpoint)
        .bearer_auth(request_auth.bearer.expose())
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    if request_auth.chatgpt {
        outgoing = outgoing
            .header("originator", "zuno")
            .header("user-agent", format!("zuno/{}", env!("CARGO_PKG_VERSION")))
            .header("version", env!("CARGO_PKG_VERSION"));
        if let Some(account_id) = request_auth.account_id {
            outgoing = outgoing.header("ChatGPT-Account-Id", account_id);
        }
        if let Some(residency) = request_auth.residency {
            outgoing = outgoing.header("x-openai-internal-codex-residency", residency);
        }
    }
    for (name, value) in &config.headers {
        outgoing = outgoing.header(name, value);
    }
    for (name, value) in &request.headers {
        outgoing = outgoing.header(name, value);
    }
    // Without a response-header deadline a peer that accepts the connection and
    // then never answers holds the turn open indefinitely; `send()` has no bound
    // of its own.
    let deadlines = RequestDeadlines::start(HttpTimeouts::native());
    let response = deadlines
        .headers(&provider, outgoing.json(&body).send())
        .await?
        .map_err(ProviderError::transient)?;
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let bytes = read_error_body(&provider, response).await?.into_bytes();
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

fn endpoint(base_url: &str, surface: ApiSurface, chatgpt: bool) -> String {
    if chatgpt && base_url == DEFAULT_BASE_URL {
        return format!("{CHATGPT_CODEX_BASE_URL}/responses");
    }
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

#[derive(Clone, Debug)]
enum OpenAiAuth {
    Bearer(Secret),
    OAuth {
        credential: Arc<Mutex<Credential>>,
        store: Option<AuthStore>,
        client: OpenAiOauthClient,
    },
}

impl OpenAiAuth {
    fn new(credential: Credential, store: Option<AuthStore>, chatgpt: bool) -> Self {
        match credential {
            Credential::Api { key, .. } => Self::Bearer(key),
            Credential::WellKnown { token, .. } => Self::Bearer(token),
            Credential::Oauth { access, .. } if !chatgpt => Self::Bearer(access),
            credential @ Credential::Oauth { .. } => Self::OAuth {
                credential: Arc::new(Mutex::new(credential)),
                store,
                client: OpenAiOauthClient::production(),
            },
        }
    }

    async fn resolve(&self, provider: &str) -> Result<RequestAuth, ProviderError> {
        match self {
            Self::Bearer(bearer) => Ok(RequestAuth {
                bearer: bearer.clone(),
                account_id: None,
                residency: None,
                chatgpt: false,
            }),
            Self::OAuth {
                credential,
                store,
                client,
            } => {
                let mut credential = credential.lock().await;
                if zuno_auth::openai::needs_refresh(&credential) {
                    let refreshed = client
                        .refresh(&credential)
                        .await
                        .map_err(|error| oauth_error(provider, error))?;
                    if let Some(store) = store
                        && !store.has_env_override()
                    {
                        store.set(provider, refreshed.clone()).map_err(|source| {
                            ProviderError::Auth {
                                provider: provider.to_owned(),
                                source: Some(Box::new(source)),
                            }
                        })?;
                    }
                    *credential = refreshed;
                }
                let Credential::Oauth {
                    access, account_id, ..
                } = &*credential
                else {
                    return Err(ProviderError::Auth {
                        provider: provider.to_owned(),
                        source: None,
                    });
                };
                Ok(RequestAuth {
                    bearer: access.clone(),
                    account_id: account_id.clone(),
                    residency: residency_from_jwt(access.expose()),
                    chatgpt: true,
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RequestAuth {
    bearer: Secret,
    account_id: Option<String>,
    residency: Option<String>,
    chatgpt: bool,
}

fn oauth_error(provider: &str, error: OpenAiOauthError) -> ProviderError {
    if error.is_transient() {
        ProviderError::transient(error)
    } else {
        ProviderError::Auth {
            provider: provider.to_owned(),
            source: Some(Box::new(error)),
        }
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
            endpoint(DEFAULT_BASE_URL, ApiSurface::Default, false),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint(DEFAULT_BASE_URL, ApiSurface::Chat, false),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint(DEFAULT_BASE_URL, ApiSurface::Responses, true),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[tokio::test]
    async fn only_the_official_openai_provider_treats_oauth_as_chatgpt() {
        let credential = Credential::Oauth {
            refresh: Secret::new("refresh"),
            access: Secret::new("access"),
            expires: u64::MAX,
            account_id: Some("acct-test".to_owned()),
            enterprise_url: None,
        };

        let official = OpenAiAuth::new(credential.clone(), None, true)
            .resolve("openai")
            .await
            .expect("official OAuth");
        assert!(official.chatgpt);
        assert_eq!(official.account_id.as_deref(), Some("acct-test"));
        assert_eq!(official.bearer.expose(), "access");
        assert!(official.residency.is_none());

        let custom = OpenAiAuth::new(credential, None, false)
            .resolve("myopenai")
            .await
            .expect("custom bearer");
        assert!(!custom.chatgpt);
        assert!(custom.account_id.is_none());
        assert!(custom.residency.is_none());
        assert_eq!(custom.bearer.expose(), "access");
    }
}
