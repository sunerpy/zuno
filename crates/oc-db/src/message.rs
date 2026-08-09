//! Message and part persistence, and the batched hydration that reassembles a
//! conversation out of them.
//!
//! # The contract this module exists to hold
//!
//! `message.data` and `part.data` are opaque JSON to SQLite and load-bearing to
//! the TypeScript binary. `sql.ts:19-20` states their shape as a subtraction:
//!
//! ```text
//! type V1MessageData = Omit<SessionV1.Info, "id" | "sessionID">
//! type V1PartData    = Omit<SessionV1.Part, "id" | "sessionID" | "messageID">
//! ```
//!
//! Those subtracted keys are not absent from the record — they live in real
//! columns, which is what the indices are built on. Writing them *also* into the
//! blob is the failure this module is shaped to prevent: nothing rejects it, the
//! schema still matches, and the duplicate silently becomes a second source of
//! truth that can disagree with the column. So the strip is performed once, in
//! [`MessageRecord::from_json`] and [`PartRecord::from_json`], and the inverse is
//! performed once, in [`MessageRecord::to_json`] and [`PartRecord::to_json`] —
//! mirroring `messageData`/`partData` in `projector.ts:78-88` and
//! `info`/`part` in `message-v2.ts:80-93` respectively.
//!
//! # Twelve variants, not nine
//!
//! The `Part` union at `packages/schema/src/v1/session.ts:357-370` has twelve
//! members. Nine of them (`text`, `reasoning`, `tool`, `step-start`,
//! `step-finish`, `patch`, `file`, `compaction`, `subtask`) are the ones a
//! long-running install actually accumulates; `snapshot`, `agent` and `retry`
//! are declared by the schema and emitted by the engine but were absent from a
//! 1,035,733-part census of a real `opencode.db`. All twelve are accepted here,
//! because a part the engine can emit and this crate cannot store is a session
//! that stops round-tripping the moment a user hits that path.
//!
//! # An unknown variant is an error, never a shrug
//!
//! [`PartKind`] carries no catch-all and [`PartKind::from_tag`] returns `None`
//! for anything outside the union, which the decode path turns into
//! [`DbError::Decode`] naming the offending tag. Dropping the part instead would
//! produce a message that renders wrong — a tool call with no result, a step with
//! no finish — with nothing in the logs to explain it. The same applies to
//! [`MessageRole`].
//!
//! # Hydration is two statements, not N+1
//!
//! [`MessageStore::hydrate`] follows `message-v2.ts:98-123`: collect the message
//! ids, fetch every part for that set in one `IN (...)` pass, group by
//! `message_id`, then zip. One statement for the messages and one per
//! [`HYDRATION_CHUNK`] of ids for the parts — so 500 messages cost two
//! statements, not 501. [`MessageStore::query_count`] reports the real number
//! because every statement in this module is prepared through one private
//! choke point that increments it.
//!
//! ```
//! use oc_db::message::{MessageRecord, MessageStore, PartRecord};
//! use oc_db::{migration, open};
//! use serde_json::json;
//!
//! let mut connection = open::open(&oc_paths::DbLocation::Memory)?;
//! migration::apply(&mut connection)?;
//! # connection.execute_batch(
//! #     "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
//! #      VALUES ('p', '/w', 1, 1, '[]');
//! #      INSERT INTO session (id, project_id, slug, directory, title, version, \
//! #        time_created, time_updated) VALUES ('ses_1', 'p', 's', '/w', 't', '1', 1, 1);",
//! # ).map_err(oc_db::open::map_error)?;
//! let store = MessageStore::new(&connection);
//!
//! let message = MessageRecord::from_json(json!({
//!     "id": "msg_1", "sessionID": "ses_1", "role": "user",
//!     "time": { "created": 1 }, "agent": "build",
//!     "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
//! }))?;
//! store.put_message(&message)?;
//!
//! let part = PartRecord::from_json(json!({
//!     "id": "prt_1", "sessionID": "ses_1", "messageID": "msg_1",
//!     "type": "text", "text": "hello",
//! }), 1)?;
//! store.put_part(&part)?;
//!
//! store.reset_query_count();
//! let conversation = store.hydrate_session("ses_1")?;
//! assert_eq!(conversation.len(), 1);
//! assert_eq!(conversation[0].parts.len(), 1);
//! assert_eq!(store.query_count(), 2);
//! # Ok::<(), oc_error::DbError>(())
//! ```

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use oc_error::DbError;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};
use serde::de::Error as _;
use serde_json::{Map, Value};

use crate::open::map_error;

