//! Publish an explicit approval request for the current durable Plan.
//!
//! This tool never changes collaboration mode. The session-control service binds
//! the request to the exact Plan, execution identity and review, then applies an
//! explicit client decision only after the source Plan turn has handed off.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use zuno_error::ToolError;
use zuno_tool::question::QuestionPort;
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, TypedTool};
use zuno_types::question::{QuestionMode, QuestionPurpose, QuestionSpec};

use crate::exposure::{ExposureFlags, exposes_plan_exit};
use crate::question::{question_error, question_origin, question_receipt_output};

pub const WIRE_ID: &str = "plan_exit";
pub const DESCRIPTION: &str = include_str!("description/plan-exit.txt");

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanExitParams {}

pub struct PlanExitTool {
    port: Arc<dyn QuestionPort>,
}

impl PlanExitTool {
    #[must_use]
    pub fn new(port: Arc<dyn QuestionPort>) -> Self {
        Self { port }
    }

    #[must_use]
    pub fn exposed_under(flags: &ExposureFlags) -> bool {
        exposes_plan_exit(flags)
    }
}

#[async_trait]
impl TypedTool for PlanExitTool {
    type Params = PlanExitParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn effect(&self, _: &serde_json::Value) -> ToolEffect {
        ToolEffect::UserMediated
    }

    async fn run(&self, _: PlanExitParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx
            .orchestration_snapshot()
            .is_some_and(|snapshot| snapshot.owner.parent_session_id.is_some())
        {
            return Err(ToolError::Denied {
                tool: WIRE_ID.to_owned(),
                denial: None,
            });
        }
        let started = Instant::now();
        let receipt = self
            .port
            .open(QuestionSpec {
                origin: question_origin(&ctx),
                mode: QuestionMode::Deferred,
                purpose: QuestionPurpose::PlanAuthorization,
                // The service reads the Plan and constructs the displayed
                // question. Model arguments cannot name or forge a binding.
                questions: Vec::new(),
                expected_goal_revision: None,
                plan: None,
            })
            .await
            .map_err(|error| question_error(WIRE_ID, error))?;
        Ok(question_receipt_output(
            &receipt.question,
            started.elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_exit_arguments_cannot_supply_approval_or_a_plan_identity() {
        for input in [
            serde_json::json!({"approved": true}),
            serde_json::json!({"planId": "different-plan"}),
            serde_json::json!({"answers": [["approve"]]}),
        ] {
            assert!(serde_json::from_value::<PlanExitParams>(input).is_err());
        }
    }
}
