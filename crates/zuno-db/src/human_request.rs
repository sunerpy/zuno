//! Durable human-input and permission requests shared by every client surface.

use crate::event_log::query_error;
use crate::inbox::{InputDelivery, NewSessionInput, SessionInput, admit_in};
use crate::{Pool, open};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use zuno_error::DbError;

const TABLE: &str = "human_request";
const COLUMNS: &str = "id, session_id, goal_id, kind, state, payload, response, message_id, \
    call_id, revision, time_created, time_updated, time_resolved";

/// The interaction a client must present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanRequestKind {
    /// A model-authored question or Goal blocker requiring user input.
    Input,
    /// Approval for a permission-gated effect.
    Permission,
}

impl HumanRequestKind {
    /// Stable SQLite and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Permission => "permission",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "input" => Ok(Self::Input),
            "permission" => Ok(Self::Permission),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown human request kind `{value}`"
            )))),
        }
    }
}

/// Durable lifecycle of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanRequestState {
    /// A client may claim and answer the request.
    Pending,
    /// The user supplied an authoritative response.
    Answered,
    /// The user deliberately dismissed the request.
    Cancelled,
    /// The request passed its configured deadline.
    Expired,
    /// Delivery or response processing failed.
    Failed,
}

impl HumanRequestState {
    /// Stable SQLite and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "pending" => Ok(Self::Pending),
            "answered" => Ok(Self::Answered),
            "cancelled" => Ok(Self::Cancelled),
            "expired" => Ok(Self::Expired),
            "failed" => Ok(Self::Failed),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown human request state `{value}`"
            )))),
        }
    }

    /// Whether no further client reply may change the request.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One request before it is committed.
#[derive(Debug, Clone, PartialEq)]
pub struct NewHumanRequest {
    pub id: String,
    pub session_id: String,
    pub goal_id: Option<String>,
    pub kind: HumanRequestKind,
    pub payload: Value,
    pub message_id: Option<String>,
    pub call_id: Option<String>,
    pub time_created: i64,
}

/// One durable request snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "goalID", skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    pub kind: HumanRequestKind,
    pub state: HumanRequestState,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(rename = "callID", skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub revision: i64,
    pub time_created: i64,
    pub time_updated: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_resolved: Option<i64>,
}

/// Shared durable request store.
#[derive(Debug, Clone)]
pub struct HumanRequestStore {
    pool: Arc<Pool>,
}

impl HumanRequestStore {
    /// Attach to an initialized application pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Persist one pending request.
    pub fn create(&self, request: NewHumanRequest) -> Result<HumanRequest, DbError> {
        self.pool
            .transaction(|transaction| create_in(transaction, &request))
    }

    /// Read one request by id.
    pub fn get(&self, request_id: &str) -> Result<Option<HumanRequest>, DbError> {
        let connection = self.pool.get()?;
        get_from(&connection, request_id)
    }

    /// Pending requests in deterministic creation order.
    pub fn pending(&self, session_id: Option<&str>) -> Result<Vec<HumanRequest>, DbError> {
        let connection = self.pool.get()?;
        pending_from(&connection, session_id)
    }

