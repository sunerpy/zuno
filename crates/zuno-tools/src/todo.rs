//! `todowrite` — the session's task list, persisted to the `todo` table.
//!
//! # The registry key is not the wire id
//!
//! Upstream keys this tool `todo` in the registry (`registry.ts:214`) and names it
//! `todowrite` on the wire (`todo.ts:15`). [`WIRE_ID`] is the wire id, because that is
//! what the model emits, what [`zuno_tool::Tool::id`] returns, and what the permission
//! layer keys on. The registry key has no wire meaning and is not reproduced here.
//!
//! # Whole-list replacement, and why `position` is not decorative
//!
//! The table's primary key is `(session_id, position)`
//! (`crates/zuno-db/src/schema.rs:193`), so writing a list without clearing the old one
//! collides the moment the new list is no longer than the old. Upstream's
//! `Todo.update` (`packages/opencode/src/session/todo.ts:29-51`) therefore does the
//! only thing that works: inside **one** transaction, `DELETE` every row for the
//! session, then insert the new list with `position` set to each item's index in the
//! array. This port does the same, for the same reason.
//!
//! Three consequences follow from that and are asserted rather than assumed:
//!
//! - **`position` is the ordering, and the array is the source of it.** Read the rows
//!   back with `ORDER BY position` and you get the model's array order; nothing else
//!   in the row records it.
//! - **An empty list clears the session.** Not a no-op — upstream returns early
//!   *after* the delete (`todo.ts:34`), so `todos: []` is how a list is emptied.
//! - **One session's list never touches another's.** The delete is scoped by
//!   `session_id`, which comes from [`zuno_tool::ToolContext`], not from the arguments.
//!
//! # Status and priority are string enums, and this port enforces them
//!
//! The column type is `text` and the values are the strings documented on the
//! oracle's schema (`packages/schema/src/session-todo.ts:6-16`): `pending`,
//! `in_progress`, `completed`, `cancelled`, and `high`, `medium`, `low`.
//!
//! **Deliberate divergence.** Upstream declares both fields as `Schema.String` with
//! the allowed values only in the *description*, so `status: "banana"` and
//! `priority: 0` are accepted and written to the database verbatim. Here they are
//! [`TodoStatus`] and [`TodoPriority`], which means the derived schema advertises the
//! permitted values to the model and a bad value is refused as
//! [`zuno_error::ToolError::InvalidArgs`] instead of being persisted. The rejection
//! message names the allowed values — see [`TodoStatus`]'s deserializer — so the model
//! can correct the call. The prose each field carries is still upstream's, word for
//! word, so the description the model reads has not changed.

use crate::exposure::{ExposureFlags, exposes_todowrite};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::{self, Deserializer, Unexpected, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use zuno_db::Pool;
use zuno_error::ToolError;
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput, TypedTool};

/// The id the model calls.
///
/// Not `todo`: that is upstream's *registry key* (`registry.ts:214`), an internal
/// handle. The model, the permission layer and every transcript use `todowrite`.
pub const WIRE_ID: &str = "todowrite";

/// The permission this tool asks for before writing.
///
/// Oracle: `todo.ts:25` asks `{ permission: "todowrite", patterns: ["*"], always:
/// ["*"] }`. Keyed on the wire id, so a `todowrite` rule in a config reaches it.
pub const PERMISSION: &str = "todowrite";

/// The description the model reads, verbatim from `tool/todowrite.txt`.
///
/// Kept byte-identical to the oracle's file. The prompt-visible text is part of the
/// behaviour being ported: rewording it changes when a model reaches for the tool.
pub const DESCRIPTION: &str = include_str!("todo/todowrite.txt");

