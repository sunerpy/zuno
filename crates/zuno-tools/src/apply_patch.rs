mod parser;

use crate::format::FormatFailure;
use crate::read::{
    FileReadConflictKind, FileReadReceipt, FileToolRuntime, IdenticalPatchConflict, PathKind,
    ResolvedPath, check_interrupt, decode_text, digest_bytes, encode_text, failed, interrupted,
    invalid, publish_error, report_formatting, slash, uncertain,
};
use async_trait::async_trait;
use parser::{ChunkLine, PatchOperation, PatchParseError, UpdateChunk, parse_patch};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zuno_error::{ToolError, ToolMutationConflict, ToolMutationConflictKind};
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/apply-patch.txt");

const CONTEXT_RECOVERY: &str = "read the current file and retry with a smaller patch using fresh, \
                                unique context; do not resend the same patch";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyPatchParams {
    /// The full patch text that describes all changes to be made.
    pub patch_text: String,
}

pub struct ApplyPatchTool {
    runtime: Arc<FileToolRuntime>,
}

impl ApplyPatchTool {
    pub(crate) fn new(runtime: Arc<FileToolRuntime>) -> Self {
        Self { runtime }
    }
}

struct FileChange {
    source: ResolvedPath,
    destination: Option<ResolvedPath>,
    kind: ChangeKind,
    /// The file as it was, so the reported patch can be taken against it.
    ///
    /// Captured during preparation rather than re-read during application because a
    /// `Delete` has no pre-image left to read once it has run.
    old_bytes: Option<Vec<u8>>,
    /// The pre-image at a move destination, absent when the destination did not exist.
    destination_old_bytes: Option<Vec<u8>>,
    new_bytes: Vec<u8>,
    /// Digest of the image preparation found at the path this change *writes*, or
    /// `None` when preparation found no file there.
    ///
    /// This is the write target — the move destination for a `Move`, the file itself
    /// otherwise — not the source, because the write is what has to be protected.
    /// [`write_verified`] re-reads the target and compares against this digest at the
    /// instant of the write. A `Delete` writes nothing and never consults it.
    target_digest: Option<String>,
    /// Digest of the image preparation found at the path this change *removes* — the
    /// source of a `Move`, or the file a `Delete` takes away — or `None` for an `Add`,
    /// which removes nothing.
    ///
    /// The counterpart of `target_digest` for the removal side of a change.
    /// [`remove_verified`] re-reads the source and compares against this digest at the
    /// instant of the removal: a removal that skips the check destroys whatever arrived
    /// since preparation, and unlike an overwritten file there is no patch to recover
    /// the bytes from. It is the digest of `old_bytes`, so an in-place `Update` records
    /// it as well but never consults it.
    source_digest: Option<String>,
}

enum PreparedOperation {
    Add {
        source: ResolvedPath,
        content: String,
    },
    Delete {
        source: ResolvedPath,
        old_bytes: Vec<u8>,
    },
    Update {
        source: ResolvedPath,
        destination: Option<ResolvedPath>,
        chunks: Vec<UpdateChunk>,
        old_bytes: Vec<u8>,
        destination_old_bytes: Option<Vec<u8>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            Self::CrLf
        } else {
            Self::Lf
        }
    }

    fn normalize<'a>(self, text: &'a str) -> Cow<'a, str> {
        match self {
            Self::Lf => Cow::Borrowed(text),
            Self::CrLf => Cow::Owned(text.replace("\r\n", "\n")),
        }
    }

    fn restore(self, text: String) -> String {
        match self {
            Self::Lf => text,
            Self::CrLf => text.replace('\n', "\r\n"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Add,
    Update,
    Move,
    Delete,
}

impl ChangeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Move => "move",
            Self::Delete => "delete",
        }
    }
}

#[async_trait]
impl TypedTool for ApplyPatchTool {
    type Params = ApplyPatchParams;

