//! Amazon Bedrock OpenAI-compatible Responses endpoints.
//!
//! Bedrock exposes two HTTP front doors that speak the OpenAI Responses protocol:
//! Mantle and Bedrock Runtime. They share request and SSE semantics with the native
//! OpenAI adapter, but authenticate with AWS credentials and different SigV4 service
//! names. Converse remains in [`crate::provider`] because its request shape and binary
//! EventStream framing are unrelated.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use futures::{TryStreamExt as _, stream};
use http::Method;
use url::Url;
use zuno_aws_auth::{AwsAccessKeys, AwsAuthConfig, AwsRequestToSign};
use zuno_error::ProviderError;
use zuno_llm::event::StreamEvent;
use zuno_llm::http::{HttpTimeouts, RequestDeadlines, read_error_body};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec,
};
use zuno_llm::sse::StreamIdleTimeout;
use zuno_provider_openai::{OpenAiConfig, OpenAiDecoder, build_request_body};

use crate::aws::{BedrockBearerToken, BedrockRequestAuth, header_map};
use crate::error::classify_bedrock_error_for;

/// Provider identity for the Bedrock Mantle Responses endpoint.
pub const MANTLE_PROVIDER_ID: &str = "amazon-bedrock";
/// Provider identity for the Bedrock Runtime Responses endpoint.
pub const RUNTIME_PROVIDER_ID: &str = "amazon-bedrock-runtime";

const MANTLE_CLIENT_AGENT_HEADER: &str = "x-amzn-mantle-client-agent";
const MANTLE_CLIENT_AGENT_VALUE: &str = "zuno";
const MANTLE_SERVICE: &str = "bedrock-mantle";
const RUNTIME_SERVICE: &str = "bedrock";

/// Regions currently exposed by the Bedrock Mantle front door.
///
/// Keep this list aligned with Amazon's published Mantle region table. Runtime
/// Responses deliberately does not use it: regional availability is service-owned
/// and an AWS response is more authoritative than a compiled allowlist there.
pub const MANTLE_SUPPORTED_REGIONS: [&str; 12] = [
    "us-east-2",
    "us-east-1",
    "us-west-2",
    "ap-southeast-3",
    "ap-south-1",
    "ap-northeast-1",
    "eu-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-south-1",
    "eu-north-1",
    "sa-east-1",
];

/// Which Bedrock OpenAI-compatible front door serves a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedrockResponsesEndpoint {
    /// `bedrock-mantle.{region}.api.aws/openai/v1/responses`.
    Mantle,
    /// `bedrock-runtime.{region}.amazonaws.com/openai/v1/responses`.
    Runtime,
}

impl BedrockResponsesEndpoint {
    const fn provider_id(self) -> &'static str {
        match self {
            Self::Mantle => MANTLE_PROVIDER_ID,
            Self::Runtime => RUNTIME_PROVIDER_ID,
        }
    }

    /// SigV4 service name for this endpoint.
    #[must_use]
    pub const fn signing_service(self) -> &'static str {
        match self {
            Self::Mantle => MANTLE_SERVICE,
            Self::Runtime => RUNTIME_SERVICE,
        }
    }

    fn default_base_url(self, region: &str) -> String {
        match self {
            Self::Mantle => format!("https://bedrock-mantle.{region}.api.aws/openai/v1"),
            Self::Runtime => {
                format!("https://bedrock-runtime.{region}.amazonaws.com/openai/v1")
            }
        }
    }
}

/// Immutable configuration for one Bedrock Responses provider.
#[derive(Clone, Debug)]
pub struct BedrockResponsesConfig {
    provider_id: String,
    region: Option<String>,
    endpoint: BedrockResponsesEndpoint,
    base_url: Option<Url>,
    auth: AwsAuthConfig,
    access_keys: Option<AwsAccessKeys>,
    openai: OpenAiConfig,
    headers: BTreeMap<String, String>,
}