/// The JSON object a `data` column holds once the identity keys are stripped.
pub type JsonObject = Map<String, Value>;

/// The `message` table, named once.
const MESSAGE_TABLE: &str = "message";
/// The `part` table, named once.
const PART_TABLE: &str = "part";

/// How many message ids go into a single `IN (...)` part lookup.
///
/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 32766 on the amalgamation
/// `libsqlite3-sys` bundles, so this is far below the ceiling; it exists to keep
/// the statement text and the bind array bounded on a session with tens of
/// thousands of messages, not to appease a limit.
pub const HYDRATION_CHUNK: usize = 900;

/// The number of `type` tags the `Part` union declares.
pub const PART_KIND_COUNT: usize = 12;

/// Every `type` tag a stored part may carry.
///
/// The order matches the union at `packages/schema/src/v1/session.ts:357-370`.
/// There is deliberately no `Unknown` arm: a tag outside this set is a decode
/// failure, so adding a variant upstream breaks every `match` here until someone
/// decides what the new part means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PartKind {
    /// `text` - assistant or user prose. `TextPart`, schema line 102.
    Text,
    /// `subtask` - a delegated child task. `SubtaskPart`, schema line 204.
    Subtask,
    /// `reasoning` - model thinking. `ReasoningPart`, schema line 118.
    Reasoning,
    /// `file` - an attachment. `FilePart`, schema line 171.
    File,
    /// `tool` - a tool call and its state machine. `ToolPart`, schema line 315.
    Tool,
    /// `step-start` - the head of a provider step. `StepStartPart`, line 233.
    StepStart,
    /// `step-finish` - the tail of a provider step, with usage. Line 240.
    StepFinish,
    /// `snapshot` - a git snapshot marker. `SnapshotPart`, schema line 87.
    Snapshot,
    /// `patch` - a hash plus the files it touched. `PatchPart`, line 94.
    Patch,
    /// `agent` - an agent mention. `AgentPart`, schema line 181.
    Agent,
    /// `retry` - a failed attempt being retried. `RetryPart`, line 220.
    Retry,
    /// `compaction` - a context compaction boundary. Line 195.
    Compaction,
}

impl PartKind {
    /// Every variant, in the order the `Part` union declares them.
    pub const ALL: [Self; PART_KIND_COUNT] = [
        Self::Text,
        Self::Subtask,
        Self::Reasoning,
        Self::File,
        Self::Tool,
        Self::StepStart,
        Self::StepFinish,
        Self::Snapshot,
        Self::Patch,
        Self::Agent,
        Self::Retry,
        Self::Compaction,
    ];

    /// The wire tag, exactly as it appears in the `type` field of `part.data`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Subtask => "subtask",
            Self::Reasoning => "reasoning",
            Self::File => "file",
            Self::Tool => "tool",
            Self::StepStart => "step-start",
            Self::StepFinish => "step-finish",
            Self::Snapshot => "snapshot",
            Self::Patch => "patch",
            Self::Agent => "agent",
            Self::Retry => "retry",
            Self::Compaction => "compaction",
        }
    }

    /// The variant a wire tag names, or `None` when the tag is outside the union.
    ///
    /// `None` is never treated as "skip this part" by callers in this module; it
    /// becomes [`DbError::Decode`].
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == tag)
    }

    /// Every tag, for error messages that need to say what was expected.
    #[must_use]
    pub fn known_tags() -> String {
        Self::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for PartKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The `role` tag on a stored message.
///
/// `Info` is a union discriminated on `role` (`v1/session.ts:490`), so the role
/// decides which of two disjoint payload shapes the blob holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageRole {
    /// `user` - `User`, schema line 332.
    User,
    /// `assistant` - `Assistant`, schema line 453.
    Assistant,
}

impl MessageRole {
    /// Both variants.
    pub const ALL: [Self; 2] = [Self::User, Self::Assistant];

    /// The wire tag, exactly as it appears in the `role` field of `message.data`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    /// The role a wire tag names, or `None` when it is neither.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == tag)
    }

    /// Both tags, for error messages.
    #[must_use]
    pub fn known_tags() -> String {
        Self::ALL
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for MessageRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One row of `message`, with the blob kept as the object that was stored.
///
/// `data` never holds `id` or `sessionID`; it does still hold `role`, because
/// `V1MessageData` subtracts only the two identity keys. [`Self::role`] is that
/// same tag, validated once at construction so dispatch does not have to be
/// fallible. Mutating `data["role"]` afterwards desynchronises the two — rebuild
/// the record through [`MessageRecord::from_json`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// `message.id`, the primary key.
    pub id: String,
    /// `message.session_id`, an `ON DELETE CASCADE` reference to `session`.
    pub session_id: String,
    /// `message.time_created`. `projector.ts:264` sources this from
    /// `info.time.created`, not from a clock read at write time.
    pub time_created: i64,
    /// `message.time_updated`, the last write. Read back from the row; ignored by
    /// the writers, which stamp it themselves.
    pub time_updated: i64,
    /// The `role` tag inside `data`, validated.
    pub role: MessageRole,
    /// The blob: the message JSON minus `id` and `sessionID`.
    pub data: JsonObject,
}

