use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zuno_db::continuity::{
    MAX_NOTE_DOCUMENT_BYTES, MAX_NOTE_DOCUMENTS, MAX_NOTE_SCOPE_BYTES, ensure_schema,
    validate_note_name,
};
use zuno_db::message::{
    MessageRecord, MessageRole, MessageStore, PartKind, PartRecord, now_millis,
};
use zuno_db::{Pool, open};
use zuno_error::DbError;

use crate::ContinuityError;
use crate::history::HistoryProvider;
use crate::notes::{NoteScope, NotesProvider};
use crate::token;

const TOKEN_VERSION: u8 = 1;
const DEFAULT_PAGE_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 20;
const MAX_QUERY_BYTES: usize = 1_000;

/// SQLite provider shared by the native history and notes interfaces.
pub struct SqliteContinuityProvider {
    pool: Arc<Pool>,
}

impl SqliteContinuityProvider {
    /// Open the provider and create additive note tables when notes are enabled.
    pub fn open(pool: Arc<Pool>, notes_enabled: bool) -> Result<Self, ContinuityError> {
        if notes_enabled {
            let connection = pool.get()?;
            ensure_schema(&connection)?;
        }
        Ok(Self { pool })
    }

    #[must_use]
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PageCursor {
    version: u8,
    kind: String,
    session_id: String,
    agent: Option<String>,
    scope_sha256: String,
    offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WindowToken {
    version: u8,
    session_id: String,
    start_id: Option<String>,
    end_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ItemToken {
    version: u8,
    session_id: String,
    message_id: String,
}

#[derive(Debug, Clone)]
struct WindowRange {
    start: usize,
    end: usize,
    token: WindowToken,
}

struct HistoryIndex {
    messages: Vec<MessageRecord>,
    windows: Vec<WindowRange>,
    internal_message_ids: BTreeSet<String>,
}

impl HistoryIndex {
    fn load(connection: &rusqlite::Connection, session_id: &str) -> Result<Self, ContinuityError> {
        ensure_session(connection, session_id)?;
        let store = MessageStore::new(connection);
        let messages = store.messages_for_session(session_id)?;
        let markers = store.parts_for_session_by_kind(session_id, PartKind::Compaction)?;
        let successful = successful_compaction_messages(&store, &messages, &markers)?;
        let internal_message_ids = markers
            .iter()
            .map(|marker| marker.message_id.clone())
            .collect();
        let positions = messages
            .iter()
            .enumerate()
            .map(|(index, message)| (message.id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut starts = BTreeSet::from([0_usize]);
        for marker in &markers {
            if successful.contains(&marker.message_id)
                && let Some(tail_start_id) =
                    marker.data.get("tail_start_id").and_then(Value::as_str)
                && let Some(index) = positions.get(tail_start_id)
            {
                starts.insert(*index);
            }
        }
        let starts = starts.into_iter().collect::<Vec<_>>();
        let windows = starts
            .iter()
            .enumerate()
            .map(|(ordinal, start)| {
                let end = starts.get(ordinal + 1).copied().unwrap_or(messages.len());
                WindowRange {
                    start: *start,
                    end,
                    token: WindowToken {
                        version: TOKEN_VERSION,
                        session_id: session_id.to_owned(),
                        start_id: messages.get(*start).map(|message| message.id.clone()),
                        end_id: messages.get(end).map(|message| message.id.clone()),
                    },
                }
            })
            .collect();
        Ok(Self {
            messages,
            windows,
            internal_message_ids,
        })
    }

    fn window_by_token(&self, encoded: &str) -> Result<&WindowRange, ContinuityError> {
        let decoded: WindowToken = token::decode(encoded, "window_id")?;
        self.windows
            .iter()
            .find(|window| window.token == decoded)
            .ok_or_else(|| {
                ContinuityError::Invalid(
                    "window_id does not name a current window in this session".to_owned(),
                )
            })
    }

    fn window_for_message(&self, index: usize) -> Option<&WindowRange> {
        self.windows
            .iter()
            .find(|window| index >= window.start && index < window.end)
    }

    fn is_visible_message(&self, message: &MessageRecord) -> bool {
        !self.internal_message_ids.contains(&message.id) && !is_internal_message(message)
    }
}

fn successful_compaction_messages(
    store: &MessageStore<'_>,
    messages: &[MessageRecord],
    markers: &[PartRecord],
) -> Result<BTreeSet<String>, DbError> {
    let marker_ids = markers
        .iter()
        .map(|marker| marker.message_id.as_str())
        .collect::<BTreeSet<_>>();
    let children = messages
        .iter()
        .filter_map(|message| {
            let parent = message.data.get("parentID").and_then(Value::as_str)?;
            (marker_ids.contains(parent)
                && !message.data.contains_key("error")
                && message.data.get("finish").and_then(Value::as_str) == Some("stop"))
            .then_some((message.id.clone(), parent.to_owned()))
        })
        .collect::<Vec<_>>();
    let child_ids = children
        .iter()
        .map(|(message_id, _)| message_id.clone())
        .collect::<Vec<_>>();
    let text_parts = store.parts_by_message_kind(&child_ids, PartKind::Text)?;
    Ok(children
        .into_iter()
        .filter(|(message_id, _)| {
            text_parts.get(message_id).is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.data
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
                })
            })
        })
        .map(|(_, parent_id)| parent_id)
        .collect())
}

impl HistoryProvider for SqliteContinuityProvider {
    fn list_windows(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError> {
        let connection = self.pool.get()?;
        let index = HistoryIndex::load(&connection, session_id)?;
        let limit = page_limit(limit, MAX_LIST_LIMIT)?;
        let scope = digest_text("windows");
        let offset = cursor_offset(cursor, "history.windows", session_id, None, &scope)?;
        let visible = index.windows.iter().rev().collect::<Vec<_>>();
        let page = visible.iter().skip(offset).take(limit);
        let mut windows = Vec::new();
        for window in page {
            let messages = &index.messages[window.start..window.end];
            let visible_messages = messages
                .iter()
                .filter(|message| index.is_visible_message(message))
                .collect::<Vec<_>>();
            windows.push(json!({
                "window_id": token::encode(&window.token)?,
                "item_count": visible_messages.len(),
                "start_time": visible_messages.first().map(|message| message.time_created),
                "end_time": visible_messages.last().map(|message| message.time_created),
                "current": window.end == index.messages.len(),
                "boundary": if window.start == 0 {
                    "session_start"
                } else {
                    "successful_compaction"
                }
            }));
        }
        let next_offset = offset.saturating_add(windows.len());
        Ok(json!({
            "windows": windows,
            "next_cursor": next_cursor(
                next_offset,
                visible.len(),
                "history.windows",
                session_id,
                None,
                &scope,
            )?,
        }))
    }

    fn list_items(
        &self,
        session_id: &str,
        window_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError> {
        let connection = self.pool.get()?;
        let index = HistoryIndex::load(&connection, session_id)?;
        let window = index.window_by_token(window_id)?;
        if window.token.session_id != session_id {
            return Err(ContinuityError::Invalid(
                "window_id belongs to another session".to_owned(),
            ));
        }
        let limit = page_limit(limit, MAX_LIST_LIMIT)?;
        let scope = digest_text(window_id);
        let offset = cursor_offset(cursor, "history.items", session_id, None, &scope)?;
        let visible = index.messages[window.start..window.end]
            .iter()
            .filter(|message| index.is_visible_message(message))
            .collect::<Vec<_>>();
        let page = visible
            .iter()
            .skip(offset)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();
        let ids = page
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        let parts = MessageStore::new(&connection).parts_by_message(&ids)?;
        let mut items = Vec::with_capacity(page.len());
        for message in page {
            items.push(normalized_item(
                session_id,
                window_id,
                message,
                parts.get(&message.id).map(Vec::as_slice).unwrap_or(&[]),
            )?);
        }
        let next_offset = offset.saturating_add(items.len());
        Ok(json!({
            "window_id": window_id,
            "items": items,
            "next_cursor": next_cursor(
                next_offset,
                visible.len(),
                "history.items",
                session_id,
                None,
                &scope,
            )?,
        }))
    }

    fn read_item(&self, session_id: &str, item_id: &str) -> Result<Value, ContinuityError> {
        let connection = self.pool.get()?;
        let index = HistoryIndex::load(&connection, session_id)?;
        let item: ItemToken = token::decode(item_id, "item_id")?;
        if item.version != TOKEN_VERSION || item.session_id != session_id {
            return Err(ContinuityError::Invalid(
                "item_id belongs to another session or version".to_owned(),
            ));
        }
        let Some((message_index, message)) = index
            .messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.id == item.message_id)
        else {
            return Err(ContinuityError::Invalid(
                "item_id does not name a current-session message".to_owned(),
            ));
        };
        if !index.is_visible_message(message) {
            return Err(ContinuityError::Invalid(
                "item_id names an internal message that history does not expose".to_owned(),
            ));
        }
        let window = index.window_for_message(message_index).ok_or_else(|| {
            ContinuityError::Invalid("item_id is outside every current history window".to_owned())
        })?;
        let window_id = token::encode(&window.token)?;
        let parts = MessageStore::new(&connection)
            .parts_by_message(std::slice::from_ref(&message.id))?
            .remove(&message.id)
            .unwrap_or_default();
        normalized_item(session_id, &window_id, message, &parts)
    }

    fn search_contents(
        &self,
        session_id: &str,
        query: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError> {
        validate_query(query)?;
        let connection = self.pool.get()?;
        let index = HistoryIndex::load(&connection, session_id)?;
        let limit = page_limit(limit, MAX_SEARCH_LIMIT)?;
        let scope = digest_text(query);
        let offset = cursor_offset(cursor, "history.search", session_id, None, &scope)?;
        let visible = index
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| index.is_visible_message(message))
            .collect::<Vec<_>>();
        let ids = visible
            .iter()
            .map(|(_, message)| message.id.clone())
            .collect::<Vec<_>>();
        let parts = MessageStore::new(&connection).parts_by_message(&ids)?;
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for (message_index, message) in visible {
            let window = index.window_for_message(message_index).ok_or_else(|| {
                ContinuityError::Invalid(
                    "current-session message is outside every history window".to_owned(),
                )
            })?;
            let window_id = token::encode(&window.token)?;
            let item = normalized_item(
                session_id,
                &window_id,
                message,
                parts.get(&message.id).map(Vec::as_slice).unwrap_or(&[]),
            )?;
            let searchable = item
                .get("contents")
                .map(Value::to_string)
                .unwrap_or_default();
            if searchable.to_lowercase().contains(&needle) {
                matches.push(json!({
                    "item_id": item["item_id"],
                    "window_id": window_id,
                    "role": item["role"],
                    "created_at": item["created_at"],
                    "snippet": snippet(&searchable, query, 500),
                }));
            }
        }
        let page = matches
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_offset = offset.saturating_add(page.len());
        Ok(json!({
            "query": query,
            "matches": page,
            "next_cursor": next_cursor(
                next_offset,
                matches.len(),
                "history.search",
                session_id,
                None,
                &scope,
            )?,
        }))
    }
}

fn normalized_item(
    session_id: &str,
    window_id: &str,
    message: &MessageRecord,
    parts: &[PartRecord],
) -> Result<Value, ContinuityError> {
    let mut contents = Vec::new();
    let mut omitted = BTreeSet::new();
    for part in parts {
        match part.kind {
            PartKind::Text => {
                let synthetic = part.data.get("synthetic").and_then(Value::as_bool) == Some(true)
                    || part
                        .data
                        .get("metadata")
                        .and_then(Value::as_object)
                        .and_then(|metadata| metadata.get("synthetic"))
                        .and_then(Value::as_bool)
                        == Some(true);
                if !synthetic
                    && let Some(text) = part.data.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    contents.push(json!({"kind": "text", "text": text}));
                }
            }
            PartKind::Subtask => {
                let mut content = Map::new();
                content.insert("kind".to_owned(), Value::String("subtask".to_owned()));
                for key in ["description", "prompt", "command", "agent"] {
                    if let Some(value) = part.data.get(key) {
                        content.insert(key.to_owned(), sanitize_value(value));
                    }
                }
                contents.push(Value::Object(content));
            }
            PartKind::Tool => {
                let state = part.data.get("state").and_then(Value::as_object);
                let mut content = Map::new();
                content.insert("kind".to_owned(), Value::String("tool".to_owned()));
                if let Some(tool) = part.data.get("tool").and_then(Value::as_str) {
                    content.insert("tool".to_owned(), Value::String(tool.to_owned()));
                }
                if let Some(status) = state
                    .and_then(|state| state.get("status"))
                    .and_then(Value::as_str)
                {
                    content.insert("status".to_owned(), Value::String(status.to_owned()));
                }
                if let Some(output) = state.and_then(|state| state.get("output")) {
                    content.insert("output".to_owned(), sanitize_value(output));
                } else if let Some(error) = state.and_then(|state| state.get("error")) {
                    content.insert("error".to_owned(), sanitize_value(error));
                }
                contents.push(Value::Object(content));
            }
            PartKind::Reasoning => {
                omitted.insert("reasoning");
            }
            PartKind::File => {
                omitted.insert("binary_attachment");
            }
            PartKind::Compaction => {
                omitted.insert("compaction_marker");
            }
            PartKind::StepStart
            | PartKind::StepFinish
            | PartKind::Snapshot
            | PartKind::Patch
            | PartKind::Agent
            | PartKind::Retry => {
                omitted.insert(part.kind.as_str());
            }
        }
    }
    let token = ItemToken {
        version: TOKEN_VERSION,
        session_id: session_id.to_owned(),
        message_id: message.id.clone(),
    };
    Ok(json!({
        "item_id": token::encode(&token)?,
        "message_id": message.id,
        "window_id": window_id,
        "role": match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        },
        "agent": message.data.get("agent").and_then(Value::as_str),
        "created_at": message.time_created,
        "contents": contents,
        "omitted_kinds": omitted,
    }))
}

fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| {
                    let normalized = key.to_ascii_lowercase();
                    !normalized.contains("encrypted")
                        && !normalized.contains("reasoning")
                        && normalized != "raw"
                })
                .map(|(key, value)| (key.clone(), sanitize_value(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(sanitize_value).collect()),
        Value::String(text) if text.starts_with("data:") && text.contains(";base64,") => {
            Value::String("[binary attachment omitted]".to_owned())
        }
        other => other.clone(),
    }
}

fn is_internal_message(message: &MessageRecord) -> bool {
    message.data.get("summary").and_then(Value::as_bool) == Some(true)
        || message.data.get("mode").and_then(Value::as_str) == Some("compaction")
}

#[derive(Debug, Clone)]
struct NoteRecord {
    name: String,
    revision: u64,
    content: String,
    content_sha256: String,
    time_created: i64,
    time_updated: i64,
}

impl NoteRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let revision: i64 = row.get(1)?;
        Ok(Self {
            name: row.get(0)?,
            revision: u64::try_from(revision).unwrap_or(0),
            content: row.get(2)?,
            content_sha256: row.get(3)?,
            time_created: row.get(4)?,
            time_updated: row.get(5)?,
        })
    }

    fn metadata(&self) -> Value {
        json!({
            "name": self.name,
            "revision": self.revision,
            "bytes": self.content.len(),
            "sha256": self.content_sha256,
            "created_at": self.time_created,
            "updated_at": self.time_updated,
        })
    }
}

