//! Durable human questions shared by tools, the host, and client adapters.
//!
//! Publication, waiting, and answering are distinct operations. In particular,
//! accepting a question never means a human answered it or authorized an action.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::execution::TurnExecutionIdentity;

/// One displayed option. Labels are values, not implicit default answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    /// Display text (1-5 words, concise).
    pub label: String,
    /// Explanation of the choice.
    pub description: String,
}

impl QuestionOption {
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }
}

/// Model-authored clarification. Only the host can suppress custom answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionPrompt {
    /// Complete question.
    pub question: String,
    /// Short client-facing label.
    pub header: String,
    /// Suggested choices; an empty list requests free text.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Whether more than one choice may be selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
}

impl QuestionPrompt {
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

/// One host-validated question before its stable item ID is assigned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequest {
    pub question: String,
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

impl QuestionRequest {
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

    #[must_use]
    pub const fn allows_custom(&self) -> bool {
        !matches!(self.custom, Some(false))
    }

    #[must_use]
    pub const fn is_multiple(&self) -> bool {
        matches!(self.multiple, Some(true))
    }

    pub fn validate(&self) -> Result<(), QuestionValidationError> {
        if self.question.trim().is_empty() || self.header.trim().is_empty() {
            return Err(QuestionValidationError(
                "question text and header must not be empty".to_owned(),
            ));
        }
        let mut labels = BTreeSet::new();
        for option in &self.options {
            if option.label.trim().is_empty() || !labels.insert(option.label.as_str()) {
                return Err(QuestionValidationError(
                    "question option labels must be non-empty and unique".to_owned(),
                ));
            }
        }
        if !self.allows_custom() && self.options.is_empty() {
            return Err(QuestionValidationError(
                "a closed question must offer at least one option".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Stable, request-local item identity. IDs survive partial answers and replay.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionItem {
    pub id: String,
    #[serde(flatten)]
    pub question: QuestionRequest,
}

/// Actual answers keyed by stable item ID. An omitted/empty item is unanswered.
pub type QuestionAnswers = BTreeMap<String, Vec<String>>;

/// Whether the originating tool waits. This is not an authorization policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionMode {
    #[default]
    Blocking,
    Deferred,
}

impl QuestionMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Deferred => "deferred",
        }
    }
}

/// The host, never a model-authored question argument, chooses this purpose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionPurpose {
    #[default]
    Clarification,
    RequiredInput,
    PlanAuthorization,
    GoalResume,
}

impl QuestionPurpose {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clarification => "clarification",
            Self::RequiredInput => "required_input",
            Self::PlanAuthorization => "plan_authorization",
            Self::GoalResume => "goal_resume",
        }
    }
}

/// Request lifecycle, independent from whether an open request is deferred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionState {
    #[default]
    Pending,
    Answered,
    Cancelled,
    Expired,
    Failed,
}

impl QuestionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "answered" => Some(Self::Answered),
            "cancelled" => Some(Self::Cancelled),
            "expired" => Some(Self::Expired),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// Source identity supplied by a trusted host/tool context.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionOrigin {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
}

/// The exact Plan and execution identity a human is asked to authorize.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanQuestionBinding {
    pub plan_id: String,
    pub plan_revision: i64,
    /// Logical Plan work cycle; older requests retain exact-turn ownership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cycle_id: Option<String>,
    pub title: String,
    pub completed_steps: usize,
    pub total_steps: usize,
    pub work_identity: TurnExecutionIdentity,
    /// Serialized native review gate, revalidated by the session-control service.
    pub review_gate: serde_json::Value,
}

/// Publication request; callers do not choose the request or question-item IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSpec {
    pub origin: QuestionOrigin,
    pub mode: QuestionMode,
    pub purpose: QuestionPurpose,
    pub questions: Vec<QuestionRequest>,
    pub expected_goal_revision: Option<i64>,
    pub plan: Option<PlanQuestionBinding>,
}

