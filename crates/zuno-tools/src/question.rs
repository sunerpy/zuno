//! Structured questions over the session-owned durable [`QuestionPort`].
//!
//! Publication and a tool's optional waiting future have separate lifetimes.
//! The tool returns a receipt; the accepted answer enters the durable inbox once.
//! Only the host can create a closed Plan authorization binding. Model-authored
//! questions retain the custom-answer affordance.

use crate::exposure::{ExposureFlags, exposes_question};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use zuno_error::ToolError;
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_tool::{
    QuestionResultPresentation, QuestionResultStatus, ToolContext, ToolEffect, ToolOutput,
    ToolResultPresentation, TypedTool,
};
use zuno_types::question::{
    QuestionCommand, QuestionItem, QuestionMode, QuestionOrigin, QuestionPurpose, QuestionReceipt,
    QuestionSpec, QuestionState, QuestionView,
};

/// The id the model calls. Registry key and wire id agree (`registry.ts:218`).
pub const WIRE_ID: &str = "question";
pub const ASYNC_WIRE_ID: &str = "question_async";
pub const ASYNC_DESCRIPTION: &str = include_str!("description/question-async.txt");

/// The description the model reads, verbatim from `tool/question.txt`.
pub const DESCRIPTION: &str = include_str!("description/question.txt");

pub use zuno_types::question::{QuestionOption, QuestionPrompt, QuestionRequest};

/// One question's answer: the labels the user selected.
///
/// A list even for a single-select question, because upstream's `Question.Answer` is
/// `Schema.Array(Schema.String)` (`v1/question.ts:41`) — empty means unanswered.
pub type Answer = Vec<String>;

/// Response script used only by the in-memory port double.
#[derive(Debug, Clone, Default)]
enum ScriptedOutcome {
    Answered(Vec<Answer>),
    Cancelled,
    Expired,
    #[default]
    Failed,
}

/// In-memory QuestionPort for isolated tool/registry tests.
#[derive(Debug)]
pub struct ScriptedAnswers {
    outcome: ScriptedOutcome,
    asked: Mutex<Vec<QuestionRequest>>,
    current: Mutex<Option<QuestionView>>,
}

impl Default for ScriptedAnswers {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ScriptedAnswers {
    /// Answers every ask with `answers`, positionally.
    #[must_use]
    pub fn new(answers: Vec<Answer>) -> Self {
        Self {
            outcome: ScriptedOutcome::Answered(answers),
            asked: Mutex::new(Vec::new()),
            current: Mutex::new(None),
        }
    }

    /// A single-question script selecting one label.
    #[must_use]
    pub fn selecting(label: impl Into<String>) -> Self {
        Self::new(vec![vec![label.into()]])
    }

    /// Cancels every ask, as a user dismissing the prompt does.
    #[must_use]
    pub fn rejecting() -> Self {
        Self::with_outcome(ScriptedOutcome::Cancelled)
    }

    /// Returns an expired terminal outcome.
    #[must_use]
    pub fn expiring() -> Self {
        Self::with_outcome(ScriptedOutcome::Expired)
    }

    /// Returns a failed-delivery terminal outcome.
    #[must_use]
    pub fn failing() -> Self {
        Self::with_outcome(ScriptedOutcome::Failed)
    }

    fn with_outcome(outcome: ScriptedOutcome) -> Self {
        Self {
            outcome,
            asked: Mutex::new(Vec::new()),
            current: Mutex::new(None),
        }
    }

    /// Every question this double has been asked, in order.
    #[must_use]
    pub fn asked(&self) -> Vec<QuestionRequest> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl QuestionPort for ScriptedAnswers {
    async fn open(&self, spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(&spec.questions);
        let view = QuestionView {
            id: "que_scripted".to_owned(),
            origin: spec.origin,
            revision: 1,
            mode: spec.mode,
            purpose: spec.purpose,
            state: QuestionState::Pending,
            questions: spec
                .questions
                .into_iter()
                .enumerate()
                .map(|(index, question)| QuestionItem {
                    id: format!("q{}", index + 1),
                    question,
                })
                .collect(),
            answers: Default::default(),
            draft_answers: Default::default(),
            plan: spec.plan,
            decision: None,
            authorization: None,
            time_created: 0,
            time_updated: 0,
        };
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(view.clone());
        Ok(QuestionReceipt {
            question: view,
            input_id: None,
            duplicate: false,
        })
    }

    async fn apply(&self, _: &str, _: &str, _: QuestionCommand) -> QuestionResult<QuestionReceipt> {
        Err(QuestionError::Unavailable(
            "scripted questions do not accept client input".to_owned(),
        ))
    }

    async fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|view| view.id == request_id && view.origin.session_id == session_id)
            .ok_or_else(|| QuestionError::NotFound {
                session_id: session_id.to_owned(),
                request_id: request_id.to_owned(),
            })
    }

