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
//!
//! # Search
//!
//! [`glob`] matches paths and [`grep`] searches contents, both over
//! [`oc_search`]'s embedded engine. Nothing here downloads a ripgrep binary, which
//! the oracle does on first search; a system `rg` is reachable only through
//! [`oc_search::Backend::from_env`] and is never selected implicitly.

pub mod glob;
pub mod grep;
pub mod search_common;

pub use crate::glob::{GlobParams, GlobTool};
pub use crate::grep::{GrepParams, GrepTool};
pub use crate::search_common::{RESULT_LIMIT, SearchScope, SearchTooling};
//!
//! # The web tools
//!
//! [`webfetch`] retrieves a URL; [`websearch`] queries a search backend. Both are
//! bounded in time, response size and redirect hops, poll the turn's interrupt while
//! a body streams, and treat everything they retrieve as data rather than
//! instruction. See [`webfetch::bounds`] for the values and where each came from.

pub mod webfetch;
pub mod websearch;

pub use crate::webfetch::WebFetchTool;
pub use crate::webfetch::bounds::WebError;
pub use crate::websearch::WebSearchTool;
pub use crate::websearch::gating::{Provider, SearchConfig, select_provider, web_search_enabled};
