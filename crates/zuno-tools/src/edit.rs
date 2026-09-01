use crate::read::{
    FileToolRuntime, PathKind, check_interrupt, decode_text, encode_text, failed, invalid,
    report_diff, report_formatting, report_post_write_warnings, uncertain, write_with_dirs,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/edit.txt");

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditOperation {
    /// The text to replace.
    pub old_string: String,
    /// The text to replace it with (must be different from oldString).
    pub new_string: String,
    /// Replace all occurrences of oldString (default false).
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditParams {
    /// The absolute path to the file to modify.
    pub file_path: String,
    /// Ordered edits applied atomically to one in-memory file image.
    pub edits: Vec<EditOperation>,
}

pub struct EditTool {
    runtime: Arc<FileToolRuntime>,
}

impl EditTool {
    pub(crate) fn new(runtime: Arc<FileToolRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl TypedTool for EditTool {
    type Params = EditParams;

    fn id(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        validate_params(&params.edits)?;
        check_interrupt("edit", &ctx)?;
        let target = self
            .runtime
            .resolve(Path::new(&params.file_path), PathKind::File)
            .map_err(|error| failed("edit", error))?;
        self.runtime
            .authorize("edit", "edit", &target, &ctx)
            .await?;
        let _guard = self.runtime.mutation.lock().await;
        check_interrupt("edit", &ctx)?;

        let source = std::fs::read(&target.canonical).map_err(|error| failed("edit", error))?;
        self.runtime
            .state
            .require_current_read(&ctx.session_id, &target.canonical, &source)
            .map_err(|message| invalid("edit", message))?;
        let decoded = decode_text(&source).map_err(|error| failed("edit", error))?;
        let ending = if decoded.text.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut next = decoded.text.clone();
        let mut replacements = 0usize;
        for (index, edit) in params.edits.iter().enumerate() {
            check_interrupt("edit", &ctx)?;
            let old_string = convert_line_endings(&edit.old_string, ending);
            let new_string = convert_line_endings(&edit.new_string, ending);
            let matches = count_occurrences(&next, &old_string, &ctx)?;
            if matches == 0 {
                return Err(invalid(
                    "edit",
                    format!(
                        "edits[{index}].oldString was not found. It must match the current file image exactly, including whitespace, indentation, and line endings."
                    ),
                ));
            }
            if matches > 1 && !edit.replace_all {
                return Err(invalid(
                    "edit",
                    format!(
                        "edits[{index}].oldString matched {matches} locations; provide more context or set replaceAll."
                    ),
                ));
            }
            next = if edit.replace_all {
                next.replace(&old_string, &new_string)
            } else {
                next.replacen(&old_string, &new_string, 1)
            };
            replacements = replacements.saturating_add(if edit.replace_all { matches } else { 1 });
        }
        let bytes = encode_text(&next, decoded.bom);
        write_with_dirs(&target.canonical, &bytes).map_err(|error| failed("edit", error))?;
        let applied = vec![target.canonical.clone()];
        let mut warnings = Vec::new();
        if ctx.is_interrupted() {
            warnings.push(
                "The edit was written before cancellation; formatting was skipped.".to_owned(),
            );
        }
        // Past this point the edit has landed, so nothing a formatter does may
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
                        "The edit was written, but the formatter service failed: {error}"
                    ));
                    Default::default()
                }
            }
        };
        let final_bytes =
            std::fs::read(&target.canonical).map_err(|error| uncertain("edit", &applied, error))?;
        self.runtime
            .state
            .record_write(&ctx.session_id, &target.canonical, &final_bytes);

        let label = self.runtime.title(&target);
        Ok(report_post_write_warnings(
            report_formatting(
                report_diff(
                    ToolOutput::text(label.clone(), "Edit applied successfully.")
                        .with_metadata("filepath", zuno_paths::wire_path(&target.canonical))
                        .with_metadata("replacements", replacements)
                        .with_metadata("formatted", outcome.changed)
                        .with_written_path(&target.canonical),
                    &target.canonical,
                    &label,
                    Some(&source),
                    &final_bytes,
                ),
                &outcome.failures,
            ),
            &warnings,
        ))
    }
}

fn validate_params(edits: &[EditOperation]) -> Result<(), ToolError> {
    if edits.is_empty() {
        return Err(invalid(
            "edit",
            "edits must contain at least one operation.",
        ));
    }
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_string == edit.new_string {
            return Err(invalid(
                "edit",
                format!("edits[{index}] makes no change: oldString and newString are identical."),
            ));
        }
        if edit.old_string.is_empty() {
            return Err(invalid(
                "edit",
                format!(
                    "edits[{index}].oldString cannot be empty. Provide exact text, or use write for an intentional full-file replacement."
                ),
            ));
        }
    }
    Ok(())
}

fn convert_line_endings(text: &str, ending: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    if ending == "\r\n" {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

fn count_occurrences(content: &str, search: &str, ctx: &ToolContext) -> Result<usize, ToolError> {
    let mut count = 0usize;
    let mut offset = 0usize;
    while let Some(index) = content[offset..].find(search) {
        check_interrupt("edit", ctx)?;
        count += 1;
        offset += index + search.len();
    }
    Ok(count)
}