impl MessageRecord {
    /// Split a full message JSON into columns and blob.
    ///
    /// `time_created` is taken from `time.created` when present, matching
    /// `projector.ts:264`; a message without it falls back to `0` rather than to
    /// a clock read, so the value stays a function of the input.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] when the value is not an object, when `id` or
    /// `sessionID` is missing or not a string, or when `role` is missing or
    /// outside the union.
    pub fn from_json(value: Value) -> Result<Self, DbError> {
        let mut object = into_object(value, MESSAGE_TABLE)?;
        let id = take_string(&mut object, "id", MESSAGE_TABLE)?;
        let session_id = take_string(&mut object, "sessionID", MESSAGE_TABLE)?;
        let role = read_role(&object)?;
        let time_created = object
            .get("time")
            .and_then(Value::as_object)
            .and_then(|time| time.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Ok(Self {
            id,
            session_id,
            time_created,
            time_updated: time_created,
            role,
            data: object,
        })
    }

    /// Reassemble the full message JSON, reinserting the two stripped keys.
    ///
    /// This is `info()` from `message-v2.ts:80-85`: the blob first, then `id` and
    /// `sessionID` from the columns.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = self.data.clone();
        object.insert("id".to_owned(), Value::String(self.id.clone()));
        object.insert(
            "sessionID".to_owned(),
            Value::String(self.session_id.clone()),
        );
        Value::Object(object)
    }

    /// The blob as it is written to the `data` column.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] if the object cannot be serialised, which for a tree
    /// built out of [`Value`] means a non-finite float reached it.
    pub fn data_json(&self) -> Result<String, DbError> {
        serde_json::to_string(&Value::Object(self.data.clone())).map_err(|source| DbError::Decode {
            table: MESSAGE_TABLE.to_owned(),
            source,
        })
    }

    /// Rebuild a record from a `message` row.
    fn from_row(row: &Row<'_>) -> Result<Self, DbError> {
        let id: String = row.get(0).map_err(map_error)?;
        let session_id: String = row.get(1).map_err(map_error)?;
        let time_created: i64 = row.get(2).map_err(map_error)?;
        let time_updated: i64 = row.get(3).map_err(map_error)?;
        let raw: String = row.get(4).map_err(map_error)?;
        let object = parse_object(&raw, MESSAGE_TABLE)?;
        reject_stored_keys(&object, MESSAGE_TABLE, &["id", "sessionID"])?;
        let role = read_role(&object)?;
        Ok(Self {
            id,
            session_id,
            time_created,
            time_updated,
            role,
            data: object,
        })
    }
}

/// One row of `part`, with the blob kept as the object that was stored.
///
/// `data` never holds `id`, `sessionID` or `messageID`; it does still hold
/// `type`. [`Self::kind`] is that tag, validated at construction.
///
/// `part.session_id` is indexed but carries **no** foreign key - only
/// `message_id` does (`schema.rs:117-125`). A part therefore cannot be written
/// before its message, but it can be written with a `session_id` that no session
/// row backs, and SQLite will not object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRecord {
    /// `part.id`, the primary key.
    pub id: String,
    /// `part.message_id`, an `ON DELETE CASCADE` reference to `message`.
    pub message_id: String,
    /// `part.session_id`, indexed but not a foreign key.
    pub session_id: String,
    /// `part.time_created`. `projector.ts:321` sources this from the event's own
    /// timestamp, which is not present in the part payload, so it is supplied by
    /// the caller.
    pub time_created: i64,
    /// `part.time_updated`, the last write. Read back from the row; ignored by
    /// the writers.
    pub time_updated: i64,
    /// The `type` tag inside `data`, validated.
    pub kind: PartKind,
    /// The blob: the part JSON minus `id`, `sessionID` and `messageID`.
    pub data: JsonObject,
}

