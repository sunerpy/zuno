//! The tool a malformed tool call lands on.
//!
//! # What it is for
//!
//! When a model emits arguments that fail validation, the failure has to become
//! something the model can read and correct. Upstream's answer is a real registered
//! tool whose only job is to render that message: `invalid.ts:14-19` returns
//! `The arguments provided to the tool are invalid: <error>`.
//!
//! It is therefore **offered to the model** — see the measured transcript in
//! [`crate::exposure`], where it is present in all 18 configurations — with the
//! description "Do not use". That reads like a contradiction and is not: the name has
//! to be in the model's vocabulary for the correction message to arrive attributed to
//! a tool call, while the description discourages the model from calling it on
//! purpose. The condition is "always", expressed as
//! [`crate::exposure::exposes_invalid`].
//!
//! # It does not decide anything
//!
//! This tool formats a diagnosis someone else made. It does not validate, does not
//! inspect the offending call, and never fails — `run` is infallible in both
//! implementations. Argument validation itself lives in [`zuno_tool`]'s adapter, which
//! produces [`zuno_error::ToolError::InvalidArgs`]; routing such a failure here is the
//! agent loop's business.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The id the model calls, and the key the registry stores.
///
/// Registry key and wire id agree here — `registry.ts:206` keys it `invalid` and
/// `invalid.ts:10` names it `invalid` — unlike `todowrite` and `plan_exit`.
pub const WIRE_ID: &str = "invalid";

/// The description the model reads.
///
/// Three words, verbatim from `invalid.ts:12`. Deliberately not expanded: a longer
/// description would spend prompt tokens teaching a tool the model should never
/// choose.
pub const DESCRIPTION: &str = include_str!("description/invalid.txt");

/// The title on the rendered result, verbatim from `invalid.ts:16`.
pub const TITLE: &str = "Invalid Tool";

/// Arguments the reporter fills in, not the model.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvalidParams {
    /// The tool whose arguments were invalid.
    pub tool: String,
    /// The validation error to show the model.
    pub error: String,
}

/// Renders a validation failure as tool output.
#[derive(Debug, Clone, Copy, Default)]
pub struct InvalidTool;

impl InvalidTool {
    /// The tool. Stateless, so this is here only for symmetry with the others.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The message body for `error`, without building a [`ToolOutput`].
    ///
    /// Exposed because the agent loop may want the sentence on a path that is not a
    /// tool call, and a second copy of the wording is a second thing to drift.
    #[must_use]
    pub fn message(error: &str) -> String {
        format!("The arguments provided to the tool are invalid: {error}")
    }
}

#[async_trait]
impl TypedTool for InvalidTool {
    type Params = InvalidParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: InvalidParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        // `metadata` is `{}` upstream (`invalid.ts:18`). The offending tool's name is
        // recorded anyway: it is the one fact a reporter cannot recover from the
        // output text, and it costs nothing the model reads.
        Ok(ToolOutput::text(TITLE, Self::message(&params.error))
            .with_metadata("tool", Value::String(params.tool)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exposure::{ExposureFlags, exposes_invalid};
    use serde_json::json;
    use std::sync::Arc;
    use zuno_tool::{AllowAll, NeverInterrupted, erase};

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_invalid",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[test]
    fn conditional_invalid_is_offered_unconditionally() {
        assert!(exposes_invalid(&ExposureFlags::default()));
        assert!(exposes_invalid(
            &ExposureFlags::default().with_client("tui").with_plan_mode()
        ));
    }

    #[test]
    fn the_wire_id_and_description_are_the_oracles() {
        let definition = erase(InvalidTool::new()).definition();
        assert_eq!(definition.id, "invalid");
        assert_eq!(definition.description, "Do not use");
    }

    #[tokio::test]
    async fn the_output_quotes_the_validation_error() {
        let output = erase(InvalidTool::new())
            .execute(
                json!({ "tool": "read", "error": "filePath is required" }),
                context(),
            )
            .await
            .expect("the invalid tool never fails");

        assert_eq!(output.title, "Invalid Tool");
        assert_eq!(
            output.output,
            "The arguments provided to the tool are invalid: filePath is required"
        );
        assert_eq!(output.metadata["tool"], "read");
    }

    #[tokio::test]
    async fn the_message_helper_and_the_tool_agree() {
        let output = erase(InvalidTool::new())
            .execute(json!({ "tool": "grep", "error": "boom" }), context())
            .await
            .expect("infallible");

        assert_eq!(output.output, InvalidTool::message("boom"));
    }

    #[test]
    fn both_arguments_are_required_by_the_schema() {
        let definition = erase(InvalidTool::new()).definition();
        let required = definition.parameters["required"]
            .as_array()
            .expect("an object schema has a required list")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert!(required.contains(&"tool"));
        assert!(required.contains(&"error"));
    }
}
