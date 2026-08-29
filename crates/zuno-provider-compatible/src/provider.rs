//! The provider: one configurable profile, many vendor identities.
//!
//! # One concrete type for fifteen provider ids
//!
//! Every id in [`crate::family::CLAIMED`] is served by this single type. The
//! differences between them are data — a base URL, a surface rule, a capability
//! set, a header — so there is no per-vendor struct and no per-vendor branch in
//! the request path. `zuno-cli` selects this crate's shared wire-family factory,
//! while [`Spec::provider`] independently tells the instance which identity it
//! took on.
//!
//! # Refusal is a construction-time decision
//!
//! A provider whose wire protocol differs is refused when the factory runs, not
//! when the first response fails to deserialize. See [`crate::family`] for why,
//! and for the message the user gets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Map, Value};
use zuno_error::ProviderError;
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, FactoryOutcome, Provider,
    ProviderStream, Spec, StreamEvent, ToolSchema, Unavailable, generation,
};
use zuno_llm::sse::{SseParser, StreamIdleTimeout};

use crate::family::{self, Profile, UnsupportedProvider};
use crate::quirks::Quirks;
use crate::request::{RequestBody, Sampling};
use crate::stream::SurfaceTranslator;
use crate::surface::endpoint_path;
use crate::transport::{HttpRequest, HttpTimeouts, Transport};

/// The `provider.*.options` key carrying provider-wide capabilities.
pub const CAPABILITIES_OPTION: &str = "capabilities";

/// The `provider.*.options` key carrying per-model capability overrides.
///
/// `Provider::capabilities` is a provider-level question, but
/// [`Capabilities::sampling_params`] is genuinely per model: one deployment serves
/// both a reasoning model that rejects `temperature` and a chat model that wants
/// it. This map is how the catalog narrows the provider default for one model.
pub const MODEL_CAPABILITIES_OPTION: &str = "modelCapabilities";

/// The `provider.*.options` key carrying body keys applied to every request.
///
/// Reasoning-effort resolution lands here: `zuno-llm`'s
/// [`EffortResolution::apply_to`](zuno_llm::effort::EffortResolution::apply_to)
/// writes a `Map`, and this is that map. Keys in
/// [`PROTECTED_KEYS`](crate::request::PROTECTED_KEYS) are ignored.
pub const EXTRA_BODY_OPTION: &str = "extraBody";

const REQUEST_TIMEOUT_OPTION: &str = "timeout";
const HEADER_TIMEOUT_OPTION: &str = "headerTimeout";
const CHUNK_TIMEOUT_OPTION: &str = "chunkTimeout";
const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(330);
const ZUNO_SESSION_METADATA_KEY: &str = "zuno_session_id";

#[derive(Debug, thiserror::Error)]
enum RequestShapeError {
    #[error(
        "OpenAI-compatible request parameter `metadata.zuno_session_id` is reserved for Zuno's typed durable session affinity"
    )]
    ReservedSessionMetadata,
    #[error(
        "OpenAI-compatible request parameter `metadata` must be an object when Zuno session affinity is attached"
    )]
    MetadataMustBeObject,
}

#[derive(Debug, thiserror::Error)]
#[error("provider `{provider}` option `{option}` must be {expected}")]
struct InvalidTimeoutOption {
    provider: String,
    option: &'static str,
    expected: &'static str,
}

/// An OpenAI-compatible provider.
#[derive(Debug)]
pub struct CompatibleProvider {
    spec: Spec,
    profile: Profile,
    base_url: String,
    capabilities: Capabilities,
    extra_body: Map<String, Value>,
    max_tokens: Option<u64>,
    sampling: Sampling,
    tool_choice: Option<Value>,
    credential: Option<String>,
    transport: Arc<dyn Transport>,
    idle: StreamIdleTimeout,
    timeouts: HttpTimeouts,
}

