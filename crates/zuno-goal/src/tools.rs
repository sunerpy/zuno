//! Model-facing tools for reading, creating, and finishing persisted goals.

use crate::{Goal, GoalError, GoalStore, ModelStatus};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{
    METADATA_HUMAN_REQUEST_ID_KEY, Tool, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy,
    TypedTool, erase,
};

/// Wire name of the goal reader.
pub const GET_GOAL_TOOL_ID: &str = "goal_get";
/// Wire name of the model-authorized goal creator.
pub const CREATE_GOAL_TOOL_ID: &str = "goal_propose";
/// Wire name of the model-authorized status updater.
pub const UPDATE_GOAL_TOOL_ID: &str = "goal_update";
/// Wire name of the durable Goal input request.
pub const REQUEST_GOAL_INPUT_TOOL_ID: &str = "goal_request_input";

/// The description the model reads for [`GetGoalTool`].
pub const GET_DESCRIPTION: &str = include_str!("description/get-goal.txt");
/// The description the model reads for [`CreateGoalTool`].
pub const CREATE_DESCRIPTION: &str = include_str!("description/create-goal.txt");
/// The description the model reads for [`UpdateGoalTool`].
pub const UPDATE_DESCRIPTION: &str = include_str!("description/update-goal.txt");
/// The description the model reads for [`GoalRequestInputTool`].
pub const REQUEST_INPUT_DESCRIPTION: &str = include_str!("description/request-input.txt");

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
    /// Concrete checks that define completion. The model cannot later rewrite them.
    #[serde(default)]
    pub success_criteria: Vec<String>,
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
    /// Revision returned by `goal_get`; stale revisions are rejected.
    pub expected_revision: i64,
    /// Terminal status justified by the completion or blocked audit.
    pub status: UpdateGoalStatus,
    /// Stable description of the impasse. Required only with `status: blocked`.
    #[serde(default)]
    pub blocking_condition: Option<String>,
}

/// One selectable response for a durable Goal request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalInputOption {
    /// Concise display label.
    pub label: String,
    /// Consequence or meaning of choosing the option.
    pub description: String,
}

/// Payload for [`GoalRequestInputTool`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalRequestInputParams {
    /// Revision returned by `goal_get`; stale requests are rejected atomically.
    pub expected_revision: i64,
    /// Complete question that names the missing decision or fact.
    pub question: String,
    /// Short client-facing label.
    pub header: String,
    /// Optional choices. An empty list requests free-form input.
    #[serde(default)]
    pub options: Vec<GoalInputOption>,
    /// Whether more than one offered option may be selected.
    #[serde(default)]
    pub multiple: Option<bool>,
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

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
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
        let success_criteria = params
            .success_criteria
            .into_iter()
            .map(|criterion| criterion.trim().to_owned())
            .filter(|criterion| !criterion.is_empty())
            .collect::<Vec<_>>();
        let token_budget = params.token_budget;
        let goal = tokio::task::spawn_blocking(move || {
            store.create_goal_with_criteria(
                &session_id,
                &objective,
                &success_criteria,
                token_budget,
            )
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
        if params.expected_revision <= 0 {
            return Err(invalid(
                UPDATE_GOAL_TOOL_ID,
                "expected_revision must be a positive integer returned by goal_get",
            ));
        }
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let expected_revision = params.expected_revision;
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
                staged_store.stage_failure_signal_checked(
                    &staged_session_id,
                    &condition,
                    expected_revision,
                )
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
        let goal = tokio::task::spawn_blocking(move || {
            if matches!(status, ModelStatus::Complete) {
                store.complete_checked(&session_id, expected_revision)
            } else {
                store.update_status_as_model_checked(&session_id, status, expected_revision)
            }
        })
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

/// Creates one durable human request and yields the autonomous Goal turn.
#[derive(Debug, Clone)]
pub struct GoalRequestInputTool {
    store: Arc<GoalStore>,
}

impl GoalRequestInputTool {
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TypedTool for GoalRequestInputTool {
    type Params = GoalRequestInputParams;

    fn id(&self) -> &str {
        REQUEST_GOAL_INPUT_TOOL_ID
    }

    fn description(&self) -> &str {
        REQUEST_INPUT_DESCRIPTION
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::UserMediated
    }

    async fn run(
        &self,
        params: GoalRequestInputParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        validate_goal_request(&params)?;
        let request_id = format!("que_{}", uuid::Uuid::new_v4().simple());
        let payload = serde_json::json!({
            "source": REQUEST_GOAL_INPUT_TOOL_ID,
            "questions": [{
                "question": params.question,
                "header": params.header,
                "options": params.options,
                "multiple": params.multiple,
                "custom": true,
            }],
        });
        let store = Arc::clone(&self.store);
        let session_id = ctx.session_id;
        let message_id = ctx.message_id;
        let call_id = ctx.call_id;
        let expected_revision = params.expected_revision;
        let request = tokio::task::spawn_blocking(move || {
            store.request_human_input(
                &session_id,
                expected_revision,
                request_id,
                payload,
                Some(message_id),
                Some(call_id),
            )
        })
        .await
        .map_err(|error| failed(REQUEST_GOAL_INPUT_TOOL_ID, error))?
        .map_err(|error| map_goal_error(REQUEST_GOAL_INPUT_TOOL_ID, error))?;
        let request_value = serde_json::to_value(&request)
            .map_err(|error| failed(REQUEST_GOAL_INPUT_TOOL_ID, error))?;
        Ok(ToolOutput::text(
            "Waiting for human input",
            format!(
                "Goal paused. Durable human request `{}` is pending; this turn will stop.",
                request.id
            ),
        )
        .with_metadata(METADATA_HUMAN_REQUEST_ID_KEY, request.id)
        .with_metadata("humanRequest", request_value)
        .with_continuation(zuno_tool::ToolContinuation::WaitingForHuman))
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

fn validate_goal_request(params: &GoalRequestInputParams) -> Result<(), ToolError> {
    if params.expected_revision <= 0 {
        return Err(invalid(
            REQUEST_GOAL_INPUT_TOOL_ID,
            "expected_revision must be a positive integer returned by goal_get",
        ));
    }
    if params.question.trim().is_empty() {
        return Err(invalid(
            REQUEST_GOAL_INPUT_TOOL_ID,
            "question must not be empty",
        ));
    }
    let header = params.header.trim();
    if header.is_empty() || header.chars().count() > 30 {
        return Err(invalid(
            REQUEST_GOAL_INPUT_TOOL_ID,
            "header must contain 1 to 30 characters",
        ));
    }
    if params.multiple == Some(true) && params.options.is_empty() {
        return Err(invalid(
            REQUEST_GOAL_INPUT_TOOL_ID,
            "multiple requires at least one option",
        ));
    }
    let mut labels = HashSet::new();
    for option in &params.options {
        let label = option.label.trim();
        if label.is_empty() || !labels.insert(label) {
            return Err(invalid(
                REQUEST_GOAL_INPUT_TOOL_ID,
                "option labels must be non-empty and unique",
            ));
        }
    }
    Ok(())
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
        crate::GoalStatus::Cancelled => "Goal cancelled",
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
