//! Conservative garbage collection for artifacts stored outside the Zuno database.
//!
//! Filesystem deletion is intentionally separate from [`crate::prune`]'s database
//! transaction: SQLite can roll a transaction back, but it cannot restore a removed
//! directory. This module instead re-reads the surviving rows under an `IMMEDIATE`
//! transaction and keeps that write lock until every filesystem decision is complete.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, TransactionBehavior};
use zuno_error::DbError;
use zuno_snapshot::{SessionRef, SnapshotError};

use crate::open;

/// Default retention window for unattributable tool output.
pub const DEFAULT_TOOL_OUTPUT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The serialized prefix of every `zuno_attachment::AttachmentId`.
const ATTACHMENT_ID_PREFIX: &[u8] = b"sha256:";
/// Hex digits in one attachment digest, which is also the object file-name stem.
const DIGEST_HEX_LEN: usize = 64;
/// The caller identity `zuno_tool`'s store reports in an error it raises for this pass.
const TOOL_OUTPUT_READER: &str = "session_prune";

/// Whether a pass only reports candidates or also removes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArtifactGcMode {
    /// Discover reclaimable artifacts without changing the filesystem.
    #[default]
    Preview,
    /// Remove every safely attributable candidate.
    Delete,
}

/// Explicit roots keep tests and callers away from process-global path state.
///
/// Tool output has a second class of root that no caller can name: the store inside each
/// checkout. Those are read from the database being pruned, by
/// [`tool_output_roots`], because `session prune` reclaims across every project at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactGcPaths {
    /// `$DATA/snapshot`.
    pub snapshots: PathBuf,
    /// `$DATA/tool-output`, the store shared by every session.
    pub tool_output: PathBuf,
    /// `$DATA/attachments`.
    pub attachments: PathBuf,
}

impl ArtifactGcPaths {
    /// Derive every managed root from an explicit data directory.
    #[must_use]
    pub fn from_data_root(data: &Path) -> Self {
        Self {
            snapshots: data.join("snapshot"),
            tool_output: data.join("tool-output"),
            attachments: data.join("attachments"),
        }
    }

    /// Derive every managed root from a resolved application layout.
    #[must_use]
    pub fn in_layout(layout: &zuno_paths::Layout) -> Self {
        Self {
            snapshots: layout.snapshot_root(),
            tool_output: layout.tool_output(),
            attachments: layout.data().join("attachments"),
        }
    }
}

/// One GC invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactGcRequest {
    /// Safe by default: omission means preview.
    pub mode: ArtifactGcMode,
    /// Session ids selected by the preceding prune operation.
    pub deleted_session_ids: Vec<String>,
    /// Age backstop for foreign, unattributable `tool_*` files.
    pub tool_output_retention: Duration,
    /// Injected clock for deterministic age decisions.
    pub now: SystemTime,
}

impl ArtifactGcRequest {
    /// Build a preview request with the oracle's seven-day age window.
    #[must_use]
    pub fn new<I, S>(deleted_session_ids: I, now: SystemTime) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            mode: ArtifactGcMode::Preview,
            deleted_session_ids: deleted_session_ids.into_iter().map(Into::into).collect(),
            tool_output_retention: DEFAULT_TOOL_OUTPUT_RETENTION,
            now,
        }
    }

    /// Select destructive execution.
    #[must_use]
    pub const fn deleting(mut self) -> Self {
        self.mode = ArtifactGcMode::Delete;
        self
    }

    /// Override the age backstop for unattributable tool output.
    #[must_use]
    pub const fn with_tool_output_retention(mut self, retention: Duration) -> Self {
        self.tool_output_retention = retention;
        self
    }
}

/// The class of one reclaimable path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A whole `(project, worktree)` snapshot store.
    SnapshotStore,
    /// One persisted tool output file.
    ToolOutput,
    /// One canonical or route-derived image object.
    AttachmentObject,
}

/// Why one path is safe to reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimReason {
    /// No surviving database row references the snapshot store.
    UnreferencedSnapshot,
    /// The path is precisely attributable to a confirmed-deleted session.
    DeletedSession(String),
    /// A foreign tool-output filename has no session attribution and is older
    /// than the configured backstop.
    UnattributedToolOutputExpired,
    /// No surviving durable part in this database references the object digest.
    UnreferencedAttachment,
}

/// One candidate observed during a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimedArtifact {
    /// Path that was measured and optionally removed.
    pub path: PathBuf,
    /// File content bytes below that path, without following symlinks.
    pub bytes: u64,
    /// Artifact class.
    pub kind: ArtifactKind,
    /// Safety proof for reclamation.
    pub reason: ReclaimReason,
    /// Whether this pass actually removed the path.
    pub removed: bool,
}

/// A tool-output root this pass could not read, and why.
///
/// Evidence rather than a failure: the root stays untouched and every other root is still
/// swept. `operation` and `reason` are the original I/O attempt and its message, so an
/// operator can tell an offline mount from a permission problem without re-running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRoot {
    /// The tool-output store directory that was being swept.
    pub root: PathBuf,
    /// The exact path whose inspection failed, which may be a file inside `root`.
    pub path: PathBuf,
    /// The I/O attempt that failed, matching [`ArtifactGcError::Filesystem`].
    pub operation: &'static str,
    /// The original I/O message.
    pub reason: String,
}

impl fmt::Display for SkippedRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // This rendering is the serialized value: `session_prune` collects it straight
        // into the report's `warnings` array, beside `artifacts[].path` entries that are
        // deliberately in wire form. Rendering natively made the one line that names the
        // files left behind disagree with the list they belong to on Windows.
        write!(
            formatter,
            "tool output under {} was not swept: could not {} {} ({})",
            zuno_paths::wire_path(&self.root),
            self.operation,
            zuno_paths::wire_path(&self.path),
            self.reason
        )
    }
}

/// A whole artifact class this pass did not evaluate, and the evidence for why.
///
/// Distinct from an empty result on purpose. A caller that previewed reclaimable bytes and
/// then saw a smaller total after deleting has to be able to tell "nothing was
/// reclaimable" from "this class was never looked at", and after a delete the rows that
/// could have attributed the class are already gone, so this value is the only record that
/// those bytes are still on disk.
///
/// It is evidence, not a work item, and it deliberately names no directory to remove. The
/// class is skipped precisely because this database cannot prove which store belongs to it,
/// so an operator deleting one under `$DATA/snapshot` by hand would be making the
/// cross-database attribution judgement the pass refuses — on a directory that may belong to
/// another channel's database. Nothing has to be done: the next pass that runs against this
/// database while at least one session survives evaluates the class and reclaims it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedClass {
    /// The class left unevaluated.
    pub kind: ArtifactKind,
    /// SQLite's path for the database whose rows would have attributed it.
    pub database: String,
    /// Session rows that remain once the operation being accounted for is applied.
    pub retained_sessions: u64,
}

impl SkippedClass {
    /// A class no surviving row in this database can be attributed to.
    #[must_use]
    pub const fn unattributable(
        kind: ArtifactKind,
        database: String,
        retained_sessions: u64,
    ) -> Self {
        Self {
            kind,
            database,
            retained_sessions,
        }
    }
}

impl fmt::Display for SkippedClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Like `SkippedRoot`, this rendering is the serialized value: `session_prune`
        // forwards it into the report's `warnings` array, and it is the same sentence the
        // pre-mutation visibility check emits, so an operator sees one wording for one
        // fact whether the survivor set was already empty or this operation emptied it.
        write!(
            formatter,
            "`{}` retains {} sessions after this operation; {} reclamation is skipped \
             because a shared artifact cannot be attributed to a surviving session and may \
             belong to another channel's database.",
            self.database,
            self.retained_sessions,
            class_noun(self.kind)
        )
    }
}

/// The operator-facing name of one artifact class.
const fn class_noun(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::SnapshotStore => "snapshot store",
        ArtifactKind::ToolOutput => "tool output",
        ArtifactKind::AttachmentObject => "attachment object",
    }
}

/// The `s` an English count needs, so a published sentence never says "1 objects".
const fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Attachment bytes this pass kept because it could not prove nothing names them.
///
/// The liveness scan reads a stored payload as bytes and treats every well-formed attachment
/// id it finds as a reference (see [`collect_attachment_digests`]). That direction is the
/// safe one — keeping an object nothing needs costs bytes a later pass reclaims, while
/// deleting the only copy of one a queued prompt still names is unrecoverable — and it is
/// also suppressible: a digest quoted in prose, or echoed by a tool result, pins the object
/// it names, and both of those are content a model or a tool can author.
///
/// This record is what makes that suppression visible. Without it a pass whose whole
/// attachment class was pinned by one untrusted payload reported a clean zero, so the
/// operator-facing reclamation ceiling was decided by model output with nothing in the report
/// to say so. It is produced only when bytes were actually held back for a reason outside
/// the operator's control, so the ordinary case — surviving sessions naming their own
/// attachments through a stored reference — stays silent and cannot spend the signal that
/// exists for the suppressed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedAttachments {
    /// SQLite's path for the database whose surviving rows were scanned.
    pub database: String,
    /// Objects and derived entries kept because only free text named their digest.
    pub objects: u64,
    /// Distinct digests surviving rows name only as free text, never as a stored reference.
    pub digests: u64,
    /// Surviving payload rows whose stored value is neither text nor a blob, so no digest
    /// could be read out of them. Such a value cannot spell the `sha256:` prefix at all, so
    /// this count is evidence about the rows, not a set of objects at risk.
    pub unscanned_rows: u64,
}