impl PartRecord {
    /// Split a full part JSON into columns and blob.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] when the value is not an object, when `id`,
    /// `sessionID` or `messageID` is missing or not a string, or when `type` is
    /// missing or outside the union - the unknown-variant case.
    pub fn from_json(value: Value, time_created: i64) -> Result<Self, DbError> {
        let mut object = into_object(value, PART_TABLE)?;
        let id = take_string(&mut object, "id", PART_TABLE)?;
        let session_id = take_string(&mut object, "sessionID", PART_TABLE)?;
        let message_id = take_string(&mut object, "messageID", PART_TABLE)?;
        let kind = read_kind(&object)?;
        Ok(Self {
            id,
            message_id,
            session_id,
            time_created,
            time_updated: time_created,
            kind,
            data: object,
        })
    }

    /// Reassemble the full part JSON, reinserting the three stripped keys.
    ///
    /// This is `part()` from `message-v2.ts:87-93`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = self.data.clone();
        object.insert("id".to_owned(), Value::String(self.id.clone()));
        object.insert(
            "sessionID".to_owned(),
            Value::String(self.session_id.clone()),
        );
        object.insert(
            "messageID".to_owned(),
            Value::String(self.message_id.clone()),
        );
        Value::Object(object)
    }

    /// The blob as it is written to the `data` column.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] if the object cannot be serialised.
    pub fn data_json(&self) -> Result<String, DbError> {
        serde_json::to_string(&Value::Object(self.data.clone())).map_err(|source| DbError::Decode {
            table: PART_TABLE.to_owned(),
            source,
        })
    }

    /// Rebuild a record from a `part` row.
    fn from_row(row: &Row<'_>) -> Result<Self, DbError> {
        let id: String = row.get(0).map_err(map_error)?;
        let message_id: String = row.get(1).map_err(map_error)?;
        let session_id: String = row.get(2).map_err(map_error)?;
        let time_created: i64 = row.get(3).map_err(map_error)?;
        let time_updated: i64 = row.get(4).map_err(map_error)?;
        let raw: String = row.get(5).map_err(map_error)?;
        let object = parse_object(&raw, PART_TABLE)?;
        reject_stored_keys(&object, PART_TABLE, &["id", "sessionID", "messageID"])?;
        let kind = read_kind(&object)?;
        Ok(Self {
            id,
            message_id,
            session_id,
            time_created,
            time_updated,
            kind,
            data: object,
        })
    }
}

/// A message and the parts that belong to it, in `part.id` order.
///
/// The `{info, parts}` pair from `WithParts` (`v1/session.ts:493-500`) and the
/// return shape of `hydrate` (`message-v2.ts:118-121`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageWithParts {
    /// The message row.
    pub info: MessageRecord,
    /// Its parts. Empty is normal - a message is written before its first part.
    pub parts: Vec<PartRecord>,
}

/// Reads and writes for `message` and `part` over one connection.
///
/// Holds a statement counter so the absence of an N+1 can be asserted rather
/// than asserted-by-inspection. Every statement this type issues is prepared
/// through [`MessageStore::prepare`], which is the only place the counter moves.
pub struct MessageStore<'conn> {
    connection: &'conn Connection,
    statements: Cell<u32>,
}

impl<'conn> MessageStore<'conn> {
    /// Wrap a connection. Cheap; hold one per unit of work.
    #[must_use]
    pub fn new(connection: &'conn Connection) -> Self {
        Self {
            connection,
            statements: Cell::new(0),
        }
    }

    /// How many SQL statements this store has prepared since it was created or
    /// last reset.
    #[must_use]
    pub fn query_count(&self) -> u32 {
        self.statements.get()
    }

    /// Zero the statement counter.
    pub fn reset_query_count(&self) {
        self.statements.set(0);
    }

