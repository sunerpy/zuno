use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use zuno_error::ToolError;
use zuno_tools::question::{Answer, QuestionAsker, QuestionOutcome, QuestionRequest};

use crate::ClientConnection;

const QUESTION_TOOL: &str = "question";

/// ACP-backed structured question delivery.
///
/// The caller must only install this asker after the client advertises
/// `clientCapabilities.elicitation.form`.
#[derive(Debug, Clone)]
pub struct AcpQuestionAsker {
    client: ClientConnection,
    route: Option<std::sync::Arc<crate::AcpSessionRoute>>,
}

impl AcpQuestionAsker {
    #[must_use]
    pub fn new(client: ClientConnection) -> Self {
        Self {
            client,
            route: None,
        }
    }

    #[must_use]
    pub fn with_route(
        client: ClientConnection,
        route: std::sync::Arc<crate::AcpSessionRoute>,
    ) -> Self {
        Self {
            client,
            route: Some(route),
        }
    }
}

#[async_trait]
impl QuestionAsker for AcpQuestionAsker {
    async fn ask(
        &self,
        session_id: &str,
        questions: &[QuestionRequest],
        call: Option<(&str, &str)>,
    ) -> Result<QuestionOutcome, ToolError> {
        let routed = self.route.as_ref().map_or_else(
            || crate::RoutedSession::direct(session_id),
            |route| route.resolve(session_id),
        );
        let mut request = elicitation_request(routed.wire_session_id(), questions, call)?;
        if let Some(child_session_id) = routed.child_session_id() {
            request["_meta"] = json!({
                "zuno": {
                    "childSessionId": child_session_id,
                },
            });
        }
        let response = match self.client.request("elicitation/create", request).await {
            Ok(response) => response,
            Err(_) => return Ok(QuestionOutcome::Failed),
        };
        Ok(elicitation_outcome(&response, questions))
    }
}

fn elicitation_request(
    session_id: &str,
    questions: &[QuestionRequest],
    call: Option<(&str, &str)>,
) -> Result<Value, ToolError> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        for field in property_schemas(index, question)? {
            if field.required {
                required.push(field.name.clone());
            }
            properties.insert(field.name, field.schema);
        }
    }
    let mut requested_schema = json!({
        "type": "object",
        "title": "Questions",
        "description": "Answer each question in order.",
        "properties": properties,
    });
    if !required.is_empty() {
        requested_schema["required"] = json!(required);
    }
    let mut request = json!({
        "mode": "form",
        "message": request_message(questions),
        "sessionId": session_id,
        "requestedSchema": requested_schema,
    });
    if let Some((_, tool_call_id)) = call {
        request["toolCallId"] = Value::String(tool_call_id.to_owned());
    }
    Ok(request)
}

fn request_message(questions: &[QuestionRequest]) -> String {
    match questions {
        [question] => question.question.clone(),
        _ => format!("Please answer {} questions.", questions.len()),
    }
}

struct FormField {
    name: String,
    schema: Value,
    required: bool,
}

fn property_schemas(index: usize, question: &QuestionRequest) -> Result<Vec<FormField>, ToolError> {
    let base_name = format!("q{index}");
    let custom = allows_custom(question);
    if question.options.is_empty() {
        if !custom {
            return Err(invalid_request(
                "a question with custom answers disabled must offer at least one option",
            ));
        }
        return Ok(vec![FormField {
            name: base_name,
            schema: custom_schema(question, false),
            required: true,
        }]);
    }

    validate_options(question)?;
    let choice = choice_schema(question);
    if custom {
        Ok(vec![
            FormField {
                name: format!("{base_name}_choice"),
                schema: choice,
                required: false,
            },
            FormField {
                name: format!("{base_name}_custom"),
                schema: custom_schema(question, true),
                required: false,
            },
        ])
    } else {
        Ok(vec![FormField {
            name: base_name,
            schema: choice,
            required: true,
        }])
    }
}