impl fmt::Display for PinnedAttachments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Like `SkippedRoot` and `SkippedClass`, this rendering is the serialized value:
        // `session_prune` forwards it into the report's `warnings` array, which is the only
        // place a CLI or HTTP caller can see that a class was held back rather than empty.
        // Each clause appears only when its own count is non-zero, so the sentence never
        // reports a zero as if it were a finding.
        write!(formatter, "`{}`", self.database)?;
        if self.objects > 0 {
            write!(
                formatter,
                " kept {} attachment object{} whose {} digest{} surviving rows name only as \
                 free text; model- or tool-authored content can produce that spelling, so \
                 those bytes are not reclaimable while such a row survives",
                self.objects,
                plural(self.objects),
                self.digests,
                plural(self.digests)
            )?;
        }
        if self.unscanned_rows > 0 {
            let joiner = if self.objects > 0 {
                ", and has"
            } else {
                " has"
            };
            write!(
                formatter,
                "{joiner} {} surviving payload row{} stored as neither text nor a blob, so \
                 no attachment id could be read out of them",
                self.unscanned_rows,
                plural(self.unscanned_rows)
            )?;
        }
        write!(formatter, ".")
    }
}

/// Stable, inspectable output for preview and deletion modes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactGcReport {
    /// Every safely reclaimable path in deterministic order.
    pub artifacts: Vec<ReclaimedArtifact>,
    /// Sum of candidate content bytes.
    pub total_bytes: u64,
    /// Tool-output roots that could not be read, in the order they were tried.
    pub skipped_roots: Vec<SkippedRoot>,
    /// Artifact classes this pass declined to evaluate at all.
    pub skipped_classes: Vec<SkippedClass>,
    /// Attachment bytes held back because untrusted text still names them, when any were.
    pub pinned_attachments: Option<PinnedAttachments>,
}

/// A classified database, snapshot scan, or filesystem failure.
#[derive(Debug)]
pub enum ArtifactGcError {
    /// The open database cannot prove ownership of channel-shared artifacts.
    NoVisibleSessions {
        /// SQLite's path for the main database, or `:memory:`.
        database: String,
        /// Total rows observed in `session`.
        session_count: u64,
    },
    /// Reading the survivor set or acquiring its lock failed.
    Database(DbError),
    /// Snapshot store discovery failed.
    Snapshot(SnapshotError),
    /// A filesystem operation failed at a known path.
    Filesystem {
        /// Operation being attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Original I/O cause.
        source: std::io::Error,
    },
}

impl fmt::Display for ArtifactGcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoVisibleSessions {
                database,
                session_count,
            } => write!(
                formatter,
                "refusing artifact gc for database {database}: total session count is {session_count}; shared artifacts may belong to sessions in another channel database"
            ),
            Self::Database(error) => error.fmt(formatter),
            Self::Snapshot(error) => error.fmt(formatter),
            Self::Filesystem {
                operation, path, ..
            } => write!(
                formatter,
                "artifact gc could not {operation} {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ArtifactGcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoVisibleSessions { .. } => None,
            Self::Database(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::Filesystem { source, .. } => Some(source),
        }
    }
}

impl From<DbError> for ArtifactGcError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl From<SnapshotError> for ArtifactGcError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

/// Discover or delete artifacts after a session prune.
///
/// # Errors
///
/// Returns a classified error if the survivor set cannot be locked/read, a
/// snapshot root cannot be scanned, or a candidate cannot be measured/removed.
pub fn execute(
    connection: &mut Connection,
    paths: &ArtifactGcPaths,
    request: &ArtifactGcRequest,
) -> Result<ArtifactGcReport, ArtifactGcError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(open::map_error)?;
    let mut survivors = read_survivors(&transaction)?;
    let requested_session_ids = request
        .deleted_session_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let survivor_ids = survivors
        .iter()
        .map(|survivor| survivor.session_id.clone())
        .collect::<BTreeSet<_>>();
    let deleted_session_ids = match request.mode {
        ArtifactGcMode::Preview => {
            survivors.retain(|survivor| !requested_session_ids.contains(&survivor.session_id));
            requested_session_ids
        }
        ArtifactGcMode::Delete => requested_session_ids
            .difference(&survivor_ids)
            .cloned()
            .collect(),
    };

    let mut candidates = Vec::new();
    let mut skipped_roots = Vec::new();
    let mut skipped_classes = Vec::new();
    // The survivor set gates the shared snapshot root and nothing else, and it is the set
    // that remains *after* the operation being accounted for: the rows are already gone in
    // `Delete`, and `retain` above removes the selection in `Preview`, so both modes reach
    // this decision with the same list. That symmetry is the point — a preview that
    // enumerated every store under the shared root and a delete that then reclaimed none
    // of them promised bytes the delete could not deliver.
    //
    // Applying the same gate to the whole pass instead turned a delete that emptied
    // `session` into a hard failure: the caller lost the report for rows `crate::prune`
    // had already committed, and the tool output of those sessions could never be named
    // again because no row was left to attribute it to. Tool output is attributed from
    // this request's own deleted-session ids plus the age backstop, and attachment objects
    // live under a root keyed by this database's identity, so both stay attributable with
    // no survivors at all. Only `$DATA/snapshot` is shared with other channel databases,
    // where an empty survivor set would make every store on disk look unreferenced.
    let retained_sessions = u64::try_from(survivors.len()).unwrap_or(u64::MAX);
    if retained_sessions == 0 {
        skipped_classes.push(SkippedClass::unattributable(
            ArtifactKind::SnapshotStore,
            database_path(&transaction),
            retained_sessions,
        ));
    } else {
        discover_snapshot_candidates(paths, &survivors, &mut candidates)?;
    }
    discover_tool_output_candidates(
        &transaction,
        paths,
        request,
        &deleted_session_ids,
        &mut candidates,
        &mut skipped_roots,
    )?;
    let mut pinned_attachments = None;
    discover_attachment_candidates(
        &transaction,
        paths,
        &deleted_session_ids,
        &mut candidates,
        &mut pinned_attachments,
    )?;
    candidates.sort_by(|left, right| left.path.cmp(&right.path));

    let mut report = ArtifactGcReport {
        skipped_roots,
        skipped_classes,
        pinned_attachments,
        ..ArtifactGcReport::default()
    };
    for candidate in candidates {
        let bytes = measure(&candidate.path)?;
        let removed = if request.mode == ArtifactGcMode::Delete {
            remove_candidate(&candidate)?
        } else {
            false
        };
        report.total_bytes = report.total_bytes.saturating_add(bytes);
        report.artifacts.push(ReclaimedArtifact {
            path: candidate.path,
            bytes,
            kind: candidate.kind,
            reason: candidate.reason,
            removed,
        });
    }
    transaction.commit().map_err(open::map_error)?;
    Ok(report)
}

pub(crate) fn ensure_visible_session_owners(
    connection: &Connection,
) -> Result<(), ArtifactGcError> {
    let session_count = visible_session_owners(connection)?;
    if session_count > 0 {
        return Ok(());
    }
    Err(ArtifactGcError::NoVisibleSessions {
        database: database_path(connection),
        session_count,
    })
}

/// SQLite's own path for the main database, or the in-memory sentinel.
fn database_path(connection: &Connection) -> String {
    connection
        .path()
        .unwrap_or(zuno_paths::MEMORY_SENTINEL)
        .to_owned()
}

/// Session rows the open database can attribute a channel-shared artifact to.
fn visible_session_owners(connection: &Connection) -> Result<u64, ArtifactGcError> {
    connection
        .query_row("SELECT count(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(i64::unsigned_abs)
        .map_err(open::map_error)
        .map_err(ArtifactGcError::from)
}

#[derive(Debug)]
struct SurvivorSet {
    session_id: String,
    project_id: String,
    worktree: Option<PathBuf>,
}

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    target: Target,
    kind: ArtifactKind,
    reason: ReclaimReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    File,
    Directory,
}

fn read_survivors(
    connection: &rusqlite::Transaction<'_>,
) -> Result<Vec<SurvivorSet>, ArtifactGcError> {
    let mut statement = connection
        .prepare(
            "SELECT session.id, session.project_id, project.worktree
             FROM session
             LEFT JOIN project ON project.id = session.project_id
             ORDER BY session.id ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            let worktree: Option<String> = row.get(2)?;
            Ok(SurvivorSet {
                session_id: row.get(0)?,
                project_id: row.get(1)?,
                worktree: worktree
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from),
            })
        })
        .map_err(open::map_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)
        .map_err(ArtifactGcError::from)
}

fn discover_snapshot_candidates(
    paths: &ArtifactGcPaths,
    survivors: &[SurvivorSet],
    candidates: &mut Vec<Candidate>,
) -> Result<(), ArtifactGcError> {
    if !is_real_directory(&paths.snapshots)? {
        return Ok(());
    }
    let ambiguous_projects = survivors
        .iter()
        .filter(|survivor| survivor.worktree.is_none())
        .map(|survivor| survivor.project_id.clone())
        .collect::<BTreeSet<_>>();
    let references = survivors.iter().filter_map(|survivor| {
        survivor.worktree.as_ref().map(|worktree| {
            SessionRef::new(
                survivor.session_id.clone(),
                survivor.project_id.clone(),
                worktree,
            )
        })
    });

    for store in zuno_snapshot::reference_counts(&paths.snapshots, references)? {
        if !store.on_disk
            || store.is_referenced()
            || ambiguous_projects.contains(&store.key.project_id)
            || !is_real_directory(&store.path)?
        {
            continue;
        }
        let Some(project_directory) = store.path.parent() else {
            continue;
        };
        if !is_real_directory(project_directory)? {
            continue;
        }
        candidates.push(Candidate {
            path: store.path,
            target: Target::Directory,
            kind: ArtifactKind::SnapshotStore,
            reason: ReclaimReason::UnreferencedSnapshot,
        });
    }
    Ok(())
}