impl QuestionSpec {
    pub fn validate(&self) -> Result<(), QuestionValidationError> {
        if self.origin.session_id.trim().is_empty() {
            return Err(QuestionValidationError("session ID is required".to_owned()));
        }
        if self.questions.is_empty() {
            return Err(QuestionValidationError(
                "at least one question is required".to_owned(),
            ));
        }
        for question in &self.questions {
            question.validate()?;
        }
        if (self.purpose == QuestionPurpose::PlanAuthorization) != self.plan.is_some() {
            return Err(QuestionValidationError(
                "only Plan authorization questions carry a Plan binding".to_owned(),
            ));
        }
        if self.origin.goal_id.is_some()
            && matches!(
                self.purpose,
                QuestionPurpose::RequiredInput | QuestionPurpose::GoalResume
            )
            && self
                .expected_goal_revision
                .is_none_or(|revision| revision < 1)
        {
            return Err(QuestionValidationError(
                "required Goal input needs the current Goal revision".to_owned(),
            ));
        }
        if self.expected_goal_revision.is_some()
            && (!matches!(
                self.purpose,
                QuestionPurpose::RequiredInput | QuestionPurpose::GoalResume
            ) || self.origin.goal_id.is_none())
        {
            return Err(QuestionValidationError(
                "a Goal revision is only valid for Goal-owned input or resume".to_owned(),
            ));
        }
        if self.purpose == QuestionPurpose::GoalResume {
            let valid_choices = self.questions.as_slice().first().is_some_and(|question| {
                !question.allows_custom()
                    && !question.is_multiple()
                    && question
                        .options
                        .iter()
                        .map(|option| option.label.as_str())
                        .eq([
                            crate::goal_resume::RESUME_GOAL_CHOICE,
                            crate::goal_resume::KEEP_GOAL_PAUSED_CHOICE,
                        ])
            });
            if self.origin.goal_id.is_none()
                || self.expected_goal_revision.is_none()
                || self.mode != QuestionMode::Deferred
                || self.questions.len() != 1
                || !valid_choices
            {
                return Err(QuestionValidationError(
                    "Goal resume requires a deferred closed choice bound to a Goal revision"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// The persisted snapshot all frontends render.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionView {
    pub id: String,
    pub origin: QuestionOrigin,
    pub revision: i64,
    pub mode: QuestionMode,
    pub purpose: QuestionPurpose,
    pub state: QuestionState,
    pub questions: Vec<QuestionItem>,
    /// Explicitly submitted answers. Drafts never contribute to completion.
    pub answers: QuestionAnswers,
    /// Unsubmitted form values, including explicit blank slots.
    #[serde(default)]
    pub draft_answers: QuestionAnswers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanQuestionBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PlanQuestionDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<PlanAuthorizationState>,
    pub time_created: i64,
    pub time_updated: i64,
}

impl QuestionView {
    pub fn validate_answers(
        &self,
        answers: &QuestionAnswers,
    ) -> Result<(), QuestionValidationError> {
        if self.purpose == QuestionPurpose::PlanAuthorization {
            return Err(QuestionValidationError(
                "Plan authorization requires an explicit Plan decision".to_owned(),
            ));
        }
        self.validate_item_values(answers)
    }

    /// Validate form data without granting consent or committing any answers.
    ///
    /// Plan choices are valid drafts, but only `PlanDecision` grants authority.
    pub fn validate_draft_answers(
        &self,
        draft_answers: &QuestionAnswers,
    ) -> Result<(), QuestionValidationError> {
        self.validate_item_values(draft_answers)
    }

    fn validate_item_values(
        &self,
        answers: &QuestionAnswers,
    ) -> Result<(), QuestionValidationError> {
        for (id, values) in answers {
            let question = self
                .questions
                .iter()
                .find(|item| item.id == *id)
                .ok_or_else(|| QuestionValidationError(format!("unknown question item `{id}`")))?;
            if !question.question.is_multiple() && values.len() > 1 {
                return Err(QuestionValidationError(format!(
                    "question item `{id}` accepts only one answer"
                )));
            }
            let mut seen = BTreeSet::new();
            for value in values {
                if value.trim().is_empty() || !seen.insert(value) {
                    return Err(QuestionValidationError(format!(
                        "question item `{id}` has an empty or duplicate answer"
                    )));
                }
                if !question.question.allows_custom()
                    && !question
                        .question
                        .options
                        .iter()
                        .any(|option| option.label == *value)
                {
                    return Err(QuestionValidationError(format!(
                        "question item `{id}` does not offer answer `{value}`"
                    )));
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn is_fully_answered(&self) -> bool {
        !self.questions.is_empty()
            && self.questions.iter().all(|item| {
                self.answers
                    .get(&item.id)
                    .is_some_and(|answers| !answers.is_empty())
            })
    }
}

/// A closed, explicit authorization choice, never inferred from missing input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanQuestionDecision {
    Approve,
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanAuthorizationState {
    WaitingForHandoff,
    Applied,
    Invalidated,
}

/// Actions available only to authenticated client adapters, not model tools.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionAction {
    Answer {
        answers: QuestionAnswers,
    },
    Defer {
        /// Merge only these form slots. Empty maps preserve existing drafts;
        /// an empty slot records an unsubmitted blank, even over a prior answer.
        #[serde(default, rename = "draftAnswers")]
        draft_answers: QuestionAnswers,
    },
    Cancel,
    PlanDecision {
        decision: PlanQuestionDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        risk_reason: Option<String>,
    },
}

/// CAS and idempotency apply to every action, including defer and cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionCommand {
    pub command_id: String,
    pub expected_revision: i64,
    pub action: QuestionAction,
}

impl QuestionCommand {
    pub fn validate(&self) -> Result<(), QuestionValidationError> {
        if self.command_id.trim().is_empty() || self.command_id.len() > 256 {
            return Err(QuestionValidationError(
                "command ID must contain 1–256 bytes".to_owned(),
            ));
        }
        if self.expected_revision < 1 {
            return Err(QuestionValidationError(
                "expectedRevision must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Committed result. An input ID proves admission, not provider consumption.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionReceipt {
    pub question: QuestionView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionValidationError(pub String);

impl fmt::Display for QuestionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for QuestionValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> QuestionView {
        QuestionView {
            id: "que_validation".to_owned(),
            origin: QuestionOrigin {
                session_id: "session".to_owned(),
                message_id: None,
                call_id: None,
                turn_id: Some("turn".to_owned()),
                goal_id: None,
            },
            revision: 1,
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::Clarification,
            state: QuestionState::Pending,
            questions: vec![QuestionItem {
                id: "stable-item".to_owned(),
                question: QuestionRequest::closed(
                    "Choose",
                    "Choice",
                    vec![
                        QuestionOption::new("first", ""),
                        QuestionOption::new("second", ""),
                    ],
                ),
            }],
            answers: QuestionAnswers::new(),
            draft_answers: QuestionAnswers::new(),
            plan: None,
            decision: None,
            authorization: None,
            time_created: 1,
            time_updated: 1,
        }
    }

    #[test]
    fn drafts_and_answers_validate_the_same_item_ids_options_and_cardinality() {
        let mut view = view();
        for invalid in [
            QuestionAnswers::from([("unknown".to_owned(), vec!["first".to_owned()])]),
            QuestionAnswers::from([("stable-item".to_owned(), vec!["unknown".to_owned()])]),
            QuestionAnswers::from([("stable-item".to_owned(), vec![" ".to_owned()])]),
            QuestionAnswers::from([(
                "stable-item".to_owned(),
                vec!["first".to_owned(), "second".to_owned()],
            )]),
        ] {
            assert!(view.validate_answers(&invalid).is_err());
            assert!(view.validate_draft_answers(&invalid).is_err());
        }
        view.questions[0].question.multiple = Some(true);
        let duplicate = QuestionAnswers::from([(
            "stable-item".to_owned(),
            vec!["first".to_owned(), "first".to_owned()],
        )]);
        assert!(view.validate_answers(&duplicate).is_err());
        assert!(view.validate_draft_answers(&duplicate).is_err());
        let multiple = QuestionAnswers::from([(
            "stable-item".to_owned(),
            vec!["first".to_owned(), "second".to_owned()],
        )]);
        assert!(view.validate_answers(&multiple).is_ok());
        assert!(view.validate_draft_answers(&multiple).is_ok());
        view.questions[0].question.custom = Some(true);
        let custom =
            QuestionAnswers::from([("stable-item".to_owned(), vec!["typed\nvalue".to_owned()])]);
        assert!(view.validate_answers(&custom).is_ok());
        assert!(view.validate_draft_answers(&custom).is_ok());
        let blank = QuestionAnswers::from([("stable-item".to_owned(), Vec::new())]);
        assert!(view.validate_answers(&blank).is_ok());
        assert!(view.validate_draft_answers(&blank).is_ok());
    }

    #[test]
    fn a_complete_plan_draft_is_neither_an_answer_nor_a_decision() {
        let mut view = view();
        view.purpose = QuestionPurpose::PlanAuthorization;
        view.questions[0].question.options = vec![
            QuestionOption::new("approve", ""),
            QuestionOption::new("decline", ""),
        ];
        view.draft_answers =
            QuestionAnswers::from([("stable-item".to_owned(), vec!["approve".to_owned()])]);
        assert!(view.validate_draft_answers(&view.draft_answers).is_ok());
        assert!(view.validate_answers(&view.draft_answers).is_err());
        assert!(!view.is_fully_answered());
        assert!(view.decision.is_none());
        assert!(view.authorization.is_none());
    }

    #[test]
    fn draft_wire_fields_and_old_views_round_trip_without_inferred_values() {
        let action: QuestionAction = serde_json::from_value(serde_json::json!({
            "type": "defer",
            "draftAnswers": {"stable-item": ["draft"]},
        }))
        .expect("wire action");
        assert_eq!(
            action,
            QuestionAction::Defer {
                draft_answers: QuestionAnswers::from([(
                    "stable-item".to_owned(),
                    vec!["draft".to_owned()]
                )]),
            }
        );
        let mut view = view();
        view.purpose = QuestionPurpose::PlanAuthorization;
        view.plan = Some(PlanQuestionBinding {
            plan_id: "plan".to_owned(),
            plan_revision: 4,
            source_cycle_id: None,
            title: "Plan".to_owned(),
            completed_steps: 0,
            total_steps: 1,
            work_identity: TurnExecutionIdentity::new("build", "provider", "model"),
            review_gate: serde_json::json!({"status": "unbound"}),
        });
        let mut encoded = serde_json::to_value(&view).expect("view");
        assert_eq!(encoded["draftAnswers"], serde_json::json!({}));
        assert!(encoded["plan"].get("sourceCycleId").is_none());
        encoded
            .as_object_mut()
            .expect("object")
            .remove("draftAnswers");
        assert_eq!(
            serde_json::from_value::<QuestionView>(encoded).expect("old view"),
            view
        );
        view.plan.as_mut().expect("binding").source_cycle_id = Some("logical-cycle".to_owned());
        let encoded = serde_json::to_value(&view).expect("cycle binding");
        assert_eq!(encoded["plan"]["sourceCycleId"], "logical-cycle");
        assert_eq!(
            serde_json::from_value::<QuestionView>(encoded).expect("new view"),
            view
        );
    }

    #[test]
    fn closed_questions_reject_missing_or_duplicate_options() {
        assert!(
            QuestionRequest::closed("Proceed?", "Plan", Vec::new())
                .validate()
                .is_err()
        );
        assert!(
            QuestionRequest::closed(
                "Proceed?",
                "Plan",
                vec![
                    QuestionOption::new("Yes", ""),
                    QuestionOption::new("Yes", "")
                ],
            )
            .validate()
            .is_err()
        );
    }

    #[test]
    fn commands_distinguish_defer_from_an_explicit_plan_decision() {
        let defer: QuestionCommand = serde_json::from_value(serde_json::json!({
            "commandId": "client-1",
            "expectedRevision": 1,
            "action": {"type": "defer"}
        }))
        .expect("defer");
        assert_eq!(
            defer.action,
            QuestionAction::Defer {
                draft_answers: QuestionAnswers::new(),
            }
        );
        assert!(
            serde_json::from_value::<QuestionCommand>(serde_json::json!({
                "commandId": "client-1",
                "expectedRevision": 1,
                "action": {"type": "plan_decision"}
            }))
            .is_err(),
            "an absent decision must never default to approval"
        );
    }
}
