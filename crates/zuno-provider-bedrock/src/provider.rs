use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use futures::{TryStreamExt as _, stream};
use http::Method;
use serde_json::{Map, Value, json};
use url::Url;
use zuno_aws_auth::{AwsAccessKeys, AwsAuthConfig, AwsRequestToSign};
use zuno_error::ProviderError;
use zuno_llm::event::{Message, RequestContentBlock, Role, StreamEvent, tool_arguments_text};
use zuno_llm::http::{HttpTimeouts, RequestDeadlines, read_error_body};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec, generation,
};
use zuno_llm::sse::{StreamIdleTimeout, upstream_stream_incomplete};

use crate::aws::{BedrockBearerToken, BedrockRequestAuth, header_map};
use crate::error::classify_bedrock_error_for;
use crate::eventstream::{BedrockDecodeError, BedrockEventDecoder};

/// The cap sent on the Anthropic-native path when nothing is configured.
///
/// The oracle's own fallback for the same required field
/// (`packages/llm/src/protocols/anthropic-messages.ts:510`).
const ANTHROPIC_NATIVE_DEFAULT_MAX_TOKENS: u64 = 4096;

/// Provider identity for the Bedrock Converse/EventStream transport.
pub const CONVERSE_PROVIDER_ID: &str = "amazon-bedrock-converse";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BedrockOperation {
    #[default]
    ConverseStream,
    InvokeModelWithResponseStream,
}

impl BedrockOperation {
    fn path(self, model_id: &str) -> String {
        let operation = match self {
            Self::ConverseStream => "converse-stream",
            Self::InvokeModelWithResponseStream => "invoke-with-response-stream",
        };
        format!("/model/{}/{operation}", encode_path_segment(model_id))
    }
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

#[derive(Debug, Clone)]
pub struct BedrockConfig {
    pub provider_id: String,
    pub region: Option<String>,
    pub endpoint: Option<Url>,
    pub operation: BedrockOperation,
    pub auth: AwsAuthConfig,
    pub access_keys: Option<AwsAccessKeys>,
    pub generation: BedrockGeneration,
    /// Which models may be sent tool definitions.
    pub tools: BedrockToolPolicy,
}

/// Whether a model this provider serves accepts tool definitions on the wire.
///
/// Bedrock is one endpoint in front of every vendor's models, and tool use is a
/// *per-model* fact there: Converse answers `toolConfig` for a family that does not
/// implement it with a `ValidationException`, and Bedrock's own model table is what says
/// which families those are. [`Provider::capabilities`] has no model to branch on, and the
/// catalog's per-model `tool_call` flag reaches only the compatible transport
/// (`with_compatible_model_capabilities` in the CLI), so this crate resolves the question
/// itself and gives an operator two levers over the answer.
///
/// Order of authority, most specific first:
///
/// 1. `modelCapabilities.<model id>.tool_calls` — one answer for one model, in the same
///    spelling the compatible provider already reads, so a composition root that forwards
///    the catalog flag needs no second shape.
/// 2. `toolCalls` — one answer for every model this provider entry serves. `false` also
///    withdraws the `tool_calls` capability, so the turn loop stops assembling a snapshot
///    the wire will not carry.
/// 3. The families Bedrock documents Converse tool use for. Anything unrecognised —
///    a provisioned-throughput ARN, a model newer than this build — resolves to *no tool
///    definitions*, which is byte for byte what 0.6.6 sent for every Bedrock model, and is
///    raised by either lever above rather than by guessing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BedrockToolPolicy {
    provider_wide: Option<bool>,
    per_model: BTreeMap<String, bool>,
}

/// The model families Bedrock's Converse API documents as accepting `toolConfig`.
///
/// Matched as a substring of the lowercased model id so that a cross-region routing
/// prefix (`us.anthropic.claude-…`) and an inference-profile ARN whose resource name
/// carries the model id both resolve like the bare id.
const CONVERSE_TOOL_USE_FAMILIES: &[&str] = &[
    "anthropic.claude",
    "amazon.nova-micro",
    "amazon.nova-lite",
    "amazon.nova-pro",
    "amazon.nova-premier",
    "cohere.command-r",
    "meta.llama3-1",
    "meta.llama3-2",
    "meta.llama3-3",
    "meta.llama4",
    "mistral.mistral-large",
    "mistral.mistral-small",
    "mistral.pixtral-large",
    "ai21.jamba-1-5",
];

/// Members of a listed family that predate its tool support.
///
/// Claude 2 and Claude Instant are `anthropic.claude*` ids that Converse serves without
/// tool use, so the family prefix alone would send them a `toolConfig` they reject.
const CONVERSE_TOOL_USE_EXCEPTIONS: &[&str] = &["anthropic.claude-v2", "anthropic.claude-instant"];

impl BedrockToolPolicy {
    /// The policy `spec.options` describes.
    fn from_spec(spec: &Spec) -> Self {
        let provider_wide = ["toolCalls", "tool_calls"]
            .iter()
            .find_map(|key| spec.options.get(*key))
            .and_then(Value::as_bool);
        let per_model = spec
            .options
            .get("modelCapabilities")
            .and_then(Value::as_object)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|(model, capabilities)| {
                        capabilities
                            .get("tool_calls")
                            .or_else(|| capabilities.get("toolcall"))
                            .and_then(Value::as_bool)
                            .map(|allowed| (model.clone(), allowed))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            provider_wide,
            per_model,
        }
    }

    /// Whether `model_id` may be sent tool definitions on this operation and surface.
    #[must_use]
    pub fn allows(&self, model_id: &str, operation: BedrockOperation, surface: ApiSurface) -> bool {
        if let Some(explicit) = self.per_model.get(model_id) {
            return *explicit;
        }
        if let Some(explicit) = self.provider_wide {
            return explicit;
        }
        // The Mantle surfaces post a real OpenAI Chat or Responses body to
        // `invoke-with-response-stream`. Converse's model table describes what *Converse*
        // accepts and says nothing about those, so the family fallback is not applied to
        // them; `toolCalls` is how an operator decides for a Mantle deployment.
        if operation == BedrockOperation::InvokeModelWithResponseStream
            && matches!(surface, ApiSurface::Chat | ApiSurface::Responses)
        {
            return true;
        }
        converse_documents_tool_use(model_id)
    }

    /// Whether any model of this provider may carry tools, for [`Provider::capabilities`].
    const fn enabled_provider_wide(&self) -> bool {
        !matches!(self.provider_wide, Some(false))
    }
}

/// Whether Bedrock documents Converse tool use for this model id.
fn converse_documents_tool_use(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    if CONVERSE_TOOL_USE_EXCEPTIONS
        .iter()
        .any(|excluded| model_id.contains(excluded))
    {
        return false;
    }
    CONVERSE_TOOL_USE_FAMILIES
        .iter()
        .any(|family| model_id.contains(family))
}