/// Every directory that may hold persisted tool output for this database.
///
/// The shared root the caller named, then `<worktree>/.zuno/tool-output/` for every
/// checkout the database has recorded. The in-checkout stores cannot be caller-supplied:
/// `session prune` reclaims across every project at once, and only the `project` rows
/// know which checkouts this database wrote into. Leaving them out is what made that
/// store grow without bound — it has two writers and, until this pass covered it, no
/// sweeper — while the shared root next to it was already swept.
///
/// Locating a store from `project.worktree` is the attribution
/// [`discover_snapshot_candidates`] already relies on, and each root is the same
/// `zuno_tool` store the writers open, so no second spelling of where artifacts live
/// enters this module.
fn tool_output_roots(
    connection: &rusqlite::Transaction<'_>,
    paths: &ArtifactGcPaths,
) -> Result<Vec<PathBuf>, ArtifactGcError> {
    let mut roots = BTreeSet::new();
    roots.insert(paths.tool_output.clone());
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT worktree FROM project
             WHERE worktree IS NOT NULL AND worktree <> ''
             ORDER BY worktree ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?;
    for row in rows {
        let worktree = PathBuf::from(row.map_err(open::map_error)?);
        roots.insert(
            zuno_tool::ToolOutputStore::in_worktree(&worktree)
                .root()
                .to_path_buf(),
        );
    }
    Ok(roots.into_iter().collect())
}

/// Sweep every tool-output root, tolerating the ones that cannot be read.
///
/// A root derived from `project.worktree` is a path the user controls: an offline network
/// mount, a checkout on an unplugged volume, a directory whose permissions changed, or a
/// `.zuno` replaced by a regular file. Propagating that I/O error abandoned the whole
/// artifact phase — including the shared `$DATA` root, which had always been swept — and
/// it did so after [`crate::prune`] had already committed the row deletion, so the files
/// belonging to those now-deleted sessions could never be named by a later pass again.
///
/// Each root is therefore scanned into a local buffer and appended only once it is
/// complete, and an I/O failure records a [`SkippedRoot`] instead. The root keeps its
/// files and the operator gets the path and the reason. Nothing else is relaxed: a
/// database error still fails the pass, because it says the survivor set is not
/// trustworthy, and the snapshot and attachment roots stay strict because they live under
/// the data directory the caller named.
fn discover_tool_output_candidates(
    connection: &rusqlite::Transaction<'_>,
    paths: &ArtifactGcPaths,
    request: &ArtifactGcRequest,
    deleted_session_ids: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
    skipped: &mut Vec<SkippedRoot>,
) -> Result<(), ArtifactGcError> {
    for root in tool_output_roots(connection, paths)? {
        let mut found = Vec::new();
        match scan_tool_output_root(&root, request, deleted_session_ids, &mut found) {
            Ok(()) => candidates.append(&mut found),
            Err(ArtifactGcError::Filesystem {
                operation,
                path,
                source,
            }) => skipped.push(SkippedRoot {
                root,
                path,
                operation,
                reason: source.to_string(),
            }),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn scan_tool_output_root(
    root: &Path,
    request: &ArtifactGcRequest,
    deleted_session_ids: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
) -> Result<(), ArtifactGcError> {
    let cutoff = request.now.checked_sub(request.tool_output_retention);
    if !is_real_directory(root)? {
        return Ok(());
    }
    // The store's own reader decides which names it minted, so every file this pass
    // can remove is one that store would hand back to a retrieval call.
    let entries = zuno_tool::ToolOutputStore::new(root)
        .entries(TOOL_OUTPUT_READER)
        .map_err(|source| filesystem_error("scan", root, std::io::Error::other(source)))?;
    for path in entries {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(filesystem_error("inspect", &path, source)),
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let reason = match zuno_tool::store::session_of(&path) {
            Some(session_id) if deleted_session_ids.contains(session_id) => {
                Some(ReclaimReason::DeletedSession(session_id.to_owned()))
            }
            Some(_) => None,
            None => {
                let modified = metadata
                    .modified()
                    .map_err(|source| filesystem_error("read metadata for", &path, source))?;
                cutoff
                    .filter(|cutoff| modified < *cutoff)
                    .map(|_| ReclaimReason::UnattributedToolOutputExpired)
            }
        };
        if let Some(reason) = reason {
            candidates.push(Candidate {
                path,
                target: Target::File,
                kind: ArtifactKind::ToolOutput,
                reason,
            });
        }
    }
    Ok(())
}

fn discover_attachment_candidates(
    connection: &Connection,
    paths: &ArtifactGcPaths,
    deleted_session_ids: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
    pinned: &mut Option<PinnedAttachments>,
) -> Result<(), ArtifactGcError> {
    if !table_exists(connection, "part")? {
        return Ok(());
    }
    let Some(database) = connection.path().filter(|path| !path.is_empty()) else {
        // A pooled in-memory URI is not recoverable from a bare rusqlite
        // connection. Refusing to guess prevents one in-memory database from
        // deleting another database's object scope.
        return Ok(());
    };
    let identity = zuno_attachment::AttachmentStore::database_identity(database.as_bytes());
    let root = paths.attachments.join("v1").join(identity);
    if !is_real_directory(&root)? {
        return Ok(());
    }
    let live = live_attachment_digests(connection, deleted_session_ids)?;
    let mut held_back = 0_u64;
    for directory in [root.join("objects"), root.join("derived")] {
        held_back = held_back.saturating_add(discover_unreferenced_attachment_files(
            &directory, &live, candidates,
        )?);
    }
    // Reported only when bytes were actually held back by something outside the operator's
    // control: an object pinned by free text, or a row nothing could be read out of. A
    // surviving session naming its own attachment through a stored reference is the ordinary
    // case and stays silent, so this line cannot be spent by the frequent benign event and
    // then be missing for the rare suppressed one.
    if held_back > 0 || live.unscanned_rows > 0 {
        *pinned = Some(PinnedAttachments {
            database: database.to_owned(),
            objects: held_back,
            digests: live.text_only_count(),
            unscanned_rows: live.unscanned_rows,
        });
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, ArtifactGcError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(open::map_error)
        .map_err(ArtifactGcError::from)
}

/// Every object digest a surviving durable row of this database still names.
///
/// A message `part` is the reference the transcript carries, but it is not the first
/// one written. The durable inbox admits a prompt — and the object it uploaded —
/// before the turn that converts it into parts ever runs, and that row can stay
/// pending for as long as the session is busy. Collecting from `part` alone deleted
/// the store's only copy of an image the user had already handed over, and the queued
/// turn could then never be executed as sent.
fn live_attachment_digests(
    connection: &Connection,
    deleted_session_ids: &BTreeSet<String>,
) -> Result<LiveDigests, ArtifactGcError> {
    let mut live = LiveDigests::default();
    collect_referenced_digests(
        connection,
        "SELECT session_id, data FROM part ORDER BY id",
        deleted_session_ids,
        &mut live,
    )?;
    if table_exists(connection, "session_input")? {
        collect_referenced_digests(
            connection,
            "SELECT session_id, prompt FROM session_input ORDER BY id",
            deleted_session_ids,
            &mut live,
        )?;
    }
    Ok(live)
}

/// The digests surviving rows name, and how each one was named.
///
/// `mentioned` is the liveness set: an object whose digest is in it is never a candidate.
/// `referenced` is the subset that appeared as a complete stored string value, and exists
/// only so the report can say how much of the liveness set came from free text instead. The
/// two are deliberately not the same decision: classification is reporting, liveness is
/// deletion, and narrowing liveness to a recognized spelling is what deleted live objects.
#[derive(Debug, Default)]
struct LiveDigests {
    /// Every digest a surviving row names, whatever carried it and however it was spelled.
    mentioned: BTreeSet<String>,
    /// Digests that appeared at least once as a complete stored string value.
    referenced: BTreeSet<String>,
    /// Surviving rows whose payload was neither text nor a blob, so nothing was scanned.
    unscanned_rows: u64,
}

impl LiveDigests {
    /// Whether a surviving row still names this object, by any spelling.
    fn pins(&self, digest: &str) -> bool {
        self.mentioned.contains(digest)
    }

    /// Whether the only thing naming this object is free text in a surviving row.
    fn text_only(&self, digest: &str) -> bool {
        self.mentioned.contains(digest) && !self.referenced.contains(digest)
    }

    /// How many distinct digests are named only as free text.
    fn text_only_count(&self) -> u64 {
        let count = self.mentioned.difference(&self.referenced).count();
        u64::try_from(count).unwrap_or(u64::MAX)
    }
}

/// Scan one `(session_id, payload)` projection and record every digest it names.
///
/// A row of a session this pass is accounting for as deleted is skipped by id, which is the
/// only thing that makes an object of a pruned session reclaimable at all.
///
/// Both columns are read as raw bytes through [`rusqlite::types::ValueRef`] rather than as
/// `String`, and that is a correctness requirement, not a style choice. Neither `part.data`
/// nor `session_input.prompt` carries a type or `json_valid` constraint, and the database is
/// not a file Zuno exclusively writes, so one value stored as a blob or as text that is not
/// valid UTF-8 used to fail `row.get::<_, String>` and abort the whole artifact pass — after
/// `crate::prune` had already committed its row deletions. The caller then saw a failed
/// prune whose database work had happened, which is the uncertain-outcome shape a mechanical
/// retry cannot resolve, and the artifacts leaked for good because the rows that could
/// attribute them were gone. Reading bytes cannot fail on any value SQLite can hold, so the
/// payload is scanned whatever its declared type: a blob that spells a digest still pins its
/// object. A value that is null, an integer, or a real cannot contain the `sha256:` prefix,
/// so those rows are counted as unscanned evidence rather than treated as a risk.
///
/// A session id this crate cannot read as UTF-8 is treated as unknown, never as a match, so
/// such a row is scanned as a survivor: it keeps objects rather than releasing them. It is
/// deliberately not normalized. `String::from_utf8_lossy` would fold every unreadable byte
/// onto `U+FFFD`, and a requested id that itself contains `U+FFFD` would then compare equal
/// to a row it has nothing to do with, releasing the objects that row still names.
fn collect_referenced_digests(
    connection: &Connection,
    query: &str,
    deleted_session_ids: &BTreeSet<String>,
    live: &mut LiveDigests,
) -> Result<(), ArtifactGcError> {
    let mut statement = connection.prepare(query).map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            let session_id = match row.get_ref(0)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                    str::from_utf8(bytes).ok().map(ToOwned::to_owned)
                }
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => None,
            };
            let payload = match row.get_ref(1)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Some(bytes.to_vec()),
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => None,
            };
            Ok((session_id, payload))
        })
        .map_err(open::map_error)?;
    for row in rows {
        let (session_id, payload) = row.map_err(open::map_error)?;
        if session_id.is_some_and(|id| deleted_session_ids.contains(&id)) {
            continue;
        }
        match payload {
            Some(payload) => collect_attachment_digests(&payload, live),
            None => live.unscanned_rows = live.unscanned_rows.saturating_add(1),
        }
    }
    Ok(())
}

