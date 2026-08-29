//! `question` — the tool that stops and asks the user something.
//!
//! # Its exposure is a client capability, not a preference
//!
//! Offered only when the client has somewhere to draw a prompt — `app`, `cli` or
//! `desktop` — or when [`crate::exposure::ENV_ENABLE_QUESTION_TOOL`] overrides that
//! (`registry.ts:202,228`). A headless host that offered it would advertise a tool
//! whose every call blocks forever, so the condition is a capability check and the
//! predicate is [`crate::exposure::exposes_question`]. Verified against the real
//! binary: `ZUNO_CLIENT=tui` drops it, the same run with the override flag
//! restores it.
//!
//! # Where the answer comes from
//!
//! Not from here. A question travels out to a client, waits for a human, and comes
//! back — a round trip through the event bus and the HTTP API upstream
//! (`packages/core/src/question.ts`). This crate has neither, so the round trip is a
//! seam: [`QuestionAsker`]. The tool's job is to shape the request, hand it over, and
//! render whatever comes back; the transport belongs to the server layer.
//!
//! [`ScriptedAnswers`] is the test double, and it is also what makes the rendering
//! assertions possible without a running server.
//!
//! # `Prompt` is not `Info`
//!
//! Upstream has two shapes that differ by one field. The **tool's** parameters are
//! `Question.Prompt` — `question`, `header`, `options`, `multiple`
//! (`packages/schema/src/v1/question.ts:20-25,30`) — while the **service** takes
//! `Question.Info`, which adds `custom`. So the model cannot switch off the
//! "type your own answer" affordance; only an internal caller can, which is exactly
//! what [`crate::plan_exit`] does with `custom: false`. [`QuestionPrompt`] is the
//! model-facing shape and [`QuestionRequest`] the internal one, kept apart for that
//! reason rather than merged with an optional field.

use crate::exposure::{ExposureFlags, exposes_question};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, TypedTool};

/// The id the model calls. Registry key and wire id agree (`registry.ts:218`).
pub const WIRE_ID: &str = "question";

/// The description the model reads, verbatim from `tool/question.txt`.
pub const DESCRIPTION: &str = include_str!("description/question.txt");

/// What a question with no selected answer renders as.
///
/// Oracle: `question.ts:31`. Rendered rather than omitted, so the model can tell
/// "the user skipped this" from "the user chose nothing meaningful".
pub const UNANSWERED: &str = "Unanswered";

/// One selectable answer.
///
/// Field descriptions are the oracle's annotations
/// (`packages/schema/src/v1/question.ts:16-19`), including the length guidance, since
/// that text is what steers the model into writing usable labels.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    /// Display text (1-5 words, concise)
    pub label: String,
    /// Explanation of choice
    pub description: String,
}

impl QuestionOption {
    /// An option.
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }
}

/// One question, as the model may write it.
///
/// Deliberately without `custom`: see the module docs. Upstream's `Prompt` omits it,
/// so a model asking for a closed set of options cannot get one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionPrompt {
    /// Complete question
    pub question: String,
    /// Very short label (max 30 chars)
    pub header: String,
    /// Available choices
    pub options: Vec<QuestionOption>,
    /// Allow selecting multiple choices
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
}

impl QuestionPrompt {
    /// A question with the given options.
    #[must_use]
    pub fn new(
        question: impl Into<String>,
        header: impl Into<String>,
        options: Vec<QuestionOption>,
    ) -> Self {
        Self {
            question: question.into(),
            header: header.into(),
            options,
            multiple: None,
        }
    }

    /// The internal request form, with the custom-answer affordance left at its
    /// default.
    #[must_use]
    pub fn into_request(self) -> QuestionRequest {
        QuestionRequest {
            question: self.question,
            header: self.header,
            options: self.options,
            multiple: self.multiple,
            custom: None,
        }
    }
}

