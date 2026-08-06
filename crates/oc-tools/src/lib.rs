//! Built-in tool implementations: file, shell, search, web, and task tools.
//!
//! # Search
//!
//! [`glob`] matches paths and [`grep`] searches contents, both over
//! [`oc_search`]'s embedded engine. Nothing here downloads a ripgrep binary, which
//! the oracle does on first search; a system `rg` is reachable only through
//! [`oc_search::Backend::from_env`] and is never selected implicitly.
//!
//! # The web tools
//!
//! [`webfetch`] retrieves a URL; [`websearch`] queries a search backend. Both are
//! bounded in time, response size and redirect hops, poll the turn's interrupt while
//! a body streams, and treat everything they retrieve as data rather than
//! instruction. See [`webfetch::bounds`] for the values and where each came from.
//!
//! # The conditional tools
//!
//! Four tools are not always offered: [`invalid`], [`question`], [`todo`]'s
//! `todowrite`, and [`plan_exit`]. Their conditions live in [`exposure`] as named
//! predicates over one flags struct, not as branches inside a registry builder, so
//! each is callable and tested at both polarities. Two of the four have a registry key
//! that differs from the wire id — upstream keys `todowrite` as `todo` and `plan_exit`
//! as `plan` — and it is the **wire** id that [`oc_tool::Tool::id`] returns.
//!
//! [`exposure`] answers only the first of two gates. The permission ruleset withholds
//! `plan_exit` again from every agent but `plan`; that layer is [`oc_permission`]'s and
//! a caller that stops at [`exposure`] will over-offer the tool.

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

pub mod glob;
pub mod grep;
pub mod search_common;

pub use crate::glob::{GlobParams, GlobTool};
pub use crate::grep::{GrepParams, GrepTool};
pub use crate::search_common::{RESULT_LIMIT, SearchScope, SearchTooling};

pub mod webfetch;
pub mod websearch;

pub use crate::webfetch::WebFetchTool;
pub use crate::webfetch::bounds::WebError;
pub use crate::websearch::WebSearchTool;
pub use crate::websearch::gating::{Provider, SearchConfig, select_provider, web_search_enabled};

pub mod exposure;
pub mod invalid;
pub mod plan_exit;
pub mod question;
pub mod todo;

pub use crate::exposure::{
    CONDITIONAL_TOOLS, Client, ExposureFlags, exposed_conditional_tools, exposes_invalid,
    exposes_plan_exit, exposes_question, exposes_todowrite, exposure_predicate,
};
pub use crate::invalid::{InvalidParams, InvalidTool};
pub use crate::plan_exit::{PlanExitHost, PlanExitParams, PlanExitTool, RecordingHost};
pub use crate::question::{
    Answer, QuestionAsker, QuestionOption, QuestionParams, QuestionPrompt, QuestionRequest,
    QuestionTool, ScriptedAnswers,
};
pub use crate::todo::{
    MemoryTodoStore, SqliteTodoStore, TodoItem, TodoPriority, TodoStatus, TodoStore,
    TodoStoreError, TodoWriteParams, TodoWriteTool,
};
