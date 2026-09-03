//! Google AI Studio Gemini, Vertex Gemini, and Vertex-hosted Anthropic.
//!
//! The three providers are intentionally separate concrete types. Google AI
//! Studio and Vertex Gemini share Gemini's `contents`/`parts` codec, while Vertex
//! Anthropic uses Anthropic Messages JSON against Vertex's `streamRawPredict`
//! endpoint. None of these paths uses an OpenAI message serializer.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine as _;
use futures::{StreamExt as _, TryStreamExt as _};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use url::Url;
use zuno_error::ProviderError;
use zuno_llm::effort::{
    DeclaredVariants, EffortCapabilities, ProviderFamily, ReasoningEffort, resolve_effort,
};
use zuno_llm::event::{
    FinishReason, Message, PromptAccounting, RequestContentBlock, Role, StreamEvent,
    ThoughtSignature,
};
use zuno_llm::http::{
    HttpTimeouts, MAX_ERROR_BODY_BYTES, RequestDeadlines, map_messages_http_error,
    map_messages_stream_error_value, read_error_body, truncated_body_error,
};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, FactoryOutcome, Provider,
    ProviderStream, Spec, Unavailable, generation,
};
use zuno_llm::sse::{
    SseEvent, SseParser, StreamIdleTimeout, ensure_tool_input_size, upstream_stream_incomplete,
};

/// Registry identity for Google AI Studio's Generative Language API.
pub const GOOGLE_PROVIDER_ID: &str = "google";
/// Registry identity for Gemini on Vertex AI.
pub const VERTEX_PROVIDER_ID: &str = "google-vertex";
/// Registry identity for Anthropic models published through Vertex AI.
pub const VERTEX_ANTHROPIC_PROVIDER_ID: &str = "google-vertex/anthropic";

const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// A configuration or request-shaping failure detected before network I/O.
#[derive(Debug, thiserror::Error)]
pub enum GoogleProviderError {
    /// A URL supplied by configuration was malformed.
    #[error("invalid Google provider URL `{url}`")]
    InvalidUrl {
        /// The rejected URL.
        url: String,
        /// URL parser detail.
        #[source]
        source: url::ParseError,
    },
    /// A base URL could not accept path segments.
    #[error("Google provider base URL `{url}` cannot be used as a hierarchical endpoint")]
    NonHierarchicalUrl {
        /// The rejected URL.
        url: String,
    },
    /// A required endpoint component was empty or invalid.
    #[error("invalid Vertex {field} `{value}`")]
    InvalidEndpointComponent {
        /// Which component was invalid.
        field: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A provider-neutral content block cannot be represented by this wire format.
    #[error("{provider} cannot serialize {block} content in a {role:?} message")]
    UnsupportedContent {
        /// Provider path doing the conversion.
        provider: &'static str,
        /// Message role containing the block.
        role: Role,
        /// Stable block kind, not user content.
        block: &'static str,
    },
    /// A tool result did not have a prior tool call from which Gemini can recover its name.
    #[error("Gemini tool result references unknown tool call `{tool_use_id}`")]
    UnknownToolResult {
        /// Provider-neutral tool call id.
        tool_use_id: String,
    },
    /// A configured ADC file could not be read.
    #[error("failed to read Google application-default credentials `{path}`")]
    CredentialFileIo {
        /// Credential file path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// An ADC or service-account JSON document was malformed.
    #[error("invalid Google credential JSON")]
    CredentialJson {
        /// JSON decoder detail, which contains no credential values.
        #[source]
        source: serde_json::Error,
    },
    /// ADC found a credential kind this implementation cannot safely use.
    #[error("unsupported Google application-default credential type `{credential_type}`")]
    UnsupportedCredentialType {
        /// The `type` discriminant from the credential file.
        credential_type: String,
    },
    /// A credential JSON document omitted its type discriminant.
    #[error("Google credential JSON has no string `type` field")]
    MissingCredentialType,
    /// The HTTP client could not be constructed.
    #[error("failed to construct Google provider HTTP client")]
    HttpClient {
        /// Client builder detail.
        #[source]
        source: reqwest::Error,
    },
}

/// A fully shaped outbound HTTP request before authentication is attached.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRequest {
    /// Absolute endpoint URL.
    pub url: String,
    /// Non-secret headers.
    pub headers: BTreeMap<String, String>,
    /// Provider-native JSON body.
    pub body: Value,
}

impl PreparedRequest {
    fn new(url: String, body: Value) -> Self {
        Self {
            url,
            headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            body,
        }
    }
}

/// Gemini generation controls represented with the API's own field names.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GeminiGenerationConfig {
    /// Maximum generated tokens.
    pub max_output_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling probability.
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff.
    pub top_k: Option<u64>,
    /// Stop strings.
    pub stop_sequences: Vec<String>,
}

/// One Gemini function declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiToolDefinition {
    /// Function name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Gemini-compatible JSON Schema.
    pub parameters: Value,
}

/// Gemini function-calling policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiToolChoice {
    /// Let the model decide whether to call a function.
    Auto,
    /// Disable function calls.
    None,
    /// Require some declared function.
    Required,
    /// Require one named function.
    Tool(String),
}

/// Request-shaping options shared by Google AI Studio and Vertex Gemini.
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiOptions {
    /// Optional API-prefix override. Google expects a prefix such as `/v1beta`.
    pub base_url: Option<String>,
    /// Canonical reasoning effort to resolve through `zuno_llm::effort`.
    pub effort: Option<ReasoningEffort>,
    /// Catalog-declared reasoning capabilities; no model-name inference occurs.
    pub effort_capabilities: EffortCapabilities,
    /// Exact catalog variant overrides, which take precedence over generic mapping.
    pub variants: DeclaredVariants,
    /// Ordinary generation controls.
    pub generation: GeminiGenerationConfig,
    /// Function declarations.
    pub tools: Vec<GeminiToolDefinition>,
    /// Function-calling policy.
    pub tool_choice: Option<GeminiToolChoice>,
}

impl Default for GeminiOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            effort: None,
            effort_capabilities: EffortCapabilities::default(),
            variants: DeclaredVariants::new(),
            generation: GeminiGenerationConfig::default(),
            tools: Vec::new(),
            tool_choice: None,
        }
    }
}

impl GeminiOptions {
    fn from_spec(spec: &Spec) -> Self {
        Self {
            base_url: spec.base_url.clone(),
            generation: GeminiGenerationConfig {
                // Gemini's own field name first, then the cross-provider option the
                // composition root writes: `gemini.ts:307` builds
                // `maxOutputTokens: generation?.maxTokens`, so both spellings name one
                // value and a config that used the Gemini name keeps winning.
                max_output_tokens: option_u64(
                    spec,
                    &[
                        "maxOutputTokens",
                        "max_output_tokens",
                        generation::MAX_TOKENS,
                        "max_tokens",
                    ],
                ),
                temperature: option_f64(spec, generation::TEMPERATURE_KEYS),
                top_p: option_f64(spec, generation::TOP_P_KEYS),
                top_k: option_u64(spec, &["topK", "top_k"]),
                stop_sequences: option_array(spec, &["stopSequences", "stop_sequences"])
                    .into_iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            },
            ..Self::default()
        }
    }
}

/// Resolve a canonical effort and return only Google API's `thinkingConfig` value.
///
/// This is deliberately a projection of [`zuno_llm::effort::resolve_effort`], not
/// another effort policy. Catalog variants and capabilities therefore behave
/// identically in the turn engine and this provider.
#[must_use]
pub fn google_thinking_config(
    effort: ReasoningEffort,
    capabilities: EffortCapabilities,
    variants: &DeclaredVariants,
) -> Value {
    resolve_effort(ProviderFamily::Google, effort, capabilities, variants)
        .options
        .remove("thinkingConfig")
        .unwrap_or(Value::Null)
}

#[derive(Clone)]
struct SecretString(String);

impl SecretString {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Google AI Studio provider using the Gemini wire protocol.
#[derive(Clone)]
pub struct GoogleGenerativeAi {
    client: Client,
    api_key: SecretString,
    options: GeminiOptions,
}

impl fmt::Debug for GoogleGenerativeAi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleGenerativeAi")
            .field("id", &GOOGLE_PROVIDER_ID)
            .field("api_key", &self.api_key)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl GoogleGenerativeAi {
    /// Construct the Google AI Studio path with an API key.
    pub fn new(
        api_key: impl Into<String>,
        options: GeminiOptions,
    ) -> Result<Self, GoogleProviderError> {
        Ok(Self {
            client: provider_client()?,
            api_key: SecretString::new(api_key),
            options,
        })
    }

    /// Shape a provider-neutral completion as Gemini `contents` and `parts`.
    pub fn prepare(
        &self,
        request: &CompletionRequest,
    ) -> Result<PreparedRequest, GoogleProviderError> {
        let base = self.options.base_url.as_deref().unwrap_or(GOOGLE_BASE_URL);
        let url = gemini_api_endpoint(base, &request.model_id)?;
        let mut body =
            build_gemini_body(&request.messages, &request.developer_context, &self.options)?;
        // Gemini's `generateContent` is neither OpenAI surface; `Messages` selects
        // no OpenAI-specific rename, and the Google rule nests `thinkingConfig`
        // under `generationConfig` on every surface.
        request.apply_parameters(&mut body, ApiSurface::Messages);
        let mut prepared = PreparedRequest::new(url, body);
        prepared.headers.extend(request.headers.clone());
        Ok(prepared)
    }
}

impl Provider for GoogleGenerativeAi {
    fn id(&self) -> &str {
        GOOGLE_PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        gemini_capabilities()
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let prepared = match self.prepare(&request) {
            Ok(prepared) => prepared,
            Err(error) => return one_error(ProviderError::fatal(error)),
        };
        let client = self.client.clone();
        let api_key = self.api_key.expose().to_owned();
        let model = request.model_id;
        Box::pin(
            futures::stream::once(async move {
                open_json_stream(
                    client,
                    prepared,
                    Some(("x-goog-api-key", api_key)),
                    GeminiStreamDecoder::new(GOOGLE_PROVIDER_ID, model.clone()),
                    GOOGLE_PROVIDER_ID,
                    model,
                    classify_gemini_http_error,
                )
                .await
            })
            .try_flatten(),
        )
    }
}

/// Gemini on Vertex AI. Authentication and endpoint routing differ from AI Studio.
#[derive(Clone)]
pub struct VertexGemini {
    client: Client,
    project: String,
    location: String,
    credentials: VertexCredentials,
    options: GeminiOptions,
}

impl fmt::Debug for VertexGemini {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VertexGemini")
            .field("id", &VERTEX_PROVIDER_ID)
            .field("project", &self.project)
            .field("location", &self.location)
            .field("credentials", &self.credentials)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl VertexGemini {
    /// Construct the Vertex Gemini path.
    pub fn new(
        project: impl Into<String>,
        location: impl Into<String>,
        credentials: VertexCredentials,
        options: GeminiOptions,
    ) -> Result<Self, GoogleProviderError> {
        let project = project.into();
        let location = location.into();
        validate_endpoint_component("project", &project, false)?;
        validate_endpoint_component("location", &location, true)?;
        Ok(Self {
            client: provider_client()?,
            project,
            location,
            credentials,
            options,
        })
    }

    /// Shape a Vertex Gemini request.
    pub fn prepare(
        &self,
        request: &CompletionRequest,
    ) -> Result<PreparedRequest, GoogleProviderError> {
        let url = if let Some(base) = self.options.base_url.as_deref() {
            gemini_api_endpoint(base, &request.model_id)?
        } else {
            vertex_gemini_endpoint(&self.project, &self.location, &request.model_id)?
        };
        let mut body =
            build_gemini_body(&request.messages, &request.developer_context, &self.options)?;
        // Gemini's `generateContent` is neither OpenAI surface; `Messages` selects
        // no OpenAI-specific rename, and the Google rule nests `thinkingConfig`
        // under `generationConfig` on every surface.
        request.apply_parameters(&mut body, ApiSurface::Messages);
        let mut prepared = PreparedRequest::new(url, body);
        prepared.headers.extend(request.headers.clone());
        Ok(prepared)
    }
}

impl Provider for VertexGemini {
    fn id(&self) -> &str {
        VERTEX_PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        gemini_capabilities()
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let prepared = match self.prepare(&request) {
            Ok(prepared) => prepared,
            Err(error) => return one_error(ProviderError::fatal(error)),
        };
        let client = self.client.clone();
        let credentials = self.credentials.clone();
        let model = request.model_id;
        Box::pin(
            futures::stream::once(async move {
                let token = credentials.obtain_access_token(&client).await?;
                open_json_stream(
                    client,
                    prepared,
                    Some(("authorization", format!("Bearer {token}"))),
                    GeminiStreamDecoder::new(VERTEX_PROVIDER_ID, model.clone()),
                    VERTEX_PROVIDER_ID,
                    model,
                    classify_gemini_http_error,
                )
                .await
            })
            .try_flatten(),
        )
    }
}

fn gemini_capabilities() -> Capabilities {
    Capabilities {
        reasoning: true,
        tool_calls: true,
        prompt_cache: false,
        attachments: true,
        sampling_params: true,
    }
}

fn build_gemini_body(
    messages: &[Message],
    developer_context: &[String],
    options: &GeminiOptions,
) -> Result<Value, GoogleProviderError> {
    let tool_names = collect_tool_names(messages);
    let mut system = Vec::new();
    let mut contents = Vec::new();

    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                let Some(text) = block.provider_text() else {
                    return Err(unsupported_content(
                        GOOGLE_PROVIDER_ID,
                        message.role,
                        block_kind(block),
                    ));
                };
                system.push(json!({"text": text.as_ref()}));
            }
            continue;
        }

