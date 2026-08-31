use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ToolError;
use zuno_tool::{
    ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool,
};

use crate::ContinuityError;

/// Stable model-facing history tool id.
pub const HISTORY_TOOL_ID: &str = "history";

/// Provider interface kept separate from the model tool consumer.
pub trait HistoryProvider: Send + Sync {
    fn list_windows(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError>;

    fn list_items(
        &self,
        session_id: &str,
        window_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError>;

    fn read_item(&self, session_id: &str, item_id: &str) -> Result<Value, ContinuityError>;

    fn search_contents(
        &self,
        session_id: &str,
        query: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError>;
}

/// Actions accepted by the native history tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryParams {
    /// List compaction-delimited windows, newest first.
    ListWindows {
        /// Opaque cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, 1 through 50.
        #[serde(default)]
        limit: Option<u32>,
    },
    /// List normalized messages inside one window, oldest first.
    ListItems {
        /// Opaque window id returned by `list_windows`.
        window_id: String,
        /// Opaque cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, 1 through 50.
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Read one normalized message.
    ReadItem {
        /// Opaque item id returned by a list or search action.
        item_id: String,
    },
    /// Search normalized contents in the current session.
    SearchContents {
        /// Non-empty case-insensitive query, at most 1000 UTF-8 bytes.
        query: String,
        /// Opaque cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, 1 through 20.
        #[serde(default)]
        limit: Option<u32>,
    },
}

/// Model consumer for one current-session [`HistoryProvider`].
pub struct HistoryTool {
    provider: Arc<dyn HistoryProvider>,
}

impl HistoryTool {
    #[must_use]
    pub fn new(provider: Arc<dyn HistoryProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl TypedTool for HistoryTool {
    type Params = HistoryParams;

    fn id(&self) -> &str {
        HISTORY_TOOL_ID
    }

    fn description(&self) -> &str {
        include_str!("description/history.txt").trim()
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

    async fn run(
        &self,
        params: HistoryParams,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let value = match params {
            HistoryParams::ListWindows { cursor, limit } => {
                self.provider
                    .list_windows(&context.session_id, cursor.as_deref(), limit)
            }
            HistoryParams::ListItems {
                window_id,
                cursor,
                limit,
            } => {
                self.provider
                    .list_items(&context.session_id, &window_id, cursor.as_deref(), limit)
            }
            HistoryParams::ReadItem { item_id } => {
                self.provider.read_item(&context.session_id, &item_id)
            }
            HistoryParams::SearchContents {
                query,
                cursor,
                limit,
            } => {
                self.provider
                    .search_contents(&context.session_id, &query, cursor.as_deref(), limit)
            }
        }
        .map_err(|error| error.into_tool_error(HISTORY_TOOL_ID))?;
        let output = serde_json::to_string_pretty(&value).map_err(|source| ToolError::Failed {
            tool: HISTORY_TOOL_ID.to_owned(),
            source: Box::new(source),
        })?;
        Ok(ToolOutput::text(HISTORY_TOOL_ID, output).with_metadata("continuityKind", "history"))
    }
}