/// Where a written todo list goes.
///
/// A trait rather than a concrete [`SqliteTodoStore`] because the tool must be
/// testable without a database, and because a host that keeps the list somewhere else
/// — a server session with no local file, a dry run — should not need a different
/// tool. Synchronous on purpose: the only real implementation is a local SQLite write,
/// and [`TodoWriteTool`] moves it off the async executor with
/// [`tokio::task::spawn_blocking`] rather than making every implementation async to
/// suit one of them.
pub trait TodoStore: Send + Sync + 'static {
    /// Replace `session_id`'s entire list with `todos`, in the order given.
    ///
    /// Must be atomic: a caller that observes the list must see either the whole old
    /// list or the whole new one. `position` is the index in `todos`.
    ///
    /// # Errors
    ///
    /// [`TodoStoreError`] when the write could not be committed.
    fn replace(&self, session_id: &str, todos: &[TodoItem]) -> Result<(), TodoStoreError>;

    /// The session's list, ordered by `position`.
    ///
    /// # Errors
    ///
    /// [`TodoStoreError`] when the rows could not be read or decoded.
    fn list(&self, session_id: &str) -> Result<Vec<TodoItem>, TodoStoreError>;
}

/// A todo write that did not land.
///
/// A local `thiserror` enum in `#[source]` position, following the crate's existing
/// convention: [`ToolError`] has no `Other(String)`, so a storage failure has to be
/// classified into a named condition before it can be reported.
#[derive(Debug, thiserror::Error)]
pub enum TodoStoreError {
    /// The database rejected or could not commit the write.
    ///
    /// Carries [`zuno_error::DbError`], so a caller can tell a busy timeout from a
    /// foreign-key violation — the two failures a session-scoped write actually has.
    #[error("the todo list could not be written")]
    Database(#[source] Box<zuno_error::DbError>),

    /// A stored row held a status or priority outside the permitted set.
    ///
    /// Reachable only for rows this port did not write: the TypeScript binary accepts
    /// any string in those columns, so a database shared with it can contain values
    /// [`TodoStatus`] refuses. Named rather than silently coerced, because guessing
    /// which enum an unknown string meant would corrupt the user's list.
    #[error("stored todo row {position} has an unrecognized {field}: {value:?}")]
    UnknownValue {
        /// `status` or `priority`.
        field: &'static str,
        /// The value found in the column.
        value: String,
        /// The row's `position`.
        position: i64,
    },
}

impl From<zuno_error::DbError> for TodoStoreError {
    fn from(error: zuno_error::DbError) -> Self {
        Self::Database(Box::new(error))
    }
}

/// How far along one task is.
///
/// The four states from `tool/todowrite.txt` and
/// `packages/schema/src/session-todo.ts:9-11`, in the order that file lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Actively being worked on. Exactly one at a time, per the description.
    InProgress,
    /// Finished successfully.
    Completed,
    /// No longer needed.
    Cancelled,
}

/// How urgent one task is.
///
/// **Strings, never numbers.** `priority` is a `text` column
/// (`crates/zuno-db/src/schema.rs:191`) and the oracle documents `high`, `medium`,
/// `low`; a numeric priority is a wrong shape, not a shorthand, and is refused with a
/// message naming these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    /// Highest urgency.
    High,
    /// Default urgency.
    Medium,
    /// Lowest urgency.
    Low,
}

impl TodoStatus {
    /// Every permitted wire value, in the order the description lists them.
    pub const ALLOWED: [&'static str; 4] = ["pending", "in_progress", "completed", "cancelled"];

    /// The string written to the `status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a stored or wire value, without a `serde` error to build.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl TodoPriority {
    /// Every permitted wire value, in the order the description lists them.
    pub const ALLOWED: [&'static str; 3] = ["high", "medium", "low"];

    /// The string written to the `priority` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    /// Parses a stored or wire value, without a `serde` error to build.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// A visitor whose `expecting` line enumerates the permitted values.
///
/// The whole reason these two `Deserialize` impls are hand-written rather than
/// derived. `serde`'s derived enum decoder reports a *type* mismatch as
/// `invalid type: integer 0, expected variant identifier`, which tells a model
/// nothing it can act on. Routing every wrong shape through one `expecting` line
/// means the numeric case reads
/// `invalid type: integer 0, expected one of "high", "medium", "low"` — the values
/// named, which the plan requires and a derived impl cannot give.
struct EnumVisitor<T> {
    allowed: &'static [&'static str],
    parse: fn(&str) -> Option<T>,
}

impl<'de, T> Visitor<'de> for EnumVisitor<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("one of ")?;
        for (index, value) in self.allowed.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{value:?}")?;
        }
        Ok(())
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        (self.parse)(value).ok_or_else(|| E::unknown_variant(value, self.allowed))
    }

    // Numbers are the shape the plan calls out by name, so they get an explicit arm
    // rather than `Visitor`'s default — which would say "invalid type: integer 0"
    // against a *borrowed* expectation and, for `u64`, is the arm `serde_json` picks
    // for `0`. Both spellings are covered because `-1` arrives as `i64`.
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Err(E::invalid_type(Unexpected::Unsigned(value), &self))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Err(E::invalid_type(Unexpected::Signed(value), &self))
    }
}

