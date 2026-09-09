//! Bounded, audited model work owned by the learning domain.

use crate::{
    ExtractionRequest, LearningExtraction, LearningExtractor, LearningServiceError, Result,
};
use async_trait::async_trait;
use futures::StreamExt;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tracing::Instrument as _;
use zuno_config::ResolvedLearningConfig;
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_error::{LearningError, ProviderError};
use zuno_llm::{
    event::{FinishReason, Message, Role, StreamEvent},
    registry::{
        ApiSurface, CompletionRequest, Provider, ProviderRequestContext, ToolSchema, generation,
    },
    stream::StreamAccumulator,
};

pub const LEARNING_EXTRACTOR_VERSION: &str = "zuno-learning-extractor-v2";

#[derive(Clone)]
pub struct LearningModel {
    pub provider_id: String,
    pub model_id: String,
    pub wire_id: String,
    pub surface: ApiSurface,
}

/// A minimal provider binding. It holds no foreground host, tools or MCP client.
#[derive(Clone)]
pub struct LearningModelClient {
    pub provider: Arc<dyn Provider>,
    pub model: LearningModel,
    pub events: SessionEventLog,
    pub limits: ResolvedLearningConfig,
}

impl LearningModelClient {
    pub async fn json<T: DeserializeOwned + JsonSchema>(
        &self,
        session_id: &str,
        operation: &str,
        system: &str,
        input: Value,
    ) -> Result<T> {
        let schema = strict_schema::<T>();
        let mut messages = vec![
            Message::new(
                Role::System,
                format!("{system}\nOutput JSON schema:\n{schema}"),
            ),
            Message::new(Role::User, input.to_string()),
        ];
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(self.limits.execution_timeout_ms);
        // A syntax repair is a new, fully logged request with the invalid answer.
        // It shares the original deadline and can happen only once.
        for attempt in 0..2 {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(timeout)?;
            let mut bounded = self.clone();
            bounded.limits.execution_timeout_ms = u64::try_from(remaining.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let completion = bounded
                .complete(
                    session_id,
                    operation,
                    messages.clone(),
                    Vec::new(),
                    Some(&schema),
                )
                .await?;
            match serde_json::from_str(strip_json_fence(completion.text())) {
                Ok(output) => return Ok(output),
                Err(error) if attempt == 0 => {
                    messages.push(Message::new(Role::Assistant, completion.text()));
                    messages.push(Message::new(
                        Role::User,
                        format!(
                            "The previous answer did not match the JSON schema: {error}. \
                         Return one corrected JSON object; do not add evidence or facts."
                        ),
                    ));
                }
                Err(error) => {
                    return Err(invalid(&format!(
                        "learning output remained invalid after one repair: {error}"
                    )));
                }
            }
        }
        unreachable!("both bounded attempts return")
    }

    /// Execute one bounded request. Tools, when supplied by an offline evaluator,
    /// are schemas only; this service has no tool dispatcher.
    pub async fn complete(
        &self,
        session_id: &str,
        operation: &str,
        messages: Vec<Message>,
        tools: Vec<ToolSchema>,
        schema: Option<&Value>,
    ) -> Result<StreamAccumulator> {
        let mut parameters = serde_json::Map::new();
        parameters.insert(
            generation::MAX_TOKENS.to_owned(),
            json!(self.limits.execution_max_output_tokens),
        );
        if self.provider.capabilities().sampling_params {
            parameters.insert(generation::TEMPERATURE.to_owned(), json!(0));
        }
        if self.limits.execution_structured_output
            && let Some(schema) = schema
        {
            match self.model.surface {
                ApiSurface::Chat => {
                    parameters.insert(
                        "response_format".to_owned(),
                        json!({"type":"json_schema","json_schema":{
                            "name":"learning_output","strict":true,"schema":schema}}),
                    );
                }
                ApiSurface::Responses => {
                    parameters.insert("text".to_owned(), json!({"format":{
                        "type":"json_schema","name":"learning_output","strict":true,"schema":schema}}));
                }
                ApiSurface::Messages => {
                    parameters.insert(
                        "output_config".to_owned(),
                        json!({"format":{"type":"json_schema","schema":schema}}),
                    );
                }
                ApiSurface::Default => {
                    return Err(invalid(
                        "structured output needs an explicit supported provider surface",
                    ));
                }
            }
        }
        let tool_values: Vec<_> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name":tool.name,"description":tool.description,"parameters":tool.parameters
                })
            })
            .collect();
        let input =
            json!({"messages": &messages, "parameters": &parameters, "tools": &tool_values});
        if input.to_string().len() > self.limits.execution_max_input_bytes as usize {
            return Err(invalid(
                "learning request exceeds its serialized input budget",
            ));
        }
        let request_id = format!("learning_request_{}", uuid::Uuid::now_v7().simple());
        self.event(session_id, &format!("{operation}.request"), json!({
            "requestID":request_id,
            "extractorVersion":LEARNING_EXTRACTOR_VERSION,
            "model":{"providerID":self.model.provider_id,"modelID":self.model.model_id,
                "wireID":self.model.wire_id},
            "tools": &tool_values, "request": &input, "promptDigest":crate::digest_text(&input.to_string()),
            "compaction":"disabled",
        }))?;
        let purpose = if operation.starts_with("learning.evaluation") {
            ProviderRequestContext::Evaluation
        } else {
            ProviderRequestContext::Learning
        };
        let request = CompletionRequest::new(self.model.wire_id.clone(), messages)
            .on_surface(self.model.surface)
            .with_parameters(parameters)
            .with_tools(tools.clone())
            .with_request_context(purpose);
        let span = zuno_observability::span::provider_request_for_session(
            session_id,
            &self.model.provider_id,
            &self.model.model_id,
            1,
            true,
            operation,
        );
        let result = tokio::time::timeout(
            Duration::from_millis(self.limits.execution_timeout_ms),
            self.collect(request, !tools.is_empty()),
        )
        .instrument(span.clone())
        .await
        .map_err(|_| timeout())
        .and_then(|result| result);
        zuno_observability::span::record_provider_outcome(
            &span,
            if result.is_ok() { "completed" } else { "error" },
            result.as_ref().err().map(|_| "learning_request"),
            None,
        );
        match &result {
            Ok(output) => self.event(
                session_id,
                &format!("{operation}.outcome"),
                json!({
                    "requestID":request_id,
                    "status":"completed", "output":output.text(),
                    "outputDigest":crate::digest_text(output.text()),
                    "toolCalls":output.tool_calls().iter().map(|call| json!({
                        "id":call.id,"name":call.name,"arguments":call.raw_input
                    })).collect::<Vec<_>>()
                }),
            )?,
            Err(error) => self.event(
                session_id,
                &format!("{operation}.outcome"),
                json!({"requestID":request_id,"status":"failed","error":error.to_string()}),
            )?,
        }
        result
    }

    async fn collect(
        &self,
        request: CompletionRequest,
        tools_allowed: bool,
    ) -> Result<StreamAccumulator> {
        let mut stream = self.provider.stream(request);
        let max_bytes = (self.limits.execution_max_output_tokens as usize)
            .saturating_mul(32)
            .clamp(16_384, 2_097_152);
        let mut output =
            StreamAccumulator::with_limits(&self.model.provider_id, "learning", max_bytes);
        let mut completed = false;
        let mut received = 0usize;
        let mut event_count = 0usize;
        while let Some(event) = stream.next().await {
            let event = event.map_err(provider_error)?;
            event_count += 1;
            if event_count > 100_000 {
                return Err(invalid("learning provider exceeded the stream event limit"));
            }
            match &event {
                StreamEvent::Error {
                    message,
                    retry_after,
                } => {
                    if let Some(after) = retry_after {
                        return Err(provider_error(ProviderError::RateLimited {
                            retry_after: Some(*after),
                        }));
                    }
                    return Err(transient(message));
                }
                StreamEvent::ToolUseStart { .. } if !tools_allowed => {
                    return Err(invalid("learning extractor attempted a tool call"));
                }
                StreamEvent::NativeToolCall { .. }
                | StreamEvent::ToolResult { .. }
                | StreamEvent::GeneratedImage { .. } => {
                    return Err(invalid(
                        "learning provider attempted an unexposed native tool",
                    ));
                }
                StreamEvent::TextDelta(text)
                | StreamEvent::ReasoningDelta(text)
                | StreamEvent::ReasoningSignatureDelta(text)
                | StreamEvent::ToolInputDelta { delta: text, .. } => {
                    received = received.saturating_add(text.len())
                }
                StreamEvent::MessageEnd { stop_reason } => {
                    if matches!(stop_reason, Some(FinishReason::Length)) {
                        return Err(invalid("learning output reached its token ceiling"));
                    }
                    completed = true;
                }
                StreamEvent::RetryRollback { .. } => completed = false,
                _ => {}
            }
            if received > max_bytes || output.tool_calls().len() > 32 {
                return Err(invalid(
                    "learning output exceeds its byte or tool-call limit",
                ));
            }
            output.apply(&event).map_err(provider_error)?;
        }
        if !completed {
            return Err(transient(
                "learning extraction stream ended before MessageEnd",
            ));
        }
        Ok(output)
    }

    pub fn event(&self, session_id: &str, kind: &str, value: Value) -> Result<()> {
        let properties = value.as_object().cloned().expect("event object");
        self.events
            .append(session_id, NewSessionEvent::new(kind, properties)?)?;
        Ok(())
    }
}

