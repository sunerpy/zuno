//! Built-in tool implementations: file, shell, search, web, and task tools.
//!
//! # Search
//!
//! [`glob`] matches paths and [`grep`] searches contents through the official
//! `rg` executable. Zuno does not maintain a second ripgrep-compatible walker or
//! download an executable during a tool call.
//!
//! # The web tools
//!
//! [`webfetch`] retrieves a URL; [`websearch`] queries a search backend. Both are
//! bounded in time, response size and redirect hops, poll the turn's interrupt while
//! a body streams, and treat everything they retrieve as data rather than
//! instruction. See [`webfetch::bounds`] for the values and where each came from.
//!
//! # Shell
//!
//! [`shell`] parses Bash and PowerShell syntax before execution so compound commands
//! are authorized as their individual command resources. Cancellation and the hard
//! ceiling terminate the entire spawned process group. Every command is registered
//! with [`zuno_pty::BackgroundExecutionService`] before spawn. Ordinary foreground
//! state is consumed and removed at completion; explicit background mode and a
//! foreground timeout retain that same execution durably. [`output_policy`] persists
//! oversized foreground output before requiring explicit context-cost acceptance.
//! [`risk`] adds a deterministic destructive-command tripwire before any foreground
//! or background spawn. This is **not a sandbox**: shell commands retain the user's
//! full filesystem, network, and credentials. A confinement layer is a named future
//! decision, not a guarantee implied by this tool. The gate fails closed to
//! reflection when a brace expansion cannot be resolved statically, and permanently
//! denies a relative destructive target after an unknown directory change. It
//! intentionally does not classify credential reads such as
//! `cat ~/.ssh/id_rsa` as destruction; see [`risk`]'s documented boundary.
//!
//! # The conditional tools
//!
//! Three tools are not always offered: [`invalid`], [`question`], and
//! [`plan_exit`]. Their conditions live in [`exposure`] as named predicates over one
//! flags struct, not as branches inside a registry builder, so each is callable and
//! tested at both polarities. Upstream keys `plan_exit` as `plan`, while it is the
//! **wire** id that [`zuno_tool::Tool::id`] returns.
//!
//! [`exposure`] answers only the first of two gates. The permission ruleset withholds
//! `plan_exit` again from every agent but `plan`; that layer is [`zuno_permission`]'s and
//! a caller that stops at [`exposure`] will over-offer the tool.
//!
//! # Registry
//!
//! [`registry`] preserves the built-in order while making the final set a projection
//! of the model, provider, runtime flags, Harness contributions, MCP tools, and merged
//! permission rules. Every source passes through the same visibility rules.
//!
//! # Session recall
//!
//! [`session_search`] provides FTS5 discovery, anchored scrolling, and recent
//! session browsing directly over SQLite, with no provider or LLM dependency.
//!
//! # Post-edit formatting
//!
//! [`format`] is the execution half of the `formatter` config, which [`zuno_catalog`]
//! resolves and this crate runs. Its central promise is that **a formatter failure
//! cannot cost an edit**: the write lands first, a formatter is offered the file
//! second, and a formatter that fails has its damage undone and its stderr reported
//! rather than raising an error for a write that succeeded. It formats in place
//! because every command in the built-in table does, it only runs a formatter that
//! claims the file's extension, and — unlike [`shell`] — it does not consult
//! [`risk`], because a formatter command is operator-authored and spawned as argv
//! with no shell, so there is no model-composed string for that gate to judge.
//!
//! # Delegation
//!
//! [`task`] hands a bounded unit of work to a child session. Its five refusals —
//! no target, two targets, a coordinator as target, the depth bound, and a
//! permission denial — each carry a message that names the fix, because a
//! delegation refusal is read by a model and `oh-my-opencode-slim` pays for the
//! absence of that property with a nine-pattern recovery hook. There is
//! deliberately **no** `load_skills` argument: skills are permission-gated per
//! agent, so nothing about them is a property of the call. Targets come from
//! [`zuno_agent::builtin::delegable`] and the child's model from
//! [`zuno_agent::model_policy`], with this call's own `model`/`effort` as a fourth,
//! highest rung on that ladder.
//!
//! # Resident memory
//!
//! [`memory`] is the one tool that writes [`zuno_memory`]'s capped stores. It takes a
//! whole batch so consolidating and adding can happen in one atomic call, reports
//! `current/limit` on every response, and withholds the entry list on success —
//! echoing it invites the model to keep "fixing" a store that is already correct. A
//! refused write is returned as a successful call carrying `success: false`, and
//! after three consolidation failures in one turn the next attempt is told to stop
//! and answer the user: a failed memory side effect must never cost the turn's
//! reply.
//!
//! # The patch a mutation reports
//!
//! Every tool that changes a file on disk — [`edit`], [`write`], [`apply_patch`] —
//! attaches the unified patch of what it changed as `metadata["diff"]`, built by
//! [`diff::unified_diff_bytes`]. The patch is metadata rather than output because
//! `output` is what the model is charged for and what the transcript prints inline; see
//! [`diff`] for the full argument and for what of the oracle's post-processing is
//! deliberately not ported. Without it the TUI's diff viewer has nothing to open on,
//! which is exactly the state this crate shipped in before: `edit` said
//! `"Edit applied successfully."` and the patch existed nowhere.