impl NotesProvider for SqliteContinuityProvider {
    fn list_files_by_prefix(
        &self,
        scope: NoteScope<'_>,
        prefix: Option<&str>,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError> {
        let prefix = prefix.unwrap_or_default();
        validate_prefix(prefix)?;
        let connection = self.pool.get()?;
        ensure_session(&connection, scope.session_id)?;
        let limit = page_limit(limit, MAX_LIST_LIMIT)?;
        let filter = digest_text(prefix);
        let offset = cursor_offset(
            cursor,
            "notes.list",
            scope.session_id,
            Some(scope.agent),
            &filter,
        )?;
        let mut statement = connection
            .prepare(
                "SELECT name, revision, content, content_sha256, time_created, time_updated
                 FROM session_note
                 WHERE session_id = ?1 AND agent = ?2
                   AND substr(name, 1, length(?3)) = ?3
                 ORDER BY name ASC",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map(
                params![scope.session_id, scope.agent, prefix],
                NoteRecord::from_row,
            )
            .map_err(open::map_error)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(open::map_error)?);
        }
        let files = records
            .iter()
            .skip(offset)
            .take(limit)
            .map(NoteRecord::metadata)
            .collect::<Vec<_>>();
        let next_offset = offset.saturating_add(files.len());
        Ok(json!({
            "prefix": prefix,
            "files": files,
            "next_cursor": next_cursor(
                next_offset,
                records.len(),
                "notes.list",
                scope.session_id,
                Some(scope.agent),
                &filter,
            )?,
        }))
    }

    fn read_file(&self, scope: NoteScope<'_>, name: &str) -> Result<Value, ContinuityError> {
        validate_name(name)?;
        let connection = self.pool.get()?;
        ensure_session(&connection, scope.session_id)?;
        let record = read_note(&connection, scope, name)?.ok_or_else(|| {
            ContinuityError::Invalid(format!("note `{name}` does not exist in this Agent scope"))
        })?;
        Ok(json!({
            "name": record.name,
            "revision": record.revision,
            "content": record.content,
            "bytes": record.content.len(),
            "sha256": record.content_sha256,
            "created_at": record.time_created,
            "updated_at": record.time_updated,
        }))
    }

    fn search_contents(
        &self,
        scope: NoteScope<'_>,
        query: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError> {
        validate_query(query)?;
        let connection = self.pool.get()?;
        ensure_session(&connection, scope.session_id)?;
        let limit = page_limit(limit, MAX_SEARCH_LIMIT)?;
        let filter = digest_text(query);
        let offset = cursor_offset(
            cursor,
            "notes.search",
            scope.session_id,
            Some(scope.agent),
            &filter,
        )?;
        let mut statement = connection
            .prepare(
                "SELECT name, revision, content, content_sha256, time_created, time_updated
                 FROM session_note
                 WHERE session_id = ?1 AND agent = ?2
                 ORDER BY name ASC",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map(params![scope.session_id, scope.agent], NoteRecord::from_row)
            .map_err(open::map_error)?;
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for row in rows {
            let record = row.map_err(open::map_error)?;
            if record.content.to_lowercase().contains(&needle) {
                matches.push(json!({
                    "name": record.name,
                    "revision": record.revision,
                    "snippet": snippet(&record.content, query, 500),
                    "updated_at": record.time_updated,
                }));
            }
        }
        let page = matches
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_offset = offset.saturating_add(page.len());
        Ok(json!({
            "query": query,
            "matches": page,
            "next_cursor": next_cursor(
                next_offset,
                matches.len(),
                "notes.search",
                scope.session_id,
                Some(scope.agent),
                &filter,
            )?,
        }))
    }

    fn append_to_file(
        &self,
        scope: NoteScope<'_>,
        name: &str,
        content: &str,
        expected_revision: u64,
    ) -> Result<Value, ContinuityError> {
        self.mutate_note(
            scope,
            NoteMutation::Append,
            name,
            content,
            expected_revision,
        )
    }

    fn write_file(
        &self,
        scope: NoteScope<'_>,
        name: &str,
        content: &str,
        expected_revision: u64,
    ) -> Result<Value, ContinuityError> {
        self.mutate_note(scope, NoteMutation::Write, name, content, expected_revision)
    }
}

#[derive(Debug, Clone, Copy)]
enum NoteMutation {
    Append,
    Write,
}

impl NoteMutation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Write => "write",
        }
    }
}

