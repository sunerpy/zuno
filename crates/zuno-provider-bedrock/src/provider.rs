use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::{TryStreamExt as _, stream};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::macros::format_description;
use url::Url;
use zuno_error::ProviderError;
use zuno_llm::event::{Message, RequestContentBlock, Role, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec, generation,
};

use crate::credentials::{CredentialChainConfig, CredentialResolver};
use crate::error::{PROVIDER_ID, classify_bedrock_error};
use crate::eventstream::{BedrockDecodeError, BedrockEventDecoder};
use crate::sigv4::{SigV4Signer, encode_path_segment};

/// The cap sent on the Anthropic-native path when nothing is configured.
///
/// The oracle's own fallback for the same required field
/// (`packages/llm/src/protocols/anthropic-messages.ts:510`).
const ANTHROPIC_NATIVE_DEFAULT_MAX_TOKENS: u64 = 4096;

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

#[derive(Debug, Clone)]
pub struct BedrockConfig {
    pub provider_id: String,
    pub region: String,
    pub endpoint: Option<Url>,
    pub operation: BedrockOperation,
    pub credentials: CredentialChainConfig,
    pub generation: BedrockGeneration,
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
        Self {
            provider_id: PROVIDER_ID.to_owned(),
            region: region.into(),
            endpoint: None,
            operation: BedrockOperation::ConverseStream,
            credentials: CredentialChainConfig::default(),
            generation: BedrockGeneration::default(),
        }
    }

    pub fn from_spec(spec: &Spec) -> Result<Self, BedrockBuildError> {
        let region = spec
            .region
            .clone()
            .or_else(|| string_option(spec, "region"))
            .or_else(|| std::env::var("AWS_REGION").ok())
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .unwrap_or_else(|| "us-east-1".to_owned());
        let endpoint = spec
            .base_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(BedrockBuildError::InvalidEndpoint)?;
        let explicit = match (
            string_option(spec, "accessKeyId"),
            string_option(spec, "secretAccessKey"),
        ) {
            (Some(access_key), Some(secret_key)) => {
                let mut credentials = crate::AwsCredentials::new(access_key, secret_key);
                if let Some(token) = string_option(spec, "sessionToken") {
                    credentials = credentials.with_session_token(token);
                }
                Some(credentials)
            }
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
                PROVIDER_ID.to_owned()
            } else {
                spec.provider.clone()
            },
            region,
            endpoint,
            operation,
            credentials: CredentialChainConfig {
                explicit,
                profile: string_option(spec, "profile"),
                ..CredentialChainConfig::default()
            },
            generation: BedrockGeneration::from_spec(spec),
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
    credentials: CredentialResolver,
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
        let client = zuno_network::client_builder()
            .build()
            .map_err(BedrockBuildError::HttpClient)?;
        let credentials =
            CredentialResolver::with_network_client(config.credentials.clone(), client.clone())
                .map_err(BedrockBuildError::HttpClient)?;
        Ok(Self {
            config,
            client,
            credentials,
        })
    }

    pub fn from_spec(spec: &Spec) -> Result<Self, BedrockBuildError> {
        Self::new(BedrockConfig::from_spec(spec)?)
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
        request_body(request, self.config.operation, &self.config.generation)
    }

    async fn open_stream(
        &self,
        mut request: CompletionRequest,
    ) -> Result<ProviderStream<'static>, ProviderError> {
        if request.surface == ApiSurface::Default && self.config.provider_id.ends_with("/mantle") {
            request.surface = mantle_surface(&request.model_id);
        }
        let body = serde_json::to_vec(&self.body_for(&request)?).map_err(ProviderError::fatal)?;
        let url = self.request_url(&request.model_id)?;
        let resolved = self
            .credentials
            .resolve()
            .await
            .map_err(|source| ProviderError::Auth {
                provider: self.config.provider_id.clone(),
                source: Some(Box::new(source)),
            })?;
        let amz_date = OffsetDateTime::now_utc()
            .format(format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .map_err(ProviderError::fatal)?;
        let payload_hash = sha256_hex(&body);
        let mut headers = BTreeMap::from([
            (
                "accept".to_owned(),
                "application/vnd.amazon.eventstream".to_owned(),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-amz-content-sha256".to_owned(), payload_hash),
        ]);
        if self.config.operation == BedrockOperation::InvokeModelWithResponseStream {
            headers.insert(
                "x-amzn-bedrock-accept".to_owned(),
                "application/json".to_owned(),
            );
        }
        headers.extend(request.headers.clone());
        let signing = SigV4Signer::new(&self.config.region, "bedrock")
            .sign(
                "POST",
                &url,
                &headers,
                &body,
                &resolved.credentials,
                &amz_date,
            )
            .map_err(ProviderError::fatal)?;
        let mut builder = self.client.post(url).body(body);
        for (name, value) in signing.headers {
            if name != "host" {
                builder = builder.header(name, value);
            }
        }
        let response = builder.send().await.map_err(ProviderError::transient)?;
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
            let response_body = response.bytes().await.map_err(ProviderError::transient)?;
            return Err(classify_bedrock_error(
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

        let state = ResponseState {
            response,
            decoder: BedrockEventDecoder::new(),
            queued: VecDeque::new(),
            finished: false,
        };
        let output = stream::try_unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.queued.pop_front() {
                    return Ok(Some((event, state)));
                }
                if state.finished {
                    return Ok(None);
                }
                match state
                    .response
                    .chunk()
                    .await
                    .map_err(ProviderError::transient)?
                {
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

    fn request_url(&self, model_id: &str) -> Result<Url, ProviderError> {
        let base = match &self.config.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => Url::parse(&format!(
                "https://bedrock-runtime.{}.amazonaws.com",
                self.config.region
            ))
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
            tool_calls: true,
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
) -> Result<Value, ProviderError> {
    let mut value = match operation {
        BedrockOperation::ConverseStream => converse_body(request, generation)?,
        BedrockOperation::InvokeModelWithResponseStream => native_body(request, generation)?,
    };
    // Bedrock speaks neither OpenAI surface; both operations are Anthropic-shaped
    // bodies posted to a Bedrock Runtime action.
    request.apply_parameters(&mut value, ApiSurface::Messages);
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
    let mut body = Map::from_iter([
        (
            "modelId".to_owned(),
            Value::String(request.model_id.clone()),
        ),
        ("messages".to_owned(), Value::Array(messages)),
    ]);
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    if let Some(config) = generation.inference_config() {
        body.insert("inferenceConfig".to_owned(), config);
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
        })
        .collect()
}

fn openai_messages(messages: &[Message]) -> Result<Vec<Value>, ProviderError> {
    messages.iter().map(openai_message).collect()
}

fn openai_message(message: &Message) -> Result<Value, ProviderError> {
    let mut text = String::new();
    for block in &message.content {
        let Some(value) = block.provider_text() else {
            return Err(ProviderError::fatal(
                RequestShapeError::OpenAiBlockUnsupported,
            ));
        };
        text.push_str(value.as_ref());
    }
    Ok(json!({
        "role": match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        },
        "content": text,
    }))
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

fn openai_responses_input(
    request: &CompletionRequest,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let mut instructions = None;
    let mut input = Vec::new();
    for message in &request.messages {
        let mut value = openai_message(message)?;
        if message.role == Role::System && instructions.is_none() {
            instructions = value
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned);
            continue;
        }
        if message.role == Role::System {
            value["role"] = Value::String("developer".to_owned());
        }
        input.push(value);
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
}

pub fn factory(spec: Spec) -> Result<Arc<dyn Provider>, BedrockBuildError> {
    Ok(Arc::new(BedrockProvider::from_spec(&spec)?))
}

#[cfg(test)]
mod tests {
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
                "modelId": "us.amazon.nova-micro-v1:0",
                "messages": [{"role": "user", "content": [{"text": "Say hello."}]}],
                "system": [{"text": "Reply with one word."}]
            }),
            "a provider configured with no generation controls must still send the \
             pre-`inferenceConfig` shape byte for byte"
        );
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