    /// The single place a statement is prepared, and so the single place the
    /// counter can move.
    fn prepare(&self, sql: &str) -> Result<rusqlite::Statement<'conn>, DbError> {
        self.statements.set(self.statements.get().saturating_add(1));
        self.connection.prepare(sql).map_err(map_error)
    }

    /// Upsert a message, stamping `time_updated` with `now`.
    ///
    /// Mirrors `projector.ts:268-272`: insert with the caller's `time_created`,
    /// and on a primary-key conflict replace `data`. The observed `message` rows
    /// in a real database have `time_updated` later than `time_created` in
    /// 19998 of 20000 sampled rows, so the conflict path bumps it too.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] if the blob cannot be serialised, [`DbError::Query`]
    /// or [`DbError::Busy`] from SQLite - including a foreign-key violation when
    /// `session_id` names no session.
    pub fn put_message_at(&self, record: &MessageRecord, now: i64) -> Result<(), DbError> {
        let data = record.data_json()?;
        self.prepare(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, time_updated = excluded.time_updated",
        )?
        .execute(rusqlite::params![
            &record.id,
            &record.session_id,
            record.time_created,
            now,
            &data,
        ])
        .map_err(map_error)?;
        Ok(())
    }

    /// [`Self::put_message_at`] with `now` read from the system clock.
    ///
    /// # Errors
    ///
    /// As [`Self::put_message_at`].
    pub fn put_message(&self, record: &MessageRecord) -> Result<(), DbError> {
        self.put_message_at(record, now_millis())
    }

    /// Insert a message only if its id is free, leaving any existing row alone.
    ///
    /// This is the writer `import` needs and [`Self::put_message_at`] cannot be:
    /// upstream imports with `onConflictDoNothing` (`cli/cmd/import.ts:196-204`),
    /// so re-importing a file is not a way to overwrite a live transcript. Using
    /// the upserting writer there would make `import` a silent editor of history.
    ///
    /// Returns whether a row was written.
    ///
    /// # Errors
    ///
    /// As [`Self::put_message_at`].
    pub fn insert_message_if_absent(&self, record: &MessageRecord) -> Result<bool, DbError> {
        let data = record.data_json()?;
        let written = self
            .prepare(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?3, ?4) \
                 ON CONFLICT(id) DO NOTHING",
            )?
            .execute(rusqlite::params![
                &record.id,
                &record.session_id,
                record.time_created,
                &data,
            ])
            .map_err(map_error)?;
        Ok(written > 0)
    }

    /// Insert a part only if its id is free. The counterpart of
    /// [`Self::insert_message_if_absent`], for the same reason.
    ///
    /// # Errors
    ///
    /// As [`Self::put_part_at`].
    pub fn insert_part_if_absent(&self, record: &PartRecord) -> Result<bool, DbError> {
        let data = record.data_json()?;
        let written = self
            .prepare(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5) \
                 ON CONFLICT(id) DO NOTHING",
            )?
            .execute(rusqlite::params![
                &record.id,
                &record.message_id,
                &record.session_id,
                record.time_created,
                &data,
            ])
            .map_err(map_error)?;
        Ok(written > 0)
    }

    /// Upsert a part, stamping `time_updated` with `now`.
    ///
    /// Mirrors `projector.ts:319-323`.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] if the blob cannot be serialised, [`DbError::Query`]
    /// or [`DbError::Busy`] from SQLite - including a foreign-key violation when
    /// `message_id` names no message.
    pub fn put_part_at(&self, record: &PartRecord, now: i64) -> Result<(), DbError> {
        let data = record.data_json()?;
        self.prepare(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, time_updated = excluded.time_updated",
        )?
        .execute(rusqlite::params![
            &record.id,
            &record.message_id,
            &record.session_id,
            record.time_created,
            now,
            &data,
        ])
        .map_err(map_error)?;
        Ok(())
    }

    /// [`Self::put_part_at`] with `now` read from the system clock.
    ///
    /// # Errors
    ///
    /// As [`Self::put_part_at`].
    pub fn put_part(&self, record: &PartRecord) -> Result<(), DbError> {
        self.put_part_at(record, now_millis())
    }

    /// One message by id.
    ///
    /// # Errors
    ///
    /// [`DbError::NotFound`] when no such row exists, [`DbError::Decode`] when
    /// its blob does not parse or carries an unknown `role`.
    pub fn message(&self, id: &str) -> Result<MessageRecord, DbError> {
        let record = self
            .prepare(
                "SELECT id, session_id, time_created, time_updated, data FROM message WHERE id = ?1",
            )?
            .query_row([id], |row| Ok(MessageRecord::from_row(row)))
            .optional()
            .map_err(map_error)?;
        match record {
            Some(result) => result,
            None => Err(DbError::NotFound {
                table: MESSAGE_TABLE.to_owned(),
                id: id.to_owned(),
            }),
        }
    }

    /// One part by id.
    ///
    /// # Errors
    ///
    /// [`DbError::NotFound`] when no such row exists, [`DbError::Decode`] when
    /// its blob does not parse or carries an unknown `type`.
    pub fn part(&self, id: &str) -> Result<PartRecord, DbError> {
        let record = self
            .prepare(
                "SELECT id, message_id, session_id, time_created, time_updated, data \
                 FROM part WHERE id = ?1",
            )?
            .query_row([id], |row| Ok(PartRecord::from_row(row)))
            .optional()
            .map_err(map_error)?;
        match record {
            Some(result) => result,
            None => Err(DbError::NotFound {
                table: PART_TABLE.to_owned(),
                id: id.to_owned(),
            }),
        }
    }

    /// The newest `time_created` in a session, or `None` when it has no messages.
    ///
    /// A writer that must sort after everything already stored cannot get that from
    /// the clock alone. [`Self::messages_for_session`] breaks a `time_created` tie
    /// with `id`, faithfully to upstream — but upstream's ids are time-ordered
    /// identifiers and this port's are random UUIDs, so two messages written inside
    /// one millisecond order by coin flip. Pair this with [`created_after`] to make
    /// the new row's position a fact rather than a race.
    ///
    /// Costs one statement, answered from `message_session_time_created_id_idx`
    /// (`schema.rs:208`) without reading a `data` blob.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] or [`DbError::Busy`] from SQLite.
    pub fn latest_time_created(&self, session_id: &str) -> Result<Option<i64>, DbError> {
        self.prepare("SELECT MAX(time_created) FROM message WHERE session_id = ?1")?
            .query_row([session_id], |row| row.get::<_, Option<i64>>(0))
            .map_err(map_error)
    }

    /// Every message of a session, oldest first.
    ///
    /// Ordered `(time_created, id)` to ride `message_session_time_created_id_idx`
    /// exactly (`schema.rs:208`), with `id` breaking ties the way the cursor
    /// comparison in `message-v2.ts:95-96` does.
    ///
    /// Costs one statement.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] on a blob that does not parse or carries an unknown
    /// `role`; [`DbError::Query`] or [`DbError::Busy`] from SQLite.
    pub fn messages_for_session(&self, session_id: &str) -> Result<Vec<MessageRecord>, DbError> {
        let mut statement = self.prepare(
            "SELECT id, session_id, time_created, time_updated, data FROM message \
             WHERE session_id = ?1 ORDER BY time_created ASC, id ASC",
        )?;
        let rows = statement
            .query_map([session_id], |row| Ok(MessageRecord::from_row(row)))
            .map_err(map_error)?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(map_error)??);
        }
        Ok(messages)
    }

    /// Every part belonging to any of `message_ids`, grouped by message id.
    ///
    /// One statement per [`HYDRATION_CHUNK`] ids, never one per message. Within
    /// each group the order is `part.id` ascending, matching
    /// `message-v2.ts:107`.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] on a blob that does not parse or carries an unknown
    /// `type`; [`DbError::Query`] or [`DbError::Busy`] from SQLite.
    pub fn parts_by_message(
        &self,
        message_ids: &[String],
    ) -> Result<HashMap<String, Vec<PartRecord>>, DbError> {
        let mut grouped: HashMap<String, Vec<PartRecord>> = HashMap::new();
        for chunk in message_ids.chunks(HYDRATION_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
                 WHERE message_id IN ({placeholders}) ORDER BY message_id ASC, id ASC"
            );
            let mut statement = self.prepare(&sql)?;
            let rows = statement
                .query_map(params_from_iter(chunk.iter()), |row| {
                    Ok(PartRecord::from_row(row))
                })
                .map_err(map_error)?;
            for row in rows {
                let record = row.map_err(map_error)??;
                grouped
                    .entry(record.message_id.clone())
                    .or_default()
                    .push(record);
            }
        }
        Ok(grouped)
    }

    /// Parts of one kind belonging to any of `message_ids`, grouped by message id.
    ///
    /// This is the metadata phase of retained-history hydration: callers can inspect
    /// compaction markers or summary text without decoding unrelated tool outputs.
    /// The result keeps the same `part.id` order as [`Self::parts_by_message`].
    ///
    /// # Errors
    ///
    /// As [`Self::parts_by_message`].
    pub fn parts_by_message_kind(
        &self,
        message_ids: &[String],
        kind: PartKind,
    ) -> Result<HashMap<String, Vec<PartRecord>>, DbError> {
        let mut grouped: HashMap<String, Vec<PartRecord>> = HashMap::new();
        for chunk in message_ids.chunks(HYDRATION_CHUNK) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let kind_parameter = chunk.len() + 1;
            let sql = format!(
                "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
                 WHERE message_id IN ({placeholders}) \
                 AND json_extract(data, '$.type') = ?{kind_parameter} \
                 ORDER BY message_id ASC, id ASC"
            );
            let mut statement = self.prepare(&sql)?;
            let parameters = chunk
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(kind.as_str()));
            let rows = statement
                .query_map(params_from_iter(parameters), |row| {
                    Ok(PartRecord::from_row(row))
                })
                .map_err(map_error)?;
            for row in rows {
                let record = row.map_err(map_error)??;
                grouped
                    .entry(record.message_id.clone())
                    .or_default()
                    .push(record);
            }
        }
        Ok(grouped)
    }

    /// Every part of `kind` in a session, ordered by message id and part id.
    ///
    /// The JSON predicate runs inside SQLite so non-matching payloads never become
    /// Rust JSON trees. This is intentionally not a replacement for full hydration;
    /// it is the lightweight first phase used to discover a compaction boundary.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] for a matching row that cannot be decoded;
    /// [`DbError::Query`] or [`DbError::Busy`] from SQLite.
    pub fn parts_for_session_by_kind(
        &self,
        session_id: &str,
        kind: PartKind,
    ) -> Result<Vec<PartRecord>, DbError> {
        let mut statement = self.prepare(
            "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
             WHERE session_id = ?1 AND json_extract(data, '$.type') = ?2 \
             ORDER BY message_id ASC, id ASC",
        )?;
        let rows = statement
            .query_map((session_id, kind.as_str()), |row| {
                Ok(PartRecord::from_row(row))
            })
            .map_err(map_error)?;
        let mut parts = Vec::new();
        for row in rows {
            parts.push(row.map_err(map_error)??);
        }
        Ok(parts)
    }

    /// Tool parts whose state is neither `completed` nor `error`.
    ///
    /// Repair must cover the entire session, including the head hidden by a valid
    /// compaction. Filtering in SQLite avoids decoding completed tool outputs, which
    /// are commonly the largest blobs in the database.
    ///
    /// # Errors
    ///
    /// [`DbError::Decode`] for a matching row that cannot be decoded;
    /// [`DbError::Query`] or [`DbError::Busy`] from SQLite.
    pub fn unfinished_tool_parts_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<PartRecord>, DbError> {
        let mut statement = self.prepare(
            "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
             WHERE session_id = ?1 AND json_extract(data, '$.type') = 'tool' \
             AND (json_extract(data, '$.state.status') IS NULL \
                  OR json_extract(data, '$.state.status') NOT IN ('completed', 'error')) \
             ORDER BY message_id ASC, id ASC",
        )?;
        let rows = statement
            .query_map([session_id], |row| Ok(PartRecord::from_row(row)))
            .map_err(map_error)?;
        let mut parts = Vec::new();
        for row in rows {
            parts.push(row.map_err(map_error)??);
        }
        Ok(parts)
    }

    /// Attach parts to messages already in hand.
    ///
    /// The batched half of `hydrate` (`message-v2.ts:98-123`): one part lookup
    /// for the whole set, then a group-by in memory. A message with no parts
    /// gets an empty vector, which is what `?? []` at line 120 produces.
    ///
    /// # Errors
    ///
    /// As [`Self::parts_by_message`].
    pub fn hydrate(&self, messages: Vec<MessageRecord>) -> Result<Vec<MessageWithParts>, DbError> {
        let ids: Vec<String> = messages.iter().map(|message| message.id.clone()).collect();
        let mut grouped = self.parts_by_message(&ids)?;
        Ok(messages
            .into_iter()
            .map(|info| {
                let parts = grouped.remove(&info.id).unwrap_or_default();
                MessageWithParts { info, parts }
            })
            .collect())
    }

    /// Every message of a session with its parts attached.
    ///
    /// Two statements for any session that fits in one [`HYDRATION_CHUNK`],
    /// which is the whole point.
    ///
    /// # Errors
    ///
    /// As [`Self::messages_for_session`] and [`Self::parts_by_message`].
    pub fn hydrate_session(&self, session_id: &str) -> Result<Vec<MessageWithParts>, DbError> {
        let messages = self.messages_for_session(session_id)?;
        self.hydrate(messages)
    }
}

