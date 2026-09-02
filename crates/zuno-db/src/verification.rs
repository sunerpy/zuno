//! Durable receipts for tool calls that carry verification authority.
//!
//! A receipt records what a tool actually did and whether its own exit status is
//! trustworthy evidence of that. The turn loop writes one receipt per verifying
//! tool call; goal completion reads them so "done" can be gated on recorded
//! evidence instead of on the model's narration of the transcript.
//!
//! Receipts are keyed by `(session_id, tool_call_id)` and written with an
//! idempotent upsert, so a replayed or retried turn cannot inflate the evidence
//! a session appears to hold.

use crate::open;
use rusqlite::{Connection, OptionalExtension as _, Row, params};
use zuno_error::DbError;

const COLUMNS: &str = "id, session_id, turn_id, tool_call_id, tool_id, summary, workdir, \
                       exit_code, exit_authority, outcome, git_head, output_digest, detail, \
                       time_created";

/// How much authority a recorded exit status carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitAuthority {
    /// The status reflects every stage of the command that ran.
    Authoritative,
    /// The status was inferred, for example from the last stage of a pipeline.
    Derived,
    /// No exit status was available at all.
    Absent,
}

impl ExitAuthority {
    /// The stored representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Derived => "derived",
            Self::Absent => "absent",
        }
    }

    /// Whether this status may be cited as evidence on its own.
    #[must_use]
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Authoritative)
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "authoritative" => Ok(Self::Authoritative),
            "derived" => Ok(Self::Derived),
            "absent" => Ok(Self::Absent),
            other => Err(query_error(format!(
                "unknown verification exit authority `{other}`"
            ))),
        }
    }
}

/// What the recorded call proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiptOutcome {
    /// The call ran to completion and reported success.
    Passed,
    /// The call ran and reported failure.
    Failed,
    /// The call's result is not decidable, so it proves nothing.
    Unknown,
}

impl ReceiptOutcome {
    /// The stored representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            other => Err(query_error(format!(
                "unknown verification outcome `{other}`"
            ))),
        }
    }
}

/// A receipt to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVerificationReceipt {
    /// Stable receipt identifier the model may cite.
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    /// The tool call that produced this receipt.
    pub tool_call_id: String,
    pub tool_id: String,
    /// One line describing what ran.
    pub summary: String,
    pub workdir: Option<String>,
    pub exit_code: Option<i64>,
    pub exit_authority: ExitAuthority,
    pub outcome: ReceiptOutcome,
    /// Repository head observed when the call ran, when the tool reports one.
    pub git_head: Option<String>,
    /// Digest of the captured output, so a citation can be checked for drift.
    pub output_digest: Option<String>,
    pub detail: Option<String>,
    pub time_created: i64,
}

/// A stored receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReceipt {
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_call_id: String,
    pub tool_id: String,
    pub summary: String,
    pub workdir: Option<String>,
    pub exit_code: Option<i64>,
    pub exit_authority: ExitAuthority,
    pub outcome: ReceiptOutcome,
    pub git_head: Option<String>,
    pub output_digest: Option<String>,
    pub detail: Option<String>,
    pub time_created: i64,
}

impl VerificationReceipt {
    /// Whether this receipt is usable as standalone evidence that work succeeded.
    #[must_use]
    pub const fn proves_success(&self) -> bool {
        matches!(self.outcome, ReceiptOutcome::Passed) && self.exit_authority.is_authoritative()
    }
}

