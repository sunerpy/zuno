//! Preview-first session archive and destructive pruning over retention-selected ids.

use std::fmt;

use oc_error::DbError;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::open;
use crate::retention::RetentionReport;
use crate::session::Tokens;

const SSE_AGGREGATE_PREFIX: &str = "sse:";

/// The ten schema tables whose rows can be attributed to a selected session set.
pub const PRUNE_TABLES: [&str; 10] = [
    "session_context_epoch",
    "session_input",
    "session_message",
    "todo",
    "part",
    "message",
    "session_share",
    "session",
    "event_sequence",
    "event",
];

/// Local rows are removed in this order before the final global `part` orphan sweep.
///
/// Explicitly naming every table avoids making destructive correctness depend on
/// which foreign-key cascades happen to exist in one schema revision.
pub const DELETE_ORDER: [&str; 10] = PRUNE_TABLES;

/// A reversible change to `session.time_archived`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveChange {
    /// Mark every selected row archived at one operator-supplied instant.
    Set {
        /// Unix milliseconds written to `time_archived`.
        at_ms: i64,
    },
    /// Clear the marker without recreating any data.
    Clear,
}

/// The operation applied to todo 81's already descendant-closed selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PruneMode {
    /// Read and report only. This is intentionally the default.
    #[default]
    Preview,
    /// Change only the reversible archive marker.
    Archive(ArchiveChange),
    /// Irreversibly remove local rows after confirmation and remote-unshare checks.
    Delete,
}

/// Operator intent for one prune invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneRequest {
    /// Requested operation; omitted means [`PruneMode::Preview`].
    pub mode: PruneMode,
    /// The separate destructive acknowledgement required by delete.
    pub confirm: bool,
    /// Permit local deletion after remote unshare failure, with a warning.
    pub force: bool,
}

impl PruneRequest {
    /// Build an unconfirmed delete request, which is still safe until
    /// [`PruneRequest::confirmed`] is applied.
    #[must_use]
    pub const fn delete() -> Self {
        Self {
            mode: PruneMode::Delete,
            confirm: false,
            force: false,
        }
    }

    /// Build a reversible archive request.
    #[must_use]
    pub const fn archive_at(at_ms: i64) -> Self {
        Self {
            mode: PruneMode::Archive(ArchiveChange::Set { at_ms }),
            confirm: false,
            force: false,
        }
    }

    /// Build the inverse of [`PruneRequest::archive_at`].
    #[must_use]
    pub const fn restore_archive() -> Self {
        Self {
            mode: PruneMode::Archive(ArchiveChange::Clear),
            confirm: false,
            force: false,
        }
    }

    /// Supply the acknowledgement without conflating it with selecting delete.
    #[must_use]
    pub const fn confirmed(mut self) -> Self {
        self.confirm = true;
        self
    }

    /// Cross only the remote-unshare refusal, never the confirmation gate.
    #[must_use]
    pub const fn forced(mut self) -> Self {
        self.force = true;
        self
    }
}

/// The local share metadata available to a remote adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSession {
    /// Session being removed remotely.
    pub session_id: String,
    /// URL cached directly on `session`, when present.
    pub share_url: Option<String>,
    /// Durable remote identifier from `session_share`, when present.
    pub share_id: Option<String>,
    /// URL stored with the durable share credentials, when present.
    pub share_record_url: Option<String>,
}

/// A remote failure kept separate from database failures so `--force` can
/// cross exactly this boundary and no other one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnshareError {
    detail: String,
}

impl UnshareError {
    /// Preserve the adapter's actionable failure detail.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// The detail rendered into a refusal or force warning.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for UnshareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for UnshareError {}

/// Injectable remote boundary; tests never need a real network call.
pub trait RemoteUnshare {
    /// Remove the remote copy before local rows are touched.
    fn unshare(&self, session: &SharedSession) -> Result<(), UnshareError>;
}

/// Count and logical payload size for one related table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableImpact {
    /// Stable schema table name.
    pub table: &'static str,
    /// Rows the delete path will remove, including the final orphan sweep.
    pub rows: u64,
    /// Sum of the selected rows' non-null column payload lengths in bytes.
    pub bytes: u64,
}

/// The complete loss report shown before an operator chooses archive or delete.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunePreview {
    /// The exact ids supplied by retention, deduplicated without another tree walk.
    pub session_ids: Vec<String>,
    /// One entry for each table in [`PRUNE_TABLES`], including zeroes.
    pub tables: Vec<TableImpact>,
    /// Sum of all table row counts.
    pub total_rows: u64,
    /// Sum of all logical table payload bytes.
    pub total_bytes: u64,
    /// Session cost that will be discarded.
    pub cost: f64,
    /// Session token accounting that will be discarded.
    pub tokens: Tokens,
}

