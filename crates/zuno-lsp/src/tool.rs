//! The single composite `lsp` tool exposed to models.

use crate::{Manager, ManagerError, Position};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{
    PermissionAsk, Tool, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool, erase,
};

const DESCRIPTION: &str = "Interact with language servers for definitions, references, hover information, symbols, implementations, and call hierarchies.";

/// An operation supported by the composite tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl LspOperation {
    fn wire_name(self) -> &'static str {
        match self {
            Self::GoToDefinition => "goToDefinition",
            Self::FindReferences => "findReferences",
            Self::Hover => "hover",
            Self::DocumentSymbol => "documentSymbol",
            Self::WorkspaceSymbol => "workspaceSymbol",
            Self::GoToImplementation => "goToImplementation",
            Self::PrepareCallHierarchy => "prepareCallHierarchy",
            Self::IncomingCalls => "incomingCalls",
            Self::OutgoingCalls => "outgoingCalls",
        }
    }
}

/// Parameters shared by every LSP operation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspParams {
    /// The LSP operation to perform.
    pub operation: LspOperation,
    /// Absolute path, or a path relative to the tool's workspace directory.
    pub file_path: String,
    /// One-based line number as displayed by editors.
    pub line: u32,
    /// One-based character offset as displayed by editors.
    pub character: u32,
    /// Query used by `workspaceSymbol`; an omitted query requests all symbols.
    #[serde(default)]
    pub query: Option<String>,
}

/// Composite model-facing LSP tool backed by one workspace manager.
#[derive(Debug, Clone)]
pub struct LspTool {
    manager: Arc<Manager>,
    directory: PathBuf,
    worktree: PathBuf,
}

impl LspTool {
    /// Build a tool whose relative paths resolve against `directory`.
    #[must_use]
    pub fn new(
        manager: Arc<Manager>,
        directory: impl Into<PathBuf>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        Self {
            manager,
            directory: directory.into(),
            worktree: worktree.into(),
        }
    }

    /// Erase the typed tool for registration in [`zuno_tool::Tool`].
    #[must_use]
    pub fn erased(self) -> Arc<dyn Tool> {
        erase(self)
    }

    fn resolve_path(&self, value: &str) -> PathBuf {
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.directory.join(path)
        }
    }

    fn title(&self, operation: LspOperation, file: &Path, line: u32, character: u32) -> String {
        if operation == LspOperation::WorkspaceSymbol {
            return operation.wire_name().to_owned();
        }
        let relative = file.strip_prefix(&self.worktree).unwrap_or(file).display();
        if operation == LspOperation::DocumentSymbol {
            format!("{} {relative}", operation.wire_name())
        } else {
            format!("{} {relative}:{line}:{character}", operation.wire_name())
        }
    }
}

#[async_trait]
impl TypedTool for LspTool {
    type Params = LspParams;

    fn id(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(&self, params: LspParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if params.line == 0 || params.character == 0 {
            return Err(ToolError::InvalidArgs {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "line and character must be one-based positive integers",
                )),
            });
        }

        let file = self.resolve_path(&params.file_path);
        let mut metadata = Map::new();
        metadata.insert(
            "operation".to_owned(),
            Value::String(params.operation.wire_name().to_owned()),
        );
        if params.operation != LspOperation::WorkspaceSymbol {
            metadata.insert(
                "filePath".to_owned(),
                Value::String(file.to_string_lossy().into_owned()),
            );
        }
        if !matches!(
            params.operation,
            LspOperation::WorkspaceSymbol | LspOperation::DocumentSymbol
        ) {
            metadata.insert("line".to_owned(), Value::from(params.line));
            metadata.insert("character".to_owned(), Value::from(params.character));
        }
        ctx.ask(
            self.id(),
            PermissionAsk {
                permission: "lsp".to_owned(),
                patterns: vec!["*".to_owned()],
                metadata,
                always: vec!["*".to_owned()],
                ..PermissionAsk::default()
            },
        )
        .await?;

