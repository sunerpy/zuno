//! Shared preview, archive, and delete orchestration for session retention.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::Serialize;
use zuno_error::DbError;

use crate::artifact_gc::{
    self, ArtifactGcError, ArtifactGcMode, ArtifactGcPaths, ArtifactGcRequest, ArtifactKind,
    ReclaimReason,
};
use crate::prune::{self, ArchiveChange, PruneError, PruneMode, PruneRequest, RemoteUnshare};
use crate::retention::{
    self, ExclusionReason, LivenessProbe, ProtectionReason, RetentionKey, RetentionRequest,
    RetentionScope,
};

/// Project boundary applied before age selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPruneScope {
    /// The project resolved from the caller's current directory.
    CurrentProject(String),
    /// One explicitly resolved project id.
    Project(String),
    /// Every project in the database.
    AllProjects,
}

/// Operation requested after retention selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPruneAction {
    /// Report database and artifact impact without mutation.
    Preview,
    /// Set `session.time_archived` to the supplied Unix millisecond instant.
    Archive {
        /// Archive marker written to every selected session.
        at_ms: i64,
    },
    /// Delete selected database rows and their attributable artifacts.
    Delete,
}

/// Complete input shared by the CLI and HTTP adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPruneRequest {
    /// Sessions strictly older than this many whole days are eligible.
    pub older_than_days: u64,
    /// Projects eligible for selection.
    pub scope: SessionPruneScope,
    /// Timestamp used for the age predicate.
    pub key: RetentionKey,
    /// Preview, archive, or delete.
    pub action: SessionPruneAction,
    /// Cross the default public-share protection.
    pub include_shared: bool,
    /// Cross the no-server one-hour recency guard.
    pub include_recent: bool,
    /// Continue local deletion when remote unshare fails.
    pub force: bool,
    /// Independent destructive acknowledgement required by delete.
    pub confirm_delete: bool,
    /// Deterministic clock value in Unix milliseconds.
    pub now_ms: i64,
}

/// Observable stage of one service invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    /// Retention selection has started.
    Selecting,
    /// The descendant-closed selection is available.
    Selected,
    /// Database impact is being measured or applied.
    Database,
    /// External artifact impact is being measured or applied.
    Artifacts,
    /// The requested operation completed.
    Completed,
}

/// One progress notification suitable for fan-out to user interfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPruneProgress {
    /// Current orchestration phase.
    pub phase: ProgressPhase,
    /// Selected session count when known.
    pub selected_sessions: Option<u64>,
}

/// Adapter boundary for progress publication.
pub trait SessionPruneProgressSink {
    /// Receive one ordered progress notification.
    fn emit(&mut self, progress: SessionPruneProgress);
}

impl<F> SessionPruneProgressSink for F
where
    F: FnMut(SessionPruneProgress),
{
    fn emit(&mut self, progress: SessionPruneProgress) {
        self(progress);
    }
}

/// An age-eligible root refused by a protection rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPruneExclusion {
    /// Refused session id.
    pub session_id: String,
    /// Stable human-readable protection descriptions.
    pub reasons: Vec<String>,
}

/// Per-table impact in a prune preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPruneTableImpact {
    /// Schema table name.
    pub table: String,
    /// Rows affected by delete.
    pub rows: u64,
    /// Logical payload bytes affected by delete.
    pub bytes: u64,
}

/// Token totals attached to selected sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SessionPruneTokens {
    /// Prompt tokens.
    pub input: i64,
    /// Completion tokens.
    pub output: i64,
    /// Reasoning tokens.
    pub reasoning: i64,
    /// Prompt-cache reads.
    pub cache_read: i64,
    /// Prompt-cache writes.
    pub cache_write: i64,
}

/// Database loss projected before mutation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionPruneDatabaseImpact {
    /// Impact for every prune-owned table, including zero counts.
    pub tables: Vec<SessionPruneTableImpact>,
    /// Sum of affected table rows.
    pub total_rows: u64,
    /// Sum of logical payload bytes.
    pub total_bytes: u64,
    /// Session cost that archive preserves and delete discards.
    pub cost: f64,
    /// Session token accounting affected by the operation.
    pub tokens: SessionPruneTokens,
}

