use crate::read::{
    FileToolRuntime, PathKind, check_interrupt, decode_text, encode_text, failed, invalid,
    report_formatting, write_with_dirs,
};
use async_trait::async_trait;
use oc_error::ToolError;
use oc_tool::{ToolContext, ToolOutput, TypedTool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const DESCRIPTION: &str = concat!(
    "Writes a file to the local filesystem.\n\n",
    "Usage:\n",
    "- This tool will overwrite the existing file if there is one at the provided path.\n",
    "- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.\n",
    "- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n",
    "- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.\n",
    "- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked."
);

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

        let existing = match std::fs::read(&target.canonical) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
                return Err(invalid(
                    "write",
                    format!(
                        "Path is a directory, not a file: {}",
                        target.canonical.display()
                    ),
                ));
            }
            Err(error) => return Err(failed("write", error)),
        };
        if let Some(bytes) = existing.as_deref() {
            self.runtime
                .state
                .require_current_read(&ctx.session_id, &target.canonical, bytes)
                .map_err(|message| invalid("write", message))?;
        }

        let old_bom = existing
            .as_deref()
            .map(decode_text)
            .transpose()
            .map_err(|error| failed("write", error))?
            .is_some_and(|file| file.bom);
        let (content, new_bom) = split_bom(&params.content);
        let bytes = encode_text(content, old_bom || new_bom);
        write_with_dirs(&target.canonical, &bytes).map_err(|error| failed("write", error))?;
        check_interrupt("write", &ctx)?;
        // Past this point the write has landed, so nothing a formatter does may
        // turn into an `Err` from this tool.
        let outcome = self
            .runtime
            .formatter
            .format_reporting(&target.canonical)
            .await
            .map_err(|error| failed("write", error))?;
        let final_bytes =
            std::fs::read(&target.canonical).map_err(|error| failed("write", error))?;
        self.runtime
            .state
            .record_write(&ctx.session_id, &target.canonical, &final_bytes);

        Ok(report_formatting(
            ToolOutput::text(self.runtime.title(&target), "Wrote file successfully.")
                .with_metadata("filepath", json!(target.canonical))
                .with_metadata("exists", json!(existing.is_some()))
                .with_metadata("formatted", outcome.changed),
            &outcome.failures,
        ))
    }
}

fn split_bom(content: &str) -> (&str, bool) {
    content
        .strip_prefix('\u{feff}')
        .map_or((content, false), |content| (content, true))
}
