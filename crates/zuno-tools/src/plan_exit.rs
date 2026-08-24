//! `plan_exit` — leaving plan mode for the build agent, with the user's consent.
//!
//! # The registry key is not the wire id, and the condition is a conjunction
//!
//! Upstream keys this `plan` in the registry (`registry.ts:220`) and names it
//! `plan_exit` on the wire (`plan.ts:16`). [`WIRE_ID`] is the wire id.
//!
//! It is offered only when plan mode is on **and** the client is `cli`
//! (`registry.ts:243`). Both halves are required, and there is no override flag for
//! the client half — unlike [`crate::question`], which `app` and `desktop` also get.
//! The predicate is [`crate::exposure::exposes_plan_exit`].
//!
//! # There is a second gate, and it is not in this crate
//!
//! Measured against the real 1.18.12 binary: with
//! `ZUNO_EXPERIMENTAL_PLAN_MODE=true`, `plan_exit` is **absent** from the `build`
//! agent and **present** on `plan`. The registry offered it in both cases; the
//! permission ruleset took it away from `build` — `plan_exit: "deny"` in the defaults
//! and `plan_exit: "allow"` only for the `plan` agent
//! (`packages/opencode/src/agent/agent.ts:128,164`). That layer is [`zuno_permission`]'s.
//! A caller that applies only [`crate::exposure`] will over-offer this tool.
//!
//! # What the tool actually does
//!
//! Three steps, in order (`plan.ts:26-75`):
//!
//! 1. Ask the user one closed yes/no question naming the plan file.
//! 2. If the answer is "No", **fail** — upstream raises `Question.RejectedError`, so
//!    staying in plan mode is an error result and not a successful no-op. This port
//!    reports [`zuno_error::ToolError::Denied`], which is the same claim: the call
//!    cannot proceed until a human decides differently.
//! 3. Otherwise switch the session to the `build` agent by appending a synthetic user
//!    message, and return "Switching to build agent".
//!
//! Step 3 writes session messages, which this crate cannot do, so it is the
//! [`PlanExitHost`] seam. Step 1 goes through the same [`crate::question::QuestionAsker`]
//! the `question` tool uses, because upstream calls the same service — one transport,
//! not two.

use crate::exposure::{ExposureFlags, exposes_plan_exit};
use crate::question::{QuestionAsker, QuestionOption, QuestionOutcome, QuestionRequest};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, TypedTool};

/// The id the model calls.
///
/// Not `plan`: that is upstream's registry key (`registry.ts:220`).
pub const WIRE_ID: &str = "plan_exit";

/// The description the model reads, verbatim from `tool/plan-exit.txt`.
pub const DESCRIPTION: &str = include_str!("description/plan-exit.txt");

/// The short label beside the approval prompt, verbatim from `plan.ts:35`.
pub const QUESTION_HEADER: &str = "Build Agent";

/// The label that approves the switch, verbatim from `plan.ts:38`.
pub const APPROVE_LABEL: &str = "Yes";

/// The label that declines it, verbatim from `plan.ts:39`.
pub const DECLINE_LABEL: &str = "No";

/// The agent the session switches to, verbatim from `plan.ts:58`.
pub const BUILD_AGENT: &str = "build";

/// The title on the rendered result, verbatim from `plan.ts:72`.
pub const TITLE: &str = "Switching to build agent";

/// The output the model reads on approval, verbatim from `plan.ts:73`.
pub const APPROVED_OUTPUT: &str =
    "User approved switching to build agent. Wait for further instructions.";