impl<'de> Deserialize<'de> for TodoStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(EnumVisitor {
            allowed: &Self::ALLOWED,
            parse: Self::parse,
        })
    }
}

impl<'de> Deserialize<'de> for TodoPriority {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(EnumVisitor {
            allowed: &Self::ALLOWED,
            parse: Self::parse,
        })
    }
}

/// One task in the list.
///
/// Field order is the oracle's (`session-todo.ts:6-16`) because the tool's output is
/// `serde_json` of this struct and a reordered field is a changed transcript.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    /// Brief description of the task
    pub content: String,
    /// Current status of the task: pending, in_progress, completed, cancelled
    pub status: TodoStatus,
    /// Priority level of the task: high, medium, low
    pub priority: TodoPriority,
}

impl TodoItem {
    /// A task, for building a list in a caller or a test.
    #[must_use]
    pub fn new(content: impl Into<String>, status: TodoStatus, priority: TodoPriority) -> Self {
        Self {
            content: content.into(),
            status,
            priority,
        }
    }
}

/// Arguments to `todowrite`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteParams {
    /// The updated todo list
    pub todos: Vec<TodoItem>,
}

/// The `todo` table, through an [`zuno_db::Pool`].
///
/// # The foreign key is real
///
/// `todo.session_id` references `session(id)` `ON DELETE CASCADE`
/// (`crates/zuno-db/src/schema.rs:194`), and [`zuno_db`] issues `PRAGMA foreign_keys = ON`
/// on every connection it opens — deliberately, because the pragma's default varies by
/// SQLite build. So a write for a session that does not exist **fails** rather than
/// orphaning rows, and deleting a session takes its todos with it. Both are asserted
/// in `tests/conditional_tools.rs` rather than assumed from the DDL.
pub struct SqliteTodoStore {
    pool: Arc<Pool>,
}