impl SqliteContinuityProvider {
    fn mutate_note(
        &self,
        scope: NoteScope<'_>,
        mutation: NoteMutation,
        name: &str,
        content: &str,
        expected_revision: u64,
    ) -> Result<Value, ContinuityError> {
        validate_name(name)?;
        if scope.call_id.trim().is_empty() {
            return Err(ContinuityError::Invalid(
                "notes writes require a non-empty trusted call id".to_owned(),
            ));
        }
        if content.len() as u64 > MAX_NOTE_DOCUMENT_BYTES {
            return Err(ContinuityError::Invalid(format!(
                "content is larger than the {MAX_NOTE_DOCUMENT_BYTES}-byte document limit"
            )));
        }
        let expected_i64 = i64::try_from(expected_revision)
            .map_err(|_| ContinuityError::Invalid("expected_revision is too large".to_owned()))?;
        let request_sha256 = digest_json(&json!({
            "action": mutation.as_str(),
            "name": name,
            "content": content,
            "expected_revision": expected_revision,
        }))?;
        self.pool.try_transaction(|transaction| {
            ensure_session(transaction, scope.session_id)?;
            let previous_operation = transaction
                .query_row(
                    "SELECT request_sha256, result_revision, result_content_sha256
                     FROM session_note_operation
                     WHERE session_id = ?1 AND agent = ?2 AND call_id = ?3",
                    params![scope.session_id, scope.agent, scope.call_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(open::map_error)?;
            if let Some((stored_request, revision, content_sha256)) = previous_operation {
                if stored_request != request_sha256 {
                    return Err(ContinuityError::Invalid(format!(
                        "call_id `{}` was already used for a different notes mutation",
                        scope.call_id
                    )));
                }
                return Ok(json!({
                    "name": name,
                    "revision": revision,
                    "sha256": content_sha256,
                    "replayed": true,
                }));
            }

            let current = read_note(transaction, scope, name)?;
            match (&current, expected_revision) {
                (None, 0) => {}
                (Some(record), 0) => {
                    return Err(ContinuityError::Invalid(format!(
                        "note `{name}` already exists at revision {}; read it and retry with that revision",
                        record.revision
                    )));
                }
                (None, _) => {
                    return Err(ContinuityError::Invalid(format!(
                        "note `{name}` does not exist; use expected_revision 0 to create it"
                    )));
                }
                (Some(record), expected) if record.revision != expected => {
                    return Err(ContinuityError::Invalid(format!(
                        "revision conflict for `{name}`: expected {expected}, current {}",
                        record.revision
                    )));
                }
                (Some(_), _) => {}
            }
            let next_content = match (mutation, current.as_ref()) {
                (NoteMutation::Append, Some(record)) => {
                    let mut combined =
                        String::with_capacity(record.content.len().saturating_add(content.len()));
                    combined.push_str(&record.content);
                    combined.push_str(content);
                    combined
                }
                (NoteMutation::Append | NoteMutation::Write, None)
                | (NoteMutation::Write, Some(_)) => content.to_owned(),
            };
            let next_bytes = next_content.len() as u64;
            if next_bytes > MAX_NOTE_DOCUMENT_BYTES {
                return Err(ContinuityError::Invalid(format!(
                    "mutation would make `{name}` {next_bytes} bytes; the per-document limit is {MAX_NOTE_DOCUMENT_BYTES}"
                )));
            }
            let (document_count, total_bytes): (i64, i64) = transaction
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(length(CAST(content AS BLOB))), 0)
                     FROM session_note WHERE session_id = ?1 AND agent = ?2",
                    params![scope.session_id, scope.agent],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(open::map_error)?;
            if current.is_none()
                && u64::try_from(document_count).unwrap_or(u64::MAX) >= MAX_NOTE_DOCUMENTS
            {
                return Err(ContinuityError::Invalid(format!(
                    "this session-Agent scope already contains the maximum {MAX_NOTE_DOCUMENTS} documents"
                )));
            }
            let previous_bytes = current
                .as_ref()
                .map_or(0_u64, |record| record.content.len() as u64);
            let aggregate = u64::try_from(total_bytes)
                .unwrap_or(u64::MAX)
                .saturating_sub(previous_bytes)
                .saturating_add(next_bytes);
            if aggregate > MAX_NOTE_SCOPE_BYTES {
                return Err(ContinuityError::Invalid(format!(
                    "mutation would make this session-Agent scope {aggregate} bytes; the aggregate limit is {MAX_NOTE_SCOPE_BYTES}"
                )));
            }
            let revision = expected_revision.checked_add(1).ok_or_else(|| {
                ContinuityError::Invalid("note revision overflowed".to_owned())
            })?;
            let revision_i64 = i64::try_from(revision).map_err(|_| {
                ContinuityError::Invalid("note revision is too large".to_owned())
            })?;
            let content_sha256 = digest_text(&next_content);
            let now = now_millis();
            if let Some(record) = &current {
                let changed = transaction
                    .execute(
                        "UPDATE session_note
                         SET revision = ?1, content = ?2, content_sha256 = ?3, time_updated = ?4
                         WHERE session_id = ?5 AND agent = ?6 AND name = ?7 AND revision = ?8",
                        params![
                            revision_i64,
                            next_content,
                            content_sha256,
                            now,
                            scope.session_id,
                            scope.agent,
                            name,
                            expected_i64,
                        ],
                    )
                    .map_err(open::map_error)?;
                if changed != 1 {
                    return Err(ContinuityError::Invalid(format!(
                        "revision conflict for `{name}` while committing revision {}",
                        record.revision
                    )));
                }
            } else {
                transaction
                    .execute(
                        "INSERT INTO session_note
                         (session_id, agent, name, revision, content, content_sha256,
                          time_created, time_updated)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        params![
                            scope.session_id,
                            scope.agent,
                            name,
                            revision_i64,
                            next_content,
                            content_sha256,
                            now,
                        ],
                    )
                    .map_err(open::map_error)?;
            }
            transaction
                .execute(
                    "INSERT INTO session_note_operation
                     (session_id, agent, call_id, request_sha256, action, name,
                      result_revision, result_content_sha256, time_created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        scope.session_id,
                        scope.agent,
                        scope.call_id,
                        request_sha256,
                        mutation.as_str(),
                        name,
                        revision_i64,
                        content_sha256,
                        now,
                    ],
                )
                .map_err(open::map_error)?;
            Ok(json!({
                "name": name,
                "revision": revision,
                "bytes": next_bytes,
                "sha256": content_sha256,
                "replayed": false,
            }))
        })
    }
}

fn read_note(
    connection: &rusqlite::Connection,
    scope: NoteScope<'_>,
    name: &str,
) -> Result<Option<NoteRecord>, ContinuityError> {
    connection
        .query_row(
            "SELECT name, revision, content, content_sha256, time_created, time_updated
             FROM session_note
             WHERE session_id = ?1 AND agent = ?2 AND name = ?3",
            params![scope.session_id, scope.agent, name],
            NoteRecord::from_row,
        )
        .optional()
        .map_err(open::map_error)
        .map_err(ContinuityError::from)
}

fn ensure_session(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<(), ContinuityError> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
            [session_id],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if exists {
        Ok(())
    } else {
        Err(DbError::NotFound {
            table: "session".to_owned(),
            id: session_id.to_owned(),
        }
        .into())
    }
}

fn page_limit(value: Option<u32>, maximum: usize) -> Result<usize, ContinuityError> {
    let value = value.map_or(DEFAULT_PAGE_LIMIT, |limit| limit as usize);
    if (1..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(ContinuityError::Invalid(format!(
            "limit must be between 1 and {maximum}"
        )))
    }
}

fn cursor_offset(
    cursor: Option<&str>,
    kind: &str,
    session_id: &str,
    agent: Option<&str>,
    scope_sha256: &str,
) -> Result<usize, ContinuityError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let cursor: PageCursor = token::decode(cursor, "cursor")?;
    if cursor.version != TOKEN_VERSION
        || cursor.kind != kind
        || cursor.session_id != session_id
        || cursor.agent.as_deref() != agent
        || cursor.scope_sha256 != scope_sha256
    {
        return Err(ContinuityError::Invalid(
            "cursor does not belong to this action, session, Agent, or filter".to_owned(),
        ));
    }
    Ok(cursor.offset)
}