    async fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        Ok(self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|view| {
                view.origin.session_id == session_id && view.state == QuestionState::Pending
            })
            .into_iter()
            .collect())
    }

    async fn wait_for_change(
        &self,
        session_id: &str,
        request_id: &str,
        _: i64,
        interrupt: Arc<dyn zuno_tool::InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        if interrupt.is_set() {
            return Err(QuestionError::Interrupted);
        }
        let mut view = self.get(session_id, request_id).await?;
        view.mode = QuestionMode::Deferred;
        view.revision += 1;
        view.state = match &self.outcome {
            ScriptedOutcome::Answered(answers) => {
                view.answers = view
                    .questions
                    .iter()
                    .zip(answers)
                    .filter(|(_, answers)| !answers.is_empty())
                    .map(|(question, answers)| (question.id.clone(), answers.clone()))
                    .collect();
                if view.is_fully_answered() {
                    QuestionState::Answered
                } else {
                    QuestionState::Pending
                }
            }
            ScriptedOutcome::Cancelled => QuestionState::Cancelled,
            ScriptedOutcome::Expired => QuestionState::Expired,
            ScriptedOutcome::Failed => QuestionState::Failed,
        };
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(view.clone());
        Ok(view)
    }
}

/// Arguments to `question`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionParams {
    /// Questions to ask
    pub questions: Vec<QuestionPrompt>,
}

/// Publishes questions and returns a receipt; answers are delivered by the inbox.
pub struct QuestionTool {
    port: Arc<dyn QuestionPort>,
    mode: QuestionMode,
    purpose: QuestionPurpose,
}

impl QuestionTool {
    /// The tool, asking through `asker`.
    #[must_use]
    pub fn new(port: Arc<dyn QuestionPort>) -> Self {
        Self {
            port,
            mode: QuestionMode::Blocking,
            purpose: QuestionPurpose::Clarification,
        }
    }

    #[must_use]
    pub fn asynchronous(port: Arc<dyn QuestionPort>) -> Self {
        Self {
            port,
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::Clarification,
        }
    }

    #[must_use]
    pub fn required(port: Arc<dyn QuestionPort>) -> Self {
        Self {
            port,
            mode: QuestionMode::Blocking,
            purpose: QuestionPurpose::RequiredInput,
        }
    }

    /// Whether the registry offers this tool under `flags`.
    ///
    /// Delegates to [`exposes_question`] so the tool and the registry cannot hold
    /// divergent copies of the condition.
    #[must_use]
    pub fn exposed_under(flags: &ExposureFlags) -> bool {
        exposes_question(flags)
    }

    /// The durable completed-card title for a terminal question outcome.
    #[must_use]
    pub fn title(status: QuestionResultStatus, count: usize, elapsed: Duration) -> String {
        let plural = if count == 1 { "" } else { "s" };
        format!(
            "{} · {count} question{plural} · {}",
            status.label(),
            format_elapsed(elapsed)
        )
    }
}

#[async_trait]
impl TypedTool for QuestionTool {
    type Params = QuestionParams;

    fn id(&self) -> &str {
        if self.mode == QuestionMode::Deferred {
            ASYNC_WIRE_ID
        } else {
            WIRE_ID
        }
    }

    fn description(&self) -> &str {
        if self.mode == QuestionMode::Deferred {
            ASYNC_DESCRIPTION
        } else {
            DESCRIPTION
        }
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::UserMediated
    }

