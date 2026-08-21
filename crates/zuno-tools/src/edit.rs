use crate::read::{
    FileToolRuntime, PathKind, check_interrupt, decode_text, encode_text, failed, invalid,
    report_diff, report_formatting, write_with_dirs,
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
pub struct EditParams {
    /// The absolute path to the file to modify.
    pub file_path: String,
    /// The text to replace.
    pub old_string: String,
    /// The text to replace it with (must be different from oldString).
    pub new_string: String,
    /// Replace all occurrences of oldString (default false).
    #[serde(default)]
    pub replace_all: bool,
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
        validate_params(&params)?;
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
        let old_string = convert_line_endings(&params.old_string, ending);
        let new_string = convert_line_endings(&params.new_string, ending);
        let replacements = count_occurrences(&decoded.text, &old_string, &ctx)?;
        if replacements == 0 {
            return Err(invalid(
                "edit",
                "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.",
            ));
        }
        if replacements > 1 && !params.replace_all {
            return Err(invalid(
                "edit",
                "Found multiple matches for oldString; provide more context or use replaceAll.",
            ));
        }
        let next = if params.replace_all {
            decoded.text.replace(&old_string, &new_string)
        } else {
            decoded.text.replacen(&old_string, &new_string, 1)
        };
        let bytes = encode_text(&next, decoded.bom);
        write_with_dirs(&target.canonical, &bytes).map_err(|error| failed("edit", error))?;
        check_interrupt("edit", &ctx)?;
        // Past this point the edit has landed, so nothing a formatter does may
        // turn into an `Err` from this tool.
        let outcome = self
            .runtime
            .formatter
            .format_reporting(&target.canonical)
            .await
            .map_err(|error| failed("edit", error))?;
        let final_bytes =
            std::fs::read(&target.canonical).map_err(|error| failed("edit", error))?;
        self.runtime
            .state
            .record_write(&ctx.session_id, &target.canonical, &final_bytes);

        let label = self.runtime.title(&target);
        Ok(report_formatting(
            report_diff(
                ToolOutput::text(label.clone(), "Edit applied successfully.")
                    .with_metadata("filepath", target.canonical.to_string_lossy().into_owned())
                    .with_metadata("replacements", replacements)
                    .with_metadata("formatted", outcome.changed)
                    .with_written_path(&target.canonical),
                &label,
                &source,
                &final_bytes,
            ),
            &outcome.failures,
        ))
    }
}

fn validate_params(params: &EditParams) -> Result<(), ToolError> {
    if params.old_string == params.new_string {
        return Err(invalid(
            "edit",
            "No changes to apply: oldString and newString are identical.",
        ));
    }
    if params.old_string.is_empty() {
        return Err(invalid(
            "edit",
            "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.",
        ));
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
