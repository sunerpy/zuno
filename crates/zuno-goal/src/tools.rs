//! Model-facing tools for reading, creating, and finishing persisted goals.

use crate::{Goal, GoalError, GoalStore, ModelStatus};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{Tool, ToolContext, ToolOutput, TypedTool, erase};

/// Wire name of the goal reader.
pub const GET_GOAL_TOOL_ID: &str = "get_goal";
/// Wire name of the model-authorized goal creator.
pub const CREATE_GOAL_TOOL_ID: &str = "create_goal";
/// Wire name of the model-authorized status updater.
pub const UPDATE_GOAL_TOOL_ID: &str = "update_goal";

const GET_DESCRIPTION: &str =
    "Get the current goal for this session, including status, budget, usage, and remaining tokens.";
const CREATE_DESCRIPTION: &str = "Create a goal only when explicitly requested. An unfinished goal cannot be replaced. Set token_budget only when the user explicitly requested one.";
const UPDATE_DESCRIPTION: &str = "Mark the current goal complete, or report a blocking condition for the current turn. Complete requires evidence for every requirement. Blocked requires blocking_condition and becomes terminal only after the same condition persists for three consecutive goal turns.";

/// No-argument payload for [`GetGoalTool`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetGoalParams {}

/// Payload for [`CreateGoalTool`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateGoalParams {
    /// Concrete objective the goal should pursue.
    pub objective: String,
    /// Positive token ceiling, only when explicitly requested.
    #[serde(default)]
    pub token_budget: Option<i64>,
}

/// Statuses the model-facing update tool accepts.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateGoalStatus {
    /// The objective and every requirement are proven complete.
    Complete,
    /// The same true impasse persisted for three consecutive goal turns.
    Blocked,
}

impl From<UpdateGoalStatus> for ModelStatus {
    fn from(status: UpdateGoalStatus) -> Self {
        match status {
            UpdateGoalStatus::Complete => Self::Complete,
            UpdateGoalStatus::Blocked => Self::Blocked,
        }
    }
}

/// Payload for [`UpdateGoalTool`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateGoalParams {
    /// Terminal status justified by the completion or blocked audit.
    pub status: UpdateGoalStatus,
    /// Stable description of the impasse. Required only with `status: blocked`.
    #[serde(default)]
    pub blocking_condition: Option<String>,
}

/// Reads the current session goal.
#[derive(Debug, Clone)]
pub struct GetGoalTool {
    store: Arc<GoalStore>,
}