        let role = match message.role {
            Role::Assistant => "model",
            Role::User | Role::Tool => "user",
            Role::System => unreachable!("system messages returned above"),
        };
        let mut parts = Vec::new();
        for block in &message.content {
            let part = lower_gemini_block(message.role, block, &tool_names)?;
            parts.push(part);
        }
        contents.push(json!({"role": role, "parts": parts}));
    }

    system.extend(
        developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"text": content})),
    );

    let mut body = Map::new();
    body.insert("contents".to_owned(), Value::Array(contents));
    if !system.is_empty() {
        body.insert("systemInstruction".to_owned(), json!({"parts": system}));
    }
    if !options.tools.is_empty() && !matches!(options.tool_choice, Some(GeminiToolChoice::None)) {
        let declarations: Vec<Value> = options
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        body.insert(
            "tools".to_owned(),
            json!([{"functionDeclarations": declarations}]),
        );
        if let Some(choice) = &options.tool_choice {
            let function_config = match choice {
                GeminiToolChoice::Auto => json!({"mode": "AUTO"}),
                GeminiToolChoice::None => json!({"mode": "NONE"}),
                GeminiToolChoice::Required => json!({"mode": "ANY"}),
                GeminiToolChoice::Tool(name) => {
                    json!({"mode": "ANY", "allowedFunctionNames": [name]})
                }
            };
            body.insert(
                "toolConfig".to_owned(),
                json!({"functionCallingConfig": function_config}),
            );
        }
    }

    let mut generation = Map::new();
    insert_optional_u64(
        &mut generation,
        "maxOutputTokens",
        options.generation.max_output_tokens,
    );
    insert_optional_f64(
        &mut generation,
        "temperature",
        options.generation.temperature,
    );
    insert_optional_f64(&mut generation, "topP", options.generation.top_p);
    insert_optional_u64(&mut generation, "topK", options.generation.top_k);
    if !options.generation.stop_sequences.is_empty() {
        generation.insert(
            "stopSequences".to_owned(),
            json!(options.generation.stop_sequences),
        );
    }
    if let Some(effort) = options.effort {
        generation.insert(
            "thinkingConfig".to_owned(),
            google_thinking_config(effort, options.effort_capabilities, &options.variants),
        );
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_owned(), Value::Object(generation));
    }
    Ok(Value::Object(body))
}

fn collect_tool_names(messages: &[Message]) -> HashMap<&str, &str> {
    let mut names = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let RequestContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    names
}

fn lower_gemini_block(
    role: Role,
    block: &RequestContentBlock,
    tool_names: &HashMap<&str, &str>,
) -> Result<Value, GoogleProviderError> {
    match (role, block) {
        (Role::User, RequestContentBlock::Text { text })
        | (Role::Assistant, RequestContentBlock::Text { text }) => Ok(json!({"text": text})),
        (Role::User | Role::Assistant, RequestContentBlock::ResourceLink { .. }) => {
            let Some(text) = block.provider_text() else {
                unreachable!("resource links always have a provider text projection")
            };
            Ok(json!({"text": text.as_ref()}))
        }
        (
            Role::User,
            RequestContentBlock::Image {
                media_type, data, ..
            },
        ) => Ok(json!({"inlineData": {"mimeType": media_type, "data": data}})),
        (
            Role::Assistant,
            RequestContentBlock::SignedThinking {
                thinking,
                signature,
            },
        ) => Ok(json!({
            "text": thinking,
            "thought": true,
            "thoughtSignature": signature,
        })),
        (
            Role::Assistant,
            RequestContentBlock::ToolUse {
                name,
                input,
                thought_signature,
                ..
            },
        ) => {
            let mut part = Map::from_iter([(
                "functionCall".to_owned(),
                json!({"name": name, "args": input}),
            )]);
            if let Some(signature) = thought_signature {
                part.insert(
                    "thoughtSignature".to_owned(),
                    Value::String(signature.as_str().to_owned()),
                );
            }
            Ok(Value::Object(part))
        }
        (
            Role::Tool,
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            },
        ) => {
            let name = tool_names.get(tool_use_id.as_str()).ok_or_else(|| {
                GoogleProviderError::UnknownToolResult {
                    tool_use_id: tool_use_id.clone(),
                }
            })?;
            Ok(json!({
                "functionResponse": {
                    "name": name,
                    "response": {"name": name, "content": content},
                }
            }))
        }
        _ => Err(unsupported_content(
            GOOGLE_PROVIDER_ID,
            role,
            block_kind(block),
        )),
    }
}

fn insert_optional_u64(map: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Value::from(value));
    }
}

fn insert_optional_f64(map: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        let number = if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64
        {
            Some(serde_json::Number::from(value as i64))
        } else {
            serde_json::Number::from_f64(value)
        };
        if let Some(number) = number {
            map.insert(key.to_owned(), Value::Number(number));
        }
    }
}

fn gemini_api_endpoint(base_url: &str, model: &str) -> Result<String, GoogleProviderError> {
    validate_endpoint_component("model", model, false)?;
    let mut url = parse_url(base_url)?;
    {
        let original = url.to_string();
        let mut segments =
            url.path_segments_mut()
                .map_err(|()| GoogleProviderError::NonHierarchicalUrl {
                    url: original.clone(),
                })?;
        segments.pop_if_empty();
        segments.push("models");
        segments.push(&format!("{model}:streamGenerateContent"));
    }
    url.set_query(Some("alt=sse"));
    Ok(url.into())
}

/// Build the documented Vertex Gemini endpoint for global or regional locations.
pub fn vertex_gemini_endpoint(
    project: &str,
    location: &str,
    model: &str,
) -> Result<String, GoogleProviderError> {
    validate_endpoint_component("project", project, false)?;
    validate_endpoint_component("location", location, true)?;
    validate_endpoint_component("model", model, false)?;
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_owned()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    vertex_endpoint(&host, project, location, "google", model, true)
}

/// Build the documented Vertex-Anthropic endpoint.
///
/// Continental `us` and `eu` locations use Regional Endpoint Platform hosts;
/// ordinary regions use `<region>-aiplatform.googleapis.com`, while `global`
/// uses `aiplatform.googleapis.com`.
pub fn vertex_anthropic_endpoint(
    project: &str,
    location: &str,
    model: &str,
) -> Result<String, GoogleProviderError> {
    validate_endpoint_component("project", project, false)?;
    validate_endpoint_component("location", location, true)?;
    validate_endpoint_component("model", model, false)?;
    let host = match location {
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        "global" => "aiplatform.googleapis.com".to_owned(),
        region => format!("{region}-aiplatform.googleapis.com"),
    };
    vertex_endpoint(&host, project, location, "anthropic", model, false)
}

fn vertex_endpoint(
    host: &str,
    project: &str,
    location: &str,
    publisher: &str,
    model: &str,
    alt_sse: bool,
) -> Result<String, GoogleProviderError> {
    let mut url = parse_url(&format!("https://{host}/v1"))?;
    {
        let original = url.to_string();
        let mut segments =
            url.path_segments_mut()
                .map_err(|()| GoogleProviderError::NonHierarchicalUrl {
                    url: original.clone(),
                })?;
        segments.pop_if_empty();
        segments.extend([
            "projects",
            project,
            "locations",
            location,
            "publishers",
            publisher,
            "models",
        ]);
        let action = if alt_sse {
            format!("{model}:streamGenerateContent")
        } else {
            format!("{model}:streamRawPredict")
        };
        segments.push(&action);
    }
    if alt_sse {
        url.set_query(Some("alt=sse"));
    }
    Ok(url.into())
}

fn parse_url(value: &str) -> Result<Url, GoogleProviderError> {
    Url::parse(value).map_err(|source| GoogleProviderError::InvalidUrl {
        url: value.to_owned(),
        source,
    })
}

fn validate_endpoint_component(
    field: &'static str,
    value: &str,
    dns_label: bool,
) -> Result<(), GoogleProviderError> {
    let valid = !value.is_empty()
        && !value.chars().any(char::is_whitespace)
        && (!dns_label
            || value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));
    if valid {
        Ok(())
    } else {
        Err(GoogleProviderError::InvalidEndpointComponent {
            field,
            value: value.to_owned(),
        })
    }
}

/// Incremental Gemini SSE decoder backed by the workspace's shared UTF-8 parser.
#[derive(Debug)]
pub struct GeminiStreamDecoder {
    provider: String,
    model: String,
    parser: SseParser,
    next_tool_call_id: u64,
    has_tool_calls: bool,
    reasoning_open: bool,
    message_ended: bool,
}