impl PrunePreview {
    /// Find one table without callers depending on vector position.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&TableImpact> {
        self.tables.iter().find(|impact| impact.table == name)
    }
}

/// What was applied, paired with the preview captured inside the same local
/// transaction for mutating modes.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneOutcome {
    /// Operation that completed.
    pub mode: PruneMode,
    /// Rows and value observed before mutation.
    pub preview: PrunePreview,
    /// Selected session rows archived, restored, or deleted.
    pub changed_sessions: u64,
    /// Explicit warnings produced only when force crosses an unshare failure.
    pub warnings: Vec<String>,
}

/// A refusal is data: callers must distinguish missing confirmation from the
/// narrowly forceable remote failure and from an ordinary database error.
#[derive(Debug)]
pub enum PruneError {
    /// SQLite could not complete a read, transaction, or mutation.
    Database(DbError),
    /// Delete was selected without the independent acknowledgement.
    ConfirmationRequired,
    /// A remote share may survive, so local history remains untouched.
    RemoteUnshareFailed {
        /// Shared session whose remote copy could not be removed.
        session_id: String,
        /// Adapter-provided cause.
        detail: String,
    },
}

impl fmt::Display for PruneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::ConfirmationRequired => {
                formatter.write_str("session delete requires explicit confirmation")
            }
            Self::RemoteUnshareFailed { session_id, detail } => write!(
                formatter,
                "remote unshare failed for shared session {session_id}: {detail}; local rows were not deleted"
            ),
        }
    }
}

impl std::error::Error for PruneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::ConfirmationRequired | Self::RemoteUnshareFailed { .. } => None,
        }
    }
}

impl From<DbError> for PruneError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

/// Run preview, reversible archive, or confirmed delete.
///
/// Delete takes an `IMMEDIATE` transaction before inspecting shares. That keeps
/// another writer from publishing a share between the remote check and the local
/// delete; all local statements then commit or roll back together.
///
/// # Errors
///
/// [`PruneError::ConfirmationRequired`] before any remote call when delete is not
/// confirmed, [`PruneError::RemoteUnshareFailed`] when a shared session cannot be
/// unpublished, or [`PruneError::Database`] for SQLite failures.
pub fn execute(
    connection: &mut Connection,
    selection: &RetentionReport,
    request: &PruneRequest,
    remote: &dyn RemoteUnshare,
) -> Result<PruneOutcome, PruneError> {
    match request.mode {
        PruneMode::Preview => Ok(PruneOutcome {
            mode: PruneMode::Preview,
            preview: preview(connection, selection)?,
            changed_sessions: 0,
            warnings: Vec::new(),
        }),
        PruneMode::Archive(change) => {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(open::map_error)?;
            let before = preview(&transaction, selection)?;
            let changed_sessions = archive_in_transaction(&transaction, selection, change)?;
            transaction.commit().map_err(open::map_error)?;
            Ok(PruneOutcome {
                mode: PruneMode::Archive(change),
                preview: before,
                changed_sessions,
                warnings: Vec::new(),
            })
        }
        PruneMode::Delete => {
            if !request.confirm {
                return Err(PruneError::ConfirmationRequired);
            }
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(open::map_error)?;
            let outcome = delete_in_transaction(
                &transaction,
                selection,
                request.confirm,
                request.force,
                remote,
            )?;
            transaction.commit().map_err(open::map_error)?;
            Ok(outcome)
        }
    }
}