impl BedrockResponsesConfig {
    /// Translate a registry spec into a strict Mantle or Runtime configuration.
    pub fn from_spec(
        spec: &Spec,
        endpoint: BedrockResponsesEndpoint,
    ) -> Result<Self, BedrockResponsesBuildError> {
        let region = spec
            .region
            .clone()
            .or_else(|| string_option(spec, "region"));
        if endpoint == BedrockResponsesEndpoint::Mantle
            && region
                .as_deref()
                .is_some_and(|region| !MANTLE_SUPPORTED_REGIONS.contains(&region))
        {
            return Err(BedrockResponsesBuildError::UnsupportedMantleRegion {
                region: region.expect("the predicate proved a configured region"),
            });
        }

        let base_url = spec
            .base_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(BedrockResponsesBuildError::InvalidEndpoint)?;
        let access_keys = explicit_access_keys(spec)?;
        let provider_id = if spec.provider.is_empty() {
            endpoint.provider_id().to_owned()
        } else {
            spec.provider.clone()
        };
        let mut openai_spec = spec.clone().with_surface(ApiSurface::Responses);
        openai_spec.provider.clone_from(&provider_id);
        openai_spec
            .options
            .entry("store".to_owned())
            .or_insert(serde_json::Value::Bool(false));
        let openai = OpenAiConfig::try_from_spec(openai_spec)
            .map_err(BedrockResponsesBuildError::OpenAiProtocol)?;

        Ok(Self {
            provider_id,
            endpoint,
            base_url,
            auth: AwsAuthConfig {
                profile: string_option(spec, "profile"),
                region: region.clone(),
                service: endpoint.signing_service().to_owned(),
            },
            region,
            access_keys,
            openai,
            headers: spec.headers.clone(),
        })
    }

    /// Provider identity carried into errors and durable events.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Explicit region supplied by provider configuration, if any.
    #[must_use]
    pub fn configured_region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// SigV4 service selected by the endpoint.
    #[must_use]
    pub const fn signing_service(&self) -> &'static str {
        self.endpoint.signing_service()
    }

    /// Fully resolved Responses URL for the SDK-resolved region.
    pub fn request_url_for_region(&self, region: &str) -> Result<Url, ProviderError> {
        let base_url = match &self.base_url {
            Some(base_url) => base_url.clone(),
            None => {
                Url::parse(&self.endpoint.default_base_url(region)).map_err(ProviderError::fatal)?
            }
        };
        responses_url(&base_url).map_err(ProviderError::fatal)
    }
}

/// A native Bedrock Mantle or Runtime Responses provider.
#[derive(Clone)]
pub struct BedrockResponsesProvider {
    config: BedrockResponsesConfig,
    client: reqwest::Client,
    auth: BedrockRequestAuth,
}

impl std::fmt::Debug for BedrockResponsesProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockResponsesProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl BedrockResponsesProvider {
    /// Construct one provider with proxy-aware Bedrock traffic and SDK-owned credentials.
    pub fn new(config: BedrockResponsesConfig) -> Result<Self, BedrockResponsesBuildError> {
        Self::new_with_bearer(config, None)
    }

