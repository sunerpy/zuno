//! One session's full transcript, in the exact envelope `opencode export` prints
//! and `opencode import` reads back.
//!
//! # Why this is a store concern and not a CLI concern
//!
//! Upstream's `export` is a thin wrapper: it asks the session service for one
//! `Info` and every `WithParts`, then stringifies the pair
//! (`cli/cmd/export.ts:283-291`). Everything that makes the payload *correct* —
//! the field list, the message order, the part order, the identity keys the two
//! `data` blobs do not carry — belongs to whoever owns the rows. Putting the
//! envelope here means the CLI cannot invent a shape and the shape can be tested
//! without a process.
//!
//! # The redaction pass is a port, not an approximation
//!
//! `--sanitize` is the flag a user reaches for before attaching a transcript to
//! a bug report, so "mostly redacted" is the one failure mode that matters. Every
//! rule in [`sanitize`] maps to a named function in `cli/cmd/export.ts:11-220`,
//! including the two that look like bugs and are not: an all-whitespace string is
//! left alone (`value.trim() ? … : value`, `:12`) and an empty object is left
//! alone (`Object.keys(value).length ? … : value`, `:17`). Both are preserved
//! because a differential against the real binary compares bytes, and because a
//! reader who sees `""` learns that the field was empty rather than hidden.
//!
//! # Import writes what upstream writes
//!
//! `import` re-homes a transcript into the *current* project: the session's
//! `projectID`, `directory` and `path` are replaced, and on a re-import only
//! those three columns are updated (`cli/cmd/import.ts:178-193`). Messages and
//! parts are inserted `ON CONFLICT DO NOTHING`, so importing the same file twice
//! is not a way to mutate a transcript that already exists.

use rusqlite::{Connection, Transaction, params};
use serde::Serialize;
use serde_json::{Map as JsonMap, Value};
use zuno_error::DbError;

use crate::message::{MessageRecord, MessageStore, PartRecord};
use crate::open;
use crate::session_list::{SessionInfo, session_info};

/// The table name reported when a document does not have the export shape.
const DOCUMENT: &str = "export document";

/// One message and its parts, upstream's `SessionV1.WithParts`.
///
/// Both halves stay as [`Value`] because the store keeps them as opaque blobs
/// plus identity columns; re-typing them here would mean this module deciding
/// which fields a message may hold, which is exactly the decision
/// [`MessageRecord`] and [`PartRecord`] already own.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportMessage {
    /// The message JSON, with `id` and `sessionID` restored.
    pub info: Value,
    /// The message's parts in `id` order, each with its three identity keys.
    pub parts: Vec<Value>,
}

/// A whole session as `export` prints it.
///
/// `info` is a bare [`SessionInfo`], not the listing's `GlobalInfo`: upstream
/// exports `svc.get(sessionID)` (`export.ts:284`), which has no `project` key.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportDocument {
    /// The session itself.
    pub info: SessionInfo,
    /// Every message, oldest first.
    pub messages: Vec<ExportMessage>,
}

impl ExportDocument {
    /// The document as JSON, with every number rendered the way
    /// `JSON.stringify` renders it.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] if the tree cannot be encoded, which needs a
    /// non-finite `cost` to happen.
    pub fn to_json(&self) -> Result<Value, DbError> {
        let mut value = serde_json::to_value(self).map_err(|source| DbError::Decode {
            table: DOCUMENT.to_owned(),
            source,
        })?;
        normalize_numbers(&mut value);
        Ok(value)
    }
}