    /// Number of pending requests tied to one Goal instance.
    pub fn pending_for_goal(&self, session_id: &str, goal_id: &str) -> Result<usize, DbError> {
        let connection = self.pool.get()?;
        let count = connection
            .query_row(
                "SELECT COUNT(*) FROM human_request \
                 WHERE session_id = ?1 AND goal_id = ?2 AND state = 'pending'",
                params![session_id, goal_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(open::map_error)?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    /// Settle a request without creating model-visible input.
    pub fn resolve(
        &self,
        request_id: &str,
        state: HumanRequestState,
        response: Option<&Value>,
        now: i64,
    ) -> Result<Option<HumanRequest>, DbError> {
        self.pool
            .transaction(|transaction| resolve_in(transaction, request_id, state, response, now))
    }

    /// Answer a request and admit the exact response to the durable FIFO inbox.
    ///
    /// The request cannot become `answered` without the model-visible input also
    /// committing. Goal resumption is intentionally a later idempotent step: a
    /// crash between the two leaves the Goal safely paused with its answer durable.
    pub fn answer_with_input(
        &self,
        request_id: &str,
        response: Value,
        now: i64,
    ) -> Result<Option<(HumanRequest, SessionInput)>, DbError> {
        self.pool.transaction(|transaction| {
            let Some(current) = get_from(transaction, request_id)? else {
                return Ok(None);
            };
            if current.state != HumanRequestState::Pending {
                return Ok(None);
            }
            let prompt = response_prompt(&current, &response);
            let input = admit_in(
                transaction,
                NewSessionInput::new(
                    format!("human_{}", current.id),
                    current.session_id.clone(),
                    prompt,
                    InputDelivery::Queue,
                    now,
                ),
            )?;
            let resolved = resolve_in(
                transaction,
                request_id,
                HumanRequestState::Answered,
                Some(&response),
                now,
            )?
            .expect("the pending request was read in this transaction");
            Ok(Some((resolved, input)))
        })
    }
}

/// Stable model-visible input produced when a request is answered after its
/// originating turn is no longer live.
#[must_use]
pub fn response_prompt(request: &HumanRequest, response: &Value) -> Value {
    let request_json = serde_json::to_string_pretty(&request.payload)
        .unwrap_or_else(|_| request.payload.to_string());
    let response_json =
        serde_json::to_string_pretty(response).unwrap_or_else(|_| response.to_string());
    serde_json::json!({
        "kind": "humanRequestAnswer",
        "requestID": request.id,
        "humanRequestKind": request.kind,
        "text": format!(
            "A human answered durable request `{}`.\n\nRequest:\n{}\n\nResponse:\n{}",
            request.id, request_json, response_json
        ),
        "request": request.payload,
        "response": response,
    })
}

/// Insert a request inside a wider application transaction.
pub fn create_in(
    transaction: &Transaction<'_>,
    request: &NewHumanRequest,
) -> Result<HumanRequest, DbError> {
    validate_new(request)?;
    let payload = serde_json::to_string(&request.payload).map_err(decode_error)?;
    let changed = transaction
        .execute(
            "INSERT INTO human_request \
             (id, session_id, goal_id, kind, state, payload, response, message_id, call_id, \
              revision, time_created, time_updated, time_resolved) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, NULL, ?6, ?7, 1, ?8, ?8, NULL)",
            params![
                request.id,
                request.session_id,
                request.goal_id,
                request.kind.as_str(),
                payload,
                request.message_id,
                request.call_id,
                request.time_created,
            ],
        )
        .map_err(open::map_error)?;
    debug_assert_eq!(changed, 1);
    get_from(transaction, &request.id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: request.id.clone(),
    })
}

/// Settle a request inside a wider application transaction.
pub fn resolve_in(
    transaction: &Transaction<'_>,
    request_id: &str,
    state: HumanRequestState,
    response: Option<&Value>,
    now: i64,
) -> Result<Option<HumanRequest>, DbError> {
    if !state.is_terminal() {
        return Err(query_error(std::io::Error::other(
            "a human request may only resolve to a terminal state",
        )));
    }
    let response = response
        .map(serde_json::to_string)
        .transpose()
        .map_err(decode_error)?;
    transaction
        .execute(
            "UPDATE human_request \
             SET state = ?1, response = ?2, revision = revision + 1, \
                 time_updated = ?3, time_resolved = ?3 \
             WHERE id = ?4 AND state = 'pending'",
            params![state.as_str(), response, now, request_id],
        )
        .map_err(open::map_error)?;
    get_from(transaction, request_id)
}

/// Read one request through either a connection or transaction.
pub fn get_from(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<HumanRequest>, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM {TABLE} WHERE id = ?1"),
            params![request_id],
            from_row,
        )
        .optional()
        .map_err(open::map_error)?
        .transpose()
}

fn pending_from(
    connection: &Connection,
    session_id: Option<&str>,
) -> Result<Vec<HumanRequest>, DbError> {
    let (sql, value): (&str, Option<&str>) = match session_id {
        Some(session_id) => (
            "SELECT id, session_id, goal_id, kind, state, payload, response, message_id, \
             call_id, revision, time_created, time_updated, time_resolved \
             FROM human_request WHERE state = 'pending' AND session_id = ?1 \
             ORDER BY time_created, id",
            Some(session_id),
        ),
        None => (
            "SELECT id, session_id, goal_id, kind, state, payload, response, message_id, \
             call_id, revision, time_created, time_updated, time_resolved \
             FROM human_request WHERE state = 'pending' ORDER BY time_created, id",
            None,
        ),
    };
    let mut statement = connection.prepare(sql).map_err(open::map_error)?;
    let mapped = match value {
        Some(value) => statement.query_map(params![value], from_row),
        None => statement.query_map([], from_row),
    }
    .map_err(open::map_error)?;
    mapped
        .map(|request| request.map_err(open::map_error).and_then(|request| request))
        .collect()
}

fn from_row(row: &Row<'_>) -> Result<Result<HumanRequest, DbError>, rusqlite::Error> {
    let kind: String = row.get("kind")?;
    let state: String = row.get("state")?;
    let payload: String = row.get("payload")?;
    let response: Option<String> = row.get("response")?;
    Ok((|| {
        Ok(HumanRequest {
            id: row.get("id").map_err(open::map_error)?,
            session_id: row.get("session_id").map_err(open::map_error)?,
            goal_id: row.get("goal_id").map_err(open::map_error)?,
            kind: HumanRequestKind::parse(&kind)?,
            state: HumanRequestState::parse(&state)?,
            payload: serde_json::from_str(&payload).map_err(decode_error)?,
            response: response
                .map(|value| serde_json::from_str(&value).map_err(decode_error))
                .transpose()?,
            message_id: row.get("message_id").map_err(open::map_error)?,
            call_id: row.get("call_id").map_err(open::map_error)?,
            revision: row.get("revision").map_err(open::map_error)?,
            time_created: row.get("time_created").map_err(open::map_error)?,
            time_updated: row.get("time_updated").map_err(open::map_error)?,
            time_resolved: row.get("time_resolved").map_err(open::map_error)?,
        })
    })())
}

fn validate_new(request: &NewHumanRequest) -> Result<(), DbError> {
    if request.id.trim().is_empty() || request.session_id.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "human request id and session id must not be empty",
        )));
    }
    Ok(())
}

