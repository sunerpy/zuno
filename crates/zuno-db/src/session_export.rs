//! One session's full transcript in Zuno's export/import document.
//!
//! # Why this is a store concern and not a CLI concern
//!
//! Everything that makes the payload correct —
//! the field list, the message order, the part order, the identity keys the two
//! `data` blobs do not carry — belongs to whoever owns the rows. Putting the
//! envelope here means the CLI cannot invent a format and the format can be tested
//! without a process.
//!
//! # Redaction
//!
//! `--sanitize` is the flag a user reaches for before attaching a transcript to
//! a bug report, so partial redaction is a failure. An all-whitespace string and
//! an empty object remain structurally empty rather than being replaced with a
//! marker, while populated sensitive values are redacted recursively.
//!
//! # Import ownership
//!
//! `import` re-homes a transcript into the *current* project: the session's
//! project identifier, directory, and path are replaced. Messages and parts are
//! inserted `ON CONFLICT DO NOTHING`, so importing the same file twice is not a
//! way to mutate an existing transcript.

use base64::Engine as _;
use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::{Map as JsonMap, Value};
use zuno_error::DbError;

use crate::continuity;
use crate::message::{MessageRecord, MessageStore, PartRecord};
use crate::open;
use crate::session_list::{SessionInfo, session_info};

/// The table name reported when a document does not have the export shape.
const DOCUMENT: &str = "export document";

/// One message and its parts.
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

/// One session-and-Agent scoped working note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportNote {
    pub agent: String,
    pub name: String,
    pub revision: u64,
    pub content: String,
    pub content_sha256: String,
    pub time_created: i64,
    pub time_updated: i64,
}

/// One persisted idempotency result for a side-effecting note call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportNoteOperation {
    pub agent: String,
    pub call_id: String,
    pub request_sha256: String,
    pub action: String,
    pub name: String,
    pub result_revision: u64,
    pub result_content_sha256: String,
    pub time_created: i64,
}

/// A whole session as `zuno export` prints it.
///
/// `info` is a bare [`SessionInfo`], not the listing's project wrapper.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportDocument {
    /// The session itself.
    pub info: SessionInfo,
    /// Every message, oldest first.
    pub messages: Vec<ExportMessage>,
    /// Current note documents, grouped by the exported session envelope.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ExportNote>,
    /// Idempotency rows needed to preserve at-most-once note mutations.
    #[serde(rename = "noteOperations", skip_serializing_if = "Vec::is_empty")]
    pub note_operations: Vec<ExportNoteOperation>,
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
/// Messages are ordered by `(time_created, id)` through
/// [`MessageStore::hydrate_session`]. Export then sorts each message's parts by
/// id to keep the serialized document deterministic independently of runtime
/// transcript chronology.
///
/// # Errors
///
/// [`DbError::NotFound`] when no session has this id, and [`DbError::Query`] or
/// [`DbError::Decode`] if a row cannot be read.
pub fn export(connection: &Connection, session_id: &str) -> Result<ExportDocument, DbError> {
    let attachments = attachment_store_for_connection(connection)?;
    export_inner(connection, session_id, attachments.as_ref())
}

/// Export using the caller's exact database-scoped attachment store.
///
/// Durable image references are converted back to portable inline data URLs so
/// importing the document never depends on an object store from another database.
pub fn export_with_attachments(
    connection: &Connection,
    session_id: &str,
    attachments: &zuno_attachment::AttachmentStore,
) -> Result<ExportDocument, DbError> {
    export_inner(connection, session_id, Some(attachments))
}