/// One external artifact projected or removed by the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPruneArtifact {
    /// Filesystem path in wire form: a string, with forward slashes on every platform.
    pub path: String,
    /// File content bytes below the path.
    pub bytes: u64,
    /// Stable artifact class.
    pub kind: String,
    /// Safety proof for reclamation.
    pub reason: String,
    /// Whether this invocation removed the path.
    pub removed: bool,
}

/// External artifact impact paired with the database preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct SessionPruneArtifactImpact {
    /// Deterministically ordered candidates.
    pub items: Vec<SessionPruneArtifact>,
    /// Sum of candidate content bytes.
    pub total_bytes: u64,
}

/// Stable result returned unchanged by both public surfaces.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionPruneReport {
    /// Operation that completed.
    pub action: SessionPruneAction,
    /// Descendant-closed selected ids in deterministic order.
    pub selected_session_ids: Vec<String>,
    /// Protected age-eligible roots.
    pub excluded: Vec<SessionPruneExclusion>,
    /// Database impact captured before mutation.
    pub database: SessionPruneDatabaseImpact,
    /// External artifact impact.
    pub artifacts: SessionPruneArtifactImpact,
    /// Session rows archived or deleted.
    pub changed_sessions: u64,
    /// Operator-visible safety and remote-unshare warnings.
    pub warnings: Vec<String>,
}

/// Classified failure from the shared orchestration layer.
#[derive(Debug)]
pub enum SessionPruneError {
    /// Retention selection failed.
    Selection(DbError),
    /// Database archive or delete failed.
    Prune(PruneError),
    /// External artifact discovery or deletion failed.
    Artifacts(ArtifactGcError),
    /// Stable JSON encoding failed.
    Encode(serde_json::Error),
}

impl fmt::Display for SessionPruneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection(error) => error.fmt(formatter),
            Self::Prune(error) => error.fmt(formatter),
            Self::Artifacts(error) => error.fmt(formatter),
            Self::Encode(error) => {
                write!(formatter, "session prune report encoding failed: {error}")
            }
        }
    }
}

impl std::error::Error for SessionPruneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Selection(error) => Some(error),
            Self::Prune(error) => Some(error),
            Self::Artifacts(error) => Some(error),
            Self::Encode(error) => Some(error),
        }
    }
}

/// Select sessions, apply the requested database operation, and account for artifacts.
///
/// Preview is structurally non-mutating. Delete still requires
/// [`SessionPruneRequest::confirm_delete`] even when an adapter has already
/// performed its own confirmation flow.
///
/// # Errors
///
/// Returns a classified selection, prune, artifact, or encoding failure.
pub fn execute(
    connection: &mut Connection,
    paths: &ArtifactGcPaths,
    request: &SessionPruneRequest,
    liveness: &impl LivenessProbe,
    remote: &dyn RemoteUnshare,
    progress: &mut impl SessionPruneProgressSink,
) -> Result<SessionPruneReport, SessionPruneError> {
    let visibility_warning = artifact_visibility_warning(connection, request.action)?;
    progress.emit(progress_event(ProgressPhase::Selecting, None));
    let retention_request = retention_request(request);
    let selection = retention::select(connection, &retention_request, liveness)
        .map_err(SessionPruneError::Selection)?;
    let selected_count = u64::try_from(selection.selected.len()).unwrap_or(u64::MAX);
    progress.emit(progress_event(
        ProgressPhase::Selected,
        Some(selected_count),
    ));

    progress.emit(progress_event(
        ProgressPhase::Database,
        Some(selected_count),
    ));
    let prune_request = prune_request(request);
    let outcome = prune::execute(connection, &selection, &prune_request, remote)
        .map_err(SessionPruneError::Prune)?;

    progress.emit(progress_event(
        ProgressPhase::Artifacts,
        Some(selected_count),
    ));
    let (artifacts, skipped_roots) = match request.action {
        SessionPruneAction::Archive { .. } => (SessionPruneArtifactImpact::default(), Vec::new()),
        SessionPruneAction::Preview | SessionPruneAction::Delete
            if outcome.preview.session_ids.is_empty() =>
        {
            (SessionPruneArtifactImpact::default(), Vec::new())
        }
        SessionPruneAction::Preview | SessionPruneAction::Delete => {
            let mut artifact_request = ArtifactGcRequest::new(
                outcome.preview.session_ids.clone(),
                system_time(request.now_ms),
            );
            if request.action == SessionPruneAction::Delete {
                artifact_request.mode = ArtifactGcMode::Delete;
            }
            let report = artifact_gc::execute(connection, paths, &artifact_request)
                .map_err(SessionPruneError::Artifacts)?;
            // A root the pass could not read is reported, not swallowed: those files were
            // left in place, and after a delete the sessions that own them are already
            // gone from the database, so this line is the only record that names them.
            let skipped = report
                .skipped_roots
                .iter()
                .map(ToString::to_string)
                .collect();
            (artifact_impact(report), skipped)
        }
    };

    let mut warnings = outcome.warnings;
    if let Some(warning) = visibility_warning {
        warnings.push(warning);
    }
    warnings.extend(skipped_roots);
    let report = SessionPruneReport {
        action: request.action,
        selected_session_ids: outcome.preview.session_ids.clone(),
        excluded: selection
            .excluded
            .iter()
            .map(|item| SessionPruneExclusion {
                session_id: item.id.clone(),
                reasons: item.reasons.iter().map(exclusion_reason).collect(),
            })
            .collect(),
        database: database_impact(&outcome.preview),
        artifacts,
        changed_sessions: outcome.changed_sessions,
        warnings,
    };
    progress.emit(progress_event(
        ProgressPhase::Completed,
        Some(selected_count),
    ));
    Ok(report)
}