fn custom_schema(question: &QuestionRequest, alongside_choices: bool) -> Value {
    let title = if alongside_choices {
        format!("{} — Other", question.header)
    } else {
        question.header.clone()
    };
    let description = if alongside_choices {
        format!(
            "{}\n\nType a custom answer instead of the listed choices.",
            question.question
        )
    } else {
        question.question.clone()
    };
    json!({
            "type": "string",
            "title": title,
            "description": description,
            "minLength": 1,
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
    if is_multiple(question) {
        json!({
            "type": "array",
            "title": question.header,
            "description": question.question,
            "minItems": 1,
            "items": {
                "anyOf": choices,
            },
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

fn validate_options(question: &QuestionRequest) -> Result<(), ToolError> {
    let mut labels = HashSet::with_capacity(question.options.len());
    for option in &question.options {
        if !labels.insert(option.label.as_str()) {
            return Err(invalid_request(format!(
                "question options must have unique labels; duplicate `{}`",
                option.label
            )));
        }
    }
    Ok(())
}

fn invalid_request(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: QUESTION_TOOL.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message.into(),
        )),
    }
}

fn elicitation_outcome(response: &Value, questions: &[QuestionRequest]) -> QuestionOutcome {
    match response.get("action").and_then(Value::as_str) {
        Some("accept") => accepted_answers(response, questions)
            .map(QuestionOutcome::Answered)
            .unwrap_or(QuestionOutcome::Failed),
        // ACP distinguishes a deliberate decline from cancellation, while Zuno's
        // durable question state has one human-withdrawal terminal. Neither is a
        // transport failure and neither may reopen the question.
        Some("decline" | "cancel") => QuestionOutcome::Cancelled,
        _ => QuestionOutcome::Failed,
    }
}

fn accepted_answers(response: &Value, questions: &[QuestionRequest]) -> Option<Vec<Answer>> {
    let content = response.get("content")?.as_object()?;
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| accepted_answer(index, question, content))
        .collect()
}

fn accepted_answer(
    index: usize,
    question: &QuestionRequest,
    content: &Map<String, Value>,
) -> Option<Answer> {
    let base_name = format!("q{index}");
    if question.options.is_empty() {
        let answer = content.get(&base_name)?.as_str()?;
        return (!answer.is_empty()).then(|| vec![answer.to_owned()]);
    }

    if allows_custom(question) {
        if let Some(custom) = content.get(&format!("{base_name}_custom")) {
            let custom = custom.as_str()?;
            if !custom.is_empty() {
                return Some(vec![custom.to_owned()]);
            }
        }
        return content.get(&format!("{base_name}_choice")).map_or_else(
            || Some(Vec::new()),
            |value| accepted_choice(question, value, true),
        );
    }

    accepted_choice(question, content.get(&base_name)?, false)
}

fn accepted_choice(question: &QuestionRequest, value: &Value, optional: bool) -> Option<Answer> {
    if is_multiple(question) {
        let values = value.as_array()?;
        if values.is_empty() {
            return optional.then(Vec::new);
        }
        values
            .iter()
            .map(|value| {
                let value = value.as_str()?;
                is_offered(question, value).then(|| value.to_owned())
            })
            .collect()
    } else {
        let value = value.as_str()?;
        is_offered(question, value).then(|| vec![value.to_owned()])
    }
}

fn is_multiple(question: &QuestionRequest) -> bool {
    question.multiple.unwrap_or(false)
}

fn allows_custom(question: &QuestionRequest) -> bool {
    question.custom.unwrap_or(true)
}

fn is_offered(question: &QuestionRequest, value: &str) -> bool {
    question.options.iter().any(|option| option.label == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_tools::question::QuestionOption;

    fn option(label: &str, description: &str) -> QuestionOption {
        QuestionOption::new(label, description)
    }

    fn strict_single() -> QuestionRequest {
        QuestionRequest {
            question: "Choose a release channel.".to_owned(),
            header: "Channel".to_owned(),
            options: vec![
                option("Stable", "Use the production channel."),
                option("Preview", "Use the preview channel."),
            ],
            multiple: None,
            custom: Some(false),
        }
    }

    fn strict_multiple() -> QuestionRequest {
        QuestionRequest {
            question: "Choose target platforms.".to_owned(),
            header: "Platforms".to_owned(),
            options: vec![
                option("Linux", "Build the Linux target."),
                option("Windows", "Build the Windows target."),
            ],
            multiple: Some(true),
            custom: Some(false),
        }
    }

    #[test]
    fn request_uses_stable_form_schema_and_tool_scope() {
        let request = elicitation_request(
            "ses-1",
            &[strict_single(), strict_multiple()],
            Some(("msg-1", "call-1")),
        )
        .expect("valid form request");

        assert_eq!(request["mode"], "form");
        assert_eq!(request["sessionId"], "ses-1");
        assert_eq!(request["toolCallId"], "call-1");
        assert_eq!(request["requestedSchema"]["type"], "object");
        assert_eq!(request["requestedSchema"]["required"], json!(["q0", "q1"]));
        assert_eq!(
            request["requestedSchema"]["properties"]["q0"],
            json!({
                "type": "string",
                "title": "Channel",
                "description": "Choose a release channel.",
                "oneOf": [
                    {
                        "const": "Stable",
                        "title": "Stable",
                        "description": "Use the production channel.",
                    },
                    {
                        "const": "Preview",
                        "title": "Preview",
                        "description": "Use the preview channel.",
                    },
                ],
            })
        );
        assert_eq!(
            request["requestedSchema"]["properties"]["q1"],
            json!({
                "type": "array",
                "title": "Platforms",
                "description": "Choose target platforms.",
                "minItems": 1,
                "items": {
                    "anyOf": [
                        {
                            "const": "Linux",
                            "title": "Linux",
                            "description": "Build the Linux target.",
                        },
                        {
                            "const": "Windows",
                            "title": "Windows",
                            "description": "Build the Windows target.",
                        },
                    ],
                },
            })
        );
    }

    #[test]
    fn custom_answers_keep_native_choices_and_add_an_other_field() {
        let mut question = strict_multiple();
        question.custom = None;
        let request =
            elicitation_request("ses-1", &[question], None).expect("valid custom request");
        let schema = &request["requestedSchema"];
        assert!(schema.get("required").is_none());
        let properties = schema["properties"].as_object().expect("form properties");
        assert_eq!(properties.len(), 2);

        let choices = &properties["q0_choice"];
        assert_eq!(choices["type"], "array");
        assert_eq!(choices["title"], "Platforms");
        assert_eq!(choices["minItems"], 1);
        assert_eq!(choices["items"]["anyOf"][0]["const"], "Linux");
        assert_eq!(choices["items"]["anyOf"][1]["const"], "Windows");

        let custom = &properties["q0_custom"];
        assert_eq!(custom["type"], "string");
        assert_eq!(custom["title"], "Platforms — Other");
        assert_eq!(custom["minLength"], 1);
        assert_eq!(
            custom["description"],
            "Choose target platforms.\n\nType a custom answer instead of the listed choices."
        );
        assert!(request.get("toolCallId").is_none());
    }

    #[test]
    fn parses_single_select_answer_positionally() {
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": "Preview" },
                }),
                &[strict_single()],
            ),
            QuestionOutcome::Answered(vec![vec!["Preview".to_owned()]])
        );
    }

    #[test]
    fn parses_multi_select_answer_positionally() {
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": ["Linux", "Windows"] },
                }),
                &[strict_multiple()],
            ),
            QuestionOutcome::Answered(vec![vec!["Linux".to_owned(), "Windows".to_owned(),]])
        );
    }

    #[test]
    fn custom_question_accepts_a_native_choice() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0_choice": "Preview" },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Preview".to_owned()]])
        );
    }

    #[test]
    fn custom_multi_select_question_accepts_native_choices() {
        let mut question = strict_multiple();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0_choice": ["Linux", "Windows"] },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Linux".to_owned(), "Windows".to_owned()]])
        );
    }

    #[test]
    fn custom_answer_takes_precedence_over_a_native_choice() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": {
                        "q0_choice": "Preview",
                        "q0_custom": "Nightly",
                    },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Nightly".to_owned()]])
        );
    }

    #[test]
    fn free_text_only_question_uses_one_required_field() {
        let question = QuestionRequest {
            question: "Name the release.".to_owned(),
            header: "Release".to_owned(),
            options: Vec::new(),
            multiple: None,
            custom: None,
        };
        let request =
            elicitation_request("ses-1", std::slice::from_ref(&question), None).expect("request");
        let schema = &request["requestedSchema"];
        assert_eq!(schema["required"], json!(["q0"]));
        assert_eq!(schema["properties"]["q0"]["type"], "string");
        assert_eq!(schema["properties"]["q0"]["title"], "Release");
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": { "q0": "Canary" },
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![vec!["Canary".to_owned()]])
        );
    }

    #[test]
    fn custom_question_may_be_submitted_unanswered() {
        let mut question = strict_single();
        question.custom = None;
        assert_eq!(
            elicitation_outcome(
                &json!({
                    "action": "accept",
                    "content": {},
                }),
                &[question],
            ),
            QuestionOutcome::Answered(vec![Vec::new()])
        );
    }

    #[test]
    fn decline_and_cancel_are_terminal_cancellations() {
        for action in ["decline", "cancel"] {
            assert_eq!(
                elicitation_outcome(&json!({ "action": action }), &[strict_single()]),
                QuestionOutcome::Cancelled
            );
        }
    }

    #[test]
    fn malformed_and_unknown_responses_fail_closed() {
        for response in [
            json!({}),
            json!({ "action": "future-action" }),
            json!({ "action": "accept" }),
            json!({ "action": "accept", "content": null }),
            json!({ "action": "accept", "content": {} }),
            json!({ "action": "accept", "content": { "q0": 7 } }),
            json!({ "action": "accept", "content": { "q0": "Nightly" } }),
        ] {
            assert_eq!(
                elicitation_outcome(&response, &[strict_single()]),
                QuestionOutcome::Failed,
                "response should fail closed: {response}"
            );
        }
    }

    #[test]
    fn duplicate_strict_labels_are_rejected_before_delivery() {
        let mut question = strict_single();
        question.options.push(option("Stable", "Duplicate label."));
        let error = elicitation_request("ses-1", &[question], None)
            .expect_err("duplicate enum labels make oneOf ambiguous");
        assert_eq!(error.tool(), QUESTION_TOOL);
    }

    #[test]
    fn duplicate_custom_labels_are_rejected_before_delivery() {
        let mut question = strict_single();
        question.custom = None;
        question.options.push(option("Stable", "Duplicate label."));
        let error = elicitation_request("ses-1", &[question], None)
            .expect_err("duplicate enum labels make oneOf ambiguous");
        assert_eq!(error.tool(), QUESTION_TOOL);
    }
}