impl GeminiStreamDecoder {
    /// Construct a decoder with provider/model context for typed parse errors.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        let model = model.into();
        Self {
            parser: SseParser::for_stream(provider.clone(), model.clone()),
            provider,
            model,
            next_tool_call_id: 0,
            has_tool_calls: false,
            reasoning_open: false,
            message_ended: false,
        }
    }

    /// Decode one raw network chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self
            .parser
            .push(chunk)
            .into_iter()
            .collect::<Result<_, _>>()?;
        self.decode_frames(frames)
    }

    /// Finish UTF-8 and SSE framing and decode any trailing event.
    ///
    /// # Errors
    ///
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`] when the byte stream
    /// ended before any candidate carried a `finishReason`. Gemini closes every
    /// completed generation with one, so its absence means the response was cut
    /// short. Reporting it as a typed stream failure lets the caller replay the
    /// identical request instead of committing a partial answer as a finished turn.
    ///
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`]: zuno_error::ProviderStreamFailure::UpstreamStreamIncomplete
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.finish().into_iter().collect::<Result<_, _>>()?;
        let events = self.decode_frames(frames)?;
        if !self.message_ended {
            return Err(upstream_stream_incomplete(&self.provider, &self.model));
        }
        Ok(events)
    }

    fn decode_frames(&mut self, frames: Vec<SseEvent>) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut output = Vec::new();
        for frame in frames {
            let event: GeminiResponse = frame.deserialize(&self.provider, &self.model)?;
            self.decode_event(event, &mut output)?;
        }
        Ok(output)
    }

    fn decode_event(
        &mut self,
        event: GeminiResponse,
        output: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let mut finish_reason = None;
        for candidate in event.candidates {
            if let Some(content) = candidate.content {
                for part in content.parts {
                    self.decode_part(part, output)?;
                }
            }
            if candidate.finish_reason.is_some() {
                finish_reason = candidate.finish_reason;
            }
        }
        if let Some(usage) = event.usage_metadata {
            output.push(StreamEvent::TokenUsage {
                input_tokens: usage.prompt_token_count,
                output_tokens: usage
                    .candidates_token_count
                    .map(|visible| visible.saturating_add(usage.thoughts_token_count.unwrap_or(0))),
                cache_read_input_tokens: usage.cached_content_token_count,
                cache_write_input_tokens: None,
                // `cachedContentTokenCount` is part of `promptTokenCount`.
                accounting: PromptAccounting::CacheInsideInput,
            });
        }
        if let Some(reason) = finish_reason
            && !self.message_ended
        {
            self.close_reasoning(output);
            output.push(StreamEvent::MessageEnd {
                stop_reason: Some(map_gemini_finish_reason(&reason, self.has_tool_calls)),
            });
            self.message_ended = true;
        }
        Ok(())
    }

    fn decode_part(
        &mut self,
        part: GeminiResponsePart,
        output: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        if part.thought.unwrap_or(false) {
            if !self.reasoning_open {
                output.push(StreamEvent::ReasoningStart);
                self.reasoning_open = true;
            }
            if let Some(text) = part.text.filter(|text| !text.is_empty()) {
                output.push(StreamEvent::ReasoningDelta(text));
            }
            if let Some(signature) = part.thought_signature.filter(|value| !value.is_empty()) {
                output.push(StreamEvent::ReasoningSignatureDelta(signature));
            }
            return Ok(());
        }

        if let Some(text) = part.text.filter(|text| !text.is_empty()) {
            self.close_reasoning(output);
            output.push(StreamEvent::TextDelta(text));
        }
        if let Some(call) = part.function_call {
            self.close_reasoning(output);
            let id = format!("tool_{}", self.next_tool_call_id);
            self.next_tool_call_id += 1;
            output.push(StreamEvent::ToolUseStart {
                id: id.clone(),
                name: call.name,
            });
            let input = serde_json::to_string(&call.args).map_err(|source| {
                ProviderError::fatal(GeminiPayloadError {
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    source,
                })
            })?;
            ensure_tool_input_size(
                input.len(),
                &self.provider,
                &self.model,
                self.parser.limits().max_tool_input_bytes(),
            )?;
            output.push(StreamEvent::ToolInputDelta {
                id: id.clone(),
                delta: input,
            });
            if let Some(signature) = part.thought_signature {
                output.push(StreamEvent::ToolUseSignature {
                    id: id.clone(),
                    signature: ThoughtSignature::new(signature),
                });
            }
            output.push(StreamEvent::ToolUseEnd { id });
            self.has_tool_calls = true;
        }
        Ok(())
    }

    fn close_reasoning(&mut self, output: &mut Vec<StreamEvent>) {
        if self.reasoning_open {
            output.push(StreamEvent::ReasoningEnd);
            self.reasoning_open = false;
        }
    }
}

impl WireDecoder for GeminiStreamDecoder {
    fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        self.push(chunk)
    }

    fn finish_bytes(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        self.finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponsePart {
    text: Option<String>,
    thought: Option<bool>,
    thought_signature: Option<String>,
    function_call: Option<GeminiFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsage {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    cached_content_token_count: Option<u64>,
    thoughts_token_count: Option<u64>,
}

fn map_gemini_finish_reason(reason: &str, has_tool_calls: bool) -> FinishReason {
    match reason {
        "STOP" if has_tool_calls => FinishReason::ToolCalls,
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "IMAGE_SAFETY" | "RECITATION" | "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            FinishReason::ContentFilter
        }
        "MALFORMED_FUNCTION_CALL" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

#[derive(Debug)]
struct GeminiPayloadError {
    provider: String,
    model: String,
    source: serde_json::Error,
}

impl fmt::Display for GeminiPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to serialize Gemini tool input for provider `{}` model `{}`",
            self.provider, self.model
        )
    }
}

impl std::error::Error for GeminiPayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Vertex-Anthropic request options. The body remains Anthropic-shaped.
#[derive(Debug, Clone, PartialEq)]
pub struct VertexAnthropicOptions {
    /// Optional base models URL override, excluding model id and action suffix.
    pub base_url: Option<String>,
    /// Anthropic's Vertex API version body field.
    pub anthropic_version: String,
    /// Maximum generated tokens.
    pub max_tokens: u64,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff, sent as `top_p`.
    pub top_p: Option<f64>,
    /// Additional system text placed before system-role request messages.
    pub system: Vec<String>,
    /// Anthropic tool definitions (`name`, `description`, `input_schema`).
    pub tools: Vec<Value>,
    /// Anthropic-native `tool_choice` object.
    pub tool_choice: Option<Value>,
    /// Canonical reasoning effort resolved with the Anthropic family policy.
    pub effort: Option<ReasoningEffort>,
    /// Catalog-declared reasoning capabilities.
    pub effort_capabilities: EffortCapabilities,
    /// Exact catalog variants.
    pub variants: DeclaredVariants,
}

impl Default for VertexAnthropicOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            anthropic_version: VERTEX_ANTHROPIC_VERSION.to_owned(),
            max_tokens: 4_096,
            temperature: None,
            top_p: None,
            system: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            effort: None,
            effort_capabilities: EffortCapabilities::default(),
            variants: DeclaredVariants::new(),
        }
    }
}

impl VertexAnthropicOptions {
    fn from_spec(spec: &Spec) -> Self {
        let mut options = Self {
            base_url: spec.base_url.clone(),
            ..Self::default()
        };
        if let Some(version) = &spec.api_version {
            options.anthropic_version.clone_from(version);
        }
        if let Some(max_tokens) = option_u64(spec, generation::MAX_TOKENS_KEYS) {
            options.max_tokens = max_tokens;
        }
        options.temperature = option_f64(spec, generation::TEMPERATURE_KEYS);
        options.top_p = option_f64(spec, generation::TOP_P_KEYS);
        options.tool_choice = spec
            .options
            .iter()
            .find_map(|(name, value)| {
                generation::TOOL_CHOICE_KEYS
                    .contains(&name.as_str())
                    .then_some(value)
            })
            .cloned();
        options
    }
}

/// Anthropic Messages protocol transported through Vertex AI.
#[derive(Clone)]
pub struct VertexAnthropic {
    client: Client,
    project: String,
    location: String,
    credentials: VertexCredentials,
    options: VertexAnthropicOptions,
}

impl fmt::Debug for VertexAnthropic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VertexAnthropic")
            .field("id", &VERTEX_ANTHROPIC_PROVIDER_ID)
            .field("project", &self.project)
            .field("location", &self.location)
            .field("credentials", &self.credentials)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl VertexAnthropic {
    /// Construct the Vertex-hosted Anthropic path.
    pub fn new(
        project: impl Into<String>,
        location: impl Into<String>,
        credentials: VertexCredentials,
        options: VertexAnthropicOptions,
    ) -> Result<Self, GoogleProviderError> {
        let project = project.into();
        let location = location.into();
        validate_endpoint_component("project", &project, false)?;
        validate_endpoint_component("location", &location, true)?;
        Ok(Self {
            client: provider_client()?,
            project,
            location,
            credentials,
            options,
        })
    }

    /// Shape a request as Anthropic Messages JSON at Vertex `streamRawPredict`.
    pub fn prepare(
        &self,
        request: &CompletionRequest,
    ) -> Result<PreparedRequest, GoogleProviderError> {
        let url = match self.options.base_url.as_deref() {
            Some(base) => vertex_anthropic_custom_endpoint(base, &request.model_id)?,
            None => vertex_anthropic_endpoint(&self.project, &self.location, &request.model_id)?,
        };
        let mut body = build_vertex_anthropic_body(
            &request.messages,
            &request.developer_context,
            &self.options,
        )?;
        request.apply_parameters(&mut body, ApiSurface::Messages);
        let mut prepared = PreparedRequest::new(url, body);
        prepared.headers.extend(request.headers.clone());
        Ok(prepared)
    }

    /// Construct the Anthropic SSE decoder used by this path.
    #[must_use]
    pub fn stream_decoder(&self, model: impl Into<String>) -> AnthropicStreamDecoder {
        AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, model)
    }
}

impl Provider for VertexAnthropic {
    fn id(&self) -> &str {
        VERTEX_ANTHROPIC_PROVIDER_ID
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
        let prepared = match self.prepare(&request) {
            Ok(prepared) => prepared,
            Err(error) => return one_error(ProviderError::fatal(error)),
        };
        let client = self.client.clone();
        let credentials = self.credentials.clone();
        let model = request.model_id;
        Box::pin(
            futures::stream::once(async move {
                let token = credentials.obtain_access_token(&client).await?;
                open_json_stream(
                    client,
                    prepared,
                    Some(("authorization", format!("Bearer {token}"))),
                    AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, model.clone()),
                    VERTEX_ANTHROPIC_PROVIDER_ID,
                    model,
                    // Vertex-hosted Anthropic answers in the Anthropic Messages error
                    // format, not Gemini's. Reading it through Gemini's classifier
                    // recognised none of its `error.type` values, so an HTTP 400
                    // `prompt_too_long` became an unrecoverable `Fatal` instead of a
                    // request for compaction.
                    map_messages_http_error,
                )
                .await
            })
            .try_flatten(),
        )
    }
}

fn vertex_anthropic_custom_endpoint(
    base: &str,
    model: &str,
) -> Result<String, GoogleProviderError> {
    validate_endpoint_component("model", model, false)?;
    let mut url = parse_url(base)?;
    {
        let original = url.to_string();
        let mut segments =
            url.path_segments_mut()
                .map_err(|()| GoogleProviderError::NonHierarchicalUrl {
                    url: original.clone(),
                })?;
        segments.pop_if_empty();
        segments.push(&format!("{model}:streamRawPredict"));
    }
    Ok(url.into())
}

fn build_vertex_anthropic_body(
    messages: &[Message],
    developer_context: &[String],
    options: &VertexAnthropicOptions,
) -> Result<Value, GoogleProviderError> {
    let mut system = options.system.clone();
    let mut wire_messages = Vec::new();
    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                let Some(text) = block.provider_text() else {
                    return Err(unsupported_content(
                        VERTEX_ANTHROPIC_PROVIDER_ID,
                        message.role,
                        block_kind(block),
                    ));
                };
                system.push(text.into_owned());
            }
            continue;
        }
        let role = match message.role {
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
            Role::System => unreachable!("system messages returned above"),
        };
        let mut content = Vec::new();
        for block in &message.content {
            content.push(lower_anthropic_block(message.role, block)?);
        }
        wire_messages.push(json!({"role": role, "content": content}));
    }
    system.extend(
        developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .cloned(),
    );

    let mut body = Map::from_iter([
        (
            "anthropic_version".to_owned(),
            Value::String(options.anthropic_version.clone()),
        ),
        ("messages".to_owned(), Value::Array(wire_messages)),
        ("stream".to_owned(), Value::Bool(true)),
        ("max_tokens".to_owned(), Value::from(options.max_tokens)),
    ]);
    if !system.is_empty() {
        body.insert(
            "system".to_owned(),
            Value::Array(
                system
                    .into_iter()
                    .map(|text| json!({"type": "text", "text": text}))
                    .collect(),
            ),
        );
    }
    if !options.tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(options.tools.clone()));
    }
    if let Some(tool_choice) = &options.tool_choice {
        body.insert("tool_choice".to_owned(), tool_choice.clone());
    }
    insert_optional_f64(&mut body, "temperature", options.temperature);
    insert_optional_f64(&mut body, "top_p", options.top_p);
    if let Some(effort) = options.effort {
        resolve_effort(
            ProviderFamily::Anthropic,
            effort,
            options.effort_capabilities,
            &options.variants,
        )
        .apply_to(&mut body, ApiSurface::Messages);
    }
    Ok(Value::Object(body))
}