impl fmt::Debug for MessageStore<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageStore")
            .field("statements", &self.statements.get())
            .finish_non_exhaustive()
    }
}

/// A `time_created` at or after `now` that sorts strictly after `latest`.
///
/// The clock is not enough on its own: a caller can persist two messages inside one
/// millisecond, and [`MessageStore::messages_for_session`] then breaks the tie with
/// the random UUID in the id rather than with the order they were written. Half of
/// those coin flips put a reply ahead of what it replies to, which reorders the
/// request prefix and costs the prompt-cache hit `oc_llm`'s append-only tracker
/// exists to protect. Clamping is the cheap half of the fix; the expensive half
/// would be replacing random ids with time-ordered ones.
#[must_use]
pub fn created_after(now: i64, latest: Option<i64>) -> i64 {
    match latest {
        Some(latest) => now.max(latest.saturating_add(1)),
        None => now,
    }
}

/// Milliseconds since the Unix epoch, the unit every `time_*` column uses.
///
/// A clock before the epoch yields `0` rather than a negative stamp, because a
/// negative `time_created` would sort ahead of every real row in the index.
#[must_use]
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A [`DbError::Decode`] carrying a message instead of a parse failure.
fn decode(table: &str, message: String) -> DbError {
    DbError::Decode {
        table: table.to_owned(),
        source: serde_json::Error::custom(message),
    }
}