impl SqliteTodoStore {
    /// A store over an already-open pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// The pool this store writes through.
    #[must_use]
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

impl TodoStore for SqliteTodoStore {
    fn replace(&self, session_id: &str, todos: &[TodoItem]) -> Result<(), TodoStoreError> {
        // One transaction for the delete and every insert, so no reader can observe
        // a half-replaced list — and so a failing insert cannot leave the session
        // with the old list deleted. `Pool::transaction` is IMMEDIATE, which is what
        // makes the busy timeout apply to a concurrent writer.
        let written = self.pool.transaction(|transaction| {
            transaction
                .execute("DELETE FROM `todo` WHERE `session_id` = ?1", [session_id])
                .map_err(zuno_db::map_error)?;

            // `time_created` and `time_updated` are `NOT NULL` with no SQL default;
            // upstream supplies both from `Date.now()` at insert time
            // (`core/src/database/schema.sql.ts:3-10`, where `$onUpdate` also fires on
            // insert). One timestamp for the whole batch, so a list written together
            // does not appear to have been written over several milliseconds.
            let now = zuno_db::message::now_millis();
            let mut insert = transaction
                .prepare(
                    "INSERT INTO `todo` \
                     (`session_id`, `content`, `status`, `priority`, `position`, `time_created`, `time_updated`) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(zuno_db::map_error)?;
            for (position, item) in todos.iter().enumerate() {
                let position = i64::try_from(position).unwrap_or(i64::MAX);
                insert
                    .execute(rusqlite::params![
                        session_id,
                        item.content,
                        item.status.as_str(),
                        item.priority.as_str(),
                        position,
                        now,
                        now,
                    ])
                    .map_err(zuno_db::map_error)?;
            }
            Ok(todos.len())
        })?;

        debug_assert_eq!(written, todos.len());
        Ok(())
    }

    fn list(&self, session_id: &str) -> Result<Vec<TodoItem>, TodoStoreError> {
        let connection = self.pool.get()?;
        // `ORDER BY position` is the only thing that recovers the model's array order;
        // SQLite makes no promise about row order without it, and the primary key
        // `(session_id, position)` means "sorted by insertion" is a coincidence rather
        // than a guarantee.
        let mut statement = connection
            .prepare(
                "SELECT `content`, `status`, `priority`, `position` FROM `todo` \
                 WHERE `session_id` = ?1 ORDER BY `position` ASC",
            )
            .map_err(zuno_db::map_error)?;
        let rows = statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(zuno_db::map_error)?;

        let mut items = Vec::new();
        for row in rows {
            let (content, status, priority, position) = row.map_err(zuno_db::map_error)?;
            items.push(TodoItem {
                content,
                status: TodoStatus::parse(&status).ok_or(TodoStoreError::UnknownValue {
                    field: "status",
                    value: status,
                    position,
                })?,
                priority: TodoPriority::parse(&priority).ok_or(TodoStoreError::UnknownValue {
                    field: "priority",
                    value: priority,
                    position,
                })?,
            });
        }
        Ok(items)
    }
}

/// An in-memory [`TodoStore`], for tests and for a host with nowhere to persist.
///
/// Keeps the same replace-the-whole-list contract as [`SqliteTodoStore`], so a test
/// that passes against this one is testing the tool rather than the store.
#[derive(Debug, Default)]
pub struct MemoryTodoStore {
    sessions: Mutex<Vec<(String, Vec<TodoItem>)>>,
}

impl MemoryTodoStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, Vec<TodoItem>)>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TodoStore for MemoryTodoStore {
    fn replace(&self, session_id: &str, todos: &[TodoItem]) -> Result<(), TodoStoreError> {
        let mut sessions = self.lock();
        match sessions.iter_mut().find(|(id, _)| id == session_id) {
            Some((_, existing)) => *existing = todos.to_vec(),
            None => sessions.push((session_id.to_owned(), todos.to_vec())),
        }
        Ok(())
    }

    fn list(&self, session_id: &str) -> Result<Vec<TodoItem>, TodoStoreError> {
        Ok(self
            .lock()
            .iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, todos)| todos.clone())
            .unwrap_or_default())
    }
}

/// Writes the session's task list.
pub struct TodoWriteTool {
    store: Arc<dyn TodoStore>,
}

impl TodoWriteTool {
    /// The tool, writing through `store`.
    #[must_use]
    pub fn new(store: Arc<dyn TodoStore>) -> Self {
        Self { store }
    }

    /// The tool over the `todo` table of `pool`.
    #[must_use]
    pub fn with_pool(pool: Arc<Pool>) -> Self {
        Self::new(Arc::new(SqliteTodoStore::new(pool)))
    }

    /// Whether the registry offers this tool under `flags`. Always.
    ///
    /// Delegates to [`exposes_todowrite`] so the tool and the registry cannot hold
    /// divergent copies of the condition.
    #[must_use]
    pub fn exposed_under(flags: &ExposureFlags) -> bool {
        exposes_todowrite(flags)
    }

    /// The title upstream renders for `todos`.
    ///
    /// Oracle: `todo.ts:37` counts the items whose status is **not** `completed`, so a
    /// finished list reads "0 todos" rather than its length. Singular is not special
    /// cased upstream either: one item reads "1 todos".
    #[must_use]
    pub fn title(todos: &[TodoItem]) -> String {
        let open = todos
            .iter()
            .filter(|item| item.status != TodoStatus::Completed)
            .count();
        format!("{open} todos")
    }
}