/// The generation controls Bedrock accepts, in Bedrock's own spelling.
///
/// `InferenceConfiguration` in the Bedrock Runtime API reference documents exactly
/// `maxTokens`, `temperature`, `topP` and `stopSequences`, all camelCase. That is a
/// vendor fact, not an unlowered SDK name — the same distinction that makes
/// `reasoningConfig` correct here while `reasoningEffort` was wrong on the OpenAI
/// surfaces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BedrockGeneration {
    /// `inferenceConfig.maxTokens`.
    pub max_tokens: Option<u64>,
    /// `inferenceConfig.temperature`.
    pub temperature: Option<f64>,
    /// `inferenceConfig.topP`.
    pub top_p: Option<f64>,
}

impl BedrockGeneration {
    fn from_spec(spec: &Spec) -> Self {
        Self {
            max_tokens: numeric_option(spec, generation::MAX_TOKENS_KEYS)
                .and_then(|value| u64::try_from(value.trunc() as i64).ok())
                .filter(|value| *value > 0),
            temperature: numeric_option(spec, generation::TEMPERATURE_KEYS),
            top_p: numeric_option(spec, generation::TOP_P_KEYS),
        }
    }

    /// `inferenceConfig`, or `None` when the caller set nothing.
    ///
    /// Omitted rather than sent empty because Converse treats an absent
    /// `inferenceConfig` as "use the model's defaults", which is what a caller who
    /// configured nothing asked for. The oracle makes the same all-absent check
    /// (`packages/llm/src/protocols/bedrock-converse.ts:409-414`).
    fn inference_config(&self) -> Option<Value> {
        let mut config = Map::new();
        if let Some(max_tokens) = self.max_tokens {
            config.insert("maxTokens".to_owned(), Value::from(max_tokens));
        }
        insert_f64(&mut config, "temperature", self.temperature);
        insert_f64(&mut config, "topP", self.top_p);
        (!config.is_empty()).then_some(Value::Object(config))
    }
}

fn insert_f64(target: &mut Map<String, Value>, name: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(serde_json::Number::from_f64) {
        target.insert(name.to_owned(), Value::Number(number));
    }
}

fn numeric_option(spec: &Spec, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| spec.options.get(*key))
        .and_then(Value::as_f64)
}

impl BedrockConfig {
    #[must_use]
    pub fn new(region: impl Into<String>) -> Self {
        let region = region.into();
        Self {
            provider_id: CONVERSE_PROVIDER_ID.to_owned(),
            region: Some(region.clone()),
            endpoint: None,
            operation: BedrockOperation::ConverseStream,
            auth: AwsAuthConfig {
                profile: None,
                region: Some(region),
                service: "bedrock".to_owned(),
            },
            access_keys: None,
            generation: BedrockGeneration::default(),
            tools: BedrockToolPolicy::default(),
        }
    }

    pub fn from_spec(spec: &Spec) -> Result<Self, BedrockBuildError> {
        let region = spec
            .region
            .clone()
            .or_else(|| string_option(spec, "region"));
        let endpoint = spec
            .base_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(BedrockBuildError::InvalidEndpoint)?;
        let access_keys = match (
            string_option(spec, "accessKeyId"),
            string_option(spec, "secretAccessKey"),
        ) {
            (Some(access_key_id), Some(secret_access_key)) => Some(AwsAccessKeys {
                access_key_id,
                secret_access_key,
                session_token: string_option(spec, "sessionToken"),
            }),
            (None, None) => None,
            _ => return Err(BedrockBuildError::IncompleteExplicitCredentials),
        };
        let operation = match string_option(spec, "operation").as_deref() {
            None | Some("converse-stream") => BedrockOperation::ConverseStream,
            Some("invoke-with-response-stream") => BedrockOperation::InvokeModelWithResponseStream,
            Some(value) => return Err(BedrockBuildError::UnknownOperation(value.to_owned())),
        };
        Ok(Self {
            provider_id: if spec.provider.is_empty() {
                CONVERSE_PROVIDER_ID.to_owned()
            } else {
                spec.provider.clone()
            },
            region,
            endpoint,
            operation,
            auth: AwsAuthConfig {
                profile: string_option(spec, "profile"),
                region: spec
                    .region
                    .clone()
                    .or_else(|| string_option(spec, "region")),
                service: "bedrock".to_owned(),
            },
            access_keys,
            generation: BedrockGeneration::from_spec(spec),
            tools: BedrockToolPolicy::from_spec(spec),
        })
    }
}