/// Store one receipt, replacing any earlier receipt for the same tool call.
///
/// Accepts a `&Transaction` through deref, so callers can record a receipt in the
/// same transaction that appends the tool result.
///
/// # Errors
///
/// [`DbError`] if SQLite rejects the write.
pub fn record(connection: &Connection, receipt: &NewVerificationReceipt) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO verification_receipt (
               id, session_id, turn_id, tool_call_id, tool_id, summary, workdir, exit_code,
               exit_authority, outcome, git_head, output_digest, detail, time_created
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (session_id, tool_call_id) DO UPDATE SET
               id = excluded.id,
               turn_id = excluded.turn_id,
               tool_id = excluded.tool_id,
               summary = excluded.summary,
               workdir = excluded.workdir,
               exit_code = excluded.exit_code,
               exit_authority = excluded.exit_authority,
               outcome = excluded.outcome,
               git_head = excluded.git_head,
               output_digest = excluded.output_digest,
               detail = excluded.detail,
               time_created = excluded.time_created",
            params![
                receipt.id,
                receipt.session_id,
                receipt.turn_id,
                receipt.tool_call_id,
                receipt.tool_id,
                receipt.summary,
                receipt.workdir,
                receipt.exit_code,
                receipt.exit_authority.as_str(),
                receipt.outcome.as_str(),
                receipt.git_head,
                receipt.output_digest,
                receipt.detail,
                receipt.time_created,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Read one receipt by the identifier a model cited.
///
/// # Errors
///
/// [`DbError`] if SQLite rejects the read or stores an unknown enum value.
pub fn find(
    connection: &Connection,
    session_id: &str,
    id: &str,
) -> Result<Option<VerificationReceipt>, DbError> {
    let sql =
        format!("SELECT {COLUMNS} FROM verification_receipt WHERE session_id = ?1 AND id = ?2");
    connection
        .query_row(&sql, params![session_id, id], read_row)
        .optional()
        .map_err(open::map_error)?
        .transpose()
}

/// Read every receipt for one session, oldest first.
///
/// # Errors
///
/// [`DbError`] if SQLite rejects the read or stores an unknown enum value.
pub fn for_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<VerificationReceipt>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM verification_receipt \
         WHERE session_id = ?1 ORDER BY time_created, id"
    );
    let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
    let rows = statement
        .query_map(params![session_id], read_row)
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    rows.into_iter().collect()
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Result<VerificationReceipt, DbError>> {
    let exit_authority: String = row.get(8)?;
    let outcome: String = row.get(9)?;
    let receipt = VerificationReceipt {
        id: row.get(0)?,
        session_id: row.get(1)?,
        turn_id: row.get(2)?,
        tool_call_id: row.get(3)?,
        tool_id: row.get(4)?,
        summary: row.get(5)?,
        workdir: row.get(6)?,
        exit_code: row.get(7)?,
        exit_authority: ExitAuthority::Authoritative,
        outcome: ReceiptOutcome::Unknown,
        git_head: row.get(10)?,
        output_digest: row.get(11)?,
        detail: row.get(12)?,
        time_created: row.get(13)?,
    };
    Ok(
        match (
            ExitAuthority::parse(&exit_authority),
            ReceiptOutcome::parse(&outcome),
        ) {
            (Ok(exit_authority), Ok(outcome)) => Ok(VerificationReceipt {
                exit_authority,
                outcome,
                ..receipt
            }),
            (Err(error), _) | (_, Err(error)) => Err(error),
        },
    )
}

fn query_error(message: String) -> DbError {
    crate::event_log::query_error(std::io::Error::other(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;

    fn memory() -> Connection {
        let mut connection =
            open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
        migration::apply(&mut connection).expect("create current schema");
        connection
    }

    fn receipt(id: &str, call_id: &str, outcome: ReceiptOutcome) -> NewVerificationReceipt {
        NewVerificationReceipt {
            id: id.to_owned(),
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            tool_call_id: call_id.to_owned(),
            tool_id: "shell".to_owned(),
            summary: "cargo test --workspace".to_owned(),
            workdir: Some("/workspace".to_owned()),
            exit_code: Some(0),
            exit_authority: ExitAuthority::Authoritative,
            outcome,
            git_head: Some("abc123".to_owned()),
            output_digest: Some("digest".to_owned()),
            detail: None,
            time_created: 10,
        }
    }

    #[test]
    fn a_recorded_receipt_reads_back_with_every_field() {
        let connection = memory();
        record(
            &connection,
            &receipt("receipt-1", "call-1", ReceiptOutcome::Passed),
        )
        .expect("record receipt");

        let stored = find(&connection, "session-1", "receipt-1")
            .expect("read receipt")
            .expect("receipt exists");
        assert_eq!(stored.tool_call_id, "call-1");
        assert_eq!(stored.outcome, ReceiptOutcome::Passed);
        assert_eq!(stored.exit_authority, ExitAuthority::Authoritative);
        assert_eq!(stored.git_head.as_deref(), Some("abc123"));
        assert!(stored.proves_success());
    }

    #[test]
    fn replaying_one_tool_call_keeps_exactly_one_receipt() {
        let connection = memory();
        record(
            &connection,
            &receipt("receipt-1", "call-1", ReceiptOutcome::Unknown),
        )
        .expect("record first attempt");
        record(
            &connection,
            &receipt("receipt-2", "call-1", ReceiptOutcome::Passed),
        )
        .expect("record replay");

        let all = for_session(&connection, "session-1").expect("list receipts");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "receipt-2");
        assert_eq!(all[0].outcome, ReceiptOutcome::Passed);
        assert_eq!(
            find(&connection, "session-1", "receipt-1").expect("query superseded id"),
            None
        );
    }

    #[test]
    fn a_derived_exit_status_never_proves_success_on_its_own() {
        let connection = memory();
        let mut derived = receipt("receipt-1", "call-1", ReceiptOutcome::Passed);
        derived.exit_authority = ExitAuthority::Derived;
        record(&connection, &derived).expect("record derived receipt");

        let stored = find(&connection, "session-1", "receipt-1")
            .expect("read receipt")
            .expect("receipt exists");
        assert!(!stored.proves_success());
    }

    #[test]
    fn receipts_are_scoped_to_one_session() {
        let connection = memory();
        record(
            &connection,
            &receipt("receipt-1", "call-1", ReceiptOutcome::Passed),
        )
        .expect("record receipt");

        assert!(
            for_session(&connection, "session-2")
                .expect("list other session")
                .is_empty()
        );
        assert_eq!(
            find(&connection, "session-2", "receipt-1").expect("query other session"),
            None
        );
    }
}
