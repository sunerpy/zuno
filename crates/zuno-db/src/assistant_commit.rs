//! One model step's message, parts and usage are a single durable fact.
//!
//! The engine prepares these records without holding a database transaction.
//! This local provider owns previous-usage lookup and atomic persistence.

use crate::message::{MessageRecord, MessageRole, MessageStore, PartKind, PartRecord};
use crate::{Connection, open, session};
use zuno_error::DbError;
use zuno_types::identity::PrincipalKey;

#[derive(Debug, Clone)]
pub struct AssistantCommit {
    pub message: MessageRecord,
    pub parts: Vec<PartRecord>,
    pub persisted_at_ms: i64,
    pub context_limit: Option<i64>,
}

/// Validate immutable message/invocation identities independently of a database.
/// The caller reads the previous records under the same lock as its writes.
pub fn validate_commit(
    commit: &AssistantCommit,
    previous: Option<&MessageRecord>,
    existing_parts: &[PartRecord],
) -> Result<(), DbError> {
    if commit.message.role != MessageRole::Assistant
        || commit.parts.iter().any(|part| {
            part.session_id != commit.message.session_id || part.message_id != commit.message.id
        })
    {
        return Err(DbError::Conflict {
            table: "message".to_owned(),
            id: commit.message.id.clone(),
            detail: "assistant commit contains a different message or session".to_owned(),
        });
    }
    if previous.is_some_and(|record| {
        record.session_id != commit.message.session_id
            || record.role != MessageRole::Assistant
            || (record
                .data
                .get("time")
                .and_then(|time| time.get("completed"))
                .is_some()
                && record.data != commit.message.data)
    }) {
        return Err(DbError::Conflict {
            table: "message".to_owned(),
            id: commit.message.id.clone(),
            detail: "assistant commit cannot replace another identity or a completed message"
                .to_owned(),
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for part in &commit.parts {
        if !ids.insert(&part.id) {
            return Err(DbError::Conflict {
                table: "part".to_owned(),
                id: part.id.clone(),
                detail: "assistant commit repeats a part identity".to_owned(),
            });
        }
        if previous.is_some_and(|message| {
            message
                .data
                .get("time")
                .and_then(|time| time.get("completed"))
                .is_some_and(|completed| !completed.is_null())
        }) && !existing_parts.iter().any(|stored| {
            stored.id == part.id
                && stored.message_id == part.message_id
                && stored.session_id == part.session_id
                && stored.time_created == part.time_created
                && stored.kind == part.kind
                && stored.data == part.data
        }) {
            return Err(DbError::Conflict {
                table: "part".to_owned(),
                id: part.id.clone(),
                detail: "a completed assistant step cannot acquire different content".to_owned(),
            });
        }
        match existing_parts
            .iter()
            .find(|previous| previous.id == part.id)
        {
            Some(previous)
                if previous.session_id != part.session_id
                    || previous.message_id != part.message_id
                    || previous.kind != part.kind
                    || (previous.kind == PartKind::Tool
                        && (previous.data.get("callID") != part.data.get("callID")
                            || previous.data.get("tool") != part.data.get("tool")
                            || previous
                                .data
                                .get("state")
                                .and_then(|state| state.get("input"))
                                != part
                                    .data
                                    .get("state")
                                    .and_then(|state| state.get("input"))
                            || (matches!(
                                previous
                                    .data
                                    .get("state")
                                    .and_then(|state| state.get("status"))
                                    .and_then(serde_json::Value::as_str),
                                Some("completed" | "error")
                            ) && previous.data != part.data))) =>
            {
                return Err(DbError::Conflict {
                    table: "part".to_owned(),
                    id: part.id.clone(),
                    detail: "assistant commit cannot replace another part identity or settled invocation".to_owned(),
                });
            }
            Some(_) | None => {}
        }
    }
    Ok(())
}

pub fn commit_assistant(
    connection: &Connection,
    owner: &PrincipalKey,
    commit: &AssistantCommit,
) -> Result<(), DbError> {
    let tx = open::immediate_transaction(connection)?;
    session::get_owned(&tx, &commit.message.session_id, owner)?;
    let store = MessageStore::new(&tx);
    let previous = store.find_message(&commit.message.id)?;
    let mut existing = Vec::with_capacity(commit.parts.len());
    for part in &commit.parts {
        match store.part(&part.id) {
            Ok(part) => existing.push(part),
            Err(DbError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    validate_commit(commit, previous.as_ref(), &existing)?;
    let previous = previous.map(|record| session::MessageUsage::from_data(&record.data));
    store.put_message_at(&commit.message, commit.persisted_at_ms)?;
    for part in &commit.parts {
        store.put_part_at(part, commit.persisted_at_ms)?;
    }
    session::reconcile_usage(
        &tx,
        &commit.message.session_id,
        previous,
        session::MessageUsage::from_data(&commit.message.data),
        commit.context_limit,
    )?;
    tx.commit().map_err(open::map_error)
}
