use crate::format::{FormatFailure, FormatOutcome, METADATA_FAILURES_KEY};
use async_trait::async_trait;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use zuno_error::ToolError;
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPath {
    pub(crate) canonical: PathBuf,
    pub(crate) resource: String,
    pub(crate) external_pattern: Option<String>,
    pub(crate) external_parent: Option<PathBuf>,
}

impl ResolvedPath {
    pub(crate) fn is_external(&self) -> bool {
        self.external_pattern.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileReadReceipt {
    pub(crate) digest: String,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileReadConflictKind {
    ReadRequired,
    StaleRead,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileReadConflict {
    pub(crate) kind: FileReadConflictKind,
    pub(crate) observed_digest: String,
}

impl FileReadConflict {
    pub(crate) fn message(&self, path: &Path) -> String {
        match self.kind {
            FileReadConflictKind::ReadRequired => format!(
                "File must be read before editing. Use the read tool on {}, then retry the edit.",
                slash(path)
            ),
            FileReadConflictKind::StaleRead => format!(
                "File changed after it was read. Read {} again, then retry the edit.",
                slash(path)
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchConflictFence {
    resources: Vec<(PathBuf, FileReadReceipt)>,
    conflict: IdenticalPatchConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdenticalPatchConflict {
    pub(crate) resource: String,
    pub(crate) observed_digest: Option<String>,
    pub(crate) hunk_index: Option<usize>,
    pub(crate) hunk_header: Option<String>,
}

#[derive(Debug, Default)]
struct FileAccessRecords {
    reads: HashMap<(String, PathBuf), FileReadReceipt>,
    patch_conflicts: HashMap<(String, String), PatchConflictFence>,
    generation: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FileAccessState {
    records: Arc<Mutex<FileAccessRecords>>,
}

impl FileAccessState {
    pub(crate) fn record_read(&self, session_id: &str, path: &Path, bytes: &[u8]) {
        let mut records = self.records.lock().expect("file read-state lock");
        records.generation = records.generation.wrapping_add(1);
        let generation = records.generation;
        records.reads.insert(
            (session_id.to_owned(), path.to_owned()),
            FileReadReceipt {
                digest: digest_bytes(bytes),
                generation,
            },
        );
    }

    pub(crate) fn require_current_read(
        &self,
        session_id: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Result<FileReadReceipt, FileReadConflict> {
        let observed_digest = digest_bytes(bytes);
        let expected = self
            .records
            .lock()
            .expect("file read-state lock")
            .reads
            .get(&(session_id.to_owned(), path.to_owned()))
            .cloned();
        let Some(expected) = expected else {
            return Err(FileReadConflict {
                kind: FileReadConflictKind::ReadRequired,
                observed_digest,
            });
        };
        if expected.digest != observed_digest {
            return Err(FileReadConflict {
                kind: FileReadConflictKind::StaleRead,
                observed_digest,
            });
        }
        Ok(expected)
    }

    pub(crate) fn record_patch_conflict(
        &self,
        session_id: &str,
        operation_digest: &str,
        resources: Vec<(PathBuf, FileReadReceipt)>,
        conflict: IdenticalPatchConflict,
    ) {
        self.records
            .lock()
            .expect("file read-state lock")
            .patch_conflicts
            .insert(
                (session_id.to_owned(), operation_digest.to_owned()),
                PatchConflictFence {
                    resources,
                    conflict,
                },
            );
    }

    pub(crate) fn identical_patch_conflict(
        &self,
        session_id: &str,
        operation_digest: &str,
        resources: &[(PathBuf, FileReadReceipt)],
    ) -> Option<IdenticalPatchConflict> {
        let records = self.records.lock().expect("file read-state lock");
        let fence = records
            .patch_conflicts
            .get(&(session_id.to_owned(), operation_digest.to_owned()))?;
        if fence.resources.len() != resources.len() {
            return None;
        }
        let same_images = fence.resources.iter().zip(resources).all(
            |((previous_path, previous), (current_path, current))| {
                // A fresh read advances the generation, but an unchanged
                // authoritative image still makes the identical failed patch a
                // deterministic replay.
                let _reread_after_conflict = current.generation != previous.generation;
                previous_path == current_path && previous.digest == current.digest
            },
        );
        same_images.then(|| fence.conflict.clone())
    }

    pub(crate) fn clear_patch_conflict(&self, session_id: &str, operation_digest: &str) {
        self.records
            .lock()
            .expect("file read-state lock")
            .patch_conflicts
            .remove(&(session_id.to_owned(), operation_digest.to_owned()));
    }

    pub(crate) fn record_write(&self, session_id: &str, path: &Path, bytes: &[u8]) {
        self.record_read(session_id, path, bytes);
    }

    pub(crate) fn forget(&self, path: &Path) {
        let mut records = self.records.lock().expect("file read-state lock");
        records.reads.retain(|(_, recorded), _| recorded != path);
        records
            .patch_conflicts
            .retain(|_, fence| !fence.resources.iter().any(|(recorded, _)| recorded == path));
    }
}

#[must_use]
pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The seam Todo 39 left for the formatter runtime, now filled by
/// [`crate::format::Formatters`].
#[async_trait]
pub trait FileFormatter: Send + Sync {
    /// Format `path` after a successful write. Returns whether bytes changed.
    async fn format(&self, path: &Path) -> io::Result<bool>;

    /// Format `path`, reporting which formatters failed rather than only whether
    /// the bytes changed.
    ///
    /// This exists as a second method with a default body rather than as a wider
    /// return type on [`FileFormatter::format`] so that every implementation
    /// written against the original seam keeps compiling and keeps working. The
    /// default says the honest thing about such an implementation: it changed the
    /// bytes or it did not, and it has no failures to report because it has no way
    /// to express one.
    ///
    /// An implementation must not report a formatter's failure as `Err`. The write
    /// has already landed by the time this is called, so an `Err` here would make
    /// the tool tell the model its edit failed when the edit is on disk — which is
    /// exactly the confusion [`crate::format::FormatOutcome::failures`] exists to
    /// prevent.
    async fn format_reporting(&self, path: &Path) -> io::Result<FormatOutcome> {
        Ok(FormatOutcome {
            changed: self.format(path).await?,
            failures: Vec::new(),
        })
    }
}

/// Formatter for a host that has no formatter configuration at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFormatter;

#[async_trait]
impl FileFormatter for NoopFormatter {
    async fn format(&self, _path: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

#[derive(Clone)]
pub(crate) struct FileToolRuntime {
    workspace: PathBuf,
    pub(crate) state: FileAccessState,
    pub(crate) formatter: Arc<dyn FileFormatter>,
    pub(crate) mutation: Arc<tokio::sync::Mutex<()>>,
}

impl FileToolRuntime {
    pub(crate) fn new(workspace: &Path, formatter: Arc<dyn FileFormatter>) -> io::Result<Self> {
        Ok(Self {
            workspace: workspace.canonicalize()?,
            state: FileAccessState::default(),
            formatter,
            mutation: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(crate) fn resolve(&self, input: &Path, kind: PathKind) -> io::Result<ResolvedPath> {
        let absolute = if input.is_absolute() {
            normalize_absolute(input)
        } else {
            normalize_absolute(&self.workspace.join(input))
        };
        let canonical = canonicalize_allow_missing(&absolute)?;
        let external = !canonical.starts_with(&self.workspace);
        let resource = if external {
            slash(&canonical)
        } else {
            let relative = canonical
                .strip_prefix(&self.workspace)
                .unwrap_or(Path::new(""));
            if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                slash(relative)
            }
        };
        let (external_pattern, external_parent) = if external {
            let directory = if kind == PathKind::Directory && canonical.is_dir() {
                canonical.clone()
            } else {
                canonical
                    .parent()
                    .map(Path::to_owned)
                    .unwrap_or_else(|| canonical.clone())
            };
            (Some(slash(&directory.join("*"))), Some(directory))
        } else {
            (None, None)
        };
        Ok(ResolvedPath {
            canonical,
            resource,
            external_pattern,
            external_parent,
        })
    }

    pub(crate) async fn authorize(
        &self,
        tool: &str,
        permission: &str,
        target: &ResolvedPath,
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
        if let (Some(pattern), Some(parent)) = (
            target.external_pattern.as_ref(),
            target.external_parent.as_ref(),
        ) {
            let mut metadata = Map::new();
            metadata.insert(
                "filepath".to_owned(),
                Value::String(slash(&target.canonical)),
            );
            metadata.insert("parentDir".to_owned(), Value::String(slash(parent)));
            ctx.ask(
                tool,
                PermissionAsk {
                    permission: "external_directory".to_owned(),
                    patterns: vec![pattern.clone()],
                    metadata,
                    always: vec![pattern.clone()],
                    ..PermissionAsk::default()
                },
            )
            .await?;
        }

        let mut metadata = Map::new();
        metadata.insert(
            "filepath".to_owned(),
            Value::String(slash(&target.canonical)),
        );
        ctx.ask(
            tool,
            PermissionAsk {
                permission: permission.to_owned(),
                patterns: vec![target.resource.clone()],
                metadata,
                always: vec!["*".to_owned()],
                ..PermissionAsk::default()
            },
        )
        .await
    }

    pub(crate) fn title(&self, target: &ResolvedPath) -> String {
        if target.is_external() {
            slash(&target.canonical)
        } else {
            target.resource.clone()
        }
    }
}

pub(crate) struct TextFile {
    pub(crate) bom: bool,
    pub(crate) text: String,
}

pub(crate) fn decode_text(bytes: &[u8]) -> Result<TextFile, std::string::FromUtf8Error> {
    let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
    let content = if bom { &bytes[3..] } else { bytes };
    String::from_utf8(content.to_vec()).map(|text| TextFile { bom, text })
}

pub(crate) fn encode_text(text: &str, bom: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + usize::from(bom) * 3);
    if bom {
        bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Attach formatter failures to a *successful* tool result.
///
/// Both the text and the metadata carry them. The text is there because that is
/// what the model reads, and a formatter that is misconfigured stays misconfigured
/// until somebody is told; the metadata is there because a UI needs the fields
/// rather than a sentence. Nothing is attached when there is nothing to say, so an
/// ordinary edit's output is byte-identical to what it was before formatters ran.
pub(crate) fn report_formatting(mut output: ToolOutput, failures: &[FormatFailure]) -> ToolOutput {
    if failures.is_empty() {
        return output;
    }
    output.output.push_str("\n\n");
    output.output.push_str(
        &failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    output.with_metadata(
        METADATA_FAILURES_KEY,
        Value::Array(failures.iter().map(FormatFailure::to_metadata).collect()),
    )
}

/// Report a post-write problem without claiming that the already-applied write failed.
pub(crate) fn report_post_write_warnings(
    mut output: ToolOutput,
    warnings: &[String],
) -> ToolOutput {
    if warnings.is_empty() {
        return output;
    }
    output.output.push_str("\n\n");
    output.output.push_str(&warnings.join("\n"));
    output.with_metadata(
        "postWriteWarnings",
        Value::Array(warnings.iter().cloned().map(Value::String).collect()),
    )
}

/// Attach the patch of `old` → `new` to a mutation's result, when there is one.
///
/// The post-image is the bytes re-read *after* the formatter ran, so the patch describes
/// what actually landed on disk rather than what the tool asked for. That is the whole
/// point of taking it here instead of inside each tool's replacement arithmetic: an edit
/// followed by `rustfmt` changed more than the edit did, and a patch that omits the
/// formatter's part is a patch of a file state that never existed.
///
/// A file with no line-oriented patch — binary, or unchanged after formatting — attaches
/// nothing, so `metadata["diff"]` being present means "here is the change" and its
/// absence means "there is none to show". See [`crate::diff`] for why this is metadata
/// and not output.
pub(crate) fn report_diff(
    mut output: ToolOutput,
    path: &Path,
    label: &str,
    old: Option<&[u8]>,
    new: &[u8],
) -> ToolOutput {
    if let Some(patch) = crate::diff::unified_diff_bytes(label, old.unwrap_or_default(), new) {
        output = output.with_metadata(crate::diff::METADATA_DIFF_KEY, patch);
    }
    if let Some(diff) = crate::diff::file_diff_bytes(path, old, new) {
        output = output.with_file_diff(diff);
    }
    output
}

pub(crate) fn write_with_dirs(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

pub(crate) fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_owned(),
        source: Box::new(io::Error::other(message.into())),
    }
}

pub(crate) fn failed<E>(tool: &str, error: E) -> ToolError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(error),
    }
}

pub(crate) fn uncertain<E>(tool: &str, paths: &[PathBuf], error: E) -> ToolError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ToolError::Uncertain {
        tool: tool.to_owned(),
        applied_paths: paths.iter().map(|path| slash(path)).collect(),
        source: Box::new(error),
    }
}

pub(crate) fn interrupted(tool: &str) -> ToolError {
    failed(
        tool,
        io::Error::new(io::ErrorKind::Interrupted, "operation interrupted"),
    )
}

pub(crate) fn check_interrupt(tool: &str, ctx: &ToolContext) -> Result<(), ToolError> {
    if ctx.is_interrupted() {
        Err(interrupted(tool))
    } else {
        Ok(())
    }
}

pub(crate) fn slash(path: &Path) -> String {
    zuno_paths::wire_path(path)
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut rooted = false;
    let mut parts: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(value) => parts.push(value.to_owned()),
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(value) = prefix {
        normalized.push(value);
    }
    if rooted {
        normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    for part in parts {
        normalized.push(part);
    }
    normalized
}

fn canonicalize_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let mut anchor = path.to_owned();
    let mut suffix = Vec::<OsString>::new();
    loop {
        match anchor.canonicalize() {
            Ok(mut canonical) => {
                for part in suffix.iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = anchor.file_name().map(ToOwned::to_owned) else {
                    return Err(error);
                };
                suffix.push(name);
                let Some(parent) = anchor.parent() else {
                    return Err(error);
                };
                anchor = parent.to_owned();
            }
            Err(error) => return Err(error),
        }
    }
}
