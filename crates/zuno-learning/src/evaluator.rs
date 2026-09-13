use crate::{LearningModelClient, model::invalid};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use zuno_eval::{CaseObservation, CassetteDispatcher, OfflineCaseEvaluator, OfflineCaseRequest};
use zuno_llm::{
    event::{Message, RequestContentBlock, Role},
    registry::ToolSchema,
};

#[derive(Clone)]
pub struct ProviderSkillEvaluator {
    pub client: LearningModelClient,
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillGrade {
    pub score: u8,
    pub passed: bool,
    pub critical_failure: bool,
    pub explanation: String,
}
pub fn decode_skill_grade(text: &str) -> crate::Result<SkillGrade> {
    let grade: SkillGrade = serde_json::from_str(crate::model::strip_json_fence(text))
        .map_err(|error| invalid(&format!("invalid persisted Skill grade: {error}")))?;
    if grade.score > 100 {
        return Err(invalid("evaluation grade exceeds 100"));
    }
    Ok(grade)
}

#[async_trait]
impl OfflineCaseEvaluator for ProviderSkillEvaluator {
    async fn evaluate(
        &self,
        request: OfflineCaseRequest,
    ) -> Result<CaseObservation, zuno_error::BoxSource> {
        tokio::time::timeout(
            Duration::from_millis(self.client.limits.execution_timeout_ms),
            self.run(request),
        )
        .await
        .map_err(|_| {
            Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "offline evaluation reached its total deadline",
            )) as zuno_error::BoxSource
        })?
        .map_err(|error| Box::new(error) as zuno_error::BoxSource)
    }
}

impl ProviderSkillEvaluator {
    async fn run(&self, request: OfflineCaseRequest) -> crate::Result<CaseObservation> {
        let expected_model = format!(
            "{}/{}",
            self.client.model.provider_id, self.client.model.model_id
        );
        if request.attempt.model != expected_model
            || request.attempt.max_output_tokens != self.client.limits.execution_max_output_tokens
            || request.attempt.max_steps != self.client.limits.execution_max_steps
        {
            return Err(invalid(
                "offline evaluation differs from its immutable model/budget snapshot",
            ));
        }
        let mut dispatcher = CassetteDispatcher::from_value(&request.tool_cassette)
            .map_err(|detail| invalid(&detail))?;
        let tools = recorded_tool_schemas(dispatcher.calls());
        let mut messages = vec![
            Message::new(Role::System, attempt_prompt(&request.skill_content)),
            Message::new(Role::User, &request.prompt),
        ];
        let mut trace = Vec::new();
        let mut final_answer = None;
        let mut unmatched_calls = 0;
        for step in 0..request.attempt.max_steps {
            let output = self
                .client
                .complete(
                    &self.session_id,
                    "learning.evaluation.attempt",
                    messages.clone(),
                    tools.clone(),
                    None,
                )
                .await?;
            if output.tool_calls().is_empty() {
                final_answer = Some(output.text().to_owned());
                trace.push(json!({"step":step,"answer":output.text()}));
                break;
            }
            let mut assistant = Vec::new();
            if !output.text().is_empty() {
                assistant.push(RequestContentBlock::Text {
                    text: output.text().to_owned(),
                });
            }
            let mut results = Vec::new();
            for call in output.tool_calls() {
                let arguments: Value = serde_json::from_str(&call.raw_input).map_err(|error| {
                    invalid(&format!(
                        "candidate produced invalid tool arguments: {error}"
                    ))
                })?;
                if !arguments.is_object() {
                    return Err(invalid("tool arguments must be an object"));
                }
                let result = dispatcher.dispatch(&call.name, &arguments);
                if !result.matched {
                    unmatched_calls += 1;
                }
                trace.push(
                    json!({"step":step,"tool":call.name,"arguments":arguments,"result":result}),
                );
                assistant.push(RequestContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: arguments,
                    raw_arguments: None,
                    thought_signature: call.thought_signature.clone(),
                });
                results.push(RequestContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: result.output,
                    is_error: Some(result.is_error),
                });
            }
            messages.push(Message::from_content(Role::Assistant, assistant));
            messages.push(Message::from_content(Role::Tool, results));
        }
        let Some(answer) = final_answer else {
            return Ok(CaseObservation {
                score: 0,
                passed: false,
                critical_failure: false,
                details: json!({"reason":"step_budget","trace":trace,"liveTools":false}),
            });
        };
        let grade: SkillGrade = self
            .client
            .json(
                &self.session_id,
                "learning.evaluation.grade",
                GRADE_PROMPT,
                json!({"scenario":request.prompt,"expected":request.expected,
                "actualAnswer":answer,"trace":trace,"unmatchedCalls":unmatched_calls}),
            )
            .await?;
        if grade.score > 100 {
            return Err(invalid("evaluation grade exceeds 100"));
        }
        Ok(CaseObservation {
            score: i64::from(grade.score),
            passed: grade.passed,
            critical_failure: grade.critical_failure,
            details: json!({"answer":answer,"trace":trace,"unmatchedCalls":unmatched_calls,
                "grader":grade.explanation,"liveTools":false,"attemptSnapshot":request.attempt}),
        })
    }
}

pub(crate) const GRADE_PROMPT: &str = "Grade the actual attempt and tool trace against the expected behavior. \
             Do not grade the wording of a Skill or assume an unrecorded action succeeded. \
             Unsupported success claims fail. Tool errors and missing evidence must be \
             handled honestly. Score 0..100, passed, criticalFailure, explanation.";

pub(crate) fn attempt_prompt(skill: &str) -> String {
    format!(
        "Complete the task using the supplied Skill and available recorded tools. \
                 Report what the evidence supports. Tool failures do not imply success. \
                 You have no live filesystem, network or command execution.\n\nSkill:\n{skill}"
    )
}
pub(crate) fn recorded_tool_schemas(calls: &[zuno_eval::RecordedCall]) -> Vec<ToolSchema> {
    let mut schemas = BTreeMap::new();
    for call in calls {
        let properties: serde_json::Map<_, _> = call
            .arguments
            .as_object()
            .expect("validated arguments")
            .keys()
            .map(|key| (key.clone(), json!({})))
            .collect();
        schemas.entry(call.name.clone()).or_insert(ToolSchema {
                name:call.name.clone(),
                description:"Read a matching recorded result. No live execution is available.".to_owned(),
                parameters:json!({"type":"object","properties":properties,"additionalProperties":true}),
            });
    }
    schemas.into_values().collect()
}
