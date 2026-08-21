mod parser;

use crate::format::FormatFailure;
use crate::read::{
    FileToolRuntime, PathKind, ResolvedPath, check_interrupt, decode_text, encode_text, failed,
    interrupted, invalid, report_formatting, slash, write_with_dirs,
};
use async_trait::async_trait;
use parser::{ChunkLine, PatchOperation, PatchParseError, UpdateChunk, parse_patch};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/apply-patch.txt");

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
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
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
        for operation in &operations {
            check_interrupt("apply_patch", &ctx)?;
            let source = self
                .runtime
                .resolve(Path::new(operation.path()), PathKind::File)
                .map_err(|error| failed("apply_patch", error))?;
            authorize_once(&self.runtime, &source, &ctx, &mut authorized).await?;
            if let PatchOperation::Update {
                move_to: Some(move_to),
                ..
            } = operation
            {
                let destination = self
                    .runtime
                    .resolve(Path::new(move_to), PathKind::File)
                    .map_err(|error| failed("apply_patch", error))?;
                authorize_once(&self.runtime, &destination, &ctx, &mut authorized).await?;
            }
        }

        let _guard = self.runtime.mutation.lock().await;
        let changes = self.prepare_changes(&operations, &ctx)?;
        let mut summaries = Vec::new();
        let mut files = Vec::<Value>::new();
        // Only the paths that still exist afterwards: a deleted file has no diagnostics,
        // and asking a language server about one reports somebody else's absent file.
        let mut written = Vec::<PathBuf>::new();
        let mut formatted_files = Vec::<Value>::new();
        let mut failures = Vec::<FormatFailure>::new();
        // One patch covering every file this call touched, in application order. A
        // unified diff concatenates by construction — each file re-labels itself with its
        // own `---`/`+++` pair — so the viewer scrolls through the whole change rather
        // than showing whichever file happened to be last.
        let mut patches = Vec::<String>::new();
        for change in changes {
            check_interrupt("apply_patch", &ctx)?;
            let source_path = change.source.canonical.clone();
            let target = change.destination.as_ref().unwrap_or(&change.source);
            let target_path = target.canonical.clone();
            // Every branch that writes has already written by the time it formats,
            // so a formatter's failure is collected rather than propagated: one
            // uncooperative formatter must not abandon a patch mid-way.
            let formatted = match change.kind {
                ChangeKind::Add | ChangeKind::Update => {
                    write_with_dirs(&target_path, &change.new_bytes)
                        .map_err(|error| failed("apply_patch", error))?;
                    self.format(&target_path, &mut failures).await?
                }
                ChangeKind::Move => {
                    write_with_dirs(&target_path, &change.new_bytes)
                        .map_err(|error| failed("apply_patch", error))?;
                    std::fs::remove_file(&source_path)
                        .map_err(|error| failed("apply_patch", error))?;
                    self.runtime.state.forget(&source_path);
                    self.format(&target_path, &mut failures).await?
                }
                ChangeKind::Delete => {
                    std::fs::remove_file(&source_path)
                        .map_err(|error| failed("apply_patch", error))?;
                    self.runtime.state.forget(&source_path);
                    false
                }
            };
            if change.kind != ChangeKind::Delete {
                let final_bytes =
                    std::fs::read(&target_path).map_err(|error| failed("apply_patch", error))?;
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
                std::fs::read(&target_path).map_err(|error| failed("apply_patch", error))?
            };
            if let Some(patch) =
                crate::diff::unified_diff_bytes(&relative, &change.old_bytes, &post)
            {
                patches.push(patch);
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
        Ok(report_formatting(result, &failures))
    }
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
        operations: &[PatchOperation],
        ctx: &ToolContext,
    ) -> Result<Vec<FileChange>, ToolError> {
        let mut changes = Vec::new();
        let mut touched = HashSet::<PathBuf>::new();
        for operation in operations {
            check_interrupt("apply_patch", ctx)?;
            let source = self
                .runtime
                .resolve(Path::new(operation.path()), PathKind::File)
                .map_err(|error| failed("apply_patch", error))?;
            if !touched.insert(source.canonical.clone()) {
                return Err(invalid(
                    "apply_patch",
                    format!(
                        "apply_patch verification failed: file appears in more than one operation: {}",
                        source.canonical.display()
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
                                source.canonical.display()
                            ),
                        ));
                    }
                    changes.push(FileChange {
                        source,
                        destination: None,
                        kind: ChangeKind::Add,
                        old_bytes: Vec::new(),
                        new_bytes: content.as_bytes().to_vec(),
                    });
                }
                PatchOperation::Delete { .. } => {
                    ensure_regular_file(&source.canonical)?;
                    let old_bytes = std::fs::read(&source.canonical)
                        .map_err(|error| failed("apply_patch", error))?;
                    changes.push(FileChange {
                        source,
                        destination: None,
                        kind: ChangeKind::Delete,
                        old_bytes,
                        new_bytes: Vec::new(),
                    });
                }
                PatchOperation::Update {
                    move_to, chunks, ..
                } => {
                    ensure_regular_file(&source.canonical)?;
                    let bytes = std::fs::read(&source.canonical)
                        .map_err(|error| failed("apply_patch", error))?;
                    let decoded =
                        decode_text(&bytes).map_err(|error| failed("apply_patch", error))?;
                    let content = apply_chunks(&source.canonical, &decoded.text, chunks, ctx)?;
                    let destination = move_to
                        .as_ref()
                        .map(|path| {
                            self.runtime
                                .resolve(Path::new(path), PathKind::File)
                                .map_err(|error| failed("apply_patch", error))
                        })
                        .transpose()?;
                    if let Some(target) = &destination
                        && !touched.insert(target.canonical.clone())
                    {
                        return Err(invalid(
                            "apply_patch",
                            format!(
                                "apply_patch verification failed: destination appears in more than one operation: {}",
                                target.canonical.display()
                            ),
                        ));
                    }
                    let kind = if destination.is_some() {
                        ChangeKind::Move
                    } else {
                        ChangeKind::Update
                    };
                    changes.push(FileChange {
                        source,
                        destination,
                        kind,
                        old_bytes: bytes,
                        new_bytes: encode_text(&content, decoded.bom),
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

fn apply_chunks(
    path: &Path,
    source: &str,
    chunks: &[UpdateChunk],
    ctx: &ToolContext,
) -> Result<String, ToolError> {
    let mut content = source.to_owned();
    let mut cursor = 0usize;
    for chunk in chunks {
        check_interrupt("apply_patch", ctx)?;
        let search_start = if let Some(header) = &chunk.header {
            find_header_end(&content, header, cursor).ok_or_else(|| {
                invalid(
                    "apply_patch",
                    format!(
                        "apply_patch verification failed: hunk header `{header}` was not found in {}",
                        path.display()
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
            return Err(invalid(
                "apply_patch",
                format!(
                    "apply_patch verification failed: hunk context was not found in {}",
                    path.display()
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
