//! Built-in tool implementations: file, shell, search, web, and task tools.

pub mod apply_patch;
pub mod edit;
pub mod read;
pub mod write;

pub use read::{FileFormatter, NoopFormatter};

use apply_patch::ApplyPatchTool;
use edit::EditTool;
use oc_tool::{Tool, erase};
use read::{FileToolRuntime, ReadTool};
use std::io;
use std::path::Path;
use std::sync::Arc;
use write::WriteTool;

/// The four file tools backed by one workspace, read-state store, and formatter.
#[derive(Clone)]
pub struct FileTools {
    pub read: Arc<dyn Tool>,
    pub write: Arc<dyn Tool>,
    pub edit: Arc<dyn Tool>,
    pub apply_patch: Arc<dyn Tool>,
}

impl FileTools {
    /// Create file tools with the no-op formatter seam used before Todo 79 lands.
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Self::with_formatter(workspace, Arc::new(NoopFormatter))
    }

    /// Create file tools that call `formatter` after every successful file write.
    pub fn with_formatter(workspace: &Path, formatter: Arc<dyn FileFormatter>) -> io::Result<Self> {
        let runtime = Arc::new(FileToolRuntime::new(workspace, formatter)?);
        Ok(Self {
            read: erase(ReadTool::new(Arc::clone(&runtime))),
            write: erase(WriteTool::new(Arc::clone(&runtime))),
            edit: erase(EditTool::new(Arc::clone(&runtime))),
            apply_patch: erase(ApplyPatchTool::new(runtime)),
        })
    }

    /// Return the model-visible file tools under the oracle's GPT patch rule.
    pub fn exposed_for_model(&self, model_id: &str) -> Vec<Arc<dyn Tool>> {
        if uses_apply_patch(model_id) {
            vec![Arc::clone(&self.read), Arc::clone(&self.apply_patch)]
        } else {
            vec![
                Arc::clone(&self.read),
                Arc::clone(&self.edit),
                Arc::clone(&self.write),
            ]
        }
    }
}

/// Match `registry.ts:292-295`: GPT models except OSS and GPT-4 use apply_patch.
#[must_use]
pub fn uses_apply_patch(model_id: &str) -> bool {
    model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4")
}