impl CompatibleProvider {
    /// Construct a provider for `spec`, or explain the refusal.
    ///
    /// # Errors
    ///
    /// - [`Declined::Failed`] wrapping [`UnsupportedProvider`] when the id belongs
    ///   to another family, or is unknown and undeclared. The message names the
    ///   crate that carries it.
    /// - [`Declined::Unavailable`] with
    ///   [`Unavailable::IncompleteConfiguration`] when no base URL is configured,
    ///   because an OpenAI-compatible endpoint is defined by its URL and there is
    ///   no correct default to guess.
    ///
    /// A missing credential is **not** refused: a local endpoint legitimately has
    /// none, and a vendor that requires one answers `401`, which is already
    /// [`ProviderError::Auth`] and already asks for re-authentication.
    pub fn new(
        spec: Spec,
        transport: Arc<dyn Transport>,
        credential: Option<String>,
    ) -> Result<Self, Declined> {
        let profile = family::resolve(&spec).map_err(unsupported)?;
        let base_url = spec
            .base_url
            .clone()
            .ok_or(Declined::Unavailable(Unavailable::IncompleteConfiguration))?;
        let capabilities = read_capabilities(spec.options.get(CAPABILITIES_OPTION))
            .unwrap_or_else(compatible_default_capabilities);
        let extra_body = spec
            .options
            .get(EXTRA_BODY_OPTION)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let max_tokens = numeric_option(&spec, generation::MAX_TOKENS_KEYS).and_then(|value| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a JSON number reaches this as f64; a negative or fractional \
                          output cap is not a cap and is dropped by the guard above"
            )]
            (value > 0.0).then_some(value as u64)
        });
        let sampling = Sampling {
            temperature: numeric_option(&spec, generation::TEMPERATURE_KEYS),
            top_p: numeric_option(&spec, generation::TOP_P_KEYS),
            frequency_penalty: numeric_option(&spec, &["frequencyPenalty", "frequency_penalty"]),
            presence_penalty: numeric_option(&spec, &["presencePenalty", "presence_penalty"]),
        };
        let tool_choice = first_option(&spec, generation::TOOL_CHOICE_KEYS).cloned();
        let timeouts = http_timeouts(&spec).map_err(Declined::Failed)?;
        let idle = timeouts
            .chunk()
            .map(StreamIdleTimeout::new)
            .unwrap_or_default();

        Ok(Self {
            spec,
            profile,
            base_url: base_url.trim_end_matches('/').to_owned(),
            capabilities,
            extra_body,
            max_tokens,
            sampling,
            tool_choice,
            credential,
            transport,
            idle,
            timeouts,
        })
    }

    /// Override the sampling parameters sent with every request.
    ///
    /// They are still subject to [`Quirks::accepts_sampling_params`]; this sets
    /// what would be sent, not whether it is.
    #[must_use]
    pub fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }

    /// Override the idle allowance between chunks.
    #[must_use]
    pub const fn with_idle_timeout(mut self, idle: StreamIdleTimeout) -> Self {
        self.idle = idle;
        self
    }

    /// The spec this instance was built from.
    #[must_use]
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The claim-table row this instance matched.
    #[must_use]
    pub const fn profile(&self) -> Profile {
        self.profile
    }

    /// The quirks that apply to one model on one surface.
    #[must_use]
    pub fn quirks_for(&self, model_id: &str, request_surface: ApiSurface) -> Quirks {
        Quirks::resolve(
            self.profile,
            &self.spec,
            self.capabilities_for(model_id),
            model_id,
            request_surface,
        )
    }

    /// The absolute URL one request will be sent to.
    ///
    /// Exposed because the Azure and Copilot rules are only observable as a URL,
    /// and a test that asserts the rule should assert the thing that goes on the
    /// wire rather than an intermediate enum.
    #[must_use]
    pub fn endpoint(&self, model_id: &str, request_surface: ApiSurface) -> String {
        let surface = self.quirks_for(model_id, request_surface).surface;
        let mut url = format!("{}{}", self.base_url, endpoint_path(surface));
        if let Some(version) = &self.spec.api_version {
            url.push_str("?api-version=");
            url.push_str(version);
        }
        url
    }

    /// The body one request will carry.
    ///
    /// Exposed for the same reason as [`endpoint`](Self::endpoint): the
    /// reasoning-content echo and the `thinking` ordering are properties of the
    /// bytes, and asserting them should not require a socket.
    ///
    /// # Panics
    ///
    /// Panics when `request` violates a local request-shape invariant. Production
    /// dispatch uses [`try_body_for`](Self::try_body_for) and returns that typed
    /// failure through the provider stream; this convenience accessor exists for
    /// diagnostics and tests that construct known-valid requests.
    #[must_use]
    pub fn body_for(&self, request: &CompletionRequest) -> Value {
        self.try_body_for(request)
            .expect("diagnostic request body must satisfy local request-shape invariants")
    }

    /// Build one request body while preserving local request-shape failures.
    ///
    /// # Errors
    ///
    /// Returns a permanent local error when caller metadata tries to override
    /// Zuno's reserved session-affinity field, or when affinity cannot be merged
    /// into an object-shaped metadata value.
    pub fn try_body_for(&self, request: &CompletionRequest) -> Result<Value, ProviderError> {
        let quirks = self.quirks_for(&request.model_id, request.surface);
        let mut body = RequestBody::new(request.model_id.clone(), request.messages.clone());
        body.developer_context
            .clone_from(&request.developer_context);
        body.tools = function_envelopes(&request.tools);
        body.sampling = self.sampling;
        body.max_tokens = self.max_tokens;
        body.tool_choice = self.tool_choice.clone();
        body.extra_body = self.extra_body.clone();
        let mut body = body.build(&quirks);
        // `quirks.surface`, not `request.surface`: the request usually carries
        // `Default` and this profile's own rules decide whether that means
        // `/chat/completions` or `/responses`. The endpoint is built from the same
        // resolved value on the next line of `http_request`.
        request.apply_parameters(&mut body, quirks.surface);
        project_session_affinity(&self.extra_body, request, quirks.surface, &mut body)?;
        Ok(body)
    }

    /// The full request one completion will send.
    ///
    /// # Errors
    ///
    /// Propagates local request-shape failures from
    /// [`try_body_for`](Self::try_body_for).
    pub fn http_request(&self, request: &CompletionRequest) -> Result<HttpRequest, ProviderError> {
        let mut headers = self.headers();
        headers.extend(request.headers.clone());
        Ok(HttpRequest {
            url: self.endpoint(&request.model_id, request.surface),
            headers,
            body: self.try_body_for(request)?,
            timeouts: self.timeouts,
        })
    }

    /// Capabilities for one model: the per-model override, else the provider set.
    #[must_use]
    pub fn capabilities_for(&self, model_id: &str) -> Capabilities {
        self.spec
            .options
            .get(MODEL_CAPABILITIES_OPTION)
            .and_then(|map| map.get(model_id))
            .and_then(|value| read_capabilities(Some(value)))
            .unwrap_or(self.capabilities)
    }

    fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = self.spec.headers.clone();
        headers.insert("accept".to_owned(), "text/event-stream".to_owned());
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        if let Some(credential) = &self.credential {
            // Azure names its key differently from everyone else; the spec's own
            // headers win, so a deployment that needs `api-key` sets it there.
            headers
                .entry("authorization".to_owned())
                .or_insert_with(|| format!("Bearer {credential}"));
        }
        headers
    }
}