    fn id(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if params.patch_text.is_empty() {
            return Err(invalid("apply_patch", "patchText is required"));
        }
        let operation_digest = digest_bytes(params.patch_text.as_bytes());
        check_interrupt("apply_patch", &ctx)?;
        let operations = parse_patch(&params.patch_text, || ctx.is_interrupted()).map_err(
            |error| match error {
                PatchParseError::Interrupted => interrupted("apply_patch"),
                PatchParseError::Invalid(message) => invalid(
                    "apply_patch",
                    format!("apply_patch verification failed: {message}"),
                ),
            },
        )?;

        let mut authorized = HashSet::new();
        let mut plan = Vec::new();
        for operation in &operations {
            check_interrupt("apply_patch", &ctx)?;
            let source = self
                .runtime
                .resolve(Path::new(operation.path()), PathKind::File)
                .map_err(|error| failed("apply_patch", error))?;
            authorize_once(&self.runtime, &source, &ctx, &mut authorized).await?;
            let destination = if let PatchOperation::Update {
                move_to: Some(move_to),
                ..
            } = operation
            {
                let destination = self
                    .runtime
                    .resolve(Path::new(move_to), PathKind::File)
                    .map_err(|error| failed("apply_patch", error))?;
                authorize_once(&self.runtime, &destination, &ctx, &mut authorized).await?;
                Some(destination)
            } else {
                None
            };
            plan.push(AuthorizedOperation {
                operation,
                source,
                destination,
            });
        }

        let _guard = self.runtime.mutation.lock().await;
        let changes = self.prepare_changes(&plan, &operation_digest, &ctx)?;
        self.runtime
            .state
            .clear_patch_conflict(&ctx.session_id, &operation_digest);
        let mut summaries = Vec::new();
        let mut files = Vec::<Value>::new();
        // Only the paths that still exist afterwards: a deleted file has no diagnostics,
        // and asking a language server about one reports somebody else's absent file.
        let mut written = Vec::<PathBuf>::new();
        let mut formatted_files = Vec::<Value>::new();
        let mut failures = Vec::<FormatFailure>::new();
        let mut applied = Vec::<PathBuf>::new();
        // One patch covering every file this call touched, in application order. A
        // unified diff concatenates by construction — each file re-labels itself with its
        // own `---`/`+++` pair — so the viewer scrolls through the whole change rather
        // than showing whichever file happened to be last.
        let mut patches = Vec::<String>::new();
        let mut file_diffs = Vec::new();
        for change in changes {
            after_effect(check_interrupt("apply_patch", &ctx), &applied)?;
            let source_path = change.source.canonical.clone();
            let target = change.destination.as_ref().unwrap_or(&change.source);
            let target_path = target.canonical.clone();
            // Every branch that writes has already written by the time it formats,
            // so a formatter's failure is collected rather than propagated: one
            // uncooperative formatter must not abandon a patch mid-way.
            //
            // Each write re-verifies its target and each removal re-verifies its
            // source first, so a file that moved since preparation is refused rather
            // than overwritten or removed. While nothing has been applied that
            // refusal is the plain typed conflict; once an earlier file of the same
            // patch is on disk, `after_effect` promotes it to an uncertain outcome,
            // because those bytes really are there and the model must not be told
            // the whole patch failed.
            let formatted = match change.kind {
                ChangeKind::Add | ChangeKind::Update => {
                    after_effect(
                        write_verified(
                            &self.runtime,
                            target,
                            &change.new_bytes,
                            change.target_digest.as_deref(),
                            &operation_digest,
                        ),
                        &applied,
                    )?;
                    applied.push(target_path.clone());
                    after_effect(self.format(&target_path, &mut failures).await, &applied)?
                }
                ChangeKind::Move => {
                    // The source is what the patch was computed against, and the move
                    // removes it last. Checking it before the destination is written
                    // lets a source that has already moved on refuse the whole move
                    // with nothing on disk, instead of leaving a destination computed
                    // from stale bytes beside it; the removal checks again at the
                    // instant it acts, because a check that is not paired with the act
                    // does not protect the act.
                    after_effect(
                        verify_pre_image(
                            &self.runtime,
                            &change.source,
                            change.source_digest.as_deref(),
                            &operation_digest,
                        ),
                        &applied,
                    )?;
                    after_effect(
                        write_verified(
                            &self.runtime,
                            target,
                            &change.new_bytes,
                            change.target_digest.as_deref(),
                            &operation_digest,
                        ),
                        &applied,
                    )?;
                    applied.push(target_path.clone());
                    after_effect(
                        remove_verified(
                            &self.runtime,
                            &change.source,
                            change.source_digest.as_deref(),
                            &operation_digest,
                        ),
                        &applied,
                    )?;
                    applied.push(source_path.clone());
                    self.runtime.state.forget(&source_path);
                    after_effect(self.format(&target_path, &mut failures).await, &applied)?
                }
                ChangeKind::Delete => {
                    after_effect(
                        remove_verified(
                            &self.runtime,
                            &change.source,
                            change.source_digest.as_deref(),
                            &operation_digest,
                        ),
                        &applied,
                    )?;
                    applied.push(source_path.clone());
                    self.runtime.state.forget(&source_path);
                    false
                }
            };
            if change.kind != ChangeKind::Delete {
                let final_bytes = after_effect(
                    self.runtime
                        .anchor_file("apply_patch", target, false)
                        .and_then(|anchored| {
                            anchored
                                .read()
                                .map_err(|error| failed("apply_patch", error))
                        }),
                    &applied,
                )?;
                written.push(target_path.clone());
                self.runtime
                    .state
                    .record_write(&ctx.session_id, &target_path, &final_bytes);
                if formatted {
                    formatted_files.push(Value::String(slash(&target_path)));
                }
            }
            let marker = match change.kind {
                ChangeKind::Add => "A",
                ChangeKind::Delete => "D",
                ChangeKind::Update | ChangeKind::Move => "M",
            };
            let relative = self.runtime.title(target);
            // The post-image is taken from disk for anything that still exists, so a
            // formatter's contribution is inside the patch; a delete's post-image is
            // empty because there is no file left to read.
            let post = if change.kind == ChangeKind::Delete {
                Vec::new()
            } else {
                after_effect(
                    std::fs::read(&target_path).map_err(|error| failed("apply_patch", error)),
                    &applied,
                )?
            };
            if let Some(patch) = crate::diff::unified_diff_bytes(
                &relative,
                change.old_bytes.as_deref().unwrap_or_default(),
                &post,
            ) {
                patches.push(patch);
            }
            match change.kind {
                ChangeKind::Add => {
                    if let Some(diff) = crate::diff::file_diff_bytes(&target_path, None, &post) {
                        file_diffs.push(diff);
                    }
                }
                ChangeKind::Update => {
                    if let Some(diff) = crate::diff::file_diff_bytes(
                        &target_path,
                        change.old_bytes.as_deref(),
                        &post,
                    ) {
                        file_diffs.push(diff);
                    }
                }
                ChangeKind::Move => {
                    if let Some(diff) =
                        crate::diff::file_diff_bytes(&source_path, change.old_bytes.as_deref(), &[])
                    {
                        file_diffs.push(diff);
                    }
                    if let Some(diff) = crate::diff::file_diff_bytes(
                        &target_path,
                        change.destination_old_bytes.as_deref(),
                        &post,
                    ) {
                        file_diffs.push(diff);
                    }
                }
                ChangeKind::Delete => {
                    if let Some(diff) =
                        crate::diff::file_diff_bytes(&source_path, change.old_bytes.as_deref(), &[])
                    {
                        file_diffs.push(diff);
                    }
                }
            }
            summaries.push(format!("{marker} {relative}"));
            files.push(json!({
                "filePath": slash(&source_path),
                "relativePath": relative,
                "type": change.kind.label(),
                "movePath": change.destination.as_ref().map(|item| slash(&item.canonical)),
            }));
        }
        let output = format!(
            "Success. Updated the following files:\n{}",
            summaries.join("\n")
        );
        let mut result = ToolOutput::text(output.clone(), output)
            .with_metadata("files", Value::Array(files))
            .with_metadata("formattedFiles", Value::Array(formatted_files));
        for path in &written {
            result = result.with_written_path(path);
        }
        if !patches.is_empty() {
            result = result.with_metadata(crate::diff::METADATA_DIFF_KEY, patches.concat());
        }
        for diff in file_diffs {
            result = result.with_file_diff(diff);
        }
        Ok(report_formatting(result, &failures))
    }
}

