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
}

impl AcpQuestionAsker {
    #[must_use]
    pub fn new(client: ClientConnection) -> Self {
        Self { client }
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
        let request = elicitation_request(session_id, questions, call)?;
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
    let properties = questions
        .iter()
        .enumerate()
        .map(|(index, question)| Ok((format!("q{index}"), property_schema(question)?)))
        .collect::<Result<Map<String, Value>, ToolError>>()?;
    let required = (0..questions.len())
        .map(|index| format!("q{index}"))
        .collect::<Vec<_>>();
    let mut request = json!({
        "mode": "form",
        "message": request_message(questions),
        "sessionId": session_id,
        "requestedSchema": {
            "type": "object",
            "title": "Questions",
            "description": "Answer each question in order.",
            "properties": properties,
            "required": required,
        },
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

fn property_schema(question: &QuestionRequest) -> Result<Value, ToolError> {
    if allows_custom(question) {
        return Ok(json!({
            "type": "string",
            "title": question.header,
            "description": custom_description(question),
            "minLength": 1,
        }));
    }

    validate_strict_options(question)?;
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
        Ok(json!({
            "type": "array",
            "title": question.header,
            "description": question.question,
            "minItems": 1,
            "items": {
                "anyOf": choices,
            },
        }))
    } else {
        Ok(json!({
            "type": "string",
            "title": question.header,
            "description": question.question,
            "oneOf": choices,
        }))
    }
}

fn custom_description(question: &QuestionRequest) -> String {
    let mut description = question.question.clone();
    if !question.options.is_empty() {
        description.push_str("\n\nSuggested choices:");
        for option in &question.options {
            description.push_str("\n- ");
            description.push_str(&option.label);
            if !option.description.is_empty() {
                description.push_str(": ");
                description.push_str(&option.description);
            }
        }
    }
    if is_multiple(question) {
        description.push_str(
            "\n\nEnter the answer as free text. ACP v1.21 cannot combine a native multi-select \
             enum with arbitrary custom entries in one field, so the complete text is returned \
             as one answer.",
        );
    } else {
        description.push_str("\n\nYou may enter a suggested choice or any custom answer.");
    }
    description
}

fn validate_strict_options(question: &QuestionRequest) -> Result<(), ToolError> {
    if question.options.is_empty() {
        return Err(invalid_request(
            "a question with custom answers disabled must offer at least one option",
        ));
    }
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
        .map(|(index, question)| {
            let value = content.get(&format!("q{index}"))?;
            accepted_answer(question, value)
        })
        .collect()
}

fn accepted_answer(question: &QuestionRequest, value: &Value) -> Option<Answer> {
    if allows_custom(question) {
        let answer = value.as_str()?;
        return (!answer.is_empty()).then(|| vec![answer.to_owned()]);
    }

    if is_multiple(question) {
        let values = value.as_array()?;
        if values.is_empty() {
            return None;
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
    fn custom_answers_use_the_conservative_free_text_mapping() {
        let mut question = strict_multiple();
        question.custom = None;
        let request =
            elicitation_request("ses-1", &[question], None).expect("valid custom request");
        let property = &request["requestedSchema"]["properties"]["q0"];

        assert_eq!(property["type"], "string");
        assert_eq!(property["minLength"], 1);
        assert!(property.get("items").is_none());
        assert!(property.get("oneOf").is_none());
        let description = property["description"].as_str().expect("description");
        assert!(description.contains("Linux"));
        assert!(description.contains("complete text is returned as one answer"));
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
}
