//! Additive storage owned by the optional continuity component.
//!
//! These tables deliberately do not participate in the core schema format
//! marker. A runtime that does not mount continuity never creates them; a runtime
//! that does can add or remove the component without making the session database
//! unreadable by the rest of the harness.

use rusqlite::Connection;
use zuno_error::DbError;

use crate::open;

/// Durable note documents, scoped by session and active Agent identity.
pub const SESSION_NOTE_TABLE: &str = "session_note";

/// Idempotency ledger for side-effecting note calls.
pub const SESSION_NOTE_OPERATION_TABLE: &str = "session_note_operation";

/// Maximum number of documents in one session-and-Agent note scope.
pub const MAX_NOTE_DOCUMENTS: u64 = 100;

/// Maximum UTF-8 bytes in one note document.
pub const MAX_NOTE_DOCUMENT_BYTES: u64 = 256 * 1024;

/// Maximum UTF-8 bytes across one session-and-Agent note scope.
pub const MAX_NOTE_SCOPE_BYTES: u64 = 1024 * 1024;

/// Why a logical note document name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidNoteName {
    detail: &'static str,
}

impl InvalidNoteName {
    const fn new(detail: &'static str) -> Self {
        Self { detail }
    }
}

impl std::fmt::Display for InvalidNoteName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for InvalidNoteName {}

/// Validate a logical note document name at durable-storage boundaries.
pub fn validate_note_name(name: &str) -> Result<(), InvalidNoteName> {
    if name.is_empty() || name.len() > 255 {
        return Err(InvalidNoteName::new(
            "note name must contain 1 through 255 UTF-8 bytes",
        ));
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains('\\') || name.contains('\0') {
        return Err(InvalidNoteName::new(
            "note name must be a logical slash-separated name, not a host path",
        ));
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.len() > 64 {
            return Err(InvalidNoteName::new(
                "each note-name segment must contain 1 through 64 bytes and cannot be `.` or `..`",
            ));
        }
        let mut characters = segment.chars();
        let Some(first) = characters.next() else {
            unreachable!("empty segments were rejected");
        };
        if !first.is_ascii_alphanumeric()
            || !characters
                .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        {
            return Err(InvalidNoteName::new(
                "note-name segments must start with an ASCII letter or digit and may then contain `.`, `_`, or `-`",
            ));
        }
    }
    Ok(())
}

/// Create the continuity component's additive tables and indices.
///
/// This is idempotent and does not alter `zuno_schema.format`.
pub fn ensure_schema(connection: &Connection) -> Result<(), DbError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_note (
               session_id TEXT NOT NULL,
               agent TEXT NOT NULL,
               name TEXT NOT NULL,
               revision INTEGER NOT NULL CHECK (revision >= 1),
               content TEXT NOT NULL,
               content_sha256 TEXT NOT NULL,
               time_created INTEGER NOT NULL,
               time_updated INTEGER NOT NULL,
               PRIMARY KEY (session_id, agent, name),
               FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS session_note_scope_name
               ON session_note(session_id, agent, name);
             CREATE TABLE IF NOT EXISTS session_note_operation (
               session_id TEXT NOT NULL,
               agent TEXT NOT NULL,
               call_id TEXT NOT NULL,
               request_sha256 TEXT NOT NULL,
               action TEXT NOT NULL CHECK (action IN ('append', 'write')),
               name TEXT NOT NULL,
               result_revision INTEGER NOT NULL CHECK (result_revision >= 1),
               result_content_sha256 TEXT NOT NULL,
               time_created INTEGER NOT NULL,
               PRIMARY KEY (session_id, agent, call_id),
               FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS session_note_operation_scope_time
               ON session_note_operation(session_id, agent, time_created, call_id);",
        )
        .map_err(open::map_error)
}

/// Whether an additive component table exists in this database.
pub fn table_exists(connection: &Connection, table: &str) -> Result<bool, DbError> {
    connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
             )",
            [table],
            |row| row.get(0),
        )
        .map_err(open::map_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_note_name_contract_is_owned_by_the_durable_boundary() {
        for invalid in ["", "/tmp/a", "../a", "a/../b", "a//b", "a b", "a\\b"] {
            assert!(validate_note_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["evidence.md", "task/ci/run-1.txt", "A_1/x.y"] {
            validate_note_name(valid).expect(valid);
        }
    }
}