fn after_effect<T>(result: Result<T, ToolError>, applied: &[PathBuf]) -> Result<T, ToolError> {
    result.map_err(|error| {
        if applied.is_empty() {
            error
        } else {
            uncertain("apply_patch", applied, error)
        }
    })
}

/// A patch operation whose paths were resolved exactly once, before authorization.
///
/// Resolving a path again after the permission prompt re-canonicalizes it, and a
/// symlink planted in between then points the operation at a directory the user never
/// approved — the same check-then-use gap the anchored write primitive exists to close,
/// reopened one layer up. The resolution the user authorized is therefore the only one
/// preparation and application use.
struct AuthorizedOperation<'a> {
    operation: &'a PatchOperation,
    source: ResolvedPath,
    destination: Option<ResolvedPath>,
}

impl ApplyPatchTool {
    /// Format one written file, accumulating any failure instead of raising it.
    ///
    /// The `Err` arm is reachable only when the formatter implementation itself is
    /// broken — a conforming one reports formatter failures in the outcome — so it
    /// stays a real error rather than being folded into the report.
    async fn format(
        &self,
        path: &Path,
        failures: &mut Vec<FormatFailure>,
    ) -> Result<bool, ToolError> {
        let outcome = self
            .runtime
            .formatter
            .format_reporting(path)
            .await
            .map_err(|error| failed("apply_patch", error))?;
        failures.extend(outcome.failures);
        Ok(outcome.changed)
    }