fn artifact_visibility_warning(
    connection: &Connection,
    action: SessionPruneAction,
) -> Result<Option<String>, SessionPruneError> {
    if matches!(action, SessionPruneAction::Archive { .. }) {
        return Ok(None);
    }
    match artifact_gc::ensure_visible_session_owners(connection) {
        Ok(()) => Ok(None),
        Err(ArtifactGcError::NoVisibleSessions {
            database,
            session_count,
        }) => Ok(Some(format!(
            "`{database}` contains {session_count} sessions; artifact reclamation is skipped because shared snapshot stores cannot be attributed and may belong to another channel's database."
        ))),
        Err(error) => Err(SessionPruneError::Artifacts(error)),
    }
}

/// Encode the canonical compact JSON used by CLI and HTTP.
///
/// # Errors
///
/// Returns [`SessionPruneError::Encode`] if a non-finite value cannot be encoded.
pub fn to_json_bytes(report: &SessionPruneReport) -> Result<Vec<u8>, SessionPruneError> {
    serde_json::to_vec(report).map_err(SessionPruneError::Encode)
}

fn retention_request(request: &SessionPruneRequest) -> RetentionRequest {
    let mut retention = RetentionRequest::new(
        request.older_than_days,
        match &request.scope {
            SessionPruneScope::CurrentProject(id) => RetentionScope::CurrentProject(id.clone()),
            SessionPruneScope::Project(id) => RetentionScope::Project(id.clone()),
            SessionPruneScope::AllProjects => RetentionScope::AllProjects,
        },
        request.now_ms,
    );
    retention.key = request.key;
    retention.include_shared = request.include_shared;
    retention.include_recent = request.include_recent;
    retention
}

fn prune_request(request: &SessionPruneRequest) -> PruneRequest {
    PruneRequest {
        mode: match request.action {
            SessionPruneAction::Preview => PruneMode::Preview,
            SessionPruneAction::Archive { at_ms } => {
                PruneMode::Archive(ArchiveChange::Set { at_ms })
            }
            SessionPruneAction::Delete => PruneMode::Delete,
        },
        confirm: request.confirm_delete,
        force: request.force,
    }
}

fn database_impact(preview: &prune::PrunePreview) -> SessionPruneDatabaseImpact {
    SessionPruneDatabaseImpact {
        tables: preview
            .tables
            .iter()
            .map(|table| SessionPruneTableImpact {
                table: table.table.to_owned(),
                rows: table.rows,
                bytes: table.bytes,
            })
            .collect(),
        total_rows: preview.total_rows,
        total_bytes: preview.total_bytes,
        cost: preview.cost,
        tokens: SessionPruneTokens {
            input: preview.tokens.input,
            output: preview.tokens.output,
            reasoning: preview.tokens.reasoning,
            cache_read: preview.tokens.cache_read,
            cache_write: preview.tokens.cache_write,
        },
    }
}