/// Record every attachment id one durable payload names, whatever carries it.
///
/// The id is the invariant; nothing else about the payload is. An HTTP prompt nests a
/// reference under `attachment` (`/prompt/files/N/attachment`), while the terminal
/// composer and ACP persist `zuno_llm::event::RequestContentBlock::ImageAttachment`,
/// whose durable key is `reference` because that enum's `rename_all` renames its variants
/// and not its fields. Keying on one writer's spelling swept the objects of the two
/// default surfaces while a test written against the third stayed green. Keying on the
/// `id` field of an object would leave the same trap one level down, and parsing the row
/// at all makes a payload this build cannot parse — a truncated write, a shape an older
/// format wrote, a `ToolUse` input deeper than `serde_json`'s 128-level limit — look like
/// a row that references nothing, which is a licence to delete.
///
/// So the scan reads the stored bytes and asks only what `zuno_attachment::AttachmentId`
/// itself guarantees: the id serializes as `sha256:` followed by 64 lowercase hex digits
/// (`#[serde(into = "String")]`). It does not require the payload to be JSON, to parse, or
/// to be text at all.
///
/// The asymmetry is deliberate. Retaining an object nothing needs costs bytes a later pass
/// reclaims once no row names the digest; deleting one a queued prompt still names is
/// permanent durable-state loss with no recovery. Consequences of reading the row as bytes
/// are recorded rather than narrowed: a digest quoted in prose or in a tool result pins that
/// object, and a payload that fabricates many well-formed ids makes this set as large as the
/// bytes it fabricated them in. Both directions only ever keep bytes, and a cap would hand
/// any writer of one large payload a switch that skips the whole class. What the pass owes
/// the operator instead is the count: [`is_stored_reference`] separates an id that was
/// serialized as its own value from one that is merely mentioned, and
/// [`PinnedAttachments`] reports the objects the second kind held back.
///
/// One spelling this cannot see is an id whose prefix a writer escaped, such as
/// `\u0073ha256:`. `serde_json` and `JSON.stringify` escape neither the prefix nor a hex
/// digit — `an_ascii_escaped_prefix_is_not_what_this_crate_writes` pins that — so no writer
/// in this repository produces it; an importer that did would have its objects reclaimed.
fn collect_attachment_digests(payload: &[u8], live: &mut LiveDigests) {
    let mut cursor = 0_usize;
    while let Some(offset) = find_bytes(&payload[cursor..], ATTACHMENT_ID_PREFIX) {
        let start = cursor + offset;
        let body = start + ATTACHMENT_ID_PREFIX.len();
        // A new occurrence cannot begin inside the prefix just matched: no proper suffix of
        // `sha256:` is a prefix of it. Advancing past the prefix therefore skips nothing.
        cursor = body;
        let Some(digest) = payload.get(body..body + DIGEST_HEX_LEN) else {
            continue;
        };
        if !is_object_digest(digest) {
            continue;
        }
        let Ok(digest) = std::str::from_utf8(digest) else {
            continue;
        };
        live.mentioned.insert(digest.to_owned());
        if is_stored_reference(payload, start, body + DIGEST_HEX_LEN) {
            live.referenced.insert(digest.to_owned());
        }
    }
}

/// The first index in `haystack` where `needle` starts.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether this occurrence is a complete stored string value rather than free text.
///
/// Every writer in this repository persists a reference as its own JSON string, so the id is
/// quote-delimited: `"sha256:<64hex>"`, or `\"sha256:<64hex>\"` when one document was
/// serialized inside another's string. Prose that merely mentions a digest, and a tool result
/// that echoes one inside a longer message, are not.
///
/// This classifies; it never decides liveness. Both forms keep the object, because a writer
/// this build has not seen — or a future format — may spell a reference some third way, and
/// the cost of guessing wrong in the other direction is deleting the only copy of an object a
/// durable row still names. Misclassifying here costs report precision only.
fn is_stored_reference(payload: &[u8], start: usize, end: usize) -> bool {
    let before = start
        .checked_sub(1)
        .and_then(|index| payload.get(index))
        .copied();
    if before != Some(b'"') {
        return false;
    }
    match payload.get(end).copied() {
        Some(b'"') => true,
        Some(b'\\') => payload.get(end + 1).copied() == Some(b'"'),
        _ => false,
    }
}

/// Whether these bytes are the hex body of an attachment id, and therefore an object name.
fn is_object_digest(digest: &[u8]) -> bool {
    digest.len() == DIGEST_HEX_LEN
        && digest
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Collect every object file no surviving row names, and count those held back by text.
///
/// The return value is the number of files that were *not* collected because the only thing
/// naming their digest was free text in a surviving row. That count is the size of the
/// suppression [`PinnedAttachments`] reports; a file kept by a stored reference is ordinary
/// liveness and is not counted.
fn discover_unreferenced_attachment_files(
    root: &Path,
    live: &LiveDigests,
    candidates: &mut Vec<Candidate>,
) -> Result<u64, ArtifactGcError> {
    if !is_real_directory(root)? {
        return Ok(0);
    }
    let mut held_back = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in read_directory(&directory)? {
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|source| filesystem_error("inspect", &path, source))?;
            if kind.is_dir() && !kind.is_symlink() {
                pending.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let digest = name.split('-').next().unwrap_or_default();
            if digest.len() != DIGEST_HEX_LEN {
                continue;
            }
            if live.pins(digest) {
                if live.text_only(digest) {
                    held_back = held_back.saturating_add(1);
                }
                continue;
            }
            candidates.push(Candidate {
                path,
                target: Target::File,
                kind: ArtifactKind::AttachmentObject,
                reason: ReclaimReason::UnreferencedAttachment,
            });
        }
    }
    Ok(held_back)
}

fn read_directory(path: &Path) -> Result<Vec<fs::DirEntry>, ArtifactGcError> {
    let read = match fs::read_dir(path) {
        Ok(read) => read,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(filesystem_error("scan", path, source)),
    };
    let mut entries = read
        .map(|entry| entry.map_err(|source| filesystem_error("scan", path, source)))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn is_real_directory(path: &Path) -> Result<bool, ArtifactGcError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(filesystem_error("inspect", path, source)),
    }
}

fn measure(path: &Path) -> Result<u64, ArtifactGcError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(filesystem_error("measure", path, source)),
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut bytes = 0_u64;
    for entry in read_directory(path)? {
        bytes = bytes.saturating_add(measure(&entry.path())?);
    }
    Ok(bytes)
}

fn remove_candidate(candidate: &Candidate) -> Result<bool, ArtifactGcError> {
    let metadata = match fs::symlink_metadata(&candidate.path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(filesystem_error("inspect", &candidate.path, source)),
    };
    let type_matches = match candidate.target {
        Target::File => metadata.file_type().is_file(),
        Target::Directory => metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
    };
    if !type_matches {
        return Err(filesystem_error(
            "remove changed path",
            &candidate.path,
            std::io::Error::other("candidate type changed after discovery"),
        ));
    }
    let result = match candidate.target {
        Target::File => fs::remove_file(&candidate.path),
        Target::Directory => fs::remove_dir_all(&candidate.path),
    };
    match result {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(filesystem_error("remove", &candidate.path, source)),
    }
}