fn export_inner(
    connection: &Connection,
    session_id: &str,
    attachments: Option<&zuno_attachment::AttachmentStore>,
) -> Result<ExportDocument, DbError> {
    let session = crate::session::get(connection, session_id)?;
    let hydrated = MessageStore::new(connection).hydrate_session(session_id)?;
    let mut messages = Vec::with_capacity(hydrated.len());
    for mut message in hydrated {
        message.parts.sort_by(|left, right| left.id.cmp(&right.id));
        let mut parts = Vec::with_capacity(message.parts.len());
        for part in &message.parts {
            let mut value = part.to_json();
            reinline_attachment(&mut value, attachments)?;
            parts.push(value);
        }
        messages.push(ExportMessage {
            info: message.info.to_json(),
            parts,
        });
    }
    Ok(ExportDocument {
        info: session_info(session),
        messages,
        notes: export_notes(connection, session_id)?,
        note_operations: export_note_operations(connection, session_id)?,
    })
}

fn reinline_attachment(
    part: &mut Value,
    attachments: Option<&zuno_attachment::AttachmentStore>,
) -> Result<(), DbError> {
    let Some(object) = part.as_object_mut() else {
        return Ok(());
    };
    let Some(reference) = object.get("attachment").cloned() else {
        return Ok(());
    };
    let reference = serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(reference)
        .map_err(|source| DbError::Decode {
            table: "part".to_owned(),
            source,
        })?;
    let store = attachments.ok_or_else(|| DbError::Query {
        source: Box::new(zuno_attachment::AttachmentError::StoreUnavailable),
    })?;
    let bytes = store.read(&reference).map_err(|source| DbError::Query {
        source: Box::new(source),
    })?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    object.insert(
        "mime".to_owned(),
        Value::String(reference.media_type.clone()),
    );
    object.insert("data".to_owned(), Value::String(data.clone()));
    object.insert(
        "url".to_owned(),
        Value::String(format!("data:{};base64,{data}", reference.media_type)),
    );
    object.remove("attachment");
    Ok(())
}

fn attachment_store_for_connection(
    connection: &Connection,
) -> Result<Option<zuno_attachment::AttachmentStore>, DbError> {
    let mut statement = connection
        .prepare("PRAGMA database_list")
        .map_err(open::map_error)?;
    let mut rows = statement.query([]).map_err(open::map_error)?;
    let mut target = None;
    while let Some(row) = rows.next().map_err(open::map_error)? {
        let name: String = row.get(1).map_err(open::map_error)?;
        if name == "main" {
            let path: String = row.get(2).map_err(open::map_error)?;
            if !path.is_empty() {
                target = Some(path);
            }
            break;
        }
    }
    target
        .map(|target| {
            zuno_attachment::AttachmentStore::new(
                zuno_paths::data(),
                &zuno_attachment::AttachmentStore::database_identity(target.as_bytes()),
                zuno_attachment::ImageAdmissionPolicy::default(),
            )
            .map_err(|source| DbError::Query {
                source: Box::new(source),
            })
        })
        .transpose()
}

