//! Native ACP presentation of questions already persisted by a [`QuestionPort`].
//!
//! This adapter owns one client interaction, not the question's lifetime. The
//! session host supervises the presentation task and decides when to reopen a
//! pending question. Publication, persistence, Goal wakeups and Plan handoff remain
//! behind the port.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_types::question::{
    PlanQuestionDecision, QuestionAction, QuestionAnswers, QuestionCommand, QuestionItem,
    QuestionPurpose, QuestionReceipt, QuestionRequest, QuestionView,
};

use crate::{AcpSessionRoute, ClientConnection, RoutedSession};

const PLAN_DECISION: &str = "planDecision";
const RISK_REASON: &str = "riskReason";

/// Presents one durable question through native `elicitation/create`.
///
/// Install only after the client advertises `clientCapabilities.elicitation.form`.
/// The connection is detached from the originating prompt's RPC ownership; the
/// caller must supervise this task for the session and drop it when that session
/// closes. A transport disconnect ends presentation without changing the question.
#[derive(Clone)]
pub struct AcpQuestionPresenter {
    port: Arc<dyn QuestionPort>,
    client: ClientConnection,
    route: Option<Arc<AcpSessionRoute>>,
}

impl AcpQuestionPresenter {
    #[must_use]
    pub fn new(port: Arc<dyn QuestionPort>, client: ClientConnection) -> Self {
        Self {
            port,
            client: client.session_scoped(),
            route: None,
        }
    }

    /// Route a child's form through the session the client knows, while commands
    /// continue to target the question's original durable session.
    #[must_use]
    pub fn with_route(mut self, route: Arc<AcpSessionRoute>) -> Self {
        self.route = Some(route);
        self
    }

    /// Present an already-persisted snapshot and apply at most one client command.
    ///
    /// `Ok(None)` means the snapshot was terminal or the RPC produced no form answer,
    /// including disconnect and transport cancellation. No command is applied in
    /// those cases. Native Cancel and an empty accepted form apply `Defer`; native
    /// Decline applies `Cancel`. Deferral stores supplied fields only as drafts;
    /// omitted fields preserve their previous values. Empty acceptance never
    /// confirms prefilled drafts or finalizes a question.
    ///
    /// Plan authorization uses only the explicit `planDecision` field (`approve`
    /// or `decline`) in an accepted form, with optional `riskReason`. It has no
    /// default. Generic question labels never grant Plan authorization.
    ///
    /// Commands carry the displayed revision and an ID derived from the durable
    /// session, question, revision and action. Replaying the same response to the
    /// same snapshot therefore asks the port for the same idempotent receipt.
    ///
    /// # Errors
    ///
    /// Invalid snapshots/responses and port failures are returned as
    /// [`QuestionError`]. CAS conflicts are not retried against a newer revision.
    pub async fn present(&self, view: QuestionView) -> QuestionResult<Option<QuestionReceipt>> {
        if view.state.is_terminal() {
            return Ok(None);
        }
        validate_view(&view)?;
        let properties = form_properties(&view);
        let fields = properties.keys().cloned().collect::<BTreeSet<_>>();
        let routed = self.route.as_ref().map_or_else(
            || RoutedSession::direct(&view.origin.session_id),
            |route| route.resolve(&view.origin.session_id),
        );
        let mut metadata = json!({
            "questionId": view.id,
            "questionRevision": view.revision,
            "questionPurpose": view.purpose,
        });
        if let Some(child) = routed.child_session_id() {
            metadata["childSessionId"] = json!(child);
        }
        let request = json!({
            "mode": "form",
            "sessionId": routed.wire_session_id(),
            "message": form_message(&view),
            "requestedSchema": {
                "type": "object",
                "title": if view.purpose == QuestionPurpose::PlanAuthorization {
                    "Plan approval"
                } else {
                    "Questions"
                },
                "description": if view.purpose == QuestionPurpose::PlanAuthorization {
                    "Choose approve or decline explicitly. Leave the decision blank or cancel to answer later."
                } else {
                    "Answer any fields now. Leave fields blank to keep existing answers. Cancel to answer later; decline to refuse."
                },
                "properties": properties,
            },
            "_meta": { "zuno": metadata },
        });
        // A durable question may outlive its tool call. Deliberately omit the
        // native toolCallId association; its identity is the stored question ID.
        let Ok(response) = self.client.request("elicitation/create", request).await else {
            return Ok(None);
        };
        let action = response_action(&view, &fields, &response)?;
        let command = response_command(&view, action)?;
        self.port
            .apply(&view.origin.session_id, &view.id, command)
            .await
            .map(Some)
    }
}

