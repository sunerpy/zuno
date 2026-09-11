//! Explicit Goal resumption binds consent to an exact Goal revision.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESUME_GOAL_CHOICE: &str = "Resume goal";
pub const KEEP_GOAL_PAUSED_CHOICE: &str = "Keep paused";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalResumeRequest {
    pub session_id: String,
    pub goal_id: String,
    #[schemars(range(min = 1))]
    pub expected_revision: i64,
    /// Existing durable input, never text to insert a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
}

impl GoalResumeRequest {
    pub fn validate(&self) -> Result<(), crate::question::QuestionValidationError> {
        if self.session_id.trim().is_empty()
            || self.goal_id.trim().is_empty()
            || self.session_id.len() > 256
            || self.goal_id.len() > 256
            || self.expected_revision < 1
            || self
                .input_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty() || id.len() > 256)
        {
            return Err(crate::question::QuestionValidationError(
                "Goal resume requires bounded non-empty identities and a positive Goal revision"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}