    fn prepare_changes(
        &self,
        plan: &[AuthorizedOperation<'_>],
        operation_digest: &str,
        ctx: &ToolContext,
    ) -> Result<Vec<FileChange>, ToolError> {
        let mut prepared = Vec::new();
        let mut resources = Vec::new();
        let mut touched = HashSet::<PathBuf>::new();
        for authorized in plan {
            check_interrupt("apply_patch", ctx)?;
            let operation = authorized.operation;
            let source = authorized.source.clone();
            if !touched.insert(source.canonical.clone()) {
                return Err(invalid(
                    "apply_patch",
                    format!(
                        "apply_patch verification failed: file appears in more than one operation: {}",
                        zuno_paths::wire_path(&source.canonical)
                    ),
                ));
            }
            match operation {
                PatchOperation::Add { content, .. } => {
                    if source.canonical.exists() {
                        return Err(invalid(
                            "apply_patch",
                            format!(
                                "apply_patch verification failed: file already exists: {}",
                                zuno_paths::wire_path(&source.canonical)
                            ),
                        ));
                    }
                    prepared.push(PreparedOperation::Add {
                        source,
                        content: content.clone(),
                    });
                }
                PatchOperation::Delete { .. } => {
                    ensure_regular_file(&source.canonical)?;
                    let old_bytes = std::fs::read(&source.canonical)
                        .map_err(|error| failed("apply_patch", error))?;
                    let receipt = require_patch_read(
                        &self.runtime,
                        ctx,
                        &source,
                        &old_bytes,
                        operation_digest,
                    )?;
                    resources.push((source.canonical.clone(), receipt));
                    prepared.push(PreparedOperation::Delete { source, old_bytes });
                }
                PatchOperation::Update { chunks, .. } => {
                    ensure_regular_file(&source.canonical)?;
                    let old_bytes = std::fs::read(&source.canonical)
                        .map_err(|error| failed("apply_patch", error))?;
                    let receipt = require_patch_read(
                        &self.runtime,
                        ctx,
                        &source,
                        &old_bytes,
                        operation_digest,
                    )?;
                    resources.push((source.canonical.clone(), receipt));
                    // The destination the user authorized, not a fresh resolution: a
                    // symlink planted after the prompt must not be able to move the
                    // write somewhere else.
                    let destination = authorized.destination.clone();
                    if let Some(target) = &destination
                        && !touched.insert(target.canonical.clone())
                    {
                        return Err(invalid(
                            "apply_patch",
                            format!(
                                "apply_patch verification failed: destination appears in more than one operation: {}",
                                zuno_paths::wire_path(&target.canonical)
                            ),
                        ));
                    }
                    let destination_old_bytes = destination
                        .as_ref()
                        .map(|target| read_optional_file(&target.canonical))
                        .transpose()?
                        .flatten();
                    if let (Some(target), Some(bytes)) =
                        (destination.as_ref(), destination_old_bytes.as_deref())
                    {
                        let receipt = require_patch_read(
                            &self.runtime,
                            ctx,
                            target,
                            bytes,
                            operation_digest,
                        )?;
                        resources.push((target.canonical.clone(), receipt));
                    }
                    prepared.push(PreparedOperation::Update {
                        source,
                        destination,
                        chunks: chunks.clone(),
                        old_bytes,
                        destination_old_bytes,
                    });
                }
            }
        }

        if let Some(IdenticalPatchConflict {
            resource,
            observed_digest,
            hunk_index,
            hunk_header,
        }) = self.runtime.state.identical_patch_conflict(
            &ctx.session_id,
            operation_digest,
            &resources,
        ) {
            return Err(mutation_conflict(
                ToolMutationConflictKind::IdenticalReplay,
                resource.clone(),
                operation_digest,
                observed_digest,
                hunk_index,
                hunk_header,
                format!(
                    "apply_patch rejected an identical failed patch for {}; read the current file \
                     and submit a revised patch with smaller, unique context",
                    resource
                ),
            ));
        }

        let mut changes = Vec::with_capacity(prepared.len());
        for operation in prepared {
            check_interrupt("apply_patch", ctx)?;
            match operation {
                PreparedOperation::Add { source, content } => changes.push(FileChange {
                    source,
                    destination: None,
                    kind: ChangeKind::Add,
                    old_bytes: None,
                    destination_old_bytes: None,
                    new_bytes: content.into_bytes(),
                    // Preparation established that nothing is there, so the write must
                    // create the file rather than replace whatever arrived since.
                    target_digest: None,
                    // An add removes nothing, so there is no source image to hold.
                    source_digest: None,
                }),
                PreparedOperation::Delete { source, old_bytes } => {
                    // A delete has no write target to protect, only the image its
                    // removal has to still find.
                    let source_digest = digest_bytes(&old_bytes);
                    changes.push(FileChange {
                        source,
                        destination: None,
                        kind: ChangeKind::Delete,
                        old_bytes: Some(old_bytes),
                        destination_old_bytes: None,
                        new_bytes: Vec::new(),
                        target_digest: None,
                        source_digest: Some(source_digest),
                    });
                }
                PreparedOperation::Update {
                    source,
                    destination,
                    chunks,
                    old_bytes,
                    destination_old_bytes,
                } => {
                    let decoded =
                        decode_text(&old_bytes).map_err(|error| failed("apply_patch", error))?;
                    let line_ending = LineEnding::detect(&decoded.text);
                    let normalized = line_ending.normalize(&decoded.text);
                    let observed_digest = digest_bytes(&old_bytes);
                    let content = match apply_chunks(
                        &source,
                        &normalized,
                        &chunks,
                        operation_digest,
                        &observed_digest,
                        ctx,
                    ) {
                        Ok(content) => content,
                        Err(error @ ToolError::MutationConflict { .. }) => {
                            let ToolError::MutationConflict { conflict, .. } = &error else {
                                unreachable!("matched mutation conflict");
                            };
                            self.runtime.state.record_patch_conflict(
                                &ctx.session_id,
                                operation_digest,
                                resources.clone(),
                                IdenticalPatchConflict {
                                    resource: conflict.resource.clone(),
                                    observed_digest: conflict.observed_digest.clone(),
                                    hunk_index: conflict.hunk_index,
                                    hunk_header: conflict.hunk_header.clone(),
                                },
                            );
                            return Err(error);
                        }
                        Err(error) => return Err(error),
                    };
                    let kind = if destination.is_some() {
                        ChangeKind::Move
                    } else {
                        ChangeKind::Update
                    };
                    // A move writes the destination, so the destination's pre-image is
                    // what the write has to still find; an in-place update writes the
                    // file the patch was computed against.
                    let target_digest = if destination.is_some() {
                        destination_old_bytes.as_deref().map(digest_bytes)
                    } else {
                        Some(observed_digest.clone())
                    };
                    changes.push(FileChange {
                        source,
                        destination,
                        kind,
                        old_bytes: Some(old_bytes),
                        destination_old_bytes,
                        new_bytes: encode_text(&line_ending.restore(content), decoded.bom),
                        target_digest,
                        // The source a move removes is the file the patch was computed
                        // against, which is exactly the image `observed_digest` names.
                        source_digest: Some(observed_digest),
                    });
                }
            }
        }
        Ok(changes)
    }
}

impl PatchOperation {
    fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

async fn authorize_once(
    runtime: &FileToolRuntime,
    target: &ResolvedPath,
    ctx: &ToolContext,
    authorized: &mut HashSet<PathBuf>,
) -> Result<(), ToolError> {
    if authorized.insert(target.canonical.clone()) {
        runtime
            .authorize("apply_patch", "edit", target, ctx)
            .await?;
    }
    Ok(())
}

fn require_patch_read(
    runtime: &FileToolRuntime,
    ctx: &ToolContext,
    target: &ResolvedPath,
    bytes: &[u8],
    operation_digest: &str,
) -> Result<FileReadReceipt, ToolError> {
    runtime
        .state
        .require_current_read(&ctx.session_id, &target.canonical, bytes)
        .map_err(|conflict| {
            let kind = match conflict.kind {
                FileReadConflictKind::ReadRequired => ToolMutationConflictKind::ReadRequired,
                FileReadConflictKind::StaleRead => ToolMutationConflictKind::StaleRead,
            };
            let message = match conflict.kind {
                FileReadConflictKind::ReadRequired => format!(
                    "apply_patch requires a current read of {} before modifying it",
                    target.canonical.display()
                ),
                FileReadConflictKind::StaleRead => format!(
                    "{} changed after it was read; read it again before applying a revised patch",
                    target.canonical.display()
                ),
            };
            mutation_conflict(
                kind,
                target.resource.clone(),
                operation_digest,
                Some(conflict.observed_digest),
                None,
                None,
                message,
            )
        })
}

/// Write `bytes` to the change's target, but only while the target still holds the
/// image preparation validated.
///
/// `prepare_changes` digests every file it is about to change and refuses a patch
/// whose read receipt is stale. That proves the file was unchanged when it was
/// *read*, which is not when it is written: the patch is applied in memory first,
/// a formatter may run over an earlier file of the same patch, and the worktree is
/// shared with background jobs, other tool calls and the user's editor. Without
/// this second check a concurrent edit is silently overwritten and the model
/// reports success over somebody's lost work. Re-reading here narrows the window
/// to one read-then-write pair; it cannot close it entirely without locking the
/// filesystem, and narrowing it is what makes the loss observable instead of
/// silent.
///
/// `expected_digest` is `None` when preparation found no file at the target. Such
/// a write must *create* the file, never replace one, so it goes through
/// `create_new`: the existence test and the creation are a single filesystem
/// operation. `exists()` followed by a write is two operations, and a file that
/// appears between them is destroyed by the second — the race this argument
/// exists to avoid.
///
/// # Errors
///
/// [`ToolError::MutationConflict`] carrying
/// [`ToolMutationConflictKind::StaleRead`] when the target no longer holds the
/// prepared image, whether it was modified, removed, or created after preparation
/// found it absent. Nothing is written in that case, so the concurrent content
/// survives. [`ToolError::Failed`] for any other filesystem failure, including a
/// publication whose result was lost, which is [`ToolError::Uncertain`] instead,
/// because those bytes may be on disk and re-running the patch must not be the way the
/// harness finds out.
///
/// Every filesystem step runs through an anchor pinned to the directory the user
/// authorized, so an ancestor replaced by a symlink after the permission prompt cannot
/// redirect the write out of that directory.
fn write_verified(
    runtime: &FileToolRuntime,
    target: &ResolvedPath,
    bytes: &[u8],
    expected_digest: Option<&str>,
    operation_digest: &str,
) -> Result<(), ToolError> {
    let anchored = runtime.anchor_file("apply_patch", target, true)?;
    let applied = [target.canonical.clone()];
    let Some(expected) = expected_digest else {
        return match anchored.create_new(bytes) {
            Ok(()) => Ok(()),
            Err(failure) if failure.error_kind() == io::ErrorKind::AlreadyExists => {
                Err(moved_pre_image(
                    target,
                    operation_digest,
                    None,
                    format!(
                        "apply_patch refused to overwrite {}: the file was created after the \
                         patch was verified against its absence; read the current file and submit \
                         a patch against it",
                        target.canonical.display()
                    ),
                ))
            }
            Err(failure) => Err(publish_error("apply_patch", &applied, failure)),
        };
    };
    let current = match anchored.read() {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(moved_pre_image(
                target,
                operation_digest,
                None,
                format!(
                    "apply_patch refused to write {}: the file was removed after it was read; read \
                     the current file and submit a patch against it",
                    target.canonical.display()
                ),
            ));
        }
        Err(error) => return Err(failed("apply_patch", error)),
    };
    let observed = digest_bytes(&current);
    if observed != expected {
        return Err(moved_pre_image(
            target,
            operation_digest,
            Some(observed),
            format!(
                "apply_patch refused to write {}: the file changed after it was read and before \
                 the write; read the current file and submit a patch against it",
                target.canonical.display()
            ),
        ));
    }
    anchored
        .publish(bytes)
        .map_err(|failure| publish_error("apply_patch", &applied, failure))
}

/// Confirm `source` still holds the image its digest was taken from, without touching
/// it.
///
/// The read half of [`remove_verified`], split out so a move can consult it before
/// writing its destination. `expected_digest` is the digest preparation recorded for
/// the source; a removal always has one, because preparation reads every file it
/// plans to remove and refuses the patch when it cannot.
///
/// # Errors
///
/// [`ToolError::MutationConflict`] carrying [`ToolMutationConflictKind::StaleRead`]
/// when the source no longer holds the prepared image, whether it was modified or is
/// gone. A source that is already gone is a conflict, not a satisfied removal: the
/// plan's pre-image was the file's content, and absence departs from it as much as
/// different content does. Reporting the removal as done would attribute to this
/// patch a deletion somebody else made, record a diff for work the tool did not do,
/// and hide from the model that another writer is active in the same tree — and
/// [`write_verified`] already treats a vanished target this way, so there is one rule
/// for a pre-image that moved, with one required action: read again.
///
/// [`ToolError::Failed`] when no digest was recorded. Preparation cannot plan a
/// removal without reading the file, so a missing digest is a planning defect rather
/// than a state of the world; with nothing to compare against the removal cannot be
/// shown safe, and an unverifiable removal is refused rather than made blind. Any
/// other filesystem failure is also [`ToolError::Failed`].
fn verify_pre_image(
    runtime: &FileToolRuntime,
    source: &ResolvedPath,
    expected_digest: Option<&str>,
    operation_digest: &str,
) -> Result<(), ToolError> {
    let path = source.canonical.as_path();
    let Some(expected) = expected_digest else {
        return Err(failed(
            "apply_patch",
            io::Error::other(format!(
                "apply_patch has no recorded pre-image for {} and will not remove it unverified",
                path.display()
            )),
        ));
    };
    let anchored = runtime.anchor_file("apply_patch", source, false)?;
    let current = match anchored.read() {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(moved_pre_image(
                source,
                operation_digest,
                None,
                format!(
                    "apply_patch refused to remove {}: the file was removed after it was read; \
                     check the path again and submit a patch against what is there now",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(failed("apply_patch", error)),
    };
    let observed = digest_bytes(&current);
    if observed != expected {
        return Err(moved_pre_image(
            source,
            operation_digest,
            Some(observed),
            format!(
                "apply_patch refused to remove {}: the file changed after it was read and before \
                 the removal; read the current file and submit a patch against it",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Remove the change's source, but only while it still holds the image preparation
/// validated.
///
/// The compare-and-swap [`write_verified`] performs for a write, for the other
/// operation that destroys data. A write that skips the check overwrites a concurrent
/// edit; a removal that skips it destroys the edit outright, and unlike the write
/// there is no patch to recover the bytes from — `old_bytes` is the image the model
/// read, not the one somebody else wrote since. Re-reading here narrows the window to
/// one read-then-remove pair, for the same reason and with the same limits as the
/// write.
///
/// # Errors
///
/// As [`verify_pre_image`], plus [`ToolError::Failed`] when the removal itself fails.
fn remove_verified(
    runtime: &FileToolRuntime,
    source: &ResolvedPath,
    expected_digest: Option<&str>,
    operation_digest: &str,
) -> Result<(), ToolError> {
    verify_pre_image(runtime, source, expected_digest, operation_digest)?;
    runtime
        .anchor_file("apply_patch", source, false)?
        .remove()
        .map_err(|error| failed("apply_patch", error))
}

/// The conflict for a pre-image that moved between preparation and the write or
/// removal.
///
/// [`ToolMutationConflictKind::StaleRead`] is the kind that describes it: the
/// resource changed after the session read it, and `reread` — its required action
/// — is exactly what repairs the situation. `ContextMismatch` would be a lie,
/// because the patch did match the image it was computed against; the file moved
/// underneath it. `ReadRequired` would be wrong because a read receipt exists, and
/// `IdenticalReplay` would be wrong because the next attempt sees different bytes
/// and is therefore not a replay of anything.
fn moved_pre_image(
    target: &ResolvedPath,
    operation_digest: &str,
    observed_digest: Option<String>,
    message: String,
) -> ToolError {
    mutation_conflict(
        ToolMutationConflictKind::StaleRead,
        target.resource.clone(),
        operation_digest,
        observed_digest,
        None,
        None,
        message,
    )
}

fn mutation_conflict(
    kind: ToolMutationConflictKind,
    resource: String,
    operation_digest: &str,
    observed_digest: Option<String>,
    hunk_index: Option<usize>,
    hunk_header: Option<String>,
    message: String,
) -> ToolError {
    ToolError::MutationConflict {
        tool: "apply_patch".to_owned(),
        conflict: Box::new(ToolMutationConflict {
            kind,
            resource,
            operation_digest: operation_digest.to_owned(),
            observed_digest,
            hunk_index,
            hunk_header,
        }),
        source: Box::new(io::Error::other(message)),
    }
}

fn ensure_regular_file(path: &Path) -> Result<(), ToolError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        invalid(
            "apply_patch",
            format!(
                "apply_patch verification failed: failed to read file: {}: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(invalid(
            "apply_patch",
            format!(
                "apply_patch verification failed: path is not a file: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, ToolError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => Err(invalid(
            "apply_patch",
            format!(
                "apply_patch verification failed: move destination is a directory: {}",
                path.display()
            ),
        )),
        Err(error) => Err(failed("apply_patch", error)),
    }
}

fn apply_chunks(
    target: &ResolvedPath,
    source: &str,
    chunks: &[UpdateChunk],
    operation_digest: &str,
    observed_digest: &str,
    ctx: &ToolContext,
) -> Result<String, ToolError> {
    let mut content = source.to_owned();
    let mut cursor = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        check_interrupt("apply_patch", ctx)?;
        let search_start = if let Some(header) = &chunk.header {
            find_header_end(&content, header, cursor).ok_or_else(|| {
                mutation_conflict(
                    ToolMutationConflictKind::ContextMismatch,
                    target.resource.clone(),
                    operation_digest,
                    Some(observed_digest.to_owned()),
                    Some(index + 1),
                    Some(header.clone()),
                    format!(
                        "apply_patch verification failed: hunk header `{header}` was not found in \
                         {}; {CONTEXT_RECOVERY}",
                        target.canonical.display(),
                    ),
                )
            })?
        } else {
            cursor
        };
        let old = chunk
            .lines
            .iter()
            .filter_map(|line| match line {
                ChunkLine::Context(value) | ChunkLine::Remove(value) => Some(value.as_str()),
                ChunkLine::Add(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let new = chunk
            .lines
            .iter()
            .filter_map(|line| match line {
                ChunkLine::Context(value) | ChunkLine::Add(value) => Some(value.as_str()),
                ChunkLine::Remove(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if old.is_empty() {
            let insertion = if new.is_empty() {
                String::new()
            } else {
                format!("{new}\n")
            };
            content.insert_str(search_start, &insertion);
            cursor = search_start + insertion.len();
            continue;
        }
        let Some(position) = find_line_block(&content, &old, search_start, chunk.end_of_file)
        else {
            return Err(mutation_conflict(
                ToolMutationConflictKind::ContextMismatch,
                target.resource.clone(),
                operation_digest,
                Some(observed_digest.to_owned()),
                Some(index + 1),
                chunk.header.clone(),
                format!(
                    "apply_patch verification failed: hunk context was not found in {}; \
                     {CONTEXT_RECOVERY}",
                    target.canonical.display(),
                ),
            ));
        };
        let end = position + old.len();
        content.replace_range(position..end, &new);
        cursor = position + new.len();
    }
    Ok(content)
}

fn find_header_end(content: &str, header: &str, start: usize) -> Option<usize> {
    let mut offset = start;
    for line in content[start..].split_inclusive('\n') {
        let value = line.trim_end_matches(['\r', '\n']);
        if value == header {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    None
}

fn find_line_block(content: &str, block: &str, start: usize, end_of_file: bool) -> Option<usize> {
    content[start..]
        .match_indices(block)
        .find_map(|(relative, _)| {
            let position = start + relative;
            let end = position + block.len();
            let starts_on_line =
                position == 0 || content.as_bytes().get(position - 1) == Some(&b'\n');
            let ends_on_line = end == content.len() || content.as_bytes().get(end) == Some(&b'\n');
            let at_end = !end_of_file || end == content.trim_end_matches('\n').len();
            (starts_on_line && ends_on_line && at_end).then_some(position)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileFormatter, NoopFormatter};
    use zuno_tool::{AllowAll, NeverInterrupted};

    fn target(path: &Path, resource: &str) -> ResolvedPath {
        ResolvedPath {
            canonical: path.to_path_buf(),
            resource: resource.to_owned(),
            external_pattern: None,
            external_parent: None,
        }
    }

    /// A runtime whose authorization boundary is the temporary workspace.
    ///
    /// The anchored write primitive descends from this boundary, so a unit test of the
    /// verified write and removal has to name it just as the tool does.
    fn runtime_at(workspace: &Path) -> FileToolRuntime {
        FileToolRuntime::new(workspace, Arc::new(NoopFormatter)).expect("file tool runtime")
    }

    fn conflict_of(error: &ToolError) -> &ToolMutationConflict {
        let ToolError::MutationConflict { conflict, .. } = error else {
            panic!("expected a typed mutation conflict, got {error:?}");
        };
        conflict
    }

    #[test]
    fn a_file_changed_between_read_and_write_is_never_overwritten() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("raced.txt");
        std::fs::write(&path, "prepared\n").expect("prepared image");
        let prepared = digest_bytes(b"prepared\n");
        std::fs::write(&path, "somebody else\n").expect("concurrent edit");

        let error = write_verified(
            &runtime_at(workspace.path()),
            &target(&path, "raced.txt"),
            b"patched\n",
            Some(&prepared),
            "patch-digest",
        )
        .expect_err("a moved pre-image must refuse the write");

        let conflict = conflict_of(&error);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "raced.txt");
        assert_eq!(conflict.operation_digest, "patch-digest");
        assert_eq!(
            conflict.observed_digest.as_deref(),
            Some(digest_bytes(b"somebody else\n").as_str()),
            "the conflict must report the bytes that are actually on disk"
        );
        assert_eq!(conflict.required_action(), "reread");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the concurrent edit survives"),
            "somebody else\n"
        );
    }

    #[test]
    fn a_file_removed_between_read_and_write_is_reported_as_a_conflict() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("vanished.txt");

        let error = write_verified(
            &runtime_at(workspace.path()),
            &target(&path, "vanished.txt"),
            b"patched\n",
            Some(&digest_bytes(b"prepared\n")),
            "patch-digest",
        )
        .expect_err("a pre-image that no longer exists must refuse the write");

        let conflict = conflict_of(&error);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.observed_digest, None);
        assert!(!path.exists(), "a refused write must not create the file");
    }

    #[test]
    fn an_add_whose_path_appeared_concurrently_is_never_overwritten() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("nested").join("added.txt");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("nested directory");
        std::fs::write(&path, "somebody else\n").expect("concurrent create");

        let error = write_verified(
            &runtime_at(workspace.path()),
            &target(&path, "nested/added.txt"),
            b"added\n",
            None,
            "patch-digest",
        )
        .expect_err("create-new semantics must refuse an existing path");

        let conflict = conflict_of(&error);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "nested/added.txt");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the concurrent create survives"),
            "somebody else\n"
        );
    }

    #[test]
    fn a_target_that_still_holds_its_prepared_image_is_written() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let existing = workspace.path().join("update.txt");
        std::fs::write(&existing, "prepared\n").expect("prepared image");
        let created = workspace.path().join("new").join("added.txt");

        let runtime = runtime_at(workspace.path());
        write_verified(
            &runtime,
            &target(&existing, "update.txt"),
            b"patched\n",
            Some(&digest_bytes(b"prepared\n")),
            "patch-digest",
        )
        .expect("an unchanged target accepts the write");
        write_verified(
            &runtime,
            &target(&created, "new/added.txt"),
            b"added\n",
            None,
            "patch-digest",
        )
        .expect("an absent target is created, parent directories included");

        assert_eq!(
            std::fs::read_to_string(&existing).expect("updated file"),
            "patched\n"
        );
        assert_eq!(
            std::fs::read_to_string(&created).expect("created file"),
            "added\n"
        );
    }

    #[test]
    fn a_source_changed_between_read_and_removal_is_never_removed() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("raced.txt");
        std::fs::write(&path, "prepared\n").expect("prepared image");
        let prepared = digest_bytes(b"prepared\n");
        std::fs::write(&path, "somebody else\n").expect("concurrent edit");

        let error = remove_verified(
            &runtime_at(workspace.path()),
            &target(&path, "raced.txt"),
            Some(&prepared),
            "patch-digest",
        )
        .expect_err("a moved pre-image must refuse the removal");

        let conflict = conflict_of(&error);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "raced.txt");
        assert_eq!(conflict.operation_digest, "patch-digest");
        assert_eq!(
            conflict.observed_digest.as_deref(),
            Some(digest_bytes(b"somebody else\n").as_str()),
            "the conflict must report the bytes that are actually on disk"
        );
        assert_eq!(conflict.required_action(), "reread");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the concurrent edit survives"),
            "somebody else\n"
        );
    }

    #[test]
    fn a_source_removed_between_read_and_removal_is_reported_as_a_conflict() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("vanished.txt");

        let error = remove_verified(
            &runtime_at(workspace.path()),
            &target(&path, "vanished.txt"),
            Some(&digest_bytes(b"prepared\n")),
            "patch-digest",
        )
        .expect_err("a pre-image that no longer exists is a conflict, not a finished removal");

        let conflict = conflict_of(&error);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.observed_digest, None);
        assert_eq!(conflict.required_action(), "reread");
    }

    #[test]
    fn a_removal_without_a_recorded_pre_image_is_refused_and_touches_nothing() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("unverified.txt");
        std::fs::write(&path, "somebody\n").expect("existing file");

        let error = remove_verified(
            &runtime_at(workspace.path()),
            &target(&path, "unverified.txt"),
            None,
            "patch-digest",
        )
        .expect_err("a removal that cannot be verified is not made");

        assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file is untouched"),
            "somebody\n"
        );
    }