#[async_trait]
impl TypedTool for TodoWriteTool {
    type Params = TodoWriteParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(
        &self,
        params: TodoWriteParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        // Asked before the write, as upstream does (`todo.ts:24-29`), so a denied
        // rule cannot be observed as a partially written list.
        ctx.permission
            .ask(
                WIRE_ID,
                PermissionAsk {
                    permission: PERMISSION.to_owned(),
                    patterns: vec!["*".to_owned()],
                    metadata: Map::new(),
                    always: vec!["*".to_owned()],
                },
            )
            .await?;

        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id.clone();
        let todos = params.todos.clone();
        // SQLite is blocking. Running it on the async executor would stall every
        // other task on this worker for the duration of the transaction, including
        // the interrupt that is supposed to be able to cancel the turn.
        tokio::task::spawn_blocking(move || store.replace(&session_id, &todos))
            .await
            .map_err(|error| ToolError::Failed {
                tool: WIRE_ID.to_owned(),
                source: Box::new(error),
            })?
            .map_err(|error| ToolError::Failed {
                tool: WIRE_ID.to_owned(),
                source: Box::new(error),
            })?;

        // `serde_json::to_string_pretty` is two-space indented, the same as the
        // oracle's `JSON.stringify(todos, null, 2)` (`todo.ts:38`).
        let rendered =
            serde_json::to_string_pretty(&params.todos).map_err(|error| ToolError::Failed {
                tool: WIRE_ID.to_owned(),
                source: Box::new(error),
            })?;
        let metadata = serde_json::to_value(&params.todos).map_err(|error| ToolError::Failed {
            tool: WIRE_ID.to_owned(),
            source: Box::new(error),
        })?;

        Ok(ToolOutput::text(Self::title(&params.todos), rendered).with_metadata("todos", metadata))
    }
}