fn lower_anthropic_block(
    role: Role,
    block: &RequestContentBlock,
) -> Result<Value, GoogleProviderError> {
    match (role, block) {
        (Role::User, RequestContentBlock::Text { text })
        | (Role::Assistant, RequestContentBlock::Text { text }) => {
            Ok(json!({"type": "text", "text": text}))
        }
        (Role::User | Role::Assistant, RequestContentBlock::ResourceLink { .. }) => {
            let Some(text) = block.provider_text() else {
                unreachable!("resource links always have a provider text projection")
            };
            Ok(json!({"type": "text", "text": text.as_ref()}))
        }
        (
            Role::User,
            RequestContentBlock::Image {
                media_type, data, ..
            },
        ) => Ok(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        })),
        (
            Role::Assistant,
            RequestContentBlock::SignedThinking {
                thinking,
                signature,
            },
        ) => Ok(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        })),
        (
            Role::Assistant,
            RequestContentBlock::ToolUse {
                id,
                name,
                input,
                thought_signature: None,
            },
        ) => Ok(json!({"type": "tool_use", "id": id, "name": name, "input": input})),
        (
            Role::Tool,
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        ) => {
            let mut value = Map::from_iter([
                ("type".to_owned(), Value::String("tool_result".to_owned())),
                ("tool_use_id".to_owned(), Value::String(tool_use_id.clone())),
                ("content".to_owned(), Value::String(content.clone())),
            ]);
            if let Some(is_error) = is_error {
                value.insert("is_error".to_owned(), Value::Bool(*is_error));
            }
            Ok(Value::Object(value))
        }
        _ => Err(unsupported_content(
            VERTEX_ANTHROPIC_PROVIDER_ID,
            role,
            block_kind(block),
        )),
    }
}

/// Incremental Anthropic Messages SSE decoder used only by Vertex-Anthropic.
#[derive(Debug)]
pub struct AnthropicStreamDecoder {
    provider: String,
    model: String,
    parser: SseParser,
    blocks: HashMap<u64, AnthropicBlockKind>,
    message_ended: bool,
}

#[derive(Debug, Clone)]
enum AnthropicBlockKind {
    Text,
    Tool { id: String },
    Thinking,
}

impl AnthropicStreamDecoder {
    /// Construct a decoder with provider/model context.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        let model = model.into();
        Self {
            parser: SseParser::for_stream(provider.clone(), model.clone()),
            provider,
            model,
            blocks: HashMap::new(),
            message_ended: false,
        }
    }

    /// Decode one raw network chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self
            .parser
            .push(chunk)
            .into_iter()
            .collect::<Result<_, _>>()?;
        self.decode_frames(frames)
    }

    /// Finish UTF-8 and SSE framing.
    ///
    /// # Errors
    ///
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`] when neither
    /// `message_stop` nor a `message_delta` stop reason arrived. Either one alone
    /// terminates the message, so requiring both would permanently fail a peer
    /// that sends only one; requiring neither committed a truncated turn as
    /// though the model had finished.
    ///
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`]: zuno_error::ProviderStreamFailure::UpstreamStreamIncomplete
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.finish().into_iter().collect::<Result<_, _>>()?;
        let events = self.decode_frames(frames)?;
        if !self.message_ended {
            return Err(upstream_stream_incomplete(&self.provider, &self.model));
        }
        Ok(events)
    }

    fn decode_frames(&mut self, frames: Vec<SseEvent>) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut output = Vec::new();
        for frame in frames {
            let value: Value = frame.deserialize(&self.provider, &self.model)?;
            self.decode_value(&value, &mut output)?;
        }
        Ok(output)
    }

    fn decode_value(
        &mut self,
        value: &Value,
        output: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        match value.get("type").and_then(Value::as_str) {
            Some("ping" | "message_start") => {}
            Some("content_block_start") => self.start_block(value, output),
            Some("content_block_delta") => self.delta_block(value, output),
            Some("content_block_stop") => self.stop_block(value, output),
            Some("message_delta") => self.message_delta(value, output),
            Some("message_stop") => {
                if !self.message_ended {
                    output.push(StreamEvent::MessageEnd {
                        stop_reason: Some(FinishReason::Unknown),
                    });
                    self.message_ended = true;
                }
            }
            // The Anthropic Messages error vocabulary is shared with the first-party
            // Anthropic provider, so it is classified by the one shared
            // implementation. The local four-arm copy this replaced lost
            // `prompt_too_long`, `context_window_exceeded`, `api_error`, and
            // `refusal_error`, which turned a compactable context overflow into an
            // unrecoverable failure.
            Some("error") => {
                return Err(map_messages_stream_error_value(&self.provider, value));
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    fn start_block(&mut self, value: &Value, output: &mut Vec<StreamEvent>) {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block = &value["content_block"];
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.blocks
                    .insert(index, AnthropicBlockKind::Tool { id: id.clone() });
                output.push(StreamEvent::ToolUseStart {
                    id,
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            Some("thinking") => {
                self.blocks.insert(index, AnthropicBlockKind::Thinking);
                output.push(StreamEvent::ReasoningStart);
            }
            _ => {
                self.blocks.insert(index, AnthropicBlockKind::Text);
            }
        }
    }

    fn delta_block(&self, value: &Value, output: &mut Vec<StreamEvent>) {
        let delta = &value["delta"];
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    output.push(StreamEvent::TextDelta(text.to_owned()));
                }
            }
            Some("input_json_delta") => {
                if let Some(json) = delta.get("partial_json").and_then(Value::as_str) {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let id = match self.blocks.get(&index) {
                        Some(AnthropicBlockKind::Tool { id }) => id.clone(),
                        _ => String::new(),
                    };
                    output.push(StreamEvent::ToolInputDelta {
                        id,
                        delta: json.to_owned(),
                    });
                }
            }
            Some("thinking_delta") => {
                if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                    output.push(StreamEvent::ReasoningDelta(thinking.to_owned()));
                }
            }
            Some("signature_delta") => {
                if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                    output.push(StreamEvent::ReasoningSignatureDelta(signature.to_owned()));
                }
            }
            _ => {}
        }
    }

    fn stop_block(&mut self, value: &Value, output: &mut Vec<StreamEvent>) {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match self.blocks.remove(&index) {
            Some(AnthropicBlockKind::Tool { id }) => {
                output.push(StreamEvent::ToolUseEnd { id });
            }
            Some(AnthropicBlockKind::Thinking) => output.push(StreamEvent::ReasoningEnd),
            Some(AnthropicBlockKind::Text) | None => {}
        }
    }

    fn message_delta(&mut self, value: &Value, output: &mut Vec<StreamEvent>) {
        let usage = &value["usage"];
        if usage.is_object() {
            output.push(StreamEvent::TokenUsage {
                input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
                output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
                cache_read_input_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64),
                cache_write_input_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64),
                // This surface speaks the Anthropic Messages shape, whose three prompt
                // figures are disjoint — unlike Gemini's native one above.
                accounting: PromptAccounting::CacheBesideInput,
            });
        }
        let reason = value["delta"].get("stop_reason").and_then(Value::as_str);
        if let Some(reason) = reason
            && !self.message_ended
        {
            output.push(StreamEvent::MessageEnd {
                stop_reason: Some(map_anthropic_finish_reason(reason)),
            });
            self.message_ended = true;
        }
    }
}

impl WireDecoder for AnthropicStreamDecoder {
    fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        self.push(chunk)
    }

    fn finish_bytes(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        self.finish()
    }
}

fn map_anthropic_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// GCP credential sources supported by Vertex providers.
///
/// `application_default` checks `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud
/// well-known ADC file, and finally the metadata server. Standard
/// `authorized_user` and `service_account` ADC JSON shapes are implemented.
#[derive(Clone)]
pub struct VertexCredentials {
    inner: Arc<VertexCredentialInner>,
}

struct VertexCredentialInner {
    source: CredentialSource,
    cache: Mutex<Option<CachedToken>>,
}

#[derive(Clone)]
enum CredentialSource {
    AccessToken(SecretString),
    AuthorizedUser(AuthorizedUserCredentials),
    ServiceAccount(ServiceAccountCredentials),
    MetadataServer,
}

#[derive(Clone, Deserialize)]
struct AuthorizedUserCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    #[serde(default = "default_google_token_uri")]
    token_uri: String,
}

#[derive(Clone, Deserialize)]
struct ServiceAccountCredentials {
    private_key_id: Option<String>,
    private_key: String,
    client_email: String,
    #[serde(default = "default_google_token_uri")]
    token_uri: String,
}

struct CachedToken {
    value: SecretString,
    expires_at: SystemTime,
}

impl fmt::Debug for VertexCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VertexCredentials")
            .field("kind", &self.kind())
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl VertexCredentials {
    /// Use an already acquired bearer token.
    #[must_use]
    pub fn access_token(token: impl Into<String>) -> Self {
        Self::new(CredentialSource::AccessToken(SecretString::new(token)))
    }

    /// Parse a standard service-account JSON document.
    pub fn service_account_json(json: &str) -> Result<Self, GoogleProviderError> {
        let credentials: ServiceAccountCredentials = serde_json::from_str(json)
            .map_err(|source| GoogleProviderError::CredentialJson { source })?;
        Ok(Self::new(CredentialSource::ServiceAccount(credentials)))
    }

    /// Resolve Application Default Credentials from environment, well-known file,
    /// or the Compute metadata server.
    pub fn application_default() -> Result<Self, GoogleProviderError> {
        if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
            return Self::from_adc_file(Path::new(&path));
        }
        if let Some(path) = well_known_adc_path()
            && path.is_file()
        {
            return Self::from_adc_file(&path);
        }
        Ok(Self::new(CredentialSource::MetadataServer))
    }

    /// Parse one ADC JSON file.
    pub fn from_adc_file(path: &Path) -> Result<Self, GoogleProviderError> {
        let json =
            fs::read_to_string(path).map_err(|source| GoogleProviderError::CredentialFileIo {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_adc_json(&json)
    }

    /// The selected ADC mechanism, suitable for diagnostics without secrets.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match &self.inner.source {
            CredentialSource::AccessToken(_) => "access_token",
            CredentialSource::AuthorizedUser(_) => "authorized_user",
            CredentialSource::ServiceAccount(_) => "service_account",
            CredentialSource::MetadataServer => "metadata_server",
        }
    }

    fn new(source: CredentialSource) -> Self {
        Self {
            inner: Arc::new(VertexCredentialInner {
                source,
                cache: Mutex::new(None),
            }),
        }
    }

    fn from_adc_json(json: &str) -> Result<Self, GoogleProviderError> {
        let value: Value = serde_json::from_str(json)
            .map_err(|source| GoogleProviderError::CredentialJson { source })?;
        let credential_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(GoogleProviderError::MissingCredentialType)?;
        match credential_type {
            "service_account" => Self::service_account_json(json),
            "authorized_user" => {
                let credentials: AuthorizedUserCredentials = serde_json::from_value(value)
                    .map_err(|source| GoogleProviderError::CredentialJson { source })?;
                Ok(Self::new(CredentialSource::AuthorizedUser(credentials)))
            }
            other => Err(GoogleProviderError::UnsupportedCredentialType {
                credential_type: other.to_owned(),
            }),
        }
    }

    async fn obtain_access_token(&self, client: &Client) -> Result<String, ProviderError> {
        if let CredentialSource::AccessToken(token) = &self.inner.source {
            return Ok(token.expose().to_owned());
        }
        let mut cache = self.inner.cache.lock().await;
        let refresh_before = SystemTime::now() + Duration::from_secs(60);
        if let Some(token) = cache.as_ref()
            && token.expires_at > refresh_before
        {
            return Ok(token.value.expose().to_owned());
        }
        let token = match &self.inner.source {
            CredentialSource::AuthorizedUser(credentials) => {
                refresh_authorized_user(client, credentials).await?
            }
            CredentialSource::ServiceAccount(credentials) => {
                refresh_service_account(client, credentials).await?
            }
            // Deliberately not the shared provider client: see `metadata_client`.
            CredentialSource::MetadataServer => refresh_metadata_token().await?,
            CredentialSource::AccessToken(token) => {
                return Ok(token.expose().to_owned());
            }
        };
        let value = token.value.expose().to_owned();
        *cache = Some(token);
        Ok(value)
    }
}

