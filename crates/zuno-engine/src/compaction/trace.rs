use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zuno_config::schema::CompactionConfig;
use zuno_db::Connection;
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::message::{MessageRecord, MessageStore};
use zuno_error::DbError;
use zuno_llm::event::{RequestContentBlock, Role};
use zuno_llm::registry::{ApiSurface, CompletionRequest};

use crate::prompt::{PromptAssembly, PromptProviderProjection};

/// Persist the exact tool-free, post-hook request before it can reach a provider.
pub(super) fn record_request(
    connection: &mut Connection,
    session_id: &str,
    attempt_id: &str,
    config: &CompactionConfig,
    completion: &CompletionRequest,
    summary: &mut MessageRecord,
) -> Result<(), DbError> {
    let mut assembly = PromptAssembly::default();
    let mut system = Vec::new();
    for (index, message) in completion.messages.iter().enumerate() {
        if message.role != Role::System && index + 1 != completion.messages.len() {
            continue;
        }
        let content = message
            .content
            .iter()
            .filter_map(|block| match block {
                RequestContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (id, source) = if message.role == Role::System {
            system.push(content.clone());
            (
                format!("agent.compaction.system.{index}"),
                "compaction:resolved-agent-prompt".to_owned(),
            )
        } else {
            (
                "runtime.compaction.summary".to_owned(),
                "compaction:post-hook".to_owned(),
            )
        };
        assembly
            .push(id, source, content)
            .expect("compaction section identifiers are unique and valid");
    }
    let projection = PromptProviderProjection {
        system_messages: &system,
        developer_context: &completion.developer_context,
    };
    let actual = json!({
        "model": completion.model_id,
        "surface": match completion.surface {
            ApiSurface::Default => "default",
            ApiSurface::Chat => "chat",
            ApiSurface::Responses => "responses",
            ApiSurface::Messages => "messages",
        },
        "messages": completion.messages.iter().map(|message| message.message()).collect::<Vec<_>>(),
        "developerContext": completion.developer_context,
        "tools": [],
        "timeoutSeconds": config.timeout_seconds.map_or(
            super::response::DEFAULT_TIMEOUT_SECONDS, |value| value.get()
        ),
        "maxSummaryBytes": config.max_summary_bytes.map_or(
            super::response::DEFAULT_MAX_SUMMARY_BYTES, |value| value.get()
        ),
    });
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&actual).expect("compaction request is JSON serializable"),
    ));
    let mut properties = assembly.event_properties("compaction", 1, projection, projection);
    properties.insert("purpose".to_owned(), Value::String("compaction".to_owned()));
    properties.insert("attemptId".to_owned(), Value::String(attempt_id.to_owned()));
    properties.insert("compactionRequest".to_owned(), actual);
    properties.insert("compactionRequestSha256".to_owned(), Value::String(digest));
    properties.insert(
        "summaryMessageID".to_owned(),
        Value::String(summary.id.clone()),
    );
    let transaction = connection.transaction().map_err(zuno_db::open::map_error)?;
    let receipt = append_in(
        &transaction,
        session_id,
        NewSessionEvent::new("session.compaction.prompt", properties)?,
    )?;
    summary
        .data
        .insert("promptReceiptID".to_owned(), Value::String(receipt.id));
    MessageStore::new(&transaction).put_message(summary)?;
    transaction.commit().map_err(zuno_db::open::map_error)
}