    async fn run(&self, params: QuestionParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx
            .orchestration_snapshot()
            .is_some_and(|snapshot| snapshot.owner.parent_session_id.is_some())
        {
            return Err(ToolError::Denied {
                tool: self.id().to_owned(),
                denial: None,
            });
        }
        let requests: Vec<QuestionRequest> = params
            .questions
            .iter()
            .cloned()
            .map(QuestionPrompt::into_request)
            .collect();

        let started = Instant::now();
        let receipt = self
            .port
            .open(QuestionSpec {
                origin: question_origin(&ctx),
                mode: self.mode,
                purpose: self.purpose,
                questions: requests,
                expected_goal_revision: None,
                plan: None,
            })
            .await
            .map_err(|error| question_error(self.id(), error))?;
        let mut view = receipt.question;
        if self.mode == QuestionMode::Blocking
            && view.state == QuestionState::Pending
            && view.mode == QuestionMode::Blocking
        {
            match self
                .port
                .wait_for_change(
                    &ctx.session_id,
                    &view.id,
                    view.revision,
                    Arc::clone(&ctx.interrupt),
                )
                .await
            {
                Ok(changed) => view = changed,
                Err(QuestionError::Interrupted) => {}
                Err(error) => return Err(question_error(self.id(), error)),
            }
        }
        let mut output = question_receipt_output(&view, started.elapsed());
        if self.purpose == QuestionPurpose::RequiredInput && view.state != QuestionState::Answered {
            output = output.with_continuation(zuno_tool::ToolContinuation::WaitingForHuman);
        }
        Ok(output)
    }
}

pub(crate) fn question_origin(ctx: &ToolContext) -> QuestionOrigin {
    QuestionOrigin {
        session_id: ctx.session_id.clone(),
        message_id: Some(ctx.message_id.clone()),
        call_id: Some(ctx.call_id.clone()),
        turn_id: ctx
            .orchestration_snapshot()
            .map(|snapshot| snapshot.turn_id.clone()),
        goal_id: None,
    }
}

pub(crate) fn question_error(tool: &str, error: QuestionError) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(error),
    }
}

pub(crate) fn question_receipt_output(view: &QuestionView, elapsed: Duration) -> ToolOutput {
    let status = match view.state {
        QuestionState::Pending if view.revision > 1 => QuestionResultStatus::Deferred,
        QuestionState::Pending => QuestionResultStatus::Pending,
        QuestionState::Answered => QuestionResultStatus::Answered,
        QuestionState::Cancelled => QuestionResultStatus::Cancelled,
        QuestionState::Expired => QuestionResultStatus::Expired,
        QuestionState::Failed => QuestionResultStatus::Failed,
    };
    let detail = match view.state {
        QuestionState::Pending if view.purpose == QuestionPurpose::RequiredInput => {
            "Required input is still missing. Wait for a real human response; do not infer an answer."
        }
        QuestionState::Pending if view.purpose == QuestionPurpose::PlanAuthorization => {
            "The Plan confirmation is pending. Continue the planning summary; do not begin Work without explicit approval."
        }
        QuestionState::Pending => {
            "The question remains open. Continue independent work and state assumptions. The reply will arrive as new input."
        }
        QuestionState::Answered => {
            "The human response is recorded and delivered through the durable inbox. This receipt does not duplicate the answer."
        }
        QuestionState::Cancelled => {
            "The user cancelled the question. Do not infer an answer or immediately repeat the same question."
        }
        QuestionState::Expired => "The question expired without an answer. Do not infer consent.",
        QuestionState::Failed => "The question could not be delivered. Do not infer an answer.",
    };
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    ToolOutput::text(
        QuestionTool::title(status, view.questions.len(), elapsed),
        format!(
            "Question `{}` revision {}: {detail}",
            view.id, view.revision
        ),
    )
    .with_metadata(zuno_tool::METADATA_HUMAN_REQUEST_ID_KEY, view.id.clone())
    .with_metadata("questionStatus", status.as_str())
    .with_metadata("questionCount", view.questions.len() as u64)
    .with_metadata("questionPurpose", view.purpose.as_str())
    .with_metadata("elapsedMs", elapsed_ms)
    .with_presentation(ToolResultPresentation::Question(
        QuestionResultPresentation::new(status, None, view.questions.len(), elapsed_ms),
    ))
}

/// A [`ToolOutput`]'s `answers` metadata, decoded back into answers.
///
/// # Errors
///
/// [`serde_json::Error`] when the value is not a list of label lists.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds == 0 {
        String::from("<1s")
    } else if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