fn option<'a>(spec: &'a Spec, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| spec.options.get(*name))
}

fn option_u64(spec: &Spec, names: &[&str]) -> Option<u64> {
    option(spec, names).and_then(Value::as_u64)
}

fn option_f64(spec: &Spec, names: &[&str]) -> Option<f64> {
    option(spec, names).and_then(Value::as_f64)
}

fn option_array<'a>(spec: &'a Spec, names: &[&str]) -> Vec<&'a Value> {
    option(spec, names)
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |values| values.iter().collect())
}

fn vertex_parts(spec: &Spec) -> Result<(String, String), Declined> {
    let project = spec
        .project
        .clone()
        .ok_or(Declined::Unavailable(Unavailable::IncompleteConfiguration))?;
    let location = spec
        .region
        .clone()
        .ok_or(Declined::Unavailable(Unavailable::IncompleteConfiguration))?;
    Ok((project, location))
}

fn vertex_credentials(token: Option<String>) -> Result<VertexCredentials, Declined> {
    token.map_or_else(
        || {
            VertexCredentials::application_default()
                .map_err(|error| Declined::Failed(ProviderError::fatal(error)))
        },
        |token| Ok(VertexCredentials::access_token(token)),
    )
}

/// Build the registry factory for Google AI Studio Gemini.
pub fn google_factory<C>(credentials: C) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    move |spec| {
        let key = credentials(&spec.provider)
            .ok_or(Declined::Unavailable(Unavailable::MissingCredential))?;
        let options = GeminiOptions::from_spec(&spec);
        let provider = GoogleGenerativeAi::new(key, options)
            .map_err(|error| Declined::Failed(ProviderError::fatal(error)))?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

/// Build the registry factory for Vertex-hosted Gemini.
pub fn vertex_gemini_factory<C>(
    credentials: C,
) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    move |spec| {
        let (project, location) = vertex_parts(&spec)?;
        let credentials = vertex_credentials(credentials(&spec.provider))?;
        let options = GeminiOptions::from_spec(&spec);
        let provider = VertexGemini::new(project, location, credentials, options)
            .map_err(|error| Declined::Failed(ProviderError::fatal(error)))?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

/// Build the registry factory for Vertex-hosted Anthropic Messages.
pub fn vertex_anthropic_factory<C>(
    credentials: C,
) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    move |spec| {
        let (project, location) = vertex_parts(&spec)?;
        let credentials = vertex_credentials(credentials(&spec.provider))?;
        let options = VertexAnthropicOptions::from_spec(&spec);
        let provider = VertexAnthropic::new(project, location, credentials, options)
            .map_err(|error| Declined::Failed(ProviderError::fatal(error)))?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

fn default_google_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_owned()
}

fn well_known_adc_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from).map(|path| {
            path.join("gcloud")
                .join("application_default_credentials.json")
        })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from).map(|path| {
            path.join(".config")
                .join("gcloud")
                .join("application_default_credentials.json")
        })
    }
}

async fn refresh_authorized_user(
    client: &Client,
    credentials: &AuthorizedUserCredentials,
) -> Result<CachedToken, ProviderError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", &credentials.client_id)
        .append_pair("client_secret", &credentials.client_secret)
        .append_pair("refresh_token", &credentials.refresh_token)
        .finish();
    exchange_oauth_token(client, &credentials.token_uri, body).await
}

async fn refresh_service_account(
    client: &Client,
    credentials: &ServiceAccountCredentials,
) -> Result<CachedToken, ProviderError> {
    let credentials = credentials.clone();
    let assertion =
        tokio::task::spawn_blocking(move || sign_service_account_assertion(&credentials))
            .await
            .map_err(|source| ProviderError::fatal(ServiceAccountSigningError::Join(source)))?
            .map_err(ProviderError::fatal)?;
    let token_uri = credentials_token_uri_from_assertion(&assertion);
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
        .append_pair("assertion", &assertion.value)
        .finish();
    exchange_oauth_token(client, &token_uri, body).await
}

struct SignedAssertion {
    value: String,
    token_uri: String,
}

fn credentials_token_uri_from_assertion(assertion: &SignedAssertion) -> String {
    assertion.token_uri.clone()
}

fn sign_service_account_assertion(
    credentials: &ServiceAccountCredentials,
) -> Result<SignedAssertion, ServiceAccountSigningError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(ServiceAccountSigningError::Clock)?
        .as_secs();
    sign_service_account_assertion_at(credentials, now)
}