/// Render an integral float as an integer, everywhere in the tree.
///
/// JavaScript has one numeric type, so a `data` blob holding `{"cost":0.0}`
/// parses to `0` and `JSON.stringify` writes `0`; `serde_json` preserves the
/// `0.0`. That is not hypothetical: this port's own turn writer stores
/// `"cost": 0.0` into the message blob (`zuno-engine/src/loop.rs:1466`) while
/// upstream stores `0`, so without this pass `export` prints different bytes than
/// the released binary prints **from the same row** — and a user comparing,
/// hashing or diffing the two backups sees two different documents.
///
/// [`crate::session_list`] already does this for the one `cost` column it
/// serialises itself; the export needs it for the whole tree, because the two
/// opaque blobs pass through untyped.
///
/// A float outside `i64` is left alone rather than truncated: the point is to
/// match a lossless JavaScript round trip, and `as i64` on `1e300` is not one.
fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if let Some(float) = number.as_f64()
                && float.fract() == 0.0
                && number.as_i64().is_none()
                && let Ok(whole) = i64::try_from(float as i128)
                && (whole as f64) == float
            {
                *value = Value::Number(whole.into());
            }
        }
        Value::Object(object) => {
            for nested in object.values_mut() {
                normalize_numbers(nested);
            }
        }
        Value::Array(items) => {
            for nested in items {
                normalize_numbers(nested);
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

/// Read one session and its whole transcript.
///
/// Ordering is inherited from [`MessageStore::hydrate_session`]: messages by
/// `(time_created, id)` ascending and parts by `id` ascending, which is the order
/// `MessageV2.page` produces after upstream's reverse
/// (`session/session.ts:837-852`).
///
/// # Errors
///
/// [`DbError::NotFound`] when no session has this id, and [`DbError::Query`] or
/// [`DbError::Decode`] if a row cannot be read.
pub fn export(connection: &Connection, session_id: &str) -> Result<ExportDocument, DbError> {
    let session = crate::session::get(connection, session_id)?;
    let hydrated = MessageStore::new(connection).hydrate_session(session_id)?;
    Ok(ExportDocument {
        info: session_info(session),
        messages: hydrated
            .into_iter()
            .map(|message| ExportMessage {
                info: message.info.to_json(),
                parts: message.parts.iter().map(PartRecord::to_json).collect(),
            })
            .collect(),
    })
}

/// Where an imported session is re-homed.
///
/// Upstream overrides these three fields with the running instance's context
/// (`import.ts:178-183`) rather than trusting the file, so a transcript exported
/// from one checkout lists in the checkout it was imported into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTarget {
    /// The project the session now belongs to.
    pub project_id: String,
    /// The absolute directory it now reports.
    pub directory: String,
    /// `directory` relative to the project worktree; `""` at the root.
    pub path: String,
}

/// What one import wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// The session id from the document.
    pub session_id: String,
    /// How many messages the document carried.
    pub messages: usize,
    /// How many parts the document carried.
    pub parts: usize,
}

/// Write an exported document into this database, re-homed onto `target`.
///
/// # Errors
///
/// [`DbError::Decode`] when the document is not an export envelope or a message
/// or part inside it cannot be split into columns and blob, and
/// [`DbError::Query`] if a write fails.
pub fn import(
    transaction: &Transaction<'_>,
    document: &Value,
    target: &ImportTarget,
) -> Result<Imported, DbError> {
    let root = object(document, DOCUMENT)?;
    let info = root
        .get("info")
        .and_then(Value::as_object)
        .ok_or_else(|| decode_error("document has no `info` object"))?;
    let session_id = string(info, "id")?;
    write_session(transaction, info, &session_id, target)?;

    let messages = root
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| decode_error("document has no `messages` array"))?;
    let store = MessageStore::new(transaction);
    let mut parts_written = 0;
    for entry in messages {
        let entry = object(entry, DOCUMENT)?;
        let message = entry
            .get("info")
            .cloned()
            .ok_or_else(|| decode_error("a message has no `info` object"))?;
        let record = MessageRecord::from_json(rehome(message, &session_id))?;
        store.insert_message_if_absent(&record)?;

        let parts = entry
            .get("parts")
            .and_then(Value::as_array)
            .ok_or_else(|| decode_error("a message has no `parts` array"))?;
        for part in parts {
            let created = part
                .get("time")
                .and_then(Value::as_object)
                .and_then(|time| time.get("start").or_else(|| time.get("created")))
                .and_then(Value::as_i64)
                .unwrap_or(record.time_created);
            let record = PartRecord::from_json(rehome(part.clone(), &session_id), created)?;
            store.insert_part_if_absent(&record)?;
            parts_written += 1;
        }
    }
    Ok(Imported {
        session_id,
        messages: messages.len(),
        parts: parts_written,
    })
}