#[async_trait]
impl LearningExtractor for LearningModelClient {
    fn version(&self) -> &str {
        LEARNING_EXTRACTOR_VERSION
    }

    fn prepare_request(&self, request: ExtractionRequest) -> Result<ExtractionRequest> {
        // Reserve room for the schema, instructions, and one bounded syntax repair.
        let budget = (self.limits.execution_max_input_bytes as usize).saturating_sub(16_384);
        request.bounded(budget)
    }

    async fn extract(&self, request: ExtractionRequest) -> Result<LearningExtraction> {
        let request = self.prepare_request(request)?;
        let output: LearningExtraction = self
            .json(
                &request.session_id,
                "learning.extraction",
                extraction_prompt(),
                serde_json::to_value(&request).expect("serializable extraction"),
            )
            .await?;
        output.validate_bounds()?;
        Ok(output)
    }
}

pub fn extraction_prompt() -> &'static str {
    "You are Zuno's isolated experience extractor. You have no tools or filesystem authority. \
     Record concrete outcomes, problems, corrections, feedback and verified procedures. \
     Use only supplied sources. Copy source.reference_id into evidence.source_id and an exact \
     substring of source.content into evidence.excerpt. Never invent evidence. Empty sources \
     require empty experiences and memories. Assistant prose alone does not prove execution. \
     Only proves_success=true marks host-verified execution. Unresolved issues must have \
     kind=unresolved_issue, resolution=null and no memory. Propose memory only for stable facts, \
     preferences or project rules supported by cited experiences. At most 32 experiences, \
     16 memories and 16 evidence items per experience. Return only JSON."
}