fn validate_view(view: &QuestionView) -> QuestionResult<()> {
    if view.id.trim().is_empty() || view.origin.session_id.trim().is_empty() || view.revision < 1 {
        return Err(invalid(
            "a persisted question requires an ID, session and positive revision",
        ));
    }
    if view.questions.is_empty() {
        return Err(invalid(
            "a persisted question must contain at least one item",
        ));
    }
    let mut ids = BTreeSet::new();
    for item in &view.questions {
        if item.id.trim().is_empty() || !ids.insert(&item.id) {
            return Err(invalid("question item IDs must be non-empty and unique"));
        }
        item.question.validate()?;
    }
    if (view.purpose == QuestionPurpose::PlanAuthorization) != view.plan.is_some() {
        return Err(invalid(
            "only Plan authorization questions carry a Plan binding",
        ));
    }
    if view.purpose != QuestionPurpose::PlanAuthorization {
        view.validate_answers(&view.answers)?;
    }
    view.validate_draft_answers(&view.draft_answers)?;
    Ok(())
}

fn form_message(view: &QuestionView) -> String {
    let mut message = view
        .questions
        .iter()
        .map(|item| item.question.question.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if let Some(plan) = &view.plan {
        message.push_str(&format!(
            "\n\nPlan: {} (revision {})",
            plan.title, plan.plan_revision
        ));
    }
    message
}

fn form_properties(view: &QuestionView) -> Map<String, Value> {
    if view.purpose == QuestionPurpose::PlanAuthorization {
        return Map::from_iter([
            (
                PLAN_DECISION.to_owned(),
                json!({
                    "type": "string",
                    "title": "Plan decision",
                    "description": "Choose explicitly to approve or decline this plan.",
                    "oneOf": [
                        { "const": "approve", "title": "Approve plan" },
                        { "const": "decline", "title": "Decline plan" },
                    ],
                }),
            ),
            (
                RISK_REASON.to_owned(),
                json!({
                    "type": "string",
                    "title": "Approval reason",
                    "description": "Optional reason for accepting the plan's review risk.",
                }),
            ),
        ]);
    }
    let mut fields = Map::new();
    for item in &view.questions {
        let question = &item.question;
        let stored = prefilled_answer(view, &item.id);
        if question.options.is_empty() {
            fields.insert(field("answer", item), text_schema(question, stored, false));
            continue;
        }
        let mut schema = choice_schema(question);
        schema
            .as_object_mut()
            .expect("choice schema")
            .remove("minItems");
        let selected = stored
            .iter()
            .filter(|value| offered(question, value))
            .cloned()
            .collect::<Vec<_>>();
        if !selected.is_empty() {
            schema["default"] = if question.is_multiple() {
                json!(selected)
            } else {
                json!(selected[0])
            };
        }
        if question.allows_custom() {
            fields.insert(field("choice", item), schema);
            let custom = custom_answers(question, stored);
            fields.insert(field("custom", item), text_schema(question, &custom, true));
        } else {
            fields.insert(field("answer", item), schema);
        }
    }
    fields
}

fn text_schema(question: &QuestionRequest, stored: &[String], alongside: bool) -> Value {
    let mut schema = custom_schema(question, alongside);
    schema
        .as_object_mut()
        .expect("custom schema")
        .remove("minLength");
    if alongside && question.is_multiple() {
        schema["description"] = json!(format!(
            "{}\n\nAdd a custom answer to the selected choices.",
            question.question
        ));
    }
    if !stored.is_empty() {
        schema["default"] = json!(stored.join("\n"));
    }
    schema
}

fn custom_schema(question: &QuestionRequest, alongside_choices: bool) -> Value {
    json!({
        "type": "string",
        "title": if alongside_choices {
            format!("{} — Other", question.header)
        } else {
            question.header.clone()
        },
        "description": if alongside_choices {
            format!("{}\n\nType a custom answer instead of the listed choices.", question.question)
        } else {
            question.question.clone()
        },
    })
}

fn choice_schema(question: &QuestionRequest) -> Value {
    let choices = question
        .options
        .iter()
        .map(|option| {
            json!({
                "const": option.label,
                "title": option.label,
                "description": option.description,
            })
        })
        .collect::<Vec<_>>();
    if question.is_multiple() {
        json!({
            "type": "array",
            "title": question.header,
            "description": question.question,
            "items": { "anyOf": choices },
        })
    } else {
        json!({
            "type": "string",
            "title": question.header,
            "description": question.question,
            "oneOf": choices,
        })
    }
}

/// Prefixes keep separate native fields collision-free even for IDs containing
/// punctuation or suffixes such as `_custom`. Item order never enters the name.
fn field(kind: &str, item: &QuestionItem) -> String {
    format!("{kind}:{}", item.id)
}

fn response_action(
    view: &QuestionView,
    fields: &BTreeSet<String>,
    response: &Value,
) -> QuestionResult<QuestionAction> {
    if response.is_null() {
        return Ok(deferred(QuestionAnswers::new()));
    }
    let defer_requested = match response.get("action").and_then(Value::as_str) {
        Some("cancel") => true,
        Some("decline") => return Ok(QuestionAction::Cancel),
        Some("accept") => false,
        _ => {
            return Err(invalid(
                "elicitation response requires accept, decline or cancel",
            ));
        }
    };
    let content = match response.get("content") {
        None | Some(Value::Null) => return Ok(deferred(QuestionAnswers::new())),
        Some(Value::Object(content)) => content,
        _ => return Err(invalid("elicitation content must be an object")),
    };
    if content.keys().any(|key| !fields.contains(key)) {
        return Err(invalid(
            "elicitation content contains an unknown question field",
        ));
    }
    if view.purpose == QuestionPurpose::PlanAuthorization {
        let decision_text = text(content.get(PLAN_DECISION))?;
        if defer_requested || decision_text.is_none() {
            let mut drafts = QuestionAnswers::new();
            if content.contains_key(PLAN_DECISION) {
                let item = view.questions.first().expect("validated Plan question");
                drafts.insert(
                    item.id.clone(),
                    decision_text
                        .map(|value| vec![value.to_owned()])
                        .unwrap_or_default(),
                );
            }
            view.validate_draft_answers(&drafts)?;
            return Ok(deferred(drafts));
        }
        let decision = match decision_text {
            Some("approve") => PlanQuestionDecision::Approve,
            Some("decline") => PlanQuestionDecision::Decline,
            _ => return Err(invalid("planDecision must be approve or decline")),
        };
        return Ok(QuestionAction::PlanDecision {
            decision,
            risk_reason: text(content.get(RISK_REASON))?.map(str::to_owned),
        });
    }
    let mut answers = QuestionAnswers::new();
    for item in &view.questions {
        let question = &item.question;
        let present = if question.options.is_empty() || !question.allows_custom() {
            content.contains_key(&field("answer", item))
        } else {
            content.contains_key(&field("choice", item))
                || content.contains_key(&field("custom", item))
        };
        if !present {
            continue;
        }
        let answer = if question.options.is_empty() {
            accepted_custom(view, item, content.get(&field("answer", item)))?
        } else if question.allows_custom() {
            let mut choices = accepted_choice(question, content.get(&field("choice", item)))?;
            let custom = accepted_custom(view, item, content.get(&field("custom", item)))?;
            if question.is_multiple() {
                choices.extend(custom);
            } else if !custom.is_empty() {
                choices = custom;
            }
            choices
        } else {
            accepted_choice(question, content.get(&field("answer", item)))?
        };
        answers.insert(item.id.clone(), answer);
    }
    if defer_requested || answers.values().all(Vec::is_empty) {
        view.validate_draft_answers(&answers)?;
        Ok(deferred(answers))
    } else {
        view.validate_answers(&answers)?;
        Ok(QuestionAction::Answer { answers })
    }
}

fn deferred(draft_answers: QuestionAnswers) -> QuestionAction {
    QuestionAction::Defer { draft_answers }
}

fn prefilled_answer<'a>(view: &'a QuestionView, item_id: &str) -> &'a [String] {
    view.draft_answers
        .get(item_id)
        .or_else(|| view.answers.get(item_id))
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn accepted_custom(
    view: &QuestionView,
    item: &QuestionItem,
    value: Option<&Value>,
) -> QuestionResult<Vec<String>> {
    let Some(value) = text(value)? else {
        return Ok(Vec::new());
    };
    let stored = prefilled_answer(view, &item.id);
    let custom = custom_answers(&item.question, stored);
    // A native free-text field is scalar. Preserve a multi-answer prefill exactly
    // when it was left unchanged, including answers that themselves have newlines.
    if !custom.is_empty() && value == custom.join("\n") {
        return Ok(custom);
    }
    Ok(vec![value.to_owned()])
}