/// Point a message or part at the session id the document declares.
///
/// The file is the authority on which session these rows belong to, and the
/// blob's own `sessionID` can disagree with it after a share round-trip.
fn rehome(mut value: Value, session_id: &str) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("sessionID".to_owned(), Value::String(session_id.to_owned()));
    }
    value
}

/// Insert the session row, updating only the three re-homed columns on conflict.
fn write_session(
    transaction: &Transaction<'_>,
    info: &JsonMap<String, Value>,
    session_id: &str,
    target: &ImportTarget,
) -> Result<(), DbError> {
    let time = info.get("time").and_then(Value::as_object);
    let created = time
        .and_then(|time| time.get("created"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let summary = info.get("summary").and_then(Value::as_object);
    let tokens = info.get("tokens").and_then(Value::as_object);
    let cache = tokens
        .and_then(|tokens| tokens.get("cache"))
        .and_then(Value::as_object);

    transaction
        .execute(
            "INSERT INTO session \
             (id, project_id, workspace_id, parent_id, slug, directory, path, title, version, \
              share_url, summary_additions, summary_deletions, summary_files, summary_diffs, \
              metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, \
              tokens_cache_write, revert, permission, agent, model, time_created, time_updated, \
              time_compacting, time_archived) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29) \
             ON CONFLICT (id) DO UPDATE SET \
               project_id = excluded.project_id, \
               directory = excluded.directory, \
               path = excluded.path",
            params![
                session_id,
                target.project_id,
                text(info, "workspaceID"),
                text(info, "parentID"),
                text(info, "slug").unwrap_or_default(),
                target.directory,
                target.path,
                text(info, "title").unwrap_or_default(),
                text(info, "version").unwrap_or_default(),
                info.get("share")
                    .and_then(Value::as_object)
                    .and_then(|share| share.get("url"))
                    .and_then(Value::as_str),
                summary.and_then(|summary| number(summary, "additions")),
                summary.and_then(|summary| number(summary, "deletions")),
                summary.and_then(|summary| number(summary, "files")),
                summary.and_then(|summary| blob(summary, "diffs")),
                blob(info, "metadata"),
                info.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
                tokens
                    .and_then(|tokens| number(tokens, "input"))
                    .unwrap_or(0),
                tokens
                    .and_then(|tokens| number(tokens, "output"))
                    .unwrap_or(0),
                tokens
                    .and_then(|tokens| number(tokens, "reasoning"))
                    .unwrap_or(0),
                cache.and_then(|cache| number(cache, "read")).unwrap_or(0),
                cache.and_then(|cache| number(cache, "write")).unwrap_or(0),
                blob(info, "revert"),
                blob(info, "permission"),
                text(info, "agent"),
                blob(info, "model"),
                created,
                time.and_then(|time| time.get("updated"))
                    .and_then(Value::as_i64)
                    .unwrap_or(created),
                time.and_then(|time| time.get("compacting"))
                    .and_then(Value::as_i64),
                time.and_then(|time| time.get("archived"))
                    .and_then(Value::as_i64),
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

fn object<'value>(
    value: &'value Value,
    table: &str,
) -> Result<&'value JsonMap<String, Value>, DbError> {
    value
        .as_object()
        .ok_or_else(|| decode_error(&format!("{table} is not a JSON object")))
}

fn string(info: &JsonMap<String, Value>, key: &str) -> Result<String, DbError> {
    text(info, key)
        .map(str::to_owned)
        .ok_or_else(|| decode_error(&format!("`info.{key}` is missing or not a string")))
}

fn text<'value>(info: &'value JsonMap<String, Value>, key: &str) -> Option<&'value str> {
    info.get(key).and_then(Value::as_str)
}

fn number(info: &JsonMap<String, Value>, key: &str) -> Option<i64> {
    info.get(key).and_then(Value::as_i64)
}

/// A column the store carries as opaque JSON text, re-encoded from the document.
fn blob(info: &JsonMap<String, Value>, key: &str) -> Option<String> {
    match info.get(key)? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

fn decode_error(detail: &str) -> DbError {
    DbError::Decode {
        table: DOCUMENT.to_owned(),
        source: serde::de::Error::custom(detail),
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Replace every transcript and filesystem string with a redaction marker.
///
/// A port of `sanitize` (`cli/cmd/export.ts:163-220`) and the seven helpers it
/// calls. The document is rewritten in place so key order survives, matching a
/// JavaScript object spread that overwrites an existing property.
#[must_use]
pub fn sanitize(mut document: Value) -> Value {
    let Some(root) = document.as_object_mut() else {
        return document;
    };
    if let Some(info) = root.get_mut("info").and_then(Value::as_object_mut) {
        sanitize_info(info);
    }
    if let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            sanitize_message(message);
        }
    }
    document
}

fn sanitize_info(info: &mut JsonMap<String, Value>) {
    let id = owned_id(info);
    redact_field(info, "title", "session-title", &id);
    redact_field(info, "directory", "session-directory", &id);
    if let Some(summary) = info.get_mut("summary").and_then(Value::as_object_mut) {
        redact_diffs(summary, "session-diff");
    }
    if let Some(revert) = info.get_mut("revert").and_then(Value::as_object_mut) {
        redact_field(revert, "snapshot", "revert-snapshot", &id);
        redact_field(revert, "diff", "revert-diff", &id);
    }
}

fn sanitize_message(message: &mut Value) {
    let Some(entry) = message.as_object_mut() else {
        return;
    };
    if let Some(info) = entry.get_mut("info").and_then(Value::as_object_mut) {
        let id = owned_id(info);
        if info.get("role").and_then(Value::as_str) == Some("user") {
            redact_field(info, "system", "system", &id);
            if let Some(summary) = info.get_mut("summary").and_then(Value::as_object_mut) {
                redact_field(summary, "title", "summary-title", &id);
                redact_field(summary, "body", "summary-body", &id);
                redact_diffs(summary, "message-diff");
            }
        } else if let Some(path) = info.get_mut("path").and_then(Value::as_object_mut) {
            redact_field(path, "cwd", "cwd", &id);
            redact_field(path, "root", "root", &id);
        }
    }
    if let Some(parts) = entry.get_mut("parts").and_then(Value::as_array_mut) {
        for part in parts {
            sanitize_part(part);
        }
    }
}

fn sanitize_part(part: &mut Value) {
    let Some(object) = part.as_object_mut() else {
        return;
    };
    let id = owned_id(object);
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" => {
            redact_field(object, "text", "text", &id);
            redact_data(object, "metadata", "text-metadata", &id);
        }
        "reasoning" => {
            redact_field(object, "text", "reasoning", &id);
            redact_data(object, "metadata", "reasoning-metadata", &id);
        }
        "file" => sanitize_file_part(object, &id),
        "subtask" => {
            redact_field(object, "prompt", "subtask-prompt", &id);
            redact_field(object, "description", "subtask-description", &id);
            redact_field(object, "command", "subtask-command", &id);
        }
        "tool" => {
            redact_data(object, "metadata", "tool-metadata", &id);
            sanitize_tool_state(object, &id);
        }
        "patch" => {
            redact_field(object, "hash", "patch", &id);
            if let Some(files) = object.get_mut("files").and_then(Value::as_array_mut) {
                for (index, file) in files.iter_mut().enumerate() {
                    redact_value(file, "patch-file", &format!("{id}-{index}"));
                }
            }
        }
        "snapshot" | "step-start" | "step-finish" => {
            redact_field(object, "snapshot", "snapshot", &id);
        }
        "agent" => {
            if let Some(source) = object.get_mut("source").and_then(Value::as_object_mut) {
                redact_field(source, "value", "agent-source", &id);
            }
        }
        _ => {}
    }
}