pub mod apply_patch;
pub mod batch;
pub mod bg;
pub mod diff;
pub mod edit;
pub mod format;
pub mod output_policy;
pub mod read;
pub mod risk;
pub mod shell;
pub mod timeout;
pub mod write;

pub use batch::{ExecuteParams, ExecuteTool, MAX_SUBCALLS, TOTAL_OUTPUT_BYTES};
pub use bg::{BackgroundAction, BackgroundParams, BackgroundTool, WIRE_ID as BG_WIRE_ID};
pub use format::{
    Availability, DEFINITIONS as FORMATTER_DEFINITIONS, Definition as FormatterDefinition,
    FailureKind, FormatFailure, FormatOutcome, Formatters, ProgramLocator, SystemPrograms,
};
pub use read::{FileFormatter, NoopFormatter};

use apply_patch::ApplyPatchTool;
use edit::EditTool;
use read::{FileToolRuntime, ReadTool};
use std::io;
use std::path::Path;
use std::sync::Arc;
use write::WriteTool;
use zuno_tool::{Tool, erase};

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

    /// Return Zuno's provider-neutral model-visible file surface.
    ///
    /// `apply_patch` owns structured edits and `write` is the explicit fallback for a
    /// full replacement after `read`. The exact-string `edit` implementation remains
    /// available to native profiles, but the default model surface does not advertise
    /// two competing patch languages.
    pub fn model_visible(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::clone(&self.read),
            Arc::clone(&self.write),
            Arc::clone(&self.apply_patch),
        ]
    }
}

pub mod glob;
pub mod grep;
pub mod search_common;
pub mod session_search;

pub use crate::glob::{GlobParams, GlobTool};
pub use crate::grep::{GrepParams, GrepTool};
pub use crate::search_common::{RESULT_LIMIT, SearchScope, SearchTooling};
pub use crate::session_search::{SessionSearchParams, SessionSearchTool};

pub mod webfetch;
pub mod websearch;

pub use crate::webfetch::WebFetchTool;
pub use crate::webfetch::bounds::WebError;
pub use crate::websearch::WebSearchTool;
pub use crate::websearch::gating::{
    Provider, SearchConfig, require_provider, select_provider, web_search_enabled,
};

pub mod exposure;
pub mod invalid;
pub mod job;
pub mod job_cancel;
pub mod plan_exit;
pub mod product_agent;
pub mod question;
pub mod registry;
pub mod work_state;
pub mod workflow;

