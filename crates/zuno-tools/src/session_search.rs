//! Pure-SQL recall over archived sessions.
//!
//! Mode is inferred from arguments: `query` discovers ranked sessions,
//! `session_id` plus `around_message_id` scrolls around an anchor, and no mode
//! arguments browses recent root sessions. No provider abstraction is accepted
//! or called; every response is assembled from SQLite rows.

use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use zuno_db::fts;
use zuno_db::{Connection, open};
use zuno_error::{DbError, ToolError};
use zuno_tool::{ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

const DEFAULT_LIMIT: u32 = 3;
const MAX_LIMIT: u32 = 10;
const DISCOVERY_SCAN_LIMIT: u32 = 300;
const DEFAULT_WINDOW: u32 = 5;
const MAX_WINDOW: u32 = 20;
const BOOKEND_SIZE: u32 = 3;

const MESSAGE_CONTENT: &str = r#"
COALESCE(group_concat(
  CASE json_extract(p.data, '$.type')
    WHEN 'text' THEN json_extract(p.data, '$.text')
    WHEN 'reasoning' THEN json_extract(p.data, '$.text')
    WHEN 'subtask' THEN trim(
      COALESCE(json_extract(p.data, '$.description'), '') || ' ' ||
      COALESCE(json_extract(p.data, '$.prompt'), '')
    )
    WHEN 'tool' THEN trim(
      COALESCE(json_extract(p.data, '$.tool'), '') || ' ' ||
      COALESCE(json_extract(p.data, '$.state.title'), '') || ' ' ||
      COALESCE(json_extract(p.data, '$.state.output'), '')
    )
    ELSE NULL
  END,
  char(10)
), '')
"#;

/// Arguments shared by discovery, scroll, and browse modes.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchParams {
    /// Full-text query. Set only for discovery mode.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum sessions returned by discovery or browse (default 3, maximum 10).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Session containing the scroll anchor. Must be paired with around_message_id.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Message to center scroll mode on. Must be paired with session_id.
    #[serde(default)]
    pub around_message_id: Option<String>,
    /// Messages on each side of a scroll anchor (default 5, maximum 20).
    #[serde(default)]
    pub window: Option<u32>,
}

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/session-search.txt");

/// FTS-backed session history tool over one opencode database file.
#[derive(Debug, Clone)]
pub struct SessionSearchTool {
    database: PathBuf,
}

impl SessionSearchTool {
    /// Bind the tool to an opencode database path.
    #[must_use]
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
        }
    }

    /// The database path this instance opens for each call.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.database
    }
}

#[async_trait]
impl TypedTool for SessionSearchTool {
    type Params = SessionSearchParams;

    fn id(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    async fn run(
        &self,
        params: SessionSearchParams,
        _ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let mode = Mode::from_params(&params).map_err(|source| ToolError::InvalidArgs {
            tool: self.id().to_owned(),
            source: Box::new(source),
        })?;
        if !self.database.is_file() {
            return Err(failed(
                self.id(),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("database does not exist: {}", self.database.display()),
                ),
            ));
        }
        let mut connection =
            open::open_at(&self.database).map_err(|error| failed(self.id(), error))?;
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let response = match &mode {
            Mode::Browse => browse(&connection, limit),
            Mode::Discovery(query) => {
                fts::ensure(&mut connection).map_err(|error| failed(self.id(), error))?;
                discover(&connection, query, limit)
            }
            Mode::Scroll {
                session_id,
                message_id,
            } => scroll(
                &connection,
                session_id,
                message_id,
                params.window.unwrap_or(DEFAULT_WINDOW).clamp(1, MAX_WINDOW),
            ),
        }
        .map_err(|error| failed(self.id(), error))?;
        let title = match mode {
            Mode::Browse => "recent sessions".to_owned(),
            Mode::Discovery(query) => query,
            Mode::Scroll { session_id, .. } => session_id,
        };
        serde_json::to_string(&response)
            .map(|output| ToolOutput::text(title, output))
            .map_err(|error| failed(self.id(), error))
    }
}

#[derive(Debug, Clone)]
enum Mode {
    Browse,
    Discovery(String),
    Scroll {
        session_id: String,
        message_id: String,
    },
}

