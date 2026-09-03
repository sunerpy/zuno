use crate::read::{
    FileToolRuntime, PathKind, check_interrupt, decode_text, encode_text, failed, invalid,
    publish_error, report_diff, report_formatting, report_post_write_warnings, uncertain,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/write.txt");

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteParams {
    /// The content to write to the file.
    pub content: String,
    /// The absolute path to the file to write (must be absolute, not relative).
    pub file_path: String,
}

pub struct WriteTool {
    runtime: Arc<FileToolRuntime>,
}

impl WriteTool {
    pub(crate) fn new(runtime: Arc<FileToolRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl TypedTool for WriteTool {
    type Params = WriteParams;

    fn id(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        check_interrupt("write", &ctx)?;
        let target = self
            .runtime
            .resolve(Path::new(&params.file_path), PathKind::File)
            .map_err(|error| failed("write", error))?;
        self.runtime
            .authorize("write", "edit", &target, &ctx)
            .await?;
        let _guard = self.runtime.mutation.lock().await;
        check_interrupt("write", &ctx)?;

        // Every filesystem step below runs through this anchor, which is pinned to the
        // directory the user authorized. An ancestor swapped for a symlink after the
        // permission prompt can no longer redirect the write.
        let anchored = self.runtime.anchor_file("write", &target, true)?;

        let existing = match anchored.read() {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
                return Err(invalid(
                    "write",
                    format!(
                        "Path is a directory, not a file: {}",
                        zuno_paths::wire_path(&target.canonical)
                    ),
                ));
            }
            Err(error) => return Err(failed("write", error)),
        };
        if let Some(bytes) = existing.as_deref() {
            self.runtime
                .state
                .require_current_read(&ctx.session_id, &target.canonical, bytes)
                .map_err(|conflict| invalid("write", conflict.message(&target.canonical)))?;
        }

        let old_bom = existing
            .as_deref()
            .map(decode_text)
            .transpose()
            .map_err(|error| failed("write", error))?
            .is_some_and(|file| file.bom);
        let (content, new_bom) = split_bom(&params.content);
        let bytes = encode_text(content, old_bom || new_bom);
        // Nothing has been published yet, so an interruption here loses no work.
        check_interrupt("write", &ctx)?;
        let applied = vec![target.canonical.clone()];
        if let Err(failure) = anchored.publish(&bytes) {
            return Err(publish_error("write", &applied, failure));
        }
        let mut warnings = Vec::new();
        if ctx.is_interrupted() {
            warnings.push(
                "The file was written before cancellation; formatting was skipped.".to_owned(),
            );
        }
        // Past this point the write has landed, so nothing a formatter does may
        // turn into an `Err` from this tool.
        let outcome = if ctx.is_interrupted() {
            Default::default()
        } else {
            match self
                .runtime
                .formatter
                .format_reporting(&target.canonical)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    warnings.push(format!(
                        "The file was written, but the formatter service failed: {error}"
                    ));
                    Default::default()
                }
            }
        };
        let final_bytes = anchored
            .read()
            .map_err(|error| uncertain("write", &applied, error))?;
        self.runtime
            .state
            .record_write(&ctx.session_id, &target.canonical, &final_bytes);

        let label = self.runtime.title(&target);
        Ok(report_post_write_warnings(
            report_formatting(
                report_diff(
                    ToolOutput::text(label.clone(), "Wrote file successfully.")
                        .with_metadata("filepath", zuno_paths::wire_path(&target.canonical))
                        .with_metadata("exists", json!(existing.is_some()))
                        .with_metadata("formatted", outcome.changed)
                        .with_written_path(&target.canonical),
                    &target.canonical,
                    &label,
                    existing.as_deref(),
                    &final_bytes,
                ),
                &outcome.failures,
            ),
            &warnings,
        ))
    }
}

fn split_bom(content: &str) -> (&str, bool) {
    content
        .strip_prefix('\u{feff}')
        .map_or((content, false), |content| (content, true))
}