pub fn answers_from_metadata(
    metadata: &serde_json::Map<String, Value>,
) -> Result<Vec<Answer>, serde_json::Error> {
    match metadata.get("answers") {
        Some(value) => serde_json::from_value(value.clone()),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zuno_tool::{AllowAll, NeverInterrupted, Tool, erase};

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_question",
            "msg_7",
            "call_9",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    fn tool(asker: Arc<dyn QuestionPort>) -> Arc<dyn Tool> {
        erase(QuestionTool::new(asker))
    }

    fn one_question() -> Value {
        json!({ "questions": [{
            "question": "Which database?",
            "header": "Database",
            "options": [
                { "label": "Postgres", "description": "Relational" },
                { "label": "SQLite",   "description": "Embedded" },
            ],
        }] })
    }

    // --- exposure: presence under the enabling condition, absence otherwise ---

    #[test]
    fn conditional_question_is_offered_to_an_interactive_client() {
        for client in ["app", "cli", "desktop"] {
            assert!(
                QuestionTool::exposed_under(&ExposureFlags::default().with_client(client)),
                "{client} must be offered the question tool"
            );
        }
    }

    #[test]
    fn conditional_question_is_offered_to_a_headless_client_only_with_the_flag() {
        let headless = ExposureFlags::default().with_client("tui");
        assert!(!QuestionTool::exposed_under(&headless));
        assert!(QuestionTool::exposed_under(&headless.with_question_tool()));
    }

    #[test]
    fn the_wire_id_and_registry_key_agree_for_this_tool() {
        assert_eq!(tool(Arc::new(ScriptedAnswers::default())).id(), "question");
    }

    // --- the parameter shape ---

    #[test]
    fn the_model_cannot_suppress_the_custom_answer_affordance() {
        // `custom` is on upstream's `Info`, not its `Prompt`; a model that sends it is
        // rejected rather than silently obeyed.
        let error = serde_json::from_value::<QuestionPrompt>(json!({
            "question": "q", "header": "h", "options": [], "custom": false
        }))
        .expect_err("custom is not a model-writable field");
        assert!(error.to_string().contains("custom"));
    }

    #[test]
    fn a_model_written_question_leaves_custom_unset() {
        let prompt = QuestionPrompt::new("q", "h", vec![QuestionOption::new("a", "b")]);
        assert_eq!(prompt.clone().into_request().custom, None);
        assert_eq!(
            QuestionRequest::closed("q", "h", vec![]).custom,
            Some(false)
        );
    }

    #[test]
    fn multiple_is_optional_and_reaches_the_asker() {
        let prompt: QuestionPrompt = serde_json::from_value(json!({
            "question": "q", "header": "h", "options": [], "multiple": true
        }))
        .expect("multiple is a documented field");
        assert_eq!(prompt.into_request().multiple, Some(true));
    }

    // --- asking ---

    #[tokio::test]
    async fn the_question_reaches_the_asker_with_the_call_coordinates() {
        let asker = Arc::new(ScriptedAnswers::selecting("Postgres"));
        let output = tool(Arc::clone(&asker) as Arc<dyn QuestionPort>)
            .execute(one_question(), context())
            .await
            .expect("the scripted answer");

        let asked = asker.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].question, "Which database?");
        assert_eq!(asked[0].header, "Database");
        assert_eq!(asked[0].options.len(), 2);
        assert!(output.title.starts_with("Answered · 1 question · "));
    }

    #[tokio::test]
    async fn tool_receipts_do_not_deliver_answers_twice() {
        let output = tool(Arc::new(ScriptedAnswers::selecting("SQLite")))
            .execute(one_question(), context())
            .await
            .expect("the scripted answer");

        assert!(output.output.contains("durable inbox"));
        assert!(!output.output.contains("SQLite"));
        assert!(!output.metadata.contains_key("answers"));
        let Some(ToolResultPresentation::Question(presentation)) = output.presentation.as_ref()
        else {
            panic!("question output must carry a typed presentation");
        };
        assert_eq!(presentation.status(), QuestionResultStatus::Answered);
        assert_eq!(presentation.answers(), None);
        assert_eq!(presentation.question_count(), 1);
    }

    #[tokio::test]
    async fn full_draft_values_never_enter_model_tool_output_or_presentation() {
        let port = ScriptedAnswers::default();
        let mut view = port
            .open(QuestionSpec {
                origin: question_origin(&context()),
                mode: QuestionMode::Deferred,
                purpose: QuestionPurpose::Clarification,
                questions: vec![QuestionPrompt::new("Choose", "Choice", Vec::new()).into_request()],
                expected_goal_revision: None,
                plan: None,
            })
            .await
            .expect("open")
            .question;
        view.revision = 2;
        view.draft_answers.insert(
            view.questions[0].id.clone(),
            vec!["PRIVATE_UNSUBMITTED_VALUE".to_owned()],
        );
        let output = question_receipt_output(&view, Duration::ZERO);
        assert_eq!(output.metadata["questionStatus"], "deferred");
        let surfaces = format!(
            "{}\n{}\n{}\n{}",
            output.title,
            output.output,
            serde_json::to_string(&output.metadata).expect("metadata"),
            serde_json::to_string(&output.presentation).expect("presentation"),
        );
        assert!(!surfaces.contains("PRIVATE_UNSUBMITTED_VALUE"));
        assert!(!surfaces.contains("draftAnswers"));
        assert!(view.answers.is_empty());
        assert!(!view.is_fully_answered());
    }

    #[tokio::test]
    async fn an_unselected_question_renders_as_unanswered() {
        let output = tool(Arc::new(ScriptedAnswers::new(vec![Vec::new()])))
            .execute(one_question(), context())
            .await
            .expect("an empty answer is not a failure");

        assert_eq!(output.metadata["questionStatus"], "deferred");
    }

    #[tokio::test]
    async fn a_missing_answer_renders_as_unanswered_rather_than_failing() {
        let output = tool(Arc::new(ScriptedAnswers::new(Vec::new())))
            .execute(one_question(), context())
            .await
            .expect("a short answer list is tolerated");

        assert_eq!(output.metadata["questionStatus"], "deferred");
    }

    #[tokio::test]
    async fn a_cancelled_request_is_a_durable_terminal_card() {
        let output = tool(Arc::new(ScriptedAnswers::rejecting()))
            .execute(one_question(), context())
            .await
            .expect("cancellation is persisted as the tool's terminal result");

        assert!(output.title.starts_with("Cancelled · 1 question · "));
        assert_eq!(output.metadata["questionStatus"], "cancelled");
        assert!(output.output.contains("Do not infer an answer"));
    }

    #[tokio::test]
    async fn expired_and_failed_requests_have_distinct_terminal_states() {
        for (asker, expected) in [
            (ScriptedAnswers::expiring(), "expired"),
            (ScriptedAnswers::failing(), "failed"),
        ] {
            let output = tool(Arc::new(asker))
                .execute(one_question(), context())
                .await
                .expect("terminal question outcomes are durable tool results");
            assert_eq!(output.metadata["questionStatus"], expected);
        }
    }

    // --- receipt presentation ---

    #[test]
    fn the_title_marks_terminal_status_count_and_elapsed_time() {
        assert_eq!(
            QuestionTool::title(QuestionResultStatus::Answered, 0, Duration::ZERO),
            "Answered · 0 questions · <1s"
        );
        assert_eq!(
            QuestionTool::title(QuestionResultStatus::Cancelled, 1, Duration::from_secs(18)),
            "Cancelled · 1 question · 18s"
        );
        assert_eq!(
            QuestionTool::title(QuestionResultStatus::Expired, 2, Duration::from_secs(62)),
            "Expired · 2 questions · 1m 02s"
        );
    }

    #[test]
    fn absent_metadata_decodes_to_no_answers() {
        assert!(
            answers_from_metadata(&serde_json::Map::new())
                .expect("an absent key is not a failure")
                .is_empty()
        );
    }

    #[test]
    fn description_explains_required_deferred_and_receipt_semantics() {
        assert!(DESCRIPTION.contains("question_async"));
        assert!(DESCRIPTION.contains("Tool results are receipts"));
        assert!(
            DESCRIPTION.contains("cannot be discovered from available evidence"),
            "{DESCRIPTION}"
        );
        assert!(
            DESCRIPTION.contains("Cancellation is a refusal"),
            "{DESCRIPTION}"
        );
    }
}
