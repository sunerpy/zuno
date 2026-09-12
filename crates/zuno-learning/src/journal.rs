//! The learning model runner depends on an asynchronous audit port, not a local
//! database. A remote journal can fence requests before contacting the provider.
use crate::{LearningServiceError, LearningUsage, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningModelIdentity {
    pub provider_id: String,
    pub model_id: String,
    pub wire_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LearningModelOutcome {
    Completed {
        output: String,
        output_digest: String,
        tool_calls: Vec<LearningToolCall>,
    },
    Failed {
        error: String,
        provider_diagnostic: Option<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LearningModelEvent {
    Request {
        request_id: String,
        extractor_version: String,
        model: LearningModelIdentity,
        tools: Vec<Value>,
        request: Value,
        prompt_digest: String,
    },
    Outcome {
        request_id: String,
        outcome: LearningModelOutcome,
        usage: LearningUsage,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningModelRecord {
    pub session_id: String,
    pub operation: String,
    pub event: LearningModelEvent,
}

impl LearningModelRecord {
    /// Preserve the local session-event vocabulary and existing readers.
    pub fn session_event(&self) -> Result<NewSessionEvent> {
        let (phase, properties) = match &self.event {
            LearningModelEvent::Request {
                request_id,
                extractor_version,
                model,
                tools,
                request,
                prompt_digest,
            } => (
                "request",
                json!({
                    "requestID": request_id, "extractorVersion": extractor_version,
                    "model": {"providerID":model.provider_id,"modelID":model.model_id,"wireID":model.wire_id},
                    "tools":tools,"request":request,"promptDigest":prompt_digest,"compaction":"disabled",
                }),
            ),
            LearningModelEvent::Outcome {
                request_id,
                outcome,
                usage,
            } => {
                let mut properties: Map<String, Value> = serde_json::to_value(outcome)
                    .expect("model outcome serializes")
                    .as_object()
                    .expect("tagged outcome")
                    .clone();
                properties.insert("requestID".to_owned(), json!(request_id));
                properties.insert("usage".to_owned(), json!(usage));
                ("outcome", Value::Object(properties))
            }
        };
        Ok(NewSessionEvent::new(
            format!("{}.{}", self.operation, phase),
            properties.as_object().expect("event object").clone(),
        )?)
    }
}

#[async_trait]
pub trait LearningModelJournal: Send + Sync {
    /// A refused/undurable request record prevents the provider call. Outcome
    /// failure is returned to the owner rather than claiming durable success.
    async fn record(&self, record: LearningModelRecord) -> Result<()>;
}

#[derive(Clone)]
pub struct SqliteLearningModelJournal {
    events: SessionEventLog,
}
impl SqliteLearningModelJournal {
    pub fn new(events: SessionEventLog) -> Self {
        Self { events }
    }
}

#[async_trait]
impl LearningModelJournal for SqliteLearningModelJournal {
    async fn record(&self, record: LearningModelRecord) -> Result<()> {
        let events = self.events.clone();
        let event = record.session_event()?;
        tokio::task::spawn_blocking(move || {
            events.append(&record.session_id, event)?;
            Ok(())
        })
        .await
        .map_err(|source| LearningServiceError::Journal {
            source: Box::new(source),
        })?
    }
}