impl Provider for CompatibleProvider {
    fn id(&self) -> &str {
        &self.spec.provider
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let quirks = self.quirks_for(&request.model_id, request.surface);
        if !quirks.accepts_attachments() && request_contains_attachment(&request) {
            let error = ProviderError::UnsupportedCapability {
                provider: self.spec.provider.clone(),
                model: request.model_id,
                capability: "attachments",
            };
            return Box::pin(futures::stream::once(async move { Err(error) }));
        }
        let surface = quirks.surface;
        let http = match self.http_request(&request) {
            Ok(http) => http,
            Err(error) => return Box::pin(futures::stream::once(async move { Err(error) })),
        };
        let provider = self.spec.provider.clone();
        let model = request.model_id.clone();
        let idle = self.idle;
        let transport = Arc::clone(&self.transport);

        Box::pin(
            futures::stream::once(async move {
                match transport.send(http).await {
                    Ok(chunks) => translate(chunks, provider, model, surface, idle).left_stream(),
                    Err(error) => futures::stream::once(async move { Err(error) }).right_stream(),
                }
            })
            .flatten(),
        )
    }
}

fn project_session_affinity(
    provider_parameters: &Map<String, Value>,
    request: &CompletionRequest,
    surface: ApiSurface,
    body: &mut Value,
) -> Result<(), ProviderError> {
    for parameters in [provider_parameters, &request.parameters] {
        let overrides_reserved_key = parameters
            .get("metadata")
            .and_then(Value::as_object)
            .is_some_and(|metadata| metadata.contains_key(ZUNO_SESSION_METADATA_KEY));
        if overrides_reserved_key {
            return Err(ProviderError::fatal(
                RequestShapeError::ReservedSessionMetadata,
            ));
        }
    }
    if surface != ApiSurface::Responses {
        return Ok(());
    }
    let Some(identity) = request
        .request_context()
        .and_then(|context| context.session_identity())
    else {
        return Ok(());
    };
    let root = body
        .as_object_mut()
        .expect("compatible request builders always return an object");
    let metadata = root
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(metadata) = metadata.as_object_mut() else {
        return Err(ProviderError::fatal(
            RequestShapeError::MetadataMustBeObject,
        ));
    };
    metadata.insert(
        ZUNO_SESSION_METADATA_KEY.to_owned(),
        Value::String(identity.as_str().to_owned()),
    );
    Ok(())
}

