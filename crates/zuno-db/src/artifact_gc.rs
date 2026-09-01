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

use rusqlite::{Connection, TransactionBehavior};
use zuno_error::DbError;
use zuno_snapshot::{SessionRef, SnapshotError};

use crate::open;

/// Default retention window for unattributable tool output.
pub const DEFAULT_TOOL_OUTPUT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactGcPaths {
    /// `$DATA/snapshot`.
    pub snapshots: PathBuf,
    /// `$DATA/tool-output`.
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

/// Stable, inspectable output for preview and deletion modes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactGcReport {
    /// Every safely reclaimable path in deterministic order.
    pub artifacts: Vec<ReclaimedArtifact>,
    /// Sum of candidate content bytes.
    pub total_bytes: u64,
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
    ensure_visible_session_owners(&transaction)?;
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
    discover_snapshot_candidates(paths, &survivors, &mut candidates)?;
    discover_tool_output_candidates(paths, request, &deleted_session_ids, &mut candidates)?;
    discover_attachment_candidates(&transaction, paths, &deleted_session_ids, &mut candidates)?;
    candidates.sort_by(|left, right| left.path.cmp(&right.path));

    let mut report = ArtifactGcReport::default();
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
    let session_count = connection
        .query_row("SELECT count(*) FROM session", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(open::map_error)?
        .unsigned_abs();
    if session_count > 0 {
        return Ok(());
    }
    Err(ArtifactGcError::NoVisibleSessions {
        database: connection
            .path()
            .unwrap_or(zuno_paths::MEMORY_SENTINEL)
            .to_owned(),
        session_count,
    })
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

fn discover_tool_output_candidates(
    paths: &ArtifactGcPaths,
    request: &ArtifactGcRequest,
    deleted_session_ids: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
) -> Result<(), ArtifactGcError> {
    if !is_real_directory(&paths.tool_output)? {
        return Ok(());
    }
    let cutoff = request.now.checked_sub(request.tool_output_retention);
    for entry in read_directory(&paths.tool_output)? {
        let path = entry.path();
        if !entry_is_regular_file(&entry)? {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(zuno_tool::store::FILE_PREFIX) {
            continue;
        }
        let reason = match zuno_tool::store::session_of(&path) {
            Some(session_id) if deleted_session_ids.contains(session_id) => {
                Some(ReclaimReason::DeletedSession(session_id.to_owned()))
            }
            Some(_) => None,
            None => {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
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
    for directory in [root.join("objects"), root.join("derived")] {
        discover_unreferenced_attachment_files(&directory, &live, candidates)?;
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

fn live_attachment_digests(
    connection: &Connection,
    deleted_session_ids: &BTreeSet<String>,
) -> Result<BTreeSet<String>, ArtifactGcError> {
    let mut statement = connection
        .prepare("SELECT session_id, data FROM part ORDER BY id")
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(open::map_error)?;
    let mut live = BTreeSet::new();
    for row in rows {
        let (session_id, data) = row.map_err(open::map_error)?;
        if deleted_session_ids.contains(&session_id) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("file") {
            continue;
        }
        if let Some(digest) = value
            .pointer("/attachment/id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.strip_prefix("sha256:"))
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            live.insert(digest.to_owned());
        }
    }
    Ok(live)
}

fn discover_unreferenced_attachment_files(
    root: &Path,
    live: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
) -> Result<(), ArtifactGcError> {
    if !is_real_directory(root)? {
        return Ok(());
    }
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
            if digest.len() != 64 || live.contains(digest) {
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
    Ok(())
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

fn entry_is_regular_file(entry: &fs::DirEntry) -> Result<bool, ArtifactGcError> {
    entry
        .file_type()
        .map(|kind| kind.is_file())
        .map_err(|source| filesystem_error("inspect", &entry.path(), source))
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

    #[test]
    fn zero_total_sessions_refuses_gc_and_names_the_database() {
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

        let error = execute(
            &mut connection,
            &paths,
            &ArtifactGcRequest::new(["ses_selected"], SystemTime::now()),
        )
        .expect_err("an empty database cannot prove shared artifacts are unreferenced");
        let message = error.to_string();
        assert!(message.contains(&database_path.to_string_lossy().into_owned()));
        assert!(message.contains("0"), "{message}");
        assert!(unowned.path_in(&paths.snapshots).is_dir());
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
            report
                .artifacts
                .iter()
                .filter(|artifact| artifact.kind == ArtifactKind::AttachmentObject)
                .count(),
            2
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
}