fn string_option(spec: &Spec, name: &str) -> Option<String> {
    spec.options
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[derive(Clone)]
pub struct BedrockProvider {
    config: BedrockConfig,
    client: reqwest::Client,
    auth: BedrockRequestAuth,
}

impl std::fmt::Debug for BedrockProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl BedrockProvider {
    pub fn new(config: BedrockConfig) -> Result<Self, BedrockBuildError> {
        Self::new_with_bearer(config, None)
    }

    /// Construct a provider with an optional Amazon Bedrock API key.
    pub fn new_with_bearer(
        config: BedrockConfig,
        bearer: Option<BedrockBearerToken>,
    ) -> Result<Self, BedrockBuildError> {
        let client = zuno_network::client_builder()
            .build()
            .map_err(BedrockBuildError::HttpClient)?;
        let auth = BedrockRequestAuth::new(
            config.provider_id.clone(),
            config.auth.clone(),
            config.access_keys.clone(),
            bearer,
        );
        Ok(Self {
            config,
            client,
            auth,
        })
    }

    pub fn from_spec(spec: &Spec) -> Result<Self, BedrockBuildError> {
        Self::new(BedrockConfig::from_spec(spec)?)
    }

    /// Build a provider from a registry spec and optional bearer token.
    pub fn from_spec_with_bearer(
        spec: &Spec,
        bearer: Option<BedrockBearerToken>,
    ) -> Result<Self, BedrockBuildError> {
        Self::new_with_bearer(BedrockConfig::from_spec(spec)?, bearer)
    }

    /// The body one request will carry.
    ///
    /// Exposed for the reason `CompatibleProvider::body_for` is: the generation
    /// controls are a property of the bytes, and asserting them should not require
    /// SigV4 credentials and a socket. [`open_stream`](Self::open_stream) serialises
    /// exactly this value, so an assertion here is an assertion about the wire.
    ///
    /// # Errors
    ///
    /// Whatever the body builder rejects — a non-text system block, or content no
    /// Bedrock operation can express.
    pub fn body_for(&self, request: &CompletionRequest) -> Result<Value, ProviderError> {
        request_body(
            request,
            self.config.operation,
            &self.config.generation,
            &self.config.tools,
        )
    }

    async fn open_stream(
        &self,
        request: CompletionRequest,
    ) -> Result<ProviderStream<'static>, ProviderError> {
        if request.model_id.starts_with("openai.gpt-") || request.model_id.contains(".openai.gpt-")
        {
            return Err(ProviderError::fatal(
                BedrockProtocolError::OpenAiModelRequiresResponses {
                    model: request.model_id,
                },
            ));
        }
        // `invoke-with-response-stream` frames Anthropic Messages events, and this crate
        // decodes only those. An OpenAI Chat chunk or Responses event arrives inside the
        // same `chunk.bytes` envelope and falls through the decoder's ignore arm, which
        // `eventstream::tests::openai_shaped_invoke_chunks_yield_no_events_at_all` pins as
        // *zero* events for both shapes. Signed and sent anyway, the turn bills a whole
        // invocation, decodes nothing, and ends as `upstream_stream_incomplete` — a
        // transient class the engine retries, billing it again on every attempt. The
        // request half of this pairing is real (see `native_body`), so the answer is not
        // to unbuild the body but to refuse the call that cannot be read back, naming the
        // operation that can. `body_for` deliberately stays reachable: the bytes remain
        // inspectable for the day a shared OpenAI stream decoder makes this path work.
        let undecodable_response = match self.config.operation {
            BedrockOperation::ConverseStream => None,
            BedrockOperation::InvokeModelWithResponseStream => match request.surface {
                ApiSurface::Chat => Some("Chat"),
                ApiSurface::Responses => Some("Responses"),
                ApiSurface::Default | ApiSurface::Messages => None,
            },
        };
        if let Some(surface) = undecodable_response {
            return Err(ProviderError::fatal(
                BedrockProtocolError::UndecodableInvokeResponse { surface },
            ));
        }
        let body = serde_json::to_vec(&self.body_for(&request)?).map_err(ProviderError::fatal)?;
        let region = self.auth.region().await?;
        let url = self.request_url(&request.model_id, region)?;
        let mut headers = BTreeMap::from([
            (
                "accept".to_owned(),
                "application/vnd.amazon.eventstream".to_owned(),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
        ]);
        if !self.auth.uses_bearer() {
            headers.insert("x-amz-content-sha256".to_owned(), sha256_hex(&body));
        }
        if self.config.operation == BedrockOperation::InvokeModelWithResponseStream {
            headers.insert(
                "x-amzn-bedrock-accept".to_owned(),
                "application/json".to_owned(),
            );
        }
        headers.extend(request.headers.clone());
        let authorized = self
            .auth
            .authorize(AwsRequestToSign {
                method: Method::POST,
                url: url.to_string(),
                headers: header_map(&headers).map_err(ProviderError::fatal)?,
                body: Bytes::copy_from_slice(&body),
            })
            .await?;
        let mut builder = self.client.post(authorized.url).body(body);
        for (name, value) in &authorized.headers {
            if name != http::header::HOST {
                builder = builder.header(name, value);
            }
        }
        // A response-header deadline is the difference between a stalled peer and a
        // turn that never ends: a load balancer that accepts the connection and then
        // loses its upstream sends nothing at all, and `send()` alone waits forever.
        let deadlines = RequestDeadlines::start(HttpTimeouts::native());
        let provider = self.config.provider_id.clone();
        let response = deadlines
            .headers(&provider, builder.send())
            .await?
            .map_err(ProviderError::transient)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let response_headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect();
            let response_body = read_error_body(&provider, response).await?.into_bytes();
            return Err(classify_bedrock_error_for(
                &provider,
                status,
                &response_headers,
                &response_body,
            ));
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/vnd.amazon.eventstream"))
        {
            return Err(ProviderError::fatal(
                BedrockProtocolError::UnexpectedContentType,
            ));
        }

        let mut queued = VecDeque::new();
        if let Some(request_id) = aws_request_id(response.headers()) {
            queued.push_back(StreamEvent::StatusDetail {
                detail: format!("AWS request ID {request_id}"),
            });
        }
        let state = ResponseState {
            response,
            decoder: BedrockEventDecoder::new(),
            queued,
            finished: false,
            message_ended: false,
            provider,
            model: request.model_id.clone(),
        };
        let output = stream::try_unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.queued.pop_front() {
                    if matches!(event, StreamEvent::MessageEnd { .. }) {
                        state.message_ended = true;
                    }
                    return Ok(Some((event, state)));
                }
                if state.finished {
                    // An EventStream that stops without `messageStop` (Converse) or a
                    // `message_delta` stop reason (`InvokeModelWithResponseStream`) is a
                    // truncated turn, not a short one. Reporting the typed stream failure
                    // lets the engine replay the identical request instead of committing
                    // a partial assistant message as though the model had finished.
                    if !state.message_ended {
                        return Err(upstream_stream_incomplete(&state.provider, &state.model));
                    }
                    return Ok(None);
                }
                let chunk = StreamIdleTimeout::default()
                    .wait(&state.provider, &state.model, state.response.chunk())
                    .await?
                    .map_err(ProviderError::transient)?;
                match chunk {
                    Some(chunk) => {
                        state
                            .queued
                            .extend(state.decoder.push(&chunk).map_err(map_decode_error)?);
                    }
                    None => {
                        state
                            .queued
                            .extend(state.decoder.finish().map_err(map_decode_error)?);
                        state.finished = true;
                    }
                }
            }
        });
        Ok(Box::pin(output))
    }

    fn request_url(&self, model_id: &str, region: &str) -> Result<Url, ProviderError> {
        let base = match &self.config.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => Url::parse(&format!("https://bedrock-runtime.{region}.amazonaws.com"))
                .map_err(ProviderError::fatal)?,
        };
        base.join(&self.config.operation.path(model_id))
            .map_err(ProviderError::fatal)
    }
}

impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.config.provider_id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            reasoning: true,
            // Per-model narrowing happens at the body, because one provider instance
            // serves every model in the account and this answer cannot see which one is
            // about to be called. A configured `toolCalls: false` is the one answer that
            // holds for all of them, and withdrawing the capability here stops the turn
            // loop from assembling a snapshot no body will carry.
            tool_calls: self.config.tools.enabled_provider_wide(),
            prompt_cache: true,
            attachments: true,
            sampling_params: true,
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        Box::pin(stream::once(self.open_stream(request)).try_flatten())
    }
}

struct ResponseState {
    response: reqwest::Response,
    decoder: BedrockEventDecoder,
    queued: VecDeque<StreamEvent>,
    finished: bool,
    message_ended: bool,
    provider: String,
    model: String,
}

pub fn mantle_surface(model_id: &str) -> ApiSurface {
    if matches!(
        model_id,
        "openai.gpt-oss-safeguard-20b" | "openai.gpt-oss-safeguard-120b"
    ) {
        ApiSurface::Chat
    } else {
        ApiSurface::Responses
    }
}

fn request_body(
    request: &CompletionRequest,
    operation: BedrockOperation,
    generation: &BedrockGeneration,
    tools: &BedrockToolPolicy,
) -> Result<Value, ProviderError> {
    // A family Bedrock does not implement tool use for answers a body carrying tool
    // definitions with a `ValidationException`: permanent, so every turn of that
    // configuration fails, where before tools were simply never sent. The snapshot is
    // cleared once, here, ahead of the surface dispatch below, so every body shape this
    // operation can produce inherits one decision instead of each re-deriving it.
    let without_tools;
    let request = if request.tools.is_empty()
        || tools.allows(&request.model_id, operation, request.surface)
    {
        request
    } else {
        without_tools = request.clone().with_tools(Vec::new());
        &without_tools
    };
    let mut value = match operation {
        BedrockOperation::ConverseStream => converse_body(request, generation)?,
        BedrockOperation::InvokeModelWithResponseStream => native_body(request, generation)?,
    };
    // `apply_parameters` takes the surface the bytes were built for, not the one the
    // provider family prefers. Converse is Anthropic-shaped, but Mantle posts a real
    // OpenAI Chat or Responses body, and a per-request parameter such as
    // `reasoningEffort` lowers to a different field name on each of the three.
    let sending_to = match operation {
        BedrockOperation::ConverseStream => ApiSurface::Messages,
        BedrockOperation::InvokeModelWithResponseStream => match request.surface {
            ApiSurface::Default => ApiSurface::Messages,
            surface => surface,
        },
    };
    request.apply_parameters(&mut value, sending_to);
    Ok(value)
}