fn request_contains_attachment(request: &CompletionRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, zuno_llm::event::RequestContentBlock::Image { .. }))
    })
}

/// Drive one response body to completion, emitting shared events.
///
/// The parser and the translator are separate on purpose: framing and UTF-8
/// boundaries belong to `zuno-llm`'s one parser, and chat-completions semantics
/// belong here.
fn translate(
    chunks: crate::transport::ChunkStream,
    provider: String,
    model: String,
    surface: ApiSurface,
    idle: StreamIdleTimeout,
) -> impl futures::Stream<Item = Result<StreamEvent, ProviderError>> + Send {
    struct State {
        chunks: crate::transport::ChunkStream,
        parser: SseParser,
        translator: SurfaceTranslator,
        pending: std::collections::VecDeque<StreamEvent>,
        provider: String,
        model: String,
        idle: StreamIdleTimeout,
        drained: bool,
    }

    let parser = SseParser::for_stream(provider.clone(), model.clone());
    let tool_input_limit = parser.limits().max_tool_input_bytes();
    let state = State {
        chunks,
        parser,
        translator: SurfaceTranslator::with_tool_input_limit(
            provider.clone(),
            model.clone(),
            surface,
            tool_input_limit,
        ),
        pending: std::collections::VecDeque::new(),
        provider,
        model,
        idle,
        drained: false,
    };

    futures::stream::unfold(Some(state), |maybe| async move {
        let mut state = maybe?;
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((Ok(event), Some(state)));
            }
            if state.drained {
                return None;
            }

            let next = state
                .idle
                .wait(&state.provider, &state.model, state.chunks.next())
                .await;
            let chunk = match next {
                Err(timeout) => return Some((Err(timeout), None)),
                Ok(Some(Err(error))) => return Some((Err(error), None)),
                Ok(Some(Ok(bytes))) => Some(bytes),
                Ok(None) => None,
            };

            let frames = match chunk {
                Some(bytes) => state.parser.push(&bytes),
                None => {
                    state.drained = true;
                    let mut frames = state.parser.finish();
                    frames.retain(|frame| match frame {
                        Ok(frame) => !frame.data.is_empty(),
                        Err(_) => true,
                    });
                    frames
                }
            };

            for frame in frames {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => return Some((Err(error), None)),
                };
                match state.translator.frame(&frame.data) {
                    Ok(events) => state.pending.extend(events),
                    Err(error) => return Some((Err(error), None)),
                }
            }

            if state.drained {
                state.pending.extend(state.translator.finish());
            }
        }
    })
}

/// Lift a refusal into the registry's decline channel.
///
/// [`Declined::Failed`] rather than [`Declined::Unavailable`] because
/// `Unavailable`'s three reasons are fixed strings that cannot name the crate the
/// user should configure instead — and naming it is the whole point. The variant is
/// still terminal: `ProviderError::Fatal` maps to `Recovery::Fail`, so nothing
/// retries a misrouted provider.
fn first_option<'spec>(spec: &'spec Spec, keys: &[&str]) -> Option<&'spec Value> {
    keys.iter().find_map(|key| spec.options.get(*key))
}

/// The first of `keys` the options bag carries as a number.
///
/// A non-numeric value is skipped rather than falling back to a later spelling:
/// `temperature: "hot"` is a mistake to leave visible in the config, not a reason to
/// read a different key.
fn numeric_option(spec: &Spec, keys: &[&str]) -> Option<f64> {
    first_option(spec, keys).and_then(Value::as_f64)
}

fn http_timeouts(spec: &Spec) -> Result<HttpTimeouts, ProviderError> {
    Ok(HttpTimeouts::new(
        timeout_option(spec, REQUEST_TIMEOUT_OPTION, true, None)?,
        timeout_option(
            spec,
            HEADER_TIMEOUT_OPTION,
            true,
            Some(DEFAULT_RESPONSE_HEADER_TIMEOUT),
        )?,
        timeout_option(spec, CHUNK_TIMEOUT_OPTION, false, None)?,
    ))
}

fn timeout_option(
    spec: &Spec,
    option: &'static str,
    accepts_false: bool,
    default: Option<Duration>,
) -> Result<Option<Duration>, ProviderError> {
    let Some(value) = spec.options.get(option) else {
        return Ok(default);
    };
    if accepts_false && value == &Value::Bool(false) {
        return Ok(None);
    }
    let millis = value
        .as_u64()
        .and_then(|millis| u32::try_from(millis).ok())
        .filter(|millis| *millis > 0)
        .ok_or_else(|| {
            ProviderError::fatal(InvalidTimeoutOption {
                provider: spec.provider.clone(),
                option,
                expected: if accepts_false {
                    "a positive millisecond integer or false"
                } else {
                    "a positive millisecond integer"
                },
            })
        })?;
    Ok(Some(Duration::from_millis(u64::from(millis))))
}

