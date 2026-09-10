//! Report artifacts are host-managed output, independent of workspace edit authority.

use crate::read::{AnchoredFile, check_interrupt, digest_bytes, failed, invalid, publish_error};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use zuno_error::ToolError;
use zuno_paths::GeneratedDirectory;
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput, TypedTool};

pub const WIRE_ID: &str = "report_write";
pub const METADATA_KEY: &str = "reportArtifact";
pub const DESCRIPTION: &str = include_str!("description/report-write.txt");
const MAX_REPORT_BYTES: usize = 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportWriteParams {
    /// A plain report filename, such as audit.md or inventory.txt; never a path.
    pub name: String,
    /// The complete UTF-8 report, at most 1 MiB.
    pub content: String,
}

/// A receipt produced only after the host has written the exact report bytes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportArtifact {
    pub name: String,
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
    pub session_id: String,
}

/// A bounded writer whose only destination is the worktree's generated report directory.
#[derive(Debug, Clone)]
pub struct ReportWriteTool {
    worktree: PathBuf,
}

impl ReportWriteTool {
    #[must_use]
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }
}

#[async_trait]
impl TypedTool for ReportWriteTool {
    type Params = ReportWriteParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(
        &self,
        params: ReportWriteParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        validate_name(&params.name)?;
        if params.content.len() > MAX_REPORT_BYTES {
            return Err(invalid(WIRE_ID, "report content exceeds the 1 MiB limit"));
        }
        check_interrupt(WIRE_ID, &ctx)?;
        let digest = digest_bytes(params.content.as_bytes());
        ctx.ask(
            WIRE_ID,
            PermissionAsk {
                permission: WIRE_ID.to_owned(),
                patterns: vec![params.name.clone()],
                metadata: serde_json::Map::from_iter([
                    ("name".to_owned(), json!(params.name)),
                    ("bytes".to_owned(), json!(params.content.len())),
                    ("sha256".to_owned(), json!(digest)),
                ]),
                always: vec!["*".to_owned()],
                ..PermissionAsk::default()
            },
        )
        .await?;
        check_interrupt(WIRE_ID, &ctx)?;
        let worktree = self.worktree.clone();
        let artifact =
            tokio::task::spawn_blocking(move || publish(&worktree, params, &ctx, digest))
                .await
                .map_err(|error| failed(WIRE_ID, error))??;
        Ok(ToolOutput::text(
            format!("Report: {}", artifact.name),
            format!(
                "Report saved: {}\nBytes: {}\nSHA-256: {}\nUse this returned path in the final report.",
                artifact.path, artifact.bytes, artifact.sha256
            ),
        )
        .with_metadata(METADATA_KEY, json!(artifact)))
    }
}

fn publish(
    worktree: &Path,
    params: ReportWriteParams,
    ctx: &ToolContext,
    digest: String,
) -> Result<ReportArtifact, ToolError> {
    let worktree = worktree
        .canonicalize()
        .map_err(|error| failed(WIRE_ID, error))?;
    let directory = GeneratedDirectory::in_worktree(&worktree, &zuno_paths::generated::REPORTS);
    // Neither a model-supplied name nor an opaque session/call id becomes a path segment.
    let identity = serde_json::to_vec(&(&ctx.session_id, &ctx.message_id, &ctx.call_id))
        .expect("string tuple serializes");
    let identity = digest_bytes(&identity);
    let path = directory.path().join(format!("{identity}-{}", params.name));
    let target =
        AnchoredFile::open(&worktree, &path, true).map_err(|error| failed(WIRE_ID, error))?;
    let marker_path = directory.path().join(".gitignore");
    let marker = AnchoredFile::open(&worktree, &marker_path, false)
        .map_err(|error| failed(WIRE_ID, error))?;
    // Use the same anchored, no-symlink writer as native file tools. A substituted
    // .zuno or reports directory must never redirect a report into project sources.
    marker
        .publish(b"*\n")
        .map_err(|error| publish_error(WIRE_ID, &[marker_path], error))?;
    check_interrupt(WIRE_ID, ctx)?;
    target
        .create_new(params.content.as_bytes())
        .map_err(|error| publish_error(WIRE_ID, std::slice::from_ref(&path), error))?;
    Ok(ReportArtifact {
        name: params.name,
        path: path.to_string_lossy().into_owned(),
        bytes: params.content.len(),
        sha256: digest,
        session_id: ctx.session_id.clone(),
    })
}

fn validate_name(name: &str) -> Result<(), ToolError> {
    let invalid_name = || {
        invalid(
            WIRE_ID,
            "name must be a plain portable filename (for example audit.md), not a directory or path",
        )
    };
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return Err(invalid_name());
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (["COM", "LPT"].iter().any(|prefix| stem.starts_with(prefix))
        && matches!(stem.chars().nth(3), Some('1'..='9' | '¹' | '²' | '³'))
        && stem.chars().count() == 4)
    {
        return Err(invalid_name());
    }
    Ok(())
}