fn converse_body(
    request: &CompletionRequest,
    generation: &BedrockGeneration,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            for block in &message.content {
                let Some(text) = block.provider_text() else {
                    return Err(ProviderError::fatal(
                        RequestShapeError::UnsupportedSystemBlock,
                    ));
                };
                system.push(json!({"text": text.as_ref()}));
            }
            continue;
        }
        messages.push(json!({
            "role": converse_role(message.role),
            "content": converse_content(&message.content)?,
        }));
    }
    system.extend(
        request
            .developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"text": content})),
    );
    let mut body = Map::from_iter([("messages".to_owned(), Value::Array(messages))]);
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    if let Some(config) = generation.inference_config() {
        body.insert("inferenceConfig".to_owned(), config);
    }
    if !request.tools.is_empty() {
        body.insert(
            "toolConfig".to_owned(),
            json!({
                "tools": request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "toolSpec": {
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": {"json": tool.parameters},
                        }
                    }))
                    .collect::<Vec<_>>()
            }),
        );
    }
    Ok(Value::Object(body))
}

fn converse_role(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        Role::System | Role::User | Role::Tool => "user",
    }
}

fn converse_content(blocks: &[RequestContentBlock]) -> Result<Vec<Value>, ProviderError> {
    blocks
        .iter()
        .map(|block| match block {
            RequestContentBlock::Text { text } => Ok(json!({"text": text})),
            RequestContentBlock::ResourceLink { .. } => {
                let Some(text) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                Ok(json!({"text": text.as_ref()}))
            }
            RequestContentBlock::SignedThinking {
                thinking,
                signature,
            } => Ok(json!({
                "reasoningContent": {
                    "reasoningText": {"text": thinking, "signature": signature}
                }
            })),
            RequestContentBlock::ProviderEncryptedReasoning { .. } => Err(ProviderError::fatal(
                RequestShapeError::EncryptedReasoningUnsupported,
            )),
            RequestContentBlock::ToolUse {
                id, name, input, ..
            } => Ok(json!({
                "toolUse": {"toolUseId": id, "name": name, "input": input}
            })),
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let result = serde_json::from_str::<Value>(content)
                    .map(|value| json!({"json": value}))
                    .unwrap_or_else(|_| json!({"text": content}));
                Ok(json!({
                    "toolResult": {
                        "toolUseId": tool_use_id,
                        "content": [result],
                        "status": if is_error.unwrap_or(false) { "error" } else { "success" }
                    }
                }))
            }
            RequestContentBlock::Image {
                media_type, data, ..
            } => Ok(json!({
                "image": {
                    "format": media_type.rsplit('/').next().unwrap_or(media_type),
                    "source": {"bytes": data}
                }
            })),
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
        })
        .collect()
}

fn native_body(
    request: &CompletionRequest,
    generation: &BedrockGeneration,
) -> Result<Value, ProviderError> {
    match request.surface {
        ApiSurface::Messages | ApiSurface::Default => anthropic_native_body(request, generation),
        ApiSurface::Chat => {
            let mut messages = openai_messages(&request.messages)?;
            append_openai_developer_context(&mut messages, &request.developer_context, "developer");
            let mut body = json!({
                "model": request.model_id,
                "stream": true,
                "messages": messages,
            });
            insert_openai_tools(&mut body, request, OpenAiToolEnvelope::Nested);
            insert_openai_generation(&mut body, "max_tokens", generation);
            Ok(body)
        }
        ApiSurface::Responses => {
            let (instructions, input) = openai_responses_input(request)?;
            let mut body = json!({
                "model": request.model_id,
                "stream": true,
                "input": input,
            });
            if let Some(instructions) = instructions {
                body["instructions"] = Value::String(instructions);
            }
            insert_openai_tools(&mut body, request, OpenAiToolEnvelope::Flat);
            insert_openai_generation(&mut body, "max_output_tokens", generation);
            Ok(body)
        }
    }
}

/// Write the generation controls onto a Mantle body under its OpenAI-surface names.
///
/// Mantle posts an OpenAI-shaped body to a Bedrock action, so the output cap is
/// spelled as that surface spells it — `max_tokens` on chat, `max_output_tokens` on
/// responses — and *not* as `inferenceConfig`, which belongs to Converse alone.
fn insert_openai_generation(body: &mut Value, cap: &str, generation: &BedrockGeneration) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(max_tokens) = generation.max_tokens {
        object.insert(cap.to_owned(), Value::from(max_tokens));
    }
    insert_f64(object, "temperature", generation.temperature);
    insert_f64(object, "top_p", generation.top_p);
}

fn anthropic_native_body(
    request: &CompletionRequest,
    generation: &BedrockGeneration,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            for block in &message.content {
                let Some(text) = block.provider_text() else {
                    return Err(ProviderError::fatal(
                        RequestShapeError::UnsupportedSystemBlock,
                    ));
                };
                system.push(json!({"type": "text", "text": text.as_ref()}));
            }
        } else {
            messages.push(json!({
                "role": converse_role(message.role),
                "content": anthropic_content(&message.content)?,
            }));
        }
    }
    system.extend(
        request
            .developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"type": "text", "text": content})),
    );
    let mut body = Map::from_iter([
        (
            "anthropic_version".to_owned(),
            Value::String("bedrock-2023-05-31".to_owned()),
        ),
        // Required by Anthropic's Messages schema (`max_tokens: Schema.Number`,
        // `packages/llm/src/protocols/anthropic-messages.ts:163`), so unlike every
        // other generation control this one has a fallback rather than being omitted.
        (
            "max_tokens".to_owned(),
            Value::from(
                generation
                    .max_tokens
                    .unwrap_or(ANTHROPIC_NATIVE_DEFAULT_MAX_TOKENS),
            ),
        ),
        ("messages".to_owned(), Value::Array(messages)),
    ]);
    insert_f64(&mut body, "temperature", generation.temperature);
    insert_f64(&mut body, "top_p", generation.top_p);
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters,
                        })
                    })
                    .collect(),
            ),
        );
    }
    Ok(Value::Object(body))
}

