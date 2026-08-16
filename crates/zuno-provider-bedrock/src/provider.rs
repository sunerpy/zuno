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
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec,
};

use crate::credentials::{CredentialChainConfig, CredentialResolver};
use crate::error::{PROVIDER_ID, classify_bedrock_error};
use crate::eventstream::{BedrockDecodeError, BedrockEventDecoder};
use crate::sigv4::{SigV4Signer, encode_path_segment};

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
        let client = reqwest::Client::builder()
            .build()
            .map_err(BedrockBuildError::HttpClient)?;
        let credentials =
            CredentialResolver::with_client(config.credentials.clone(), client.clone());
        Ok(Self {
            config,
            client,
            credentials,
        })
    }

    pub fn from_spec(spec: &Spec) -> Result<Self, BedrockBuildError> {
        Self::new(BedrockConfig::from_spec(spec)?)
    }

    async fn open_stream(
        &self,
        mut request: CompletionRequest,
    ) -> Result<ProviderStream<'static>, ProviderError> {
        if request.surface == ApiSurface::Default && self.config.provider_id.ends_with("/mantle") {
            request.surface = mantle_surface(&request.model_id);
        }
        let body = request_body(&request, self.config.operation)?;
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
) -> Result<Vec<u8>, ProviderError> {
    let mut value = match operation {
        BedrockOperation::ConverseStream => converse_body(request)?,
        BedrockOperation::InvokeModelWithResponseStream => native_body(request)?,
    };
    request.apply_parameters(&mut value);
    serde_json::to_vec(&value).map_err(ProviderError::fatal)
}

fn converse_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            for block in &message.content {
                let RequestContentBlock::Text { text } = block else {
                    return Err(ProviderError::fatal(
                        RequestShapeError::UnsupportedSystemBlock,
                    ));
                };
                system.push(json!({"text": text}));
            }
            continue;
        }
        messages.push(json!({
            "role": converse_role(message.role),
            "content": converse_content(&message.content)?,
        }));
    }
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
            RequestContentBlock::Image { media_type, data } => Ok(json!({
                "image": {
                    "format": media_type.rsplit('/').next().unwrap_or(media_type),
                    "source": {"bytes": data}
                }
            })),
        })
        .collect()
}

fn native_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    match request.surface {
        ApiSurface::Messages | ApiSurface::Default => anthropic_native_body(request),
        ApiSurface::Chat => Ok(json!({
            "model": request.model_id,
            "stream": true,
            "messages": openai_messages(&request.messages)?,
        })),
        ApiSurface::Responses => Ok(json!({
            "model": request.model_id,
            "stream": true,
            "input": openai_messages(&request.messages)?,
        })),
    }
}

fn anthropic_native_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            for block in &message.content {
                if let RequestContentBlock::Text { text } = block {
                    system.push(text.clone());
                } else {
                    return Err(ProviderError::fatal(
                        RequestShapeError::UnsupportedSystemBlock,
                    ));
                }
            }
        } else {
            messages.push(json!({
                "role": converse_role(message.role),
                "content": anthropic_content(&message.content)?,
            }));
        }
    }
    let mut body = Map::from_iter([
        (
            "anthropic_version".to_owned(),
            Value::String("bedrock-2023-05-31".to_owned()),
        ),
        ("max_tokens".to_owned(), Value::from(4096)),
        ("messages".to_owned(), Value::Array(messages)),
    ]);
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::String(system.join("\n")));
    }
    Ok(Value::Object(body))
}

fn anthropic_content(blocks: &[RequestContentBlock]) -> Result<Vec<Value>, ProviderError> {
    blocks
        .iter()
        .map(|block| match block {
            RequestContentBlock::Text { text } => Ok(json!({"type": "text", "text": text})),
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
            RequestContentBlock::Image { media_type, data } => Ok(json!({
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
    messages
        .iter()
        .map(|message| {
            let mut text = String::new();
            for block in &message.content {
                match block {
                    RequestContentBlock::Text { text: value } => text.push_str(value),
                    _ => {
                        return Err(ProviderError::fatal(
                            RequestShapeError::OpenAiBlockUnsupported,
                        ));
                    }
                }
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
        })
        .collect()
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
            converse_body(&request).expect("request body"),
            json!({
                "modelId": "us.amazon.nova-micro-v1:0",
                "messages": [{"role": "user", "content": [{"text": "Say hello."}]}],
                "system": [{"text": "Reply with one word."}]
            })
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