fn unsupported(error: UnsupportedProvider) -> Declined {
    Declined::Failed(ProviderError::fatal(error))
}

/// The capability floor for an endpoint whose catalog entry says nothing.
///
/// Chat-completions endpoints universally accept `tools` and `temperature`, and a
/// meaningful share emit reasoning under a non-standard field, so those are the
/// defaults. Attachments and explicit prompt-cache breakpoints are not universal,
/// so they are off until declared. The catalog (todo 26) narrows this per model,
/// and [`MODEL_CAPABILITIES_OPTION`] is how it arrives.
#[must_use]
pub const fn compatible_default_capabilities() -> Capabilities {
    Capabilities {
        reasoning: true,
        tool_calls: true,
        prompt_cache: false,
        attachments: false,
        sampling_params: true,
    }
}

/// Wrap each tool in OpenAI's `function` envelope, or `None` when there are none.
///
/// `None` rather than an empty array: several compatible vendors reject
/// `tools: []` outright instead of reading it as "no tools", so an empty snapshot
/// has to leave the key off entirely.
fn function_envelopes(tools: &[ToolSchema]) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    Some(Value::Array(
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect(),
    ))
}

/// Read a capability object, keeping the default for any absent field.
fn read_capabilities(value: Option<&Value>) -> Option<Capabilities> {
    let object = value?.as_object()?;
    let default = compatible_default_capabilities();
    let flag =
        |key: &str, fallback: bool| object.get(key).and_then(Value::as_bool).unwrap_or(fallback);
    Some(Capabilities {
        reasoning: flag("reasoning", default.reasoning),
        tool_calls: flag("tool_calls", default.tool_calls),
        prompt_cache: flag("prompt_cache", default.prompt_cache),
        attachments: flag("attachments", default.attachments),
        sampling_params: flag("sampling_params", default.sampling_params),
    })
}