/// Require a JSON object.
fn into_object(value: Value, table: &str) -> Result<JsonObject, DbError> {
    match value {
        Value::Object(object) => Ok(object),
        other => Err(decode(
            table,
            format!("expected a JSON object, found {}", type_name(&other)),
        )),
    }
}

/// Parse a stored `data` column into an object.
fn parse_object(raw: &str, table: &str) -> Result<JsonObject, DbError> {
    let value = serde_json::from_str::<Value>(raw).map_err(|source| DbError::Decode {
        table: table.to_owned(),
        source,
    })?;
    into_object(value, table)
}

/// Remove a required string key.
fn take_string(object: &mut JsonObject, key: &str, table: &str) -> Result<String, DbError> {
    match object.remove(key) {
        Some(Value::String(value)) => Ok(value),
        Some(other) => Err(decode(
            table,
            format!("`{key}` must be a string, found {}", type_name(&other)),
        )),
        None => Err(decode(table, format!("`{key}` is missing"))),
    }
}

/// Fail when a stripped identity key was written into the blob after all.
///
/// A duplicated key is not a parse error and no constraint catches it, so it is
/// checked on the way out: a blob that carries `id` alongside the `id` column has
/// two answers to the same question, and only one of them is indexed.
fn reject_stored_keys(object: &JsonObject, table: &str, keys: &[&str]) -> Result<(), DbError> {
    for key in keys {
        if object.contains_key(*key) {
            return Err(decode(
                table,
                format!("`{key}` belongs to a column and must not appear in `data`"),
            ));
        }
    }
    Ok(())
}

