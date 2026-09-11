//! Bounded, source-addressed evidence for isolated learning requests.

mod closed;

pub use closed::{LearningSourceSnapshot, source_manifest_digest};
pub(crate) use closed::{closed_turn_on, snapshot_current_on};

use crate::{Pool, open};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use zuno_error::DbError;

const MAX_SOURCES: usize = 256;
const MAX_SOURCE_BYTES: usize = 8_192;
const MAX_TOTAL_BYTES: usize = 128 * 1_024;
const MAX_SOURCE_READ_BYTES: i64 = 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSourceKind {
    Message,
    Tool,
    Artifact,
    User,
    Feedback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSourceField {
    Text,
    Output,
    Error,
    Artifact,
    Feedback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSource {
    pub reference_id: String,
    pub source_id: String,
    pub message_id: String,
    pub kind: LearningSourceKind,
    pub field: LearningSourceField,
    /// Digest before secret redaction, used only to detect durable source drift.
    pub source_digest: String,
    /// Digest of the exact bounded, redacted content supplied to the extractor.
    pub content_digest: String,
    pub content: String,
    pub tool: Option<String>,
    pub arguments: Option<String>,
    pub proves_success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSourceWindow {
    pub sources: Vec<LearningSource>,
    pub truncated: bool,
    pub external_context: bool,
}

#[derive(Clone)]
pub struct LearningSourceStore {
    pool: Arc<Pool>,
}

impl LearningSourceStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Prioritize human input and assistant conclusions, then restore source order.
    pub fn for_turn(
        &self,
        session_id: &str,
        end_message_id: &str,
        redact: &dyn Fn(&str) -> String,
    ) -> Result<LearningSourceWindow, DbError> {
        self.collect(session_id, end_message_id, false, redact)
    }

    pub fn for_session(
        &self,
        session_id: &str,
        end_message_id: &str,
        redact: &dyn Fn(&str) -> String,
    ) -> Result<LearningSourceWindow, DbError> {
        self.collect(session_id, end_message_id, true, redact)
    }

    fn collect(
        &self,
        session_id: &str,
        end_message_id: &str,
        session_wide: bool,
        redact: &dyn Fn(&str) -> String,
    ) -> Result<LearningSourceWindow, DbError> {
        let connection = self.pool.get()?;
        // Closure, source selection, and receipt reads observe one SQLite snapshot.
        let transaction = connection
            .unchecked_transaction()
            .map_err(open::map_error)?;
        let connection = &transaction;
        let turn = closed_turn_on(connection, session_id, end_message_id)?;
        let (mut start, end) = (turn.start, turn.end);
        if session_wide {
            start = (i64::MIN, String::new());
        }
        let mut statement = connection
            .prepare(
                "SELECT p.id, p.message_id, json_extract(m.data, '$.role'),
                    json_extract(p.data, '$.type'),
                    COALESCE(json_extract(p.data, '$.text'), ''),
                    COALESCE(json_extract(p.data, '$.state.output'), ''),
                    COALESCE(json_extract(p.data, '$.state.error'), ''),
                    json_extract(p.data, '$.tool'),
                    CAST(json_extract(p.data, '$.state.input') AS TEXT),
                    EXISTS (
                      SELECT 1 FROM verification_receipt v
                      WHERE v.session_id = m.session_id
                        AND v.tool_call_id = json_extract(p.data, '$.callID')
                        AND v.outcome = 'passed' AND v.exit_authority = 'authoritative'
                        AND v.exit_code=0 AND v.tool_id=json_extract(p.data,'$.tool')
                        AND json_extract(m.data,'$.role')='assistant'
                        AND json_extract(p.data,'$.state.status')='completed'
                    ),
                    m.time_created, p.time_created,
                    json_object('type', json_extract(p.data, '$.type'),
                                'hash', json_extract(p.data, '$.hash'),
                                'files', json_extract(p.data, '$.files'),
                                'snapshot', json_extract(p.data, '$.snapshot'),
                                'filename', json_extract(p.data, '$.filename'),
                                'mime', json_extract(p.data, '$.mime'),
                                'url', json_extract(p.data, '$.url')),
                    p.data
             FROM message m JOIN part p ON p.message_id = m.id AND p.session_id = m.session_id
             WHERE m.session_id = ?1 AND (m.time_created, m.id) >= (?2, ?3)
               AND (m.time_created, m.id) <= (?4, ?5)
               AND length(CAST(p.data AS BLOB)) <= ?6
               AND json_extract(p.data, '$.type') IN ('text','tool','file','patch','snapshot')
             ORDER BY CASE
               WHEN json_extract(m.data, '$.role') = 'user' THEN 0
               WHEN json_extract(p.data, '$.type') = 'text' THEN 1
               ELSE 2 END,
               m.time_created DESC, m.id DESC, p.time_created DESC, p.id DESC
             LIMIT ?7",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    session_id,
                    start.0,
                    start.1,
                    end.0,
                    end.1,
                    MAX_SOURCE_READ_BYTES,
                    (MAX_SOURCES + 1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, bool>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, String>(13)?,
                    ))
                },
            )
            .map_err(open::map_error)?;
        let mut selected = Vec::new();
        let mut used = 0_usize;
        let mut truncated: bool = connection
            .query_row(
                "SELECT EXISTS(
               SELECT 1 FROM message m JOIN part p ON p.message_id = m.id
               WHERE m.session_id = ?1 AND (m.time_created, m.id) >= (?2, ?3)
                 AND (m.time_created, m.id) <= (?4, ?5)
                 AND length(CAST(p.data AS BLOB)) > ?6
             )",
                rusqlite::params![
                    session_id,
                    start.0,
                    start.1,
                    end.0,
                    end.1,
                    MAX_SOURCE_READ_BYTES
                ],
                |row| row.get(0),
            )
            .map_err(open::map_error)?;
        for (ordinal, row) in rows.enumerate() {
            let row = row.map_err(open::map_error)?;
            if ordinal >= MAX_SOURCES {
                truncated = true;
                break;
            }
            let (kind, field, raw) = match (row.2.as_str(), row.3.as_str()) {
                ("user", "text") => (LearningSourceKind::User, LearningSourceField::Text, row.4),
                ("assistant", "text") => (
                    LearningSourceKind::Message,
                    LearningSourceField::Text,
                    row.4,
                ),
                ("assistant", "tool") if !row.5.is_empty() => {
                    (LearningSourceKind::Tool, LearningSourceField::Output, row.5)
                }
                ("assistant", "tool") => {
                    (LearningSourceKind::Tool, LearningSourceField::Error, row.6)
                }
                (_, "file" | "patch" | "snapshot") => (
                    LearningSourceKind::Artifact,
                    LearningSourceField::Artifact,
                    row.12,
                ),
                _ => continue,
            };
            if raw.trim().is_empty() {
                continue;
            }
            // Redact complete bounded source values before clipping; clipping a
            // credential first could expose a prefix the redactor no longer matches.
            let redacted = redact(&raw);
            let content = bounded_text(&redacted, MAX_SOURCE_BYTES).to_owned();
            let arguments = row.8.and_then(|value| {
                let redacted = redact(&value);
                if redacted.len() > MAX_SOURCE_BYTES {
                    truncated = true;
                    None
                } else {
                    Some(redacted)
                }
            });
            let cost = content
                .len()
                .saturating_add(arguments.as_ref().map_or(0, String::len));
            if used.saturating_add(cost) > MAX_TOTAL_BYTES {
                truncated = true;
                continue;
            }
            truncated |= content.len() < redacted.len();
            used = used.saturating_add(cost);
            let digest = digest(&content);
            selected.push((
                (row.10, row.1.clone(), row.11, row.0.clone()),
                LearningSource {
                    reference_id: format!("part:{}", row.0),
                    source_id: row.0,
                    message_id: row.1,
                    kind,
                    field,
                    source_digest: self::digest(&row.13),
                    content_digest: digest,
                    content,
                    tool: row.7.map(|tool| redact(&tool)),
                    arguments,
                    proves_success: kind == LearningSourceKind::Tool
                        && field == LearningSourceField::Output
                        && row.9,
                },
            ));
        }
        let mut feedback = connection.prepare(
            "SELECT f.message_id, f.revision,
                json_object('rating', f.rating, 'note', f.note, 'revision', f.revision), m.time_created
             FROM message_feedback f JOIN message m ON m.id = f.message_id AND m.session_id = f.session_id
             WHERE f.session_id = ?1 AND (m.time_created,m.id) >= (?2,?3)
               AND (m.time_created,m.id) <= (?4,?5) AND length(CAST(COALESCE(f.note,'') AS BLOB)) <= ?6
             ORDER BY m.time_created DESC,m.id DESC LIMIT 64"
        ).map_err(open::map_error)?;
        let rows = feedback
            .query_map(
                rusqlite::params![
                    session_id,
                    start.0,
                    start.1,
                    end.0,
                    end.1,
                    MAX_SOURCE_READ_BYTES
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(open::map_error)?;
        for row in rows {
            let (message_id, revision, raw, created) = row.map_err(open::map_error)?;
            let content = bounded_text(&redact(&raw), MAX_SOURCE_BYTES).to_owned();
            if selected.len() == MAX_SOURCES || used.saturating_add(content.len()) > MAX_TOTAL_BYTES
            {
                truncated = true;
                break;
            }
            used += content.len();
            let reference = format!("feedback:{message_id}:{revision}");
            selected.push((
                (created, message_id.clone(), created, reference.clone()),
                LearningSource {
                    reference_id: reference,
                    source_id: message_id.clone(),
                    message_id,
                    kind: LearningSourceKind::Feedback,
                    field: LearningSourceField::Feedback,
                    source_digest: digest(&raw),
                    content_digest: digest(&content),
                    content,
                    tool: None,
                    arguments: None,
                    proves_success: false,
                },
            ));
        }
        selected.sort_by(|(left, _), (right, _)| left.cmp(right));
        let external_context = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM part p WHERE p.session_id=?1
               AND json_extract(p.data,'$.state.status')='completed'
               AND json_extract(p.data,'$.state.metadata.externalContext')=1)",
                [session_id],
                |row| row.get(0),
            )
            .map_err(open::map_error)?;
        Ok(LearningSourceWindow {
            sources: selected.into_iter().map(|(_, source)| source).collect(),
            truncated,
            external_context,
        })
    }

    pub fn latest_assistant(&self, session_id: &str) -> Result<Option<String>, DbError> {
        use rusqlite::OptionalExtension as _;
        self.pool.get()?.query_row(
            "SELECT id FROM message WHERE session_id=?1 AND json_extract(data,'$.role')='assistant'
             AND json_extract(data,'$.finish')='stop'
             AND json_type(data,'$.time.completed')='integer'
             AND json_extract(data,'$.error') IS NULL
             AND COALESCE(json_extract(data,'$.summary'),0)=0
             ORDER BY time_created DESC,id DESC LIMIT 1",
            [session_id],|row|row.get(0),
        ).optional().map_err(open::map_error)
    }

    /// A bounded startup scan, excluding any turn already admitted by an earlier process/version.
    pub fn unlearned_turns(
        &self,
        project_id: &str,
        since: i64,
    ) -> Result<Vec<(String, String, i64)>, DbError> {
        let connection = self.pool.get()?;
        let mut query=connection.prepare(
            "SELECT m.session_id,m.id,CAST(json_extract(m.data,'$.time.completed') AS INTEGER)
             FROM message m JOIN session s ON s.id=m.session_id
             WHERE s.project_id=?1 AND m.time_created>=?2
               AND json_extract(m.data,'$.role')='assistant' AND json_extract(m.data,'$.finish')='stop'
               AND json_type(m.data,'$.time.completed')='integer'
               AND json_extract(m.data,'$.error') IS NULL
               AND COALESCE(json_extract(m.data,'$.summary'),0)=0
               AND NOT EXISTS(SELECT 1 FROM learning_job j WHERE j.session_id=m.session_id
                   AND j.source_message_id=m.id AND j.kind='extraction')
               AND NOT EXISTS(SELECT 1 FROM session_memory_policy p WHERE p.session_id=m.session_id
                   AND p.generation<>'enabled')
             ORDER BY m.time_created DESC,m.id DESC LIMIT 64"
        ).map_err(open::map_error)?;
        query
            .query_map(rusqlite::params![project_id, since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(open::map_error)?
            .collect::<Result<_, _>>()
            .map_err(open::map_error)
    }

    /// Revalidate the persisted source address and bytes before accepting a citation.
    pub fn source_is_current(
        &self,
        session_id: &str,
        source: &LearningSource,
    ) -> Result<bool, DbError> {
        let connection = self.pool.get()?;
        Self::source_is_current_on(&connection, session_id, source)
    }

    /// Reuse source validation inside the transaction that commits derived memory.
    pub(crate) fn source_is_current_on(
        connection: &rusqlite::Connection,
        session_id: &str,
        source: &LearningSource,
    ) -> Result<bool, DbError> {
        if source.content.len() > MAX_SOURCE_BYTES
            || source.reference_id.len() > 512
            || source.source_id.len() > 256
            || source.message_id.len() > 256
        {
            return Ok(false);
        }
        if source.field == LearningSourceField::Feedback {
            use rusqlite::OptionalExtension as _;
            let row = connection
                .query_row(
                    "SELECT revision,json_object('rating',rating,'note',note,'revision',revision)
                 FROM message_feedback WHERE message_id=?1 AND session_id=?2
                   AND length(CAST(COALESCE(note,'') AS BLOB))<=?3",
                    rusqlite::params![source.source_id, session_id, MAX_SOURCE_READ_BYTES],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(open::map_error)?;
            return Ok(row.is_some_and(|(revision, raw)| {
                source.kind == LearningSourceKind::Feedback
                    && !source.proves_success
                    && source.message_id == source.source_id
                    && source.reference_id == format!("feedback:{}:{revision}", source.source_id)
                    && digest(&raw) == source.source_digest
                    && digest(&source.content) == source.content_digest
            }));
        }
        let selector = match source.field {
            LearningSourceField::Text => "COALESCE(json_extract(p.data, '$.text'), '')",
            LearningSourceField::Output => "COALESCE(json_extract(p.data, '$.state.output'), '')",
            LearningSourceField::Error => "COALESCE(json_extract(p.data, '$.state.error'), '')",
            LearningSourceField::Artifact => {
                "json_object('type', json_extract(p.data, '$.type'), 'hash', json_extract(p.data, '$.hash'), 'files', json_extract(p.data, '$.files'), 'snapshot', json_extract(p.data, '$.snapshot'), 'filename', json_extract(p.data, '$.filename'), 'mime', json_extract(p.data, '$.mime'), 'url', json_extract(p.data, '$.url'))"
            }
            LearningSourceField::Feedback => unreachable!("feedback checked above"),
        };
        let mut statement = connection
            .prepare(&format!(
                "SELECT {selector},
                    json_extract(m.data, '$.role'), json_extract(p.data, '$.type'),
                    EXISTS (
                      SELECT 1 FROM verification_receipt v
                      WHERE v.session_id = m.session_id
                        AND v.tool_call_id = json_extract(p.data, '$.callID')
                        AND v.outcome = 'passed' AND v.exit_authority = 'authoritative'
                        AND v.exit_code=0 AND v.tool_id=json_extract(p.data,'$.tool')
                        AND json_extract(m.data,'$.role')='assistant'
                        AND json_extract(p.data,'$.state.status')='completed'
                    ), p.data
             FROM part p JOIN message m ON m.id = p.message_id AND m.session_id = p.session_id
             WHERE p.id = ?1 AND p.message_id = ?2 AND p.session_id = ?3
               AND length(CAST(p.data AS BLOB)) <= ?4"
            ))
            .map_err(open::map_error)?;
        let mut rows = statement
            .query((
                &source.source_id,
                &source.message_id,
                session_id,
                MAX_SOURCE_READ_BYTES,
            ))
            .map_err(open::map_error)?;
        let Some(row) = rows.next().map_err(open::map_error)? else {
            return Ok(false);
        };
        let _raw: String = row.get(0).map_err(open::map_error)?;
        let role: String = row.get(1).map_err(open::map_error)?;
        let kind: String = row.get(2).map_err(open::map_error)?;
        let success: bool = row.get(3).map_err(open::map_error)?;
        let raw_part: String = row.get(4).map_err(open::map_error)?;
        let valid_kind = matches!(
            (source.kind, source.field, role.as_str(), kind.as_str()),
            (
                LearningSourceKind::User,
                LearningSourceField::Text,
                "user",
                "text"
            ) | (
                LearningSourceKind::Message,
                LearningSourceField::Text,
                "assistant",
                "text"
            ) | (
                LearningSourceKind::Tool,
                LearningSourceField::Output | LearningSourceField::Error,
                "assistant",
                "tool"
            ) | (
                LearningSourceKind::Artifact,
                LearningSourceField::Artifact,
                _,
                "file" | "patch" | "snapshot"
            )
        );
        Ok(source.reference_id == format!("part:{}", source.source_id)
            && valid_kind
            && source.proves_success
                == (source.kind == LearningSourceKind::Tool
                    && source.field == LearningSourceField::Output
                    && success)
            && digest(&raw_part) == source.source_digest
            && digest(&source.content) == source.content_digest)
    }

    pub fn source_belongs_to_request(
        &self,
        project_id: &str,
        session_id: &str,
        end_message_id: &str,
        source: &LearningSource,
    ) -> Result<bool, DbError> {
        self.pool
            .get()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session s
               JOIN message m ON m.session_id=s.id
               JOIN message e ON e.session_id=s.id
               WHERE s.id=?1 AND s.project_id=?2 AND m.id=?3 AND e.id=?4
                 AND (m.time_created,m.id)<=(e.time_created,e.id))",
                rusqlite::params![session_id, project_id, source.message_id, end_message_id],
                |row| row.get(0),
            )
            .map_err(open::map_error)
    }
}

#[must_use]
pub fn bounded_text(value: &str, maximum_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(value.len().min(maximum_bytes))]
}

#[must_use]
pub fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
