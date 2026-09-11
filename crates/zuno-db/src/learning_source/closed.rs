use super::{
    LearningSource, LearningSourceStore, MAX_SOURCE_READ_BYTES, MAX_SOURCES, MAX_TOTAL_BYTES,
    digest,
};
use crate::{Connection, message::MessageStore, open};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use zuno_error::DbError;

/// Host-validated closure and the exact bounded manifest stored with one job.
/// The marker alone is never authority; claims revalidate it against SQLite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningSourceSnapshot {
    pub version: u32,
    pub project_id: String,
    pub session_id: String,
    pub source_message_id: String,
    pub start_message_id: String,
    pub completed_at: i64,
    pub end_digest: String,
    pub manifest_digest: String,
    pub session_wide: bool,
    pub allow_busy: bool,
}

pub(crate) struct ClosedTurn {
    pub project_id: String,
    pub start: (i64, String),
    pub end: (i64, String),
    pub completed_at: i64,
    pub end_digest: String,
}

fn not_closed(id: &str) -> DbError {
    DbError::Conflict {
        table: "message".to_owned(),
        id: id.to_owned(),
        detail: "learning requires a successful closed assistant turn without pending tools"
            .to_owned(),
    }
}

pub(crate) fn closed_turn_on(
    connection: &Connection,
    session_id: &str,
    end_message_id: &str,
) -> Result<ClosedTurn, DbError> {
    let row = connection
        .query_row(
            "SELECT s.project_id,m.data,json_extract(m.data,'$.time.completed')
             FROM message m JOIN session s ON s.id=m.session_id
             WHERE m.session_id=?1 AND m.id=?2
               AND length(CAST(m.data AS BLOB))<=?3
               AND json_extract(m.data,'$.role')='assistant'
               AND json_extract(m.data,'$.finish')='stop'
               AND json_type(m.data,'$.time.completed')='integer'
               AND json_extract(m.data,'$.time.completed')>=m.time_created
               AND json_extract(m.data,'$.error') IS NULL
               AND COALESCE(json_extract(m.data,'$.summary'),0)=0",
            params![session_id, end_message_id, MAX_SOURCE_READ_BYTES],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                ))
            },
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| not_closed(end_message_id))?;
    let (start, end) =
        MessageStore::new(connection).completed_turn_bounds(session_id, end_message_id)?;
    let pending: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM part p JOIN message m
                 ON m.id=p.message_id AND m.session_id=p.session_id
               WHERE m.session_id=?1 AND (m.time_created,m.id)>=(?2,?3)
                 AND (m.time_created,m.id)<=(?4,?5)
                 AND json_extract(p.data,'$.type')='tool'
                 AND COALESCE(json_extract(p.data,'$.state.status'),'')
                   NOT IN ('completed','error')
             )",
            params![session_id, start.0, start.1, end.0, end.1],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if pending {
        return Err(not_closed(end_message_id));
    }
    Ok(ClosedTurn {
        project_id: row.0,
        start,
        end,
        completed_at: row.2,
        end_digest: digest(&row.1),
    })
}

#[must_use]
pub fn source_manifest_digest(sources: &[LearningSource]) -> String {
    digest(&serde_json::to_string(sources).expect("source manifests serialize"))
}

impl LearningSourceStore {
    pub fn snapshot(
        &self,
        project_id: &str,
        session_id: &str,
        end_message_id: &str,
        sources: &[LearningSource],
        session_wide: bool,
        allow_busy: bool,
    ) -> Result<LearningSourceSnapshot, DbError> {
        let connection = self.pool.get()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(open::map_error)?;
        let turn = closed_turn_on(&transaction, session_id, end_message_id)?;
        let snapshot = LearningSourceSnapshot {
            version: 1,
            project_id: project_id.to_owned(),
            session_id: session_id.to_owned(),
            source_message_id: end_message_id.to_owned(),
            start_message_id: turn.start.1.clone(),
            completed_at: turn.completed_at,
            end_digest: turn.end_digest.clone(),
            manifest_digest: source_manifest_digest(sources),
            session_wide,
            allow_busy,
        };
        if !manifest_current_on(&transaction, &snapshot, sources, &turn)? {
            return Err(not_closed(end_message_id));
        }
        Ok(snapshot)
    }

    /// Resolve only an assistant message owned by this project.
    pub fn session_for_message(
        &self,
        project_id: &str,
        message_id: &str,
    ) -> Result<Option<String>, DbError> {
        self.pool
            .get()?
            .query_row(
                "SELECT m.session_id FROM message m JOIN session s ON s.id=m.session_id
             WHERE m.id=?1 AND s.project_id=?2 AND json_extract(m.data,'$.role')='assistant'",
                params![message_id, project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(open::map_error)
    }
}

pub(crate) fn snapshot_current_on(
    connection: &Connection,
    snapshot: &LearningSourceSnapshot,
    sources: &[LearningSource],
) -> Result<bool, DbError> {
    let turn = match closed_turn_on(
        connection,
        &snapshot.session_id,
        &snapshot.source_message_id,
    ) {
        Ok(turn) => turn,
        Err(DbError::Conflict { .. } | DbError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error),
    };
    manifest_current_on(connection, snapshot, sources, &turn)
}

fn manifest_current_on(
    connection: &Connection,
    snapshot: &LearningSourceSnapshot,
    sources: &[LearningSource],
    turn: &ClosedTurn,
) -> Result<bool, DbError> {
    if snapshot.version != 1
        || snapshot.project_id != turn.project_id
        || snapshot.source_message_id != turn.end.1
        || snapshot.start_message_id != turn.start.1
        || snapshot.completed_at != turn.completed_at
        || snapshot.end_digest != turn.end_digest
        || snapshot.manifest_digest != source_manifest_digest(sources)
        || sources.is_empty()
        || sources.len() > MAX_SOURCES
    {
        return Ok(false);
    }
    let mut bytes = 0_usize;
    let mut references = BTreeSet::new();
    for source in sources {
        bytes = bytes
            .saturating_add(source.content.len())
            .saturating_add(source.arguments.as_ref().map_or(0, String::len));
        if bytes > MAX_TOTAL_BYTES
            || !references.insert(&source.reference_id)
            || !LearningSourceStore::source_is_current_on(connection, &snapshot.session_id, source)?
        {
            return Ok(false);
        }
        let belongs: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM message
             WHERE session_id=?1 AND id=?2 AND (time_created,id)<=(?3,?4)
               AND (?5 OR (time_created,id)>=(?6,?7)))",
                params![
                    snapshot.session_id,
                    source.message_id,
                    turn.end.0,
                    turn.end.1,
                    snapshot.session_wide,
                    turn.start.0,
                    turn.start.1
                ],
                |row| row.get(0),
            )
            .map_err(open::map_error)?;
        if !belongs {
            return Ok(false);
        }
    }
    Ok(true)
}