impl GetGoalTool {
    /// Bind the tool to a shared goal store.
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TypedTool for GetGoalTool {
    type Params = GetGoalParams;

    fn id(&self) -> &str {
        GET_GOAL_TOOL_ID
    }

    fn description(&self) -> &str {
        GET_DESCRIPTION
    }

    async fn run(&self, _params: GetGoalParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let goal = tokio::task::spawn_blocking(move || store.goal(&session_id))
            .await
            .map_err(|error| failed(GET_GOAL_TOOL_ID, error))?
            .map_err(|error| map_goal_error(GET_GOAL_TOOL_ID, error))?;
        goal_output(GET_GOAL_TOOL_ID, goal)
    }
}

/// Creates the current session goal through the guarded model write path.
#[derive(Debug, Clone)]
pub struct CreateGoalTool {
    store: Arc<GoalStore>,
}

impl CreateGoalTool {
    /// Bind the tool to a shared goal store.
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TypedTool for CreateGoalTool {
    type Params = CreateGoalParams;

    fn id(&self) -> &str {
        CREATE_GOAL_TOOL_ID
    }

    fn description(&self) -> &str {
        CREATE_DESCRIPTION
    }

    async fn run(
        &self,
        params: CreateGoalParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        if params.token_budget.is_some_and(|budget| budget <= 0) {
            return Err(invalid(
                CREATE_GOAL_TOOL_ID,
                "token_budget must be a positive integer",
            ));
        }
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let objective = params.objective;
        let token_budget = params.token_budget;
        let goal = tokio::task::spawn_blocking(move || {
            store.create_goal(&session_id, &objective, token_budget)
        })
        .await
        .map_err(|error| failed(CREATE_GOAL_TOOL_ID, error))?
        .map_err(|error| map_goal_error(CREATE_GOAL_TOOL_ID, error))?;
        goal_output(CREATE_GOAL_TOOL_ID, Some(goal))
    }
}

/// Updates the current goal through [`GoalStore::update_status_as_model`].
#[derive(Debug, Clone)]
pub struct UpdateGoalTool {
    store: Arc<GoalStore>,
}

impl UpdateGoalTool {
    /// Bind the tool to a shared goal store.
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TypedTool for UpdateGoalTool {
    type Params = UpdateGoalParams;

    fn id(&self) -> &str {
        UPDATE_GOAL_TOOL_ID
    }

    fn description(&self) -> &str {
        UPDATE_DESCRIPTION
    }

    async fn run(
        &self,
        params: UpdateGoalParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let status = ModelStatus::from(params.status);
        if matches!(status, ModelStatus::Blocked) {
            let condition = params
                .blocking_condition
                .as_deref()
                .map(str::trim)
                .filter(|condition| !condition.is_empty())
                .ok_or_else(|| {
                    invalid(
                        UPDATE_GOAL_TOOL_ID,
                        "blocking_condition is required when status is blocked",
                    )
                })?;
            let staged_store = Arc::clone(&store);
            let staged_session_id = session_id.clone();
            let condition = condition.to_owned();
            let staged = tokio::task::spawn_blocking(move || {
                staged_store.stage_failure_signal(&staged_session_id, &condition)
            })
            .await
            .map_err(|error| failed(UPDATE_GOAL_TOOL_ID, error))?
            .map_err(|error| map_goal_error(UPDATE_GOAL_TOOL_ID, error))?;
            if !staged {
                return Err(invalid(
                    UPDATE_GOAL_TOOL_ID,
                    "cannot report a blocker because this session has no active goal",
                ));
            }
            let goal = tokio::task::spawn_blocking(move || store.goal(&session_id))
                .await
                .map_err(|error| failed(UPDATE_GOAL_TOOL_ID, error))?
                .map_err(|error| map_goal_error(UPDATE_GOAL_TOOL_ID, error))?;
            return goal_output(UPDATE_GOAL_TOOL_ID, goal);
        }
        if params.blocking_condition.is_some() {
            return Err(invalid(
                UPDATE_GOAL_TOOL_ID,
                "blocking_condition is only valid when status is blocked",
            ));
        }
        let goal =
            tokio::task::spawn_blocking(move || store.update_status_as_model(&session_id, status))
                .await
                .map_err(|error| failed(UPDATE_GOAL_TOOL_ID, error))?
                .map_err(|error| map_goal_error(UPDATE_GOAL_TOOL_ID, error))?;
        if goal.is_none() {
            return Err(invalid(
                UPDATE_GOAL_TOOL_ID,
                "cannot update goal because this session has no goal",
            ));
        }
        goal_output(UPDATE_GOAL_TOOL_ID, goal)
    }
}

/// Build all three goal tools over one authoritative store.
#[must_use]
pub fn goal_tools(store: Arc<GoalStore>) -> Vec<Arc<dyn Tool>> {
    vec![
        erase(GetGoalTool::new(Arc::clone(&store))),
        erase(CreateGoalTool::new(Arc::clone(&store))),
        erase(UpdateGoalTool::new(store)),
    ]
}

fn goal_output(tool: &str, goal: Option<Goal>) -> Result<ToolOutput, ToolError> {
    let value = serde_json::to_value(&goal).map_err(|error| failed(tool, error))?;
    let rendered = serde_json::to_string_pretty(&value).map_err(|error| failed(tool, error))?;
    let title = goal.as_ref().map_or("No goal", |goal| match goal.status {
        crate::GoalStatus::Active => "Goal active",
        crate::GoalStatus::Paused => "Goal paused",
        crate::GoalStatus::Blocked => "Goal blocked",
        crate::GoalStatus::UsageLimited => "Goal usage limited",
        crate::GoalStatus::BudgetLimited => "Goal budget limited",
        crate::GoalStatus::Complete => "Goal complete",
    });
    Ok(ToolOutput::text(title, rendered).with_metadata("goal", value))
}

fn map_goal_error(tool: &str, error: GoalError) -> ToolError {
    if error.is_model_refusal() {
        ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(error),
        }
    } else {
        failed(tool, error)
    }
}

fn invalid(tool: &str, message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn failed(tool: &str, source: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(source),
    }
}

/// Decode the structured goal metadata from a tool result.
pub fn goal_from_metadata(output: &ToolOutput) -> Result<Option<Goal>, serde_json::Error> {
    serde_json::from_value(output.metadata.get("goal").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