/// `filepart` (`export.ts:60-67`) plus the `source` switch it calls.
fn sanitize_file_part(object: &mut JsonMap<String, Value>, id: &str) {
    redact_field(object, "url", "file-url", id);
    redact_field(object, "filename", "file-name", id);
    let Some(source) = object.get_mut("source").and_then(Value::as_object_mut) else {
        return;
    };
    match source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "symbol" => {
            redact_field(source, "path", "file-path", id);
            redact_field(source, "name", "file-symbol", id);
        }
        "resource" => {
            redact_field(source, "clientName", "file-client", id);
            redact_field(source, "uri", "file-uri", id);
        }
        _ => redact_field(source, "path", "file-path", id),
    }
    if let Some(span) = source.get_mut("text").and_then(Value::as_object_mut) {
        redact_field(span, "value", "file-text", id);
    }
}

/// The four `state.status` arms of the `tool` case (`export.ts:96-123`).
fn sanitize_tool_state(object: &mut JsonMap<String, Value>, id: &str) {
    let Some(state) = object.get_mut("state").and_then(Value::as_object_mut) else {
        return;
    };
    redact_data(state, "input", "tool-input", id);
    let status = state
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match status.as_str() {
        "pending" => redact_field(state, "raw", "tool-raw", id),
        "running" => {
            redact_field(state, "title", "tool-title", id);
            redact_data(state, "metadata", "tool-state-metadata", id);
        }
        "completed" => {
            redact_field(state, "output", "tool-output", id);
            redact_field(state, "title", "tool-title", id);
            redact_data(state, "metadata", "tool-state-metadata", id);
            if let Some(attachments) = state.get_mut("attachments").and_then(Value::as_array_mut) {
                for attachment in attachments {
                    if let Some(object) = attachment.as_object_mut() {
                        let attachment_id = owned_id(object);
                        sanitize_file_part(object, &attachment_id);
                    }
                }
            }
        }
        _ => redact_data(state, "metadata", "tool-state-metadata", id),
    }
}