fn artifact_impact(report: artifact_gc::ArtifactGcReport) -> SessionPruneArtifactImpact {
    SessionPruneArtifactImpact {
        items: report
            .artifacts
            .into_iter()
            .map(|artifact| SessionPruneArtifact {
                // Rendered the way every other path Zuno publishes is rendered. The
                // field is serialized into `session prune`'s JSON, and
                // `to_string_lossy` handed a Windows reader backslashes where the same
                // report's neighbours, the tool results naming these very files, and the
                // ACP and HTTP surfaces all use forward slashes. A consumer that joins
                // or compares the two forms sees two different files.
                path: zuno_paths::wire_path(&artifact.path),
                bytes: artifact.bytes,
                kind: artifact_kind(artifact.kind).to_owned(),
                reason: reclaim_reason(&artifact.reason),
                removed: artifact.removed,
            })
            .collect(),
        total_bytes: report.total_bytes,
    }
}

fn artifact_kind(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::SnapshotStore => "snapshot_store",
        ArtifactKind::ToolOutput => "tool_output",
        ArtifactKind::AttachmentObject => "attachment_object",
    }
}

fn reclaim_reason(reason: &ReclaimReason) -> String {
    match reason {
        ReclaimReason::UnreferencedSnapshot => "unreferenced_snapshot".to_owned(),
        ReclaimReason::DeletedSession(id) => format!("deleted_session:{id}"),
        ReclaimReason::UnattributedToolOutputExpired => {
            "unattributed_tool_output_expired".to_owned()
        }
        ReclaimReason::UnreferencedAttachment => "unreferenced_attachment".to_owned(),
    }
}

fn exclusion_reason(reason: &ExclusionReason) -> String {
    match reason {
        ExclusionReason::Protected(protection) => protection_reason(protection),
        ExclusionReason::ProtectedDescendant {
            descendant_id,
            protections,
        } => format!(
            "protected descendant {descendant_id}: {}",
            protections
                .iter()
                .map(protection_reason)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn protection_reason(reason: &ProtectionReason) -> String {
    match reason {
        ProtectionReason::Shared => "shared; pass include_shared to cross".to_owned(),
        ProtectionReason::Compacting => "compaction is in progress".to_owned(),
        ProtectionReason::Active => "reported active by a reachable server".to_owned(),
        ProtectionReason::Recent { .. } => {
            "recently updated; pass include_recent to cross".to_owned()
        }
    }
}

fn progress_event(phase: ProgressPhase, selected_sessions: Option<u64>) -> SessionPruneProgress {
    SessionPruneProgress {
        phase,
        selected_sessions,
    }
}

fn system_time(now_ms: i64) -> SystemTime {
    if now_ms >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::from_millis(now_ms.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_millis(now_ms.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_gc::{ArtifactGcReport, ReclaimedArtifact};
    use std::path::Path;

    #[test]
    fn a_projected_artifact_path_is_rendered_in_wire_form_on_every_platform() {
        // The field is a `String` in `session prune`'s JSON, so its separator is a
        // published detail. Rendering it natively made the same report disagree with
        // itself on Windows: this path arrived with backslashes while every neighbouring
        // surface naming the very same files used forward slashes.
        let path = Path::new("checkout")
            .join(".zuno")
            .join("tool-output")
            .join("tool_ses_old_call.jsonl");
        let impact = artifact_impact(ArtifactGcReport {
            artifacts: vec![ReclaimedArtifact {
                path,
                bytes: 12,
                kind: ArtifactKind::ToolOutput,
                reason: ReclaimReason::DeletedSession("ses_old".to_owned()),
                removed: false,
            }],
            total_bytes: 12,
            skipped_roots: Vec::new(),
        });

        assert_eq!(
            impact.items[0].path, "checkout/.zuno/tool-output/tool_ses_old_call.jsonl",
            "a published artifact path uses forward slashes on every platform"
        );
    }
}