fn accepted_choice(
    question: &QuestionRequest,
    value: Option<&Value>,
) -> QuestionResult<Vec<String>> {
    let values = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(value)) if value.trim().is_empty() => return Ok(Vec::new()),
        Some(Value::String(value)) if !question.is_multiple() => vec![value.as_str()],
        Some(Value::Array(values)) if question.is_multiple() => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| invalid("choice values must be strings"))
            })
            .collect::<QuestionResult<Vec<_>>>()?,
        _ => return Err(invalid("question choice has the wrong value type")),
    };
    values
        .into_iter()
        .map(|value| {
            offered(question, value)
                .then(|| value.to_owned())
                .ok_or_else(|| invalid("question choice was not offered"))
        })
        .collect()
}

fn text(value: Option<&Value>) -> QuestionResult<Option<&str>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(invalid("question text fields must contain strings")),
    }
}

fn custom_answers(question: &QuestionRequest, stored: &[String]) -> Vec<String> {
    stored
        .iter()
        .filter(|value| !offered(question, value))
        .cloned()
        .collect()
}

fn offered(question: &QuestionRequest, value: &str) -> bool {
    question.options.iter().any(|option| option.label == value)
}

fn response_command(
    view: &QuestionView,
    action: QuestionAction,
) -> QuestionResult<QuestionCommand> {
    let encoded = serde_json::to_vec(&(&view.origin.session_id, &view.id, view.revision, &action))
        .map_err(|error| invalid(format!("question command cannot be encoded: {error}")))?;
    let command = QuestionCommand {
        command_id: format!("acp-question:{}", hex::encode(Sha256::digest(encoded))),
        expected_revision: view.revision,
        action,
    };
    command.validate()?;
    Ok(command)
}

fn invalid(message: impl Into<String>) -> QuestionError {
    QuestionError::Invalid(message.into())
}