fn next_cursor(
    next_offset: usize,
    total: usize,
    kind: &str,
    session_id: &str,
    agent: Option<&str>,
    scope_sha256: &str,
) -> Result<Option<String>, ContinuityError> {
    if next_offset >= total {
        return Ok(None);
    }
    token::encode(&PageCursor {
        version: TOKEN_VERSION,
        kind: kind.to_owned(),
        session_id: session_id.to_owned(),
        agent: agent.map(str::to_owned),
        scope_sha256: scope_sha256.to_owned(),
        offset: next_offset,
    })
    .map(Some)
}

fn validate_query(query: &str) -> Result<(), ContinuityError> {
    if query.trim().is_empty() {
        return Err(ContinuityError::Invalid(
            "query must contain non-whitespace text".to_owned(),
        ));
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(ContinuityError::Invalid(format!(
            "query is longer than {MAX_QUERY_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), ContinuityError> {
    if prefix.len() > 255 {
        return Err(ContinuityError::Invalid(
            "note prefix is longer than 255 UTF-8 bytes".to_owned(),
        ));
    }
    if prefix.starts_with('/') || prefix.contains('\\') || prefix.contains('\0') {
        return Err(ContinuityError::Invalid(
            "note prefix must be a logical name prefix, not a host path".to_owned(),
        ));
    }
    if prefix.split('/').any(|segment| segment == "..") {
        return Err(ContinuityError::Invalid(
            "note prefix cannot contain a `..` segment".to_owned(),
        ));
    }
    if !prefix
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-".contains(character))
    {
        return Err(ContinuityError::Invalid(
            "note prefix may contain only ASCII letters, digits, `/`, `.`, `_`, and `-`".to_owned(),
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), ContinuityError> {
    validate_note_name(name).map_err(|error| ContinuityError::Invalid(error.to_string()))
}

fn digest_text(text: &str) -> String {
    hex_digest(Sha256::digest(text.as_bytes()).as_ref())
}

fn digest_json(value: &Value) -> Result<String, ContinuityError> {
    serde_json::to_vec(value)
        .map(|bytes| hex_digest(Sha256::digest(bytes).as_ref()))
        .map_err(ContinuityError::Encoding)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn snippet(text: &str, _query: &str, maximum_chars: usize) -> String {
    let mut chars = text.chars();
    let snippet = chars.by_ref().take(maximum_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_names_never_accept_host_paths() {
        for invalid in ["", "/tmp/a", "../a", "a/../b", "a//b", "a b", "a\\b"] {
            assert!(validate_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["evidence.md", "task/ci/run-1.txt", "A_1/x.y"] {
            validate_name(valid).expect(valid);
        }
    }

    #[test]
    fn sanitized_tool_output_removes_encrypted_and_binary_values() {
        assert_eq!(
            sanitize_value(&json!({
                "answer": "ok",
                "encryptedContent": "secret",
                "nested": {
                    "reasoning": "hidden",
                    "reasoningContent": "also hidden"
                },
                "image": "data:image/png;base64,AAAA"
            })),
            json!({
                "answer": "ok",
                "nested": {},
                "image": "[binary attachment omitted]"
            })
        );
    }

    #[test]
    fn cursor_scope_is_bound_to_the_current_session_and_agent() {
        let cursor = next_cursor(2, 3, "notes.list", "ses_a", Some("build"), "scope")
            .expect("cursor")
            .expect("next");
        assert_eq!(
            cursor_offset(Some(&cursor), "notes.list", "ses_a", Some("build"), "scope")
                .expect("same scope"),
            2
        );
        assert!(
            cursor_offset(Some(&cursor), "notes.list", "ses_a", Some("plan"), "scope").is_err()
        );
    }

    #[test]
    fn table_names_are_stable_for_export_and_prune() {
        assert_eq!(zuno_db::continuity::SESSION_NOTE_TABLE, "session_note");
        assert_eq!(
            zuno_db::continuity::SESSION_NOTE_OPERATION_TABLE,
            "session_note_operation"
        );
    }
}