impl Mode {
    fn from_params(params: &SessionSearchParams) -> Result<Self, InvalidMode> {
        let query = non_empty(params.query.as_deref());
        let session_id = non_empty(params.session_id.as_deref());
        let message_id = non_empty(params.around_message_id.as_deref());
        match (query, session_id, message_id, params.window) {
            (None, None, None, None) => Ok(Self::Browse),
            (Some(query), None, None, None) => Ok(Self::Discovery(query.to_owned())),
            (None, Some(session_id), Some(message_id), _) => Ok(Self::Scroll {
                session_id: session_id.to_owned(),
                message_id: message_id.to_owned(),
            }),
            _ => Err(InvalidMode),
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Debug, thiserror::Error)]
#[error(
    "use exactly one mode: query for discovery, session_id with around_message_id for scroll, or no mode arguments for browse"
)]
struct InvalidMode;

#[derive(Debug, Serialize)]
struct MessageView {
    id: String,
    role: String,
    content: String,
    timestamp: i64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    anchor: bool,
}

#[derive(Debug)]
struct Window {
    messages: Vec<MessageView>,
    before: i64,
    after: i64,
}

fn browse(connection: &Connection, limit: u32) -> Result<Value, DbError> {
    let preview_sql = format!(
        "SELECT {MESSAGE_CONTENT} \
         FROM message AS m \
         LEFT JOIN part AS p ON p.message_id = m.id \
         WHERE m.session_id = s.id \
         GROUP BY m.id, m.time_created \
         ORDER BY m.time_created ASC, m.id ASC \
         LIMIT 1"
    );
    let sql = format!(
        "SELECT s.id, s.title, s.time_created, s.time_updated, \
                (SELECT count(*) FROM message WHERE session_id = s.id), \
                COALESCE(({preview_sql}), '') \
         FROM session AS s \
         WHERE s.parent_id IS NULL \
         ORDER BY s.time_updated DESC, s.id DESC \
         LIMIT ?1"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map([limit], |row| {
            Ok(json!({
                "session_id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "started_at": row.get::<_, i64>(2)?,
                "last_active": row.get::<_, i64>(3)?,
                "message_count": row.get::<_, i64>(4)?,
                "preview": row.get::<_, String>(5)?,
            }))
        })
        .map_err(open::map_error)?;
    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(open::map_error)?);
    }
    Ok(json!({
        "success": true,
        "mode": "browse",
        "count": results.len(),
        "results": results,
    }))
}

fn discover(connection: &Connection, query: &str, limit: u32) -> Result<Value, DbError> {
    let hits = fts::search(connection, query, DISCOVERY_SCAN_LIMIT)?;
    let mut classified = Vec::new();
    for hit in hits {
        let session = session_meta(connection, &hit.session_id)?;
        if let Some(session) = session {
            classified.push((session.parent_id.is_some(), hit, session));
        }
    }
    classified.sort_by_key(|(demoted, _, _)| *demoted);

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for (_, hit, session) in classified {
        if results.len() >= limit as usize {
            break;
        }
        if !seen.insert(hit.session_id.clone()) {
            continue;
        }
        let window = message_window(connection, &hit.session_id, &hit.message_id, DEFAULT_WINDOW)?;
        let bookend_start = bookend(connection, &hit.session_id, true)?;
        let bookend_end = bookend(connection, &hit.session_id, false)?;
        results.push(json!({
            "session_id": hit.session_id,
            "parent_session_id": session.parent_id,
            "title": session.title,
            "when": session.time_created,
            "matched_role": hit.role,
            "match_message_id": hit.message_id,
            "snippet": hit.snippet,
            "bookend_start": bookend_start,
            "messages": window.messages,
            "bookend_end": bookend_end,
            "messages_before": window.before,
            "messages_after": window.after,
        }));
    }
    Ok(json!({
        "success": true,
        "mode": "discovery",
        "query": query,
        "count": results.len(),
        "results": results,
    }))
}