fn anthropic_content(blocks: &[RequestContentBlock]) -> Result<Vec<Value>, ProviderError> {
    blocks
        .iter()
        .map(|block| match block {
            RequestContentBlock::Text { text } => Ok(json!({"type": "text", "text": text})),
            RequestContentBlock::ResourceLink { .. } => {
                let Some(text) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                Ok(json!({"type": "text", "text": text.as_ref()}))
            }
            RequestContentBlock::SignedThinking {
                thinking,
                signature,
            } => Ok(json!({
                "type": "thinking", "thinking": thinking, "signature": signature
            })),
            RequestContentBlock::ToolUse {
                id, name, input, ..
            } => Ok(json!({
                "type": "tool_use", "id": id, "name": name, "input": input
            })),
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Ok(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error.unwrap_or(false)
            })),
            RequestContentBlock::Image {
                media_type, data, ..
            } => Ok(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            })),
            RequestContentBlock::ProviderEncryptedReasoning { .. } => Err(ProviderError::fatal(
                RequestShapeError::EncryptedReasoningUnsupported,
            )),
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
        })
        .collect()
}

/// How the surface nests a tool definition: Chat under `function`, Responses flat.
#[derive(Clone, Copy)]
enum OpenAiToolEnvelope {
    Nested,
    Flat,
}

/// Write the locked tool snapshot onto a Mantle body in that surface's envelope.
///
/// The snapshot is a property of the request, not of the provider instance: it is what
/// the turn loop locked and what tool dispatch is checked against, so a body built
/// without it advertises `tool_calls` while making a tool call impossible.
fn insert_openai_tools(
    body: &mut Value,
    request: &CompletionRequest,
    envelope: OpenAiToolEnvelope,
) {
    if request.tools.is_empty() {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let tools = request
        .tools
        .iter()
        .map(|tool| match envelope {
            OpenAiToolEnvelope::Nested => json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }
            }),
            OpenAiToolEnvelope::Flat => json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            }),
        })
        .collect::<Vec<_>>();
    object.insert("tools".to_owned(), Value::Array(tools));
}

/// Lower a history into OpenAI Chat messages.
///
/// One provider-neutral message can become several wire messages: Chat carries tool
/// results as their own `role: "tool"` entries rather than as blocks inside the message
/// that produced them.
fn openai_messages(messages: &[Message]) -> Result<Vec<Value>, ProviderError> {
    let mut wire = Vec::new();
    for message in messages {
        openai_chat_message(message, &mut wire)?;
    }
    Ok(wire)
}

fn openai_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Chat's `image_url` part, which takes an inline data URL rather than raw bytes.
fn openai_image_part(media_type: &str, data: &str) -> Value {
    json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media_type};base64,{data}")}
    })
}

fn openai_chat_message(message: &Message, wire: &mut Vec<Value>) -> Result<(), ProviderError> {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    for block in &message.content {
        match block {
            RequestContentBlock::ToolUse {
                id,
                name,
                input,
                raw_arguments,
                ..
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": tool_arguments_text(input, raw_arguments.as_deref()),
                },
            })),
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_results.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            })),
            RequestContentBlock::Image {
                media_type, data, ..
            } => images.push(openai_image_part(media_type, data)),
            // Neither OpenAI surface has a field for an Anthropic-signed thinking
            // block, and Mantle does not ask for reasoning to be replayed, so this is
            // dropped rather than refused — the turn continues without it.
            RequestContentBlock::SignedThinking { .. } => {}
            RequestContentBlock::ProviderEncryptedReasoning { .. } => {
                return Err(ProviderError::fatal(
                    RequestShapeError::EncryptedReasoningUnsupported,
                ));
            }
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
            RequestContentBlock::Text { .. } | RequestContentBlock::ResourceLink { .. } => {
                let Some(value) = block.provider_text() else {
                    return Err(ProviderError::fatal(
                        RequestShapeError::OpenAiBlockUnsupported,
                    ));
                };
                text.push_str(value.as_ref());
            }
        }
    }

    // A message that produced no wire item of its own still becomes an empty-content
    // message, which is what the text-only path has always sent.
    let produced_items = !tool_calls.is_empty() || !tool_results.is_empty();
    if !text.is_empty() || !images.is_empty() || !produced_items {
        let mut value = Map::from_iter([(
            "role".to_owned(),
            Value::String(openai_role(message.role).to_owned()),
        )]);
        if images.is_empty() {
            value.insert("content".to_owned(), Value::String(text));
        } else {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(json!({"type": "text", "text": text}));
            }
            parts.extend(images);
            value.insert("content".to_owned(), Value::Array(parts));
        }
        if !tool_calls.is_empty() {
            value.insert("tool_calls".to_owned(), Value::Array(tool_calls));
            tool_calls = Vec::new();
        }
        wire.push(Value::Object(value));
    }
    if !tool_calls.is_empty() {
        wire.push(json!({
            "role": openai_role(message.role),
            "tool_calls": tool_calls,
        }));
    }
    wire.extend(tool_results);
    Ok(())
}

fn append_openai_developer_context(
    messages: &mut Vec<Value>,
    developer_context: &[String],
    role: &str,
) {
    messages.extend(
        developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"role": role, "content": content})),
    );
}

/// Lower a history into OpenAI Responses `input` items.
///
/// Responses is an item list, not a message list: a tool call and its result are
/// sibling `function_call` / `function_call_output` items rather than blocks inside a
/// message, so one provider-neutral message can produce several items.
fn openai_responses_input(
    request: &CompletionRequest,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let mut instructions = None;
    let mut input = Vec::new();
    for message in &request.messages {
        let mut text = String::new();
        let mut images = Vec::new();
        let mut items = Vec::new();
        for block in &message.content {
            match block {
                RequestContentBlock::ToolUse {
                    id,
                    name,
                    input: arguments,
                    raw_arguments,
                    ..
                } => items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": tool_arguments_text(arguments, raw_arguments.as_deref()),
                })),
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => items.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
                })),
                RequestContentBlock::Image {
                    media_type, data, ..
                } => images.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                })),
                // See `openai_chat_message`: no field exists for it and Mantle does
                // not ask for reasoning replay.
                RequestContentBlock::SignedThinking { .. } => {}
                RequestContentBlock::ProviderEncryptedReasoning { .. } => {
                    return Err(ProviderError::fatal(
                        RequestShapeError::EncryptedReasoningUnsupported,
                    ));
                }
                RequestContentBlock::ImageAttachment { .. } => {
                    unreachable!(
                        "attachment references must be resolved before provider request shaping"
                    )
                }
                RequestContentBlock::Text { .. } | RequestContentBlock::ResourceLink { .. } => {
                    let Some(value) = block.provider_text() else {
                        return Err(ProviderError::fatal(
                            RequestShapeError::OpenAiBlockUnsupported,
                        ));
                    };
                    text.push_str(value.as_ref());
                }
            }
        }

        if message.role == Role::System && instructions.is_none() && images.is_empty() {
            instructions = Some(text);
            input.extend(items);
            continue;
        }
        if !text.is_empty() || !images.is_empty() || items.is_empty() {
            let role = if message.role == Role::System {
                "developer"
            } else {
                openai_role(message.role)
            };
            let content = if images.is_empty() {
                Value::String(text)
            } else {
                let mut parts = Vec::new();
                if !text.is_empty() {
                    parts.push(json!({"type": "input_text", "text": text}));
                }
                parts.extend(images);
                Value::Array(parts)
            };
            input.push(json!({"role": role, "content": content}));
        }
        input.extend(items);
    }
    append_openai_developer_context(&mut input, &request.developer_context, "developer");
    Ok((instructions.filter(|value| !value.is_empty()), input))
}