/// One question as the asking layer receives it.
///
/// Upstream's `Question.Info`: `Prompt` plus `custom`
/// (`packages/schema/src/v1/question.ts:27-30`). Only internal callers construct this
/// with `custom` set; a model-written question always leaves it `None`, which the
/// client reads as "allow a typed answer" — the documented default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuestionRequest {
    /// The question text.
    pub question: String,
    /// The short label shown beside it.
    pub header: String,
    /// The offered choices.
    pub options: Vec<QuestionOption>,
    /// Whether more than one choice may be selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Whether a typed answer is offered. `None` means the client's default, which is
    /// on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

impl QuestionRequest {
    /// A question with the custom-answer affordance suppressed.
    ///
    /// The shape [`crate::plan_exit`] needs: a strict yes/no where a typed answer
    /// would have no meaning.
    #[must_use]
    pub fn closed(
        question: impl Into<String>,
        header: impl Into<String>,
        options: Vec<QuestionOption>,
    ) -> Self {
        Self {
            question: question.into(),
            header: header.into(),
            options,
            multiple: None,
            custom: Some(false),
        }
    }
}

/// One question's answer: the labels the user selected.
///
/// A list even for a single-select question, because upstream's `Question.Answer` is
/// `Schema.Array(Schema.String)` (`v1/question.ts:41`) — empty means unanswered.
pub type Answer = Vec<String>;

/// The terminal state persisted on a completed question card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionStatus {
    Answered,
    Cancelled,
    Expired,
    Failed,
}

impl QuestionStatus {
    /// Stable metadata spelling used by transcript replays and non-TUI clients.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Answered => "Answered",
            Self::Cancelled => "Cancelled",
            Self::Expired => "Expired",
            Self::Failed => "Failed",
        }
    }
}

/// An authoritative result from the client that owned the human prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(Vec<Answer>),
    Cancelled,
    Expired,
    Failed,
}

impl QuestionOutcome {
    /// The terminal status represented by this outcome.
    #[must_use]
    pub const fn status(&self) -> QuestionStatus {
        match self {
            Self::Answered(_) => QuestionStatus::Answered,
            Self::Cancelled => QuestionStatus::Cancelled,
            Self::Expired => QuestionStatus::Expired,
            Self::Failed => QuestionStatus::Failed,
        }
    }
}

impl Default for QuestionOutcome {
    fn default() -> Self {
        Self::Answered(Vec::new())
    }
}

