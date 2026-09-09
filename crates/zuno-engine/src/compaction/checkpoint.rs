use serde_json::Value;
use zuno_db::message::{MessageRole, MessageWithParts, PartKind};

pub(crate) struct HistoryCheckpoint<'a> {
    pub tail_index: usize,
    pub summary: &'a MessageWithParts,
}

pub(crate) fn is_compaction_summary(message: &MessageWithParts) -> bool {
    message.info.role == MessageRole::Assistant
        && message.info.data.get("summary").and_then(Value::as_bool) == Some(true)
}

fn completed_summary(message: &MessageWithParts) -> bool {
    is_compaction_summary(message)
        && message.info.data.get("finish").and_then(Value::as_str) == Some("stop")
        && message.info.data.get("error").is_none_or(Value::is_null)
        && message.parts.iter().any(|part| {
            part.kind == PartKind::Text
                && part
                    .data
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        })
}

pub(crate) fn visible_in_history(message: &MessageWithParts, active_summary: Option<&str>) -> bool {
    !is_compaction_summary(message)
        || (completed_summary(message) && active_summary.is_none_or(|id| id == message.info.id))
}

/// A newer failed or partial attempt cannot replace the last accepted checkpoint.
pub(crate) fn latest_checkpoint(history: &[MessageWithParts]) -> Option<HistoryCheckpoint<'_>> {
    for (marker_index, marker) in history.iter().enumerate().rev() {
        if marker.info.role != MessageRole::User {
            continue;
        }
        let tail_id = marker.parts.iter().find_map(|part| {
            (part.kind == PartKind::Compaction)
                .then(|| part.data.get("tail_start_id").and_then(Value::as_str))
                .flatten()
        });
        let Some(tail_id) = tail_id else { continue };
        let tail_index = history[..marker_index].iter().position(|message| {
            message.info.id == tail_id && message.info.session_id == marker.info.session_id
        });
        let Some(tail_index) = tail_index else {
            continue;
        };
        let summary = history[marker_index + 1..].iter().rev().find(|message| {
            completed_summary(message)
                && message.info.session_id == marker.info.session_id
                && message.info.data.get("parentID").and_then(Value::as_str)
                    == Some(marker.info.id.as_str())
        });
        if let Some(summary) = summary {
            return Some(HistoryCheckpoint {
                tail_index,
                summary,
            });
        }
    }
    None
}