fn sign_service_account_assertion_at(
    credentials: &ServiceAccountCredentials,
    now: u64,
) -> Result<SignedAssertion, ServiceAccountSigningError> {
    let header = json!({
        "alg": "RS256",
        "typ": "JWT",
        "kid": credentials.private_key_id,
    });
    let claims = json!({
        "iss": credentials.client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": credentials.token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let header = serde_json::to_vec(&header).map_err(ServiceAccountSigningError::Json)?;
    let claims = serde_json::to_vec(&claims).map_err(ServiceAccountSigningError::Json)?;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let signing_input = format!("{}.{}", encoder.encode(header), encoder.encode(claims));
    let signature = sign_rs256(&credentials.private_key, signing_input.as_bytes())?;
    Ok(SignedAssertion {
        value: format!("{signing_input}.{}", encoder.encode(signature)),
        token_uri: credentials.token_uri.clone(),
    })
}

/// RS256 — RSASSA-PKCS1-v1_5 over SHA-256 — computed entirely in this process.
///
/// The private key travels PEM text → DER → `RsaKeyPair` in memory and is handed
/// straight to aws-lc-rs. It is never written to a file and never handed to a child
/// process, so a crash cannot strand key material on disk and a host without an
/// `openssl` binary — every musl target among them — still signs.
fn sign_rs256(
    private_key_pem: &str,
    message: &[u8],
) -> Result<Vec<u8>, ServiceAccountSigningError> {
    let key = parse_rsa_private_key(private_key_pem)?;
    let mut signature = vec![0_u8; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &aws_lc_rs::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(ServiceAccountSigningError::Sign)?;
    Ok(signature)
}

/// PKCS#8 is what a GCP service-account JSON carries; PKCS#1 is accepted because a
/// key minted by an older tool, or re-exported by one, still arrives that way and the
/// only difference is which DER parser reads it.
fn parse_rsa_private_key(pem: &str) -> Result<RsaKeyPair, ServiceAccountSigningError> {
    const PKCS8: (&str, &str) = ("-----BEGIN PRIVATE KEY-----", "-----END PRIVATE KEY-----");
    const PKCS1: (&str, &str) = (
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----END RSA PRIVATE KEY-----",
    );

    if let Some(der) = pem_der(pem, PKCS8)? {
        RsaKeyPair::from_pkcs8(&der)
    } else if let Some(der) = pem_der(pem, PKCS1)? {
        RsaKeyPair::from_der(&der)
    } else {
        return Err(ServiceAccountSigningError::PrivateKeyNotPem);
    }
    .map_err(ServiceAccountSigningError::PrivateKeyRejected)
}

/// `Ok(None)` means "this armor is absent", which is a different outcome from a
/// present-but-corrupt body and lets the caller try the other encoding.
fn pem_der(
    pem: &str,
    (begin, end): (&str, &str),
) -> Result<Option<Vec<u8>>, ServiceAccountSigningError> {
    let Some(after_begin) = pem.find(begin).map(|at| at + begin.len()) else {
        return Ok(None);
    };
    let body = &pem[after_begin..];
    let Some(before_end) = body.find(end) else {
        return Ok(None);
    };
    let base64_body: String = body[..before_end]
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(base64_body)
        .map(Some)
        .map_err(ServiceAccountSigningError::PrivateKeyBase64)
}

/// No variant carries key material, and none carries a free-form `String`: every
/// failure here is one of a closed set, and a caller logging the chain must not be
/// able to log the key.
#[derive(Debug, thiserror::Error)]
enum ServiceAccountSigningError {
    #[error("system clock is before the Unix epoch")]
    Clock(#[source] std::time::SystemTimeError),
    #[error("failed to encode service-account JWT")]
    Json(#[source] serde_json::Error),
    #[error("service-account private_key is not a PEM-armored RSA private key")]
    PrivateKeyNotPem,
    #[error("service-account private_key PEM body is not valid base64")]
    PrivateKeyBase64(#[source] base64::DecodeError),
    #[error("service-account private_key is not a usable RSA private key")]
    PrivateKeyRejected(#[source] aws_lc_rs::error::KeyRejected),
    #[error("RSA-PKCS1-SHA256 signing of the service-account JWT failed")]
    Sign(#[source] aws_lc_rs::error::Unspecified),
    #[error("service-account signing worker failed")]
    Join(#[source] tokio::task::JoinError),
}

/// How long the link-local metadata server gets to answer.
///
/// `metadata.google.internal` is one hop away on the host's own link, so a wait
/// measured in minutes only ever means the endpoint is absent — the usual case on
/// a developer laptop, where the name resolves to nothing.
const METADATA_TIMEOUT: Duration = Duration::from_secs(5);

/// A client that reaches the instance metadata server without a proxy.
///
/// The process proxy environment must not see this request. `HTTP_PROXY` is
/// commonly set to a corporate or debugging proxy, and routing a
/// `computeMetadata` token request through it hands a freshly minted GCP access
/// token to a third party. It is also plain HTTP, so the token would travel in
/// clear text to that proxy. `DirectPurpose::CloudMetadata` is the declared
/// bypass for exactly this case.
fn metadata_client() -> Result<Client, ProviderError> {
    zuno_network::direct_client_builder(zuno_network::DirectPurpose::CloudMetadata)
        .connect_timeout(METADATA_TIMEOUT)
        .timeout(METADATA_TIMEOUT)
        .build()
        .map_err(ProviderError::transient)
}

async fn refresh_metadata_token() -> Result<CachedToken, ProviderError> {
    let response = metadata_client()?
        .get(METADATA_TOKEN_URL)
        .header("metadata-flavor", "Google")
        .send()
        .await
        .map_err(ProviderError::transient)?;
    decode_token_response(response, VERTEX_PROVIDER_ID).await
}

async fn exchange_oauth_token(
    client: &Client,
    token_uri: &str,
    body: String,
) -> Result<CachedToken, ProviderError> {
    let deadlines = RequestDeadlines::start(HttpTimeouts::native());
    let response = deadlines
        .headers(
            VERTEX_PROVIDER_ID,
            client
                .post(token_uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
                .send(),
        )
        .await?
        .map_err(ProviderError::transient)?;
    decode_token_response(response, VERTEX_PROVIDER_ID).await
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default = "default_token_lifetime")]
    expires_in: u64,
}

const fn default_token_lifetime() -> u64 {
    3_600
}

async fn decode_token_response(
    response: reqwest::Response,
    provider: &str,
) -> Result<CachedToken, ProviderError> {
    if !response.status().is_success() {
        return Err(classify_http_error(response, provider, classify_gemini_http_error).await);
    }
    // A token document is a few hundred bytes. Reading it with a size and time
    // bound keeps a misrouted endpoint that streams forever from consuming the
    // process instead of failing the refresh. Unlike an error body, this one has to
    // be complete: half a token document must not be parsed as a token.
    let body = read_error_body(provider, response).await?;
    if body.truncated() {
        return Err(truncated_body_error(provider, MAX_ERROR_BODY_BYTES));
    }
    let token: OAuthTokenResponse =
        serde_json::from_slice(body.bytes()).map_err(ProviderError::fatal)?;
    Ok(CachedToken {
        value: SecretString::new(token.access_token),
        expires_at: SystemTime::now() + Duration::from_secs(token.expires_in),
    })
}

trait WireDecoder: Send + 'static {
    fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finish_bytes(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}

/// How one surface turns a non-success HTTP response into a typed failure.
///
/// The three providers in this crate do not speak one error format: Gemini
/// returns `error.status`, while Vertex-hosted Anthropic returns the Anthropic
/// Messages `error.type`. Passing the classifier in keeps each surface on the
/// vocabulary it actually emits instead of reading one shape through the other's
/// classifier.
type HttpErrorClassifier = fn(&str, u16, &reqwest::header::HeaderMap, &[u8]) -> ProviderError;

async fn open_json_stream<D>(
    client: Client,
    prepared: PreparedRequest,
    auth_header: Option<(&'static str, String)>,
    decoder: D,
    provider: &'static str,
    model: String,
    classify: HttpErrorClassifier,
) -> Result<ProviderStream<'static>, ProviderError>
where
    D: WireDecoder,
{
    let mut request = client.post(&prepared.url);
    for (name, value) in prepared.headers {
        request = request.header(name, value);
    }
    if let Some((name, value)) = auth_header {
        request = request.header(name, value);
    }
    // A response-header deadline is the difference between a stalled peer and a
    // turn that never ends: a front end that accepts the connection and then
    // loses its backend sends nothing at all, and `send()` alone waits forever.
    let deadlines = RequestDeadlines::start(HttpTimeouts::native());
    let response = deadlines
        .headers(provider, request.json(&prepared.body).send())
        .await?
        .map_err(ProviderError::transient)?;
    if !response.status().is_success() {
        return Err(classify_http_error(response, provider, classify).await);
    }

    let bytes = response.bytes_stream();
    let state = (
        bytes,
        decoder,
        Vec::<Result<StreamEvent, ProviderError>>::new(),
        false,
    );
    let stream = futures::stream::unfold(
        state,
        move |(mut bytes, mut decoder, mut pending, mut finished)| {
            let model = model.clone();
            async move {
                loop {
                    if !pending.is_empty() {
                        let item = pending.remove(0);
                        return Some((item, (bytes, decoder, pending, finished)));
                    }
                    if finished {
                        return None;
                    }
                    // Without a per-chunk idle timeout a half-open connection holds the
                    // turn open forever: TCP has nothing to report and the decoder has
                    // nothing to decode.
                    let chunk = match StreamIdleTimeout::default()
                        .wait(provider, &model, bytes.next())
                        .await
                    {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            pending.push(Err(error));
                            finished = true;
                            continue;
                        }
                    };
                    match chunk {
                        Some(Ok(chunk)) => match decoder.push_bytes(&chunk) {
                            Ok(events) => pending.extend(events.into_iter().map(Ok)),
                            Err(error) => {
                                pending.push(Err(error));
                                finished = true;
                            }
                        },
                        Some(Err(error)) => {
                            pending.push(Err(ProviderError::transient(error)));
                            finished = true;
                        }
                        None => {
                            match decoder.finish_bytes() {
                                Ok(events) => pending.extend(events.into_iter().map(Ok)),
                                Err(error) => pending.push(Err(error)),
                            }
                            finished = true;
                        }
                    }
                }
            }
        },
    );
    Ok(Box::pin(stream))
}

/// Read a non-success response under a size and time bound, then classify it.
///
/// The body read is bounded because an error response is attacker- or
/// accident-shaped: a proxy error page can be arbitrarily long, and
/// `Response::json` would buffer all of it into the provider task.
async fn classify_http_error(
    response: reqwest::Response,
    provider: &str,
    classify: HttpErrorClassifier,
) -> ProviderError {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    match read_error_body(provider, response).await {
        Ok(body) => classify(provider, status, &headers, body.bytes()),
        // The status is the classification signal; a body that could not be read
        // only costs the human-readable detail.
        Err(_) => classify(provider, status, &headers, &[]),
    }
}

/// Classify a non-success Gemini response from its structured `error.status`.
fn classify_gemini_http_error(
    provider: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> ProviderError {
    let body = serde_json::from_slice::<Value>(body).ok();
    let structured_code = body
        .as_ref()
        .and_then(|value| value["error"].get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    if status == 429 {
        return ProviderError::RateLimited {
            retry_after: zuno_llm::http::retry_after(headers),
        };
    }
    if matches!(
        structured_code.as_deref(),
        Some("CONTEXT_LENGTH_EXCEEDED" | "context_length_exceeded")
    ) {
        return ProviderError::ContextLimit {
            limit_tokens: None,
            used_tokens: None,
        };
    }
    let source = HttpStatusError {
        provider: provider.to_owned(),
        status,
        structured_code,
    };
    match status {
        401 | 403 => ProviderError::Auth {
            provider: provider.to_owned(),
            source: Some(Box::new(source)),
        },
        408 | 425 | 500..=599 => ProviderError::Transient {
            status: Some(status),
            source: Some(Box::new(source)),
        },
        _ => ProviderError::Fatal {
            status: Some(status),
            source: Some(Box::new(source)),
        },
    }
}

#[derive(Debug)]
struct HttpStatusError {
    provider: String,
    status: u16,
    structured_code: Option<String>,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` returned HTTP {} (structured code {:?})",
            self.provider, self.status, self.structured_code
        )
    }
}

impl std::error::Error for HttpStatusError {}

fn provider_client() -> Result<Client, GoogleProviderError> {
    zuno_network::client_builder()
        .build()
        .map_err(|source| GoogleProviderError::HttpClient { source })
}

fn one_error(error: ProviderError) -> ProviderStream<'static> {
    Box::pin(futures::stream::once(async move { Err(error) }))
}

fn unsupported_content(
    provider: &'static str,
    role: Role,
    block: &'static str,
) -> GoogleProviderError {
    GoogleProviderError::UnsupportedContent {
        provider,
        role,
        block,
    }
}

fn block_kind(block: &RequestContentBlock) -> &'static str {
    match block {
        RequestContentBlock::Text { .. } => "text",
        RequestContentBlock::ResourceLink { .. } => "resource-link",
        RequestContentBlock::SignedThinking { .. } => "signed-thinking",
        RequestContentBlock::ProviderEncryptedReasoning { .. } => "provider-encrypted-reasoning",
        RequestContentBlock::ToolUse { .. } => "tool-use",
        RequestContentBlock::ToolResult { .. } => "tool-result",
        RequestContentBlock::Image { .. } => "image",
        RequestContentBlock::ImageAttachment { .. } => {
            unreachable!("attachment references must be resolved before provider request shaping")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway 2048-bit RSA key, generated once for this test and committed. It
    /// protects nothing: the assertions it signs are never sent to Google.
    const TEST_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
         MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC/Z94dayqx5JBb\n\
         h2gYJ5LvnbLHWBACFrDInFMLZ9T72SDv7zkbFCoIaec6yn3uBFMA4dsVjlmQOADk\n\
         Nj4JN8enwOdxuUmrUtdBDY5iNpZ9RNoSuGXCseItiKeom2vgOZke89pwX6QS+Alb\n\
         3CTRf+vuhxPgq2lm/jgE0Oe3nhi9gTxI/EsGIgVBX6rE7/V0D1oQmbgpwOKPx50R\n\
         Mjwfa1uMUIP8/k57tu+9yQnG3usGDt6dingZvp76a4UBwypS/ukeVJnj+WZALnbU\n\
         ABBxgahKNJbHU2lYPFHnspLnDIw2yIDFM3LPEQb0HhmYGbNl3BfL6HNHLJBa9aAi\n\
         +5sNewMpAgMBAAECggEABSPk8yVNoDljJxIb2Yo2h/jUNEZJJ8U0Oi74i/Xd4mWS\n\
         XN8vyWphNpihfRKzDxFOqVdnaszH2vemDnrmb5jv47FqhcNUFyXCYhzbFgghQnv2\n\
         30nUccYVLOPenMiPvRXO5uXll975qQjAN5dR5c5pp545Cm+QBRQOrRJvJp84St6B\n\
         slkTMIHMC7e+V2RS/XLWCaGvIEl9DVt3k71iFM22feFeO9QG/RcXLf0WhArgh26t\n\
         oIn7s02UbglD7cdd93SrNuA7XgJOXaqOXvrT4hRF3ixpKbvsMm8/g5uebOzIUeal\n\
         4jUWj1AsV8czvYQTzHUljivUNlkPIx8csbl00nXDgQKBgQDv9t/9Wew82KWoTRSn\n\
         XBxugisHAy2HI800KizfKxmxQT6nqQbI6v94EzH4PvTEx0aAq5ughVdlKOwDV3NG\n\
         yxlPIA8MnrMjGZRS3ENzbJZd45Xf27rWKwvQoXwGSJqalx40bJZIoYBbvT18X7Ge\n\
         99xLaDiY546XykPdXAGDmOw/owKBgQDMMkoLHYVpWjY4HwhXJRCq6BKnjgzDJz+C\n\
         nids7V38Y4bKHhxZL0AnzODi0i9NeRRuWY2kxkDdXXg9Jld9TVTsGIi3atloZCQG\n\
         p0WBirbvSAizswBwgaJCBcHj0APMlC3SCjfRGfezXeznAb3/aAskV9llsXfvz8kP\n\
         dKCz+f/uwwKBgQCVW0ypLUobySC6s1dSn8NWiRBs6e5xebgkasfJE9OG/zwXMN53\n\
         OcVOoGvuvois3fek6KsR60ytOx5DKjAm9QzIsgSL709CXo5yUIRvGDwzLg8/6UzO\n\
         NrbA4XIHmzMXW03ChX+4r0TsVMorWoh8kHt+N91aVm3rTkqVQcnzdcA+DwKBgGOl\n\
         zvhpqadl/LuaeUl9rwqYQjI+YgACcT3ezEKd+5WlRCvyUcc8BcTmeIB4LdlS0yOe\n\
         1D6q+RCOApVk1qExUdX9iwpnPD1zURlmG8dB2FAhCQ4Ytogw2uv5P0tbQd9eGJY9\n\
         okuKrpR7q5Z4BS5UqctMi6zS1ELVVbsTITFzOPBdAoGBAKBy7BbmMOqGfPpDDzmu\n\
         HxdSaLdyjykYVT5Xshpcihv5cAV8p+x1BpHZvmRcn3EA2Z0EFL8aJm5vuNKLnd/H\n\
         mNsbEgXy1RjfCNrz2DAEyycfuEzHn9vE2tN6nDAO4FLlX/sPfobSqifScg9Q3U+u\n\
         3AdfAVm537FuaitHjB/ho5UO\n\
         -----END PRIVATE KEY-----\n";

    /// The same key re-armored as PKCS#1 by `openssl rsa -traditional`, so the two DER
    /// parsers can be proved to agree on one key rather than on two look-alikes.
    const TEST_PKCS1_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
         MIIEpAIBAAKCAQEAv2feHWsqseSQW4doGCeS752yx1gQAhawyJxTC2fU+9kg7+85\n\
         GxQqCGnnOsp97gRTAOHbFY5ZkDgA5DY+CTfHp8DncblJq1LXQQ2OYjaWfUTaErhl\n\
         wrHiLYinqJtr4DmZHvPacF+kEvgJW9wk0X/r7ocT4KtpZv44BNDnt54YvYE8SPxL\n\
         BiIFQV+qxO/1dA9aEJm4KcDij8edETI8H2tbjFCD/P5Oe7bvvckJxt7rBg7enYp4\n\
         Gb6e+muFAcMqUv7pHlSZ4/lmQC521AAQcYGoSjSWx1NpWDxR57KS5wyMNsiAxTNy\n\
         zxEG9B4ZmBmzZdwXy+hzRyyQWvWgIvubDXsDKQIDAQABAoIBAAUj5PMlTaA5YycS\n\
         G9mKNof41DRGSSfFNDou+Iv13eJlklzfL8lqYTaYoX0Ssw8RTqlXZ2rMx9r3pg56\n\
         5m+Y7+OxaoXDVBclwmIc2xYIIUJ79t9J1HHGFSzj3pzIj70Vzubl5Zfe+akIwDeX\n\
         UeXOaaeeOQpvkAUUDq0SbyafOEregbJZEzCBzAu3vldkUv1y1gmhryBJfQ1bd5O9\n\
         YhTNtn3hXjvUBv0XFy39FoQK4IduraCJ+7NNlG4JQ+3HXfd0qzbgO14CTl2qjl76\n\
         0+IURd4saSm77DJvP4ObnmzsyFHmpeI1Fo9QLFfHM72EE8x1JY4r1DZZDyMfHLG5\n\
         dNJ1w4ECgYEA7/bf/VnsPNilqE0Up1wcboIrBwMthyPNNCos3ysZsUE+p6kGyOr/\n\
         eBMx+D70xMdGgKuboIVXZSjsA1dzRssZTyAPDJ6zIxmUUtxDc2yWXeOV39u61isL\n\
         0KF8BkiampceNGyWSKGAW709fF+xnvfcS2g4mOeOl8pD3VwBg5jsP6MCgYEAzDJK\n\
         Cx2FaVo2OB8IVyUQqugSp44Mwyc/gp4nbO1d/GOGyh4cWS9AJ8zg4tIvTXkUblmN\n\
         pMZA3V14PSZXfU1U7BiIt2rZaGQkBqdFgYq270gIs7MAcIGiQgXB49ADzJQt0go3\n\
         0Rn3s13s5wG9/2gLJFfZZbF378/JD3Sgs/n/7sMCgYEAlVtMqS1KG8kgurNXUp/D\n\
         VokQbOnucXm4JGrHyRPThv88FzDedznFTqBr7r6IrN33pOirEetMrTseQyowJvUM\n\
         yLIEi+9PQl6OclCEbxg8My4PP+lMzja2wOFyB5szF1tNwoV/uK9E7FTKK1qIfJB7\n\
         fjfdWlZt605KlUHJ83XAPg8CgYBjpc74aamnZfy7mnlJfa8KmEIyPmIAAnE93sxC\n\
         nfuVpUQr8lHHPAXE5niAeC3ZUtMjntQ+qvkQjgKVZNahMVHV/YsKZzw9c1EZZhvH\n\
         QdhQIQkOGLaIMNrr+T9LW0HfXhiWPaJLiq6Ue6uWeAUuVKnLTIus0tRC1VW7EyEx\n\
         czjwXQKBgQCgcuwW5jDqhnz6Qw85rh8XUmi3co8pGFU+V7IaXIob+XAFfKfsdQaR\n\
         2b5kXJ9xANmdBBS/GiZub7jSi53fx5jbGxIF8tUY3wja89gwBMsnH7hMx5/bxNrT\n\
         epwwDuBS5V/7D36G0qon0nIPUN1PrtwHXwFZud+xbmorR4wf4aOVDg==\n\
         -----END RSA PRIVATE KEY-----\n";

    /// The exact bytes a real JWT signing input has: the URL-safe unpadded base64 of a
    /// header and a claim set, joined by a dot.
    const KNOWN_ANSWER_MESSAGE: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InRlc3Qta2V5LWlkIn0.\
         eyJpc3MiOiJrbm93bi1hbnN3ZXJAdGVzdC5pYW0uZ3NlcnZpY2VhY2NvdW50LmNvbSJ9";

    /// Produced once by `openssl dgst -sha256 -sign key.pem` (OpenSSL 3.0.13) over
    /// [`KNOWN_ANSWER_MESSAGE`] with [`TEST_PKCS8_PEM`] — an implementation sharing no
    /// code with the one under test. RSASSA-PKCS1-v1_5 is deterministic, so every
    /// conforming signer reproduces these exact bytes; a regression in the digest, the
    /// PKCS#1 padding, or the DER parse fails here instead of surfacing later as a
    /// token Google refuses without saying why.
    const KNOWN_ANSWER_SIGNATURE_BASE64: &str = "PrmUy1ghJQIgihroPdxNy2GkSFctkbrm0MvfNhHVG6e9QhcG+JfCeEL+qeuquj38L5fea2a0S9tI\
         eV8+QG8cA4NQ/syFiPAQklvfcGTQd6sW4PFTT4pyGzJvgy/4Ltwx0bkEkbiqe2heALQcsESxmhMV\
         mvOrGSKdMu/sKNszP2BTZsPaMcglm9XqnrbCW2K7PkA8pyH3NMFIw1uFHCSzkOdWE1P6EFtMRuKT\
         b7CzXDZjT3GKMHgpcSPK9k1lQkgJNDilYib2R3vr+jnwDpF52yqdhOSd3lM1ALOOAaDHdNRpEIf8\
         NSp+5cyd6rcoQJUwQJoUpeG/M+R0zxkFFd8iww==";

    /// A slice of the key's own base64 body, long enough that nothing else on the
    /// machine contains it. It is what the disk scan looks for.
    const KEY_MATERIAL_MARKER: &str =
        "h2gYJ5LvnbLHWBACFrDInFMLZ9T72SDv7zkbFCoIaec6yn3uBFMA4dsVjlmQOADk";

    /// Everything in this library that could put bytes on disk or hand them to another
    /// program. Reading the ADC credentials file is legitimate and stays permitted;
    /// writing anything, or spawning anything, is not — that is precisely the shape the
    /// OpenSSL subprocess had.
    const FILESYSTEM_AND_SUBPROCESS_TOKENS: &[&str] = &[
        "Command::new",
        "std::process",
        "NamedTempFile",
        "tempfile::",
        "fs::write",
        "File::create",
        "OpenOptions",
    ];

    fn test_credentials(private_key: &str) -> ServiceAccountCredentials {
        ServiceAccountCredentials {
            private_key_id: Some("test-key-id".to_owned()),
            private_key: private_key.to_owned(),
            client_email: "known-answer@test.iam.gserviceaccount.com".to_owned(),
            token_uri: "https://oauth2.googleapis.com/token".to_owned(),
        }
    }

    /// The half of this file that ships. The test module below quotes the banned tokens
    /// in order to name them, so scanning the whole file would accuse the guard itself.
    fn library_source() -> &'static str {
        const MARKER: &str = "\n#[cfg(test)]\nmod tests {";
        let source = include_str!("lib.rs");
        let at = source
            .find(MARKER)
            .expect("the test module marker must exist, or the scan covers nothing");
        &source[..at]
    }

    /// Drops trailing line comments so that prose *about* the ban does not read as a
    /// violation of it.
    fn strip_line_comment(line: &str) -> &str {
        match line.find("//") {
            Some(at) if !line[..at].contains('"') => &line[..at],
            _ => line,
        }
    }

    /// Top-level entries only, and only small ones: a `NamedTempFile` lands exactly
    /// there, so this covers the regression without walking the whole filesystem.
    fn temp_dir_files_leaking_key_material() -> Vec<PathBuf> {
        let mut leaks = Vec::new();
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return leaks;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > 64 * 1024 {
                continue;
            }
            if let Ok(contents) = fs::read_to_string(entry.path())
                && contents.contains(KEY_MATERIAL_MARKER)
            {
                leaks.push(entry.path());
            }
        }
        leaks
    }

    #[test]
    fn rs256_signing_reproduces_the_openssl_known_answer() {
        let signature = sign_rs256(TEST_PKCS8_PEM, KNOWN_ANSWER_MESSAGE.as_bytes())
            .expect("the committed PKCS#8 test key must sign");

        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(&signature),
            KNOWN_ANSWER_SIGNATURE_BASE64,
            "in-process RS256 diverged from the OpenSSL-produced reference signature"
        );
        assert_eq!(
            signature.len(),
            256,
            "a 2048-bit modulus must yield a 256-byte signature"
        );
    }

    #[test]
    fn pkcs8_and_pkcs1_armor_of_one_key_sign_identically() {
        let from_pkcs8 = sign_rs256(TEST_PKCS8_PEM, KNOWN_ANSWER_MESSAGE.as_bytes())
            .expect("PKCS#8 armor must parse");
        let from_pkcs1 = sign_rs256(TEST_PKCS1_PEM, KNOWN_ANSWER_MESSAGE.as_bytes())
            .expect("PKCS#1 armor must parse");

        assert_eq!(
            from_pkcs8, from_pkcs1,
            "both armors carry the same key, so both must produce the same signature"
        );
    }

    #[test]
    fn signing_leaves_no_key_material_anywhere_in_the_temp_directory() {
        assert!(
            temp_dir_files_leaking_key_material().is_empty(),
            "the temp directory already held this key before signing; the test cannot \
             attribute a leak"
        );

        let assertion = sign_service_account_assertion_at(&test_credentials(TEST_PKCS8_PEM), 1_000)
            .expect("signing must succeed");
        assert!(!assertion.value.is_empty());

        let leaks = temp_dir_files_leaking_key_material();
        assert!(
            leaks.is_empty(),
            "signing wrote the private key to disk: {leaks:?}"
        );
    }

    #[test]
    fn the_signing_path_cannot_reach_the_filesystem_or_a_subprocess() {
        let source = library_source();
        let mut violations = Vec::new();

        for (index, raw_line) in source.lines().enumerate() {
            let code = strip_line_comment(raw_line);
            for token in FILESYSTEM_AND_SUBPROCESS_TOKENS {
                if code.contains(token) {
                    violations.push(format!(
                        "  line {}: {:?} in {}",
                        index + 1,
                        token,
                        code.trim()
                    ));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "the shipped half of zuno-provider-google can write to disk or spawn a process, \
             which is how the service-account private key used to leak:\n{}",
            violations.join("\n")
        );
    }

    /// A scanner that cannot fail is not a guard.
    #[test]
    fn the_source_scanner_detects_the_violation_it_replaced() {
        for case in [
            "let mut child = Command::new(signing_binary).args([\"dgst\"]).spawn()?;",
            "let key_file = NamedTempFile::new()?;",
            "use tempfile::NamedTempFile;",
            "fs::write(path, credentials.private_key.as_bytes())?;",
            "let file = File::create(path)?;",
            "use std::process::Stdio;",
        ] {
            let code = strip_line_comment(case);
            assert!(
                FILESYSTEM_AND_SUBPROCESS_TOKENS
                    .iter()
                    .any(|token| code.contains(token)),
                "the scanner missed a violation in {case:?}"
            );
        }
    }

    #[test]
    fn the_source_scanner_permits_reading_the_credentials_file() {
        for case in [
            "fs::read_to_string(path).map_err(|source| GoogleProviderError::CredentialFileIo {",
            "// the OpenSSL subprocess this replaced used Command::new and NamedTempFile",
        ] {
            let code = strip_line_comment(case);
            assert!(
                !FILESYSTEM_AND_SUBPROCESS_TOKENS
                    .iter()
                    .any(|token| code.contains(token)),
                "the scanner falsely accused {case:?}"
            );
        }
    }

    #[test]
    fn the_assertion_is_a_three_part_jwt_carrying_the_declared_claims() {
        let assertion = sign_service_account_assertion_at(&test_credentials(TEST_PKCS8_PEM), 1_700)
            .expect("signing must succeed");

        let segments: Vec<&str> = assertion.value.split('.').collect();
        assert_eq!(segments.len(), 3, "a JWS compact serialization has 3 parts");

        let decoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: Value = serde_json::from_slice(
            &decoder
                .decode(segments[0])
                .expect("the header segment must be URL-safe base64"),
        )
        .expect("the header must be JSON");
        let claims: Value = serde_json::from_slice(
            &decoder
                .decode(segments[1])
                .expect("the claims segment must be URL-safe base64"),
        )
        .expect("the claims must be JSON");

        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "test-key-id");
        assert_eq!(claims["iss"], "known-answer@test.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["iat"], 1_700);
        assert_eq!(claims["exp"], 1_700 + 3_600);
        assert_eq!(assertion.token_uri, "https://oauth2.googleapis.com/token");

        let signature = decoder
            .decode(segments[2])
            .expect("the signature segment must be URL-safe base64");
        assert_eq!(
            signature,
            sign_rs256(TEST_PKCS8_PEM, segments[0..2].join(".").as_bytes())
                .expect("re-signing the same input must succeed"),
            "the third segment must be RS256 over the first two"
        );
    }

    #[test]
    fn a_non_pem_private_key_is_rejected_without_echoing_the_input() {
        let secret = "not-a-pem-but-still-secret";
        let error = sign_rs256(secret, b"payload").expect_err("a non-PEM key must be rejected");

        assert!(matches!(
            error,
            ServiceAccountSigningError::PrivateKeyNotPem
        ));
        let rendered = format!("{error} / {error:?}");
        assert!(
            !rendered.contains(secret),
            "the error rendered the key material it rejected: {rendered}"
        );
    }

    #[test]
    fn a_corrupt_pem_body_is_reported_as_base64_rather_than_as_a_missing_key() {
        let pem = "-----BEGIN PRIVATE KEY-----\n!!!not base64!!!\n-----END PRIVATE KEY-----\n";
        let error = sign_rs256(pem, b"payload").expect_err("a corrupt body must be rejected");

        assert!(matches!(
            error,
            ServiceAccountSigningError::PrivateKeyBase64(_)
        ));
    }

    #[test]
    fn a_well_formed_pem_that_is_not_an_rsa_key_is_rejected_by_the_parser() {
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(b"valid base64, invalid DER")
        );
        let error = sign_rs256(&pem, b"payload").expect_err("a non-key must be rejected");

        assert!(matches!(
            error,
            ServiceAccountSigningError::PrivateKeyRejected(_)
        ));
    }

    /// The Gemini body a provider configured from an option bag sends.
    ///
    /// Through `GeminiOptions::from_spec` — what `google_factory` calls — rather than
    /// by constructing `GeminiGenerationConfig`, because a test that filled the struct
    /// directly would keep passing while `from_spec` ignored the key the composition
    /// root writes. That is exactly how an accepted-and-ignored option survives.
    fn sse_frame(value: serde_json::Value) -> Vec<u8> {
        format!("data: {value}\n\n").into_bytes()
    }

    #[test]
    fn a_gemini_stream_cut_off_before_a_finish_reason_is_a_retryable_stream_failure() {
        let mut decoder = GeminiStreamDecoder::new(GOOGLE_PROVIDER_ID, "gemini-3-pro");
        let events = decoder
            .push(&sse_frame(
                json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
            ))
            .expect("the frame decodes");
        assert_eq!(events, vec![StreamEvent::TextDelta("partial".to_owned())]);

        let error = decoder
            .finish()
            .expect_err("a truncated Gemini stream must not be committed as a complete turn");
        let ProviderError::Stream {
            code: zuno_error::ProviderStreamFailure::UpstreamStreamIncomplete,
            ..
        } = &error
        else {
            panic!("expected a typed incomplete-stream failure, got {error:?}");
        };
        assert!(error.is_retryable(), "{error:?}");
        assert!(error.permits_partial_output_retry(), "{error:?}");
    }

    #[test]
    fn a_gemini_stream_that_reported_a_finish_reason_completes() {
        let mut decoder = GeminiStreamDecoder::new(GOOGLE_PROVIDER_ID, "gemini-3-pro");
        let mut events = decoder
            .push(&sse_frame(json!({
                "candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]
            })))
            .expect("the frame decodes");
        events.extend(decoder.finish().expect("a terminated stream finishes"));
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("done".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ]
        );
    }

    #[test]
    fn a_vertex_anthropic_stream_cut_off_before_a_terminator_is_a_retryable_stream_failure() {
        let mut decoder =
            AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, "claude-opus-4-5");
        let events = decoder
            .push(&sse_frame(json!({
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"text_delta","text":"partial"}
            })))
            .expect("the frame decodes");
        assert_eq!(events, vec![StreamEvent::TextDelta("partial".to_owned())]);

        let error = decoder
            .finish()
            .expect_err("a truncated Messages stream must not be committed as a complete turn");
        assert!(
            matches!(
                error,
                ProviderError::Stream {
                    code: zuno_error::ProviderStreamFailure::UpstreamStreamIncomplete,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn message_stop_alone_terminates_a_vertex_anthropic_stream() {
        let mut decoder =
            AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, "claude-opus-4-5");
        let mut events = decoder
            .push(&sse_frame(json!({"type":"message_stop"})))
            .expect("the frame decodes");
        events.extend(decoder.finish().expect("a terminated stream finishes"));
        assert_eq!(
            events,
            vec![StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Unknown),
            }]
        );
    }

    #[test]
    fn a_vertex_anthropic_context_overflow_asks_for_compaction_rather_than_failing() {
        // The deciding case for finding F4. The four-arm classifier this replaced
        // matched only `rate_limit_error`, `overloaded_error`, `authentication_error`
        // and `permission_error`, so `prompt_too_long` fell into its `_` arm as
        // `Fatal` — `Recovery::Fail`, with no compaction and no retry.
        let mut decoder =
            AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, "claude-opus-4-5");
        let error = decoder
            .push(&sse_frame(json!({
                "type":"error",
                "error":{"type":"prompt_too_long","message":"prompt is too long"}
            })))
            .expect_err("an error event terminates the stream");
        assert!(
            matches!(error, ProviderError::ContextLimit { .. }),
            "{error:?}"
        );
        assert!(matches!(error.recovery(), zuno_error::Recovery::Compact));
    }

    #[test]
    fn a_vertex_anthropic_http_error_is_read_with_the_messages_vocabulary() {
        let error = map_messages_http_error(
            VERTEX_ANTHROPIC_PROVIDER_ID,
            400,
            &reqwest::header::HeaderMap::new(),
            br#"{"type":"error","error":{"type":"prompt_too_long","message":"too long"}}"#,
        );
        assert!(
            matches!(error, ProviderError::ContextLimit { .. }),
            "the Gemini classifier returned Fatal for exactly this body: {error:?}"
        );

        let gemini = classify_gemini_http_error(
            VERTEX_PROVIDER_ID,
            400,
            &reqwest::header::HeaderMap::new(),
            br#"{"error":{"status":"INVALID_ARGUMENT","message":"bad request"}}"#,
        );
        assert!(matches!(gemini, ProviderError::Fatal { .. }), "{gemini:?}");
    }

    #[test]
    fn a_gemini_throttle_keeps_the_peers_retry_after_in_both_forms() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Sun, 06 Nov 2044 08:49:37 GMT"),
        );
        let error = classify_gemini_http_error(GOOGLE_PROVIDER_ID, 429, &headers, b"{}");
        let ProviderError::RateLimited {
            retry_after: Some(delay),
        } = error
        else {
            panic!("expected a rate limit carrying the peer's deadline, got {error:?}");
        };
        assert!(
            !delay.is_zero(),
            "the HTTP-date form was previously dropped because only `parse::<u64>` was tried"
        );
    }

    fn gemini_body_from_options(options: serde_json::Value) -> Value {
        let mut spec = Spec::new("google");
        for (name, value) in options.as_object().expect("options are an object") {
            spec = spec.with_option(name.clone(), value.clone());
        }
        let provider = GoogleGenerativeAi::new("test-api-key", GeminiOptions::from_spec(&spec))
            .expect("provider configuration");
        let request = CompletionRequest::new(
            "gemini-2.5-flash",
            vec![Message::new(Role::User, "Say hello.")],
        );
        provider.prepare(&request).expect("Gemini request").body
    }

    #[test]
    fn the_cross_provider_output_cap_reaches_gemini_generation_config() {
        assert_eq!(
            gemini_body_from_options(json!({"maxTokens": 16_384}))["generationConfig"]["maxOutputTokens"],
            json!(16_384),
            "`gemini.ts:307` builds `maxOutputTokens` from `generation.maxTokens`, so a \
             provider reading only Gemini's own spelling drops the cap the composition \
             root writes for every family"
        );
    }

    #[test]
    fn geminis_own_spelling_still_outranks_the_cross_provider_one() {
        assert_eq!(
            gemini_body_from_options(json!({"maxOutputTokens": 2_048, "maxTokens": 16_384}))["generationConfig"]
                ["maxOutputTokens"],
            json!(2_048),
            "a config written against Gemini's own field name must keep winning, or \
             adding the alias would silently change an existing deployment"
        );
    }

    #[test]
    fn gemini_sampling_controls_reach_generation_config() {
        let config = gemini_body_from_options(json!({"temperature": 0.3, "topP": 0.9}));
        assert_eq!(config["generationConfig"]["temperature"], json!(0.3));
        assert_eq!(config["generationConfig"]["topP"], json!(0.9));
    }

    /// The Vertex-Anthropic body a provider configured from an option bag sends.
    fn vertex_anthropic_body_from_options(options: serde_json::Value) -> Value {
        let mut spec = Spec::new("google-vertex-anthropic");
        for (name, value) in options.as_object().expect("options are an object") {
            spec = spec.with_option(name.clone(), value.clone());
        }
        let provider = VertexAnthropic::new(
            "project-a",
            "us",
            VertexCredentials::access_token("test-token"),
            VertexAnthropicOptions::from_spec(&spec),
        )
        .expect("Vertex Anthropic");
        let request = CompletionRequest::new(
            "claude-model-under-test",
            vec![Message::new(Role::User, "Say hello.")],
        );
        provider.prepare(&request).expect("Anthropic request").body
    }

    #[test]
    fn vertex_anthropic_carries_every_configured_generation_control() {
        let body = vertex_anthropic_body_from_options(json!({
            "maxTokens": 8_192,
            "temperature": 0.3,
            "topP": 0.9,
            "toolChoice": {"type": "any"}
        }));

        assert_eq!(body["max_tokens"], json!(8_192));
        assert_eq!(body["temperature"], json!(0.3));
        assert_eq!(
            body["top_p"],
            json!(0.9),
            "this path had no `top_p` field at all, so a configured cutoff was accepted \
             and dropped"
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "any"}),
            "the field existed and only the `with_*` builders could set it, so a \
             configured `toolChoice` never reached the wire"
        );
    }
}
