//! Google AI Studio Gemini, Vertex Gemini, and Vertex-hosted Anthropic.
//!
//! The three providers are intentionally separate concrete types. Google AI
//! Studio and Vertex Gemini share Gemini's `contents`/`parts` codec, while Vertex
//! Anthropic uses Anthropic Messages JSON against Vertex's `streamRawPredict`
//! endpoint. None of these paths uses an OpenAI message serializer.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures::{StreamExt as _, TryStreamExt as _};
use oc_error::ProviderError;
use oc_llm::effort::{
    DeclaredVariants, EffortCapabilities, ProviderFamily, ReasoningEffort, resolve_effort,
};
use oc_llm::event::{
    FinishReason, Message, RequestContentBlock, Role, StreamEvent, ThoughtSignature,
};
use oc_llm::registry::{Capabilities, CompletionRequest, Provider, ProviderStream};
use oc_llm::sse::{SseEvent, SseParser};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use url::Url;

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
    /// Canonical reasoning effort to resolve through `oc_llm::effort`.
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

/// Resolve a canonical effort and return only Google API's `thinkingConfig` value.
///
/// This is deliberately a projection of [`oc_llm::effort::resolve_effort`], not
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
        let body = build_gemini_body(&request.messages, &self.options)?;
        Ok(PreparedRequest::new(url, body))
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
                    GeminiStreamDecoder::new(GOOGLE_PROVIDER_ID, model),
                    GOOGLE_PROVIDER_ID,
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
        let body = build_gemini_body(&request.messages, &self.options)?;
        Ok(PreparedRequest::new(url, body))
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
                    GeminiStreamDecoder::new(VERTEX_PROVIDER_ID, model),
                    VERTEX_PROVIDER_ID,
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
    options: &GeminiOptions,
) -> Result<Value, GoogleProviderError> {
    let tool_names = collect_tool_names(messages);
    let mut system = Vec::new();
    let mut contents = Vec::new();

    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                match block {
                    RequestContentBlock::Text { text } => system.push(text.clone()),
                    other => {
                        return Err(unsupported_content(
                            GOOGLE_PROVIDER_ID,
                            message.role,
                            block_kind(other),
                        ));
                    }
                }
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

    let mut body = Map::new();
    body.insert("contents".to_owned(), Value::Array(contents));
    if !system.is_empty() {
        body.insert(
            "systemInstruction".to_owned(),
            json!({"parts": [{"text": system.join("\n")}]}),
        );
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
        (Role::User, RequestContentBlock::Image { media_type, data }) => {
            Ok(json!({"inlineData": {"mimeType": media_type, "data": data}}))
        }
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
        Self {
            provider: provider.into(),
            model: model.into(),
            parser: SseParser::new(),
            next_tool_call_id: 0,
            has_tool_calls: false,
            reasoning_open: false,
            message_ended: false,
        }
    }

    /// Decode one raw network chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.push(chunk);
        self.decode_frames(frames)
    }

    /// Finish UTF-8 and SSE framing and decode any trailing event.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.finish();
        self.decode_frames(frames)
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
                id,
                name: call.name,
            });
            let input = serde_json::to_string(&call.args).map_err(|source| {
                ProviderError::fatal(GeminiPayloadError {
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    source,
                })
            })?;
            output.push(StreamEvent::ToolInputDelta(input));
            if let Some(signature) = part.thought_signature {
                output.push(StreamEvent::ToolUseSignature(ThoughtSignature::new(
                    signature,
                )));
            }
            output.push(StreamEvent::ToolUseEnd);
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
            system: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            effort: None,
            effort_capabilities: EffortCapabilities::default(),
            variants: DeclaredVariants::new(),
        }
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
        let body = build_vertex_anthropic_body(&request.messages, &self.options)?;
        Ok(PreparedRequest::new(url, body))
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
                    AnthropicStreamDecoder::new(VERTEX_ANTHROPIC_PROVIDER_ID, model),
                    VERTEX_ANTHROPIC_PROVIDER_ID,
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
    options: &VertexAnthropicOptions,
) -> Result<Value, GoogleProviderError> {
    let mut system = options.system.clone();
    let mut wire_messages = Vec::new();
    for message in messages {
        if message.role == Role::System {
            for block in &message.content {
                match block {
                    RequestContentBlock::Text { text } => system.push(text.clone()),
                    other => {
                        return Err(unsupported_content(
                            VERTEX_ANTHROPIC_PROVIDER_ID,
                            message.role,
                            block_kind(other),
                        ));
                    }
                }
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
    if let Some(effort) = options.effort {
        resolve_effort(
            ProviderFamily::Anthropic,
            effort,
            options.effort_capabilities,
            &options.variants,
        )
        .apply_to(&mut body);
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
        (Role::User, RequestContentBlock::Image { media_type, data }) => Ok(json!({
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

#[derive(Debug, Clone, Copy)]
enum AnthropicBlockKind {
    Text,
    Tool,
    Thinking,
}

impl AnthropicStreamDecoder {
    /// Construct a decoder with provider/model context.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            parser: SseParser::new(),
            blocks: HashMap::new(),
            message_ended: false,
        }
    }

    /// Decode one raw network chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.push(chunk);
        self.decode_frames(frames)
    }

    /// Finish UTF-8 and SSE framing.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let frames = self.parser.finish();
        self.decode_frames(frames)
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
            Some("error") => return Err(classify_anthropic_stream_error(&self.provider, value)),
            Some(_) | None => {}
        }
        Ok(())
    }

    fn start_block(&mut self, value: &Value, output: &mut Vec<StreamEvent>) {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block = &value["content_block"];
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                self.blocks.insert(index, AnthropicBlockKind::Tool);
                output.push(StreamEvent::ToolUseStart {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
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
                    output.push(StreamEvent::ToolInputDelta(json.to_owned()));
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
            Some(AnthropicBlockKind::Tool) => output.push(StreamEvent::ToolUseEnd),
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

fn classify_anthropic_stream_error(provider: &str, value: &Value) -> ProviderError {
    match value["error"].get("type").and_then(Value::as_str) {
        Some("rate_limit_error") => ProviderError::RateLimited { retry_after: None },
        Some("overloaded_error") => ProviderError::Transient {
            status: Some(529),
            source: None,
        },
        Some("authentication_error" | "permission_error") => ProviderError::Auth {
            provider: provider.to_owned(),
            source: None,
        },
        _ => ProviderError::Fatal {
            status: None,
            source: None,
        },
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
            CredentialSource::MetadataServer => refresh_metadata_token(client).await?,
            CredentialSource::AccessToken(token) => {
                return Ok(token.expose().to_owned());
            }
        };
        let value = token.value.expose().to_owned();
        *cache = Some(token);
        Ok(value)
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

    let mut key_file = NamedTempFile::new().map_err(ServiceAccountSigningError::TempKey)?;
    key_file
        .write_all(credentials.private_key.as_bytes())
        .map_err(ServiceAccountSigningError::TempKey)?;
    key_file
        .flush()
        .map_err(ServiceAccountSigningError::TempKey)?;

    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(key_file.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ServiceAccountSigningError::OpenSslStart)?;
    child
        .stdin
        .take()
        .ok_or(ServiceAccountSigningError::OpenSslStdin)?
        .write_all(signing_input.as_bytes())
        .map_err(ServiceAccountSigningError::OpenSslWrite)?;
    let output = child
        .wait_with_output()
        .map_err(ServiceAccountSigningError::OpenSslWait)?;
    if !output.status.success() {
        return Err(ServiceAccountSigningError::OpenSslStatus {
            status: output.status.code(),
        });
    }
    Ok(SignedAssertion {
        value: format!("{signing_input}.{}", encoder.encode(output.stdout)),
        token_uri: credentials.token_uri.clone(),
    })
}

#[derive(Debug, thiserror::Error)]
enum ServiceAccountSigningError {
    #[error("system clock is before the Unix epoch")]
    Clock(#[source] std::time::SystemTimeError),
    #[error("failed to encode service-account JWT")]
    Json(#[source] serde_json::Error),
    #[error("failed to create a protected temporary key file for service-account signing")]
    TempKey(#[source] std::io::Error),
    #[error("failed to start the pinned workspace's OpenSSL signing fallback")]
    OpenSslStart(#[source] std::io::Error),
    #[error("OpenSSL signing process exposed no stdin")]
    OpenSslStdin,
    #[error("failed to send JWT bytes to OpenSSL")]
    OpenSslWrite(#[source] std::io::Error),
    #[error("failed to wait for OpenSSL signing")]
    OpenSslWait(#[source] std::io::Error),
    #[error("OpenSSL rejected the service-account private key (status={status:?})")]
    OpenSslStatus { status: Option<i32> },
    #[error("service-account signing worker failed")]
    Join(#[source] tokio::task::JoinError),
}

async fn refresh_metadata_token(client: &Client) -> Result<CachedToken, ProviderError> {
    let response = client
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
    let response = client
        .post(token_uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
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
        return Err(classify_http_error(response, provider).await);
    }
    let token: OAuthTokenResponse = response.json().await.map_err(ProviderError::fatal)?;
    Ok(CachedToken {
        value: SecretString::new(token.access_token),
        expires_at: SystemTime::now() + Duration::from_secs(token.expires_in),
    })
}

trait WireDecoder: Send + 'static {
    fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finish_bytes(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}

async fn open_json_stream<D>(
    client: Client,
    prepared: PreparedRequest,
    auth_header: Option<(&'static str, String)>,
    decoder: D,
    provider: &'static str,
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
    let response = request
        .json(&prepared.body)
        .send()
        .await
        .map_err(ProviderError::transient)?;
    if !response.status().is_success() {
        return Err(classify_http_error(response, provider).await);
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
        |(mut bytes, mut decoder, mut pending, mut finished)| async move {
            loop {
                if !pending.is_empty() {
                    let item = pending.remove(0);
                    return Some((item, (bytes, decoder, pending, finished)));
                }
                if finished {
                    return None;
                }
                match bytes.next().await {
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
        },
    );
    Ok(Box::pin(stream))
}

async fn classify_http_error(response: reqwest::Response, provider: &str) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.json::<Value>().await.ok();
    let structured_code = body
        .as_ref()
        .and_then(|value| {
            value["error"]
                .get("status")
                .or_else(|| value["error"].get("type"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned);

    if status == 429 {
        return ProviderError::RateLimited { retry_after };
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
    Client::builder()
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

const fn block_kind(block: &RequestContentBlock) -> &'static str {
    match block {
        RequestContentBlock::Text { .. } => "text",
        RequestContentBlock::SignedThinking { .. } => "signed-thinking",
        RequestContentBlock::ProviderEncryptedReasoning { .. } => "provider-encrypted-reasoning",
        RequestContentBlock::ToolUse { .. } => "tool-use",
        RequestContentBlock::ToolResult { .. } => "tool-result",
        RequestContentBlock::Image { .. } => "image",
    }
}