/// Read and validate the `type` discriminator.
fn read_kind(object: &JsonObject) -> Result<PartKind, DbError> {
    match object.get("type") {
        Some(Value::String(tag)) => PartKind::from_tag(tag).ok_or_else(|| {
            decode(
                PART_TABLE,
                format!(
                    "unknown part variant `{tag}`; the Part union declares {}",
                    PartKind::known_tags()
                ),
            )
        }),
        Some(other) => Err(decode(
            PART_TABLE,
            format!("`type` must be a string, found {}", type_name(other)),
        )),
        None => Err(decode(
            PART_TABLE,
            format!(
                "`type` is missing; the Part union declares {}",
                PartKind::known_tags()
            ),
        )),
    }
}

/// Read and validate the `role` discriminator.
fn read_role(object: &JsonObject) -> Result<MessageRole, DbError> {
    match object.get("role") {
        Some(Value::String(tag)) => MessageRole::from_tag(tag).ok_or_else(|| {
            decode(
                MESSAGE_TABLE,
                format!(
                    "unknown message role `{tag}`; Info declares {}",
                    MessageRole::known_tags()
                ),
            )
        }),
        Some(other) => Err(decode(
            MESSAGE_TABLE,
            format!("`role` must be a string, found {}", type_name(other)),
        )),
        None => Err(decode(
            MESSAGE_TABLE,
            format!(
                "`role` is missing; Info declares {}",
                MessageRole::known_tags()
            ),
        )),
    }
}

/// The JSON type name, for error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_tag_round_trips_through_from_tag() {
        for kind in PartKind::ALL {
            assert_eq!(PartKind::from_tag(kind.as_str()), Some(kind));
        }
        for role in MessageRole::ALL {
            assert_eq!(MessageRole::from_tag(role.as_str()), Some(role));
        }
    }

    #[test]
    fn all_holds_every_variant_exactly_once() {
        let mut tags: Vec<&str> = PartKind::ALL.iter().map(|kind| kind.as_str()).collect();
        tags.sort_unstable();
        let count = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), count, "a tag appears twice in PartKind::ALL");
        assert_eq!(count, PART_KIND_COUNT);
    }

    #[test]
    fn an_unknown_tag_is_none_not_a_fallback() {
        assert_eq!(PartKind::from_tag("telepathy"), None);
        assert_eq!(PartKind::from_tag("Text"), None);
        assert_eq!(PartKind::from_tag(""), None);
        assert_eq!(MessageRole::from_tag("system"), None);
    }
}