fn map_decode_error(error: BedrockDecodeError) -> ProviderError {
    match error {
        BedrockDecodeError::Provider(error) => error,
        BedrockDecodeError::Framing(error) => ProviderError::fatal(error),
        BedrockDecodeError::Payload(error) => ProviderError::fatal(error),
    }
}

fn sha256_hex(value: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(value);
    let mut output = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, thiserror::Error)]
pub enum BedrockBuildError {
    #[error("invalid Bedrock endpoint")]
    InvalidEndpoint(#[source] url::ParseError),
    #[error("Bedrock explicit credentials require both accessKeyId and secretAccessKey")]
    IncompleteExplicitCredentials,
    #[error("unknown Bedrock streaming operation `{0}`")]
    UnknownOperation(String),
    #[error("failed to construct Bedrock HTTP client")]
    HttpClient(#[source] reqwest::Error),
}

#[derive(Debug, thiserror::Error)]
enum RequestShapeError {
    #[error("Bedrock system messages only accept text blocks")]
    UnsupportedSystemBlock,
    #[error("Bedrock cannot replay another provider's encrypted reasoning item")]
    EncryptedReasoningUnsupported,
    #[error("the selected Bedrock OpenAI surface currently accepts text message blocks only")]
    OpenAiBlockUnsupported,
}

#[derive(Debug, thiserror::Error)]
enum BedrockProtocolError {
    #[error("Bedrock streaming response did not use application/vnd.amazon.eventstream")]
    UnexpectedContentType,
    #[error(
        "model `{model}` uses the OpenAI Responses protocol; select `amazon-bedrock` for \
         Mantle or `amazon-bedrock-runtime` for Runtime instead of \
         `amazon-bedrock-converse`"
    )]
    OpenAiModelRequiresResponses { model: String },
    /// An OpenAI-shaped body was about to be posted to `invoke-with-response-stream`,
    /// whose reply this crate cannot decode.
    ///
    /// Fatal rather than transient on purpose: the combination is a property of the
    /// configuration, so every retry would reproduce it exactly, at the price of
    /// another billed invocation.
    #[error(
        "Bedrock `invoke-with-response-stream` cannot decode an OpenAI {surface} response; \
         set `options.operation` to `converse-stream` for this model"
    )]
    UndecodableInvokeResponse { surface: &'static str },
}

pub fn factory(spec: Spec) -> Result<Arc<dyn Provider>, BedrockBuildError> {
    Ok(Arc::new(BedrockProvider::from_spec(&spec)?))
}

/// Build the Converse provider with an optional Amazon Bedrock API key.
pub fn factory_with_bearer(
    spec: Spec,
    bearer: Option<BedrockBearerToken>,
) -> Result<Arc<dyn Provider>, BedrockBuildError> {
    Ok(Arc::new(BedrockProvider::from_spec_with_bearer(
        &spec, bearer,
    )?))
}