fn redact_diffs(summary: &mut JsonMap<String, Value>, kind: &str) {
    let Some(diffs) = summary.get_mut("diffs").and_then(Value::as_array_mut) else {
        return;
    };
    for (index, diff) in diffs.iter_mut().enumerate() {
        let Some(object) = diff.as_object_mut() else {
            continue;
        };
        let index = index.to_string();
        redact_field(object, "file", &format!("{kind}-file"), &index);
        redact_field(object, "patch", &format!("{kind}-patch"), &index);
    }
}

fn redact_field(object: &mut JsonMap<String, Value>, key: &str, kind: &str, id: &str) {
    if let Some(value) = object.get_mut(key) {
        redact_value(value, kind, id);
    }
}

/// `redact` (`export.ts:11-13`): an all-whitespace string is left alone.
fn redact_value(value: &mut Value, kind: &str, id: &str) {
    if let Value::String(text) = value
        && !text.trim().is_empty()
    {
        *value = Value::String(format!("[redacted:{kind}:{id}]"));
    }
}

/// `data` (`export.ts:15-18`): a container is replaced by a marker only when it
/// has at least one key, because `Object.keys({}).length` is `0`.
fn redact_data(object: &mut JsonMap<String, Value>, key: &str, kind: &str, id: &str) {
    let Some(value) = object.get_mut(key) else {
        return;
    };
    let keys = match &value {
        Value::Object(nested) => nested.len(),
        Value::Array(items) => items.len(),
        Value::String(text) => text.chars().count(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    };
    if keys > 0 {
        *value = Value::Object(
            [("redacted".to_owned(), Value::String(format!("{kind}:{id}")))]
                .into_iter()
                .collect(),
        );
    }
}

fn owned_id(object: &JsonMap<String, Value>) -> String {
    object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