    /// Construct one provider with an optional Amazon Bedrock API key.
    pub fn new_with_bearer(
        config: BedrockResponsesConfig,
        bearer: Option<BedrockBearerToken>,
    ) -> Result<Self, BedrockResponsesBuildError> {
        let client = zuno_network::client_builder()
            .build()
            .map_err(BedrockResponsesBuildError::HttpClient)?;
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

    /// Build a strict endpoint-specific provider from a registry spec.
    pub fn from_spec(
        spec: &Spec,
        endpoint: BedrockResponsesEndpoint,
    ) -> Result<Self, BedrockResponsesBuildError> {
        Self::new(BedrockResponsesConfig::from_spec(spec, endpoint)?)
    }

    /// Build an endpoint-specific provider with an optional bearer token.
    pub fn from_spec_with_bearer(
        spec: &Spec,
        endpoint: BedrockResponsesEndpoint,
        bearer: Option<BedrockBearerToken>,
    ) -> Result<Self, BedrockResponsesBuildError> {
        Self::new_with_bearer(BedrockResponsesConfig::from_spec(spec, endpoint)?, bearer)
    }

    /// Exact OpenAI Responses request body that will be signed.
    pub fn body_for(
        &self,
        request: &CompletionRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        validate_model(self.config.endpoint, &request.model_id)?;
        let mut request = request.clone();
        request.surface = ApiSurface::Responses;
        build_request_body(&request, &self.config.openai)
    }

    /// Immutable resolved configuration.
    #[must_use]
    pub const fn config(&self) -> &BedrockResponsesConfig {
        &self.config
    }

    async fn open_stream(
        &self,
        mut request: CompletionRequest,
    ) -> Result<ProviderStream<'static>, ProviderError> {
        validate_model(self.config.endpoint, &request.model_id)?;
        request.surface = ApiSurface::Responses;
        let body = serde_json::to_vec(&build_request_body(&request, &self.config.openai)?)
            .map_err(ProviderError::fatal)?;
        let region = self.auth.region().await?;
        if self.config.endpoint == BedrockResponsesEndpoint::Mantle
            && !MANTLE_SUPPORTED_REGIONS.contains(&region)
        {
            return Err(ProviderError::fatal(
                BedrockResponsesBuildError::UnsupportedMantleRegion {
                    region: region.to_owned(),
                },
            ));
        }
        let url = self.config.request_url_for_region(region)?;
        let mut headers = BTreeMap::from([
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ]);
        if !self.auth.uses_bearer() {
            headers.insert("x-amz-content-sha256".to_owned(), sha256_hex(&body));
        }
        if self.config.endpoint == BedrockResponsesEndpoint::Mantle {
            headers.insert(
                MANTLE_CLIENT_AGENT_HEADER.to_owned(),
                MANTLE_CLIENT_AGENT_VALUE.to_owned(),
            );
        }
        headers.extend(self.config.headers.clone());
        headers.extend(request.headers.clone());
        if self.config.endpoint == BedrockResponsesEndpoint::Mantle {
            headers.retain(|name, _| !name.contains('_'));
        }
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

        let deadlines = RequestDeadlines::start(HttpTimeouts::native());
        let provider = self.config.provider_id.clone();
        let response = deadlines
            .headers(&provider, builder.send())
            .await?
            .map_err(ProviderError::transient)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let response_headers = response_headers(&response);
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
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(ProviderError::fatal(
                BedrockResponsesProtocolError::UnexpectedContentType,
            ));
        }