fn scroll(
    connection: &Connection,
    session_id: &str,
    message_id: &str,
    radius: u32,
) -> Result<Value, DbError> {
    if session_meta(connection, session_id)?.is_none() {
        return Err(DbError::NotFound {
            table: "session".to_owned(),
            id: session_id.to_owned(),
        });
    }
    let window = message_window(connection, session_id, message_id, radius)?;
    if window.messages.is_empty() {
        return Err(DbError::NotFound {
            table: "message".to_owned(),
            id: message_id.to_owned(),
        });
    }
    Ok(json!({
        "success": true,
        "mode": "scroll",
        "session_id": session_id,
        "around_message_id": message_id,
        "window": radius,
        "count": window.messages.len(),
        "messages": window.messages,
        "messages_before": window.before,
        "messages_after": window.after,
    }))
}

#[derive(Debug)]
struct SessionMeta {
    title: String,
    parent_id: Option<String>,
    time_created: i64,
}

fn session_meta(connection: &Connection, session_id: &str) -> Result<Option<SessionMeta>, DbError> {
    connection
        .query_row(
            "SELECT title, parent_id, time_created FROM session WHERE id = ?1",
            [session_id],
            |row| {
                Ok(SessionMeta {
                    title: row.get(0)?,
                    parent_id: row.get(1)?,
                    time_created: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(open::map_error)
}

fn message_window(
    connection: &Connection,
    session_id: &str,
    anchor_id: &str,
    radius: u32,
) -> Result<Window, DbError> {
    let sql = format!(
        "WITH content AS ( \
           SELECT m.id, json_extract(m.data, '$.role') AS role, \
                  m.time_created, {MESSAGE_CONTENT} AS content \
           FROM message AS m \
           LEFT JOIN part AS p ON p.message_id = m.id \
           WHERE m.session_id = ?1 \
           GROUP BY m.id, m.data, m.time_created \
         ), ordered AS ( \
           SELECT *, row_number() OVER (ORDER BY time_created ASC, id ASC) AS position, \
                  count(*) OVER () AS total \
           FROM content \
         ), anchor AS ( \
           SELECT position FROM ordered WHERE id = ?2 \
         ) \
         SELECT ordered.id, ordered.role, ordered.content, ordered.time_created, \
                ordered.position, ordered.total \
         FROM ordered, anchor \
         WHERE ordered.position BETWEEN anchor.position - ?3 AND anchor.position + ?3 \
         ORDER BY ordered.position ASC"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params![session_id, anchor_id, radius], |row| {
            Ok((
                MessageView {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    timestamp: row.get(3)?,
                    anchor: row.get::<_, String>(0)? == anchor_id,
                },
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(open::map_error)?;
    let mut messages = Vec::new();
    let mut before = 0;
    let mut after = 0;
    for row in rows {
        let (message, position, total) = row.map_err(open::map_error)?;
        if message.anchor {
            before = position - 1;
            after = total - position;
        }
        messages.push(message);
    }
    Ok(Window {
        messages,
        before,
        after,
    })
}

fn bookend(
    connection: &Connection,
    session_id: &str,
    ascending: bool,
) -> Result<Vec<MessageView>, DbError> {
    let direction = if ascending { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT m.id, json_extract(m.data, '$.role'), {MESSAGE_CONTENT}, m.time_created \
         FROM message AS m \
         LEFT JOIN part AS p ON p.message_id = m.id \
         WHERE m.session_id = ?1 \
         GROUP BY m.id, m.data, m.time_created \
         ORDER BY m.time_created {direction}, m.id {direction} \
         LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params![session_id, BOOKEND_SIZE], |row| {
            Ok(MessageView {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                timestamp: row.get(3)?,
                anchor: false,
            })
        })
        .map_err(open::map_error)?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row.map_err(open::map_error)?);
    }
    if !ascending {
        messages.reverse();
    }
    Ok(messages)
}

fn failed(tool: &str, source: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(source),
    }
}

#[cfg(test)]
mod replay_policy_tests {
    use super::*;

    #[test]
    fn session_recall_is_safe_to_repeat() {
        assert_eq!(
            SessionSearchTool::new("/unused").replay_policy(),
            ToolReplayPolicy::Safe
        );
    }
}