fn filesystem_error(
    operation: &'static str,
    path: &Path,
    source: std::io::Error,
) -> ArtifactGcError {
    ArtifactGcError::Filesystem {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use zuno_snapshot::StoreKey;

    use super::*;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("open database");
        connection
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);\
                 CREATE TABLE session (\
                   id TEXT PRIMARY KEY,\
                   project_id TEXT NOT NULL,\
                   directory TEXT NOT NULL\
                 );",
            )
            .expect("create schema");
        connection
    }

    fn insert_project(connection: &Connection, id: &str, worktree: &Path) {
        connection
            .execute(
                "INSERT INTO project (id, worktree) VALUES (?1, ?2)",
                params![id, worktree.to_string_lossy()],
            )
            .expect("insert project");
    }

    fn insert_session(connection: &Connection, id: &str, project: &str, directory: &Path) {
        connection
            .execute(
                "INSERT INTO session (id, project_id, directory) VALUES (?1, ?2, ?3)",
                params![id, project, directory.to_string_lossy()],
            )
            .expect("insert session");
    }

    fn insert_visibility_sentinel(connection: &Connection, worktree: &Path) {
        insert_project(connection, "guard-project", worktree);
        insert_session(connection, "ses_guard", "guard-project", worktree);
    }

    fn write(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().expect("fixture path has parent"))
            .expect("create fixture parent");
        fs::write(path, content).expect("write fixture");
    }

    fn backdate(path: &Path, age: Duration) {
        let when = SystemTime::now()
            .checked_sub(age)
            .expect("representable old timestamp");
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open fixture")
            .set_modified(when)
            .expect("set fixture mtime");
    }

    fn attachment_database(path: &Path, session_id: &str, live_digest: &str) -> Connection {
        let connection = Connection::open(path).expect("open attachment database");
        connection
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);\
                 CREATE TABLE session (\
                   id TEXT PRIMARY KEY,\
                   project_id TEXT NOT NULL,\
                   directory TEXT NOT NULL\
                 );\
                 CREATE TABLE part (\
                   id TEXT PRIMARY KEY,\
                   session_id TEXT NOT NULL,\
                   data TEXT NOT NULL\
                 );\
                 CREATE TABLE session_input (\
                   id TEXT PRIMARY KEY,\
                   session_id TEXT NOT NULL,\
                   prompt TEXT NOT NULL,\
                   state TEXT NOT NULL\
                 );",
            )
            .expect("create attachment GC schema");
        insert_project(
            &connection,
            "project",
            path.parent().expect("database parent"),
        );
        insert_session(
            &connection,
            session_id,
            "project",
            path.parent().expect("database parent"),
        );
        connection
            .execute(
                "INSERT INTO part (id, session_id, data) VALUES (?1, ?2, ?3)",
                params![
                    format!("part_{session_id}"),
                    session_id,
                    serde_json::json!({
                        "type": "file",
                        "attachment": {
                            "id": format!("sha256:{live_digest}"),
                            "mediaType": "image/png",
                            "width": 1,
                            "height": 1,
                            "encodedBytes": 1
                        }
                    })
                    .to_string()
                ],
            )
            .expect("insert live attachment reference");
        connection
    }

    /// The exact shape `zuno-server` admits: the reference nests two levels below the
    /// row's `prompt`, which is why liveness cannot be one hard-coded pointer per table.
    fn insert_pending_input(connection: &Connection, id: &str, session_id: &str, digest: &str) {
        insert_pending_prompt(
            connection,
            id,
            session_id,
            &serde_json::json!({
                "kind": "user",
                "prompt": {
                    "text": "what does this screenshot show?",
                    "files": [{
                        "type": "image",
                        "attachment": {
                            "id": format!("sha256:{digest}"),
                            "mediaType": "image/png",
                            "width": 1,
                            "height": 1,
                            "encodedBytes": 1
                        }
                    }],
                    "agents": []
                },
                "agent": null,
                "model": null
            }),
        );
    }

    /// The exact row `zuno-cli`'s terminal composer admits, copied field for field from
    /// `PersistedTuiInput::TuiPrompt` wrapping `PromptSubmission::Content`.
    ///
    /// The reference is under `reference`, not `attachment`: the block is
    /// `RequestContentBlock::ImageAttachment`, and that enum's `rename_all` renames its
    /// variants, not its fields. This is the default surface — a paste while a turn runs
    /// is admitted with `TurnLease::Deferred`, so the row is pending and no `part` row
    /// exists yet.
    fn insert_pending_tui_input(connection: &Connection, id: &str, session_id: &str, digest: &str) {
        insert_pending_prompt(
            connection,
            id,
            session_id,
            &serde_json::json!({
                "kind": "tuiPrompt",
                "submission": {
                    "kind": "content",
                    "data": {
                        "text": "what does this screenshot show?",
                        "content": [{
                            "type": "image_attachment",
                            "reference": {
                                "id": format!("sha256:{digest}"),
                                "mediaType": "image/png",
                                "width": 1,
                                "height": 1,
                                "encodedBytes": 1
                            }
                        }]
                    }
                },
                "origin": "tui_keybinding"
            }),
        );
    }

    /// The exact row `acp_prompt_payload` admits for an ACP image block.
    fn insert_pending_acp_input(connection: &Connection, id: &str, session_id: &str, digest: &str) {
        insert_pending_prompt(
            connection,
            id,
            session_id,
            &serde_json::json!({
                "kind": "acpPrompt",
                "text": "what does this screenshot show?",
                "content": [{
                    "type": "image_attachment",
                    "reference": {
                        "id": format!("sha256:{digest}"),
                        "mediaType": "image/png",
                        "width": 1,
                        "height": 1,
                        "encodedBytes": 1
                    }
                }]
            }),
        );
    }

    fn insert_pending_prompt(
        connection: &Connection,
        id: &str,
        session_id: &str,
        prompt: &serde_json::Value,
    ) {
        connection
            .execute(
                "INSERT INTO session_input (id, session_id, prompt, state)
                 VALUES (?1, ?2, ?3, 'queued')",
                params![id, session_id, prompt.to_string()],
            )
            .expect("insert pending inbox prompt");
    }

    /// One `part` row with exactly the bytes given, valid JSON or not.
    fn insert_part_payload(connection: &Connection, id: &str, session_id: &str, data: &str) {
        connection
            .execute(
                "INSERT INTO part (id, session_id, data) VALUES (?1, ?2, ?3)",
                params![id, session_id, data],
            )
            .expect("insert part payload");
    }

    fn attachment_file(
        paths: &ArtifactGcPaths,
        database: &Path,
        directory: &str,
        name: &str,
    ) -> PathBuf {
        let identity = zuno_attachment::AttachmentStore::database_identity(
            database.to_string_lossy().as_bytes(),
        );
        paths
            .attachments
            .join("v1")
            .join(identity)
            .join(directory)
            .join(&name[..2])
            .join(name)
    }

    /// `session_prune` collects this rendering straight into the serialized report's
    /// `warnings` array, next to `artifacts[].path` values that are already in wire form.
    /// A consumer that joins the two lists needs one spelling of the same directory.
    #[test]
    fn a_skipped_root_warning_renders_both_paths_in_wire_form() {
        let skipped = SkippedRoot {
            root: PathBuf::from(r"C:\repo\.zuno\tool-output"),
            path: PathBuf::from(r"C:\repo\.zuno\tool-output\tool_ses_old_call.jsonl"),
            operation: "scan",
            reason: "access is denied".to_owned(),
        };

        assert_eq!(
            skipped.to_string(),
            "tool output under C:/repo/.zuno/tool-output was not swept: could not scan \
             C:/repo/.zuno/tool-output/tool_ses_old_call.jsonl (access is denied)"
        );
    }

    /// The gate protects the shared snapshot root, and only it. A pass whose caller has
    /// already committed a delete that emptied `session` must still complete and report,
    /// because the ids it was handed are the last record of which files belonged to those
    /// sessions; every later pass would see an unattributable name instead.
    #[test]
    fn zero_total_sessions_leaves_shared_snapshots_alone_and_still_completes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let database_path = temp.path().join("wrong-channel.db");
        let mut connection = Connection::open(&database_path).expect("open file database");
        connection
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);\
                 CREATE TABLE session (\
                   id TEXT PRIMARY KEY,\
                   project_id TEXT NOT NULL,\
                   directory TEXT NOT NULL\
                 );",
            )
            .expect("create schema");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let unowned = StoreKey::new("project", &temp.path().join("worktree"));
        write(
            &unowned.path_in(&paths.snapshots).join("objects/history"),
            b"must survive",
        );
        let attributable = paths
            .tool_output
            .join("tool_ses_selected_00000000000000000000000000000001");
        write(&attributable, b"the deleted session's output");

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_selected"], SystemTime::now()).deleting(),
        )
        .expect("a committed delete that emptied the table still reports its artifacts");

        assert!(
            unowned.path_in(&paths.snapshots).is_dir(),
            "an empty database cannot prove a shared snapshot store is unreferenced"
        );
        assert!(
            !attributable.exists(),
            "tool output named by the request's own ids is still attributable"
        );
        assert!(report.artifacts.iter().any(|artifact| {
            artifact.path == attributable
                && artifact.kind == ArtifactKind::ToolOutput
                && artifact.removed
        }));
        // Completing is not licence to be silent. The rows that could have attributed those
        // stores are already gone, so the report is the only place the bytes still get named.
        assert_eq!(
            report.skipped_classes,
            [SkippedClass::unattributable(
                ArtifactKind::SnapshotStore,
                database_path.to_string_lossy().into_owned(),
                0,
            )]
        );
        let error = ensure_visible_session_owners(&connection)
            .expect_err("the pre-mutation gate still names the database and the count");
        let message = error.to_string();
        assert!(message.contains(&database_path.to_string_lossy().into_owned()));
        assert!(message.contains('0'), "{message}");
    }

    /// The skipped snapshot class needs no operator action, and the doc comment says so.
    ///
    /// The previous wording told an operator the bytes now needed a human, and the pending
    /// documentation asked them to `rm` the store directory by hand. That is the one thing
    /// they must not do: `$ZUNO_DATA/snapshot` is shared, a store there can belong to another
    /// channel's database, and deciding which is exactly the cross-database attribution this
    /// pass refuses to make. This test is the evidence for the replacement claim -- the bytes
    /// survive the survivorless pass and the next pass over the same database reclaims them
    /// once one session exists to attribute against.
    #[test]
    fn a_class_skipped_without_survivors_is_reclaimed_by_the_next_pass_with_one() {
        let temp = tempfile::tempdir().expect("temp dir");
        let database_path = temp.path().join("later.db");
        let mut connection = Connection::open(&database_path).expect("open file database");
        connection
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);\
                 CREATE TABLE session (\
                   id TEXT PRIMARY KEY,\
                   project_id TEXT NOT NULL,\
                   directory TEXT NOT NULL\
                 );",
            )
            .expect("create schema");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let worktree = temp.path().join("worktree");
        let stale = StoreKey::new("project", &temp.path().join("gone"));
        write(
            &stale.path_in(&paths.snapshots).join("objects/history"),
            b"nothing references this",
        );

        let first = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_selected"], SystemTime::now()).deleting(),
        )
        .expect("a pass with no surviving session still completes");
        assert!(
            stale.path_in(&paths.snapshots).is_dir(),
            "an empty database cannot attribute a shared store, so the bytes stay"
        );
        assert_eq!(
            first.skipped_classes,
            [SkippedClass::unattributable(
                ArtifactKind::SnapshotStore,
                database_path.to_string_lossy().into_owned(),
                0,
            )],
            "the skip is recorded, and it is a record, not a work item"
        );

        // The ordinary course of operation: the database gains a session again.
        insert_project(&connection, "project", &worktree);
        insert_session(&connection, "ses_live", "project", &worktree);
        let second = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(Vec::<String>::new(), SystemTime::now()).deleting(),
        )
        .expect("the later pass evaluates the class it previously skipped");

        assert!(
            !stale.path_in(&paths.snapshots).exists(),
            "the next pass with a survivor reclaims the store, with no manual step"
        );
        assert!(second.skipped_classes.is_empty());
    }

    /// No operator-facing text in this module may hand out a removal recipe for shared bytes.
    ///
    /// The claim above is only worth anything if the file cannot drift back to advising a
    /// manual delete under `$ZUNO_DATA/snapshot`, where a store may belong to another
    /// channel's database. Only the production half of the file is inspected, so this
    /// assertion cannot match its own text.
    #[test]
    fn no_doc_comment_here_tells_an_operator_to_remove_shared_bytes_by_hand() {
        let source = include_str!("artifact_gc.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the source before the test module");
        for recipe in ["rm $ZUNO_DATA", "rm -rf", "rmdir", "delete the directory"] {
            assert!(
                !production.contains(recipe),
                "a skipped class is evidence, not an invitation to delete shared bytes: \
                 {recipe:?}"
            );
        }
    }

    #[test]
    fn referenced_snapshot_store_is_retained_and_unreferenced_store_is_removed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let live_worktree = temp.path().join("live-worktree");
        let dead_worktree = temp.path().join("dead-worktree");
        let live = StoreKey::new("project", &live_worktree);
        let dead = StoreKey::new("project", &dead_worktree);
        write(
            &live.path_in(&paths.snapshots).join("objects/live"),
            b"live",
        );
        write(
            &dead.path_in(&paths.snapshots).join("objects/dead"),
            b"dead",
        );

        let mut connection = database();
        insert_project(&connection, "project", &live_worktree);
        insert_session(&connection, "ses_live", "project", &live_worktree);

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(live.path_in(&paths.snapshots).is_dir());
        assert!(!dead.path_in(&paths.snapshots).exists());
        assert!(report.artifacts.iter().any(|artifact| {
            artifact.path == dead.path_in(&paths.snapshots)
                && artifact.kind == ArtifactKind::SnapshotStore
                && artifact.removed
        }));
    }

    #[test]
    fn ambiguous_project_reference_retains_every_store_for_that_project() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let store = StoreKey::new("missing-project", temp.path().join("worktree").as_path());
        write(
            &store.path_in(&paths.snapshots).join("objects/data"),
            b"keep",
        );

        let mut connection = database();
        insert_session(&connection, "ses_ambiguous", "missing-project", temp.path());

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(Vec::<String>::new(), SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(store.path_in(&paths.snapshots).is_dir());
        assert!(report.artifacts.is_empty());
    }

    #[test]
    fn tool_output_uses_session_attribution_and_only_age_sweeps_foreign_names() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let deleted = paths
            .tool_output
            .join("tool_ses_deleted_00000000000000000000000000000001");
        let live = paths
            .tool_output
            .join("tool_ses_live_00000000000000000000000000000002");
        let old_foreign = paths.tool_output.join("tool_019abcdef0123456789ABCDEFG");
        let fresh_foreign = paths.tool_output.join("tool_019abcdef0123456789ABCDEH");
        for path in [&deleted, &live, &old_foreign, &fresh_foreign] {
            write(path, b"tool output");
        }
        backdate(&live, Duration::from_secs(30 * 24 * 60 * 60));
        backdate(&old_foreign, Duration::from_secs(8 * 24 * 60 * 60));

        let live_worktree = temp.path().join("worktree");
        let mut connection = database();
        insert_project(&connection, "project", &live_worktree);
        insert_session(&connection, "ses_live", "project", &live_worktree);

        execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(!deleted.exists(), "deleted-session output is attributable");
        assert!(live.exists(), "an attributed survivor is not age-swept");
        assert!(
            !old_foreign.exists(),
            "old foreign output uses the backstop"
        );
        assert!(fresh_foreign.exists(), "fresh foreign output is retained");
    }

    /// The in-checkout store is swept by the same rules as the shared one, and only for
    /// a checkout the database names. Before this pass it had two writers and no
    /// sweeper, so every artifact ever written beside a working copy stayed forever.
    #[test]
    fn in_checkout_tool_output_is_swept_only_for_a_recorded_worktree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let recorded = temp.path().join("recorded");
        let unrecorded = temp.path().join("unrecorded");
        let root = zuno_tool::ToolOutputStore::in_worktree(&recorded)
            .root()
            .to_path_buf();
        let deleted = root.join("tool_ses_deleted_00000000000000000000000000000001");
        let live = root.join("tool_ses_live_00000000000000000000000000000002");
        let old_foreign = root.join("tool_019abcdef0123456789ABCDEFG");
        let fresh_foreign = root.join("tool_019abcdef0123456789ABCDEH");
        let elsewhere = zuno_tool::ToolOutputStore::in_worktree(&unrecorded)
            .root()
            .join("tool_ses_deleted_00000000000000000000000000000003");
        for path in [&deleted, &live, &old_foreign, &fresh_foreign, &elsewhere] {
            write(path, b"in-checkout output");
        }
        backdate(&live, Duration::from_secs(30 * 24 * 60 * 60));
        backdate(&old_foreign, Duration::from_secs(8 * 24 * 60 * 60));
        backdate(&elsewhere, Duration::from_secs(30 * 24 * 60 * 60));

        let mut connection = database();
        insert_project(&connection, "project", &recorded);
        insert_session(&connection, "ses_live", "project", &recorded);

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(
            !deleted.exists(),
            "a deleted session's output is attributable"
        );
        assert!(live.exists(), "an attributed survivor is not age-swept");
        assert!(
            !old_foreign.exists(),
            "an unattributed name uses the backstop"
        );
        assert!(
            fresh_foreign.exists(),
            "a fresh unattributed name is retained"
        );
        assert!(
            elsewhere.is_file(),
            "a checkout no project row names is not scanned at all"
        );
        assert!(report.artifacts.iter().any(|artifact| {
            artifact.path == deleted
                && artifact.kind == ArtifactKind::ToolOutput
                && artifact.removed
        }));
    }

    #[test]
    fn attachment_gc_is_scoped_to_the_open_database_and_retains_live_derivations() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database_a = temp.path().join("a.db");
        let database_b = temp.path().join("b.db");
        let live_a = "11".repeat(32);
        let orphan_a = "22".repeat(32);
        let live_b = "33".repeat(32);
        let mut connection_a = attachment_database(&database_a, "ses_a", &live_a);
        let _connection_b = attachment_database(&database_b, "ses_b", &live_b);

        let live_a_object = attachment_file(&paths, &database_a, "objects", &live_a);
        let live_a_derived =
            attachment_file(&paths, &database_a, "derived", &format!("{live_a}-policy"));
        let orphan_a_object = attachment_file(&paths, &database_a, "objects", &orphan_a);
        let orphan_a_derived = attachment_file(
            &paths,
            &database_a,
            "derived",
            &format!("{orphan_a}-policy"),
        );
        let live_b_object = attachment_file(&paths, &database_b, "objects", &live_b);
        let orphan_b = "44".repeat(32);
        let orphan_b_object = attachment_file(&paths, &database_b, "objects", &orphan_b);
        for path in [
            &live_a_object,
            &live_a_derived,
            &orphan_a_object,
            &orphan_a_derived,
            &live_b_object,
            &orphan_b_object,
        ] {
            write(path, b"object");
        }

        let report = execute(
            &mut connection_a,
            &paths,
            &ArtifactGcRequest::new(Vec::<String>::new(), SystemTime::now()).deleting(),
        )
        .expect("collect database A attachments");

        assert!(live_a_object.is_file());
        assert!(live_a_derived.is_file());
        assert!(!orphan_a_object.exists());
        assert!(!orphan_a_derived.exists());
        assert!(live_b_object.is_file());
        assert!(
            orphan_b_object.is_file(),
            "database A GC must not inspect database B's object scope"
        );
        assert_eq!(
            report.pinned_attachments, None,
            "a surviving session naming its own attachment through a stored reference is \
             ordinary liveness, and must not spend the suppression signal"
        );
        assert_eq!(
            report
                .artifacts
                .iter()
                .filter(|artifact| artifact.kind == ArtifactKind::AttachmentObject)
                .count(),
            2
        );
    }

    /// A queued prompt is durable, accepted user input: the row names the object long
    /// before the turn that would write a `part` row is admitted, and it stays pending
    /// for as long as the session is busy. Sweeping on `part` alone deleted the store's
    /// only copy of an image the user had already sent, and the pass that did it was a
    /// prune of an unrelated old session.
    #[test]
    fn a_pending_inbox_prompt_keeps_its_attachment_object_live() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("inbox.db");
        let transcript = "11".repeat(32);
        let queued = "55".repeat(32);
        let pruned = "66".repeat(32);
        let orphan = "22".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);
        insert_session(
            &connection,
            "ses_busy",
            "project",
            database.parent().expect("database parent"),
        );
        insert_pending_input(&connection, "inp_busy", "ses_busy", &queued);
        insert_pending_input(&connection, "inp_pruned", "ses_pruned", &pruned);

        let transcript_object = attachment_file(&paths, &database, "objects", &transcript);
        let queued_object = attachment_file(&paths, &database, "objects", &queued);
        let pruned_object = attachment_file(&paths, &database, "objects", &pruned);
        let orphan_object = attachment_file(&paths, &database, "objects", &orphan);
        for path in [
            &transcript_object,
            &queued_object,
            &pruned_object,
            &orphan_object,
        ] {
            write(path, b"object");
        }

        execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_pruned"], SystemTime::now()).deleting(),
        )
        .expect("collect attachments");

        assert!(
            queued_object.is_file(),
            "a durable inbox row is a live reference"
        );
        assert!(transcript_object.is_file());
        assert!(
            !pruned_object.exists(),
            "the pruned session's own queued object is still reclaimable"
        );
        assert!(!orphan_object.exists());
    }

    /// The two default surfaces do not spell the reference `attachment`.
    ///
    /// A terminal paste and an ACP image block both persist
    /// `RequestContentBlock::ImageAttachment`, whose durable key is `reference`, so a scan
    /// keyed on the HTTP writer's spelling swept the objects of the surfaces almost every
    /// user is on while a test written against the HTTP shape stayed green. The rows here
    /// are copied field for field from `PersistedTuiInput::TuiPrompt` and
    /// `acp_prompt_payload`, and the digests they name must survive a prune of an
    /// unrelated session.
    #[test]
    fn a_pending_tui_or_acp_prompt_keeps_its_attachment_object_live() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("inbox.db");
        let transcript = "11".repeat(32);
        let tui = "77".repeat(32);
        let acp = "88".repeat(32);
        let orphan = "22".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);
        let parent = database.parent().expect("database parent");
        insert_session(&connection, "ses_tui_busy", "project", parent);
        insert_session(&connection, "ses_acp_busy", "project", parent);
        insert_pending_tui_input(&connection, "inp_tui", "ses_tui_busy", &tui);
        insert_pending_acp_input(&connection, "inp_acp", "ses_acp_busy", &acp);

        let tui_object = attachment_file(&paths, &database, "objects", &tui);
        let acp_object = attachment_file(&paths, &database, "objects", &acp);
        let orphan_object = attachment_file(&paths, &database, "objects", &orphan);
        for path in [&tui_object, &acp_object, &orphan_object] {
            write(path, b"object");
        }

        execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_pruned"], SystemTime::now()).deleting(),
        )
        .expect("collect attachments");

        assert!(
            tui_object.is_file(),
            "a pending terminal prompt names its object under `reference`, not `attachment`"
        );
        assert!(
            acp_object.is_file(),
            "a pending ACP prompt names its object under `reference`, not `attachment`"
        );
        assert!(
            !orphan_object.exists(),
            "a key-agnostic scan must still reclaim an object no durable row names"
        );
    }

    /// A payload this build cannot turn into a value is not a payload that names nothing.
    ///
    /// Neither `part.data` nor `session_input.prompt` carries a `json_valid` constraint, so
    /// a half-written row, a shape an older format persisted, and a `ToolUse` input nested
    /// past `serde_json`'s 128-level parse limit all arrive as text no parse will accept.
    /// A liveness scan that parses first has to skip such a row, and skipping it deleted
    /// the object the row still names: corrupt durable state became a licence to destroy
    /// the bytes that could have recovered it. The two payloads here are asserted
    /// unparsable by this build first, so the fixture cannot quietly become a parsable one.
    #[test]
    fn an_unparsable_payload_still_keeps_the_object_it_names_live() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("corrupt.db");
        let transcript = "11".repeat(32);
        let truncated_digest = "33".repeat(32);
        let deep_digest = "44".repeat(32);
        let orphan = "22".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);

        // A write cut off mid-object: the id is durable, the closing braces are not.
        let truncated = format!(
            "{{\"type\":\"file\",\"attachment\":{{\"id\":\"sha256:{truncated_digest}\",\"mediaTy"
        );
        // 200 levels of array around the same reference a `ToolUse` input could nest.
        let deep = format!(
            "{}{}{}",
            "[".repeat(200),
            serde_json::json!({"id": format!("sha256:{deep_digest}")}),
            "]".repeat(200)
        );
        for payload in [&truncated, &deep] {
            assert!(
                serde_json::from_str::<serde_json::Value>(payload).is_err(),
                "the fixture must be a payload this build cannot parse: {payload}"
            );
        }
        insert_part_payload(&connection, "part_truncated", "ses_live", &truncated);
        insert_part_payload(&connection, "part_deep", "ses_live", &deep);

        let truncated_object = attachment_file(&paths, &database, "objects", &truncated_digest);
        let deep_object = attachment_file(&paths, &database, "objects", &deep_digest);
        let orphan_object = attachment_file(&paths, &database, "objects", &orphan);
        for path in [&truncated_object, &deep_object, &orphan_object] {
            write(path, b"object");
        }

        execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_pruned"], SystemTime::now()).deleting(),
        )
        .expect("collect attachments");

        assert!(
            truncated_object.is_file(),
            "a truncated payload still names its object, and the object is the only copy"
        );
        assert!(
            deep_object.is_file(),
            "a payload deeper than the parse limit still names its object"
        );
        assert!(
            !orphan_object.exists(),
            "failing closed on unreadable rows must not stop reclaiming a real orphan"
        );
    }

    #[test]
    fn a_requested_session_that_still_exists_is_not_treated_as_deleted() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let output = paths
            .tool_output
            .join("tool_ses_live_00000000000000000000000000000002");
        write(&output, b"keep");
        let worktree = temp.path().join("worktree");
        let mut connection = database();
        insert_project(&connection, "project", &worktree);
        insert_session(&connection, "ses_live", "project", &worktree);

        execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_live"], SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(
            output.exists(),
            "the database survivor set wins over caller input"
        );
    }

    #[test]
    fn preview_reports_bytes_without_removing_any_candidate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(temp.path());
        let output = paths
            .tool_output
            .join("tool_ses_deleted_00000000000000000000000000000001");
        write(&output, b"12345");
        let mut connection = database();
        insert_visibility_sentinel(&connection, temp.path());

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()),
        )
        .expect("preview artifacts");

        assert!(output.exists());
        assert_eq!(report.total_bytes, 5);
        assert_eq!(report.artifacts.len(), 1);
        assert!(!report.artifacts[0].removed);
    }

    /// One unreadable checkout must not cost every other root its sweep.
    ///
    /// The roots are tried in path order, so a failing one used to abandon the phase
    /// before the shared data root and the healthy checkout were reached. `session prune`
    /// had already committed the row deletion by then, so those files could never be
    /// attributed to a deleted session again: the disk was leaked for good. The
    /// unreadable root is now recorded and skipped, and the rest of the pass proceeds.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_tool_output_root_is_recorded_and_the_other_roots_are_still_swept() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data = temp.path().join("data");
        let paths = ArtifactGcPaths::from_data_root(&data);
        // Sorted before both other roots, which is what made it fatal for them.
        let blocked = temp.path().join("blocked");
        write(&blocked, b"a checkout replaced by a file");
        let blocked_root = zuno_tool::ToolOutputStore::in_worktree(&blocked)
            .root()
            .to_path_buf();
        let recorded = temp.path().join("recorded");
        let in_checkout = zuno_tool::ToolOutputStore::in_worktree(&recorded)
            .root()
            .join("tool_ses_deleted_00000000000000000000000000000001");
        let shared = paths
            .tool_output
            .join("tool_ses_deleted_00000000000000000000000000000002");
        write(&in_checkout, b"in-checkout output");
        write(&shared, b"shared output");

        let mut connection = database();
        insert_project(&connection, "blocked-project", &blocked);
        insert_project(&connection, "recorded-project", &recorded);
        insert_session(&connection, "ses_live", "recorded-project", &recorded);

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()).deleting(),
        )
        .expect("an unreadable root is not a failed pass");

        assert!(
            !in_checkout.exists(),
            "the healthy checkout must still be swept"
        );
        assert!(!shared.exists(), "the shared root must still be swept");
        assert_eq!(report.artifacts.len(), 2);
        assert_eq!(report.skipped_roots.len(), 1);
        let skipped = &report.skipped_roots[0];
        assert_eq!(skipped.root, blocked_root);
        assert_eq!(skipped.operation, "inspect");
        let reported = skipped.to_string();
        assert!(reported.contains("was not swept"), "{reported}");
        assert!(
            reported.contains(&blocked_root.display().to_string()),
            "an operator needs the path: {reported}"
        );
        assert!(
            !skipped.reason.is_empty(),
            "the original cause is the evidence: {reported}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_root_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let data = temp.path().join("data");
        let targets = temp.path().join("targets");
        let tool_target = targets.join("tool-output");
        let tool_file = tool_target.join("tool_ses_deleted_00000000000000000000000000000001");
        write(&tool_file, b"tool");
        fs::create_dir_all(&data).expect("create data root");
        symlink(&tool_target, data.join("tool-output")).expect("link tool root");

        let paths = ArtifactGcPaths::from_data_root(&data);
        let mut connection = database();
        insert_visibility_sentinel(&connection, temp.path());
        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_deleted"], SystemTime::now()).deleting(),
        )
        .expect("collect artifacts");

        assert!(report.artifacts.is_empty());
        assert!(tool_file.is_file());
    }

    /// A payload the released build could have stored must not fail the whole pass.
    ///
    /// `part.data` and `session_input.prompt` are declared `text` with no `json_valid`
    /// constraint, and a TEXT-affinity column stores a blob as a blob and keeps text SQLite
    /// never validated as UTF-8, so both values below are ordinary content of a 0.6.6
    /// database. Reading either through `row.get::<_, String>` returns `Err`, and this pass
    /// runs *after* `crate::prune` has already committed its row deletions: the caller saw a
    /// failed prune whose database work had happened, and the artifacts of the sessions it
    /// deleted could never be attributed again, because the rows that named them were gone.
    ///
    /// So the scan reads bytes. A digest inside a blob still pins its object, a digest inside
    /// text that is not valid UTF-8 still pins its object, and a real orphan in the same
    /// database is still reclaimed in the same pass.
    #[test]
    fn a_blob_or_invalid_utf8_payload_is_scanned_instead_of_failing_the_pass() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("bytes.db");
        let transcript = "11".repeat(32);
        let blob_digest = "33".repeat(32);
        let latin1_digest = "44".repeat(32);
        let orphan = "22".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);

        // The exact bytes of a stored reference, stored in the TEXT column as a blob.
        let mut blob = br#"{"type":"file","attachment":{"id":"sha256:"#.to_vec();
        blob.extend_from_slice(blob_digest.as_bytes());
        blob.extend_from_slice(br#""}}"#);
        connection
            .execute(
                "INSERT INTO part (id, session_id, data) VALUES ('part_blob', 'ses_live', ?1)",
                params![blob],
            )
            .expect("insert blob payload");

        // Text SQLite stored without validating it: a legacy paste, byte 0xff first.
        let mut latin1 = b"\xff\xfe pasted transcript, attachment sha256:".to_vec();
        latin1.extend_from_slice(latin1_digest.as_bytes());
        latin1.extend_from_slice(b" was reviewed");
        connection
            .execute(
                "INSERT INTO session_input (id, session_id, prompt, state) \
                 VALUES ('inp_latin1', 'ses_live', CAST(?1 AS TEXT), 'queued')",
                params![latin1],
            )
            .expect("insert invalid utf-8 prompt");

        // The fixture is only the reviewer's input if these are really the two storage
        // classes that used to abort the pass.
        let stored: String = connection
            .query_row(
                "SELECT typeof(data) FROM part WHERE id = 'part_blob'",
                [],
                |row| row.get(0),
            )
            .expect("read stored type");
        assert_eq!(stored, "blob", "the payload must be stored as a blob");
        let stored: String = connection
            .query_row(
                "SELECT typeof(prompt) FROM session_input WHERE id = 'inp_latin1'",
                [],
                |row| row.get(0),
            )
            .expect("read stored type");
        assert_eq!(stored, "text", "the prompt must be stored as text");
        for query in [
            "SELECT data FROM part WHERE id = 'part_blob'",
            "SELECT prompt FROM session_input WHERE id = 'inp_latin1'",
        ] {
            assert!(
                connection
                    .query_row(query, [], |row| row.get::<_, String>(0))
                    .is_err(),
                "the fixture must be a value a `String` read cannot decode: {query}"
            );
        }

        let blob_object = attachment_file(&paths, &database, "objects", &blob_digest);
        let latin1_object = attachment_file(&paths, &database, "objects", &latin1_digest);
        let orphan_object = attachment_file(&paths, &database, "objects", &orphan);
        for path in [&blob_object, &latin1_object, &orphan_object] {
            write(path, b"object");
        }

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_pruned"], SystemTime::now()).deleting(),
        )
        .expect("a blob or non-UTF-8 payload must not fail the pass");

        assert!(
            blob_object.is_file(),
            "a digest stored as a blob still names the only copy of its object"
        );
        assert!(
            latin1_object.is_file(),
            "a digest inside text that is not valid UTF-8 still names its object"
        );
        assert!(
            !orphan_object.exists(),
            "a real orphan is still reclaimed in the same pass"
        );
        // The blob spelled a complete stored reference, so it is ordinary liveness; the
        // legacy paste named its digest as free text, so it is the reported suppression.
        assert_eq!(
            report.pinned_attachments,
            Some(PinnedAttachments {
                database: database.to_string_lossy().into_owned(),
                objects: 1,
                digests: 1,
                unscanned_rows: 0,
            }),
            "only the free-text digest is counted as held back"
        );
    }

    /// A row whose session id is not UTF-8 is never folded onto the id being deleted.
    ///
    /// The scan compares a row's `session_id` against the ids this pass is deleting. Reading
    /// that column with `String::from_utf8_lossy` replaces every unreadable byte with
    /// `U+FFFD`, so a row keyed by the single byte `0xff` reads back as `"\u{FFFD}"` — and a
    /// requested id spelled `"\u{FFFD}"` then matches it. The row is skipped as belonging to
    /// a deleted session, its stored reference is never collected, and the only copy of the
    /// object it names is deleted while the row survives. That is data loss produced by a
    /// normalization step, not by any decision an operator made.
    #[test]
    fn a_row_whose_session_id_is_not_utf8_is_never_folded_onto_a_deleted_id() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("folded.db");
        let transcript = "11".repeat(32);
        let held = "55".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);

        let reference = serde_json::json!({
            "type": "file",
            "attachment": { "id": format!("sha256:{held}") }
        })
        .to_string();
        connection
            .execute(
                "INSERT INTO part (id, session_id, data) \
                 VALUES ('part_unreadable', CAST(?1 AS TEXT), ?2)",
                params![b"\xff".to_vec(), reference],
            )
            .expect("insert row keyed by a byte that is not UTF-8");

        // The fixture is only the dangerous input if the stored key really is unreadable and
        // really does fold onto the id this pass deletes.
        let stored: Vec<u8> = connection
            .query_row(
                "SELECT CAST(session_id AS BLOB) FROM part WHERE id = 'part_unreadable'",
                [],
                |row| row.get(0),
            )
            .expect("read stored key");
        assert_eq!(stored, b"\xff", "the key must be the single byte 0xff");
        assert_eq!(
            String::from_utf8_lossy(&stored),
            "\u{FFFD}",
            "the fixture must fold onto the requested id"
        );

        let held_object = attachment_file(&paths, &database, "objects", &held);
        write(&held_object, b"object");

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["\u{FFFD}"], SystemTime::now()).deleting(),
        )
        .expect("an unreadable session key must not fail the pass");

        assert!(
            held_object.is_file(),
            "a row this crate cannot attribute keeps the object it names"
        );
        assert!(
            !report
                .artifacts
                .iter()
                .any(|artifact| artifact.path.to_string_lossy().contains(&held)),
            "the object must not even be projected as reclaimable: {:?}",
            report.artifacts
        );
    }

    /// An attachment class held back by untrusted text is reported, not a clean zero.
    ///
    /// One surviving `part` row lists 200 digests as prose — the shape a tool result or a
    /// model turn can author — and every one of those objects belongs to a session this pass
    /// is deleting. The scan keeps all 200, which is the safe direction and stays, but the
    /// report used to say `total_bytes = 0` with no warning at all: model output had decided
    /// the operator's reclamation ceiling and nothing in the report said so.
    ///
    /// The transcript object of the same surviving session is kept too, by a stored
    /// reference. It must not be counted here: the benign case is what would spend the
    /// signal that exists for the suppressed one.
    #[test]
    fn attachment_objects_kept_only_by_free_text_are_reported_not_a_silent_zero() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = ArtifactGcPaths::from_data_root(&temp.path().join("data"));
        let database = temp.path().join("prose.db");
        let transcript = "11".repeat(32);
        let mut connection = attachment_database(&database, "ses_live", &transcript);

        let digests: Vec<String> = (0..200_u32).map(|index| format!("{index:064x}")).collect();
        let prose = format!(
            "I reviewed the pruned session's attachments: {}. None of them mattered.",
            digests
                .iter()
                .map(|digest| format!("sha256:{digest}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(
            !prose.contains(&format!("\"sha256:{}\"", digests[0])),
            "no digest here is a stored reference; every one is prose"
        );
        insert_part_payload(&connection, "part_prose", "ses_live", &prose);

        let transcript_object = attachment_file(&paths, &database, "objects", &transcript);
        write(&transcript_object, b"object");
        let objects: Vec<PathBuf> = digests
            .iter()
            .map(|digest| attachment_file(&paths, &database, "objects", digest))
            .collect();
        for path in &objects {
            write(path, b"object");
        }

        let report = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_pruned"], SystemTime::now()).deleting(),
        )
        .expect("collect attachments");

        for path in &objects {
            assert!(
                path.is_file(),
                "a digest a surviving row names is never a candidate: {}",
                path.display()
            );
        }
        assert!(transcript_object.is_file());
        assert!(
            report.artifacts.is_empty() && report.total_bytes == 0,
            "the pass reclaimed nothing, which is exactly why it has to say why"
        );
        assert_eq!(
            report.pinned_attachments,
            Some(PinnedAttachments {
                database: database.to_string_lossy().into_owned(),
                objects: 200,
                digests: 200,
                unscanned_rows: 0,
            }),
            "the report must name the suppressed class, and must not count the transcript \
             object that a stored reference keeps"
        );
        let rendered = report
            .pinned_attachments
            .as_ref()
            .expect("pinned record")
            .to_string();
        assert_eq!(
            rendered,
            format!(
                "`{}` kept 200 attachment objects whose 200 digests surviving rows name \
                 only as free text; model- or tool-authored content can produce that \
                 spelling, so those bytes are not reclaimable while such a row survives.",
                database.display()
            ),
            "this sentence is the operator-visible value `session_prune` forwards"
        );
    }

    /// The one spelling the byte scan cannot see is not a spelling this repository writes.
    ///
    /// `collect_attachment_digests` matches the literal bytes `sha256:`, so an id whose
    /// prefix a writer escaped as `\u0073ha256:` is invisible to it and its object would be
    /// reclaimed. That residual is bounded by what the serializers actually emit: neither
    /// `serde_json` nor `JSON.stringify` escapes an ASCII letter or a hex digit.
    #[test]
    fn an_ascii_escaped_prefix_is_not_what_this_crate_writes() {
        let digest = "ab".repeat(32);
        let id = format!("sha256:{digest}");
        let document = serde_json::json!({"attachment": {"id": id.clone()}}).to_string();
        assert!(
            document.contains(&format!("\"{id}\"")),
            "the id is persisted verbatim: {document}"
        );
        assert!(
            !document.contains("\\u"),
            "no ASCII escape appears in a serialized attachment id: {document}"
        );

        let mut live = LiveDigests::default();
        collect_attachment_digests(document.as_bytes(), &mut live);
        assert!(live.pins(&digest), "the real spelling pins the object");
        assert!(
            !live.text_only(&digest),
            "and is classified as a stored reference, not as free text"
        );

        let escaped = document.replace("sha256:", "\\u0073ha256:");
        let mut live = LiveDigests::default();
        collect_attachment_digests(escaped.as_bytes(), &mut live);
        assert!(
            !live.pins(&digest),
            "an escaped prefix is the known residual: no writer here produces it"
        );
    }
}