/// The session-layer effects this tool needs and this crate cannot perform.
///
/// Two operations, both owned by the session layer: naming the plan file and
/// recording the approval. A trait rather than a dependency on that layer, because
/// `zuno-tools` sits below it — and because the tool's interesting behaviour is the
/// question and the refusal, which a test must be able to drive without a session
/// store.
#[async_trait]
pub trait PlanExitHost: Send + Sync + 'static {
    /// The session's plan file, relative to the worktree.
    ///
    /// Upstream computes `path.relative(worktree, Session.plan(info, instance))`
    /// (`plan.ts:29`) and puts the result straight into the question text, so the user
    /// approves a named file rather than an abstraction.
    ///
    /// # Errors
    ///
    /// [`ToolError`] when the session or its plan path cannot be resolved.
    async fn plan_path(&self, session_id: &str) -> Result<String, ToolError>;

    /// Record the approval: switch `session_id` to the `build` agent.
    ///
    /// Upstream appends a synthetic user message carrying the agent switch and a
    /// text part naming the approved plan (`plan.ts:53-69`). The model that comes
    /// next reads that part, which is why the plan path is passed in rather than
    /// re-derived.
    ///
    /// # Errors
    ///
    /// [`ToolError`] when the switch could not be recorded.
    async fn switch_to_build(&self, session_id: &str, plan: &str) -> Result<(), ToolError>;
}

/// A [`PlanExitHost`] that records the switch instead of performing it.
///
/// The test double, and a usable stand-in for a host that has no session store yet.
#[derive(Debug)]
pub struct RecordingHost {
    plan: String,
    switched: Mutex<Vec<(String, String)>>,
}

impl RecordingHost {
    /// A host reporting `plan` as every session's plan file.
    #[must_use]
    pub fn new(plan: impl Into<String>) -> Self {
        Self {
            plan: plan.into(),
            switched: Mutex::new(Vec::new()),
        }
    }