        let model = request.model_id;
        let mut pending = VecDeque::new();
        if let Some(request_id) = aws_request_id(response.headers()) {
            pending.push_back(Ok(StreamEvent::StatusDetail {
                detail: format!("AWS request ID {request_id}"),
            }));
        }
        let state = ResponsesState {
            response,
            decoder: OpenAiDecoder::new(provider.clone(), model.clone(), ApiSurface::Responses),
            pending,
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
}

impl Provider for BedrockResponsesProvider {
    fn id(&self) -> &str {
        self.config.provider_id()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            reasoning: true,
            tool_calls: true,
            prompt_cache: true,
            attachments: true,
            sampling_params: false,
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        Box::pin(stream::once(self.open_stream(request)).try_flatten())
    }
}

struct ResponsesState {
    response: reqwest::Response,
    decoder: OpenAiDecoder,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    provider: String,
    model: String,
    ended: bool,
}

/// Build the provider registered as `amazon-bedrock`.
pub fn mantle_factory(spec: Spec) -> Result<Arc<dyn Provider>, BedrockResponsesBuildError> {
    Ok(Arc::new(BedrockResponsesProvider::from_spec(
        &spec,
        BedrockResponsesEndpoint::Mantle,
    )?))
}

/// Build the Mantle provider with an optional Amazon Bedrock API key.
pub fn mantle_factory_with_bearer(
    spec: Spec,
    bearer: Option<BedrockBearerToken>,
) -> Result<Arc<dyn Provider>, BedrockResponsesBuildError> {
    Ok(Arc::new(BedrockResponsesProvider::from_spec_with_bearer(
        &spec,
        BedrockResponsesEndpoint::Mantle,
        bearer,
    )?))
}

/// Build the provider registered as `amazon-bedrock-runtime`.
pub fn runtime_factory(spec: Spec) -> Result<Arc<dyn Provider>, BedrockResponsesBuildError> {
    Ok(Arc::new(BedrockResponsesProvider::from_spec(
        &spec,
        BedrockResponsesEndpoint::Runtime,
    )?))
}

/// Build the Runtime Responses provider with an optional Amazon Bedrock API key.
pub fn runtime_factory_with_bearer(
    spec: Spec,
    bearer: Option<BedrockBearerToken>,
) -> Result<Arc<dyn Provider>, BedrockResponsesBuildError> {
    Ok(Arc::new(BedrockResponsesProvider::from_spec_with_bearer(
        &spec,
        BedrockResponsesEndpoint::Runtime,
        bearer,
    )?))
}

fn explicit_access_keys(spec: &Spec) -> Result<Option<AwsAccessKeys>, BedrockResponsesBuildError> {
    match (
        string_option(spec, "accessKeyId"),
        string_option(spec, "secretAccessKey"),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(Some(AwsAccessKeys {
            access_key_id,
            secret_access_key,
            session_token: string_option(spec, "sessionToken"),
        })),
        (None, None) => Ok(None),
        _ => Err(BedrockResponsesBuildError::IncompleteExplicitCredentials),
    }
}

fn string_option(spec: &Spec, name: &str) -> Option<String> {
    spec.options
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn responses_url(base_url: &Url) -> Result<Url, url::ParseError> {
    if base_url
        .path()
        .trim_end_matches('/')
        .ends_with("/responses")
    {
        return Ok(base_url.clone());
    }
    Url::parse(&format!(
        "{}/responses",
        base_url.as_str().trim_end_matches('/')
    ))
}

fn validate_model(endpoint: BedrockResponsesEndpoint, model_id: &str) -> Result<(), ProviderError> {
    let valid = match endpoint {
        BedrockResponsesEndpoint::Mantle => model_id.starts_with("openai.gpt-"),
        BedrockResponsesEndpoint::Runtime => {
            model_id.starts_with("openai.gpt-") || model_id.contains(".openai.gpt-")
        }
    };
    if valid {
        Ok(())
    } else {
        Err(ProviderError::fatal(
            BedrockResponsesProtocolError::UnsupportedModel {
                provider: endpoint.provider_id(),
                model: model_id.to_owned(),
            },
        ))
    }
}

fn response_headers(response: &reqwest::Response) -> BTreeMap<String, String> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn aws_request_id(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    ["x-amzn-requestid", "x-amzn-request-id"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
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

/// Construction failures that can be detected before the first request.
#[derive(Debug, thiserror::Error)]
pub enum BedrockResponsesBuildError {
    /// The configured endpoint is not a URL.
    #[error("invalid Bedrock Responses endpoint")]
    InvalidEndpoint(#[source] url::ParseError),
    /// A static access key was only half configured.
    #[error("Bedrock explicit credentials require both accessKeyId and secretAccessKey")]
    IncompleteExplicitCredentials,
    /// Mantle has a fixed published regional footprint.
    #[error("Amazon Bedrock Mantle does not support region `{region}`")]
    UnsupportedMantleRegion { region: String },
    /// The shared OpenAI Responses protocol rejected provider options.
    #[error("invalid Bedrock Responses protocol options")]
    OpenAiProtocol(#[source] ProviderError),
    /// The proxy-aware HTTP client could not be constructed.
    #[error("failed to construct Bedrock Responses HTTP client")]
    HttpClient(#[source] reqwest::Error),
}

#[derive(Debug, thiserror::Error)]
enum BedrockResponsesProtocolError {
    #[error("Bedrock Responses stream did not use text/event-stream")]
    UnexpectedContentType,
    #[error(
        "model `{model}` is not valid for provider `{provider}`; use `amazon-bedrock` for \
         Mantle OpenAI model IDs, `amazon-bedrock-runtime` for Runtime inference-profile IDs, \
         or `amazon-bedrock-converse` for Claude, Nova, and other Converse models"
    )]
    UnsupportedModel {
        provider: &'static str,
        model: String,
    },
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zuno_llm::event::{Message, Role};

    use super::*;

    fn spec(provider: &str, endpoint: Option<String>) -> Spec {
        let mut spec = Spec::new(provider)
            .with_region("us-east-2")
            .with_option("accessKeyId", json!("AKIDEXAMPLE"))
            .with_option(
                "secretAccessKey",
                json!("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            )
            .with_option("maxTokens", json!(512));
        if let Some(endpoint) = endpoint {
            spec = spec.with_base_url(endpoint);
        }
        spec
    }

    #[test]
    fn endpoints_and_signing_services_are_strictly_separated() {
        let mantle = BedrockResponsesConfig::from_spec(
            &spec(MANTLE_PROVIDER_ID, None),
            BedrockResponsesEndpoint::Mantle,
        )
        .expect("Mantle config");
        let runtime = BedrockResponsesConfig::from_spec(
            &spec(RUNTIME_PROVIDER_ID, None),
            BedrockResponsesEndpoint::Runtime,
        )
        .expect("Runtime config");

        assert_eq!(
            mantle
                .request_url_for_region("us-east-2")
                .expect("Mantle URL")
                .as_str(),
            "https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses"
        );
        assert_eq!(mantle.signing_service(), "bedrock-mantle");
        assert_eq!(
            runtime
                .request_url_for_region("us-east-2")
                .expect("Runtime URL")
                .as_str(),
            "https://bedrock-runtime.us-east-2.amazonaws.com/openai/v1/responses"
        );
        assert_eq!(runtime.signing_service(), "bedrock");
    }

    #[test]
    fn mantle_rejects_an_unsupported_region_before_credentials_or_network() {
        let error = BedrockResponsesConfig::from_spec(
            &spec(MANTLE_PROVIDER_ID, None).with_region("us-west-1"),
            BedrockResponsesEndpoint::Mantle,
        )
        .expect_err("the unsupported region must fail locally");
        assert!(
            error.to_string().contains("us-west-1"),
            "the repair must name the configured region: {error}"
        );
    }

    #[test]
    fn responses_body_uses_the_openai_effort_shape_and_locked_tools() {
        let provider = BedrockResponsesProvider::from_spec(
            &spec(MANTLE_PROVIDER_ID, None),
            BedrockResponsesEndpoint::Mantle,
        )
        .expect("provider");
        let mut request = CompletionRequest::new(
            "openai.gpt-5.6-sol",
            vec![Message::new(Role::User, "hello")],
        )
        .with_tools(vec![zuno_llm::registry::ToolSchema {
            name: "read_file".to_owned(),
            description: "Read one file.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }]);
        request
            .parameters
            .insert("reasoningEffort".to_owned(), json!("max"));

        let body = provider.body_for(&request).expect("Responses body");
        assert_eq!(body["reasoning"], json!({"effort": "max"}));
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn mantle_signs_with_its_service_and_decodes_a_real_sse_shape() {
        let server = MockServer::start().await;
        let expected_body = json!({
            "model": "openai.gpt-5.6-sol",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "max_output_tokens": 512,
            "store": false,
            "stream": true
        });
        Mock::given(method("POST"))
            .and(path("/openai/v1/responses"))
            .and(body_json(expected_body))
            .and(header_regex(
                "authorization",
                ".*/us-east-2/bedrock-mantle/aws4_request.*",
            ))
            .and(header_regex(MANTLE_CLIENT_AGENT_HEADER, "^zuno$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\n\
                         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
                        "text/event-stream",
                    )
                    .insert_header("x-amzn-requestid", "req-mantle-123"),
            )
            .mount(&server)
            .await;
        let provider = BedrockResponsesProvider::from_spec(
            &spec(
                MANTLE_PROVIDER_ID,
                Some(format!("{}/openai/v1", server.uri())),
            ),
            BedrockResponsesEndpoint::Mantle,
        )
        .expect("provider");

        let events = provider
            .stream(CompletionRequest::new(
                "openai.gpt-5.6-sol",
                vec![Message::new(Role::User, "hello")],
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("complete stream");

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::StatusDetail { detail } if detail.contains("req-mantle-123")
        )));
        assert!(events.contains(&StreamEvent::TextDelta("OK".to_owned())));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageEnd { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(1),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn mantle_bearer_token_replaces_sigv4_and_decodes_the_same_stream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/v1/responses"))
            .and(header("authorization", "Bearer bedrock-api-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\n\
                     data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
                    "text/event-stream",
                ),
            )
            .mount(&server)
            .await;
        let provider = BedrockResponsesProvider::from_spec_with_bearer(
            &spec(
                MANTLE_PROVIDER_ID,
                Some(format!("{}/openai/v1", server.uri())),
            ),
            BedrockResponsesEndpoint::Mantle,
            Some(BedrockBearerToken::new("bedrock-api-key")),
        )
        .expect("provider");

        let events = provider
            .stream(CompletionRequest::new(
                "openai.gpt-5.6-sol",
                vec![Message::new(Role::User, "hello")],
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("complete stream");

        assert!(events.contains(&StreamEvent::TextDelta("OK".to_owned())));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageEnd { .. }))
        );
    }
}