/// A factory suitable for
/// [`ProviderRegistry::register_fallible`](zuno_llm::registry::ProviderRegistry::register_fallible).
///
/// `credentials` is a lookup rather than a value because a factory may run again
/// after a token refresh, and because this crate must not depend on `zuno-auth` —
/// the composition root already does, and passing a closure keeps the credential
/// store out of this dependency graph entirely.
pub fn factory<C>(
    transport: Arc<dyn Transport>,
    credentials: C,
) -> impl Fn(Spec) -> FactoryOutcome + Send + Sync + 'static
where
    C: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    move |spec: Spec| {
        let credential = credentials(&spec.provider);
        let provider = CompatibleProvider::new(spec, Arc::clone(&transport), credential)?;
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::error::Error as _;
    use std::sync::Mutex;
    use zuno_llm::registry::{ProviderRequestContext, ProviderSessionIdentity};

    #[derive(Debug)]
    struct NeverCalled;

    impl Transport for NeverCalled {
        fn send(
            &self,
            _request: HttpRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::transport::ChunkStream, ProviderError>,
                    > + Send
                    + '_,
            >,
        > {
            unreachable!("these tests never open a stream")
        }
    }

    #[derive(Debug, Default)]
    struct RecordingTransport {
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl Transport for RecordingTransport {
        fn send(
            &self,
            request: HttpRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::transport::ChunkStream, ProviderError>,
                    > + Send
                    + '_,
            >,
        > {
            self.requests.lock().expect("request lock").push(request);
            Box::pin(async {
                let stream: crate::transport::ChunkStream =
                    Box::pin(futures::stream::empty::<Result<Vec<u8>, ProviderError>>());
                Ok(stream)
            })
        }
    }

    fn build(spec: Spec) -> Result<CompatibleProvider, Declined> {
        CompatibleProvider::new(spec, Arc::new(NeverCalled), Some("k".to_owned()))
    }

    fn groq() -> CompatibleProvider {
        build(Spec::new("groq").with_base_url("https://api.groq.com/openai/v1")).expect("claimed")
    }

    fn responses_provider() -> CompatibleProvider {
        build(
            Spec::new("wire-test")
                .with_base_url("https://gateway.example/v1")
                .with_surface(ApiSurface::Responses)
                .with_option(crate::family::TRANSPORT_OPTION, json!("openai-compatible")),
        )
        .expect("declared compatible Responses provider")
    }

    fn affinity_request() -> CompletionRequest {
        CompletionRequest::new(
            "gateway-model",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hello",
            )],
        )
        .with_request_context(ProviderRequestContext::MainTurn(
            ProviderSessionIdentity::parse("ses_compatible_affinity")
                .expect("valid session identity"),
        ))
    }

    #[test]
    fn compatible_responses_projects_typed_session_affinity() {
        let body = responses_provider().body_for(&affinity_request());

        assert_eq!(
            body["metadata"]["zuno_session_id"],
            "ses_compatible_affinity"
        );
        assert!(
            !body["input"]
                .to_string()
                .contains("ses_compatible_affinity"),
            "routing identity became model-visible: {body}"
        );
    }

    #[test]
    fn compatible_responses_preserves_unrelated_metadata_beside_affinity() {
        let mut request = affinity_request();
        request
            .parameters
            .insert("metadata".to_owned(), json!({"tenant": "tenant-a"}));

        let body = responses_provider().body_for(&request);

        assert_eq!(body["metadata"]["tenant"], "tenant-a");
        assert_eq!(
            body["metadata"]["zuno_session_id"],
            "ses_compatible_affinity"
        );
    }

    #[test]
    fn compatible_responses_rejects_reserved_affinity_overrides() {
        let mut request = affinity_request();
        request.parameters.insert(
            "metadata".to_owned(),
            json!({"zuno_session_id":"ses_overridden"}),
        );

        let error = responses_provider()
            .try_body_for(&request)
            .expect_err("reserved metadata must fail locally");
        let source = error.source().expect("request-shape source is preserved");
        assert!(source.to_string().contains("zuno_session_id"), "{source}");
    }

    #[test]
    fn compatible_responses_rejects_provider_extra_body_affinity_overrides() {
        let provider = build(
            Spec::new("wire-test")
                .with_base_url("https://gateway.example/v1")
                .with_surface(ApiSurface::Responses)
                .with_option(crate::family::TRANSPORT_OPTION, json!("openai-compatible"))
                .with_option(
                    EXTRA_BODY_OPTION,
                    json!({"metadata": {"zuno_session_id": "ses_provider_override"}}),
                ),
        )
        .expect("declared compatible Responses provider");

        let error = provider
            .try_body_for(&affinity_request())
            .expect_err("provider metadata must not replace durable affinity");
        let source = error.source().expect("request-shape source is preserved");
        assert!(source.to_string().contains("zuno_session_id"), "{source}");
    }

    #[test]
    fn compatible_chat_does_not_fabricate_session_affinity_metadata() {
        let provider = build(
            Spec::new("wire-test")
                .with_base_url("https://gateway.example/v1")
                .with_surface(ApiSurface::Chat)
                .with_option(crate::family::TRANSPORT_OPTION, json!("openai-compatible")),
        )
        .expect("declared compatible Chat provider");

        let body = provider.body_for(&affinity_request());

        assert!(
            body.get("metadata").is_none(),
            "Chat Completions has no Zuno affinity projection: {body}"
        );
    }

    #[test]
    fn a_missing_base_url_is_an_incomplete_configuration_not_a_guess() {
        let declined = build(Spec::new("groq")).expect_err("no base URL");
        assert!(matches!(
            declined,
            Declined::Unavailable(Unavailable::IncompleteConfiguration)
        ));
    }

    #[test]
    fn a_trailing_slash_in_the_base_url_does_not_double_up() {
        let provider =
            build(Spec::new("groq").with_base_url("https://api.groq.com/openai/v1/")).expect("ok");
        assert_eq!(
            provider.endpoint("llama-3.3-70b", ApiSurface::Default),
            "https://api.groq.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn the_provider_reports_the_identity_it_was_constructed_for() {
        assert_eq!(groq().id(), "groq");
    }

    #[test]
    fn a_per_model_capability_override_narrows_the_provider_default() {
        let spec = Spec::new("groq")
            .with_base_url("https://api.groq.com/openai/v1")
            .with_option(
                MODEL_CAPABILITIES_OPTION,
                json!({"o3": {"sampling_params": false}}),
            );
        let provider = build(spec).expect("ok");
        assert!(provider.capabilities().sampling_params);
        assert!(!provider.capabilities_for("o3").sampling_params);
        assert!(provider.capabilities_for("llama-3.3-70b").sampling_params);
    }

    #[tokio::test]
    async fn a_text_only_model_rejects_an_image_before_the_transport_is_called() {
        let provider = groq();
        let request = CompletionRequest::new(
            "llama-3.3-70b",
            vec![zuno_llm::event::Message::from_content(
                zuno_llm::event::Role::User,
                vec![zuno_llm::event::RequestContentBlock::Image {
                    filename: Some("diagram.png".to_owned()),
                    media_type: "image/png".to_owned(),
                    data: "AAAA".to_owned(),
                }],
            )],
        );

        let error = provider
            .stream(request)
            .next()
            .await
            .expect("one local failure event")
            .expect_err("the image must not be silently dropped");
        let ProviderError::UnsupportedCapability {
            provider,
            model,
            capability,
        } = error
        else {
            panic!("unsupported attachments must be a typed permanent failure");
        };
        assert_eq!(provider, "groq");
        assert_eq!(model, "llama-3.3-70b");
        assert_eq!(capability, "attachments");
    }

    #[tokio::test]
    async fn an_image_capable_model_serializes_the_image_and_calls_transport() {
        let transport = Arc::new(RecordingTransport::default());
        let spec = Spec::new("groq")
            .with_base_url("https://api.groq.com/openai/v1")
            .with_option(
                MODEL_CAPABILITIES_OPTION,
                json!({"vision-model": {"attachments": true}}),
            );
        let provider = CompatibleProvider::new(spec, transport.clone(), Some("k".to_owned()))
            .expect("image-capable provider");
        let request = CompletionRequest::new(
            "vision-model",
            vec![zuno_llm::event::Message::from_content(
                zuno_llm::event::Role::User,
                vec![zuno_llm::event::RequestContentBlock::Image {
                    filename: Some("synthetic.png".to_owned()),
                    media_type: "image/png".to_owned(),
                    data: "AAAA".to_owned(),
                }],
            )],
        );

        let _events = provider.stream(request).collect::<Vec<_>>().await;

        let requests = transport.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 1, "the image request must reach transport");
        assert!(
            requests[0]
                .body
                .to_string()
                .contains("data:image/png;base64,AAAA"),
            "the serialized request must retain the typed image: {}",
            requests[0].body
        );
    }

    /// A spec's `extraBody` is copied onto the body verbatim.
    ///
    /// The key changed from `reasoning_effort` to `service_tier`. The old
    /// assertion was true, but it was about `extraBody` passthrough while reading
    /// as though the session's reasoning effort were covered — and it was not:
    /// `resolve_effort` was emitting `reasoningEffort`, which no endpoint reads.
    /// Real effort coverage now lives in
    /// `request.rs::the_sessions_effort_reaches_the_chat_body_as_reasoning_effort`.
    #[test]
    fn extra_body_from_the_spec_reaches_the_request() {
        let spec = Spec::new("groq")
            .with_base_url("https://api.groq.com/openai/v1")
            .with_option(EXTRA_BODY_OPTION, json!({"service_tier": "flex"}));
        let provider = build(spec).expect("ok");
        let request = CompletionRequest::new(
            "llama-3.3-70b",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hi",
            )],
        );
        assert_eq!(provider.body_for(&request)["service_tier"], json!("flex"));
    }

    #[test]
    fn provider_timeout_options_reach_the_http_request() {
        let spec = Spec::new("groq")
            .with_base_url("https://api.groq.com/openai/v1")
            .with_option("timeout", json!(300_000))
            .with_option("headerTimeout", json!(330_000))
            .with_option("chunkTimeout", json!(210_000));
        let provider = build(spec).expect("ok");
        let request = CompletionRequest::new(
            "llama-3.3-70b",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hi",
            )],
        );

        let http = provider.http_request(&request).expect("valid request");
        assert_eq!(
            http.timeouts.total(),
            Some(std::time::Duration::from_millis(300_000))
        );
        assert_eq!(
            http.timeouts.header(),
            Some(std::time::Duration::from_millis(330_000))
        );
        assert_eq!(
            http.timeouts.chunk(),
            Some(std::time::Duration::from_millis(210_000))
        );
    }

    #[test]
    fn response_header_timeout_has_a_safe_default_and_false_disables_it() {
        let default = build(Spec::new("groq").with_base_url("https://api.groq.com/openai/v1"))
            .expect("default provider");
        let disabled = build(
            Spec::new("groq")
                .with_base_url("https://api.groq.com/openai/v1")
                .with_option("headerTimeout", json!(false)),
        )
        .expect("provider with disabled header timeout");
        let request = CompletionRequest::new(
            "llama-3.3-70b",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hi",
            )],
        );

        assert_eq!(
            default
                .http_request(&request)
                .expect("default request")
                .timeouts
                .header(),
            Some(DEFAULT_RESPONSE_HEADER_TIMEOUT)
        );
        assert_eq!(
            disabled
                .http_request(&request)
                .expect("disabled request")
                .timeouts
                .header(),
            None
        );
    }

    /// A session's reasoning level must reach the provider's own body builder.
    ///
    /// `body_for` is what `http_request` sends, so this closes the last hop: the
    /// options come from production [`resolve_effort`], the body from production
    /// `body_for`, and the assertion is on the wire field name rather than on the
    /// SDK option name the resolver produced.
    #[test]
    fn a_sessions_reasoning_level_reaches_body_for_under_its_wire_name() {
        let spec = Spec::new("groq").with_base_url("https://api.groq.com/openai/v1");
        let provider = build(spec).expect("ok");
        let resolved = zuno_llm::effort::resolve_effort(
            zuno_llm::effort::ProviderFamily::OpenAi,
            zuno_llm::effort::ReasoningEffort::High,
            zuno_llm::effort::EffortCapabilities::default(),
            &zuno_llm::effort::DeclaredVariants::new(),
        );
        let mut request = CompletionRequest::new(
            "llama-3.3-70b",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hi",
            )],
        )
        .on_surface(ApiSurface::Chat);
        request.parameters = resolved.options;

        let body = provider.body_for(&request);
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(
            body.get("reasoningEffort").is_none(),
            "the SDK option name reached the wire: {body}"
        );
    }

    /// The body must be lowered against the surface it is actually posted to.
    ///
    /// A wire capture caught this: a request whose own `surface` is `Default` gets
    /// routed to `/responses` by this profile's rules, and lowering against the
    /// request's hint instead of the resolved surface put the flat Chat field
    /// `reasoning_effort` on a Responses request. Both halves are read from
    /// production here — the endpoint from `endpoint`, the body from `body_for` —
    /// so they cannot disagree without failing.
    #[test]
    fn the_body_is_lowered_against_the_surface_the_endpoint_resolves_to() {
        // `xai` is one of the ids this profile pins to Responses by rule rather
        // than by anything on the request, which is exactly the trap.
        let spec = Spec::new("xai").with_base_url("http://127.0.0.1/v1");
        let provider = build(spec).expect("ok");
        let resolved = zuno_llm::effort::resolve_effort(
            zuno_llm::effort::ProviderFamily::OpenAi,
            zuno_llm::effort::ReasoningEffort::High,
            zuno_llm::effort::EffortCapabilities::default(),
            &zuno_llm::effort::DeclaredVariants::new(),
        );
        let mut request = CompletionRequest::new(
            "stub-reasoner",
            vec![zuno_llm::event::Message::new(
                zuno_llm::event::Role::User,
                "hi",
            )],
        );
        request.parameters = resolved.options;
        assert_eq!(
            request.surface,
            ApiSurface::Default,
            "the trap only exists when the request itself names no surface"
        );

        let sent = provider
            .http_request(&request)
            .expect("valid Responses request");
        assert!(
            sent.url.ends_with("/responses"),
            "this spec must route to the Responses surface: {}",
            sent.url
        );
        assert_eq!(sent.body["reasoning"], json!({"effort": "high"}));
        assert!(
            sent.body.get("reasoning_effort").is_none(),
            "a Chat field reached a Responses request: {}",
            sent.body
        );
    }

    #[test]
    fn an_api_version_is_appended_as_a_query_parameter() {
        let spec = Spec::new("azure")
            .with_base_url("https://r.openai.azure.com/openai/v1")
            .with_api_version("2025-04-01-preview");
        let provider = build(spec).expect("ok");
        assert_eq!(
            provider.endpoint("gpt-4o", ApiSurface::Default),
            "https://r.openai.azure.com/openai/v1/responses?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn a_bearer_token_is_added_but_a_spec_header_wins() {
        let provider = groq();
        assert_eq!(
            provider.headers().get("authorization").map(String::as_str),
            Some("Bearer k")
        );

        let explicit = build(
            Spec::new("azure")
                .with_base_url("https://r.openai.azure.com/openai/v1")
                .with_header("authorization", "api-key k"),
        )
        .expect("ok");
        assert_eq!(
            explicit.headers().get("authorization").map(String::as_str),
            Some("api-key k")
        );
    }

    #[test]
    fn the_factory_declines_a_foreign_family_before_any_request_is_built() {
        let make = factory(Arc::new(NeverCalled), |_| Some("k".to_owned()));
        let declined = make(Spec::new("amazon-bedrock").with_base_url("https://example.test"))
            .expect_err("bedrock is not compatible");
        let Declined::Failed(error) = declined else {
            panic!("a misrouted provider must be a failure, not an availability state");
        };
        assert_eq!(error.recovery(), zuno_error::Recovery::Fail);
    }
}