/// Build the inert loss report for todo 81's selected set.
///
/// `bytes` is a logical payload measure, not allocated SQLite pages: page-level
/// accounting cannot attribute shared B-tree pages to a subset of rows. The sum
/// includes every non-null column's byte length and is stable before/after a
/// later delete on an unchanged database.
///
/// # Errors
///
/// [`PruneError::Database`] if related rows cannot be read.
pub fn preview(
    connection: &Connection,
    selection: &RetentionReport,
) -> Result<PrunePreview, PruneError> {
    let session_ids = selected_ids(selection);
    let selected_json = ids_json(&session_ids)?;
    let aggregate_json = ids_json(&event_aggregate_ids(&session_ids))?;
    let mut tables = Vec::with_capacity(TABLE_SPECS.len());

    for spec in TABLE_SPECS {
        let (predicate, binding) = match spec.relation {
            Relation::SessionId => (
                "session_id IN (SELECT value FROM json_each(?1))",
                selected_json.as_str(),
            ),
            Relation::SessionPrimaryKey => (
                "id IN (SELECT value FROM json_each(?1))",
                selected_json.as_str(),
            ),
            Relation::PartWithOrphanSweep => (
                "session_id IN (SELECT value FROM json_each(?1)) OR NOT EXISTS \
                 (SELECT 1 FROM session WHERE session.id = part.session_id)",
                selected_json.as_str(),
            ),
            Relation::Aggregate => (
                "aggregate_id IN (SELECT value FROM json_each(?1))",
                aggregate_json.as_str(),
            ),
        };
        tables.push(table_impact(connection, spec, predicate, binding)?);
    }

    let (cost, input, output, reasoning, cache_read, cache_write) = connection
        .query_row(
            "SELECT COALESCE(SUM(cost), 0),
                    COALESCE(SUM(tokens_input), 0),
                    COALESCE(SUM(tokens_output), 0),
                    COALESCE(SUM(tokens_reasoning), 0),
                    COALESCE(SUM(tokens_cache_read), 0),
                    COALESCE(SUM(tokens_cache_write), 0)
             FROM session WHERE id IN (SELECT value FROM json_each(?1))",
            [selected_json.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(open::map_error)?;
    let total_rows = tables.iter().map(|impact| impact.rows).sum();
    let total_bytes = tables.iter().map(|impact| impact.bytes).sum();

    Ok(PrunePreview {
        session_ids,
        tables,
        total_rows,
        total_bytes,
        cost,
        tokens: Tokens {
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
        },
    })
}

/// Execute the irreversible statements inside a caller-owned transaction.
///
/// This seam exists so abnormal termination can be tested honestly by rolling
/// the transaction back after every delete statement has run. It repeats the
/// confirmation guard so bypassing [`execute`] cannot bypass acknowledgement.
///
/// # Errors
///
/// The same refusal and database errors as [`execute`].
pub fn delete_in_transaction(
    transaction: &Transaction<'_>,
    selection: &RetentionReport,
    confirm: bool,
    force: bool,
    remote: &dyn RemoteUnshare,
) -> Result<PruneOutcome, PruneError> {
    if !confirm {
        return Err(PruneError::ConfirmationRequired);
    }

    let before = preview(transaction, selection)?;
    let selected_json = ids_json(&before.session_ids)?;
    let aggregate_json = ids_json(&event_aggregate_ids(&before.session_ids))?;
    let mut warnings = Vec::new();

    for shared in shared_sessions(transaction, &selected_json)? {
        if let Err(error) = remote.unshare(&shared) {
            if !force {
                return Err(PruneError::RemoteUnshareFailed {
                    session_id: shared.session_id,
                    detail: error.detail,
                });
            }
            warnings.push(format!(
                "remote unshare failed for shared session {}: {}; local rows were deleted because --force was supplied and the remote copy may survive",
                shared.session_id, error.detail
            ));
        }
    }

    let mut changed_sessions = 0;
    for spec in TABLE_SPECS {
        let (predicate, binding) = match spec.relation {
            Relation::SessionId | Relation::PartWithOrphanSweep => (
                "session_id IN (SELECT value FROM json_each(?1))",
                selected_json.as_str(),
            ),
            Relation::SessionPrimaryKey => (
                "id IN (SELECT value FROM json_each(?1))",
                selected_json.as_str(),
            ),
            Relation::Aggregate => (
                "aggregate_id IN (SELECT value FROM json_each(?1))",
                aggregate_json.as_str(),
            ),
        };
        let sql = format!("DELETE FROM {} WHERE {predicate}", spec.name);
        let changed = transaction
            .execute(&sql, [binding])
            .map_err(open::map_error)?;
        if spec.name == "session" {
            changed_sessions = count_from_usize(changed)?;
        }
    }

    transaction
        .execute(
            "DELETE FROM part WHERE NOT EXISTS
             (SELECT 1 FROM session WHERE session.id = part.session_id)",
            [],
        )
        .map_err(open::map_error)?;

    Ok(PruneOutcome {
        mode: PruneMode::Delete,
        preview: before,
        changed_sessions,
        warnings,
    })
}

fn archive_in_transaction(
    transaction: &Transaction<'_>,
    selection: &RetentionReport,
    change: ArchiveChange,
) -> Result<u64, PruneError> {
    let selected_json = ids_json(&selected_ids(selection))?;
    let changed = match change {
        ArchiveChange::Set { at_ms } => transaction.execute(
            "UPDATE session SET time_archived = ?2
             WHERE id IN (SELECT value FROM json_each(?1))",
            params![selected_json, at_ms],
        ),
        ArchiveChange::Clear => transaction.execute(
            "UPDATE session SET time_archived = NULL
             WHERE id IN (SELECT value FROM json_each(?1))",
            [selected_json],
        ),
    }
    .map_err(open::map_error)?;
    count_from_usize(changed)
}

fn selected_ids(selection: &RetentionReport) -> Vec<String> {
    let mut ids = selection
        .selected
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

fn event_aggregate_ids(session_ids: &[String]) -> Vec<String> {
    session_ids
        .iter()
        .flat_map(|id| [id.clone(), format!("{SSE_AGGREGATE_PREFIX}{id}")])
        .collect()
}

fn ids_json(ids: &[String]) -> Result<String, PruneError> {
    serde_json::to_string(ids)
        .map_err(|source| DbError::Query {
            source: Box::new(source),
        })
        .map_err(PruneError::from)
}

fn shared_sessions(
    connection: &Connection,
    selected_json: &str,
) -> Result<Vec<SharedSession>, PruneError> {
    let mut statement = connection
        .prepare(
            "SELECT session.id, session.share_url, session_share.id, session_share.url
             FROM session
             LEFT JOIN session_share ON session_share.session_id = session.id
             WHERE session.id IN (SELECT value FROM json_each(?1))
               AND (session.share_url IS NOT NULL OR session_share.session_id IS NOT NULL)
             ORDER BY session.id ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([selected_json], |row| {
            Ok(SharedSession {
                session_id: row.get(0)?,
                share_url: row.get(1)?,
                share_id: row.get(2)?,
                share_record_url: row.get(3)?,
            })
        })
        .map_err(open::map_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)
        .map_err(PruneError::from)
}

fn table_impact(
    connection: &Connection,
    spec: TableSpec,
    predicate: &str,
    binding: &str,
) -> Result<TableImpact, PruneError> {
    let bytes = spec
        .columns
        .iter()
        .map(|column| format!("COALESCE(length(CAST({column} AS BLOB)), 0)"))
        .collect::<Vec<_>>()
        .join(" + ");
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM({bytes}), 0) FROM {} WHERE {predicate}",
        spec.name
    );
    let (rows, bytes): (i64, i64) = connection
        .query_row(&sql, [binding], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(open::map_error)?;
    Ok(TableImpact {
        table: spec.name,
        rows: count_from_i64(rows)?,
        bytes: count_from_i64(bytes)?,
    })
}

fn count_from_usize(value: usize) -> Result<u64, PruneError> {
    u64::try_from(value).map_err(|source| {
        PruneError::from(DbError::Query {
            source: Box::new(source),
        })
    })
}

fn count_from_i64(value: i64) -> Result<u64, PruneError> {
    u64::try_from(value).map_err(|source| {
        PruneError::from(DbError::Query {
            source: Box::new(source),
        })
    })
}

#[derive(Clone, Copy)]
enum Relation {
    SessionId,
    SessionPrimaryKey,
    PartWithOrphanSweep,
    Aggregate,
}

#[derive(Clone, Copy)]
struct TableSpec {
    name: &'static str,
    relation: Relation,
    columns: &'static [&'static str],
}

const TABLE_SPECS: [TableSpec; 10] = [
    TableSpec {
        name: "session_context_epoch",
        relation: Relation::SessionId,
        columns: &["session_id", "baseline", "snapshot", "baseline_seq"],
    },
    TableSpec {
        name: "session_input",
        relation: Relation::SessionId,
        columns: &[
            "id",
            "session_id",
            "prompt",
            "delivery",
            "admitted_seq",
            "promoted_seq",
            "time_created",
        ],
    },
    TableSpec {
        name: "session_message",
        relation: Relation::SessionId,
        columns: &[
            "id",
            "session_id",
            "type",
            "seq",
            "time_created",
            "time_updated",
            "data",
        ],
    },
    TableSpec {
        name: "todo",
        relation: Relation::SessionId,
        columns: &[
            "session_id",
            "content",
            "status",
            "priority",
            "position",
            "time_created",
            "time_updated",
        ],
    },
    TableSpec {
        name: "part",
        relation: Relation::PartWithOrphanSweep,
        columns: &[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
            "data",
        ],
    },
    TableSpec {
        name: "message",
        relation: Relation::SessionId,
        columns: &["id", "session_id", "time_created", "time_updated", "data"],
    },
    TableSpec {
        name: "session_share",
        relation: Relation::SessionId,
        columns: &[
            "session_id",
            "id",
            "secret",
            "url",
            "time_created",
            "time_updated",
        ],
    },
    TableSpec {
        name: "session",
        relation: Relation::SessionPrimaryKey,
        columns: &[
            "id",
            "project_id",
            "workspace_id",
            "parent_id",
            "slug",
            "directory",
            "path",
            "title",
            "version",
            "share_url",
            "summary_additions",
            "summary_deletions",
            "summary_files",
            "summary_diffs",
            "metadata",
            "cost",
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
            "revert",
            "permission",
            "agent",
            "model",
            "time_created",
            "time_updated",
            "time_compacting",
            "time_archived",
        ],
    },
    TableSpec {
        name: "event_sequence",
        relation: Relation::Aggregate,
        columns: &["aggregate_id", "seq", "owner_id"],
    },
    TableSpec {
        name: "event",
        relation: Relation::Aggregate,
        columns: &["id", "aggregate_id", "seq", "type", "data"],
    },
];