fn export_notes(connection: &Connection, session_id: &str) -> Result<Vec<ExportNote>, DbError> {
    if !continuity::table_exists(connection, continuity::SESSION_NOTE_TABLE)? {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT agent, name, revision, content, content_sha256, time_created, time_updated
             FROM session_note WHERE session_id = ?1 ORDER BY agent ASC, name ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([session_id], |row| {
            let revision: i64 = row.get(2)?;
            Ok(ExportNote {
                agent: row.get(0)?,
                name: row.get(1)?,
                revision: u64::try_from(revision).unwrap_or(0),
                content: row.get(3)?,
                content_sha256: row.get(4)?,
                time_created: row.get(5)?,
                time_updated: row.get(6)?,
            })
        })
        .map_err(open::map_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(open::map_error)
}

fn export_note_operations(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<ExportNoteOperation>, DbError> {
    if !continuity::table_exists(connection, continuity::SESSION_NOTE_OPERATION_TABLE)? {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT agent, call_id, request_sha256, action, name, result_revision,
                    result_content_sha256, time_created
             FROM session_note_operation
             WHERE session_id = ?1 ORDER BY agent ASC, time_created ASC, call_id ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([session_id], |row| {
            let revision: i64 = row.get(5)?;
            Ok(ExportNoteOperation {
                agent: row.get(0)?,
                call_id: row.get(1)?,
                request_sha256: row.get(2)?,
                action: row.get(3)?,
                name: row.get(4)?,
                result_revision: u64::try_from(revision).unwrap_or(0),
                result_content_sha256: row.get(6)?,
                time_created: row.get(7)?,
            })
        })
        .map_err(open::map_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(open::map_error)
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
    /// How many note documents the document carried.
    pub notes: usize,
    /// How many note idempotency operations the document carried.
    pub note_operations: usize,
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
    let attachments = attachment_store_for_connection(transaction)?;
    import_inner(transaction, document, target, attachments.as_ref())
}

/// Import using the caller's exact database-scoped attachment store.
///
/// Inline image parts in portable documents are admitted before their rows are
/// written, so all newly persisted image content uses durable references.
pub fn import_with_attachments(
    transaction: &Transaction<'_>,
    document: &Value,
    target: &ImportTarget,
    attachments: &zuno_attachment::AttachmentStore,
) -> Result<Imported, DbError> {
    import_inner(transaction, document, target, Some(attachments))
}

fn import_inner(
    transaction: &Transaction<'_>,
    document: &Value,
    target: &ImportTarget,
    attachments: Option<&zuno_attachment::AttachmentStore>,
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
            let mut part = part.clone();
            admit_imported_image(&mut part, attachments)?;
            let created = part
                .get("time")
                .and_then(Value::as_object)
                .and_then(|time| time.get("start").or_else(|| time.get("created")))
                .and_then(Value::as_i64)
                .unwrap_or(record.time_created);
            let record = PartRecord::from_json(rehome(part, &session_id), created)?;
            store.insert_part_if_absent(&record)?;
            parts_written += 1;
        }
    }
    let notes = match root.get("notes") {
        None => &[][..],
        Some(Value::Array(notes)) => notes.as_slice(),
        Some(_) => return Err(decode_error("export field `notes` must be an array")),
    };
    let note_operations = match root.get("noteOperations") {
        None => &[][..],
        Some(Value::Array(operations)) => operations.as_slice(),
        Some(_) => {
            return Err(decode_error(
                "export field `noteOperations` must be an array",
            ));
        }
    };
    if !notes.is_empty() || !note_operations.is_empty() {
        continuity::ensure_schema(transaction)?;
    }
    for note in notes {
        import_note(transaction, &session_id, note)?;
    }
    for operation in note_operations {
        import_note_operation(transaction, &session_id, operation)?;
    }
    Ok(Imported {
        session_id,
        messages: messages.len(),
        parts: parts_written,
        notes: notes.len(),
        note_operations: note_operations.len(),
    })
}

fn admit_imported_image(
    part: &mut Value,
    attachments: Option<&zuno_attachment::AttachmentStore>,
) -> Result<(), DbError> {
    let Some(object) = part.as_object_mut() else {
        return Ok(());
    };
    if object.get("type").and_then(Value::as_str) != Some("file") {
        return Ok(());
    }
    if let Some(reference) = object.get("attachment").cloned() {
        let reference = serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(reference)
            .map_err(|source| DbError::Decode {
                table: "part".to_owned(),
                source,
            })?;
        let store = attachments.ok_or_else(|| DbError::Query {
            source: Box::new(zuno_attachment::AttachmentError::StoreUnavailable),
        })?;
        store.read(&reference).map_err(|source| DbError::Query {
            source: Box::new(source),
        })?;
        return Ok(());
    }

    let media_type = object
        .get("mime")
        .and_then(Value::as_str)
        .filter(|media_type| {
            matches!(
                *media_type,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
        .map(str::to_owned);
    let Some(media_type) = media_type else {
        return Ok(());
    };
    let data = object
        .get("data")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            object
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| url.strip_prefix(&format!("data:{media_type};base64,")))
                .map(str::to_owned)
        });
    let Some(data) = data else {
        return Ok(());
    };
    let store = attachments.ok_or_else(|| DbError::Query {
        source: Box::new(zuno_attachment::AttachmentError::StoreUnavailable),
    })?;
    let filename = object
        .get("filename")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let reference = store
        .admit_base64_typed(&data, Some(&media_type), filename)
        .map_err(|source| DbError::Query {
            source: Box::new(source),
        })?;
    match reference.filename.clone() {
        Some(filename) => {
            object.insert("filename".to_owned(), Value::String(filename));
        }
        None => {
            object.remove("filename");
        }
    }
    object.insert(
        "mime".to_owned(),
        Value::String(reference.media_type.clone()),
    );
    object.insert(
        "attachment".to_owned(),
        serde_json::to_value(reference).map_err(|source| DbError::Decode {
            table: "part".to_owned(),
            source,
        })?,
    );
    object.remove("data");
    object.remove("url");
    Ok(())
}

fn import_note(
    transaction: &Transaction<'_>,
    session_id: &str,
    value: &Value,
) -> Result<(), DbError> {
    let note = object(value, "export note")?;
    let agent = string_at(note, "agent", "export note")?;
    let name = string_at(note, "name", "export note")?;
    let revision = positive_number(note, "revision", "export note")?;
    let content = string_at(note, "content", "export note")?;
    continuity::validate_note_name(&name)
        .map_err(|error| decode_error(&format!("invalid export note name: {error}")))?;
    let content_bytes = content.len() as u64;
    if content_bytes > continuity::MAX_NOTE_DOCUMENT_BYTES {
        return Err(decode_error(&format!(
            "export note `{name}` is {content_bytes} bytes; the per-document limit is {}",
            continuity::MAX_NOTE_DOCUMENT_BYTES
        )));
    }
    let created = required_number(note, "timeCreated", "export note")?;
    let updated = required_number(note, "timeUpdated", "export note")?;
    let content_sha256 = zuno_orchestration::sha256_text(&content);
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM session_note
               WHERE session_id = ?1 AND agent = ?2 AND name = ?3
             )",
            params![session_id, agent, name],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if !exists {
        let (document_count, total_bytes): (i64, i64) = transaction
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(CAST(content AS BLOB))), 0)
                 FROM session_note WHERE session_id = ?1 AND agent = ?2",
                params![session_id, agent],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(open::map_error)?;
        if u64::try_from(document_count).unwrap_or(u64::MAX) >= continuity::MAX_NOTE_DOCUMENTS {
            return Err(decode_error(&format!(
                "export note scope `{agent}` exceeds the {}-document limit",
                continuity::MAX_NOTE_DOCUMENTS
            )));
        }
        let aggregate = u64::try_from(total_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(content_bytes);
        if aggregate > continuity::MAX_NOTE_SCOPE_BYTES {
            return Err(decode_error(&format!(
                "export note scope `{agent}` would contain {aggregate} bytes; the aggregate limit is {}",
                continuity::MAX_NOTE_SCOPE_BYTES
            )));
        }
    }
    transaction
        .execute(
            "INSERT INTO session_note
             (session_id, agent, name, revision, content, content_sha256,
              time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (session_id, agent, name) DO NOTHING",
            params![
                session_id,
                agent,
                name,
                revision,
                content,
                content_sha256,
                created,
                updated,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

fn import_note_operation(
    transaction: &Transaction<'_>,
    session_id: &str,
    value: &Value,
) -> Result<(), DbError> {
    let operation = object(value, "export note operation")?;
    let agent = string_at(operation, "agent", "export note operation")?;
    let call_id = string_at(operation, "callId", "export note operation")?;
    let request_sha256 = string_at(operation, "requestSha256", "export note operation")?;
    let action = string_at(operation, "action", "export note operation")?;
    if !matches!(action.as_str(), "append" | "write") {
        return Err(decode_error(
            "export note operation action must be `append` or `write`",
        ));
    }
    let name = string_at(operation, "name", "export note operation")?;
    continuity::validate_note_name(&name)
        .map_err(|error| decode_error(&format!("invalid export note operation name: {error}")))?;
    let result_revision = positive_number(operation, "resultRevision", "export note operation")?;
    let result_content_sha256 =
        string_at(operation, "resultContentSha256", "export note operation")?;
    let created = required_number(operation, "timeCreated", "export note operation")?;
    let current_revision: Option<i64> = transaction
        .query_row(
            "SELECT revision FROM session_note
             WHERE session_id = ?1 AND agent = ?2 AND name = ?3",
            params![session_id, agent, name],
            |row| row.get(0),
        )
        .optional()
        .map_err(open::map_error)?;
    match current_revision {
        Some(current) if result_revision <= current => {}
        Some(current) => {
            return Err(decode_error(&format!(
                "export note operation revision {result_revision} is newer than current note revision {current}"
            )));
        }
        None => {
            return Err(decode_error(
                "export note operation does not reference an imported note",
            ));
        }
    }
    transaction
        .execute(
            "INSERT INTO session_note_operation
             (session_id, agent, call_id, request_sha256, action, name,
              result_revision, result_content_sha256, time_created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (session_id, agent, call_id) DO NOTHING",
            params![
                session_id,
                agent,
                call_id,
                request_sha256,
                action,
                name,
                result_revision,
                result_content_sha256,
                created,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
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

fn string_at(
    object: &JsonMap<String, Value>,
    key: &str,
    description: &str,
) -> Result<String, DbError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            decode_error(&format!(
                "{description} field `{key}` is missing, empty, or not a string"
            ))
        })
}

fn required_number(
    object: &JsonMap<String, Value>,
    key: &str,
    description: &str,
) -> Result<i64, DbError> {
    object.get(key).and_then(Value::as_i64).ok_or_else(|| {
        decode_error(&format!(
            "{description} field `{key}` is missing or not an integer"
        ))
    })
}

fn positive_number(
    object: &JsonMap<String, Value>,
    key: &str,
    description: &str,
) -> Result<i64, DbError> {
    let value = required_number(object, key, description)?;
    if value >= 1 {
        Ok(value)
    } else {
        Err(decode_error(&format!(
            "{description} field `{key}` must be positive"
        )))
    }
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
    if let Some(notes) = root.get_mut("notes").and_then(Value::as_array_mut) {
        let mut redacted_agents = BTreeMap::<String, String>::new();
        for (index, note) in notes.iter_mut().enumerate() {
            let original_agent = note
                .as_object()
                .and_then(|note| note.get("agent"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let next_ordinal = redacted_agents.len() + 1;
            let redacted_agent = redacted_agents
                .entry(original_agent)
                .or_insert_with(|| {
                    if next_ordinal == 1 {
                        "redacted-agent".to_owned()
                    } else {
                        format!("redacted-agent-{next_ordinal}")
                    }
                })
                .clone();
            sanitize_note(note, index, &redacted_agent);
        }
    }
    if let Some(operations) = root.get_mut("noteOperations").and_then(Value::as_array_mut) {
        operations.clear();
    }
    document
}

fn sanitize_note(note: &mut Value, index: usize, agent: &str) {
    let Some(note) = note.as_object_mut() else {
        return;
    };
    let name = format!("redacted-note-{index}.md");
    let content = format!("[redacted:note-content:{index}]");
    note.insert("agent".to_owned(), Value::String(agent.to_owned()));
    note.insert("name".to_owned(), Value::String(name));
    note.insert("content".to_owned(), Value::String(content.clone()));
    note.insert(
        "contentSha256".to_owned(),
        Value::String(zuno_orchestration::sha256_text(&content)),
    );
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