fn aws_request_id(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    ["x-amzn-requestid", "x-amzn-request-id"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn converse_request_matches_the_recorded_minimal_shape() {
        let request = CompletionRequest::new(
            "us.amazon.nova-micro-v1:0",
            vec![
                Message::new(Role::System, "Reply with one word."),
                Message::new(Role::User, "Say hello."),
            ],
        );
        assert_eq!(
            converse_body(&request, &BedrockGeneration::default()).expect("request body"),
            json!({
                "messages": [{"role": "user", "content": [{"text": "Say hello."}]}],
                "system": [{"text": "Reply with one word."}]
            }),
            "a provider configured with no generation controls must still send the \
             pre-`inferenceConfig` shape byte for byte"
        );
    }

    #[tokio::test]
    async fn converse_bearer_token_reaches_the_authorization_header_without_leaking() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/us.anthropic.claude-opus-5/converse-stream"))
            .and(header("authorization", "Bearer bedrock-api-key"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(json!({"message": "request denied"})),
            )
            .mount(&server)
            .await;
        let spec = Spec::new("amazon-bedrock")
            .with_region("us-east-2")
            .with_base_url(server.uri());
        let provider = BedrockProvider::from_spec_with_bearer(
            &spec,
            Some(BedrockBearerToken::new("bedrock-api-key")),
        )
        .expect("provider");

        let error = provider
            .stream(CompletionRequest::new(
                "us.anthropic.claude-opus-5",
                vec![Message::new(Role::User, "hello")],
            ))
            .next()
            .await
            .expect("one result")
            .expect_err("fixture rejects the request");

        assert!(!error.to_string().contains("bedrock-api-key"));
    }

    #[test]
    fn developer_context_uses_native_system_or_developer_items() {
        let base = CompletionRequest::new(
            "model-under-test",
            vec![
                Message::new(Role::System, "stable kernel"),
                Message::new(Role::User, "exact user text"),
            ],
        )
        .with_developer_context(vec!["active goal".to_owned(), "memory".to_owned()]);

        let converse = converse_body(&base, &BedrockGeneration::default()).expect("Converse body");
        assert_eq!(
            converse["system"],
            json!([
                {"text": "stable kernel"},
                {"text": "active goal"},
                {"text": "memory"},
            ])
        );

        let messages = native_body(
            &base.clone().on_surface(ApiSurface::Messages),
            &BedrockGeneration::default(),
        )
        .expect("Anthropic body");
        assert_eq!(
            messages["system"],
            json!([
                {"type": "text", "text": "stable kernel"},
                {"type": "text", "text": "active goal"},
                {"type": "text", "text": "memory"},
            ])
        );

        let responses = native_body(
            &base.on_surface(ApiSurface::Responses),
            &BedrockGeneration::default(),
        )
        .expect("Mantle Responses body");
        assert_eq!(responses["instructions"], "stable kernel");
        assert_eq!(responses["input"][0]["role"], "user");
        assert_eq!(
            responses["input"][1],
            json!({"role": "developer", "content": "active goal"})
        );
        assert_eq!(
            responses["input"][2],
            json!({"role": "developer", "content": "memory"})
        );
    }

    fn read_tool() -> zuno_llm::registry::ToolSchema {
        zuno_llm::registry::ToolSchema {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }
    }

    fn tool_request(surface: ApiSurface) -> CompletionRequest {
        CompletionRequest::new(
            "anthropic.claude-sonnet-4-5",
            vec![Message::new(Role::User, "Read the file.")],
        )
        .on_surface(surface)
        .with_tools(vec![read_tool()])
    }

    /// The turn loop locks its tool snapshot into `CompletionRequest::tools`, so every
    /// Bedrock body must show the model those tools in that operation's own envelope;
    /// anything else advertises `tool_calls` while making a tool call impossible.
    #[test]
    fn the_locked_tool_snapshot_reaches_every_bedrock_body_shape() {
        let converse = converse_body(
            &tool_request(ApiSurface::Default),
            &BedrockGeneration::default(),
        )
        .expect("Converse body");
        assert_eq!(
            converse["toolConfig"],
            json!({"tools": [{"toolSpec": {
                "name": "read",
                "description": "Read a file",
                "inputSchema": {"json": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }},
            }}]})
        );

        let messages = native_body(
            &tool_request(ApiSurface::Messages),
            &BedrockGeneration::default(),
        )
        .expect("Anthropic body");
        assert_eq!(
            messages["tools"],
            json!([{
                "name": "read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                },
            }])
        );

        let chat = native_body(
            &tool_request(ApiSurface::Chat),
            &BedrockGeneration::default(),
        )
        .expect("Mantle Chat body");
        assert_eq!(chat["tools"][0]["type"], "function");
        assert_eq!(chat["tools"][0]["function"]["name"], "read");

        let responses = native_body(
            &tool_request(ApiSurface::Responses),
            &BedrockGeneration::default(),
        )
        .expect("Mantle Responses body");
        assert_eq!(
            responses["tools"],
            json!([{
                "type": "function",
                "name": "read",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                },
            }])
        );
    }

    /// One model id, one policy, through the production entry point for that operation.
    fn body_for_model(
        model_id: &str,
        operation: BedrockOperation,
        surface: ApiSurface,
        policy: &BedrockToolPolicy,
    ) -> Value {
        let request =
            CompletionRequest::new(model_id, vec![Message::new(Role::User, "Read the file.")])
                .on_surface(surface)
                .with_tools(vec![read_tool()]);
        request_body(&request, operation, &BedrockGeneration::default(), policy)
            .expect("request body")
    }

    fn spec_with(options: Value) -> Spec {
        let mut spec = Spec::new("amazon-bedrock");
        for (name, value) in options.as_object().expect("options are an object") {
            spec = spec.with_option(name.clone(), value.clone());
        }
        spec
    }

    /// A model family Converse does not implement tool use for.
    ///
    /// Converse answers `toolConfig` for these with a `ValidationException`, which is
    /// permanent: every turn of the configuration fails. 0.6.6 sent no tool definitions at
    /// all on this transport, so the model simply answered in prose, and that is what an
    /// unrecognised or documented-tool-less family must keep doing.
    #[test]
    fn a_tool_less_bedrock_family_carries_no_tool_definitions() {
        let policy = BedrockToolPolicy::default();
        for model_id in [
            "amazon.titan-text-express-v1",
            "amazon.titan-text-lite-v1",
            "amazon.titan-text-premier-v1:0",
            "meta.llama2-13b-chat-v1",
            "meta.llama3-8b-instruct-v1:0",
            "anthropic.claude-v2:1",
            "anthropic.claude-instant-v1",
            "mistral.mistral-7b-instruct-v0:2",
            "cohere.command-text-v14",
            "arn:aws:bedrock:us-east-1:123456789012:provisioned-model/abcdefghijkl",
        ] {
            let converse = body_for_model(
                model_id,
                BedrockOperation::ConverseStream,
                ApiSurface::Default,
                &policy,
            );
            assert!(
                converse.get("toolConfig").is_none(),
                "`{model_id}` answers a toolConfig with a ValidationException, and this \
                 body carried one: {converse}"
            );
            let native = body_for_model(
                model_id,
                BedrockOperation::InvokeModelWithResponseStream,
                ApiSurface::Messages,
                &policy,
            );
            assert!(
                native.get("tools").is_none(),
                "the Anthropic-native invoke body reads the same snapshot: {native}"
            );
        }
    }

    /// The families Bedrock documents tool use for still carry the snapshot, including
    /// through a cross-region routing prefix and an inference-profile ARN.
    #[test]
    fn a_tool_capable_bedrock_family_still_carries_the_locked_snapshot() {
        let policy = BedrockToolPolicy::default();
        for model_id in [
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.amazon.nova-pro-v1:0",
            "amazon.nova-micro-v1:0",
            "meta.llama3-1-70b-instruct-v1:0",
            "us.meta.llama3-2-11b-instruct-v1:0",
            "meta.llama4-maverick-17b-instruct-v1:0",
            "mistral.mistral-large-2407-v1:0",
            "cohere.command-r-plus-v1:0",
            "ai21.jamba-1-5-large-v1:0",
        ] {
            let converse = body_for_model(
                model_id,
                BedrockOperation::ConverseStream,
                ApiSurface::Default,
                &policy,
            );
            assert_eq!(
                converse["toolConfig"]["tools"][0]["toolSpec"]["name"],
                json!("read"),
                "`{model_id}` accepts tool use, and a body without it advertises \
                 `tool_calls` while making a tool call impossible"
            );
        }
    }

    /// `toolCalls` is the provider-wide lever over the family table, in both directions.
    #[test]
    fn the_tool_calls_option_overrides_the_family_table_in_both_directions() {
        let forced = BedrockToolPolicy::from_spec(&spec_with(json!({"toolCalls": true})));
        let converse = body_for_model(
            "amazon.titan-text-express-v1",
            BedrockOperation::ConverseStream,
            ApiSurface::Default,
            &forced,
        );
        assert_eq!(
            converse["toolConfig"]["tools"][0]["toolSpec"]["name"],
            json!("read"),
            "an operator who knows a model accepts tool use can say so"
        );

        let withheld = BedrockToolPolicy::from_spec(&spec_with(json!({"toolCalls": false})));
        let converse = body_for_model(
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            BedrockOperation::ConverseStream,
            ApiSurface::Default,
            &withheld,
        );
        assert!(
            converse.get("toolConfig").is_none(),
            "an operator who wants no tool traffic at all can say that too"
        );
        assert!(
            !withheld.enabled_provider_wide(),
            "`toolCalls: false` also withdraws the capability, so no snapshot is assembled"
        );
        assert!(BedrockToolPolicy::default().enabled_provider_wide());
    }

    /// `modelCapabilities.<model>.tool_calls` decides one model and leaves the rest alone,
    /// in the spelling the compatible transport already reads.
    #[test]
    fn a_per_model_capability_decides_exactly_that_model() {
        let policy = BedrockToolPolicy::from_spec(&spec_with(json!({
            "modelCapabilities": {
                "amazon.titan-text-express-v1": {"tool_calls": true},
                "anthropic.claude-sonnet-4-5-20250929-v1:0": {"tool_calls": false},
            }
        })));

        assert_eq!(
            body_for_model(
                "amazon.titan-text-express-v1",
                BedrockOperation::ConverseStream,
                ApiSurface::Default,
                &policy,
            )["toolConfig"]["tools"][0]["toolSpec"]["name"],
            json!("read")
        );
        assert!(
            body_for_model(
                "anthropic.claude-sonnet-4-5-20250929-v1:0",
                BedrockOperation::ConverseStream,
                ApiSurface::Default,
                &policy,
            )
            .get("toolConfig")
            .is_none(),
            "a catalog entry that declares no tool support overrides the family table"
        );
        assert_eq!(
            body_for_model(
                "us.amazon.nova-pro-v1:0",
                BedrockOperation::ConverseStream,
                ApiSurface::Default,
                &policy,
            )["toolConfig"]["tools"][0]["toolSpec"]["name"],
            json!("read"),
            "a per-model answer is not a provider-wide one"
        );
    }

    /// The Mantle surfaces are not Converse, so Converse's model table does not decide for
    /// them: an `openai.*` id keeps the OpenAI-shaped `tools` array it was given.
    #[test]
    fn the_mantle_surfaces_keep_their_tools_for_an_openai_model_id() {
        let policy = BedrockToolPolicy::default();
        let chat = body_for_model(
            "openai.gpt-oss-120b",
            BedrockOperation::InvokeModelWithResponseStream,
            ApiSurface::Chat,
            &policy,
        );
        assert_eq!(chat["tools"][0]["function"]["name"], json!("read"));

        let responses = body_for_model(
            "openai.gpt-oss-120b",
            BedrockOperation::InvokeModelWithResponseStream,
            ApiSurface::Responses,
            &policy,
        );
        assert_eq!(responses["tools"][0]["name"], json!("read"));

        let withheld = BedrockToolPolicy::from_spec(&spec_with(json!({"toolCalls": false})));
        assert!(
            body_for_model(
                "openai.gpt-oss-120b",
                BedrockOperation::InvokeModelWithResponseStream,
                ApiSurface::Chat,
                &withheld,
            )
            .get("tools")
            .is_none(),
            "`toolCalls: false` is the lever that covers a Mantle deployment"
        );
    }

    /// `capabilities()` claims `attachments` and `tool_calls` for both operations, so the
    /// Mantle surfaces have to be able to express the content that claim invites. They
    /// previously rejected every non-text block as a fatal request-shape error, which
    /// turned the first image attachment — and, once tools are sent, the first tool
    /// result — into an aborted turn.
    #[test]
    fn mantle_surfaces_express_the_content_their_capabilities_claim() {
        let history = vec![
            Message::from_content(
                Role::User,
                vec![
                    RequestContentBlock::Text {
                        text: "What is this?".to_owned(),
                    },
                    RequestContentBlock::Image {
                        media_type: "image/png".to_owned(),
                        data: "aGk=".to_owned(),
                        filename: None,
                    },
                ],
            ),
            Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ToolUse {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "a.txt"}),
                    raw_arguments: None,
                    thought_signature: None,
                }],
            ),
            Message::from_content(
                Role::Tool,
                vec![RequestContentBlock::ToolResult {
                    tool_use_id: "call_1".to_owned(),
                    content: "file body".to_owned(),
                    is_error: None,
                }],
            ),
        ];
        let request = CompletionRequest::new("openai.gpt-oss-120b", history);

        let chat = native_body(
            &request.clone().on_surface(ApiSurface::Chat),
            &BedrockGeneration::default(),
        )
        .expect("a Chat body must accept image, tool-call and tool-result content");
        assert_eq!(
            chat["messages"][0]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}})
        );
        assert_eq!(chat["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a.txt"}"#
        );
        assert_eq!(
            chat["messages"][2],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file body"})
        );

        let responses = native_body(
            &request.on_surface(ApiSurface::Responses),
            &BedrockGeneration::default(),
        )
        .expect("a Responses body must accept the same content");
        assert_eq!(
            responses["input"][0]["content"][1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,aGk="})
        );
        assert_eq!(
            responses["input"][1],
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": r#"{"path":"a.txt"}"#,
            })
        );
        assert_eq!(
            responses["input"][2],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "file body",
            })
        );
    }

    /// `apply_parameters` takes the surface the bytes were built for. Mantle posts a real
    /// OpenAI Responses body, so a per-request `reasoningEffort` has to lower to
    /// `reasoning.effort` there, not to Chat's `reasoning_effort`.
    #[test]
    fn mantle_parameters_lower_against_the_surface_the_body_was_built_for() {
        let mut parameters = Map::new();
        parameters.insert("reasoningEffort".to_owned(), json!("high"));
        let request = CompletionRequest::new(
            "openai.gpt-oss-120b",
            vec![Message::new(Role::User, "Say hello.")],
        )
        .on_surface(ApiSurface::Responses)
        .with_parameters(parameters.clone());

        let responses = request_body(
            &request,
            BedrockOperation::InvokeModelWithResponseStream,
            &BedrockGeneration::default(),
            &BedrockToolPolicy::default(),
        )
        .expect("Mantle Responses body");
        assert_eq!(responses["reasoning"], json!({"effort": "high"}));
        assert!(responses.get("reasoning_effort").is_none());

        let chat = request_body(
            &request.on_surface(ApiSurface::Chat),
            BedrockOperation::InvokeModelWithResponseStream,
            &BedrockGeneration::default(),
            &BedrockToolPolicy::default(),
        )
        .expect("Mantle Chat body");
        assert_eq!(chat["reasoning_effort"], json!("high"));

        let converse = request_body(
            &CompletionRequest::new(
                "anthropic.claude-sonnet-4-5",
                vec![Message::new(Role::User, "Say hello.")],
            )
            .with_parameters(parameters),
            BedrockOperation::ConverseStream,
            &BedrockGeneration::default(),
            &BedrockToolPolicy::default(),
        )
        .expect("Converse body");
        assert_eq!(
            converse["reasoning_effort"],
            json!("high"),
            "Converse is Anthropic-shaped and keeps the non-Responses lowering"
        );
    }

    fn converse_provider(operation: BedrockOperation) -> BedrockProvider {
        let mut config = BedrockConfig::new("us-east-1");
        config.operation = operation;
        config.access_keys = Some(AwsAccessKeys {
            access_key_id: "AKIAREPLAY".to_owned(),
            secret_access_key: "replay-secret".to_owned(),
            session_token: None,
        });
        BedrockProvider::new(config).expect("a Converse provider with explicit credentials")
    }

    /// OpenAI GPT model ids never reach the Converse or native-model invocation paths.
    ///
    /// This guard is before credential resolution and signing, so a wrong provider cannot
    /// bill a request before telling the user which Responses provider owns the model.
    #[tokio::test]
    async fn an_openai_model_is_refused_by_converse_before_credentials_or_network() {
        use futures::StreamExt as _;

        let request = CompletionRequest::new(
            "openai.gpt-oss-120b",
            vec![Message::new(Role::User, "Say hello.")],
        );
        let refused = converse_provider(BedrockOperation::ConverseStream)
            .stream(request)
            .next()
            .await
            .expect("the refusal is the first item of the stream")
            .expect_err("an OpenAI model must not be requested through Converse");
        assert!(
            !refused.is_retryable(),
            "a mismatch the peer cannot fix must not bill a second invocation: {refused:?}"
        );
        assert!(
            matches!(&refused, ProviderError::Fatal { source: Some(source), .. }
            if matches!(
                source.downcast_ref::<BedrockProtocolError>(),
                Some(BedrockProtocolError::OpenAiModelRequiresResponses { .. })
            )),
            "the typed repair must name the Responses provider: {refused:?}"
        );
    }

    #[test]
    fn operation_paths_percent_encode_model_identifiers() {
        assert_eq!(
            BedrockOperation::ConverseStream.path("us.amazon.nova-micro-v1:0"),
            "/model/us.amazon.nova-micro-v1%3A0/converse-stream"
        );
        assert_eq!(
            BedrockOperation::InvokeModelWithResponseStream.path("arn:aws:bedrock:x/model/y"),
            "/model/arn%3Aaws%3Abedrock%3Ax%2Fmodel%2Fy/invoke-with-response-stream"
        );
    }
}