        if !tokio::fs::try_exists(&file)
            .await
            .map_err(|source| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(source),
            })?
        {
            return Err(ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("file not found: {}", file.display()),
                )),
            });
        }
        if !self.manager.has_server(&file) {
            return Err(ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no LSP server available for this file type",
                )),
            });
        }
        self.manager.touch_file(&file).await.map_err(tool_failure)?;

        let position = Position {
            line: params.line - 1,
            character: params.character - 1,
        };
        let result = match params.operation {
            LspOperation::GoToDefinition => {
                self.manager
                    .position_request(&file, position, "textDocument/definition", json!({}))
                    .await
            }
            LspOperation::FindReferences => {
                self.manager
                    .position_request(
                        &file,
                        position,
                        "textDocument/references",
                        json!({ "context": { "includeDeclaration": true } }),
                    )
                    .await
            }
            LspOperation::Hover => {
                self.manager
                    .position_request(&file, position, "textDocument/hover", json!({}))
                    .await
            }
            LspOperation::DocumentSymbol => self.manager.document_symbols(&file).await,
            LspOperation::WorkspaceSymbol => {
                self.manager
                    .workspace_symbols(params.query.as_deref().unwrap_or_default())
                    .await
            }
            LspOperation::GoToImplementation => {
                self.manager
                    .position_request(&file, position, "textDocument/implementation", json!({}))
                    .await
            }
            LspOperation::PrepareCallHierarchy => {
                self.manager
                    .position_request(
                        &file,
                        position,
                        "textDocument/prepareCallHierarchy",
                        json!({}),
                    )
                    .await
            }
            LspOperation::IncomingCalls => {
                self.manager
                    .call_hierarchy(&file, position, "callHierarchy/incomingCalls")
                    .await
            }
            LspOperation::OutgoingCalls => {
                self.manager
                    .call_hierarchy(&file, position, "callHierarchy/outgoingCalls")
                    .await
            }
        }
        .map_err(tool_failure)?;

        let output = if result.is_empty() {
            format!("No results found for {}", params.operation.wire_name())
        } else {
            serde_json::to_string_pretty(&result).map_err(|source| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(source),
            })?
        };
        Ok(ToolOutput::text(
            self.title(params.operation, &file, params.line, params.character),
            output,
        )
        .with_metadata("result", Value::Array(result)))
    }
}

fn tool_failure(source: ManagerError) -> ToolError {
    ToolError::Failed {
        tool: "lsp".to_owned(),
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_tool::INTENT_KEY;

    #[test]
    fn composite_schema_uses_upstream_operation_and_field_names() {
        let config = zuno_config::schema::lsp::LspConfig::Enabled(false);
        let registry = Arc::new(crate::ServerRegistry::offline(
            &zuno_catalog::lsp_config::ResolvedLsp::resolve(Some(&config)),
        ));
        let manager = Arc::new(Manager::new(
            "/tmp",
            registry,
            crate::RestartPolicy::default(),
            std::num::NonZeroUsize::new(4).expect("non-zero"),
        ));
        let tool = LspTool::new(manager, "/tmp", "/tmp").erased();
        let definition = tool.definition();
        assert_eq!(definition.id, "lsp");
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Safe);
        assert_eq!(
            definition.parameters["properties"]["filePath"]["type"],
            "string"
        );
        let operations = definition.parameters["properties"]["operation"]["enum"]
            .as_array()
            .expect("operation enum");
        assert!(operations.contains(&json!("goToDefinition")));
        assert!(operations.contains(&json!("outgoingCalls")));
        assert_eq!(
            definition.parameters["properties"][INTENT_KEY]["type"],
            "string"
        );
        assert!(
            !definition.parameters["required"]
                .as_array()
                .expect("required fields")
                .contains(&json!(INTENT_KEY)),
            "intent is optional metadata for LSP just as it is for every tool"
        );
    }
}