pub(crate) fn strict_schema<T: JsonSchema>() -> Value {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("$schema");
                object.remove("default");
                object.remove("format");
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    let keys: Vec<_> = properties.keys().cloned().collect();
                    object.insert("required".to_owned(), json!(keys));
                    object.insert("additionalProperties".to_owned(), json!(false));
                }
                for child in object.values_mut() {
                    normalize(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    normalize(child);
                }
            }
            _ => {}
        }
    }
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    normalize(&mut schema);
    schema
}

pub(crate) fn strip_json_fence(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix("```json")
        .or_else(|| value.strip_prefix("```"))
        .and_then(|value| value.trim().strip_suffix("```"))
        .unwrap_or(value)
        .trim()
}

pub(crate) fn invalid(detail: &str) -> LearningServiceError {
    LearningError::InvalidRequest {
        field: "learning.execution".to_owned(),
        detail: detail.to_owned(),
    }
    .into()
}

fn transient(detail: &str) -> LearningServiceError {
    LearningServiceError::Extractor {
        version: LEARNING_EXTRACTOR_VERSION.to_owned(),
        source: Box::new(std::io::Error::other(detail.to_owned())),
    }
}

fn timeout() -> LearningServiceError {
    LearningServiceError::Extractor {
        version: LEARNING_EXTRACTOR_VERSION.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "learning request reached its total deadline",
        )),
    }
}

fn provider_error(source: ProviderError) -> LearningServiceError {
    LearningServiceError::ExtractorProvider {
        version: LEARNING_EXTRACTOR_VERSION.to_owned(),
        source,
    }
}