    #[test]
    fn a_source_that_still_holds_its_prepared_image_is_removed() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let path = workspace.path().join("delete.txt");
        std::fs::write(&path, "prepared\n").expect("prepared image");

        remove_verified(
            &runtime_at(workspace.path()),
            &target(&path, "delete.txt"),
            Some(&digest_bytes(b"prepared\n")),
            "patch-digest",
        )
        .expect("an unchanged source accepts the removal");

        assert!(!path.exists(), "the verified removal takes the file away");
    }

    // The removal checks, driven through the tool itself. Preparation and application
    // run back to back inside one call, so the only writer a test can schedule between
    // them is the formatter that runs after an earlier file of the same patch — which
    // is also where the window is widest in production.

    const SESSION: &str = "session-patch";

    /// A formatter that edits *another* file while formatting the one it was given.
    ///
    /// It stands in for every writer that shares a worktree with the agent — a
    /// background job, a second tool call, the user's editor. `contents: None` removes
    /// the file instead of rewriting it.
    struct ConcurrentEditor {
        path: PathBuf,
        contents: Option<&'static str>,
    }

    #[async_trait]
    impl FileFormatter for ConcurrentEditor {
        async fn format(&self, _path: &Path) -> io::Result<bool> {
            match self.contents {
                Some(contents) => std::fs::write(&self.path, contents)?,
                None => std::fs::remove_file(&self.path)?,
            }
            Ok(false)
        }
    }

    fn context() -> ToolContext {
        ToolContext::new(
            SESSION,
            "message-patch",
            "call-patch",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    /// The tool over `workspace`, with every path in `read` recorded as read by the
    /// session, so preparation's read-receipt check passes and the re-check at the
    /// instant of the change is the only thing between the patch and the disk.
    fn patch_tool(
        workspace: &Path,
        formatter: Arc<dyn FileFormatter>,
        read: &[&Path],
    ) -> ApplyPatchTool {
        let runtime =
            Arc::new(FileToolRuntime::new(workspace, formatter).expect("file tool runtime"));
        for path in read {
            let resolved = runtime
                .resolve(path, PathKind::File)
                .expect("fixture path resolves");
            let bytes = std::fs::read(&resolved.canonical).expect("fixture to read");
            runtime
                .state
                .record_read(SESSION, &resolved.canonical, &bytes);
        }
        ApplyPatchTool::new(runtime)
    }

    async fn apply(tool: &ApplyPatchTool, patch_text: &str) -> Result<ToolOutput, ToolError> {
        tool.run(
            ApplyPatchParams {
                patch_text: patch_text.to_owned(),
            },
            context(),
        )
        .await
    }

    /// The applied paths and the typed conflict an uncertain outcome carries as its
    /// cause. A refusal after an earlier file landed keeps its full [`ToolError`] in
    /// `#[source]` position, so the kind is still readable off the chain.
    fn uncertain_conflict(error: &ToolError) -> (&[String], &ToolMutationConflict) {
        let ToolError::Uncertain {
            applied_paths,
            source,
            ..
        } = error
        else {
            panic!("expected an uncertain outcome, got {error:?}");
        };
        let Some(ToolError::MutationConflict { conflict, .. }) = source.downcast_ref::<ToolError>()
        else {
            panic!("expected a typed mutation conflict as the cause, got {source:?}");
        };
        (applied_paths, conflict)
    }

    fn canonical_slash(path: &Path) -> String {
        slash(&path.canonicalize().expect("path exists"))
    }

    const DELETE_AFTER_UPDATE: &str = concat!(
        "*** Begin Patch\n",
        "*** Update File: first.txt\n",
        "@@\n",
        "-one\n",
        "+ONE\n",
        "*** Delete File: doomed.txt\n",
        "*** End Patch"
    );

    #[tokio::test]
    async fn a_delete_is_refused_when_the_file_changed_after_the_patch_was_read() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let first = workspace.path().join("first.txt");
        let doomed = workspace.path().join("doomed.txt");
        std::fs::write(&first, "one\n").expect("first fixture");
        std::fs::write(&doomed, "planned\n").expect("delete fixture");
        let tool = patch_tool(
            workspace.path(),
            Arc::new(ConcurrentEditor {
                path: doomed.clone(),
                contents: Some("somebody else\n"),
            }),
            &[&first, &doomed],
        );

        let error = apply(&tool, DELETE_AFTER_UPDATE)
            .await
            .expect_err("a file that changed under the delete must not be removed");

        // The first file's bytes really are on disk, so the refusal is promoted to an
        // uncertain outcome exactly as a refused write would be.
        let (applied, conflict) = uncertain_conflict(&error);
        assert_eq!(applied, vec![canonical_slash(&first)]);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "doomed.txt");
        assert_eq!(
            conflict.observed_digest.as_deref(),
            Some(digest_bytes(b"somebody else\n").as_str()),
            "the conflict must report the bytes that are actually on disk"
        );
        assert_eq!(conflict.required_action(), "reread");
        assert_eq!(
            std::fs::read_to_string(&doomed).expect("the concurrent edit survives"),
            "somebody else\n",
            "the patch must not remove bytes it never read"
        );
        assert_eq!(
            std::fs::read_to_string(&first).expect("the first write landed"),
            "ONE\n"
        );
    }

    #[tokio::test]
    async fn a_move_is_refused_when_its_source_changed_after_the_patch_was_read() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let first = workspace.path().join("first.txt");
        let source = workspace.path().join("source.txt");
        let moved = workspace.path().join("moved.txt");
        std::fs::write(&first, "one\n").expect("first fixture");
        std::fs::write(&source, "old\n").expect("source fixture");
        let tool = patch_tool(
            workspace.path(),
            Arc::new(ConcurrentEditor {
                path: source.clone(),
                contents: Some("somebody else\n"),
            }),
            &[&first, &source],
        );

        let error = apply(
            &tool,
            concat!(
                "*** Begin Patch\n",
                "*** Update File: first.txt\n",
                "@@\n",
                "-one\n",
                "+ONE\n",
                "*** Update File: source.txt\n",
                "*** Move to: moved.txt\n",
                "@@\n",
                "-old\n",
                "+new\n",
                "*** End Patch"
            ),
        )
        .await
        .expect_err("a source that changed under the move must not be removed");

        let (applied, conflict) = uncertain_conflict(&error);
        assert_eq!(
            applied,
            vec![canonical_slash(&first)],
            "the move is refused before its destination is written"
        );
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "source.txt");
        assert_eq!(conflict.required_action(), "reread");
        assert_eq!(
            std::fs::read_to_string(&source).expect("the concurrent edit survives"),
            "somebody else\n"
        );
        assert!(
            !moved.exists(),
            "a refused move must not leave a destination computed from a stale source"
        );
    }

    #[tokio::test]
    async fn a_delete_is_refused_when_the_file_vanished_after_the_patch_was_read() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let first = workspace.path().join("first.txt");
        let doomed = workspace.path().join("doomed.txt");
        std::fs::write(&first, "one\n").expect("first fixture");
        std::fs::write(&doomed, "planned\n").expect("delete fixture");
        let tool = patch_tool(
            workspace.path(),
            Arc::new(ConcurrentEditor {
                path: doomed.clone(),
                contents: None,
            }),
            &[&first, &doomed],
        );

        let error = apply(&tool, DELETE_AFTER_UPDATE)
            .await
            .expect_err("a delete whose file is already gone is a conflict, not a success");

        // Somebody else removed the file. Claiming the deletion would attribute their
        // change to this patch and hide from the model that another writer is active.
        let (applied, conflict) = uncertain_conflict(&error);
        assert_eq!(applied, vec![canonical_slash(&first)]);
        assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
        assert_eq!(conflict.resource, "doomed.txt");
        assert_eq!(
            conflict.observed_digest, None,
            "there is nothing on disk to digest"
        );
        assert_eq!(conflict.required_action(), "reread");
        assert!(!doomed.exists());
    }

    #[tokio::test]
    async fn an_unchanged_delete_and_an_unchanged_move_still_succeed() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let source = workspace.path().join("source.txt");
        let moved = workspace.path().join("moved.txt");
        let doomed = workspace.path().join("doomed.txt");
        std::fs::write(&source, "old\n").expect("source fixture");
        std::fs::write(&doomed, "gone\n").expect("delete fixture");
        let tool = patch_tool(
            workspace.path(),
            Arc::new(NoopFormatter),
            &[&source, &doomed],
        );

        let output = apply(
            &tool,
            concat!(
                "*** Begin Patch\n",
                "*** Update File: source.txt\n",
                "*** Move to: moved.txt\n",
                "@@\n",
                "-old\n",
                "+new\n",
                "*** Delete File: doomed.txt\n",
                "*** End Patch"
            ),
        )
        .await
        .expect("sources that still hold their prepared images are removed");

        assert!(!source.exists(), "the move removes its source");
        assert_eq!(
            std::fs::read_to_string(&moved).expect("moved file"),
            "new\n"
        );
        assert!(!doomed.exists(), "the delete removes its file");
        assert!(output.output.contains("M moved.txt"), "{}", output.output);
        assert!(output.output.contains("D doomed.txt"), "{}", output.output);
    }
}