fn decode_error(error: serde_json::Error) -> DbError {
    DbError::Decode {
        table: TABLE.to_owned(),
        source: error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::SessionInbox;
    use crate::migration;
    use zuno_paths::DbLocation;

    fn store() -> (Arc<Pool>, HumanRequestStore) {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open pool"));
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        drop(connection);
        (Arc::clone(&pool), HumanRequestStore::new(pool))
    }

    fn request() -> NewHumanRequest {
        NewHumanRequest {
            id: "req_1".to_owned(),
            session_id: "ses_1".to_owned(),
            goal_id: Some("goal_1".to_owned()),
            kind: HumanRequestKind::Input,
            payload: serde_json::json!({"question":"Which region?"}),
            message_id: Some("msg_1".to_owned()),
            call_id: Some("call_1".to_owned()),
            time_created: 10,
        }
    }

    #[test]
    fn request_and_answer_survive_reopening_the_store() {
        let (pool, store) = store();
        store.create(request()).expect("create");
        let reopened = HumanRequestStore::new(pool);
        let pending = reopened.pending(Some("ses_1")).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, HumanRequestState::Pending);

        let answered = reopened
            .resolve(
                "req_1",
                HumanRequestState::Answered,
                Some(&serde_json::json!({"answer":"cn-northwest-1"})),
                20,
            )
            .expect("resolve")
            .expect("request exists");
        assert_eq!(answered.state, HumanRequestState::Answered);
        assert!(reopened.pending(Some("ses_1")).expect("pending").is_empty());
    }

    #[test]
    fn answering_and_admitting_model_visible_input_are_atomic() {
        let (pool, store) = store();
        let connection = pool.get().expect("connection");
        connection
            .execute(
                "INSERT INTO project \
                 (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
                  time_updated,time_initialized,sandboxes,commands) \
                 VALUES ('prj','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
                [],
            )
            .expect("project");
        connection
            .execute(
                "INSERT INTO session \
                 (id,project_id,slug,directory,title,version,time_created,time_updated) \
                 VALUES ('ses_1','prj','ses_1','/tmp','session','test',1,1)",
                [],
            )
            .expect("session");
        drop(connection);
        store.create(request()).expect("create");

        let (answered, input) = store
            .answer_with_input("req_1", serde_json::json!({"answer":"cn-northwest-1"}), 20)
            .expect("answer")
            .expect("pending request");
        assert_eq!(answered.state, HumanRequestState::Answered);
        assert_eq!(input.id, "human_req_1");
        assert_eq!(input.prompt["kind"], "humanRequestAnswer");
        assert_eq!(input.prompt["requestID"], "req_1");
        assert_eq!(
            SessionInbox::new(pool)
                .pending("ses_1")
                .expect("pending inbox")
                .len(),
            1
        );
        assert!(
            store
                .answer_with_input("req_1", serde_json::json!({"answer":"duplicate"}), 30)
                .expect("idempotent second answer")
                .is_none(),
            "a settled request must not admit a second model-visible input"
        );
    }
}