pub use crate::exposure::{
    CONDITIONAL_TOOLS, Client, ExposureFlags, exposed_conditional_tools, exposes_invalid,
    exposes_plan_exit, exposes_question, exposure_predicate,
};
pub use crate::invalid::{InvalidParams, InvalidTool};
pub use crate::job::{DESCRIPTION as JOB_DESCRIPTION, JobParams, JobTool, WIRE_ID as JOB_WIRE_ID};
pub use crate::job_cancel::{
    CancelOutcome, JobCancelParams, JobCancelTool, JobController, WIRE_ID as JOB_CANCEL_WIRE_ID,
};
pub use crate::plan_exit::{PlanExitHost, PlanExitParams, PlanExitTool, RecordingHost};
pub use crate::product_agent::{
    ProductAgentHost, ProductAgentParams, ProductAgentRequest, ProductAgentTool, ProductAgentTurn,
};
pub use crate::question::{
    Answer, QuestionAsker, QuestionOption, QuestionParams, QuestionPrompt, QuestionRequest,
    QuestionTool, ScriptedAnswers,
};
pub use crate::work_state::{
    PLAN_GET_DESCRIPTION, PLAN_GET_TOOL_ID, PLAN_UPDATE_DESCRIPTION, PLAN_UPDATE_TOOL_ID,
    PlanGetTool, PlanStep, PlanUpdateParams, PlanUpdateTool, TODO_GET_DESCRIPTION,
    TODO_GET_TOOL_ID, TODO_UPDATE_DESCRIPTION, TODO_UPDATE_TOOL_ID, TodoGetTool, TodoUpdateParams,
    TodoUpdateTool, WorkItem, WorkItemChange, WorkItemPriority, WorkItemStatus, WorkPlan,
    WorkStateError, WorkStateGetParams, WorkStateSnapshot, WorkStateStore, work_state_tools,
};
pub use crate::workflow::{
    WIRE_ID as WORKFLOW_WIRE_ID, WorkflowHost, WorkflowNodeRequest, WorkflowParams,
    WorkflowRequest, WorkflowTool, WorkflowTurn,
};

pub mod memory;
pub mod skill;
pub mod task;

pub use crate::skill::{
    DEFAULT_SEARCH_LIMIT as SKILL_DEFAULT_SEARCH_LIMIT, DESCRIPTION as SKILL_DESCRIPTION,
    LoadedSkillDocument, MAX_SEARCH_LIMIT as SKILL_MAX_SEARCH_LIMIT,
    RESOURCE_MAX_BYTES as SKILL_RESOURCE_MAX_BYTES,
    SEARCH_DESCRIPTION_MAX_BYTES as SKILL_SEARCH_DESCRIPTION_MAX_BYTES,
    SUGGESTION_LIMIT as SKILL_SUGGESTION_LIMIT, SkillParams, SkillRejection, SkillTool,
    WIRE_ID as SKILL_WIRE_ID, load_skill_document,
};

pub use crate::task::{
    ChildTurn, ChildTurnError, ChildTurnHost, ChildTurnRequest, DEFAULT_SUBAGENT_DEPTH,
    DESCRIPTION as TASK_DESCRIPTION, DelegationLimits, DelegationPlan, FixedFacts,
    GENERIC_EXECUTOR, GUIDANCE_KEY, ModelFacts, NoProviders, PERMISSION_KEY as TASK_PERMISSION_KEY,
    ProviderFacts, RecordingHost as RecordingChildTurnHost, ReportDelivery, TaskParams,
    TaskRejection, TaskTool, WIRE_ID as TASK_WIRE_ID, denial_guidance, valid_targets,
};

pub use crate::memory::{
    DESCRIPTION as MEMORY_DESCRIPTION, MEMORY_TOOL_ID, MemoryAction, MemoryParams, MemoryTarget,
    MemoryTool,
};

#[cfg(test)]
mod replay_policy_tests {
    use super::*;
    use zuno_tool::ToolReplayPolicy;

    #[test]
    fn file_reads_opt_in_but_file_mutations_keep_the_safe_default() {
        let root = tempfile::tempdir().expect("temporary workspace");
        let tools = FileTools::new(root.path()).expect("file tools");

        assert_eq!(tools.read.replay_policy(), ToolReplayPolicy::Safe);
        assert_eq!(tools.write.replay_policy(), ToolReplayPolicy::Never);
        assert_eq!(tools.edit.replay_policy(), ToolReplayPolicy::Never);
        assert_eq!(tools.apply_patch.replay_policy(), ToolReplayPolicy::Never);
    }
}