/// The round trip to a human.
///
/// # Errors
///
/// [`ToolError::Denied`] when the user dismissed the request rather than answering.
/// That maps upstream's `Question.RejectedError`, and `Denied` is the honest variant:
/// the call cannot proceed until a human decides differently, which is exactly what
/// `Denied` means to the retry policy.
#[async_trait]
pub trait QuestionAsker: Send + Sync + 'static {
    /// Ask `questions` for `session_id` and wait for the answers.
    ///
    /// The returned list is positional: `answers[i]` belongs to `questions[i]`. An
    /// implementation that returns a shorter list is treated as having left the
    /// remainder unanswered rather than as an error, matching upstream's
    /// `answers[i]?.length` guard (`question.ts:31`).
    ///
    /// `call` is `(message_id, call_id)` when the ask originated in a tool call, so
    /// the client can attach the prompt to that call in the transcript. `None` for an
    /// ask that did not (`question.ts:27`).
    ///
    /// # Errors
    ///
    /// [`ToolError`] only when the asker cannot even construct the request. Delivery
    /// and human terminal states are values so the originating tool call can be
    /// persisted and is never re-opened after a session replay.
    async fn ask(
        &self,
        session_id: &str,
        questions: &[QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError>;
}

/// A [`QuestionAsker`] that answers from a script and records what it was asked.
///
/// The only way to assert the rendering without a client on the other end. Also used
/// by [`crate::plan_exit`]'s tests, which is why it lives here rather than in a test
/// module: one double, one contract.
#[derive(Debug, Default)]
pub struct ScriptedAnswers {
    outcome: QuestionOutcome,
    asked: Mutex<Vec<QuestionRequest>>,
}

impl ScriptedAnswers {
    /// Answers every ask with `answers`, positionally.
    #[must_use]
    pub fn new(answers: Vec<Answer>) -> Self {
        Self {
            outcome: QuestionOutcome::Answered(answers),
            asked: Mutex::new(Vec::new()),
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
        Self::with_outcome(QuestionOutcome::Cancelled)
    }

    /// Returns an expired terminal outcome.
    #[must_use]
    pub fn expiring() -> Self {
        Self::with_outcome(QuestionOutcome::Expired)
    }

    /// Returns a failed-delivery terminal outcome.
    #[must_use]
    pub fn failing() -> Self {
        Self::with_outcome(QuestionOutcome::Failed)
    }

    fn with_outcome(outcome: QuestionOutcome) -> Self {
        Self {
            outcome,
            asked: Mutex::new(Vec::new()),
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
impl QuestionAsker for ScriptedAnswers {
    async fn ask(
        &self,
        _session_id: &str,
        questions: &[QuestionRequest],
        _call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(questions);
        Ok(self.outcome.clone())
    }
}

/// Arguments to `question`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionParams {
    /// Questions to ask
    pub questions: Vec<QuestionPrompt>,
}

/// Asks the user one or more questions and returns their answers.
pub struct QuestionTool {
    asker: Arc<dyn QuestionAsker>,
}

impl QuestionTool {
    /// The tool, asking through `asker`.
    #[must_use]
    pub fn new(asker: Arc<dyn QuestionAsker>) -> Self {
        Self { asker }
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
    pub fn title(status: QuestionStatus, count: usize, elapsed: Duration) -> String {
        let plural = if count == 1 { "" } else { "s" };
        format!(
            "{} · {count} question{plural} · {}",
            status.label(),
            format_elapsed(elapsed)
        )
    }

    /// The `"question"="answer"` list upstream builds from the answers.
    ///
    /// Oracle: `question.ts:30-32`. A question with no selected labels renders as
    /// [`UNANSWERED`]; multiple labels join with `", "` — the same separator that
    /// joins the pairs, which is upstream's ambiguity and not this port's to fix.
    #[must_use]
    pub fn format_answers(questions: &[QuestionPrompt], answers: &[Answer]) -> String {
        questions
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                let selected = answers.get(index).filter(|labels| !labels.is_empty());
                let rendered =
                    selected.map_or_else(|| UNANSWERED.to_owned(), |labels| labels.join(", "));
                format!("\"{}\"=\"{}\"", prompt.question, rendered)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl TypedTool for QuestionTool {
    type Params = QuestionParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::UserMediated
    }

    async fn run(&self, params: QuestionParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        // No permission ask. Upstream's `question.ts` has none (contrast `todo.ts:24`),
        // because the tool's whole effect *is* asking the user — gating a prompt behind
        // a prompt would be circular.
        let requests: Vec<QuestionRequest> = params
            .questions
            .iter()
            .cloned()
            .map(QuestionPrompt::into_request)
            .collect();

        let started = Instant::now();
        let outcome = self
            .asker
            .ask(
                &ctx.session_id,
                &requests,
                Some((&ctx.message_id, &ctx.call_id)),
            )
            .await?;
        let elapsed = started.elapsed();
        let status = outcome.status();

        let output = match outcome {
            QuestionOutcome::Answered(answers) => {
                let formatted = Self::format_answers(&params.questions, &answers);
                let metadata =
                    serde_json::to_value(&answers).map_err(|error| ToolError::Failed {
                        tool: WIRE_ID.to_owned(),
                        source: Box::new(error),
                    })?;
                ToolOutput::text(
                    Self::title(status, params.questions.len(), elapsed),
                    format!(
                        "User has answered your questions: {formatted}. \
                         You can now continue with the user's answers in mind."
                    ),
                )
                .with_metadata("answers", metadata)
            }
            QuestionOutcome::Cancelled => ToolOutput::text(
                Self::title(status, params.questions.len(), elapsed),
                "The user cancelled this question request. Do not infer an answer or immediately \
                 repeat the same question.",
            ),
            QuestionOutcome::Expired => ToolOutput::text(
                Self::title(status, params.questions.len(), elapsed),
                "This question request expired before the user answered. Do not infer an answer.",
            ),
            QuestionOutcome::Failed => ToolOutput::text(
                Self::title(status, params.questions.len(), elapsed),
                "This question request could not be delivered. Do not infer an answer.",
            ),
        };
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        Ok(output
            .with_metadata("questionStatus", status.as_str())
            .with_metadata("questionCount", params.questions.len() as u64)
            .with_metadata("elapsedMs", elapsed_ms))
    }
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

    fn tool(asker: Arc<dyn QuestionAsker>) -> Arc<dyn Tool> {
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
        let output = tool(Arc::clone(&asker) as Arc<dyn QuestionAsker>)
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
    async fn the_output_quotes_the_question_and_the_selected_label() {
        let output = tool(Arc::new(ScriptedAnswers::selecting("SQLite")))
            .execute(one_question(), context())
            .await
            .expect("the scripted answer");

        assert_eq!(
            output.output,
            "User has answered your questions: \"Which database?\"=\"SQLite\". \
             You can now continue with the user's answers in mind."
        );
        assert_eq!(
            answers_from_metadata(&output.metadata).expect("decodable"),
            vec![vec!["SQLite".to_owned()]]
        );
    }

    #[tokio::test]
    async fn an_unselected_question_renders_as_unanswered() {
        let output = tool(Arc::new(ScriptedAnswers::new(vec![Vec::new()])))
            .execute(one_question(), context())
            .await
            .expect("an empty answer is not a failure");

        assert!(output.output.contains("\"Which database?\"=\"Unanswered\""));
    }

    #[tokio::test]
    async fn a_missing_answer_renders_as_unanswered_rather_than_failing() {
        // Upstream's `answers[i]?.length` tolerates a short list; so does this.
        let output = tool(Arc::new(ScriptedAnswers::new(Vec::new())))
            .execute(one_question(), context())
            .await
            .expect("a short answer list is tolerated");

        assert!(output.output.contains("\"Unanswered\""));
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

    // --- rendering rules that look wrong and are upstream's ---

    #[test]
    fn the_title_marks_terminal_status_count_and_elapsed_time() {
        assert_eq!(
            QuestionTool::title(QuestionStatus::Answered, 0, Duration::ZERO),
            "Answered · 0 questions · <1s"
        );
        assert_eq!(
            QuestionTool::title(QuestionStatus::Cancelled, 1, Duration::from_secs(18)),
            "Cancelled · 1 question · 18s"
        );
        assert_eq!(
            QuestionTool::title(QuestionStatus::Expired, 2, Duration::from_secs(62)),
            "Expired · 2 questions · 1m 02s"
        );
    }

    #[test]
    fn multiple_selected_labels_join_with_a_comma() {
        let questions = vec![QuestionPrompt::new("Pick two", "Pick", Vec::new())];
        let answers = vec![vec!["a".to_owned(), "b".to_owned()]];
        assert_eq!(
            QuestionTool::format_answers(&questions, &answers),
            "\"Pick two\"=\"a, b\""
        );
    }

    #[test]
    fn several_questions_join_with_the_same_separator_as_their_labels() {
        // Upstream's ambiguity, reproduced: the pair separator and the label separator
        // are both ", ", so a multi-select answer is indistinguishable from two
        // questions by the separator alone.
        let questions = vec![
            QuestionPrompt::new("One?", "1", Vec::new()),
            QuestionPrompt::new("Two?", "2", Vec::new()),
        ];
        let answers = vec![vec!["x".to_owned()], vec!["y".to_owned()]];
        assert_eq!(
            QuestionTool::format_answers(&questions, &answers),
            "\"One?\"=\"x\", \"Two?\"=\"y\""
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
    fn the_description_is_the_oracles_file() {
        assert!(DESCRIPTION.starts_with(
            "Use this tool when you need to ask the user questions during execution."
        ));
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