    /// Every `(session_id, plan)` this host was asked to switch, in order.
    #[must_use]
    pub fn switched(&self) -> Vec<(String, String)> {
        self.switched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Default for RecordingHost {
    fn default() -> Self {
        Self::new(".zuno/plans/plan.md")
    }
}

#[async_trait]
impl PlanExitHost for RecordingHost {
    async fn plan_path(&self, _session_id: &str) -> Result<String, ToolError> {
        Ok(self.plan.clone())
    }

    async fn switch_to_build(&self, session_id: &str, plan: &str) -> Result<(), ToolError> {
        self.switched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((session_id.to_owned(), plan.to_owned()));
        Ok(())
    }
}

/// Arguments to `plan_exit`: none.
///
/// Upstream's `Schema.Struct({})` (`plan.ts:13`). An empty struct rather than a unit
/// type, so [`schemars`] derives `{"type": "object"}` and the central augmentation in
/// [`zuno_tool`] has an object to add its cross-cutting properties to.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanExitParams {}

/// Asks the user to approve the plan, then switches the session to `build`.
pub struct PlanExitTool {
    asker: Arc<dyn QuestionAsker>,
    host: Arc<dyn PlanExitHost>,
}

impl PlanExitTool {
    /// The tool, asking through `asker` and switching through `host`.
    #[must_use]
    pub fn new(asker: Arc<dyn QuestionAsker>, host: Arc<dyn PlanExitHost>) -> Self {
        Self { asker, host }
    }

    /// Whether the registry offers this tool under `flags`.
    ///
    /// Delegates to [`exposes_plan_exit`] so the tool and the registry cannot hold
    /// divergent copies of the condition. Remember the permission layer's second gate,
    /// documented on this module — this predicate alone is not the whole answer.
    #[must_use]
    pub fn exposed_under(flags: &ExposureFlags) -> bool {
        exposes_plan_exit(flags)
    }

    /// The approval question for a plan at `plan`.
    ///
    /// Verbatim from `plan.ts:32-42`, including `custom: false` — a typed answer has
    /// no meaning for a two-way switch, and this is the one place upstream suppresses
    /// the affordance the model itself cannot.
    #[must_use]
    pub fn approval_question(plan: &str) -> QuestionRequest {
        QuestionRequest::closed(
            format!(
                "Plan at {plan} is complete. \
                 Would you like to switch to the build agent and start implementing?"
            ),
            QUESTION_HEADER,
            vec![
                QuestionOption::new(
                    APPROVE_LABEL,
                    "Switch to build agent and start implementing the plan",
                ),
                QuestionOption::new(
                    DECLINE_LABEL,
                    "Stay with plan agent to continue refining the plan",
                ),
            ],
        )
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

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::UserMediated
    }

    async fn run(
        &self,
        _params: PlanExitParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let plan = self.host.plan_path(&ctx.session_id).await?;
        let started = Instant::now();
        let answers = match self
            .asker
            .ask(
                &ctx.session_id,
                &[Self::approval_question(&plan)],
                Some((&ctx.message_id, &ctx.call_id)),
            )
            .await?
        {
            QuestionOutcome::Answered(answers) => answers,
            QuestionOutcome::Cancelled => {
                return Err(ToolError::Denied {
                    tool: WIRE_ID.to_owned(),
                });
            }
            QuestionOutcome::Expired => {
                return Err(ToolError::Timeout {
                    tool: WIRE_ID.to_owned(),
                    elapsed: started.elapsed(),
                });
            }
            QuestionOutcome::Failed => {
                return Err(ToolError::Failed {
                    tool: WIRE_ID.to_owned(),
                    source: Box::new(std::io::Error::other(
                        "question request failed before the user could answer",
                    )),
                });
            }
        };

        // Upstream tests the *first selected label* of the first answer
        // (`plan.ts:46`), so an empty answer is not a refusal and falls through to the
        // switch. Reproduced: a client that returns no selection has not said "No", and
        // inventing a refusal would strand the session in plan mode.
        if answers
            .first()
            .and_then(|labels| labels.first())
            .is_some_and(|label| label == DECLINE_LABEL)
        {
            return Err(ToolError::Denied {
                tool: WIRE_ID.to_owned(),
            });
        }

        self.host.switch_to_build(&ctx.session_id, &plan).await?;

        Ok(ToolOutput::text(TITLE, APPROVED_OUTPUT)
            .with_metadata("agent", serde_json::Value::String(BUILD_AGENT.to_owned()))
            .with_metadata("plan", serde_json::Value::String(plan)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::ScriptedAnswers;
    use serde_json::json;
    use zuno_tool::{AllowAll, NeverInterrupted, Tool, erase};

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_plan",
            "msg_3",
            "call_4",
            "plan",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    fn tool(asker: Arc<dyn QuestionAsker>, host: Arc<dyn PlanExitHost>) -> Arc<dyn Tool> {
        erase(PlanExitTool::new(asker, host))
    }

    // --- exposure: the conjunction, at both polarities of both halves ---

    #[test]
    fn conditional_plan_exit_is_offered_in_plan_mode_on_a_cli_client() {
        assert!(PlanExitTool::exposed_under(
            &ExposureFlags::default().with_plan_mode()
        ));
    }

    #[test]
    fn conditional_plan_exit_is_withheld_without_plan_mode() {
        assert!(!PlanExitTool::exposed_under(&ExposureFlags::default()));
    }

    #[test]
    fn conditional_plan_exit_is_withheld_from_a_non_cli_client() {
        for client in ["tui", "app", "desktop", ""] {
            assert!(
                !PlanExitTool::exposed_under(
                    &ExposureFlags::default()
                        .with_client(client)
                        .with_plan_mode()
                ),
                "plan_exit must be withheld from the {client:?} client"
            );
        }
    }

    #[test]
    fn conditional_plan_exit_has_no_override_flag_for_the_client_half() {
        // `enable_question_tool` rescues `question` on a headless client; nothing
        // rescues `plan_exit`.
        let configuration = ExposureFlags::default()
            .with_client("tui")
            .with_plan_mode()
            .with_question_tool();
        assert!(!PlanExitTool::exposed_under(&configuration));
    }

    #[test]
    fn the_wire_id_is_plan_exit_not_the_registry_key_plan() {
        let erased = tool(
            Arc::new(ScriptedAnswers::default()),
            Arc::new(RecordingHost::default()),
        );
        assert_eq!(erased.id(), "plan_exit");
        assert_ne!(erased.id(), "plan");
    }

    #[test]
    fn the_parameters_are_an_empty_object_schema() {
        let erased = tool(
            Arc::new(ScriptedAnswers::default()),
            Arc::new(RecordingHost::default()),
        );
        let schema = erased.definition().parameters;
        assert_eq!(schema["type"], "object");
        // The central augmentation still applies, which is why an empty *object* and
        // not a unit type.
        assert_eq!(
            schema["properties"][zuno_tool::INTENT_KEY]["type"],
            "string"
        );
    }

    // --- the question ---

    #[test]
    fn the_approval_question_names_the_plan_file_and_closes_the_answer_set() {
        let question = PlanExitTool::approval_question(".zuno/plans/auth.md");
        assert_eq!(
            question.question,
            "Plan at .zuno/plans/auth.md is complete. \
             Would you like to switch to the build agent and start implementing?"
        );
        assert_eq!(question.header, "Build Agent");
        assert_eq!(question.custom, Some(false));
        let labels: Vec<&str> = question
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        assert_eq!(labels, vec!["Yes", "No"]);
    }

    #[tokio::test]
    async fn the_question_reaches_the_asker_with_the_hosts_plan_path() {
        let asker = Arc::new(ScriptedAnswers::selecting("Yes"));
        let host = Arc::new(RecordingHost::new("plans/feature.md"));
        tool(
            Arc::clone(&asker) as Arc<dyn QuestionAsker>,
            Arc::clone(&host) as Arc<dyn PlanExitHost>,
        )
        .execute(json!({}), context())
        .await
        .expect("approved");

        let asked = asker.asked();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].question.contains("plans/feature.md"));
    }

    // --- approval ---

    #[tokio::test]
    async fn an_approval_switches_the_session_to_the_build_agent() {
        let host = Arc::new(RecordingHost::new("plans/x.md"));
        let output = tool(
            Arc::new(ScriptedAnswers::selecting("Yes")),
            Arc::clone(&host) as Arc<dyn PlanExitHost>,
        )
        .execute(json!({}), context())
        .await
        .expect("approved");

        assert_eq!(output.title, "Switching to build agent");
        assert_eq!(
            output.output,
            "User approved switching to build agent. Wait for further instructions."
        );
        assert_eq!(output.metadata["agent"], "build");
        assert_eq!(output.metadata["plan"], "plans/x.md");
        assert_eq!(
            host.switched(),
            vec![("ses_plan".to_owned(), "plans/x.md".to_owned())]
        );
    }

    // --- refusal ---

    #[tokio::test]
    async fn declining_fails_the_call_and_leaves_the_session_in_plan_mode() {
        let host = Arc::new(RecordingHost::default());
        let error = tool(
            Arc::new(ScriptedAnswers::selecting("No")),
            Arc::clone(&host) as Arc<dyn PlanExitHost>,
        )
        .execute(json!({}), context())
        .await
        .expect_err("the user declined");

        assert!(matches!(error, ToolError::Denied { .. }));
        assert_eq!(error.tool(), "plan_exit");
        assert!(
            host.switched().is_empty(),
            "a declined exit must not switch the agent"
        );
    }

    #[tokio::test]
    async fn a_dismissed_prompt_is_denied_and_does_not_switch() {
        let host = Arc::new(RecordingHost::default());
        let error = tool(
            Arc::new(ScriptedAnswers::rejecting()),
            Arc::clone(&host) as Arc<dyn PlanExitHost>,
        )
        .execute(json!({}), context())
        .await
        .expect_err("the user dismissed the prompt");

        assert!(matches!(error, ToolError::Denied { .. }));
        assert!(host.switched().is_empty());
    }

    #[tokio::test]
    async fn an_empty_answer_is_not_a_refusal() {
        // Upstream tests `answers[0]?.[0] === "No"`, so no selection falls through to
        // the switch rather than stranding the session.
        let host = Arc::new(RecordingHost::default());
        tool(
            Arc::new(ScriptedAnswers::new(vec![Vec::new()])),
            Arc::clone(&host) as Arc<dyn PlanExitHost>,
        )
        .execute(json!({}), context())
        .await
        .expect("an unselected answer is not a decline");

        assert_eq!(host.switched().len(), 1);
    }

    #[test]
    fn the_description_is_the_oracles_file() {
        assert!(DESCRIPTION.starts_with(
            "Use this tool when you have completed the planning phase and are ready to exit plan agent."
        ));
        assert!(
            DESCRIPTION.contains("Do not duplicate the approval question"),
            "{DESCRIPTION}"
        );
        assert!(
            DESCRIPTION.contains("no unresolved implementation decision remains"),
            "{DESCRIPTION}"
        );
    }
}