/// A [`ToolOutput`]'s `todos` metadata, decoded back into items.
///
/// The transcript renderer needs the list as data, not as the pretty-printed text, and
/// re-parsing the output string would be parsing this crate's own formatting.
///
/// # Errors
///
/// [`serde_json::Error`] when the value is not a list of todo items.
pub fn todos_from_metadata(
    metadata: &Map<String, Value>,
) -> Result<Vec<TodoItem>, serde_json::Error> {
    match metadata.get("todos") {
        Some(value) => serde_json::from_value(value.clone()),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exposure::ExposureFlags;
    use serde_json::json;
    use zuno_tool::{AllowAll, DenyAll, NeverInterrupted, Tool, erase};

    fn context(store_session: &str) -> ToolContext {
        ToolContext::new(
            store_session,
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    fn denying_context() -> ToolContext {
        ToolContext::new(
            "ses_todo",
            "msg_1",
            "call_1",
            "build",
            Arc::new(DenyAll),
            Arc::new(NeverInterrupted),
        )
    }

    fn tool() -> (Arc<MemoryTodoStore>, Arc<dyn Tool>) {
        let store = Arc::new(MemoryTodoStore::new());
        let erased = erase(TodoWriteTool::new(Arc::clone(&store) as Arc<dyn TodoStore>));
        (store, erased)
    }

    /// Renders an error and every cause in its chain.
    fn chain(error: &dyn std::error::Error) -> String {
        let mut rendered = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            rendered.push_str(" -> ");
            rendered.push_str(&cause.to_string());
            source = cause.source();
        }
        rendered
    }

    // --- exposure ---

    #[test]
    fn conditional_todowrite_is_offered_unconditionally() {
        assert!(TodoWriteTool::exposed_under(&ExposureFlags::default()));
        assert!(TodoWriteTool::exposed_under(
            &ExposureFlags::default().with_client("tui")
        ));
    }

    #[test]
    fn the_wire_id_is_todowrite_not_the_registry_key_todo() {
        let (_store, erased) = tool();
        assert_eq!(erased.id(), "todowrite");
        assert_ne!(erased.id(), "todo");
    }

    // --- the string enums ---

    #[test]
    fn every_status_round_trips_through_its_wire_string() {
        for value in TodoStatus::ALLOWED {
            let parsed = TodoStatus::parse(value).expect("a documented status");
            assert_eq!(parsed.as_str(), value);
            assert_eq!(
                serde_json::from_value::<TodoStatus>(json!(value)).expect("valid"),
                parsed
            );
        }
    }

    #[test]
    fn every_priority_round_trips_through_its_wire_string() {
        for value in TodoPriority::ALLOWED {
            let parsed = TodoPriority::parse(value).expect("a documented priority");
            assert_eq!(parsed.as_str(), value);
            assert_eq!(
                serde_json::from_value::<TodoPriority>(json!(value)).expect("valid"),
                parsed
            );
        }
    }

    #[test]
    fn a_numeric_priority_is_rejected_naming_the_allowed_strings() {
        let error = serde_json::from_value::<TodoPriority>(json!(0))
            .expect_err("0 is not a priority; the schema is string-valued");
        let message = error.to_string();

        assert!(
            message.contains("integer `0`") || message.contains("integer 0"),
            "the message must say what was received: {message}"
        );
        for allowed in TodoPriority::ALLOWED {
            assert!(
                message.contains(allowed),
                "the message must name {allowed}: {message}"
            );
        }
    }

    #[test]
    fn an_unknown_status_string_is_rejected_naming_the_allowed_strings() {
        let error = serde_json::from_value::<TodoStatus>(json!("banana"))
            .expect_err("banana is not a status");
        let message = error.to_string();

        for allowed in TodoStatus::ALLOWED {
            assert!(
                message.contains(allowed),
                "the message must name {allowed}: {message}"
            );
        }
    }

    #[test]
    fn the_schema_advertises_the_permitted_values_to_the_model() {
        // The deliberate divergence from upstream's bare `Schema.String`, pinned so
        // that losing it is a test failure rather than a quiet loosening.
        let (_store, erased) = tool();
        let schema = erased.definition().parameters;
        let rendered = schema.to_string();

        for allowed in TodoStatus::ALLOWED {
            assert!(
                rendered.contains(allowed),
                "the schema must offer {allowed}: {rendered}"
            );
        }
        for allowed in TodoPriority::ALLOWED {
            assert!(
                rendered.contains(allowed),
                "the schema must offer {allowed}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn a_numeric_priority_fails_the_call_with_a_correctable_error() {
        let (store, erased) = tool();
        let error = erased
            .execute(
                json!({ "todos": [{ "content": "c", "status": "pending", "priority": 0 }] }),
                context("ses_todo"),
            )
            .await
            .expect_err("a numeric priority must not be accepted");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert_eq!(error.tool(), "todowrite");
        assert!(error.is_model_correctable());

        let rendered = chain(&error);
        for allowed in TodoPriority::ALLOWED {
            assert!(
                rendered.contains(allowed),
                "the error chain must name {allowed}: {rendered}"
            );
        }
        assert!(
            store.list("ses_todo").expect("readable").is_empty(),
            "a rejected call must not have written anything"
        );
    }

    // --- writing ---

    #[tokio::test]
    async fn a_write_persists_the_list_in_array_order() {
        let (store, erased) = tool();
        erased
            .execute(
                json!({ "todos": [
                    { "content": "first",  "status": "in_progress", "priority": "high" },
                    { "content": "second", "status": "pending",     "priority": "medium" },
                    { "content": "third",  "status": "pending",     "priority": "low" },
                ] }),
                context("ses_todo"),
            )
            .await
            .expect("a valid list");

        let stored = store.list("ses_todo").expect("readable");
        let contents: Vec<&str> = stored.iter().map(|item| item.content.as_str()).collect();
        assert_eq!(contents, vec!["first", "second", "third"]);
        assert_eq!(stored[0].status, TodoStatus::InProgress);
        assert_eq!(stored[2].priority, TodoPriority::Low);
    }

    #[tokio::test]
    async fn a_second_write_replaces_the_list_rather_than_appending() {
        let (store, erased) = tool();
        erased
            .execute(
                json!({ "todos": [
                    { "content": "a", "status": "pending", "priority": "high" },
                    { "content": "b", "status": "pending", "priority": "high" },
                ] }),
                context("ses_todo"),
            )
            .await
            .expect("first write");
        erased
            .execute(
                json!({ "todos": [
                    { "content": "c", "status": "completed", "priority": "low" },
                ] }),
                context("ses_todo"),
            )
            .await
            .expect("second write");

        let stored = store.list("ses_todo").expect("readable");
        assert_eq!(stored.len(), 1, "the list is replaced, not merged");
        assert_eq!(stored[0].content, "c");
    }

    #[tokio::test]
    async fn an_empty_list_clears_the_session() {
        let (store, erased) = tool();
        erased
            .execute(
                json!({ "todos": [{ "content": "a", "status": "pending", "priority": "high" }] }),
                context("ses_todo"),
            )
            .await
            .expect("first write");
        erased
            .execute(json!({ "todos": [] }), context("ses_todo"))
            .await
            .expect("clearing write");

        assert!(store.list("ses_todo").expect("readable").is_empty());
    }

    #[tokio::test]
    async fn one_session_write_does_not_disturb_another() {
        let (store, erased) = tool();
        erased
            .execute(
                json!({ "todos": [{ "content": "mine", "status": "pending", "priority": "high" }] }),
                context("ses_a"),
            )
            .await
            .expect("write a");
        erased
            .execute(
                json!({ "todos": [{ "content": "yours", "status": "pending", "priority": "low" }] }),
                context("ses_b"),
            )
            .await
            .expect("write b");

        assert_eq!(store.list("ses_a").expect("readable")[0].content, "mine");
        assert_eq!(store.list("ses_b").expect("readable")[0].content, "yours");
    }

    // --- rendering ---

    #[test]
    fn the_title_counts_only_the_unfinished_items() {
        let todos = vec![
            TodoItem::new("a", TodoStatus::Completed, TodoPriority::High),
            TodoItem::new("b", TodoStatus::Pending, TodoPriority::High),
            TodoItem::new("c", TodoStatus::Cancelled, TodoPriority::Low),
        ];
        // `cancelled` is not `completed`, so upstream counts it as open.
        assert_eq!(TodoWriteTool::title(&todos), "2 todos");
        assert_eq!(TodoWriteTool::title(&[]), "0 todos");
    }

    #[tokio::test]
    async fn the_output_is_the_list_as_two_space_indented_json() {
        let (_store, erased) = tool();
        let output = erased
            .execute(
                json!({ "todos": [{ "content": "a", "status": "pending", "priority": "high" }] }),
                context("ses_todo"),
            )
            .await
            .expect("a valid list");

        assert_eq!(output.title, "1 todos");
        assert_eq!(
            output.output,
            "[\n  {\n    \"content\": \"a\",\n    \"status\": \"pending\",\n    \"priority\": \"high\"\n  }\n]"
        );
    }

    #[tokio::test]
    async fn the_metadata_carries_the_list_as_data() {
        let (_store, erased) = tool();
        let output = erased
            .execute(
                json!({ "todos": [
                    { "content": "a", "status": "in_progress", "priority": "high" },
                ] }),
                context("ses_todo"),
            )
            .await
            .expect("a valid list");

        let todos = todos_from_metadata(&output.metadata).expect("decodable");
        assert_eq!(
            todos,
            vec![TodoItem::new(
                "a",
                TodoStatus::InProgress,
                TodoPriority::High
            )]
        );
    }

    #[test]
    fn absent_metadata_decodes_to_an_empty_list() {
        assert!(
            todos_from_metadata(&Map::new())
                .expect("an absent key is not a failure")
                .is_empty()
        );
    }

    // --- permission ---

    #[tokio::test]
    async fn a_denied_permission_prevents_the_write() {
        let store = Arc::new(MemoryTodoStore::new());
        let erased = erase(TodoWriteTool::new(Arc::clone(&store) as Arc<dyn TodoStore>));

        let error = erased
            .execute(
                json!({ "todos": [{ "content": "a", "status": "pending", "priority": "high" }] }),
                denying_context(),
            )
            .await
            .expect_err("the permission layer refused");

        assert!(matches!(error, ToolError::Denied { .. }));
        assert!(store.list("ses_todo").expect("readable").is_empty());
    }

    #[test]
    fn the_description_is_the_oracles_file() {
        assert!(DESCRIPTION.starts_with(
            "Create and maintain a structured task list for the current coding session."
        ));
        for state in TodoStatus::ALLOWED {
            assert!(
                DESCRIPTION.contains(state),
                "the description must document {state}"
            );
        }
    }

    #[test]
    fn an_unknown_field_on_a_todo_is_rejected() {
        let error = serde_json::from_value::<TodoItem>(json!({
            "content": "a", "status": "pending", "priority": "high", "id": 7
        }))
        .expect_err("deny_unknown_fields");
        assert!(error.to_string().contains("id"));
    }
}
